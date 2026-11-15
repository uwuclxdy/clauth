//! The route table and the one capability check.
//!
//! Every route is a row of [`ROUTES`] carrying the access it needs, and
//! [`handle`] reads that table for dispatch and for the check alike, so no
//! route is served outside it or reached without its check. The surface is
//! narrow on purpose: read the feed, switch the account, redeem a pairing
//! code. A switch is the one mutation the daemon already performs unattended,
//! so exposing it adds no capability the fallback chain does not have;
//! everything that needs a human (a diverged live login, an unprovable
//! identity) is refused here exactly as it is refused for the scheduler and
//! the MCP tool, and the TUI stays the only place to resolve it.

use std::collections::BTreeSet;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, PoisonError};
use std::time::Duration;

use sha2::Digest as _;

use crate::actions::{SwitchError, switch_profile_noninteractive};
use crate::daemon::build_status;
use crate::lock::StateLockTimeout;
use crate::lockorder::{RankedMutex, rank};
use crate::logline::logline;
use crate::oauth;
use crate::oauth_login::{Malformed, percent_decode_bytes};
use crate::profile::{ConfigHandle, origin_entry_fault};

use super::agent;
use super::chain;
use super::create;
use super::devices::{self, Device, Tier};
use super::events::__path_events;
use super::events::HerdrSeam;
use super::events::events as events_handler;
use super::gateway;
pub(crate) use super::http::ErrorBody;
use super::http::{Request, Response, SoloHeader, flatten_control_chars, sanitize_for_log};
use super::pairing::{self, Code, Redeemed};
use super::panes::{self, PaneProbe};
use super::proxies;
use super::push;
use super::sessions;
use super::terminal;
use super::ticket;

/// Every route lives under this prefix, and it is spelled once.
///
/// `/api/` keeps the daemon's own surface out of the way of anything a reverse
/// proxy in front of it may want to own, and the version segment is what makes a
/// breaking schema change additive: `/api/v2/status` can be served beside this
/// one rather than replacing it. Bumping it is a one-line change here, which is
/// the point of the constant — the route table below matches on the remainder.
pub(crate) const API_PREFIX: &str = "/api/v1";

/// What a route asks of its caller.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Access {
    /// No bearer at all: the pairing redemption, the one route a device
    /// reaches before it holds a token.
    None,
    /// Any paired device.
    View,
    /// A device paired with control.
    Control,
}

/// One row of [`ROUTES`].
pub(crate) struct Route {
    pub(crate) method: &'static str,
    /// The path under [`API_PREFIX`], as the OpenAPI document spells it: one
    /// segment may be [`ID_SEGMENT`], which matches any non-empty request
    /// segment [`decode_segment`] accepts and hands the decoded id to the
    /// handler as [`Caller::target`].
    pub(crate) path: &'static str,
    pub(crate) access: Access,
    handler: fn(&ApiContext, &Request, &Caller<'_>) -> Response,
}

/// The one path-parameter spelling a row may carry, utoipa's own, so the
/// route table and the document compare by the same string.
const ID_SEGMENT: &str = "{id}";
/// The second parameter, on the ask route: bound like [`ID_SEGMENT`], through
/// the same decoder, as the second element of the bound list.
const ASK_SEGMENT: &str = "{ask}";

/// A bound path segment as the client meant it: `%XX` pairs decoded to their
/// bytes, once, at the binding. The PWA's generator runtime (`openapi-fetch`)
/// substitutes a path parameter through `encodeURIComponent`, so every pane
/// id arrives as `w1N%3Ap19`; a `+` is a `+` (a path segment, not a form).
/// `None` for a `%` not followed by two hex digits or bytes that are not
/// UTF-8: the segment then matches nothing, and the request answers what an
/// unknown path answers.
pub(crate) fn decode_segment(segment: &str) -> Option<String> {
    let bytes = percent_decode_bytes(segment, false, Malformed::Refuse)?;
    String::from_utf8(bytes).ok()
}

/// Whether `path` is an instance of `template`: the same segments, with a
/// `{id}` or `{ask}` segment matching any non-empty request segment that
/// [`decode_segment`] accepts. The bound segments return in template order,
/// decoded; the terminal bridge binds its pane id through the same decoder.
fn match_template(template: &str, path: &str) -> Option<Vec<String>> {
    let mut want = template.split('/');
    let mut have = path.split('/');
    let mut bound = Vec::new();
    loop {
        match (want.next(), have.next()) {
            (None, None) => return Some(bound),
            (Some(ID_SEGMENT | ASK_SEGMENT), Some(segment)) if !segment.is_empty() => {
                bound.push(decode_segment(segment)?);
            }
            (Some(expected), Some(segment)) if expected == segment => {}
            _ => return None,
        }
    }
}

/// Every route the API serves. A path in no row is 404 and a known path with
/// the wrong method 405.
///
/// The HEAD rows serve their GET handler (RFC 9110 §9.3: the server SHOULD
/// respond as it would to GET, minus the content); the serve loop in `mod.rs`
/// strips the body off whatever comes back, at every status.
pub(crate) static ROUTES: &[Route] = &[
    Route {
        method: "GET",
        path: "/health",
        access: Access::View,
        handler: health,
    },
    Route {
        method: "HEAD",
        path: "/health",
        access: Access::View,
        handler: health,
    },
    Route {
        method: "GET",
        path: "/status",
        access: Access::View,
        handler: status,
    },
    Route {
        method: "HEAD",
        path: "/status",
        access: Access::View,
        handler: status,
    },
    Route {
        method: "GET",
        path: "/events",
        access: Access::View,
        handler: events_handler,
    },
    Route {
        method: "HEAD",
        path: "/events",
        access: Access::View,
        handler: events_handler,
    },
    Route {
        method: "GET",
        path: "/openapi.json",
        access: Access::View,
        handler: openapi_document,
    },
    Route {
        method: "HEAD",
        path: "/openapi.json",
        access: Access::View,
        handler: openapi_document,
    },
    Route {
        method: "POST",
        path: "/switch",
        access: Access::Control,
        handler: switch,
    },
    Route {
        method: "POST",
        path: "/chain/order",
        access: Access::Control,
        handler: chain::order,
    },
    Route {
        method: "POST",
        path: "/chain/threshold",
        access: Access::Control,
        handler: chain::threshold,
    },
    Route {
        method: "POST",
        path: "/chain/wrap-off",
        access: Access::Control,
        handler: chain::wrap_off,
    },
    Route {
        method: "POST",
        path: "/pair",
        access: Access::None,
        handler: pair,
    },
    Route {
        method: "GET",
        path: "/panes",
        access: Access::View,
        handler: panes::panes,
    },
    Route {
        method: "HEAD",
        path: "/panes",
        access: Access::View,
        handler: panes::panes,
    },
    Route {
        method: "GET",
        path: "/sessions",
        access: Access::View,
        handler: sessions::sessions,
    },
    Route {
        method: "HEAD",
        path: "/sessions",
        access: Access::View,
        handler: sessions::sessions,
    },
    Route {
        method: "POST",
        path: "/sessions",
        access: Access::Control,
        handler: create::create,
    },
    Route {
        method: "GET",
        path: "/sessions/{id}",
        access: Access::View,
        handler: sessions::session_history,
    },
    Route {
        method: "HEAD",
        path: "/sessions/{id}",
        access: Access::View,
        handler: sessions::session_history,
    },
    Route {
        method: "POST",
        path: "/panes/{id}/prompt",
        access: Access::Control,
        handler: agent::prompt,
    },
    Route {
        method: "POST",
        path: "/panes/{id}/keys",
        access: Access::Control,
        handler: agent::keys,
    },
    // A POST the view tier may make: it writes no file, and the ticket's stream
    // takes its mode from the device's row at the upgrade, so a mint grants
    // nothing the tier does not already hold.
    Route {
        method: "POST",
        path: "/panes/{id}/ticket",
        access: Access::View,
        handler: ticket::mint,
    },
    Route {
        method: "POST",
        path: "/sessions/{id}/prompt",
        access: Access::Control,
        handler: agent::session_prompt,
    },
    Route {
        method: "POST",
        path: "/sessions/{id}/interrupt",
        access: Access::Control,
        handler: agent::session_interrupt,
    },
    Route {
        method: "POST",
        path: "/sessions/{id}/asks/{ask}",
        access: Access::Control,
        handler: agent::session_answer,
    },
    Route {
        method: "GET",
        path: "/gateway",
        access: Access::View,
        handler: gateway::gateway,
    },
    Route {
        method: "HEAD",
        path: "/gateway",
        access: Access::View,
        handler: gateway::gateway,
    },
    Route {
        method: "GET",
        path: "/proxies",
        access: Access::View,
        handler: proxies::proxies,
    },
    Route {
        method: "HEAD",
        path: "/proxies",
        access: Access::View,
        handler: proxies::proxies,
    },
    Route {
        method: "GET",
        path: "/push/key",
        access: Access::View,
        handler: push::key,
    },
    Route {
        method: "HEAD",
        path: "/push/key",
        access: Access::View,
        handler: push::key,
    },
    // The two rows below are the one durable write the view tier can make, a
    // deliberate exception to "every mutating route needs control": a device
    // writes only its own subscription row, and the row reaches nothing the
    // tier does not already read. Two bounds hold it there: the endpoint must
    // be https on Google's, Apple's or Mozilla's push service, so a device
    // cannot point the daemon at any other host; and a push carries the event
    // kind and the account name alone, both already in the view feed.
    Route {
        method: "PUT",
        path: "/push/subscription",
        access: Access::View,
        handler: push::subscribe,
    },
    Route {
        method: "DELETE",
        path: "/push/subscription",
        access: Access::View,
        handler: push::unsubscribe,
    },
];

/// The methods a browser is told it may send, derived from [`ROUTES`] so a
/// route added with a new method cannot leave the header stale. First-appearance
/// order, one copy each: `GET, HEAD, POST, PUT, DELETE`.
pub(crate) fn advertised_methods() -> String {
    let mut methods: Vec<&str> = Vec::new();
    for route in ROUTES {
        if !methods.contains(&route.method) {
            methods.push(route.method);
        }
    }
    methods.join(", ")
}

/// Who a handler is answering, and what its path named.
pub(crate) struct Caller<'a> {
    pub(crate) peer: SocketAddr,
    /// The device the bearer authenticated as; `None` exactly on an
    /// [`Access::None`] route, which reads no bearer.
    pub(crate) device: Option<&'a Device>,
    /// The request segment the row's [`ID_SEGMENT`] bound — the pane or
    /// session the request names; `None` on a row without one.
    pub(crate) target: Option<&'a str>,
    /// The request segment the row's [`ASK_SEGMENT`] bound — the ask the
    /// request answers; `None` on a row without one.
    pub(crate) ask: Option<&'a str>,
}

