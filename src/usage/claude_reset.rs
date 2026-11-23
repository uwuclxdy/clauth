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

use serde::{Deserialize, Serialize};

use super::codex_reset::{
    FailureVoice, TableRow, cap_drawn, minute_stamp, quoted_escaped, re_send_deadline,
    re_send_hint, read_failure as shared_read_failure, read_reply, read_reply_hinted, reset_table,
    shared_not_listed_line, shared_pinned_record_line, spend_failure as shared_spend_failure,
    stamp, trim_padding,
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
    pub(crate) weekly_resets_at: Option<String>,
    /// One stamp for the whole block: while it lies ahead, no held grant the
    /// server marks unusable can be spent.
    #[serde(default)]
    pub(crate) cooldown_until: Option<String>,
}

impl ResetStatus {
    /// The grant a spend of `id` would send now, or why it refuses: the grant
    /// the server lists under that id, when the server marks it usable now and
    /// its id fits the pattern Claude Code sends, since Claude Code refuses to
    /// send an id off the pattern before it spends. A grant both cooling and
    /// off the pattern reads as not usable now. Any listed grant can be named,
    /// not only the one the server would spend next.
    pub(crate) fn try_spend_grant_id(&self, id: &str) -> Result<&Grant, SpendRefusal<'_>> {
        let grant = self
            .grants
            .iter()
            .find(|g| g.id == id)
            .ok_or(SpendRefusal::NotListed)?;
        if !grant.is_usable_now() {
            return Err(SpendRefusal::NotUsableNow(grant));
        }
        if !grant_id_is_sendable(&grant.id) {
            return Err(SpendRefusal::UnsendableId);
        }
        Ok(grant)
    }

    /// The account's banked resets a spend can use now: the sum of
    /// `resets_left` over every grant [`Self::try_spend_grant_id`] would send.
    /// The scheduler's side read and the post-spend refresh land this count in
    /// `limit_resets.json`. Saturated, as the codex arm clamps its server
    /// count: a body whose grants sum past `u32::MAX` reads as maxed, never
    /// wraps or panics.
    pub(crate) fn usable_resets(&self) -> u32 {
        self.grants
            .iter()
            .filter(|g| g.is_usable_now() && grant_id_is_sendable(&g.id))
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

/// Why [`ResetStatus::try_spend_grant_id`] refuses, each variant carrying what
/// its refusal line names.
#[derive(Debug)]
pub(crate) enum SpendRefusal<'a> {
    /// The server lists no grant under the id.
    NotListed,
    /// The server does not mark the grant usable now.
    NotUsableNow(&'a Grant),
    /// The grant's id is off `^[a-z0-9_-]{1,40}$`.
    UnsendableId,
}

/// One grant, as the status block lists it and `limit_resets.json` keeps it.
/// Every field but `id` defaults, so a grant the server describes more
/// sparsely than Claude Code expects still lists, and a stored grant an older
/// clauth wrote still loads.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub(crate) struct Grant {
    pub(crate) id: String,
    #[serde(default)]
    label: String,
    #[serde(default)]
    resets_total: u32,
    #[serde(default)]
    resets_left: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    ends_at: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    clears: Vec<String>,
    #[serde(default)]
    paused: bool,
    #[serde(default)]
    usable_now: bool,
    #[serde(default)]
    use_requires_limit: bool,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    percent_used: HashMap<String, u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    unavailable_reason: Option<String>,
}

impl Grant {
    /// Whether the server marks this grant spendable now.
    pub(crate) fn is_usable_now(&self) -> bool {
        self.usable_now
    }

    /// The resets this grant still holds.
    pub(crate) fn resets_left(&self) -> u32 {
        self.resets_left
    }

    /// The window names this grant clears, raw server spelling.
    pub(crate) fn clears(&self) -> &[String] {
        &self.clears
    }

    /// The grant's ISO end stamp, raw server spelling.
    pub(crate) fn ends_at(&self) -> Option<&str> {
        self.ends_at.as_deref()
    }

    /// `left/total`, the table's LEFT cell; a total the server never sent
    /// reads as the resets still held.
    pub(crate) fn left_of_total(&self) -> String {
        format!(
            "{}/{}",
            self.resets_left,
            self.resets_total.max(self.resets_left)
        )
    }

