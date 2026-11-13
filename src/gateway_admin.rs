//! The client for shunt's pool-account admin routes: add, complete, remove and
//! refresh for the claude, codex and antigravity families, and the typed
//! `/admin/api/pool` listing. A 401 on a mutation is recovered once by
//! rewriting the adopted config's unchanged bytes through
//! [`gateway::write_checked`] (shunt's watcher reloads and re-reads the
//! `${file:}` admin key) and retrying; a second 401, or a poll that never
//! clears, is [`ShuntAdminError::RestartRequired`]. The Overview shunt section
//! is the listing/remove/refresh caller; add and complete stay uncalled until
//! the Setup `+ new` form lands.
//!
//! No credential byte, response body or header value reaches an error here:
//! [`ShuntAdminError`] carries clauth's own sentence per status, and the pasted
//! login code is relayed verbatim into the request body and never stored.

use std::net::SocketAddr;
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::Result;
use serde::Deserialize;

use crate::gateway::{
    AdminMethod, AdminNeed, AdminRequestFailure, ChatgptOauthProvidersError, ConfigEditRefusal,
    GatewayRecord, ProbeAddr, SecretToken, admin_agent, admin_need_in_text, admin_request,
    probe_admin_addr, read_admin_token, read_config_for_edit, write_checked,
};

// ── families ────────────────────────────────────────────────────────────────

/// A pool-account family with admin routes: the URL path's `{family}` segment.
/// kimi is absent by construction (shunt has no kimi admin routes).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PoolFamily {
    Claude,
    Codex,
    Antigravity,
}

impl PoolFamily {
    /// The family's path segment in `/admin/api/accounts/{segment}`; also the
    /// family's label on the shunt form's `family` cycle row.
    pub(crate) fn segment(self) -> &'static str {
        match self {
            PoolFamily::Claude => "claude",
            PoolFamily::Codex => "codex",
            PoolFamily::Antigravity => "antigravity",
        }
    }

    /// The next family the `family` cycle's `space` advances to, wrapping.
    pub(crate) fn next(self) -> Self {
        match self {
            PoolFamily::Claude => PoolFamily::Codex,
            PoolFamily::Codex => PoolFamily::Antigravity,
            PoolFamily::Antigravity => PoolFamily::Claude,
        }
    }

    /// The add body: claude sends `mode: "oauth"` always (refreshable,
    /// usage-readable), never shunt's `setup_token` default; codex and
    /// antigravity take a name alone.
    fn add_body(self, name: &ShuntAccountName) -> String {
        match self {
            PoolFamily::Claude => {
                serde_json::json!({ "name": name.as_str(), "mode": "oauth" }).to_string()
            }
            PoolFamily::Codex | PoolFamily::Antigravity => {
                serde_json::json!({ "name": name.as_str() }).to_string()
            }
        }
    }
}

/// The families shunt has a refresh route for: claude and antigravity only.
/// Codex and kimi are unrepresentable, so their refresh can never be called.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RefreshFamily {
    Claude,
    Antigravity,
}

impl RefreshFamily {
    fn segment(self) -> &'static str {
        match self {
            RefreshFamily::Claude => "claude",
            RefreshFamily::Antigravity => "antigravity",
        }
    }
}

// ── the typed pool listing ─────────────────────────────────────────────────

/// A pool provider's auth mode, as `/admin/api/pool` reports it. kimi and an
/// unknown future mode stay their own variants so the listing carries them
/// inert ([`PoolAuth::family`] is `None` for both).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PoolAuth {
    Claude,
    Codex,
    Kimi,
    Antigravity,
    /// A mode a newer shunt added; listed, never acted on.
    Unknown(String),
}

impl PoolAuth {
    fn from_str(auth: &str) -> Self {
        match auth {
            "claude_oauth" => PoolAuth::Claude,
            "chatgpt_oauth" => PoolAuth::Codex,
            "kimi_oauth" => PoolAuth::Kimi,
            "antigravity_oauth" => PoolAuth::Antigravity,
            other => PoolAuth::Unknown(other.to_string()),
        }
    }

    /// The mutation family this auth mode's accounts belong to; `None` for a
    /// family shunt has no routes for (kimi, and anything unknown).
    pub(crate) fn family(self) -> Option<PoolFamily> {
        match self {
            PoolAuth::Claude => Some(PoolFamily::Claude),
            PoolAuth::Codex => Some(PoolFamily::Codex),
            PoolAuth::Antigravity => Some(PoolFamily::Antigravity),
            PoolAuth::Kimi | PoolAuth::Unknown(_) => None,
        }
    }

