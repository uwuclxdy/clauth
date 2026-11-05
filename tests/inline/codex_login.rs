#![allow(clippy::unwrap_used, clippy::expect_used)]
//! The codex PKCE login, held against the values read from openai/codex at
//! tag rust-v0.145.0: the URL/claim shapes and the assembly as pure values,
//! the loopback callback over a real socket pair (the state check and the
//! closed-set `error` parse), the registered-port bind order, and the two
//! network legs (the code exchange, the api-key mint) against a local stub.

use super::*;

use std::io::{Read as _, Write as _};
use std::net::TcpStream;

/// Feed one request through `handle_callback` over a real loopback socket
/// pair; returns its verdict and the raw HTTP response the "browser" received.
/// The request carries a complete header block, so the whole-connection read
/// ends at the terminator; the deadline is the production one and the login is
/// still live, so the security paths below are what this helper pins.
fn callback_roundtrip(target: &str, expected_state: &str) -> (Result<Option<String>>, String) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind loopback");
    let addr = listener.local_addr().expect("local addr");
    let mut client = TcpStream::connect(addr).expect("connect");
    client
        .write_all(format!("GET {target} HTTP/1.1\r\nHost: localhost\r\n\r\n").as_bytes())
        .expect("send request");
    let (server, _) = listener.accept().expect("accept");
    let wait_over = std::sync::atomic::AtomicBool::new(false);
    let verdict = handle_callback(
        server,
        expected_state,
        std::time::Instant::now() + callback_read_timeout(),
        &wait_over,
    );
    let mut response = String::new();
    client.read_to_string(&mut response).expect("read response");
    (verdict, response)
}

#[test]
fn the_callback_takes_the_code_only_under_the_expected_state() {
    let (verdict, response) =
        callback_roundtrip("/auth/callback?code=authcode-1&state=STATE", "STATE");
    assert_eq!(
        verdict.expect("valid callback").as_deref(),
        Some("authcode-1")
    );
    assert!(
        response.starts_with("HTTP/1.1 200 OK"),
        "got: {}",
        response.lines().next().unwrap_or("")
    );

    let (verdict, _) = callback_roundtrip("/auth/callback?code=authcode-1&state=EVIL", "STATE");
    assert_eq!(
        verdict.expect_err("a state mismatch aborts").to_string(),
        "codex login state mismatch (possible CSRF); aborted"
    );

    let (verdict, _) = callback_roundtrip("/favicon.ico", "STATE");
    assert!(
        verdict.expect("an unrelated path keeps waiting").is_none(),
        "not the redirect"
    );
}

/// The page a callback writes needs no follow-up request, so a stray request
/// on the same listener is answered as the waiting page and never taken for
/// the callback.
#[test]
fn a_stray_request_is_answered_as_waiting_without_taking_a_code() {
    let (verdict, response) = callback_roundtrip("/assets/login.css", "STATE");
    assert!(verdict.expect("a non-callback is not a callback").is_none());
    assert!(
        response.starts_with("HTTP/1.1 404 Not Found"),
        "got: {}",
        response.lines().next().unwrap_or("")
    );
    assert!(
        response.contains("Waiting for the codex login"),
        "the waiting page: {}",
        response.lines().next().unwrap_or("")
    );
}

/// Bytes only the browser redirect could have supplied, shaped to forge a log
/// line and a TUI span if they were ever echoed: the wire form the query
/// carries, and the form `query_param` decodes it to. Neither is a string any
/// page copy or terminal line of this module can contain, so their absence is
/// the echo check, whichever form an echo would carry.
const CALLBACK_CANARY: &str = "CANARY%0Aclauth:%20forged%20line%20%60rm%20-rf%60";
const CALLBACK_CANARY_DECODED: &str = "CANARY\nclauth: forged line `rm -rf`";

