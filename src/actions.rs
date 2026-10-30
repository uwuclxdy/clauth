//! Pure-data mutations against `AppConfig` and the live `~/.claude` state.
//!
//! Each function takes already-validated inputs from the TUI layer and applies
//! the change under the cross-process state lock.

use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;

use anyhow::{Context, Result, bail};

use crate::claude::{
    ClaudeEndpoint, apply_profile_to_claude_settings, clear_claude_credentials,
    force_link_profile_credentials, force_snapshot_active_credentials, link_profile_credentials,
    live_diverged_and_unsaved, managed_env_key_label, read_claude_credentials,
    read_claude_endpoint_config, snapshot_active_credentials,
};
use crate::harness::Harness;
use crate::lock::{StateLockHeld, StateLockTimeout, with_state_lock};
use crate::lockorder::RankedMutex;
use crate::oauth;
use crate::out::{out, outln};
use crate::profile::{
    AccountId, AppConfig, ClaudeCredentials, ConfigHandle, ConsoleCredential, DivergenceChoice,
    ModelSettings, Profile, ProfileId, ProfileName, StoredEndpointIdentity, load_app_state,
    load_profile, profile_dir, require_profile_id, save_app_state, save_new_profile, save_profile,
    save_profile_config,
};
use crate::providers::Provider;
use crate::runtime::RotationGuard;
use crate::spinner::Spinner;
use crate::usage::{
    ForgetPlan, Half, OAuthIdentity, PlanInfo, SpendMarker, endpoint_identity_plan, forget_disk,
    forget_disk_plan, names_another_account, note_forget, note_forget_plan, note_rename,
    oauth_changed, read_before_identities, read_codex_identity, third_party_identity,
    throughput_for_endpoint, throughput_identity,
};

/// One character a name may hold: ASCII alphanumeric or `-_.@+`. `@`/`+` let
/// an account be named after its email; both are path-separator-free so the
/// name stays a single `profiles/<name>` segment with no traversal.
pub(crate) fn is_name_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '@' | '+')
}

/// Every char an [`is_name_char`], not leading-dot, not empty. The charset
/// half of [`validate_profile_name`], standing alone for names that live in a
/// namespace of their own (the preset store), where neither roster has a say.
/// Returns the trimmed name the checks ran against.
pub(crate) fn validate_name_chars(name: &str) -> Result<&str> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        bail!("name cannot be empty");
    }
    let valid_chars = trimmed.chars().all(is_name_char);
    if !valid_chars || trimmed.starts_with('.') {
        bail!("name: letters, digits and - _ . @ + only, and can't start with '.'");
    }
    Ok(trimmed)
}

/// Refuse a name the OTHER harness's roster holds, naming the holder. Profile
/// names are one namespace across both state files — `profiles/<name>/` is one
/// dir set, and every name-keyed subsystem (the live tally, the pending-switch
/// set, the per-profile caches) carries one key per name. The half of
/// [`validate_profile_name`] a creation flow can take alone when it
/// deliberately tolerates an own-roster collision (the capture-name prompt
/// routes that case into capture-into-existing) but must still refuse to
/// shadow the other harness, which no flow can adopt across.
pub(crate) fn validate_foreign_harness_free(name: &str, harness: Harness) -> Result<()> {
    let foreign = match harness {
        Harness::Claude => Harness::Codex,
        Harness::Codex => Harness::Claude,
    };
    let held = match foreign {
        Harness::Claude => crate::profile::claude_roster_names()?
            .iter()
            .any(|n| n.eq_ignore_ascii_case(name)),
        Harness::Codex => crate::codex_profiles::CodexState::load()?
            .profiles()
            .iter()
            .any(|n| n.eq_ignore_ascii_case(name)),
    };
    if held {
        bail!(
            "'{name}' is a {foreign} profile. Profile names span both harnesses, so pick another"
        );
    }
    Ok(())
}

/// The full gate for creating or renaming a profile on `harness`: charset,
/// then the other harness's roster (refused by name), then this harness's own
/// duplicate check (`exclude` exempts the current name for rename-in-place).
///
/// Reads both rosters itself rather than trusting a caller-supplied list: the
/// cross-harness half must run at every creation site, and a caller curating
/// its own `existing` slice would silently skip it. The reads are two small
/// TOML stats on an interactive path, never a per-frame one.
pub(crate) fn validate_profile_name(
    name: &str,
    harness: Harness,
    exclude: Option<&str>,
) -> Result<()> {
    let trimmed = validate_name_chars(name)?;
    validate_foreign_harness_free(trimmed, harness)?;
    let own: Vec<String> = match harness {
        Harness::Claude => crate::profile::claude_roster_names()?
            .iter()
            .map(|n| n.as_str().to_string())
            .collect(),
        Harness::Codex => crate::codex_profiles::CodexState::load()?
            .profiles()
            .iter()
            .map(|n| n.as_str().to_string())
            .collect(),
    };
    if own
        .iter()
        .any(|n| n.eq_ignore_ascii_case(trimmed) && Some(n.as_str()) != exclude)
    {
        bail!("a profile named '{trimmed}' already exists");
    }
    Ok(())
}

/// Every switch primitive tears the live credentials link down before
/// `finish_switch` would notice a ghost, and the discard path takes no prior
/// snapshot — an uncaptured re-login would be gone for good. So this runs
/// FIRST, before any side effect: a caller holding a stale name (a queued
/// auto-switch target, the MCP switch tool with a divergence default) bounces
/// off instead of stranding the machine half-switched with the live link
/// destroyed, and a disabled target is refused before that same link gets
/// force-relinked to it.
///
/// This is the ONE authoritative "never active while disabled" gate — every
/// switch primitive that can write `active_profile`
/// ([`switch_profile`]/[`switch_profile_discard`]/[`switch_profile_reconciled`],
/// and so [`switch_profile_noninteractive`] and `switch_profile_cli`, which
/// only ever reach `active_profile` through one of those three) calls this
/// as its first line, inside the same `with_state_lock` closure that runs
/// the write at the end. The lock is held continuously from here to that
/// write, so a concurrent `disable_profile` can't land in the gap — a
/// pre-lock check in a CLI/MCP wrapper is a friendly early error at best,
/// never the authoritative one.
/// An authored refusal raised by a deep leg rather than one of
/// [`switch_profile_noninteractive`]'s own arms: the same closed diagnostic set
/// (condition + fix, never a path), lifted out of the open anyhow chain so a
/// remote surface can reflect it. Carried through anyhow's chain by the legs,
/// so it reaches a caller as the head line — the CLI prints the sentence, the
/// MCP tool's `reason` holds it, byte-identical to the old `bail!` head.
#[derive(Debug)]
pub(crate) struct DeepRefusal(pub(crate) String);

impl std::fmt::Display for DeepRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for DeepRefusal {}

/// The closed set of refusals a chain-edit action can raise. An enum rather
/// than a wire string so the route's match is exhaustive: a fifth refusal added
/// here does not compile until its answer arm exists, instead of silently
/// folding into the router's 500.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ChainRefusal {
    OrderInvalid,
    ProfileNotFound,
    NotAMember,
    BadRequest,
}

impl ChainRefusal {
    /// The fixed wire code the route answers with for this refusal.
    pub(crate) const fn code(self) -> &'static str {
        match self {
            ChainRefusal::OrderInvalid => "chain_order_invalid",
            ChainRefusal::ProfileNotFound => "profile_not_found",
            ChainRefusal::NotAMember => "not_a_member",
            ChainRefusal::BadRequest => "bad_request",
        }
    }
}

/// An authored refusal raised by a chain-edit action, carrying the fixed error
/// code the route answers with and, for the refusals that name a profile, the
/// sentence. Kept downcastable so the route maps validation failures to their
/// own codes instead of folding them into the open anyhow chain — the body is
/// the surface a remote reader sees, so the open chain never leaves the log.
#[derive(Debug)]
pub(crate) struct ChainEditRefusal {
    pub(crate) code: ChainRefusal,
    pub(crate) reason: Option<String>,
}

impl ChainEditRefusal {
    pub(crate) fn new(code: ChainRefusal, reason: Option<String>) -> Self {
        Self { code, reason }
    }
}

impl std::fmt::Display for ChainEditRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.reason {
            Some(reason) => f.write_str(reason),
            None => f.write_str(self.code.code()),
        }
    }
}

impl std::error::Error for ChainEditRefusal {}

fn ensure_switch_target_ok(config: &AppConfig, name: &ProfileName) -> Result<()> {
    // Fresh membership, not just the in-memory list: a caller can hold a config
    // older than a concurrent CLI delete/rename (the daemon reloads once a
    // tick). This must run FIRST — `switch_profile` then calls
    // `force_link_profile_credentials`, which tears the live slot down before
    // `finish_switch` could notice the ghost — so a vanished target bounces
    // here, before any side effect. Runs under the state flock, which makes the
    // on-disk read stable.
    if !crate::profile::is_configured(name)? {
        bail!(DeepRefusal(format!("profile '{name}' not found")));
    }
    let Some(profile) = config.find(name) else {
        bail!(DeepRefusal(format!("profile '{name}' not found")));
    };
    if profile.is_disabled() {
        bail!(DeepRefusal(format!(
            "'{name}': account is disabled, run `clauth enable {name}`"
        )));
    }
    Ok(())
}

/// Switch to `name`: relink the live credentials, then republish the feed.
///
/// Takes the shared [`crate::profile::ConfigHandle`]: the config guard is
/// acquired FIRST and held across the state flock (the order
/// [`crate::lockorder`] ranks them), and released before the republish below —
/// the reverse order (a republish under the config mutex) is what the round-2
/// review flagged: [`crate::daemon::publish_status`] stats and reads every
/// profile's cache under it.
///
/// The no-op switch (already active) republishes nothing: the feed on disk
/// already names this account.
pub(crate) fn switch_profile(config: &ConfigHandle, name: &ProfileName) -> Result<()> {
    switch_profile_synced(config, name, || {})
}

/// The injected closure runs after the switch has persisted and released both
/// Config and State, between the status body's construction and its commit —
/// the window a competing publisher can land in. Production passes a no-op;
/// the regression tests use it to order two real wrappers.
fn switch_profile_synced(
    config: &ConfigHandle,
    name: &ProfileName,
    before_commit: impl FnOnce(),
) -> Result<()> {
    #[allow(
        clippy::expect_used,
        reason = "config mutex poisoning is unrecoverable"
    )]
    let mut guard = config.lock().expect("config mutex poisoned");
    let changed = switch_profile_locked(&mut guard, name)?;
    drop(guard);
    strip_stale_identity_after_switch(changed);
    if changed {
        crate::daemon::publish_status_with(config, before_commit);
    }
    Ok(())
}

/// issue #17: after a switch that moved the active profile (or switched off),
/// drop the outgoing account's cached identity so Claude Code re-derives it
/// from the relinked credentials. Runs AFTER the state flock, gated on `changed`
/// so only a switch that actually proceeded strips: the strip try-onces the
/// `.claude.json` sync lock, ranked OUTSIDE `State`, so it must never run under
/// the flock, and a strip that cannot take the lock is deferred to the next
/// reconcile rather than failing the switch. A no-op switch (already active)
/// never reaches this.
fn strip_stale_identity_after_switch(changed: bool) {
    if changed {
        crate::claude_json::strip_home_oauth_account();
    }
}

/// [`switch_profile`]'s locked body: the caller holds the config guard and
/// receives the did-the-active-move answer so it can gate its own republish.
/// Guard acquired before the flock — see the wrapper. The daemon's tick drain
/// and `fallback::auto_switch_if_needed` are the cross-module callers: each
/// takes the guard first and holds it across this fn's flock (the fallback so
/// its decision and dispatch share one state hold), keeping the config guard
/// outer, the ranked order.
pub(crate) fn switch_profile_locked(config: &mut AppConfig, name: &ProfileName) -> Result<bool> {
    with_state_lock(|held| {
        ensure_switch_target_ok(config, name)?;
        if config.is_active(name) {
            return Ok(false);
        }
        // Is the outgoing live file an UNCAPTURED CC re-login? `snapshot_active_
        // credentials` deliberately skips capturing that case (Diverged & not a
        // first-login), so dropping it would strand a fresh `/login` chain — keep
        // the non-force refuse-guard there. Every other state is captured or
        // adoptable by the snapshot below, so force the relink: on macOS the live
        // `.credentials.json` is a regular-file Keychain mirror of the active
        // account, so it legitimately differs from the target, which the non-force
        // guard's live-vs-target byte check would wrongly reject. The SAME
        // predicate the defer/banner gates use — `live_diverged_and_unsaved` —
        // decides here, so a login already saved in the store (the mirror, a
        // clauth symlink) forces the relink even once a sidecar capture flips the
        // install source and makes classify read Diverged over it; without that
        // exemption the guarded link byte-rejects the macOS mirror and the switch
        // fails "unsaved credentials" though nothing is unsaved. (Interactive
        // callers already route a real divergence to the reconcile path, so this
        // branch is only reachable uncaptured via the scheduler — where refusing,
        // not dropping, is the safe outcome.) A logged-out shell holds no login to
        // strand, so it too forfeits the refuse-guard.
        let uncaptured_relogin = match config.state.active_profile.as_ref() {
            Some(active) => live_diverged_and_unsaved(active)?,
            None => false,
        };
        snapshot_active_credentials(config)?;
        // Through the credential-install seam — this chokepoint is where a
        // future harness's install would dispatch; the sibling switch flavors
        // below keep their direct calls (claude-only by construction).
        let engine: &dyn crate::harness::HarnessEngine = &crate::harness::ClaudeEngine;
        if uncaptured_relogin {
            engine.install_credentials(name)?;
        } else {
            engine.force_install_credentials(name)?;
        }
        finish_switch(config, name, held)?;
        Ok(true)
    })
}

/// Discard the live login: force-relink to `target`'s stored creds WITHOUT
/// capturing the foreign live file into any profile. Bypasses the non-force
/// `link_profile_credentials` refuse-guard (which exists to protect an
/// un-captured re-login) precisely because the caller chose to drop it.
///
/// Same lock shape as [`switch_profile`]: guard first, dropped before the
/// gated republish.
pub(crate) fn switch_profile_discard(config: &ConfigHandle, target: &ProfileName) -> Result<()> {
    #[allow(
        clippy::expect_used,
        reason = "config mutex poisoning is unrecoverable"
    )]
    let mut guard = config.lock().expect("config mutex poisoned");
    let changed = with_state_lock(|held| {
        let config = &mut *guard;
        ensure_switch_target_ok(config, target)?;
        if config.is_active(target) {
            return Ok(false);
        }
        force_link_profile_credentials(target)?;
        finish_switch(config, target, held)?;
        Ok(true)
    })?;
    drop(guard);
    strip_stale_identity_after_switch(changed);
    if changed {
        crate::daemon::publish_status(config);
    }
    Ok(())
}

/// Force-snapshot the outgoing creds then force the symlink. CLI prompt path only.
///
/// Same lock shape as [`switch_profile`]: guard first, dropped before the
/// gated republish.
pub(crate) fn switch_profile_reconciled(config: &ConfigHandle, name: &ProfileName) -> Result<()> {
    #[allow(
        clippy::expect_used,
        reason = "config mutex poisoning is unrecoverable"
    )]
    let mut guard = config.lock().expect("config mutex poisoned");
    let (changed, outgoing, oauth_changed) = with_state_lock(|held| {
        let config = &mut *guard;
        ensure_switch_target_ok(config, name)?;
        if config.is_active(name) {
            return Ok((false, None, false));
        }
        let outgoing = config.state.active_profile.as_ref().cloned();
        let oauth_changed = force_snapshot_active_credentials(config)?;
        force_link_profile_credentials(name)?;
        finish_switch(config, name, held)?;
        Ok((true, outgoing, oauth_changed))
    })?;
    drop(guard);
    strip_stale_identity_after_switch(changed);
    if changed {
        // The reconcile captured a live login that may be a DIFFERENT account:
        // expire the outgoing profile's `/profile` clock (ranked below the flock,
        // so after the hold), the way `overwrite_captured_profile` does.
        if oauth_changed && let Some(out) = outgoing {
            crate::usage::expire_profile_ttl(&out);
        }
        crate::daemon::publish_status(config);
    }
    Ok(())
}

/// macOS, CLI: what a command does with its entry's
/// [`crate::claude::settle_bare_keychain_item`] before it writes the bare
/// item. A settled item: say what the settle did on stderr (its carry has
/// landed), then hand its verdict to the writer guards the command runs
/// while the returned placement lives. An item the settle cannot place: ask
/// on a terminal ([`ask_bare_item_choice`]), else refuse with a `clauth:`
/// line. A save lands in `config` and the settle runs again; a save whose
/// owner's store kept a later login lets the write go on over the item.
#[cfg(target_os = "macos")]
pub(crate) fn bare_item_placement_for_cli(
    config: &mut AppConfig,
    writer: crate::claude::BareItemWriter,
    settle: crate::claude::BareItemSettle,
) -> Result<crate::claude::BareItemPlacement> {
    use crate::claude::{BareItemPlacement, BareItemSettle};
    use std::io::IsTerminal as _;

    let mut settle = settle;
    loop {
        let unplaced = match settle {
            BareItemSettle::Settled(settled) => {
                if let Some(notice) = &settled.notice {
                    crate::out::errln!("clauth: {notice}");
                }
                return Ok(BareItemPlacement::enter(settled.placed));
            }
            BareItemSettle::NeedsChoice(unplaced) => unplaced,
        };
        if !(std::io::stdin().is_terminal() && std::io::stderr().is_terminal()) {
            crate::out::errln!("{}", bare_item_refusal_line(&unplaced.refusal(writer)));
            return Err(anyhow::Error::new(crate::FailureReported));
        }
        match ask_bare_item_choice(&unplaced, writer)? {
            BareItemChoice::Save => {
                if let Some(placed) =
                    save_bare_keychain_item(config, writer).map_err(report_failed_cli_settle)?
                {
                    return Ok(BareItemPlacement::enter(Some(placed)));
                }
            }
            BareItemChoice::Replace => {
                let placed = crate::claude::read_bare_keychain_item()
                    .as_ref()
                    .and_then(crate::claude::bare_item_replacement);
                return Ok(BareItemPlacement::enter(placed));
            }
            BareItemChoice::Cancel => {
                crate::out::errln!("clauth: aborted, no changes made");
                return Err(anyhow::Error::new(crate::FailureReported));
            }
        }
        settle =
            crate::claude::settle_bare_keychain_item(writer).map_err(report_failed_cli_settle)?;
    }
}

/// macOS, CLI: a failed [`crate::claude::settle_bare_keychain_item`] or
/// prompt save, reported on stderr like the refusal of an item it cannot
/// place. A failure the prompt already reported passes through unprinted.
#[cfg(target_os = "macos")]
pub(crate) fn report_failed_cli_settle(e: anyhow::Error) -> anyhow::Error {
    if e.is::<crate::FailureReported>() {
        return e;
    }
    crate::out::errln!("{}", bare_item_refusal_line(&e));
    anyhow::Error::new(crate::FailureReported)
}

/// The CLI's stderr line for a bare-item settle that refused or failed.
#[cfg_attr(
    not(target_os = "macos"),
    allow(
        dead_code,
        reason = "the callers are the macOS CLI settles; the line is pinned on every platform"
    )
)]
pub(crate) fn bare_item_refusal_line(e: &anyhow::Error) -> String {
    format!("clauth: {e:#}")
}

