//! `POST /api/v1/panes/<id>/ticket` and the upgrade that spends what it mints.
//!
//! A page's `new WebSocket(url, protocols)` can set no `Authorization`, so a
//! browser trades its bearer for a ticket and offers it as a subprotocol beside
//! the fixed [`TERMINAL_PROTOCOL`], which is what the 101 selects. A ticket is
//! one use and 30 s, in memory only, and binds the pane, the pairing and the
//! minting `Origin`. It reaches [`attach`] and nothing else: `routes::handle`
//! reads an offer only where no `Authorization` header was presented and the
//! path is the stream, so it authenticates no route of the table, and its
//! device goes to `terminal::request` alone.
//!
//! The table holds SHA-256 digests, and an offer is digested as it is parsed, so
//! past the mint's own answer no structure this module keeps holds ticket text.
//! No type here derives `Debug`, the mint's answer being the one that holds the
//! text.
//!
//! Every dead presentation answers `Response::unauthorized()` with no device,
//! so a refusal tells no one what the ticket was and earns no connection. Only
//! a presentation of a live ticket can write a line, one fixed line, and it
//! burns the ticket doing so: the first presentation on the stream path spends
//! it, whatever follows.

use std::time::{Duration, Instant, SystemTime};

use sha2::{Digest as _, Sha256};
use subtle::ConstantTimeEq;

use super::devices;
use super::http::{Request, Response, SoloHeader, sanitize_for_log, trim_ows};
use super::routes::{self, ApiContext, Caller, ErrorBody, Handled};
use super::terminal::{self, Credential};
use crate::lockorder::{RankedMutex, rank};
use crate::logline::logline;

/// The one subprotocol a ticket upgrade selects. Fixed, so no request byte
/// reaches the 101's head.
pub(crate) const TERMINAL_PROTOCOL: &str = "clauth.terminal.v1";
/// An offer element is a ticket only as this, byte for byte, plus the shape
/// [`devices::is_well_formed`] accepts.
const TICKET_PREFIX: &str = "clauth.ticket.";
/// A ticket's life, on either clock.
const TTL: Duration = Duration::from_secs(30);
/// Live tickets per pairing; a further mint drops that pairing's oldest.
const PER_DEVICE: usize = 8;

/// The reason an `Origin`-less mint gets. A ticket binds the minting page's
/// origin, and a client that sends none has its bearer on the upgrade.
const ORIGIN_REQUIRED: &str = "a ticket binds the page that minted it, and this request named no \
                               Origin; a client that is not a page sends its bearer on the upgrade";

/// A ticket's SHA-256: all the table and an offer keep of it.
pub(crate) type TicketDigest = [u8; 32];

fn digest_of(ticket: &[u8]) -> TicketDigest {
    Sha256::digest(ticket).into()
}

/// One reading of both clocks. `Instant` stops across a suspend and the wall
/// clock can step, so a ticket dies on whichever reaches its 30 s first.
#[derive(Clone, Copy)]
pub(crate) struct Stamp {
    pub(crate) mono: Instant,
    pub(crate) wall: SystemTime,
}

impl Stamp {
    pub(crate) fn now() -> Self {
        Self {
            mono: Instant::now(),
            wall: SystemTime::now(),
        }
    }
}

/// Where the table reads the time; a test moves the clocks by hand.
pub(crate) type Clock = Box<dyn Fn() -> Stamp + Send + Sync>;

/// What `Sec-WebSocket-Protocol` offered, across every header line.
#[derive(Default)]
pub(crate) struct Offer {
    terminal: bool,
    tickets: Vec<TicketDigest>,
}

impl Offer {
    /// Take in one header line: a comma-separated list, OWS stripped, no
    /// comparison folding case. Any other element is ignored and never selected.
    pub(crate) fn see(&mut self, line: &[u8]) {
        for element in line.split(|&byte| byte == b',').map(trim_ows) {
            if element == TERMINAL_PROTOCOL.as_bytes() {
                self.terminal = true;
            } else if let Some(ticket) = element.strip_prefix(TICKET_PREFIX.as_bytes())
                && std::str::from_utf8(ticket).is_ok_and(devices::is_well_formed)
            {
                self.tickets.push(digest_of(ticket));
            }
        }
    }

