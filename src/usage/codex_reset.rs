//! Spending a banked codex usage-limit reset (`clauth use-reset`).
//!
//! The usage poll already READS the banked count off `wham/usage`
//! (`rate_limit_reset_credits.available_count`, published as status.json
//! `codex_reset_credits`). This module is the one place clauth SPENDS one, and
//! only because the operator asked for it: `clauth use-reset <name>`. Nothing
//! here is on a timer, and nothing retries.
//!
//! The wire is codex's own, verified against openai/codex
//! (`backend-client/src/client/rate_limit_resets.rs`, `types.rs`, and the TUI's
//! `/usage` reset picker): a GET lists the account's credits, and a POST
//! consumes one by id under a fresh idempotency key. codex reads a reply's
//! `code` as a closed set; clauth keeps it an open string, so a code a newer
//! backend adds is reported as unconfirmed rather than failing to parse AFTER
//! the reset may already have been spent.
//!
//! Read-only on the credential: the access token and account id come from the
//! profile store as it stands. A 401 is reported, never answered with a
//! refresh — the store has a single writer (see `crate::codex_auth`).

use std::time::Duration;

use serde::Deserialize;
use serde::de::DeserializeOwned;

use super::fetch::{http_agent, iso_to_epoch_secs};
use crate::format::{local_stamp, plural, truncate};

/// Lists the account's reset credits. The ChatGPT-flavored spelling, the only
/// one a clauth-held login reaches (see `codex::CODEX_USAGE_URL`).
pub(crate) const CODEX_RESET_CREDITS_URL: &str =
    "https://chatgpt.com/backend-api/wham/rate-limit-reset-credits";
/// Consumes one credit.
pub(crate) const CODEX_RESET_CONSUME_URL: &str =
    "https://chatgpt.com/backend-api/wham/rate-limit-reset-credits/consume";

/// What codex's backend client sends when it has no richer User-Agent.
const CODEX_USER_AGENT: &str = "codex-cli";

/// End to end, body included. The shared agent's 8s wait for headers is lifted
/// for these calls, because codex itself gives the consume 10s; this 15s
/// deadline is the only bound after connect (the agent's 4s connect bound
/// stays), so a stalled body can't hang the command.
const RESET_REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

/// The only reset type codex's picker knows by name; preferred when an account
/// holds credits of more than one type.
const CODEX_RATE_LIMITS: &str = "codex_rate_limits";
const AVAILABLE: &str = "available";

/// Where the two calls go. Split out so a test points both at a loopback stub.
#[derive(Debug, Clone)]
pub(crate) struct ResetUrls {
    pub(crate) list: String,
    pub(crate) consume: String,
}

impl ResetUrls {
    pub(crate) fn live() -> Self {
        Self {
            list: CODEX_RESET_CREDITS_URL.to_string(),
            consume: CODEX_RESET_CONSUME_URL.to_string(),
        }
    }

    /// The same two paths under a stub's `http://127.0.0.1:<port>` origin.
    #[cfg(test)]
    pub(crate) fn under(origin: &str) -> Self {
        Self {
            list: format!("{origin}/backend-api/wham/rate-limit-reset-credits"),
            consume: format!("{origin}/backend-api/wham/rate-limit-reset-credits/consume"),
        }
    }
}

/// One credit as the list reports it. Every field but `id` defaults, so a
/// credit the backend describes more sparsely than codex expects still lists; a
/// status or type it leaves out reads as not usable, never as usable.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub(crate) struct ResetCredit {
    pub(crate) id: String,
    #[serde(default)]
    pub(crate) reset_type: String,
    #[serde(default)]
    pub(crate) status: String,
    #[serde(default)]
    pub(crate) granted_at: Option<String>,
    #[serde(default)]
    pub(crate) expires_at: Option<String>,
    #[serde(default)]
    pub(crate) title: Option<String>,
}

impl ResetCredit {
    fn is_available(&self) -> bool {
        self.status == AVAILABLE
    }

