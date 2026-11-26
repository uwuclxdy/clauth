//! The one banked-reset spend `clauth limit-reset <name> <id>` and the TUI's
//! `use a limit reset` both run, per arm: decide (the account's refusals, the
//! spend lock, the pending record's pin, the read, the pick by id), record,
//! send once, settle, then refresh what the account's reading says. A surface
//! supplies only its confirmation and where its lines go
//! ([`SpendSurface`]), so the two surfaces can never spend by different
//! rules.
//!
//! Read-only on the credential, as both arms' modules are: the stored token is
//! sent as it stands and a rejection is reported, never refreshed from here.

use std::sync::atomic::AtomicBool;

use anyhow::Result;

use crate::profile::{AppConfig, Profile, ProfileName};
use crate::usage::codex_reset::ResetUrls;
use crate::usage::{self, claude_reset, codex_reset, limit_reset_pending as pending};

/// What a spend asks its surface and tells it.
pub(crate) trait SpendSurface {
    /// The question before anything is spent; `false` aborts with nothing
    /// sent. Never asked under `yes`.
    fn confirm(&mut self, prompt: &str) -> Result<bool>;
    /// A diagnostic line: the lock-wait note, a record that would not clear,
    /// a failed refresh.
    fn note(&mut self, line: &str);
    /// A settled reply's outcome, told before the refresh that follows it: the
    /// success summary, or the refusal of a reply that still moved the count.
    /// Any other refusal comes back as the spend's error instead.
    fn outcome(&mut self, outcome: &std::result::Result<String, String>);
    /// The server's own reason when its reply refused a spend that reached it,
    /// terminal-safe and cut like the CLI's reason. Told beside
    /// [`SpendSurface::outcome`], whose line the CLI prints or returns as the
    /// error; a surface with its own words for a refusal shows this reason
    /// instead.
    fn refused(&mut self, reason: &str);
    /// A reply that moved the account's count: a reset, an already-used
    /// grant, a no-credit answer. The core re-reads the account's list after
    /// it, and that re-read may drop the stored copy or fail to store what it
    /// read, so a surface must not offer the spent reset again when no reading
    /// re-landed. `reread_stored` says whether that re-read's reading reached
    /// the stored copy: only then may a surface rebuild its rows from disk.
    /// Never told for a refusal that spent nothing or a refusal made before
    /// the spend left.
    fn moved_count(&mut self, reread_stored: bool);
}

/// The modal's line for a reply that refused a spend after it was sent, both
/// arms: the server's own reason, and that this request used no reset.
pub(crate) fn server_refused_line(reason: &str) -> String {
    format!("the server refused this reset ({reason}); no reset was used")
}

/// How a spend that returned `Ok` ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Spent {
    /// The reply was sent and its outcome told through
    /// [`SpendSurface::outcome`].
    Sent,
    /// The surface declined; the line says what that left behind.
    Declined(String),
}

/// The claude arm's listing: the read, then the table and, while a fresh
/// record waits, the note naming the command that re-sends it.
pub(crate) fn list_claude(
    config: &AppConfig,
    profile: &Profile,
    urls: &ResetUrls,
) -> Result<Vec<String>> {
    let canonical = &profile.name;
    let access_token = claude_preconditions(config, profile)?;
    let status =
        claude_reset::read_status_at(&claude_reset::read_url(&urls.claude_origin), access_token)
            .map_err(|e| anyhow::anyhow!(claude_reset::read_failure(canonical, &e)))?;
    let mut lines = claude_reset::describe_grants(canonical, &status);
    let path = pending::path::<pending::ClaudePending>(canonical)?;
    if let Some(record) = pending::fresh_record::<pending::ClaudePending>(&path) {
        lines.push(claude_reset::list_pending_note(
            canonical,
            &record.grant_id,
            record.sent_at_ms,
        ));
    }
    Ok(lines)
}