/// An OAuth `error` param is fatal, and its bytes reach nothing: the code is
/// parsed into RFC 6749's closed set and the description is never read, so
/// the terminal line and the reply page are this module's own literals.
#[test]
fn the_callback_parses_an_error_into_the_closed_set_and_echoes_nothing() {
    let (verdict, response) = callback_roundtrip(
        &format!(
            "/auth/callback?state=STATE&error=access_denied&error_description={CALLBACK_CANARY}"
        ),
        "STATE",
    );
    let err = verdict.expect_err("an error param is fatal").to_string();
    assert_eq!(err, "you declined the authorization request");
    for injected in [CALLBACK_CANARY, CALLBACK_CANARY_DECODED] {
        assert!(!err.contains(injected), "echoed into the line: {err}");
        assert!(
            !response.contains(injected),
            "echoed onto the page: {}",
            response.lines().next().unwrap_or("")
        );
    }
    assert!(
        response.contains("Authorization request declined. You can close this tab now."),
        "the declined arm's own page: {}",
        response.lines().next().unwrap_or("")
    );

    // A code outside the spec's set is where the bytes are least trustworthy:
    // none are kept, not even into the line.
    let (verdict, response) = callback_roundtrip(
        &format!(
            "/auth/callback?state=STATE&error=made_up_code&error_description={CALLBACK_CANARY}"
        ),
        "STATE",
    );
    let err = verdict.expect_err("an error param is fatal").to_string();
    assert_eq!(err, "openai refused the login");
    assert!(
        !response.contains("made_up_code"),
        "echoed onto the page: {}",
        response.lines().next().unwrap_or("")
    );

    let (verdict, _) = callback_roundtrip("/auth/callback?state=STATE&error=server_error", "STATE");
    assert_eq!(
        verdict.expect_err("an error param is fatal").to_string(),
        "openai is having trouble"
    );

    // Neither a code nor an error: a fixed line with nothing interpolated.
    let (verdict, _) = callback_roundtrip("/auth/callback?state=STATE", "STATE");
    assert_eq!(
        verdict.expect_err("no code is fatal").to_string(),
        "codex login callback carried no code"
    );
}

/// codex's registered redirect ports, pinned by literal: the redirect_uri must
/// match a registered one, so a swapped order or a dropped fallback ships a
/// browser login whose callback never reaches clauth.
#[test]
fn the_registered_ports_are_pinned_by_literal() {
    assert_eq!(REGISTERED_PORTS, [1455, 1457]);
}

/// The redirect set is two ports tried in order; a free port elsewhere would
/// not match a registered redirect_uri, so with both held the login refuses
/// instead of binding one. Driven through `bind_port_pair` on two ephemeral
/// ports the test reserves itself, so it never binds codex's registered
/// 1455/1457 and shares no lock with the cancel test (a failing sibling cannot
/// poison it).
#[test]
fn the_login_binds_the_seam_ports_in_order_and_refuses_with_both_held() {
    // Two free ephemeral ports, held together while the second binds so they
    // are distinct, then released for the seam to re-bind.
    let (p1, p2) = {
        let first = TcpListener::bind(("127.0.0.1", 0)).expect("bind");
        let second = TcpListener::bind(("127.0.0.1", 0)).expect("bind");
        let p1 = first.local_addr().expect("addr").port();
        let p2 = second.local_addr().expect("addr").port();
        drop(first);
        drop(second);
        (p1, p2)
    };
    let (primary, port) = bind_port_pair([p1, p2]).expect("the first port is free");
    assert_eq!(port, p1);
    let (fallback, port) = bind_port_pair([p1, p2]).expect("the fallback once the primary is held");
    assert_eq!(port, p2);
    let err = bind_port_pair([p1, p2]).expect_err("both held").to_string();
    assert_eq!(
        err,
        "codex's login ports (1455 and 1457) are both in use. Close whatever holds them \
         (another codex or clauth login?) and retry"
    );
    drop(primary);
    drop(fallback);
}

/// A cancel stops the callback wait within one 50 ms poll (the listener drops,
/// freeing the port) — never the 300 s timeout the worker would otherwise wait
/// out. Driven through an ephemeral listener so the test never binds codex's
/// registered ports 1455/1457 nor launches a browser.
#[test]
fn a_cancel_stops_the_callback_wait_and_frees_the_port() {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let cancel = LoginCancel::new();
    let flag = cancel.clone();
    let t0 = std::time::Instant::now();
    let h = std::thread::spawn(move || {
        let r = wait_for_code(&listener, "state", &cancel);
        drop(listener);
        r
    });
    std::thread::sleep(std::time::Duration::from_millis(200));
    flag.cancel();
    let r = h.join().expect("join");
    let took = t0.elapsed();
    let err = r.expect_err("canceled");
    assert_eq!(err.to_string(), "login canceled");
    assert!(took < std::time::Duration::from_secs(2), "took {took:?}");
    std::net::TcpListener::bind(("127.0.0.1", port)).expect("the port is free again");
}

