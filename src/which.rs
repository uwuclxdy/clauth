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
    // And a clauth-SHAPED home answers or answers unknown — never falls
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
                // unknown, not somebody else's profile.
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
    match (dir.canonicalize(), claude_dir.canonicalize()) {
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
    let bytes = std::fs::read(keychain_cache_entry_path(dir, service)).ok()?;
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

#[cfg(test)]
#[path = "../tests/inline/which.rs"]
mod tests;