/// The refusals every claude call runs before anything leaves the machine,
/// and the access token they leave: a proxy account, a profile with no claude
/// login, a quarantined chain.
fn claude_preconditions<'a>(config: &AppConfig, profile: &'a Profile) -> Result<&'a str> {
    crate::actions::refuse_limit_reset_on_proxy(profile)?;
    let canonical = &profile.name;
    let Some(access_token) = profile.access_token().filter(|t| !t.is_empty()) else {
        anyhow::bail!(
            "'{canonical}' has no stored claude login to use a reset with; run `clauth login \
             {canonical}`"
        );
    };
    crate::oauth::refuse_if_chain_dead(config, Some(profile), canonical)?;
    Ok(access_token)
}

/// The confirm policy both arms share: a spend nobody can answer for (no
/// `yes`, no terminal) is refused before any request.
fn refuse_unconfirmed(canonical: &str, yes: bool, interactive: bool) -> Result<()> {
    if !yes && !interactive {
        anyhow::bail!("refusing to use a reset on '{canonical}' without confirmation; pass --yes");
    }
    Ok(())
}

/// Spend the claude grant `grant_id` names. A fresh pending record pins its
/// own grant: naming it re-sends that request, naming any other is refused
/// with nothing sent. Every other refusal lands before the spend leaves.
pub(crate) fn spend_claude(
    config: &AppConfig,
    profile: &Profile,
    grant_id: &str,
    yes: bool,
    interactive: bool,
    urls: &ResetUrls,
    surface: &mut impl SpendSurface,
) -> Result<Spent> {
    let canonical = &profile.name;
    let access_token = claude_preconditions(config, profile)?;
    refuse_unconfirmed(canonical, yes, interactive)?;
    // The identity the spend runs under, captured beside the token it sends:
    // the post-spend refresh lands its re-read only while the stored login is
    // still this one.
    let identity = usage::spawn_oauth_identity(canonical, access_token);
    // The spend serializes on the account's limit-reset lock before the first
    // request, so a second run waits and re-decides on what this one leaves.
    let spend_lock = pending::LimitResetLock::acquire(canonical, |line| surface.note(line))?;
    let pending_path = pending::path::<pending::ClaudePending>(canonical)?;
    let recorded = pending::take_fresh::<pending::ClaudePending>(&pending_path)?;
    if let Some(record) = &recorded
        && record.grant_id != grant_id
    {
        anyhow::bail!(
            "{}",
            claude_reset::pinned_record_line(canonical, &record.grant_id, record.sent_at_ms)
        );
    }
    let status =
        claude_reset::read_status_at(&claude_reset::read_url(&urls.claude_origin), access_token)
            .map_err(|e| anyhow::anyhow!(claude_reset::read_failure(canonical, &e)))?;
    let org = || {
        claude_reset::read_org_uuid_at(&claude_reset::org_url(&urls.claude_origin), access_token)
            .map_err(|e| anyhow::anyhow!(claude_reset::org_failure(canonical, &e)))
    };
    let re_send = recorded.is_some();
    let (org_uuid, request_id, sent_at_ms) = match recorded {
        Some(record) => {
            let org_uuid = org()?;
            if !yes && !surface.confirm(&claude_reset::re_send_prompt(canonical))? {
                return Ok(Spent::Declined(claude_reset::declined_re_send_line(
                    canonical,
                )));
            }
            (org_uuid, record.request_id, record.sent_at_ms)
        }
        None => {
            let grant = match status.try_spend_grant_id(grant_id) {
                Ok(grant) => grant,
                Err(claude_reset::SpendRefusal::NotListed) if status.grants.is_empty() => {
                    anyhow::bail!("{}", claude_reset::no_resets_available(canonical, &status))
                }
                Err(claude_reset::SpendRefusal::NotListed) => {
                    anyhow::bail!("{}", claude_reset::not_listed_line(canonical, grant_id))
                }
                Err(claude_reset::SpendRefusal::NotUsableNow(grant)) => {
                    anyhow::bail!(
                        "{}",
                        claude_reset::unusable_grant_line(canonical, &status, grant)
                    )
                }
                Err(claude_reset::SpendRefusal::UnsendableId) => {
                    anyhow::bail!("{}", claude_reset::unsendable_grant_line(canonical))
                }
            };
            let org_uuid = org()?;
            if !yes && !surface.confirm(&claude_reset::spend_prompt(canonical, &status, grant))? {
                return Ok(Spent::Declined(format!(
                    "clauth: aborted. no reset was used on '{canonical}'."
                )));
            }
            let request_id = claude_reset::new_request_id()?;
            let sent_at_ms = usage::now_ms();
            pending::write(
                &pending_path,
                &pending::ClaudePending {
                    grant_id: grant.id.clone(),
                    request_id: request_id.clone(),
                    sent_at_ms,
                },
            )?;
            (org_uuid, request_id, sent_at_ms)
        }
    };
    let this_send_at_ms = usage::now_ms();
    let reply = claude_reset::spend_reset_at(
        &claude_reset::spend_url(&urls.claude_origin, &org_uuid),
        access_token,
        grant_id,
        &request_id,
    );
    let settled = claude_reset::post_settles(re_send, &reply, claude_reset::spend_settled);
    if settled && let Err(e) = pending::clear(&pending_path) {
        surface.note(&format!(
            "clauth: could not remove {} after the reset settled: {e}",
            pending_path.display()
        ));
    }
    // The spend lock is a true leaf (`lockorder`'s `LimitResetSpend`), so it
    // drops before the refresh, whose state-flock hold is a ranked lock.
    drop(spend_lock);
    // A settled record is gone by here, or still re-sendable when its clear
    // failed, so the outcome is told before the re-read: a run killed during
    // the refresh must still have said what it spent. An already-used answer
    // is the one refusal that still moved the count: it is told before the
    // re-read too, and reports through `FailureReported` after it. Every
    // other refusal is the returned error.
    match reply {
        Ok(reply) => {
            let outcome = claude_reset::spend_outcome_line(canonical, &reply, sent_at_ms);
            if let Some(reason) = reply.refusal_reason() {
                surface.refused(&reason);
            }
            if !claude_reset::spend_moved_the_count(&reply) {
                return outcome
                    .map(|summary| {
                        surface.outcome(&Ok(summary));
                        Spent::Sent
                    })
                    .map_err(|e| anyhow::anyhow!(e));
            }
            surface.outcome(&outcome);
            let reread_stored = refresh_claude(
                canonical,
                &urls.claude_origin,
                access_token,
                &identity,
                this_send_at_ms,
                surface,
            );
            // The re-read's landing decides whether a surface may rebuild its
            // rows from the stored copy.
            surface.moved_count(reread_stored);
            match outcome {
                Ok(_) => Ok(Spent::Sent),
                Err(_) => Err(anyhow::Error::new(crate::FailureReported)),
            }
        }
        Err(e) => Err(anyhow::anyhow!(claude_reset::spend_failure(
            canonical, &e, sent_at_ms, re_send
        ))),
    }
}

