#![allow(clippy::unwrap_used, clippy::expect_used)]
//! OAUTH-1 — deterministic, network-free tests for the PKCE + URL machinery,
//! plus the loopback callback's security paths over in-process sockets.
//! The browser round-trip and token exchange are manual acceptance.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};

use super::{
    authorize_url, base64url_nopad, callback_read_timeout, challenge_from_verifier,
    handle_callback, percent_decode, percent_encode, query_param, request_target, wait_for_code,
};

#[test]
fn pkce_challenge_matches_rfc7636_vector() {
    // RFC 7636 Appendix B: verifier → S256 challenge.
    let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
    assert_eq!(
        challenge_from_verifier(verifier),
        "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
    );
}

#[test]
fn base64url_nopad_matches_rfc4648_vectors() {
    // RFC 4648 §10 test vectors (base64url == base64 for these ASCII inputs).
    assert_eq!(base64url_nopad(b""), "");
    assert_eq!(base64url_nopad(b"f"), "Zg");
    assert_eq!(base64url_nopad(b"fo"), "Zm8");
    assert_eq!(base64url_nopad(b"foo"), "Zm9v");
    assert_eq!(base64url_nopad(b"foob"), "Zm9vYg");
    assert_eq!(base64url_nopad(b"fooba"), "Zm9vYmE");
    assert_eq!(base64url_nopad(b"foobar"), "Zm9vYmFy");
    // Exercises the URL-safe alphabet (`-` and `_` instead of `+`/`/`).
    assert_eq!(base64url_nopad(&[0xfb, 0xff, 0xbf]), "-_-_");
}

#[test]
fn percent_encode_and_decode_round_trip() {
    let scope = "user:profile user:inference";
    assert_eq!(percent_encode(scope), "user%3Aprofile%20user%3Ainference");
    assert_eq!(percent_decode(&percent_encode(scope)), scope);

    let redirect = "http://localhost:52341/callback";
    assert_eq!(percent_decode(&percent_encode(redirect)), redirect);
}

#[test]
fn query_param_extracts_and_decodes() {
    let q = "code=abc%2D123&state=xyz";
    assert_eq!(query_param(q, "code").as_deref(), Some("abc-123"));
    assert_eq!(query_param(q, "state").as_deref(), Some("xyz"));
    assert_eq!(query_param(q, "missing"), None);
}

#[test]
fn percent_decode_survives_malformed_and_multibyte() {
    // A bare '%' followed by a multi-byte UTF-8 char must not panic (byte-index
    // slicing would); the '%' passes through and the char is preserved.
    assert_eq!(percent_decode("%€"), "%€");
    // Trailing '%' with no hex digits, and a non-hex escape, pass through raw.
    assert_eq!(percent_decode("a%"), "a%");
    assert_eq!(percent_decode("a%zz"), "a%zz");
    // Valid escapes still decode.
    assert_eq!(percent_decode("%2Fpath"), "/path");
    // A form value's `+` is a space; `percent_encode` never writes one, so the
    // round trip above cannot pin this half.
    assert_eq!(percent_decode("a+b"), "a b");
    // `%+1` is not an escape: `from_str_radix` alone would take the `+`.
    assert_eq!(percent_decode("%+1"), "% 1");
}

#[test]
fn request_target_pulls_path_and_query() {
    assert_eq!(
        request_target("GET /callback?code=x&state=y HTTP/1.1"),
        Some("/callback?code=x&state=y")
    );
    assert_eq!(request_target(""), None);
}

#[test]
fn authorize_url_uses_claude_ai_host_code_true_and_six_scopes() {
    let url = authorize_url("http://localhost:1234/callback", "CHAL", "STATE");
    // Pro/Max subscription authorize host (CLAUDE_AI_AUTHORIZE_URL), NOT the
    // platform.claude.com Console host which doesn't mint claude.ai creds.
    assert!(url.starts_with("https://claude.com/cai/oauth/authorize?"));
    // code=true is appended unconditionally, as the binary does for every request.
    assert!(url.contains("code=true"));
    assert!(url.contains(&format!("client_id={}", crate::oauth::CLIENT_ID)));
    assert!(url.contains("response_type=code"));
    assert!(url.contains("code_challenge=CHAL"));
    assert!(url.contains("code_challenge_method=S256"));
    assert!(url.contains("state=STATE"));
    assert!(url.contains("redirect_uri=http%3A%2F%2Flocalhost%3A1234%2Fcallback"));
    // The full 6-scope union (ALL_OAUTH_SCOPES), colons percent-encoded and
    // separators as `+` — the exact byte shape Claude Code sends (wire-parity).
    assert!(url.contains(
        "scope=org%3Acreate_api_key+user%3Aprofile+user%3Ainference+user%3Asessions%3Aclaude_code+user%3Amcp_servers+user%3Afile_upload"
    ));
}

// ── handle_callback / wait_for_code: the redirect's security paths ────────────

/// Feed one request line through `handle_callback` over a real loopback socket
/// pair; returns its verdict and the raw HTTP response the "browser" received.
/// The connection deadline is the production one and the login is still live:
/// the security paths below are what this helper pins.
fn callback_roundtrip(
    request_line: &str,
    expected_state: &str,
) -> (anyhow::Result<Option<String>>, String) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind loopback");
    let addr = listener.local_addr().expect("local addr");
    let mut client = TcpStream::connect(addr).expect("connect");
    client
        .write_all(request_line.as_bytes())
        .expect("send request");
    let (server, _) = listener.accept().expect("accept");
    let wait_over = std::sync::atomic::AtomicBool::new(false);
    let verdict = handle_callback(
        server,
        expected_state,
        std::time::Instant::now() + callback_read_timeout(),
        &wait_over,
    );
    // handle_callback dropped its stream → EOF, so this reads the full response.
    let mut response = String::new();
    client.read_to_string(&mut response).expect("read response");
    (verdict, response)
}

#[test]
fn callback_accepts_matching_state_and_extracts_the_code() {
    let (verdict, response) = callback_roundtrip(
        "GET /callback?code=authcode-1&state=STATE HTTP/1.1\r\n",
        "STATE",
    );
    assert_eq!(
        verdict.expect("valid callback").as_deref(),
        Some("authcode-1")
    );
    assert!(response.starts_with("HTTP/1.1 200 OK"), "got: {response}");
}

#[test]
fn callback_aborts_on_a_state_mismatch() {
    let (verdict, response) = callback_roundtrip(
        "GET /callback?code=authcode-1&state=EVIL HTTP/1.1\r\n",
        "STATE",
    );
    let err = verdict
        .expect_err("a state mismatch must abort the login")
        .to_string();
    assert!(err.contains("state mismatch"), "err was: {err}");
    assert!(response.starts_with("HTTP/1.1 400"), "got: {response}");
}

