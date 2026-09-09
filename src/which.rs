//! `clauth which [--json]` — identify which stored profile owns the OAuth
//! tokens in the credentials.json currently loaded by Claude Code.
//!
//! Resolution: (1) match the loaded file's `refreshToken` against each stored
//! profile's `refreshToken` — the clauth symlink layout keeps the live file
//! and the matching profile's file byte-identical across rotations. (1b) When
//! the loaded file carries NO refresh token, match its `accessToken` against
//! each profile's long-lived session-token sidecar (CLA-SPLIT): that is what a
//! switch installs for such a profile, and both things a sidecar can hold — a
//! `claude setup-token` mint, or a rolling stamp (CLA-ROLL) — carry no refresh
//! token by construction, so tier 1 can never see either. (2) Inside
//! a `clauth start` runtime, fall back to the profile named by
//! `CLAUDE_CONFIG_DIR` (`profiles/<name>/runtime-<sid>`, or a bare
//! `profiles/<name>/runtime` where the tree is shared): a runtime tree belongs
//! to exactly one profile, so that profile owns the session even before its
//! first login is stored. (3) Otherwise, attribute to the credential-less active profile
//! (an API-key/endpoint profile, whose creds file is absent after a switch, or
//! a fresh OAuth login not yet snapshotted).
//!
//! Path: honors `CLAUDE_CONFIG_DIR` (the same env var `clauth start` sets) so
//! a status line running inside an isolated session finds the right file.

use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::out::outln;
use crate::profile::{AppConfig, ClaudeCredentials, Profile, claude_dir, load_config};
use crate::profile_json::tier_label;

/// Which resolution branch attributed the loaded credentials to a profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Source {
    /// Exact `refreshToken` match against a stored profile.
    RefreshMatch,
    /// Exact `accessToken` match against a profile's long-lived session-token
    /// sidecar — the credential a switch installs for a CLA-SPLIT profile.
    SessionTokenMatch,
    /// Profile named by a `clauth start` runtime `CLAUDE_CONFIG_DIR`.
    SessionDir,
    /// Fresh first-login attributed to the credential-less active profile.
    CredentialLessActive,
}

impl Source {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Source::RefreshMatch => "refresh_match",
            Source::SessionTokenMatch => "session_token_match",
            Source::SessionDir => "session_dir",
            Source::CredentialLessActive => "credential_less_active",
        }
    }
}

pub(crate) fn run(json: bool) -> Result<()> {
    let config = load_config()?;
    let resolved = resolve_active(&config);

    if json {
        emit_json(&config, resolved);
    } else {
        emit_plain(resolved.as_ref().map(|(name, _)| name.as_str()));
    }
    Ok(())
}

/// Gather the session env + loaded credentials and resolve them to the owning
/// profile, returning an owned name plus the branch that matched, or `None` when
/// nothing matched. Shared by `clauth which` and the MCP `which` tool.
pub(crate) fn resolve_active(config: &AppConfig) -> Option<(String, Source)> {
    resolve_at(config, session_config_dir().as_deref())
}

/// The config dir THIS process's session reads. One derivation so a caller that
/// wants the INPUT to [`resolve_active`] cannot read a different env than the
/// resolution does.
///
/// An empty value is treated as unset, matching [`session_auth`]. Without that
/// filter the two disagreed: `session_auth` read `CLAUDE_CONFIG_DIR=""` as
/// Global while this resolved a CWD-RELATIVE `.credentials.json`, so a bare
/// session with the variable exported empty attributed itself off a file in
/// whatever directory it happened to be started from.
pub(crate) fn session_config_dir() -> Option<PathBuf> {
    std::env::var_os("CLAUDE_CONFIG_DIR")
        .filter(|dir| !dir.is_empty())
        .map(PathBuf::from)
}