    /// The server's title, or the generic name codex's picker falls back to.
    /// Control characters are dropped: this text reaches a terminal.
    fn label(&self) -> String {
        let title = terminal_safe(self.title.as_deref().unwrap_or_default());
        let title = title.trim();
        if title.is_empty() {
            "usage-limit reset".to_string()
        } else {
            truncate(title, 60)
        }
    }
}

/// The list reply.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq, Default)]
pub(crate) struct ResetCredits {
    #[serde(default)]
    pub(crate) credits: Vec<ResetCredit>,
    /// The server's count — the same figure `wham/usage` carries and
    /// `status.json` publishes, so the prompt's "1 of N" agrees with it.
    #[serde(default)]
    pub(crate) available_count: i64,
}

impl ResetCredits {
    /// The credit `use-reset` spends. Among available credits a
    /// `codex_rate_limits` one wins, then the earliest `expires_at` (a credit with none, or one that does not parse,
    /// goes last — it is the one that can wait), then the earliest `granted_at`.
    /// A full tie keeps the server's order. `None` when nothing is available.
    pub(crate) fn next_to_use(&self) -> Option<&ResetCredit> {
        self.credits
            .iter()
            .filter(|c| c.is_available())
            .min_by_key(|c| {
                let expires = c.expires_at.as_deref().and_then(iso_to_epoch_secs);
                let granted = c.granted_at.as_deref().and_then(iso_to_epoch_secs);
                (
                    c.reset_type != CODEX_RATE_LIMITS,
                    expires.is_none(),
                    expires,
                    granted.is_none(),
                    granted,
                )
            })
    }

    /// The count to state: never below one while a credit is about to be used,
    /// so a server count that lags its own list cannot print "1 of 0".
    fn stated_count(&self) -> i64 {
        self.available_count.max(1)
    }
}

/// The consume reply. `code` stays a string: see the module doc.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub(crate) struct ConsumeReply {
    pub(crate) code: String,
    #[serde(default)]
    pub(crate) windows_reset: i64,
}

/// What a consume reply means for the operator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ConsumeOutcome {
    /// `reset`, or `already_redeemed` — codex reads both as done: the windows
    /// are open and the credit is spent.
    Reset { windows_reset: i64 },
    /// The account had nothing to reset, so the credit was NOT spent.
    NothingToReset,
    /// The chosen credit was gone by the time the request landed.
    NoCredit,
    /// A code codex does not define. Whether anything was spent is unknown.
    Unknown(String),
}

impl ConsumeReply {
    pub(crate) fn outcome(&self) -> ConsumeOutcome {
        match self.code.as_str() {
            "reset" | "already_redeemed" => ConsumeOutcome::Reset {
                windows_reset: self.windows_reset,
            },
            "nothing_to_reset" => ConsumeOutcome::NothingToReset,
            "no_credit" => ConsumeOutcome::NoCredit,
            other => ConsumeOutcome::Unknown(other.to_string()),
        }
    }
}

/// Why a call failed. No variant carries the body: it may echo account data,
/// and none of these messages need it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ResetCallError {
    /// The stored access token was refused.
    Unauthorized,
    /// Any other non-2xx.
    Status(u16),
    /// No answer: connect, TLS, the timeout, or a body cut off mid-read.
    Transport,
    /// A 2xx whose body is not the documented shape.
    Parse,
}

/// codex's headers on both calls (its `BackendClient::headers`), plus the
/// per-request deadline.
fn codex_request<B>(
    req: ureq::RequestBuilder<B>,
    access_token: &str,
    account_id: Option<&str>,
) -> ureq::RequestBuilder<B> {
    let mut req = req
        .config()
        .timeout_recv_response(None)
        .timeout_global(Some(RESET_REQUEST_TIMEOUT))
        .build()
        .header("Authorization", &format!("Bearer {access_token}"))
        .header("User-Agent", CODEX_USER_AGENT)
        .header("Accept", "application/json");
    // A multi-workspace login answers for whichever account this names; without
    // it the server picks, and the reset could land on the wrong workspace.
    if let Some(id) = account_id.map(str::trim).filter(|id| !id.is_empty()) {
        req = req.header("ChatGPT-Account-Id", id);
    }
    req
}

