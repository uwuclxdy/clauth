//! The clauth-side client for a proxy's control API (`/clauth/v1/*`): the
//! login flow `clauth login --proxy` drives, the typed usage read the
//! scheduler's proxy arm polls, the per-profile settings engine, and the
//! account deletion `clauth delete` sends. [`crate::proxy_check`] drives the
//! same shapes from the checking side and imports its contract primitives
//! from here, so the two cannot drift onto different paths or verdicts.
//!
//! The client never reads, stores or logs an upstream credential: the login's
//! `done` body is read for the account id and the inference key alone, and
//! only the inference key reaches a clauth file (as the profile's api key).
//! Proxy-supplied error text never lands on a clauth surface either — control
//! refusals are reported by their HTTP status alone, with `clauth proxy
//! check` named for the details — and every other proxy-supplied string a
//! clauth line or row carries goes through the checker's escaping
//! ([`Control::printable`]). Every request that carries clauth's admin token
//! first passes the one identity gate, [`Control::bearer`].

use std::path::Path;
use std::time::{Duration, SystemTime};

use anyhow::{Result, anyhow};
use serde_json::{Value, json};

use crate::profile::ProfileName;
use crate::proxy::{CONTROL, Service};

/// How long one control call may take. Shared with the checker, which pins a
/// proxy slower than this.
pub(crate) const CONTROL_TIMEOUT: Duration = Duration::from_secs(15);

/// A control body past this many bytes is not one.
const BODY_LIMIT: u64 = 1 << 20;

// ── credentials ─────────────────────────────────────────────────────────────

/// A credential read from a file: no `Display`, and a `Debug` that never
/// prints the value.
#[derive(Clone)]
pub(crate) struct Secret(String);

impl Secret {
    pub(crate) fn new(value: String) -> Self {
        Self(value)
    }

    pub(crate) fn expose(&self) -> &str {
        &self.0
    }

    pub(crate) fn into_inner(self) -> String {
        self.0
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}

/// One credential alone in a file, never on argv. A trailing newline is
/// dropped; anything else that is not the credential is refused, as is a file
/// other users can read.
pub(crate) fn read_secret_file(path: &Path, what: &str) -> Result<Secret> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) => {
            return Err(crate::usage_error(format!(
                "cannot read {what} file {path:?} ({})",
                e.kind()
            )));
        }
    };
    validate_secret_text(path, what, text)
}

/// The one rule for a credential's text and mode, after it was read: a file
/// other users can read is refused, as is an empty value or one holding
/// whitespace or control characters.
pub(crate) fn validate_secret_text(path: &Path, what: &str, text: String) -> Result<Secret> {
    #[cfg(unix)]
    if secret_file_open_to_others(path) {
        return Err(crate::usage_error(format!(
            "{what} file {path:?} is accessible by other users; run `chmod 600` on it"
        )));
    }
    let value = text.trim_end_matches(['\n', '\r']);
    if value.is_empty() {
        return Err(crate::usage_error(format!("{what} file {path:?} is empty")));
    }
    if value.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return Err(crate::usage_error(format!(
            "{what} file {path:?} holds whitespace or control characters"
        )));
    }
    Ok(Secret(value.to_string()))
}

/// Whether a credential file's mode lets other users read it; `true` also
/// when the file cannot be inspected, so the refusal names `chmod 600` there
/// too. The one check behind both the reader's refusal and the admin-token
/// rule's choice of fix.
#[cfg(unix)]
pub(crate) fn secret_file_open_to_others(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|meta| meta.permissions().mode() & 0o077 != 0)
        .unwrap_or(true)
}

/// Windows has no owner-only file mode to check.
#[cfg(not(unix))]
pub(crate) fn secret_file_open_to_others(_path: &Path) -> bool {
    false
}

/// The proxy's admin token, from the file `clauth proxy enable` minted, under
/// the one admin-token rule ([`crate::proxy_check::read_admin_token_optional`]).
/// A missing one names the enable, never a token value.
fn admin_token(service: &Service) -> Result<Secret> {
    let path = crate::proxy::admin_token_path(service)?;
    match crate::proxy_check::read_admin_token_optional(&path)? {
        Some(secret) => Ok(secret),
        None => Err(crate::usage_error(format!(
            "proxy {:?} has no admin token file {path:?}; run `clauth proxy enable {service}` to mint it",
            service.as_str()
        ))),
    }
}

// ── the wire ────────────────────────────────────────────────────────────────

/// The agent every control call runs on: no redirects (a proxy that sends one
/// is answered by `clauth proxy check`, never followed), no forward proxy —
/// a loopback bind resolves on this host directly.
pub(crate) fn control_agent(timeout: Duration) -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(timeout))
        .http_status_as_error(false)
        .max_redirects(0)
        .max_redirects_will_error(false)
        .proxy(None)
        .tls_config(
            ureq::tls::TlsConfig::builder()
                .root_certs(crate::tls_roots::root_certs())
                .build(),
        )
        .build()
        .into()
}

/// A control-route id: `[a-z0-9][a-z0-9._-]{0,63}`. The shape the checker
/// pins on every id the contract names, so anything clauth puts in a control
/// path or query is proven safe there first.
pub(crate) fn is_id(s: &str) -> bool {
    let mut chars = s.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && s.len() <= 64
        && chars
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-'))
}

/// A login flow id as a path segment: alphanumerics plus `-._~`, never `.`
/// or `..`, 1..=256 long — the same filter the checker's cancel applies, so a
/// flow a departing proxy opened can always be cancelled by path.
fn is_flow_id(f: &str) -> bool {
    !matches!(f, "." | "..")
        && (1..=256).contains(&f.len())
        && f.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_' | '~'))
}

/// The account id as a `?account=` query value, refusing a malformed one.
fn account_query(account: &str) -> Result<String> {
    if is_id(account) {
        Ok(format!("account={account}"))
    } else {
        Err(crate::usage_error(format!(
            "malformed proxy account id {account:?}"
        )))
    }
}

/// The account id as a path segment, refusing a malformed one.
fn account_seg(account: &str) -> Result<String> {
    if is_id(account) {
        Ok(account.to_string())
    } else {
        Err(crate::usage_error(format!(
            "malformed proxy account id {account:?}"
        )))
    }
}

/// The flow id as a path segment, refusing a malformed one.
fn flow_seg(flow: &str) -> Result<String> {
    if is_flow_id(flow) {
        Ok(flow.to_string())
    } else {
        Err(crate::usage_error(format!(
            "malformed proxy login flow id {flow:?}"
        )))
    }
}

/// A control refusal by its HTTP status alone — the body's free-text `reason`
/// stays off clauth's surfaces (`clauth proxy check` reports it). Status 0
/// means the proxy did not answer at all.
fn refused(service: &Service, status: u16) -> anyhow::Error {
    if status == 0 {
        crate::usage_error(format!(
            "proxy {:?} did not answer; run `clauth proxy check {service}` for details",
            service.as_str()
        ))
    } else {
        crate::usage_error(format!(
            "proxy {:?} refused the request (HTTP {status}); run `clauth proxy check {service}` for details",
            service.as_str()
        ))
    }
}

/// Why a login request gave clauth no usable answer, kept apart where the
/// login reads the flow again: a lost answer may have been the one carrying
/// the inference key, and a paste refused 409 may meet a flow the browser door
/// already finished.
#[derive(Debug)]
pub(crate) enum Unusable {
    /// The answer never arrived whole: none at all, or one cut off mid-body.
    Lost,
    /// A non-2xx answer, by its status.
    Status(u16),
    /// Every other refusal (the identity gate, a malformed body), as its line.
    Refused(anyhow::Error),
}

impl Unusable {
    /// The line the login fails with.
    fn into_error(self, service: &Service) -> anyhow::Error {
        match self {
            Self::Lost => refused(service, 0),
            Self::Status(status) => refused(service, status),
            Self::Refused(error) => error,
        }
    }
}

/// A login that finished without leaving clauth an inference key.
pub(crate) fn keyless(service: &Service) -> anyhow::Error {
    anyhow!(
        "proxy {:?} completed the login without an inference key; run `clauth proxy check \
         {service}` for details",
        service.as_str()
    )
}

// ── the identity gate ───────────────────────────────────────────────────────

/// What a login refused mid-flow left behind: the flow already exists on the
/// proxy, but no profile was created or changed.
const MID_LOGIN: &str = "The login stopped and clauth saved nothing";

/// A proxy's registered bind with clauth's admin token for it.
/// [`Control::connect`] is the only way to hold that token here, every control
/// call takes a `Control`, and the token leaves only through
/// [`Control::bearer`], which asks the bind to name itself first on EVERY
/// request: a foreign process on the port, or a proxy on a contract clauth
/// does not speak, is sent nothing — also when it took the port over midway
/// through a long login.
#[derive(Clone)]
pub(crate) struct Control {
    service: Service,
    addr: std::net::SocketAddr,
    base: String,
    admin: Secret,
}

impl Control {
    /// The registered row's bind and the admin token under the one
    /// admin-token rule. Sends nothing: the identity gate runs per request.
    pub(crate) fn connect(service: &Service) -> Result<Self> {
        let row = crate::proxy::Registry::load()?
            .get(service)
            .cloned()
            .ok_or_else(|| crate::proxy::not_registered(service))?;
        let addr = crate::proxy::bind(row.port);
        Ok(Self {
            service: service.clone(),
            addr,
            base: format!("http://{addr}"),
            admin: admin_token(service)?,
        })
    }

    pub(crate) fn service(&self) -> &Service {
        &self.service
    }

    fn url(&self, path: &str) -> String {
        format!("{}{CONTROL}{path}", self.base)
    }

    /// The `Authorization` value for ONE request, handed out only once the
    /// bind answered `/health` as this proxy on a contract major clauth speaks,
    /// held to the daemon probe's own parse and bounds
    /// ([`crate::gateway::probe_proxy_health`]). A bind that does not refuses
    /// naming what answered and what clauth expects, then `consequence`, a
    /// capitalized sentence saying what the caller therefore did not do.
    ///
    /// The residual window is the gap between that `/health` answer and the
    /// request it admits: a process that takes the port over inside it still
    /// receives the bearer. Asking per request shrinks it from the length of
    /// a login (minutes) to one loopback round trip.
    fn bearer(&self, consequence: &str) -> Result<String> {
        if let Some(answered) = foreign_answer(&self.service, self.addr) {
            return Err(crate::usage_error(format!(
                "proxy {:?} at {} is not answering as itself (expected service {:?}, \
                 contract {}): {answered}. {consequence}. Check it with `clauth proxy list`, \
                 then retry",
                self.service.as_str(),
                self.addr,
                self.service.as_str(),
                crate::proxy::SPOKEN_MAJOR
            )));
        }
        Ok(format!("Bearer {}", self.admin.expose()))
    }

    /// Proxy-supplied text as a clauth line or row carries it: the admin
    /// token redacted, control and bidi characters escaped, cut short.
    pub(crate) fn printable(&self, text: &str) -> String {
        crate::proxy_check::quoted_value(text, &[self.admin.expose().to_string()])
    }

    /// A proxy-supplied login page as clauth prints it: escaped and redacted
    /// like [`Self::printable`], never cut, since the user copies it whole
    /// wherever the opener cannot reach a browser.
    pub(crate) fn printable_url(&self, url: &str) -> String {
        crate::proxy_check::printable(url, &[self.admin.expose().to_string()], usize::MAX)
    }
}

