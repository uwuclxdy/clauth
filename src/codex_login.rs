//! `clauth login <name> --codex --browser` — codex's own OAuth PKCE flow,
//! reimplemented rather than shelled out to `codex login` (settled question 7).
//!
//! Shelling out would write the operator's LIVE `~/.codex/auth.json` and fork
//! the chain — the very two-carrier death the whole codex design avoids. So
//! clauth runs the loopback dance itself and lands the minted chain straight
//! into the profile store, touching nothing of the operator's.
//!
//! Every wire fact here was read from openai/codex at tag `rust-v0.145.0`
//! (`login/src/server.rs`), the same source the spec pins:
//! - the loopback ports are FIXED at 1455, fallback 1457, path
//!   `/auth/callback` — the registered redirect set, not an ephemeral port
//!   (an ephemeral port would not match a registered redirect_uri);
//! - the code exchange is `application/x-www-form-urlencoded` while the
//!   refresh at the same endpoint is JSON (the encoding trap);
//! - `auth_mode: "chatgpt"` is written explicitly — codex's `resolved_mode()`
//!   infers ApiKey from a bare `OPENAI_API_KEY`, so omitting it mislabels the
//!   account;
//! - `tokens.account_id` comes from the id_token's `chatgpt_account_id` claim,
//!   nested under the `https://api.openai.com/auth` object;
//! - the RFC-8693 secondary exchange that mints `OPENAI_API_KEY` is
//!   best-effort — its failure must not fail the login.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use crate::codex_auth::{CODEX_CLIENT_ID, CODEX_TOKEN_URL};
use crate::logline::logline;
use crate::oauth_login::{base64url_nopad, percent_encode, query_param};

/// codex's registered loopback ports and callback path — a redirect_uri the
/// server will accept, not a free port.
const PRIMARY_PORT: u16 = 1455;
const FALLBACK_PORT: u16 = 1457;
const CALLBACK_PATH: &str = "/auth/callback";
const AUTHORIZE_URL: &str = "https://auth.openai.com/oauth/authorize";
/// codex's originator header/param value.
const ORIGINATOR: &str = "codex_cli_rs";
/// codex's verified scope set.
const SCOPE: &str = "openid profile email offline_access api.connectors.read api.connectors.invoke";

const LOGIN_TIMEOUT: Duration = Duration::from_secs(300);

/// The minted login, ready to land in a profile store.
pub(crate) struct CodexLoginOutcome {
    /// The full `auth.json` bytes clauth will write — `auth_mode`, the token
    /// chain, `last_refresh`, and `OPENAI_API_KEY` when the secondary exchange
    /// succeeded.
    pub(crate) auth_json: Vec<u8>,
    /// The ChatGPT account id, for the success line.
    pub(crate) account_id: Option<String>,
}

/// Run the loopback PKCE flow. `announce` receives the authorize URL (the CLI
/// prints it and opens a browser). Blocks until the callback lands or the
/// timeout elapses.
pub(crate) fn login_with(announce: impl Fn(&str)) -> Result<CodexLoginOutcome> {
    let (verifier, challenge) = new_pkce()?;
    let state = random_b64url(32)?;

    // The redirect_uri must be one codex registered, so we bind its ports
    // rather than an ephemeral one — primary, then the single fallback.
    let (listener, port) = bind_registered_port()?;
    let redirect_uri = format!("http://localhost:{port}{CALLBACK_PATH}");
    let url = authorize_url(&redirect_uri, &challenge, &state);

    announce(&url);
    let _ = crate::platform::open_url(&url);

    let code = wait_for_code(&listener, &state)?;
    let tokens = exchange_code(&code, &verifier, &redirect_uri)?;
    Ok(build_auth_json(tokens))
}

fn bind_registered_port() -> Result<(TcpListener, u16)> {
    for port in [PRIMARY_PORT, FALLBACK_PORT] {
        if let Ok(l) = TcpListener::bind(("127.0.0.1", port)) {
            return Ok((l, port));
        }
    }
    bail!(
        "codex's login ports ({PRIMARY_PORT} and {FALLBACK_PORT}) are both in use — \
         close whatever holds them (another codex or clauth login?) and retry"
    )
}

/// `n` CSPRNG bytes, base64url. Reuses the claude login's encoder so the two
/// share one alphabet.
fn random_b64url(n: usize) -> Result<String> {
    let mut buf = vec![0u8; n];
    getrandom::fill(&mut buf).map_err(|e| anyhow::anyhow!("CSPRNG failed: {e}"))?;
    Ok(base64url_nopad(&buf))
}

