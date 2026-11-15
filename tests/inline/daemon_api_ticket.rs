#![allow(clippy::unwrap_used, clippy::expect_used)]

//! The terminal ticket without a socket: the offer parser, the table, and the
//! `routes::handle` arm, every clock and store state driven directly. The wire
//! half (heads, log lines per connection, open streams) sits with the bridge's
//! own tests in `daemon_api_terminal.rs`.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Barrier, Mutex, OnceLock, Weak};
use std::time::Duration;

use super::{Offer, Stamp, TicketDigest, Tickets};
use crate::daemon::api::devices::{self, Tier};
use crate::daemon::api::http::{Cors, CrossOrigin, Request, Response, SoloHeader, WsHeaders};
use crate::daemon::api::panes::{HerdrOut, HerdrProbeOut, PaneProbe};
use crate::daemon::api::routes::{self, ApiContext, Handled, handle, listed_origins};
use crate::logline::LogLines;
use crate::testutil::HomeSandbox;

const CONTROL: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const VIEW: &str = "fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210";
const DRIVER: &str = "driver";
const VIEWER: &str = "viewer";
const ORIGIN: &str = "https://a.example";
const OTHER_ORIGIN: &str = "https://b.example";
const TICKET_PATH: &str = "/api/v1/panes/w9:p1/ticket";
const STREAM_PATH: &str = "/api/v1/panes/w9:p1/stream";
const WS_KEY: &str = "dGhlIHNhbXBsZSBub25jZQ==";
const SNAPSHOT: &str = r#"{"id":"cli:api:snapshot","result":{"snapshot":{"focused_pane_id":"w9:p1","panes":[{"pane_id":"w9:p1","rect":{"height":57,"width":88,"x":0,"y":0}},{"pane_id":"w1N:p18","rect":{"height":57,"width":88,"x":0,"y":0}},{"pane_id":"w1N:p19","rect":{"height":57,"width":88,"x":0,"y":0}}],"layouts":[]},"type":"api_result"}}"#;
const REFUSED_LINE: &str =
    "clauth api: ticket for device 'driver' refused: pane, origin or revoked";

fn config() -> crate::profile::ConfigHandle {
    Arc::new(crate::lockorder::RankedMutex::new(
        crate::profile::AppConfig {
            state: crate::profile::AppState::default(),
            profiles: Vec::new(),
        },
    ))
}

fn peer() -> SocketAddr {
    SocketAddr::from(([192, 0, 2, 7], 50_000))
}

fn body_json(response: &Response) -> serde_json::Value {
    serde_json::from_slice(&response.body).expect("a JSON body")
}

fn clauth_dir() -> std::path::PathBuf {
    crate::profile::clauth_dir().expect("clauth dir")
}

/// `[serve] app_origins`, as a hand edit leaves it.
fn list_origins(origins: &[&str]) {
    let quoted: Vec<String> = origins.iter().map(|origin| format!("{origin:?}")).collect();
    std::fs::write(
        clauth_dir().join("profiles.toml"),
        format!(
            "profiles = []\n\n[serve]\napp_origins = [{}]\n",
            quoted.join(", ")
        ),
    )
    .expect("write profiles.toml");
}

/// A context with both devices paired, a herdr probe that counts its calls,
/// and a clock the test moves.
struct Fixture {
    ctx: Arc<ApiContext>,
    probes: Arc<AtomicUsize>,
    clock: Arc<Mutex<Stamp>>,
}

impl Fixture {
    fn new() -> Self {
        let probes = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&probes);
        Self::with_probe(
            probes,
            Box::new(move |_, _| {
                counted.fetch_add(1, Ordering::SeqCst);
                HerdrProbeOut::Ran(Some(HerdrOut {
                    success: true,
                    stdout: SNAPSHOT.as_bytes().to_vec(),
                    stderr: Vec::new(),
                }))
            }),
        )
    }

    fn with_probe(probes: Arc<AtomicUsize>, probe: PaneProbe) -> Self {
        devices::seed_for_tests(DRIVER, Tier::Control, CONTROL).expect("seed the control device");
        devices::seed_for_tests(VIEWER, Tier::View, VIEW).expect("seed the view device");
        let mut ctx =
            ApiContext::for_tests(config(), clauth_dir().join("status.json"), None, probe);
        let clock = Arc::new(Mutex::new(Stamp::now()));
        let reader = Arc::clone(&clock);
        Arc::get_mut(&mut ctx).expect("a fresh context").tickets =
            Tickets::new(Box::new(move || *reader.lock().expect("the clock")));
        Self { ctx, probes, clock }
    }

    /// Move the monotonic clock and the wall clock each by its own amount.
    fn advance(&self, mono: Duration, wall: Duration) {
        let mut clock = self.clock.lock().expect("the clock");
        clock.mono += mono;
        clock.wall += wall;
    }

    /// What `serve_connection` does around `handle`: judge the list first.
    fn answer(&self, mut req: Request) -> Handled {
        req.cross_origin.refused =
            Cors::judge(&listed_origins(&self.ctx), &req.cross_origin).refuses(&req.cross_origin);
        handle(&self.ctx, &req, peer())
    }

    fn mint_at(&self, path: &str, bearer: &str, origin: &str) -> String {
        let handled = self.answer(mint_request(
            path,
            Some(bearer),
            SoloHeader::One(origin.to_string()),
        ));
        assert_eq!(handled.response.status, 201, "the mint");
        body_json(&handled.response)["ticket"]
            .as_str()
            .expect("a ticket")
            .to_string()
    }

    fn mint(&self, bearer: &str) -> String {
        self.mint_at(TICKET_PATH, bearer, ORIGIN)
    }

    fn present(&self, ticket: &str) -> Handled {
        self.answer(upgrade(
            STREAM_PATH,
            SoloHeader::One(ORIGIN.to_string()),
            &[&offer_of(ticket)],
        ))
    }

    fn assert_live(&self, ticket: &str) {
        assert!(
            self.present(ticket).hijack.is_some(),
            "the ticket was still live"
        );
    }

    fn assert_dead(&self, ticket: &str) {
        assert_refused(&self.present(ticket));
    }
}

fn offer_of(ticket: &str) -> String {
    format!("clauth.terminal.v1, clauth.ticket.{ticket}")
}

fn request(method: &str, path: &str, bearer: Option<&str>, origin: SoloHeader) -> Request {
    Request {
        method: method.to_string(),
        path: path.to_string(),
        query: String::new(),
        bearer: bearer.map(str::to_string),
        authorization_seen: bearer.is_some(),
        if_none_match: None,
        body: Vec::new(),
        keep_alive: true,
        ws: WsHeaders::default(),
        cross_origin: CrossOrigin {
            origin,
            ..CrossOrigin::default()
        },
        content_type: SoloHeader::Absent,
    }
}

fn mint_request(path: &str, bearer: Option<&str>, origin: SoloHeader) -> Request {
    request("POST", path, bearer, origin)
}