/// A cancel on the real `login_with_cancel` drops the listener the login
/// bound — the ephemeral port binds again right after — and returns within a
/// bounded time, never the 300 s timeout. Driven through the seam on an
/// ephemeral port with a no-op browser opener, so the test never binds codex's
/// registered ports 1455/1457 nor launches a browser (M4). The port is read
/// off the announce URL the seam's binding produced, never hardcoded.
#[test]
fn a_cancel_frees_the_login_port_end_to_end() {
    let seam = CodexLoginSeam::new([0, 0], |_| {});
    let cancel = LoginCancel::new();
    let flag = cancel.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    let t0 = std::time::Instant::now();
    let h = std::thread::spawn(move || {
        login_with_cancel(
            |url| {
                let query = url.split_once('?').map(|(_, q)| q).unwrap_or(url);
                let redirect_uri = query_param(query, "redirect_uri").expect("redirect_uri");
                let port = redirect_uri
                    .strip_prefix("http://localhost:")
                    .and_then(|rest| rest.split('/').next())
                    .and_then(|p| p.parse::<u16>().ok())
                    .expect("the redirect_uri names the bound port");
                let _ = tx.send(port);
            },
            &cancel,
        )
    });
    let port = rx
        .recv_timeout(std::time::Duration::from_secs(5))
        .expect("the login announced the authorize URL");
    std::thread::sleep(std::time::Duration::from_millis(200));
    flag.cancel();
    let r = h.join().expect("join");
    let took = t0.elapsed();
    // No `Debug` on the token-carrying outcome, so no `expect_err`: match it.
    let err = match r {
        Ok(_) => panic!("a cancel must not land a login"),
        Err(e) => e,
    };
    assert_eq!(err.to_string(), "login canceled");
    assert!(
        took < std::time::Duration::from_secs(2),
        "a cancel returns within 2 s, took {took:?}"
    );
    std::net::TcpListener::bind(("127.0.0.1", port)).expect("the port is free again");
    drop(seam);
}

/// The wire shape of the code exchange against a local stub: form-urlencoded
/// (the encoding that differs from the JSON refresh at the same endpoint),
/// exactly the five pairs, and the three-field reply parsed back.
#[test]
fn the_code_exchange_sends_the_five_form_pairs_and_parses_the_three_fields() {
    let (addr, handle) = crate::testutil::serve_endpoints_raw(2, |_path, _i| {
        (
            200,
            r#"{"id_token":"id.x","access_token":"at.x","refresh_token":"rt.x"}"#.to_string(),
        )
    });
    let tok = exchange_code_at(
        &format!("{addr}/oauth/token"),
        "the code",
        "the-verifier",
        "http://localhost:1455/auth/callback",
    )
    .expect("exchange succeeds");
    assert_eq!(tok.id_token, "id.x");
    assert_eq!(tok.access_token, "at.x");
    assert_eq!(tok.refresh_token, "rt.x");

    let seen = handle.join().expect("join stub");
    assert_eq!(seen.len(), 1, "one call");
    let raw = &seen[0];
    assert_eq!(crate::testutil::request_path(raw), "/oauth/token");
    assert_eq!(
        crate::testutil::request_header(raw, "content-type").as_deref(),
        Some("application/x-www-form-urlencoded")
    );
    assert_eq!(
        crate::testutil::request_body(raw),
        format!(
            "grant_type=authorization_code&code=the%20code\
             &redirect_uri=http%3A%2F%2Flocalhost%3A1455%2Fauth%2Fcallback\
             &client_id={CODEX_CLIENT_ID}&code_verifier=the-verifier"
        )
    );
}