/// What answered `addr`'s `/health`, worded for [`Control::bearer`]'s
/// refusal, when it is not `service` on a contract major clauth speaks; `None`
/// when it is. A `/health` whose `status` is not `ok` still names the proxy:
/// an unhealthy proxy is the expected one, and its control routes answer for
/// themselves.
pub(crate) fn foreign_answer(service: &Service, addr: std::net::SocketAddr) -> Option<String> {
    use crate::gateway::ProxyProbe;
    // The quotes bound a foreign value, so a `"` inside it is escaped too.
    let quote = |text: &str| {
        format!(
            "\"{}\"",
            crate::proxy_check::quoted_value(text, &[]).replace('"', "\\\"")
        )
    };
    match crate::gateway::probe_proxy_health(addr) {
        Err(_) => Some("its /health did not answer".to_string()),
        Ok(ProxyProbe::Silent) => Some("nothing answers there".to_string()),
        Ok(ProxyProbe::NotProxy { status }) => Some(format!(
            "something answers there with HTTP {status} and no proxy /health"
        )),
        Ok(ProxyProbe::Answered(health)) => match health.service.as_deref() {
            None => Some("a proxy answers naming no service".to_string()),
            Some(other) if other != service.as_str() => {
                Some(format!("a proxy answers as service {}", quote(other)))
            }
            Some(_) => match crate::proxy::contract_version(&health.contract) {
                Some((major, _)) if major == crate::proxy::SPOKEN_MAJOR => None,
                _ => Some(format!(
                    "it answers on contract {}",
                    quote(&health.contract)
                )),
            },
        },
    }
}

// ── the login flow ──────────────────────────────────────────────────────────

/// A login flow the proxy opened.
pub(crate) struct FlowStart {
    pub(crate) flow: String,
    /// The page to open, the proxy's own login URL: a plain `http(s)://`
    /// URL with no control, bidi or whitespace character ([`plain_url`]).
    pub(crate) url: String,
    /// The doors the proxy offers (`poll`, `paste`).
    pub(crate) modes: Vec<String>,
    /// What the flow's `poll_interval_ms` hint asks clauth to pace at, floored
    /// at the contract's 1000 ms and capped at [`POLL_CEILING`].
    pub(crate) poll_interval: Duration,
    /// `expires_at` as epoch seconds — the poll deadline.
    expires_epoch: i64,
}

/// One read of a login flow.
#[derive(Debug)]
pub(crate) enum FlowState {
    Pending,
    /// The proxy completed the login. `inference_key` is absent on a re-bind,
    /// which keeps the account's existing key, and on every answer after the
    /// one that first saw `done`.
    Done {
        /// The `id` of the account object the body names.
        account: String,
        inference_key: Option<Secret>,
    },
    Failed,
    Expired,
    /// The flow reads back 404: expired, cancelled, or never the proxy's.
    Gone,
}

/// What a completed login yields. The `done` body may carry more (a re-bind
/// keeps upstream fields beside the key); only these two are read, and only
/// the key is a credential.
#[derive(Debug)]
pub(crate) struct LoginOutcome {
    pub(crate) account: String,
    pub(crate) inference_key: Option<Secret>,
}

/// The doors [`LoginFlow::await_done`] watches besides the browser's (the
/// poll): the paste door and an interrupt. Real callers pace with
/// [`std::thread::sleep`]; tests pass a no-op.
pub(crate) struct AwaitLogin<'a> {
    /// Codes from the paste door, when the flow offers it: the first to land
    /// is submitted, a closed sender closes the door and the poll keeps
    /// waiting ([`crate::oauth_login::PasteDoor`]).
    pub(crate) paste: &'a std::sync::mpsc::Receiver<String>,
    /// The SIGINT or SIGTERM that reached clauth since the login began.
    pub(crate) interrupted: &'a mut dyn FnMut() -> Option<i32>,
    pub(crate) wait: &'a dyn Fn(Duration),
    /// One line on stderr about a login that keeps waiting.
    pub(crate) say: &'a dyn Fn(std::fmt::Arguments<'_>),
    /// The profile the login is for, which an interrupted re-login names.
    pub(crate) profile: &'a str,
    /// On a re-bind, whether the profile stores a key it keeps now: a
    /// non-blank one with no dead-key verdict on it. The proxy hands a key out
    /// only with a new account's `done`, so a login that finishes keyless
    /// mints one (`POST /accounts/{id}/key`), except on a re-bind this answers
    /// true for: the account has one key, and a mint replaces it under every
    /// session using the stored one. A key carrying the verdict is minted
    /// over, since this login is that verdict's cure. Asked once the flow is
    /// done, so a key another clauth stored meanwhile counts; the save asks
    /// again under the state flock, and a key cleared since is minted there
    /// ([`mint_rebound_key`]).
    pub(crate) key_stored: &'a dyn Fn() -> bool,
    /// On a new login, the profile whose `[proxy]` record names an account,
    /// read fresh. A bare login of an upstream user the proxy already holds
    /// re-binds that account, so a `done` is no proof of a new account: the
    /// holder's key is never minted over and its account never deleted.
    pub(crate) holder: &'a dyn Fn(&str) -> Option<ProfileName>,
}

/// The longest clauth waits between two polls, whatever the flow's hint asks:
/// a proxy forgets a finished flow 60 s after it ends (the core's
/// `TERMINAL_GRACE_MS`), so the read after a `done` lands inside that grace
/// even when a poll waits out its whole [`CONTROL_TIMEOUT`] too.
const POLL_CEILING: Duration = Duration::from_secs(30);

/// How long the wait between two polls sleeps before it looks at the paste
/// door and the interrupt again, so either lands within this of arriving.
const DOOR_SLICE: Duration = Duration::from_millis(100);

/// The login stopped on SIGINT or SIGTERM. Its text is the line the CLI prints
/// before it exits `128 + signal` (130 for a Ctrl-C), saying how the login
/// ended on the proxy: its flow cancelled (or not confirmed so), or, for a
/// flow the proxy had already finished, the account it created and what
/// clauth did with it. A second signal during that work returns
/// [`crate::Interrupted`] in its place, with nothing to print.
#[derive(Debug)]
pub(crate) struct LoginInterrupted {
    service: Service,
    signal: i32,
    ending: Ending,
}

/// How an interrupted login ended on the proxy.
#[derive(Debug)]
enum Ending {
    /// The flow cancel ran; `confirmed` when the proxy answered it with a 2xx
    /// or a 404.
    Cancelled { confirmed: bool },
    /// The flow was already `done` on a new account, which clauth deleted.
    Deleted { account: String },
    /// The flow was already `done` on a new account, and its delete failed.
    DeleteFailed { account: String },
    /// The flow was already `done` on a re-login, or on a new login of an
    /// account `profile` holds: its account is bound to the new upstream login
    /// and stays.
    Rebound { profile: String },
}

impl LoginInterrupted {
    /// The signal that stopped the login.
    pub(crate) fn signal(&self) -> i32 {
        self.signal
    }
}

impl std::fmt::Display for LoginInterrupted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let service = self.service.as_str();
        match &self.ending {
            Ending::Cancelled { confirmed: true } => write!(
                f,
                "clauth: login cancelled; proxy {service:?} stopped its login flow"
            ),
            Ending::Cancelled { confirmed: false } => write!(
                f,
                "clauth: login cancelled, but proxy {service:?} did not confirm its login flow \
                 stopped; do not finish the login in the browser"
            ),
            Ending::Deleted { account } => write!(
                f,
                "clauth: login cancelled; proxy {service:?} had already created account \
                 {account:?} and clauth deleted it"
            ),
            Ending::DeleteFailed { account } => write!(
                f,
                "clauth: login cancelled after proxy {service:?} created account {account:?}, \
                 and deleting it failed; delete it on the proxy"
            ),
            Ending::Rebound { profile } => write!(
                f,
                "clauth: login cancelled too late: proxy {service:?} had already re-bound \
                 profile {profile:?}, which stays logged in"
            ),
        }
    }
}

impl std::error::Error for LoginInterrupted {}

/// Why the poll loop stopped short of `done`.
enum Halt {
    /// SIGINT or SIGTERM, by its number.
    Signal(i32),
    Failed(anyhow::Error),
}

impl From<anyhow::Error> for Halt {
    fn from(e: anyhow::Error) -> Self {
        Self::Failed(e)
    }
}

/// An open login flow. Every failed exit after the proxy opened it, and before
/// the flow read `done`, stops the proxy polling upstream for it
/// (`DELETE /accounts/login/{flow}`): [`Self::await_done`] cancels on a
/// refused paste, a dropped poll or paste the read after it cannot place, a failed or
/// expired flow and a signal, [`Self::cancel`] before an exit that leaves
/// it unawaited, and the flow cancels itself when it drops un-awaited.
pub(crate) struct LoginFlow<'a> {
    control: &'a Control,
    start: FlowStart,
    /// The account the start re-bound, when it re-bound one rather than
    /// creating one.
    rebind: Option<String>,
    open: bool,
}