    /// The refresh family, claude and antigravity only (shunt has no codex
    /// refresh route).
    pub(crate) fn refresh_family(self) -> Option<RefreshFamily> {
        match self {
            PoolAuth::Claude => Some(RefreshFamily::Claude),
            PoolAuth::Antigravity => Some(RefreshFamily::Antigravity),
            PoolAuth::Codex | PoolAuth::Kimi | PoolAuth::Unknown(_) => None,
        }
    }
}

/// One provider in the pool listing.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PoolProvider {
    pub(crate) name: String,
    pub(crate) auth: PoolAuth,
    pub(crate) accounts: Vec<PoolAccount>,
}

/// One pool account's fields the Overview and Usage panes render. Every other
/// field shunt reports is ignored, `requests_succeeded` included. `paused`
/// defaults: shunt adds the key in v0.52.0, so a row from the v0.48.0 floor
/// lacks it and must read as `false` (unpaused) instead of failing the whole
/// listing; every `Option` field parses an absent key as `None` on its own.
/// The request counters (`requests_attempted`, `requests_failed`,
/// `requests_cancelled`, counted since shunt's process start or the account's
/// add, whichever is later) and
/// `mean_latency_ms` arrive in v0.61.0, so an older row reads `None` for all
/// four; `mean_latency_ms` is also `null` until an attempt got response
/// headers.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub(crate) struct PoolAccount {
    pub(crate) name: String,
    pub(crate) available: bool,
    pub(crate) near_quota: bool,
    pub(crate) cooldown_secs_remaining: Option<u64>,
    pub(crate) needs_relogin: bool,
    #[serde(default)]
    pub(crate) paused: bool,
    pub(crate) disabled: bool,
    pub(crate) utilization_5h: Option<f64>,
    pub(crate) reset_5h: Option<u64>,
    pub(crate) utilization_7d: Option<f64>,
    pub(crate) reset_7d: Option<u64>,
    pub(crate) utilization_7d_oi: Option<f64>,
    pub(crate) reset_7d_oi: Option<u64>,
    pub(crate) plan: Option<String>,
    pub(crate) status: Option<String>,
    pub(crate) requests_attempted: Option<u64>,
    pub(crate) requests_failed: Option<u64>,
    pub(crate) requests_cancelled: Option<u64>,
    pub(crate) mean_latency_ms: Option<f64>,
}

// ── names ───────────────────────────────────────────────────────────────────

/// A validated pool-account name: non-empty, every byte lowercase ASCII, a
/// digit or `-` (shunt's `validate_account_name`, `src/auth/shared.rs:203`
/// @8ed21d0). Only [`ShuntAccountName::new`] constructs one, so an invalid
/// name can never reach a request, and the name is path-safe in a URL segment.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct ShuntAccountName(String);

impl ShuntAccountName {
    /// The name, or `None` when it fails shunt's grammar.
    pub(crate) fn new(name: &str) -> Option<Self> {
        if name.is_empty()
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        {
            return None;
        }
        Some(ShuntAccountName(name.to_string()))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

// ── outcomes ────────────────────────────────────────────────────────────────

/// A started add: the authorize URL the TUI opens in the browser.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AddOutcome {
    pub(crate) authorize_url: String,
}

/// A completed login. shunt's `message` sentence is dropped: a response body
/// never reaches a user surface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CompleteOutcome {
    pub(crate) live: bool,
}

/// A remove. `removed: false` is shunt's "it did not exist", never "gone".
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RemoveOutcome {
    pub(crate) removed: bool,
}

/// A refresh: whether the account still needs a re-login.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RefreshOutcome {
    pub(crate) needs_relogin: bool,
}

// ── errors ──────────────────────────────────────────────────────────────────