/// Bytes only the browser redirect could have supplied.
const CALLBACK_CANARY: &str = "CALLBACK-BYTES-CANARY";

/// An OAuth `error` param is still fatal, and its bytes now reach nothing. Both
/// params are uncapped upstream text and used to be interpolated whole into the
/// `bail!` — which lands on `clauth login`'s stderr and the TUI Setup toast.
/// Worse than the token-endpoint bodies this module's sibling fix removed: those
/// at least passed a first-line + 200-char trim first.
#[test]
fn callback_aborts_on_an_oauth_error_param_without_echoing_it() {
    let (verdict, response) = callback_roundtrip(
        &format!(
            "GET /callback?error=access_denied&error_description={CALLBACK_CANARY} HTTP/1.1\r\n"
        ),
        "STATE",
    );
    let err = verdict
        .expect_err("an OAuth error param is fatal")
        .to_string();
    assert_eq!(err, "you declined the authorization request");
    assert!(response.starts_with("HTTP/1.1 400"), "got: {response}");
    assert!(
        !response.contains(CALLBACK_CANARY),
        "the page must not reflect it either: {response}"
    );

    // An `error` code outside RFC 6749's set is where the bytes are least
    // trustworthy, so NONE of them are kept — not even capped into the log.
    let (verdict, _) = callback_roundtrip(
        &format!(
            "GET /callback?error={CALLBACK_CANARY}&error_description={CALLBACK_CANARY} HTTP/1.1\r\n"
        ),
        "STATE",
    );
    let err = verdict
        .expect_err("an unrecognized error code is still fatal")
        .to_string();
    assert!(
        !err.contains(CALLBACK_CANARY),
        "an unrecognized code must not be echoed: {err}"
    );
    assert_eq!(err, "anthropic refused the login");
}

/// The parse is what drops the browser's bytes, so pin the whole map. Every
/// value any arm carries onward is one of `parse`'s own literals — asserted by
/// reading `log_detail` back, which is the only thing that still names a code.
#[test]
fn authorize_rejection_parses_rfc6749_codes_and_anonymizes_the_rest() {
    use crate::oauth_login::AuthorizeRejection;
    for (code, detail, user) in [
        (
            "access_denied",
            "access_denied",
            "you declined the authorization request",
        ),
        (
            "server_error",
            "server_error",
            "anthropic is having trouble",
        ),
        (
            "temporarily_unavailable",
            "temporarily_unavailable",
            "anthropic is having trouble",
        ),
        (
            "invalid_request",
            "invalid_request",
            "anthropic refused the login",
        ),
        (
            "unauthorized_client",
            "unauthorized_client",
            "anthropic refused the login",
        ),
        (
            "unsupported_response_type",
            "unsupported_response_type",
            "anthropic refused the login",
        ),
        (
            "invalid_scope",
            "invalid_scope",
            "anthropic refused the login",
        ),
    ] {
        let r = AuthorizeRejection::parse(code);
        assert_eq!(r.log_detail(), detail, "log detail for {code}");
        assert_eq!(r.user_message(), user, "user copy for {code}");
    }

    let odd = AuthorizeRejection::parse(CALLBACK_CANARY);
    assert!(
        !odd.log_detail().contains(CALLBACK_CANARY),
        "an unrecognized code must not survive into the log"
    );
    assert_eq!(
        odd.log_detail(),
        "an error code outside RFC 6749's set (withheld)"
    );
}

/// `clauth login` is the path where the status ruling bites hardest: an
/// interactive user hitting a 400 on a fresh login has no log open beside the
/// terminal. So stderr names the status and the TUI toast does not — asserted
/// together, because a status that silently stops reaching stderr looks exactly
/// like one that was never added.
#[test]
fn login_names_the_status_on_stderr_but_not_in_the_toast() {
    use crate::oauth::TokenFailure;
    use crate::oauth_login::LoginError;

    let rejected = LoginError::Exchange(TokenFailure::Status(400));
    assert_eq!(
        rejected.cli_message(),
        "anthropic rejected the request (HTTP 400): run `clauth login` again for a fresh code"
    );
    assert_eq!(
        rejected.user_message(),
        "anthropic rejected the request: run `clauth login` again for a fresh code"
    );
    assert!(
        !rejected.user_message().contains("400"),
        "the toast must not carry the status: {}",
        rejected.user_message()
    );

    // The advice is NOT "retry in a moment", which is what reusing the refresh
    // path's mapping produced. A code exchange is rejected at the END of the
    // flow: the loopback listener is gone and the code is spent, expired, or
    // never matched the PKCE verifier, so waiting fixes none of them. A 429 is
    // the case that makes it obvious — a refresh would genuinely wait and
    // re-attempt next tick, a login has no next tick.
    for status in [400, 429, 503] {
        let m = LoginError::Exchange(TokenFailure::Status(status)).user_message();
        assert!(
            m.ends_with(": run `clauth login` again for a fresh code"),
            "a spent authorization code is only fixed by a new login, got: {m}"
        );
        assert!(
            !m.contains("retry in a moment"),
            "there is nothing left to retry once the flow has ended: {m}"
        );
    }

    // A transport failure never saw a status, so the two forms coincide rather
    // than inventing one, and the advice is the connection — the one blocker
    // worth clearing BEFORE running the command again.
    let offline = LoginError::Exchange(TokenFailure::Transport);
    assert_eq!(
        offline.cli_message(),
        "could not reach anthropic: check your connection and retry"
    );
    assert_eq!(offline.cli_message(), offline.user_message());

    // Every clauth-authored failure renders identically on both surfaces —
    // there is no upstream status to withhold from one of them.
    let local = LoginError::Local(anyhow::anyhow!("OAuth state mismatch (possible CSRF)"));
    assert_eq!(local.cli_message(), local.user_message());
    assert_eq!(local.user_message(), "OAuth state mismatch (possible CSRF)");
}

/// Same structural guard as `oauth::TokenFailure`: no `Display` and no
/// `Into<anyhow::Error>`, so a surface cannot print the browser's words even by
/// accident. Positive controls prove the probe discriminates.
#[test]
fn authorize_rejection_has_no_printable_escape_hatch() {
    use crate::oauth_login::{AuthorizeRejection, LoginError};
    use crate::testutil::{NotDisplay as _, NotIntoAnyhow as _, Probe};
    assert!(Probe::<String>::is_display(), "positive control");
    assert!(Probe::<std::io::Error>::into_anyhow(), "positive control");
    for (name, displays, converts) in [
        (
            "AuthorizeRejection",
            Probe::<AuthorizeRejection>::is_display(),
            Probe::<AuthorizeRejection>::into_anyhow(),
        ),
        (
            "LoginError",
            Probe::<LoginError>::is_display(),
            Probe::<LoginError>::into_anyhow(),
        ),
    ] {
        assert!(!displays, "{name} must stay Display-free");
        assert!(
            !converts,
            "{name} must not convert into anyhow::Error — that is the `?` that \
             would collapse the CLI/toast split back into one rendering"
        );
    }
}

