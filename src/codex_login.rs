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

use std::net::TcpListener;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use crate::codex_auth::{CODEX_CLIENT_ID, CODEX_TOKEN_URL};
use crate::login_page::{self, Id};
use crate::logline::logline;
use crate::oauth_login::{
    AuthorizeRejection, MAX_CALLBACK_HEAD_BYTES, WaitOver, base64url_nopad, percent_encode,
    query_param, read_callback_head, spawn_callback_reader,
};

/// codex's registered loopback ports and callback path — a redirect_uri the
/// server will accept, not a free port.
const PRIMARY_PORT: u16 = 1455;
const FALLBACK_PORT: u16 = 1457;
/// codex's registered redirect ports in bind order — the one const both arms of
/// [`bind_registered_port`] read, so a swapped order or a dropped fallback reds
/// the pin instead of shipping green from a `#[cfg]` arm no test compiles.
const REGISTERED_PORTS: [u16; 2] = [PRIMARY_PORT, FALLBACK_PORT];
const CALLBACK_PATH: &str = "/auth/callback";
const AUTHORIZE_URL: &str = "https://auth.openai.com/oauth/authorize";
/// codex's originator header/param value.
const ORIGINATOR: &str = "codex_cli_rs";
/// codex's verified scope set.
const SCOPE: &str = "openid profile email offline_access api.connectors.read api.connectors.invoke";

const LOGIN_TIMEOUT: Duration = Duration::from_secs(300);

/// The WHOLE deadline one accepted callback connection carries for delivering
/// its request line, armed when the listener accepts it and never reset by a
/// byte. Raised from the old serial accept's 5 s read bound to 60 s per
/// cloudy's ruling: a slow request line still yields the code, while a silent
/// or trickling socket ends at the deadline. A per-read timeout alone would let
/// a peer sending one byte just under the bound hold its reader for days, so
/// [`read_callback_head`] re-arms each read to the time REMAINING to this
/// deadline (the `daemon::bridge::REQUEST_DEADLINE` shape).
const CALLBACK_READ_TIMEOUT: Duration = Duration::from_secs(60);

/// The whole-connection bound [`wait_for_code`] arms at accept:
/// [`CALLBACK_READ_TIMEOUT`], or a shorter value a test poses a slow or
/// trickling request line under. The one source, so the listener and its tests
/// read the same number; production never sets the override.
pub(crate) fn callback_read_timeout() -> Duration {
    #[cfg(test)]
    if let Some(t) = *CALLBACK_READ_TIMEOUT_OVERRIDE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
    {
        return t;
    }
    CALLBACK_READ_TIMEOUT
}

/// Test seam shortening the callback connection deadline so a slow or trickling
/// request line is drivable in milliseconds, the `lock::STATE_LOCK_TIMEOUT_OVERRIDE`
/// shape. `None` is the production deadline. `HomeSandbox` also clears it.
#[cfg(test)]
static CALLBACK_READ_TIMEOUT_OVERRIDE: std::sync::Mutex<Option<Duration>> =
    std::sync::Mutex::new(None);

/// Set or clear the test-only connection-deadline override. `None` restores
/// [`CALLBACK_READ_TIMEOUT`].
#[cfg(test)]
pub(crate) fn set_callback_read_timeout_override(timeout: Option<Duration>) {
    *CALLBACK_READ_TIMEOUT_OVERRIDE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = timeout;
}

/// The minted login, ready to land in a profile store.
#[derive(Clone)]
pub(crate) struct CodexLoginOutcome {
    /// The full `auth.json` bytes clauth will write — `auth_mode`, the token
    /// chain, `last_refresh`, and `OPENAI_API_KEY` when the secondary exchange
    /// succeeded.
    pub(crate) auth_json: Vec<u8>,
    /// The ChatGPT account id, for the success line.
    pub(crate) account_id: Option<String>,
}

/// The token chain is a bearer-grade secret, so the derived form would print
/// it; redact the bytes while keeping the account id (the `LoginSession`
/// precedent keeps the whole struct off `Debug`, but a staged draft variant
/// needs one).
impl std::fmt::Debug for CodexLoginOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CodexLoginOutcome")
            .field("auth_json", &"<redacted>")
            .field("account_id", &self.account_id)
            .finish()
    }
}