impl LoginFlow<'_> {
    pub(crate) fn start(&self) -> &FlowStart {
        &self.start
    }

    /// Poll the flow until it completes, racing the paste door and the
    /// interrupt against the poll, then keep the key ([`Self::with_key`]). A
    /// failed exit before `done` cancels the flow, raced
    /// against a signal the way every request of the login is; a signal
    /// during a failure's cancel still reports the failure
    /// ([`crate::InterruptedAfter`]).
    pub(crate) fn await_done(mut self, mut opts: AwaitLogin<'_>) -> Result<LoginOutcome> {
        let halt = match poll_until_done(self.control, &self.start, &mut opts) {
            Ok(done) => {
                // `done`: the flow is over on the proxy's side, nothing to cancel.
                self.open = false;
                match self.with_key(done, &mut opts) {
                    Ok(outcome) => return Ok(outcome),
                    // The account stands as the proxy finished it; a cancel
                    // of its done flow would change nothing.
                    Err(Halt::Failed(error)) => return Err(error),
                    Err(signal) => signal,
                }
            }
            Err(halt) => halt,
        };
        self.open = false;
        let signal = match halt {
            Halt::Failed(error) => {
                let cancelled =
                    cancel_unless_signalled(self.control, &self.start.flow, opts.interrupted);
                return Err(match cancelled {
                    Ok(_) => error,
                    Err(signal) => crate::InterruptedAfter { error, signal }.into(),
                });
            }
            Halt::Signal(signal) => signal,
        };
        let ending = self
            .interrupted_ending(signal, &mut opts)
            .map_err(|second| anyhow::Error::from(crate::Interrupted(second)))?;
        Err(LoginInterrupted {
            service: self.control.service().clone(),
            signal,
            ending,
        }
        .into())
    }

    /// `done` with the key the login keeps: the one its answer carried, the
    /// profile's stored one on a re-bind ([`AwaitLogin::key_stored`]), or else
    /// one minted for the account now, raced like every request of the login.
    /// A re-bind that landed on another account mints nothing, nor does a new
    /// login of an account a profile holds ([`AwaitLogin::holder`]): that key
    /// is another login's, and the caller refuses the account.
    fn with_key(
        &self,
        done: LoginOutcome,
        opts: &mut AwaitLogin<'_>,
    ) -> std::result::Result<LoginOutcome, Halt> {
        let keeps = match &self.rebind {
            Some(own) => *own != done.account || (opts.key_stored)(),
            None => (opts.holder)(&done.account).is_some(),
        };
        if done.inference_key.is_some() || keeps {
            return Ok(done);
        }
        let (worker, account) = (self.control.clone(), done.account.clone());
        let key = unless_signalled(opts.interrupted, move || mint_key(&worker, &account))
            .map_err(Halt::Signal)??;
        Ok(LoginOutcome {
            account: done.account,
            inference_key: Some(key),
        })
    }

    /// What a signal ends the login with. The proxy may have finished the
    /// flow already (a browser door that landed after the last poll, or a
    /// poll in flight the signal cut loose), so the flow is read once more,
    /// raced like every request of the login: a `done` new account is
    /// deleted, a `done` re-login is left bound, and a `done` on an account a
    /// profile holds ([`AwaitLogin::holder`]) is left bound in that profile's
    /// name; anything else is cancelled. `signal` began the ending, so it is
    /// the ending's first; `Err` is a second signal, by its number.
    fn interrupted_ending(
        &self,
        signal: i32,
        opts: &mut AwaitLogin<'_>,
    ) -> std::result::Result<Ending, i32> {
        let (worker, flow) = (self.control.clone(), self.start.flow.clone());
        let last = unless_signalled(opts.interrupted, move || read_login_flow(&worker, &flow))?;
        if let Ok(FlowState::Done { account, .. }) = last {
            if self.rebind.is_some() {
                return Ok(Ending::Rebound {
                    profile: opts.profile.to_string(),
                });
            }
            if let Some(holder) = (opts.holder)(&account) {
                return Ok(Ending::Rebound {
                    profile: holder.to_string(),
                });
            }
            let deleted = delete_unless_signalled(
                self.control,
                &account,
                &mut Some(signal),
                opts.interrupted,
            )?;
            return Ok(if deleted {
                Ending::Deleted { account }
            } else {
                Ending::DeleteFailed { account }
            });
        }
        cancel_unless_signalled(self.control, &self.start.flow, opts.interrupted)
            .map(|confirmed| Ending::Cancelled { confirmed })
    }

    /// Cancel the flow now, before an exit that leaves it unawaited, raced
    /// against a signal: a second one returns [`crate::Interrupted`].
    pub(crate) fn cancel(&mut self, interrupted: &mut dyn FnMut() -> Option<i32>) -> Result<()> {
        self.open = false;
        cancel_unless_signalled(self.control, &self.start.flow, interrupted)
            .map(|_| ())
            .map_err(|signal| crate::Interrupted(signal).into())
    }
}

/// A request running on a thread of its own while the login thread watches a
/// signal ([`Self::wait`]). Waited on past a first signal
/// ([`Self::wait_past_first`]) only by the start, whose flow a signal must not
/// leave open, and by the delete of a new login's account.
struct InFlight<T> {
    answered: std::sync::mpsc::Receiver<T>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl<T: Send + 'static> InFlight<T> {
    fn spawn(work: impl FnOnce() -> T + Send + 'static) -> Self {
        let (answer, answered) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let _ = answer.send(work());
        });
        Self {
            answered,
            worker: Some(worker),
        }
    }

    /// The work's answer, or the signal that came first, looked for every
    /// [`DOOR_SLICE`]. A signal leaves the work running, detached, for as long
    /// as clauth lives, so a request a wedged proxy never answers holds a
    /// Ctrl-C up no longer than a slice.
    fn wait(
        &mut self,
        interrupted: &mut dyn FnMut() -> Option<i32>,
    ) -> std::result::Result<T, i32> {
        use std::sync::mpsc::RecvTimeoutError;
        loop {
            if let Some(signal) = interrupted() {
                return Err(signal);
            }
            match self.answered.recv_timeout(DOOR_SLICE) {
                Ok(value) => return Ok(value),
                Err(RecvTimeoutError::Timeout) => {}
                // The sender drops unsent only when `work` panicked: the panic
                // is the caller's, as it would be had the work run inline.
                Err(RecvTimeoutError::Disconnected) => std::panic::resume_unwind(
                    self.worker
                        .take()
                        .and_then(|worker| worker.join().err())
                        .unwrap_or_else(|| Box::new("the login request ended unanswered")),
                ),
            }
        }
    }

    /// [`Self::wait`] that lets one signal by: the first, kept in `first`
    /// when it arrives here, leaves the work waited out, and only a signal
    /// past it comes back. A `first` already set lets none by.
    fn wait_past_first(
        &mut self,
        first: &mut Option<i32>,
        interrupted: &mut dyn FnMut() -> Option<i32>,
    ) -> std::result::Result<T, i32> {
        loop {
            match self.wait(interrupted) {
                Err(signal) if first.is_none() => *first = Some(signal),
                answered => return answered,
            }
        }
    }
}

/// Run `work` on a thread of its own while this one watches `interrupted`
/// ([`InFlight::wait`]): the work's answer, or the signal that came first.
fn unless_signalled<T: Send + 'static>(
    interrupted: &mut dyn FnMut() -> Option<i32>,
    work: impl FnOnce() -> T + Send + 'static,
) -> std::result::Result<T, i32> {
    InFlight::spawn(work).wait(interrupted)
}

/// The flow cancel, raced against a signal ([`unless_signalled`]): whether
/// the proxy confirmed it, or the signal that came first, the operator's way
/// out of a cancel still waiting out its bound.
fn cancel_unless_signalled(
    control: &Control,
    flow: &str,
    interrupted: &mut dyn FnMut() -> Option<i32>,
) -> std::result::Result<bool, i32> {
    let (control, flow) = (control.clone(), flow.to_string());
    unless_signalled(interrupted, move || cancel_login(&control, &flow))
}

/// The delete of an account a NEW login just created ([`delete_account`]),
/// run to its end past the first signal of the login's ending
/// ([`InFlight::wait_past_first`]): whether it succeeded, or the signal past
/// that first one, which cuts it. The one delete both ends of such a login
/// share: a signal once the flow read `done` ([`LoginFlow::await_done`]),
/// which is that ending's first, and a save that failed after it
/// ([`refuse_unsaved_account`]), whose first is the one pending from the save
/// or arriving during the delete.
fn delete_unless_signalled(
    control: &Control,
    account: &str,
    first: &mut Option<i32>,
    interrupted: &mut dyn FnMut() -> Option<i32>,
) -> std::result::Result<bool, i32> {
    let (service, account) = (control.service().clone(), account.to_string());
    InFlight::spawn(move || delete_account(&service, &account).is_ok())
        .wait_past_first(first, interrupted)
}

/// A NEW login whose profile save failed after the proxy answered `done`:
/// the account it created is deleted on the proxy, so no provider login
/// stays there with no profile behind it, and the save's `error` comes back
/// saying what became of that account ([`UnsavedAccount`]). A signal pending
/// from the save, or the first during the delete, leaves the delete running
/// to its end, and the run then ends at `128 +` that signal with the same
/// error ([`crate::InterruptedAfter`]); a further signal during the delete
/// ends the run at once with the save's failure alone, at `128 +` that one.
/// A re-login never comes here: its account is the profile's own and stays;
/// nor does a save that found the account held ([`AccountHeld`]), which is
/// that profile's.
pub(crate) fn refuse_unsaved_account(
    control: &Control,
    account: &str,
    error: anyhow::Error,
    interrupted: &mut dyn FnMut() -> Option<i32>,
) -> anyhow::Error {
    let mut first = None;
    match delete_unless_signalled(control, account, &mut first, interrupted) {
        Ok(deleted) => {
            let unsaved = UnsavedAccount {
                error,
                service: control.service().clone(),
                account: account.to_string(),
                deleted,
            }
            .into();
            match first {
                Some(signal) => crate::InterruptedAfter {
                    error: unsaved,
                    signal,
                }
                .into(),
                None => unsaved,
            }
        }
        Err(signal) => crate::InterruptedAfter { error, signal }.into(),
    }
}

/// A new login's failed save ([`refuse_unsaved_account`]): the save's own
/// text, then what became of the account the proxy had just created. The
/// save's causes stay its causes.
#[derive(Debug)]
struct UnsavedAccount {
    error: anyhow::Error,
    service: Service,
    account: String,
    deleted: bool,
}

impl std::fmt::Display for UnsavedAccount {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (service, account) = (self.service.as_str(), &self.account);
        if self.deleted {
            write!(
                f,
                "{}; clauth deleted the account proxy {service:?} had just created ({account:?})",
                self.error
            )
        } else {
            write!(
                f,
                "{}, and deleting the account proxy {service:?} had just created ({account:?}) \
                 failed; delete it on the proxy",
                self.error
            )
        }
    }
}

impl std::error::Error for UnsavedAccount {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.error.source()
    }
}

/// A new login's save refused, under the state flock, because `holder`'s
/// `[proxy]` record names the account: a claim that landed after the login
/// read the roster at `done`. The login ends on [`held_account`], and the
/// account is never deleted.
#[derive(Debug)]
pub(crate) struct AccountHeld {
    pub(crate) holder: ProfileName,
}

impl std::fmt::Display for AccountHeld {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "the proxy account is held by '{}'", self.holder)
    }
}

impl std::error::Error for AccountHeld {}

/// What a new login of an account `holder` already holds ends with: the proxy
/// re-bound that account, and clauth changed nothing.
pub(crate) fn held_account(
    control: &Control,
    target: &ProfileName,
    account: &str,
    holder: &ProfileName,
) -> anyhow::Error {
    anyhow!(
        "proxy {:?} signed '{target}' in as account {:?}, which '{holder}' already holds: \
         '{holder}' is logged in again, and clauth saved no '{target}'",
        control.service().as_str(),
        control.printable(account)
    )
}

impl Drop for LoginFlow<'_> {
    fn drop(&mut self) {
        if self.open {
            cancel_login(self.control, &self.start.flow);
        }
    }
}

/// Open a login flow on the proxy (`POST /accounts/login`), re-binding the
/// existing upstream login of `rebind` when given, raced against
/// `interrupted` ([`unless_signalled`]). The proxy opens the flow once it
/// handles the request, whenever clauth stopped waiting, so a signal before
/// it answered keeps waiting for that answer (a second signal returns
/// [`crate::Interrupted`] at once) and cancels the flow it names: the login
/// ends as [`LoginInterrupted`], or as [`crate::Interrupted`] when the start
/// opened none.
pub(crate) fn start_login<'a>(
    control: &'a Control,
    rebind: Option<&str>,
    interrupted: &mut dyn FnMut() -> Option<i32>,
) -> Result<LoginFlow<'a>> {
    let request = match rebind {
        None => json!({}),
        Some(id) => json!({ "account": account_seg(id)? }),
    };
    let worker = control.clone();
    let second = |signal| anyhow::Error::from(crate::Interrupted(signal));
    let mut first = None;
    let opened = InFlight::spawn(move || open_flow(&worker, &request))
        .wait_past_first(&mut first, interrupted)
        .map_err(second)?;
    let start = match first {
        None => opened?,
        Some(signal) => {
            let Ok(start) = opened else {
                return Err(crate::Interrupted(signal).into());
            };
            let confirmed =
                cancel_unless_signalled(control, &start.flow, interrupted).map_err(second)?;
            return Err(LoginInterrupted {
                service: control.service().clone(),
                signal,
                ending: Ending::Cancelled { confirmed },
            }
            .into());
        }
    };
    Ok(LoginFlow {
        control,
        start,
        rebind: rebind.map(str::to_string),
        open: true,
    })
}