impl Caller<'_> {
    /// The device's name as it may appear in a log line.
    pub(crate) fn device_for_log(&self) -> String {
        self.device
            .map_or_else(|| "-".to_string(), |device| sanitize_for_log(&device.name))
    }
}

/// An answer, and the device it went to: the serve loop names the device in
/// its per-request line and closes a connection no device earned.
pub(crate) struct Handled {
    pub(crate) response: Response,
    /// `None` for an unauthenticated request and for the pairing redemption.
    pub(crate) device: Option<String>,
    /// A validated WebSocket upgrade the connection loop runs itself; the
    /// response is never written (the loop writes the `101` head). `None` for
    /// every ordinary answer.
    pub(crate) hijack: Option<super::terminal::Hijack>,
}

/// Everything a request handler is allowed to touch.
pub(crate) struct ApiContext {
    pub(crate) config: ConfigHandle,
    /// `~/.clauth/status.json` — the feed the main loop rewrites each tick.
    pub(crate) status_path: PathBuf,
    /// One in-flight `POST /api/v1/switch` at a time. See [`rank::ApiSwitch`].
    pub(crate) switch_gate: RankedMutex<(), rank::ApiSwitch>,
    /// The scheduler's in-memory signals, when a daemon built this context.
    ///
    /// Every route that BUILDS a body rather than serving the published file
    /// needs them, or it answers with `fetch_status`, `next_refresh_at`, `stale`
    /// and `pending_switch` derived from a file mtime while the plain route,
    /// reading the file the scheduler wrote, carries the real ones. `None` only
    /// where there is no scheduler to ask — the tests that exercise a route
    /// without a daemon behind it.
    pub(crate) live: Option<crate::daemon::LiveStores>,
    /// The reading stores, when a daemon built this context: the REST switch
    /// drains the forgets its change site noted right after the locks drop.
    pub(crate) memory_stores: Option<crate::usage::MemoryStores>,
    /// The seam every bounded herdr call goes through (the pane route, the
    /// terminal bridge's snapshot, the agent routes); see [`super::panes`].
    pub(crate) herdr_probe: PaneProbe,
    /// Resolves herdr's API socket for `GET /events`. The daemon passes the
    /// production resolver; a test passes an explicit path or `None`.
    pub(crate) herdr: HerdrSeam,
    /// Spawns the `herdr terminal session …` child the WebSocket bridge
    /// relays. The daemon passes [`super::terminal::real_terminal_spawn`]; a
    /// test spawns a fixture script, and the default refuses so no test ever
    /// runs a real herdr.
    pub(crate) terminal_spawn: super::terminal::TerminalSpawn,
    /// Spawns a background session launch (`clauth start … -- --bg` or
    /// `claude --bg`). The daemon passes [`super::create::real_session_spawn`];
    /// a test records the launch and answers a fixture, and the default panics
    /// so no test ever runs a real `clauth`/`claude`.
    pub(crate) session_spawn: super::create::SessionSpawn,
    /// The mod bridge hub, when a `--listen` daemon bound the socket and built
    /// this context with it. `None` in tests without a bridge and on platforms
    /// with no bridge (Windows).
    #[cfg(unix)]
    pub(crate) bridge: Option<Arc<crate::daemon::bridge::Hub>>,
    /// Which device holds control of each pane: the terminal bridge records
    /// it, `GET /events` relays the contention it detects.
    pub(crate) terminal_holders: Arc<super::terminal::TerminalHolders>,
    /// The `[serve] app_origins` texts already logged as skipped, one line per
    /// distinct text per context (`note_skipped_origin`): the daemon's own.
    pub(crate) skipped_origins: RankedMutex<BTreeSet<String>, rank::SkippedOrigins>,
    /// The last `[serve] app_origins` list that read (`listed_origins`), the
    /// fallback a failed or missing read answers from.
    pub(crate) last_origins: RankedMutex<Option<Arc<[String]>>, rank::LastOrigins>,
    /// The one-line-per-failure-span latch on [`listed_origins`]: re-arms on
    /// the next read that succeeds.
    pub(crate) origins_failed_noted: AtomicBool,
    /// The live terminal tickets. Memory only, so a restart empties the table.
    pub(crate) tickets: super::ticket::Tickets,
}