/// A browser's upgrade: no `Authorization`, each `offers` entry one header line.
fn upgrade(path: &str, origin: SoloHeader, offers: &[&str]) -> Request {
    let mut req = request("GET", path, None, origin);
    req.ws.requested = true;
    req.ws.protocol = Some("websocket".to_string());
    req.ws.key = Some(WS_KEY.to_string());
    req.ws.version = Some("13".to_string());
    for offer in offers {
        req.ws.offer.see(offer.as_bytes());
    }
    req
}

/// The one refusal every dead presentation shares.
fn assert_refused(handled: &Handled) {
    assert_eq!(handled.response.status, 401, "the status");
    assert_eq!(
        body_json(&handled.response),
        serde_json::json!({"ok": false, "error": "unauthorized"})
    );
    assert!(handled.response.challenge, "the bearer challenge");
    assert_eq!(handled.device, None, "no device earned this connection");
    assert!(handled.hijack.is_none(), "no attach");
}

// --- The mint ----------------------------------------------------------------

#[test]
fn a_view_and_a_control_device_both_mint() {
    let _home = HomeSandbox::new();
    let fixture = Fixture::new();
    for bearer in [VIEW, CONTROL] {
        let handled = fixture.answer(mint_request(
            TICKET_PATH,
            Some(bearer),
            SoloHeader::One(ORIGIN.to_string()),
        ));
        assert_eq!(handled.response.status, 201, "{bearer}");
        let body = body_json(&handled.response);
        let ticket = body["ticket"].as_str().expect("a ticket");
        assert_eq!(
            body,
            serde_json::json!({"ticket": ticket, "expires_in": 30})
        );
    }
    assert_eq!(fixture.ctx.tickets.held(), 2);
}

#[test]
fn a_mint_needs_a_bearer() {
    let _home = HomeSandbox::new();
    let fixture = Fixture::new();
    let handled = fixture.answer(mint_request(
        TICKET_PATH,
        None,
        SoloHeader::One(ORIGIN.to_string()),
    ));
    assert_refused(&handled);
    assert_eq!(fixture.ctx.tickets.held(), 0);
}

#[test]
fn an_unknown_tier_gets_its_refusal_and_no_ticket() {
    let _home = HomeSandbox::new();
    let fixture = Fixture::new();
    let future = "aa".repeat(32);
    devices::seed_for_tests("future", Tier::Unknown("root".to_string()), &future)
        .expect("seed the device");
    let handled = fixture.answer(mint_request(
        TICKET_PATH,
        Some(&future),
        SoloHeader::One(ORIGIN.to_string()),
    ));
    assert_eq!(handled.response.status, 403);
    assert_eq!(body_json(&handled.response)["error"], "device_tier_unknown");
    assert_eq!(fixture.ctx.tickets.held(), 0);
}

#[test]
fn a_csprng_failure_answers_500_and_mints_nothing() {
    let _home = HomeSandbox::new();
    let fixture = Fixture::new();
    devices::fail_next_generate();
    let handled = fixture.answer(mint_request(
        TICKET_PATH,
        Some(CONTROL),
        SoloHeader::One(ORIGIN.to_string()),
    ));
    assert_eq!(handled.response.status, 500);
    assert_eq!(
        body_json(&handled.response),
        serde_json::json!({"ok": false, "error": "internal"})
    );
    assert_eq!(fixture.ctx.tickets.held(), 0);
}

#[test]
fn a_mint_with_no_origin_answers_400_and_holds_nothing() {
    let _home = HomeSandbox::new();
    let fixture = Fixture::new();
    for origins in [&[][..], &[ORIGIN][..]] {
        list_origins(origins);
        let handled = fixture.answer(mint_request(TICKET_PATH, Some(CONTROL), SoloHeader::Absent));
        assert_eq!(handled.response.status, 400, "{origins:?}");
        assert_eq!(body_json(&handled.response)["error"], "origin_required");
        assert_eq!(fixture.ctx.tickets.held(), 0, "{origins:?}");
    }
}

/// The 400's reason makes two claims, so it is pinned by equality: the ticket
/// binds the minting page, and a client that is not a page sends its bearer.
#[test]
fn a_mint_with_no_origin_names_the_origin_requirement() {
    let _home = HomeSandbox::new();
    let fixture = Fixture::new();
    let handled = fixture.answer(mint_request(TICKET_PATH, Some(CONTROL), SoloHeader::Absent));
    assert_eq!(handled.response.status, 400);
    assert_eq!(
        body_json(&handled.response),
        serde_json::json!({
            "ok": false,
            "error": "origin_required",
            "reason": "a ticket binds the page that minted it, and this request named no Origin; \
                       a client that is not a page sends its bearer on the upgrade"
        })
    );
    assert_eq!(fixture.ctx.tickets.held(), 0);
}

#[test]
fn an_origin_no_page_can_be_bound_to_answers_403_on_every_list() {
    let _home = HomeSandbox::new();
    let fixture = Fixture::new();
    for origins in [&[][..], &[ORIGIN][..]] {
        list_origins(origins);
        for origin in [SoloHeader::Unusable, SoloHeader::One("null".to_string())] {
            let handled = fixture.answer(mint_request(TICKET_PATH, Some(CONTROL), origin));
            assert_eq!(handled.response.status, 403, "{origins:?}");
            assert_eq!(body_json(&handled.response)["error"], "origin_not_allowed");
            assert_eq!(fixture.ctx.tickets.held(), 0, "{origins:?}");
        }
    }
}

/// No list entry can ever equal these, so the reason says so instead of naming
/// the list as the fix, whatever the list holds.
#[test]
fn a_mint_from_an_origin_no_list_can_hold_says_so_on_every_list() {
    let _home = HomeSandbox::new();
    let fixture = Fixture::new();
    for origins in [&[][..], &[ORIGIN][..]] {
        list_origins(origins);
        for (case, origin) in [SoloHeader::Unusable, SoloHeader::One("null".to_string())]
            .into_iter()
            .enumerate()
        {
            let handled = fixture.answer(mint_request(TICKET_PATH, Some(CONTROL), origin));
            assert_eq!(
                body_json(&handled.response),
                serde_json::json!({
                    "ok": false,
                    "error": "origin_not_allowed",
                    "reason": "this page sends an origin `[serve] app_origins` can never list \
                               (`null`, sent twice, or not UTF-8); open the app from the web \
                               address it is served at"
                }),
                "case {case}, list {origins:?}"
            );
        }
    }
}

// --- The offer ---------------------------------------------------------------

const TICKET_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
/// [`TICKET_A`]'s SHA-256, by `sha256sum`.
const TICKET_A_DIGEST: &str = "ffe054fe7ae0cb6dc65c3af9b61d5209f439851db43d0ba5997337df154668eb";
const TICKET_B: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

fn offered(lines: &[&[u8]]) -> Option<TicketDigest> {
    let mut offer = Offer::default();
    for line in lines {
        offer.see(line);
    }
    offer.ticket().copied()
}