/// The credentials file [`resolve_active`] reads for this process's session.
///
/// Exposed so a caller can stat the resolution's input instead of redoing the
/// resolution: the swap executor moves that file's mtime by construction
/// (`runtime::touch_store` — Claude Code re-reads only when it moves), so an
/// unmoved stamp is evidence the attributed account cannot have changed with it.
pub(crate) fn active_credentials_path() -> Option<PathBuf> {
    credentials_path(session_config_dir().as_deref()).ok()
}

/// Which profile owns the GLOBAL `~/.claude/.credentials.json` — the file a bare
/// `claude` reads — regardless of the READER's own `CLAUDE_CONFIG_DIR`.
///
/// That env var describes the process asking, so [`resolve_active`] is the wrong
/// question for attributing a DIFFERENT process's credentials: a TUI running
/// inside a `clauth start` session would claim every bare `claude` on the box for
/// its own runtime profile.
pub(crate) fn resolve_global(config: &AppConfig) -> Option<(String, Source)> {
    resolve_at(config, None)
}

/// The shared resolution: read the credentials a session with this config dir
/// loads, and attribute them. One predicate for both entry points, so the two can
/// never drift into different answers for the same file.
fn resolve_at(config: &AppConfig, config_dir: Option<&Path>) -> Option<(String, Source)> {
    let session_profile = config_dir.and_then(session_profile_from_config_dir);
    let creds = credentials_path(config_dir)
        .ok()
        .and_then(|path| read_credentials(&path));
    resolve_profile(
        config,
        creds.as_ref(),
        config_dir.is_some(),
        session_profile.as_deref(),
        &crate::claude::installed_session_token,
    )
    .map(|(name, source)| (name.to_string(), source))
}

/// How this session reads its credentials, used to explain what `switch` does to
/// *it*. The session's config dir is the discriminator: a `clauth start` runtime
/// and a custom `CLAUDE_CONFIG_DIR` each read their own `.credentials.json`, which
/// a global relink never touches; only a session on the global `~/.claude/` reads
/// the very file `switch` repoints.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SessionAuth {
    /// `clauth start <name>` runtime — pinned to its own creds; a global switch can't reach it.
    IsolatedRuntime(String),
    /// A non-clauth `CLAUDE_CONFIG_DIR` — reads its own creds; a global switch can't reach it.
    IsolatedCustom,
    /// No `CLAUDE_CONFIG_DIR` — reads the global `~/.claude/` creds that `switch` repoints.
    Global,
}

/// Classify the current session's credential source from `CLAUDE_CONFIG_DIR` (the
/// same env `clauth start` sets). An empty value is treated as unset.
pub(crate) fn session_auth() -> SessionAuth {
    match std::env::var_os("CLAUDE_CONFIG_DIR").filter(|d| !d.is_empty()) {
        Some(dir) => match session_profile_from_config_dir(Path::new(&dir)) {
            Some(name) => SessionAuth::IsolatedRuntime(name),
            None => SessionAuth::IsolatedCustom,
        },
        None => SessionAuth::Global,
    }
}

fn credentials_path(config_dir: Option<&Path>) -> Result<PathBuf> {
    match config_dir {
        Some(dir) => Ok(dir.join(".credentials.json")),
        None => Ok(claude_dir()?.join(".credentials.json")),
    }
}

/// Extract the `<name>` from a `clauth start` runtime path
/// (`~/.clauth/profiles/<name>/runtime-<sid>`, or a legacy bare `runtime`).
/// Returns `None` for any other shape, an isolated runtime included: that tier
/// has never covered the isolated flavor, and an isolated session's stored creds
/// are already reached by the credential matches above (`refreshToken`, or the
/// session-token match when the install is a refresh-less mint).
fn session_profile_from_config_dir(dir: &Path) -> Option<String> {
    if !dir
        .file_name()?
        .to_str()
        .is_some_and(crate::runtime::is_shared_runtime_dir_name)
    {
        return None;
    }
    let profile_dir = dir.parent()?;
    if profile_dir.parent()?.file_name()? != "profiles" {
        return None;
    }
    Some(profile_dir.file_name()?.to_str()?.to_string())
}