impl ApiContext {
    /// The daemon's constructor: both herdr seams are passed explicitly, the
    /// pane probe for `GET /panes` and the socket resolver for `GET /events`,
    /// and the mod bridge hub (unix) — `None` when the bind failed or a test
    /// runs without one (SB-1).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        config: ConfigHandle,
        status_path: PathBuf,
        live: Option<crate::daemon::LiveStores>,
        memory_stores: Option<crate::usage::MemoryStores>,
        herdr_probe: PaneProbe,
        herdr: HerdrSeam,
        terminal_spawn: super::terminal::TerminalSpawn,
        session_spawn: super::create::SessionSpawn,
        #[cfg(unix)] bridge: Option<Arc<crate::daemon::bridge::Hub>>,
    ) -> Arc<Self> {
        Arc::new(Self {
            config,
            status_path,
            switch_gate: RankedMutex::new(()),
            live,
            memory_stores,
            herdr_probe,
            herdr,
            terminal_spawn,
            session_spawn,
            #[cfg(unix)]
            bridge,
            terminal_holders: Arc::new(super::terminal::TerminalHolders::new()),
            skipped_origins: RankedMutex::new(BTreeSet::new()),
            last_origins: RankedMutex::new(None),
            origins_failed_noted: AtomicBool::new(false),
            tickets: super::ticket::Tickets::new(Box::new(super::ticket::Stamp::now)),
        })
    }

    /// The test constructor: no herdr socket resolver, so a stream never probes
    /// a real socket; the pane probe is whatever the test hands over, and the
    /// terminal spawn refuses rather than reach a real herdr.
    #[cfg(test)]
    pub(crate) fn for_tests(
        config: ConfigHandle,
        status_path: PathBuf,
        live: Option<crate::daemon::LiveStores>,
        herdr_probe: PaneProbe,
    ) -> Arc<Self> {
        Self::new(
            config,
            status_path,
            live,
            None,
            herdr_probe,
            Arc::new(|| None),
            super::terminal::unspawnable_terminal(),
            super::create::unspawnable_session(),
            #[cfg(unix)]
            None,
        )
    }

    /// The test constructor over a bridge hub: the bridge the tests drive and a
    /// pane probe that answers nothing by default.
    #[cfg(all(test, unix))]
    pub(crate) fn for_tests_with_bridge(
        config: ConfigHandle,
        status_path: PathBuf,
        herdr_probe: PaneProbe,
        bridge: Arc<crate::daemon::bridge::Hub>,
    ) -> Arc<Self> {
        Self::new(
            config,
            status_path,
            None,
            None,
            herdr_probe,
            Arc::new(|| None),
            super::terminal::unspawnable_terminal(),
            super::create::unspawnable_session(),
            Some(bridge),
        )
    }

    /// The test constructor with an explicit session spawn: the pane probe and
    /// the launch seam are both whatever the test hands over. Unix-gated: its
    /// only callers are the unix-gated sessions test modules.
    #[cfg(all(test, unix))]
    pub(crate) fn for_tests_with_spawn(
        config: ConfigHandle,
        status_path: PathBuf,
        herdr_probe: PaneProbe,
        session_spawn: super::create::SessionSpawn,
    ) -> Arc<Self> {
        Self::new(
            config,
            status_path,
            None,
            None,
            herdr_probe,
            Arc::new(|| None),
            super::terminal::unspawnable_terminal(),
            session_spawn,
            #[cfg(unix)]
            None,
        )
    }
}

/// Republish the feed now rather than leaving it to the next scheduler tick: a
/// snapshot cloned out of the config mutex plus the live signals, written
/// before the answer so every `GET /api/v1/status?wait=` reader wakes on the
/// edit. The one copy `switch` and the chain routes share.
pub(crate) fn republish(ctx: &ApiContext) {
    let live = ctx.live.as_ref().map(crate::daemon::LiveStores::snapshot);
    let snapshot = {
        #[allow(
            clippy::expect_used,
            reason = "config mutex poisoning is unrecoverable"
        )]
        let cfg = ctx.config.lock().expect("config mutex poisoned");
        cfg.clone()
    };
    crate::daemon::write_status_feed(
        &snapshot,
        live.as_ref()
            .map(crate::daemon::LiveSnapshot::signals)
            .as_ref(),
    );
}

/// The reason every failed pairing redemption carries, whatever failed.
const PAIRING_REFUSED: &str = "that code did not pair a device: check it, or run `clauth devices \
                               pair <name>` on the host for a new one";
/// The reason a view-only device gets from a route that needs control.
const CONTROL_REQUIRED: &str = "this device is paired view-only; this needs a device paired with \
                                `clauth devices pair <name> --control` on the host";
/// The reason a device with a tier this build does not know gets everywhere.
const TIER_UNKNOWN: &str = "this device was paired by a newer clauth with a tier this one does \
                            not know; run that clauth, or revoke the device and pair it again";
/// The reason a bearer gets from a page whose origin the host does not list.
const ORIGIN_NOT_ALLOWED: &str = "this page's origin is not in `[serve] app_origins` in \
                                  profiles.toml on the host";
/// The reason a bearer gets from a page whose `Origin` no list entry can equal.
const ORIGIN_UNLISTABLE: &str = "this page sends an origin `[serve] app_origins` can never list \
                                 (`null`, sent twice, or not UTF-8); open the app from the web \
                                 address it is served at";

/// The reason an `origin_not_allowed` refusal of `origin` carries. Entry
/// validation rejects `null`, and a repeated or non-UTF-8 header equals no
/// entry, so no edit to the list can let those in.
pub(super) fn origin_refusal(origin: &SoloHeader) -> &'static str {
    match origin {
        SoloHeader::Unusable => ORIGIN_UNLISTABLE,
        SoloHeader::One(origin) if origin == "null" => ORIGIN_UNLISTABLE,
        SoloHeader::Absent | SoloHeader::One(_) => ORIGIN_NOT_ALLOWED,
    }
}