#[test]
fn the_offer_reads_one_ticket_beside_the_fixed_protocol() {
    let mut digest = [0u8; 32];
    hex::decode_to_slice(TICKET_A_DIGEST, &mut digest).expect("the fixture's digest");
    let ticket = format!("clauth.ticket.{TICKET_A}");
    let ticket = ticket.as_bytes();
    let pair = [b"clauth.terminal.v1, ".as_slice(), ticket].concat();
    let reversed = [ticket, b",clauth.terminal.v1".as_slice()].concat();
    let spaced = [b"\t clauth.terminal.v1 \t,\t ".as_slice(), ticket, b" \t"].concat();
    let beside_unknown = [b"chat, clauth.terminal.v1, soap, ".as_slice(), ticket].concat();
    let split_case = [
        b"clauth.terminal.v1, ".as_slice(),
        ticket,
        b", CLAUTH.TICKET.abc",
    ]
    .concat();
    let not_text = [b"\xff, clauth.terminal.v1, ".as_slice(), ticket].concat();

    for lines in [
        vec![pair.as_slice()],
        vec![reversed.as_slice()],
        vec![spaced.as_slice()],
        vec![beside_unknown.as_slice()],
        vec![split_case.as_slice()],
        vec![not_text.as_slice()],
        vec![b"clauth.terminal.v1".as_slice(), ticket],
        vec![ticket, b"clauth.terminal.v1".as_slice()],
    ] {
        assert_eq!(offered(&lines), Some(digest), "{lines:?}");
    }
}

#[test]
fn an_offer_with_no_single_well_formed_ticket_reads_none() {
    let ticket = format!("clauth.ticket.{TICKET_A}");
    let second = format!("clauth.ticket.{TICKET_B}");
    let upper = format!("clauth.ticket.{}", TICKET_B.replace('a', "A"));
    let short = format!("clauth.ticket.{}", &TICKET_A[..63]);
    let long = format!("clauth.ticket.{TICKET_A}a");
    let non_hex = format!("clauth.ticket.{}g", &TICKET_A[..63]);
    let prefix_case = format!("CLAUTH.TICKET.{TICKET_A}");
    let two_pair = format!("clauth.terminal.v1, {ticket}, {second}");
    let repeated = format!("clauth.terminal.v1, {ticket}, {ticket}");
    let cases: Vec<(&str, Vec<&str>)> = vec![
        ("no lines", vec![]),
        ("empty", vec![""]),
        ("only the protocol", vec!["clauth.terminal.v1"]),
        ("a ticket alone", vec![&ticket]),
        ("beside another protocol", vec![&ticket, "chat"]),
        ("two tickets", vec![&two_pair]),
        ("the same ticket twice", vec![&repeated]),
        (
            "two tickets on two lines",
            vec!["clauth.terminal.v1", &ticket, &second],
        ),
        ("an uppercase digit", vec!["clauth.terminal.v1", &upper]),
        ("63 digits", vec!["clauth.terminal.v1", &short]),
        ("65 digits", vec!["clauth.terminal.v1", &long]),
        ("a digit out of range", vec!["clauth.terminal.v1", &non_hex]),
        (
            "a prefix in another case",
            vec!["clauth.terminal.v1", &prefix_case],
        ),
        (
            "the protocol in another case",
            vec!["CLAUTH.TERMINAL.V1", &ticket],
        ),
        (
            "the protocol with a suffix",
            vec!["clauth.terminal.v10", &ticket],
        ),
    ];
    for (name, lines) in cases {
        let lines: Vec<&[u8]> = lines.iter().map(|line| line.as_bytes()).collect();
        assert_eq!(offered(&lines), None, "{name}");
    }
}

/// Each of these three carries a live ticket and offers it in a shape the
/// parser refuses, so the ticket stays live for a later right offer.
#[test]
fn a_refused_offer_spends_nothing() {
    let _home = HomeSandbox::new();
    let fixture = Fixture::new();
    let ticket = fixture.mint(CONTROL);
    let second = fixture.mint(CONTROL);
    for lines in [
        vec![format!("clauth.ticket.{ticket}")],
        vec![format!("chat, clauth.ticket.{ticket}")],
        vec![format!(
            "clauth.terminal.v1, clauth.ticket.{ticket}, clauth.ticket.{second}"
        )],
        vec![
            "clauth.terminal.v1".to_string(),
            format!("clauth.ticket.{ticket}"),
            format!("clauth.ticket.{second}"),
        ],
        vec![format!("clauth.terminal.v1, CLAUTH.TICKET.{ticket}")],
        vec![format!(
            "clauth.terminal.v1, clauth.ticket.{}",
            ticket.to_ascii_uppercase()
        )],
    ] {
        let lines: Vec<&str> = lines.iter().map(String::as_str).collect();
        let handled = fixture.answer(upgrade(
            STREAM_PATH,
            SoloHeader::One(ORIGIN.to_string()),
            &lines,
        ));
        assert_refused(&handled);
        assert_eq!(fixture.ctx.tickets.held(), 2, "{lines:?}");
    }
    fixture.assert_live(&ticket);
    fixture.assert_live(&second);
}

#[test]
fn a_ticket_in_the_query_authenticates_nothing_and_is_not_spent() {
    let _home = HomeSandbox::new();
    let fixture = Fixture::new();
    let ticket = fixture.mint(CONTROL);
    let mut req = upgrade(
        STREAM_PATH,
        SoloHeader::One(ORIGIN.to_string()),
        &["clauth.terminal.v1"],
    );
    req.query = format!("ticket={ticket}");
    assert_refused(&fixture.answer(req));
    fixture.assert_live(&ticket);
}

#[test]
fn a_ticket_off_the_stream_path_is_never_looked_up() {
    let _home = HomeSandbox::new();
    let fixture = Fixture::new();
    let ticket = fixture.mint(CONTROL);
    for path in [
        "/api/v1/status",
        "/api/v1/health",
        "/api/v1/panes",
        "/api/v1/pane/w9:p1/stream",
        "/api/v1/panes/w9:p1/stream/",
        "/api/v1/panes/w9:p1/ticket",
        "/api/v1/panes/w9:p1/keys",
    ] {
        let mut req = upgrade(
            path,
            SoloHeader::One(ORIGIN.to_string()),
            &[&offer_of(&ticket)],
        );
        if path.ends_with("/ticket") || path.ends_with("/keys") {
            req.method = "POST".to_string();
        }
        assert_refused(&fixture.answer(req));
        assert_eq!(fixture.ctx.tickets.held(), 1, "{path}");
    }
    fixture.assert_live(&ticket);
}

// --- A presentation ----------------------------------------------------------