/// Whether the account's stored reading was taken at or after this run's
/// send: a side read that already saw that send is the best reading clauth
/// holds, so a failed re-read keeps it. A reading with no stamp counts as
/// older.
fn stored_reading_is_newer(name: &ProfileName, sent_at_ms: u64) -> bool {
    crate::profile_cache::load_profile_cache::<usage::LimitResets>(
        name,
        crate::profile_cache::LIMIT_RESETS_FILE,
    )
    .and_then(|r| r.fetched_at)
    .is_some_and(|at| at >= sent_at_ms)
}

/// The post-spend refresh: after a settled spend that moved the count, the
/// spend re-reads the account's resets itself and lands what the server says
/// in the account's `limit_resets.json` under the state flock, so no surface
/// keeps showing the spent reset until the hourly side read. The write and the
/// delete share the scheduler's side-read gate: a moved or unreadable login
/// lands nothing and deletes nothing, and a stored reading read at or after
/// this run's send (`sent_at_ms`) stays, since a side read that already saw
/// that send is the best reading clauth holds; a reading with no stamp counts
/// as older and goes. Every failure of the refresh (the re-read, the store, the
/// drop, the state flock) is one note, while the spend's outcome stays what it
/// was. The caller releases the spend lock first: it is a true leaf
/// (`lockorder`'s `LimitResetSpend`), and the flock hold below is a ranked
/// lock.
///
/// Returns whether this run's re-read stored its reading: `false` on every
/// drop, kept and not-stored arm, so the caller knows whether the surface may
/// rebuild its rows from the stored copy.
fn refresh_claude(
    canonical: &ProfileName,
    origin: &str,
    access_token: &str,
    identity: &usage::OAuthIdentity,
    sent_at_ms: u64,
    surface: &mut impl SpendSurface,
) -> bool {
    match claude_reset::read_status_at(&claude_reset::read_url(origin), access_token) {
        Ok(status) => {
            let reading = claude_reset::status_reading(&status);
            // `None` when the gate kept the reading off a login that moved or
            // could not be read; otherwise whether the write landed.
            let stored = crate::lock::with_state_lock(|_held| {
                let (disk, unreadable) = usage::read_oauth_for_compare(canonical);
                if unreadable || usage::oauth_changed(identity, &disk) {
                    return Ok(None);
                }
                Ok(Some(
                    crate::profile_cache::write_profile_cache_reported_for(
                        crate::harness::Harness::Claude,
                        canonical,
                        crate::profile_cache::LIMIT_RESETS_FILE,
                        &reading,
                    ),
                ))
            });
            match stored {
                Ok(Some(true)) => true,
                Ok(Some(false)) => {
                    surface.note(&claude_reset::refresh_store_failure(canonical));
                    false
                }
                Ok(None) => false,
                Err(_) => {
                    surface.note(&claude_reset::refresh_lock_failure(canonical));
                    false
                }
            }
        }
        Err(e) => {
            let drop = match crate::lock::with_state_lock(|_held| {
                let (disk, unreadable) = usage::read_oauth_for_compare(canonical);
                if unreadable || usage::oauth_changed(identity, &disk) {
                    return Ok(claude_reset::RefreshDrop::LoginChanged);
                }
                if stored_reading_is_newer(canonical, sent_at_ms) {
                    return Ok(claude_reset::RefreshDrop::KeptNewer);
                }
                Ok(
                    if crate::profile_cache::remove_profile_cache_reported(
                        canonical,
                        crate::profile_cache::LIMIT_RESETS_FILE,
                    ) {
                        claude_reset::RefreshDrop::Dropped
                    } else {
                        claude_reset::RefreshDrop::Failed
                    },
                )
            }) {
                Ok(drop) => drop,
                Err(_) => claude_reset::RefreshDrop::LockTimedOut,
            };
            surface.note(&claude_reset::refresh_read_failure(canonical, &e, drop));
            if drop == claude_reset::RefreshDrop::Failed {
                surface.note(&claude_reset::refresh_drop_failure(canonical));
            }
            false
        }
    }
}