/// A non-2xx exchange is an error naming the status, never a parse of the
/// error body.
#[test]
fn a_rejected_code_exchange_names_the_status() {
    let (addr, handle) = crate::testutil::serve_endpoints_raw(2, |_path, _i| {
        (400, r#"{"error":"invalid_grant"}"#.to_string())
    });
    // No `Debug` on the token-carrying reply, so no `expect_err`.
    let err = match exchange_code_at(
        &format!("{addr}/oauth/token"),
        "c",
        "v",
        "http://localhost:1455/auth/callback",
    ) {
        Ok(_) => panic!("a 400 is an error"),
        Err(e) => e.to_string(),
    };
    assert_eq!(err, "codex token exchange returned HTTP 400");
    assert_eq!(handle.join().expect("join stub").len(), 1);
}

/// A 200 body torn mid-string still names the position in the error, and the
/// parser error never rides as its source.
#[test]
fn an_unparseable_exchange_body_names_the_position() {
    let (addr, handle) = crate::testutil::serve_endpoints_raw(1, |_path, _i| {
        (200, "{\"id_token\": \"sk-c2probe-NOTAKEY".to_string())
    });
    let err = match exchange_code_at(
        &format!("{addr}/oauth/token"),
        "c",
        "v",
        "http://localhost:1455/auth/callback",
    ) {
        Ok(_) => panic!("a torn body fails the exchange"),
        Err(e) => e,
    };
    let rendered = format!("{err:#}");
    assert_eq!(
        rendered,
        "codex token exchange response did not parse (line 1, column 32)"
    );
    assert!(
        err.source().is_none(),
        "the parse error must not ride as a source: {err:?}"
    );
    let _ = handle.join();
}

/// A 200 body that is a bare JSON string where the exchange object belongs is
/// a type error whose base rendering quoted the string value; the composed
/// line names the position alone, never the value.
#[test]
fn a_mistyped_exchange_body_names_the_position_and_never_the_value() {
    let (addr, handle) = crate::testutil::serve_endpoints_raw(1, |_path, _i| {
        (200, "\"sk-c2probe-NOTAKEY\"".to_string())
    });
    let err = match exchange_code_at(
        &format!("{addr}/oauth/token"),
        "c",
        "v",
        "http://localhost:1455/auth/callback",
    ) {
        Ok(_) => panic!("a string body fails the exchange"),
        Err(e) => e,
    };
    let rendered = format!("{err:#}");
    assert_eq!(
        rendered,
        "codex token exchange response did not parse (line 1, column 20)"
    );
    assert!(
        err.source().is_none(),
        "the parse error must not ride as a source: {err:?}"
    );
    let _ = handle.join();
}

/// An empty 2xx body is an end-of-input error at line 1, column 0: the
/// exchange body is a network reply, never a file, so it still names line 1
/// instead of staying bare like an empty file.
#[test]
fn an_empty_exchange_body_names_line_1() {
    let (addr, handle) = crate::testutil::serve_endpoints_raw(1, |_path, _i| (200, String::new()));
    let err = match exchange_code_at(
        &format!("{addr}/oauth/token"),
        "c",
        "v",
        "http://localhost:1455/auth/callback",
    ) {
        Ok(_) => panic!("an empty body fails the exchange"),
        Err(e) => e,
    };
    let rendered = format!("{err:#}");
    assert_eq!(
        rendered,
        "codex token exchange response did not parse (line 1)"
    );
    assert!(
        err.source().is_none(),
        "the parse error must not ride as a source: {err:?}"
    );
    let _ = handle.join();
}

/// The api-key mint is best-effort by contract: the key on a 2xx body that
/// carries one, `Ok(None)` on a 4xx, so a login that only wants the ChatGPT
/// chain still completes.
#[test]
fn the_api_key_exchange_is_best_effort() {
    let (addr, handle) = crate::testutil::serve_endpoints_raw(2, |_path, _i| {
        (200, r#"{"access_token":"sk-minted"}"#.to_string())
    });
    assert_eq!(
        exchange_api_key_at(&format!("{addr}/oauth/token"), "h.p.s").expect("2xx"),
        Some("sk-minted".to_string())
    );
    let seen = handle.join().expect("join stub");
    assert_eq!(seen.len(), 1);
    assert_eq!(
        crate::testutil::request_header(&seen[0], "content-type").as_deref(),
        Some("application/x-www-form-urlencoded")
    );
    assert_eq!(
        crate::testutil::request_body(&seen[0]),
        format!(
            "grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Atoken-exchange\
             &client_id={CODEX_CLIENT_ID}&requested_token=openai-api-key\
             &subject_token=h.p.s&subject_token_type=urn%3Aietf%3Aparams%3Aoauth%3Atoken-type%3Aid_token"
        )
    );

    let (addr, handle) = crate::testutil::serve_endpoints_raw(2, |_path, _i| {
        (403, r#"{"error":"forbidden"}"#.to_string())
    });
    assert_eq!(
        exchange_api_key_at(&format!("{addr}/oauth/token"), "h.p.s")
            .expect("a 4xx is not an error"),
        None
    );
    assert_eq!(handle.join().expect("join stub").len(), 1);
}

#[test]
fn the_authorize_url_carries_the_verified_params() {
    let url = authorize_url("http://localhost:1455/auth/callback", "CHAL", "STATE");
    assert!(url.starts_with("https://auth.openai.com/oauth/authorize?"));
    for needle in [
        "response_type=code",
        &format!("client_id={CODEX_CLIENT_ID}"),
        "redirect_uri=http%3A%2F%2Flocalhost%3A1455%2Fauth%2Fcallback",
        "code_challenge=CHAL",
        "code_challenge_method=S256",
        "id_token_add_organizations=true",
        "codex_cli_simplified_flow=true",
        "state=STATE",
        "originator=codex_cli_rs",
    ] {
        assert!(url.contains(needle), "missing {needle} in {url}");
    }
    // The verified scope set, percent-encoded.
    assert!(
        url.contains("scope=openid%20profile%20email%20offline_access"),
        "scope: {url}"
    );
}

/// The account id is the `chatgpt_account_id` claim nested under the
/// id_token's `https://api.openai.com/auth` object — codex's exact nesting.
#[test]
fn the_account_id_comes_from_the_nested_claim() {
    let payload = crate::oauth_login::base64url_nopad(
        br#"{"https://api.openai.com/auth":{"chatgpt_account_id":"acc-123"}}"#,
    );
    let jwt = format!("h.{payload}.s");
    assert_eq!(chatgpt_account_id(&jwt).as_deref(), Some("acc-123"));

    // A flat claim (not nested) is NOT read — the nesting is the contract.
    let flat = crate::oauth_login::base64url_nopad(br#"{"chatgpt_account_id":"nope"}"#);
    assert_eq!(chatgpt_account_id(&format!("h.{flat}.s")), None);
}

/// The assembled auth.json: explicit `auth_mode` (codex infers ApiKey from a
/// bare key otherwise), the chain, the account id folded in, and no
/// OPENAI_API_KEY when the secondary exchange yielded none.
#[test]
fn the_auth_json_is_codexs_shape() {
    let payload = crate::oauth_login::base64url_nopad(
        br#"{"https://api.openai.com/auth":{"chatgpt_account_id":"acc-9"}}"#,
    );
    let tok = CodeExchange {
        id_token: format!("h.{payload}.s"),
        access_token: "at.x".into(),
        refresh_token: "rt.x".into(),
    };
    let outcome = assemble_auth_json(tok, None);
    let v: serde_json::Value = serde_json::from_slice(&outcome.auth_json).unwrap();
    assert_eq!(v["auth_mode"], "chatgpt", "explicit, never inferred");
    assert_eq!(v["tokens"]["access_token"], "at.x");
    assert_eq!(v["tokens"]["refresh_token"], "rt.x");
    assert_eq!(v["tokens"]["account_id"], "acc-9");
    assert!(v["last_refresh"].is_string());
    assert!(
        v.get("OPENAI_API_KEY").is_none(),
        "no api key when the exchange is skipped/offline"
    );
    assert_eq!(outcome.account_id.as_deref(), Some("acc-9"));
    // The chain reparses through the same model the runtime uses.
    let parsed = crate::codex_auth::CodexAuth::parse(&outcome.auth_json).expect("parse");
    assert_eq!(parsed.refresh_token(), Some("rt.x"));
    assert_eq!(parsed.account_id(), Some("acc-9"));
}

/// The encoding trap the spec pins: the authorization-code exchange is
/// form-urlencoded, while the refresh at the SAME endpoint is JSON. A
/// mutation swapping the exchange to JSON reds this.
#[test]
fn the_code_exchange_is_form_urlencoded_unlike_the_json_refresh() {
    let body = code_exchange_body(
        "the-code",
        "the-verifier",
        "http://localhost:1455/auth/callback",
    );
    assert!(body.starts_with("grant_type=authorization_code&"), "{body}");
    assert!(body.contains("&code=the-code&"), "{body}");
    assert!(body.contains("&code_verifier=the-verifier"), "{body}");
    assert!(
        !body.trim_start().starts_with('{'),
        "form, never JSON: {body}"
    );

    // The refresh body IS json, at the same endpoint — the two must not drift.
    let refresh = crate::codex_auth::CODEX_TOKEN_URL;
    assert_eq!(refresh, "https://auth.openai.com/oauth/token");
}

/// A browser preconnect that never sends a request must not hold codex's
/// real callback queued behind it: the code lands at once, never after the
/// silent socket's read bound.
#[test]
fn a_silent_preconnect_never_delays_the_queued_codex_callback() {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("bind");
    let addr = listener.local_addr().expect("addr");
    let _silent = TcpStream::connect(addr).expect("connect the silent socket");
    let mut real = TcpStream::connect(addr).expect("connect the callback");
    real.write_all(
        b"GET /auth/callback?code=authcode-1&state=state HTTP/1.1\r\nHost: localhost\r\n\r\n",
    )
    .expect("send the callback");
    let cancel = LoginCancel::new();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(wait_for_code(&listener, "state", &cancel));
    });
    let code = rx
        .recv_timeout(std::time::Duration::from_secs(2))
        .expect("the callback lands while the silent socket is still open")
        .expect("a valid callback");
    assert_eq!(code, "authcode-1");
}

/// A cancel lands at once even while a silent socket is open: the wait never
/// sits inside that socket's read with the cancel unseen.
#[test]
fn a_cancel_lands_while_a_silent_preconnect_is_open() {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("bind");
    let addr = listener.local_addr().expect("addr");
    let _silent = TcpStream::connect(addr).expect("connect the silent socket");
    let cancel = LoginCancel::new();
    let flag = cancel.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(wait_for_code(&listener, "state", &cancel));
    });
    std::thread::sleep(std::time::Duration::from_millis(300));
    flag.cancel();
    let err = rx
        .recv_timeout(std::time::Duration::from_secs(2))
        .expect("the cancel lands while the silent socket is still open")
        .expect_err("canceled");
    assert_eq!(err.to_string(), "login canceled");
}