/// `[serve] app_origins` as this request sees it: read fresh, so an edit
/// applies to the next request, and only the entries that count. A
/// `profiles.toml` that does not read — or that is missing while a list is
/// held, as it is between a rename-aside save's rename and its recreate —
/// answers from the last list that did, so an editor save cannot turn the
/// gate off for a removed origin's cached preflight; with no list read yet
/// the answer is empty, the behaviour of an empty list. The last list, the
/// skip set and the failure latch live on the context, one daemon's own:
/// each skip logs once per distinct text, and each failure span logs one line
/// (the latch re-arms on the next read that succeeds), where the list read
/// per request would otherwise write one line per request for the daemon's
/// life.
pub(crate) fn listed_origins(ctx: &ApiContext) -> Arc<[String]> {
    let state = match crate::profile::read_app_state() {
        Ok(Some(state)) => state,
        // While a list is held, the file being gone is a failed read, not a
        // good empty one, or a save that renames the file aside would drop the
        // gate and overwrite the memory. With no list held the missing file is
        // the first run: an empty list, silently.
        Ok(None) => {
            let kept = ctx
                .last_origins
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone();
            if kept.is_some() && !ctx.origins_failed_noted.swap(true, Ordering::AcqRel) {
                logline!(
                    "clauth api: profiles.toml is missing; the last [serve] app_origins \
                     list stands until it reads"
                );
            }
            return kept.unwrap_or_else(|| Arc::from(Vec::new()));
        }
        Err(e) => {
            let kept = ctx
                .last_origins
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone();
            if !ctx.origins_failed_noted.swap(true, Ordering::AcqRel) {
                logline!(
                    "clauth api: profiles.toml does not read; {}: {}",
                    if kept.is_some() {
                        "the last [serve] app_origins list stands until it reads"
                    } else {
                        "no [serve] app_origins entry counts until it reads"
                    },
                    sanitize_for_log(&format!("{e:#}"))
                );
            }
            return kept.unwrap_or_else(|| Arc::from(Vec::new()));
        }
    };
    ctx.origins_failed_noted.store(false, Ordering::Release);
    for value in &state.serve.app_origins.unreadable {
        note_skipped_origin(ctx, value, "it is not a string in a list");
    }
    let mut listed = state.serve.app_origins.entries;
    listed.retain(|entry| match origin_entry_fault(entry) {
        Some(reason) => {
            note_skipped_origin(ctx, entry, reason);
            false
        }
        None => true,
    });
    let listed: Arc<[String]> = Arc::from(listed);
    *ctx.last_origins
        .lock()
        .unwrap_or_else(PoisonError::into_inner) = Some(Arc::clone(&listed));
    listed
}

/// One line per distinct text per context, whatever reads it how often.
fn note_skipped_origin(ctx: &ApiContext, text: &str, reason: &str) {
    let first = {
        let mut noted = ctx
            .skipped_origins
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if noted.contains(text) {
            false
        } else {
            noted.insert(text.to_string());
            true
        }
    };
    if first {
        logline!(
            "clauth api: skipping {:?} in [serve] app_origins: {reason}",
            sanitize_for_log(text)
        );
    }
}

/// Resolve the route, authenticate, check the route's access, dispatch.
///
/// Authentication runs before the path is judged, an unknown path included, so
/// an unpaired caller learns nothing past a 401 about which paths exist. The
/// pairing redemption is the one route that reads no bearer, since a device
/// holds none until it succeeds. The stream's upgrade is the one request a
/// ticket can stand in for a bearer on: read only when no `Authorization`
/// header was presented and on that path, so a bearer decides alone wherever
/// one is sent, and its device reaches the bridge and never the route table
/// ([`ticket`]).
pub(crate) fn handle(ctx: &ApiContext, req: &Request, peer: SocketAddr) -> Handled {
    let path = req.path.strip_prefix(API_PREFIX);
    let route = path.and_then(|path| {
        ROUTES.iter().find_map(|route| {
            (route.method == req.method)
                .then(|| match_template(route.path, path))
                .flatten()
                .map(|bound| (route, bound))
        })
    });
    if let Some((route, bound)) = &route
        && route.access == Access::None
    {
        return Handled {
            response: (route.handler)(
                ctx,
                req,
                &Caller {
                    peer,
                    device: None,
                    target: bound.first().map(String::as_str),
                    ask: bound.get(1).map(String::as_str),
                },
            ),
            device: None,
            hijack: None,
        };
    }

    let device = match devices::authenticate(req.bearer.as_deref()) {
        Ok(Some(device)) => device,
        Ok(None) => {
            if !req.presented_authorization()
                && let Some(pane_id) = path.and_then(terminal::pane_stream_target)
                && let Some(ticket) = req.ws.offer.ticket()
            {
                return ticket::attach(ctx, req, ticket, &pane_id);
            }
            return Handled {
                response: Response::unauthorized(),
                device: None,
                hijack: None,
            };
        }
        Err(e) => return refuse_unread_devices(&e),
    };

    // CORS reaches no write and no upgrade, so a removed origin whose page still
    // holds a token and a cached preflight stops here. A device list refusal and
    // an unpaired caller's 401 both come first.
    if req.cross_origin.refused {
        return Handled {
            response: Response::refused(
                403,
                "origin_not_allowed",
                origin_refusal(&req.cross_origin.origin),
            ),
            device: Some(device.name),
            hijack: None,
        };
    }

    // The one parametrized path, and the only route that can answer an
    // upgrade: `/panes/<id>/stream`. It sits outside the route table because a
    // WebSocket is not an HTTP resource — OpenAPI covers HTTP only, and the
    // frame vocabulary is hand-written in the plan doc.
    if let Some(pane_id) = path.and_then(terminal::pane_stream_target) {
        return terminal::request(ctx, req, &device, &pane_id, terminal::Credential::Bearer);
    }

    let response = match route {
        Some((route, bound)) => match authorize(&device.tier, route.access) {
            Grant::Allowed => (route.handler)(
                ctx,
                req,
                &Caller {
                    peer,
                    device: Some(&device),
                    target: bound.first().map(String::as_str),
                    ask: bound.get(1).map(String::as_str),
                },
            ),
            Grant::NeedsControl => Response::refused(403, "control_required", CONTROL_REQUIRED),
            Grant::TierUnknown => refuse_unknown_tier(&device),
        },
        // A known path reached with the wrong method is 405, so a client with a
        // typo'd verb gets told which half is wrong.
        None if path.is_some_and(|path| {
            ROUTES
                .iter()
                .any(|route| match_template(route.path, path).is_some())
        }) =>
        {
            Response::error(405, "method_not_allowed")
        }
        None => Response::error(404, "not_found"),
    };
    Handled {
        response,
        device: Some(device.name),
        hijack: None,
    }
}

/// The one-shot latch behind the line an unreadable device list earns: the store
/// is read per request and per stream, so an unlatched line would be one line per
/// reader for the daemon's life.
pub(super) static READ_FAILED_NOTED: AtomicBool = AtomicBool::new(false);