/// What the user chose for a bare-item login no store saved.
#[cfg(target_os = "macos")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BareItemChoice {
    Save,
    Replace,
    Cancel,
}

/// The answer a line typed at [`ask_bare_item_choice`] names, if any.
#[cfg(target_os = "macos")]
fn parse_bare_item_choice(line: &str) -> Option<BareItemChoice> {
    match line.trim().to_ascii_lowercase().as_str() {
        "s" | "save" => Some(BareItemChoice::Save),
        "p" | "replace" => Some(BareItemChoice::Replace),
        "c" | "cancel" => Some(BareItemChoice::Cancel),
        _ => None,
    }
}

/// Ask on stderr what to do with the unplaced login, reading one line from
/// stdin: a miss asks once more, a second miss or a closed stdin cancels.
#[cfg(target_os = "macos")]
fn ask_bare_item_choice(
    unplaced: &crate::claude::BareItemUnplaced,
    writer: crate::claude::BareItemWriter,
) -> Result<BareItemChoice> {
    let action = writer.action();
    crate::out::errln!("clauth: {}", unplaced.question());
    crate::out::errln!(
        "  s  save     save it as an account (asks for a name if none matches), then {action}"
    );
    crate::out::errln!(
        "  p  replace  {action} anyway; that account will need a browser login again"
    );
    crate::out::errln!("  c  cancel   leave everything as it is");
    if let Some(tip) = unplaced.tip() {
        crate::out::errln!("  {tip}");
    }
    for _ in 0..2 {
        crate::out::err!("choose [s/p/c]: ");
        let mut line = String::new();
        if std::io::stdin().read_line(&mut line)? == 0 {
            break;
        }
        if let Some(choice) = parse_bare_item_choice(&line) {
            return Ok(choice);
        }
    }
    Ok(BareItemChoice::Cancel)
}

/// The prompt's save: the bare item's unsaved login goes into the profile
/// already cached with its account (the one the user names, when two are),
/// else into a new profile named at a second prompt and created like
/// `clauth capture`. A name that is neither of the two cancels. The key the
/// writer may replace the item by when the owner's store kept a later login
/// ([`crate::claude::save_bare_item_into`]).
#[cfg(target_os = "macos")]
fn save_bare_keychain_item(
    config: &mut AppConfig,
    writer: crate::claude::BareItemWriter,
) -> Result<Option<[u8; 32]>> {
    use crate::claude::BareItemSave;

    let Some(item) = crate::claude::read_bare_keychain_item() else {
        return Ok(None);
    };
    match crate::claude::bare_item_save(&item) {
        BareItemSave::Nothing => Ok(None),
        BareItemSave::Owner(owner) => {
            let account = crate::claude::claim_bare_item_owner(&owner)?;
            crate::claude::save_bare_item_into(&owner, &account, &item, writer)
        }
        BareItemSave::Owners(first, second) => {
            // Claimed before the question: the pick lands only on the account
            // each name held when it was asked about.
            let accounts = [
                crate::claude::claim_bare_item_owner(&first)?,
                crate::claude::claim_bare_item_owner(&second)?,
            ];
            crate::out::err!("save it into which profile? [{first}/{second}]: ");
            let mut line = String::new();
            std::io::stdin().read_line(&mut line)?;
            match pick_bare_item_owner(&line, [&first, &second]) {
                Some(owner) => {
                    let account = if owner == &first {
                        &accounts[0]
                    } else {
                        &accounts[1]
                    };
                    crate::claude::save_bare_item_into(owner, account, &item, writer)
                }
                None => {
                    crate::out::errln!("clauth: aborted, no changes made");
                    Err(anyhow::Error::new(crate::FailureReported))
                }
            }
        }
        BareItemSave::NewProfile { login, account } => {
            crate::out::err!("name for the new profile: ");
            let mut line = String::new();
            std::io::stdin().read_line(&mut line)?;
            save_bare_item_as_profile_fresh(config, &line, login, account).map(|()| None)
        }
    }
}

/// The CLI prompt's [`save_bare_item_as_profile`], on config read after the
/// name was typed: the caller's config predates its own prompts, and the
/// save writes profiles.toml from the config it is given, which would revert
/// a profile another clauth created or a switch the daemon made meanwhile.
/// `config` becomes the config the save landed on.
#[cfg_attr(
    not(target_os = "macos"),
    allow(
        dead_code,
        reason = "the caller is the macOS prompt; pinned on every platform"
    )
)]
pub(crate) fn save_bare_item_as_profile_fresh(
    config: &mut AppConfig,
    name: &str,
    login: ClaudeCredentials,
    account: Option<crate::profile::AccountId>,
) -> Result<()> {
    let mut fresh = crate::profile::load_config()?;
    save_bare_item_as_profile(&mut fresh, name, login, account)?;
    *config = fresh;
    Ok(())
}

/// The one of `owners` a line typed at the prompt's `which profile?` names,
/// if any.
#[cfg_attr(
    not(target_os = "macos"),
    allow(
        dead_code,
        reason = "the caller is the macOS prompt; pinned on every platform"
    )
)]
pub(crate) fn pick_bare_item_owner<'a>(
    line: &str,
    owners: [&'a ProfileName; 2],
) -> Option<&'a ProfileName> {
    let line = line.trim();
    owners.into_iter().find(|owner| owner.as_str() == line)
}

/// A bare-item save's [`crate::claude::BareItemSave::NewProfile`]: a new
/// profile named `name` takes the login, created like `clauth capture`, and
/// anchored to the account the lookup proved for it, if any. The CLI prompt
/// and the TUI modal both create through here.
pub(crate) fn save_bare_item_as_profile(
    config: &mut AppConfig,
    name: &str,
    login: ClaudeCredentials,
    account: Option<crate::profile::AccountId>,
) -> Result<()> {
    let name = validate_capture_name(config, name)?.to_string();
    let saving = || format!("saving the macOS Keychain login into {name}");
    let mut snapshot = snapshot_of_login(Some(login)).with_context(saving)?;
    snapshot.anchor = account.map_or(AnchorAction::Unproven, AnchorAction::Proven);
    capture_into_profile(config, name.clone(), Default::default(), snapshot).with_context(saving)
}

/// CLI switch: relink (reconciling diverged live file via `[Y/n]` prompt), then
/// prime the 5h window. No token rotation — stale chains rotate lazily on first use.
pub(crate) fn switch_profile_cli(config: AppConfig, canonical: &ProfileName) -> Result<()> {
    #[cfg(target_os = "macos")]
    let mut config = config;
    let outgoing = config.state.active_profile.as_ref().cloned();

    // Diverged link = CC re-logged and wrote a regular file; must reconcile
    // (capture into outgoing profile) rather than refuse. A logged-out shell is
    // exempt: capturing its blank tokens would destroy the outgoing profile's
    // stored login.
    let reconciled = match outgoing.as_ref() {
        Some(active) => live_diverged_and_unsaved(active)?,
        None => false,
    };

    // Asked before anything below writes a store, so a no leaves none
    // changed.
    if reconciled {
        let active = outgoing.as_deref().unwrap_or("");
        out!(
            "clauth: '{active}' has a newer login in ~/.claude. save it into '{active}' \
             and switch to '{canonical}'? [Y/n] "
        );
        let mut answer = String::new();
        std::io::stdin().read_line(&mut answer)?;
        let answer = answer.trim().to_ascii_lowercase();
        if !(answer.is_empty() || answer == "y" || answer == "yes") {
            outln!("clauth: aborted, no changes made");
            return Ok(());
        }
    }

    // Above the AUTH-1 gate: when the target owns the item's login, the gate
    // would refresh the target's consumed predecessor and quarantine it,
    // while the settle saves the live successor into it first.
    #[cfg(target_os = "macos")]
    let _placed = if outgoing.as_deref() == Some(canonical) {
        None
    } else {
        let writer = crate::claude::BareItemWriter::Switch;
        let settle =
            crate::claude::settle_bare_keychain_item(writer).map_err(report_failed_cli_settle)?;
        Some(bare_item_placement_for_cli(&mut config, writer, settle)?)
    };

    let config = Arc::new(RankedMutex::new(config));

    // AUTH-1 (Incident C): gate the target before its credentials land in the
    // Keychain (which re-authenticates every running `claude` on this machine).
    // Refusal + `clauth login` hint pinned by
    // `switch_cli_refuses_dead_target_with_login_hint`.
    // The already-active profile is exempt: there is nothing new to install
    // (`switch_profile` no-ops on `is_active`), and its chain is the one a
    // plain `claude` may be refreshing through the symlink right now — gating
    // it can lose that race and false-quarantine a healthy login.
    if outgoing.as_deref() != Some(canonical) {
        match oauth::ensure_installable(&config, canonical, oauth::refresh_result, None) {
            oauth::AuthGate::Ready | oauth::AuthGate::Refreshed => {}
            oauth::AuthGate::Broken => bail!("{}", crate::format::login_expired(canonical).line()),
            // CLI stderr: name the HTTP status too. This lands on `main.rs`'s
            // `errln!` backstop, a terminal with no companion log open, so the
            // status is the one wire fact the operator has nowhere else to read.
            oauth::AuthGate::Transient(e) => {
                bail!(
                    "{}",
                    crate::format::refresh_transient_cli(canonical, &e).line()
                )
            }
            // Unreachable: the CLI passes `None` as the spend marker.
            oauth::AuthGate::Stopping => bail!("{}", crate::format::DAEMON_STOPPING),
        }
    }

    if reconciled {
        switch_profile_reconciled(&config, canonical)?;
    } else {
        switch_profile(&config, canonical)?;
    }

    // Prime the 5h window if opted in. Kicks with the current access token and
    // rotates once on a 401/429. One-shot — the CLI has no scheduler tick to
    // re-arm against, so no side channels.
    {
        let _spinner = Spinner::start("clauth: priming usage window");
        let _ = oauth::prime_window(&config, canonical);
    }
    outln!("clauth: switched to '{canonical}'");
    Ok(())
}

/// Headless switch for the MCP `switch` tool: relink the global active profile
/// to `target` without prompting and without priming the 5h window (zero quota;
/// the profile primes its own window when a session next uses it).
///
/// On credential divergence (the active link is a regular file CC re-logged into)
/// the caller-supplied `on_divergence` decides: `Overwrite` captures the live
/// tokens into the outgoing profile then relinks ([`switch_profile_reconciled`]),
/// `Discard` drops the foreign live login and force-relinks `target`'s stored
/// tokens without capturing it into any profile ([`switch_profile_discard`]),
/// `NewProfile` is interactive-only (would need a name prompt) so it errors, and
/// `None` means no default is set so it errors. A non-diverged link
/// (`LinkedTo`/`Missing`) always takes the plain [`switch_profile`].
///
/// Returns `(previous_active, new_active, notice)`: `notice` says what the
/// macOS bare-item settle saved before the switch, for the caller's reply.
///
/// Accepted TOCTOU: the divergence classify runs before the locked relink (same
/// shape as the CLI path); a live change in that gap self-heals on the next switch.
///
/// Takes the shared [`crate::profile::ConfigHandle`] (not `&mut AppConfig`)
/// because the AUTH-1 gate below may refresh over HTTP, which must never run
/// under the config mutex. `refresher` is injected so the gate is testable
/// offline (production callers pass [`oauth::refresh_result`]).
/// Why a headless switch ([`switch_profile_noninteractive`]) failed, split so
/// each caller can reflect only what its surface may show.
///
/// [`SwitchError::Refused`] carries an authored sentence — the `bail!` arms
/// and the `format::Message` renders below, the closed diagnostic set every
/// clauth surface already spells the same way. A reflectable refusal: it
/// names the condition and the fix, never a path.
///
/// [`SwitchError::Deferred`] is a refusal that names no failure: the gate's
/// refresh met a login change whose kept login is expiring, so the switch waits
/// for a retry. Same reflection rules as a refusal; surfaces that frame a
/// refusal as a failure show its sentence alone.
///
/// [`SwitchError::Failed`] carries the open anyhow chain (the IO arms and
/// path-bearing contexts under `finish_switch` and the link/snapshot
/// helpers). A chain like that names absolute paths under the operator's
/// home, so only local surfaces may read it: the MCP tool (stdio to the
/// operator's own machine) via the plain Display, the daemon's own
/// `daemon.log` via the alternate `{:#}` form. An HTTP body reflects none of
/// the chain itself — the route reflects only the fixed literal, the
/// path-free `StateLockTimeout` Display, and the closed-set `DeepRefusal`.
#[derive(Debug)]
pub(crate) enum SwitchError {
    Refused(String),
    Deferred(String),
    Failed(anyhow::Error),
    /// The daemon is stopping: the AUTH-1 gate refused a refresh under the
    /// rotation lock, so no request went out. Reflected as a 503.
    Stopping,
}

impl SwitchError {
    /// The retryable condition inside a [`Failed`] chain, if any: contention
    /// on the state flock can be raised anywhere down the switch, so it is
    /// asked of the chain rather than caught at one site.
    pub(crate) fn state_lock_timeout(&self) -> Option<&StateLockTimeout> {
        match self {
            Self::Refused(_) | Self::Deferred(_) | Self::Stopping => None,
            Self::Failed(e) => e.downcast_ref(),
        }
    }

    /// A [`DeepRefusal`] raised by a leg's own gate rather than one of the
    /// arms above, if any: the same closed set, so the route reflects it as
    /// a 409 rather than answering an authored refusal with the 500 literal.
    pub(crate) fn deep_refusal(&self) -> Option<String> {
        match self {
            Self::Refused(_) | Self::Deferred(_) | Self::Stopping => None,
            Self::Failed(e) => e.downcast_ref::<DeepRefusal>().map(|r| r.0.clone()),
        }
    }
}

impl std::fmt::Display for SwitchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            // `alternate()` is what `{:#}` sets: the full chain for the one
            // surface (daemon.log) that may read it. The plain form keeps
            // anyhow's head-line semantics, so the MCP tool's `reason`
            // payload stays byte-identical for every input.
            Self::Refused(sentence) | Self::Deferred(sentence) => f.write_str(sentence),
            Self::Stopping => f.write_str(crate::format::DAEMON_STOPPING),
            Self::Failed(e) => {
                if f.alternate() {
                    write!(f, "{e:#}")
                } else {
                    write!(f, "{e}")
                }
            }
        }
    }
}

impl std::error::Error for SwitchError {}

/// Every anyhow arm below (the IO legs, the path-bearing contexts) funnels
/// through `?` into `Failed`, the half a remote surface may not reflect;
/// `From` is what keeps the call sites bare.
impl From<anyhow::Error> for SwitchError {
    fn from(e: anyhow::Error) -> Self {
        Self::Failed(e)
    }
}

pub(crate) fn switch_profile_noninteractive(
    config: &crate::profile::ConfigHandle,
    target: &ProfileName,
    on_divergence: Option<DivergenceChoice>,
    refresher: impl Fn(
        &str,
        Option<&str>,
    ) -> std::result::Result<oauth::TokenResponse, oauth::RefreshError>,
    spend_marker: SpendMarker<'_>,
) -> std::result::Result<(Option<String>, String, Option<String>), SwitchError> {
    let (previous, target_disabled) = {
        #[allow(clippy::expect_used, reason = "mutex poisoning is unrecoverable")]
        let cfg = config.lock().expect("config mutex poisoned");
        (
            cfg.state.active_profile.as_deref().map(str::to_string),
            cfg.find(target).is_some_and(|p| p.is_disabled()),
        )
    };

    // Friendly early refuse, unconditional like `refuse_if_disabled` (no
    // active-exempt — a disabled profile can never be the active one, since
    // `disable_profile` itself refuses the active target). Placed BEFORE the
    // AUTH-1 gate below so a disabled, clock-expired target is refused before
    // its single-use refresh token ever gets rotated over HTTP; the
    // authoritative `ensure_switch_target_ok` gate inside `switch_profile`
    // stays the backstop, this only prevents the spurious rotation.
    if target_disabled {
        return Err(SwitchError::Refused(format!(
            "'{target}': account is disabled, run `clauth enable {target}`"
        )));
    }

    // Above the AUTH-1 gate, as in `switch_profile_cli`. Nobody can answer a
    // question here, so an item the settle cannot place refuses. The notice
    // rides the reply and the event log once its carry has landed.
    #[cfg(target_os = "macos")]
    let (_placed, notice) = if previous.as_deref() == Some(target) {
        (None, None)
    } else {
        let writer = crate::claude::BareItemWriter::Switch;
        let settled = match crate::claude::settle_bare_keychain_item(writer)
            .map_err(|e| SwitchError::Refused(format!("{e:#}")))?
        {
            crate::claude::BareItemSettle::Settled(settled) => settled,
            crate::claude::BareItemSettle::NeedsChoice(unplaced) => {
                return Err(SwitchError::Refused(format!(
                    "{:#}",
                    unplaced.refusal(writer)
                )));
            }
        };
        if let Some(notice) = &settled.notice {
            crate::logline::logline!(
                "{}: {notice}",
                crate::logline::role(crate::logline::stamped())
            );
        }
        (
            Some(crate::claude::BareItemPlacement::enter(settled.placed)),
            settled.notice,
        )
    };
    #[cfg(not(target_os = "macos"))]
    let notice = None;

    // AUTH-1 (Incident C): gate the target before its credentials land in the
    // Keychain — the same gate as the CLI switch, so "a quarantined account is
    // refused as a switch target" holds for EVERY noninteractive entry point
    // (MCP today; any future headless caller inherits it).
    // The already-active profile is exempt for the same reason as the CLI
    // path: nothing new to install, and gating it races a plain `claude`
    // refreshing the symlinked live file (a lost race false-quarantines).
    if previous.as_deref() != Some(target) {
        match oauth::ensure_installable(config, target, refresher, spend_marker) {
            oauth::AuthGate::Ready | oauth::AuthGate::Refreshed => {}
            oauth::AuthGate::Broken => {
                return Err(SwitchError::Refused(
                    crate::format::login_expired(target).line(),
                ));
            }
            // NOT a CLI stderr path — this is the MCP tool's JSON `reason`, so it
            // keeps the canned line without the status.
            oauth::AuthGate::Transient(e) => {
                let sentence = crate::format::refresh_transient(target, &e).line();
                return Err(if e.is_login_change() {
                    SwitchError::Deferred(sentence)
                } else {
                    SwitchError::Refused(sentence)
                });
            }
            // The daemon's stop flag refused the spend under the rotation lock:
            // no request went out, so answer with the typed stopping variant.
            oauth::AuthGate::Stopping => return Err(SwitchError::Stopping),
        }
    }

    // A logged-out shell is no divergence to resolve: skip the default and take
    // the plain switch, which replaces the empty file.
    let diverged = match previous.as_deref() {
        Some(active) => live_diverged_and_unsaved(&ProfileName::from(active.to_string()))?,
        None => false,
    };

    // The variant fns take the handle and lock internally, so this dispatch
    // holds no guard across the switch.
    if diverged {
        match on_divergence {
            Some(DivergenceChoice::Overwrite) => switch_profile_reconciled(config, target)?,
            Some(DivergenceChoice::Discard) => switch_profile_discard(config, target)?,
            Some(DivergenceChoice::NewProfile) | None => {
                let active = previous.as_deref().unwrap_or_default();
                return Err(SwitchError::Refused(crate::format::unsaved_live_login(
                    active,
                )));
            }
        }
    } else {
        switch_profile(config, target)?;
    }

    Ok((previous, target.to_string(), notice))
}

