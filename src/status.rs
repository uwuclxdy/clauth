//! Claude status feed layer — fetches the Statuspage incidents JSON API at
//! <https://status.claude.com/api/v2/incidents.json>, caches it to disk, and
//! streams results to the TUI over an `mpsc` channel.
//!
//! This module is deliberately TUI-free: it owns the data model, the HTTP
//! fetch, the timestamp parsing, and the on-disk cache, but never touches
//! ratatui. The TUI reads [`StatusEvent`]s and renders [`Incident`]s from its
//! own UI-thread state — there is no shared lock here (the background thread and
//! the UI thread communicate purely through the channel, mirroring `update.rs`).
//!
//! # Source
//!
//! The Statuspage v2 JSON API returns the ~50 most recent incidents (about 30
//! days) with no auth and no pagination. serde deserializes it directly; a thin
//! wire layer mirrors the API and converts into the internal model below, with
//! `#[serde(default)]` on every nullable/optional field and string-enum fallback
//! so an unknown status / impact never errors the whole parse.

use std::sync::mpsc::{Receiver, Sender};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::poll::{first_delay, run_polling_loop};
use crate::profile::clauth_dir;
use crate::usage::{iso_to_epoch_secs, now_ms};

/// Live feed URL (Statuspage v2 JSON API).
const FEED_URL: &str = "https://status.claude.com/api/v2/incidents.json";

/// Background refresh cadence. A manual refresh signal short-circuits the wait.
const REFRESH_INTERVAL: Duration = Duration::from_secs(5 * 60);

/// HTTP connect timeout.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// HTTP response-receive timeout. Counts through the BODY read in ureq 3
/// (see the proxy's pinned timeout-semantics tests), so it must cover the
/// whole ~194 KiB feed on a slow link — background poll, a generous bound
/// costs nothing (timeout-sweep 2026-07-18).
const RECV_TIMEOUT: Duration = Duration::from_secs(30);
/// Hard cap on the response body. The real feed is ~194 KiB; 2 MiB is generous
/// headroom while still bounding a hostile / runaway response.
const MAX_BODY_BYTES: u64 = 2 * 1024 * 1024;

// ── Public data model ───────────────────────────────────────────────────────

/// A single incident from the API. Cached to disk verbatim, so every field is
/// `serde`-round-trippable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Incident {
    /// Incident slug id, stable across refreshes; the TUI uses it to detect a
    /// newly-arrived incident.
    pub(crate) id: String,
    pub(crate) title: String,
    /// `shortlink` — the incident's public page.
    pub(crate) link: String,
    /// Incident-level lifecycle status (the same vocabulary as an update phase).
    pub(crate) phase: UpdatePhase,
    /// Impact severity.
    pub(crate) impact: Impact,
    /// Epoch ms the incident started (`started_at`).
    pub(crate) started_ms: u64,
    /// Epoch ms the incident resolved (`resolved_at`), if it has.
    pub(crate) resolved_ms: Option<u64>,
    /// Affected components from the incident's snapshot: `(name, status)`. Names
    /// are paren-stripped and deduped (first occurrence kept).
    pub(crate) components: Vec<(String, String)>,
    /// Updates in feed order (newest first), as the API delivers them.
    pub(crate) updates: Vec<IncidentUpdate>,
}

impl Incident {
    /// An incident is active while its status isn't a terminal one
    /// (`resolved` / `completed`).
    pub(crate) fn is_active(&self) -> bool {
        !self.phase.is_terminal()
    }
}

/// One status update within an incident's timeline.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct IncidentUpdate {
    pub(crate) phase: UpdatePhase,
    /// Epoch ms from the update's `display_at`.
    pub(crate) at_ms: u64,
    /// Update body text.
    pub(crate) text: String,
    /// Component status changes carried by this update, filtered to entries that
    /// actually changed (`old != new`): `(name, old_status, new_status)`.
    pub(crate) transitions: Vec<(String, String, String)>,
}

