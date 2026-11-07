//! Spending a banked claude usage-limit reset (`clauth limit-reset`).
//!
//! Claude Code's own `/limit-reset` reads a `cedar_ember` block off the usage
//! endpoint and spends one grant through `reset_rate_limits`; this module
//! drives the same wire for an operator who asked clauth to. A run sends the
//! spend once, exactly like the codex arm, because a lost answer may still
//! have spent the grant; a lost reply leaves a pending record so a later run
//! re-sends the same ids instead of picking another grant.
//!
//! Read-only on the credential: the access token comes from the profile store
//! as it stands. A 401 is reported, never answered with a refresh — the store
//! has a single writer (the daemon's OAuth rotation, in `crate::oauth`).

use std::collections::HashMap;
use std::time::Duration;

use serde::Deserialize;

use super::codex_reset::{
    FailureVoice, cap_drawn, quoted_escaped, re_send_deadline, re_send_hint,
    read_failure as shared_read_failure, read_reply, spend_failure as shared_spend_failure, stamp,
    trim_padding,
};
pub(crate) use super::codex_reset::{ResetCallError, declined_re_send_line, post_settles};
use super::fetch::{AuthClient, LimitResets};
use crate::format::{local_stamp, plural, shows_nothing, terminal_safe};

/// The reset inventory read: the poll's `/usage` path plus the two query flags
/// Claude Code's own read adds.
const READ_PATH: &str = "/api/oauth/usage?cedar_ember=1&skip_spend=1";
/// The `/profile` read that names the org uuid the spend POSTs to.
const PROFILE_PATH: &str = "/api/oauth/profile";

/// The reads give themselves the codex list's 15 s global deadline; the spend
/// gets 30 s, past Claude Code's own 25 s.
const READ_TIMEOUT: Duration = Duration::from_secs(15);
const SPEND_TIMEOUT: Duration = Duration::from_secs(30);

/// The `cedar_ember` block, as the usage body carries it. `Default` is the
/// block's absence: an unrolled-out or headless read answers `null`, which
/// reads as no grant rather than a shape clauth cannot read.
#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct ResetStatus {
    #[serde(default)]
    pub(crate) at_limit: bool,
    #[serde(default)]
    pub(crate) ineligible_reason: Option<String>,
    #[serde(default)]
    pub(crate) grants: Vec<Grant>,
    #[serde(default)]
    pub(crate) next_grant_id: Option<String>,
    #[serde(default)]
    pub(crate) weekly_resets_at: Option<String>,
}

impl ResetStatus {
    /// The grant the server says is next to spend, by id — never the first
    /// usable grant. `None` when the server names none, or none matches.
    pub(crate) fn next_grant(&self) -> Option<&Grant> {
        let id = self.next_grant_id.as_deref()?;
        self.grants.iter().find(|g| g.id == id)
    }

    /// The grant a spend would send now: the server's next grant, when the
    /// server marks it usable now and its id fits the pattern Claude Code
    /// sends. A fresh spend refuses when this is `None`.
    pub(crate) fn spend_grant(&self) -> Option<&Grant> {
        let grant = self.next_grant()?;
        (grant.is_usable_now() && grant_id_is_sendable(&grant.id)).then_some(grant)
    }

    /// The account's banked resets a fresh spend can use now: the sum of
    /// `resets_left` over grants the server marks usable, and 0 while the
    /// server's next reset cannot be spent. The scheduler's side read and the
    /// CLI's post-spend refresh land this count in `limit_resets.json`.
    /// Saturated, as the codex arm clamps its server count: a body whose
    /// grants sum past `u32::MAX` reads as maxed, never wraps or panics.
    pub(crate) fn usable_resets(&self) -> u32 {
        if self.spend_grant().is_none() {
            return 0;
        }
        self.grants
            .iter()
            .filter(|g| g.usable_now)
            .fold(0u32, |acc, g| acc.saturating_add(g.resets_left))
    }

    /// Every banked reset the last read found, usable or not: the sum of
    /// `resets_left` over every grant, saturated. The Usage row counts the
    /// spendable ones against this total, so a cooldown hides no banked reset.
    pub(crate) fn banked_resets(&self) -> u32 {
        self.grants
            .iter()
            .fold(0u32, |acc, g| acc.saturating_add(g.resets_left))
    }
}