fn new_pkce() -> Result<(String, String)> {
    let verifier = random_b64url(32)?;
    use sha2::Digest as _;
    let mut hasher = sha2::Sha256::new();
    hasher.update(verifier.as_bytes());
    let challenge = base64url_nopad(&hasher.finalize());
    Ok((verifier, challenge))
}

fn authorize_url(redirect_uri: &str, challenge: &str, state: &str) -> String {
    let params = [
        ("response_type", "code"),
        ("client_id", CODEX_CLIENT_ID),
        ("redirect_uri", redirect_uri),
        ("scope", SCOPE),
        ("code_challenge", challenge),
        ("code_challenge_method", "S256"),
        ("id_token_add_organizations", "true"),
        ("codex_cli_simplified_flow", "true"),
        ("state", state),
        ("originator", ORIGINATOR),
    ];
    let qs = params
        .iter()
        .map(|(k, v)| format!("{k}={}", percent_encode(v)))
        .collect::<Vec<_>>()
        .join("&");
    format!("{AUTHORIZE_URL}?{qs}")
}

/// Block on the loopback socket for the OAuth redirect, validating `state`.
fn wait_for_code(listener: &TcpListener, expected_state: &str) -> Result<String> {
    listener
        .set_nonblocking(true)
        .context("failed to set the callback listener non-blocking")?;
    let deadline = Instant::now() + LOGIN_TIMEOUT;
    loop {
        if Instant::now() >= deadline {
            bail!("timed out waiting for the codex login callback");
        }
        match listener.accept() {
            Ok((stream, _)) => {
                if let Some(code) = handle_callback(stream, expected_state)? {
                    return Ok(code);
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => return Err(e).context("callback listener accept failed"),
        }
    }
}

fn handle_callback(
    mut stream: std::net::TcpStream,
    expected_state: &str,
) -> Result<Option<String>> {
    // The accepted socket inherits the listener's O_NONBLOCK on some
    // platforms (macOS), so a single `read` could return WouldBlock and drop
    // the code on the floor. Force blocking with a read deadline: the browser
    // has already connected, so the request is milliseconds away, and the
    // deadline keeps a half-open connection from hanging the login. Read in a
    // short loop so a redirect split across packets still arrives whole.
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                // The request line plus the query is all we need; stop once
                // the header block is complete or the buffer is generous.
                if buf.windows(4).any(|w| w == b"\r\n\r\n") || buf.len() >= 16384 {
                    break;
                }
            }
            Err(_) => break,
        }
    }
    let request = String::from_utf8_lossy(&buf);
    let target = request
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or("");
    if !target.starts_with(CALLBACK_PATH) {
        // Some other path (a favicon probe) — not the redirect.
        write_reply(&mut stream, "waiting for the codex login…");
        return Ok(None);
    }
    let query = target.split_once('?').map(|(_, q)| q).unwrap_or("");
    let state = query_param(query, "state").unwrap_or_default();
    if state != expected_state {
        write_reply(&mut stream, "login state mismatch — close this and retry");
        bail!("codex login state mismatch (possible CSRF); aborted");
    }
    match query_param(query, "code") {
        Some(code) if !code.is_empty() => {
            write_reply(
                &mut stream,
                "codex login complete — return to your terminal.",
            );
            Ok(Some(code))
        }
        _ => {
            let err = query_param(query, "error").unwrap_or_else(|| "no code".into());
            write_reply(&mut stream, "codex login failed — return to your terminal");
            bail!("codex login callback carried no code: {err}");
        }
    }
}

fn write_reply(stream: &mut std::net::TcpStream, body: &str) {
    let page = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(page.as_bytes());
}

/// codex's token response — three fields, and the code exchange returns all
/// three (unlike a refresh, which may omit id_token).
#[derive(serde::Deserialize)]
struct CodeExchange {
    id_token: String,
    access_token: String,
    refresh_token: String,
}

/// The authorization-code exchange BODY — `application/x-www-form-urlencoded`,
/// the encoding that differs from the JSON refresh at the same endpoint (the
/// trap the spec pins). Split out so the encoding is a unit-testable value.
fn code_exchange_body(code: &str, verifier: &str, redirect_uri: &str) -> String {
    format!(
        "grant_type=authorization_code&code={}&redirect_uri={}&client_id={}&code_verifier={}",
        percent_encode(code),
        percent_encode(redirect_uri),
        percent_encode(CODEX_CLIENT_ID),
        percent_encode(verifier),
    )
}