/// Statuspage lifecycle phase (incident- and update-level share the vocabulary).
/// `Other` keeps any unrecognized status (lowercased) so it still renders rather
/// than being dropped.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum UpdatePhase {
    // Incident flavors.
    Investigating,
    Identified,
    Monitoring,
    Update,
    Resolved,
    // Maintenance flavors.
    Scheduled,
    InProgress,
    Verifying,
    Completed,
    Other(String),
}

impl UpdatePhase {
    /// Map an API status string (any case) to a phase. Unknown strings become
    /// [`UpdatePhase::Other`] with the lowercased value — never an error.
    pub(crate) fn from_status(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "investigating" => Self::Investigating,
            "identified" => Self::Identified,
            "monitoring" => Self::Monitoring,
            "update" => Self::Update,
            "resolved" => Self::Resolved,
            "scheduled" => Self::Scheduled,
            "in_progress" => Self::InProgress,
            "verifying" => Self::Verifying,
            "completed" => Self::Completed,
            other => Self::Other(other.to_string()),
        }
    }

    /// A terminal phase closes an incident: `resolved` or `completed`.
    pub(crate) fn is_terminal(&self) -> bool {
        matches!(self, Self::Resolved | Self::Completed)
    }

    /// Lowercase display word.
    pub(crate) fn word(&self) -> String {
        match self {
            Self::Investigating => "investigating".into(),
            Self::Identified => "identified".into(),
            Self::Monitoring => "monitoring".into(),
            Self::Update => "update".into(),
            Self::Resolved => "resolved".into(),
            Self::Scheduled => "scheduled".into(),
            Self::InProgress => "in progress".into(),
            Self::Verifying => "verifying".into(),
            Self::Completed => "completed".into(),
            Self::Other(w) => {
                if w.is_empty() {
                    "update".into()
                } else {
                    w.clone()
                }
            }
        }
    }
}

/// Incident impact severity. `Other` keeps an unrecognized value (lowercased).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum Impact {
    None,
    Minor,
    Major,
    Critical,
    Maintenance,
    Other(String),
}

impl Impact {
    /// Map an API impact string (any case) to an [`Impact`]; unknown → `Other`.
    pub(crate) fn from_str(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "none" => Self::None,
            "minor" => Self::Minor,
            "major" => Self::Major,
            "critical" => Self::Critical,
            "maintenance" => Self::Maintenance,
            other => Self::Other(other.to_string()),
        }
    }

    /// Lowercase display word.
    pub(crate) fn word(&self) -> String {
        match self {
            Self::None => "none".into(),
            Self::Minor => "minor".into(),
            Self::Major => "major".into(),
            Self::Critical => "critical".into(),
            Self::Maintenance => "maintenance".into(),
            Self::Other(w) => w.clone(),
        }
    }

    /// Numeric severity for ordering: larger = worse.
    pub(crate) fn severity(&self) -> u8 {
        match self {
            Self::Critical => 4,
            Self::Major => 3,
            Self::Minor => 2,
            Self::Maintenance => 1,
            Self::None | Self::Other(_) => 0,
        }
    }
}

/// Outcome of one fetch attempt, streamed to the TUI.
pub(crate) enum StatusEvent {
    /// Fresh data straight from the network. Render as live.
    Fetched {
        incidents: Vec<Incident>,
        fetched_at_ms: u64,
    },
    /// Startup cache load or a network failure with a cache to fall back on.
    /// Render as cached — staleness is derived from `fetched_at_ms` age.
    Cached {
        incidents: Vec<Incident>,
        fetched_at_ms: u64,
    },
    /// A fetch failed and no cache was available. Carries the error message.
    Failed(String),
}

// ── Disk cache ────────────────────────────────────────────────────────────────

/// On-disk cache shape: the incidents plus the wall-clock time they were fetched.
#[derive(Debug, Serialize, Deserialize)]
struct CacheFile {
    fetched_at_ms: u64,
    incidents: Vec<Incident>,
}

/// `~/.clauth/status_cache.json`. Resolved ONCE at spawn time and passed into
/// the worker so the detached thread never re-resolves `home_dir()` later — that
/// would race a test's `HOME_OVERRIDE` scope and could touch the real `~/.clauth`.
fn cache_path() -> Option<std::path::PathBuf> {
    clauth_dir().ok().map(|d| d.join("status_cache.json"))
}