/// One grant, as the status block lists it. Every field but `id` defaults, so
/// a grant the server describes more sparsely than Claude Code expects still
/// lists.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct Grant {
    pub(crate) id: String,
    #[serde(default)]
    label: String,
    #[serde(default)]
    resets_total: u32,
    #[serde(default)]
    resets_left: u32,
    #[serde(default)]
    ends_at: Option<String>,
    #[serde(default)]
    clears: Vec<String>,
    #[serde(default)]
    paused: bool,
    #[serde(default)]
    usable_now: bool,
    #[serde(default)]
    percent_used: HashMap<String, u32>,
    #[serde(default)]
    unavailable_reason: Option<String>,
}

impl Grant {
    /// Whether the server marks this grant spendable now.
    pub(crate) fn is_usable_now(&self) -> bool {
        self.usable_now
    }

    /// The window names this grant clears, raw server spelling — the side
    /// read's file and the TUI's labels both take them off here.
    pub(crate) fn clears(&self) -> &[String] {
        &self.clears
    }

    /// The grant's ISO end stamp, raw server spelling.
    pub(crate) fn ends_at(&self) -> Option<&str> {
        self.ends_at.as_deref()
    }

    /// The server's label, or the generic name; control characters dropped,
    /// cut to the same 40 the window names use (this text reaches a terminal).
    fn label(&self) -> String {
        let label = terminal_safe(&self.label);
        if shows_nothing(&label) {
            "limit reset".to_string()
        } else {
            cap_drawn(trim_padding(&label), 40)
        }
    }

    /// Whether the display label fell back to the generic name, so the prompt
    /// drops the quoted name.
    fn is_untitled(&self) -> bool {
        shows_nothing(&terminal_safe(&self.label))
    }
}

/// The top-level body wrapper: the read answers the normal `/usage` body plus
/// a `cedar_ember` block.
#[derive(Deserialize)]
struct CedarEmberBody {
    #[serde(default)]
    cedar_ember: Option<ResetStatus>,
}

impl CedarEmberBody {
    /// The block, or the empty status when the server sent none.
    fn into_status(self) -> ResetStatus {
        self.cedar_ember.unwrap_or_default()
    }
}

/// Parse the `cedar_ember` block off a raw usage body. Test-only: the wire
/// read ([`read_status_at`]) parses the typed body through [`read_reply`], and
/// this seam lets tests drive fixture bytes straight into the parser.
#[cfg(test)]
pub(crate) fn read_status(body: &str) -> Result<ResetStatus, ResetCallError> {
    serde_json::from_str::<CedarEmberBody>(body)
        .map(CedarEmberBody::into_status)
        .map_err(|_| ResetCallError::Parse)
}

/// GET the account's reset grants. Spends nothing.
pub(crate) fn read_status_at(url: &str, access_token: &str) -> Result<ResetStatus, ResetCallError> {
    read_reply::<CedarEmberBody>(
        super::fetch::apply_auth_headers(
            super::fetch::http_agent().get(url),
            access_token,
            AuthClient::Usage,
        )
        .config()
        .timeout_recv_response(None)
        .timeout_global(Some(READ_TIMEOUT))
        .build()
        .call(),
    )
    .map(CedarEmberBody::into_status)
}

/// The reading a banked-reset status lands on disk: the usable count, the
/// banked total, the next grant's raw window names and end, and the read's own
/// epoch-ms stamp. The scheduler's hourly side read and the CLI's post-spend
/// refresh share it, so a spend's re-read lands exactly what the hourly read
/// would have.
pub(crate) fn status_reading(status: &ResetStatus) -> LimitResets {
    let next = status.next_grant();
    LimitResets {
        usable: status.usable_resets(),
        banked: Some(status.banked_resets()),
        clears: next.map(|g| g.clears().to_vec()).unwrap_or_default(),
        ends_at: next.and_then(|g| g.ends_at().map(str::to_string)),
        fetched_at: Some(super::now_ms()),
    }
}

/// The profile reply's org id, the only field the spend needs.
#[derive(Deserialize)]
struct ProfileReply {
    organization: ProfileOrganization,
}

#[derive(Deserialize)]
struct ProfileOrganization {
    uuid: String,
}