    /// The offered ticket, when the offer is exactly one beside
    /// [`TERMINAL_PROTOCOL`]. So the 101's one value is always one the client
    /// offered, and one request is one guess however many it lists.
    pub(crate) fn ticket(&self) -> Option<&TicketDigest> {
        match self.tickets.as_slice() {
            [ticket] if self.terminal => Some(ticket),
            _ => None,
        }
    }
}

/// One live ticket. `device` is the name at the mint, for the refusal line of a
/// row that is gone or was never read.
struct Entry {
    digest: TicketDigest,
    pane_id: String,
    pairing_id: String,
    device: String,
    origin: String,
    minted: Stamp,
}

impl Entry {
    /// Past 30 s on either clock, boundary included. The wall clock counts in
    /// both directions, so a step can only shorten a ticket.
    fn expired(&self, now: Stamp) -> bool {
        let wall = now
            .wall
            .duration_since(self.minted.wall)
            .unwrap_or_else(|behind| behind.duration());
        now.mono.saturating_duration_since(self.minted.mono) >= TTL || wall >= TTL
    }

    /// The answer to a live ticket refused for its pane, its origin or a
    /// revoked device, with its one line. Only a holder of a live ticket gets
    /// here, so the line cannot flood.
    fn refuse_with(&self, response: Response) -> Handled {
        logline!(
            "clauth api: ticket for device '{}' refused: pane, origin or revoked",
            sanitize_for_log(&self.device)
        );
        refused(response)
    }
}

fn refused(response: Response) -> Handled {
    Handled {
        response,
        device: None,
        hijack: None,
    }
}

/// The live tickets, on the [`ApiContext`], so a restart empties them.
pub(crate) struct Tickets {
    table: RankedMutex<Vec<Entry>, rank::TerminalTickets>,
    clock: Clock,
}

impl Tickets {
    pub(crate) fn new(clock: Clock) -> Self {
        Self {
            table: RankedMutex::new(Vec::new()),
            clock,
        }
    }

    /// Remove the entry `digest` names and hand it out: one locked step, so of
    /// any number of presentations exactly one gets it.
    fn take(&self, digest: &TicketDigest) -> Option<Entry> {
        let mut table = self
            .table
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let index = table
            .iter()
            .position(|entry| bool::from(entry.digest.ct_eq(digest)))?;
        Some(table.remove(index))
    }

    /// Hold a new entry. Every expired entry goes first, so a revoked device's
    /// leftovers leave memory at the next mint by anyone, and the entry's own
    /// pairing keeps its newest [`PER_DEVICE`]. The table is in mint order.
    fn hold(&self, entry: Entry) {
        let now = entry.minted;
        let mut table = self
            .table
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        table.retain(|held| !held.expired(now));
        let of_this_pairing = table
            .iter()
            .filter(|held| held.pairing_id == entry.pairing_id)
            .count();
        if of_this_pairing >= PER_DEVICE
            && let Some(oldest) = table
                .iter()
                .position(|held| held.pairing_id == entry.pairing_id)
        {
            table.remove(oldest);
        }
        table.push(entry);
    }

    #[cfg(test)]
    pub(crate) fn held(&self) -> usize {
        self.table
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }
}

/// The body of a mint's `201`.
#[derive(serde::Serialize, utoipa::ToSchema)]
pub(crate) struct TicketMinted {
    ticket: String,
    /// Seconds the ticket lives.
    expires_in: u64,
}