/// [`start_login`]'s request. A start body that names a flow but is otherwise
/// unusable cancels that flow before refusing.
fn open_flow(control: &Control, request: &Value) -> Result<FlowStart> {
    let service = control.service();
    let mut response = control_agent(CONTROL_TIMEOUT)
        .post(&control.url("/accounts/login"))
        .header("authorization", &control.bearer("No login was started")?)
        .header("content-type", "application/json")
        .send(request.to_string().as_str())
        .map_err(|_| refused(service, 0))?;
    let status = response.status().as_u16();
    if !(200..300).contains(&status) {
        return Err(refused(service, status));
    }
    let body = read_body(service, &mut response)?;
    let flow = body
        .get("flow")
        .and_then(Value::as_str)
        .filter(|f| is_flow_id(f))
        .ok_or_else(|| malformed(service, "flow"))?
        .to_string();
    flow_start(service, &body, flow.clone()).inspect_err(|_| {
        cancel_login(control, &flow);
    })
}

/// The rest of a login-start body once its flow id parsed.
fn flow_start(service: &Service, body: &Value, flow: String) -> Result<FlowStart> {
    let url = body
        .get("url")
        .and_then(Value::as_str)
        .filter(|u| plain_url(u))
        .ok_or_else(|| malformed(service, "url"))?
        .to_string();
    let modes = body
        .get("modes")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    let poll_interval = body
        .get("poll_interval_ms")
        .and_then(Value::as_u64)
        .filter(|ms| *ms >= 1000)
        .map_or(Duration::from_millis(1000), Duration::from_millis)
        .min(POLL_CEILING);
    let expires_epoch = body
        .get("expires_at")
        .and_then(Value::as_str)
        .and_then(crate::usage::iso_to_epoch_secs)
        .ok_or_else(|| malformed(service, "expires_at"))?;
    Ok(FlowStart {
        flow,
        url,
        modes,
        poll_interval,
        expires_epoch,
    })
}

/// A login page clauth may print and hand to the browser opener: `http://` or
/// `https://` and a host, with no control, bidi or whitespace character
/// anywhere, so the URL can neither drive the terminal nor split into a
/// second opener argument.
fn plain_url(url: &str) -> bool {
    url.strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .is_some_and(|rest| !rest.is_empty())
        && crate::proxy_check::is_plain(url)
        && !url.chars().any(char::is_whitespace)
}

fn malformed(service: &Service, field: &str) -> anyhow::Error {
    crate::usage_error(format!(
        "proxy {:?} answered a login body without a usable {field}; run `clauth proxy check {service}` for details",
        service.as_str()
    ))
}

fn read_body(service: &Service, response: &mut ureq::http::Response<ureq::Body>) -> Result<Value> {
    let text = body_text(response).ok_or_else(|| refused(service, 0))?;
    json_body(service, &text)
}

/// A response's body, `None` when it does not arrive whole within the cap.
fn body_text(response: &mut ureq::http::Response<ureq::Body>) -> Option<String> {
    response
        .body_mut()
        .with_config()
        .limit(BODY_LIMIT)
        .read_to_string()
        .ok()
}

fn json_body(service: &Service, text: &str) -> Result<Value> {
    serde_json::from_str(text).map_err(|_| malformed(service, "JSON body"))
}

/// Read a login flow once (`GET /accounts/login/{flow}`).
pub(crate) fn read_login_flow(control: &Control, flow: &str) -> Result<FlowState> {
    poll_flow(control, flow).map_err(|unusable| unusable.into_error(control.service()))
}

/// [`read_login_flow`] with a lost answer told apart.
fn poll_flow(control: &Control, flow: &str) -> std::result::Result<FlowState, Unusable> {
    let service = control.service();
    let seg = flow_seg(flow).map_err(Unusable::Refused)?;
    let bearer = control.bearer(MID_LOGIN).map_err(Unusable::Refused)?;
    let Ok(mut response) = control_agent(CONTROL_TIMEOUT)
        .get(&control.url(&format!("/accounts/login/{seg}")))
        .header("authorization", &bearer)
        .call()
    else {
        return Err(Unusable::Lost);
    };
    let status = response.status().as_u16();
    if status == 404 {
        return Ok(FlowState::Gone);
    }
    if !(200..300).contains(&status) {
        return Err(Unusable::Status(status));
    }
    let text = body_text(&mut response).ok_or(Unusable::Lost)?;
    json_body(service, &text)
        .and_then(|body| flow_state(service, &body))
        .map_err(Unusable::Refused)
}

/// A login-status body as one read of its flow. A `done` body names the
/// account as the account object, of which only its `id` is read.
fn flow_state(service: &Service, body: &Value) -> Result<FlowState> {
    match body.get("state").and_then(Value::as_str) {
        Some("pending") => Ok(FlowState::Pending),
        Some("done") => {
            let account = body
                .get("account")
                .and_then(|account| account.get("id"))
                .and_then(Value::as_str)
                .filter(|a| is_id(a))
                .ok_or_else(|| malformed(service, "account"))?
                .to_string();
            Ok(FlowState::Done {
                account,
                inference_key: inference_key(body),
            })
        }
        Some("failed") => Ok(FlowState::Failed),
        Some("expired") => Ok(FlowState::Expired),
        _ => Err(malformed(service, "state")),
    }
}

/// The `inference_key` a login-status or key body carries; a blank one is
/// none, since it authenticates nothing.
fn inference_key(body: &Value) -> Option<Secret> {
    body.get("inference_key")
        .and_then(Value::as_str)
        .filter(|key| !key.trim().is_empty())
        .map(|key| Secret::new(key.to_string()))
}

/// Submit a code through the paste door (`POST /accounts/login/{flow}`): the
/// flow as the proxy's answer reads it, or why it gave none.
pub(crate) fn submit_paste(
    control: &Control,
    flow: &str,
    code: &str,
) -> std::result::Result<FlowState, Unusable> {
    let service = control.service();
    let seg = flow_seg(flow).map_err(Unusable::Refused)?;
    let bearer = control.bearer(MID_LOGIN).map_err(Unusable::Refused)?;
    let Ok(mut response) = control_agent(CONTROL_TIMEOUT)
        .post(&control.url(&format!("/accounts/login/{seg}")))
        .header("authorization", &bearer)
        .header("content-type", "application/json")
        .send(json!({ "code": code }).to_string().as_str())
    else {
        return Err(Unusable::Lost);
    };
    let status = response.status().as_u16();
    if !(200..300).contains(&status) {
        return Err(Unusable::Status(status));
    }
    let text = body_text(&mut response).ok_or(Unusable::Lost)?;
    json_body(service, &text)
        .and_then(|body| flow_state(service, &body))
        .map_err(Unusable::Refused)
}

/// Mint the account a new inference key (`POST /accounts/{id}/key`). The
/// proxy keeps one key per account, so the new one replaces whatever it held.
fn mint_key(control: &Control, account: &str) -> Result<Secret> {
    let service = control.service();
    let seg = account_seg(account)?;
    let mut response = control_agent(CONTROL_TIMEOUT)
        .post(&control.url(&format!("/accounts/{seg}/key")))
        .header("authorization", &control.bearer(MID_LOGIN)?)
        .send_empty()
        .map_err(|_| refused(service, 0))?;
    let status = response.status().as_u16();
    if !(200..300).contains(&status) {
        return Err(refused(service, status));
    }
    inference_key(&read_body(service, &mut response)?).ok_or_else(|| keyless(service))
}

/// Mint `account` a key for re-bound `profile` after its login's flow read
/// `done`: the save found the key the flow kept on the profile's word gone.
/// Raced against a signal like every request of the login; a signal ends it
/// the way it ends a `done` re-login, the profile left bound and nothing
/// saved.
pub(crate) fn mint_rebound_key(
    control: &Control,
    account: &str,
    profile: &str,
    interrupted: &mut dyn FnMut() -> Option<i32>,
) -> Result<Secret> {
    let (worker, id) = (control.clone(), account.to_string());
    match unless_signalled(interrupted, move || mint_key(&worker, &id)) {
        Ok(key) => key,
        Err(signal) => Err(LoginInterrupted {
            service: control.service().clone(),
            signal,
            ending: Ending::Rebound {
                profile: profile.to_string(),
            },
        }
        .into()),
    }
}

/// Cancel a flow (`DELETE /accounts/login/{flow}`) so it stops polling
/// upstream: whether the proxy confirmed it, by a 2xx or by a 404 (a proxy
/// that already forgot the flow). A bind no longer answering as the proxy is
/// sent nothing, which confirms nothing.
fn cancel_login(control: &Control, flow: &str) -> bool {
    let Ok(seg) = flow_seg(flow) else {
        return false;
    };
    let Ok(bearer) = control.bearer(MID_LOGIN) else {
        return false;
    };
    control_agent(CONTROL_TIMEOUT)
        .delete(&control.url(&format!("/accounts/login/{seg}")))
        .header("authorization", &bearer)
        .call()
        .is_ok_and(|response| {
            let status = response.status().as_u16();
            (200..300).contains(&status) || status == 404
        })
}

/// The poll loop behind [`LoginFlow::await_done`], which cancels the flow on
/// every halt it returns. Each poll and paste runs raced against the
/// interrupt ([`unless_signalled`]), and between two polls it races the paste
/// door and the interrupt the way the OAuth login's wait races its two doors
/// ([`crate::oauth_login::PasteDoor`]): the first to deliver wins, so a
/// browser `done` lands while the paste prompt still waits, and a pasted code
/// is submitted the moment it arrives. A `done` may come back keyless: its
/// key went to an answer clauth lost, or to another reader.
fn poll_until_done(
    control: &Control,
    start: &FlowStart,
    opts: &mut AwaitLogin<'_>,
) -> std::result::Result<LoginOutcome, Halt> {
    let service = control.service();
    let mut paste = crate::oauth_login::PasteDoor::new(opts.paste);
    loop {
        let state = match read_raced(control, &start.flow, opts)? {
            Ok(state) => state,
            Err(Unusable::Lost) => read_again(control, &start.flow, opts)?,
            Err(unusable) => return Err(unusable.into_error(service).into()),
        };
        if let Some(end) = settled(state) {
            return end;
        }
        if crate::usage::now_epoch_secs() >= start.expires_epoch {
            return Err(anyhow!("the login flow expired; {LOGIN_AGAIN}").into());
        }
        let mut waited = Duration::ZERO;
        while waited < start.poll_interval {
            if let Some(signal) = (opts.interrupted)() {
                return Err(Halt::Signal(signal));
            }
            if let Some(code) = paste.poll() {
                let (worker, flow) = (control.clone(), start.flow.clone());
                match unless_signalled(opts.interrupted, move || {
                    submit_paste(&worker, &flow, &code)
                })
                .map_err(Halt::Signal)?
                {
                    // The paste that completes a new account's flow is the one
                    // answer carrying its key. A keyless `done` can be a flow
                    // the browser door finished while the paste was in flight,
                    // its key still waiting for the next read, so only a key
                    // ends the login here; the poll reads every other answer
                    // again.
                    Ok(FlowState::Done {
                        account,
                        inference_key: Some(key),
                    }) => {
                        return Ok(LoginOutcome {
                            account,
                            inference_key: Some(key),
                        });
                    }
                    Ok(_) => {}
                    // A paste into a flow that is no longer pending is refused
                    // 409, and the browser door may be what finished it: one
                    // read decides, a `done` ends the login, any other state
                    // fails as the refused paste did, and a read that fails
                    // fails with its own line.
                    Err(Unusable::Status(409)) => {
                        return match read_raced(control, &start.flow, opts)? {
                            Ok(FlowState::Done {
                                account,
                                inference_key,
                            }) => Ok(LoginOutcome {
                                account,
                                inference_key,
                            }),
                            Ok(_) => Err(refused(service, 409).into()),
                            Err(unusable) => Err(unusable.into_error(service).into()),
                        };
                    }
                    // The proxy could not check the code upstream and keeps
                    // the flow pending with its poll door running, so the
                    // browser can still finish it. The prompt's reader read
                    // its one line, so no second paste can come.
                    Err(Unusable::Status(503)) => (opts.say)(format_args!(
                        "clauth: proxy {:?} could not check that code (HTTP 503); finish \
                         signing in in the browser, or press Ctrl-C and run the login again",
                        service.as_str()
                    )),
                    // The lost answer may have been the one carrying the
                    // key of the account the paste completed, as a lost poll
                    // answer may be.
                    Err(Unusable::Lost) => {
                        if let Some(end) = settled(read_again(control, &start.flow, opts)?) {
                            return end;
                        }
                    }
                    Err(unusable) => return Err(unusable.into_error(service).into()),
                }
                break;
            }
            let slice = DOOR_SLICE.min(start.poll_interval - waited);
            (opts.wait)(slice);
            waited += slice;
        }
    }
}