    /// The server's label whole, or the generic name; control characters
    /// dropped, since this text reaches a terminal. The prompt cuts it to 40.
    pub(crate) fn label(&self) -> String {
        let label = terminal_safe(&self.label);
        if shows_nothing(&label) {
            "limit reset".to_string()
        } else {
            trim_padding(&label).to_string()
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
/// read ([`read_status_at`]) parses the typed body through [`read_reply_hinted`], and
/// this seam lets tests drive fixture bytes straight into the parser.
#[cfg(test)]
pub(crate) fn read_status(body: &str) -> Result<ResetStatus, ResetCallError> {
    serde_json::from_str::<CedarEmberBody>(body)
        .map(CedarEmberBody::into_status)
        .map_err(|_| ResetCallError::Parse)
}

/// GET the account's reset grants. Spends nothing.
pub(crate) fn read_status_at(url: &str, access_token: &str) -> Result<ResetStatus, ResetCallError> {
    read_status_hinted_at(url, access_token).map_err(|(err, _)| err)
}

/// [`read_status_at`] carrying a non-2xx reply's `retry-after` beside its
/// error (see [`read_reply_hinted`]), for the scheduler's side read.
pub(crate) fn read_status_hinted_at(
    url: &str,
    access_token: &str,
) -> Result<ResetStatus, (ResetCallError, Option<Duration>)> {
    read_reply_hinted::<CedarEmberBody>(
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
/// banked total, every grant as the server listed it, the block's cooldown and
/// limit flag, and the read's own epoch-ms stamp. The scheduler's hourly side
/// read and the post-spend refresh share it, so a spend's re-read lands
/// exactly what the hourly read would have.
pub(crate) fn status_reading(status: &ResetStatus) -> LimitResets {
    LimitResets {
        usable: status.usable_resets(),
        banked: Some(status.banked_resets()),
        grants: status.grants.clone(),
        cooldown_until: status.cooldown_until.clone(),
        weekly_resets_at: status.weekly_resets_at.clone(),
        at_limit: status.at_limit,
        fetched_at: Some(super::now_ms()),
        ..LimitResets::default()
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
/// Code's own copy uses, `7d fable` for the weekly window Fable counts into
/// (the name the usage bars give it), any other name as sent with its control
/// characters dropped and cut to 40. Shared by `--list` and the TUI's resets
/// row, so the two surfaces never spell a label apart.
pub(crate) fn window_label(name: &str) -> String {
    match name {
        "five_hour" => "5h".to_string(),
        "seven_day" => "7d".to_string(),
        "seven_day_opus" => "7d opus".to_string(),
        "seven_day_overage_included" => "7d fable".to_string(),
        "seven_day_sonnet" => "7d sonnet".to_string(),
        other => cap_drawn(&terminal_safe(other), 40),
    }
}

/// `N available` when a spend can use every reset still held, else
/// `usable/held available`. Shared by `--list` and the TUI's resets row, so
/// both count against what is held, never a grant's original size.
pub(crate) fn available_of(usable: u32, held: u32) -> String {
    if usable == held {
        format!("{usable} available")
    } else {
        format!("{usable}/{held} available")
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

/// The windows a grant clears as a table cell and a rail line spell them:
/// `7d, 7d fable`, empty when the server names none.
pub(crate) fn windows_cell(clears: &[String]) -> String {
    clears
        .iter()
        .map(|w| window_label(w))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The seconds left on the block's cooldown at `now_secs`, `None` once it
/// lies behind or when the server sent no stamp clauth reads.
pub(crate) fn cooldown_left(cooldown_until: Option<&str>, now_secs: i64) -> Option<u64> {
    let until = cooldown_until.and_then(super::fetch::iso_to_epoch_secs)?;
    u64::try_from(until.saturating_sub(now_secs))
        .ok()
        .filter(|left| *left > 0)
}

/// A grant's STATE word: `used` once it holds none, `ready` while the server
/// marks it usable now and its id is one a spend would send, `unavailable` for
/// a usable grant whose id is off that pattern, `cooldown <dur>` for any other
/// held grant while the block's cooldown lies ahead, `paused`, `at limit only`
/// for a grant that needs a limit the account is under, else the server's own
/// reason cut to 20 (terminal-safe), else `unavailable`. The table, the TUI
/// modal and the refusal of an unusable grant all name it from here.
pub(crate) fn grant_state(
    grant: &Grant,
    cooldown_until: Option<&str>,
    at_limit: bool,
    now_secs: i64,
) -> String {
    if grant.resets_left == 0 {
        return "used".to_string();
    }
    if grant.usable_now {
        return if grant_id_is_sendable(&grant.id) {
            "ready".to_string()
        } else {
            "unavailable".to_string()
        };
    }
    if let Some(left) = cooldown_left(cooldown_until, now_secs) {
        return format!("cooldown {}", super::fetch::humanize_duration(left as i64));
    }
    if grant.paused {
        return "paused".to_string();
    }
    if grant.use_requires_limit && !at_limit {
        return "at limit only".to_string();
    }
    match grant.unavailable_reason.as_deref().map(terminal_safe) {
        Some(reason) if !shows_nothing(&reason) => cap_drawn(trim_padding(&reason), 20),
        Some(_) | None => "unavailable".to_string(),
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

/// The listing's extra line when a fresh record exists: a spend sent at a
/// local time got no answer, and naming its grant again before the window's
/// end re-sends it.
pub(crate) fn list_pending_note(name: &str, grant_id: &str, sent_at_ms: u64) -> String {
    let at = local_stamp((sent_at_ms / 1000) as i64).unwrap_or_else(|| sent_at_ms.to_string());
    format!(
        "clauth: a limit reset sent at {at} got no answer; run `clauth limit-reset {name} {}` \
         before {} to re-send it.",
        terminal_safe(grant_id),
        re_send_deadline(sent_at_ms)
    )
}

/// The refusal of a spend naming a grant other than the one a fresh record
/// holds: the record's grant is the only one sent until its window ends, so
/// one intent never spends two resets.
pub(crate) fn pinned_record_line(name: &str, grant_id: &str, sent_at_ms: u64) -> String {
    shared_pinned_record_line("a limit reset", name, grant_id, sent_at_ms)
}

/// The refusal of an id the read does not list.
pub(crate) fn not_listed_line(name: &str, grant_id: &str) -> String {
    shared_not_listed_line("a limit reset", name, grant_id)
}

/// The spend path's refusal for a listed grant the server does not mark
/// usable now, naming its STATE word.
pub(crate) fn unusable_grant_line(name: &str, status: &ResetStatus, grant: &Grant) -> String {
    let state = grant_state(
        grant,
        status.cooldown_until.as_deref(),
        status.at_limit,
        super::now_epoch_secs(),
    );
    format!(
        "the limit reset {} on '{name}' cannot be used now ({state}), so no reset was used",
        quoted_escaped(&grant.id)
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

/// The spend path's refusal for a listed grant whose id is off that pattern.
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
    let (window, left) = headroom(grant)?;
    Some(format!(" '{name}' still has {left}% of its {window} left."))
}

/// The window this grant clears with the most headroom left (100 −
/// `percent_used`) and that headroom, skipping a cleared window the grant
/// names no `percent_used` for: what spending it now throws away. The prompt's
/// warning and the TUI modal's `now` row both read it.
pub(crate) fn headroom(grant: &Grant) -> Option<(String, u32)> {
    grant
        .clears
        .iter()
        .filter_map(|window| {
            grant
                .percent_used
                .get(window)
                .map(|used| (window.as_str(), 100u32.saturating_sub(*used)))
        })
        .max_by_key(|(_, headroom)| *headroom)
        .map(|(window, left)| (window_label(window), left))
}

/// The listing: a count line — or, with no grant listed, the no-grant line
/// with the server's `ineligible_reason` — then a blank line and the table,
/// one row per grant in the server's order. No grant is marked: the operator
/// names the one to spend by its id.
pub(crate) fn describe_grants(name: &str, status: &ResetStatus) -> Vec<String> {
    if status.grants.is_empty() {
        return vec![format!("clauth: {}.", no_resets_available(name, status))];
    }
    let usable = status.usable_resets();
    let held = status.banked_resets();
    let mut lines = vec![
        format!(
            "clauth: '{name}' has {} limit reset{} available.",
            if usable == held {
                usable.to_string()
            } else {
                format!("{usable}/{held}")
            },
            plural(held.max(usable) as usize)
        ),
        String::new(),
    ];
    let now = super::now_epoch_secs();
    lines.extend(reset_table(status.grants.iter().map(|grant| {
        TableRow {
            id: grant.id.clone(),
            left: grant.left_of_total(),
            state: grant_state(
                grant,
                status.cooldown_until.as_deref(),
                status.at_limit,
                now,
            ),
            resets: windows_cell(&grant.clears),
            expires: grant
                .ends_at
                .as_deref()
                .filter(|e| !shows_nothing(&terminal_safe(e)))
                .map_or_else(|| "-".to_string(), minute_stamp),
            label: grant.label(),
        }
    })));
    lines
}

/// The `[y/N]` question, without the `[y/N]` (the caller's prompt adds it).
/// Only a grant the spend rule accepts is asked about, so every reset it holds
/// is usable and the count reads bare.
pub(crate) fn spend_prompt(name: &str, status: &ResetStatus, grant: &Grant) -> String {
    let head = if grant.is_untitled() {
        format!("clauth: use a limit reset on '{name}'?")
    } else {
        format!(
            "clauth: use the limit reset \"{}\" on '{name}'?",
            cap_drawn(&grant.label(), 40)
        )
    };
    let mut prompt = format!(
        "{head} It resets {} now and cannot be undone; {} available on it.",
        clears_summary(&grant.clears),
        grant.resets_left,
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

    /// The server's own word for refusing the spend: the reason it named, or
    /// the result when it names none, terminal-safe and cut like the CLI's
    /// reason. `None` for a reply that landed, for one reporting the grant
    /// already spent (that reply still moved the count), and for one whose
    /// outcome is still open.
    pub(crate) fn refusal_reason(&self) -> Option<String> {
        let named = self
            .reason
            .as_deref()
            .map(terminal_safe)
            .filter(|reason| !shows_nothing(reason));
        let result = terminal_safe(&self.result);
        match self.outcome() {
            SpendOutcome::Reset
            | SpendOutcome::AlreadyUsed
            | SpendOutcome::Unconfirmed(_)
            | SpendOutcome::Unknown(_) => None,
            SpendOutcome::NotLimited
            | SpendOutcome::Cooldown
            | SpendOutcome::Ineligible
            | SpendOutcome::Unavailable => {
                Some(cap_drawn(named.as_deref().unwrap_or(result.as_str()), 40))
            }
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
