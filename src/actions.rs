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
use crate::lock::with_state_lock;
use crate::lockorder::RankedMutex;
use crate::oauth;
use crate::profile::{
    AppConfig, ClaudeCredentials, DivergenceChoice, ModelSettings, Profile, profile_dir,
    save_app_state, save_profile, update_app_state,
};
use crate::providers::Provider;
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
    // Reserved: these bare single tokens are `clauth` subcommands (`clauth
    // daemon`, `clauth status`, `clauth doctor`, ...) — including the two hidden
    // internal ones (`__complete`, `mcp-await-job`). `clauth <reserved>` runs
    // the subcommand, never a switch (see the dispatch in main.rs), so a profile
    // with one of these names would be permanently unreachable by `clauth
    // <name>` — refuse it at creation across every path (CLI login, TUI
    // create/rename, daemon socket rename, fallback-config rename all funnel
    // through here). Case-insensitive: names are matched case-insensitively for
    // dedup below, and the case-sensitive dispatch would otherwise let `Daemon`
    // switch while `daemon` runs the daemon — a footgun, not a feature.
    // (`completions` is NOT reserved: bare `clauth completions` falls through to
    // a switch.)
    const RESERVED: &[&str] = &[
        "daemon",
        "status",
        "doctor",
        "which",
        "start",
        "login",
        "delete",
        "fallback",
        "proxy",
        "resume",
        "run",
        "mcp",
        "__complete",
        "mcp-await-job",
    ];
    if RESERVED.iter().any(|r| r.eq_ignore_ascii_case(trimmed)) {
        bail!("name '{trimmed}' is reserved for the `clauth {trimmed}` command; pick another");
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
fn ensure_switch_target_ok(config: &AppConfig, name: &str) -> Result<()> {
    let Some(profile) = config.find(name) else {
        bail!("profile '{name}' not found");
    };
    // CDX-1: every claude switch primitive funnels through here, so a codex
    // target can never reach the claude link/Keychain machinery (it has no
    // credentials.json to link — the harness dispatch belongs to callers, this
    // is the backstop).
    if profile.is_codex() {
        bail!("profile '{name}' is a codex profile — it switches via the codex path");
    }
    if profile.is_disabled() {
        bail!("'{name}': account is disabled, run `clauth enable {name}`");
    }
    Ok(())
}

pub(crate) fn switch_profile(config: &mut AppConfig, name: &str) -> Result<()> {
    with_state_lock(|| {
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
        let uncaptured_relogin = match config.state.active_profile.as_deref() {
            Some(active) => live_diverged_and_unsaved(active)?,
            None => false,
        };
        snapshot_active_credentials(config)?;
        if uncaptured_relogin {
            link_profile_credentials(name)?;
        } else {
            force_link_profile_credentials(name)?;
        }
        finish_switch(config, name)
    })
}

/// Discard the live login: force-relink to `target`'s stored creds WITHOUT
/// capturing the foreign live file into any profile. Bypasses the non-force
/// `link_profile_credentials` refuse-guard (which exists to protect an
/// un-captured re-login) precisely because the caller chose to drop it.
pub(crate) fn switch_profile_discard(config: &mut AppConfig, target: &str) -> Result<()> {
    with_state_lock(|| {
        ensure_switch_target_ok(config, target)?;
        if config.is_active(target) {
            return Ok(());
        }
        force_link_profile_credentials(target)?;
        finish_switch(config, target)
    })
}

/// Force-snapshot the outgoing creds then force the symlink. CLI prompt path only.
pub(crate) fn switch_profile_reconciled(config: &mut AppConfig, name: &str) -> Result<()> {
    with_state_lock(|| {
        ensure_switch_target_ok(config, name)?;
        if config.is_active(name) {
            return Ok(());
        }
        force_snapshot_active_credentials(config)?;
        force_link_profile_credentials(name)?;
        finish_switch(config, name)
    })
}

/// CLI switch: relink (reconciling diverged live file via `[Y/n]` prompt), then
/// prime the 5h window. No token rotation — stale chains rotate lazily on first use.
pub(crate) fn switch_profile_cli(config: AppConfig, canonical: &str) -> Result<()> {
    let outgoing = config.state.active_profile.clone();

    // Diverged link = CC re-logged and wrote a regular file; must reconcile
    // (capture into outgoing profile) rather than refuse. A logged-out shell is
    // exempt: capturing its blank tokens would destroy the outgoing profile's
    // stored login.
    let reconciled = match outgoing.as_deref() {
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
            oauth::AuthGate::Transient(e) => {
                bail!(
                    "{}",
                    crate::format::refresh_transient(canonical, &e.to_string()).line()
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
        print!(
            "clauth: '{active}' has a newer login in ~/.claude. save it into '{active}' \
             and switch to '{canonical}'? [Y/n] "
        );
        use std::io::Write;
        std::io::stdout().flush()?;
        let mut answer = String::new();
        std::io::stdin().read_line(&mut answer)?;
        let answer = answer.trim().to_ascii_lowercase();
        if answer.is_empty() || answer == "y" || answer == "yes" {
            #[allow(clippy::expect_used, reason = "mutex poisoning is unrecoverable")]
            let mut cfg = config.lock().expect("config mutex poisoned");
            switch_profile_reconciled(&mut cfg, canonical)?;
        } else {
            println!("clauth: aborted, no changes made");
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
    println!("clauth: switched to '{canonical}'");
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
    target: &str,
    on_divergence: Option<DivergenceChoice>,
    refresher: impl Fn(
        &str,
        Option<&str>,
    ) -> std::result::Result<oauth::TokenResponse, oauth::RefreshError>,
) -> Result<(Option<String>, String)> {
    // CDX-1 T6: harness dispatch — a codex target takes the codex path; the
    // AUTH-1 OAuth gate and claude divergence machinery below don't apply to
    // it. Every noninteractive caller (MCP today) is an explicit user
    // decision, so a foreign live login is archived (User-origin semantics)
    // rather than wedging the tool on a refusal.
    {
        #[allow(clippy::expect_used, reason = "mutex poisoning is unrecoverable")]
        let cfg = &mut *config.lock().expect("config mutex poisoned");
        if cfg.find(target).is_some_and(|p| p.is_codex()) {
            let previous = cfg
                .state
                .active_codex_profile
                .as_deref()
                .map(str::to_string);
            codex_switch_profile(cfg, target, ForeignLivePolicy::Archive)?;
            return Ok((previous, target.to_string()));
        }
    }

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
            oauth::AuthGate::Transient(e) => {
                bail!(
                    "{}",
                    crate::format::refresh_transient(target, &e.to_string()).line()
                )
            }
        }
    }

    // A logged-out shell is no divergence to resolve: skip the default and take
    // the plain switch, which replaces the empty file.
    let diverged = match previous.as_deref() {
        Some(active) => live_diverged_and_unsaved(active)?,
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
    with_state_lock(|| {
        if config.state.active_profile.is_none() {
            return Ok(());
        }
        snapshot_active_credentials(config)?;
        clear_claude_credentials()?;
        // No active account left to show; issue #17 applies here too — a
        // stale identity block is just as wrong once creds are cleared.
        crate::claude_json::strip_home_oauth_account()?;
        config.state.active_profile = None;
        // TECH-7: merge the active_profile=None delta into the latest on-disk state
        // so a concurrent `clauth login` that appended a profile is preserved.
        update_app_state(|s| s.active_profile = None)?;
        Ok(())
    })
}

fn finish_switch(config: &mut AppConfig, name: &str) -> Result<()> {
    // Capture outgoing env keys before active_profile is reassigned.
    let prev_env_keys: Vec<String> = config
        .state
        .active_profile
        .as_deref()
        .and_then(|n| config.find(n))
        .map(|p| p.env.keys().cloned().collect())
        .unwrap_or_default();
    let profile = config.find(name).context("profile not found")?;
    apply_profile_to_claude_settings(profile, &prev_env_keys)?;
    // issue #17: drop the outgoing account's cached identity so Claude Code
    // re-derives it from the just-relinked credentials instead of showing
    // the wrong account until its next `/login`.
    crate::claude_json::strip_home_oauth_account()?;
    config.state.active_profile = Some(name.into());
    // TECH-7: persist only the active_profile delta, merged into the latest on-disk
    // state, so a concurrent `clauth login` that appended a profile is not orphaned
    // by this switch's blind rewrite (finding #1).
    let active = config.state.active_profile.clone();
    update_app_state(move |s| s.active_profile = active)?;
    Ok(())
}

pub(crate) fn edit_profile_endpoint(
    config: &mut AppConfig,
    name: &str,
    base_url: Option<String>,
    api_key: Option<String>,
) -> Result<()> {
    with_state_lock(|| {
        // CDX-1 harness immutability: an endpoint (base_url + api_key) is a
        // claude-shaped credential — writing one onto a codex profile would
        // set `provider`/`is_third_party()` and re-enter the excluded fetch
        // legs (same class as the overwrite_captured_profile backstop).
        if config.find(name).is_some_and(|p| p.is_codex()) {
            bail!("profile '{name}' is a codex profile — it has no Anthropic endpoint to edit");
        }
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

/// Persist a profile's model configuration. Re-applies to the live
/// `~/.claude/settings.json` when the profile is active so a running `claude`
/// picks it up on its next settings read. Mirrors [`edit_profile_endpoint`].
pub(crate) fn edit_profile_model(
    config: &mut AppConfig,
    name: &str,
    models: ModelSettings,
) -> Result<()> {
    with_state_lock(|| {
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

/// Persist a profile's custom env map (the Setup-tab field editor). Captures the
/// OLD env keys first so a re-apply to the live `~/.claude/settings.json` strips
/// any key the new map dropped — passing the new keys instead would leak a removed
/// entry into the live file. Mirrors [`edit_profile_model`].
pub(crate) fn edit_profile_env(
    config: &mut AppConfig,
    name: &str,
    env: BTreeMap<String, String>,
) -> Result<()> {
    with_state_lock(|| {
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

pub(crate) fn rename_profile(config: &mut AppConfig, old: &str, new: &str) -> Result<()> {
    with_state_lock(|| {
        let old_dir = profile_dir(old)?;
        if old_dir.exists() {
            std::fs::rename(&old_dir, profile_dir(new)?)
                .with_context(|| format!("failed to rename profile directory to '{new}'"))?;
        }

        let was_active = config.is_active(old);
        config.rename_all_occurrences(old, new);

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

pub(crate) fn delete_profile(config: &mut AppConfig, name: &str, force: bool) -> Result<()> {
    with_state_lock(|| {
        // Refuse to pull an account out from under a running `clauth start`
        // session (either flavor), checked before any removal so a refused
        // delete is a clean no-op. `--yes` skips the confirm prompt but does NOT
        // override this; only `force` does.
        if !force && crate::runtime::has_live_session(name) {
            bail!("'{name}' is running a session, pass --force to delete it anyway");
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
        config.remove(name);
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
pub(crate) fn disable_profile(config: &mut AppConfig, name: &str) -> Result<bool> {
    with_state_lock(|| {
        let profile = config
            .find(name)
            .with_context(|| format!("profile '{name}' not found"))?;
        if profile.is_disabled() {
            return Ok(false);
        }
        if config.is_active(name) {
            bail!("'{name}' is the active account — switch away first");
        }
        if crate::runtime::has_live_session(name) {
            bail!("'{name}' has an open session — close it first");
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
pub(crate) fn enable_profile(config: &mut AppConfig, name: &str) -> Result<bool> {
    with_state_lock(|| {
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
    with_state_lock(|| {
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

/// Set a profile's default `model` (the Setup tab's base model row / the
/// `clauth login --model` flag), preserving any alias overrides already on it.
/// An empty (post-trim) value clears the default, matching the Setup tab's ⏎
/// commit on the model row. Persists via [`edit_profile_model`], so a caller
/// that runs this before starting a session (`clauth login`) has the model
/// routed into that session's runtime settings from the first launch.
pub(crate) fn set_profile_default_model(
    config: &mut AppConfig,
    name: &str,
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
pub(crate) fn identify_live_login_owner(config: &AppConfig) -> Option<String> {
    let live = read_claude_credentials().ok().flatten()?;
    let live_access = live.access_token().filter(|t| !t.is_empty());
    let live_refresh = live.refresh_token().filter(|t| !t.is_empty());

    // Tier 1 — token equality: authoritative, never stale.
    if let Some(owner) = config.profiles.iter().find(|p| {
        (live_refresh.is_some() && p.refresh_token() == live_refresh)
            || (live_access.is_some() && p.access_token() == live_access)
    }) {
        return Some(owner.name.as_str().to_string());
    }

    // Tier 2 — account uuid: a sibling's CC re-login mints fresh tokens tier 1
    // can't recognize, so match CC's cached identity against the anchor instead.
    let live_uuid = crate::claude_json::home_oauth_account_uuid()?;
    config.profiles.iter().find_map(|p| {
        let anchor = crate::profile_cache::load_profile_cache::<String>(
            &p.name,
            crate::profile_cache::ACCOUNT_ID_CACHE_FILE,
        )?;
        let anchor = anchor.trim();
        (!anchor.is_empty() && anchor == live_uuid.as_str()).then(|| p.name.as_str().to_string())
    })
}

/// Returns a profile whose `refresh_token` matches `live`. Matches on refresh
/// token only (stable identity); access tokens rotate and would produce false
/// misses and duplicate profiles.
pub(crate) fn find_matching_oauth_profile<'a>(
    config: &'a AppConfig,
    live: Option<&ClaudeCredentials>,
) -> Option<&'a str> {
    let live_refresh = live?.refresh_token().filter(|t| !t.is_empty())?;
    config
        .profiles
        .iter()
        .find(|p| p.refresh_token() == Some(live_refresh))
        .map(|p| p.name.as_str())
}

/// CAP-2: where a snapshot's credentials CAME FROM, which decides how the
/// captured profile's identity anchor (`account_id.json`) is written. The
/// 2026-07-12 incident follow-up: `clauth login` probed the fresh token's real
/// account, then the capture blindly re-anchored from the LIVE login's hint —
/// an account the minted tokens have nothing to do with — leaving a
/// store/anchor split the adopt gate would later trust.
#[derive(Debug, Clone)]
pub(crate) enum CaptureIdentity {
    /// Browser-minted and probed: the caller asked the API which account the
    /// fresh token belongs to — anchor to exactly that identity.
    Known(crate::usage::AccountIdentity),
    /// The snapshot's bytes were read from the live login, so the live
    /// account hint (`refresh_account_anchor`) describes the same login.
    LiveLogin,
    /// Browser-minted but unprobed (TUI re-login flow): no trustworthy
    /// identity in hand — drop any stale anchor and let the usage fetcher's
    /// first-fetch backfill seed the truth.
    Unknown,
}

#[derive(Debug, Clone)]
pub(crate) struct CaptureSnapshot {
    pub(crate) credentials: Option<ClaudeCredentials>,
    pub(crate) base_url: Option<String>,
    pub(crate) api_key: Option<String>,
    pub(crate) identity: CaptureIdentity,
}

pub(crate) fn capture_snapshot() -> Result<CaptureSnapshot> {
    let credentials = read_claude_credentials()?;
    let ClaudeEndpoint { base_url, api_key } = read_claude_endpoint_config()?;
    Ok(CaptureSnapshot {
        credentials,
        base_url,
        api_key,
        identity: CaptureIdentity::LiveLogin,
    })
}

/// Write the identity anchor for a just-captured login per the snapshot's
/// provenance (see [`CaptureIdentity`]).
fn anchor_captured_login(name: &str, identity: &CaptureIdentity) {
    use crate::profile_cache::{
        ACCOUNT_EMAIL_CACHE_FILE, ACCOUNT_ID_CACHE_FILE, write_profile_cache,
    };
    match identity {
        CaptureIdentity::Known(id) => {
            write_profile_cache(name, ACCOUNT_ID_CACHE_FILE, &id.uuid);
            match &id.email {
                Some(email) => write_profile_cache(name, ACCOUNT_EMAIL_CACHE_FILE, email),
                // No email in the probe: drop any stale one rather than let
                // the pair describe two accounts (backfill re-seeds).
                None => {
                    if let Some(p) =
                        crate::profile_cache::profile_cache_path(name, ACCOUNT_EMAIL_CACHE_FILE)
                    {
                        let _ = std::fs::remove_file(p);
                    }
                }
            }
        }
        CaptureIdentity::LiveLogin => crate::profile_cache::refresh_account_anchor(name),
        CaptureIdentity::Unknown => crate::profile_cache::drop_account_anchor(name),
    }
}

/// CAP-3 (same-account dedup): the profile — other than `exclude` — whose
/// identity anchor already names `identity`'s account. Uuid equality is
/// authoritative; a cached-email match (case-insensitive) catches profiles
/// whose uuid anchor hasn't been backfilled yet. `clauth login` refuses when
/// this returns a sibling: storing one account under two profiles makes them
/// double-poll it into a rate-limit pin (the 2026-07-12 incident, both times).
pub(crate) fn account_owner(
    config: &AppConfig,
    identity: &crate::usage::AccountIdentity,
    exclude: &str,
) -> Option<String> {
    use crate::profile_cache::{
        ACCOUNT_EMAIL_CACHE_FILE, ACCOUNT_ID_CACHE_FILE, load_profile_cache,
    };
    let anchor_matches = |name: &str| -> bool {
        let uuid_match =
            load_profile_cache::<String>(name, ACCOUNT_ID_CACHE_FILE).is_some_and(|u| {
                let u = u.trim();
                !u.is_empty() && u == identity.uuid
            });
        let email_match = identity.email.as_deref().is_some_and(|e| {
            load_profile_cache::<String>(name, ACCOUNT_EMAIL_CACHE_FILE).is_some_and(|c| {
                let c = c.trim();
                !c.is_empty() && c.eq_ignore_ascii_case(e)
            })
        });
        uuid_match || email_match
    };
    // Re-minting the account `exclude` ALREADY holds is a refresh, never a
    // new duplicate — even when a sibling anomalously holds it too. Refusing
    // that case would wedge the double-hold recovery in BOTH directions
    // (each profile's re-login names the other).
    if anchor_matches(exclude) {
        return None;
    }
    config
        .profiles
        .iter()
        .filter(|p| p.name.as_str() != exclude)
        .find(|p| anchor_matches(p.name.as_str()))
        .map(|p| p.name.as_str().to_string())
}

pub(crate) fn capture_into_profile(
    config: &mut AppConfig,
    name: String,
    snapshot: CaptureSnapshot,
) -> Result<()> {
    let CaptureSnapshot {
        credentials,
        base_url,
        api_key,
        identity,
    } = snapshot;
    with_state_lock(|| {
        let mut profile = Profile::new(name.clone(), base_url, api_key);
        profile.credentials = credentials;
        save_profile(&profile)?;
        // CAP-1: anchor the new profile to the login it just captured, so the
        // identity-guarded adopt/follow paths can vouch for it immediately —
        // sourced per the snapshot's provenance (CAP-2).
        if profile.credentials.is_some() {
            anchor_captured_login(&name, &identity);
        }
        config.add(profile);
        // AUTH-1: a fresh login/capture clears any stale auth-broken quarantine
        // for this name (e.g. delete-then-relogin of a revoked account).
        config.set_auth_broken(&name, false);

        // TECH-7: merge only THIS profile's delta into the latest on-disk state —
        // add its name, clear its auth-broken quarantine, and adopt it as active
        // ONLY if no profile is active on disk. Deciding against disk (not the
        // possibly-stale in-memory snapshot) means a concurrent daemon auto-switch's
        // `active_profile` is never clobbered by this login's blind rewrite.
        let name_owned = name.clone();
        let mut adopted_active = false;
        let merged = update_app_state(|s| {
            if !s.profiles.iter().any(|p| p.as_str() == name_owned) {
                s.profiles.push(name_owned.as_str().into());
            }
            s.auth_broken.retain(|n| n.as_str() != name_owned);
            if s.active_profile.is_none() {
                s.active_profile = Some(name_owned.as_str().into());
                adopted_active = true;
            }
        })?;
        // Make the fresh tokens LIVE when this capture targets the active account.
        // Two cases:
        //  - adopted_active: disk had no active and we just adopted this profile → link
        //    its creds (non-force; a foreign live file is protected by the guard).
        //  - RE-AUTH of the already-active profile (`clauth login <active>`): the live
        //    `.credentials.json` is this profile's OWN now-stale Keychain mirror, so it
        //    DIFFERS from the fresh tokens and the non-force guard would refuse. Force
        //    the relink — which on macOS rewrites the Keychain — so a running `claude`
        //    stops reading the dead token ("Not logged in · run /login"). Without this,
        //    a re-auth updated the profile + file but never the Keychain, leaving the
        //    live session broken.
        let active_is_this = merged.active_profile.as_deref() == Some(name.as_str());
        if adopted_active {
            link_profile_credentials(&name)?;
        } else if active_is_this {
            force_link_profile_credentials(&name)?;
        }
        // Adopt the merged active-profile truth (a concurrent switch wins; our
        // adoption only applied when disk had none).
        config.state.active_profile = merged.active_profile;
        Ok(())
    })
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
    account_uuid: Option<String>,
) -> Result<()> {
    let seed_name = name.clone();
    with_state_lock(|| {
        let mut profile = Profile::new(name.clone(), None, None);
        profile.models.default = model
            .as_deref()
            .map(str::trim)
            .filter(|m| !m.is_empty())
            .map(str::to_string);
        profile.credentials = Some(credentials);
        save_profile(&profile)?;
        config.add(profile);

        if config.state.active_profile.is_none() {
            link_profile_credentials(&name)?;
            config.state.active_profile = Some(name.into());
        }
        save_app_state(&config.state)
    })?;
    // The draft parked the login's uuid until `create account` fixed the name;
    // this is that name, so the anchor lands here rather than at the call site.
    crate::usage::seed_login_anchor(&seed_name, account_uuid.as_deref());
    Ok(())
}

/// Capture-name collision (issue #7): replace an EXISTING profile's credential
/// set with the freshly captured snapshot, mutating it in place. Never
/// delete+append — that would duplicate the name and desync `state.profiles`
/// and `fallback_chain`, which both index by name already, so the target
/// simply keeps its chain position, env, model settings, and `auto_start`.
/// `usage_history.jsonl` is a persisted log, not a cache, and is left alone;
/// the per-profile fetch caches (`usage_cache.json`, `third_party_cache.json`,
/// `throughput_cache.json`) describe the OLD account and are dropped so the
/// UI doesn't show stale numbers under the swapped-in credentials. The
/// `/profile` TTL clock describes the old account too and is expired for the
/// same reason — otherwise the swapped-in account's tier stays unfetched (and,
/// with `usage_cache.json` just dropped, unrendered) for up to an hour. A
/// snapshot carrying a proven identity (`account_uuid`, from an interactive
/// login's probe) re-anchors the profile here, on the commit — the confirm-gated
/// relogin parks the snapshot in a modal, so the anchor can only be seeded by
/// whoever finally commits it.
pub(crate) fn overwrite_captured_profile(
    config: &mut AppConfig,
    name: &str,
    snapshot: CaptureSnapshot,
) -> Result<()> {
    // CDX-1 writer-level backstop: claude-shaped credentials must never land
    // in a codex profile (harness immutability — the corrupt hybrid would
    // also re-enter the Anthropic fetch legs). The CLI errors earlier with a
    // friendlier message; this guard covers every other caller.
    if config.find(name).is_some_and(|p| p.is_codex()) {
        bail!(
            "profile '{name}' is a codex profile — re-auth it with `clauth login {name} --codex`"
        );
    }
    let CaptureSnapshot {
        credentials,
        base_url,
        api_key,
        identity,
    } = snapshot;
    with_state_lock(|| {
        let provider = base_url.as_deref().and_then(Provider::from_base_url);
        let was_active = config.is_active(name);
        let profile = config
            .find_mut(name)
            .with_context(|| format!("profile '{name}' vanished before overwrite"))?;
        profile.base_url = base_url;
        profile.api_key = api_key;
        profile.credentials = credentials;
        profile.provider = provider;
        profile.usage = None;
        profile.fetch_status = None;
        profile.third_party_usage = None;
        save_profile(profile)?;
        let captured_login = profile.credentials.is_some();

        for file in [
            crate::profile_cache::USAGE_CACHE_FILE,
            crate::profile_cache::THIRD_PARTY_CACHE_FILE,
            crate::throughput::THROUGHPUT_CACHE_FILE,
        ] {
            crate::profile_cache::remove_profile_cache(name, file);
        }

        // CAP-1: the capture may have changed which ACCOUNT this profile holds
        // — the identity anchor moves with the store (and drops for a
        // third-party capture that stores no OAuth login), sourced per the
        // snapshot's provenance (CAP-2).
        if captured_login {
            anchor_captured_login(name, &identity);
        } else {
            crate::profile_cache::drop_account_anchor(name);
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
            link_profile_credentials(name)?;
            config.state.active_profile = Some(name.into());
        } else if was_active {
            // The overwritten profile is (and stays) the active one: unlike a
            // brand-new capture, `save_profile` just rewrote credentials.json
            // in place (or removed it, if the snapshot had none — a third-
            // party capture). Re-run `link_profile_credentials` so the live
            // `.credentials.json` symlink is recreated against the new file,
            // or dropped instead of left dangling when the file is now gone;
            // and re-apply `base_url`/`api_key` to `settings.json` the same
            // way `edit_profile_endpoint` does, so a running `claude` doesn't
            // keep reading the OLD endpoint/token until the next switch.
            link_profile_credentials(name)?;
            let profile = config.find(name).context("profile not found")?;
            let prev_env_keys: Vec<String> = profile.env.keys().cloned().collect();
            apply_profile_to_claude_settings(profile, &prev_env_keys)?;
        }
        // AUTH-1: the re-login replaced the dead credential chain — the whole
        // point of the reauth — so lift the profile's auth-broken quarantine,
        // exactly like the fresh-capture path (`capture_into_profile`) does.
        // Left set, the flag keeps the just-relogged account excluded from
        // every chain walk and keeps the "login expired" banner up.
        config.set_auth_broken(name, false);
        save_app_state(&config.state)?;
        // A daemon poll racing this reauth can still be spending the OLD
        // (dead) token; its terminal failure re-marks the quarantine after
        // the blind save above (TECH-7 lost-update surface). Re-apply the
        // clear as a narrow delta against the LATEST disk state so the fresh
        // login wins that race instead of waiting a self-heal round-trip.
        let name_owned = name.to_string();
        config.state = update_app_state(|s| {
            s.auth_broken.retain(|n| n.as_str() != name_owned);
        })?;
        Ok(())
    })?;
    // Outside the closure — see `rename_profile` on the rank order. Skipped when
    // the swap fails, which is imprecise rather than atomic: a failure after
    // `save_profile` leaves the new credentials on disk under the old account's
    // stamp. Bounded either way — an unexpired stamp lapses within the hour, and a
    // tick racing the gap between the flock release and this expire spends the
    // stale stamp once or loses a fresh one and re-pulls once.
    crate::usage::expire_profile_ttl(name);
    Ok(())
}

/// Blank a profile's OAuth login: drop its stored credentials and per-account
/// fetch caches, returning it to the credential-less shell `Profile::new`
/// produces. Keeps name, model, env, and chain slot. When it's the active
/// profile, clear the live `~/.claude` link and deactivate — a credential-less
/// profile can't be meaningfully active, and the honest state is "no active".
pub(crate) fn clear_profile_credentials(config: &mut AppConfig, name: &str) -> Result<()> {
    with_state_lock(|| {
        let was_active = config.is_active(name);
        let profile = config
            .find_mut(name)
            .with_context(|| format!("profile '{name}' not found"))?;
        profile.credentials = None;
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
            crate::throughput::THROUGHPUT_CACHE_FILE,
        ] {
            crate::profile_cache::remove_profile_cache(name, file);
        }

        if was_active {
            clear_claude_credentials()?;
            config.state.active_profile = None;
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
pub(crate) fn clear_profile_api_key(config: &mut AppConfig, name: &str) -> Result<()> {
    with_state_lock(|| {
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
    with_state_lock(|| {
        // Resync to fix length drift from a partial save in a prior session.
        config.sync_state_profiles();
        let profile = config.profiles.remove(from);
        config.profiles.insert(to, profile);
        let name = config.state.profiles.remove(from);
        config.state.profiles.insert(to, name);
        save_app_state(&config.state)
    })
}

// ---- Codex harness actions (CDX-1 T3/T4) ------------------------------------
//
// The codex siblings of capture/switch. Deliberately simpler than the claude
// set: no Keychain, no symlink — the live file `~/.codex/auth.json` is
// compared by content (account_id anchor) and every store/install copies raw
// bytes whole-file (docs/codex-support/PLAN.md §0.3–0.5). All mutations run
// under `with_state_lock` (re-entrant), the same lock the daemon's follow and
// drain paths hold, so a switch (store→live) can never interleave with an
// adopt-back tick (live→store).

/// What the caller decided about a FOREIGN live login (one no stored codex
/// profile anchors) standing in the way of a switch. A user decision archives
/// it to quarantine (loss-free) and proceeds; automation refuses and leaves it
/// alone — the same split RESCUE-2 established on the claude side.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ForeignLivePolicy {
    Refuse,
    Archive,
}

/// What a codex switch did, for the caller's messaging.
#[derive(Debug, Clone, Default)]
pub(crate) struct CodexSwitchReport {
    /// The outgoing live login was adopted back into this profile's store
    /// (codex had rotated the chain; the snapshot was stale).
    pub(crate) adopted_back: Option<String>,
    /// A foreign/unparseable live login was archived here before the install.
    pub(crate) archived: Option<std::path::PathBuf>,
}

/// Codex logout (CDX-1 T8): drop `name`'s stored codex-auth.json and, when it
/// held the codex active slot, clear the marker. Never touches the live file
/// — a running codex login is codex's own to keep; the profile shell (env,
/// chain slot, settings) survives for a later re-capture.
pub(crate) fn codex_clear_profile_auth(config: &mut AppConfig, name: &str) -> Result<()> {
    with_state_lock(|| {
        ensure_codex_profile(config, name)?;
        let path = crate::codex::profile_auth_path(name)?;
        if path.exists() {
            std::fs::remove_file(&path).context("failed to remove codex-auth.json")?;
        }
        if config.is_active_codex(name) {
            let cleared = name.to_string();
            config.state = update_app_state(move |s| {
                if s.active_codex_profile.as_deref() == Some(cleared.as_str()) {
                    s.active_codex_profile = None;
                }
            })?;
        }
        Ok(())
    })
}

/// True when the live `~/.codex/auth.json` holds a real login that matches no
/// stored codex profile's account — the state a switch cannot displace without
/// an explicit decision. Missing/shell/unparseable live files answer `false`
/// (the switch handles those without user input).
pub(crate) fn codex_live_is_foreign(config: &AppConfig) -> Result<bool> {
    let Some(bytes) = crate::codex::read_live()? else {
        return Ok(false);
    };
    let Ok(live) = crate::codex::CodexAuthFile::parse(&bytes) else {
        return Ok(false);
    };
    if !live.has_login() {
        return Ok(false);
    }
    let candidates = codex_candidates(config);
    Ok(crate::codex::live_owner(
        &live,
        candidates.iter().map(|(n, b)| (n.as_str(), b.as_slice())),
    )
    .is_none())
}

/// The codex-harness profiles that hold a stored login, with their raw bytes.
fn codex_candidates(config: &AppConfig) -> Vec<(String, Vec<u8>)> {
    config
        .profiles
        .iter()
        .filter(|p| p.is_codex())
        .filter_map(|p| {
            let bytes = crate::codex::read_profile_auth(&p.name).ok().flatten()?;
            Some((p.name.to_string(), bytes))
        })
        .collect()
}

/// The codex profiles whose stored chain clauth EXCLUSIVELY holds — the only
/// chains the CDX-3 standby refresh may spend (PLAN.md §0.9: single-use
/// refresh tokens; a second consumer kills the chain). Excluded: the live
/// owner (codex itself advances that chain; the follow's adopt-back keeps our
/// snapshot fresh), profiles with a live isolated codex session (the isolated
/// `CODEX_HOME` carries theirs), `auth_broken` profiles (dead chain), and
/// anything without a stored refresh token. Returns `(name, stored bytes)` so
/// the caller's due-check needn't re-read. Callers re-derive this INSIDE the
/// per-profile `RotationGuard` before spending — a switch/capture landing
/// between snapshot and spend must flip the answer.
pub(crate) fn codex_standby_candidates(config: &AppConfig) -> Vec<(String, Vec<u8>)> {
    let candidates = codex_candidates(config);
    let live_owner: Option<String> = crate::codex::read_live()
        .ok()
        .flatten()
        .and_then(|bytes| crate::codex::CodexAuthFile::parse(&bytes).ok())
        .and_then(|live| {
            crate::codex::live_owner(
                &live,
                candidates.iter().map(|(n, b)| (n.as_str(), b.as_slice())),
            )
        });
    candidates
        .into_iter()
        .filter(|(name, bytes)| {
            if live_owner.as_deref() == Some(name.as_str()) {
                return false;
            }
            if config.is_auth_broken(name) {
                return false;
            }
            if crate::runtime::has_live_codex_session(name) {
                return false;
            }
            crate::codex::CodexAuthFile::parse(bytes).is_ok_and(|a| a.refresh_token().is_some())
        })
        .collect()
}

/// The OTHER codex profile (if any) already anchoring `account_id` — the
/// CAP-3 dedup shared by capture and the browser login; `exempt` is the
/// profile being (re-)authed, which is a refresh, not a dup.
fn codex_account_owner_elsewhere(
    config: &AppConfig,
    exempt: &str,
    account_id: &str,
) -> Option<String> {
    codex_candidates(config)
        .iter()
        .filter(|(n, _)| n != exempt)
        .find(|(_, stored)| {
            crate::codex::CodexAuthFile::parse(stored)
                .ok()
                .and_then(|s| s.account_id())
                .as_deref()
                == Some(account_id)
        })
        .map(|(n, _)| n.clone())
}

fn ensure_codex_file_store() -> Result<()> {
    match crate::codex::store_mode() {
        mode if mode.is_file() => Ok(()),
        crate::codex::StoreMode::Other(mode) => bail!(
            "codex stores credentials in '{mode}' mode (cli_auth_credentials_store in \
             ~/.codex/config.toml) — clauth supports only the default 'file' mode"
        ),
        crate::codex::StoreMode::File => unreachable!("is_file() covered above"),
    }
}

fn ensure_codex_profile(config: &AppConfig, name: &str) -> Result<()> {
    let Some(profile) = config.find(name) else {
        bail!("profile '{name}' not found");
    };
    if !profile.is_codex() {
        bail!("profile '{name}' is a claude profile — it switches via the claude path");
    }
    Ok(())
}

/// Capture the live `~/.codex/auth.json` into `name` — create the profile, or
/// re-auth an existing codex profile in place. The captured login is live by
/// definition, so `active_codex_profile` always lands on `name`.
pub(crate) fn codex_capture_into_profile(config: &mut AppConfig, name: &str) -> Result<()> {
    with_state_lock(|| {
        // CDX-3 §0.9: a standby refresh may hold this profile's rotation lock
        // across its HTTP window. A blocking acquire here would invert the
        // Rotation-outermost rank (we may already hold the state flock), so
        // probe instead — busy means "its chain is being advanced right now".
        let _rotation_probe =
            crate::runtime::RotationProbe::try_acquire(name)?.ok_or_else(|| {
                anyhow::anyhow!("a token refresh for '{name}' is in flight — retry in a moment")
            })?;
        // CDX-1b §0.14: an isolated session's watchdog owns this store slot
        // while it runs (it adopts the session's rotations back); a capture
        // overwriting it concurrently would interleave two writers.
        if crate::runtime::has_live_codex_session(name) {
            bail!(
                "profile '{name}' is running via `clauth start` — exit that session before \
                 re-capturing it"
            );
        }
        ensure_codex_file_store()?;
        let bytes = crate::codex::read_live()?
            .ok_or_else(|| anyhow::anyhow!("no live codex login — run `codex login` first"))?;
        let live = crate::codex::CodexAuthFile::parse(&bytes)
            .context("live ~/.codex/auth.json is unparseable")?;
        if !live.has_login() {
            bail!("the live codex login is a logged-out shell — run `codex login` first");
        }
        // CAP-3 sibling: one account under two codex profiles would make the
        // eventual chain walk (CDX-4) treat one login as two lanes. The
        // profile being re-authed is exempt (that is a refresh, not a dup).
        if let Some(live_id) = live.account_id()
            && let Some(owner) = codex_account_owner_elsewhere(config, name, &live_id)
        {
            bail!("profile '{owner}' already holds this codex account — re-auth it instead");
        }

        codex_install_login_locked(config, name, &bytes, true)
    })
}

/// Store a validated codex login into `name` — the shared tail of capture and
/// the browser PKCE login. Caller holds the state lock, has probed the
/// rotation lock, and has run the entry-point checks (lease, CAP-3 dedup,
/// and capture's live-file store-mode gate). `set_active` flips the codex
/// active slot (true for capture — the captured login IS the live one; false
/// for the browser login — the live file was never touched). A successful
/// install always clears `auth_broken`: a fresh chain is the heal (mirrors
/// the claude re-login path).
fn codex_install_login_locked(
    config: &mut AppConfig,
    name: &str,
    bytes: &[u8],
    set_active: bool,
) -> Result<()> {
    match config.find(name) {
        Some(existing) if existing.is_codex() => {
            // Re-auth in place: keep env/models/chain position, swap bytes.
            crate::codex::write_profile_auth(name, bytes)?;
        }
        Some(_) => bail!(
            "profile '{name}' is a claude profile — a profile never converts across \
             harnesses; pick a new name"
        ),
        None => {
            validate_profile_name(name, &config.names(), None)?;
            let mut profile = Profile::new(name.to_string(), None, None);
            profile.harness = crate::profile::Harness::Codex;
            save_profile(&profile)?;
            crate::codex::write_profile_auth(name, bytes)?;
            config.add(profile);
        }
    }
    config.set_auth_broken(name, false);

    // TECH-7: merge only this install's delta into the latest on-disk state.
    // Wholesale re-sync: `merged` is the freshest on-disk state plus this
    // delta; partial copy-back would leave the caller's snapshot stale
    // against a concurrent writer (same rationale as the claude follow).
    let name_owned = name.to_string();
    config.state = update_app_state(move |s| {
        if !s.profiles.iter().any(|p| p.as_str() == name_owned) {
            s.profiles.push(name_owned.as_str().into());
        }
        if set_active {
            s.active_codex_profile = Some(name_owned.as_str().into());
        }
        s.auth_broken.retain(|n| n.as_str() != name_owned);
    })?;
    Ok(())
}

/// Store a browser-PKCE-minted codex login into `name` (CDX-3 R5). Unlike
/// capture this NEVER reads or affects the live `~/.codex/auth.json` or the
/// codex active slot — the snapshot goes straight to the profile store, ready
/// for a later switch. Same dedup/lease/probe discipline as capture.
pub(crate) fn codex_store_browser_login(
    config: &mut AppConfig,
    name: &str,
    bytes: &[u8],
) -> Result<()> {
    with_state_lock(|| {
        let _rotation_probe =
            crate::runtime::RotationProbe::try_acquire(name)?.ok_or_else(|| {
                anyhow::anyhow!("a token refresh for '{name}' is in flight — retry in a moment")
            })?;
        if crate::runtime::has_live_codex_session(name) {
            bail!(
                "profile '{name}' is running via `clauth start` — exit that session before \
                 re-authenticating it"
            );
        }
        let minted = crate::codex::CodexAuthFile::parse(bytes)
            .context("the minted login snapshot is unparseable")?;
        if !minted.has_login() {
            bail!("the minted login snapshot holds no tokens");
        }
        // CAP-3 sibling (same rule as capture): one account under two codex
        // profiles would make the chain walk treat one login as two lanes.
        if let Some(minted_id) = minted.account_id()
            && let Some(owner) = codex_account_owner_elsewhere(config, name, &minted_id)
        {
            bail!("profile '{owner}' already holds this codex account — re-auth it instead");
        }
        codex_install_login_locked(config, name, bytes, false)
    })
}

/// Switch the live codex login to `target`'s stored chain (session-boundary
/// semantics — a running codex keeps its in-memory account until its next
/// refresh boundary; the swap takes effect for NEW sessions). Loss-free by
/// construction: an outgoing login owned by a stored profile is adopted back
/// first; a foreign one is archived or refused per `on_foreign`.
pub(crate) fn codex_switch_profile(
    config: &mut AppConfig,
    target: &str,
    on_foreign: ForeignLivePolicy,
) -> Result<CodexSwitchReport> {
    with_state_lock(|| {
        ensure_codex_profile(config, target)?;
        if config.is_auth_broken(target) {
            bail!("profile '{target}' is quarantined after a permanent auth failure");
        }
        // CDX-3 §0.9: never install a chain a standby refresh is mid-flight on
        // (the store bytes are about to be superseded). Non-blocking probe —
        // see the capture path for the rank rationale; the daemon drain
        // converts this error into its retry backoff.
        let _rotation_probe =
            crate::runtime::RotationProbe::try_acquire(target)?.ok_or_else(|| {
                anyhow::anyhow!("a token refresh for '{target}' is in flight — retry in a moment")
            })?;
        // CDX-1b §0.14: a live isolated session carries this profile's chain
        // in its own CODEX_HOME — installing the store snapshot to the shared
        // home would fork the chain (two carriers → refresh_token_reused).
        if crate::runtime::has_live_codex_session(target) {
            bail!(
                "profile '{target}' is running via `clauth start` — its login lives in that \
                 session's isolated home; exit the session before switching to it"
            );
        }
        ensure_codex_file_store()?;
        let stored = crate::codex::read_profile_auth(target)?.ok_or_else(|| {
            anyhow::anyhow!("profile '{target}' has no stored codex login — capture one first")
        })?;

        let mut report = CodexSwitchReport::default();
        match crate::codex::read_live()? {
            None => {}
            Some(live_bytes) => match crate::codex::CodexAuthFile::parse(&live_bytes) {
                Err(_) => {
                    // Unparseable bytes might still be a half-written login:
                    // quarantine them rather than destroy them.
                    report.archived = Some(crate::codex::archive_live_auth("unparseable")?);
                }
                Ok(live) if !live.has_login() => {} // logged-out shell: nothing to protect
                Ok(live) => {
                    let owner = crate::codex::live_owner(
                        &live,
                        codex_candidates(config)
                            .iter()
                            .map(|(n, b)| (n.as_str(), b.as_slice())),
                    );
                    match owner {
                        Some(owner) => {
                            if crate::codex::read_profile_auth(&owner)?.as_deref()
                                != Some(&live_bytes[..])
                            {
                                // codex rotated the chain since our snapshot —
                                // the live file is the truth, adopt it back.
                                crate::codex::write_profile_auth(&owner, &live_bytes)?;
                                report.adopted_back = Some(owner.clone());
                            }
                            if owner == target {
                                // The live login already IS the target's chain
                                // (now freshly adopted). Installing the older
                                // snapshot over it would roll the chain back.
                                config.state = update_app_state(|s| {
                                    s.active_codex_profile = Some(target.into());
                                })?;
                                return Ok(report);
                            }
                        }
                        None => match on_foreign {
                            ForeignLivePolicy::Refuse => bail!(
                                "the live codex login matches no stored profile — capture it \
                                 with `clauth login <name> --codex` or switch with an explicit \
                                 discard"
                            ),
                            ForeignLivePolicy::Archive => {
                                report.archived = Some(crate::codex::archive_live_auth("foreign")?);
                            }
                        },
                    }
                }
            },
        }

        crate::codex::write_live(&stored)?;
        config.state = update_app_state(|s| {
            s.active_codex_profile = Some(target.into());
        })?;
        Ok(report)
    })
}

#[cfg(test)]
#[path = "../tests/inline/actions.rs"]
mod tests;

#[cfg(test)]
#[path = "../tests/inline/mcp_switch.rs"]
mod tests_mcp_switch;

#[cfg(test)]
#[path = "../tests/inline/codex_actions.rs"]
mod tests_codex;
