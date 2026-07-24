//! CDX-5: the opt-in localhost injection proxy (proxy-design.md). Codex points
//! at `http://127.0.0.1:<port>/backend-api/codex` via a printed
//! `[model_providers.clauth]` block; this proxy strips codex's own identity
//! headers, injects the selected pool account's `Authorization` +
//! `ChatGPT-Account-ID`, forwards to `https://chatgpt.com/backend-api/codex`,
//! and streams the SSE response back. On a pre-commit 429/401/5xx it rotates
//! to the next pool account and replays before the client sees a byte — the
//! true in-session fallback the whole CDX ladder points at.
//!
//! Deliberately its own process (`clauth proxy`), not the daemon: proxy-down
//! must be codex-down, never clauth-down, and a `pkill` daemon restart must
//! not sever in-flight codex streams. SSE-only, plain loopback HTTP, no TLS,
//! no WebSocket (proxy-design §1.2) — hand-rolled `TcpListener` + `ureq`
//! upstream, no new async runtime.

pub(crate) mod http;
pub(crate) mod pool;
pub(crate) mod sse;

use std::io::{BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use crate::logline::logline;
use crate::profile::AppConfig;
use crate::profile_cache::{USAGE_CACHE_FILE, write_profile_cache};
use crate::usage::{UsageInfo, now_ms};

use self::http::{RequestError, RequestHead, read_body, read_request_head};
use self::pool::{Cooldowns, PoolMember, Selection, next_after_failure, select_account};

/// Default loopback port (unclaimed; overridable). Named in the printed config.
pub(crate) const DEFAULT_PROXY_PORT: u16 = 4517;

/// The ChatGPT-mode base the printed config points codex at, and the upstream
/// authority the proxy forwards to. Scheme+authority are constants — no client
/// byte can relocate them (proxy-design §1.4).
const UPSTREAM_BASE: &str = "https://chatgpt.com/backend-api/codex";

/// The path prefix codex requests under the printed provider config; a request
/// target not beginning here is answered 404 without forwarding.
const EXPECTED_PREFIX: &str = "/backend-api/codex";

/// Heartbeat file: while fresh, the passive JSONL leg stands down so the
/// proxy's per-account header feed is the sole codex usage writer
/// (proxy-design §1.7).
pub(crate) fn heartbeat_path() -> Result<PathBuf> {
    Ok(crate::profile::clauth_dir()?.join("codex-proxy.json"))
}

/// Whether a proxy is actively serving — the heartbeat is younger than
/// `2 × interval_ms`. Read by `codex_passive_tick` to decide whether to stand
/// down. A missing/unreadable/stale heartbeat = no active proxy.
pub(crate) fn proxy_active(interval_ms: u64) -> bool {
    let Ok(path) = heartbeat_path() else {
        return false;
    };
    let Ok(meta) = std::fs::metadata(&path) else {
        return false;
    };
    let Some(age) = meta.modified().ok().and_then(|m| m.elapsed().ok()) else {
        return false;
    };
    age.as_millis() <= (interval_ms.saturating_mul(2)) as u128
}

/// Print the `config.toml` block a user pastes to point codex at the proxy
/// (proxy-design §1.3 — clauth NEVER writes the live config).
pub(crate) fn print_config(port: u16) {
    println!(
        "# Paste into ~/.codex/config.toml to route codex through clauth's proxy.\n\
         # (clauth never edits this file for you — proxy off = delete this block.)\n\
         model_provider = \"clauth\"\n\n\
         [model_providers.clauth]\n\
         name = \"openai\"\n\
         base_url = \"http://127.0.0.1:{port}{EXPECTED_PREFIX}\"\n\
         wire_api = \"responses\"\n\
         requires_openai_auth = true"
    );
}

/// Shared proxy state across connection threads.
struct ProxyState {
    config: crate::profile::ConfigHandle,
    cooldowns: Mutex<Cooldowns>,
    /// The upstream base URL requests are forwarded to. Production always uses
    /// [`UPSTREAM_BASE`]; the sandbox e2e points it at a local stub server.
    /// NEVER read from config (a client byte can never relocate it — §1.4).
    upstream_base: String,
}

/// Run the proxy until interrupted. Binds loopback only.
pub(crate) fn run(port: u16) -> Result<()> {
    let listener = TcpListener::bind(("127.0.0.1", port))
        .with_context(|| format!("failed to bind 127.0.0.1:{port} — is another proxy running?"))?;
    let config: crate::profile::ConfigHandle = std::sync::Arc::new(
        crate::lockorder::RankedMutex::new(crate::profile::load_config()?),
    );
    let state = std::sync::Arc::new(ProxyState {
        config,
        cooldowns: Mutex::new(Cooldowns::default()),
        upstream_base: UPSTREAM_BASE.to_string(),
    });

    println!("clauth proxy listening on http://127.0.0.1:{port}{EXPECTED_PREFIX}");
    println!("  point codex at it with:  clauth proxy --print-config --port {port}");
    println!("  (loopback only, no client auth — any local process can use the pool)");
    // Stamp log lines like the daemon does: the proxy is a supervised process
    // whose stderr lands in `proxy.log`, and the 2026-07-18 incident had to be
    // reconstructed from an unstamped error flood.
    crate::logline::enable_timestamps();
    logline!("clauth proxy: listening on 127.0.0.1:{port}");
    touch_heartbeat(port);

    // Bounded thread-per-connection: codex holds few concurrent requests, so a
    // small cap can never throttle real use, but it stops a local slow-loris
    // (a trickle client resets the per-read timeout every byte) from exhausting
    // threads/FDs on clauth itself (review LOW). Over the cap → immediate 503.
    let live = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    for stream in listener.incoming() {
        match stream {
            Ok(mut stream) => {
                if live.load(std::sync::atomic::Ordering::Relaxed) >= MAX_CONCURRENT_CONNECTIONS {
                    let _ = http::write_error(&mut stream, "503 Service Unavailable", "proxy busy");
                    continue;
                }
                let state = std::sync::Arc::clone(&state);
                let live = std::sync::Arc::clone(&live);
                live.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                std::thread::spawn(move || {
                    touch_heartbeat(port);
                    if let Err(e) = handle_connection(&state, stream) {
                        logline!("clauth proxy: connection error: {e}");
                    }
                    live.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
                });
            }
            Err(e) => logline!("clauth proxy: accept failed: {e}"),
        }
    }
    Ok(())
}

/// Concurrent-connection cap (review LOW: unbounded thread-per-connection).
/// Far above real codex concurrency; only a runaway/slow-loris ever hits it.
const MAX_CONCURRENT_CONNECTIONS: usize = 64;

/// Refresh the heartbeat file (atomic). Best-effort — a write failure just
/// means the passive leg may double-publish (benign, §1.7).
fn touch_heartbeat(port: u16) {
    let Ok(path) = heartbeat_path() else { return };
    let body = serde_json::json!({ "port": port, "at_ms": now_ms() }).to_string();
    let _ = crate::profile::atomic_write(&path, body.as_bytes());
}

/// Handle one client connection: read the request, then run the account-
/// rotating replay loop (proxy-design §2).
fn handle_connection(state: &ProxyState, stream: TcpStream) -> Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(30))).ok();
    let mut reader = BufReader::new(stream.try_clone().context("clone client stream")?);
    let mut client = stream;

    let head = match read_request_head(&mut reader) {
        Ok(h) => h,
        Err(RequestError::Malformed(m)) => {
            let _ = http::write_error(&mut client, "400 Bad Request", &m);
            return Ok(());
        }
        Err(e) => {
            let _ = http::write_error(&mut client, e.status(), "request rejected");
            return Ok(());
        }
    };

    // Fixed authority (§1.4): only our own prefix is forwarded; an absolute-
    // form target or a foreign path is a 404, never a forward.
    if !head.target.starts_with(EXPECTED_PREFIX) {
        return http::write_error(&mut client, "404 Not Found", "unknown path").map_err(Into::into);
    }
    // Codex issues POST /responses (the model turn) and GET /models (metadata
    // refresh) under our prefix — forward both with their own method. Any other
    // method is 405'd rather than silently rewritten (rewriting a non-idempotent
    // method was the original review LOW). Real-backend run 2026-07-16: 405'ing
    // GET /models made codex log errors and lose its model list, so it is now
    // forwarded too.
    let method_ok =
        head.method.eq_ignore_ascii_case("POST") || head.method.eq_ignore_ascii_case("GET");
    if !method_ok {
        return http::write_error(&mut client, "405 Method Not Allowed", "POST or GET only")
            .map_err(Into::into);
    }
    let body_len = match head.content_length() {
        Ok(n) => n,
        Err(e) => {
            let _ = http::write_error(&mut client, e.status(), "bad request framing");
            return Ok(());
        }
    };
    let body = read_body(&mut reader, body_len).context("read request body")?;

    forward_with_rotation(state, &head, &body, &mut client)
}