/// Snapshot active creds then clear them so Claude Code can't spend any account.
/// Used by wrap-off mode when the whole chain is exhausted. No-op when no profile
/// is active. A diverged live file is cleared WITHOUT being snapshotted
/// (`snapshot_active_credentials` skips it, keeping the stored identity), so a
/// fresh `/login` is dropped: the TUI gates that on the divergence prompt, while
/// the automatic wrap-off leg accepts the drop, unattended by design.
pub(crate) fn switch_off(config: &ConfigHandle) -> Result<()> {
    #[allow(
        clippy::expect_used,
        reason = "config mutex poisoning is unrecoverable"
    )]
    let mut guard = config.lock().expect("config mutex poisoned");
    let changed = switch_off_locked(&mut guard)?;
    drop(guard);
    strip_stale_identity_after_switch(changed);
    if changed {
        crate::daemon::publish_status(config);
    }
    Ok(())
}

/// [`switch_off`]'s locked body: the caller holds the config guard (ranked
/// outer of the state flock) and receives the did-anything-change answer so
/// it can gate its own republish.
pub(crate) fn switch_off_locked(config: &mut AppConfig) -> Result<bool> {
    with_state_lock(|held| {
        if config.state.active_profile.is_none() {
            return Ok(false);
        }
        snapshot_active_credentials(config)?;
        clear_claude_credentials()?;
        // No active account left to show; issue #17 applies here too — the
        // stale identity block is stripped by the callers (after the flock) via
        // `strip_stale_identity_after_switch`.
        config.state.set_active(None, held);
        // Same fresh-state rule as `finish_switch`: only the active marker is
        // this leg's change, so write it onto the current on-disk state rather
        // than a possibly-stale in-memory list.
        let mut state = load_app_state()?;
        state.set_active(None, held);
        save_app_state(&state)?;
        Ok(true)
    })
}

/// The env keys an activation has to strip out of `settings.json` before it
/// writes the incoming profile's: the OUTGOING profile's, or — with no active
/// marker to read — every configured profile's.
///
/// The fallback is the point. A cleared `active_profile` is clauth's record,
/// never a statement about `settings.json`: `switch_off` clears the marker
/// without touching the file, so a departed account's `[env]` entries are
/// still sitting there while the next activation repoints `ANTHROPIC_BASE_URL`
/// and `apiKeyHelper` at a different account. Stripping nothing pairs that
/// stale key with a new host. The incoming profile's own keys are re-applied
/// immediately after every call, so the wider strip costs nothing.
///
/// One helper rather than the expression inlined per site: the same
/// record-vs-file confusion produced this bug twice, at `finish_switch` and at
/// the reauth auto-activate arm, and a third copy is how it comes back.
pub(crate) fn outgoing_env_keys(config: &AppConfig) -> Vec<String> {
    config
        .state
        .active_profile
        .as_ref()
        .and_then(|n| config.find(n))
        .map(|p| p.env.keys().cloned().collect())
        .unwrap_or_else(|| {
            config
                .profiles
                .iter()
                .flat_map(|p| p.env.keys().cloned())
                .collect()
        })
}

fn finish_switch(config: &mut AppConfig, name: &ProfileName, held: &StateLockHeld) -> Result<()> {
    // Captured before `active_profile` is reassigned.
    let prev_env_keys = outgoing_env_keys(config);
    let profile = config.find(name).context("profile not found")?;
    apply_profile_to_claude_settings(profile, &prev_env_keys)?;
    // The issue #17 strip runs AFTER this fn, at the caller
    // (`strip_stale_identity_after_switch`, gated on `changed`), so it takes the
    // .claude.json sync lock (ranked outside the state flock) rather than
    // running under the flock here.
    config.state.set_active(Some(name.clone()), held);
    // Fresh state, not the whole in-memory list: a daemon drain may hold a
    // config older than a concurrent CLI delete/rename/login, and re-serializing
    // it would resurrect a deleted row or rewind an edit. Only the active marker
    // is this leg's own change, so read the current profiles.toml and change
    // that one field.
    let mut state = load_app_state()?;
    state.set_active(Some(name.clone()), held);
    save_app_state(&state)
}

pub(crate) fn edit_profile_endpoint(
    config: &mut AppConfig,
    name: &ProfileName,
    base_url: Option<String>,
    api_key: Option<String>,
) -> Result<()> {
    with_state_lock(|_held| {
        let before = read_before_identities(name);
        let profile = config.find_mut(name).context("profile not found")?;
        // A failed save must leave the in-memory profile matching disk: the
        // next write would otherwise persist a change the error said did not
        // happen.
        let snapshot = profile.clone();
        profile.base_url = base_url;
        profile.api_key = api_key;
        // Re-derive the provider — the in-memory config is authoritative until
        // the next disk reload, so a stale value here would keep (or block)
        // third-party fetches against the wrong endpoint.
        let provider = profile
            .base_url
            .as_deref()
            .and_then(crate::providers::Provider::from_base_url);
        // The console session is a FOURTH credential and it means nothing off
        // Alibaba: left behind, an endpoint move parks a live Model Studio
        // session on a profile that no longer talks to Model Studio, and every
        // later reload carries it forward. A move BETWEEN Alibaba endpoints
        // keeps it — the session the operator just captured is still the right
        // one, and a genuinely wrong site answers `AuthExpired`, which is
        // visible and one re-login from fixed.
        if provider != Some(crate::providers::Provider::Alibaba) {
            profile.console = None;
        }
        profile.provider = provider;
        let plan = endpoint_identity_plan(
            &before,
            &before.oauth,
            &StoredEndpointIdentity {
                base_url: profile.base_url.clone(),
                api_key: profile.api_key.clone(),
                console: profile.console.clone(),
                env: profile.env.clone(),
            },
            before.has_credentials,
        );
        forget_disk_plan(name, plan);
        if let Err(e) = save_profile_config(profile) {
            *profile = snapshot;
            return Err(e);
        }
        note_forget_plan(name, plan);

        if config.is_active(name) {
            let profile = config.find(name).context("profile not found")?;
            let prev_env_keys: Vec<String> = profile.env.keys().cloned().collect();
            apply_profile_to_claude_settings(profile, &prev_env_keys)?;
        }
        Ok(())
    })
}

/// Persist a captured Alibaba console session onto a profile — the session and
/// NOTHING else.
///
/// **The api key and base_url are deliberately not written, not even into an
/// empty slot.** The console callback returns a WORKSPACE key (`sk-ws-…`) and
/// the workspace endpoint (`ws-<id>.<region>.maas.aliyuncs.com`), which are a
/// different product from the Token Plan the profile runs on (`sk-sp-…` against
/// `token-plan.<region>.maas.aliyuncs.com`) and are billed differently — prepaid
/// plan vs pay-as-you-go. Writing either would silently move that account's
/// spend onto the other product. `ConsoleLoginOutcome` carries neither, so this
/// signature is the second place that has to change before one could.
///
/// The stale third-party cache is dropped: it was fetched under the previous
/// session, and a new login can be a different account entirely.
///
/// Lands only while `name` still holds `account`, the [`ProfileId`] claimed
/// when the login opened, and still points at the console front the session
/// came from: a session is minted on one front and meaningless on the other,
/// and the usage fetch keys on the credential's OWN site rather than on
/// `base_url`, so an endpoint moved between two Model Studio hosts during the
/// trip would quietly report the other plan's quota under this name.
pub(crate) fn store_console_login(
    config: &mut AppConfig,
    name: &ProfileName,
    account: &ProfileId,
    console: ConsoleCredential,
) -> Result<()> {
    with_state_lock(|held| {
        require_profile_id(name, account, held)?;
        let before = read_before_identities(name);
        let profile = config
            .find_mut(name)
            .with_context(|| format!("profile '{name}' not found"))?;
        if profile.console_login_target().map(|(site, _)| site) != Some(console.site) {
            bail!("'{name}' no longer points at the console this session came from");
        }
        profile.console = Some(console);
        let after = third_party_identity(
            profile.provider,
            profile.api_key.as_deref(),
            profile.console.as_ref(),
            profile.base_url.as_deref(),
        );
        let plan = ForgetPlan {
            third_party: before.third_party != after,
            ..ForgetPlan::default()
        };
        forget_disk_plan(name, plan);
        save_profile_config(profile)?;
        note_forget_plan(name, plan);
        Ok(())
    })
}

/// Persist a profile's model configuration. Re-applies to the live
/// `~/.claude/settings.json` when the profile is active so a running `claude`
/// picks it up on its next settings read. Mirrors [`edit_profile_endpoint`].
pub(crate) fn edit_profile_model(
    config: &mut AppConfig,
    name: &ProfileName,
    models: ModelSettings,
) -> Result<()> {
    with_state_lock(|_held| {
        let profile = config.find_mut(name).context("profile not found")?;
        profile.models = models;
        save_profile_config(profile)?;

        if config.is_active(name) {
            // A model-only edit never touches the generic `env` map, so passing
            // this profile's own keys as `prev` strips nothing (the removal loop
            // keeps every key the profile still carries). The model env keys
            // (`ANTHROPIC_DEFAULT_*`/`CLAUDE_CODE_SUBAGENT_MODEL`) are set or
            // cleared unconditionally inside `build_claude_settings_json`.
            let profile = config.find(name).context("profile not found")?;
            let prev_env_keys: Vec<String> = profile.env.keys().cloned().collect();
            apply_profile_to_claude_settings(profile, &prev_env_keys)?;
        }
        Ok(())
    })
}

/// Rewrite an account's `preferred_days` list and persist it — the Fallback
/// card's `preferred days` row commit. `edit` maps the list on disk to the new
/// one; the saved list comes back.
///
/// [`set_member_threshold`]'s fresh-state shape: the roster is read off disk so
/// an account deleted since the last reload is refused rather than recreated,
/// the profile is re-read so a field another writer changed meanwhile is not
/// rewound, and the in-memory copy moves only once the save landed. `edit` runs
/// on the list read under the lock, so a concurrent writer's day is kept rather
/// than overwritten by a toggle computed from a stale copy.
///
/// No `apply_profile_to_claude_settings` follow-up, unlike its model-field
/// twin: the list is read per chain build (`AppConfig::is_home_today`) and
/// never stamped into Claude Code's environment, so an edit on the active
/// account needs no re-stamp to take effect.
pub(crate) fn edit_profile_preferred_days(
    config: &mut AppConfig,
    name: &ProfileName,
    edit: impl FnOnce(&[chrono::Weekday]) -> Vec<chrono::Weekday>,
) -> Result<Vec<chrono::Weekday>> {
    with_state_lock(|_held| {
        if !load_app_state()?.profiles.iter().any(|n| n == name) {
            bail!("profile not found");
        }
        let mut fresh = load_profile(name)?;
        fresh.preferred_days = edit(&fresh.preferred_days);
        save_profile_config(&fresh)?;
        if let Some(profile) = config.find_mut(name) {
            profile.preferred_days = fresh.preferred_days.clone();
        }
        Ok(fresh.preferred_days)
    })
}

/// Write an applied preset's result (`base_url` + `models`, already merged over
/// the account's own by the caller) in a single locked transaction. A preset
/// never carries the api key, so the account's own credential is preserved. Building the full profile state and writing it once — one lock
/// acquisition, one disk write, one live-settings re-apply — means a failure
/// leaves the account on its prior state rather than half-stamped (new endpoint,
/// old models) the way chaining [`edit_profile_endpoint`] +
/// [`edit_profile_model`] would.
pub(crate) fn edit_profile_preset(
    config: &mut AppConfig,
    name: &ProfileName,
    base_url: Option<String>,
    models: ModelSettings,
) -> Result<()> {
    with_state_lock(|_held| {
        let before = read_before_identities(name);
        let profile = config.find_mut(name).context("profile not found")?;
        profile.base_url = base_url;
        profile.models = models;
        // Re-derive the provider exactly like `edit_profile_endpoint`: a stale
        // value here keeps (or blocks) third-party fetches against the wrong
        // endpoint. The api_key is unchanged, so only a moved endpoint can flip
        // the provider — no need to clear `third_party_usage` on a key rotation.
        let provider = profile
            .base_url
            .as_deref()
            .and_then(Provider::from_base_url);
        profile.provider = provider;
        let plan = endpoint_identity_plan(
            &before,
            &before.oauth,
            &StoredEndpointIdentity {
                base_url: profile.base_url.clone(),
                api_key: profile.api_key.clone(),
                console: profile.console.clone(),
                env: profile.env.clone(),
            },
            before.has_credentials,
        );
        forget_disk_plan(name, plan);
        save_profile_config(profile)?;
        note_forget_plan(name, plan);

        if config.is_active(name) {
            let profile = config.find(name).context("profile not found")?;
            let prev_env_keys: Vec<String> = profile.env.keys().cloned().collect();
            apply_profile_to_claude_settings(profile, &prev_env_keys)?;
        }
        Ok(())
    })
}

/// An account re-bind whose rows were already saved to disk before the active
/// account's live `settings.json` re-apply failed: the saved rows still count
/// as moved, and the account still counts as failed. The caller downcasts this
/// to recover both halves instead of losing the applied count with the error.
#[derive(Debug)]
pub(crate) struct RebindSavedBeforeFailure {
    pub(crate) applied: usize,
    pub(crate) error: anyhow::Error,
}

impl std::fmt::Display for RebindSavedBeforeFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.error, f)
    }
}

impl std::error::Error for RebindSavedBeforeFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.error.source()
    }
}

/// Apply one account's re-bind moves through the per-account model write path.
/// Each move lands only where the fresh on-disk row still holds the move's
/// `old` (still empty for an empty-row offer), so a row a concurrent writer
/// changed since the offer is skipped, never overwritten. Returns how many
/// rows moved. Only the offered rows change; base_url, env, keys and every
/// other model row stay byte-identical. A live-settings re-apply that fails
/// after the save returns the saved count inside a [`RebindSavedBeforeFailure`].
pub(crate) fn apply_rebind_account(
    config: &mut AppConfig,
    name: &ProfileName,
    moves: &[crate::presets::RowMove],
) -> Result<usize> {
    with_state_lock(|_held| {
        if !load_app_state()?.profiles.iter().any(|n| n == name) {
            bail!("profile not found");
        }
        let mut fresh = load_profile(name)?;
        let mut applied = 0usize;
        for m in moves {
            if fresh.models.field(m.row).cloned() == m.old {
                fresh.models.set_field(m.row, m.new.clone());
                applied += 1;
            }
        }
        if applied == 0 {
            return Ok(0);
        }
        // A failed save must leave the in-memory account unchanged, so the disk
        // write lands before the in-memory copy moves (`edit_profile_endpoint`'s
        // snapshot-restore shape).
        save_profile_config(&fresh)?;
        if let Some(profile) = config.find_mut(name) {
            profile.models = fresh.models.clone();
        }
        if config.is_active(name) {
            let Some(profile) = config.find(name) else {
                return Err(anyhow::Error::new(RebindSavedBeforeFailure {
                    applied,
                    error: anyhow::anyhow!("profile not found"),
                }));
            };
            let prev_env_keys: Vec<String> = profile.env.keys().cloned().collect();
            if let Err(error) = apply_profile_to_claude_settings(profile, &prev_env_keys) {
                return Err(anyhow::Error::new(RebindSavedBeforeFailure {
                    applied,
                    error,
                }));
            }
        }
        Ok(applied)
    })
}

/// Store the built-in bind-table digest the TUI offered, under the state flock:
/// fresh-state read-modify-write, so a concurrent state writer is never
/// clobbered. The CLI never reaches this.
pub(crate) fn store_preset_binds_offered(digest: String) -> Result<()> {
    with_state_lock(|_held| {
        let mut state = load_app_state()?;
        state.preset_binds_offered = Some(digest);
        save_app_state(&state)
    })
}

/// Persist a profile's custom env map (the Setup-tab field editor). Captures the
/// OLD env keys first so a re-apply to the live `~/.claude/settings.json` strips
/// any key the new map dropped — passing the new keys instead would leak a removed
/// entry into the live file. Mirrors [`edit_profile_model`].
pub(crate) fn edit_profile_env(
    config: &mut AppConfig,
    name: &ProfileName,
    env: BTreeMap<String, String>,
) -> Result<()> {
    with_state_lock(|_held| {
        let before = read_before_identities(name);
        let profile = config.find_mut(name).context("profile not found")?;
        // Snapshot before overwrite — a removed key is only stripped from live
        // settings when it appears in `prev` but not in the new `profile.env`.
        let old_env_keys: Vec<String> = profile.env.keys().cloned().collect();
        profile.env = env;
        let after_throughput = throughput_for_endpoint(
            &before.oauth,
            profile.base_url.as_deref(),
            before.has_credentials,
            profile.api_key.as_deref(),
            &profile.env,
        );
        let plan = ForgetPlan {
            throughput: before.throughput != after_throughput,
            ..ForgetPlan::default()
        };
        forget_disk_plan(name, plan);
        save_profile_config(profile)?;
        note_forget_plan(name, plan);

        if config.is_active(name) {
            let profile = config.find(name).context("profile not found")?;
            apply_profile_to_claude_settings(profile, &old_env_keys)?;
        }
        Ok(())
    })
}

/// Which source a candidate custom env key collides with, in priority order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum EnvKeyCollision {
    /// A clauth-managed key derived from a profile field; carries the field's
    /// human label (`the base url field`, …).
    Managed(&'static str),
    /// Already a custom env entry on this account; carries the sorted index.
    ProfileField(usize),
    /// Already present in the inherited `~/.claude/settings.json` `env` block.
    BaseSettings,
}

/// Classify a candidate custom env key against the three sources, highest
/// priority first: a clauth-managed field key, then this account's existing
/// custom entries, then the inherited base `settings.json`. The managed and
/// own-field checks return before the base check, so a base hit means a key set
/// outside clauth. `base_env_keys` is read from the live settings by the caller.
pub(crate) fn classify_env_key(
    profile: &Profile,
    base_env_keys: &[String],
    candidate: &str,
) -> Option<EnvKeyCollision> {
    if let Some(label) = managed_env_key_label(candidate) {
        return Some(EnvKeyCollision::Managed(label));
    }
    if let Some(idx) = profile.env.keys().position(|k| k == candidate) {
        return Some(EnvKeyCollision::ProfileField(idx));
    }
    base_env_keys
        .iter()
        .any(|k| k == candidate)
        .then_some(EnvKeyCollision::BaseSettings)
}

