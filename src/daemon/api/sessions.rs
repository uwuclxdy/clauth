//! `GET /api/v1/sessions` and `GET /api/v1/sessions/{id}` — the Claude Code
//! transcript index a page at a time, and one transcript's records verbatim.
//!
//! Both read the stores `clauth sessions` browses through [`crate::sessions`]
//! and pay only for the rows they return: the listing walks filenames and
//! mtimes, cuts its page, then previews those rows alone; the history page
//! walks one transcript's lines backward from a byte cursor. clauth owns no
//! transcript-turn parser: a record is one JSONL line parsed as a JSON object
//! and handed over as it was written, so a Claude Code record change never
//! needs a clauth release, and the clients hand-model what they render.

use std::cmp::Ordering;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use serde::Serialize;
use utoipa::ToSchema;

use super::http::{Request, Response, sanitize_for_log};
use super::routes::{ApiContext, Caller, ErrorBody};
use crate::logline::logline;
use crate::sessions::{Labels, PAGE_MAX_BYTES, SessionInfo, SessionSource, Walked, newest_first};

const LISTING_LIMIT_DEFAULT: usize = 50;
const LISTING_LIMIT_MAX: usize = 200;
const HISTORY_LIMIT_DEFAULT: usize = 100;
const HISTORY_LIMIT_MAX: usize = 500;

/// The one fixed sentence an id that resolves to no readable transcript
/// carries; nothing off the wire.
const SESSION_NOT_FOUND: &str =
    "no readable transcript of that id in the shared store or a live isolated store";

/// `GET /api/v1/sessions`'s answer.
#[derive(Serialize, ToSchema)]
pub(crate) struct SessionsBody {
    ok: bool,
    /// Newest first: `updated` descending, then `id` ascending.
    sessions: Vec<SessionRow>,
    /// The `before` of the next page, `null` when this page ends the index.
    #[schema(required = true)]
    next_before: Option<String>,
}

/// One row of the transcript index: the fields `clauth sessions --json`
/// emits minus its `tokens`/`cost`, plus the session's name, git branch and
/// the store the transcript lives in.
#[derive(Serialize, ToSchema)]
pub(crate) struct SessionRow {
    /// The transcript filename stem, the id `GET /api/v1/sessions/{id}` pages.
    id: String,
    /// The profile the session last ran under, `null` when unknown.
    #[schema(required = true)]
    last_ran_profile: Option<String>,
    /// The `cwd` off the transcript head, empty when it records none.
    workspace: String,
    /// The transcript's mtime, ISO-8601 UTC (`+00:00`).
    updated: String,
    /// The first user message, redacted and cut like `clauth sessions --json`,
    /// `null` when the head held none.
    #[schema(required = true)]
    first_message: Option<String>,
    /// The last user message, redacted and cut the same way, `null` when the
    /// tail held none.
    #[schema(required = true)]
    last_message: Option<String>,
    /// The session's Claude Code name (`-n`, `/rename`): the newest
    /// `custom-title` record within the transcript's bounded tail window (the
    /// last 1 MiB, `TAIL_MAX` in `src/sessions.rs`), redacted and cut like the
    /// previews; `null` when that window holds no such record, or when the
    /// newest one has no usable name (an older one is never used).
    #[schema(required = true)]
    title: Option<String>,
    /// The branch of the newest user record that names one, within the
    /// transcript's bounded tail window (the last 1 MiB, `TAIL_MAX` in
    /// `src/sessions.rs`), with control and bidi characters dropped; `null`
    /// when none there names one.
    #[schema(required = true)]
    git_branch: Option<String>,
    /// `global`, or `isolated:<profile>` for a live isolated run's own store.
    store: String,
    /// How a command reaches this session now: `mod` when a mod instance is
    /// live, `herdr` when a live pane joins on the id, both, or neither.
    #[schema(required = false)]
    via: Vec<String>,
    /// `terminal_mirror`, `engine_dialogs`, `plan_approval` when the mod alone
    /// reaches the session (`via == ["mod"]`); empty otherwise.
    #[schema(required = false)]
    lacks: Vec<String>,
}