/// What a login that fails on the flow's own verdict tells the user to do.
const LOGIN_AGAIN: &str = "start `clauth login <name> --proxy <service>` again";

/// What one read of the flow ends the login with; `None` while it is pending.
fn settled(state: FlowState) -> Option<std::result::Result<LoginOutcome, Halt>> {
    match state {
        FlowState::Pending => None,
        FlowState::Done {
            account,
            inference_key,
        } => Some(Ok(LoginOutcome {
            account,
            inference_key,
        })),
        FlowState::Failed => Some(Err(
            anyhow!("the proxy's login failed; {LOGIN_AGAIN}").into()
        )),
        FlowState::Expired | FlowState::Gone => Some(Err(anyhow!(
            "the login flow expired or is gone; {LOGIN_AGAIN}"
        )
        .into())),
    }
}

/// The read after an answer that never arrived whole, which may have been the
/// `done` carrying the key: the flow as it reads now, the lost answer's
/// failure when it reads gone, or this read's own failure (a lost answer
/// again, a status, the identity gate's refusal).
fn read_again(
    control: &Control,
    flow: &str,
    opts: &mut AwaitLogin<'_>,
) -> std::result::Result<FlowState, Halt> {
    match read_raced(control, flow, opts)? {
        Ok(FlowState::Gone) => Err(refused(control.service(), 0).into()),
        Ok(state) => Ok(state),
        Err(unusable) => Err(unusable.into_error(control.service()).into()),
    }
}

/// One read of the flow ([`poll_flow`]), raced against the interrupt.
fn read_raced(
    control: &Control,
    flow: &str,
    opts: &mut AwaitLogin<'_>,
) -> std::result::Result<std::result::Result<FlowState, Unusable>, Halt> {
    let (worker, flow) = (control.clone(), flow.to_string());
    unless_signalled(opts.interrupted, move || poll_flow(&worker, &flow)).map_err(Halt::Signal)
}

// ── account deletion ────────────────────────────────────────────────────────

/// Delete the proxy account (`DELETE /accounts/{id}`, which drops its upstream
/// login). The proxy must answer as itself first ([`Control::bearer`]): a
/// refusal here deletes nothing, and the caller removes nothing locally.
///
/// A 404 `not_found` reads as already deleted — a prior delete that landed
/// before its local half — so a retry converges.
pub(crate) fn delete_account(service: &Service, account: &str) -> Result<()> {
    let seg = account_seg(account)?;
    let control = Control::connect(service)?;
    let mut response = control_agent(CONTROL_TIMEOUT)
        .delete(&control.url(&format!("/accounts/{seg}")))
        .header("authorization", &control.bearer("Nothing was deleted")?)
        .call()
        .map_err(|_| {
            crate::usage_error(format!(
                "proxy {:?} at {} did not answer the account delete. Nothing was deleted. Check \
                 it with `clauth proxy list`, then retry",
                service.as_str(),
                control.addr
            ))
        })?;
    let status = response.status().as_u16();
    if status == 204 {
        return Ok(());
    }
    if status == 404
        && let Ok(body) = read_body(service, &mut response)
        && body.get("error").and_then(Value::as_str) == Some("not_found")
    {
        return Ok(());
    }
    Err(crate::usage_error(format!(
        "proxy {:?} refused the account delete (HTTP {status}); nothing was deleted. Run \
         `clauth proxy check {service}` for details",
        service.as_str()
    )))
}

// ── the usage read ──────────────────────────────────────────────────────────

/// The profile's usage figures, read through the proxy's
/// `GET /clauth/v1/usage?account={id}` with clauth's admin token, once the
/// proxy answered as itself. The profile's own api key never rides this call:
/// the proxy authenticates the read by its admin, and the figures are the
/// account's, not the key's.
///
/// Every failure collapses to a cache-fallback verdict — the figure goes
/// stale keeping its last value — never [`crate::providers::ThirdPartyError::AuthExpired`]:
/// a refused read is about clauth's admin token or the proxy itself, never
/// the inference key the account spends.
pub(crate) fn fetch_usage(
    service: &Service,
    account: &str,
) -> std::result::Result<crate::providers::ThirdPartyStats, crate::providers::ThirdPartyError> {
    use crate::providers::ThirdPartyError;
    let query = account_query(account).map_err(|_| ThirdPartyError::Parse)?;
    let control = Control::connect(service).map_err(|_| ThirdPartyError::Network)?;
    let value = usage_body(&control, &format!("/usage?{query}"))?;
    let block = value
        .get("accounts")
        .and_then(Value::as_array)
        .and_then(|accounts| {
            accounts
                .iter()
                .find(|a| a.get("account").and_then(Value::as_str) == Some(account))
        })
        .ok_or(ThirdPartyError::Parse)?;
    let (stats, dates) = account_stats(block, &|text| control.printable(text))?;
    if let Some(dates) = dates {
        ReadDate::record(dates);
    }
    Ok(stats)
}

/// The profile's whole proxy pool, read through `GET /clauth/v1/usage` with
/// clauth's admin token, once the proxy answered as itself: every account the
/// proxy holds, the pool-wide totals, one bar per balance pool and one row per
/// account. The profile's own api key never rides this call — a funnel profile
/// spends through whichever account the proxy routes to, never one key.
///
/// Every failure collapses to a cache-fallback verdict for the same reason
/// [`fetch_usage`]'s does: the read speaks about clauth's admin token and the
/// proxy, never about an account's inference key.
pub(crate) fn fetch_pool_usage(
    service: &Service,
) -> std::result::Result<crate::providers::ThirdPartyStats, crate::providers::ThirdPartyError> {
    use crate::providers::ThirdPartyError;
    let control = Control::connect(service).map_err(|_| ThirdPartyError::Network)?;
    let value = usage_body(&control, "/usage")?;
    let accounts = value
        .get("accounts")
        .and_then(Value::as_array)
        .ok_or(ThirdPartyError::Parse)?;
    let (stats, dates) = pool_stats(accounts, &|text| control.printable(text))?;
    if let Some(dates) = dates {
        ReadDate::record(dates);
    }
    Ok(stats)
}

/// One `/usage` body, read past [`Control::bearer`]'s identity gate, with the
/// error mapping every control read shares: a 429 keeps the server's
/// retry-after, any other refusal is its status alone, and a transport or
/// parse failure gets its own verdict.
fn usage_body(
    control: &Control,
    path: &str,
) -> std::result::Result<Value, crate::providers::ThirdPartyError> {
    use crate::providers::ThirdPartyError;
    let bearer = control
        .bearer("Nothing was read")
        .map_err(|_| ThirdPartyError::Network)?;
    let mut response = control_agent(CONTROL_TIMEOUT)
        .get(&control.url(path))
        .header("authorization", &bearer)
        .call()
        .map_err(|_| ThirdPartyError::Network)?;
    let status = response.status().as_u16();
    if status == 429 {
        let retry_after = response
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(crate::usage::parse_retry_after);
        return Err(ThirdPartyError::RateLimited { retry_after });
    }
    if status >= 400 {
        return Err(ThirdPartyError::Status);
    }
    let body = response
        .body_mut()
        .with_config()
        .limit(BODY_LIMIT)
        .read_to_string()
        .map_err(|_| ThirdPartyError::Network)?;
    serde_json::from_str(&body).map_err(|_| ThirdPartyError::Parse)
}

// ── the reading's date ──────────────────────────────────────────────────────

/// The two dates of one proxy reading, each the OLDEST `read_at` among a set
/// of the figures clauth stores (a figure the proxy marks stale keeps its old
/// `read_at`).
#[derive(Clone, Copy, Debug)]
pub(crate) struct ReadDates {
    /// Among every stored figure: the age clauth's copy shows, never younger
    /// than its stalest figure.
    pub(crate) stalest: SystemTime,
    /// Among the 5h/7d chain windows alone, `None` when no stored chain
    /// window carries one: whether the walk may act on the reading, so a
    /// stale display figure never holds back what its chain windows call for.
    pub(crate) chain: Option<SystemTime>,
}

thread_local! {
    /// `Some` while a [`ReadDate`] is held on this thread; inside, the dates
    /// of the reading [`fetch_usage`] last returned here.
    static READ_DATE: std::cell::Cell<Option<Option<ReadDates>>> =
        const { std::cell::Cell::new(None) };
}

/// The [`ReadDates`] of the proxy reading a fetch returns on the calling
/// thread. The fetch leg holds one across the fetcher call on the worker
/// thread that runs it and dates the cache it writes from it; the fetcher's
/// signature carries the stats alone, which every provider shares. `!Send`,
/// so it cannot be carried off the thread the fetch runs on.
pub(crate) struct ReadDate(std::marker::PhantomData<*const ()>);

impl ReadDate {
    pub(crate) fn capture_here() -> Self {
        READ_DATE.set(Some(None));
        Self(std::marker::PhantomData)
    }

    /// The dates of the reading the fetch returned, `None` when it was no
    /// proxy reading or no figure it stores carries a `read_at`.
    pub(crate) fn take(self) -> Option<ReadDates> {
        READ_DATE.take().flatten()
    }

    fn record(dates: ReadDates) {
        if READ_DATE.get().is_some() {
            READ_DATE.set(Some(Some(dates)));
        }
    }
}

impl Drop for ReadDate {
    fn drop(&mut self) {
        READ_DATE.set(None);
    }
}

/// An epoch-seconds instant as a [`SystemTime`], saturating before the epoch.
fn at_epoch(secs: i64) -> SystemTime {
    std::time::UNIX_EPOCH + Duration::from_secs(u64::try_from(secs).unwrap_or_default())
}