/// A codex account's stored login, as both codex calls send it. The
/// scheduler's side read captures one per candidate off the same refusals.
pub(crate) struct CodexLogin {
    pub(crate) access_token: String,
    pub(crate) account_id: Option<String>,
    pub(crate) fedramp: bool,
}

// Test-only count of `codex_preconditions` runs, so a pin can assert a tick
// whose accounts all sit inside their hour pays none of its login or
// quarantine reads. Thread-local: the tick's due loop runs on its caller's
// thread.
#[cfg(test)]
thread_local! {
    pub(crate) static CODEX_PRECONDITION_RUNS: std::cell::Cell<u64> =
        const { std::cell::Cell::new(0) };
}

/// The refusals every codex call runs before anything leaves the machine: a
/// quarantined chain, a profile with no stored codex login.
pub(crate) fn codex_preconditions(canonical: &str) -> Result<CodexLogin> {
    #[cfg(test)]
    CODEX_PRECONDITION_RUNS.with(|c| c.set(c.get() + 1));
    crate::codex_auth::refuse_if_quarantined(canonical)?;
    let auth = crate::codex_auth::read_store_auth(canonical);
    let Some(access_token) = auth.as_ref().and_then(|a| a.access_token()) else {
        anyhow::bail!(
            "'{canonical}' has no stored codex login to use a reset with; run `clauth login \
             {canonical} --codex --browser`"
        );
    };
    Ok(CodexLogin {
        access_token: access_token.to_string(),
        account_id: auth
            .as_ref()
            .and_then(|a| a.account_id())
            .map(str::to_string),
        fedramp: auth
            .as_ref()
            .is_some_and(crate::codex_auth::CodexAuth::is_fedramp),
    })
}

