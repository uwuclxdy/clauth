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
//! token by construction, so tier 1 can never see either. (2) On macOS, when
//! the file tiers miss, match the login in the Keychain item Claude Code
//! actually reads — the bare item for the global login, a config dir's
//! namespaced twin for a session — read through a ~30 s TTL cache, so the
//! post-migration steady state (CC moves the login into the item and deletes
//! the file) still resolves. (3) Inside
//! a `clauth start` runtime, fall back to the profile named by
//! `CLAUDE_CONFIG_DIR` (`profiles/<name>/runtime-<sid>`, or a bare
//! `profiles/<name>/runtime` where the tree is shared): a runtime tree belongs
//! to exactly one profile, so that profile owns the session even before its
//! first login is stored. (4) Otherwise, attribute to the credential-less active profile
//! (an API-key/endpoint profile, whose creds file is absent after a switch, or
//! a fresh OAuth login not yet snapshotted).
//!
//! Path: honors `CLAUDE_CONFIG_DIR` (the same env var `clauth start` sets) so
//! a status line running inside an isolated session finds the right file.
//!
//! `clauth which --pid <pid>` answers for another process instead: a
//! live-session registry row found up its parent chain, else a bare codex's
//! adopted login, else the global login a bare `claude` reads.

use std::path::{Path, PathBuf};

#[cfg(any(target_os = "macos", test))]
use anyhow::Context;
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
    /// Exact `refreshToken` match against the macOS Keychain item — the bare
    /// global login, or a config dir's namespaced twin — read when the file
    /// tiers miss.
    KeychainMatch,
}

impl Source {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Source::RefreshMatch => "refresh_match",
            Source::SessionTokenMatch => "session_token_match",
            Source::SessionDir => "session_dir",
            Source::CredentialLessActive => "credential_less_active",
            Source::KeychainMatch => "keychain_match",
        }
    }
}

pub(crate) fn run(json: bool) -> Result<()> {
    // The codex arm, OUTRANKED by a claude session-dir claim: env vars
    // inherit, so a claude session started from inside a codex session
    // carries the ancestor's CODEX_HOME — but its own CLAUDE_CONFIG_DIR names
    // a clauth runtime, which is this process's identity. Only when no clauth
    // claude runtime claims the asker does a clauth-shaped CODEX_HOME answer.
    // And a clauth-SHAPED home answers, or says nothing — never falls
    // through: the claude tiers below would attribute a codex session to
    // whatever the GLOBAL ~/.claude credentials resolve to, a different
    // harness's answer entirely. A CODEX_HOME that is not a clauth codex home
    // (the operator's own ~/.codex) says nothing and falls through.
    if !claude_session_dir_claims_this_process() {
        match codex_session_profile() {
            CodexClaim::Member(name) => {
                if json {
                    outln!("{}", codex_json_view(&name));
                } else {
                    emit_plain(Some(&name));
                }
                return Ok(());
            }
            CodexClaim::UnknownHome => {
                // Shaped like ours, naming no stored codex profile (or the
                // roster was unreadable): the same fail-closed answer the
                // claude session-dir tier gives an unrecognized runtime —
                // no answer, not somebody else's profile.
                if json {
                    outln!(
                        "{}",
                        serde_json::json!({
                            "profile": serde_json::Value::Null,
                            "source": serde_json::Value::Null,
                            "harness": "codex",
                            "base_url": serde_json::Value::Null,
                            "tier": serde_json::Value::Null,
                            "oauth": serde_json::Value::Null,
                            "active": false,
                        })
                    );
                } else {
                    emit_plain(None);
                }
                return Ok(());
            }
            CodexClaim::NotACodexHome => {}
        }
    }
    let config = load_config()?;
    let resolved = resolve_active(&config);

    if json {
        emit_json(&config, resolved);
    } else {
        emit_plain(resolved.as_ref().map(|(name, _)| name.as_str()));
    }
    Ok(())
}

/// Whether this process's `CLAUDE_CONFIG_DIR` names a clauth claude runtime —
/// the strongest identity claim there is, and the tie-break against an
/// inherited `CODEX_HOME`.
fn claude_session_dir_claims_this_process() -> bool {
    std::env::var_os("CLAUDE_CONFIG_DIR")
        .filter(|d| !d.is_empty())
        .is_some_and(|dir| session_profile_from_config_dir(Path::new(&dir)).is_some())
}