/// A request line that arrives slower than the old 5 s read bound still yields
/// the code now that the bound is 60 s. Scaled: the seam shrinks the bound to
/// 600 ms and the request line lands at 300 ms — past the old bound's scaled
/// form, inside the new one — so the code is not lost.
#[test]
fn a_request_line_past_the_old_read_bound_still_yields_the_code() {
    let _home = crate::testutil::HomeSandbox::new();
    super::set_callback_read_timeout_override(Some(std::time::Duration::from_millis(600)));
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("bind");
    let addr = listener.local_addr().expect("addr");
    let cancel = LoginCancel::new();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(wait_for_code(&listener, "state", &cancel));
    });
    let mut client = TcpStream::connect(addr).expect("connect");
    std::thread::sleep(std::time::Duration::from_millis(300));
    client
        .write_all(b"GET /auth/callback?code=slow-code&state=state HTTP/1.1\r\n\r\n")
        .expect("send the slow callback");
    let code = rx
        .recv_timeout(std::time::Duration::from_secs(2))
        .expect("the slow callback lands")
        .expect("a valid callback");
    assert_eq!(code, "slow-code");
    super::set_callback_read_timeout_override(None);
}

/// A state mismatch is a security stop wherever it arrives: one queued behind
/// a silent socket still ends the wait with the mismatch error, never an
/// ignored request. The reader thread's `Err` is what carries it back.
#[test]
fn a_state_mismatch_behind_a_silent_preconnect_still_stops_the_wait() {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("bind");
    let addr = listener.local_addr().expect("addr");
    let _silent = TcpStream::connect(addr).expect("connect the silent socket");
    let mut evil = TcpStream::connect(addr).expect("connect the callback");
    evil.write_all(b"GET /auth/callback?code=authcode-1&state=EVIL HTTP/1.1\r\n\r\n")
        .expect("send the callback");
    let cancel = LoginCancel::new();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(wait_for_code(&listener, "STATE", &cancel));
    });
    let err = rx
        .recv_timeout(std::time::Duration::from_secs(2))
        .expect("the wait ends")
        .expect_err("a state mismatch stops the login");
    assert_eq!(
        err.to_string(),
        "codex login state mismatch (possible CSRF); aborted"
    );
}