/// Take `name`'s rotation lock for an account mutation, or refuse.
///
/// A rotation holds this lock for its whole OAuth round trip and resolves the
/// profile by NAME when it persists, so a delete or rename landing inside that
/// window either resurrects the directory the delete removed or strands the
/// spent refresh token on the renamed account. Refused rather than queued
/// because the blocking acquisition carries no deadline: a stuck round trip would
/// park the command instead of failing it. The bounded form a session start takes
/// (`runtime::ROTATION_LOCK_TIMEOUT`) is no substitute — it waits tens of seconds,
/// and a mutation the operator typed should answer now.
///
/// Creates `~/.clauth/rotation-locks/` and this profile's lock file when they
/// are absent (`RotationGuard::try_acquire` does), so it is a write rather than
/// a pure read and the name does not say so. It creates no profile directory —
/// which is what lets `delete_profile` and `rename_profile` below keep their
/// `dir.exists()` branches meaningful.
///
/// Handed to [`delete_profile`] / [`rename_profile`] rather than taken inside
/// them: the TUI holds the `config` guard across a rename and takes it inside a
/// delete, and ROTATION ranks outside `Config`, so the acquisition has to happen
/// before the config lock. The guard parameter makes a caller with no lock a
/// compile error; what makes a caller who takes it in the wrong ORDER fail is
/// `lockorder`'s assertion.
pub(crate) fn rotation_guard_for_mutation(name: &ProfileName) -> Result<RotationGuard> {
    match RotationGuard::try_acquire(name) {
        Ok(Some(guard)) => Ok(guard),
        Ok(None) => bail!("'{name}' has a token rotation in progress, retry in a moment"),
        // The fault arm speaks the same vocabulary as its two siblings in
        // `oauth`: the typed copy names the fix, which a raw errno does not.
        // Added as context rather than replacing the io error, so the chain
        // still carries which path failed and why.
        Err(e) => Err(e.context(
            crate::format::Transient::new(
                crate::format::Cause::RotationLockUnavailable(name.to_string()),
                crate::format::Retry::Stated,
            )
            .text(),
        )),
    }
}

/// `_rotation` is [`rotation_guard_for_mutation`]'s guard for `old`: a second
/// acquisition at the same rank is a lock-order violation whichever profile it
/// names, so `new` cannot carry one of its own.
pub(crate) fn rename_profile(
    config: &mut AppConfig,
    old: &ProfileName,
    new: &ProfileName,
    _rotation: &RotationGuard,
) -> Result<()> {
    // Asserted rather than stated in prose: every caller today rejects a
    // duplicate `new` first, and a future one that forgets renames onto a live
    // account — under a guard held for `old`, which would not serialize it.
    //
    // Folding, and excluding `old` the way `validate_profile_name` does. A
    // case-EXACT check is the wrong question here: every resolution site reaches
    // an account through `canonical_name`, which folds, so `work` and `WORK`
    // resolve to one account while occupying two directories.
    debug_assert!(
        config.canonical_name(new).is_none_or(|n| n == old.as_str()),
        "rename target '{new}' already names an account"
    );
    // The gateway's client token follows the profile; a clash with a client
    // the gateway's own env names is read here, outside the flock.
    let token_planned = crate::gateway_tokens::refuse_profile_rename(old.as_str(), new.as_str())?;
    #[cfg(test)]
    crate::gateway_tokens::run_pre_hold_hook(old.as_str());
    with_state_lock(|_held| {
        // Same gate delete and disable carry, same predicate and copy: a live
        // session's runtime tree, markers and env paths all live under this
        // directory, so moving it out from under the child breaks the session
        // (the registry rows keep naming the old profile; nothing rekeys them).
        if crate::runtime::has_live_session(old) {
            bail!("'{old}' has a live session, close it first");
        }
        Ok(())
    })?;
    // Stop every runtime tree's supervisor before the profile dir moves,
    // with the flock dropped (a signal or wait never spans it). The rotation
    // guard the caller holds for `old` keeps an acquire from re-minting a path
    // during the stop. Rename has no `--force` flag, so a refusal names only
    // the retry.
    let old_dir = profile_dir(old)?;
    crate::runtime::stop_profile_tree_supervisors(
        old,
        &old_dir,
        crate::runtime::SupervisorStopGate::Refused {
            offers_force: false,
        },
    )?;
    with_state_lock(|held| {
        let old_dir = profile_dir(old)?;
        let new_dir = profile_dir(new)?;
        // The name validation above checked the RECORD; a directory can outlive
        // its record — a per-profile cache a stale-config fetch leg wrote after
        // the account was deleted re-creates the dir (the writer is gated now,
        // but leftovers predate it). rename(2) onto an existing non-empty dir
        // fails ENOTEMPTY, which reads as an internal failure; refuse here with
        // the actionable shape instead. Gated on `old_dir` existing: that is
        // the only branch that renames, so it is the only one ENOTEMPTY can
        // fire in — and with `old` absent, `new` present is the OTHER half of a
        // rename this process (or a dead one) moved the dir for but never
        // recorded: a SIGKILL or a failing save between the move and
        // `save_app_state` below. That directory holds this profile's own
        // content, so its retry must complete the record rename (the pre-gate
        // recovery), never send the operator to delete it. A case-only rename
        // (`d3` -> `D3`) resolves to ONE directory on a case-insensitive
        // filesystem (the macOS default), so same-inode pairs are exempt.
        if old_dir.exists() && new_dir.exists() {
            let same_dir = crate::canonical::identity(&old_dir)
                .ok()
                .zip(crate::canonical::identity(&new_dir).ok())
                .is_some_and(|(a, b)| a == b);
            if !same_dir {
                bail!(
                    "'{new}' already has a directory at {} with no account behind it, \
                     delete the directory or pick another name",
                    new_dir.display()
                );
            }
        }
        // Before the first move: a failed store write leaves everything as it
        // was, and a retry after a later failure finds the token already moved.
        crate::gateway_tokens::rename_profile_token(
            old.as_str(),
            new.as_str(),
            token_planned,
            held,
        )?;
        if old_dir.exists() {
            std::fs::rename(&old_dir, &new_dir)
                .with_context(|| format!("failed to rename profile directory to '{new}'"))?;
            // The live link pointed into the moved directory: follow the move
            // before anything below can fail, so a refused relink (an unsaved
            // Keychain login in the way) never leaves it dangling.
            repoint_link_into_moved_dir(&old_dir, &new_dir)?;
        }

        let was_active = config.is_active(old);
        config.rename_all_occurrences(old, new, held);

        save_app_state(&config.state)?;
        // The dir move carried every disk cache to `new`; note after the record
        // move lands so a failed rename notes nothing.
        note_rename(old, new);

        if was_active {
            link_profile_credentials(new)?;
        }
        Ok(())
    })?;
    // The dir move carried the durable `/profile` stamp to `new`, so only the OLD
    // name's memo is left — authoritative over a stamp no longer under that name.
    // Sequential, never inside the closure: `ProfileTtl` (450) ranks outside the
    // state flock (500), so this asserts if it ever moves in — see that rank's doc
    // for why the clock's file IO must not hold a cross-process flock.
    crate::usage::expire_profile_ttl(old);
    Ok(())
}

/// Point the live credentials link at the same file under `new_dir` when it
/// points into `old_dir`, which a rename just moved to `new_dir`. The file it
/// names is the one it named before, so what Claude Code reads is unchanged
/// and no Keychain write is involved; any other live slot is left alone.
pub(crate) fn repoint_link_into_moved_dir(
    old_dir: &std::path::Path,
    new_dir: &std::path::Path,
) -> Result<()> {
    let link = crate::claude::claude_credentials_path()?;
    let Ok(target) = std::fs::read_link(&link) else {
        return Ok(());
    };
    let Ok(rest) = target.strip_prefix(old_dir) else {
        return Ok(());
    };
    crate::claude::publish_credential_link(&link, &new_dir.join(rest))
}

/// The CLI's delete: its live-session, supervisor and unrescued-files refusals
/// offer `--force`.
pub(crate) fn delete_profile(
    config: &mut AppConfig,
    name: &ProfileName,
    force: bool,
    rotation: &RotationGuard,
) -> Result<()> {
    delete_profile_offering(config, name, force, true, rotation)
}

/// `rotation` is [`rotation_guard_for_mutation`]'s guard for `name`, and
/// `force` does not waive it: `force` waives the live-session gate, while an
/// in-flight rotation is a different hazard that no confirmation makes safe.
/// `offers_force` is false for a caller with no `--force` flag (the TUI), so no
/// refusal names the flag.
pub(crate) fn delete_profile_offering(
    config: &mut AppConfig,
    name: &ProfileName,
    force: bool,
    offers_force: bool,
    rotation: &RotationGuard,
) -> Result<()> {
    delete_profile_taking_config(move || config, name, force, offers_force, rotation)
}

/// [`delete_profile_offering`] for a caller that shares its config with other
/// threads (the TUI's delete worker): `config` is called only for the final
/// unwire-and-remove hold, so the config lock is never held across the
/// guards' waits or the rescue's tree-sized IO.
pub(crate) fn delete_profile_taking_config<C: std::ops::DerefMut<Target = AppConfig>>(
    config: impl FnOnce() -> C,
    name: &ProfileName,
    force: bool,
    offers_force: bool,
    _rotation: &RotationGuard,
) -> Result<()> {
    // The gateway reads the last-token guard needs happen outside the flock.
    let token = crate::gateway_tokens::plan_profile_delete(name.as_str())?;
    #[cfg(test)]
    crate::gateway_tokens::run_pre_hold_hook(name.as_str());
    with_state_lock(|held| {
        // Refuse to pull an account out from under a running `clauth start`
        // session (either flavor), checked before any removal so a refused
        // delete is a clean no-op. `--yes` skips the confirm prompt but does NOT
        // override this; only `force` does. A Claude Code background job naming
        // any tree under this profile's dir is a live session for this refusal
        // too — `has_live_session` unions both sources.
        if !force && crate::runtime::has_live_session(name) {
            if offers_force {
                bail!("'{name}' has a live session, pass --force to delete it anyway");
            }
            bail!("'{name}' has a live session");
        }
        // Same discipline: the gateway's last client token refuses here,
        // before anything is removed; `force` waives the session gate alone.
        crate::gateway_tokens::refuse_profile_delete(token.as_ref(), name.as_str(), held)?;
        Ok(())
    })?;
    // Stop every runtime tree's supervisor before any unwire or removal,
    // with the flock dropped (a signal or wait never spans it). A tree whose
    // supervisor cannot be verified or stopped refuses the delete, naming the
    // tree; the rotation guard held by the caller keeps an acquire from
    // re-minting a path during the stop. `--force` still stops every verifiable
    // supervisor and waives the refusal for any supervisor it cannot stop
    // (unverifiable, or verified but still alive after SIGTERM): the tree is
    // removed anyway.
    let dir = profile_dir(name)?;
    crate::runtime::stop_profile_tree_supervisors(
        name,
        &dir,
        if force {
            crate::runtime::SupervisorStopGate::Forced
        } else {
            crate::runtime::SupervisorStopGate::Refused { offers_force }
        },
    )?;
    // Lift what the profile's dead runtime trees alone hold before the removal
    // below takes them, with the flock dropped (the rescue is tree-sized IO).
    // Anything left refuses before the unwire: a refused delete keeps the
    // record, the credentials link, the settings and the profile dir, while
    // the supervisors stopped above stay stopped and whatever the rescue
    // landed stays in `~/.claude`. `force` still rescues first, then deletes
    // what is left.
    let unrescued = crate::runtime::rescue_profile_trees(&dir);
    if !force && let Some(tree) = unrescued {
        if offers_force {
            bail!(
                "'{name}' has unrescued session files in {}, pass --force to delete them anyway",
                tree.display()
            );
        }
        bail!("'{name}' has unrescued session files in {}", tree.display());
    }
    let mut config = config();
    with_state_lock(|held| {
        // Unwire the active account from the live credentials link + settings.json
        // BEFORE the irreversible removal, and AFTER the supervisor stop above:
        // a refused stop leaves `settings.json`, the credentials link and the
        // active marker exactly as they were. These are fallible external
        // writes; a failure here still leaves both the record and the dir intact
        // and fully retryable, rather than stranding the api key in plaintext
        // settings.json with the profile record already gone. A blank profile
        // clears its endpoint/key/model env so the key can't linger and the next
        // session doesn't route to a dead endpoint.
        if config.is_active(name) {
            // An active API profile's base_url + api_key (and model-tier keys)
            // live in ~/.claude/settings.json, not the credentials link. Capture
            // its custom env keys before removal so the unwire can strip those too.
            let active_env_keys: Vec<String> = config
                .find(name)
                .map(|p| p.env.keys().cloned().collect())
                .unwrap_or_default();
            clear_claude_credentials()?;
            let blank = Profile::new(name.to_string(), None, None);
            apply_profile_to_claude_settings(&blank, &active_env_keys)?;
        }
        // Dir before state: a failed removal keeps the profile in state so the
        // user can retry; persisting state first would leave an orphan dir.
        let dir = profile_dir(name)?;
        if dir.exists() {
            std::fs::remove_dir_all(&dir)
                .with_context(|| format!("failed to delete profile directory for '{name}'"))?;
        }
        // Revoked with the record, after the irreversible removal succeeded:
        // a failure here keeps the profile in state, and the retry revokes.
        if token.is_some() {
            crate::gateway_tokens::revoke_profile_token(name.as_str(), held)?;
        }
        config.remove(name, held);
        save_app_state(&config.state)?;
        // `remove_dir_all` swept every disk cache; only the memory entries need
        // noting.
        note_forget_plan(
            name,
            ForgetPlan {
                oauth: true,
                third_party: true,
                ..ForgetPlan::default()
            },
        );
        Ok(())
    })?;
    // `remove_dir_all` took the durable stamp with it; the memo would outlive the
    // profile and mute the first `/profile` of a same-name relogin inside the hour.
    // Outside the closure — see `rename_profile` on the rank order.
    crate::usage::expire_profile_ttl(name);
    Ok(())
}

/// Move the codex active slot to `name` — and, when the operator's own
/// `~/.codex/auth.json` is a link clauth installed, move that link with it.
///
/// Returns the operator slot it repointed, if any.
///
/// The marker alone is not a switch for anyone who runs bare `codex`: that
/// reads the operator slot, which the capture linked onto ONE profile's store
/// and nothing ever moved again. So a switch changed the panel and the feed
/// while every codex the operator started kept spending the old account. The
/// slot follows only when it is clauth's own link (the ownership rule
/// [`detach_operator_auth_slot`] already uses); a regular file is the
/// operator's own login and an absent slot is a login they never gave us —
/// both are left exactly as found.
///
/// Membership is re-made against the state [`CodexState::update`] loaded
/// under the lock, so a concurrent delete can't be switched onto. A
/// quarantined chain refuses the way a disabled claude account does: the
/// slot would name an account no session can authenticate as.
///
/// The link moves BEFORE the marker, inside the same state lock: if the link
/// cannot move, the switch fails before anything is written instead of
/// reporting a switch that did not happen. The one partial outcome is a
/// failure SAVING the state after the link moved, which leaves the link on
/// `name` and the marker where it was. That heals on the next switch to
/// `name`: the link is checked on the already-active path too, so switching
/// to the account the marker already names REPAIRS a slot that drifted. On
/// Windows a failed repoint can leave the slot absent rather than on the old
/// profile — the rename there replaces by remove-then-rename — and the next
/// switch to either account re-links it.
pub(crate) fn switch_codex_profile(name: &str) -> Result<Option<std::path::PathBuf>> {
    crate::codex_profiles::CodexState::update(|state| {
        if !state.holds(name) {
            bail!("codex profile '{name}' not found");
        }
        crate::codex_auth::refuse_if_quarantined(name)?;
        let repointed = follow_operator_auth_slot(name)?;
        // Same early return the claude switch takes on `is_active` — nothing
        // to move, and `update`'s dirty check then leaves the file untouched.
        if state.active_profile().map(ProfileName::as_str) != Some(name) {
            state.set_active(Some(name));
        }
        Ok(repointed)
    })
}

/// Repoint the operator's `auth.json` at `name`'s store when it is a link into
/// a DIFFERENT clauth profile's store; `None` when there is nothing of ours to
/// move (a regular file, no slot, a foreign link, or already pointing here).
fn follow_operator_auth_slot(name: &str) -> Result<Option<std::path::PathBuf>> {
    // Resolved the way the capture and the delete resolve it: inside a
    // `clauth start` session CODEX_HOME names the session home, and the
    // operator's real slot is still the default one.
    let operator = codex_operator_home().or_else(|_| default_codex_operator_home())?;
    let slot = operator.join("auth.json");
    let Ok(target) = std::fs::read_link(&slot) else {
        return Ok(None);
    };
    let Some(holder) = clauth_auth_store_owner(&target) else {
        return Ok(None);
    };
    if holder.eq_ignore_ascii_case(name) {
        return Ok(None);
    }
    let store = crate::profile::profile_subpath(&ProfileName::from(name), "auth.json")?;
    if !store.exists() {
        bail!(
            "'{name}' has no stored codex login, so {} cannot follow it. Log it in \
             with `clauth login {name} --codex --browser`",
            slot.display()
        );
    }
    if !adopt_operator_auth_slot(&slot, &store) {
        bail!(
            "could not repoint {} at '{name}'. The switch was not made.",
            slot.display()
        );
    }
    Ok(Some(slot))
}

/// `clauth delete <name>` for a codex profile. Same shape as the claude
/// [`delete_profile`]: the live gate, an unwire of what the profile installed
/// globally BEFORE the irreversible removal, then dir before state — a refused
/// or failed delete leaves the record intact and retryable. What codex
/// installs globally is the operator's `auth.json` slot the capture linked
/// onto this store ([`adopt_operator_auth_slot`]); the claude-only halves (the
/// credentials link, the settings.json endpoint, the usage-TTL memo) have no
/// counterpart here. Returns the slot it detached, so the caller can say the
/// operator's own codex is logged out now.
///
/// The typed live-session refusal [`delete_codex_profile`] raises when the
/// profile still runs a live session and `force` was not passed. Kept
/// downcastable so the CLI caller appends its `--force` escape-hatch tail to
/// THIS refusal alone: a second `has_live_marker` read would re-answer a
/// question the delete already answered, and tag an unrelated failure (a state
/// flock timeout) with a flag that cannot fix it.
#[derive(Debug)]
pub(crate) struct CodexLiveSession(pub(crate) String);

impl std::fmt::Display for CodexLiveSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "'{}' has a live session", self.0)
    }
}

impl std::error::Error for CodexLiveSession {}

/// `_rotation` is [`rotation_guard_for_mutation`]'s guard for `name`, and
/// `force` does not waive it: a standby rotation racing this removal would
/// either resurrect an orphan `auth.json` holding the pair it minted, or lose
/// that pair after the old single-use token was spent — a dead chain no
/// re-capture can revive. `force` waives the live-session gate alone.
pub(crate) fn delete_codex_profile(
    name: &str,
    force: bool,
    _rotation: &RotationGuard,
) -> Result<Option<std::path::PathBuf>> {
    crate::codex_profiles::CodexState::update(|state| {
        // Membership re-made against the state loaded UNDER the lock, before
        // anything irreversible: the caller resolved this name from a
        // lock-free snapshot and then parked on an unbounded confirm prompt.
        // In that window the profile can be deleted elsewhere and the name
        // re-created — on either harness — and `remove_dir_all` below would
        // then destroy a dir this record no longer owns.
        if !state.holds(name) {
            bail!("codex profile '{name}' not found");
        }
        let owned = ProfileName::from(name);
        if !force && crate::runtime::has_live_marker(&owned) {
            // One sentence source for both harnesses: the CLI caller appends its
            // `--force` tail, the TUI surfaces this alone (a TUI refusal never
            // names a CLI flag). Raised as a TYPED error so the CLI matches it,
            // never re-reads the predicate.
            return Err(anyhow::Error::new(CodexLiveSession(name.to_string())));
        }
        // Before the dir goes: a slot still linked into it would dangle, and
        // the operator's next `codex login` would revoke through that link
        // first (see `codex_login_capture`'s refusal) — into a store that no
        // longer exists, leaving their own codex with no login and no word why.
        let detached = detach_operator_auth_slot(name)?;
        let dir = profile_dir(&owned)?;
        if dir.exists() {
            // The detach is irreversible and precedes this step, so a removal
            // that fails after it must still say the operator's codex has no
            // login now: the retry finds no link and can never say it.
            std::fs::remove_dir_all(&dir).with_context(|| match &detached {
                Some(slot) => format!(
                    "failed to remove profile directory for '{name}' after {} was detached from \
                     it, so your own codex has no login now; run `codex login` to mint a fresh \
                     one",
                    slot.display()
                ),
                None => format!("failed to remove profile directory for '{name}'"),
            })?;
        }
        state.remove_profile(name);
        // The dir removal swept the shared `usage_cache.json` and the auth
        // store; only the memory entry needs noting.
        note_forget(&owned, Half::Codex);
        Ok(detached)
    })
}