/// The one line an unreadable device list earns, whichever reader met it: a
/// request's refusal, an open terminal stream, or an `/events` stream. The
/// per-request line names the route and status only, so the cause has to be
/// carried by this one line or nowhere.
pub(super) fn note_unread_devices(e: &anyhow::Error) {
    if !READ_FAILED_NOTED.swap(true, Ordering::AcqRel) {
        logline!("clauth api: refusing every request until the device list reads: {e:#}");
    }
}

/// The answer to a device list that does not read, whichever credential met it.
pub(super) fn refuse_unread_devices(e: &anyhow::Error) -> Handled {
    note_unread_devices(e);
    Handled {
        response: Response::error(500, "internal"),
        device: None,
        hijack: None,
    }
}

/// The one-shot latch behind [`refuse_unknown_tier`]'s log line.
static UNKNOWN_TIER_NOTED: AtomicBool = AtomicBool::new(false);

/// The refusal every route gives a device carrying a tier this build does not
/// know, logged once per process. Shared by the route table's arm and the
/// terminal bridge, which runs before the table.
pub(crate) fn refuse_unknown_tier(device: &devices::Device) -> Response {
    if !UNKNOWN_TIER_NOTED.swap(true, Ordering::AcqRel) {
        logline!(
            "clauth api: device '{}' carries tier {:?}, which this build does not \
             know, so every route refuses it; run the clauth that paired it",
            sanitize_for_log(&device.name),
            sanitize_for_log(device.tier.as_str())
        );
    }
    Response::refused(403, "device_tier_unknown", TIER_UNKNOWN)
}

/// What [`authorize`] decided.
enum Grant {
    Allowed,
    NeedsControl,
    TierUnknown,
}

/// The one capability check, deny by default: every pairing of a tier with an
/// access level is spelled out, so a new tier or access level does not compile
/// until someone decides what it grants.
fn authorize(tier: &Tier, access: Access) -> Grant {
    match (tier, access) {
        (_, Access::None)
        | (Tier::Control, Access::View | Access::Control)
        | (Tier::View, Access::View) => Grant::Allowed,
        (Tier::View, Access::Control) => Grant::NeedsControl,
        (Tier::Unknown(_), Access::View | Access::Control) => Grant::TierUnknown,
    }
}

/// `GET /api/v1/health` — the build's version and the feed schema, so a client
/// refuses a daemon newer than it knows.
#[derive(serde::Serialize, utoipa::ToSchema)]
pub(crate) struct HealthBody {
    ok: bool,
    version: String,
    schema: u64,
}

#[utoipa::path(
    get,
    path = "/api/v1/health",
    responses(
        (status = 200, description = "the build's version and the feed schema", body = HealthBody),
        (status = 401, description = "no bearer, or one matching no paired device (`unauthorized`)", body = ErrorBody),
        (status = 403, description = "a page's origin outside the app_origins list (`origin_not_allowed`); a device paired by a newer clauth with a tier this one does not know (`device_tier_unknown`)", body = ErrorBody),
        (status = 500, description = "the device list does not read (`internal`)", body = ErrorBody)
    ),
    security(("bearer" = ["view"]))
)]
fn health(_: &ApiContext, _: &Request, _: &Caller<'_>) -> Response {
    Response::serialize(
        200,
        &HealthBody {
            ok: true,
            version: env!("CARGO_PKG_VERSION").to_string(),
            schema: crate::daemon::SCHEMA_VERSION,
        },
    )
}

/// `GET /api/v1/status` — the same body `~/.clauth/status.json` carries.
///
/// Served straight off disk in the common case. The main loop already rewrites
/// that file atomically every tick, so passing the bytes through takes no lock,
/// duplicates no serialization, and cannot drift from the documented schema.
/// `?all=1` and a missing file both fall back to building a body here, which is
/// the same `build_status` the file itself came from.
///
/// Conditional, and optionally BLOCKING: `?wait=` with a matching
/// `If-None-Match` holds the request open until the feed's content actually
/// changes. That is what turns a client's account display from
/// "correct within its poll interval" into "correct within a round trip", and
/// with `POST /api/v1/switch` republishing the file itself, a switch made through
/// the API wakes every waiting reader immediately.
///
/// `?all=1` never waits: it builds its body from config rather than the file,
/// so there is no file to watch for it — but it is still conditional off its
/// built body's tag, so a roster that has not moved answers 304.
#[utoipa::path(
    get,
    path = "/api/v1/status",
    params(
        ("all" = Option<bool>, Query, description = "include disabled accounts (`all=1`)"),
        ("wait" = Option<u64>, Query, description = "hold the request until the feed changes, at most 60 seconds, only with a matching If-None-Match", maximum = 60),
        ("If-None-Match" = Option<String>, Header, description = "an `ETag` from an earlier answer; a match answers 304 or, with `wait`, holds the request")
    ),
    responses(
        (status = 200, description = "the status feed", body = crate::daemon::status_json::StatusBody, headers(("ETag" = String, description = "the feed's entity tag"))),
        (status = 304, description = "the feed has not changed", headers(("ETag" = String, description = "the feed's entity tag"))),
        (status = 401, description = "no bearer, or one matching no paired device (`unauthorized`)", body = ErrorBody),
        (status = 403, description = "a page's origin outside the app_origins list (`origin_not_allowed`); a device paired by a newer clauth with a tier this one does not know (`device_tier_unknown`)", body = ErrorBody),
        (status = 500, description = "the device list does not read, or the status body failed to serialize (`internal`)", body = ErrorBody)
    ),
    security(("bearer" = ["view"]))
)]
fn status(ctx: &ApiContext, req: &Request, caller: &Caller<'_>) -> Response {
    let include_disabled = req.flag("all");
    if !include_disabled {
        let waited = req
            .param("wait")
            .and_then(|v| v.parse::<u64>().ok())
            .map(|secs| Duration::from_secs(secs.min(MAX_WAIT_SECS)));
        // Only a client that says what it already holds can be made to wait;
        // with no tag every answer is a change, so there is nothing to wait for.
        if let (Some(wait), Some(tag)) = (waited, req.if_none_match.as_deref()) {
            return wait_for_status_change(ctx, tag, wait, caller);
        }
        // A body caught mid-replacement never reaches the client here either:
        // it does not parse, so the read falls through to the rebuild below —
        // the same answer a missing file gets — instead of handing out the
        // truncated bytes with a tag fabricated from them, the one shape the
        // wait loop already refuses.
        if let Some((body, etag)) = read_feed_tagged(&ctx.status_path) {
            if req.if_none_match.as_deref() == Some(etag.as_str()) {
                return Response::not_modified(etag);
            }
            return Response::raw_json_tagged(200, body, etag);
        }
    }
    // No file yet (the daemon is still in its first tick), or the caller asked
    // for the disabled accounts the published feed always hides.
    //
    // The live stores are snapshotted FIRST and their locks released inside
    // `snapshot`, so nothing below holds one when CONFIG — which outranks every
    // one of them — is taken next.
    let live = ctx.live.as_ref().map(crate::daemon::LiveStores::snapshot);
    let (snapshot, interval) = {
        #[allow(
            clippy::expect_used,
            reason = "config mutex poisoning is unrecoverable"
        )]
        let cfg = ctx.config.lock().expect("config mutex poisoned");
        (cfg.clone(), cfg.state.refresh_interval_ms)
    };
    let body = build_status(
        &snapshot,
        interval,
        live.as_ref()
            .map(crate::daemon::LiveSnapshot::signals)
            .as_ref(),
        include_disabled,
    );
    match serde_json::to_vec(&body) {
        Ok(bytes) => {
            // The one `etag_for` the whole route family uses, so a client's
            // conditional-request code works against the query the same way it
            // does against the published feed: `?all` waits on nothing (there
            // is no file to watch), but an unchanged roster is still answered
            // 304 rather than resending every profile entry on every poll.
            let etag = etag_for(&bytes);
            if req.if_none_match.as_deref() == Some(etag.as_str()) {
                return Response::not_modified(etag);
            }
            Response::raw_json_tagged(200, bytes, etag)
        }
        Err(e) => {
            logline!("clauth api: failed to serialize a status body: {e}");
            Response::error(500, "internal")
        }
    }
}