/// The replay loop (§2): pick an account, forward, and on a PRE-COMMIT
/// failure rotate to the next account and replay — up to one attempt per pool
/// member. Once any response byte has reached the client, propagate as-is.
fn forward_with_rotation(
    state: &ProxyState,
    head: &RequestHead,
    body: &[u8],
    client: &mut TcpStream,
) -> Result<()> {
    let ordered = pool_snapshot(state);
    if ordered.is_empty() {
        return http::write_error(
            client,
            "503 Service Unavailable",
            "no codex accounts in the pool",
        )
        .map_err(Into::into);
    }
    let active = active_codex(state);
    let now = now_ms();

    let mut current = match select_account(&ordered, active.as_deref(), now) {
        Selection::Use(name) => name,
        Selection::Exhausted => {
            return http::write_error(
                client,
                "429 Too Many Requests",
                "every codex account is in cooldown",
            )
            .map_err(Into::into);
        }
    };
    let mut tried: Vec<String> = Vec::new();

    loop {
        tried.push(current.clone());
        match forward_once(state, &current, head, body, client)? {
            ForwardOutcome::Streamed => return Ok(()),
            ForwardOutcome::UpstreamUnreachable => {
                return http::write_error(
                    client,
                    "502 Bad Gateway",
                    "codex upstream is unreachable",
                )
                .map_err(Into::into);
            }
            ForwardOutcome::PreCommitFailure { reset_ms } => {
                if let Ok(mut cd) = state.cooldowns.lock() {
                    cd.stamp(&current, now_ms(), reset_ms);
                }
                match next_after_failure(&ordered, &current, &tried, now_ms()) {
                    Selection::Use(next) => {
                        logline!("clauth proxy: rotating {current} → {next} (pre-commit failure)");
                        current = next;
                    }
                    Selection::Exhausted => {
                        return http::write_error(
                            client,
                            "429 Too Many Requests",
                            "every codex account rejected this request",
                        )
                        .map_err(Into::into);
                    }
                }
            }
        }
    }
}