/// Unknown, spent, expired, wrong pane, wrong `Origin` and revoked, each one
/// `Response::unauthorized()` with no device; the wire half compares the bytes.
#[test]
fn every_dead_presentation_answers_the_401_with_no_device() {
    let _home = HomeSandbox::new();
    let fixture = Fixture::new();

    fixture.assert_dead(TICKET_A);

    let spent = fixture.mint(CONTROL);
    fixture.assert_live(&spent);
    fixture.assert_dead(&spent);

    let expired = fixture.mint(CONTROL);
    fixture.advance(Duration::from_secs(30), Duration::from_secs(30));
    fixture.assert_dead(&expired);

    let wrong_pane = fixture.mint(CONTROL);
    assert_refused(&fixture.answer(upgrade(
        "/api/v1/panes/w1N:p19/stream",
        SoloHeader::One(ORIGIN.to_string()),
        &[&offer_of(&wrong_pane)],
    )));
    fixture.assert_dead(&wrong_pane);

    let wrong_origin = fixture.mint(CONTROL);
    assert_refused(&fixture.answer(upgrade(
        STREAM_PATH,
        SoloHeader::One(OTHER_ORIGIN.to_string()),
        &[&offer_of(&wrong_origin)],
    )));
    fixture.assert_dead(&wrong_origin);

    let revoked = fixture.mint(VIEW);
    devices::revoke(VIEWER).expect("revoke");
    fixture.assert_dead(&revoked);

    assert_eq!(
        fixture.probes.load(Ordering::SeqCst),
        1,
        "only the live attach"
    );
}

/// 200 rounds of 8 threads released by one barrier, so one run is 200 tries at
/// a double spend.
#[test]
fn of_many_presentations_of_one_ticket_exactly_one_attaches() {
    const ROUNDS: usize = 200;
    const THREADS: usize = 8;
    let _home = HomeSandbox::new();
    let fixture = Fixture::new();
    for round in 0..ROUNDS {
        let ticket = fixture.mint(CONTROL);
        let barrier = Barrier::new(THREADS);
        let attached = AtomicUsize::new(0);
        let refused = AtomicUsize::new(0);
        std::thread::scope(|scope| {
            for _ in 0..THREADS {
                scope.spawn(|| {
                    let mut req = upgrade(
                        STREAM_PATH,
                        SoloHeader::One(ORIGIN.to_string()),
                        &[&offer_of(&ticket)],
                    );
                    req.cross_origin.refused = false;
                    barrier.wait();
                    let handled = handle(&fixture.ctx, &req, peer());
                    if handled.hijack.is_some() {
                        attached.fetch_add(1, Ordering::SeqCst);
                    } else if handled.response.status == 401 {
                        refused.fetch_add(1, Ordering::SeqCst);
                    }
                });
            }
        });
        assert_eq!(attached.load(Ordering::SeqCst), 1, "round {round}");
        assert_eq!(refused.load(Ordering::SeqCst), THREADS - 1, "round {round}");
    }
    assert_eq!(fixture.probes.load(Ordering::SeqCst), ROUNDS);
}

#[test]
fn a_failed_handshake_spends_the_ticket_and_earns_nothing() {
    let _home = HomeSandbox::new();
    let fixture = Fixture::new();
    let ticket = fixture.mint(CONTROL);
    let mut req = upgrade(
        STREAM_PATH,
        SoloHeader::One(ORIGIN.to_string()),
        &[&offer_of(&ticket)],
    );
    req.ws.requested = false;
    let handled = fixture.answer(req);
    assert_eq!(handled.response.status, 426);
    assert_eq!(handled.device, None, "a burned ticket earns no connection");
    fixture.assert_dead(&ticket);
}

/// A version this server does not speak is a 400, and the ticket that reached
/// the refusal is spent anyway: the first presentation on the stream path
/// spends it, whatever follows.
#[test]
fn a_refused_websocket_version_spends_the_ticket() {
    let _home = HomeSandbox::new();
    let fixture = Fixture::new();
    let ticket = fixture.mint(CONTROL);
    let mut req = upgrade(
        STREAM_PATH,
        SoloHeader::One(ORIGIN.to_string()),
        &[&offer_of(&ticket)],
    );
    req.ws.version = Some("8".to_string());
    let handled = fixture.answer(req);
    assert_eq!(handled.response.status, 400);
    assert_eq!(
        body_json(&handled.response)["error"],
        "bad_websocket_version"
    );
    assert_eq!(handled.device, None, "a burned ticket earns no connection");
    fixture.assert_dead(&ticket);
}

#[test]
fn a_ticket_on_another_method_is_spent() {
    let _home = HomeSandbox::new();
    let fixture = Fixture::new();
    let ticket = fixture.mint(CONTROL);
    let mut req = upgrade(
        STREAM_PATH,
        SoloHeader::One(ORIGIN.to_string()),
        &[&offer_of(&ticket)],
    );
    req.method = "POST".to_string();
    let handled = fixture.answer(req);
    assert_eq!(handled.response.status, 405);
    assert_eq!(handled.device, None);
    fixture.assert_dead(&ticket);
}

#[test]
fn a_pane_herdr_does_not_hold_spends_the_ticket() {
    let _home = HomeSandbox::new();
    let fixture = Fixture::new();
    let path = "/api/v1/panes/w9:pZZ/stream";
    let ticket = fixture.mint_at("/api/v1/panes/w9:pZZ/ticket", CONTROL, ORIGIN);
    let present = || {
        fixture.answer(upgrade(
            path,
            SoloHeader::One(ORIGIN.to_string()),
            &[&offer_of(&ticket)],
        ))
    };
    let handled = present();
    assert_eq!(handled.response.status, 404);
    assert_eq!(body_json(&handled.response)["error"], "pane_not_found");
    assert_eq!(handled.device, None);
    assert_refused(&present());
}

#[test]
fn no_herdr_server_spends_the_ticket() {
    let _home = HomeSandbox::new();
    let probes = Arc::new(AtomicUsize::new(0));
    let fixture = Fixture::with_probe(probes, Box::new(|_, _| HerdrProbeOut::Ran(None)));
    let ticket = fixture.mint(CONTROL);
    let handled = fixture.present(&ticket);
    assert_eq!(handled.response.status, 503);
    assert_eq!(body_json(&handled.response)["error"], "herdr_unavailable");
    assert_eq!(handled.device, None);
    fixture.assert_dead(&ticket);
}

// --- The table's guard -------------------------------------------------------

/// The table's guard is free again by the time the attach reaches herdr: the
/// probe itself takes it.
#[test]
fn the_herdr_probe_runs_with_the_table_guard_dropped() {
    let _home = HomeSandbox::new();
    let context: Arc<OnceLock<Weak<ApiContext>>> = Arc::new(OnceLock::new());
    let seen = Arc::clone(&context);
    let free = Arc::new(AtomicBool::new(false));
    let noted = Arc::clone(&free);
    let fixture = Fixture::with_probe(
        Arc::new(AtomicUsize::new(0)),
        Box::new(move |_, _| {
            let ctx = seen.get().and_then(Weak::upgrade).expect("the context");
            noted.store(ctx.tickets.table.try_lock().is_ok(), Ordering::SeqCst);
            HerdrProbeOut::Ran(Some(HerdrOut {
                success: true,
                stdout: SNAPSHOT.as_bytes().to_vec(),
                stderr: Vec::new(),
            }))
        }),
    );
    context
        .set(Arc::downgrade(&fixture.ctx))
        .expect("set the context");
    let ticket = fixture.mint(CONTROL);
    fixture.assert_live(&ticket);
    assert!(free.load(Ordering::SeqCst), "the probe ran under the guard");
}

