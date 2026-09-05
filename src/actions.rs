//! Pure-data mutations against `AppConfig` and the live `~/.claude` state.
//!
//! Each function takes already-validated inputs from the TUI layer and applies
//! the change under the cross-process state lock.

use std::collections::BTreeMap;
use std::sync::Arc;

use anyhow::{Context, Result, bail};

use crate::claude::{
    ClaudeEndpoint, apply_profile_to_claude_settings, clear_claude_credentials,
    force_link_profile_credentials, force_snapshot_active_credentials, link_profile_credentials,
    live_diverged_and_unsaved, managed_env_key_label, read_claude_credentials,
    read_claude_endpoint_config, snapshot_active_credentials,
};
use crate::lock::{StateLockHeld, with_state_lock};
use crate::lockorder::RankedMutex;
use crate::oauth;
use crate::out::{out, outln};
use crate::profile::{
    AccountId, AppConfig, ClaudeCredentials, ConsoleCredential, DivergenceChoice, ModelSettings,
    Profile, ProfileName, load_app_state, profile_dir, save_app_state, save_profile,
};
use crate::providers::Provider;
use crate::runtime::RotationGuard;
use crate::spinner::Spinner;

/// ASCII alphanumeric + `-_.@+`, not leading-dot, not empty, not a duplicate
/// (`exclude` exempts the current name for rename-in-place). `@`/`+` let an
/// account be named after its email; both are path-separator-free so the name
/// stays a single `profiles/<name>` segment with no traversal.
pub(crate) fn validate_profile_name(
    name: &str,
    existing: &[&str],
    exclude: Option<&str>,
) -> Result<()> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        bail!("name cannot be empty");
    }
    let valid_chars = trimmed
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '@' | '+'));
    if !valid_chars || trimmed.starts_with('.') {
        bail!("name: letters, digits and - _ . @ + only, and can't start with '.'");
    }
    if existing
        .iter()
        .any(|&n| n.eq_ignore_ascii_case(trimmed) && Some(n) != exclude)
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
fn ensure_switch_target_ok(config: &AppConfig, name: &ProfileName) -> Result<()> {
    // Fresh membership, not just the in-memory list: a caller can hold a config
    // older than a concurrent CLI delete/rename (the daemon reloads once a
    // tick). This must run FIRST — `switch_profile` then calls
    // `force_link_profile_credentials`, which tears the live slot down before
    // `finish_switch` could notice the ghost — so a vanished target bounces
    // here, before any side effect. Runs under the state flock, which makes the
    // on-disk read stable.
    if !crate::profile::is_configured(name)? {
        bail!("profile '{name}' not found");
    }
    let Some(profile) = config.find(name) else {
        bail!("profile '{name}' not found");
    };
    if profile.is_disabled() {
        bail!("'{name}': account is disabled, run `clauth enable {name}`");
    }
    Ok(())
}