impl SessionRow {
    fn new(info: SessionInfo, labels: Labels, (via, lacks): (Vec<String>, Vec<String>)) -> Self {
        Self {
            id: info.id,
            last_ran_profile: info.last_ran_profile,
            workspace: info.workspace,
            updated: crate::sessions::updated_iso(info.updated),
            first_message: info.first_message,
            last_message: info.last_message,
            title: labels.title,
            git_branch: labels.git_branch,
            store: match info.source {
                SessionSource::Global => "global".to_string(),
                SessionSource::Isolated { profile } => format!("isolated:{profile}"),
            },
            via,
            lacks,
        }
    }
}

/// `GET /api/v1/sessions/{id}`'s answer.
#[derive(Serialize, ToSchema)]
pub(crate) struct HistoryBody {
    ok: bool,
    id: String,
    /// The page's records in file order, oldest first.
    records: Vec<HistoryRecord>,
    /// The `before` of the page of older records, `null` when nothing older
    /// remains.
    #[schema(required = true)]
    next_before: Option<u64>,
    /// Lines in the page's range that were not a JSON object (a torn last line
    /// mid-write, a blank line), skipped rather than served.
    malformed: u32,
}

/// One record of a transcript page.
#[derive(Serialize, ToSchema)]
pub(crate) struct HistoryRecord {
    /// The byte offset the record's line starts at.
    offset: u64,
    /// One Claude Code transcript record, verbatim.
    #[schema(value_type = Object)]
    record: serde_json::Value,
}

/// The listing cursor: the last row's `(updated, id)`, base64url-encoded so it
/// survives whatever query encoding a client applies. Opaque to clients by
/// contract.
struct Cursor {
    updated: SystemTime,
    id: String,
}

impl Cursor {
    fn encode(updated: SystemTime, id: &str) -> String {
        let nanos: i128 = match updated.duration_since(UNIX_EPOCH) {
            Ok(since) => since.as_nanos() as i128,
            Err(before) => -(before.duration().as_nanos() as i128),
        };
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(format!("{nanos}:{id}"))
    }

    fn decode(text: &str) -> Option<Self> {
        let raw = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(text)
            .ok()?;
        let raw = String::from_utf8(raw).ok()?;
        let (nanos, id) = raw.split_once(':')?;
        if id.is_empty() {
            return None;
        }
        let nanos: i128 = nanos.parse().ok()?;
        let magnitude = Duration::from_nanos(u64::try_from(nanos.unsigned_abs()).ok()?);
        let updated = if nanos >= 0 {
            UNIX_EPOCH.checked_add(magnitude)?
        } else {
            UNIX_EPOCH.checked_sub(magnitude)?
        };
        Some(Self {
            updated,
            id: id.to_string(),
        })
    }

    /// Whether `row` comes after this cursor in the listing order.
    fn precedes(&self, row: (SystemTime, &str)) -> bool {
        newest_first((self.updated, &self.id), row) == Ordering::Less
    }
}

/// `?limit=` within `1..=max`, `default` when absent; `None` when present and
/// not that.
fn limit_param(req: &Request, default: usize, max: usize) -> Option<usize> {
    match req.param("limit") {
        None => Some(default),
        Some(raw) => raw
            .parse::<usize>()
            .ok()
            .filter(|limit| (1..=max).contains(limit)),
    }
}