/// Why a pool-account admin call failed. One typed error whose [`Display`] is
/// clauth's own sentence per status; no response body, header value or
/// credential byte rides along.
#[derive(Debug)]
pub(crate) enum ShuntAdminError {
    /// clauth's gateway admin token file does not exist; the reads never mint
    /// one.
    NoAdminToken,
    /// The gateway admin token file could not be read; the cause is the source.
    ReadTokenFailed(anyhow::Error),
    /// A daemon holds the singleton but no env record names it, so the
    /// gateway's address cannot be resolved the way the daemon spawned it.
    DaemonEnvUnrecorded,
    /// The gateway did not answer: a refused connection or any other
    /// transport failure.
    Unreachable { addr: SocketAddr },
    /// The gateway did not answer within the route's response bound: a
    /// connect or response that timed out, distinct from
    /// [`ShuntAdminError::Unreachable`] (a refused or failed connection).
    TimedOut { addr: SocketAddr, secs: u64 },
    /// A read's 401/403: the gateway did not accept clauth's admin token.
    Unauthorized { status: u16 },
    /// A mutation's 403: clauth's admin token is a read key, which cannot
    /// change pool accounts.
    ReadOnlyKey,
    /// 400: the gateway refused the request (a bad name, a bad code, or no
    /// pending login).
    BadRequest,
    /// 404: the gateway has no such route or account.
    NotFound,
    /// 429: the gateway rate-limited the request.
    RateLimited,
    /// 500: the gateway failed the request.
    GatewayError,
    /// 502: the gateway's upstream failed (a code exchange that failed or
    /// timed out, a transient refresh failure).
    BadGateway,
    /// A status no admin route maps here.
    UnexpectedStatus { status: u16 },
    /// The 401 recovery could not run: clauth's admin entry is absent from
    /// the adopted config (the [`AdminNeed`] the check found), so a rewrite
    /// cannot reload the key.
    AdminKeyMissing(AdminNeed),
    /// The 401 recovery could not read clauth's admin entry: the config does
    /// not parse, or its `id = "clauth"` entry points at another key file.
    /// The cause is clauth's own sentence, and a restart cannot fix it.
    AdminEntryUnreadable(anyhow::Error),
    /// The 401 recovery could not clear the old admin key: the reload never
    /// happened or the retry was refused again.
    RestartRequired,
    /// The config rewrite that re-arms the watcher failed; the cause is the
    /// source.
    RewriteFailed(anyhow::Error),
    /// A 200 whose body is not the expected JSON.
    Unparseable,
    /// The response body exceeds [`gateway::ADMIN_BODY_LIMIT`].
    TooLarge { limit: u64 },
    /// A 200 whose body stopped before its declared end.
    BodyBroken,
}

impl std::fmt::Display for ShuntAdminError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ShuntAdminError::NoAdminToken => f.write_str("no gateway admin token"),
            ShuntAdminError::ReadTokenFailed(_) => {
                f.write_str("clauth could not read its gateway admin token")
            }
            ShuntAdminError::DaemonEnvUnrecorded => {
                f.write_str("the running daemon recorded no environment; restart it")
            }
            ShuntAdminError::Unreachable { addr } => {
                write!(f, "the gateway at {addr} did not answer")
            }
            ShuntAdminError::TimedOut { addr, secs } => {
                write!(f, "the gateway at {addr} did not answer within {secs} s")
            }
            ShuntAdminError::Unauthorized { status } => {
                write!(
                    f,
                    "the gateway did not accept clauth's admin token (status {status})"
                )
            }
            ShuntAdminError::ReadOnlyKey => f.write_str(
                "clauth's gateway admin token is read-only, so it cannot change pool accounts",
            ),
            ShuntAdminError::BadRequest => {
                write!(f, "the gateway refused the request (status 400)")
            }
            ShuntAdminError::NotFound => {
                write!(f, "the gateway has no such route or account (status 404)")
            }
            ShuntAdminError::RateLimited => {
                write!(f, "the gateway rate-limited the request (status 429)")
            }
            ShuntAdminError::GatewayError => {
                write!(f, "the gateway failed the request (status 500)")
            }
            ShuntAdminError::BadGateway => {
                write!(f, "the gateway's upstream failed (status 502)")
            }
            ShuntAdminError::UnexpectedStatus { status } => {
                write!(f, "the gateway answered status {status}")
            }
            ShuntAdminError::AdminKeyMissing(need) => match need {
                AdminNeed::AdminTable => f.write_str(
                    "the shunt config has no [server.admin] table; add it from the shunt card (f  add admin table)",
                ),
                // WriteKey — and Neither, unreachable here — both name the
                // missing entry itself.
                AdminNeed::WriteKey | AdminNeed::Neither => f.write_str(
                    "clauth's admin key is not in the shunt config; add it from the shunt card (f  add admin key)",
                ),
            },
            ShuntAdminError::AdminEntryUnreadable(err) => write!(
                f,
                "clauth could not read its admin entry in the shunt config: {err}"
            ),
            ShuntAdminError::RestartRequired => f.write_str(
                "the gateway still refuses clauth's admin key; restart the gateway: a → stop shunt, then start shunt",
            ),
            ShuntAdminError::RewriteFailed(_) => f.write_str(
                "clauth could not rewrite the gateway config to reload its admin key; restart the gateway: a → stop shunt, then start shunt",
            ),
            ShuntAdminError::Unparseable => {
                f.write_str("the gateway's admin response does not parse")
            }
            ShuntAdminError::TooLarge { limit } => {
                write!(f, "the gateway's admin response exceeds {limit} bytes")
            }
            ShuntAdminError::BodyBroken => f.write_str("the gateway's admin response broke off"),
        }
    }
}