pub(crate) fn switch_profile(config: &mut AppConfig, name: &ProfileName) -> Result<()> {
    with_state_lock(|held| {
        ensure_switch_target_ok(config, name)?;
        if config.is_active(name) {
            return Ok(());
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
        if uncaptured_relogin {
            link_profile_credentials(name)?;
        } else {
            force_link_profile_credentials(name)?;
        }
        finish_switch(config, name, held)
    })?;
    // The active account moved: republish the feed for its external readers.
    // Outside the lock — see `daemon::publish_status`.
    crate::daemon::publish_status(config);
    Ok(())
}

/// Discard the live login: force-relink to `target`'s stored creds WITHOUT
/// capturing the foreign live file into any profile. Bypasses the non-force
/// `link_profile_credentials` refuse-guard (which exists to protect an
/// un-captured re-login) precisely because the caller chose to drop it.
pub(crate) fn switch_profile_discard(config: &mut AppConfig, target: &ProfileName) -> Result<()> {
    with_state_lock(|held| {
        ensure_switch_target_ok(config, target)?;
        if config.is_active(target) {
            return Ok(());
        }
        force_link_profile_credentials(target)?;
        finish_switch(config, target, held)
    })?;
    // The active account moved: republish the feed for its external readers.
    // Outside the lock — see `daemon::publish_status`.
    crate::daemon::publish_status(config);
    Ok(())
}

/// Force-snapshot the outgoing creds then force the symlink. CLI prompt path only.
pub(crate) fn switch_profile_reconciled(config: &mut AppConfig, name: &ProfileName) -> Result<()> {
    with_state_lock(|held| {
        ensure_switch_target_ok(config, name)?;
        if config.is_active(name) {
            return Ok(());
        }
        force_snapshot_active_credentials(config)?;
        force_link_profile_credentials(name)?;
        finish_switch(config, name, held)
    })?;
    // The active account moved: republish the feed for its external readers.
    // Outside the lock — see `daemon::publish_status`.
    crate::daemon::publish_status(config);
    Ok(())
}

/// CLI switch: relink (reconciling diverged live file via `[Y/n]` prompt), then
/// prime the 5h window. No token rotation — stale chains rotate lazily on first use.
pub(crate) fn switch_profile_cli(config: AppConfig, canonical: &ProfileName) -> Result<()> {
    let outgoing = config.state.active_profile.as_ref().cloned();

    // Diverged link = CC re-logged and wrote a regular file; must reconcile
    // (capture into outgoing profile) rather than refuse. A logged-out shell is
    // exempt: capturing its blank tokens would destroy the outgoing profile's
    // stored login.
    let reconciled = match outgoing.as_ref() {
        Some(active) => live_diverged_and_unsaved(active)?,
        None => false,
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
        match oauth::ensure_installable(&config, canonical, oauth::refresh_result) {
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
        }
    }

    if reconciled {
        let active = {
            #[allow(clippy::expect_used, reason = "mutex poisoning is unrecoverable")]
            let cfg = config.lock().expect("config mutex poisoned");
            cfg.state
                .active_profile
                .as_deref()
                .unwrap_or("")
                .to_string()
        };
        out!(
            "clauth: '{active}' has a newer login in ~/.claude. save it into '{active}' \
             and switch to '{canonical}'? [Y/n] "
        );
        let mut answer = String::new();
        std::io::stdin().read_line(&mut answer)?;
        let answer = answer.trim().to_ascii_lowercase();
        if answer.is_empty() || answer == "y" || answer == "yes" {
            #[allow(clippy::expect_used, reason = "mutex poisoning is unrecoverable")]
            let mut cfg = config.lock().expect("config mutex poisoned");
            switch_profile_reconciled(&mut cfg, canonical)?;
        } else {
            outln!("clauth: aborted, no changes made");
            return Ok(());
        }
    } else {
        #[allow(clippy::expect_used, reason = "mutex poisoning is unrecoverable")]
        let mut cfg = config.lock().expect("config mutex poisoned");
        switch_profile(&mut cfg, canonical)?;
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
/// Returns `(previous_active, new_active)`.
///
/// Accepted TOCTOU: the divergence classify runs before the locked relink (same
/// shape as the CLI path); a live change in that gap self-heals on the next switch.
///
/// Takes the shared [`crate::profile::ConfigHandle`] (not `&mut AppConfig`)
/// because the AUTH-1 gate below may refresh over HTTP, which must never run
/// under the config mutex. `refresher` is injected so the gate is testable
/// offline (production callers pass [`oauth::refresh_result`]).
pub(crate) fn switch_profile_noninteractive(
    config: &crate::profile::ConfigHandle,
    target: &ProfileName,
    on_divergence: Option<DivergenceChoice>,
    refresher: impl Fn(
        &str,
        Option<&str>,
    ) -> std::result::Result<oauth::TokenResponse, oauth::RefreshError>,
) -> Result<(Option<String>, String)> {
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
        bail!("'{target}': account is disabled, run `clauth enable {target}`");
    }

    // AUTH-1 (Incident C): gate the target before its credentials land in the
    // Keychain — the same gate as the CLI switch, so "a quarantined account is
    // refused as a switch target" holds for EVERY noninteractive entry point
    // (MCP today; any future headless caller inherits it).
    // The already-active profile is exempt for the same reason as the CLI
    // path: nothing new to install, and gating it races a plain `claude`
    // refreshing the symlinked live file (a lost race false-quarantines).
    if previous.as_deref() != Some(target) {
        match oauth::ensure_installable(config, target, refresher) {
            oauth::AuthGate::Ready | oauth::AuthGate::Refreshed => {}
            oauth::AuthGate::Broken => bail!("{}", crate::format::login_expired(target).line()),
            // NOT a CLI stderr path — this is the MCP tool's JSON `reason`, so it
            // keeps the canned line without the status.
            oauth::AuthGate::Transient(e) => {
                bail!("{}", crate::format::refresh_transient(target, &e).line())
            }
        }
    }

    // A logged-out shell is no divergence to resolve: skip the default and take
    // the plain switch, which replaces the empty file.
    let diverged = match previous.as_deref() {
        Some(active) => live_diverged_and_unsaved(&ProfileName::from(active.to_string()))?,
        None => false,
    };

    #[allow(clippy::expect_used, reason = "mutex poisoning is unrecoverable")]
    let config = &mut *config.lock().expect("config mutex poisoned");
    if diverged {
        match on_divergence {
            Some(DivergenceChoice::Overwrite) => switch_profile_reconciled(config, target)?,
            Some(DivergenceChoice::Discard) => switch_profile_discard(config, target)?,
            Some(DivergenceChoice::NewProfile) | None => {
                let active = previous.as_deref().unwrap_or_default();
                bail!(
                    "'{active}' has a login clauth hasn't saved, {}",
                    crate::format::RESOLVE_IN_TUI
                )
            }
        }
    } else {
        switch_profile(config, target)?;
    }

    Ok((previous, target.to_string()))
}

/// Snapshot active creds then clear them so Claude Code can't spend any account.
/// Used by wrap-off mode when the whole chain is exhausted. No-op when no profile
/// is active. A diverged live file is cleared WITHOUT being snapshotted
/// (`snapshot_active_credentials` skips it, keeping the stored identity), so a
/// fresh `/login` is dropped: the TUI gates that on the divergence prompt, while
/// the automatic wrap-off leg accepts the drop, unattended by design.
pub(crate) fn switch_off(config: &mut AppConfig) -> Result<()> {
    with_state_lock(|held| {
        if config.state.active_profile.is_none() {
            return Ok(());
        }
        snapshot_active_credentials(config)?;
        clear_claude_credentials()?;
        // No active account left to show; issue #17 applies here too — a
        // stale identity block is just as wrong once creds are cleared.
        crate::claude_json::strip_home_oauth_account()?;
        config.state.set_active(None, held);
        // Same fresh-state rule as `finish_switch`: only the active marker is
        // this leg's change, so write it onto the current on-disk state rather
        // than a possibly-stale in-memory list.
        let mut state = load_app_state()?;
        state.set_active(None, held);
        save_app_state(&state)
    })?;
    // The active account moved: republish the feed for its external readers.
    // Outside the lock — see `daemon::publish_status`.
    crate::daemon::publish_status(config);
    Ok(())
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
    // issue #17: drop the outgoing account's cached identity so Claude Code
    // re-derives it from the just-relinked credentials instead of showing
    // the wrong account until its next `/login`.
    crate::claude_json::strip_home_oauth_account()?;
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
        let profile = config.find_mut(name).context("profile not found")?;
        let old_api_key = profile.api_key.clone();
        profile.base_url = base_url;
        profile.api_key = api_key;
        // Re-derive the provider — the in-memory config is authoritative until
        // the next disk reload, so a stale value here would keep (or block)
        // third-party fetches against the wrong endpoint. Also clear when only the
        // api key changed for the same provider (rotated key — old stats are stale).
        let provider = profile
            .base_url
            .as_deref()
            .and_then(crate::providers::Provider::from_base_url);
        if provider != profile.provider || (provider.is_some() && profile.api_key != old_api_key) {
            profile.third_party_usage = None;
        }
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
        save_profile(profile)?;

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
pub(crate) fn store_console_login(
    config: &mut AppConfig,
    name: &ProfileName,
    console: ConsoleCredential,
) -> Result<()> {
    with_state_lock(|_held| {
        let profile = config.find_mut(name).context("profile not found")?;
        profile.console = Some(console);
        profile.third_party_usage = None;
        save_profile(profile)?;
        crate::profile_cache::remove_profile_cache(
            name,
            crate::profile_cache::THIRD_PARTY_CACHE_FILE,
        );
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
        save_profile(profile)?;

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

/// Apply a preset (`base_url` + `models`) in a single locked transaction. A
/// preset never carries the api key, so the account's own credential is
/// preserved. Building the full profile state and writing it once — one lock
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
        if provider != profile.provider {
            profile.third_party_usage = None;
        }
        profile.provider = provider;
        save_profile(profile)?;

        if config.is_active(name) {
            let profile = config.find(name).context("profile not found")?;
            let prev_env_keys: Vec<String> = profile.env.keys().cloned().collect();
            apply_profile_to_claude_settings(profile, &prev_env_keys)?;
        }
        Ok(())
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
        let profile = config.find_mut(name).context("profile not found")?;
        // Snapshot before overwrite — a removed key is only stripped from live
        // settings when it appears in `prev` but not in the new `profile.env`.
        let old_env_keys: Vec<String> = profile.env.keys().cloned().collect();
        profile.env = env;
        save_profile(profile)?;

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
/// them: the TUI holds the `config` guard across both calls, and ROTATION ranks
/// outside `Config`, so the acquisition has to happen before the config lock. The
/// guard parameter makes a caller with no lock a compile error; what makes a
/// caller who takes it in the wrong ORDER fail is `lockorder`'s assertion.
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
    with_state_lock(|held| {
        // Same gate delete and disable carry, same predicate and copy: a live
        // session's runtime tree, markers and env paths all live under this
        // directory, so moving it out from under the child breaks the session
        // (the registry rows keep naming the old profile; nothing rekeys them).
        if crate::runtime::has_live_session(old) {
            bail!("'{old}' has a live session, close it first");
        }
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
            let same_dir = old_dir
                .canonicalize()
                .ok()
                .zip(new_dir.canonicalize().ok())
                .is_some_and(|(a, b)| a == b);
            if !same_dir {
                bail!(
                    "'{new}' already has a directory at {} with no account behind it, \
                     delete the directory or pick another name",
                    new_dir.display()
                );
            }
        }
        if old_dir.exists() {
            std::fs::rename(&old_dir, &new_dir)
                .with_context(|| format!("failed to rename profile directory to '{new}'"))?;
        }

        let was_active = config.is_active(old);
        config.rename_all_occurrences(old, new, held);

        save_app_state(&config.state)?;

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

/// `_rotation` is [`rotation_guard_for_mutation`]'s guard for `name`, and
/// `force` does not waive it: `force` waives the live-session gate below, while
/// an in-flight rotation is a different hazard that no confirmation makes safe.
pub(crate) fn delete_profile(
    config: &mut AppConfig,
    name: &ProfileName,
    force: bool,
    _rotation: &RotationGuard,
) -> Result<()> {
    with_state_lock(|held| {
        // Refuse to pull an account out from under a running `clauth start`
        // session (either flavor), checked before any removal so a refused
        // delete is a clean no-op. `--yes` skips the confirm prompt but does NOT
        // override this; only `force` does.
        if !force && crate::runtime::has_live_session(name) {
            bail!("'{name}' has a live session, pass --force to delete it anyway");
        }

        let was_active = config.is_active(name);
        // An active API profile's base_url + api_key (and model-tier keys) live in
        // ~/.claude/settings.json, not the credentials link. Capture its custom
        // env keys before removal so the unwire below can strip those too.
        let active_env_keys: Vec<String> = if was_active {
            config
                .find(name)
                .map(|p| p.env.keys().cloned().collect())
                .unwrap_or_default()
        } else {
            Vec::new()
        };

        // Unwire the active account from the live credentials link + settings.json
        // BEFORE any irreversible local removal. These are fallible external
        // writes: running them first means a failure leaves both the record and
        // the dir intact and fully retryable, rather than stranding the api key in
        // plaintext settings.json with the profile record already gone. A blank
        // profile clears its endpoint/key/model env so the key can't linger and
        // the next session doesn't route to a dead endpoint.
        if was_active {
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
        config.remove(name, held);
        save_app_state(&config.state)?;
        Ok(())
    })?;
    // `remove_dir_all` took the durable stamp with it; the memo would outlive the
    // profile and mute the first `/profile` of a same-name relogin inside the hour.
    // Outside the closure — see `rename_profile` on the rank order.
    crate::usage::expire_profile_ttl(name);
    Ok(())
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
        save_profile(profile)?;
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
        save_profile(profile)?;
        Ok(true)
    })
}

pub(crate) fn create_blank_profile(
    config: &mut AppConfig,
    name: String,
    base_url: Option<String>,
    api_key: Option<String>,
    model: Option<String>,
) -> Result<()> {
    with_state_lock(|_held| {
        let mut profile = Profile::new(name, base_url, api_key);
        // Part of the same single save as the profile itself — a chained
        // edit-after-create would leave a saved-but-model-less profile behind
        // when the second write fails, reported as a flat "create failed".
        profile.models.default = model
            .as_deref()
            .map(str::trim)
            .filter(|m| !m.is_empty())
            .map(str::to_string);
        save_profile(&profile)?;
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
///   first it finds, so the loser would just vanish silently.
///
/// The api key IS copied: it is a per-endpoint setting the Setup tab edits like
/// any other field, and a duplicate of an api account with no key cannot talk
/// to anything.
pub(crate) fn duplicate_profile(
    config: &mut AppConfig,
    source: &ProfileName,
    name: String,
) -> Result<()> {
    with_state_lock(|_held| {
        let src = config.find(source).context("profile not found")?;
        let mut profile = Profile::new(name, src.base_url.clone(), src.api_key.clone());
        profile.auto_start = src.auto_start;
        profile.env = src.env.clone();
        profile.models = src.models.clone();
        profile.fallback_threshold = src.fallback_threshold;
        profile.weekly_threshold = src.weekly_threshold;
        profile.max_auto_spend = src.max_auto_spend;
        profile.check_weekly = src.check_weekly;
        profile.check_scoped = src.check_scoped;
        profile.bell_threshold = src.bell_threshold;
        profile.disabled = src.disabled;
        save_profile(&profile)?;
        config.add(profile);
        save_app_state(&config.state)
    })
}

/// Set a profile's default `model` (the Setup tab's base model row / the
/// `clauth login --model` flag), preserving any alias overrides already on it.
/// An empty (post-trim) value clears the default, matching the Setup tab's ⏎
/// commit on the model row. Persists via [`edit_profile_model`], so a caller
/// that runs this before starting a session (`clauth login`) has the model
/// routed into that session's runtime settings from the first launch.
pub(crate) fn set_profile_default_model(
    config: &mut AppConfig,
    name: &ProfileName,
    raw_model: &str,
) -> Result<()> {
    let mut models = config
        .find(name)
        .map(|p| p.models.clone())
        .unwrap_or_default();
    let trimmed = raw_model.trim();
    models.default = (!trimmed.is_empty()).then(|| trimmed.to_string());
    edit_profile_model(config, name, models)
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
/// last booted identity", not fresh proof of the live token's account. That can
/// only bias the verdict conservatively: pointing at a SIBLING routes the
/// divergence to the banner (user decides), and pointing at the active profile
/// is filtered out by the caller (`note_divergence` drops an owner equal to
/// active) — the same as no match, so the configured default applies unchanged.
/// The tier can never manufacture the one harmful outcome — auto-capturing a
/// sibling's login into the wrong profile — so its worst case is the banner.
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

#[derive(Debug, Clone)]
pub(crate) struct CaptureSnapshot {
    pub(crate) credentials: Option<ClaudeCredentials>,
    pub(crate) base_url: Option<String>,
    pub(crate) api_key: Option<String>,
    /// The account uuid an interactive login's own `/profile` probe saw these
    /// credentials authenticate as. Travels with the snapshot so whichever
    /// function COMMITS it seeds the identity anchor — including the paths that
    /// park the snapshot in a confirm modal first. `None` for a snapshot with no
    /// proven identity (a probe failure, or [`capture_snapshot`] reading live
    /// credentials off disk); that seeds nothing and leaves any existing anchor
    /// alone, exactly as before.
    pub(crate) account_uuid: Option<AccountId>,
}

pub(crate) fn capture_snapshot() -> Result<CaptureSnapshot> {
    let credentials = read_claude_credentials()?;
    let ClaudeEndpoint { base_url, api_key } = read_claude_endpoint_config()?;
    Ok(CaptureSnapshot {
        credentials,
        base_url,
        api_key,
        // Read off disk, not from a login — this snapshot proves no identity.
        account_uuid: None,
    })
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
    let name = name.trim();
    if let Some(existing) = config.canonical_name(name) {
        bail!(
            "a profile named '{existing}' already exists; re-authenticate it with:  clauth login {existing}"
        );
    }
    validate_profile_name(name, &config.names(), None)?;
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
    capture_into_profile(config, name.to_string(), None, snapshot)?;
    Ok(becomes_active)
}

/// `model` rides along the same way [`create_profile_from_login`]'s does: the
/// Setup `+ new` form's typed default model. Every capture-from-a-name-prompt
/// caller passes `None`.
pub(crate) fn capture_into_profile(
    config: &mut AppConfig,
    name: String,
    model: Option<String>,
    snapshot: CaptureSnapshot,
) -> Result<()> {
    let CaptureSnapshot {
        credentials,
        base_url,
        api_key,
        account_uuid,
    } = snapshot;
    let name = ProfileName::from(name);
    let seed_name = name.clone();
    with_state_lock(|held| {
        let mut profile = Profile::new(name.to_string(), base_url, api_key);
        profile.models.default = model
            .as_deref()
            .map(str::trim)
            .filter(|m| !m.is_empty())
            .map(str::to_string);
        profile.set_credentials(credentials, held);
        save_profile(&profile)?;
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
        save_app_state(&config.state)
    })?;
    // Only once the credentials are committed, and only here — no caller seeds
    // its own anchor, so no caller can forget to.
    crate::usage::seed_login_anchor(&seed_name, account_uuid.as_ref());
    Ok(())
}

/// Create a fresh OAuth profile from an in-memory minted login — the Setup
/// tab's capture-then-commit path (`create account` consuming the draft-held
/// mint). One save carries credentials + model so a failed write never leaves
/// a half-configured profile behind; the first profile links + activates
/// exactly like [`capture_into_profile`].
pub(crate) fn create_profile_from_login(
    config: &mut AppConfig,
    name: String,
    model: Option<String>,
    credentials: ClaudeCredentials,
    account_uuid: Option<AccountId>,
) -> Result<()> {
    let name = ProfileName::from(name);
    let seed_name = name.clone();
    with_state_lock(|held| {
        let mut profile = Profile::new(name.to_string(), None, None);
        profile.models.default = model
            .as_deref()
            .map(str::trim)
            .filter(|m| !m.is_empty())
            .map(str::to_string);
        profile.set_credentials(Some(credentials), held);
        save_profile(&profile)?;
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
        save_app_state(&config.state)
    })?;
    // The draft parked the login's uuid until `create account` fixed the name;
    // this is that name, so the anchor lands here rather than at the call site.
    crate::usage::seed_login_anchor(&seed_name, account_uuid.as_ref());
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
/// the per-profile fetch caches (`usage_cache.json`, `third_party_cache.json`,
/// `throughput_cache.json`) are dropped unconditionally, because a swapped
/// credential set can be a different account entirely and stale numbers under
/// it are the worse failure. On the preserve path (endpoint + key kept) the
/// third-party half describes the CURRENT account and is dropped anyway: the
/// provider bar refills on the next scheduler tick, while the throughput
/// history only rebuilds from later delegate runs — a real cost, taken over a
/// wrong-account read on the paths where the credential DID change. The
/// `/profile` TTL clock describes the old account too and is expired for the
/// same reason — otherwise the swapped-in account's tier stays unfetched (and,
/// with `usage_cache.json` just dropped, unrendered) for up to an hour. A
/// snapshot carrying a proven identity (`account_uuid`, from an interactive
/// login's probe) re-anchors the profile here, on the commit — the confirm-gated
/// relogin parks the snapshot in a modal, so the anchor can only be seeded by
/// whoever finally commits it.
pub(crate) fn overwrite_captured_profile(
    config: &mut AppConfig,
    name: &ProfileName,
    snapshot: CaptureSnapshot,
) -> Result<()> {
    let CaptureSnapshot {
        credentials,
        base_url,
        api_key,
        account_uuid,
    } = snapshot;
    with_state_lock(|held| {
        let was_active = config.is_active(name);
        let profile = config
            .find_mut(name)
            .with_context(|| format!("profile '{name}' vanished before overwrite"))?;
        // A browser reauth's snapshot carries the minted tokens and nothing
        // else (`run_oauth_browser`), so a uniform replace strips the profile's
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
        profile.set_credentials(credentials, held);
        profile.provider = provider;
        // Same rule as `edit_profile_endpoint`: a reauth replaces the credential
        // set, and the console session is one of them.
        if provider != Some(Provider::Alibaba) {
            profile.console = None;
        }
        profile.usage = None;
        profile.fetch_status = None;
        profile.third_party_usage = None;
        save_profile(profile)?;

        for file in [
            crate::profile_cache::USAGE_CACHE_FILE,
            crate::profile_cache::THIRD_PARTY_CACHE_FILE,
            // Inert once the credential changes, but this profile may now be a
            // different account entirely — drop it with the rest.
            crate::profile_cache::THIRD_PARTY_AUTH_FILE,
            crate::throughput::THROUGHPUT_CACHE_FILE,
        ] {
            crate::profile_cache::remove_profile_cache(name, file);
        }

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
        save_app_state(&config.state)
    })?;
    // Outside the closure — see `rename_profile` on the rank order. Skipped when
    // the swap fails, which is imprecise rather than atomic: a failure after
    // `save_profile` leaves the new credentials on disk under the old account's
    // stamp. Bounded either way — an unexpired stamp lapses within the hour, and a
    // tick racing the gap between the flock release and this expire spends the
    // stale stamp once or loses a fresh one and re-pulls once.
    crate::usage::expire_profile_ttl(name);
    // Same commit-or-nothing rule for the identity: only credentials this profile
    // now actually holds may be vouched for by its anchor. The same
    // failure-after-`save_profile` window is NOT bounded here the way the stamp's
    // is: the anchor would keep proving the old account against the new pair, and
    // `seed_identity_anchor`'s ride-along is write-if-missing, so nothing corrects
    // it until the next successful login.
    crate::usage::seed_login_anchor(name, account_uuid.as_ref());
    Ok(())
}

/// Blank a profile's OAuth login: drop its stored credentials and per-account
/// fetch caches, returning it to the credential-less shell `Profile::new`
/// produces. Keeps name, model, env, and chain slot. When it's the active
/// profile, clear the live `~/.claude` link and deactivate — a credential-less
/// profile can't be meaningfully active, and the honest state is "no active".
pub(crate) fn clear_profile_credentials(config: &mut AppConfig, name: &ProfileName) -> Result<()> {
    with_state_lock(|held| {
        let was_active = config.is_active(name);
        let profile = config
            .find_mut(name)
            .with_context(|| format!("profile '{name}' not found"))?;
        profile.set_credentials(None, held);
        profile.usage = None;
        profile.fetch_status = None;
        profile.third_party_usage = None;
        save_profile(profile)?;
        // Drop any uncommitted rotation sidecar too: with credentials.json gone,
        // `recover_pending_credentials` would treat the sidecar as a failed commit
        // and resurrect the just-deleted login on next load.
        crate::profile::clear_staged_credentials(name);

        for file in [
            crate::profile_cache::USAGE_CACHE_FILE,
            crate::profile_cache::THIRD_PARTY_CACHE_FILE,
            // Inert once the credential changes, but this profile may now be a
            // different account entirely — drop it with the rest.
            crate::profile_cache::THIRD_PARTY_AUTH_FILE,
            crate::throughput::THROUGHPUT_CACHE_FILE,
        ] {
            crate::profile_cache::remove_profile_cache(name, file);
        }

        if was_active {
            clear_claude_credentials()?;
            config.state.set_active(None, held);
            save_app_state(&config.state)?;
        }
        Ok(())
    })?;
    // The dropped login's TTL clock is the old account's; a re-login into this
    // shell must pull its own tier now, not an hour from now. Outside the closure
    // — see `rename_profile` on the rank order. Skipped when the logout fails,
    // which `clear_claude_credentials` makes imprecise rather than atomic: the
    // stored credentials are already gone by then, with the stamp left to lapse.
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
    with_state_lock(|_held| {
        let base_url = config.find(name).and_then(|p| p.base_url.clone());
        edit_profile_endpoint(config, name, base_url, None)?;
        // The endpoint editor clears the in-memory stats; also drop the on-disk
        // third-party cache so a stale copy can't resurface on reload (no key left
        // to refresh it).
        if let Some(path) = crate::profile_cache::profile_cache_path(
            name,
            crate::profile_cache::THIRD_PARTY_CACHE_FILE,
        ) {
            let _ = std::fs::remove_file(path);
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

#[cfg(test)]
#[path = "../tests/inline/actions.rs"]
mod tests;

#[cfg(test)]
#[path = "../tests/inline/mcp_switch.rs"]
mod tests_mcp_switch;