enum ForwardOutcome {
    /// Response bytes reached the client (committed — never retried).
    Streamed,
    /// Upstream returned 429/401/5xx BEFORE any byte reached the client.
    PreCommitFailure { reset_ms: Option<u64> },
    /// A TRANSPORT error reaching the upstream (DNS/TLS/conn-refused). The
    /// upstream authority is a single fixed host, so this is not
    /// account-specific — walking the rest of the pool would re-hit the same
    /// unreachable host and pay the connect timeout per member (review LOW).
    /// Fail the request fast instead.
    UpstreamUnreachable,
}

/// Relay surviving inbound headers (minus the stripped set — §1.4), then inject
/// the identity + framing this proxy owns. Generic over ureq's request-body
/// typestate so one impl serves both the POST (/responses) and GET (/models)
/// builders.
fn inject_upstream_headers<B>(
    mut req: ureq::RequestBuilder<B>,
    head: &RequestHead,
    identity: &Identity,
) -> ureq::RequestBuilder<B> {
    for (name, value) in &head.headers {
        if !http::is_stripped_request_header(name) {
            req = req.header(name.as_str(), value.as_str());
        }
    }
    req.header("Host", "chatgpt.com")
        .header("Authorization", format!("Bearer {}", identity.access_token))
        .header("ChatGPT-Account-ID", identity.account_id.as_str())
        .header("Accept-Encoding", "identity")
}