/// The authorization-code exchange: `application/x-www-form-urlencoded`, the
/// encoding that differs from the JSON refresh at the same endpoint.
fn exchange_code(code: &str, verifier: &str, redirect_uri: &str) -> Result<CodeExchange> {
    let body = code_exchange_body(code, verifier, redirect_uri);
    let mut resp = crate::oauth::http_agent()
        .post(CODEX_TOKEN_URL)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .send(body)
        .context("codex token exchange transport failed")?;
    let status = resp.status().as_u16();
    let text = resp.body_mut().read_to_string().unwrap_or_default();
    if !(200..300).contains(&status) {
        bail!("codex token exchange returned HTTP {status}");
    }
    serde_json::from_str(&text).context("codex token exchange response did not parse")
}

/// Assemble the `auth.json` codex expects, performing the best-effort api-key
/// exchange. The pure assembly is [`assemble_auth_json`]; this is the one
/// place the RFC-8693 network call happens.
fn build_auth_json(tok: CodeExchange) -> CodexLoginOutcome {
    let api_key = exchange_api_key(&tok.id_token).unwrap_or(None);
    assemble_auth_json(tok, api_key)
}

/// The pure assembly (no network): `auth_mode` explicit, the chain,
/// `last_refresh` now, the account id off the id_token, and the api key when
/// one was minted. Split from the network exchange so the shape is unit-tested
/// hermetically.
fn assemble_auth_json(tok: CodeExchange, api_key: Option<String>) -> CodexLoginOutcome {
    let account_id = chatgpt_account_id(&tok.id_token);
    let mut tokens = serde_json::json!({
        "id_token": tok.id_token,
        "access_token": tok.access_token,
        "refresh_token": tok.refresh_token,
    });
    if let Some(acc) = &account_id {
        tokens["account_id"] = serde_json::json!(acc);
    }
    let mut auth = serde_json::json!({
        "auth_mode": "chatgpt",
        "tokens": tokens,
        "last_refresh": now_rfc3339(),
    });
    if let Some(key) = api_key {
        auth["OPENAI_API_KEY"] = serde_json::json!(key);
    }
    CodexLoginOutcome {
        auth_json: serde_json::to_vec(&auth).unwrap_or_default(),
        account_id,
    }
}

/// The `chatgpt_account_id` claim, nested under the id_token's
/// `https://api.openai.com/auth` object (verified against
/// `success_page::jwt_auth_claims`).
fn chatgpt_account_id(id_token: &str) -> Option<String> {
    crate::codex_auth::jwt_payload(id_token)?
        .get("https://api.openai.com/auth")?
        .get("chatgpt_account_id")?
        .as_str()
        .map(str::to_string)
}

/// The RFC-8693 token-exchange that mints an `OPENAI_API_KEY` from the
/// id_token. Best-effort by contract: `Ok(None)` on any non-success, so a
/// login that only wants the ChatGPT chain still completes.
fn exchange_api_key(id_token: &str) -> Result<Option<String>> {
    let body = format!(
        "grant_type={}&client_id={}&requested_token={}&subject_token={}&subject_token_type={}",
        percent_encode("urn:ietf:params:oauth:grant-type:token-exchange"),
        percent_encode(CODEX_CLIENT_ID),
        percent_encode("openai-api-key"),
        percent_encode(id_token),
        percent_encode("urn:ietf:params:oauth:token-type:id_token"),
    );
    let mut resp = match crate::oauth::http_agent()
        .post(CODEX_TOKEN_URL)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .send(body)
    {
        Ok(r) => r,
        Err(e) => {
            logline!("clauth: codex api-key exchange skipped (transport): {e}");
            return Ok(None);
        }
    };
    if !(200..300).contains(&resp.status().as_u16()) {
        return Ok(None);
    }
    let text = resp.body_mut().read_to_string().unwrap_or_default();
    let value: serde_json::Value = serde_json::from_str(&text).unwrap_or_default();
    Ok(value
        .get("access_token")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string))
}

fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339()
}

#[cfg(test)]
#[path = "../tests/inline/codex_login.rs"]
mod tests;