/// One account block of `/usage` as clauth's typed stats, every
/// proxy-supplied string passed through `text` ([`Control::printable`]), with
/// the [`ReadDates`] of the figures the stats keep.
///
/// A window that claims a chain role (`chain: "5h" | "7d"`) and does not map
/// to a bar fails the whole read: the walk then keeps the member's last good
/// window off the cache-fallback path, where dropping the figure alone would
/// leave the member windowless — headroom — for a cadence. So does a block
/// without the contract's `available` verdict: the delegate's cannot-pay gate
/// keys on it, so a missing one is never read as funded.
fn account_stats(
    block: &Value,
    text: &dyn Fn(&str) -> String,
) -> std::result::Result<
    (crate::providers::ThirdPartyStats, Option<ReadDates>),
    crate::providers::ThirdPartyError,
> {
    use crate::providers::{StatRow, StatRowKind, ThirdPartyError, ThirdPartyStats, UsageBar};
    let available = block
        .get("available")
        .and_then(Value::as_bool)
        .ok_or(ThirdPartyError::Parse)?;
    let state = block.get("state").and_then(Value::as_str).unwrap_or("");
    let figures = block
        .get("figures")
        .and_then(Value::as_array)
        .ok_or(ThirdPartyError::Parse)?;
    let mut bars: Vec<UsageBar> = Vec::new();
    let mut rows: Vec<StatRow> = Vec::new();
    // A state other than ready is the proxy's own verdict about the upstream
    // login, an answer worth one dim line ahead of whatever figures remain.
    if !state.is_empty() && state != "ready" {
        rows.push(StatRow {
            label: String::new(),
            value: format!("account {}", text(state)),
            kind: StatRowKind::Faint,
            raw: None,
        });
    }
    let mut oldest: Option<i64> = None;
    let mut oldest_chain: Option<i64> = None;
    for figure in figures {
        let (stored, chain) = match figure.get("kind").and_then(Value::as_str) {
            Some("window") => match figure_bar(figure, text) {
                Some(bar) => {
                    bars.push(bar);
                    (true, chain_role(figure).is_some())
                }
                None if chain_role(figure).is_some() => return Err(ThirdPartyError::Parse),
                None => (extend_rows(&mut rows, figure_rows(figure, text)), false),
            },
            _ => (extend_rows(&mut rows, figure_rows(figure, text)), false),
        };
        let Some(read_at) = figure
            .get("read_at")
            .and_then(Value::as_str)
            .and_then(crate::usage::iso_to_epoch_secs)
        else {
            continue;
        };
        let older = |date: Option<i64>| Some(date.map_or(read_at, |d| d.min(read_at)));
        if stored {
            oldest = older(oldest);
        }
        if chain {
            oldest_chain = older(oldest_chain);
        }
    }
    let dates = oldest.map(|stalest| ReadDates {
        stalest: at_epoch(stalest),
        chain: oldest_chain.map(at_epoch),
    });
    Ok((
        ThirdPartyStats {
            is_available: available,
            rows,
            bars,
            plan: None,
            endpoint: None,
            best_effort: false,
        },
        dates,
    ))
}

/// Append a figure's rows; whether it stored any.
fn extend_rows(
    rows: &mut Vec<crate::providers::StatRow>,
    figure_rows: Option<Vec<crate::providers::StatRow>>,
) -> bool {
    let figure_rows = figure_rows.unwrap_or_default();
    let stored = !figure_rows.is_empty();
    rows.extend(figure_rows);
    stored
}

/// The chain role a `window` figure claims: `5h` or `7d`, the exact spellings
/// `to_usage_info` maps onto clauth's windows.
fn chain_role(figure: &Value) -> Option<&'static str> {
    match figure.get("chain").and_then(Value::as_str) {
        Some("5h") => Some("5h"),
        Some("7d") => Some("7d"),
        _ => None,
    }
}

/// A `window` figure's `used` and `limit`, when both map to a bar.
fn window_use(figure: &Value) -> Option<(f64, f64)> {
    let used = figure.get("used").and_then(Value::as_f64)?;
    let limit = figure
        .get("limit")
        .and_then(Value::as_f64)
        .filter(|limit| *limit > 0.0)?;
    Some((used, limit))
}

/// A bar label the chain reads a window by ([`crate::providers::ThirdPartyStats::windows_info`]).
fn is_chain_label(label: &str) -> bool {
    [
        crate::usage::LABEL_5H,
        crate::usage::LABEL_7D,
        crate::usage::LABEL_30D,
    ]
    .contains(&label)
}

/// A `window` figure as the bar the chain and the render read. A figure
/// claiming a chain role carries that role as its label; any other window
/// keeps its own label and renders display-only — as a row instead
/// ([`figure_rows`]) when that label is one the chain reads its windows by,
/// so only the `chain` field ever gives a figure a chain role.
fn figure_bar(figure: &Value, text: &dyn Fn(&str) -> String) -> Option<crate::providers::UsageBar> {
    let (used, limit) = window_use(figure)?;
    let label = match chain_role(figure) {
        Some(chain) => chain.to_string(),
        None => Some(text(figure.get("label").and_then(Value::as_str)?))
            .filter(|label| !is_chain_label(label))?,
    };
    Some(crate::providers::UsageBar {
        label,
        pct: (used / limit * 100.0).clamp(0.0, 100.0),
        resets_at: figure
            .get("resets_at")
            .and_then(Value::as_str)
            .map(str::to_string),
        used: Some(used),
        total: Some(limit),
        inference_quota: false,
    })
}

/// Every non-window figure as the text rows the Usage tab renders, in source
/// order. A kind clauth does not know renders through its `summary`, or not
/// at all. `None` = the figure is not renderable as rows (a required field is
/// missing or malformed), which drops it.
fn figure_rows(
    figure: &Value,
    text: &dyn Fn(&str) -> String,
) -> Option<Vec<crate::providers::StatRow>> {
    use crate::providers::{StatRow, StatRowKind};
    let label = text(
        figure
            .get("label")
            .and_then(Value::as_str)
            .unwrap_or_default(),
    );
    // `raw` stays absent: every value below spells its figure in full (the
    // proxy's own number, never an abbreviation), so the Usage detail has no
    // fuller spelling to render.
    let row = |label: &str, value: String, kind: StatRowKind| StatRow {
        label: label.to_string(),
        value,
        kind,
        raw: None,
    };
    match figure.get("kind").and_then(Value::as_str) {
        // A display-only window whose label the chain reads renders here
        // ([`figure_bar`]); any other window that reaches here does not map,
        // and drops.
        Some("window") => {
            window_use(figure)
                .filter(|_| is_chain_label(&label))
                .map(|(used, limit)| {
                    vec![row(
                        &label,
                        format!(
                            "{} used",
                            crate::format::format_pct((used / limit * 100.0).clamp(0.0, 100.0))
                        ),
                        StatRowKind::Body,
                    )]
                })
        }
        // A balance figure's row carries the ONE label the wallet machinery
        // recognizes (`is_balance_row`): it is what makes the proxy account's
        // balance a first-class wallet — the overview balance column, the
        // roster rank and the burn rate all key on it.
        Some("balance") => balance_value(figure, text).map(|value| {
            vec![row(
                crate::providers::DEEPSEEK_BALANCE_ROW_LABEL,
                value,
                StatRowKind::Body,
            )]
        }),
        Some("channel") => {
            let open = figure.get("open").and_then(Value::as_bool)?;
            let mut value = if open { "open" } else { "closed" }.to_string();
            if let Some(queue) = figure.get("queue_position").and_then(Value::as_i64) {
                value.push_str(&format!(" (queue {queue})"));
            }
            Some(vec![row(&label, value, StatRowKind::Body)])
        }
        Some("offer") => Some(
            figure
                .get("items")
                .and_then(Value::as_array)?
                .iter()
                .filter_map(|item| {
                    let label = item.get("label").and_then(Value::as_str)?;
                    let state = item.get("state").and_then(Value::as_str)?;
                    let (value, kind) = match item.get("failure").and_then(Value::as_str) {
                        Some(failure) => (text(failure), StatRowKind::Danger),
                        None => (text(state), StatRowKind::Body),
                    };
                    Some(StatRow {
                        label: text(label),
                        value,
                        kind,
                        raw: None,
                    })
                })
                .collect(),
        ),
        Some("stats") => {
            let requests = figure.get("requests").and_then(Value::as_object)?;
            let attempted = requests.get("attempted").and_then(Value::as_i64)?;
            let failed = requests.get("failed").and_then(Value::as_i64)?;
            let mut value = format!("{attempted} requests, {failed} failed");
            if let Some(ms) = figure.get("mean_latency_ms").filter(|ms| !ms.is_null()) {
                value.push_str(&format!(
                    ", {} ms avg",
                    ms.as_f64().unwrap_or_default() as i64
                ));
            }
            Some(vec![row(&label, value, StatRowKind::Body)])
        }
        _ => figure
            .get("summary")
            .and_then(Value::as_str)
            .map(|summary| vec![row(&label, text(summary), StatRowKind::Body)]),
    }
}

/// A balance figure as the row value the wallet machinery parses: a currency
/// unit spells `<amount> <CURRENCY>`, the one shape [`crate::providers::parse_balance`]
/// admits a wallet from.
fn balance_value(figure: &Value, text: &dyn Fn(&str) -> String) -> Option<String> {
    let remaining = figure.get("remaining").and_then(Value::as_f64)?;
    let value = match figure.get("unit").and_then(Value::as_str) {
        Some("currency") => {
            let currency = figure.get("currency").and_then(Value::as_str)?;
            if currency.is_empty() {
                return None;
            }
            format!("{remaining} {}", text(currency))
        }
        Some("percent") => format!("{remaining}%"),
        Some("custom") => {
            let unit = figure.get("unit_label").and_then(Value::as_str)?;
            if unit.is_empty() {
                format!("{remaining}")
            } else {
                format!("{remaining} {}", text(unit))
            }
        }
        Some("") | None => format!("{remaining}"),
        Some(unit) => format!("{remaining} {}", text(unit)),
    };
    Some(value)
}

// The pool read's figures: one bar per balance pool, the totals, one row per
// account.

/// One account block of a whole-pool `/usage` read.
struct PoolAccount<'a> {
    id: &'a str,
    state: &'a str,
    available: bool,
    figures: &'a [Value],
}

/// One `accounts` entry as a [`PoolAccount`]. An entry without the contract's
/// `available` verdict, its figures or its id fails the whole read: a missing
/// verdict read as funded, or a nameless row, is a figure nobody published.
fn pool_account(
    block: &Value,
) -> std::result::Result<PoolAccount<'_>, crate::providers::ThirdPartyError> {
    use crate::providers::ThirdPartyError;
    Ok(PoolAccount {
        id: block
            .get("account")
            .and_then(Value::as_str)
            .ok_or(ThirdPartyError::Parse)?,
        state: block
            .get("state")
            .and_then(Value::as_str)
            .unwrap_or_default(),
        available: block
            .get("available")
            .and_then(Value::as_bool)
            .ok_or(ThirdPartyError::Parse)?,
        figures: block
            .get("figures")
            .and_then(Value::as_array)
            .ok_or(ThirdPartyError::Parse)?,
    })
}

