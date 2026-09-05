//! `clauth daemon --listen` — the TLS REST API.
//!
//! Opt-in, off unless an address is passed. It exists for one deployment: the
//! daemon on the machine that holds the accounts, a client (the tray) on
//! another machine on the same network. Everything else about the daemon stays
//! file-based.
//!
//! Cross-platform, like the rest of clauth: macOS, Linux, and Windows. Only
//! [`tls`] knows which platform it is on — where lego's certificates live and
//! how this host's FQDN is discovered. Nothing else below branches.
//!
//! Shape:
//!   * **TLS always.** There is no plaintext mode and no flag to ask for one —
//!     the bearer token crosses this connection on every request.
//!   * **Token always.** Every route, health included, needs
//!     `Authorization: Bearer <token>`; see [`token`].
//!   * **Two operations.** Read the status feed, switch the active account. The
//!     switch goes through the same action the MCP tool uses, so anything
//!     needing human eyes is refused here too ([`routes`]).
//!   * **Thread per connection**, capped and time-bounded. Connections persist
//!     across requests and serve pipelined ones in order; see [`http`] for the
//!     framing rules that makes safe. No async runtime.

mod http;
pub(crate) mod routes;
pub(crate) mod tls;
pub(crate) mod token;

use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use anyhow::{Context, Result};

use crate::logline::logline;
use crate::profile::ConfigHandle;

use routes::ApiContext;
use token::AuthToken;

/// Concurrent connections served at once. The cap is what stops a connection
/// flood from spawning threads without bound.
///
/// Raised from 8 when connections became persistent: a slot is now held for a
/// client's whole polling session rather than for one request, so the old
/// figure would have let a handful of idle clients lock everyone else out.
const MAX_CONNECTIONS: usize = 32;
/// How long the accept loop pauses after an error it cannot attribute to one
/// connection, so a persistent failure cannot spin a core.
const ACCEPT_BACKOFF: Duration = Duration::from_millis(100);

/// The three bounds on one connection, together in one place because they only
/// make sense relative to each other and because getting that relationship
/// wrong is exactly how the advertised keep-alive window came to be a lie.
///
/// Injected rather than read from constants so a test can drive a whole
/// connection lifecycle in milliseconds instead of minutes; production always
/// passes [`Limits::DEFAULT`].
#[derive(Clone, Copy)]
pub(crate) struct Limits {
    /// How long ONE read or write may block. Short, because mid-request it is
    /// the defense against a peer trickling bytes to hold a slot.
    pub(crate) io_timeout: Duration,
    /// How long a connection may live in total, idle time included. Must be
    /// well above a client's polling interval or the connection is never
    /// actually reused; must stay finite so no client holds a slot forever.
    pub(crate) lifetime: Duration,
    /// Requests served on one connection before it is closed. Bounds how long
    /// one client can hold a slot no matter how politely it behaves.
    pub(crate) max_requests: u32,
}

impl Limits {
    pub(crate) const DEFAULT: Self = Self {
        io_timeout: Duration::from_secs(10),
        // Comfortably above the tray's 30s poll, so a polling client reuses its
        // connection for several polls rather than handshaking every time.
        lifetime: Duration::from_secs(120),
        max_requests: 100,
    };
}

/// Live connection count, and whether we have already said we are saturated.
/// The flag makes the "at capacity" line fire on the transition rather than
/// once per rejected connection — otherwise the flood being rejected would
/// simply move into `daemon.log`.
static LIVE_CONNECTIONS: AtomicUsize = AtomicUsize::new(0);
static SATURATION_LOGGED: AtomicBool = AtomicBool::new(false);

/// How far the connection count must fall before another "at capacity" line is
/// allowed.
///
/// Hysteresis, and the whole reason the flag works at all. Re-arming on ANY
/// release re-armed it constantly: at the cap a flood ends one connection and
/// takes its slot immediately, so every refusal found the flag clear and logged
/// — which is the per-rejection spam the flag exists to prevent, just reached by
/// a longer route. Half the cap is a level the server only reaches by genuinely
/// coming back down, which is what makes the next line a new episode.
const SATURATION_REARM_BELOW: usize = MAX_CONNECTIONS / 2;

/// Decrements [`LIVE_CONNECTIONS`] however the handler thread ends, panic
/// included — a leaked slot here would permanently shrink the cap.
struct ConnectionSlot;

impl Drop for ConnectionSlot {
    fn drop(&mut self) {
        // `fetch_sub` returns the value BEFORE the subtraction.
        let remaining = LIVE_CONNECTIONS.fetch_sub(1, Ordering::AcqRel) - 1;
        if remaining < SATURATION_REARM_BELOW {
            SATURATION_LOGGED.store(false, Ordering::Release);
        }
    }
}