/// `GET /api/v1/sessions` — a page of the transcript index, newest first.
///
/// The walk lists every transcript from filenames and mtimes alone, the cursor
/// and limit cut the page, and only the rows on it are previewed (a bounded
/// head and tail read each) and stamped with their owner — so the whole-store
/// index `clauth sessions` builds is never paid per request.
#[utoipa::path(
    get,
    path = "/api/v1/sessions",
    params(
        ("limit" = Option<u32>, Query, description = "rows per page, 1..=200 (default 50)", minimum = 1, maximum = 200),
        ("before" = Option<String>, Query, description = "the `next_before` of an earlier page; absent for the newest page")
    ),
    responses(
        (status = 200, description = "a page of the Claude Code transcript index across the shared store and every live isolated store, newest first, previews redacted like `clauth sessions --json`; each row also carries its `via`/`lacks` reach, its `title` and `git_branch` (both `null` when the tail window holds none), at the cost of one `herdr pane list` at 2 s per request", body = SessionsBody),
        (status = 400, description = "a limit outside 1..=200, or a before that no earlier page issued (`bad_request`)", body = ErrorBody),
        (status = 401, description = "no bearer, or one matching no paired device (`unauthorized`)", body = ErrorBody),
        (status = 403, description = "a page's origin outside the app_origins list (`origin_not_allowed`); a device paired by a newer clauth with a tier this one does not know (`device_tier_unknown`)", body = ErrorBody),
        (status = 500, description = "the device list does not read (`internal`)", body = ErrorBody)
    ),
    security(("bearer" = ["view"]))
)]
pub(crate) fn sessions(ctx: &ApiContext, req: &Request, _: &Caller<'_>) -> Response {
    let Some(limit) = limit_param(req, LISTING_LIMIT_DEFAULT, LISTING_LIMIT_MAX) else {
        return Response::error(400, "bad_request");
    };
    let cursor = match req.param("before") {
        None => None,
        Some(raw) => match Cursor::decode(raw) {
            Some(cursor) => Some(cursor),
            None => return Response::error(400, "bad_request"),
        },
    };

    let mut rows: Vec<Walked> = crate::sessions::walk()
        .into_iter()
        .filter(|row| {
            cursor
                .as_ref()
                .is_none_or(|cursor| cursor.precedes(row.sort_key()))
        })
        .collect();
    rows.sort_by(|a, b| newest_first(a.sort_key(), b.sort_key()));
    let more = rows.len() > limit;
    rows.truncate(limit);

    let mut page: Vec<(SessionInfo, Labels)> = rows
        .into_iter()
        .map(crate::sessions::preview_labelled)
        .collect();
    crate::sessions::annotate_owners_of(page.iter_mut().map(|(info, _)| info));
    let next_before = if more {
        page.last()
            .map(|(last, _)| Cursor::encode(last.updated, &last.id))
    } else {
        None
    };
    // The reach inputs, each at most one bounded call per listing: the bridge's
    // live instances, and one `herdr pane list` through the seam.
    let mod_ids = mod_session_ids(ctx);
    let pane_ids = herdr_session_ids(ctx);
    Response::serialize(
        200,
        &SessionsBody {
            ok: true,
            sessions: page
                .into_iter()
                .map(|(info, labels)| {
                    let reach = reach(&mod_ids, &pane_ids, &info.id);
                    SessionRow::new(info, labels, reach)
                })
                .collect(),
            next_before,
        },
    )
}

/// The session ids a live mod instance holds, from the bridge's own table.
/// Empty on platforms with no bridge (Windows).
fn mod_session_ids(ctx: &ApiContext) -> std::collections::HashSet<String> {
    #[cfg(unix)]
    {
        if let Some(hub) = &ctx.bridge {
            return hub.live_session_ids().into_iter().collect();
        }
    }
    #[cfg(not(unix))]
    let _ = ctx;
    std::collections::HashSet::new()
}

/// The session ids a live herdr pane joins on, through at most one bounded
/// `herdr pane list` call. herdr absent or unanswerable reads as no pane.
fn herdr_session_ids(ctx: &ApiContext) -> std::collections::HashSet<String> {
    let Ok(panes) = super::panes::listed_panes(ctx) else {
        return std::collections::HashSet::new();
    };
    panes
        .into_iter()
        .filter_map(|pane| pane.agent_session_id().map(str::to_owned))
        .collect()
}