/// Load the cache at `path` if it exists and parses; `None` on any miss/error.
/// A parse failure (e.g. an old atom-shaped cache) is silently treated as no
/// cache, not an error path.
fn load_cache(path: &std::path::Path) -> Option<CacheFile> {
    let bytes = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&bytes).ok()
}

/// Persist the cache best-effort (atomic tmp + rename). Errors are swallowed —
/// a cache write failing never blocks the live feed.
fn save_cache(path: &std::path::Path, cache: &CacheFile) {
    if let Ok(json) = serde_json::to_string_pretty(cache) {
        let _ = crate::profile::atomic_write_600(path, json);
    }
}

// ── JSON wire layer ───────────────────────────────────────────────────────────

/// Top-level `incidents.json` response. Only `incidents` is modeled.
#[derive(Debug, Deserialize)]
struct IncidentsResponse {
    #[serde(default)]
    incidents: Vec<IncidentWire>,
}

#[derive(Debug, Deserialize)]
struct IncidentWire {
    #[serde(default)]
    id: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    status: String,
    #[serde(default)]
    impact: String,
    #[serde(default)]
    shortlink: String,
    #[serde(default)]
    started_at: Option<String>,
    #[serde(default)]
    created_at: Option<String>,
    #[serde(default)]
    resolved_at: Option<String>,
    #[serde(default)]
    incident_updates: Vec<UpdateWire>,
    #[serde(default)]
    components: Vec<ComponentWire>,
}

#[derive(Debug, Deserialize)]
struct UpdateWire {
    #[serde(default)]
    status: String,
    #[serde(default)]
    body: String,
    #[serde(default)]
    display_at: Option<String>,
    #[serde(default)]
    created_at: Option<String>,
    /// `null` in the JSON when an update changed no components.
    #[serde(default)]
    affected_components: Option<Vec<AffectedComponentWire>>,
}

#[derive(Debug, Deserialize)]
struct AffectedComponentWire {
    #[serde(default)]
    name: String,
    #[serde(default)]
    old_status: String,
    #[serde(default)]
    new_status: String,
}

#[derive(Debug, Deserialize)]
struct ComponentWire {
    #[serde(default)]
    name: String,
    #[serde(default)]
    status: String,
}

/// Strip a trailing/embedded balanced parenthesized group from a component name and
/// tidy the leftover whitespace: `Claude Console (platform.claude.com)` →
/// `Claude Console`. Nested parens are handled by depth-tracking so the whole
/// balanced span is dropped (`Foo (bar (baz) qux)` → `Foo`). An unbalanced `(`
/// (depth never returns to zero) is restored verbatim.
fn strip_parens(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut chars = name.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '(' {
            let mut group = String::from('(');
            let mut depth = 1usize;
            for inner in chars.by_ref() {
                group.push(inner);
                match inner {
                    '(' => depth += 1,
                    ')' => {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    }
                    _ => {}
                }
            }
            if depth != 0 {
                // Unbalanced: restore the consumed text verbatim.
                out.push_str(&group);
            }
        } else {
            out.push(ch);
        }
    }
    // Collapse the double space left where the parenthesized group was and trim the ends.
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Parse a full `incidents.json` body into the internal model. Unknown enum
/// strings fall back; a wire entry missing a usable timestamp is skipped rather
/// than failing the whole parse.
pub(crate) fn parse_incidents(json: &str) -> anyhow::Result<Vec<Incident>> {
    let resp: IncidentsResponse = serde_json::from_str(json)?;
    Ok(resp
        .incidents
        .into_iter()
        .filter_map(wire_to_incident)
        .collect())
}

/// Parse an ISO-8601 timestamp (with optional `.SSS` fraction) into epoch ms.
fn iso_to_ms(s: &str) -> Option<u64> {
    let secs = iso_to_epoch_secs(s.trim())?;
    Some((secs.max(0) as u64).saturating_mul(1000))
}