impl std::error::Error for ShuntAdminError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ShuntAdminError::ReadTokenFailed(cause)
            | ShuntAdminError::RewriteFailed(cause)
            | ShuntAdminError::AdminEntryUnreadable(cause) => Some(cause.as_ref()),
            _ => None,
        }
    }
}

// ── bounds ──────────────────────────────────────────────────────────────────

/// shunt's own bound on the code exchange, `COMPLETION_EXCHANGE_TIMEOUT`
/// (`src/admin/session.rs:238` @8ed21d0): each complete handler awaits it and
/// answers 502 past it, so a client bound must exceed it.
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "doc-only: the complete route's bound sits above this, so a login still exchanging reads as success"
    )
)]
const COMPLETION_EXCHANGE_TIMEOUT_SECS: u64 = 30;

/// The complete route's response bound, above
/// [`COMPLETION_EXCHANGE_TIMEOUT_SECS`] so a login still exchanging reads as
/// success, never a client give-up.
const COMPLETE_RESPONSE_SECS: u64 = 40;

/// The refresh route's response bound. shunt bounds a claude refresh by
/// nothing: `token_refresh_client` carries no timeout (`src/auth/shared.rs`
/// @8ed21d0). An antigravity refresh (`force_refresh_if_access_token`,
/// `src/auth/antigravity/auth.rs` @8ed21d0) sends and reads its token call at
/// 30 s each (`CREDENTIAL_REQUEST_TIMEOUT`), waits up to 5 s for the
/// credential file lock, and, for a stored login with no project id, then
/// runs project discovery (2 x 30 s) and onboarding (2 x 30 s, then its polls
/// under one `ONBOARD_POLL_DEADLINE` of 300 s) and takes the file lock again
/// to merge the project id: about 490 s in all. Each family queues on its own
/// process-global `REFRESH_LOCK` first, unbounded. So a refresh past the pool
/// read's 20 s is shunt still working, never a down gateway: 600 s outlasts
/// one whole antigravity refresh that meets no queue; a claude refresh has no
/// bound to outlast.
const REFRESH_RESPONSE_SECS: u64 = 600;

/// The recovery poll request's response bound: one hung poll answers or
/// refuses within 2 s, so it cannot eat the recovery budget.
const POLL_REQUEST_SECS: u64 = 2;

/// The read-tier GET for the 401 recovery's poll: a pure route-tree snapshot
/// with no account backfill or store scan (shunt `src/admin/mod.rs` `routes`
/// @8ed21d0), present at the v0.48.0 floor.
const READ_POLL_PATH: &str = "/admin/api/routes";

/// How often the recovery re-polls: inside shunt's 400 ms reload debounce
/// (shunt `src/reload.rs` `DEBOUNCE` @8ed21d0), so the first poll after the
/// reload lands within ~200 ms of it, without hammering the gateway.
const RECOVERY_POLL_INTERVAL: Duration = Duration::from_millis(200);

/// How long the recovery polls before giving up: shunt's 400 ms debounce plus
/// its reload re-read, with headroom.
const RECOVERY_POLL_BUDGET: Duration = Duration::from_secs(5);

/// One admin client's route, keying its response bound. The operations build
/// their agent and name their timeout off this one table, and a test reads the
/// same table (through [`route_agent`]'s record) to pin that an operation used
/// the bound its route names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AdminRoute {
    Add,
    Complete,
    Remove,
    Refresh,
    List,
    /// The recovery's read-tier poll.
    Poll,
}