/// One row's `via`/`lacks`: `via` is the sorted reach set, and `lacks` names the
/// surfaces missing when the mod alone reaches the session (SB-12, MB-7).
fn reach(
    mod_ids: &std::collections::HashSet<String>,
    pane_ids: &std::collections::HashSet<String>,
    id: &str,
) -> (Vec<String>, Vec<String>) {
    let mut via = Vec::new();
    if mod_ids.contains(id) {
        via.push("mod".to_string());
    }
    if pane_ids.contains(id) {
        via.push("herdr".to_string());
    }
    via.sort();
    let lacks = if via == ["mod".to_string()] {
        ["terminal_mirror", "engine_dialogs", "plan_approval"]
            .into_iter()
            .map(str::to_string)
            .collect()
    } else {
        Vec::new()
    };
    (via, lacks)
}

/// `GET /api/v1/sessions/{id}` — a page of one transcript's records, paged
/// backward from the file's end by byte offset.
///
/// The id is looked up among the stems the walk yields, never joined onto a
/// store path, so an id spelled as a path can only answer 404. The page is
/// bounded by records and by bytes ([`PAGE_MAX_BYTES`]), and a record longer
/// than the byte bound is served alone, never cut.
#[utoipa::path(
    get,
    path = "/api/v1/sessions/{id}",
    params(
        ("id" = String, Path, description = "the session id: the transcript filename stem, as the listing's `id` and a pane's `agent_session_id` name it"),
        ("limit" = Option<u32>, Query, description = "records per page, 1..=500 (default 100)", minimum = 1, maximum = 500),
        ("before" = Option<u64>, Query, description = "the `next_before` of an earlier page, a byte offset; absent for the newest page")
    ),
    responses(
        (status = 200, description = "the last records whose lines end at or before the cursor, oldest first, each one Claude Code transcript record verbatim; at most 4 MiB of records unless the page's one record is larger", body = HistoryBody),
        (status = 400, description = "a limit outside 1..=500, or a before that is not a byte offset (`bad_request`)", body = ErrorBody),
        (status = 401, description = "no bearer, or one matching no paired device (`unauthorized`)", body = ErrorBody),
        (status = 403, description = "a page's origin outside the app_origins list (`origin_not_allowed`); a device paired by a newer clauth with a tier this one does not know (`device_tier_unknown`)", body = ErrorBody),
        (status = 404, description = "no readable transcript of that id in the shared store or a live isolated store (`session_not_found`)", body = ErrorBody),
        (status = 500, description = "the device list does not read (`internal`)", body = ErrorBody)
    ),
    security(("bearer" = ["view"]))
)]
pub(crate) fn session_history(_: &ApiContext, req: &Request, caller: &Caller<'_>) -> Response {
    let Some(id) = caller.target else {
        return Response::error(500, "internal");
    };
    let Some(limit) = limit_param(req, HISTORY_LIMIT_DEFAULT, HISTORY_LIMIT_MAX) else {
        return Response::error(400, "bad_request");
    };
    let before = match req.param("before") {
        None => None,
        Some(raw) => match raw.parse::<u64>() {
            Ok(offset) => Some(offset),
            Err(_) => return Response::error(400, "bad_request"),
        },
    };
    let Some(located) = crate::sessions::locate(id) else {
        return Response::refused(404, "session_not_found", SESSION_NOT_FOUND);
    };
    let page = match crate::sessions::read_page(&located.path, before, limit, PAGE_MAX_BYTES) {
        Ok(page) => page,
        Err(e) => {
            logline!(
                "clauth api: session '{}' failed to read: {e}",
                sanitize_for_log(id)
            );
            return Response::refused(404, "session_not_found", SESSION_NOT_FOUND);
        }
    };
    Response::serialize(
        200,
        &HistoryBody {
            ok: true,
            id: located.id,
            records: page
                .records
                .into_iter()
                .map(|(offset, record)| HistoryRecord { offset, record })
                .collect(),
            next_before: page.next_before,
            malformed: page.malformed,
        },
    )
}

#[cfg(test)]
#[path = "../../../tests/inline/daemon_api_sessions.rs"]
mod tests;