/// Claim a connection slot, or `None` when the server is already at
/// [`MAX_CONNECTIONS`].
///
/// Split out of the accept loop so the cap is testable without a socket: it is
/// the only thing standing between a connection flood and unbounded thread
/// creation, which makes it the piece most worth pinning.
///
/// The check and the increment are ONE atomic operation, so the cap cannot be
/// overshot however many threads call this. The earlier load-then-`fetch_add`
/// left a window between them and leaned on there being exactly one accepting
/// thread to keep that window harmless — an invariant that lives in the caller,
/// not here, and that a second acceptor would break silently.
fn claim_slot() -> Option<ConnectionSlot> {
    let claimed = LIVE_CONNECTIONS
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |live| {
            (live < MAX_CONNECTIONS).then_some(live + 1)
        })
        .is_ok();
    if claimed {
        return Some(ConnectionSlot);
    }
    // Say so once per saturation episode rather than once per rejected
    // connection: otherwise the flood being refused just moves into
    // `daemon.log`. See [`SATURATION_REARM_BELOW`] for when the next episode
    // may start.
    if !SATURATION_LOGGED.swap(true, Ordering::AcqRel) {
        logline!(
            "clauth api: at {MAX_CONNECTIONS} concurrent connections; refusing more \
             until one finishes"
        );
    }
    None
}

/// Start the listener. Returns once it is bound and its accept thread is
/// running; the daemon's main loop carries on.
///
/// Every failure here is fatal to the daemon by design (the caller propagates
/// it): the operator asked for a listener, and a daemon that silently ran
/// without one — no certificate, port already taken — would look healthy while
/// the remote client stayed dark.
/// Everything that can FAIL about the listener, done before anything is at
/// stake: the certificate is read and the socket is bound.
///
/// Split from [`serve_prepared`] so `daemon::serve` can run it BEFORE claiming
/// the singleton. Under `--replace` the claim terminates the running daemon, so
/// a certificate that had just been renewed badly used to take the incumbent
/// down and then abort — leaving the host with no daemon at all, and with it no
/// refresh and no auto-switch, not merely no listener. `wiki/Daemon.md`
/// recommends `clauth daemon --replace --listen` as the post-`lego renew` hook,
/// which makes the documented automation the trigger. Prepared first, a bad
/// renewal is a no-op: the incumbent keeps running.
pub(crate) struct Prepared {
    listen: SocketAddr,
    listener: TcpListener,
    tls_config: Arc<rustls::ServerConfig>,
    auth: AuthToken,
}

pub(crate) fn prepare(listen: SocketAddr, certs: &tls::CertSource) -> Result<Prepared> {
    // The plaintext token lives only for this scope; `AuthToken` keeps its
    // digest.
    let auth = AuthToken::from_plaintext(&token::load_or_create()?);
    let tls_config = tls::server_config(certs)?;
    let listener = TcpListener::bind(listen)
        .with_context(|| format!("failed to bind the REST API to {listen}"))?;
    Ok(Prepared {
        listen,
        listener,
        tls_config,
        auth,
    })
}

/// Start serving on an already-[`prepare`]d listener. Only the accept thread's
/// creation can fail here, and that failure is not one a certificate or a busy
/// port can cause.
pub(crate) fn serve_prepared(
    prepared: Prepared,
    config: ConfigHandle,
    status_path: PathBuf,
    live: super::LiveStores,
) -> Result<()> {
    let Prepared {
        listen,
        listener,
        tls_config,
        auth,
    } = prepared;
    let ctx = ApiContext::new(config, status_path, auth, Some(live));

    let spawned = std::thread::Builder::new()
        .name("clauth-api-accept".into())
        .spawn(move || accept_loop(&listener, &tls_config, &ctx));
    spawned.context("failed to spawn the REST API accept thread")?;

    logline!("clauth daemon: REST API listening on https://{listen}");
    Ok(())
}

fn accept_loop(
    listener: &TcpListener,
    tls_config: &Arc<rustls::ServerConfig>,
    ctx: &Arc<ApiContext>,
) {
    loop {
        let (stream, peer) = match listener.accept() {
            Ok(accepted) => accepted,
            Err(e) => {
                logline!("clauth api: accept failed: {e}");
                std::thread::sleep(ACCEPT_BACKOFF);
                continue;
            }
        };

        let Some(slot) = claim_slot() else {
            // Dropped before the handshake — the cheapest possible refusal, and
            // it costs an attacker a full TCP round trip per attempt.
            drop(stream);
            continue;
        };

        let tls_config = Arc::clone(tls_config);
        let ctx = Arc::clone(ctx);
        let spawned = std::thread::Builder::new()
            .name("clauth-api-conn".into())
            .spawn(move || {
                // Moved in, so the slot is released when this thread ends
                // however it ends.
                let _slot = slot;
                serve_connection(stream, peer, &tls_config, &ctx, Limits::DEFAULT);
            });
        if let Err(e) = spawned {
            // The slot moved into the closure that was never created, so it
            // dropped with it; nothing to release here.
            logline!("clauth api: failed to spawn a connection thread: {e}");
        }
    }
}