/// `POST /api/v1/panes/<id>/ticket` — mint a one-use ticket for this pane's
/// stream. The view tier may mint, and the upgrade takes its mode from the
/// device's row, so a control device's ticket opens a control stream; no field
/// here names a mode. The pane is not checked: a mint runs no herdr call.
#[utoipa::path(
    post,
    path = "/api/v1/panes/{id}/ticket",
    params(
        ("id" = String, Path, description = "herdr's pane id, as `GET /api/v1/panes` lists it")
    ),
    responses(
        (status = 201, description = "a one-use ticket for this pane's stream, good for 30 seconds from this page's origin: offer it on the upgrade as the subprotocols `clauth.terminal.v1` and `clauth.ticket.<ticket>`; the 101 selects `clauth.terminal.v1`", body = TicketMinted),
        (status = 400, description = "the request named no Origin (`origin_required`)", body = ErrorBody),
        (status = 401, description = "no bearer, or one matching no paired device (`unauthorized`)", body = ErrorBody),
        (status = 403, description = "a page's origin outside the app_origins list, an Origin of null, or one that is repeated or not UTF-8 (`origin_not_allowed`); a device paired by a newer clauth with a tier this one does not know (`device_tier_unknown`)", body = ErrorBody),
        (status = 500, description = "the device list does not read, or no ticket could be drawn (`internal`)", body = ErrorBody)
    ),
    security(("bearer" = ["view"]))
)]
pub(crate) fn mint(ctx: &ApiContext, req: &Request, caller: &Caller<'_>) -> Response {
    let (Some(device), Some(pane_id)) = (caller.device, caller.target) else {
        return Response::error(500, "internal");
    };
    let origin = match &req.cross_origin.origin {
        SoloHeader::Absent => return Response::refused(400, "origin_required", ORIGIN_REQUIRED),
        SoloHeader::One(origin) if origin != "null" => origin,
        unbindable @ (SoloHeader::One(_) | SoloHeader::Unusable) => {
            return Response::refused(
                403,
                "origin_not_allowed",
                routes::origin_refusal(unbindable),
            );
        }
    };
    let ticket = match devices::generate() {
        Ok(ticket) => ticket,
        Err(e) => {
            logline!(
                "clauth api: no ticket minted: {}",
                sanitize_for_log(&format!("{e:#}"))
            );
            return Response::error(500, "internal");
        }
    };
    ctx.tickets.hold(Entry {
        digest: digest_of(ticket.as_bytes()),
        pane_id: pane_id.to_string(),
        pairing_id: device.pairing_id(),
        device: device.name.clone(),
        origin: origin.clone(),
        minted: (ctx.tickets.clock)(),
    });
    Response::serialize(
        201,
        &TicketMinted {
            ticket,
            expires_in: TTL.as_secs(),
        },
    )
}

/// Spend `ticket` on this upgrade: `routes::handle`'s arm for a request with no
/// `Authorization` header on the stream path. The entry leaves the table first,
/// so nothing below runs under its guard and whatever follows finds it gone.
///
/// A wrong bind answers ahead of the store read and ahead of the list, so a
/// `403` means a bind that matched an entry since removed, and only a ticket
/// with a matching bind reads `devices.json`. The row is found by pairing id,
/// so a device revoked and paired again under its name finds none, and the
/// row's own tier and name are what attach. A non-browser can write any
/// `Origin`: the bind stops a page on another origin, never a stolen ticket.
pub(super) fn attach(
    ctx: &ApiContext,
    req: &Request,
    ticket: &TicketDigest,
    pane_id: &str,
) -> Handled {
    let Some(entry) = ctx.tickets.take(ticket) else {
        return refused(Response::unauthorized());
    };
    if entry.expired((ctx.tickets.clock)()) {
        return refused(Response::unauthorized());
    }
    let bound_origin = matches!(
        &req.cross_origin.origin,
        SoloHeader::One(origin) if *origin == entry.origin
    );
    if entry.pane_id != pane_id || !bound_origin {
        return entry.refuse_with(Response::unauthorized());
    }
    let device = match devices::by_pairing_id(&entry.pairing_id) {
        Ok(Some(device)) => device,
        Ok(None) => return entry.refuse_with(Response::unauthorized()),
        Err(e) => return routes::refuse_unread_devices(&e),
    };
    if req.cross_origin.refused {
        return entry.refuse_with(Response::refused(
            403,
            "origin_not_allowed",
            routes::origin_refusal(&req.cross_origin.origin),
        ));
    }
    // A spent ticket earns nothing but its one stream: an upgrade that did not
    // happen closes its connection, as every other refusal here does.
    let handled = terminal::request(ctx, req, &device, pane_id, Credential::Ticket);
    match handled.hijack {
        Some(_) => handled,
        None => refused(handled.response),
    }
}

#[cfg(test)]
#[path = "../../../tests/inline/daemon_api_ticket.rs"]
mod tests;