/// A `.table` access in `source` outside `impl Tickets` and the test module:
/// every such line can take the guard in `attach` or a helper and hold it
/// across a store read or a log line. The residual this cannot see is an impl
/// method that hands its guard out for another caller to hold.
fn table_accesses_outside_impl(source: &str) -> Vec<&str> {
    let (before_impl, after_impl) = source.split_once("impl Tickets {").unwrap_or((source, ""));
    let after_impl = after_impl
        .split_once("\n}\n")
        .map(|(_, after)| after)
        .unwrap_or_default();
    // The file's trailing `mod tests` declaration is the one `#[cfg(test)]` item
    // the scan skips; any other one is scanned like the rest of the file.
    let after_impl = after_impl
        .rsplit_once("#[cfg(test)]\n")
        .filter(|(_, tail)| tail.starts_with("#[path") || tail.starts_with("mod tests"))
        .map_or(after_impl, |(kept, _)| kept);
    before_impl
        .lines()
        .chain(after_impl.lines())
        .map(str::trim)
        .filter(|line| line.contains(".table"))
        .collect()
}

/// The names a table hold must not reach, in the source of `impl Tickets`. The
/// residual this cannot see is a helper the impl calls that reaches one of
/// them under another name; the impl calls `Vec`'s and iterators' own,
/// `RankedMutex::{new, lock}`, `Entry::expired`, `ct_eq`, `bool::from` and
/// `PoisonError::into_inner`.
fn hold_faults(source: &str) -> Vec<&'static str> {
    let Some((_, rest)) = source.split_once("impl Tickets {") else {
        return vec!["no impl Tickets"];
    };
    let body: String = rest
        .split("\n}\n")
        .next()
        .unwrap_or_default()
        .lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n");
    [
        "logline!",
        "read_store",
        "devices::",
        "herdr",
        "terminal::",
        "std::fs",
    ]
    .into_iter()
    .filter(|name| body.contains(name))
    .collect()
}

#[test]
fn the_table_impl_reaches_no_log_store_or_herdr() {
    assert_eq!(
        hold_faults("impl Tickets {\n    fn x(&self) { logline!(\"y\"); read_store(); }\n}\n"),
        ["logline!", "read_store"],
        "the scan flags a hold it must flag"
    );
    assert!(
        hold_faults("impl Tickets {\n    fn x(&self) { self.table.len(); }\n}\n").is_empty(),
        "the scan passes a hold it must pass"
    );
    let source = include_str!("../../src/daemon/api/ticket.rs");
    assert!(
        source.contains("fn take("),
        "the scan is reading the real table"
    );
    assert_eq!(hold_faults(source), Vec::<&str>::new());
}

#[test]
fn the_table_is_reached_only_inside_its_impl() {
    assert_eq!(
        table_accesses_outside_impl(
            "fn attach() { ctx.tickets.table.lock(); }\n\
             impl Tickets {\n    fn x(&self) { self.table.len(); }\n}\n\
             #[cfg(test)]\nmod tests {\n    fn y() { ctx.tickets.table.lock(); }\n}\n"
        ),
        ["fn attach() { ctx.tickets.table.lock(); }"],
        "the scan flags an access outside the impl"
    );
    assert_eq!(
        table_accesses_outside_impl(
            "impl Tickets {\n    fn x(&self) { self.table.len(); }\n}\n\
             #[cfg(test)]\nfn helper() {}\n\
             fn attach() { ctx.tickets.table.lock(); }\n\
             #[cfg(test)]\nmod tests;\n"
        ),
        ["fn attach() { ctx.tickets.table.lock(); }"],
        "a #[cfg(test)] helper above attach cannot hide it"
    );
    assert!(
        table_accesses_outside_impl(
            "impl Tickets {\n    fn x(&self) { self.table.len(); }\n}\n\
             #[cfg(test)]\nmod tests;\n"
        )
        .is_empty(),
        "the scan passes the impl's own access and the test tail"
    );
    let source = include_str!("../../src/daemon/api/ticket.rs");
    assert!(
        source.contains(".table"),
        "the scan is reading the real file's table accesses"
    );
    assert_eq!(table_accesses_outside_impl(source), Vec::<&str>::new());
}

#[test]
fn the_table_ranks_as_a_leaf_after_the_last_origins() {
    use crate::lockorder::{Rank, rank};
    assert_eq!(
        <rank::TerminalTickets as Rank>::VALUE,
        <rank::LastOrigins as Rank>::VALUE + 1
    );
}

// --- A bearer on the same upgrade ---------------------------------------------

#[test]
fn a_valid_bearer_decides_alone_and_leaves_the_ticket_live() {
    let _home = HomeSandbox::new();
    let fixture = Fixture::new();
    let ticket = fixture.mint(CONTROL);
    let mut req = upgrade(
        STREAM_PATH,
        SoloHeader::One(ORIGIN.to_string()),
        &[&offer_of(&ticket)],
    );
    req.bearer = Some(VIEW.to_string());
    let handled = fixture.answer(req);
    assert!(handled.hijack.is_some(), "the bearer attaches");
    assert_eq!(handled.device.as_deref(), Some(VIEWER));
    assert_eq!(
        fixture.ctx.tickets.held(),
        1,
        "the ticket was never looked up"
    );
    fixture.assert_live(&ticket);
}

#[test]
fn an_invalid_bearer_is_not_rescued_by_a_live_ticket() {
    let _home = HomeSandbox::new();
    let fixture = Fixture::new();
    let ticket = fixture.mint(CONTROL);
    let mut req = upgrade(
        STREAM_PATH,
        SoloHeader::One(ORIGIN.to_string()),
        &[&offer_of(&ticket)],
    );
    req.bearer = Some("not a token".to_string());
    assert_refused(&fixture.answer(req));
    assert_eq!(fixture.ctx.tickets.held(), 1);
    fixture.assert_live(&ticket);
}

/// An upgrade as the parser sees the wire, with `extra_authorization` after the
/// view bearer's line: the two-header cases a hand-built request cannot pose.
fn parsed_upgrade(extra_authorization: &str, ticket: &str) -> Request {
    parsed_upgrade_with(
        &format!("Authorization: Bearer {VIEW}\r\n{extra_authorization}"),
        ticket,
    )
}