/// Read the account's credits raw. Spends nothing. Callers that print a line
/// map the error through [`codex_reset::list_failure`]; the refresh branches
/// on it.
fn read_codex(
    login: &CodexLogin,
    urls: &ResetUrls,
) -> Result<codex_reset::ResetCredits, codex_reset::ResetCallError> {
    codex_reset::list_reset_credits_at(
        &urls.list,
        &login.access_token,
        login.account_id.as_deref(),
        login.fedramp,
    )
}

/// What a [`land_codex`] landing did: the CLI ignores it, and the scheduler's
/// side read dates its next attempt off it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CodexLand {
    /// The stored account id matched the read's: the reading was written.
    Written,
    /// The store could not be read: nothing written, and the account keeps
    /// its hour.
    Unreadable,
    /// The store names another account now: nothing written, and the next
    /// pass reads under the new login.
    AccountMoved,
    /// The state flock could not be taken: nothing written, and the account
    /// keeps its hour.
    LockTimedOut,
    /// The gate admitted the reading and the cache write did not land: the
    /// account keeps its hour.
    NotStored,
}

/// Land a codex list read under the state flock, only while the store still
/// names the account it was read for: the TUI names each credit off this file,
/// and an account change must never show the old account's credits. Best
/// effort: the file is a display copy, and the spend decides on the read it
/// just made. Never called under the spend lock, a true leaf the state flock
/// ranks above. `timed_out` is a tick's one-wait flag: once any hold of that
/// tick has timed out, this one skips the flock rather than spend a second
/// bound waiting.
pub(crate) fn land_codex(
    canonical: &str,
    login: &CodexLogin,
    credits: &codex_reset::ResetCredits,
    timed_out: Option<&AtomicBool>,
) -> CodexLand {
    let name = ProfileName::from(canonical);
    let reading = codex_reset::credits_reading(credits);
    match crate::lock::with_state_lock_flag(timed_out, |_held| {
        let (account_id, unreadable) = usage::read_codex_for_compare(&name);
        if unreadable {
            return Ok(CodexLand::Unreadable);
        }
        if account_id != login.account_id {
            return Ok(CodexLand::AccountMoved);
        }
        if !crate::profile_cache::write_profile_cache_reported_for(
            crate::harness::Harness::Codex,
            &name,
            crate::profile_cache::LIMIT_RESETS_FILE,
            &reading,
        ) {
            return Ok(CodexLand::NotStored);
        }
        Ok(CodexLand::Written)
    }) {
        Ok(land) => land,
        Err(_) => CodexLand::LockTimedOut,
    }
}