#[test]
fn callback_ignores_unrelated_requests_and_keeps_waiting() {
    // A favicon probe is answered 404 and is NOT fatal — the login keeps waiting.
    let (verdict, response) = callback_roundtrip("GET /favicon.ico HTTP/1.1\r\n", "STATE");
    assert!(verdict.expect("non-callback path is ignored").is_none());
    assert!(response.starts_with("HTTP/1.1 404"), "got: {response}");

    // /callback with no code at all → 400, still not fatal.
    let (verdict, response) = callback_roundtrip("GET /callback?state=STATE HTTP/1.1\r\n", "STATE");
    assert!(verdict.expect("missing code is ignored").is_none());
    assert!(response.starts_with("HTTP/1.1 400"), "got: {response}");
}

#[test]
fn callback_pages_are_distinct_and_never_reflect_query_values() {
    // Success: styled page that attempts a tab auto-close.
    let (_, ok) = callback_roundtrip("GET /callback?code=c&state=STATE HTTP/1.1\r\n", "STATE");
    assert!(ok.contains("You're logged in"), "got: {ok}");
    assert!(
        ok.contains("window.close"),
        "the success page attempts a tab auto-close"
    );

    // A user-declined consent reads as canceled, not broken — and the
    // attacker-controllable error_description never reaches the HTML.
    let (_, denied) = callback_roundtrip(
        "GET /callback?error=access_denied&error_description=evil-marker HTTP/1.1\r\n",
        "STATE",
    );
    assert!(denied.contains("Login canceled"), "got: {denied}");
    assert!(
        !denied.contains("evil-marker"),
        "untrusted query values must never be reflected into the page"
    );

    // Any other OAuth error keeps the generic failure page, with no auto-close.
    let (_, failed) = callback_roundtrip("GET /callback?error=server_error HTTP/1.1\r\n", "STATE");
    assert!(failed.contains("Login failed"), "got: {failed}");
    assert!(
        !failed.contains("window.close"),
        "only the success page auto-closes"
    );
}

#[test]
fn wait_for_code_times_out_naming_both_doors_and_the_bound() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind loopback");
    // An EMPTY channel (sender still alive): the deadline is what fires, not a
    // disconnected paste door.
    let (_tx, paste) = std::sync::mpsc::channel::<super::ManualCode>();
    let err = wait_for_code(&listener, &paste, "STATE", std::time::Instant::now())
        .expect_err("an already-passed deadline must bail")
        .to_string();
    assert_eq!(
        err,
        "timed out waiting for the login code (browser callback or pasted code, 600s)"
    );
}

/// The `clauth login` summary names the account's plan from the token the login
/// just stored. `login_profile_from_raw` mints `"free"` for a Free account, so
/// this is the round trip that keeps the line reading `Claude Free` rather than
/// echoing a raw wire token or claiming a tier the account never had.
#[test]
fn login_summary_names_the_plan_a_free_login_stored() {
    let creds = crate::profile::ClaudeCredentials {
        claude_ai_oauth: Some(crate::profile::OAuthToken {
            access_token: "at".into(),
            refresh_token: Some("rt".into()),
            expires_at: None,
            scopes: None,
            subscription_type: Some("free".into()),
            ..crate::profile::OAuthToken::default_extra()
        }),
    };
    assert!(
        super::login_summary(&creds, None)
            .contains("  plan: Claude Free (token verified against the API)"),
        "got: {}",
        super::login_summary(&creds, None)
    );
}

fn token_stamped(sub: Option<&str>) -> crate::profile::ClaudeCredentials {
    crate::profile::ClaudeCredentials {
        claude_ai_oauth: Some(crate::profile::OAuthToken {
            access_token: "at".into(),
            refresh_token: Some("rt".into()),
            expires_at: None,
            scopes: None,
            subscription_type: sub.map(str::to_string),
            ..crate::profile::OAuthToken::default_extra()
        }),
    }
}

/// The login's own `/profile` probe knows the plan better than the bare token
/// stamp: a 20x Max login stamps `max` but reads as Ultra.
#[test]
fn login_summary_names_the_plan_the_probe_read_over_the_token_stamp() {
    let plan = crate::usage::PlanInfo {
        tier: crate::usage::PlanTier::Max(Some(20)),
        ..Default::default()
    };
    let summary = super::login_summary(&token_stamped(Some("max")), Some(&plan));
    assert!(
        summary.contains("  plan: Claude Ultra (token verified against the API)"),
        "got: {summary}"
    );
}

/// An Ultra organization mints no token stamp, yet its probe still named the
/// plan, and the server's own name wins there too.
#[test]
fn login_summary_names_a_probed_plan_when_the_token_has_no_stamp() {
    let ultra = crate::usage::PlanInfo {
        tier: crate::usage::PlanTier::Ultra,
        ..Default::default()
    };
    let summary = super::login_summary(&token_stamped(None), Some(&ultra));
    assert!(
        summary.contains("  plan: Claude Ultra (token verified against the API)"),
        "got: {summary}"
    );
    let named = crate::usage::PlanInfo {
        tier: crate::usage::PlanTier::Max(Some(5)),
        plan_display_name: Some("Ultra Lite".to_string()),
        ..Default::default()
    };
    let summary = super::login_summary(&token_stamped(Some("max")), Some(&named));
    assert!(
        summary.contains("  plan: Claude Ultra Lite (token verified against the API)"),
        "got: {summary}"
    );
}

/// A probe that classified nothing is no plan: the token stamp still speaks.
#[test]
fn login_summary_falls_back_to_the_token_stamp_over_an_unclassified_probe() {
    let unknown = crate::usage::PlanInfo::default();
    let summary = super::login_summary(&token_stamped(Some("pro")), Some(&unknown));
    assert!(
        summary.contains("  plan: Claude Pro (token verified against the API)"),
        "got: {summary}"
    );
}