/// Whether a profile's org id is one this arm may splice into its spend path:
/// the same shape the account uuid uses, so `#`, `?`, `/`, a dot segment or a
/// multi-kilobyte value can never retarget the bearer POST.
fn is_sendable_org_uuid(uuid: &str) -> bool {
    (1..=64).contains(&uuid.len()) && uuid.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

/// GET the profile and return the org uuid the spend POSTs to, refused as a
/// shape clauth does not read when it is not one.
pub(crate) fn read_org_uuid_at(url: &str, access_token: &str) -> Result<String, ResetCallError> {
    let reply = read_reply::<ProfileReply>(
        super::fetch::apply_auth_headers(
            super::fetch::http_agent().get(url),
            access_token,
            AuthClient::Profile,
        )
        .config()
        .timeout_recv_response(None)
        .timeout_global(Some(READ_TIMEOUT))
        .build()
        .call(),
    )?;
    let uuid = reply.organization.uuid;
    if is_sendable_org_uuid(&uuid) {
        Ok(uuid)
    } else {
        Err(ResetCallError::Parse)
    }
}

/// The spend reply. `result` stays an open string: a result a newer server
/// adds is reported as unconfirmed rather than failing to parse AFTER the
/// reset may already have been spent.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct SpendReply {
    pub(crate) result: String,
    #[serde(default)]
    pub(crate) reason: Option<String>,
    #[serde(default)]
    pub(crate) resets_left: Option<u32>,
    #[serde(default)]
    pub(crate) cleared: Vec<String>,
    #[serde(default)]
    pub(crate) cooldown_until: Option<String>,
}

/// POST one spend. Sent once: a lost reply is reported as unconfirmed rather
/// than retried within the run, since the server may already have spent the
/// grant; the caller's pending record lets a later run re-send these ids.
pub(crate) fn spend_reset_at(
    url: &str,
    access_token: &str,
    grant_id: &str,
    request_id: &str,
) -> Result<SpendReply, ResetCallError> {
    let body = serde_json::json!({
        "program": "cedar_ember",
        "grant_id": grant_id,
        "request_id": request_id,
    })
    .to_string();
    read_reply(
        // Claude Code's transport merges the OAuth headers into the spend as
        // into the read, so the POST starts from the same builder the GET does.
        super::fetch::apply_auth_headers(
            super::fetch::http_agent().post(url),
            access_token,
            AuthClient::Usage,
        )
        .config()
        .timeout_recv_response(None)
        .timeout_global(Some(SPEND_TIMEOUT))
        .build()
        .send(body),
    )
}

/// The claude URLs, off the origin `ResetUrls` carries.
pub(crate) fn read_url(origin: &str) -> String {
    format!("{origin}{READ_PATH}")
}

/// The side read's URL off the poll's own resolved `/usage` endpoint: that
/// endpoint already ends in the path [`READ_PATH`] spells, so only its query
/// flags join — the flags are spelled once, in [`READ_PATH`].
pub(crate) fn read_url_from_endpoint(endpoint: &str) -> String {
    let Some((_, query)) = READ_PATH.split_once('?') else {
        return format!("{endpoint}{READ_PATH}");
    };
    format!("{endpoint}?{query}")
}

pub(crate) fn org_url(origin: &str) -> String {
    format!("{origin}{PROFILE_PATH}")
}

pub(crate) fn spend_url(origin: &str, org_uuid: &str) -> String {
    format!("{origin}/api/organizations/{org_uuid}/reset_rate_limits")
}

/// A fresh idempotency key per operator action, in codex's v4-UUID shape —
/// the same generator both arms share, which the server's `^[A-Za-z0-9_-]{1,64}$`
/// pattern accepts.
pub(crate) fn new_request_id() -> anyhow::Result<String> {
    super::codex_reset::new_redeem_request_id()
}

// ── the operator-facing text ────────────────────────────────────────────────

/// A cleared-window name as the terminal reads it: the short labels Claude
/// Code's own copy uses, any other name as sent with its control characters
/// dropped and cut to 40. Shared by `--list` and the TUI's resets row, so the
/// two surfaces never spell a label apart.
pub(crate) fn window_label(name: &str) -> String {
    match name {
        "five_hour" => "5h".to_string(),
        "seven_day" => "7d".to_string(),
        "seven_day_opus" => "7d opus".to_string(),
        "seven_day_sonnet" => "7d sonnet".to_string(),
        other => cap_drawn(&terminal_safe(other), 40),
    }
}