fn wire_to_incident(w: IncidentWire) -> Option<Incident> {
    // Need a start time for the relative-age display; prefer `started_at`,
    // fall back to `created_at`. An entry with neither is skipped.
    let started_ms = w
        .started_at
        .as_deref()
        .and_then(iso_to_ms)
        .or_else(|| w.created_at.as_deref().and_then(iso_to_ms))?;

    let resolved_ms = w.resolved_at.as_deref().and_then(iso_to_ms);

    let updates: Vec<IncidentUpdate> = w
        .incident_updates
        .into_iter()
        .filter_map(wire_to_update)
        .collect();

    // The components row shows what each component FIRST reported during the
    // incident, not the (often all-operational) closing snapshot. The snapshot
    // status is only the fallback for a component that never appears in a
    // transition.
    let components = dedup_components(
        w.components
            .into_iter()
            .map(|c| {
                let name = strip_parens(&c.name);
                let status = first_reported_status(&name, &updates).unwrap_or(c.status);
                (name, status)
            })
            .filter(|(n, _)| !n.is_empty()),
    );

    Some(Incident {
        id: w.id.trim().to_string(),
        title: w.name.trim().to_string(),
        link: w.shortlink.trim().to_string(),
        phase: UpdatePhase::from_status(&w.status),
        impact: Impact::from_str(&w.impact),
        started_ms,
        resolved_ms,
        components,
        updates,
    })
}

/// The status `name` FIRST reported during the incident: scan updates
/// oldest-first (the array is newest-first, so iterate in reverse) and take the
/// `new_status` of the first transition that names it. `None` when the component
/// never transitions (caller falls back to the closing snapshot status).
///
/// `name` is already paren-stripped; transition names are too, so they compare
/// directly.
fn first_reported_status(name: &str, updates: &[IncidentUpdate]) -> Option<String> {
    updates
        .iter()
        .rev()
        .flat_map(|u| u.transitions.iter())
        .find(|(tname, _, _)| tname == name)
        .map(|(_, _, new)| new.clone())
}

/// Severity rank for a component status — higher is worse. Two raw names can
/// strip to the same display name (e.g. a parenthesized-suffix dupe) with different
/// statuses; the merged entry must show the WORST so a half-degraded component
/// never gets a green dot.
fn status_rank(status: &str) -> u8 {
    match status.trim().to_ascii_lowercase().as_str() {
        "operational" => 0,
        "under_maintenance" => 1,
        "degraded_performance" => 2,
        "partial_outage" => 3,
        "major_outage" => 4,
        // Unknown: ranked alongside maintenance — non-green but not an outage.
        _ => 1,
    }
}

/// Dedup `(name, status)` pairs by name in first-seen order; on a name collision
/// keep the worst status (see [`status_rank`]).
fn dedup_components(it: impl Iterator<Item = (String, String)>) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    for (name, status) in it {
        if let Some(existing) = out.iter_mut().find(|(n, _)| *n == name) {
            if status_rank(&status) > status_rank(&existing.1) {
                existing.1 = status;
            }
        } else {
            out.push((name, status));
        }
    }
    out
}

/// Short human label for a Statuspage component status word, shared by the
/// detail components row and the per-update transition block:
/// `degraded_performance` → `degraded`, `under_maintenance` → `maintenance`,
/// outages keep two words. Unknown values just lose their underscores.
pub(crate) fn shorten_component_status(s: &str) -> String {
    match s.trim().to_ascii_lowercase().as_str() {
        "operational" => "operational".into(),
        "degraded_performance" => "degraded".into(),
        "partial_outage" => "partial outage".into(),
        "major_outage" => "major outage".into(),
        "under_maintenance" => "maintenance".into(),
        other => other.replace('_', " "),
    }
}

fn wire_to_update(w: UpdateWire) -> Option<IncidentUpdate> {
    // `display_at` drives the timeline time column; fall back to `created_at`.
    let at_ms = w
        .display_at
        .as_deref()
        .and_then(iso_to_ms)
        .or_else(|| w.created_at.as_deref().and_then(iso_to_ms))?;

    // Keep only component entries that actually changed status; paren-strip names.
    let transitions = w
        .affected_components
        .unwrap_or_default()
        .into_iter()
        .filter(|c| c.old_status != c.new_status)
        .map(|c| (strip_parens(&c.name), c.old_status, c.new_status))
        .collect();

    Some(IncidentUpdate {
        phase: UpdatePhase::from_status(&w.status),
        at_ms,
        text: w.body.split_whitespace().collect::<Vec<_>>().join(" "),
        transitions,
    })
}