fn read_reply<T: DeserializeOwned>(
    result: Result<ureq::http::Response<ureq::Body>, ureq::Error>,
) -> Result<T, ResetCallError> {
    let mut response = result.map_err(|_| ResetCallError::Transport)?;
    match response.status().as_u16() {
        200..=299 => {}
        401 => return Err(ResetCallError::Unauthorized),
        status => return Err(ResetCallError::Status(status)),
    }
    let body = response
        .body_mut()
        .read_to_string()
        .map_err(|_| ResetCallError::Transport)?;
    serde_json::from_str(&body).map_err(|_| ResetCallError::Parse)
}

/// GET the account's credits. Spends nothing.
pub(crate) fn list_reset_credits_at(
    url: &str,
    access_token: &str,
    account_id: Option<&str>,
) -> Result<ResetCredits, ResetCallError> {
    read_reply(codex_request(http_agent().get(url), access_token, account_id).call())
}

/// The consume body. `credit_id` is optional on codex's wire (the server then
/// picks); clauth always names the credit its prompt described.
fn consume_body(redeem_request_id: &str, credit_id: &str) -> String {
    serde_json::json!({
        "redeem_request_id": redeem_request_id,
        "credit_id": credit_id,
    })
    .to_string()
}

/// POST one consume. Sent once: a lost reply is reported as unconfirmed rather
/// than retried, since the server may already have spent the credit.
pub(crate) fn consume_reset_credit_at(
    url: &str,
    access_token: &str,
    account_id: Option<&str>,
    redeem_request_id: &str,
    credit_id: &str,
) -> Result<ConsumeReply, ResetCallError> {
    read_reply(
        codex_request(http_agent().post(url), access_token, account_id)
            .header("Content-Type", "application/json")
            .send(consume_body(redeem_request_id, credit_id)),
    )
}

/// A fresh idempotency key per operator action, in the UUID v4 shape codex
/// sends (`Uuid::new_v4`).
pub(crate) fn new_redeem_request_id() -> anyhow::Result<String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|e| anyhow::anyhow!("CSPRNG failure: {e}"))?;
    Ok(uuid_v4_from(bytes))
}

/// Stamp the version (4) and variant (RFC 4122) bits onto 16 random bytes.
fn uuid_v4_from(mut bytes: [u8; 16]) -> String {
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let h = hex::encode(bytes);
    format!(
        "{}-{}-{}-{}-{}",
        &h[0..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..32]
    )
}

// ── the operator-facing text ────────────────────────────────────────────────
//
// One spelling per outcome. A script can read the first stdout line as the
// success summary and a failure off stderr.

/// Server text with its control characters dropped: it reaches a terminal,
/// where an escape sequence could retitle it or hide the confirm line.
fn terminal_safe(s: &str) -> String {
    s.chars().filter(|c| !c.is_control()).collect()
}

/// An RFC 3339 stamp as local wall-clock time, or the raw text (made
/// terminal-safe) when it does not parse — a stamp shown verbatim beats one
/// dropped.
fn stamp(raw: &str) -> String {
    iso_to_epoch_secs(raw)
        .and_then(local_stamp)
        .unwrap_or_else(|| truncate(&terminal_safe(raw), 40))
}

fn expiry(credit: &ResetCredit) -> String {
    match credit.expires_at.as_deref() {
        Some(raw) => format!("expires {}", stamp(raw)),
        None => "no expiry".to_string(),
    }
}

/// The `[y/N]` question, without the `[y/N]` (the caller's prompt adds it).
pub(crate) fn use_reset_prompt(name: &str, credits: &ResetCredits, credit: &ResetCredit) -> String {
    format!(
        "clauth: use a usage-limit reset on '{name}'? {} · {} · 1 of {} available. \
         It reopens the account's usage windows now and cannot be undone.",
        credit.label(),
        expiry(credit),
        credits.stated_count()
    )
}