/// One upstream attempt against `account`. Injects identity, forwards, and on
/// a < 400 status streams the body through to the client (capturing usage
/// headers). A 429/401/5xx returns [`ForwardOutcome::PreCommitFailure`]
/// WITHOUT writing anything to the client, so the caller can replay.
fn forward_once(
    state: &ProxyState,
    account: &str,
    head: &RequestHead,
    body: &[u8],
    client: &mut TcpStream,
) -> Result<ForwardOutcome> {
    let Some(identity) = account_identity(state, account) else {
        // Can't make this account usable (logged out / unrefreshable) — treat
        // as a pre-commit failure so the loop rotates past it.
        logline!("clauth proxy: '{account}' has no usable token — skipping");
        return Ok(ForwardOutcome::PreCommitFailure { reset_ms: None });
    };

    let url = format!(
        "{}{}",
        state.upstream_base,
        &head.target[EXPECTED_PREFIX.len()..]
    );
    // PROXY_AGENT carries http_status_as_error(false) so a 429/5xx returns
    // Ok(response) here, not Err — the pre-commit branch below reads status.
    // Forward with the request's OWN method: POST /responses carries the body;
    // GET /models is bodyless (handle_connection admits only these two).
    let send_result = if head.method.eq_ignore_ascii_case("GET") {
        inject_upstream_headers(crate::oauth::PROXY_AGENT.get(&url), head, &identity).call()
    } else {
        inject_upstream_headers(crate::oauth::PROXY_AGENT.post(&url), head, &identity).send(body)
    };
    let response = match send_result {
        Ok(r) => r,
        Err(e) => {
            logline!("clauth proxy: upstream transport error on '{account}': {e}");
            // The upstream host is fixed — a transport failure is not
            // account-specific, so fail fast rather than walk the pool.
            return Ok(ForwardOutcome::UpstreamUnreachable);
        }
    };
    let status = response.status().as_u16();
    // Capture usage from the flow-through rate-limit headers (§1.7) regardless
    // of status — even a 429 carries fresh counters.
    capture_usage_headers(account, &response);

    if status == 429 || status == 401 || status >= 500 {
        let reset_ms = parse_reset_header(&response);
        return Ok(ForwardOutcome::PreCommitFailure { reset_ms });
    }

    // Commit: write the status line + relayed headers, then stream the body.
    // Once a byte is written we are committed — every end shape below closes
    // this connection (truncated or complete), never replays (pre-commit rule).
    let started = Instant::now();
    let path = &head.target[EXPECTED_PREFIX.len()..];
    if let Err(e) = write_response_head(client, status, &response) {
        logline!(
            "clauth proxy: {account} {} {path} → {status} · client closed before head relay ({e})",
            head.method
        );
        return Ok(ForwardOutcome::Streamed);
    }
    let mut body_reader = response.into_body().into_reader();
    let mut sniffer = sse::TerminalSniffer::default();
    let mut relayed: u64 = 0;
    let mut buf = [0u8; 16 * 1024];
    let end = loop {
        let n = match body_reader.read(&mut buf) {
            Ok(0) => break RelayEnd::UpstreamEof,
            Ok(n) => n,
            Err(e) => break RelayEnd::UpstreamError(e),
        };
        // Sniff BEFORE write so the chunk carrying the terminal event is still
        // relayed, then the stream closes on our side — the upstream holds SSE
        // streams open past `response.completed` (see `proxy::sse`), so EOF
        // alone would leak this thread until the backstop timeout.
        let terminal = sniffer.feed(&buf[..n]);
        if let Err(e) = client.write_all(&buf[..n]) {
            break RelayEnd::ClientClosed(e);
        }
        relayed += n as u64;
        if terminal {
            break RelayEnd::Terminal;
        }
    };
    client.flush().ok();
    let secs = started.elapsed().as_secs();
    match end {
        // Normal ends — one summary line per request, stamped, greppable.
        RelayEnd::Terminal => {
            logline!(
                "clauth proxy: {account} {} {path} → {status} · {relayed}B in {secs}s · completed",
                head.method
            );
        }
        RelayEnd::UpstreamEof => {
            logline!(
                "clauth proxy: {account} {} {path} → {status} · {relayed}B in {secs}s · upstream EOF",
                head.method
            );
        }
        // The client bailed mid-relay (user interrupt, codex gave up) — its
        // decision, not a proxy fault; log as an end shape, not an error.
        RelayEnd::ClientClosed(e) => {
            logline!(
                "clauth proxy: {account} {} {path} → {status} · {relayed}B in {secs}s · client closed ({e})",
                head.method
            );
        }
        // The genuine anomaly: upstream died (or the backstop fired — the
        // elapsed seconds make that distinction readable) mid-stream, and the
        // client sees a truncated stream.
        RelayEnd::UpstreamError(e) => {
            logline!(
                "clauth proxy: {account} {} {path} → {status} · TRUNCATED after {relayed}B in {secs}s · upstream error: {e}",
                head.method
            );
        }
    }
    Ok(ForwardOutcome::Streamed)
}