/// The bound is cloudy's ruled number, pinned by equality: the scaled test
/// above pins the SHAPE, this one pins the 60 s the ruling names, so retuning
/// the constant cannot ship green.
#[test]
fn the_callback_connection_bound_is_the_ruled_sixty_seconds() {
    assert_eq!(
        super::CALLBACK_READ_TIMEOUT,
        std::time::Duration::from_secs(60),
        "cloudy's 2026-10-21 ruling: one accepted codex callback connection gets 60 s"
    );
}

/// A peer trickling one byte just under any per-read bound must still end at
/// the connection's ONE deadline, driven through `codex_login::wait_for_code`
/// (the codex listener arms its own per-connection deadline). On a per-read
/// timeout it would hold its reader for as long as it kept dribbling.
#[test]
fn a_trickling_peer_cannot_outlive_the_whole_connection_deadline() {
    use std::io::Read as _;

    let _home = crate::testutil::HomeSandbox::new();
    super::set_callback_read_timeout_override(Some(std::time::Duration::from_millis(400)));
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("bind");
    let addr = listener.local_addr().expect("addr");
    let cancel = LoginCancel::new();
    let flag = cancel.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(wait_for_code(&listener, "state", &cancel));
    });

    let mut trickler = TcpStream::connect(addr).expect("connect the trickler");
    let mut watcher = trickler.try_clone().expect("clone the socket");
    // Set the timeout right after connect, before any peer end is served or
    // dropped (test-infrastructure.md RULE).
    watcher
        .set_read_timeout(Some(std::time::Duration::from_secs(2)))
        .expect("read timeout");
    std::thread::spawn(move || {
        // One byte every 50 ms for far longer than the watcher waits: inside any
        // per-read bound of 400 ms, so a per-read scheme never closes the socket.
        for _ in 0..200 {
            if trickler.write_all(b"G").is_err() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    });

    let started = std::time::Instant::now();
    let mut byte = [0u8; 1];
    let verdict = watcher.read(&mut byte);
    let elapsed = started.elapsed();
    let ended = match &verdict {
        Ok(0) => true,
        Err(e) => matches!(
            e.kind(),
            std::io::ErrorKind::ConnectionReset
                | std::io::ErrorKind::ConnectionAborted
                | std::io::ErrorKind::BrokenPipe
        ),
        Ok(_) => false,
    };
    assert!(
        ended,
        "the server closes a trickling connection at its whole-connection deadline; got: {verdict:?}"
    );
    assert!(
        elapsed >= std::time::Duration::from_millis(300)
            && elapsed < std::time::Duration::from_secs(1),
        "the connection ends at the scaled 400 ms deadline, not some longer bound: {elapsed:?}"
    );

    // End the wait so its thread does not outlive the test.
    flag.cancel();
    rx.recv_timeout(std::time::Duration::from_secs(2))
        .expect("the wait ends")
        .expect_err("canceled");
}