/// Remove the operator's `auth.json` when it is the link
/// [`adopt_operator_auth_slot`] installed onto THIS profile's store — the link
/// alone, never its target — returning the slot's path. A link naming another
/// profile, a regular file (the operator re-logged in on their own), or an
/// absent slot is left exactly as found. The operator home resolves as the
/// capture resolves it; where that refuses (`CODEX_HOME` inside a clauth
/// session home: a delete typed from a shell codex spawned by `clauth start`)
/// the operator's real slot is still the default home's, which the capture
/// linked exactly as it would have from any other shell, so that is the one
/// checked — the ownership predicate is what makes the fallback safe.
fn detach_operator_auth_slot(name: &str) -> Result<Option<std::path::PathBuf>> {
    let operator = codex_operator_home().or_else(|_| default_codex_operator_home())?;
    let slot = operator.join("auth.json");
    let Ok(target) = std::fs::read_link(&slot) else {
        return Ok(None);
    };
    if !clauth_auth_store_owner(&target).is_some_and(|holder| holder.eq_ignore_ascii_case(name)) {
        return Ok(None);
    }
    std::fs::remove_file(&slot)
        .with_context(|| format!("failed to detach {} from '{name}'", slot.display()))?;
    Ok(Some(slot))
}

/// `clauth login <name> --codex` — create (or re-authenticate) a codex
/// profile by ADOPTING the operator's own `codex login`: the chain moves into
/// `profiles/<name>/auth.json` (atomic, 0600 — this writer owns that mode)
/// with every key codex wrote and `last_refresh` re-stamped to the capture
/// time, and the operator's `auth.json` becomes a symlink to it. One
/// physical file is the design's own safety mechanism (decision 8): the
/// operator's bare `codex`, every clauth session, and clauth's rotation all
/// hold the same chain. A snapshot-copy here would be the forbidden
/// configuration decisions 7/8 exist to prevent — two carriers of a
/// single-use rotating chain, where the first refresh on either side strands
/// the other. Where the operator slot cannot be linked (a host without
/// symlink privilege), the copy is taken anyway and that exact hazard is
/// said out loud instead of implied away.
///
/// The operator home is the one the operator's codex actually uses: a set
/// `CODEX_HOME` is honored — unless it names a home clauth built, which means
/// this shell is INSIDE a clauth codex session and "the operator's login" is
/// some profile's store; that refuses rather than snapshotting a sibling.
///
/// Refusals, each naming its fix:
/// - any store mode other than the file default (`keyring`, `auto`,
///   `ephemeral`, or something newer): the file is absent, stale, or
///   nonexistent BY DESIGN under those, so a capture would snapshot nothing
///   or yesterday's chain. Allow-list, not deny-list — an unknown future
///   mode refuses instead of guessing.
/// - no `tokens` chain in the file (an API-key-only setup): nothing there
///   for rotation, usage, or the session symlink to manage.
/// - a slot already adopted by ANOTHER profile: one chain, one profile.
/// - a live session on the target profile: re-capture replaces the chain the
///   running session holds.
pub(crate) fn codex_login_capture(name: &str) -> Result<()> {
    let outcome = codex_login_capture_at(
        name,
        &chrono::Utc::now().to_rfc3339(),
        CodexLandMode::CreateOrReauth,
    )?;
    for line in codex_capture_lines(&outcome) {
        outln!("{line}");
    }
    Ok(())
}

/// The operator-codex capture's outcome, before any surface renders it: what
/// moved and where. The CLI prints [`codex_capture_lines`]; the TUI reads the
/// flags and toasts, so the capture itself never writes stdout.
pub(crate) enum CodexCaptureOutcome {
    /// The chain was written into the profile store — freshly, or re-captured
    /// over the operator's own chain. `adopted` is false when the operator's
    /// slot could not be repointed (no symlink support), so it is now a
    /// separate copy of a single-use rotating chain.
    Captured {
        canonical: String,
        reauth: bool,
        adopted: bool,
        auth_path: std::path::PathBuf,
    },
    /// The operator's slot already followed THIS profile: nothing moved.
    AlreadyFollows {
        canonical: String,
        auth_path: std::path::PathBuf,
    },
}

/// The CLI's capture report, one entry per line, exactly as the operator sees
/// it. A pure function so a test pins the lines by equality and the TUI never
/// prints them.
pub(crate) fn codex_capture_lines(outcome: &CodexCaptureOutcome) -> Vec<String> {
    let mut lines = Vec::new();
    match outcome {
        CodexCaptureOutcome::AlreadyFollows {
            canonical,
            auth_path,
        } => lines.push(format!(
            "clauth: {} already follows codex profile '{canonical}'. Nothing to capture",
            auth_path.display()
        )),
        CodexCaptureOutcome::Captured {
            canonical,
            reauth,
            adopted,
            auth_path,
        } => {
            if *reauth {
                lines.push(format!(
                    "clauth: re-captured the operator codex login into '{canonical}'"
                ));
            } else {
                lines.push(format!(
                    "clauth: captured the operator codex login into codex profile '{canonical}'"
                ));
            }
            if *adopted {
                lines.push(format!(
                    "clauth: {} now follows the profile store. Your own codex and clauth sessions share one chain",
                    auth_path.display()
                ));
                lines.push(format!(
                    "clauth: while it does, `codex login` and `codex logout` reach '{canonical}'s chain through that link and revoke it server-side. Remove the link first if you mean to mint a chain for a different account"
                ));
            } else {
                lines.push(format!(
                    "clauth: could not repoint {} (no symlink support?). It is now a SEPARATE copy of a single-use rotating chain, and the first refresh on either side strands the other. Run codex only through `clauth start {canonical}` from here on, or `codex login` again for your own use",
                    auth_path.display()
                ));
            }
        }
    }
    lines
}

/// [`codex_login_capture`] with the capture time and landing mode injected, so
/// the re-stamp is pinnable and the create-only refusal is testable. `mode`
/// decides what a name the codex roster already holds means: the CLI re-auths
/// it ([`CodexLandMode::CreateOrReauth`]); the TUI's `+ new` create refuses it
/// ([`CodexLandMode::CreateOnly`]).
pub(crate) fn codex_login_capture_at(
    name: &str,
    now_rfc3339: &str,
    mode: CodexLandMode,
) -> Result<CodexCaptureOutcome> {
    let trimmed = validate_name_chars(name)?.to_string();
    let operator = codex_operator_capture_home()?;
    let auth_path = operator.join("auth.json");

    // A slot clauth already adopted: the chain belongs to exactly one profile.
    if let Some(holder) = operator_auth_slot_holder(&auth_path) {
        if holder.eq_ignore_ascii_case(&trimmed) {
            match mode {
                CodexLandMode::CreateOnly => {
                    bail!("codex profile '{holder}' already exists. Create it under a new name")
                }
                _ => {
                    return Ok(CodexCaptureOutcome::AlreadyFollows {
                        canonical: holder.clone(),
                        auth_path,
                    });
                }
            }
        }
        // NOT "run `codex login`": that slot is a LINK to '{holder}'s store, and
        // codex's login opens with `clear_existing_auth_before_login` ->
        // `logout_with_revoke`, which LOADS the stored auth through the link and
        // POSTs its refresh token to the revoke endpoint before minting. The
        // obvious next step would therefore kill the already-captured profile
        // server-side, which no re-login of '{trimmed}' can undo.
        bail!(
            "{slot} is already captured as codex profile '{holder}': one chain, one \
             profile. That slot is a LINK to '{holder}'s store, and `codex login` \
             revokes whatever it finds there before minting, so running it now would \
             kill '{holder}'s chain for good. Remove the link first (`rm {slot}` \
             leaves '{holder}' itself intact), then `codex login` and capture that \
             into '{trimmed}'",
            slot = auth_path.display()
        );
    }

    let parsed = read_codex_operator_auth(&auth_path)?;
    // A capture is a chain event, so it carries the stamp every chain event
    // carries (codex's `persist_tokens` writes it; the browser login stamps it
    // too). Left at the operator's login time, the captured chain would read
    // OLDER than a session copy rotated since, and the store convergence,
    // which reads `last_refresh` first, would hand that copy the win and drop
    // the chain the operator just captured.
    let parsed = parsed.with_last_refresh(now_rfc3339);
    let raw = parsed.to_bytes();

    // RotationGuard outermost, state flock inside — the module-wide order. The
    // guard is what a live rotation (a running codex refreshing through the
    // store symlink) holds; taking it means the store rewrite below can never
    // land mid-rotation. The name is resolved lock-free first and re-resolved
    // under the state lock; a rename racing that window bails rather than
    // guarding one name and writing another.
    let guess = crate::codex_profiles::CodexState::load()?
        .canonical_name(&trimmed)
        .unwrap_or_else(|| trimmed.clone());
    let _rotation_guard =
        crate::runtime::RotationGuard::acquire(&ProfileName::from(guess.as_str()))?;
    let (canonical, reauth, adopted) = crate::codex_profiles::CodexState::update(|state| {
        let (canonical, reauth) = match (state.canonical_name(&trimmed), mode) {
            (Some(canonical), CodexLandMode::CreateOnly) => {
                bail!("codex profile '{canonical}' already exists. Create it under a new name");
            }
            (Some(canonical), CodexLandMode::CreateOrReauth) => (canonical, true),
            (None, _) => {
                // Validated UNDER the same lock the roster write lands under —
                // the pre-IO window rule. (The claude half reads profiles.toml,
                // which this lock also serializes.)
                validate_profile_name(&trimmed, Harness::Codex, None)?;
                (trimmed.clone(), false)
            }
        };
        if canonical != guess {
            bail!("'{trimmed}' was renamed while the capture prepared. Re-run it");
        }
        if crate::runtime::has_live_marker(&ProfileName::from(canonical.as_str())) {
            bail!(
                "'{canonical}' has a live codex session, which holds the chain this \
                 capture would replace. Close it first"
            );
        }
        if reauth {
            refuse_codex_account_swap(&canonical, parsed.account_id())?;
        }
        let store = write_codex_profile_store(state, &canonical, &raw)?;
        // The adoption itself: the operator slot becomes a link to the store,
        // atomically (symlink at a staging sibling, renamed over). Best-effort
        // — a host that cannot symlink keeps the copy and hears the cost.
        let adopted = adopt_operator_auth_slot(&auth_path, &store);
        Ok((canonical, reauth, adopted))
    })?;

    Ok(CodexCaptureOutcome::Captured {
        canonical,
        reauth,
        adopted,
        auth_path,
    })
}

/// The capture's first read: the operator home, refused unless its store mode
/// is the file default (`auth.json`). The read-only half of
/// [`codex_login_capture_at`]'s refusals, shared with the capturable predicate
/// so the row's visibility and the capture run on one check.
fn codex_operator_capture_home() -> Result<std::path::PathBuf> {
    let operator = codex_operator_home()?;
    match codex_operator_store_mode(&operator).as_deref() {
        None | Some("file") => {}
        Some(mode) => bail!(
            "the operator codex does not keep its login in auth.json \
             (cli_auth_credentials_store = \"{mode}\" in {}/config.toml), so there is \
             nothing current to capture there. Set it to \"file\", run `codex login`, \
             then re-run this capture",
            operator.display()
        ),
    }
    Ok(operator)
}

/// The clauth profile whose store the operator's `auth.json` slot already
/// links into, or `None` for any other shape (a plain file, a foreign link).
fn operator_auth_slot_holder(auth_path: &std::path::Path) -> Option<String> {
    std::fs::read_link(auth_path)
        .ok()
        .and_then(|target| clauth_auth_store_owner(&target))
}

/// Read and parse the operator's `auth.json` into a chain-bearing auth, or the
/// capture's refusal for an absent, unreadable, unparseable or chainless file.
fn read_codex_operator_auth(auth_path: &std::path::Path) -> Result<crate::codex_auth::CodexAuth> {
    let raw = match std::fs::read(auth_path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            bail!(
                "no codex login to capture: {} does not exist. Run `codex login` first",
                auth_path.display()
            )
        }
        Err(e) => return Err(e).with_context(|| format!("failed to read {}", auth_path.display())),
    };
    let parsed = crate::codex_auth::CodexAuth::parse(&raw).with_context(|| {
        format!(
            "failed to parse {}: codex writes this file in place, so a login caught \
             mid-write reads half-written. Re-run the capture",
            auth_path.display()
        )
    })?;
    if parsed.refresh_token().is_none() {
        bail!(
            "{} holds no ChatGPT token chain (an API-key-only setup?). Only a \
             `codex login` chain can be captured",
            auth_path.display()
        );
    }
    Ok(parsed)
}

/// Whether the operator's own codex login is capturable right now: the
/// operator home resolves, its store mode is the file default, `auth.json`
/// exists and parses with a refresh token, and the slot is not already a link
/// into a clauth profile store. The read-only half of
/// [`codex_login_capture_at`]'s refusals, sharing its reads — the row's
/// visibility and the capture run on one predicate, so the row can never show
/// where the capture would refuse.
pub(crate) fn codex_operator_login_capturable() -> bool {
    let Ok(operator) = codex_operator_capture_home() else {
        return false;
    };
    let auth_path = operator.join("auth.json");
    // A slot already adopted (a link into a clauth profile store) is captured,
    // never capturable.
    if operator_auth_slot_holder(&auth_path).is_some() {
        return false;
    }
    read_codex_operator_auth(&auth_path).is_ok()
}

/// Refuse a re-auth that would silently swap the ChatGPT account under a
/// profile's name — usage, chain slots, and the operator's own mental model
/// are all keyed on the profile, so a different `account_id` is almost always
/// a wrong target, and the fix (a new name, or a delete first) is cheap. An
/// unreadable or chainless existing store, or an incoming login with no
/// account id, is exempt: re-auth is the repair for exactly those states.
/// Shared by adopt-capture and the browser login.
fn refuse_codex_account_swap(canonical: &str, incoming: Option<&str>) -> Result<()> {
    let Some(new) = incoming else { return Ok(()) };
    let Ok(bytes) = std::fs::read(profile_dir(&ProfileName::from(canonical))?.join("auth.json"))
    else {
        return Ok(());
    };
    let Ok(existing) = crate::codex_auth::CodexAuth::parse(&bytes) else {
        return Ok(());
    };
    if let Some(old) = existing.account_id()
        && old != new
    {
        bail!(
            "'{canonical}' stores ChatGPT account {old}, but this login is account \
             {new}. Log into a new profile, or delete '{canonical}' first"
        );
    }
    Ok(())
}

/// Land a codex chain into `profiles/<name>/auth.json` (atomic, 0600), stamp
/// the self-describing harness marker, and add the name to the roster —
/// returning the store path. The shared core of every codex-profile creation
/// (adopt-capture and browser login), always called inside a
/// [`CodexState::update`] closure so the roster write and the store write
/// land under one lock.
fn write_codex_profile_store(
    state: &mut crate::codex_profiles::CodexState,
    name: &str,
    raw: &[u8],
) -> Result<std::path::PathBuf> {
    let profile_name = ProfileName::from(name);
    // `refuse_codex_account_swap` blocks a different account on re-auth, so only
    // a fresh capture moves the codex identity. `before` reads the stored
    // `auth.json` guard-free, so an unreadable one reads `None` and a
    // same-account re-capture over it drops the shared cache once; the next poll
    // refills it.
    let before = read_codex_identity(&profile_name);
    let after = crate::codex_auth::CodexAuth::parse(raw)
        .ok()
        .and_then(|a| a.account_id().map(str::to_string));
    // Forget-first: the shared cache drops before the `auth.json` write below, so
    // a failed write leaves the old account's identity on disk with its reading
    // gone until the next poll — codex has no durable verdict to lose.
    if before != after {
        forget_disk(&profile_name, Half::Codex);
    }
    let dir = profile_dir(&profile_name)?;
    crate::profile::mkdir_700(&dir)
        .with_context(|| format!("failed to create {}", dir.display()))?;
    let store = dir.join("auth.json");
    crate::profile::atomic_write_600(&store, raw)
        .with_context(|| format!("failed to write {}", store.display()))?;
    let config_path = dir.join("config.toml");
    if !config_path.exists() {
        crate::profile::atomic_write_600(&config_path, "harness = \"codex\"\n")
            .with_context(|| format!("failed to write {}", config_path.display()))?;
    }
    state.add_profile(name)?;
    // The account change already dropped the shared cache above, so the re-seed
    // finds nothing.
    if before != after {
        note_forget(&profile_name, Half::Codex);
    }
    // Seed the last-known-good belt from this fresh, well-formed chain and
    // retire any stale no-replay memo and quarantine verdict: a capture/login
    // is an out-of-band store write, and without this the belt could later
    // restore a chain SUPERSEDED by the one just written, a memo from a
    // pre-capture attempt could block the new token, and the old chain's
    // death sentence would keep the fresh one out of every walk.
    crate::codex_auth::record_lkg(name, raw);
    crate::codex_auth::forget_attempt(name);
    crate::codex_auth::clear_quarantine(name);
    Ok(store)
}

/// The browser login's pre-browser gate: charset, then refuse ONLY a
/// cross-harness clash (an own-roster codex name is a re-auth, exempt — the
/// full check under the lock allows it). Returns the name the landing writes
/// under and, for a profile the codex roster holds, the [`ProfileId`] it
/// re-auths. Extracted so the "own-roster passes, cross-harness refuses" rule
/// is testable without opening a real browser; the TUI's `re-login` shares it.
pub(crate) fn codex_browser_preflight(name: &str) -> Result<(String, Option<ProfileId>)> {
    let trimmed = validate_name_chars(name)?.to_string();
    validate_foreign_harness_free(&trimmed, Harness::Codex)?;
    crate::codex_profiles::CodexState::update(|state| match state.canonical_name(&trimmed) {
        Some(canonical) => {
            let id = state.claim_id(&canonical)?;
            Ok((canonical, id))
        }
        None => Ok((trimmed, None)),
    })
}

/// Whether the operator capture may re-auth a name the codex roster already
/// holds: the CLI's `clauth login --codex` creates-or-re-auths; the TUI's
/// `+ new` create only ever creates, so a name either roster holds refuses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CodexLandMode {
    /// The CLI: create when the name is free, re-auth when it exists.
    CreateOrReauth,
    /// The TUI `+ new` create: the name must not exist in either roster.
    CreateOnly,
}

