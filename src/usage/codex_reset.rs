//! Spending a banked codex usage-limit reset (`clauth limit-reset`).
//!
//! The usage poll already READS the banked count off `wham/usage`
//! (`rate_limit_reset_credits.available_count`, carried on `UsageInfo` as
//! `limit_resets.usable` and shown as `↺ N` on the Overview's codex row while
//! one is available).
//! This module is the one place clauth SPENDS a codex one, and only because
//! the operator asked for it: `clauth limit-reset <name>`. A run sends each
//! call once; a lost reply leaves a pending record so a later run re-sends the
//! same ids instead of picking another credit.
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

use ratatui::style::Style;
use ratatui::text::Span;
use serde::Deserialize;
use serde::de::DeserializeOwned;

use super::fetch::{ANTHROPIC_ORIGIN, iso_to_epoch_secs};
use crate::format::{
    draws_nothing, grapheme_cells, is_blank, local_stamp, plural, shows_nothing, terminal_safe,
    truncate,
};

/// Lists the account's reset credits. The ChatGPT-flavored spelling, the only
/// one a clauth-held login reaches (see `codex::CODEX_USAGE_URL`).
pub(crate) const CODEX_RESET_CREDITS_URL: &str =
    "https://chatgpt.com/backend-api/wham/rate-limit-reset-credits";
/// Consumes one credit.
pub(crate) const CODEX_RESET_CONSUME_URL: &str =
    "https://chatgpt.com/backend-api/wham/rate-limit-reset-credits/consume";

/// End to end, body included: codex itself gives the consume 10s and the list
/// 5s (`app-server/src/request_processors/account_processor/rate_limit_resets.rs`),
/// so this 15s global deadline is the only bound after connect — a stalled
/// body can't hang the command.
const RESET_REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

/// The only reset type codex's picker knows by name; preferred when an account
/// holds credits of more than one type.
const CODEX_RATE_LIMITS: &str = "codex_rate_limits";
/// The server's re-send window: a pending record stamped within this of now is
/// re-sent. The copy that names the deadline derives it from the same constant.
pub(crate) const RESEND_WINDOW_MS: u64 = 600_000;
const AVAILABLE: &str = "available";

/// Where the five calls go: codex's list and consume, and the claude arm's
/// read, profile read and spend. Split out so a test points them all at a
/// loopback stub.
#[derive(Debug, Clone)]
pub(crate) struct ResetUrls {
    pub(crate) list: String,
    pub(crate) consume: String,
    /// The claude `api.anthropic.com` origin; the claude read, profile read and
    /// spend append their own paths (the spend's names the org uuid).
    pub(crate) claude_origin: String,
}

impl ResetUrls {
    pub(crate) fn live() -> Self {
        Self {
            list: CODEX_RESET_CREDITS_URL.to_string(),
            consume: CODEX_RESET_CONSUME_URL.to_string(),
            claude_origin: ANTHROPIC_ORIGIN.to_string(),
        }
    }