/// No claim on the token is not a failed login: the probe is best-effort and the
/// usage poll re-derives the tier within a cycle, so the line says so instead of
/// naming a tier or reading as an error.
#[test]
fn login_summary_defers_when_the_token_claims_no_plan() {
    let creds = crate::profile::ClaudeCredentials {
        claude_ai_oauth: Some(crate::profile::OAuthToken {
            access_token: "at".into(),
            refresh_token: Some("rt".into()),
            expires_at: None,
            scopes: None,
            subscription_type: None,
            ..crate::profile::OAuthToken::default_extra()
        }),
    };
    let summary = super::login_summary(&creds, None);
    assert!(
        summary.contains("  plan: will populate on the first usage refresh"),
        "got: {summary}"
    );
    assert!(!summary.contains("verified"), "got: {summary}");
}

// ── manual (no browser) flow ─────────────────────────────────────────────────

#[test]
fn authorize_url_takes_the_manual_redirect_and_keeps_code_true() {
    let url = authorize_url(super::MANUAL_REDIRECT_URI, "CHAL", "STATE");
    assert!(url.starts_with("https://claude.com/cai/oauth/authorize?"));
    assert!(url.contains("code=true"));
    assert!(
        url.contains("redirect_uri=https%3A%2F%2Fplatform.claude.com%2Foauth%2Fcode%2Fcallback")
    );
    assert!(url.contains("state=STATE"));
}

/// The query string with `key`'s pair removed, everything else verbatim.
fn query_without(query: &str, key: &str) -> String {
    query
        .split('&')
        .filter(|pair| match pair.split_once('=') {
            Some((k, _)) => k != key,
            None => true,
        })
        .collect::<Vec<_>>()
        .join("&")
}

#[test]
fn begin_login_builds_both_urls_from_one_pair() {
    let pending = super::begin_login().unwrap_or_else(|e| panic!("{}", e.user_message()));
    let links = pending.links();
    let (_, bq) = links.browser_url.split_once('?').expect("browser query");
    let (_, hq) = links.hosted_url.split_once('?').expect("hosted query");

    // ONE `state` and ONE `code_challenge` on both URLs.
    let b_state = query_param(bq, "state").expect("browser state");
    let h_state = query_param(hq, "state").expect("hosted state");
    assert_eq!(b_state, h_state);
    assert_eq!(b_state, links.state);
    assert_eq!(
        query_param(bq, "code_challenge").expect("browser challenge"),
        query_param(hq, "code_challenge").expect("hosted challenge"),
    );

    // The redirect_uri differs: loopback vs the hosted MANUAL page.
    let b_redirect = query_param(bq, "redirect_uri").expect("browser redirect");
    let h_redirect = query_param(hq, "redirect_uri").expect("hosted redirect");
    assert!(
        b_redirect.starts_with("http://localhost:"),
        "got: {b_redirect}"
    );
    assert!(b_redirect.ends_with("/callback"), "got: {b_redirect}");
    assert_eq!(h_redirect, super::MANUAL_REDIRECT_URI);

    // And nothing else differs: drop the redirect_uri pair, the rest is equal.
    assert_eq!(
        query_without(bq, "redirect_uri"),
        query_without(hq, "redirect_uri"),
    );
}

#[test]
fn login_links_parse_accepts_its_own_state_and_refuses_any_other() {
    let links = super::LoginLinks {
        browser_url: "https://example.invalid/loopback".to_string(),
        hosted_url: "https://example.invalid/hosted".to_string(),
        state: "st".to_string(),
    };
    let code = links.parse("code#st").unwrap_or_else(|e| panic!("{e:?}"));
    assert_eq!(code.0, "code");
    assert_eq!(
        links.parse("code#other").unwrap_err(),
        super::ManualCodeError::StateMismatch
    );
}

#[test]
fn parse_manual_code_splits_on_the_hash_and_trims() {
    use super::parse_manual_code;
    assert_eq!(parse_manual_code("abc#st", "st"), Ok("abc".to_string()));
    assert_eq!(parse_manual_code("  abc#st\n", "st"), Ok("abc".to_string()));
    // Only the FIRST `#` splits; a code never carries one, a state never does
    // either, so anything after a second one is the state half and mismatches.
    assert_eq!(
        parse_manual_code("abc#st#x", "st"),
        Err(super::ManualCodeError::StateMismatch)
    );
}

#[test]
fn parse_manual_code_refuses_every_malformed_shape() {
    use super::{MANUAL_CODE_MAX, ManualCodeError, parse_manual_code};
    assert_eq!(parse_manual_code("", "st"), Err(ManualCodeError::Empty));
    assert_eq!(
        parse_manual_code("   \n", "st"),
        Err(ManualCodeError::Empty)
    );
    assert_eq!(
        parse_manual_code("abcst", "st"),
        Err(ManualCodeError::Shape)
    );
    assert_eq!(parse_manual_code("#st", "st"), Err(ManualCodeError::Shape));
    assert_eq!(parse_manual_code("abc#", "st"), Err(ManualCodeError::Shape));
    assert_eq!(
        parse_manual_code("abc#other", "st"),
        Err(ManualCodeError::StateMismatch)
    );
    // The cap applies to the trimmed paste: exactly the cap is accepted even
    // with a newline behind it, one byte over is refused.
    let exact = format!("{}#st", "a".repeat(MANUAL_CODE_MAX - 3));
    assert_eq!(exact.len(), MANUAL_CODE_MAX);
    assert_eq!(
        parse_manual_code(&format!("{exact}\n"), "st").as_deref(),
        Ok("a".repeat(MANUAL_CODE_MAX - 3).as_str())
    );
    let long = format!("{}#st", "a".repeat(MANUAL_CODE_MAX - 2));
    assert_eq!(long.len(), MANUAL_CODE_MAX + 1);
    assert_eq!(
        parse_manual_code(&long, "st"),
        Err(ManualCodeError::TooLong)
    );
}

#[test]
fn manual_code_errors_and_debug_carry_no_pasted_bytes() {
    use super::{ManualCodeError, begin_login};
    let canary = "CANARY-CODE-7f3a";
    for e in [
        ManualCodeError::Empty,
        ManualCodeError::TooLong,
        ManualCodeError::Shape,
        ManualCodeError::StateMismatch,
    ] {
        assert!(!e.message().contains(canary));
    }
    let pending = begin_login().unwrap_or_else(|e| panic!("{}", e.user_message()));
    // `parse` refuses with the typed error, whose canned `message` is the only
    // way a toast or stderr line is built; it cannot echo the paste.
    let err = pending
        .links()
        .parse(&format!("{canary}#wrong-state"))
        .expect_err("state mismatch");
    assert_eq!(err, ManualCodeError::StateMismatch);
    assert!(!err.message().contains(canary));
    let code = pending
        .links()
        .parse(&format!("{canary}#{}", pending.links().state))
        .unwrap_or_else(|e| panic!("{}", e.message()));
    assert_eq!(format!("{code:?}"), "ManualCode(..)");
    assert_eq!(code.as_str(), canary);
}