/// Block until `status.json`'s content leaves `tag`, or `wait` elapses.
///
/// Content, not mtime. The feed is a single small file, so re-reading and
/// digesting it every [`WAIT_POLL`] costs almost nothing — and unlike an mtime
/// it cannot be fooled. (A filesystem that stamps two writes microseconds apart
/// with one mtime is not hypothetical; comparing content is immune to it.)
///
/// Both answers pass through [`waited_answer`]: a device revoked while the
/// request is parked got through the authentication at its start, so the row is
/// read again before this answers, once per answer and never per poll tick.
fn wait_for_status_change(
    ctx: &ApiContext,
    tag: &str,
    wait: Duration,
    caller: &Caller<'_>,
) -> Response {
    let deadline = std::time::Instant::now() + wait;
    loop {
        // Read before the deadline is tested: a zero-wait conditional request is
        // a legitimate "has it moved?" probe, and every non-zero wait gets its
        // first read at once instead of one poll interval in.
        //
        // A file caught mid-replacement is not this request's problem: keep
        // waiting rather than handing the client an error to interpret — and a
        // body that does not parse is exactly that, never a change to answer
        // with a tag fabricated from the raw bytes.
        if let Some((body, etag)) = read_feed_tagged(&ctx.status_path)
            && etag != tag
        {
            return waited_answer(caller, || Response::raw_json_tagged(200, body, etag));
        }
        let now = std::time::Instant::now();
        if now >= deadline {
            return waited_answer(caller, || Response::not_modified(tag.to_string()));
        }
        std::thread::sleep(WAIT_POLL.min(deadline - now));
    }
}

/// The answer a long poll owes once it is ready: its caller's row, read fresh,
/// decides whether the answer reaches it. A revoke during the wait is a CLI
/// writing a file after this request authenticated, so its change answer and its
/// deadline answer both answer the 401 its next request would get; a list that
/// does not read answers as it does at a request's start. A caller with no
/// device, which the route table does not build on this path, fails closed with
/// the 500 the other in-handler re-checks answer. One read per answer.
fn waited_answer(caller: &Caller<'_>, answer: impl FnOnce() -> Response) -> Response {
    let Some(device) = caller.device else {
        // No device means no row to re-read, and the feed must not go out
        // unread: answer as the store read failure below does.
        return Response::error(500, "internal");
    };
    match devices::by_pairing_id(&device.pairing_id()) {
        Ok(Some(_)) => answer(),
        Ok(None) => Response::unauthorized(),
        Err(e) => refuse_unread_devices(&e).response,
    }
}

/// The published feed, parsed-then-tagged, in one step: `None` when the file is
/// missing or does not parse. The long poll and the events stream both call
/// this, so the two cannot disagree about what counts as a change or about the
/// torn-file rule (a body that does not parse is never served, tagged, or
/// streamed).
pub(crate) fn read_feed_tagged(status_path: &Path) -> Option<(Vec<u8>, String)> {
    let body = std::fs::read(status_path).ok()?;
    serde_json::from_slice::<serde_json::Value>(&body).ok()?;
    let etag = etag_for(&body);
    Some((body, etag))
}

/// The feed's entity tag: a digest of everything in the body a reader could act
/// on, so it changes when and only when they would see something different.
/// Quoted, as HTTP wants.
///
/// `generated_at` is excluded, and that exclusion is the difference between a
/// long poll and a one-second one: the main loop rewrites the feed every tick,
/// and on a quiet system that stamp is the ONLY field that moves. Digesting it
/// would wake every waiting reader once a second to hand them a body identical
/// in every respect they care about.
///
/// A body that will not parse is digested whole. That is the safe direction: a
/// tag that changes too often costs a wakeup, while one that changes too rarely
/// leaves a reader showing an account the operator has already left.
pub(crate) fn etag_for(body: &[u8]) -> String {
    let meaningful = serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|mut value| {
            value.as_object_mut()?.remove("generated_at");
            serde_json::to_vec(&value).ok()
        });
    let bytes = meaningful.as_deref().unwrap_or(body);
    format!(
        "\"{}\"",
        hex::encode(<[u8; 32]>::from(sha2::Sha256::digest(bytes)))
    )
}

/// Longest a `?wait` may hold a connection. Comfortably inside `Limits`'
/// 120-second connection lifetime, so a waiting request always gets to answer on
/// the connection it arrived on.
const MAX_WAIT_SECS: u64 = 60;

/// How often a wait re-checks. Short enough that a switch reads as instant.
/// The events stream polls the feed at the same cadence.
pub(crate) const WAIT_POLL: Duration = Duration::from_millis(250);

/// The one field `POST /api/v1/switch` accepts.
#[derive(serde::Deserialize, utoipa::ToSchema)]
pub(crate) struct SwitchBody {
    profile: String,
}

/// The answer a successful switch carries: the profile left behind and the one
/// now active. `previous` is `None` exactly when there was no active profile to
/// leave, which today's wire answers as `null` — so the `Option` is serialized,
/// never skipped.
#[derive(serde::Serialize, utoipa::ToSchema)]
pub(crate) struct SwitchOk {
    ok: bool,
    #[schema(required = true)]
    previous: Option<String>,
    active: String,
}