/// `5h`, `5h and 7d`, `5h, 7d and 7d opus` — a comma list with the last pair
/// joined by `and`.
fn join_windows(clears: &[String]) -> String {
    let labels: Vec<String> = clears.iter().map(|w| window_label(w)).collect();
    match labels.len() {
        0 => String::new(),
        1 => labels[0].clone(),
        2 => format!("{} and {}", labels[0], labels[1]),
        _ => {
            let n = labels.len();
            let last = &labels[n - 1];
            format!("{} and {last}", labels[..n - 1].join(", "))
        }
    }
}

/// The window phrase a grant's `--list` row and the prompt share: `nothing`
/// when the server names no window.
fn clears_summary(clears: &[String]) -> String {
    if clears.is_empty() {
        "nothing".to_string()
    } else {
        join_windows(clears)
    }
}

fn grant_state(grant: &Grant) -> String {
    if grant.usable_now {
        "usable now".to_string()
    } else if grant.paused {
        "paused".to_string()
    } else {
        match grant.unavailable_reason.as_deref() {
            Some(reason) if !shows_nothing(&terminal_safe(reason)) => {
                format!("unavailable ({})", cap_drawn(&terminal_safe(reason), 40))
            }
            Some(_) | None => "unavailable".to_string(),
        }
    }
}

/// The tail a `--list` row carries after its windows: nothing on a grant the
/// server marks usable now, `paused`, else the server's own reason — cleaned
/// and capped, since this text reaches a terminal. Nothing when it gives none.
fn row_state(grant: &Grant) -> Option<String> {
    if grant.usable_now {
        return None;
    }
    if grant.paused {
        return Some("paused".to_string());
    }
    match grant.unavailable_reason.as_deref() {
        Some(reason) if !shows_nothing(&terminal_safe(reason)) => {
            Some(cap_drawn(&terminal_safe(reason), 40))
        }
        Some(_) | None => None,
    }
}

/// The no-grant sentence, shared by `--list`'s header and the spend path's
/// bail: the server's `ineligible_reason`, made terminal-safe and bounded.
pub(crate) fn no_resets_available(name: &str, status: &ResetStatus) -> String {
    match status.ineligible_reason.as_deref().map(terminal_safe) {
        Some(reason) if !shows_nothing(&reason) => {
            format!(
                "no limit resets available on '{name}' ({})",
                cap_drawn(&reason, 40)
            )
        }
        _ => format!("no limit resets available on '{name}'"),
    }
}

/// `--list`'s extra line when a fresh record exists: a spend sent at a local
/// time got no answer, and running `limit-reset` again before the window's end
/// re-sends it.
pub(crate) fn list_pending_note(name: &str, sent_at_ms: u64) -> String {
    let at = local_stamp((sent_at_ms / 1000) as i64).unwrap_or_else(|| sent_at_ms.to_string());
    format!(
        "clauth: a limit reset sent at {at} got no answer; run `clauth limit-reset {name}` again \
         before {} to re-send it.",
        re_send_deadline(sent_at_ms)
    )
}

/// The spend path's refusal for a next grant the server does not mark usable.
pub(crate) fn unusable_next_line(name: &str, grant: &Grant) -> String {
    format!(
        "the limit reset '{name}' would use next is {}, so no reset was used",
        grant_state(grant)
    )
}