/// The paste door, end to end through [`PendingLogin::run`] against a loopback
/// stand-in for both hosts: the token request carries the MANUAL redirect, the
/// login's `state`, the verifier behind the URL's challenge, and the parsed code
/// (never the `#state` suffix); progress names the door that won.
#[test]
fn pending_login_run_completes_the_paste_door_against_the_manual_redirect() {
    let home = crate::testutil::HomeSandbox::new();
    let (base, server) = crate::testutil::serve_endpoints_recording(3, |path, _| {
        if path.starts_with("/v1/oauth/token") {
            (
                200,
                r#"{"access_token":"at-manual","refresh_token":"rt-manual","expires_in":28800,"scope":"user:profile user:inference"}"#
                    .to_string(),
            )
        } else {
            (
                200,
                r#"{"account":{"uuid":"uuid-manual","has_claude_max":true},"organization":{"organization_type":"claude_max","rate_limit_tier":"default_claude_max_5x"}}"#
                    .to_string(),
            )
        }
    });
    let _endpoints = crate::testutil::EndpointSandbox::new(&home, &base);

    let pending = super::begin_login().unwrap_or_else(|e| panic!("{}", e.user_message()));
    let state = pending.links().state.clone();
    let (_, query) = pending.links().hosted_url.split_once('?').expect("query");
    let challenge = query_param(query, "code_challenge").expect("challenge");
    let code = pending
        .links()
        .parse(&format!("the-code#{state}"))
        .unwrap_or_else(|e| panic!("{e:?}"));

    let (tx, paste) = std::sync::mpsc::channel();
    tx.send(code).expect("send");
    drop(tx);

    let stages = std::sync::Mutex::new(Vec::new());
    let outcome = pending
        .run(paste, |p| {
            stages.lock().expect("stages").push(format!("{p:?}"))
        })
        .unwrap_or_else(|e| panic!("{}", e.cli_message()));

    let seen = server.join().expect("listener");
    let (_, body) = seen
        .iter()
        .find(|(p, _)| p.starts_with("/v1/oauth/token"))
        .expect("token request");
    let body: serde_json::Value = serde_json::from_str(body).expect("json body");
    assert_eq!(body["grant_type"], "authorization_code");
    assert_eq!(body["code"], "the-code");
    assert_eq!(body["redirect_uri"], super::MANUAL_REDIRECT_URI);
    assert_eq!(body["state"], state);
    assert_eq!(body["client_id"], crate::oauth::CLIENT_ID);
    let verifier = body["code_verifier"].as_str().expect("verifier");
    assert_eq!(challenge_from_verifier(verifier), challenge);
    assert!(
        seen.iter()
            .any(|(p, _)| p.starts_with("/api/oauth/profile"))
    );

    let oauth = outcome.credentials.claude_ai_oauth.expect("oauth block");
    assert_eq!(oauth.access_token, "at-manual");
    assert_eq!(oauth.refresh_token.as_deref(), Some("rt-manual"));
    assert!(
        oauth.subscription_type.is_some(),
        "tier stamped from the probe"
    );
    assert_eq!(
        oauth.rate_limit_tier(),
        Some("default_claude_max_5x"),
        "rate-limit tier stamped from the same probe (#78)"
    );
    assert_eq!(outcome.account_uuid.as_deref(), Some("uuid-manual"));
    assert_eq!(
        outcome.plan.as_ref().map(|p| p.tier.clone()),
        Some(crate::usage::PlanTier::Max(Some(5))),
        "the probe body's plan rides the login outcome for the commit to seed"
    );
    assert_eq!(
        *stages.lock().expect("stages"),
        vec![
            "ExchangingCode(Manual)".to_string(),
            "Verifying".to_string()
        ]
    );
}

/// A probe that fails (an endpoint answering 500) never fails the login — but it
/// proves no account and carries no plan, and a commit built from the outcome
/// seeds nothing.
#[test]
fn a_failed_probe_carries_no_plan_and_seeds_nothing() {
    let home = crate::testutil::HomeSandbox::new();
    let (base, server) = crate::testutil::serve_endpoints_recording(2, |path, _| {
        if path.starts_with("/v1/oauth/token") {
            (
                200,
                r#"{"access_token":"at-failed","refresh_token":"rt-failed","expires_in":28800,"scope":"user:profile user:inference"}"#
                    .to_string(),
            )
        } else {
            (500, "{}".to_string())
        }
    });
    let _endpoints = crate::testutil::EndpointSandbox::new(&home, &base);

    let pending = super::begin_login().unwrap_or_else(|e| panic!("{}", e.user_message()));
    let state = pending.links().state.clone();
    let code = pending
        .links()
        .parse(&format!("the-code#{state}"))
        .unwrap_or_else(|e| panic!("{e:?}"));

    let (tx, paste) = std::sync::mpsc::channel();
    tx.send(code).expect("send");
    drop(tx);

    let outcome = pending
        .run(paste, |_p| {})
        .unwrap_or_else(|e| panic!("{}", e.cli_message()));
    let seen = server.join().expect("listener");
    let paths: Vec<&str> = seen
        .iter()
        .map(|(p, _)| p.split('?').next().unwrap_or(p))
        .collect();
    assert_eq!(
        paths,
        ["/v1/oauth/token", "/api/oauth/profile"],
        "the probe was sent and answered 500"
    );

    assert!(
        outcome.account_uuid.is_none(),
        "a failed probe proves no account"
    );
    assert!(
        outcome.plan.is_none(),
        "a failed probe carries no plan to seed"
    );

    // The commit the login carrier builds from the outcome seeds nothing: no
    // probe body, no seed.
    let snapshot = crate::actions::capture_snapshot_from_outcome(outcome);
    let mut config = crate::profile::AppConfig {
        state: crate::profile::AppState::default(),
        profiles: vec![],
    };
    crate::actions::capture_into_profile(
        &mut config,
        "fresh".to_string(),
        Default::default(),
        snapshot,
    )
    .expect("capture");
    assert!(
        crate::profile_cache::load_profile_cache::<crate::usage::UsageInfo>(
            &crate::profile::ProfileName::from("fresh"),
            crate::profile_cache::USAGE_CACHE_FILE,
        )
        .is_none(),
        "a failed probe's commit seeds nothing"
    );
}