/// The codex arm's listing: the read (landed for the TUI), then the table and,
/// while a fresh record waits, the note naming the command that re-sends it.
pub(crate) fn list_codex(canonical: &str, urls: &ResetUrls) -> Result<Vec<String>> {
    let login = codex_preconditions(canonical)?;
    let credits = read_codex(&login, urls)
        .map_err(|e| anyhow::anyhow!(codex_reset::list_failure(canonical, &e)))?;
    land_codex(canonical, &login, &credits, None);
    let mut lines = codex_reset::describe_reset_credits(canonical, &credits);
    let path = pending::path::<pending::CodexPending>(&ProfileName::from(canonical))?;
    if let Some(record) = pending::fresh_record::<pending::CodexPending>(&path) {
        lines.push(codex_reset::list_pending_note(
            canonical,
            &record.credit_id,
            record.sent_at_ms,
        ));
    }
    Ok(lines)
}

/// Spend the codex credit `credit_id` names, under the claude arm's rules: a
/// fresh record pins its own credit, a credit the list does not name or does
/// not read available is refused before the consume leaves, the consume is
/// sent once, and a reset re-reads the list so the stored credits follow.
pub(crate) fn spend_codex(
    canonical: &str,
    credit_id: &str,
    yes: bool,
    interactive: bool,
    urls: &ResetUrls,
    surface: &mut impl SpendSurface,
) -> Result<Spent> {
    let login = codex_preconditions(canonical)?;
    refuse_unconfirmed(canonical, yes, interactive)?;
    let profile = ProfileName::from(canonical);
    let spend_lock = pending::LimitResetLock::acquire(&profile, |line| surface.note(line))?;
    let pending_path = pending::path::<pending::CodexPending>(&profile)?;
    let recorded = pending::take_fresh::<pending::CodexPending>(&pending_path)?;
    if let Some(record) = &recorded
        && record.credit_id != credit_id
    {
        anyhow::bail!(
            "{}",
            codex_reset::pinned_record_line(canonical, &record.credit_id, record.sent_at_ms)
        );
    }
    let credits = read_codex(&login, urls)
        .map_err(|e| anyhow::anyhow!(codex_reset::list_failure(canonical, &e)))?;
    let re_send = recorded.is_some();
    let (redeem_request_id, sent_at_ms) = match recorded {
        Some(record) => {
            if !yes && !surface.confirm(&codex_reset::re_send_prompt(canonical))? {
                return Ok(Spent::Declined(codex_reset::declined_re_send_line(
                    canonical,
                )));
            }
            (record.redeem_request_id, record.sent_at_ms)
        }
        None => {
            let credit = match credits.try_spend_credit_id(credit_id) {
                Ok(credit) => credit,
                Err(codex_reset::CreditRefusal::NotListed) if credits.credits.is_empty() => {
                    anyhow::bail!("{}", codex_reset::no_resets_available(canonical))
                }
                Err(codex_reset::CreditRefusal::NotListed) => {
                    anyhow::bail!("{}", codex_reset::not_listed_line(canonical, credit_id))
                }
                Err(codex_reset::CreditRefusal::NotAvailable(credit)) => {
                    anyhow::bail!(
                        "{}",
                        codex_reset::unavailable_credit_line(canonical, credit)
                    )
                }
            };
            if !yes
                && !surface.confirm(&codex_reset::limit_reset_prompt(
                    canonical, &credits, credit,
                ))?
            {
                return Ok(Spent::Declined(format!(
                    "clauth: aborted. no reset was used on '{canonical}'."
                )));
            }
            let redeem_request_id = codex_reset::new_redeem_request_id()?;
            let sent_at_ms = usage::now_ms();
            pending::write(
                &pending_path,
                &pending::CodexPending {
                    credit_id: credit.id.clone(),
                    redeem_request_id: redeem_request_id.clone(),
                    sent_at_ms,
                },
            )?;
            (redeem_request_id, sent_at_ms)
        }
    };
    let this_send_at_ms = usage::now_ms();
    let reply = codex_reset::consume_reset_credit_at(
        &urls.consume,
        &login.access_token,
        login.account_id.as_deref(),
        login.fedramp,
        &redeem_request_id,
        credit_id,
    );
    let settled = codex_reset::post_settles(re_send, &reply, codex_reset::consume_settled);
    if settled && let Err(e) = pending::clear(&pending_path) {
        surface.note(&format!(
            "clauth: could not remove {} after the reset settled: {e}",
            pending_path.display()
        ));
    }
    drop(spend_lock);
    let reply = reply.map_err(|e| {
        anyhow::anyhow!(codex_reset::consume_failure(
            canonical, &e, sent_at_ms, re_send
        ))
    })?;
    if let Some(reason) = reply.refusal_reason() {
        surface.refused(&reason);
    }
    // A `no_credit` answer is the one refusal that still moved the count: the
    // list names the credit but it no longer reads available, so the stored
    // copy is re-read before the refusal leaves — the claude arm's rule for an
    // already-used reply. A `nothing_to_reset` answer spent nothing, so its
    // list stays as it was and needs no re-read.
    if reply.outcome() == codex_reset::ConsumeOutcome::NoCredit {
        let reread_stored = refresh_codex(canonical, &login, urls, this_send_at_ms, surface);
        surface.moved_count(reread_stored);
    }
    let summary = codex_reset::outcome_line(canonical, &credits, &reply, sent_at_ms)
        .map_err(|e| anyhow::anyhow!(e))?;
    surface.outcome(&Ok(summary));
    let reread_stored = refresh_codex(canonical, &login, urls, this_send_at_ms, surface);
    // The re-read's landing decides whether a surface may rebuild its rows
    // from the stored copy.
    surface.moved_count(reread_stored);
    Ok(Spent::Sent)
}