impl AdminRoute {
    /// The bound the route's handler needs: `complete` above shunt's own 30 s
    /// exchange, `refresh` its own 600 s (past one whole antigravity refresh),
    /// the poll its own 2 s, everything else the pool read's 20 s.
    fn response_bound(self) -> Duration {
        match self {
            AdminRoute::Complete => Duration::from_secs(COMPLETE_RESPONSE_SECS),
            AdminRoute::Refresh => Duration::from_secs(REFRESH_RESPONSE_SECS),
            AdminRoute::Poll => poll_request_bound(),
            AdminRoute::Add | AdminRoute::Remove | AdminRoute::List => {
                Duration::from_secs(crate::gateway::POOL_RESPONSE_SECS)
            }
        }
    }
}

/// The client for one route, under the bound [`AdminRoute::response_bound`]
/// names. A `#[cfg(test)]` record notes the bound each call built its agent
/// with, so a test asserts which bound an operation used without reaching into
/// the agent's config.
fn route_agent(route: AdminRoute) -> ureq::Agent {
    let bound = route.response_bound();
    #[cfg(test)]
    record_agent_bound(route, bound);
    admin_agent(bound)
}

#[cfg(test)]
thread_local! {
    static AGENT_BOUNDS: std::cell::RefCell<Vec<(AdminRoute, Duration)>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

#[cfg(test)]
fn record_agent_bound(route: AdminRoute, bound: Duration) {
    AGENT_BOUNDS.with(|cell| cell.borrow_mut().push((route, bound)));
}

#[cfg(test)]
fn recorded_agent_bounds() -> Vec<(AdminRoute, Duration)> {
    AGENT_BOUNDS.with(|cell| cell.borrow().clone())
}

// ── the operations ──────────────────────────────────────────────────────────

/// Start a claude, codex or antigravity add and return the authorize URL.
pub(crate) fn add_pool_account(
    record: &GatewayRecord,
    family: PoolFamily,
    name: &ShuntAccountName,
) -> Result<AddOutcome> {
    let addr = admin_addr(record)?;
    let token = admin_token()?;
    let agent = route_agent(AdminRoute::Add);
    let path = format!("/admin/api/accounts/{}", family.segment());
    let body = family.add_body(name);
    let send = || {
        admin_request(&agent, AdminMethod::Post(Some(&body)), addr, &path, &token)
            .map_err(|failure| transport_err(failure, AdminRoute::Add))
    };
    let body = mutation_with_recovery(record, addr, &token, send)?;
    let wire: AddOutcomeWire =
        serde_json::from_slice(&body).map_err(|_| ShuntAdminError::Unparseable)?;
    Ok(AddOutcome {
        authorize_url: wire.authorize_url,
    })
}

/// Finish an add with the pasted code, relayed verbatim (clauth never parses
/// or stores it).
pub(crate) fn complete_pool_account(
    record: &GatewayRecord,
    family: PoolFamily,
    name: &ShuntAccountName,
    code: &str,
) -> Result<CompleteOutcome> {
    let addr = admin_addr(record)?;
    let token = admin_token()?;
    let agent = route_agent(AdminRoute::Complete);
    let path = format!(
        "/admin/api/accounts/{}/{}/complete",
        family.segment(),
        name.as_str()
    );
    let body = serde_json::json!({ "code": code }).to_string();
    let send = || {
        admin_request(&agent, AdminMethod::Post(Some(&body)), addr, &path, &token)
            .map_err(|failure| transport_err(failure, AdminRoute::Complete))
    };
    let body = mutation_with_recovery(record, addr, &token, send)?;
    let wire: CompleteOutcomeWire =
        serde_json::from_slice(&body).map_err(|_| ShuntAdminError::Unparseable)?;
    Ok(CompleteOutcome { live: wire.live })
}

/// Remove an account; the outcome carries shunt's `removed: bool`.
pub(crate) fn remove_pool_account(
    record: &GatewayRecord,
    family: PoolFamily,
    name: &ShuntAccountName,
) -> Result<RemoveOutcome> {
    let addr = admin_addr(record)?;
    let token = admin_token()?;
    let agent = route_agent(AdminRoute::Remove);
    let path = format!("/admin/api/accounts/{}/{}", family.segment(), name.as_str());
    let send = || {
        admin_request(&agent, AdminMethod::Delete, addr, &path, &token)
            .map_err(|failure| transport_err(failure, AdminRoute::Remove))
    };
    let body = mutation_with_recovery(record, addr, &token, send)?;
    let wire: RemoveOutcomeWire =
        serde_json::from_slice(&body).map_err(|_| ShuntAdminError::Unparseable)?;
    Ok(RemoveOutcome {
        removed: wire.removed,
    })
}

/// Probe one account's refresh grant (claude or antigravity; the type makes
/// codex and kimi unrepresentable).
pub(crate) fn refresh_pool_account(
    record: &GatewayRecord,
    family: RefreshFamily,
    name: &ShuntAccountName,
) -> Result<RefreshOutcome> {
    let addr = admin_addr(record)?;
    let token = admin_token()?;
    let agent = route_agent(AdminRoute::Refresh);
    let path = format!(
        "/admin/api/accounts/{}/{}/refresh",
        family.segment(),
        name.as_str()
    );
    let send = || {
        admin_request(&agent, AdminMethod::Post(None), addr, &path, &token)
            .map_err(|failure| transport_err(failure, AdminRoute::Refresh))
    };
    let body = mutation_with_recovery(record, addr, &token, send)?;
    let wire: RefreshOutcomeWire =
        serde_json::from_slice(&body).map_err(|_| ShuntAdminError::Unparseable)?;
    Ok(RefreshOutcome {
        needs_relogin: wire.needs_relogin,
    })
}

/// The typed pool listing. A read: it never runs the 401 recovery.
pub(crate) fn list_pool(record: &GatewayRecord) -> Result<Vec<PoolProvider>> {
    let addr = admin_addr(record)?;
    let token = admin_token()?;
    let agent = route_agent(AdminRoute::List);
    let (status, body) = admin_request(&agent, AdminMethod::Get, addr, "/admin/api/pool", &token)
        .map_err(|failure| transport_err(failure, AdminRoute::List))?;
    match status {
        200 => {}
        401 | 403 => return Err(ShuntAdminError::Unauthorized { status }.into()),
        other => return Err(status_error(other).into()),
    }
    let parsed: PoolWire =
        serde_json::from_slice(&body).map_err(|_| ShuntAdminError::Unparseable)?;
    Ok(parsed
        .providers
        .into_iter()
        .map(|provider| PoolProvider {
            name: provider.provider,
            auth: PoolAuth::from_str(&provider.auth),
            accounts: provider.accounts,
        })
        .collect())
}

// ── plumbing ────────────────────────────────────────────────────────────────

/// The gateway's probe address, with an unrecorded daemon mapped to this
/// module's typed error; every other probe failure is the probe's own.
fn admin_addr(record: &GatewayRecord) -> Result<SocketAddr> {
    match probe_admin_addr(record)? {
        ProbeAddr::Addr(addr) => Ok(addr),
        ProbeAddr::DaemonEnvUnrecorded => Err(ShuntAdminError::DaemonEnvUnrecorded.into()),
    }
}

/// The gateway's admin token for a read, never minted: a missing file is
/// [`ShuntAdminError::NoAdminToken`], a read failure is
/// [`ShuntAdminError::ReadTokenFailed`].
fn admin_token() -> Result<SecretToken> {
    read_admin_token().map_err(
        |err| match err.downcast_ref::<ChatgptOauthProvidersError>() {
            Some(ChatgptOauthProvidersError::NoAdminToken) => ShuntAdminError::NoAdminToken.into(),
            _ => ShuntAdminError::ReadTokenFailed(err).into(),
        },
    )
}

/// Map an admin request's transport failure to this module's typed error; the
/// route names the timeout bound for [`ShuntAdminError::TimedOut`].
fn transport_err(failure: AdminRequestFailure, route: AdminRoute) -> ShuntAdminError {
    match failure {
        AdminRequestFailure::Unreachable { addr } => ShuntAdminError::Unreachable { addr },
        AdminRequestFailure::TimedOut { addr } => ShuntAdminError::TimedOut {
            addr,
            secs: route.response_bound().as_secs(),
        },
        AdminRequestFailure::TooLarge { limit } => ShuntAdminError::TooLarge { limit },
        AdminRequestFailure::BodyBroken => ShuntAdminError::BodyBroken,
    }
}

/// Map a mutation or read status to its typed error; 200 (parse), 401 and 403
/// (recovery / read-only) are the caller's.
fn status_error(status: u16) -> ShuntAdminError {
    match status {
        400 => ShuntAdminError::BadRequest,
        404 => ShuntAdminError::NotFound,
        429 => ShuntAdminError::RateLimited,
        500 => ShuntAdminError::GatewayError,
        502 => ShuntAdminError::BadGateway,
        other => ShuntAdminError::UnexpectedStatus { status: other },
    }
}

/// Map a mutation send's status to its typed error. A 403 is
/// [`ShuntAdminError::ReadOnlyKey`] on the first send and on the retry alike,
/// so the two arms cannot drift; every other non-200 status maps through
/// [`status_error`]. 401 is the caller's (recovery, or
/// [`ShuntAdminError::RestartRequired`]).
fn mutation_status_error(status: u16) -> ShuntAdminError {
    match status {
        403 => ShuntAdminError::ReadOnlyKey,
        other => status_error(other),
    }
}

/// Send `send` once and, on a 401, recover the admin key and retry exactly
/// once: rewrite the config through [`write_checked`], poll
/// [`READ_POLL_PATH`] until the token is accepted, then re-send. A 403 on
/// either send is [`ShuntAdminError::ReadOnlyKey`] with no recovery; a second
/// 401 (or a poll that never clears while clauth's entry is still present) is
/// [`ShuntAdminError::RestartRequired`].
fn mutation_with_recovery(
    record: &GatewayRecord,
    addr: SocketAddr,
    token: &SecretToken,
    send: impl Fn() -> Result<(u16, Vec<u8>), ShuntAdminError>,
) -> Result<Vec<u8>> {
    match send()? {
        (200, body) => Ok(body),
        (401, _) => {
            recover_admin_key(record, addr, token)?;
            match send()? {
                (200, body) => Ok(body),
                (401, _) => Err(ShuntAdminError::RestartRequired.into()),
                (status, _) => Err(mutation_status_error(status).into()),
            }
        }
        (status, _) => Err(mutation_status_error(status).into()),
    }
}

/// The recovery's one config read: the original bytes [`write_checked`]
/// compares against, their text (the unchanged candidate), and the admin
/// entry's need decided from the same text. A failed read is
/// [`ShuntAdminError::RewriteFailed`]; an unreadable entry is
/// [`ShuntAdminError::AdminEntryUnreadable`].
fn read_admin_entry(config: &Path) -> Result<(Vec<u8>, String, AdminNeed)> {
    let (bytes, text) = read_config_for_edit(config).map_err(ShuntAdminError::RewriteFailed)?;
    let need = admin_need_in_text(&text).map_err(ShuntAdminError::AdminEntryUnreadable)?;
    Ok((bytes, text, need))
}

/// The 401 recovery: when clauth's admin entry is present, rewrite the adopted
/// config's unchanged bytes through [`write_checked`], then poll a read-tier
/// GET with the same token until it stops answering 401, bounded
/// at [`RECOVERY_POLL_BUDGET`]. An absent entry is
/// [`ShuntAdminError::AdminKeyMissing`] with no rewrite and no poll (a rewrite
/// cannot reload a key the config does not carry); an unreadable entry is
/// [`ShuntAdminError::AdminEntryUnreadable`]; a failed rewrite is
/// [`ShuntAdminError::RewriteFailed`]; a poll that never clears is
/// [`ShuntAdminError::RestartRequired`], unless a peer edit removed the entry
/// meanwhile, which is [`ShuntAdminError::AdminKeyMissing`] again.
fn recover_admin_key(record: &GatewayRecord, addr: SocketAddr, token: &SecretToken) -> Result<()> {
    let config = record.config();
    let (bytes, text, need) = read_admin_entry(config)?;
    if need != AdminNeed::Neither {
        return Err(ShuntAdminError::AdminKeyMissing(need).into());
    }
    match write_checked(record, &bytes, &text) {
        Ok(()) => {}
        Err(err) => match err.downcast_ref::<ConfigEditRefusal>() {
            // A peer's write already renamed the config and triggered shunt's
            // reload; go on to the poll: not a failure.
            Some(ConfigEditRefusal::ChangedDuringEdit { .. }) => {}
            _ => return Err(ShuntAdminError::RewriteFailed(err).into()),
        },
    }
    let agent = route_agent(AdminRoute::Poll);
    let deadline = Instant::now() + recovery_poll_budget();
    loop {
        match admin_request(&agent, AdminMethod::Get, addr, READ_POLL_PATH, token) {
            Ok((200, _)) => return Ok(()),
            Ok((401, _)) => {}
            Ok((status, _)) => return Err(status_error(status).into()),
            // A poll that times out is shunt still reloading, not a down
            // gateway: count it as "not yet" until the budget runs out.
            Err(AdminRequestFailure::TimedOut { .. }) => {}
            Err(failure) => return Err(transport_err(failure, AdminRoute::Poll).into()),
        }
        if Instant::now() >= deadline {
            // A peer edit may have removed clauth's entry while the poll ran,
            // which the restart remedy cannot fix: re-read before naming it.
            let (_, _, need) = read_admin_entry(config)?;
            if need != AdminNeed::Neither {
                return Err(ShuntAdminError::AdminKeyMissing(need).into());
            }
            return Err(ShuntAdminError::RestartRequired.into());
        }
        std::thread::sleep(RECOVERY_POLL_INTERVAL);
    }
}

/// The recovery's poll budget, shortened by a test seam so a never-clearing
/// poll is posed without waiting out the real [`RECOVERY_POLL_BUDGET`].
fn recovery_poll_budget() -> Duration {
    #[cfg(all(test, unix))]
    if let Some(bound) = POLL_BUDGET_OVERRIDE.get() {
        return bound;
    }
    RECOVERY_POLL_BUDGET
}

/// The recovery poll request's response bound, shortened by a test seam so a
/// poll that stalls past its bound is posed without waiting out the real
/// [`POLL_REQUEST_SECS`].
fn poll_request_bound() -> Duration {
    #[cfg(all(test, unix))]
    if let Some(bound) = POLL_REQUEST_OVERRIDE.get() {
        return bound;
    }
    Duration::from_secs(POLL_REQUEST_SECS)
}

// Test seam shortening the recovery's poll budget on this thread (the poll
// runs on its caller's thread), on the `CheckTimeoutOverride` precedent in
// `src/gateway.rs`; cleared on drop.
#[cfg(all(test, unix))]
thread_local! {
    static POLL_BUDGET_OVERRIDE: std::cell::Cell<Option<Duration>> =
        const { std::cell::Cell::new(None) };
}

#[cfg(all(test, unix))]
struct PollBudgetOverride(());

#[cfg(all(test, unix))]
impl PollBudgetOverride {
    fn set(bound: Duration) -> Self {
        POLL_BUDGET_OVERRIDE.set(Some(bound));
        Self(())
    }
}

#[cfg(all(test, unix))]
impl Drop for PollBudgetOverride {
    fn drop(&mut self) {
        POLL_BUDGET_OVERRIDE.set(None);
    }
}

// Test seam shortening the recovery poll request's bound on this thread (the
// poll runs on its caller's thread), on the `PollBudgetOverride` precedent
// above; cleared on drop.
#[cfg(all(test, unix))]
thread_local! {
    static POLL_REQUEST_OVERRIDE: std::cell::Cell<Option<Duration>> =
        const { std::cell::Cell::new(None) };
}

#[cfg(all(test, unix))]
struct PollRequestOverride(());

#[cfg(all(test, unix))]
impl PollRequestOverride {
    fn set(bound: Duration) -> Self {
        POLL_REQUEST_OVERRIDE.set(Some(bound));
        Self(())
    }
}

#[cfg(all(test, unix))]
impl Drop for PollRequestOverride {
    fn drop(&mut self) {
        POLL_REQUEST_OVERRIDE.set(None);
    }
}

// ── wire shapes ─────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct AddOutcomeWire {
    authorize_url: String,
}

#[derive(Deserialize)]
struct CompleteOutcomeWire {
    live: bool,
}

#[derive(Deserialize)]
struct RemoveOutcomeWire {
    removed: bool,
}

#[derive(Deserialize)]
struct RefreshOutcomeWire {
    needs_relogin: bool,
}

#[derive(Deserialize)]
struct PoolWire {
    providers: Vec<PoolProviderWire>,
}

#[derive(Deserialize)]
struct PoolProviderWire {
    provider: String,
    /// A string, never a mirror of shunt's `AuthMode`: an unknown future mode
    /// still parses ([`PoolAuth::Unknown`]).
    auth: String,
    accounts: Vec<PoolAccount>,
}

#[cfg(test)]
#[path = "../tests/inline/gateway_admin.rs"]
mod tests;