/// The loopback listener's port, read off the authorize URL's `redirect_uri`.
fn loopback_port(browser_url: &str) -> u16 {
    let (_, query) = browser_url.split_once('?').expect("query");
    let redirect = query_param(query, "redirect_uri").expect("redirect_uri");
    assert!(redirect.starts_with("http://localhost:"), "got: {redirect}");
    assert!(redirect.ends_with("/callback"), "got: {redirect}");
    redirect
        .strip_prefix("http://localhost:")
        .expect("localhost prefix")
        .split_once('/')
        .expect("callback suffix")
        .0
        .parse()
        .expect("port")
}

/// The loopback door, end to end through [`PendingLogin::run`]: a thread plays
/// the browser against the port read off `browser_url`, the token request
/// carries the loopback redirect, and progress names the door that won.
#[test]
fn pending_login_run_completes_the_loopback_door_against_its_redirect() {
    let home = crate::testutil::HomeSandbox::new();
    let (base, server) = crate::testutil::serve_endpoints_recording(3, |path, _| {
        if path.starts_with("/v1/oauth/token") {
            (
                200,
                r#"{"access_token":"at-loop","refresh_token":"rt-loop","expires_in":28800,"scope":"user:profile user:inference"}"#
                    .to_string(),
            )
        } else {
            (
                200,
                r#"{"account":{"uuid":"uuid-loop","has_claude_max":true},"organization":{"organization_type":"claude_max"}}"#
                    .to_string(),
            )
        }
    });
    let _endpoints = crate::testutil::EndpointSandbox::new(&home, &base);

    let pending = super::begin_login().unwrap_or_else(|e| panic!("{}", e.user_message()));
    let state = pending.links().state.clone();
    let port = loopback_port(&pending.links().browser_url);

    // An open, EMPTY paste channel: `try_recv` keeps saying Empty and the
    // loopback accept wins.
    let (_tx, paste) = std::sync::mpsc::channel::<super::ManualCode>();
    let stages = std::sync::Mutex::new(Vec::new());
    let browser = std::thread::spawn({
        let request_state = state.clone();
        move || {
            let mut client = std::net::TcpStream::connect(("127.0.0.1", port)).expect("connect");
            client
                .write_all(
                    format!("GET /callback?code=the-code&state={request_state} HTTP/1.1\r\n")
                        .as_bytes(),
                )
                .expect("send callback");
            // Read the response so the server's write lands before this socket
            // closes (the browser always reads it).
            let mut response = String::new();
            let _ = client.read_to_string(&mut response);
        }
    });

    let outcome = pending
        .run(paste, |p| {
            stages.lock().expect("stages").push(format!("{p:?}"))
        })
        .unwrap_or_else(|e| panic!("{}", e.cli_message()));
    browser.join().expect("browser thread");

    let seen = server.join().expect("listener");
    let (_, body) = seen
        .iter()
        .find(|(p, _)| p.starts_with("/v1/oauth/token"))
        .expect("token request");
    let body: serde_json::Value = serde_json::from_str(body).expect("json body");
    assert_eq!(body["code"], "the-code");
    assert_eq!(
        body["redirect_uri"],
        format!("http://localhost:{port}/callback")
    );
    assert_eq!(body["state"], state);
    assert!(
        seen.iter()
            .any(|(p, _)| p.starts_with("/api/oauth/profile"))
    );

    let oauth = outcome.credentials.claude_ai_oauth.expect("oauth block");
    assert_eq!(oauth.access_token, "at-loop");
    assert_eq!(outcome.account_uuid.as_deref(), Some("uuid-loop"));
    assert_eq!(
        *stages.lock().expect("stages"),
        vec![
            "ExchangingCode(Browser)".to_string(),
            "Verifying".to_string()
        ]
    );
}

/// A `Disconnected` paste door must NOT end the wait: dropping the `Sender`
/// closes only the paste door, and the loopback callback still completes the
/// login through the browser door.
#[test]
fn a_dropped_paste_sender_keeps_the_loopback_door_open() {
    let home = crate::testutil::HomeSandbox::new();
    let (base, server) = crate::testutil::serve_endpoints_recording(3, |path, _| {
        if path.starts_with("/v1/oauth/token") {
            (
                200,
                r#"{"access_token":"at-drop","refresh_token":"rt-drop","expires_in":28800,"scope":"user:profile user:inference"}"#
                    .to_string(),
            )
        } else {
            (
                200,
                r#"{"account":{"uuid":"uuid-drop","has_claude_max":true},"organization":{"organization_type":"claude_max"}}"#
                    .to_string(),
            )
        }
    });
    let _endpoints = crate::testutil::EndpointSandbox::new(&home, &base);

    let pending = super::begin_login().unwrap_or_else(|e| panic!("{}", e.user_message()));
    let state = pending.links().state.clone();
    let port = loopback_port(&pending.links().browser_url);

    let (tx, paste) = std::sync::mpsc::channel::<super::ManualCode>();
    drop(tx); // close the paste door before the callback lands

    let stages = std::sync::Mutex::new(Vec::new());
    let browser = std::thread::spawn(move || {
        let mut client = std::net::TcpStream::connect(("127.0.0.1", port)).expect("connect");
        client
            .write_all(format!("GET /callback?code=the-code&state={state} HTTP/1.1\r\n").as_bytes())
            .expect("send callback");
        let mut response = String::new();
        let _ = client.read_to_string(&mut response);
    });

    let outcome = pending
        .run(paste, |p| {
            stages.lock().expect("stages").push(format!("{p:?}"))
        })
        .unwrap_or_else(|e| panic!("{}", e.cli_message()));
    browser.join().expect("browser thread");

    let seen = server.join().expect("listener");
    assert!(
        seen.iter().any(|(p, _)| p.starts_with("/v1/oauth/token")),
        "the token exchange must still run"
    );
    let oauth = outcome.credentials.claude_ai_oauth.expect("oauth block");
    assert_eq!(oauth.access_token, "at-drop");
    assert_eq!(
        *stages.lock().expect("stages"),
        vec![
            "ExchangingCode(Browser)".to_string(),
            "Verifying".to_string()
        ]
    );
}

/// Run `wait_for_code` on its own thread over `listener`, sending its outcome
/// on the returned channel, so a test can bound how long the outcome takes.
fn wait_on_thread(
    listener: TcpListener,
    paste: std::sync::mpsc::Receiver<super::ManualCode>,
) -> std::sync::mpsc::Receiver<anyhow::Result<(String, super::LoginMethod)>> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        let _ = tx.send(wait_for_code(&listener, &paste, "STATE", deadline));
    });
    rx
}