/// An upgrade as the parser sees the wire, its `Authorization` lines (or none)
/// exactly `authorization`.
fn parsed_upgrade_with(authorization: &str, ticket: &str) -> Request {
    let raw = format!(
        "GET {STREAM_PATH} HTTP/1.1\r\n\
         Host: h\r\n\
         {authorization}\
         Connection: Upgrade\r\n\
         Upgrade: websocket\r\n\
         Sec-WebSocket-Version: 13\r\n\
         Sec-WebSocket-Key: {WS_KEY}\r\n\
         Origin: {ORIGIN}\r\n\
         Sec-WebSocket-Protocol: {}\r\n\
         \r\n",
        offer_of(ticket)
    );
    crate::daemon::api::http::RequestReader::new(
        std::io::Cursor::new(raw.into_bytes()),
        std::time::Instant::now() + Duration::from_secs(60),
    )
    .next_request()
    .ok()
    .flatten()
    .expect("the raw upgrade parses")
}

/// The control: one `Authorization` line reaches the parser as a bearer, and
/// that bearer decides alone and leaves the ticket unlooked-at.
#[test]
fn a_bearer_presented_on_the_wire_decides_alone() {
    let _home = HomeSandbox::new();
    let fixture = Fixture::new();
    let ticket = fixture.mint(CONTROL);
    let handled = fixture.answer(parsed_upgrade("", &ticket));
    assert!(handled.hijack.is_some(), "the presented bearer attaches");
    assert_eq!(handled.device.as_deref(), Some(VIEWER));
    assert_eq!(
        fixture.ctx.tickets.held(),
        1,
        "the ticket was never looked up"
    );
}

/// An `Authorization` header was presented, so the bearer path decides, even
/// though its last line is no bearer: the ticket arm must not run, and the
/// ticket is not spent.
#[test]
fn an_authorization_line_that_is_no_bearer_does_not_reach_the_ticket() {
    let _home = HomeSandbox::new();
    let fixture = Fixture::new();
    let ticket = fixture.mint(CONTROL);
    let handled = fixture.answer(parsed_upgrade("Authorization: Basic Zm9v\r\n", &ticket));
    assert_eq!(handled.response.status, 401, "the bearer path's answer");
    assert_eq!(handled.device, None, "the ticket's device never decided");
    assert!(handled.hijack.is_none(), "no attach");
    assert_eq!(fixture.ctx.tickets.held(), 1, "the ticket was spent");
    fixture.assert_live(&ticket);
}

/// The presented header decides, not a bearer seen earlier: one lone
/// non-Bearer `Authorization` line, an empty one, and a lower-case header name
/// each keep the ticket arm off.
#[test]
fn a_lone_authorization_line_that_is_no_bearer_does_not_reach_the_ticket() {
    let _home = HomeSandbox::new();
    let fixture = Fixture::new();
    for lines in [
        "Authorization: Basic Zm9v\r\n",
        "Authorization:\r\n",
        "authorization: Basic Zm9v\r\n",
    ] {
        let ticket = fixture.mint(CONTROL);
        let handled = fixture.answer(parsed_upgrade_with(lines, &ticket));
        assert_eq!(handled.response.status, 401, "{lines:?}");
        assert_eq!(handled.device, None, "{lines:?}");
        assert!(handled.hijack.is_none(), "{lines:?}");
        fixture.assert_live(&ticket);
    }
}

// --- The row behind a ticket ---------------------------------------------------

#[test]
fn a_revoke_and_a_new_device_under_the_name_refuses_the_old_ticket() {
    let _home = HomeSandbox::new();
    let fixture = Fixture::new();
    let ticket = fixture.mint(VIEW);
    devices::revoke(VIEWER).expect("revoke");
    let name = devices::DeviceName::parse(VIEWER).expect("the name");
    devices::add(&name, Tier::Control, false).expect("add a new row under the name");
    fixture.assert_dead(&ticket);
}

#[test]
fn a_hand_edited_name_keeps_the_ticket_and_names_the_new_name() {
    let _home = HomeSandbox::new();
    let fixture = Fixture::new();
    let ticket = fixture.mint(VIEW);
    let path = clauth_dir().join("devices.json");
    let text = std::fs::read_to_string(&path).expect("read the list");
    assert!(text.contains("\"viewer\""), "the fixture holds the name");
    std::fs::write(&path, text.replace("\"viewer\"", "\"tablet\"")).expect("edit the list");
    let handled = fixture.present(&ticket);
    assert!(handled.hijack.is_some(), "the digest kept the row");
    assert_eq!(handled.device.as_deref(), Some("tablet"));
}

#[test]
fn a_tier_this_build_does_not_know_refuses_the_attach() {
    let _home = HomeSandbox::new();
    let fixture = Fixture::new();
    let ticket = fixture.mint(CONTROL);
    let path = clauth_dir().join("devices.json");
    let text = std::fs::read_to_string(&path).expect("read the list");
    assert!(text.contains("\"control\""), "the fixture holds the tier");
    std::fs::write(&path, text.replace("\"control\"", "\"root\"")).expect("edit the list");
    let handled = fixture.present(&ticket);
    assert_eq!(handled.response.status, 403);
    assert_eq!(body_json(&handled.response)["error"], "device_tier_unknown");
    assert_eq!(handled.device, None);
    assert!(handled.hijack.is_none());
    assert_eq!(fixture.probes.load(Ordering::SeqCst), 0);
    fixture.assert_dead(&ticket);
}

#[test]
fn a_device_list_that_does_not_read_answers_500_and_logs_once() {
    let _home = HomeSandbox::new();
    let fixture = Fixture::new();
    let first = fixture.mint(CONTROL);
    let second = fixture.mint(CONTROL);
    std::fs::write(clauth_dir().join("devices.json"), "not a list").expect("corrupt the list");
    routes::READ_FAILED_NOTED.store(false, Ordering::SeqCst);
    let lines = LogLines::new();
    let capture = lines.capture_here();
    for ticket in [&first, &second] {
        let handled = fixture.present(ticket);
        assert_eq!(handled.response.status, 500);
        assert_eq!(
            body_json(&handled.response),
            serde_json::json!({"ok": false, "error": "internal"})
        );
        assert_eq!(handled.device, None);
        assert!(handled.hijack.is_none());
    }
    drop(capture);
    let latched = lines
        .snapshot()
        .iter()
        .filter(|line| line.starts_with("clauth api: refusing every request until"))
        .count();
    assert_eq!(latched, 1);
    assert_eq!(fixture.probes.load(Ordering::SeqCst), 0);
}

// --- The audit line ------------------------------------------------------------