/// What a session's `CODEX_HOME` says about it. The three outcomes are
/// deliberately distinct: only [`CodexClaim::NotACodexHome`] may fall through
/// to the claude tiers.
enum CodexClaim {
    /// A clauth codex home whose profile the roster holds.
    Member(String),
    /// Shaped like a clauth codex home, but the roster misses the name or
    /// could not be read — fail closed, never attribute.
    UnknownHome,
    /// Not a clauth codex home at all (unset, empty, or foreign).
    NotACodexHome,
}

/// The codex arm off `CODEX_HOME`. Empty env is unset, same as the
/// `CLAUDE_CONFIG_DIR` rule.
fn codex_session_profile() -> CodexClaim {
    match std::env::var_os("CODEX_HOME").filter(|d| !d.is_empty()) {
        Some(home) => codex_session_profile_at(Path::new(&home)),
        None => CodexClaim::NotACodexHome,
    }
}

/// [`codex_session_profile`] with the path injected, shared with its tests:
/// parse `profiles/<name>/codex-home*`, then require the codex roster to hold
/// `<name>` — the membership gate the claude session-dir tier gets from
/// `config.find`, with the roster miss (and an unreadable roster) reported as
/// its own outcome rather than collapsed into "not ours".
fn codex_session_profile_at(home: &Path) -> CodexClaim {
    let Some(name) = session_profile_from_codex_home(home) else {
        return CodexClaim::NotACodexHome;
    };
    match crate::codex_profiles::CodexState::load() {
        Ok(state) if state.holds(&name) => CodexClaim::Member(name),
        Ok(_) | Err(_) => CodexClaim::UnknownHome,
    }
}

/// Extract the `<name>` from a clauth codex home path
/// (`~/.clauth/profiles/<name>/codex-home[-<sid>]`). Returns `None` for any
/// other shape — the structural twin of [`session_profile_from_config_dir`],
/// answering off the same predicate the perms sweep asks
/// ([`crate::runtime::is_codex_home_path`]) so the two can never disagree
/// about what a codex home is.
fn session_profile_from_codex_home(dir: &Path) -> Option<String> {
    if !crate::runtime::is_codex_home_path(dir) {
        return None;
    }
    Some(dir.parent()?.file_name()?.to_str()?.to_string())
}