fn read_credentials(path: &Path) -> Option<ClaudeCredentials> {
    let content = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&content).ok()
}

/// [`resolve_profile_candidate`] filtered so a user-disabled account is never
/// attributed, no matter which tier matched it — including a stale token
/// match against creds that predate the disable (a disabled profile's stored
/// files are left untouched on disk, so its old refresh token can still sit
/// there). Shared by `clauth which`, the MCP `which` tool, and the bare-session
/// tally via [`resolve_at`], the only caller of this function.
fn resolve_profile<'a>(
    config: &'a AppConfig,
    creds: Option<&ClaudeCredentials>,
    in_session: bool,
    session_profile: Option<&str>,
    installed_session_token: &dyn Fn(&crate::profile::ProfileName) -> Option<String>,
) -> Option<(&'a crate::profile::ProfileName, Source)> {
    let (name, source) = resolve_profile_candidate(
        config,
        creds,
        in_session,
        session_profile,
        installed_session_token,
    )?;
    if config.find(name).is_some_and(Profile::is_disabled) {
        return None;
    }
    Some((name, source))
}

/// Resolve loaded credentials to a stored profile.
///
/// Order: (1) exact refresh-token match; (1b) exact session-token match for a
/// refresh-token-less login; (2) inside a `clauth start` runtime,
/// the profile named by `CLAUDE_CONFIG_DIR` owns the session even before its
/// first login is stored; (3) for a non-runtime caller, the credential-less
/// active profile (API-key/endpoint, or a fresh login not yet snapshotted).
///
/// A `CLAUDE_CONFIG_DIR` that isn't a clauth runtime gets steps 1/1b only — its
/// credentials don't belong to the global active profile.
fn resolve_profile_candidate<'a>(
    config: &'a AppConfig,
    creds: Option<&ClaudeCredentials>,
    in_session: bool,
    session_profile: Option<&str>,
    installed_session_token: &dyn Fn(&crate::profile::ProfileName) -> Option<String>,
) -> Option<(&'a crate::profile::ProfileName, Source)> {
    if let Some(name) = creds
        .and_then(ClaudeCredentials::refresh_token)
        .and_then(|rt| match_by_refresh_token(config, rt))
    {
        return Some((name, Source::RefreshMatch));
    }
    // CLA-SPLIT tier. Gated on the loaded file carrying NO refresh token, which
    // is both the correctness condition (a `claude setup-token` mint and a
    // rolling stamp are refresh-less by construction, so a rotating login can
    // never be attributed to a sidecar) and the cost one: the common case runs
    // zero extra disk reads, and this resolves once per second behind a
    // statusline. The sidecar read behind `installed_session_token` is itself
    // content-gated, so a rolling bearer attributes here exactly like a mint.
    if creds.is_some_and(|c| c.refresh_token().is_none())
        && let Some(at) = creds.and_then(ClaudeCredentials::access_token)
        && let Some(name) = match_by_session_token(config, at, installed_session_token)
    {
        return Some((name, Source::SessionTokenMatch));
    }
    if let Some(profile) =
        session_profile.and_then(|n| config.find(&crate::profile::ProfileName::from(n)))
    {
        return Some((&profile.name, Source::SessionDir));
    }
    if in_session {
        return None;
    }
    // Attribute to the active profile when it has no stored OAuth creds
    // (API-key/endpoint, or a fresh login not yet snapshotted). Not gated on
    // `creds`: switching to an API-key profile deletes the creds file, so a
    // prior refresh-token guard here mis-attributed the active profile as
    // `unknown`.
    config
        .state
        .active_profile
        .as_ref()
        .and_then(|n| config.find(n))
        .filter(|p| p.credentials.is_none())
        .map(|p| (&p.name, Source::CredentialLessActive))
}