/// The pool-wide counts and sums the totals rows spell, and the stalest
/// figure's `read_at` that dates the copy.
#[derive(Default)]
struct PoolTotals {
    ready: usize,
    unread: usize,
    not_ready: usize,
    reset_5h: usize,
    reset_7d: usize,
    has_reset: bool,
    claimable: usize,
    held: usize,
    has_offer: bool,
    attempted: i64,
    failed: i64,
    has_stats: bool,
    /// A proxy-published negative count the sum may not include.
    negative_stats: bool,
    oldest_read_at: Option<i64>,
}

impl PoolTotals {
    fn add(&mut self, account: &PoolAccount<'_>) {
        if account.available {
            if account.figures.is_empty() {
                self.unread += 1;
            } else {
                self.ready += 1;
            }
        } else {
            self.not_ready += 1;
        }
        for figure in account.figures {
            if let Some(read_at) = figure
                .get("read_at")
                .and_then(Value::as_str)
                .and_then(crate::usage::iso_to_epoch_secs)
            {
                self.oldest_read_at = Some(
                    self.oldest_read_at
                        .map_or(read_at, |oldest| oldest.min(read_at)),
                );
            }
            match figure.get("kind").and_then(Value::as_str) {
                Some("channel") => self.add_channel(figure),
                Some("offer") => self.add_offer(figure),
                Some("stats") => self.add_stats(figure),
                _ => {}
            }
        }
    }

    fn add_channel(&mut self, figure: &Value) {
        let id = figure.get("id").and_then(Value::as_str);
        if !matches!(id, Some("reset-5h" | "reset-7d")) {
            return;
        }
        // A channel without its `open` verdict is no count clauth read: its row
        // stays out rather than printing a fabricated `0 open`.
        let Some(open) = figure.get("open").and_then(Value::as_bool) else {
            return;
        };
        self.has_reset = true;
        if open {
            if id == Some("reset-5h") {
                self.reset_5h += 1;
            } else {
                self.reset_7d += 1;
            }
        }
    }

    fn add_offer(&mut self, figure: &Value) {
        // An offer without its `items` array states no claim clauth read.
        let Some(items) = figure.get("items").and_then(Value::as_array) else {
            return;
        };
        self.has_offer = true;
        for item in items {
            match item.get("state").and_then(Value::as_str) {
                Some("available") => self.claimable += 1,
                Some("claimed") => self.held += 1,
                _ => {}
            }
        }
    }

    fn add_stats(&mut self, figure: &Value) {
        // Both counters must be integers clauth can add up, and one negative
        // count poisons the whole sum: the row never prints a total that a
        // junk count made nonsense.
        let Some(requests) = figure.get("requests").and_then(Value::as_object) else {
            return;
        };
        let (Some(attempted), Some(failed)) = (
            requests.get("attempted").and_then(Value::as_i64),
            requests.get("failed").and_then(Value::as_i64),
        ) else {
            return;
        };
        self.has_stats = true;
        if attempted < 0 || failed < 0 {
            self.negative_stats = true;
            return;
        }
        self.attempted = self.attempted.saturating_add(attempted);
        self.failed = self.failed.saturating_add(failed);
    }

    fn rows(&self) -> Vec<crate::providers::StatRow> {
        let mut rows = vec![stat_row("accounts", self.accounts_value())];
        if self.has_reset {
            rows.push(stat_row(
                "resets",
                format!("{} open (5h) | {} open (7d)", self.reset_5h, self.reset_7d),
            ));
        }
        if self.has_offer {
            rows.push(stat_row(
                "offers",
                format!("{} claimable | {} held", self.claimable, self.held),
            ));
        }
        if self.has_stats && !self.negative_stats {
            rows.push(stat_row(
                "requests",
                format!(
                    "{} ({} failed)",
                    crate::format::group_thousands(self.attempted as f64, 0),
                    crate::format::group_thousands(self.failed as f64, 0),
                ),
            ));
        }
        rows
    }

    fn accounts_value(&self) -> String {
        let mut value = format!("{} ready", self.ready);
        if self.not_ready > 0 {
            value.push_str(&format!(" | {} not ready", self.not_ready));
        }
        value.push_str(&format!(" | {} unread", self.unread));
        value
    }
}

fn stat_row(label: &str, value: String) -> crate::providers::StatRow {
    crate::providers::StatRow {
        label: label.to_string(),
        value,
        kind: crate::providers::StatRowKind::Body,
        raw: None,
    }
}

/// How a balance amount is spelled: a token count compacts (`205M`); every
/// other amount keeps its exact value through
/// [`crate::format::format_amount`] and names the unit its figure spells.
#[derive(Default, PartialEq)]
enum BalanceUnit {
    #[default]
    Tokens,
    Named(Option<String>),
}

/// One balance pool: every `balance` figure of one `id` that carries the
/// first-seen figure's unit and the grant and left it was read with.
#[derive(Default)]
struct BalancePool {
    label: String,
    /// The first-seen figure's unit owns the pool: a figure of another unit
    /// names another balance and never enters this bar's figures.
    unit: BalanceUnit,
    granted: f64,
    /// The summed `remaining` of the same figures that published a grant.
    left: f64,
    spent: f64,
    expiry: Option<(i64, String)>,
}

/// One bar per balance pool, in the order the pools first appear. A pool with
/// no finite granted total (or a non-positive one) gets no bar: its share is
/// not a figure the proxy published. Grant, left and spent come from one set —
/// the figures carrying a finite `limit` AND a finite `remaining`, of the
/// first-seen unit — so no side of the bar names a figure another side did not
/// read.
fn balance_bars(
    accounts: &[PoolAccount<'_>],
    text: &dyn Fn(&str) -> String,
) -> Vec<crate::providers::UsageBar> {
    let mut pools: Vec<(String, BalancePool)> = Vec::new();
    for figure in accounts
        .iter()
        .flat_map(|account| account.figures)
        .filter(|figure| figure.get("kind").and_then(Value::as_str) == Some("balance"))
    {
        let Some(id) = figure.get("id").and_then(Value::as_str) else {
            continue;
        };
        let index = match pools.iter().position(|(pool_id, _)| pool_id == id) {
            Some(index) => index,
            None => {
                pools.push((
                    id.to_string(),
                    BalancePool {
                        label: balance_label(figure, text),
                        unit: balance_unit(figure, text),
                        ..BalancePool::default()
                    },
                ));
                pools.len() - 1
            }
        };
        let pool = &mut pools[index].1;
        // Another unit's figure keeps its own account line and never enters
        // this bar's figures.
        if balance_unit(figure, text) != pool.unit {
            continue;
        }
        if let Some((epoch, stamp)) = figure
            .get("expires_at")
            .and_then(Value::as_str)
            .and_then(|stamp| crate::usage::iso_to_epoch_secs(stamp).map(|epoch| (epoch, stamp)))
            && pool
                .expiry
                .as_ref()
                .is_none_or(|(earliest, _)| epoch < *earliest)
        {
            pool.expiry = Some((epoch, stamp.to_string()));
        }
        // A figure naming only one side of the bar is no member of its set:
        // the grant and the left must both be figures this read produced.
        let (Some(limit), Some(remaining)) = (
            figure
                .get("limit")
                .and_then(Value::as_f64)
                .filter(|limit| limit.is_finite()),
            figure
                .get("remaining")
                .and_then(Value::as_f64)
                .filter(|remaining| remaining.is_finite()),
        ) else {
            continue;
        };
        pool.granted += limit;
        pool.left += remaining;
        pool.spent += figure
            .get("used")
            .and_then(Value::as_f64)
            .filter(|used| used.is_finite())
            .unwrap_or(limit - remaining);
    }
    pools
        .into_iter()
        .filter_map(|(_, pool)| {
            if !(pool.granted.is_finite() && pool.granted > 0.0) {
                return None;
            }
            let label = format!(
                "{} | {} left of {}",
                pool.label,
                balance_amount(pool.left, &pool.unit),
                balance_amount(pool.granted, &pool.unit),
            );
            Some(crate::providers::UsageBar {
                label,
                pct: (pool.spent / pool.granted * 100.0).clamp(0.0, 100.0),
                resets_at: pool.expiry.map(|(_, stamp)| stamp),
                used: None,
                total: None,
                inference_quota: false,
            })
        })
        .collect()
}

/// A pool bar's label: a bare chain label gains a word so
/// [`crate::providers::ThirdPartyStats::windows_info`] never reads a balance
/// as a `5h`/`7d` window and steers the auto-switch with it.
fn balance_label(figure: &Value, text: &dyn Fn(&str) -> String) -> String {
    let label = text(
        figure
            .get("label")
            .and_then(Value::as_str)
            .unwrap_or_default(),
    );
    if is_chain_label(&label) {
        format!("{label} balance")
    } else {
        label
    }
}

/// A balance figure's amount unit: its currency code, a non-empty
/// `unit_label`, else the unit's own name. [`BalanceUnit::Tokens`] for a token
/// count (which compacts); [`BalanceUnit::Named`]`(None)` for a figure naming
/// no unit at all.
fn balance_unit(figure: &Value, text: &dyn Fn(&str) -> String) -> BalanceUnit {
    let unit = figure
        .get("unit")
        .and_then(Value::as_str)
        .filter(|unit| !unit.is_empty());
    if unit == Some("tokens") {
        return BalanceUnit::Tokens;
    }
    let word = |key: &str| {
        figure
            .get(key)
            .and_then(Value::as_str)
            .filter(|word| !word.is_empty())
    };
    BalanceUnit::Named(
        word("currency")
            .or_else(|| word("unit_label"))
            .or(unit)
            .map(text),
    )
}

/// One side of a pool bar's `left of` phrase or a balance line's `left`: a
/// token count compacts, every other amount keeps its exact value and names
/// the unit word its figure spells.
fn balance_amount(amount: f64, unit: &BalanceUnit) -> String {
    match unit {
        BalanceUnit::Tokens => crate::format::format_compact(amount),
        BalanceUnit::Named(Some(word)) => {
            format!("{} {word}", crate::format::format_amount(amount))
        }
        BalanceUnit::Named(None) => crate::format::format_amount(amount),
    }
}

/// One account's line: its two chain windows always, then every other window,
/// balance and channel the proxy read for it in source order. A reading with
/// no figures spells the account's state instead.
fn pool_account_value(account: &PoolAccount<'_>, text: &dyn Fn(&str) -> String) -> String {
    if account.figures.is_empty() {
        return if account.state == "ready" {
            "no figures".to_string()
        } else {
            text(account.state).replace('_', " ")
        };
    }
    let mut parts = Vec::new();
    for role in ["5h", "7d"] {
        let pct = account
            .figures
            .iter()
            .find(|figure| {
                figure.get("kind").and_then(Value::as_str) == Some("window")
                    && chain_role(figure) == Some(role)
            })
            .and_then(window_pct)
            .unwrap_or_else(|| "-".to_string());
        parts.push(format!("{role} {pct}"));
    }
    for figure in account.figures.iter().filter(|figure| {
        figure.get("kind").and_then(Value::as_str) == Some("window") && chain_role(figure).is_none()
    }) {
        let label = text(
            figure
                .get("label")
                .and_then(Value::as_str)
                .unwrap_or_default(),
        );
        let pct = window_pct(figure).unwrap_or_else(|| "-".to_string());
        parts.push(format!("{label} {pct}"));
    }
    for figure in account
        .figures
        .iter()
        .filter(|figure| figure.get("kind").and_then(Value::as_str) == Some("balance"))
    {
        if let Some(part) = balance_part(figure, text) {
            parts.push(part);
        }
    }
    for figure in account
        .figures
        .iter()
        .filter(|figure| figure.get("kind").and_then(Value::as_str) == Some("channel"))
    {
        if let Some(part) = channel_part(figure, text) {
            parts.push(part);
        }
    }
    parts.join(" | ")
}

/// A window figure's used share, `None` when it draws no bar: both sides must
/// map ([`window_use`], which demands a positive limit) and the share is
/// clamped like the per-account pane's bars.
fn window_pct(figure: &Value) -> Option<String> {
    let (used, limit) = window_use(figure)?;
    Some(crate::format::format_pct(
        (used / limit * 100.0).clamp(0.0, 100.0),
    ))
}

/// A balance figure's `... left` phrase: the same amount spelling the pool
/// bar uses, so the line and the bar cannot disagree about one figure.
fn balance_part(figure: &Value, text: &dyn Fn(&str) -> String) -> Option<String> {
    let remaining = figure
        .get("remaining")
        .and_then(Value::as_f64)
        .filter(|remaining| remaining.is_finite())?;
    Some(format!(
        "{} left",
        balance_amount(remaining, &balance_unit(figure, text))
    ))
}

/// A channel figure's phrase: its summary when it carries one, else its open
/// verdict.
fn channel_part(figure: &Value, text: &dyn Fn(&str) -> String) -> Option<String> {
    let label = text(
        figure
            .get("label")
            .and_then(Value::as_str)
            .unwrap_or_default(),
    );
    if let Some(summary) = figure.get("summary").and_then(Value::as_str) {
        return Some(format!("{label} {}", text(summary)));
    }
    let open = figure.get("open").and_then(Value::as_bool)?;
    Some(format!("{label} {}", if open { "open" } else { "closed" }))
}

/// A whole-pool `/usage` body's `accounts` as the pool's stats: one bar per
/// balance pool, the pool's totals, then one line per account in source order,
/// with the [`ReadDates`] of the stalest figure it keeps. Every proxy-supplied
/// string passes through `text` ([`Control::printable`]).
pub(crate) fn pool_stats(
    accounts: &[Value],
    text: &dyn Fn(&str) -> String,
) -> std::result::Result<
    (crate::providers::ThirdPartyStats, Option<ReadDates>),
    crate::providers::ThirdPartyError,
> {
    use crate::providers::{StatRow, StatRowKind, ThirdPartyStats};
    let accounts = accounts
        .iter()
        .map(pool_account)
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let mut totals = PoolTotals::default();
    for account in &accounts {
        totals.add(account);
    }
    let mut rows = totals.rows();
    for account in &accounts {
        rows.push(StatRow {
            label: text(account.id),
            value: pool_account_value(account, text),
            kind: StatRowKind::Body,
            raw: None,
        });
    }
    let dates = totals.oldest_read_at.map(|stalest| ReadDates {
        stalest: at_epoch(stalest),
        chain: None,
    });
    Ok((
        ThirdPartyStats {
            is_available: accounts.iter().any(|account| account.available),
            rows,
            bars: balance_bars(&accounts, text),
            plan: None,
            endpoint: None,
            best_effort: false,
        },
        dates,
    ))
}

// ── per-profile settings ────────────────────────────────────────────────────

/// Which side of the proxy a setting configures.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Scope {
    Proxy,
    Account,
}