/// Past the live-reader cap a new connection is dropped, never spawned: a local
/// flood costs connections, not threads — driven through `codex_login`'s own
/// public wait entry, so the codex accept loop's cap spawn is what this pins.
#[test]
fn a_callback_connection_past_the_reader_cap_is_dropped() {
    use std::io::Read as _;

    let _home = crate::testutil::HomeSandbox::new();
    // Long enough that the first reader still holds its slot when the second
    // connects.
    super::set_callback_read_timeout_override(Some(std::time::Duration::from_secs(2)));
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("bind");
    let addr = listener.local_addr().expect("addr");
    let cancel = LoginCancel::new();
    let flag = cancel.clone();

    // The accept loop (and so `spawn_callback_reader`) runs on the thread that
    // enters `wait_for_code` — this one — because the cap override is
    // thread-local (a process-wide tiny cap would refuse a sibling test's
    // reader). The browser side runs on a helper thread.
    let (verdict_tx, verdict_rx) = std::sync::mpsc::channel();
    let browser = std::thread::spawn(move || {
        // The first silent socket fills the one slot; give the accept loop a
        // poll to take it before the second arrives. The override carries its
        // own counter, so this first reader is ours and holds the slot for the
        // whole scaled deadline.
        let _first = TcpStream::connect(addr).expect("connect the first");
        std::thread::sleep(std::time::Duration::from_millis(300));
        let mut second = TcpStream::connect(addr).expect("connect the second");
        // A client bound well under the server's 2 s deadline: under the cap the
        // read sees EOF at once; a cap bypass holds the reader to its 2 s
        // deadline and this times out instead (two equal timers would race).
        second
            .set_read_timeout(Some(std::time::Duration::from_millis(500)))
            .expect("read timeout");
        let mut byte = [0u8; 1];
        let verdict = second.read(&mut byte);
        let _ = verdict_tx.send(verdict);
        flag.cancel();
    });

    crate::oauth_login::set_callback_reader_cap_override(Some(1));
    let outcome = wait_for_code(&listener, "state", &cancel);
    crate::oauth_login::set_callback_reader_cap_override(None);
    assert_eq!(
        outcome.expect_err("the wait ends on cancel").to_string(),
        "login canceled"
    );

    let verdict = verdict_rx
        .recv_timeout(std::time::Duration::from_secs(2))
        .expect("the refused connection reports its verdict");
    assert_eq!(
        verdict.expect("the refused connection is closed"),
        0,
        "past the cap the server drops the connection instead of spawning a reader"
    );
    browser.join().expect("browser thread");
}