/// How the committed relay of one response body ended.
enum RelayEnd {
    /// The sniffer saw a terminal SSE event (`response.completed` / `.failed`
    /// / `.incomplete` / `[DONE]`) — the turn is over; close without waiting
    /// for an upstream EOF that never comes.
    Terminal,
    /// Upstream finished the body (Content-Length'd responses, or a server
    /// that does close).
    UpstreamEof,
    /// The client hung up mid-relay.
    ClientClosed(std::io::Error),
    /// The upstream read failed mid-stream — the client sees truncation.
    UpstreamError(std::io::Error),
}

/// Write the committed response's status line + relayed headers (minus
/// hop-by-hop), forcing `Connection: close` (§1.8 single-request framing).
fn write_response_head(
    client: &mut TcpStream,
    status: u16,
    response: &ureq::http::Response<ureq::Body>,
) -> Result<()> {
    let reason = response.status().canonical_reason().unwrap_or("");
    let mut head = format!("HTTP/1.1 {status} {reason}\r\n");
    for (name, value) in response.headers() {
        let lname = name.as_str().to_ascii_lowercase();
        if http::HOP_BY_HOP.contains(&lname.as_str()) {
            continue;
        }
        if let Ok(v) = value.to_str() {
            head.push_str(&format!("{}: {}\r\n", name.as_str(), v));
        }
    }
    head.push_str("Connection: close\r\n\r\n");
    client
        .write_all(head.as_bytes())
        .context("write response head")?;
    Ok(())
}

/// Identity for one account: the fresh access token + account id to inject.
struct Identity {
    access_token: String,
    account_id: String,
}

/// Resolve the injectable identity for `account` (proxy-design §1.6): the
/// live-owner profile reads the LIVE auth.json (codex keeps it fresh); every
/// other member reads its store snapshot, refreshing through the CDX-3
/// machinery when the access token is inside the expiry margin. `None` when no
/// usable token can be produced.
fn account_identity(state: &ProxyState, account: &str) -> Option<Identity> {
    // Live owner? Read the live file directly.
    let live_bytes = crate::codex::read_live().ok().flatten();
    let live = live_bytes
        .as_deref()
        .and_then(|b| crate::codex::CodexAuthFile::parse(b).ok());
    let is_live_owner = live
        .as_ref()
        .and_then(|l| l.account_id())
        .zip(stored_account_id(account))
        .is_some_and(|(live_id, stored_id)| live_id == stored_id);

    let bytes = if is_live_owner {
        live_bytes?
    } else {
        // Parked chain: refresh through CDX-3 if near expiry, then read store.
        ensure_fresh_parked(state, account);
        crate::codex::read_profile_auth(account).ok().flatten()?
    };
    let auth = crate::codex::CodexAuthFile::parse(&bytes).ok()?;
    Some(Identity {
        access_token: auth.access_token()?.to_string(),
        account_id: auth.account_id()?,
    })
}