/// `POST /api/v1/switch` — relink the global active profile.
///
/// A thin wrapper over [`switch_profile_noninteractive`], the same action the
/// MCP `switch` tool calls. That is deliberate and load-bearing: the AUTH-1
/// gate (never install credentials a refresh has rejected), the disabled-target
/// refusal, and the divergence policy all live inside it, so this endpoint
/// cannot drift into a weaker switch than the rest of clauth performs.
#[utoipa::path(
    post,
    path = "/api/v1/switch",
    request_body = SwitchBody,
    responses(
        (status = 200, description = "the switch landed; the profile left behind and the one now active", body = SwitchOk),
        (status = 400, description = "the body held no parseable profile (`bad_request`)", body = ErrorBody),
        (status = 401, description = "no bearer, or one matching no paired device (`unauthorized`)", body = ErrorBody),
        (status = 403, description = "a page's origin outside the app_origins list (`origin_not_allowed`); a device paired by a newer clauth with a tier this one does not know (`device_tier_unknown`), or a view-only device (`control_required`)", body = ErrorBody),
        (status = 404, description = "the profile is not stored (`profile_not_found`)", body = ErrorBody),
        (status = 409, description = "a switch is already in flight (`switch_in_progress`), or a switch was refused (`switch_refused`)", body = ErrorBody),
        (status = 503, description = "the state flock is held (`state_locked`; retry shortly), or the daemon is stopping (`daemon_stopping`; retry once it is back)", body = ErrorBody),
        (status = 500, description = "the device list does not read (`internal`), or the switch failed (`switch_failed`)", body = ErrorBody)
    ),
    security(("bearer" = ["control"]))
)]
fn switch(ctx: &ApiContext, req: &Request, caller: &Caller<'_>) -> Response {
    let Ok(parsed) = serde_json::from_slice::<SwitchBody>(&req.body) else {
        return Response::error(400, "bad_request");
    };

    // Resolve to a stored profile BEFORE any mutation — the guard the CLI and
    // MCP paths both apply. Without it an unknown name reaches
    // `link_profile_credentials`, which strips the live credential symlink and
    // creates no replacement, leaving the global session logged out.
    let (canonical, on_divergence) = {
        #[allow(
            clippy::expect_used,
            reason = "config mutex poisoning is unrecoverable"
        )]
        let cfg = ctx.config.lock().expect("config mutex poisoned");
        (
            cfg.canonical_name(&parsed.profile),
            cfg.state.default_divergence,
        )
    };
    let Some(canonical) = canonical else {
        return Response::error(404, "profile_not_found");
    };

    // One switch at a time. Without this, a second request parks on the
    // cross-process state flock for its full 25s deadline and then fails
    // anyway; a 409 now is the honest answer.
    let Ok(_gate) = ctx.switch_gate.try_lock() else {
        return Response::error(409, "switch_in_progress");
    };

    // The switch's AUTH-1 gate can refresh the target on THIS request thread,
    // and a stop signal does not see that thread: the spend marker travels into
    // the gate, which claims it under the rotation lock right before the POST.
    // A stop that lands while the gate is parked then refuses the spend (the
    // gate answers `Stopping`), and a stop during the save is waited out by the
    // drain.
    let target = crate::profile::ProfileName::from(canonical.as_str());
    let spend_marker = match &ctx.live {
        Some(live) => {
            // A stopping daemon takes no new switch, a no-spend one included:
            // the exit waits out a token spend, never a switch's install. A
            // stop landing after this check is refused inside the gate, which
            // reads the flag under the rotation lock before any token request.
            if live.shutting_down.load(Ordering::SeqCst) {
                return Response::error(503, "daemon_stopping");
            }
            Some((&live.activity, live.shutting_down.as_ref().into()))
        }
        // No daemon built this context, so no stop path can cut it.
        None => None,
    };

    match switch_profile_noninteractive(
        &ctx.config,
        &target,
        on_divergence,
        oauth::refresh_result,
        spend_marker,
    ) {
        // A bare-item notice is on daemon.log already: the body's schema has
        // no text field to carry it.
        Ok((previous, active, _notice)) => {
            logline!(
                "clauth api: device '{}' switched to '{active}'",
                caller.device_for_log()
            );
            // Apply now that the state flock and config guard dropped, before
            // the republish so the feed reflects the forget. This drain runs on
            // the request thread, so a main-loop compare can overlap it and
            // forget this switch's change a second time, counting it external
            // (one extra reload). The second forget also clears memory-only
            // state that landed between the two (a fetch's streak and cadence,
            // a kick block, the `/profile` memo); readings, status and leg come
            // back from the disk re-seed, the epoch is monotonic and the
            // re-seed idempotent, and the compare's per-half CAS keeps the
            // COMPARE from clobbering a concurrent refresh, so the cost is at
            // most one extra request, never a wrong figure.
            if let Some(stores) = &ctx.memory_stores {
                crate::usage::apply_pending_forgets(stores);
            }
            republish(ctx);
            Response::serialize(
                200,
                &SwitchOk {
                    ok: true,
                    previous,
                    active,
                },
            )
        }
        Err(e) => {
            // Apply so a half-switched capture does not leave the old reading
            // in the daemon's stores.
            if let Some(stores) = &ctx.memory_stores {
                crate::usage::apply_pending_forgets(stores);
            }
            // The chain (`{:#}`, every context anyhow carries) goes to
            // daemon.log, the surface the operator owns — bounded like every
            // logline is; the body keeps only what the closed set reflects. A
            // `Failed` chain carries absolute home paths in its contexts
            // (`failed to publish /home/…/credentials.json`), and a body is
            // the one surface handed to a remote reader.
            logline!(
                "clauth api: device '{}' switch to '{canonical}' refused: {}",
                caller.device_for_log(),
                sanitize_for_log(&format!("{e:#}"))
            );
            // A held state flock is the one retryable failure here: another
            // clauth process is mid-write, and the same request will work in a
            // moment. Everything else needs the operator to change something.
            if let Some(timeout) = e.state_lock_timeout() {
                Response::refused(
                    503,
                    "state_locked",
                    &flatten_control_chars(&timeout.to_string()),
                )
            } else if let Some(sentence) = e.deep_refusal() {
                // A refusal authored by a leg's own gate (the deep membership
                // re-read under the state flock): the sentence is the same
                // closed set, so it reflects like the arms above.
                Response::refused(409, "switch_refused", &sentence)
            } else {
                match &e {
                    SwitchError::Refused(sentence) | SwitchError::Deferred(sentence) => {
                        Response::refused(409, "switch_refused", sentence)
                    }
                    // The daemon's stop flag refused the gate's refresh under
                    // the rotation lock: nothing was sent, retry once it is back.
                    SwitchError::Stopping => Response::error(503, "daemon_stopping"),
                    // The reflected copy is a fixed literal, so it needs no
                    // sanitize; the variable chain stays in daemon.log.
                    SwitchError::Failed(_) => {
                        Response::refused(500, "switch_failed", "the switch failed; see daemon.log")
                    }
                }
            }
        }
    }
}

/// The one field `POST /api/v1/pair` accepts.
#[derive(serde::Deserialize, utoipa::ToSchema)]
pub(crate) struct PairBody {
    code: String,
}

/// The answer a successful pairing carries: the device's name and tier, and the
/// one-time token the device keeps from here.
#[derive(serde::Serialize, utoipa::ToSchema)]
pub(crate) struct PairOk {
    ok: bool,
    name: String,
    tier: String,
    token: String,
}