/// The shared landing every browser login calls after the flow minted a chain:
/// guard the rotation, refuse a live session or account swap, write the store.
/// `account` is what [`codex_browser_preflight`] found under `name`: `Some`
/// re-auths that profile only while `name` still holds it, so one deleted,
/// recreated or renamed during the browser trip receives nothing; `None`
/// creates `name`, refusing it once either roster holds it.
pub(crate) fn land_codex_login(
    name: &str,
    outcome: &crate::codex_login::CodexLoginOutcome,
    account: Option<&ProfileId>,
) -> Result<String> {
    let _rotation_guard = crate::runtime::RotationGuard::acquire(&ProfileName::from(name))?;
    let account_id = crate::codex_auth::CodexAuth::parse(&outcome.auth_json)
        .ok()
        .and_then(|a| a.account_id().map(str::to_string));
    crate::codex_profiles::CodexState::update(|state| {
        match account {
            Some(id) if state.id(name) != Some(id) => {
                bail!("codex profile '{name}' no longer exists");
            }
            Some(_) => {}
            None => {
                if let Some(canonical) = state.canonical_name(name) {
                    bail!("codex profile '{canonical}' already exists. Create it under a new name");
                }
                validate_profile_name(name, Harness::Codex, None)?;
            }
        }
        if crate::runtime::has_live_marker(&ProfileName::from(name)) {
            bail!("'{name}' has a live codex session. Close it before re-authenticating");
        }
        if account.is_some() {
            refuse_codex_account_swap(name, account_id.as_deref())?;
        }
        write_codex_profile_store(state, name, &outcome.auth_json)?;
        Ok(name.to_string())
    })
}

/// `clauth login <name> --codex --browser` — mint a FRESH codex chain via
/// codex's own PKCE flow and land it as a new profile, without touching
/// `~/.codex`. Unlike the adopt-capture this is not a second carrier of an
/// existing chain — it is a brand-new login clauth alone holds.
pub(crate) fn codex_login_browser(name: &str) -> Result<()> {
    let (canonical, outcome) = codex_browser_login_with(name, || {
        crate::codex_login::login_with(|url| {
            outln!("clauth: opening {url}");
            outln!("clauth: if the browser did not open, paste that URL into it");
        })
    })?;

    outln!("clauth: logged a fresh codex chain into codex profile '{canonical}'");
    if let Some(acc) = outcome.account_id {
        outln!("clauth: ChatGPT account {acc}");
    }
    outln!("clauth: run it with `clauth start {canonical}`. Your own ~/.codex is untouched");
    Ok(())
}

/// [`codex_login_browser`]'s round trip and landing, with the browser flow
/// injected: what the preflight found under `name` before `trip` is what the
/// landing re-auths, or creates when it found nothing.
pub(crate) fn codex_browser_login_with(
    name: &str,
    trip: impl FnOnce() -> Result<crate::codex_login::CodexLoginOutcome>,
) -> Result<(String, crate::codex_login::CodexLoginOutcome)> {
    let (canonical, account) = codex_browser_preflight(name)?;
    let outcome = trip()?;
    let canonical = land_codex_login(&canonical, &outcome, account.as_ref())?;
    Ok((canonical, outcome))
}

/// The home the OPERATOR's codex reads: an explicit non-empty `CODEX_HOME`,
/// else `~/.codex`. A `CODEX_HOME` naming a clauth-built session home refuses
/// — inside a `clauth start` codex session "the operator's login" resolves to
/// some profile's store, and capturing a sibling profile's chain is never
/// what this verb means.
fn codex_operator_home() -> Result<std::path::PathBuf> {
    if let Some(dir) = std::env::var_os("CODEX_HOME").filter(|d| !d.is_empty()) {
        let dir = std::path::PathBuf::from(dir);
        if crate::runtime::is_codex_home_path(&dir) {
            bail!(
                "CODEX_HOME points into a clauth codex session home. Run the capture \
                 from a shell outside `clauth start`, where ~/.codex (or your own \
                 CODEX_HOME) holds the operator's login"
            );
        }
        return Ok(dir);
    }
    default_codex_operator_home()
}

/// The home codex reads with no `CODEX_HOME` set: `~/.codex`.
pub(crate) fn default_codex_operator_home() -> Result<std::path::PathBuf> {
    Ok(crate::profile::home_dir()?.join(".codex"))
}

/// The codex profile owning a clauth auth store path
/// (`…/profiles/<name>/auth.json`), or `None` for any other shape.
fn clauth_auth_store_owner(target: &std::path::Path) -> Option<String> {
    if target.file_name()? != "auth.json" {
        return None;
    }
    let dir = target.parent()?;
    if dir.parent()?.file_name()? != "profiles" {
        return None;
    }
    Some(dir.file_name()?.to_str()?.to_string())
}

/// Replace the operator's `auth.json` with a symlink to `store`, atomically:
/// the link is created at a staging sibling and renamed over the file, so no
/// observer meets a missing slot. `false` — never an error — when the host
/// cannot create symlinks; the caller owns saying what that costs.
fn adopt_operator_auth_slot(auth_path: &std::path::Path, store: &std::path::Path) -> bool {
    let tmp = crate::profile::tmp_sibling(auth_path);
    #[cfg(unix)]
    let linked = std::os::unix::fs::symlink(store, &tmp).is_ok();
    #[cfg(windows)]
    let linked = std::os::windows::fs::symlink_file(store, &tmp).is_ok();
    #[cfg(not(any(unix, windows)))]
    let linked = false;
    if !linked {
        return false;
    }
    // Windows cannot rename over an existing file; the remove narrows the
    // atomic swap to a remove+rename there, which is the platform's best.
    #[cfg(windows)]
    let _ = std::fs::remove_file(auth_path);
    if std::fs::rename(&tmp, auth_path).is_ok() {
        true
    } else {
        let _ = std::fs::remove_file(&tmp);
        false
    }
}

/// The operator's `cli_auth_credentials_store`, read tolerantly from the
/// operator home's `config.toml` — `None` when the file or key is absent
/// (codex defaults to the file store) or the TOML does not parse (the capture
/// then proceeds on the file-store assumption and fails honestly on the
/// read).
fn codex_operator_store_mode(operator: &std::path::Path) -> Option<String> {
    let raw = std::fs::read_to_string(operator.join("config.toml")).ok()?;
    let parsed: toml::Value = toml::from_str(&raw).ok()?;
    parsed
        .get("cli_auth_credentials_store")
        .and_then(toml::Value::as_str)
        .map(str::to_string)
}

/// `clauth disable <name>` — mark `name` as user-disabled (see
/// [`Profile::disabled`]): invisible to the fallback-chain walk, the
/// usage/rotation scheduler, and the daemon status feed by default, while its
/// profile directory and stored credentials stay on disk untouched. Refuses
/// when `name` is the global active profile or holds a live `clauth start`
/// session, naming the blocker — a disabled account must never be reachable
/// as an active target, so both gates run before any write.
///
/// Idempotent: an already-disabled account returns `Ok(false)` with no write
/// and no error, checked BEFORE the blocker gates so re-running `disable` on
/// an account that's already off never trips them (e.g. one that's also
/// currently active from before this feature). Returns `Ok(true)` when it
/// flips the flag and persists.
pub(crate) fn disable_profile(config: &mut AppConfig, name: &ProfileName) -> Result<bool> {
    with_state_lock(|_held| {
        let profile = config
            .find(name)
            .with_context(|| format!("profile '{name}' not found"))?;
        if profile.is_disabled() {
            return Ok(false);
        }
        if config.is_active(name) {
            bail!("'{name}' is the active account, switch away first");
        }
        if crate::runtime::has_live_session(name) {
            bail!("'{name}' has a live session, close it first");
        }
        let profile = config.find_mut(name).context("profile not found")?;
        profile.disabled = true;
        save_profile_config(profile)?;
        Ok(true)
    })
}

/// `clauth enable <name>` — clear [`Profile::disabled`], restoring `name` to
/// every operational surface. No other side effects: chain slot, env, model
/// settings, and stored credentials are untouched.
///
/// Idempotent: an already-enabled account returns `Ok(false)` with no write
/// and no error. Returns `Ok(true)` when it clears the flag and persists.
pub(crate) fn enable_profile(config: &mut AppConfig, name: &ProfileName) -> Result<bool> {
    with_state_lock(|_held| {
        let profile = config
            .find_mut(name)
            .with_context(|| format!("profile '{name}' not found"))?;
        if !profile.is_disabled() {
            return Ok(false);
        }
        profile.disabled = false;
        save_profile_config(profile)?;
        Ok(true)
    })
}

/// `models` with every id trimmed and a blank one dropped to unset.
fn trimmed_models(models: ModelSettings) -> ModelSettings {
    let trim = |m: Option<String>| {
        m.as_deref()
            .map(str::trim)
            .filter(|m| !m.is_empty())
            .map(str::to_string)
    };
    ModelSettings {
        default: trim(models.default),
        opus: trim(models.opus),
        sonnet: trim(models.sonnet),
        haiku: trim(models.haiku),
        fable: trim(models.fable),
        subagent: trim(models.subagent),
        advisor: trim(models.advisor),
    }
}

pub(crate) fn create_blank_profile(
    config: &mut AppConfig,
    name: String,
    base_url: Option<String>,
    api_key: Option<String>,
    models: ModelSettings,
) -> Result<()> {
    with_state_lock(|_held| {
        validate_profile_name(&name, Harness::Claude, None)?;
        let mut profile = Profile::new(name, base_url, api_key);
        // Part of the same single save as the profile itself — a chained
        // edit-after-create would leave a saved-but-model-less profile behind
        // when the second write fails, reported as a flat "create failed".
        profile.models = trimmed_models(models);
        save_new_profile(&profile)?;
        config.add(profile);
        save_app_state(&config.state)
    })
}

/// Copy every configured setting of `source` onto a new profile named `name`.
///
/// What is DELIBERATELY not copied:
/// - the stored OAuth pair, the usage cache and the fetch/third-party state —
///   all per-login, and a duplicate holds no login yet;
/// - `preferred` and `last_resort`, which are radios across the whole profile
///   list (`toggle_preferred` clears every sibling): copying either would put
///   two profiles in a slot only one may hold, and `fallback.rs` picks the
///   first it finds, so the loser would just vanish silently;
/// - `preferred_days`, for the same reason one day at a time: a copied list
///   would leave two profiles claiming the same weekday, decided by that same
///   first-match.
///
/// The api key IS copied: it is a per-endpoint setting the Setup tab edits like
/// any other field, and a duplicate of an api account with no key cannot talk
/// to anything.
pub(crate) fn duplicate_profile(
    config: &mut AppConfig,
    source: &ProfileName,
    name: String,
) -> Result<()> {
    let new_name = ProfileName::from(name.clone());
    with_state_lock(|_held| {
        let before = read_before_identities(&new_name);
        let src = config.find(source).context("profile not found")?;
        let mut profile = Profile::new(name, src.base_url.clone(), src.api_key.clone());
        profile.auto_start = src.auto_start;
        profile.usage_probe = src.usage_probe;
        profile.usage_probe_interval_ms = src.usage_probe_interval_ms;
        profile.env = src.env.clone();
        profile.models = src.models.clone();
        profile.fallback_threshold = src.fallback_threshold;
        profile.weekly_threshold = src.weekly_threshold;
        profile.max_auto_spend = src.max_auto_spend;
        profile.check_weekly = src.check_weekly;
        profile.check_scoped = src.check_scoped;
        profile.check_credit_cap = src.check_credit_cap;
        profile.bell_threshold = src.bell_threshold;
        profile.disabled = src.disabled;
        // A fresh name holds no login; a reused name (delete-then-recreate)
        // drops its old reading instead of inheriting it.
        let after_oauth = OAuthIdentity::for_values(None, None);
        let plan = endpoint_identity_plan(
            &before,
            &after_oauth,
            &StoredEndpointIdentity {
                base_url: profile.base_url.clone(),
                api_key: profile.api_key.clone(),
                console: None,
                env: profile.env.clone(),
            },
            false,
        );
        forget_disk_plan(&new_name, plan);
        save_new_profile(&profile)?;
        note_forget_plan(&new_name, plan);
        config.add(profile);
        save_app_state(&config.state)
    })
}

/// Set a profile's default `model` (the Setup tab's base model row / the
/// `clauth login --model` flag), preserving any alias overrides already on it.
/// An empty (post-trim) value clears the default, matching the Setup tab's ⏎
/// commit on the model row. A caller that runs this before starting a session
/// (`clauth login`) has the model routed into that session's runtime settings
/// from the first launch.
///
/// Lands only while `name` holds `account`, the [`ProfileId`] the login
/// claimed, on the profile re-read under the lock (the
/// [`edit_profile_preferred_days`] shape): its endpoint, key and env go back
/// to disk as disk has them, never as an earlier read had them.
pub(crate) fn set_profile_default_model(
    config: &mut AppConfig,
    name: &ProfileName,
    account: &ProfileId,
    raw_model: &str,
) -> Result<()> {
    with_state_lock(|held| {
        require_profile_id(name, account, held)?;
        let mut fresh = load_profile(name)?;
        let trimmed = raw_model.trim();
        fresh.models.default = (!trimmed.is_empty()).then(|| trimmed.to_string());
        save_profile_config(&fresh)?;
        if let Some(profile) = config.find_mut(name) {
            profile.models = fresh.models.clone();
        }
        if config.is_active(name) {
            // Model-only, as in `edit_profile_model`: the profile's own env
            // keys as `prev` strip nothing.
            let prev_env_keys: Vec<String> = fresh.env.keys().cloned().collect();
            apply_profile_to_claude_settings(&fresh, &prev_env_keys)?;
        }
        Ok(())
    })
}

/// Which profile the CURRENT live login (`~/.claude/.credentials.json`)
/// belongs to, fully offline. Two tiers, tried in order:
///
/// **Token equality** (authoritative): the live refresh OR access token equals
/// a profile's stored pair — the live file IS that profile's credential. Never
/// stale, so it wins outright when it hits.
///
/// **Account uuid** (fallback, only when token equality misses): a sibling's
/// genuine re-login through Claude Code mints all-new tokens that match no
/// stored pair, so tier 1 reads UNKNOWN — and a configured `overwrite`/`new`
/// default would then capture that login into the WRONG (active) profile. This
/// tier matches CC's own identity record (`~/.claude.json`'s
/// `oauthAccount.accountUuid`) against each profile's cached anchor
/// (`profile_cache::ACCOUNT_ID_CACHE_FILE`). A missing/unparseable file, a
/// missing block, or a blank uuid on either side yields no match — two blanks
/// never prove identity.
///
/// Returns the owning profile's name — possibly the ACTIVE profile itself (a
/// same-account divergence the adopt path self-heals). Callers wanting a SIBLING
/// compare against the active name. `None` when neither tier proves ownership: a
/// genuinely foreign account, which is a human decision.
///
/// Staleness caveat: CC trusts the cached `oauthAccount` block and does not
/// re-derive it from a swapped credentials file (exactly why clauth strips it on
/// switch — [`crate::claude_json::strip_home_oauth_account`]). So a tier-2 hit is "CC's
/// last booted identity", not fresh proof of the live token's account. For the
/// divergence callers that can only bias the verdict conservatively: pointing at
/// a SIBLING routes the divergence to the banner (user decides), and pointing at
/// the active profile is filtered out by the caller (`note_divergence` drops an
/// owner equal to active) — the same as no match, so the configured default
/// applies unchanged, and the worst case is the banner. `clauth __oauth-token`
/// is the exception: it takes an owner equal to the active profile as the
/// permission to print that profile's live token, so a stale block naming the
/// active account in front of another account's login prints that login.
pub(crate) fn identify_live_login_owner(config: &AppConfig) -> Option<ProfileName> {
    let live = read_claude_credentials().ok().flatten()?;
    let live_access = live.access_token().filter(|t| !t.is_empty());
    let live_refresh = live.refresh_token().filter(|t| !t.is_empty());

    // Tier 1 — token equality: authoritative, never stale.
    if let Some(owner) = config.profiles.iter().find(|p| {
        (live_refresh.is_some() && p.refresh_token() == live_refresh)
            || (live_access.is_some() && p.access_token() == live_access)
    }) {
        return Some(owner.name.clone());
    }

    // Tier 2 — account uuid: a sibling's CC re-login mints fresh tokens tier 1
    // can't recognize, so match CC's cached identity against the anchor instead.
    let live_uuid = crate::claude_json::home_oauth_account_uuid()?;
    config.profiles.iter().find_map(|p| {
        let anchor = crate::profile_cache::load_profile_cache::<AccountId>(
            &p.name,
            crate::profile_cache::ACCOUNT_ID_CACHE_FILE,
        )?;
        (!anchor.trim().is_empty() && anchor == live_uuid).then(|| p.name.clone())
    })
}

/// Returns the profile `live` belongs to, over the two shapes a stored login
/// takes.
///
/// A rotating pair is matched on its `refresh_token`, the only stable identity
/// it has: its access token rotates, and matching that would produce false
/// misses and duplicate profiles.
///
/// A `claude setup-token` mint carries NO refresh token, so the first tier
/// cannot see one at all and every mint read as an unknown credential. Its
/// access token IS its stable identity — a mint never rotates, which is the
/// whole reason the split installs it — so the second tier matches that against
/// each profile's stored sidecar. Only a [`SidecarKind::Mint`] qualifies: a
/// rolling bearer is re-stamped hourly and a mis-fill is a copy of a pair the
/// first tier already answers.
pub(crate) fn find_matching_oauth_profile(
    config: &AppConfig,
    live: Option<&ClaudeCredentials>,
) -> Option<ProfileName> {
    let live = live?;
    if let Some(live_refresh) = live.refresh_token().filter(|t| !t.is_empty()) {
        return config
            .profiles
            .iter()
            .find(|p| p.refresh_token() == Some(live_refresh))
            .map(|p| p.name.clone());
    }
    let live_access = live.access_token().filter(|t| !t.is_empty())?;
    config
        .profiles
        .iter()
        .find(|p| match crate::claude::sidecar_summary(&p.name) {
            Some((crate::claude::SidecarKind::Mint, oauth)) => {
                oauth.access_token.as_str() == live_access
            }
            _ => false,
        })
        .map(|p| p.name.clone())
}

/// What committing a [`CaptureSnapshot`] must do to the profile's identity
/// anchor. Three-valued rather than an `Option`, so "leave the anchor untouched"
/// is its own spelling and can never be mistaken for "no proven identity".
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AnchorAction {
    /// The snapshot's OAuth login provably belongs to this account uuid — an
    /// interactive login's own `/profile` probe. Write it on commit.
    Proven(AccountId),
    /// The snapshot proves no identity (an off-disk read, a failed probe):
    /// remove the anchor on commit, so the old account's cannot vouch for the
    /// login the snapshot just installed.
    Unproven,
    /// The snapshot's OAuth login is the profile's own, carried unchanged (an
    /// api re-key): leave the anchor untouched.
    Unchanged,
}