#[test]
fn a_refused_live_ticket_writes_one_fixed_line() {
    let _home = HomeSandbox::new();
    let fixture = Fixture::new();
    let wrong_pane = fixture.mint(CONTROL);
    let wrong_origin = fixture.mint(CONTROL);
    let revoked = fixture.mint(CONTROL);
    let dead = TICKET_A;
    let lines = LogLines::new();
    let capture = lines.capture_here();
    let refused = |path: &str, origin: &str, ticket: &str| {
        fixture.answer(upgrade(
            path,
            SoloHeader::One(origin.to_string()),
            &[&offer_of(ticket)],
        ))
    };

    assert_refused(&refused(
        "/api/v1/panes/w1N:p19/stream",
        ORIGIN,
        &wrong_pane,
    ));
    assert_eq!(lines.snapshot(), [REFUSED_LINE], "the wrong pane");
    assert_refused(&refused(STREAM_PATH, OTHER_ORIGIN, &wrong_origin));
    assert_eq!(
        lines.snapshot(),
        [REFUSED_LINE, REFUSED_LINE],
        "the wrong origin"
    );
    devices::revoke(DRIVER).expect("revoke");
    assert_refused(&refused(STREAM_PATH, ORIGIN, &revoked));
    assert_eq!(
        lines.snapshot(),
        [REFUSED_LINE, REFUSED_LINE, REFUSED_LINE],
        "the revoked device"
    );
    assert_refused(&refused(STREAM_PATH, ORIGIN, dead));
    assert_refused(&refused(STREAM_PATH, ORIGIN, &revoked));
    drop(capture);
    assert_eq!(lines.snapshot().len(), 3, "a dead ticket writes nothing");
}

#[test]
fn an_expired_ticket_writes_no_line() {
    let _home = HomeSandbox::new();
    let fixture = Fixture::new();
    let ticket = fixture.mint(CONTROL);
    fixture.advance(Duration::from_secs(31), Duration::from_secs(31));
    let lines = LogLines::new();
    let capture = lines.capture_here();
    assert_refused(&fixture.answer(upgrade(
        "/api/v1/panes/w1N:p19/stream",
        SoloHeader::One(ORIGIN.to_string()),
        &[&offer_of(&ticket)],
    )));
    drop(capture);
    assert_eq!(lines.snapshot(), Vec::<String>::new());
}

// --- The `Origin` bind ---------------------------------------------------------

#[test]
fn a_bound_origin_removed_after_the_mint_answers_403_with_no_herdr_call() {
    let _home = HomeSandbox::new();
    let fixture = Fixture::new();
    list_origins(&[ORIGIN, OTHER_ORIGIN]);
    let ticket = fixture.mint(CONTROL);
    list_origins(&[OTHER_ORIGIN]);
    let lines = LogLines::new();
    let capture = lines.capture_here();
    let handled = fixture.present(&ticket);
    drop(capture);
    assert_eq!(handled.response.status, 403);
    assert_eq!(body_json(&handled.response)["error"], "origin_not_allowed");
    assert_eq!(
        body_json(&handled.response)["reason"],
        "this page's origin is not in `[serve] app_origins` in profiles.toml on the host"
    );
    assert_eq!(handled.device, None);
    assert!(handled.hijack.is_none());
    assert_eq!(fixture.probes.load(Ordering::SeqCst), 0);
    assert_eq!(lines.snapshot(), [REFUSED_LINE]);
    assert_eq!(fixture.ctx.tickets.held(), 0, "the presentation spent it");
}

/// The bind answers ahead of the list, so a `403` always means a bind that
/// matched an entry since removed.
#[test]
fn a_wrong_bind_answers_the_401_on_any_list() {
    let _home = HomeSandbox::new();
    let fixture = Fixture::new();
    for (origins, offered_by) in [
        (&[][..], OTHER_ORIGIN),
        (&[ORIGIN, OTHER_ORIGIN][..], OTHER_ORIGIN),
        (&[ORIGIN][..], OTHER_ORIGIN),
    ] {
        list_origins(origins);
        let ticket = fixture.mint(CONTROL);
        assert_refused(&fixture.answer(upgrade(
            STREAM_PATH,
            SoloHeader::One(offered_by.to_string()),
            &[&offer_of(&ticket)],
        )));
        fixture.assert_dead(&ticket);
    }
}

/// The bind is byte-exact equality and nothing else: on an empty list, where it
/// is the upgrade's only origin check, no fold, slash, port, scheme, prefix,
/// suffix or substring of the bound value matches, whichever side holds it.
#[test]
fn a_near_miss_of_the_bound_origin_answers_the_401() {
    let _home = HomeSandbox::new();
    let fixture = Fixture::new();
    for near in [
        "https://a.exampl",
        "https://a.example.evil",
        "https://a.example/",
        "https://a.example:443",
        "https://a.example:8443",
        "https://A.example",
        "HTTPS://a.example",
        "http://a.example",
        "a.example",
        "//a.example",
        "xhttps://a.example",
    ] {
        let ticket = fixture.mint(CONTROL);
        assert_refused(&fixture.answer(upgrade(
            STREAM_PATH,
            SoloHeader::One(near.to_string()),
            &[&offer_of(&ticket)],
        )));
        fixture.assert_dead(&ticket);
    }
    let ticket = fixture.mint(CONTROL);
    fixture.assert_live(&ticket);
}

#[test]
fn a_presentation_with_no_usable_origin_answers_the_401_on_any_list() {
    let _home = HomeSandbox::new();
    let fixture = Fixture::new();
    for origins in [&[][..], &[ORIGIN][..]] {
        list_origins(origins);
        for origin in [SoloHeader::Absent, SoloHeader::Unusable] {
            let ticket = fixture.mint(CONTROL);
            assert_refused(&fixture.answer(upgrade(STREAM_PATH, origin, &[&offer_of(&ticket)])));
            fixture.assert_dead(&ticket);
        }
    }
}

// --- The pane bind ---------------------------------------------------------------

#[test]
fn a_pane_binds_by_its_decoded_id() {
    let _home = HomeSandbox::new();
    let fixture = Fixture::new();
    let mint = || fixture.mint_at("/api/v1/panes/w1N%3Ap19/ticket", CONTROL, ORIGIN);
    let present = |path: &str, ticket: &str| {
        fixture.answer(upgrade(
            path,
            SoloHeader::One(ORIGIN.to_string()),
            &[&offer_of(ticket)],
        ))
    };
    let ticket = mint();
    assert!(
        present("/api/v1/panes/w1N:p19/stream", &ticket)
            .hijack
            .is_some(),
        "the two spellings are one pane"
    );
    let ticket = mint();
    assert_refused(&present("/api/v1/panes/w1N:p18/stream", &ticket));
    let ticket = fixture.mint_at("/api/v1/panes/w1N:p19/ticket", CONTROL, ORIGIN);
    assert!(
        present("/api/v1/panes/w1N%3Ap19/stream", &ticket)
            .hijack
            .is_some(),
        "and the other way round"
    );
}

// --- The clocks --------------------------------------------------------------------

#[test]
fn a_ticket_lives_to_29_seconds_and_dies_at_30() {
    let _home = HomeSandbox::new();
    let fixture = Fixture::new();
    let at_29 = fixture.mint(CONTROL);
    let at_30 = fixture.mint(CONTROL);
    fixture.advance(Duration::from_secs(29), Duration::from_secs(29));
    fixture.assert_live(&at_29);
    fixture.advance(Duration::from_secs(1), Duration::from_secs(1));
    fixture.assert_dead(&at_30);
    assert_eq!(
        fixture.ctx.tickets.held(),
        0,
        "an expired entry goes when met"
    );
}