/// One option of an enum setting, as the proxy declared it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SettingOption {
    pub(crate) value: Value,
    pub(crate) label: String,
}

/// One declared setting, the proxy's own schema for it: the small form the
/// contract settles (`key`, `scope`, `label`, `hint`, `type`, `default`,
/// `options`, `min`/`max`/`step`, `active_when`, `restart`). The checker
/// validates a proxy's declarations against this type; the settings engine
/// validates a change against it — one type, so the two cannot disagree
/// about what a setting admits.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SettingDecl {
    pub(crate) key: String,
    pub(crate) scope: Scope,
    pub(crate) label: String,
    pub(crate) hint: String,
    /// `bool` | `enum` | `int` | `number` | `string`, the contract's set.
    pub(crate) typ: String,
    /// The declared `default`, `None` when the declaration omits the key. A
    /// present `null` default means the setting may hold no value, so `null`
    /// is a valid current value; an absent default marks nothing.
    pub(crate) default: Option<Value>,
    pub(crate) options: Vec<SettingOption>,
    pub(crate) min: Option<f64>,
    pub(crate) max: Option<f64>,
    pub(crate) step: Option<f64>,
    pub(crate) unit: Option<String>,
    /// `(key, values)` — the setting only applies while the named sibling
    /// setting holds one of these values.
    pub(crate) active_when: Option<(String, Vec<Value>)>,
    pub(crate) inactive_hint: Option<String>,
    pub(crate) restart: bool,
}

impl SettingDecl {
    /// Whether `v` is a value the proxy accepts for this setting: the same
    /// verdict the checker's default pin applies to a declared default.
    pub(crate) fn admits(&self, v: &Value) -> bool {
        let in_range = |n: f64| self.min.is_none_or(|m| n >= m) && self.max.is_none_or(|m| n <= m);
        match self.typ.as_str() {
            "bool" => v.is_boolean(),
            "enum" => self.options.iter().any(|o| &o.value == v),
            "int" => (v.is_i64() || v.is_u64()) && v.as_f64().is_some_and(in_range),
            "number" => v.as_f64().is_some_and(in_range),
            "string" => v.is_string(),
            _ => false,
        }
    }

    /// Whether a `null` current value is valid: the declaration carries an
    /// explicit `null` default. An absent default marks nothing, so `null` is
    /// not admitted for it.
    pub(crate) fn nullable(&self) -> bool {
        self.default.as_ref().is_some_and(Value::is_null)
    }

    /// Whether the setting configures the account (rather than the proxy).
    pub(crate) fn account_scope(&self) -> bool {
        self.scope == Scope::Account
    }
}

/// The setting types the contract declares.
pub(crate) const SETTING_TYPES: &[&str] = &["bool", "enum", "int", "number", "string"];

/// The account-scope settings the proxy declares for `service`, read off
/// `GET /clauth/v1/info`. A declaration the checker would flag is skipped, so
/// a departing proxy degrades one row instead of the engine.
///
/// `cfg(test)` while no production caller exists: the gate denies dead code.
#[cfg(test)]
pub(crate) fn declared_account_settings(service: &Service) -> Result<Vec<SettingDecl>> {
    declared_settings(&Control::connect(service)?)
}

#[cfg(test)]
fn declared_settings(control: &Control) -> Result<Vec<SettingDecl>> {
    let service = control.service();
    let mut response = control_agent(CONTROL_TIMEOUT)
        .get(&control.url("/info"))
        .header("authorization", &control.bearer("Nothing was changed")?)
        .call()
        .map_err(|_| refused(service, 0))?;
    let status = response.status().as_u16();
    if !(200..300).contains(&status) {
        return Err(refused(service, status));
    }
    let body = read_body(service, &mut response)?;
    let declared = body
        .get("settings")
        .and_then(Value::as_array)
        .map(|settings| {
            settings
                .iter()
                .filter_map(parse_setting)
                .filter(SettingDecl::account_scope)
                .collect()
        })
        .unwrap_or_default();
    Ok(declared)
}

#[cfg(test)]
fn parse_setting(s: &Value) -> Option<SettingDecl> {
    let key = s
        .get("key")
        .and_then(Value::as_str)
        .filter(|key| is_id(key))?
        .to_string();
    let scope = match s.get("scope").and_then(Value::as_str)? {
        "proxy" => Scope::Proxy,
        "account" => Scope::Account,
        _ => return None,
    };
    let label = s.get("label").and_then(Value::as_str)?.to_string();
    let hint = s.get("hint").and_then(Value::as_str)?.to_string();
    let typ = s.get("type").and_then(Value::as_str)?.to_string();
    if !SETTING_TYPES.contains(&typ.as_str()) {
        return None;
    }
    let default = Some(s.get("default")?.clone());
    let options = if typ == "enum" {
        s.get("options")?
            .as_array()?
            .iter()
            .map(|option| {
                Some(SettingOption {
                    value: option.get("value")?.clone(),
                    label: option.get("label").and_then(Value::as_str)?.to_string(),
                })
            })
            .collect::<Option<Vec<_>>>()?
    } else {
        Vec::new()
    };
    let numeric = typ == "int" || typ == "number";
    let bound = |name: &str| {
        if numeric {
            s.get(name).and_then(Value::as_f64)
        } else {
            None
        }
    };
    let active_when = match s.get("active_when") {
        None | Some(Value::Null) => None,
        Some(when) => Some((
            when.get("key")?.as_str()?.to_string(),
            when.get("in")?.as_array()?.clone(),
        )),
    };
    Some(SettingDecl {
        key,
        scope,
        label,
        hint,
        typ,
        default,
        options,
        min: bound("min"),
        max: bound("max"),
        step: bound("step"),
        unit: if numeric {
            s.get("unit").and_then(Value::as_str).map(str::to_string)
        } else {
            None
        },
        active_when,
        inactive_hint: s
            .get("inactive_hint")
            .and_then(Value::as_str)
            .map(str::to_string),
        restart: s.get("restart").and_then(Value::as_bool).unwrap_or(false),
    })
}

/// Write account settings through `PATCH /accounts/{id}`. Every change is
/// validated client-side against the proxy's own declaration first — a
/// wrong-typed or unknown key is refused here, after the declaration read and
/// before the PATCH, and the patch goes out as one body.
///
/// `cfg(test)` while no production caller exists: the gate denies dead code.
#[cfg(test)]
pub(crate) fn apply_account_settings(
    service: &Service,
    account: &str,
    changes: &[(String, Value)],
) -> Result<()> {
    anyhow::ensure!(
        !changes.is_empty(),
        "no settings to change on proxy account {account:?}"
    );
    let control = Control::connect(service)?;
    let declared = declared_settings(&control)?;
    let mut settings = serde_json::Map::new();
    for (key, value) in changes {
        let decl = declared
            .iter()
            .find(|decl| &decl.key == key)
            .ok_or_else(|| {
                crate::usage_error(format!(
                    "unknown setting {key:?} on proxy {:?}",
                    service.as_str()
                ))
            })?;
        anyhow::ensure!(
            (value.is_null() && decl.nullable()) || decl.admits(value),
            "setting {key:?} wants a {} value, got {value}",
            decl.typ
        );
        settings.insert(key.clone(), value.clone());
    }
    let seg = account_seg(account)?;
    let response = control_agent(CONTROL_TIMEOUT)
        .patch(&control.url(&format!("/accounts/{seg}")))
        .header("authorization", &control.bearer("Nothing was changed")?)
        .header("content-type", "application/json")
        .send(json!({ "settings": settings }).to_string().as_str())
        .map_err(|_| {
            crate::usage_error(format!(
                "proxy {:?} did not answer the settings change; nothing was changed",
                service.as_str()
            ))
        })?;
    let status = response.status().as_u16();
    if (200..300).contains(&status) {
        Ok(())
    } else {
        Err(refused(service, status))
    }
}