/// One connection: handshake, then requests until the client stops asking, the
/// budget runs out, or something goes wrong.
///
/// Requests are served strictly one at a time. That is what keeps a pipelining
/// client's responses in the order it asked for them, and it means a `POST
/// /api/v1/switch` on a reused connection is serialized against the next request
/// exactly as it would be on its own.
fn serve_connection(
    stream: TcpStream,
    peer: SocketAddr,
    tls_config: &Arc<rustls::ServerConfig>,
    ctx: &ApiContext,
    limits: Limits,
) {
    let _ = stream.set_read_timeout(Some(limits.io_timeout));
    let _ = stream.set_write_timeout(Some(limits.io_timeout));
    let _ = stream.set_nodelay(true);

    let conn = match rustls::ServerConnection::new(Arc::clone(tls_config)) {
        Ok(conn) => conn,
        Err(e) => {
            logline!("clauth api: TLS setup failed for {peer}: {e}");
            return;
        }
    };
    let opened = std::time::Instant::now();
    let expires = opened + limits.lifetime;
    // The FIRST request gets only `io_timeout` to arrive: a client that
    // connects and says nothing has not earned the idle allowance, and at 32
    // slots the difference between 10s and the full lifetime is what a trivial
    // flood would exploit. An established connection then gets the rest.
    let mut reader = http::RequestReader::new(
        rustls::StreamOwned::new(conn, stream),
        expires.min(opened + limits.io_timeout),
    );
    let mut served: u32 = 0;

    loop {
        // The TLS handshake runs lazily inside the first read here, so a
        // plaintext client or a bad SNI surfaces as an IO error and gets no
        // response: there is no secure channel to answer over.
        let request = match reader.next_request() {
            // Clean close, or an idled-out budget, between requests. Either is
            // how a kept-alive connection normally ends.
            Ok(None) => break,
            Ok(Some(req)) => req,
            Err(http::RequestError::Io(e)) => {
                if served == 0 {
                    logline!("clauth api: {peer}: connection failed before a request arrived: {e}");
                }
                break;
            }
            Err(e) => {
                // A framing error means we no longer know where the next
                // message would start, so answer and close rather than try to
                // resynchronize on a stream an attacker may be framing.
                let response = e.response();
                logline!("clauth api: {peer} <unparsed> -> {}", response.status);
                let _ =
                    http::write_response(reader.stream_mut(), &response, &http::Disposition::Close);
                break;
            }
        };

        served = served.saturating_add(1);
        let summary = format!(
            "{} {}",
            request.method,
            http::sanitize_for_log(&request.path)
        );
        let response = routes::handle(ctx, &request);

        // Both sides have to agree, and the budget is ours alone to enforce.
        // What is advertised is the budget genuinely left, so a client is never
        // told it has time it does not have.
        //
        // An unauthenticated request never keeps the connection. Otherwise
        // anyone able to reach the port could hold a slot for the whole budget
        // by sending one bogus request and going quiet, and at 32 slots that is
        // a lockout for the price of a TCP connection. Before connections
        // persisted, one socket timeout capped that; now the budget would. A
        // real client always presents its token, so this costs nothing
        // legitimate.
        let remaining = expires.saturating_duration_since(std::time::Instant::now());
        let disposition = if request.keep_alive
            && response.status != 401
            && served < limits.max_requests
            && !remaining.is_zero()
        {
            http::Disposition::KeepAlive {
                timeout_secs: remaining.as_secs(),
                max_requests: limits.max_requests - served,
            }
        } else {
            http::Disposition::Close
        };

        logline!("clauth api: {peer} {summary} -> {}", response.status);
        if let Err(e) = http::write_response(reader.stream_mut(), &response, &disposition) {
            logline!("clauth api: {peer}: failed to write the response: {e}");
            break;
        }
        if matches!(disposition, http::Disposition::Close) {
            break;
        }
        // Established: the idle wait for the next request runs to the
        // connection's own expiry, not to one socket timeout.
        reader.set_deadline(expires);
    }

    // Close cleanly so the client sees an orderly shutdown rather than a
    // truncation it has to distinguish from an attack.
    let mut tls_stream = reader.into_inner();
    tls_stream.conn.send_close_notify();
    let _ = std::io::Write::flush(&mut tls_stream);
}

#[cfg(test)]
#[path = "../../../tests/inline/daemon_api_server.rs"]
mod tests;