/// A browser preconnect that never sends a request line (a speculative socket
/// opened before the redirect) must not hold the real callback queued behind
/// it: the code lands at once, never after the silent socket's read bound.
#[test]
fn a_silent_preconnect_never_delays_the_queued_callback() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind loopback");
    let addr = listener.local_addr().expect("local addr");
    let _silent = TcpStream::connect(addr).expect("connect the silent socket");
    let mut real = TcpStream::connect(addr).expect("connect the callback");
    real.write_all(b"GET /callback?code=authcode-1&state=STATE HTTP/1.1\r\n\r\n")
        .expect("send the callback");
    let (_tx, paste) = std::sync::mpsc::channel::<super::ManualCode>();
    let outcome = wait_on_thread(listener, paste)
        .recv_timeout(std::time::Duration::from_secs(2))
        .expect("the callback lands while the silent socket is still open");
    assert_eq!(
        outcome.expect("a valid callback"),
        ("authcode-1".to_string(), super::LoginMethod::Browser)
    );
}

/// The paste door stays live while a silent socket is open: a pasted code
/// wins at once rather than waiting out that socket's read bound.
#[test]
fn a_silent_preconnect_never_starves_the_paste_door() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind loopback");
    let addr = listener.local_addr().expect("local addr");
    let _silent = TcpStream::connect(addr).expect("connect the silent socket");
    let (tx, paste) = std::sync::mpsc::channel::<super::ManualCode>();
    let rx = wait_on_thread(listener, paste);
    std::thread::sleep(std::time::Duration::from_millis(300));
    tx.send(super::ManualCode("pasted-1".to_string()))
        .expect("the wait holds the paste door");
    let outcome = rx
        .recv_timeout(std::time::Duration::from_secs(2))
        .expect("the paste lands while the silent socket is still open");
    assert_eq!(
        outcome.expect("a pasted code"),
        ("pasted-1".to_string(), super::LoginMethod::Manual)
    );
}

/// A state mismatch is a security stop wherever it arrives: one queued behind
/// a silent socket still ends the wait with the mismatch error, never an
/// ignored request.
#[test]
fn a_state_mismatch_behind_a_silent_preconnect_still_stops_the_wait() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind loopback");
    let addr = listener.local_addr().expect("local addr");
    let _silent = TcpStream::connect(addr).expect("connect the silent socket");
    let mut evil = TcpStream::connect(addr).expect("connect the callback");
    evil.write_all(b"GET /callback?code=authcode-1&state=EVIL HTTP/1.1\r\n\r\n")
        .expect("send the callback");
    let (_tx, paste) = std::sync::mpsc::channel::<super::ManualCode>();
    let outcome = wait_on_thread(listener, paste)
        .recv_timeout(std::time::Duration::from_secs(20))
        .expect("the wait ends");
    let err = outcome.expect_err("a state mismatch stops the login");
    let (direct, _) = callback_roundtrip(
        "GET /callback?code=authcode-1&state=EVIL HTTP/1.1\r\n",
        "STATE",
    );
    assert_eq!(
        err.to_string(),
        direct.expect_err("the same request alone").to_string(),
        "the wait ends on handle_callback's own mismatch error"
    );
}

/// A request line that arrives slower than the old 10 s read bound still yields
/// the code now that the bound is 60 s. Scaled: the seam shrinks the bound to
/// 600 ms and the request line lands at 300 ms — past the old bound's scaled
/// form, inside the new one — so the code is not lost.
#[test]
fn a_request_line_past_the_old_read_bound_still_yields_the_code() {
    let _home = crate::testutil::HomeSandbox::new();
    super::set_callback_read_timeout_override(Some(std::time::Duration::from_millis(600)));
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind loopback");
    let addr = listener.local_addr().expect("local addr");
    let (_tx, paste) = std::sync::mpsc::channel::<super::ManualCode>();
    let outcome = wait_on_thread(listener, paste);
    let mut client = TcpStream::connect(addr).expect("connect");
    std::thread::sleep(std::time::Duration::from_millis(300));
    client
        .write_all(b"GET /callback?code=slow-code&state=STATE HTTP/1.1\r\n\r\n")
        .expect("send the slow callback");
    let code = outcome
        .recv_timeout(std::time::Duration::from_secs(2))
        .expect("the slow callback lands")
        .expect("a valid callback");
    assert_eq!(code, ("slow-code".to_string(), super::LoginMethod::Browser));
    super::set_callback_read_timeout_override(None);
}

/// The bound is cloudy's ruled number, pinned by equality: the scaled tests
/// above pin the SHAPE, this one pins the 60 s the ruling names, so retuning the
/// constant cannot ship green.
#[test]
fn the_callback_connection_bound_is_the_ruled_sixty_seconds() {
    assert_eq!(
        super::CALLBACK_READ_TIMEOUT,
        std::time::Duration::from_secs(60),
        "cloudy's 2026-10-21 ruling: one accepted callback connection gets 60 s"
    );
}

/// A peer trickling one byte just under any per-read bound must still end at
/// the connection's ONE deadline. On a per-read timeout it would hold its
/// reader (and socket) for as long as it kept dribbling; here the socket closes
/// at the scaled deadline and the client sees EOF.
#[test]
fn a_trickling_peer_cannot_outlive_the_whole_connection_deadline() {
    use std::io::Read as _;

    let _home = crate::testutil::HomeSandbox::new();
    super::set_callback_read_timeout_override(Some(std::time::Duration::from_millis(400)));
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind loopback");
    let addr = listener.local_addr().expect("local addr");
    let (paste_tx, paste) = std::sync::mpsc::channel::<super::ManualCode>();
    let outcome = wait_on_thread(listener, paste);

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
    // EOF, or the RST Linux sends when a close finds bytes the reader never
    // consumed (the trickle continues past the deadline). Either proves the
    // server ended the connection; a per-read timeout leaves it open and this
    // times out instead.
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
    paste_tx
        .send(super::ManualCode("cleanup".to_string()))
        .expect("the wait still holds the paste door");
    let _ = outcome
        .recv_timeout(std::time::Duration::from_secs(2))
        .expect("the paste ends the wait");
}