fn stored_account_id(account: &str) -> Option<String> {
    let bytes = crate::codex::read_profile_auth(account).ok().flatten()?;
    crate::codex::CodexAuthFile::parse(&bytes)
        .ok()?
        .account_id()
}

/// Refresh a parked account's chain if it is due — delegating to the SHARED
/// single-writer entry point `codex_refresh_parked` (RotationGuard + in-guard
/// re-read + adopt-back-first + apply-time chain re-check). The proxy MUST NOT
/// carry its own refresh: the review-confirmed CRIT was exactly a second,
/// guardless copy here that read the token before the guard and double-spent
/// the chain. There is now one implementation; both the daemon standby scan
/// and this path go through it.
fn ensure_fresh_parked(state: &ProxyState, account: &str) {
    // Cheap pre-gate outside the guard (re-checked authoritatively inside):
    // skip the guard entirely when the store already holds a fresh token.
    let due = crate::codex::read_profile_auth(account)
        .ok()
        .flatten()
        .and_then(|b| crate::codex::CodexAuthFile::parse(&b).ok())
        .is_some_and(|a| crate::codex::oauth::standby_due(&a, now_ms()));
    if !due {
        return;
    }
    if let crate::usage::CodexStandbyOutcome::Transient(e) = crate::usage::codex_refresh_parked(
        &state.config,
        account,
        None,
        &crate::codex::oauth::refresh,
        false,
    ) {
        logline!("clauth proxy: parked refresh for '{account}' failed (will retry): {e}");
    }
}

/// Parse usage from the upstream `x-codex-*` rate-limit headers and write the
/// account's usage cache (§1.7) — per-account live usage, zero extra requests.
fn capture_usage_headers(account: &str, response: &ureq::http::Response<ureq::Body>) {
    let h = |name: &str| {
        response
            .headers()
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
    };
    // Read only the default `x-codex-*` header family. Model-specific quota
    // families use a longer prefix (for example
    // `x-codex-bengalfox-primary-*`) and are intentionally ignored.
    let window = |prefix: &str| -> Option<crate::codex::usage::LimiterWindow> {
        let pct: f64 = h(&format!("x-codex-{prefix}-used-percent"))?.parse().ok()?;
        let resets_at =
            h(&format!("x-codex-{prefix}-reset-at")).and_then(|s| s.parse::<i64>().ok());
        let window_minutes =
            h(&format!("x-codex-{prefix}-window-minutes")).and_then(|s| s.parse::<i64>().ok());
        Some(crate::codex::usage::LimiterWindow {
            used_percent: pct,
            resets_at,
            window_minutes,
        })
    };
    let primary = window("primary");
    let secondary = window("secondary");
    if primary.is_none() && secondary.is_none() {
        return; // no rate-limit headers on this response
    }
    let (five_hour, seven_day, codex_rate_limit_reached) = crate::codex::usage::route_windows(
        primary,
        secondary,
        h("x-codex-rate-limit-reached-type").filter(|s| !s.is_empty()),
    );
    let info = UsageInfo {
        five_hour,
        seven_day,
        codex_rate_limit_reached,
        ..UsageInfo::default()
    };
    write_profile_cache(account, USAGE_CACHE_FILE, &info);
}