/// `POST /api/v1/pair` — redeem the live pairing code for a device token.
///
/// Every failed redemption (a wrong code, a spent one, an expired one, none
/// live) answers the same `403 pairing_refused`, so a guesser learns nothing
/// about the host's state from the answer. A body that holds no code at all is
/// `400 bad_request` and costs no attempt: it is judged before the pairing is
/// read. The token rides this one response and no log line.
///
/// Any `Content-Type` but `application/json` is `415`, judged before the body
/// and on every config: a page can post `text/plain` with no preflight, and
/// the browser hiding the answer would not stop the try being spent.
#[utoipa::path(
    post,
    path = "/api/v1/pair",
    request_body = PairBody,
    responses(
        (status = 201, description = "the device is paired; the token rides this one response", body = PairOk),
        (status = 400, description = "the body held no code (`bad_request`)", body = ErrorBody),
        (status = 403, description = "the code did not pair a device (`pairing_refused`)", body = ErrorBody),
        (status = 415, description = "the Content-Type is not application/json (`unsupported_media_type`)", body = ErrorBody),
        (status = 503, description = "the state flock is held (`state_locked`)", body = ErrorBody),
        (status = 500, description = "pairing failed (`internal`)", body = ErrorBody)
    )
)]
fn pair(_: &ApiContext, req: &Request, caller: &Caller<'_>) -> Response {
    if !req.sends_json() {
        return Response::error(415, "unsupported_media_type");
    }
    let Some(code) = serde_json::from_slice::<PairBody>(&req.body)
        .ok()
        .and_then(|body| Code::normalize(&body.code))
    else {
        return Response::error(400, "bad_request");
    };
    match pairing::redeem(&code) {
        Ok(Redeemed::Paired { name, tier, token }) => {
            logline!(
                "clauth api: {} paired device '{}' ({})",
                caller.peer,
                sanitize_for_log(&name),
                sanitize_for_log(tier.as_str())
            );
            Response::serialize(
                201,
                &PairOk {
                    ok: true,
                    name,
                    tier: tier.as_str().to_string(),
                    token,
                },
            )
        }
        Ok(Redeemed::Refused) => Response::refused(403, "pairing_refused", PAIRING_REFUSED),
        Err(e) => {
            logline!(
                "clauth api: {} pairing failed: {}",
                caller.peer,
                sanitize_for_log(&format!("{e:#}"))
            );
            match e
                .chain()
                .find_map(|cause| cause.downcast_ref::<StateLockTimeout>())
            {
                Some(timeout) => Response::refused(
                    503,
                    "state_locked",
                    &flatten_control_chars(&timeout.to_string()),
                ),
                None => Response::error(500, "internal"),
            }
        }
    }
}

/// The one OpenAPI document, derived from the handlers. Every route's
/// `#[utoipa::path]` feeds this struct, so the operations it emits are the
/// same rows [`ROUTES`] serves; the test pins the two tables together so an
/// endpoint cannot ship undocumented.
#[derive(utoipa::OpenApi)]
#[openapi(
    paths(health, status, events, switch, chain::order, chain::threshold, chain::wrap_off, pair, openapi_document, panes::panes, sessions::sessions, sessions::session_history, create::create, agent::prompt, agent::keys, ticket::mint, agent::session_prompt, agent::session_interrupt, agent::session_answer, gateway::gateway, proxies::proxies, push::key, push::subscribe, push::unsubscribe),
    components(schemas(create::BackgroundSessionCreated)),
    modifiers(&BearerScheme, &SessionsCreateOneOf)
)]
struct ApiDoc;

/// Adds the `bearer` security scheme every operation but `POST /pair` names.
struct BearerScheme;

impl utoipa::Modify for BearerScheme {
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        if let Some(components) = openapi.components.as_mut() {
            components.add_security_scheme(
                "bearer",
                utoipa::openapi::security::SecurityScheme::Http(
                    utoipa::openapi::security::HttpBuilder::new()
                        .scheme(utoipa::openapi::security::HttpAuthScheme::Bearer)
                        .description(Some("Authorization: Bearer <device token>"))
                        .build(),
                ),
            );
        }
    }
}

/// `POST /sessions`' 200 is `oneOf` the pane [`super::create::SessionCreated`]
/// and the background [`super::create::BackgroundSessionCreated`], discriminated
/// on each body's `type`, so the two arms stay two schemas rather than one with
/// optional fields.
struct SessionsCreateOneOf;

impl utoipa::Modify for SessionsCreateOneOf {
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        use utoipa::openapi::RefOr;
        use utoipa::openapi::schema::{Discriminator, OneOfBuilder, Ref};

        let Some(path_item) = openapi.paths.paths.get_mut("/api/v1/sessions") else {
            return;
        };
        let Some(operation) = path_item.post.as_mut() else {
            return;
        };
        let Some(RefOr::T(response)) = operation.responses.responses.get_mut("200") else {
            return;
        };
        let Some(RefOr::T(content)) = response.content.get_mut("application/json") else {
            return;
        };
        let [pane, background] = [
            <create::SessionCreated as utoipa::ToSchema>::name(),
            <create::BackgroundSessionCreated as utoipa::ToSchema>::name(),
        ]
        .map(Ref::from_schema_name);
        content.schema = Some(
            OneOfBuilder::new()
                .discriminator(Some(Discriminator::with_mapping(
                    "type",
                    [
                        ("pane", pane.ref_location.clone()),
                        ("background", background.ref_location.clone()),
                    ],
                )))
                .item(pane)
                .item(background)
                .into(),
        );
    }
}

/// The OpenAPI document as pretty JSON bytes. One function is the whole
/// source: the `openapi.json` handler serves these bytes and `--dump-openapi`
/// prints them, so the two cannot drift apart. A serializer failure is
/// returned to the caller rather than answered with a stub.
pub(crate) fn openapi_document_bytes() -> Result<Vec<u8>, String> {
    <ApiDoc as utoipa::OpenApi>::openapi()
        .to_pretty_json()
        .map(|document| document.into_bytes())
        .map_err(|e| format!("failed to serialize the OpenAPI document: {e}"))
}

/// `GET /api/v1/openapi.json` — this API's own contract.
#[utoipa::path(
    get,
    path = "/api/v1/openapi.json",
    responses(
        (status = 200, description = "the OpenAPI document for this API", body = serde_json::Value, content_type = "application/json"),
        (status = 401, description = "no bearer, or one matching no paired device (`unauthorized`)", body = ErrorBody),
        (status = 403, description = "a page's origin outside the app_origins list (`origin_not_allowed`); a device paired by a newer clauth with a tier this one does not know (`device_tier_unknown`)", body = ErrorBody),
        (status = 500, description = "the device list does not read, or the document failed to serialize (`internal`)", body = ErrorBody)
    ),
    security(("bearer" = ["view"]))
)]
fn openapi_document(_: &ApiContext, _: &Request, _: &Caller<'_>) -> Response {
    match openapi_document_bytes() {
        Ok(document) => Response::raw_json(200, document),
        Err(e) => {
            logline!("clauth api: {e}");
            Response::error(500, "internal")
        }
    }
}

#[cfg(test)]
#[path = "../../../tests/inline/daemon_api_routes.rs"]
mod tests;

#[cfg(test)]
#[path = "../../../tests/inline/daemon_api_push.rs"]
mod push_tests;