/// The `--json` payload for a codex session. The claude-shaped fields keep
/// their keys with the honest empty answer (`null`) so an existing reader's
/// indexing stays valid, `active` compares against the CODEX active slot, and
/// `harness` says which roster answered — the claude payload stays untouched
/// until the published-surface phase adds its own field.
fn codex_json_view(name: &str) -> serde_json::Value {
    let active = crate::codex_profiles::CodexState::load()
        .ok()
        .is_some_and(|s| s.active_profile().map(|n| n.as_str()) == Some(name));
    serde_json::json!({
        "profile": name,
        "source": "codex_home",
        "harness": "codex",
        "base_url": serde_json::Value::Null,
        "tier": serde_json::Value::Null,
        "oauth": serde_json::Value::Null,
        "active": active,
        "tps": serde_json::Value::Null,
    })
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

/// The config dir the running Claude Code session reads: [`session_config_dir`],
/// else the global `~/.claude`.
pub(crate) fn session_config_dir_or_global() -> Option<PathBuf> {
    session_config_dir().or_else(|| crate::profile::claude_dir().ok())
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
    #[cfg(target_os = "macos")]
    let keychain_login = || keychain_evidence(config_dir);
    #[cfg(not(target_os = "macos"))]
    let keychain_login = || None;
    resolve_profile(
        config,
        creds.as_ref(),
        config_dir.is_some(),
        session_profile.as_deref(),
        &crate::claude::installed_session_token,
        &keychain_login,
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
    let claude_dir = crate::profile::claude_dir().ok();
    session_auth_for(
        std::env::var_os("CLAUDE_CONFIG_DIR").as_deref(),
        claude_dir.as_deref(),
    )
}

/// [`session_auth`] with its inputs spelled, so the decision is pinned on
/// every platform. The default-dir arm is gated off macOS: a macOS session's
/// Keychain item is namespaced per the SET `CLAUDE_CONFIG_DIR` and migrates on
/// its first refresh (docs/domain-knowledge.md), so after the migration the
/// session reads an item the global switch never writes — `Global` is only
/// honest while the file layer still answers.
fn session_auth_for(
    config_dir: Option<&std::ffi::OsStr>,
    claude_dir: Option<&Path>,
) -> SessionAuth {
    match config_dir.filter(|d| !d.is_empty()) {
        Some(dir) => match session_profile_from_config_dir(Path::new(dir)) {
            Some(name) => SessionAuth::IsolatedRuntime(name),
            None if !cfg!(target_os = "macos")
                && claude_dir.is_some_and(|cd| dir_is_claude_dir(Path::new(dir), cd)) =>
            {
                SessionAuth::Global
            }
            None => SessionAuth::IsolatedCustom,
        },
        None => SessionAuth::Global,
    }
}

/// Whether `dir` names the default `~/.claude` the global switch repoints.
/// Canonicalizes both sides so a symlinked `$HOME` still matches (the
/// `cwd_is_real_home` shape); a side that cannot resolve falls back to the
/// plain compare.
fn dir_is_claude_dir(dir: &Path, claude_dir: &Path) -> bool {
    match (
        crate::canonical::identity(dir),
        crate::canonical::identity(claude_dir),
    ) {
        (Ok(a), Ok(b)) => a == b,
        _ => dir == claude_dir,
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
    // A regular file only, bounded: the credentials path is read every
    // session-scope `profiles` call and every bare-marker tally tick, and an
    // `open` on a FIFO blocks forever (a device read never ends). A
    // non-regular or unreadable file reads `None`, the arm an unreadable
    // credentials file already takes: the resolution proceeds credential-less.
    // The cap is the Keychain blob ceiling, not the job-record one: a
    // credentials file carries Claude Code's `mcpOAuth` block beside the login
    // and grows far past a job record's size.
    let bytes =
        crate::runtime::read_regular_capped(path, crate::runtime::CREDENTIALS_READ_CAP).ok()??;
    serde_json::from_slice(&bytes).ok()
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
    keychain_login: &dyn Fn() -> Option<serde_json::Value>,
) -> Option<(&'a crate::profile::ProfileName, Source)> {
    let (name, source) = resolve_profile_candidate(
        config,
        creds,
        in_session,
        session_profile,
        installed_session_token,
        keychain_login,
    )?;
    if config.find(name).is_some_and(Profile::is_disabled) {
        return None;
    }
    Some((name, source))
}

/// Resolve loaded credentials to a stored profile.
///
/// Order: (1) exact refresh-token match; (1b) exact session-token match for a
/// refresh-token-less login; (2) the macOS Keychain item's login (bare or
/// namespaced), when the file tiers miss; (3) inside a `clauth start` runtime,
/// the profile named by `CLAUDE_CONFIG_DIR` owns the session even before its
/// first login is stored; (4) for a non-runtime caller, the credential-less
/// active profile (API-key/endpoint, or a fresh login not yet snapshotted).
///
/// A `CLAUDE_CONFIG_DIR` that isn't a clauth runtime gets steps 1/1b plus the
/// Keychain item — its credentials don't belong to the global active profile.
fn resolve_profile_candidate<'a>(
    config: &'a AppConfig,
    creds: Option<&ClaudeCredentials>,
    in_session: bool,
    session_profile: Option<&str>,
    installed_session_token: &dyn Fn(&crate::profile::ProfileName) -> Option<String>,
    keychain_login: &dyn Fn() -> Option<serde_json::Value>,
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
    // Keychain tier (macOS; a no-op closure everywhere else). Fires only when
    // the file tiers miss — exactly the post-migration steady state, where CC
    // has moved the login into the item and deleted the file — and reads
    // through the TTL cache, so the common pre-migration path costs nothing.
    // A blank or absent login is a logged-out shell: no evidence, fall through.
    if let Some(name) = keychain_login()
        .as_ref()
        .and_then(|blob| match_by_keychain_blob(config, blob))
    {
        return Some((name, Source::KeychainMatch));
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
    // prior refresh-token guard here left the active profile unresolved.
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

/// The pure core of the Keychain tier: attribute an already-read item blob's
/// login by its refresh token. The `login_blob_is_ours` pattern — a blank or
/// absent login is a logged-out shell, never a match — over
/// [`match_by_refresh_token`], so the tier shares tier 1's active-first
/// tie-break. Pure and pinned cross-platform; only the READ is macOS's.
fn match_by_keychain_blob<'a>(
    config: &'a AppConfig,
    blob: &serde_json::Value,
) -> Option<&'a crate::profile::ProfileName> {
    let refresh_token = blob
        .get("claudeAiOauth")
        .and_then(|login| login.get("refreshToken"))
        .and_then(serde_json::Value::as_str)
        .filter(|rt| !rt.is_empty())?;
    match_by_refresh_token(config, refresh_token)
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

/// The plain answer: the profile, or nothing at all when nothing resolved.
fn emit_plain(matched: Option<&str>) {
    if let Some(name) = matched {
        outln!("{name}");
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
    let profile_name = resolved.map(|(name, _)| name.as_str());
    serde_json::json!({
        "profile": profile.map(|p| &p.name),
        "source": resolved.map(|(_, s)| s.as_str()),
        "base_url": profile.and_then(|p| p.base_url.as_ref()),
        "tier": profile.and_then(tier_label),
        "oauth": profile.map(Profile::is_oauth),
        "active": profile.is_some_and(|p| config.is_active(&p.name)),
        "tps": tps_view(profile_name),
    })
}

/// The `--json` tps field for the resolved profile, read from the producer's
/// cache only (no self-tail): `null` with no cache or one older than
/// [`crate::tps::STALE_AFTER_MS`]. `speed`/`rate` are the profile's live
/// figures; `session` is the current Claude Code session's live figures when
/// `CLAUDE_CODE_SESSION_ID` names one in the cache; `peaks.today` is the
/// profile's own today peaks from the cache's `peaks.today.accounts`.
fn tps_view(profile: Option<&str>) -> serde_json::Value {
    tps_view_at(profile, crate::usage::now_ms())
}

/// [`tps_view`] with the clock fixed, so tests can pin the staleness edge.
fn tps_view_at(profile: Option<&str>, now_ms: u64) -> serde_json::Value {
    let Some(snap) = crate::tps::load_cache() else {
        return serde_json::Value::Null;
    };
    let Some(age_ms) = snap.age_ms(now_ms) else {
        return serde_json::Value::Null;
    };
    if age_ms > crate::tps::STALE_AFTER_MS {
        return serde_json::Value::Null;
    }
    let figures = profile.and_then(|name| snap.accounts.get(name));
    let peaks = profile.and_then(|name| snap.peaks.today.accounts.get(name));
    let session = std::env::var("CLAUDE_CODE_SESSION_ID")
        .ok()
        .and_then(|sid| snap.sessions.get(&sid))
        .map(|s| {
            serde_json::json!({
                "speed": s.figures.live.speed,
                "rate": s.figures.live.rate,
            })
        });
    serde_json::json!({
        "speed": figures.and_then(|f| f.live.speed),
        "rate": figures.map_or(0.0, |f| f.live.rate),
        "session": session,
        "peaks": serde_json::json!({"today": peaks.cloned().unwrap_or_default()}),
        "age_secs": age_ms / 1000,
    })
}

// ---- Keychain evidence (macOS): the tier's read, behind a ~30 s TTL cache ----
//
// `which` runs once per second wherever the resolution is asked (a statusline,
// a herdr pane, the TUI's bare-session sweep), so only a DURABLE cache helps:
// each call pays one small file read, and the `security` subprocess runs on a
// cache miss or a stale entry. One 0600 file per service under `~/.clauth`
// (security.md's tree invariant: `atomic_write_600`), so a write takes no
// state flock — the rename keeps a concurrent reader on old-or-new bytes, the
// accepted race ("N stale panes re-resolve once") costs one subprocess, and
// no caller thread (the TUI render thread included) ever waits on a
// concurrent switch. A locked Keychain stalls the caller once per TTL per
// service rather than once per call. Each write sweeps entries older than a
// day, so session churn cannot grow the dir unbounded. Compiled off non-macOS
// shipped builds: the tier is inert there and Linux stays byte-identical
// (test builds keep the pure logic pinned).

#[cfg(target_os = "macos")]
fn keychain_evidence(config_dir: Option<&Path>) -> Option<serde_json::Value> {
    if !crate::keychain::enabled() {
        return None;
    }
    let service = keychain_evidence_service(config_dir)?;
    let dir = keychain_cache_dir()?;
    let now = now_secs();
    if let Some(entry) =
        keychain_cache_entry(read_keychain_cache_entry(&dir, &service).as_ref(), now)
    {
        return entry.blob.clone();
    }
    // Cache miss or stale entry: one `security` subprocess per TTL per
    // service, bounded by keychain::SECURITY_TIMEOUT.
    let blob = crate::keychain::read_blob_for_service(&service)
        .ok()
        .flatten();
    let entry = KeychainEvidenceEntry {
        at: now,
        blob: blob.clone(),
    };
    if write_keychain_cache_entry(&dir, &service, &entry).is_err() {
        // Best-effort: a read-only home must not fail `which`; the next call
        // pays one more subprocess.
    }
    blob
}

/// The Keychain service holding the login this resolution asks: the bare
/// `Claude Code-credentials` item for the global login, a config dir's
/// namespaced twin for a session. The bare arm is pure; the namespaced arm
/// needs the canonicalized dir, which is the macOS module's half.
#[cfg(any(target_os = "macos", test))]
fn keychain_evidence_service(config_dir: Option<&Path>) -> Option<String> {
    let Some(dir) = config_dir else {
        return Some(crate::claude::CLAUDE_KEYCHAIN_SERVICE.to_string());
    };
    #[cfg(target_os = "macos")]
    {
        crate::keychain::keychain_service_for_config_dir(dir).ok()
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = dir;
        None
    }
}

#[cfg(any(target_os = "macos", test))]
const KEYCHAIN_EVIDENCE_TTL_SECS: u64 = 30;

/// Entries older than this are swept on the next write, so session churn
/// cannot grow the cache dir unbounded.
#[cfg(any(target_os = "macos", test))]
const KEYCHAIN_EVIDENCE_RETENTION_SECS: u64 = 86_400;

/// One service's cached read: the item's login blob, or `None` when the item
/// was absent or unreadable (both read as "no evidence").
#[cfg(any(target_os = "macos", test))]
#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct KeychainEvidenceEntry {
    at: u64,
    #[serde(default)]
    blob: Option<serde_json::Value>,
}

#[cfg(any(target_os = "macos", test))]
fn keychain_cache_dir() -> Option<PathBuf> {
    crate::profile::clauth_dir()
        .ok()
        .map(|dir| dir.join("keychain-which-cache"))
}

/// The entry file for `service`: the service name IS the file name (the bare
/// `Claude Code-credentials` and the `-<sha8>` twins carry no separator), so
/// a write replaces only its own row and can never lose a sibling's.
#[cfg(any(target_os = "macos", test))]
fn keychain_cache_entry_path(dir: &Path, service: &str) -> PathBuf {
    dir.join(service)
}

/// The cached read for `service`, when one is still fresh. `None` means "run
/// the subprocess": an absent file, a stale entry, or unparseable bytes.
#[cfg(any(target_os = "macos", test))]
fn keychain_cache_entry(
    entry: Option<&KeychainEvidenceEntry>,
    now: u64,
) -> Option<&KeychainEvidenceEntry> {
    let entry = entry?;
    (now.saturating_sub(entry.at) <= KEYCHAIN_EVIDENCE_TTL_SECS).then_some(entry)
}

#[cfg(any(target_os = "macos", test))]
fn read_keychain_cache_entry(dir: &Path, service: &str) -> Option<KeychainEvidenceEntry> {
    // A regular file only, bounded like `read_credentials` and by the same
    // ceiling: the entry wraps one credential blob, so it can run as large.
    // A non-regular or unreadable entry reads `None`, the arm an absent or
    // unparseable entry already takes ("no evidence").
    let bytes = crate::runtime::read_regular_capped(
        &keychain_cache_entry_path(dir, service),
        crate::runtime::CREDENTIALS_READ_CAP,
    )
    .ok()??;
    serde_json::from_slice(&bytes).ok()
}

/// Write `service`'s entry (atomic 0600, the security.md tree invariant) and
/// sweep entries older than a day. No state flock: the rename keeps a
/// concurrent reader on old-or-new bytes, and a racing writer of the SAME
/// service can only lose its own row, which costs one subprocess later.
#[cfg(any(target_os = "macos", test))]
fn write_keychain_cache_entry(
    dir: &Path,
    service: &str,
    entry: &KeychainEvidenceEntry,
) -> anyhow::Result<()> {
    let bytes =
        serde_json::to_vec(entry).context("failed to serialize the keychain evidence cache")?;
    crate::profile::atomic_write_600(&keychain_cache_entry_path(dir, service), bytes)
        .context("failed to write the keychain evidence cache")?;
    sweep_keychain_cache(dir);
    Ok(())
}

/// Drop entries older than [`KEYCHAIN_EVIDENCE_RETENTION_SECS`], by file
/// mtime (the write time, which is the entry's `at`). Best-effort throughout:
/// a failed sweep leaves a stale file the next write sweeps again.
#[cfg(any(target_os = "macos", test))]
fn sweep_keychain_cache(dir: &Path) {
    let Some(cutoff) = std::time::SystemTime::now().checked_sub(std::time::Duration::from_secs(
        KEYCHAIN_EVIDENCE_RETENTION_SECS,
    )) else {
        return;
    };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(modified) = entry.metadata().and_then(|m| m.modified()) else {
            continue;
        };
        if modified < cutoff {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

#[cfg(target_os = "macos")]
fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Where `clauth which --pid` found the account a process spends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PidSource {
    /// A live-session registry row keyed on the process or an ancestor.
    LiveSession,
    /// A bare codex whose login clauth adopted.
    CodexAdopted,
    /// A bare `claude`: whatever owns the global credentials.
    Global,
}

impl PidSource {
    fn as_str(self) -> &'static str {
        match self {
            PidSource::LiveSession => "live_session",
            PidSource::CodexAdopted => "codex_adopted",
            PidSource::Global => "global",
        }
    }
}

/// One `which --pid` answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PidResolution {
    pub(crate) profile: String,
    pub(crate) source: PidSource,
    pub(crate) harness: crate::harness::Harness,
}

/// The two process facts the parent walk reads, behind a seam so tests feed a
/// fake table.
pub(crate) trait ProcTable {
    /// The parent pid, `None` when the process is gone or unreadable.
    fn parent(&self, pid: u32) -> Option<u32>;
    /// The argv, `None` when the process is gone or unreadable.
    fn argv(&self, pid: u32) -> Option<Vec<String>>;
}

/// The host's process table.
pub(crate) struct SystemProcs;

/// How many processes the parent walk visits, the target included.
const WALK_LIMIT: usize = 8;

#[cfg(target_os = "linux")]
impl ProcTable for SystemProcs {
    fn parent(&self, pid: u32) -> Option<u32> {
        parent_at(Path::new("/proc"), pid)
    }

    fn argv(&self, pid: u32) -> Option<Vec<String>> {
        argv_at(Path::new("/proc"), pid)
    }
}

#[cfg(all(unix, not(target_os = "linux")))]
impl ProcTable for SystemProcs {
    fn parent(&self, pid: u32) -> Option<u32> {
        ps_parent(pid)
    }

    fn argv(&self, pid: u32) -> Option<Vec<String>> {
        ps_argv(pid)
    }
}

#[cfg(not(unix))]
impl ProcTable for SystemProcs {
    fn parent(&self, _pid: u32) -> Option<u32> {
        None
    }

    fn argv(&self, _pid: u32) -> Option<Vec<String>> {
        None
    }
}

/// The parent from `<root>/<pid>/stat`. The comm sits in parentheses and may
/// hold `) ` itself, so the split runs from the LAST one: what follows is the
/// state letter, then the ppid.
#[cfg(target_os = "linux")]
fn parent_at(root: &Path, pid: u32) -> Option<u32> {
    let path = root.join(pid.to_string()).join("stat");
    let bytes =
        crate::runtime::read_regular_capped(&path, crate::runtime::RECORD_READ_CAP).ok()??;
    let stat = String::from_utf8_lossy(&bytes);
    let rest = stat.rsplit_once(") ")?.1;
    rest.split_whitespace().nth(1)?.parse().ok()
}

/// The NUL-separated argv from `<root>/<pid>/cmdline`, without the trailing
/// NUL's empty tail.
#[cfg(target_os = "linux")]
fn argv_at(root: &Path, pid: u32) -> Option<Vec<String>> {
    let path = root.join(pid.to_string()).join("cmdline");
    let bytes =
        crate::runtime::read_regular_capped(&path, crate::runtime::RECORD_READ_CAP).ok()??;
    let mut argv: Vec<String> = bytes
        .split(|byte| *byte == 0)
        .map(|arg| String::from_utf8_lossy(arg).into_owned())
        .collect();
    if argv.last().is_some_and(String::is_empty) {
        argv.pop();
    }
    (!argv.is_empty()).then_some(argv)
}

#[cfg(all(unix, not(target_os = "linux")))]
fn ps_field(pid: u32, field: &str) -> Option<String> {
    let out = std::process::Command::new("ps")
        .args(["-o", field, "-p", &pid.to_string()])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!text.is_empty()).then_some(text)
}

#[cfg(all(unix, not(target_os = "linux")))]
fn ps_parent(pid: u32) -> Option<u32> {
    ps_field(pid, "ppid=")?.parse().ok()
}

#[cfg(all(unix, not(target_os = "linux")))]
fn ps_argv(pid: u32) -> Option<Vec<String>> {
    Some(
        ps_field(pid, "args=")?
            .split_whitespace()
            .map(str::to_owned)
            .collect(),
    )
}

/// What `which --pid` resolves against, injected so tests never touch the
/// real registry, `$CODEX_HOME` or the global credentials.
pub(crate) struct PidContext<'a> {
    /// `~/.clauth/live_sessions`.
    pub(crate) registry: &'a Path,
    /// `~/.clauth/profiles`.
    pub(crate) profiles: &'a Path,
    /// The CALLER's `$CODEX_HOME`, else `~/.codex`.
    pub(crate) codex_home: &'a Path,
    /// The profile owning the global credentials, asked only for a bare `claude`.
    pub(crate) global: &'a dyn Fn() -> Option<String>,
}

pub(crate) fn resolve_pid(
    pid: u32,
    procs: &dyn ProcTable,
    ctx: &PidContext<'_>,
) -> Option<PidResolution> {
    let rows = crate::live_sessions::rows_in(ctx.registry);
    let mut cursor = Some(pid);
    let mut depth = 0;
    let mut target_argv = None;
    while depth < WALK_LIMIT {
        let Some(current) = cursor.filter(|p| *p > 1) else {
            break;
        };
        let Some(argv) = procs.argv(current) else {
            break;
        };
        if depth == 0 {
            target_argv = Some(argv.clone());
        }
        // A `clauth mcp` hop is never matched: rows keyed on it belong to
        // delegate runs the pane hosts, never the pane. The climb still passes
        // through it, because the pane's own supervisor sits beyond.
        if !is_clauth_mcp(&argv)
            && let Some(row) = newest_row(&rows, current)
        {
            return Some(row_resolution(row));
        }
        cursor = procs.parent(current);
        depth += 1;
    }
    match agent_kind(&target_argv?)? {
        crate::harness::Harness::Claude => (ctx.global)().map(|profile| PidResolution {
            profile,
            source: PidSource::Global,
            harness: crate::harness::Harness::Claude,
        }),
        crate::harness::Harness::Codex => {
            adopted_codex_profile(ctx.profiles, ctx.codex_home).map(|profile| PidResolution {
                profile,
                source: PidSource::CodexAdopted,
                harness: crate::harness::Harness::Codex,
            })
        }
    }
}

/// The newest row keyed on `pid`. A recycled pid can match a stale row and a
/// live one at once, and the stale one may sort first by name.
fn newest_row(
    rows: &[(
        crate::live_sessions::LiveSession,
        Option<std::time::SystemTime>,
    )],
    pid: u32,
) -> Option<&crate::live_sessions::LiveSession> {
    rows.iter()
        .filter(|(row, _)| row.pid == pid)
        .max_by(|(_, left), (_, right)| left.cmp(right))
        .map(|(row, _)| row)
}

/// A row answers with the member a `--with-fallback` session swapped onto,
/// else its launch member.
fn row_resolution(row: &crate::live_sessions::LiveSession) -> PidResolution {
    let profile = row
        .current_member
        .as_deref()
        .filter(|member| !member.is_empty())
        .unwrap_or(&row.start_profile)
        .to_owned();
    PidResolution {
        profile,
        source: PidSource::LiveSession,
        harness: row.harness,
    }
}

fn is_clauth_mcp(argv: &[String]) -> bool {
    matches!(
        argv,
        [program, subcommand, ..]
            if command_basename(program) == Some("clauth") && subcommand == "mcp"
    )
}

/// Which agent a process runs, off its PROGRAM and never a substring anywhere
/// in the argv: an editor whose argument names `claude` spends no account.
fn agent_kind(argv: &[String]) -> Option<crate::harness::Harness> {
    let program = argv.first()?;
    let basename = command_basename(program)?;
    if basename == "claude" || program.contains("/claude/versions/") {
        return Some(crate::harness::Harness::Claude);
    }
    if basename == "codex" {
        return Some(crate::harness::Harness::Codex);
    }
    if matches!(basename, "node" | "bun" | "deno") {
        let script = argv.get(1)?;
        if script.contains("/@anthropic-ai/claude-code/") {
            return Some(crate::harness::Harness::Claude);
        }
        if script.contains("/@openai/codex/") {
            return Some(crate::harness::Harness::Codex);
        }
    }
    None
}

fn command_basename(program: &str) -> Option<&str> {
    Path::new(program).file_name()?.to_str()
}

/// The profile a bare codex's login was adopted into: `$CODEX_HOME/auth.json`
/// is a link onto `profiles/<name>/auth.json` after
/// `clauth login <name> --codex`. A plain file, a link elsewhere, or a nested
/// target names no profile.
fn adopted_codex_profile(profiles: &Path, codex_home: &Path) -> Option<String> {
    let target = std::fs::read_link(codex_home.join("auth.json")).ok()?;
    let target = if target.is_absolute() {
        target
    } else {
        codex_home.join(target)
    };
    let target = lexical_normalize(&target);
    let profiles = lexical_normalize(profiles);
    let relative = target.strip_prefix(&profiles).ok()?;
    let mut parts = relative.components();
    let std::path::Component::Normal(name) = parts.next()? else {
        return None;
    };
    if parts.next()?.as_os_str().to_str()? != "auth.json" {
        return None;
    }
    if parts.next().is_some() {
        return None;
    }
    name.to_str().map(str::to_owned)
}

/// Resolve `..` without touching the filesystem: a relative adoption link is
/// read as the caller spelled it, and its target may not exist yet.
fn lexical_normalize(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut normalized = PathBuf::new();
    for part in path.components() {
        match part {
            Component::ParentDir => {
                normalized.pop();
            }
            Component::CurDir => {}
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

/// `clauth which --pid <pid>`: the account a running process spends.
pub(crate) fn run_pid(pid: u32, json: bool) -> Result<()> {
    if cfg!(windows) {
        anyhow::bail!("`clauth which --pid` is unsupported on Windows");
    }
    let resolved = resolve_pid_env(pid, &SystemProcs, &|| load_config().ok())?;
    if json {
        outln!("{}", pid_json_view(resolved.as_ref()));
    } else {
        emit_plain(resolved.as_ref().map(|resolved| resolved.profile.as_str()));
    }
    Ok(())
}

/// `which --pid`'s answer for `pid` against this process's environment: the
/// registry and profiles under `~/.clauth`, the caller's codex home, and the
/// global login. `config` is called only when the global login is asked for,
/// so a registry or adoption answer never depends on `profiles.toml`.
pub(crate) fn resolve_pid_env(
    pid: u32,
    procs: &dyn ProcTable,
    config: &dyn Fn() -> Option<AppConfig>,
) -> Result<Option<PidResolution>> {
    let base = crate::profile::clauth_dir()?;
    let registry = base.join("live_sessions");
    let profiles = base.join("profiles");
    let codex_home = caller_codex_home()?;
    let global = || {
        config()
            .as_ref()
            .and_then(resolve_global)
            .map(|(name, _)| name)
    };
    let ctx = PidContext {
        registry: &registry,
        profiles: &profiles,
        codex_home: &codex_home,
        global: &global,
    };
    Ok(resolve_pid(pid, procs, &ctx))
}

/// A codex adoption is read from the CALLER's home, never the target
/// process's: the link is the caller's own `codex login`, which the target
/// inherited nothing of.
fn caller_codex_home() -> Result<PathBuf> {
    match std::env::var_os("CODEX_HOME").filter(|dir| !dir.is_empty()) {
        Some(home) => Ok(PathBuf::from(home)),
        None => Ok(crate::profile::home_dir()?.join(".codex")),
    }
}

fn pid_json_view(resolved: Option<&PidResolution>) -> serde_json::Value {
    serde_json::json!({
        "profile": resolved.map(|resolved| resolved.profile.as_str()),
        "source": resolved.map(|resolved| resolved.source.as_str()),
        "harness": resolved.map(|resolved| resolved.harness.as_str()),
    })
}

#[cfg(test)]
#[path = "../tests/inline/which.rs"]
mod tests;