/// The advertised reset (epoch-ms) from the primary window's reset header, for
/// the cooldown stamp.
fn parse_reset_header(response: &ureq::http::Response<ureq::Body>) -> Option<u64> {
    response
        .headers()
        .get("x-codex-primary-reset-at")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<i64>().ok())
        .and_then(|secs| u64::try_from(secs).ok())
        .map(|secs| secs * 1000)
}

/// The pool in chain order (proxy-design §1.5): `codex_fallback_chain` when
/// non-empty, else every codex profile with a stored login. Availability =
/// not auth_broken, not leased, not in cooldown; cached-usage exhaustion is
/// carried separately as ADVISORY rank (see [`PoolMember::cached_spent`]) —
/// it deprioritizes a member but never excludes it, so a stale cache can't
/// wedge the proxy into 429ing traffic upstream would have served.
fn pool_snapshot(state: &ProxyState) -> Vec<PoolMember> {
    let Ok(cfg) = state.config.lock() else {
        return Vec::new();
    };
    let names: Vec<String> = if !cfg.state.codex_fallback_chain.is_empty() {
        cfg.state
            .codex_fallback_chain
            .iter()
            .map(|n| n.as_str().to_string())
            .filter(|n| cfg.find(n).is_some_and(|p| p.is_codex()))
            .collect()
    } else {
        cfg.profiles
            .iter()
            .filter(|p| p.is_codex())
            .map(|p| p.name.to_string())
            .collect()
    };
    let cooldowns = state.cooldowns.lock().ok();
    let now = now_ms();
    names
        .into_iter()
        .filter(|n| matches!(crate::codex::read_profile_auth(n), Ok(Some(_))))
        .map(|name| {
            let cooldown_until_ms = cooldowns.as_ref().map(|c| c.get(&name)).unwrap_or(0);
            let unavailable =
                cfg.is_auth_broken(&name) || crate::runtime::has_live_codex_session(&name);
            let cached_spent = cached_exhausted(&name, now, &cfg);
            PoolMember {
                name,
                cooldown_until_ms,
                unavailable,
                cached_spent,
            }
        })
        .collect()
}

/// Whether `name`'s cached usage says it is spent (the CDX-4 exhaustion shape
/// against its own cache). Best-effort — no cache = not exhausted.
fn cached_exhausted(name: &str, now_ms: u64, cfg: &AppConfig) -> bool {
    let now_secs = (now_ms / 1000) as i64;
    let weekly_pct = cfg.state.weekly_switch_threshold_pct();
    crate::profile_cache::load_profile_cache::<UsageInfo>(name, USAGE_CACHE_FILE)
        .is_some_and(|info| crate::fallback::codex_info_exhausted(&info, now_secs, weekly_pct))
}

fn active_codex(state: &ProxyState) -> Option<String> {
    state
        .config
        .lock()
        .ok()?
        .state
        .active_codex_profile
        .as_deref()
        .map(str::to_string)
}

#[cfg(test)]
impl ProxyState {
    /// Build a state pointed at a stub upstream — the e2e seam (§1.4: the
    /// base is never config-derived in production).
    fn for_test(config: crate::profile::ConfigHandle, upstream_base: String) -> Self {
        Self {
            config,
            cooldowns: Mutex::new(Cooldowns::default()),
            upstream_base,
        }
    }
}

/// Write a heartbeat under the sandbox home — for doctor/passive-leg tests.
#[cfg(test)]
pub(crate) fn touch_heartbeat_for_test(port: u16) {
    touch_heartbeat(port);
}

/// Accept and handle exactly one connection — the e2e driver (production uses
/// the `incoming()` loop in [`run`]).
#[cfg(test)]
pub(crate) fn serve_one_for_test(
    config: crate::profile::ConfigHandle,
    upstream_base: String,
    listener: &TcpListener,
) -> Result<()> {
    let state = ProxyState::for_test(config, upstream_base);
    let (stream, _) = listener.accept().context("accept")?;
    handle_connection(&state, stream)
}

#[cfg(test)]
#[path = "../../tests/inline/proxy.rs"]
mod tests;