#[test]
fn a_wall_clock_past_30_seconds_kills_a_ticket_whatever_the_instant_says() {
    let _home = HomeSandbox::new();
    let fixture = Fixture::new();
    let ticket = fixture.mint(CONTROL);
    fixture.advance(Duration::ZERO, Duration::from_secs(30));
    fixture.assert_dead(&ticket);
}

#[test]
fn an_instant_past_30_seconds_kills_a_ticket_whatever_the_wall_says() {
    let _home = HomeSandbox::new();
    let fixture = Fixture::new();
    let ticket = fixture.mint(CONTROL);
    fixture.advance(Duration::from_secs(30), Duration::ZERO);
    fixture.assert_dead(&ticket);
}

#[test]
fn a_wall_clock_stepped_back_30_seconds_kills_a_ticket() {
    let _home = HomeSandbox::new();
    let fixture = Fixture::new();
    let ticket = fixture.mint(CONTROL);
    let back = Duration::from_secs(30);
    fixture.clock.lock().expect("the clock").wall -= back;
    fixture.assert_dead(&ticket);
}

#[test]
fn a_small_wall_step_either_way_leaves_a_ticket_live() {
    let _home = HomeSandbox::new();
    let fixture = Fixture::new();
    let forward = fixture.mint(CONTROL);
    fixture.advance(Duration::ZERO, Duration::from_secs(29));
    fixture.assert_live(&forward);
    let backward = fixture.mint(CONTROL);
    fixture.clock.lock().expect("the clock").wall -= Duration::from_secs(29);
    fixture.assert_live(&backward);
}

// --- Memory only -------------------------------------------------------------------

fn tree(
    dir: &std::path::Path,
    files: &mut std::collections::BTreeMap<std::path::PathBuf, Vec<u8>>,
) {
    for entry in std::fs::read_dir(dir).expect("read the tree") {
        let path = entry.expect("an entry").path();
        if path.is_dir() {
            tree(&path, files);
        } else {
            files.insert(path.clone(), std::fs::read(&path).expect("read a file"));
        }
    }
}

#[test]
fn a_new_context_holds_no_old_ticket() {
    let _home = HomeSandbox::new();
    let before = Fixture::new();
    let ticket = before.mint(CONTROL);
    let after = ApiContext::for_tests(
        config(),
        clauth_dir().join("status.json"),
        None,
        Box::new(|_, _| HerdrProbeOut::NotInstalled),
    );
    let req = upgrade(
        STREAM_PATH,
        SoloHeader::One(ORIGIN.to_string()),
        &[&offer_of(&ticket)],
    );
    assert_refused(&handle(&after, &req, peer()));
    before.assert_live(&ticket);
}

#[test]
fn a_mint_changes_no_file() {
    let _home = HomeSandbox::new();
    let fixture = Fixture::new();
    let mut before = std::collections::BTreeMap::new();
    tree(&clauth_dir(), &mut before);
    assert!(
        before.keys().any(|path| path.ends_with("devices.json")),
        "the walk reads the tree"
    );
    fixture.mint(CONTROL);
    let mut after = std::collections::BTreeMap::new();
    tree(&clauth_dir(), &mut after);
    assert!(before == after, "a mint moved a file under ~/.clauth");
}

// --- The cap and the sweep ----------------------------------------------------------

#[test]
fn a_ninth_mint_drops_only_that_devices_oldest() {
    let _home = HomeSandbox::new();
    let fixture = Fixture::new();
    let others: Vec<String> = (0..2).map(|_| fixture.mint(VIEW)).collect();
    let mine: Vec<String> = (0..9).map(|_| fixture.mint(CONTROL)).collect();
    assert_eq!(
        fixture.ctx.tickets.held(),
        10,
        "8 of one device, 2 of the other"
    );
    fixture.assert_dead(&mine[0]);
    for ticket in &mine[1..] {
        fixture.assert_live(ticket);
    }
    for ticket in &others {
        fixture.assert_live(ticket);
    }
}

#[test]
fn the_table_never_holds_more_than_8_for_one_pairing() {
    let _home = HomeSandbox::new();
    let fixture = Fixture::new();
    for minted in 1..=20 {
        fixture.mint(CONTROL);
        assert_eq!(fixture.ctx.tickets.held(), minted.min(8));
    }
}

#[test]
fn a_mint_sweeps_every_expired_entry_of_every_device() {
    let _home = HomeSandbox::new();
    let fixture = Fixture::new();
    for _ in 0..8 {
        fixture.mint(VIEW);
    }
    devices::revoke(VIEWER).expect("revoke");
    assert_eq!(fixture.ctx.tickets.held(), 8, "a revoke purges nothing");
    fixture.advance(Duration::from_secs(29), Duration::from_secs(29));
    let survivor = fixture.mint(CONTROL);
    assert_eq!(fixture.ctx.tickets.held(), 9, "nothing has expired yet");
    fixture.advance(Duration::from_secs(1), Duration::from_secs(1));
    fixture.mint(CONTROL);
    assert_eq!(
        fixture.ctx.tickets.held(),
        2,
        "the revoked device's 8 went, the 1 s old ticket and the new one stayed"
    );
    fixture.assert_live(&survivor);
}

// --- What a ticket's types may print -------------------------------------------------

/// An inherent const outranks the blanket trait's, so `IMPLS` reads true
/// exactly when `T: Debug`.
struct Printable<T>(std::marker::PhantomData<T>);

trait NotPrintable {
    const IMPLS: bool = false;
}

impl<T> NotPrintable for Printable<T> {}

impl<T: std::fmt::Debug> Printable<T> {
    const IMPLS: bool = true;
}

/// The list below is every type that holds a ticket's text (`TicketMinted`) or
/// carries an offer toward the table, as of writing. Residual it cannot see: a
/// type added later that holds a ticket and is not listed here.
#[test]
fn no_type_holding_a_ticket_prints_through_debug() {
    let printable = [
        ("String, the probe's control", Printable::<String>::IMPLS),
        ("TicketMinted", Printable::<super::TicketMinted>::IMPLS),
        ("Request", Printable::<Request>::IMPLS),
        ("WsHeaders", Printable::<WsHeaders>::IMPLS),
        ("Offer", Printable::<Offer>::IMPLS),
        ("Tickets", Printable::<Tickets>::IMPLS),
        ("Response", Printable::<Response>::IMPLS),
        ("Handled", Printable::<Handled>::IMPLS),
    ];
    assert_eq!(
        printable,
        [
            ("String, the probe's control", true),
            ("TicketMinted", false),
            ("Request", false),
            ("WsHeaders", false),
            ("Offer", false),
            ("Tickets", false),
            ("Response", false),
            ("Handled", false),
        ]
    );
}