/// `--list`: a count line, then one line per credit, the one `use-reset` would
/// spend marked `*`.
pub(crate) fn describe_reset_credits(name: &str, credits: &ResetCredits) -> Vec<String> {
    let next = credits.next_to_use().map(|c| c.id.as_str());
    let mut lines = vec![if next.is_some() {
        let n = credits.stated_count();
        format!(
            "clauth: '{name}' has {n} usage-limit reset{} available.",
            plural(n as usize)
        )
    } else {
        format!("clauth: {}.", no_resets_available(name))
    }];
    for credit in &credits.credits {
        let marked = next == Some(credit.id.as_str());
        let status = terminal_safe(&credit.status);
        let granted = credit
            .granted_at
            .as_deref()
            .map(|g| format!(", granted {}", stamp(g)))
            .unwrap_or_default();
        lines.push(format!(
            "  {} {} — {}, {}{granted}{}",
            if marked { "*" } else { " " },
            credit.label(),
            truncate(&status, 20),
            expiry(credit),
            if marked { "  (used next)" } else { "" },
        ));
    }
    lines
}

pub(crate) fn no_resets_available(name: &str) -> String {
    format!("no usage-limit resets available on '{name}'")
}

/// A 401 on either call. The store is never refreshed here, so the message says
/// who does refresh it.
fn token_rejected(name: &str) -> String {
    format!(
        "codex rejected the stored access token for '{name}' — the daemon refreshes it \
         for a parked account and codex does for the one in use; try again after that"
    )
}

fn check_list_hint(name: &str) -> String {
    format!("check `clauth use-reset {name} --list` before retrying")
}

/// A failed GET. Nothing was spent, and every line says so.
pub(crate) fn list_failure(name: &str, err: &ResetCallError) -> String {
    match err {
        ResetCallError::Unauthorized => format!("{}; no reset was used", token_rejected(name)),
        ResetCallError::Status(status) => format!(
            "codex answered the reset list for '{name}' with HTTP {status}; no reset was used"
        ),
        ResetCallError::Transport => {
            format!("could not reach codex to list the resets on '{name}'; no reset was used")
        }
        ResetCallError::Parse => format!(
            "codex answered the reset list for '{name}' in a shape clauth does not read; \
             no reset was used"
        ),
    }
}

/// A failed POST. Only a 401 proves nothing was spent; every other failure
/// leaves the outcome open, and the line says to look before retrying.
pub(crate) fn consume_failure(name: &str, err: &ResetCallError) -> String {
    match err {
        ResetCallError::Unauthorized => format!("{}; no reset was used", token_rejected(name)),
        ResetCallError::Status(status) => format!(
            "codex answered the reset request for '{name}' with HTTP {status}; {}",
            check_list_hint(name)
        ),
        ResetCallError::Transport => format!(
            "the reset request for '{name}' got no answer, so the reset may or may not have \
             gone through; {}",
            check_list_hint(name)
        ),
        ResetCallError::Parse => format!(
            "codex answered the reset request for '{name}' in a shape clauth does not read, \
             so the reset may or may not have gone through; {}",
            check_list_hint(name)
        ),
    }
}

/// A consume reply as the line to print (`Ok`) or the error to exit with
/// (`Err`). `credits` is the list the credit was chosen from; "left" is its
/// count less the one just used, since the reply does not carry one.
pub(crate) fn outcome_line(
    name: &str,
    credits: &ResetCredits,
    reply: &ConsumeReply,
) -> Result<String, String> {
    match reply.outcome() {
        ConsumeOutcome::Reset { windows_reset } => {
            let left = (credits.stated_count() - 1).max(0);
            Ok(format!(
                "clauth: used a usage-limit reset on '{name}': {windows_reset} window{} \
                 reopened, {left} left.",
                plural(windows_reset.max(0) as usize)
            ))
        }
        ConsumeOutcome::NothingToReset => Err(format!(
            "there is nothing to reset on '{name}' right now, so no reset was used"
        )),
        ConsumeOutcome::NoCredit => Err(format!(
            "that reset on '{name}' is no longer available (used or expired meanwhile); \
             run `clauth use-reset {name} --list` to see what is left"
        )),
        ConsumeOutcome::Unknown(code) => Err(format!(
            "codex answered the reset request for '{name}' with an unrecognized code {:?}, \
             so the outcome is unconfirmed; {}",
            truncate(&code, 40),
            check_list_hint(name)
        )),
    }
}

#[cfg(test)]
#[path = "../../tests/inline/codex_reset.rs"]
mod tests;