/// A cancel handle for a running browser login: `cancel` stops the callback
/// wait — the listener drops, freeing the registered ports — and the worker
/// returns the `login canceled` sentence instead of waiting out the 300 s
/// timeout. `Clone` shares one flag between the UI and the worker.
#[derive(Clone)]
pub(crate) struct LoginCancel(std::sync::Arc<std::sync::atomic::AtomicBool>);

impl LoginCancel {
    pub(crate) fn new() -> Self {
        Self(std::sync::Arc::new(std::sync::atomic::AtomicBool::new(
            false,
        )))
    }

    pub(crate) fn cancel(&self) {
        self.0.store(true, std::sync::atomic::Ordering::Release);
    }

    pub(crate) fn is_canceled(&self) -> bool {
        self.0.load(std::sync::atomic::Ordering::Acquire)
    }
}

/// `#[cfg(test)]`: the loopback port pair a login binds instead of the
/// registered 1455/1457, so a test drives the flow on an ephemeral port.
#[cfg(test)]
static LOGIN_PORTS_OVERRIDE: std::sync::Mutex<Option<[u16; 2]>> = std::sync::Mutex::new(None);

/// `#[cfg(test)]`: the browser-open step, overridden to a no-op so a test never
/// launches a browser.
#[cfg(test)]
static URL_OPENER_OVERRIDE: std::sync::Mutex<Option<fn(&str)>> = std::sync::Mutex::new(None);

/// Test-only RAII: drive [`login_with_cancel`] on the given loopback ports with
/// the given browser opener (a no-op in tests), restoring both on drop even
/// under a panic — the `EndpointSandbox` shape, so no override can leak into a
/// sibling test.
#[cfg(test)]
pub(crate) struct CodexLoginSeam;

#[cfg(test)]
impl CodexLoginSeam {
    pub(crate) fn new(ports: [u16; 2], opener: fn(&str)) -> Self {
        if let Ok(mut guard) = LOGIN_PORTS_OVERRIDE.lock() {
            *guard = Some(ports);
        }
        if let Ok(mut guard) = URL_OPENER_OVERRIDE.lock() {
            *guard = Some(opener);
        }
        Self
    }
}

#[cfg(test)]
impl Drop for CodexLoginSeam {
    fn drop(&mut self) {
        if let Ok(mut guard) = LOGIN_PORTS_OVERRIDE.lock() {
            *guard = None;
        }
        if let Ok(mut guard) = URL_OPENER_OVERRIDE.lock() {
            *guard = None;
        }
    }
}

/// Run the loopback PKCE flow. `announce` receives the authorize URL (the CLI
/// prints it and opens a browser). Blocks until the callback lands or the
/// timeout elapses.
pub(crate) fn login_with(announce: impl Fn(&str)) -> Result<CodexLoginOutcome> {
    login_with_cancel(announce, &LoginCancel::new())
}

/// [`login_with`], honouring a cancel: the callback wait checks the flag each
/// 50 ms iteration and bails the moment it is set, dropping the listener so the
/// registered ports free immediately. The TUI re-login's worker passes the
/// handle its session holds; the CLI's never-cancelled one lives for this call
/// alone.
pub(crate) fn login_with_cancel(
    announce: impl Fn(&str),
    cancel: &LoginCancel,
) -> Result<CodexLoginOutcome> {
    let (verifier, challenge) = new_pkce()?;
    let state = random_b64url(32)?;

    // The redirect_uri must be one codex registered, so we bind its ports
    // rather than an ephemeral one — primary, then the single fallback.
    let (listener, port) = bind_registered_port()?;
    let redirect_uri = format!("http://localhost:{port}{CALLBACK_PATH}");
    let url = authorize_url(&redirect_uri, &challenge, &state);

    announce(&url);
    open_authorize_url(&url);

    let code = wait_for_code(&listener, &state, cancel)?;
    let tokens = exchange_code(&code, &verifier, &redirect_uri)?;
    Ok(build_auth_json(tokens))
}

/// Open the authorize URL in the operator's browser, or the test's no-op when
/// [`CodexLoginSeam`] armed one — a login under test never launches a browser.
fn open_authorize_url(url: &str) {
    #[cfg(test)]
    if let Some(opener) = URL_OPENER_OVERRIDE.lock().ok().and_then(|guard| *guard) {
        opener(url);
        return;
    }
    let _ = crate::platform::open_url(url);
}