/// The codex post-spend refresh: the list landed before the consume still
/// names the used credit available, so the spend re-reads it, and a failed
/// re-read drops that copy (under the same account gate the landing uses,
/// and never while a stored list is stamped at or after this run's send)
/// rather than keep naming a spent credit ready.
///
/// Returns whether this run's re-read stored its reading: `false` on every
/// gate that kept it off, on a write that did not land, and on the failed
/// re-read that drops the copy.
fn refresh_codex(
    canonical: &str,
    login: &CodexLogin,
    urls: &ResetUrls,
    this_send_at_ms: u64,
    surface: &mut impl SpendSurface,
) -> bool {
    let err = match read_codex(login, urls) {
        Ok(credits) => return land_codex(canonical, login, &credits, None) == CodexLand::Written,
        Err(err) => err,
    };
    let name = ProfileName::from(canonical);
    // The drop's outcome, decided under the state flock so the line prints
    // only what happened, the claude arm's shape.
    let drop = match crate::lock::with_state_lock(|_held| {
        let (account_id, unreadable) = usage::read_codex_for_compare(&name);
        if unreadable || account_id != login.account_id {
            return Ok(claude_reset::RefreshDrop::LoginChanged);
        }
        if stored_reading_is_newer(&name, this_send_at_ms) {
            return Ok(claude_reset::RefreshDrop::KeptNewer);
        }
        Ok(
            if crate::profile_cache::remove_profile_cache_reported(
                &name,
                crate::profile_cache::LIMIT_RESETS_FILE,
            ) {
                claude_reset::RefreshDrop::Dropped
            } else {
                claude_reset::RefreshDrop::Failed
            },
        )
    }) {
        Ok(drop) => drop,
        Err(_) => claude_reset::RefreshDrop::LockTimedOut,
    };
    surface.note(&codex_reset::refresh_read_failure(canonical, &err, drop));
    if drop == claude_reset::RefreshDrop::Failed {
        surface.note(&codex_reset::refresh_drop_failure(canonical));
    }
    false
}