/// Whether a grant id fits `^[a-z0-9_-]{1,40}$` — the pattern Claude Code
/// checks before it sends one.
pub(crate) fn grant_id_is_sendable(id: &str) -> bool {
    (1..=40).contains(&id.len())
        && id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

/// The spend path's refusal for a next grant whose id is off that pattern.
pub(crate) fn unsendable_grant_line(name: &str) -> String {
    format!(
        "Anthropic named a limit reset on '{name}' with an id clauth does not send, so no reset \
         was used"
    )
}

/// The not-at-limit warning: the cleared window with the most headroom left
/// (100 − `percent_used`), skipping a cleared window the grant names no
/// `percent_used` for. `None` when no cleared window has one.
fn not_at_limit_warning(name: &str, grant: &Grant) -> Option<String> {
    let best = grant
        .clears
        .iter()
        .filter_map(|window| {
            grant
                .percent_used
                .get(window)
                .map(|used| (window.as_str(), 100u32.saturating_sub(*used)))
        })
        .max_by_key(|(_, headroom)| *headroom)?;
    Some(format!(
        " '{name}' still has {}% of its {} left.",
        best.1,
        window_label(best.0)
    ))
}

/// `--list`: a count line — or the no-grant line with the server's
/// `ineligible_reason` — then every grant, the one the server named next
/// marked `*`.
pub(crate) fn describe_grants(name: &str, status: &ResetStatus) -> Vec<String> {
    let next = status.next_grant();
    let spendable = status.spend_grant();
    let mut lines = vec![match next {
        Some(_) => {
            let count = status.usable_resets();
            format!(
                "clauth: '{name}' has {count} limit reset{} available.",
                plural(count as usize)
            )
        }
        None => format!("clauth: {}.", no_resets_available(name, status)),
    }];
    for grant in &status.grants {
        let marked = next.is_some_and(|next| next.id == grant.id);
        let ends = grant
            .ends_at
            .as_deref()
            .filter(|e| !shows_nothing(&terminal_safe(e)))
            .map(|e| format!(", ends {}", stamp(e)))
            .unwrap_or_default();
        let suffix = if marked { "  (used next)" } else { "" };
        // The row counts only what a spend would use now: with no spendable
        // next grant, the header says 0 and every row reads 0 too.
        let count = if grant.usable_now && spendable.is_some() {
            grant.resets_left
        } else {
            0
        };
        let tail = row_state(grant)
            .map(|state| format!(", {state}"))
            .unwrap_or_default();
        lines.push(format!(
            "  {} {}: {count} of {} available, resets {}{tail}{ends}{suffix}",
            if marked { "*" } else { " " },
            grant.label(),
            grant.resets_total,
            clears_summary(&grant.clears),
        ));
    }
    lines
}

/// The `[y/N]` question, without the `[y/N]` (the caller's prompt adds it).
pub(crate) fn spend_prompt(name: &str, status: &ResetStatus, grant: &Grant) -> String {
    let head = if grant.is_untitled() {
        format!("clauth: use a limit reset on '{name}'?")
    } else {
        format!(
            "clauth: use the limit reset \"{}\" on '{name}'?",
            grant.label()
        )
    };
    let mut prompt = format!(
        "{head} It resets {} now and cannot be undone; {} of {} available on it.",
        clears_summary(&grant.clears),
        grant.resets_left,
        grant.resets_total,
    );
    if !status.at_limit
        && let Some(warning) = not_at_limit_warning(name, grant)
    {
        prompt.push_str(&warning);
    }
    if let Some(weekly) = status
        .weekly_resets_at
        .as_deref()
        .filter(|w| !shows_nothing(&terminal_safe(w)))
    {
        prompt.push_str(&format!(" The weekly reset stays {}.", stamp(weekly)));
    }
    prompt
}

/// The `[y/N]` question for a re-send, without the `[y/N]`: a fresh record
/// means the earlier spend's answer never arrived, so the prompt names that
/// request, not a grant pick.
pub(crate) fn re_send_prompt(name: &str) -> String {
    format!(
        "clauth: re-send the limit reset on '{name}' whose answer never arrived? It cannot be \
         undone."
    )
}

/// A 401 on any call: nothing was spent, and the poll is what refreshes the
/// stored token.
fn token_rejected(name: &str) -> String {
    format!(
        "Anthropic rejected the stored access token for '{name}'; clauth refreshes it at its \
         next poll"
    )
}

/// The claude arm's voice for a failed list read (and for a failed spend,
/// whose body names no read).
const CLAUDE_LIST_VOICE: FailureVoice = FailureVoice {
    provider: "Anthropic",
    rejected: token_rejected,
    read_noun: "the reset list",
    read_phrase: "list the resets on",
};

/// The claude arm's voice for a failed account read.
const CLAUDE_ORG_VOICE: FailureVoice = FailureVoice {
    provider: "Anthropic",
    rejected: token_rejected,
    read_noun: "the account read",
    read_phrase: "read the account for",
};

/// A failed read. Nothing was spent, and every line says so.
pub(crate) fn read_failure(name: &str, err: &ResetCallError) -> String {
    shared_read_failure(name, err, &CLAUDE_LIST_VOICE)
}

/// A failed org read. Nothing was spent.
pub(crate) fn org_failure(name: &str, err: &ResetCallError) -> String {
    shared_read_failure(name, err, &CLAUDE_ORG_VOICE)
}

/// What the post-spend refresh did with the stored count after its re-read
/// failed, decided under the state flock so the line it prints says only what
/// happened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RefreshDrop {
    /// The delete landed under the state flock.
    Dropped,
    /// The stored count was read at or after this run's send, so the delete
    /// was skipped: a side read that already saw the spend is the best count
    /// clauth holds.
    KeptNewer,
    /// The gate refused: the stored login changed or could not be read.
    LoginChanged,
    /// The state flock could not be taken; nothing was attempted.
    LockTimedOut,
    /// The delete was attempted and did not land.
    Failed,
}