fn match_by_refresh_token<'a>(
    config: &'a AppConfig,
    refresh_token: &str,
) -> Option<&'a crate::profile::ProfileName> {
    let active = config.state.active_profile.as_ref();
    let mut fallback = None;
    for p in &config.profiles {
        if p.refresh_token() != Some(refresh_token) {
            continue;
        }
        if Some(&p.name) == active {
            return Some(&p.name);
        }
        fallback.get_or_insert(&p.name);
    }
    fallback
}

/// Active-first tie-break, same as [`match_by_refresh_token`]: two profiles can
/// legitimately hold the same mint (a duplicated account), and the active one is
/// the honest answer for the live slot.
fn match_by_session_token<'a>(
    config: &'a AppConfig,
    access_token: &str,
    installed_session_token: &dyn Fn(&crate::profile::ProfileName) -> Option<String>,
) -> Option<&'a crate::profile::ProfileName> {
    if access_token.is_empty() {
        return None;
    }
    let active = config.state.active_profile.as_ref();
    let mut fallback = None;
    for p in &config.profiles {
        if installed_session_token(&p.name).as_deref() != Some(access_token) {
            continue;
        }
        if Some(&p.name) == active {
            return Some(&p.name);
        }
        fallback.get_or_insert(&p.name);
    }
    fallback
}

fn emit_plain(matched: Option<&str>) {
    match matched {
        Some(name) => outln!("{name}"),
        None => outln!("unknown"),
    }
}

fn emit_json(config: &AppConfig, resolved: Option<(String, Source)>) {
    outln!("{}", json_view(config, resolved.as_ref()));
}

/// The `--json` payload, split from the print so its field shapes are assertable
/// without capturing stdout.
///
/// `tier` describes the credential the profile STORES and says nothing about
/// where its requests route: `null` there means either that nothing on disk
/// claims a tier or that the profile carries a RECOGNISED third-party provider,
/// and an unrecognised endpoint (a local proxy, a self-hosted router) reports an
/// Anthropic tier off its stored pair while routing elsewhere entirely. `oauth`
/// answers the MANAGED half of routing: it is exactly `base_url.is_none()`, the
/// managed field alone. An operator-authored `[env] ANTHROPIC_BASE_URL` routes
/// the account even when `base_url` is empty, so `oauth: true` does not
/// guarantee requests reach Anthropic. A caller asking where requests go asks
/// `crate::profile::stored_endpoint`, which reads both sources.
///
/// `tier` goes through `profile_json::tier_label`, the same helper `status.json`
/// and the MCP tools call, so one account cannot read a different tier depending
/// on which JSON surface asked. Reading the `Profile` in hand instead would be
/// frozen, not merely differently formatted: `load_config` leaves
/// `Profile::usage` at `None` and only the TUI ever fills it, so this would fall
/// through to the OAuth token's `subscription_type` — written once at login and
/// carried by no refresh response — and report a canceled account's
/// pre-cancellation plan forever.
///
/// `base_url` carries the managed endpoint half, spelled as
/// `status.json` publishes it, so a reader gets the managed field without a
/// second call. It reads neither the profile's `[env]` block nor anything
/// else; the routing answer is `crate::profile::stored_endpoint`.
fn json_view(config: &AppConfig, resolved: Option<&(String, Source)>) -> serde_json::Value {
    let profile = resolved
        .and_then(|(name, _)| config.find(&crate::profile::ProfileName::from(name.clone())));
    serde_json::json!({
        "profile": profile.map(|p| &p.name),
        "source": resolved.map(|(_, s)| s.as_str()),
        "base_url": profile.and_then(|p| p.base_url.as_ref()),
        "tier": profile.and_then(tier_label),
        "oauth": profile.map(Profile::is_oauth),
        "active": profile.is_some_and(|p| config.is_active(&p.name)),
    })
}

#[cfg(test)]
#[path = "../tests/inline/which.rs"]
mod tests;