/// Past the live-reader cap a new connection is dropped, never spawned: a local
/// flood costs connections, not threads. The `thread::Builder` refusal path is
/// the same shape (the OS refusing a thread), so a refused spawn also just
/// closes the socket.
#[test]
fn a_callback_connection_past_the_reader_cap_is_dropped() {
    use std::io::Read as _;

    let _home = crate::testutil::HomeSandbox::new();
    // Long enough that the first reader still holds its slot when the second
    // connects.
    super::set_callback_read_timeout_override(Some(std::time::Duration::from_secs(2)));
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind loopback");
    let addr = listener.local_addr().expect("local addr");
    let (paste_tx, paste) = std::sync::mpsc::channel::<super::ManualCode>();

    // The accept loop (and so `spawn_callback_reader`) runs on the thread that
    // enters `wait_for_code` — this one — because the cap override is
    // thread-local (a process-wide tiny cap would refuse a sibling test's
    // reader). The browser side runs on a helper thread.
    let (verdict_tx, verdict_rx) = std::sync::mpsc::channel();
    let browser = std::thread::spawn(move || {
        // The first silent socket fills the one slot; give the accept loop a
        // poll to take it before the second arrives.
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
        paste_tx
            .send(super::ManualCode("cleanup".to_string()))
            .expect("the wait still holds the paste door");
    });

    super::set_callback_reader_cap_override(Some(1));
    let outcome = wait_for_code(
        &listener,
        &paste,
        "STATE",
        std::time::Instant::now() + std::time::Duration::from_secs(30),
    );
    super::set_callback_reader_cap_override(None);
    assert_eq!(
        outcome.expect("the pasted cleanup code ends the wait").0,
        "cleanup"
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

/// The cap override carries its OWN live-reader counter, so a reader from
/// ANOTHER thread holding the process-wide count up does not change which
/// connection the override's cap refuses: the first fills the override's one
/// slot (the process-wide slot is invisible to it) and the second is refused.
#[test]
fn a_sibling_process_wide_reader_does_not_shift_the_cap_refusal() {
    use std::io::Read as _;

    let _home = crate::testutil::HomeSandbox::new();
    super::set_callback_read_timeout_override(Some(std::time::Duration::from_secs(2)));
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind loopback");
    let addr = listener.local_addr().expect("local addr");
    let (paste_tx, paste) = std::sync::mpsc::channel::<super::ManualCode>();

    // Hold a PROCESS-WIDE slot live for the whole cap test. Acquired on THIS
    // thread before the cap override is set (the override is thread-local, so
    // the slot counts against the process-wide counter, not the override's),
    // and released only after the wait ends. The override carries its own
    // counter, so this held slot must not change which connection it refuses.
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    super::spawn_callback_reader(move || {
        let _ = release_rx.recv();
    });

    // The accept loop (and so `spawn_callback_reader`) runs on the thread that
    // enters `wait_for_code` — this one. The browser side runs on a helper
    // thread and observes BOTH connections' verdicts.
    let (verdict_tx, verdict_rx) =
        std::sync::mpsc::channel::<(std::io::Result<usize>, std::io::Result<usize>)>();
    let browser = std::thread::spawn(move || {
        // The first silent socket fills the override's one slot: its read then
        // blocks (accepted), so a short read timeout observes "open", never
        // EOF. A refused connection would return EOF at once instead.
        let mut first = TcpStream::connect(addr).expect("connect the first");
        first
            .set_read_timeout(Some(std::time::Duration::from_millis(200)))
            .expect("first read timeout");
        let mut byte = [0u8; 1];
        let first_verdict = first.read(&mut byte);
        // The first reader now holds its slot for the whole 2 s deadline, so
        // the second connection must be the one the override's cap refuses.
        std::thread::sleep(std::time::Duration::from_millis(300));
        let mut second = TcpStream::connect(addr).expect("connect the second");
        // A client bound well under the server's 2 s deadline: under the cap the
        // read sees EOF at once; a cap bypass holds the reader to its 2 s
        // deadline and this times out instead (two equal timers would race).
        second
            .set_read_timeout(Some(std::time::Duration::from_millis(500)))
            .expect("read timeout");
        let second_verdict = second.read(&mut byte);
        let _ = verdict_tx.send((first_verdict, second_verdict));
        paste_tx
            .send(super::ManualCode("cleanup".to_string()))
            .expect("the wait still holds the paste door");
    });

    super::set_callback_reader_cap_override(Some(1));
    let outcome = wait_for_code(
        &listener,
        &paste,
        "STATE",
        std::time::Instant::now() + std::time::Duration::from_secs(30),
    );
    super::set_callback_reader_cap_override(None);
    assert_eq!(
        outcome.expect("the pasted cleanup code ends the wait").0,
        "cleanup"
    );
    let (first_verdict, second_verdict) = verdict_rx
        .recv_timeout(std::time::Duration::from_secs(2))
        .expect("the browser reports both verdicts");
    assert!(
        first_verdict.is_err(),
        "the first connection is ACCEPTED (its silent read times out), not refused by the held process-wide slot: {first_verdict:?}"
    );
    assert_eq!(
        second_verdict.expect("the refused connection is closed"),
        0,
        "the override's cap still refuses the SECOND connection"
    );
    browser.join().expect("browser thread");
    release_tx.send(()).expect("release the held slot");
}

/// A reader that finishes after the wait already ended writes NO page of any
/// kind — here a state-mismatch request arriving late: the login is over, so
/// even the security page would lie. The connection just closes.
#[test]
fn a_reader_finishing_after_the_wait_ended_writes_no_page() {
    use std::io::Read as _;

    let _home = crate::testutil::HomeSandbox::new();
    super::set_callback_read_timeout_override(Some(std::time::Duration::from_secs(5)));
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind loopback");
    let addr = listener.local_addr().expect("local addr");
    let (_tx, paste) = std::sync::mpsc::channel::<super::ManualCode>();
    let outcome = wait_on_thread(listener, paste);

    // Accepted first, and silent: its reader is still waiting when the login ends.
    let mut late = TcpStream::connect(addr).expect("connect the late callback");
    // Set the timeout right after connect, before any write or peer drop
    // (test-infrastructure.md RULE).
    late.set_read_timeout(Some(std::time::Duration::from_secs(2)))
        .expect("read timeout");
    std::thread::sleep(std::time::Duration::from_millis(300));
    let mut evil = TcpStream::connect(addr).expect("connect the mismatch");
    evil.write_all(b"GET /callback?code=x&state=EVIL HTTP/1.1\r\n\r\n")
        .expect("send the mismatch");
    let err = outcome
        .recv_timeout(std::time::Duration::from_secs(2))
        .expect("the mismatch ends the wait")
        .expect_err("a state mismatch stops the login");
    assert!(
        err.to_string().contains("state mismatch"),
        "the wait ended on the security stop: {err}"
    );

    // A non-success request arrives after the wait returned: no page of any
    // kind is written, the connection just closes.
    late.write_all(b"GET /callback?code=late-code&state=EVIL HTTP/1.1\r\n\r\n")
        .expect("send the late callback");
    let mut response = String::new();
    let _ = late.read_to_string(&mut response);
    assert_eq!(
        response, "",
        "an already-ended login writes no page of any kind"
    );
}