/// The stderr line for the post-spend re-read's failure: the spend already
/// landed, so the line names what the re-read hit and what became of the
/// stored count, never that no reset was used.
pub(crate) fn refresh_read_failure(name: &str, err: &ResetCallError, drop: RefreshDrop) -> String {
    let why = match err {
        ResetCallError::Unauthorized => "Anthropic rejected the stored access token".to_string(),
        ResetCallError::Status(status) => format!("Anthropic answered with HTTP {status}"),
        ResetCallError::Transport => "could not reach Anthropic".to_string(),
        ResetCallError::Parse => "Anthropic answered in a shape clauth does not read".to_string(),
    };
    let after = match drop {
        RefreshDrop::Dropped => "; the stored count was dropped",
        RefreshDrop::KeptNewer => "; the stored count is newer than the spend, so it stays",
        RefreshDrop::LoginChanged => {
            "; the stored login changed or could not be read, so nothing was touched"
        }
        RefreshDrop::LockTimedOut => "; the state lock was unavailable, so nothing was touched",
        RefreshDrop::Failed => "",
    };
    format!("clauth: '{name}': the limit reset read after the spend failed ({why}){after}")
}

/// The stderr line for a failed drop of the stored count after a failed
/// re-read: one more line, and the run's exit stays what it was.
pub(crate) fn refresh_drop_failure(name: &str) -> String {
    format!("clauth: '{name}': could not remove the stored limit reset count")
}

/// The stderr line for a re-read the refresh could not store: the write leg's
/// gate admitted it and the cache write did not land.
pub(crate) fn refresh_store_failure(name: &str) -> String {
    format!("clauth: '{name}': the limit reset count read after the spend was not stored")
}

/// The stderr line for a state flock the refresh could not take: nothing was
/// stored, and the count stays what it was until the hourly side read.
pub(crate) fn refresh_lock_failure(name: &str) -> String {
    format!(
        "clauth: '{name}': the state lock was unavailable, so the limit reset count read after \
         the spend was not stored"
    )
}

/// A failed POST. A fresh send's 401 proves nothing was spent and any other 4xx
/// spent nothing; every other failure leaves the outcome open, and the shared
/// line points at the re-run the pending record makes safe. A re-send's
/// failure leaves the earlier request's outcome unknown, and says so.
pub(crate) fn spend_failure(
    name: &str,
    err: &ResetCallError,
    sent_at_ms: u64,
    re_send: bool,
) -> String {
    shared_spend_failure(name, err, &CLAUDE_LIST_VOICE, sent_at_ms, re_send)
}

/// What a spend reply means for the operator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SpendOutcome {
    /// A reset landed.
    Reset,
    /// The grant was already spent.
    AlreadyUsed,
    /// The account is not at a limit a reset covers.
    NotLimited,
    /// The account's resets are cooling down.
    Cooldown,
    /// The account cannot use a reset now.
    Ineligible,
    /// The grant is unavailable.
    Unavailable,
    /// The server said its own outcome is unconfirmed.
    Unconfirmed(String),
    /// A result this arm does not define. Whether anything was spent is unknown.
    Unknown(String),
}