/// A reader that finishes after the wait already ended writes NO page of any
/// kind — here a state-mismatch request arriving late: the login is over, so
/// even the security page would lie. The connection just closes.
#[test]
fn a_reader_finishing_after_the_wait_ended_writes_no_page() {
    use std::io::Read as _;

    let _home = crate::testutil::HomeSandbox::new();
    super::set_callback_read_timeout_override(Some(std::time::Duration::from_secs(5)));
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("bind");
    let addr = listener.local_addr().expect("addr");
    let cancel = LoginCancel::new();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(wait_for_code(&listener, "state", &cancel));
    });

    // Accepted first, and silent: its reader is still waiting when the login ends.
    let mut late = TcpStream::connect(addr).expect("connect the late callback");
    // Set the timeout right after connect, before any write or peer drop
    // (test-infrastructure.md RULE).
    late.set_read_timeout(Some(std::time::Duration::from_secs(2)))
        .expect("read timeout");
    std::thread::sleep(std::time::Duration::from_millis(300));
    let mut evil = TcpStream::connect(addr).expect("connect the mismatch");
    evil.write_all(b"GET /auth/callback?code=x&state=EVIL HTTP/1.1\r\n\r\n")
        .expect("send the mismatch");
    let err = rx
        .recv_timeout(std::time::Duration::from_secs(2))
        .expect("the mismatch ends the wait")
        .expect_err("a state mismatch stops the login");
    assert!(
        err.to_string().contains("state mismatch"),
        "the wait ended on the security stop: {err}"
    );

    // A non-success request arrives after the wait returned: no page of any
    // kind is written, the connection just closes.
    late.write_all(b"GET /auth/callback?code=late-code&state=EVIL HTTP/1.1\r\n\r\n")
        .expect("send the late callback");
    let mut response = String::new();
    let _ = late.read_to_string(&mut response);
    assert_eq!(
        response, "",
        "an already-ended login writes no page of any kind"
    );
}

/// Serve one POST a reply whose body read fails: it declares `content_length`
/// but writes only `body` bytes, then closes. A `body` shorter than
/// `content_length` cuts the reply mid-body; a `body` of invalid UTF-8 (with an
/// honest `content_length`) fails the read's UTF-8 check. Both read failures
/// reach the caller before any parse, so no position exists to name.
fn serve_unreadable_body(
    status: u16,
    body: &'static [u8],
    content_length: usize,
) -> (String, std::thread::JoinHandle<()>) {
    use std::io::Write;
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind loopback");
    let port = listener.local_addr().expect("local_addr").port();
    let handle = std::thread::spawn(move || {
        let (mut sock, _) = listener.accept().expect("accept");
        crate::testutil::drain_http_request(&mut sock);
        let _ = sock.write_all(
            format!(
                "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {content_length}\r\nConnection: close\r\n\r\n"
            )
            .as_bytes(),
        );
        let _ = sock.write_all(body);
        let _ = sock.shutdown(std::net::Shutdown::Write);
    });
    (format!("http://127.0.0.1:{port}"), handle)
}

/// A 2xx exchange reply whose body read fails (the connection cut mid-body) is
/// a transport failure: the status is checked first, then the body read fails
/// under the exchange's transport context, carrying no body byte and no
/// position.
#[test]
fn a_2xx_exchange_body_cut_short_is_a_transport_failure() {
    let (addr, handle) = serve_unreadable_body(
        200,
        b"{\"access_token\": \"at\", \"refresh_token\": \"rt\"",
        300,
    );
    let err = match exchange_code_at(
        &format!("{addr}/oauth/token"),
        "c",
        "v",
        "http://localhost:1455/auth/callback",
    ) {
        Ok(_) => panic!("a cut body fails the exchange"),
        Err(e) => e,
    };
    assert_eq!(
        format!("{err:#}"),
        "codex token exchange transport failed: io: Peer disconnected"
    );
    let _ = handle.join();
}

/// A 2xx exchange reply whose body is not UTF-8 fails the read itself, under
/// the same transport context and with no body byte.
#[test]
fn a_non_utf8_exchange_body_is_a_transport_failure() {
    let (addr, handle) = serve_unreadable_body(200, &[0xFF, 0xFE, 0x00, 0x01], 4);
    let err = match exchange_code_at(
        &format!("{addr}/oauth/token"),
        "c",
        "v",
        "http://localhost:1455/auth/callback",
    ) {
        Ok(_) => panic!("a non-UTF-8 body fails the exchange"),
        Err(e) => e,
    };
    assert_eq!(
        format!("{err:#}"),
        "codex token exchange transport failed: io: stream did not contain valid UTF-8"
    );
    let _ = handle.join();
}

/// A non-2xx exchange reply names its status before the body is read, so an
/// unreadable body never masks the status.
#[test]
fn a_non_2xx_exchange_reply_with_an_unreadable_body_names_the_status() {
    let (addr, handle) = serve_unreadable_body(400, b"short", 300);
    let err = match exchange_code_at(
        &format!("{addr}/oauth/token"),
        "c",
        "v",
        "http://localhost:1455/auth/callback",
    ) {
        Ok(_) => panic!("a 400 fails the exchange"),
        Err(e) => e,
    };
    assert_eq!(err.to_string(), "codex token exchange returned HTTP 400");
    let _ = handle.join();
}