#[derive(Debug, Clone)]
pub(crate) struct CaptureSnapshot {
    pub(crate) credentials: Option<ClaudeCredentials>,
    pub(crate) base_url: Option<String>,
    pub(crate) api_key: Option<String>,
    /// What the commit must do to the anchor. Travels with the snapshot so
    /// whichever function COMMITS it acts — including the paths that park the
    /// snapshot in a confirm modal first.
    pub(crate) anchor: AnchorAction,
    /// The login probe body's plan, when the snapshot came from an interactive
    /// OAuth login whose probe ran. Rides the snapshot so a confirm-gated
    /// re-login still seeds it at commit time. `None` for every non-login
    /// capture (a live read, an api re-key) and a failed probe.
    pub(crate) plan: Option<PlanInfo>,
}

/// The login Claude Code uses now, as a snapshot. On macOS CC reads the
/// Keychain first, so a login there that no store saved is the one to
/// capture; otherwise the credentials file's. A `security` read on macOS, so
/// a TUI caller runs it on a worker. Only a capture of "the login in use"
/// reads it: a divergence resolves the diverged file
/// ([`capture_live_file_snapshot`]).
pub(crate) fn capture_snapshot() -> Result<CaptureSnapshot> {
    #[cfg(target_os = "macos")]
    if crate::keychain::enabled()
        && let Some(login) = crate::claude::read_bare_keychain_item()
            .as_ref()
            .and_then(crate::claude::unsaved_bare_item_login)
    {
        return snapshot_of_login(Some(login));
    }
    capture_live_file_snapshot()
}

/// The live credentials file's login, never the Keychain item's, as a
/// snapshot: what a divergence prompt resolves, since the divergence it
/// names is that file's.
pub(crate) fn capture_live_file_snapshot() -> Result<CaptureSnapshot> {
    snapshot_of_login(read_claude_credentials()?)
}

/// `credentials` beside the live endpoint keys, as a capture snapshot.
fn snapshot_of_login(credentials: Option<ClaudeCredentials>) -> Result<CaptureSnapshot> {
    let ClaudeEndpoint { base_url, api_key } = read_claude_endpoint_config()?;
    Ok(CaptureSnapshot {
        credentials,
        base_url,
        api_key,
        // Read off disk, not from a login — this snapshot proves no identity.
        anchor: AnchorAction::Unproven,
        plan: None,
    })
}

/// The snapshot the one OAuth login carrier builds: the minted credentials plus
/// the probe's account uuid (as the anchor) and its plan (as the seed). Shared
/// by the CLI's `run_oauth` and the TUI's `apply_login` so neither can drop the
/// plan.
pub(crate) fn capture_snapshot_from_outcome(
    outcome: crate::oauth_login::LoginOutcome,
) -> CaptureSnapshot {
    CaptureSnapshot {
        credentials: Some(outcome.credentials),
        base_url: None,
        api_key: None,
        anchor: outcome
            .account_uuid
            .map(AnchorAction::Proven)
            .unwrap_or(AnchorAction::Unproven),
        plan: outcome.plan,
    }
}

/// An all-empty snapshot (no OAuth login, no endpoint, no key) holds nothing a
/// profile could authenticate with. Both capture surfaces refuse it rather than
/// persisting a credential-less profile behind a success message.
pub(crate) fn snapshot_is_empty(snapshot: &CaptureSnapshot) -> bool {
    let has_oauth = snapshot
        .credentials
        .as_ref()
        .is_some_and(|c| c.claude_ai_oauth.is_some());
    !has_oauth && snapshot.base_url.is_none() && snapshot.api_key.is_none()
}

/// `clauth capture <name>`: save the login Claude Code is using now as a new
/// profile. That is the way out when the live credentials file holds a login no
/// profile owns (#72) — every other create path refuses over it. Returns
/// whether the new profile became the active account: the first one
/// auto-activates, any later one needs an explicit switch.
pub(crate) fn capture_current_login(config: &mut AppConfig, name: &str) -> Result<bool> {
    let name = validate_capture_name(config, name)?;
    let snapshot = capture_snapshot()?;
    if snapshot_is_empty(&snapshot) {
        bail!("no live login found to capture");
    }
    // The TUI's capture asks before duplicating a login another profile already
    // owns; the CLI has no confirm flow, so it refuses with the owner named.
    if let Some(owner) = find_matching_oauth_profile(config, snapshot.credentials.as_ref()) {
        bail!("these credentials already belong to '{owner}'; switch to it with:  clauth {owner}");
    }
    let becomes_active = config.state.active_profile.is_none();
    capture_into_profile(config, name.to_string(), Default::default(), snapshot)?;
    Ok(becomes_active)
}

/// The name a capture saves into, trimmed: refused when a profile already
/// holds it or it is no valid profile name.
fn validate_capture_name<'a>(config: &AppConfig, name: &'a str) -> Result<&'a str> {
    let name = name.trim();
    if let Some(existing) = config.canonical_name(name) {
        bail!(
            "a profile named '{existing}' already exists; re-authenticate it with:  clauth login {existing}"
        );
    }
    validate_profile_name(name, Harness::Claude, None)?;
    Ok(name)
}

/// Undo a seed when the install's hold does not return `Ok`: the seed lands
/// before the hold's fallible steps and bypasses `write_profile_cache`'s
/// off-roster gate (a new account's directory may not be registered yet), so an
/// error after it would strand a plan for a profile the roster never registered.
/// `arm` runs on every seed attempt under the OAuth gate — a refused pairing or
/// a failed write included — and `release` disarms it once every step inside
/// the hold succeeded, so an unreleased drop (any error return) removes the
/// seeded file, best-effort.
struct SeedCleanupGuard {
    name: Option<ProfileName>,
}

impl SeedCleanupGuard {
    fn new() -> Self {
        Self { name: None }
    }

    fn arm(&mut self, name: &ProfileName) {
        self.name = Some(name.clone());
    }

    fn release(&mut self) {
        self.name = None;
    }
}

impl Drop for SeedCleanupGuard {
    fn drop(&mut self) {
        if let Some(name) = &self.name {
            crate::profile_cache::remove_profile_cache(
                name,
                crate::profile_cache::USAGE_CACHE_FILE,
            );
        }
    }
}

/// A test's stand-in for observing the seed's on-disk order inside an install's
/// hold: called right after the seed block and before the first identity write
/// at each install site, so a test can assert the seeded `usage_cache.json` is
/// already on disk while the new credentials and `config.toml` are not yet
/// written. Registered per profile name, run once, like the anchor compare hook.
#[cfg(test)]
pub(crate) type SeedOrderHook = std::sync::Arc<dyn Fn(&str) + Send + Sync>;

#[cfg(test)]
static SEED_ORDER_HOOK: std::sync::Mutex<Option<std::collections::HashMap<String, SeedOrderHook>>> =
    std::sync::Mutex::new(None);

#[cfg(test)]
pub(crate) fn set_seed_order_hook(profile: &str, f: SeedOrderHook) {
    if let Ok(mut hooks) = SEED_ORDER_HOOK.lock() {
        hooks
            .get_or_insert_with(std::collections::HashMap::new)
            .insert(profile.to_string(), f);
    }
}

#[cfg(test)]
fn run_seed_order_hook(profile: &str) {
    let hook = SEED_ORDER_HOOK
        .lock()
        .ok()
        .and_then(|mut hooks| hooks.as_mut().and_then(|hooks| hooks.remove(profile)));
    if let Some(f) = hook {
        f(profile);
    }
}

/// `models` rides along the same way [`create_profile_from_login`]'s does: the
/// Setup `+ new` form's model block. Every capture-from-a-name-prompt caller
/// passes an empty one.
pub(crate) fn capture_into_profile(
    config: &mut AppConfig,
    name: String,
    models: ModelSettings,
    snapshot: CaptureSnapshot,
) -> Result<()> {
    let CaptureSnapshot {
        credentials,
        base_url,
        api_key,
        anchor,
        plan: seed_plan,
    } = snapshot;
    let name = ProfileName::from(name);
    let seed_name = name.clone();
    with_state_lock(|held| {
        // Under the hold the roster write lands under: a name taken while a
        // login ran is another account, never this create's.
        validate_profile_name(&name, Harness::Claude, None)?;
        let before = read_before_identities(&name);
        // The name may hold a stale anchor, cache or reading (a crash before
        // the record landed, a delete-then-recreate): every half whose identity
        // moves forgets.
        let after_anchor = match &anchor {
            AnchorAction::Proven(uuid) => Some(uuid),
            _ => None,
        };
        let after_token = credentials.as_ref().and_then(|c| c.access_token());
        let after_oauth = OAuthIdentity::for_values(after_anchor, after_token);
        let provider = base_url.as_deref().and_then(Provider::from_base_url);
        let after_third_party =
            third_party_identity(provider, api_key.as_deref(), None, base_url.as_deref());
        let after_throughput =
            throughput_identity(&after_oauth, base_url.as_deref(), api_key.as_deref(), None);
        let plan = ForgetPlan {
            oauth: oauth_changed(&before.oauth, &after_oauth),
            oauth_account: names_another_account(before.last_account.as_ref(), &after_oauth),
            third_party: before.third_party != after_third_party,
            throughput: before.throughput != after_throughput,
            ..ForgetPlan::default()
        };
        forget_disk_plan(&name, plan);
        // The seed lands after the forget and before the identity writes: a
        // reloading process re-seeds from disk and finds it, and this process's
        // `forget_memory` re-seed picks it up once the hold drops. Only when the
        // forget plan dropped the OAuth half — a proven same-account re-login
        // forgets nothing, so it must keep the account's live windows instead of
        // replacing them with a plan-only body.
        let mut seed_guard = SeedCleanupGuard::new();
        if plan.oauth
            && let Some(seed) = seed_plan
        {
            crate::usage::seed_plan_cache(
                &name,
                seed,
                match &anchor {
                    AnchorAction::Proven(uuid) => Some(uuid),
                    _ => None,
                },
            );
            seed_guard.arm(&name);
        }
        #[cfg(test)]
        run_seed_order_hook(name.as_str());

        let mut profile = Profile::new(name.to_string(), base_url, api_key);
        profile.models = trimmed_models(models);
        // A fresh profile can sit on an orphan dir holding a stale anchor (a
        // hand-edited roster, a crash before the record landed). A proven
        // re-login onto the SAME uuid keeps it in place: seed_login_anchor's
        // atomic replace at the end of the hold means a compare reads anchored
        // -> anchored, never a gap where the anchor is absent over a present
        // login. Any other install removes it before the store write, so a
        // later failure leaves the profile unanchored, never on the displaced
        // account.
        let same_account = matches!(
            &anchor,
            AnchorAction::Proven(uuid) if before.oauth.anchor.as_deref() == Some(uuid.as_str())
        );
        if !same_account {
            crate::profile_cache::remove_profile_cache(
                &name,
                crate::profile_cache::ACCOUNT_ID_CACHE_FILE,
            );
        }
        crate::oauth::retire_unreadable_spent_record(&name);
        profile.set_credentials(credentials, held);
        save_new_profile(&profile)?;
        note_forget_plan(&name, plan);
        config.add(profile);
        // AUTH-1: a fresh login/capture clears any stale auth-broken quarantine
        // for this name (e.g. a delete-then-relogin of a revoked account).
        config.set_auth_broken(&name, false);

        if config.state.active_profile.is_none() {
            // BEFORE `set_active`, like `finish_switch`: once the marker names
            // the incoming profile the helper answers with its keys, which
            // strips nothing that was already in the file.
            let stale_env_keys = outgoing_env_keys(config);
            if let Err(link_err) = link_profile_credentials(&name) {
                return Err(rollback_first_account_create(config, &name, held, link_err));
            }
            config.state.set_active(Some(name.clone()), held);
            // Same settings write the reauth auto-activate arm makes, for the
            // same reason: this profile IS the active one now, and the live
            // settings may still carry the departed account's entries —
            // `switch_off` clears the marker without touching the file. The
            // strip list captured above removes them.
            let profile = config.find(&name).context("profile not found")?;
            apply_profile_to_claude_settings(profile, &stale_env_keys)?;
        }
        let saved = save_app_state(&config.state);
        // A proven install writes its anchor inside its hold, after the state
        // write registered the profile (the anchor write requires it) and
        // before the hold ends, so a same-account re-login in another process
        // never reads anchored -> unanchored.
        if let AnchorAction::Proven(uuid) = &anchor {
            crate::usage::seed_login_anchor(&seed_name, Some(uuid));
        }
        if saved.is_ok() {
            seed_guard.release();
        }
        saved
    })?;
    Ok(())
}

/// Create a fresh OAuth profile from an in-memory minted login — the Setup
/// tab's capture-then-commit path (`create account` consuming the draft-held
/// mint). One save carries credentials + models so a failed write never leaves
/// a half-configured profile behind; the first profile links + activates
/// exactly like [`capture_into_profile`].
pub(crate) fn create_profile_from_login(
    config: &mut AppConfig,
    name: String,
    models: ModelSettings,
    credentials: ClaudeCredentials,
    account_uuid: Option<AccountId>,
    seed_plan: Option<PlanInfo>,
) -> Result<()> {
    let name = ProfileName::from(name);
    let seed_name = name.clone();
    with_state_lock(|held| {
        let before = read_before_identities(&name);
        let after_token = credentials.access_token();
        let after_oauth = OAuthIdentity::for_values(account_uuid.as_ref(), after_token);
        let plan = ForgetPlan {
            oauth: oauth_changed(&before.oauth, &after_oauth),
            oauth_account: names_another_account(before.last_account.as_ref(), &after_oauth),
            third_party: before.third_party.is_some(),
            throughput: before.throughput != throughput_identity(&after_oauth, None, None, None),
            ..ForgetPlan::default()
        };
        forget_disk_plan(&name, plan);
        // After the forget and before the identity writes, like every install;
        // only when the forget plan dropped the OAuth half (a same-account
        // re-login forgets nothing and keeps its live windows).
        let mut seed_guard = SeedCleanupGuard::new();
        if plan.oauth
            && let Some(seed) = seed_plan
        {
            crate::usage::seed_plan_cache(&name, seed, account_uuid.as_ref());
            seed_guard.arm(&name);
        }
        #[cfg(test)]
        run_seed_order_hook(name.as_str());

        let mut profile = Profile::new(name.to_string(), None, None);
        profile.models = trimmed_models(models);
        // Same orphan-dir guard as `capture_into_profile`: remove any stale
        // anchor before the store write, unless the proven uuid matches it (a
        // same-account re-anchor keeps it in place for the atomic replace).
        let same_account = match account_uuid.as_ref() {
            Some(uuid) => before.oauth.anchor.as_deref() == Some(uuid.as_str()),
            None => false,
        };
        if !same_account {
            crate::profile_cache::remove_profile_cache(
                &name,
                crate::profile_cache::ACCOUNT_ID_CACHE_FILE,
            );
        }
        crate::oauth::retire_unreadable_spent_record(&name);
        profile.set_credentials(Some(credentials), held);
        save_new_profile(&profile)?;
        note_forget_plan(&name, plan);
        config.add(profile);

        if config.state.active_profile.is_none() {
            // BEFORE `set_active`, like `finish_switch`: once the marker names
            // the incoming profile the helper answers with its keys, which
            // strips nothing that was already in the file.
            let stale_env_keys = outgoing_env_keys(config);
            if let Err(link_err) = link_profile_credentials(&name) {
                return Err(rollback_first_account_create(config, &name, held, link_err));
            }
            config.state.set_active(Some(name.clone()), held);
            // Same settings write + rationale as `capture_into_profile`'s
            // arm: the departed account's entries are still in the live
            // settings, and this profile is the active one now.
            let profile = config.find(&name).context("profile not found")?;
            apply_profile_to_claude_settings(profile, &stale_env_keys)?;
        }
        let saved = save_app_state(&config.state);
        // The draft parked the login's uuid until `create account` fixed the
        // name.
        crate::usage::seed_login_anchor(&seed_name, account_uuid.as_ref());
        if saved.is_ok() {
            seed_guard.release();
        }
        saved
    })?;
    Ok(())
}

/// The zero-account create arm's `link_profile_credentials` refusal (#72): the
/// live credentials file holds a login clauth never saved, and the resolve step
/// the guard's own message points at is unreachable with no active profile. Roll
/// the half-created profile back — dir off disk, in-memory records dropped, so
/// config matches disk again — and name the two actions that CAN save the login.
/// The guard's refusal rides along as the error's cause rather than being
/// masked.
fn rollback_first_account_create(
    config: &mut AppConfig,
    name: &ProfileName,
    held: &StateLockHeld,
    link_err: anyhow::Error,
) -> anyhow::Error {
    let mut rollback_note = String::new();
    match profile_dir(name) {
        Ok(dir) => {
            if let Err(e) = std::fs::remove_dir_all(&dir)
                && e.kind() != std::io::ErrorKind::NotFound
            {
                rollback_note = format!(" (rollback could not remove the profile directory: {e})");
            }
        }
        Err(e) => {
            rollback_note = format!(" (rollback could not resolve the profile directory: {e})");
        }
    }
    config.remove(name, held);
    let outer = format!(
        "profile '{name}' not created, the attempt was rolled back: the live \
         ~/.claude/.credentials.json holds a login no profile owns. Save it first with \
         'clauth capture {name}', or with the '+ capture current login' row on the \
         '+ new' form{rollback_note}"
    );
    // The CLI error printer (`exit_code`) shows the whole anyhow chain in
    // Debug, and the guard's message ends in the divergence-TUI pointer this
    // site must not pass on (no active profile, so no divergence modal to
    // resolve). Chain the guard's own message with that tail stripped: the
    // refusal is unmasked, and the two real actions replace the pointer in the
    // outer message. A link failure that is NOT the guard (a publish error)
    // keeps its original chain untouched.
    match link_err
        .to_string()
        .strip_suffix(&format!("; {} first", crate::format::RESOLVE_IN_TUI))
    {
        Some(trimmed) => anyhow::anyhow!(trimmed.to_string()).context(outer),
        None => link_err.context(outer),
    }
}

/// Capture-name collision (issue #7): replace an EXISTING profile's credential
/// set with the freshly captured snapshot, mutating it in place. Never
/// delete+append — that would duplicate the name and desync `state.profiles`
/// and `fallback_chain`, which both index by name already, so the target
/// simply keeps its chain position, env, model settings, and `auto_start`.
/// `usage_history.jsonl` is a persisted log, not a cache, and is left alone;
/// each half's fetch caches are dropped only when that half's stored identity
/// actually changed (a proven same-account re-login forgets nothing), never
/// unconditionally. The `/profile` TTL clock describes the old account too, so
/// it is expired only when the OAuth half changed — a proven same-account
/// re-login keeps the stamp and its memo — otherwise the swapped-in account's
/// tier stays unfetched (and, with `usage_cache.json` just dropped, unrendered)
/// for up to an hour. A snapshot carrying a proven identity (an interactive
/// login's probe) re-anchors the profile here, on the commit — the confirm-gated
/// relogin parks the snapshot in a modal, so the anchor can only be seeded by
/// whoever finally commits it. `account` is the [`ProfileId`] claimed when the
/// login, capture or adopt started; the write lands only while `name` still
/// holds it.
pub(crate) fn overwrite_captured_profile(
    config: &mut AppConfig,
    name: &ProfileName,
    account: &ProfileId,
    snapshot: CaptureSnapshot,
) -> Result<()> {
    overwrite_captured_profile_with(config, name, account, snapshot, false)
}