    /// The same paths under a stub's `http://127.0.0.1:<port>` origin.
    #[cfg(test)]
    pub(crate) fn under(origin: &str) -> Self {
        Self {
            list: format!("{origin}/backend-api/wham/rate-limit-reset-credits"),
            consume: format!("{origin}/backend-api/wham/rate-limit-reset-credits/consume"),
            claude_origin: origin.to_string(),
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
    /// The status as `--list` shows it: `terminal_safe`, its padding off.
    fn shown_status(&self) -> String {
        trim_padding(&terminal_safe(&self.status)).to_string()
    }

    /// Whether the shown status reads `available`: its invisible characters
    /// never keep a row that reads available from being the one spent.
    fn is_available(&self) -> bool {
        let shown: String = self
            .shown_status()
            .chars()
            .filter(|&c| !is_invisible(c))
            .collect();
        shown == AVAILABLE
    }

    /// The server's title, else `None` when it is absent or, after
    /// `terminal_safe`, holds nothing a terminal draws: blanks and invisible
    /// characters only. Blank and invisible padding comes off both ends, and
    /// the cap counts only what takes a cell, since this text reaches a
    /// terminal.
    fn shown_title(&self) -> Option<String> {
        let title = terminal_safe(self.title.as_deref().unwrap_or_default());
        if shows_nothing(&title) {
            None
        } else {
            Some(cap_drawn(trim_padding(&title), 60))
        }
    }

    /// The title, or the generic name codex's picker falls back to.
    fn label(&self) -> String {
        self.shown_title()
            .unwrap_or_else(|| "usage-limit reset".to_string())
    }
}

/// The list reply.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq, Default)]
pub(crate) struct ResetCredits {
    #[serde(default)]
    pub(crate) credits: Vec<ResetCredit>,
    /// The server's count — the same figure `wham/usage` carries, so the
    /// prompt's "1 of N" agrees with it.
    #[serde(default)]
    pub(crate) available_count: i64,
}

impl ResetCredits {
    /// The credit `limit-reset` spends. Among available credits a
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

/// Whether an error settles the spend's outcome, so its pending record is
/// cleared: a 401 proves nothing was spent, and any other 4xx spent nothing.
/// A transport failure, a body clauth cannot read, or a 5xx leaves the outcome
/// open, so the record stays.
pub(crate) fn settled(err: &ResetCallError) -> bool {
    matches!(
        err,
        ResetCallError::Unauthorized | ResetCallError::Status(400..=499)
    )
}

/// Whether a POST's reply lets the pending record go. A fresh send's 401 or any
/// other 4xx proves that request spent nothing, so it settles. A re-send's
/// failure only refuses the re-send, and the earlier request's outcome stays
/// unknown, so there only a reply naming a definite outcome settles.
pub(crate) fn post_settles<T>(
    re_send: bool,
    reply: &Result<T, ResetCallError>,
    reply_settles: fn(&T) -> bool,
) -> bool {
    match reply {
        Ok(reply) => reply_settles(reply),
        Err(_) if re_send => false,
        Err(err) => settled(err),
    }
}

/// Whether a consume reply names a definite outcome, so its pending record
/// clears: a defined code (`reset`, `already_redeemed`, `nothing_to_reset`,
/// `no_credit`) is settled, an unknown one leaves the outcome open.
pub(crate) fn consume_settled(reply: &ConsumeReply) -> bool {
    !matches!(reply.outcome(), ConsumeOutcome::Unknown(_))
}

/// Read a 2xx body as `T`, mapping the status and transport into the one
/// [`ResetCallError`] both reset arms share.
pub(crate) fn read_reply<T: DeserializeOwned>(
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
    fedramp: bool,
) -> Result<ResetCredits, ResetCallError> {
    read_reply(
        super::codex_headers::apply_codex_headers(
            super::codex_headers::codex_agent().get(url),
            access_token,
            account_id,
            fedramp,
        )
        .config()
        .timeout_recv_response(None)
        .timeout_global(Some(RESET_REQUEST_TIMEOUT))
        .build()
        .call(),
    )
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
/// than retried within the run, since the server may already have spent the
/// credit; the caller's pending record lets a later run re-send these ids.
pub(crate) fn consume_reset_credit_at(
    url: &str,
    access_token: &str,
    account_id: Option<&str>,
    fedramp: bool,
    redeem_request_id: &str,
    credit_id: &str,
) -> Result<ConsumeReply, ResetCallError> {
    read_reply(
        super::codex_headers::apply_codex_headers(
            super::codex_headers::codex_agent().post(url),
            access_token,
            account_id,
            fedramp,
        )
        .config()
        .timeout_recv_response(None)
        .timeout_global(Some(RESET_REQUEST_TIMEOUT))
        .build()
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

/// Whether an id fits the server's `^[A-Za-z0-9_-]{1,64}$` pattern, the shape
/// [`new_redeem_request_id`]'s v4 UUIDs satisfy. A pending record's id off
/// this shape was not written by clauth.
pub(crate) fn request_id_is_sendable(id: &str) -> bool {
    (1..=64).contains(&id.len())
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
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

/// Whether a terminal draws nothing for `c` and it takes no cell either: a
/// `Default_Ignorable_Code_Point` character that is neither a blank nor the
/// soft hyphen `U+00AD`, which glibc's `wcwidth` gives a cell.
fn is_invisible(c: char) -> bool {
    draws_nothing(c) && !is_blank(c) && c != '\u{AD}'
}

/// Whether `c` takes a cell in a terminal: every character but an invisible one
/// does, blanks included.
fn takes_a_cell(c: char) -> bool {
    !is_invisible(c)
}

/// `text` without the blank and `draws_nothing` padding on either end: the slice from
/// the first drawn character through the last, plus the invisible characters
/// directly after the last one (a variation selector travels with its emoji).
/// `""` when `text` holds no drawn character.
pub(crate) fn trim_padding(text: &str) -> &str {
    let drawn = |c: char| !is_blank(c) && !draws_nothing(c);
    let Some(start) = text.find(drawn) else {
        return "";
    };
    let last = text.rfind(drawn).unwrap_or(start);
    let after = last + text[last..].chars().next().map_or(0, char::len_utf8);
    let tail = text[after..]
        .find(|c: char| !is_invisible(c))
        .unwrap_or(text.len() - after);
    &text[start..after + tail]
}

/// Cap `text` at `max` terminal cells — the widths the drawn characters and
/// blanks take, never the invisible ones, so a wide CJK glyph or an emoji
/// presentation sequence spends its two — appending `...` when it is over; at
/// or under the cap the text comes back unchanged, padding and all. A cut
/// keeps whole graphemes, so an emoji's variation selector travels with its
/// base and never counts against the cap. A cut drops a grapheme the
/// remaining budget cannot hold whole. Cost is per grapheme, never below one
/// per character the model counts as taking a cell: a filler the grapheme
/// measure gives no cell still takes its blank one, and an emoji sequence
/// draws both its cells, so a cap cuts early rather than late.
pub(crate) fn cap_drawn(text: &str, max: usize) -> String {
    // The whole-string grapheme measure is not the guard: a combining mark
    // costs its own cell here.
    let cost = |g: &str| grapheme_cells(g).max(g.chars().filter(|&c| takes_a_cell(c)).count());
    let cells = |t: &str| {
        Span::raw(t)
            .styled_graphemes(Style::default())
            .map(|g| cost(g.symbol))
            .sum::<usize>()
    };
    if cells(text) <= max {
        return text.to_string();
    }
    if max < 3 {
        return truncate(text, max);
    }
    let mut kept = String::new();
    let mut spent = 0;
    for grapheme in Span::raw(text).styled_graphemes(Style::default()) {
        let c = cost(grapheme.symbol);
        if spent + c > max - 3 {
            break;
        }
        spent += c;
        kept.push_str(grapheme.symbol);
    }
    kept.push_str("...");
    kept
}

/// An RFC 3339 stamp as local wall-clock time, or the raw text (made
/// terminal-safe) when it does not parse — a stamp shown verbatim beats one
/// dropped.
pub(crate) fn stamp(raw: &str) -> String {
    iso_to_epoch_secs(raw)
        .and_then(local_stamp)
        .unwrap_or_else(|| cap_drawn(&terminal_safe(raw), 40))
}

fn expiry(credit: &ResetCredit) -> String {
    match credit.expires_at.as_deref() {
        Some(raw) if !shows_nothing(&terminal_safe(raw)) => {
            format!("expires {}", stamp(raw))
        }
        Some(_) | None => "no expiry".to_string(),
    }
}

/// Raise a clause's first letter so it can open a sentence.
fn capitalize(clause: &str) -> String {
    let mut chars = clause.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// The `[y/N]` question, without the `[y/N]` (the caller's prompt adds it).
/// An untitled credit opens on its expiry alone: the question already says a
/// usage-limit reset is what gets spent.
pub(crate) fn limit_reset_prompt(
    name: &str,
    credits: &ResetCredits,
    credit: &ResetCredit,
) -> String {
    let subject = match credit.shown_title() {
        Some(title) => format!("{title}, {}", expiry(credit)),
        None => capitalize(&expiry(credit)),
    };
    format!(
        "clauth: use a usage-limit reset on '{name}'? {subject}, 1 of {} available. \
         It reopens the account's usage windows now and cannot be undone.",
        credits.stated_count()
    )
}

/// The `[y/N]` question for a re-send, without the `[y/N]`: a fresh record
/// means the earlier consume's answer never arrived, so the prompt names that
/// request, not a credit pick.
pub(crate) fn re_send_prompt(name: &str) -> String {
    format!(
        "clauth: re-send the usage-limit reset on '{name}' whose answer never arrived? It \
         cannot be undone."
    )
}

/// The abort line after a declined re-send: the record stays, so the earlier
/// request's outcome is still unknown — the line never claims nothing was
/// spent. Shared by both arms.
pub(crate) fn declined_re_send_line(name: &str) -> String {
    format!(
        "clauth: aborted. the earlier reset request on '{name}' was not re-sent; its outcome \
         is still unknown."
    )
}

/// `--list`: a count line, then one line per credit, the one `limit-reset` would
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
        let status = credit.shown_status();
        let status_clause = if shows_nothing(&status) {
            String::new()
        } else {
            format!("{}, ", cap_drawn(&status, 20))
        };
        let granted = match credit.granted_at.as_deref() {
            Some(raw) if !shows_nothing(&terminal_safe(raw)) => {
                format!(", granted {}", stamp(raw))
            }
            Some(_) | None => String::new(),
        };
        lines.push(format!(
            "  {} {}: {status_clause}{}{granted}{}",
            if marked { "*" } else { " " },
            credit.label(),
            expiry(credit),
            if marked { "  (used next)" } else { "" },
        ));
    }
    lines
}

pub(crate) fn no_resets_available(name: &str) -> String {
    format!("no usage-limit resets available on '{name}'")
}

/// `--list`'s extra line when a fresh record exists: a consume sent at a local
/// time got no answer, and running `limit-reset` again before the window's end
/// re-sends it.
pub(crate) fn list_pending_note(name: &str, sent_at_ms: u64) -> String {
    let at = local_stamp((sent_at_ms / 1000) as i64).unwrap_or_else(|| sent_at_ms.to_string());
    format!(
        "clauth: a usage-limit reset sent at {at} got no answer; run `clauth limit-reset {name}` \
         again before {} to re-send it.",
        re_send_deadline(sent_at_ms)
    )
}

/// A 401 on either call. The store is never refreshed here, so the message says
/// who does refresh it.
fn token_rejected(name: &str) -> String {
    format!(
        "codex rejected the stored access token for '{name}'. The daemon refreshes it \
         for a parked account and codex does for the one in use. Try again after that"
    )
}

pub(super) fn check_list_hint(name: &str) -> String {
    format!("check `clauth limit-reset {name} --list` before retrying")
}

/// One reset arm's words for the shared failure lines: who answered, the 401
/// sentence, and how that arm names its read. Both arms call one body with
/// their own voice, so neither arm's copy can drift from the other's.
pub(crate) struct FailureVoice {
    pub(crate) provider: &'static str,
    pub(crate) rejected: fn(&str) -> String,
    /// The object of the read: `the reset list` or `the account read`.
    pub(crate) read_noun: &'static str,
    /// The read's verb phrase: `list the resets on` or `read the account for`.
    pub(crate) read_phrase: &'static str,
}

const CODEX_VOICE: FailureVoice = FailureVoice {
    provider: "codex",
    rejected: token_rejected,
    read_noun: "the reset list",
    read_phrase: "list the resets on",
};

/// A failed read. Nothing was spent, and every line says so.
pub(crate) fn read_failure(name: &str, err: &ResetCallError, voice: &FailureVoice) -> String {
    match err {
        ResetCallError::Unauthorized => {
            format!("{}. No reset was used", (voice.rejected)(name))
        }
        ResetCallError::Status(status) => format!(
            "{} answered {} for '{name}' with HTTP {status}; no reset was used",
            voice.provider, voice.read_noun
        ),
        ResetCallError::Transport => format!(
            "could not reach {} to {} '{name}'; no reset was used",
            voice.provider, voice.read_phrase
        ),
        ResetCallError::Parse => format!(
            "{} answered {} for '{name}' in a shape clauth does not read; no reset was used",
            voice.provider, voice.read_noun
        ),
    }
}

/// The clause a re-send's failed POST adds: the earlier request's reply never
/// arrived, so this refusal says nothing about whether it spent.
const EARLIER_OUTCOME_OPEN: &str = "the earlier request's outcome is still unknown";

/// A failed POST. A fresh send's 401 proves nothing was spent and any other 4xx
/// spent nothing, so those keep the look-before-retrying hint; every other
/// failure leaves the outcome open, and the pending record means a re-run
/// before the window's end re-sends the same ids, so those end on that re-run
/// instead. A re-send's failure only refuses the re-send, so every line says
/// the earlier request's outcome is still unknown.
pub(crate) fn spend_failure(
    name: &str,
    err: &ResetCallError,
    voice: &FailureVoice,
    sent_at_ms: u64,
    re_send: bool,
) -> String {
    match err {
        ResetCallError::Unauthorized if re_send => format!(
            "{}, and {EARLIER_OUTCOME_OPEN}; {}",
            (voice.rejected)(name),
            re_send_hint(sent_at_ms)
        ),
        ResetCallError::Unauthorized => {
            format!("{}. No reset was used", (voice.rejected)(name))
        }
        ResetCallError::Status(status) if re_send => format!(
            "{} answered the re-sent reset request for '{name}' with HTTP {status}; \
             {EARLIER_OUTCOME_OPEN}; {}",
            voice.provider,
            re_send_hint(sent_at_ms)
        ),
        ResetCallError::Status(status) if (400..=499).contains(status) => format!(
            "{} answered the reset request for '{name}' with HTTP {status}; {}",
            voice.provider,
            check_list_hint(name)
        ),
        ResetCallError::Status(status) => format!(
            "{} answered the reset request for '{name}' with HTTP {status}; {}",
            voice.provider,
            re_send_hint(sent_at_ms)
        ),
        ResetCallError::Transport if re_send => format!(
            "the re-sent reset request for '{name}' got no answer; {EARLIER_OUTCOME_OPEN}; {}",
            re_send_hint(sent_at_ms)
        ),
        ResetCallError::Transport => format!(
            "the reset request for '{name}' got no answer, so the reset may or may not have \
             gone through; {}",
            re_send_hint(sent_at_ms)
        ),
        ResetCallError::Parse if re_send => format!(
            "{} answered the re-sent reset request for '{name}' in a shape clauth does not \
             read; {EARLIER_OUTCOME_OPEN}; {}",
            voice.provider,
            re_send_hint(sent_at_ms)
        ),
        ResetCallError::Parse => format!(
            "{} answered the reset request for '{name}' in a shape clauth does not read, \
             so the reset may or may not have gone through; {}",
            voice.provider,
            re_send_hint(sent_at_ms)
        ),
    }
}

/// The end of the re-send window as a local wall-clock stamp: the record's
/// instant plus the 600 s the server holds the request. The raw instant is the
/// out-of-range fallback, matching the note's `at`.
pub(crate) fn re_send_deadline(sent_at_ms: u64) -> String {
    local_stamp(((sent_at_ms.saturating_add(RESEND_WINDOW_MS)) / 1000) as i64)
        .unwrap_or_else(|| sent_at_ms.to_string())
}

/// The clause an open-outcome failure ends on: the pending record re-sends the
/// same ids, so a re-run before the window's end is the safe move, not a look.
pub(crate) fn re_send_hint(sent_at_ms: u64) -> String {
    format!(
        "a re-run before {} re-sends the same request",
        re_send_deadline(sent_at_ms)
    )
}

/// A failed codex GET.
pub(crate) fn list_failure(name: &str, err: &ResetCallError) -> String {
    read_failure(name, err, &CODEX_VOICE)
}

/// A failed codex consume.
pub(crate) fn consume_failure(
    name: &str,
    err: &ResetCallError,
    sent_at_ms: u64,
    re_send: bool,
) -> String {
    spend_failure(name, err, &CODEX_VOICE, sent_at_ms, re_send)
}

/// A consume reply as the line to print (`Ok`) or the error to exit with
/// (`Err`). `credits` is the list the credit was chosen from; "left" is its
/// count less the one just used, since the reply does not carry one.
pub(crate) fn outcome_line(
    name: &str,
    credits: &ResetCredits,
    reply: &ConsumeReply,
    sent_at_ms: u64,
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
             run `clauth limit-reset {name} --list` to see what is left"
        )),
        ConsumeOutcome::Unknown(code) => {
            // Each character is escaped on its own before the cut, so the cut
            // can neither drop a control character nor split an escape; inside
            // double quotes an apostrophe needs no escape, as `{:?}` on a string
            // prints it.
            let shown = crate::format::truncate_escaped(
                &code,
                40,
                |c| match c {
                    '\'' => c.to_string(),
                    _ => c.escape_debug().to_string(),
                },
                true,
            );
            Err(format!(
                "codex answered the reset request for '{name}' with an unrecognized code \
                 {shown}, so the outcome is unconfirmed; {}",
                re_send_hint(sent_at_ms)
            ))
        }
    }
}

#[cfg(test)]
#[path = "../../tests/inline/codex_reset.rs"]
mod tests;