// ── Background thread ───────────────────────────────────────────────────────

/// Spawn the status feed worker. On start it cold-loads the disk cache (so the
/// TUI is never empty offline), then fetches the live feed once the cache has
/// aged past the cadence and loops on it; a `()` on `refresh_rx` triggers an
/// immediate refetch. Exits when the refresh channel disconnects (TUI shutdown).
///
/// Mirrors `update::spawn`: a plain `std::thread`, a ureq agent with short
/// timeouts, and `anyhow::Error::from` for error mapping. No shared
/// lock crosses the thread boundary — only the `StatusEvent` channel does.
pub(crate) fn spawn(tx: Sender<StatusEvent>, refresh_rx: Receiver<()>) {
    // Resolve the cache path on the calling thread, before detaching, so the
    // worker never re-resolves `home_dir()` (which would race HOME overrides).
    let Some(cache_file) = cache_path() else {
        return;
    };
    std::thread::spawn(move || {
        // Cold-fill from cache first so the first paint has data.
        let mut cached_at_ms = None;
        if let Some(cache) = load_cache(&cache_file) {
            cached_at_ms = Some(cache.fetched_at_ms);
            let _ = tx.send(StatusEvent::Cached {
                incidents: cache.incidents,
                fetched_at_ms: cache.fetched_at_ms,
            });
        }

        let first = first_delay(cached_at_ms, now_ms(), REFRESH_INTERVAL);
        run_polling_loop(&refresh_rx, first, REFRESH_INTERVAL, || {
            run_fetch(&tx, &cache_file)
        });
    });
}

/// One fetch attempt. On success: parse, cache, send `Fetched`. On failure:
/// fall back to the cache (`Cached`) when one exists so the UI can clear an
/// in-flight refresh spinner and mark the data stale; only when nothing is
/// cached do we surface `Failed`.
fn run_fetch(tx: &Sender<StatusEvent>, cache_file: &std::path::Path) {
    match fetch_feed() {
        Ok(incidents) => {
            let fetched_at_ms = now_ms();
            save_cache(
                cache_file,
                &CacheFile {
                    fetched_at_ms,
                    incidents: incidents.clone(),
                },
            );
            let _ = tx.send(StatusEvent::Fetched {
                incidents,
                fetched_at_ms,
            });
        }
        Err(e) => match load_cache(cache_file) {
            Some(cache) => {
                let _ = tx.send(StatusEvent::Cached {
                    incidents: cache.incidents,
                    fetched_at_ms: cache.fetched_at_ms,
                });
            }
            None => {
                let _ = tx.send(StatusEvent::Failed(e.to_string()));
            }
        },
    }
}

/// Fetch and parse the live feed. The response body is capped at
/// [`MAX_BODY_BYTES`]; anything larger errors rather than buffering unbounded
/// network input.
fn fetch_feed() -> anyhow::Result<Vec<Incident>> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_connect(Some(CONNECT_TIMEOUT))
        .timeout_recv_response(Some(RECV_TIMEOUT))
        .build()
        .into();

    use std::io::Read as _;
    let reader = agent
        .get(FEED_URL)
        .header("User-Agent", "clauth-status")
        .call()
        .map_err(anyhow::Error::from)?
        .into_body()
        .into_reader();
    // +1 so a body exactly at the cap still trips the over-limit check.
    let mut capped = reader.take(MAX_BODY_BYTES + 1);

    let mut bytes = Vec::new();
    capped
        .read_to_end(&mut bytes)
        .map_err(anyhow::Error::from)?;
    if bytes.len() as u64 > MAX_BODY_BYTES {
        anyhow::bail!("status feed too large");
    }
    let json = String::from_utf8(bytes).map_err(anyhow::Error::from)?;

    parse_incidents(&json)
}

#[cfg(test)]
#[path = "../tests/inline/status_parse.rs"]
mod status_parse;