/// [`overwrite_captured_profile`], first disowning the active account when
/// `deactivate` (the divergence capture): the live link detached and the
/// marker cleared, inside the same hold and after the id check, so a refused
/// overwrite leaves both as they were.
pub(crate) fn overwrite_captured_profile_with(
    config: &mut AppConfig,
    name: &ProfileName,
    account: &ProfileId,
    snapshot: CaptureSnapshot,
    deactivate: bool,
) -> Result<()> {
    let CaptureSnapshot {
        credentials,
        base_url,
        api_key,
        anchor,
        plan: seed_plan,
    } = snapshot;
    // An api re-key keeps the stored OAuth chain unchanged, so it is not a
    // login install: the anchor stays exactly as found. Every other overwrite
    // installs a login that may be a different account.
    let installs_login = !matches!(anchor, AnchorAction::Unchanged);
    let mut oauth_moved = false;
    with_state_lock(|held| {
        require_profile_id(name, account, held)?;
        if deactivate {
            // Best effort, as the divergence capture always was: the overwrite
            // below decides the outcome.
            let _ = crate::claude::detach_credentials_link();
            config.state.set_active(None, held);
            let _ = save_app_state(&config.state);
        }
        let before = read_before_identities(name);
        let was_active = config.is_active(name);
        let profile = config
            .find_mut(name)
            .with_context(|| format!("profile '{name}' not found"))?;
        // An api re-key keeps the pair the store holds now, read under this
        // hold: one read before the login's waits could be another account's
        // (a rename-in) or one a rotation has spent since.
        let credentials = if installs_login {
            credentials
        } else {
            let stored = crate::profile::read_profile(name)?;
            if let Some(refusal) = crate::profile::unreadable_store_refusal(&stored) {
                bail!("{refusal}");
            }
            stored.credentials.as_ref().cloned()
        };
        // An OAuth reauth's snapshot carries the minted tokens and nothing
        // else (`run_oauth`), so a uniform replace strips the profile's
        // endpoint and key — the login was about the chain, and its side effect
        // deleted the credential its inference actually runs on. A field the
        // snapshot omits keeps the stored one; an api-mode login carries both
        // and still replaces.
        //
        // ANY endpoint, never `is_third_party` (owner ruling 2026-08-30): a
        // recognised provider is a fraction of the endpoints in use, and an
        // unrecognised host loses exactly as much. `has_own_inference_endpoint`
        // is that predicate, shared with the delegate gate so the two rulings
        // cannot drift; it also refuses to preserve an endpoint with no
        // credential behind it, which would leave the freshly minted ANTHROPIC
        // bearer pointed at that host (`was_active` writes the endpoint into
        // the live settings at once).
        //
        // The endpoint and the key are preserved as a PAIR, and only when the
        // snapshot carries neither. Per-field fallback re-pairs one vendor's
        // stored key with another vendor's incoming host — a live-read
        // snapshot (the divergence adopt) can fill either field alone — and
        // that key then travels to a host it does not belong to.
        let keep_stored_endpoint = base_url.is_none()
            && api_key.is_none()
            && crate::claude::has_own_inference_endpoint(profile);
        let (base_url, api_key) = if keep_stored_endpoint {
            (profile.base_url.clone(), profile.api_key.clone())
        } else {
            (base_url, api_key)
        };
        let provider = base_url.as_deref().and_then(Provider::from_base_url);
        profile.base_url = base_url;
        profile.api_key = api_key;
        profile.provider = provider;
        // Same rule as `edit_profile_endpoint`: a reauth replaces the credential
        // set, and the console session is one of them.
        if provider != Some(Provider::Alibaba) {
            profile.console = None;
        }
        // The changed halves, from the values about to be written. The OAuth
        // identity follows the anchor action: a proven re-login re-anchors the
        // SAME uuid (so a same-account re-login forgets nothing), an unproven
        // install removes the anchor while a login stays, and an api re-key
        // carries the anchor unchanged.
        let after_anchor = match &anchor {
            AnchorAction::Proven(uuid) => Some(uuid),
            AnchorAction::Unproven => None,
            AnchorAction::Unchanged => before.oauth.anchor.as_ref(),
        };
        let after_token = credentials.as_ref().and_then(|c| c.access_token());
        let after_oauth = OAuthIdentity::for_values(after_anchor, after_token);
        let plan = endpoint_identity_plan(
            &before,
            &after_oauth,
            &StoredEndpointIdentity {
                base_url: profile.base_url.clone(),
                api_key: profile.api_key.clone(),
                console: profile.console.clone(),
                env: profile.env.clone(),
            },
            credentials.is_some(),
        );
        oauth_moved = plan.oauth;
        forget_disk_plan(name, plan);
        // After the forget and before the identity writes, like every install;
        // only when the forget plan dropped the OAuth half (a same-account
        // re-login forgets nothing and keeps its live windows).
        let mut seed_guard = SeedCleanupGuard::new();
        if plan.oauth
            && let Some(seed) = seed_plan
        {
            crate::usage::seed_plan_cache(
                name,
                seed,
                match &anchor {
                    AnchorAction::Proven(uuid) => Some(uuid),
                    _ => None,
                },
            );
            seed_guard.arm(name);
        }
        #[cfg(test)]
        run_seed_order_hook(name.as_str());
        // A proven re-login onto the SAME uuid keeps the old anchor in place
        // through the store write: seed_login_anchor's atomic replace at the
        // end of the hold means a compare reads anchored -> anchored, never a
        // gap where the anchor is absent over a present login. Every other
        // login install removes the old anchor BEFORE the store write, inside
        // the same hold that commits the credentials: a later failure in this
        // hold (the relink, the settings write) leaves the profile unanchored,
        // never on the account the overwrite just displaced. A proven uuid
        // re-anchors at the end of this same hold.
        let same_account = matches!(
            &anchor,
            AnchorAction::Proven(uuid) if before.oauth.anchor.as_deref() == Some(uuid.as_str())
        );
        if installs_login && !same_account {
            crate::profile_cache::remove_profile_cache(
                name,
                crate::profile_cache::ACCOUNT_ID_CACHE_FILE,
            );
        }
        profile.set_credentials(credentials, held);
        crate::oauth::retire_unreadable_spent_record(&profile.name);
        save_profile(profile)?;
        note_forget_plan(name, plan);

        // A disabled profile's creds are still captured above (the operator
        // asked for that), but it must never become the active account this
        // way — reachable via login → switch away → disable → delete the
        // active (clears `active_profile` to None) → `clauth login
        // <disabled>` (the documented revoked-token recovery) auto-activating
        // it. `is_disabled` is re-read fresh rather than reusing a stale bool
        // from before `save_profile` — nothing above this line touches the
        // flag, but the check must describe the profile as committed.
        let disabled = config.find(name).is_some_and(Profile::is_disabled);
        if config.state.active_profile.is_none() && !disabled {
            // BEFORE `set_active`, like `finish_switch`: once the marker names
            // the incoming profile the helper answers with its keys, which
            // strips nothing that was already in the file.
            let stale_env_keys = outgoing_env_keys(config);
            link_profile_credentials(name)?;
            config.state.set_active(Some(name.clone()), held);
            // Same settings write the `was_active` arm makes, for the same
            // reason: this profile IS the active one now, and an endpoint that
            // never reaches `settings.json` routes the live session at
            // Anthropic while the profile says otherwise. Added with the
            // preserve arm, which made a browser reauth able to leave an
            // endpoint on a profile this branch then activates.
            //
            let profile = config.find(name).context("profile not found")?;
            apply_profile_to_claude_settings(profile, &stale_env_keys)?;
        } else if was_active {
            // The overwritten profile is (and stays) the active one: unlike a
            // brand-new capture, `save_profile` just rewrote credentials.json
            // in place (or removed it, if the snapshot had none — a third-
            // party capture). Relink so the live `.credentials.json` is
            // recreated against the new file, or dropped instead of left
            // dangling when the file is now gone; and re-apply
            // `base_url`/`api_key` to `settings.json` the same way
            // `edit_profile_endpoint` does, so a running `claude` doesn't keep
            // reading the OLD endpoint/token until the next switch.
            //
            // FORCE-links, joining the two sites that already do: this branch
            // has resolved the divergence by definition, since the operator
            // asked for exactly this profile's credentials to be replaced. The
            // guarded call cannot work here — it reads any REGULAR live file as
            // an unresolved re-login and refuses, naming a divergence whose
            // other half `save_profile` overwrote a few lines up, so nothing
            // downstream can resolve it. That made this path unreachable on any
            // host where `create_symlink` degrades to a copy and a regular file
            // is the only shape a live slot ever has (Windows without
            // `SeCreateSymbolicLinkPrivilege`). Cost, accepted: an unsaved
            // re-login for a DIFFERENT account sitting in the live slot is
            // dropped here rather than refused. The forcing variant carries
            // `mcpOAuth` across first, as on every other switch — but ONLY when
            // the new snapshot stored a credentials file to carry it into. A
            // third-party recapture stores none, so the carry no-ops and the
            // live slot's MCP logins go with it. Pre-existing wherever the slot
            // is a symlink (the guard never ran there either); this branch
            // widens it to the hosts where the slot is a regular file.
            force_link_profile_credentials(name)?;
            let profile = config.find(name).context("profile not found")?;
            let prev_env_keys: Vec<String> = profile.env.keys().cloned().collect();
            apply_profile_to_claude_settings(profile, &prev_env_keys)?;
        }
        // AUTH-1: re-authenticating an existing profile (`clauth login <name>`) is
        // the documented recovery for a revoked login — clear its quarantine.
        // Pinned by `reauth_overwrite_clears_broken_flag`.
        config.set_auth_broken(name, false);
        let saved = save_app_state(&config.state);
        if let AnchorAction::Proven(uuid) = &anchor {
            crate::usage::seed_login_anchor(name, Some(uuid));
        }
        if saved.is_ok() {
            seed_guard.release();
        }
        saved
    })?;
    // Outside the closure — see `rename_profile` on the rank order. Skipped when
    // the swap fails, which is imprecise rather than atomic: a failure after
    // `save_profile` leaves the new credentials on disk under the old account's
    // stamp. Only when the OAuth half actually changed: a proven same-account
    // re-login keeps the stamp and its memo.
    if oauth_moved {
        crate::usage::expire_profile_ttl(name);
    }
    Ok(())
}

/// Blank a profile's OAuth login: drop its stored credentials and per-account
/// fetch caches, returning it to the credential-less shell `Profile::new`
/// produces. Keeps name, model, env, and chain slot. When it's the active
/// profile, clear the live `~/.claude` link and deactivate — a credential-less
/// profile can't be meaningfully active, and the honest state is "no active".
pub(crate) fn clear_profile_credentials(config: &mut AppConfig, name: &ProfileName) -> Result<()> {
    with_state_lock(|held| {
        let before = read_before_identities(name);
        let was_active = config.is_active(name);
        let profile = config
            .find_mut(name)
            .with_context(|| format!("profile '{name}' not found"))?;
        // The OAuth half forgets; the provider half is this profile's own and
        // stays (a hybrid's OAuth log out keeps the provider reading). The
        // throughput record survives when the delegates still run on the key.
        // The setup-token sidecar stays too, so a profile that held no pair
        // keeps its identity and its reading.
        let after_oauth = crate::usage::pairless_identity(name);
        let after_throughput = throughput_identity(
            &after_oauth,
            before.base_url.as_deref(),
            before.api_key.as_deref(),
            before.env_token.as_deref(),
        );
        let plan = ForgetPlan {
            oauth: oauth_changed(&before.oauth, &after_oauth),
            oauth_account: names_another_account(before.last_account.as_ref(), &after_oauth),
            throughput: before.throughput != after_throughput,
            ..ForgetPlan::default()
        };
        profile.set_credentials(None, held);
        crate::oauth::retire_unreadable_spent_record(name);
        forget_disk_plan(name, plan);
        save_profile(profile)?;
        // Drop any legacy `.pending` rotation sidecar too: with credentials.json gone,
        // `recover_pending_credentials` would treat the sidecar as a failed commit
        // and resurrect the just-deleted login on next load.
        crate::profile::clear_staged_credentials(name);
        note_forget_plan(name, plan);

        if was_active {
            clear_claude_credentials()?;
            config.state.set_active(None, held);
            save_app_state(&config.state)?;
        }
        Ok(())
    })?;
    // The dropped login's TTL clock is the old account's; a re-login into this
    // shell must pull its own tier now, not an hour from now. Outside the closure
    // — see `rename_profile` on the rank order. The durable stamp is already
    // gone (`forget_disk`), so this clears the memo the CLI (which never drains
    // `forget_memory`) would otherwise leave behind.
    crate::usage::expire_profile_ttl(name);
    Ok(())
}

/// Setup-tab "log out" for an API account: drop the stored api key while keeping
/// the base-url shell so it stays an API account you can re-login. The OAuth arm
/// is [`clear_profile_credentials`]; this one reuses [`edit_profile_endpoint`],
/// which re-derives the provider, drops stale third-party stats, and re-applies
/// the live `settings.json` (removing `ANTHROPIC_AUTH_TOKEN`) when the account is
/// active — so a running `claude` loses the token too. The account stays active:
/// its base url is still wired, only the key is gone.
pub(crate) fn clear_profile_api_key(config: &mut AppConfig, name: &ProfileName) -> Result<()> {
    with_state_lock(|held| {
        let base_url = config.find(name).and_then(|p| p.base_url.clone());
        edit_profile_endpoint(config, name, base_url, None)?;
        if config
            .find(name)
            .is_some_and(|p| p.unreadable_credentials_store.is_some())
        {
            // Disk first: the flag clears only once the store is gone, so a
            // failed delete leaves the log-out row up to retry.
            crate::profile::write_profile_credentials(name, None, held)?;
            if let Some(profile) = config.find_mut(name) {
                profile.set_credentials(None, held);
            }
        }
        Ok(())
    })
}

pub(crate) fn reorder_profile(config: &mut AppConfig, from: usize, to: usize) -> Result<()> {
    if from == to || from >= config.profiles.len() || to >= config.profiles.len() {
        return Ok(());
    }
    with_state_lock(|_held| {
        // Resync to fix length drift from a partial save in a prior session.
        config.sync_state_profiles();
        let profile = config.profiles.remove(from);
        config.profiles.insert(to, profile);
        let name = config.state.profiles.remove(from);
        config.state.profiles.insert(to, name);
        save_app_state(&config.state)
    })
}

fn validate_chain_order(current: &[ProfileName], members: &[ProfileName]) -> Result<()> {
    let mut seen = HashSet::new();
    for member in members {
        if !seen.insert(member) {
            bail!(ChainEditRefusal::new(
                ChainRefusal::OrderInvalid,
                Some(format!("duplicate chain member '{member}'")),
            ));
        }
        if !current.contains(member) {
            bail!(ChainEditRefusal::new(
                ChainRefusal::OrderInvalid,
                Some(format!("extra chain member '{member}'")),
            ));
        }
    }
    for member in current {
        if !members.contains(member) {
            bail!(ChainEditRefusal::new(
                ChainRefusal::OrderInvalid,
                Some(format!("missing chain member '{member}'")),
            ));
        }
    }
    Ok(())
}

/// Resolve a name list against the on-disk roster, case-insensitively, dropping
/// every entry no profile carries. A hand-edited or legacy `fallback_chain`
/// entry is tolerated (pruned only on remove) but must not make the chain
/// un-sendable: reordering drops it the same way `remove` prunes it.
fn resolve_chain(chain: &[ProfileName], roster: &[ProfileName]) -> Vec<ProfileName> {
    chain
        .iter()
        .filter_map(|entry| {
            roster
                .iter()
                .find(|n| n.as_str().eq_ignore_ascii_case(entry.as_str()))
                .cloned()
        })
        .collect()
}

/// Reorder the fallback chain to `members`, which must be a permutation of the
/// current chain. Shared by the Fallback tab's move rows and `POST
/// /api/v1/chain/order`; the permutation is validated here so the two surfaces
/// cannot drift on what a valid order is.
pub(crate) fn set_chain_order(
    config: &mut AppConfig,
    members: &[ProfileName],
) -> Result<Vec<ProfileName>> {
    with_state_lock(|_held| {
        // Same fresh-state rule as `finish_switch`: only the chain is this
        // leg's change, so read the current profiles.toml and change that one
        // field — never re-serialize a possibly-stale in-memory copy. The
        // saved order is what the caller answers with: a member the fresh
        // roster no longer carries drops out of it here.
        let mut state = load_app_state()?;
        let current = resolve_chain(&state.fallback_chain, &state.profiles);
        let resolved = resolve_chain(members, &state.profiles);
        validate_chain_order(&current, &resolved)?;
        state.fallback_chain = resolved.clone();
        save_app_state(&state)?;
        config.state.fallback_chain = resolved.clone();
        Ok(resolved)
    })
}

/// Set one chain member's fallback threshold. Shared by the Fallback tab's
/// threshold editor and `POST /api/v1/chain/threshold`; the range check lives
/// in `fallback::threshold_in_range` so the TUI parser and this action agree on
/// the one band.
pub(crate) fn set_member_threshold(
    config: &mut AppConfig,
    name: &ProfileName,
    value: f64,
) -> Result<()> {
    if !crate::fallback::threshold_in_range(value) {
        bail!(ChainEditRefusal::new(ChainRefusal::BadRequest, None));
    }
    with_state_lock(|_held| {
        // Fresh roster AND fresh chain off disk, not the in-memory copies: the
        // daemon's config can lag a concurrent CLI/TUI edit, and the save
        // would recreate the profile file for a member that is gone, or write
        // a threshold on one the chain just dropped.
        let fresh = load_app_state()?;
        if !fresh.profiles.iter().any(|n| n == name) {
            bail!(ChainEditRefusal::new(ChainRefusal::ProfileNotFound, None));
        }
        if !fresh.fallback_chain.iter().any(|n| n == name) {
            bail!(ChainEditRefusal::new(
                ChainRefusal::NotAMember,
                Some(format!(
                    "'{name}' is not in the fallback chain; add it on the Fallback tab first"
                )),
            ));
        }
        // Same fresh-state rule as `set_chain_order`: re-read the profile off
        // disk so a concurrent edit to another field is not rewound, change
        // only this leg's field, then mirror it into the in-memory profile.
        let mut fresh = load_profile(name)?;
        fresh.fallback_threshold = Some(value);
        save_profile_config(&fresh)?;
        if let Some(profile) = config.find_mut(name) {
            profile.fallback_threshold = Some(value);
        }
        Ok(())
    })
}

/// Set the chain-global wrap-off behaviour. Shared by the Config tab's toggle
/// and `POST /api/v1/chain/wrap-off`; the on-disk key stays `wrap_off` (see
/// `AppState::switch_off_when_spent`).
pub(crate) fn set_wrap_off(config: &mut AppConfig, on: bool) -> Result<()> {
    with_state_lock(|_held| {
        let mut state = load_app_state()?;
        state.switch_off_when_spent = on;
        save_app_state(&state)?;
        config.state.switch_off_when_spent = on;
        Ok(())
    })
}

#[cfg(test)]
#[path = "../tests/inline/actions.rs"]
mod tests;

#[cfg(test)]
#[path = "../tests/inline/mcp_switch.rs"]
mod tests_mcp_switch;