fn bind_registered_port() -> Result<(TcpListener, u16)> {
    let ports = {
        #[cfg(test)]
        {
            LOGIN_PORTS_OVERRIDE
                .lock()
                .ok()
                .and_then(|guard| *guard)
                .unwrap_or(REGISTERED_PORTS)
        }
        #[cfg(not(test))]
        {
            REGISTERED_PORTS
        }
    };
    bind_port_pair(ports)
}

/// Try each of `ports` in order; the first that binds wins. The port actually
/// bound is returned (a `0` binds an ephemeral port, so the redirect_uri names
/// that, not the requested `0`). Refuses once both are held. Split out of
/// [`bind_registered_port`] so a test drives the order on its own ephemeral
/// ports without touching the process-global override.
fn bind_port_pair(ports: [u16; 2]) -> Result<(TcpListener, u16)> {
    for port in ports {
        if let Ok(l) = TcpListener::bind(("127.0.0.1", port)) {
            let bound = l.local_addr().map(|a| a.port()).unwrap_or(port);
            return Ok((l, bound));
        }
    }
    bail!(
        "codex's login ports ({PRIMARY_PORT} and {FALLBACK_PORT}) are both in use. \
         Close whatever holds them (another codex or clauth login?) and retry"
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
/// Every accepted connection is read on its own reader thread under one
/// whole-connection [`callback_read_timeout`] deadline armed at accept, so a
/// silent or trickling socket never blocks the accept loop and the cancel flag
/// is seen within one poll whatever a reader is doing; the first reader's valid
/// code wins, and a reader's `Err` ends the wait. Bails `login canceled` as soon
/// as `cancel` is set, so an abandoned login never holds codex's registered
/// ports for the rest of its timeout.
fn wait_for_code(
    listener: &TcpListener,
    expected_state: &str,
    cancel: &LoginCancel,
) -> Result<String> {
    listener
        .set_nonblocking(true)
        .context("failed to set the callback listener non-blocking")?;
    let deadline = Instant::now() + LOGIN_TIMEOUT;
    let (tx, rx) = std::sync::mpsc::channel::<Result<Option<String>>>();
    let (wait_over, _wait_over_guard) = WaitOver::new();
    loop {
        if cancel.is_canceled() {
            bail!("login canceled");
        }
        if Instant::now() >= deadline {
            bail!("timed out waiting for the codex login callback");
        }
        // A reader thread's outcome; `tx` stays alive in this loop, so the
        // channel never disconnects here.
        match rx.try_recv() {
            Ok(Ok(Some(code))) => return Ok(code),
            Ok(Ok(None)) => {}
            Ok(Err(e)) => return Err(e),
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {}
        }
        match listener.accept() {
            Ok((stream, _)) => {
                let tx = tx.clone();
                let state = expected_state.to_string();
                let over = wait_over.clone();
                // Fixed at ACCEPT: the reader's budget is this connection's,
                // never re-armed by a later byte.
                let connection_deadline = Instant::now() + callback_read_timeout();
                spawn_callback_reader(move || {
                    let _ = tx.send(handle_callback(stream, &state, connection_deadline, &over));
                });
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
    deadline: Instant,
    wait_over: &std::sync::atomic::AtomicBool,
) -> Result<Option<String>> {
    // The accepted socket inherits the listener's O_NONBLOCK on some
    // platforms (macOS), so a single `read` could return WouldBlock and drop
    // the code on the floor. Force blocking, then read under the connection's
    // whole deadline: the browser has already connected, so the request is
    // milliseconds away, and a half-open or trickling connection ends at the
    // deadline instead of holding its reader.
    let _ = stream.set_nonblocking(false);
    let Some(buf) = read_callback_head(&mut stream, deadline, MAX_CALLBACK_HEAD_BYTES, |buf| {
        buf.windows(4).any(|w| w == b"\r\n\r\n")
    }) else {
        return Ok(None);
    };
    if wait_over.load(std::sync::atomic::Ordering::Acquire) {
        // The login already ended (a cancel, another door, an aborted reader):
        // write NO page of any kind. The connection closes when `stream` drops.
        return Ok(None);
    }
    let request = String::from_utf8_lossy(&buf);
    let target = request
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or("");
    if !target.starts_with(CALLBACK_PATH) {
        // Some other path (a favicon probe) — not the redirect.
        login_page::write_page(&mut stream, Id::CodexWaiting);
        return Ok(None);
    }
    let query = target.split_once('?').map(|(_, q)| q).unwrap_or("");
    let state = query_param(query, "state").unwrap_or_default();
    if state != expected_state {
        login_page::write_page(&mut stream, Id::CodexStateMismatch);
        bail!("codex login state mismatch (possible CSRF); aborted");
    }
    match query_param(query, "code") {
        Some(code) if !code.is_empty() => {
            login_page::write_page(&mut stream, Id::CodexSuccess);
            Ok(Some(code))
        }
        _ => {
            // `error` and the `error_description` beside it are uncapped
            // browser-supplied text: the code is parsed into RFC 6749's closed
            // set and its bytes dropped, the description never read.
            let Some(err) = query_param(query, "error") else {
                login_page::write_page(&mut stream, Id::CodexNoCode);
                bail!("codex login callback carried no code");
            };
            let rejection = AuthorizeRejection::parse(&err);
            let (page, line) = rejection_copy(&rejection);
            login_page::write_page(&mut stream, page);
            logline!(
                "clauth: the codex authorize callback refused the login: {}",
                rejection.log_detail()
            );
            bail!("{line}");
        }
    }
}

/// The page id and terminal line per rejection arm, every byte of the line
/// this module's own literal. The claude callback's `user_message` names
/// anthropic in its vendor arms, so only the vendor-free declined line is
/// shared.
fn rejection_copy(rejection: &AuthorizeRejection) -> (Id, &'static str) {
    match rejection {
        AuthorizeRejection::Declined => (Id::CodexDeclined, rejection.user_message()),
        AuthorizeRejection::Upstream(_) => (Id::CodexUpstream, "openai is having trouble"),
        AuthorizeRejection::Refused(_) | AuthorizeRejection::Unrecognized => {
            (Id::CodexRefused, "openai refused the login")
        }
    }
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

/// The authorization-code exchange against `url`: `application/x-www-form-urlencoded`,
/// the encoding that differs from the JSON refresh at the same endpoint. Split
/// from [`exchange_code`] so tests drive the wire shape against a local stub.
fn exchange_code_at(
    url: &str,
    code: &str,
    verifier: &str,
    redirect_uri: &str,
) -> Result<CodeExchange> {
    let body = code_exchange_body(code, verifier, redirect_uri);
    let mut resp = crate::oauth::http_agent()
        .post(url)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .send(body)
        .context("codex token exchange transport failed")?;
    let status = resp.status().as_u16();
    if !(200..300).contains(&status) {
        bail!("codex token exchange returned HTTP {status}");
    }
    let text = resp
        .body_mut()
        .read_to_string()
        .context("codex token exchange transport failed")?;
    serde_json::from_str(&text).map_err(|e| {
        crate::format::parse_refusal(
            "codex token exchange response did not parse",
            crate::format::json_body_position(&e),
        )
    })
}

fn exchange_code(code: &str, verifier: &str, redirect_uri: &str) -> Result<CodeExchange> {
    exchange_code_at(CODEX_TOKEN_URL, code, verifier, redirect_uri)
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

/// The RFC-8693 token-exchange against `url` that mints an `OPENAI_API_KEY`
/// from the id_token. Best-effort by contract: `Ok(None)` on any non-success,
/// so a login that only wants the ChatGPT chain still completes. Split from
/// [`exchange_api_key`] so tests drive both answers against a local stub.
fn exchange_api_key_at(url: &str, id_token: &str) -> Result<Option<String>> {
    let body = format!(
        "grant_type={}&client_id={}&requested_token={}&subject_token={}&subject_token_type={}",
        percent_encode("urn:ietf:params:oauth:grant-type:token-exchange"),
        percent_encode(CODEX_CLIENT_ID),
        percent_encode("openai-api-key"),
        percent_encode(id_token),
        percent_encode("urn:ietf:params:oauth:token-type:id_token"),
    );
    let mut resp = match crate::oauth::http_agent()
        .post(url)
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

fn exchange_api_key(id_token: &str) -> Result<Option<String>> {
    exchange_api_key_at(CODEX_TOKEN_URL, id_token)
}

fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339()
}

#[cfg(test)]
#[path = "../tests/inline/codex_login.rs"]
mod tests;