impl SpendReply {
    /// The reply's outcome, read once: the indeterminate reason overrides
    /// whatever the result says.
    pub(crate) fn outcome(&self) -> SpendOutcome {
        match self.reason.as_deref() {
            Some(reason @ ("reset_unconfirmed" | "stamp_indeterminate")) => {
                SpendOutcome::Unconfirmed(reason.to_string())
            }
            _ => match self.result.as_str() {
                "reset" => SpendOutcome::Reset,
                "already_used" => SpendOutcome::AlreadyUsed,
                "not_limited" => SpendOutcome::NotLimited,
                "cooldown" => SpendOutcome::Cooldown,
                "ineligible" => SpendOutcome::Ineligible,
                "unavailable" => SpendOutcome::Unavailable,
                other => SpendOutcome::Unknown(other.to_string()),
            },
        }
    }
}

/// Whether a reply names a definite outcome, so its pending record clears: an
/// unconfirmed reason or an unrecognized result leaves the outcome open.
pub(crate) fn spend_settled(reply: &SpendReply) -> bool {
    !matches!(
        reply.outcome(),
        SpendOutcome::Unconfirmed(_) | SpendOutcome::Unknown(_)
    )
}

/// Whether a reply means a reset actually came off the account's count: the
/// spend landed, or the grant was already gone. Narrower than
/// [`spend_settled`], which is also true for refusals that spent nothing;
/// the CLI's post-spend refresh re-reads only on this predicate.
pub(crate) fn spend_moved_the_count(reply: &SpendReply) -> bool {
    matches!(
        reply.outcome(),
        SpendOutcome::Reset | SpendOutcome::AlreadyUsed
    )
}

/// A spend reply as the line to print (`Ok`) or the error to exit with
/// (`Err`). Only `reset` is a success; a result or reason that leaves the
/// outcome open says so and names the re-send window's end.
pub(crate) fn spend_outcome_line(
    name: &str,
    reply: &SpendReply,
    sent_at_ms: u64,
) -> Result<String, String> {
    match reply.outcome() {
        SpendOutcome::Reset => {
            let mut line = format!("clauth: used a limit reset on '{name}'");
            if !reply.cleared.is_empty() {
                line.push_str(&format!(": reset {}", join_windows(&reply.cleared)));
            }
            if let Some(left) = reply.resets_left {
                line.push_str(&format!(", {left} left"));
            }
            line.push('.');
            Ok(line)
        }
        SpendOutcome::AlreadyUsed => Err(format!(
            "that limit reset on '{name}' was already used; run `clauth limit-reset {name} \
             --list` to see what is left"
        )),
        SpendOutcome::NotLimited => Err(format!(
            "'{name}' is not at a limit that reset covers, so no reset was used"
        )),
        SpendOutcome::Cooldown => {
            let until = reply
                .cooldown_until
                .as_deref()
                .filter(|u| !shows_nothing(&terminal_safe(u)))
                .map(|u| format!(" until {}", stamp(u)))
                .unwrap_or_default();
            Err(format!(
                "limit resets on '{name}' are cooling down{until}, so no reset was used"
            ))
        }
        SpendOutcome::Ineligible => {
            let reason = reply
                .reason
                .as_deref()
                .filter(|r| !shows_nothing(&terminal_safe(r)))
                .map(|r| format!(" ({})", cap_drawn(&terminal_safe(r), 40)))
                .unwrap_or_default();
            Err(format!(
                "'{name}' cannot use a limit reset{reason}, so no reset was used"
            ))
        }
        SpendOutcome::Unavailable => match reply.reason.as_deref() {
            Some(reason) if !shows_nothing(&terminal_safe(reason)) => Err(format!(
                "that limit reset on '{name}' is unavailable ({}), so no reset was used",
                cap_drawn(&terminal_safe(reason), 40)
            )),
            Some(_) | None => Err(format!(
                "that limit reset on '{name}' is unavailable, so no reset was used"
            )),
        },
        SpendOutcome::Unconfirmed(reason) => Err(format!(
            "Anthropic could not confirm the reset on '{name}' ({reason}), so the outcome is \
             unconfirmed; {}",
            re_send_hint(sent_at_ms)
        )),
        SpendOutcome::Unknown(other) => {
            let shown = quoted_escaped(&other);
            Err(format!(
                "Anthropic answered the reset request for '{name}' with an unrecognized result \
                 {shown}, so the outcome is unconfirmed; {}",
                re_send_hint(sent_at_ms)
            ))
        }
    }
}

#[cfg(test)]
#[path = "../../tests/inline/claude_reset.rs"]
mod tests;
