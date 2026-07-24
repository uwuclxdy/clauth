use std::env;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::lock::with_state_lock;
use crate::profile::{
    AppConfig, ClaudeCredentials, Profile, atomic_write_600, claude_dir, profile_dir,
    read_json_file, save_profile,
};

fn claude_credentials_path() -> Result<PathBuf> {
    Ok(claude_dir()?.join(".credentials.json"))
}

fn claude_settings_path() -> Result<PathBuf> {
    Ok(claude_dir()?.join("settings.json"))
}

/// CLA-SPLIT: true when the profile carries a long-lived session token
/// (`session-token.json`, e.g. a `claude setup-token` mint). Such a profile
/// splits its credentials: the STATIC session token is what switches install
/// for Claude Code sessions to run on, while the rotating OAuth pair in
/// `credentials.json` stays clauth-private for usage polling. Sessions then
/// hold a token that never rotates, so they can never race clauth's refresher
/// on a single-use refresh chain (the root cause of the 2026-07-16..18
/// serial `refresh token revoked` deaths: N live sessions + clauth all
/// rotating the same chains through one live slot).
pub(crate) fn has_session_token(name: &str) -> bool {
    matches!(
        session_token_status(name),
        Some(SessionTokenStatus::LongLived(_))
    )
}

/// What the `session-token.json` sidecar actually holds (#53 review: the
/// split must engage only when the installed token IS long-lived).
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum SessionTokenStatus {
    /// A genuine long-lived login — defined by carrying NO refresh token:
    /// with nothing to rotate, sessions can never race clauth's refresher on
    /// it, which is the whole point of the split. Carries the recorded
    /// epoch-ms expiry when stamped.
    LongLived(Option<i64>),
    /// The sidecar holds a rotating pair (refresh token present) — a
    /// mis-fill, not a `claude setup-token` mint. The split stays DISENGAGED:
    /// installing it would put a dies-in-hours token in front of sessions
    /// with no refresher behind it, so switches keep installing
    /// `credentials.json` as if the sidecar weren't there.
    NotLongLived,
}

impl SessionTokenStatus {
    /// Whether this profile actually runs its sessions on a long-lived token —
    /// the "token mode" the overview type tag marks. A `NotLongLived` sidecar is
    /// disengaged (sessions run on `credentials.json`), so it is NOT token mode.
    pub(crate) fn is_long_lived_mode(&self) -> bool {
        matches!(self, SessionTokenStatus::LongLived(_))
    }

    /// Whether the sidecar is in a state a switch would install to sessions'
    /// harm: an expired long-lived token (every switch signs sessions out) or a
    /// mis-fill the operator believes is armed. Drives the overview `⊘` marker.
    /// A stamped-but-live or unstamped long-lived token is fine (`false`).
    pub(crate) fn is_danger(&self, now_ms: i64) -> bool {
        match self {
            SessionTokenStatus::LongLived(Some(ms)) => now_ms >= *ms,
            SessionTokenStatus::LongLived(None) => false,
            SessionTokenStatus::NotLongLived => true,
        }
    }
}

/// Content-aware read of a profile's sidecar: `None` = no sidecar (or one too
/// corrupt to parse a login out of — same disengaged outcome either way).
pub(crate) fn session_token_status(name: &str) -> Option<SessionTokenStatus> {
    let path = profile_dir(name).ok()?.join("session-token.json");
    if !path.exists() {
        return None;
    }
    let creds = read_json_file::<ClaudeCredentials>(&path).ok()?;
    let oauth = creds.claude_ai_oauth.as_ref()?;
    if oauth.refresh_token.is_some() {
        return Some(SessionTokenStatus::NotLongLived);
    }
    Some(SessionTokenStatus::LongLived(oauth.expires_at))
}

/// Documented lifetime of a `claude setup-token` mint (~1 year). The minted
/// string carries no expiry of its own, so the capture flow stamps this
/// assumed horizon into the sidecar — the Setup-tab countdown and
/// `ensure_installable`'s clock gate both read that stamp, and a re-mint
/// refreshes it. An operator who knows better can edit `expiresAt` by hand.
pub(crate) const SETUP_TOKEN_ASSUMED_LIFETIME_MS: i64 = 365 * 24 * 60 * 60 * 1000;

/// Scopes a `claude setup-token` mint carries (verified live in the #52 root
/// cause: `/api/oauth/usage` 403s them, which is exactly why the rotating
/// usage pair stays separate). Recorded in the sidecar for the record.
const SETUP_TOKEN_SCOPES: [&str; 2] = ["user:inference", "user:sessions:claude_code"];

/// CLA-FEED: the mint-vs-fed horizon discriminator. A genuine `setup-token`
/// mint is stamped ~1 year out; a fed access token dies in hours. An expiry
/// under this horizon is FED-shaped; at or beyond it, MINT-shaped. Shared by
/// [`preserve_static_mint`] (never back up a fed value as "the mint") and the
/// install gate (a mint on a feed profile is a live fallback to be
/// superseded, not a fresh fed token).
pub(crate) const MINT_HORIZON_MS: i64 = 30 * 24 * 60 * 60 * 1000;

/// Shape-check a pasted `claude setup-token` mint before anything is written:
/// trimmed, non-empty, `sk-ant-` prefixed, no interior whitespace (a partial
/// paste or a paste-with-prompt both fail loud here instead of producing a
/// sidecar that signs sessions out on first use). Returns the trimmed token.
/// Never logs the value — the error names the failure, not the paste.
pub(crate) fn validate_setup_token(raw: &str) -> Result<String> {
    let token = raw.trim();
    if token.is_empty() {
        anyhow::bail!("no token pasted");
    }
    if !token.starts_with("sk-ant-") {
        anyhow::bail!(
            "that doesn't look like a `claude setup-token` mint (expected an sk-ant-… value)"
        );
    }
    if token.starts_with("sk-ant-api") {
        anyhow::bail!(
            "that looks like an API key (sk-ant-api…), not a `claude setup-token` mint. \
             Installing it as the session bearer signs sessions out on first use; capture an \
             API key with `clauth login <name> --base-url <url> --api-key <key>` instead"
        );
    }
    if token.chars().any(char::is_whitespace) {
        anyhow::bail!(
            "the pasted token contains whitespace — looks like a partial or padded paste"
        );
    }
    if token.len() < 40 {
        anyhow::bail!("the pasted token is too short to be a real mint");
    }
    Ok(token.to_string())
}

/// Reject an api key that can't be a well-formed HTTP header value. CC forwards
/// the `apiKeyHelper` stdout verbatim as `X-Api-Key` / `Authorization: Bearer`,
/// so an interior control char (a CRLF from a bad paste or a hand-edited
/// `config.toml`) would inject or malform a header. Callers trim first, so any
/// remaining whitespace or control char is a real defect.
pub(crate) fn validate_api_key(key: &str) -> Result<()> {
    if key.chars().any(|c| c.is_control() || c.is_whitespace()) {
        anyhow::bail!(
            "api key contains whitespace or control characters — a bad paste or an edited config"
        );
    }
    Ok(())
}

/// Write `name`'s `session-token.json` from a validated mint, stamping the
/// assumed one-year expiry. 0600 like every credential file. Returns the
/// stamped epoch-ms expiry for the caller's summary line.
pub(crate) fn write_session_token(name: &str, token: &str, now_ms: i64) -> Result<i64> {
    let expires_at = now_ms + SETUP_TOKEN_ASSUMED_LIFETIME_MS;
    let sidecar = ClaudeCredentials {
        claude_ai_oauth: Some(crate::profile::OAuthToken {
            access_token: token.to_string(),
            refresh_token: None,
            expires_at: Some(expires_at),
            scopes: Some(SETUP_TOKEN_SCOPES.iter().map(|s| s.to_string()).collect()),
            subscription_type: None,
        }),
    };
    let bytes = serde_json::to_vec_pretty(&sidecar).context("serialize session token")?;
    let path = profile_dir(name)?.join("session-token.json");
    with_state_lock(|| {
        atomic_write_600(&path, &bytes).context("write session-token.json")?;
        Ok(())
    })?;
    Ok(expires_at)
}

/// CLA-FEED: write `name`'s `session-token.json` from the usage chain's
/// just-persisted OAuth fields — the access token as a Fable-capable bearer
/// with the chain's REAL expiry, full scopes, and `subscriptionType`, and NO
/// refresh token (the classifier stays [`SessionTokenStatus::LongLived`], so
/// every split guard keeps working unmodified; sessions get nothing to rotate,
/// the refresh chain stays clauth-private). The honest expiry is deliberate: a
/// dead feed must LOOK dead on every surface (the CLA-SPLIT-3 display-gap
/// lesson), never a far-future stamp over a dies-in-hours token.
///
/// Before the FIRST feed overwrites a genuine static mint, the mint is
/// preserved at `session-token.static.json` ([`preserve_static_mint`]) so
/// disabling the feed — or a terminally dead chain — can restore Sonnet-cap
/// service instead of signing sessions out.
pub(crate) fn feed_session_token(name: &str, chain: &crate::profile::OAuthToken) -> Result<()> {
    let sidecar = ClaudeCredentials {
        claude_ai_oauth: Some(crate::profile::OAuthToken {
            access_token: chain.access_token.clone(),
            refresh_token: None,
            expires_at: chain.expires_at,
            scopes: chain.scopes.clone(),
            subscription_type: chain.subscription_type.clone(),
        }),
    };
    let bytes = serde_json::to_vec_pretty(&sidecar).context("serialize fed session token")?;
    let path = profile_dir(name)?.join("session-token.json");
    with_state_lock(|| {
        preserve_static_mint(name)?;
        atomic_write_600(&path, &bytes).context("write fed session-token.json")?;
        Ok(())
    })
}

/// Copy a genuine static mint aside to `session-token.static.json` before the
/// feed first overwrites it. Idempotent: an existing backup is never replaced
/// (the first preserved mint is the real one — later sidecar contents are fed
/// values), and a sidecar that is absent or holds a fed/mis-filled value has
/// no mint to preserve. Callers hold the state flock.
fn preserve_static_mint(name: &str) -> Result<()> {
    let dir = profile_dir(name)?;
    let sidecar = dir.join("session-token.json");
    let backup = dir.join("session-token.static.json");
    if backup.exists() || !sidecar.exists() {
        return Ok(());
    }
    // Only a long-lived sidecar that was NOT produced by the feed is a mint
    // worth preserving. Fed sidecars carry the chain's `subscriptionType`;
    // `write_session_token` mints never do — that asymmetry plus the scope
    // list distinguishes them without a marker key (the sidecar shape is a
    // wire contract with CC and ccsbar and stays unmarked).
    let Ok(creds) = read_json_file::<ClaudeCredentials>(&sidecar) else {
        return Ok(());
    };
    let Some(oauth) = creds.claude_ai_oauth.as_ref() else {
        return Ok(());
    };
    if oauth.refresh_token.is_some() || oauth.subscription_type.is_some() {
        return Ok(());
    }
    // Horizon check, the robust discriminator: a genuine mint is stamped ~1
    // year out; a fed access token dies in hours. A stamp under 30 days (or a
    // sidecar already expired) is not a mint worth preserving — backing up a
    // dies-in-hours value would make a later restore install a dead token.
    let now = crate::usage::now_ms() as i64;
    if oauth
        .expires_at
        .is_some_and(|exp| exp < now + MINT_HORIZON_MS)
    {
        return Ok(());
    }
    let bytes = std::fs::read(&sidecar).context("read static mint for preservation")?;
    atomic_write_600(&backup, bytes).context("write session-token.static.json")
}

/// CLA-FEED: capture a fresh mint into the sidecar AND stamp it as the static
/// backup, in ONE state-flock section from the SAME serialized bytes — the
/// re-mint path on a feed-enabled profile. A two-step (write sidecar, then
/// read it back into the backup) leaves a window where a concurrent rotation
/// feed replaces the mint with an hours-horizon token that then gets
/// snapshotted as "the mint" (review round 1: the poisoned-degrade-backup
/// TOCTOU). Returns the stamped expiry like [`write_session_token`].
pub(crate) fn write_session_token_with_backup(name: &str, token: &str, now_ms: i64) -> Result<i64> {
    let expires_at = now_ms + SETUP_TOKEN_ASSUMED_LIFETIME_MS;
    let sidecar = ClaudeCredentials {
        claude_ai_oauth: Some(crate::profile::OAuthToken {
            access_token: token.to_string(),
            refresh_token: None,
            expires_at: Some(expires_at),
            scopes: Some(SETUP_TOKEN_SCOPES.iter().map(|s| s.to_string()).collect()),
            subscription_type: None,
        }),
    };
    let bytes = serde_json::to_vec_pretty(&sidecar).context("serialize session token")?;
    let dir = profile_dir(name)?;
    with_state_lock(|| {
        atomic_write_600(&dir.join("session-token.json"), &bytes)
            .context("write session-token.json")?;
        atomic_write_600(&dir.join("session-token.static.json"), &bytes)
            .context("write session-token.static.json")?;
        Ok(())
    })?;
    Ok(expires_at)
}

/// CLA-FEED: heal a mis-filled sidecar on a FEED profile — quarantine the
/// evidence (`~/.clauth/quarantine/…-<name>.session-token.json`), restore the
/// preserved static mint over it. `Ok(true)` = healed; `Ok(false)` = no
/// backup to restore (the caller keeps the disengaged-vanilla posture) OR the
/// sidecar is not actually mis-filled (re-checked under the lock — a
/// concurrent repair may have beaten us).
pub(crate) fn heal_misfilled_sidecar(name: &str) -> Result<bool> {
    let dir = profile_dir(name)?;
    let sidecar = dir.join("session-token.json");
    let backup = dir.join("session-token.static.json");
    with_state_lock(|| {
        if !backup.exists()
            || !matches!(
                session_token_status(name),
                Some(SessionTokenStatus::NotLongLived)
            )
        {
            return Ok(false);
        }
        quarantine_sidecar_locked(name, &sidecar)?;
        let bytes = std::fs::read(&backup).context("read session-token.static.json")?;
        atomic_write_600(&sidecar, bytes).context("restore session-token.json")?;
        std::fs::remove_file(&backup).context("remove consumed static backup")?;
        Ok(true)
    })
}

/// CLA-FEED: quarantine a mis-filled sidecar and REMOVE it (leaving the
/// sidecar absent) — the CLI `clauth feed <p> on` pre-clear, where overwriting
/// is explicit operator intent but the evidence still goes to quarantine
/// first. `Ok(true)` when a mis-fill was cleared.
pub(crate) fn quarantine_misfilled_sidecar(name: &str) -> Result<bool> {
    let dir = profile_dir(name)?;
    let sidecar = dir.join("session-token.json");
    with_state_lock(|| {
        if !matches!(
            session_token_status(name),
            Some(SessionTokenStatus::NotLongLived)
        ) {
            return Ok(false);
        }
        quarantine_sidecar_locked(name, &sidecar)?;
        std::fs::remove_file(&sidecar).context("remove mis-filled session-token.json")?;
        Ok(true)
    })
}

/// Copy the sidecar's bytes into the quarantine dir (same naming scheme as
/// [`archive_live_credentials`], `.session-token.json` suffixed). Callers hold
/// the state flock.
fn quarantine_sidecar_locked(name: &str, sidecar: &Path) -> Result<()> {
    let bytes = std::fs::read(sidecar).context("read mis-filled sidecar for quarantine")?;
    let dir = crate::profile::clauth_dir()?.join("quarantine");
    std::fs::create_dir_all(&dir).context("failed to create quarantine dir")?;
    static QUARANTINE_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = QUARANTINE_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dest = dir.join(format!(
        "{}-{seq:04}-{name}.session-token.json",
        crate::usage::now_ms()
    ));
    atomic_write_600(&dest, bytes).context("write quarantined sidecar")
}

/// CLA-FEED: best-effort arming at session start (`clauth start` resolves its
/// credentials through [`install_source_path`], never `ensure_installable`) —
/// a feed profile whose sidecar is absent or stale is fed from the
/// DISK-loaded chain when comfortably live, so a session launched inside an
/// arming window never copies the rotating pair (review round 1: the
/// pinned-pair double-spend). Never touches a NotLongLived mis-fill, never
/// spends a refresh, and never fails the caller — a feed hiccup must not
/// block a session start (the vanilla fallback still works; the daemon heals
/// the sidecar on its next rotation).
pub(crate) fn arm_feed_from_disk(name: &str) {
    const FEED_ARM_GRACE_MS: i64 = 30 * 60 * 1000;
    let Ok(profile) = crate::profile::load_profile(name) else {
        return;
    };
    if !profile.session_feed {
        return;
    }
    let now = crate::usage::now_ms() as i64;
    match session_token_status(name) {
        // Mis-fill: evidence stays; NotLongLived semantics apply elsewhere.
        Some(SessionTokenStatus::NotLongLived) => return,
        // A comfortably live fed token (or a healthy mint) needs nothing.
        Some(SessionTokenStatus::LongLived(exp))
            if exp.is_none_or(|e| now + FEED_ARM_GRACE_MS < e) =>
        {
            return;
        }
        _ => {}
    }
    let Some(oauth) = profile
        .credentials
        .as_ref()
        .and_then(|c| c.claude_ai_oauth.as_ref())
    else {
        return;
    };
    if oauth
        .expires_at
        .is_none_or(|e| now + FEED_ARM_GRACE_MS >= e)
    {
        return; // chain itself stale — the daemon's guarded refresh will feed
    }
    // Serialize the read-and-restamp with rotations; a busy guard means a
    // rotation is arming it right now.
    let Ok(_guard) = crate::runtime::RotationGuard::acquire(name) else {
        return;
    };
    if let Err(e) = feed_session_token(name, oauth) {
        crate::logline::logline!("clauth: start-time feed arming for '{name}' failed: {e:#}");
    }
}

/// Restore the preserved static mint over the fed sidecar (feed disabled, or
/// the usage chain died terminally). `Ok(true)` when a backup existed and was
/// restored; `Ok(false)` when there was nothing to restore (the sidecar is
/// left as-is — a last fed token keeps serving until its real expiry).
pub(crate) fn restore_static_mint(name: &str) -> Result<bool> {
    let dir = profile_dir(name)?;
    let backup = dir.join("session-token.static.json");
    let sidecar = dir.join("session-token.json");
    with_state_lock(|| {
        if !backup.exists() {
            return Ok(false);
        }
        let bytes = std::fs::read(&backup).context("read session-token.static.json")?;
        atomic_write_600(&sidecar, bytes).context("restore session-token.json")?;
        std::fs::remove_file(&backup).context("remove consumed static backup")?;
        Ok(true)
    })
}

/// The file a switch INSTALLS as the live login: the profile's
/// `session-token.json` when present ([`has_session_token`]), else its
/// `credentials.json` — which is exactly the pre-split behavior, so profiles
/// without the sidecar are byte-identical to before.
pub(crate) fn install_source_path(name: &str) -> Result<PathBuf> {
    let dir = profile_dir(name)?;
    // Content-aware, not a bare existence check (#53 review): a sidecar that
    // isn't genuinely long-lived must not become the install source — see
    // [`SessionTokenStatus::NotLongLived`].
    if has_session_token(name) {
        return Ok(dir.join("session-token.json"));
    }
    Ok(dir.join("credentials.json"))
}

/// State of `~/.claude/.credentials.json` relative to a profile's stored credentials.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LinkState {
    /// Symlink resolves to the profile's stored credentials, OR a regular file
    /// whose live OAuth access token matches the profile's stored one (macOS: Claude
    /// Code rewrites the file from the Keychain, replacing our symlink with an
    /// identical-content regular file — not divergence).
    LinkedTo,
    /// Path exists and its live credential differs from the profile's stored one —
    /// a genuine CC re-login / token rotation the user may want to capture.
    Diverged,
    /// Path does not exist.
    Missing,
}

pub(crate) fn classify_credentials_link(active: &str) -> Result<LinkState> {
    let link = claude_credentials_path()?;
    // CLA-SPLIT: the live slot is compared against what a switch INSTALLS —
    // for a session-token profile that's the static token, so a live slot
    // holding it classifies LinkedTo and the whole divergence machinery
    // stays dormant (a static token never rotates out from under us).
    let expected = install_source_path(active)?;
    classify_link_at(&link, &expected)
}

/// Classify a symlink at `link` against `expected`; canonical paths when resolvable.
pub(crate) fn classify_link_at(link: &Path, expected: &Path) -> Result<LinkState> {
    let meta = match link.symlink_metadata() {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(LinkState::Missing),
        Err(e) => return Err(e).context("failed to stat .credentials.json"),
    };
    if !meta.file_type().is_symlink() {
        // The live credentials are a regular file, not our symlink. This is
        // handled the same way on EVERY platform (not gated to macOS): trust
        // content, not symlink identity, so an ordinary switch doesn't falsely
        // prompt to capture credentials that already match the profile. macOS is
        // where it happens on every run — Claude Code rewrites
        // ~/.claude/.credentials.json as a regular-file mirror of the Keychain,
        // clobbering our symlink — but the same regular-file state can arise on
        // Linux/Windows if anything replaces the symlink, and the correct answer
        // is identical: equal access token ⇒ LinkedTo, otherwise a genuine
        // re-login / rotation ⇒ Diverged. (Gating this to macOS would make a
        // non-symlink file on other platforms fall through to `read_link` below
        // and error.)
        return Ok(
            match (
                read_json_file::<ClaudeCredentials>(link),
                read_json_file::<ClaudeCredentials>(expected),
            ) {
                (Ok(live), Ok(stored))
                    if live.access_token().is_some_and(|t| !t.is_empty())
                        && live.access_token() == stored.access_token() =>
                {
                    LinkState::LinkedTo
                }
                _ => LinkState::Diverged,
            },
        );
    }
    let target = std::fs::read_link(link).context("failed to read .credentials.json link")?;
    if paths_equivalent(&target, expected) {
        Ok(LinkState::LinkedTo)
    } else {
        Ok(LinkState::Diverged)
    }
}

fn paths_equivalent(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

/// True when the profile has no stored credentials but the live path is a regular
/// file with a completed OAuth login — first login after blank profile creation.
/// clauth adopts this rather than treating it as divergence.
/// SipHash of the live access token — a cheap identity for "which login sits
/// in `~/.claude/.credentials.json` right now". It changes on every re-login
/// and every refresh, so memos keyed to it release exactly when the creds
/// change. `None` when no readable OAuth login is present. Shared by the
/// TUI's divergence banner and the daemon's follow/refusal memos.
pub(crate) fn live_credentials_fingerprint() -> Option<u64> {
    use std::hash::{DefaultHasher, Hash, Hasher};
    let creds = read_claude_credentials().ok().flatten()?;
    let token = creds.access_token().filter(|t| !t.is_empty())?;
    let mut hasher = DefaultHasher::new();
    token.hash(&mut hasher);
    Some(hasher.finish())
}

/// True when the live credentials hold NO usable login: no OAuth block, or one
/// whose access AND refresh tokens are both absent/blank — Claude Code's
/// logged-out shell (it blanks the tokens and zeroes `expiresAt` when its own
/// refresh dies, keeping unrelated keys like `mcpOAuth`). A shell still
/// classifies [`LinkState::Diverged`], but there is no login in it to protect.
pub(crate) fn live_login_is_empty(creds: &ClaudeCredentials) -> bool {
    creds.access_token().filter(|t| !t.is_empty()).is_none()
        && creds.refresh_token().filter(|t| !t.is_empty()).is_none()
}

pub(crate) fn is_first_login(active: &str) -> Result<bool> {
    let link = claude_credentials_path()?;
    // CLA-SPLIT: a profile whose install source is its session token is never
    // "credential-less" — a live OAuth login must not be adopted over it.
    let expected = install_source_path(active)?;
    Ok(is_first_login_at(&link, &expected))
}

/// Path-based core of [`is_first_login`], split for testing. The OAuth check
/// rejects partial writes (e.g. `{}`) and a logged-out shell (blank tokens,
/// see [`live_login_is_empty`]) so adoption waits for a completed login —
/// otherwise a shell's `claudeAiOauth` block alone would pass, and adopting
/// it later strands `force_link_profile_credentials` with no install source
/// to relink, deleting the live file (and its unrelated `mcpOAuth`) outright.
fn is_first_login_at(link: &Path, expected: &Path) -> bool {
    if expected.exists() {
        return false;
    }
    let Ok(meta) = link.symlink_metadata() else {
        return false;
    };
    if meta.file_type().is_symlink() {
        return false;
    }
    std::fs::read(link)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<ClaudeCredentials>(&bytes).ok())
        .is_some_and(|creds| !live_login_is_empty(&creds))
}

/// True when the live `.credentials.json` login is already saved in `active`'s
/// store — so the unsaved-credentials gates have nothing to protect and must
/// not defer a switch (or raise the divergence prompt) on it. Two ways to be
/// saved, one structural and one by content:
///
/// * The live slot is clauth's own symlink. CC writes a regular file; only a
///   switch symlinks the slot, so a symlink there points into a profile store
///   by construction — that login is saved whatever it resolves to, even if
///   the target is momentarily unreadable (a store file removed under a live
///   link).
/// * The live login's OAuth access token matches one of `active`'s stored
///   credential files (`credentials.json` or `session-token.json`). This is
///   the cross-platform half, and the one that matters on macOS: Claude Code
///   rewrites `~/.claude/.credentials.json` as a regular-file mirror of the
///   Keychain after every run, clobbering our symlink with an identical-content
///   regular file — `is_symlink()` then reads false, but the content is still
///   saved. On Windows the live slot is always a copy (no symlinks), so the
///   content half carries it there too — no unix-only footnote.
///
/// The `Diverged`-but-saved state this clears arises when a profile's INSTALL
/// SOURCE changes under the live slot: capturing a `setup-token` sidecar for
/// the ACTIVE profile flips [`install_source_path`] from `credentials.json` to
/// `session-token.json` (removing it flips back) while the live slot still
/// holds the previous source's content — a stale slot the next switch
/// re-installs, not an unsaved login. Both stores are checked because the flip
/// can leave the slot holding either the OAuth login or the static mint.
/// Without this exemption every unattended switch fails "unsaved credentials;
/// resolve in the TUI" until its retry TTL, and the TUI prompts about
/// credentials that are fully saved (observed live 2026-07-21 on the macOS
/// fork as a symlink; recurs there as a regular file after any CC session).
pub(crate) fn live_login_is_stored(active: &str) -> bool {
    let Ok(link) = claude_credentials_path() else {
        return false;
    };
    // Structural half: a symlink at the live slot is clauth's own, pointing
    // into a store by construction — saved even if the target is unreadable.
    if link
        .symlink_metadata()
        .is_ok_and(|m| m.file_type().is_symlink())
    {
        return true;
    }
    // Content half (the macOS regular-file mirror, the Windows copy): the live
    // login's token equals one the profile already stores. A blank/absent live
    // token can't "match" a real login — a logged-out shell is handled by
    // [`live_credentials_are_shell`], not here.
    let Ok(dir) = profile_dir(active) else {
        return false;
    };
    let Ok(live) = read_json_file::<ClaudeCredentials>(&link) else {
        return false;
    };
    if live.access_token().filter(|t| !t.is_empty()).is_none() {
        return false;
    }
    ["credentials.json", "session-token.json"]
        .into_iter()
        .any(|file| {
            read_json_file::<ClaudeCredentials>(&dir.join(file))
                .ok()
                .is_some_and(|stored| stored.access_token() == live.access_token())
        })
}

pub(crate) fn read_claude_credentials() -> Result<Option<ClaudeCredentials>> {
    let path = claude_credentials_path()?;
    if !path.exists() {
        return Ok(None);
    }
    read_json_file(&path).map(Some)
}

/// Outcome of [`write_live_oauth_pair`]: written, or benignly superseded by a
/// concurrent actor (nothing lost, nothing to recover).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LiveWriteBack {
    Written,
    /// The live slot changed hands between the caller's judgment and this
    /// write — a fresh CC login landed, or a profile's own store took the
    /// slot (symlink). The rotated pair continued a lineage that fresh login
    /// just superseded; discarding it loses nothing.
    Superseded,
}

/// Surgically update the LIVE `.credentials.json`'s `claudeAiOauth` block with
/// a freshly rotated pair, preserving every other top-level key (Claude Code
/// also parks e.g. `mcpOAuth` entries in this file — a struct round-trip would
/// silently drop them). Used by the daemon's dead-login rescue (RESCUE-1) when
/// its confirm-dead refresh probe SUCCEEDED: the probe consumed the file's
/// single-use refresh token, so the fresh pair MUST land back in the file or
/// the live session's chain is destroyed.
///
/// RESCUE-2c: the authoritative guards live INSIDE the state flock,
/// immediately before the mutation — a symlinked live path (a profile's own
/// store took the slot; writing an unowned pair through it would corrupt that
/// profile) and a fingerprint that no longer matches `expected_fingerprint`
/// (a fresh CC login landed) both return [`LiveWriteBack::Superseded`]
/// instead of clobbering.
pub(crate) fn write_live_oauth_pair(
    tokens: &crate::oauth::TokenResponse,
    expected_fingerprint: Option<u64>,
) -> Result<LiveWriteBack> {
    with_state_lock(|| {
        let path = claude_credentials_path()?;
        let meta = path
            .symlink_metadata()
            .context("live .credentials.json vanished mid-rescue")?;
        if meta.file_type().is_symlink() || live_credentials_fingerprint() != expected_fingerprint {
            return Ok(LiveWriteBack::Superseded);
        }
        let bytes = std::fs::read(&path).context("failed to read live .credentials.json")?;
        let mut root: serde_json::Value =
            serde_json::from_slice(&bytes).context("live .credentials.json is not JSON")?;
        let obj = root
            .as_object_mut()
            .context("live .credentials.json is not a JSON object")?;
        let oauth = obj
            .entry("claudeAiOauth")
            .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
        let oauth = oauth
            .as_object_mut()
            .context("live claudeAiOauth is not a JSON object")?;
        oauth.insert("accessToken".into(), tokens.access_token.clone().into());
        oauth.insert("refreshToken".into(), tokens.refresh_token.clone().into());
        // CC stores epoch-ms; `expires_in` is seconds-from-now.
        let expires_at = crate::usage::now_ms().saturating_add(tokens.expires_in * 1000);
        oauth.insert("expiresAt".into(), expires_at.into());
        if let Some(scope) = tokens.scope.as_deref().filter(|s| !s.trim().is_empty()) {
            let scopes: Vec<serde_json::Value> =
                scope.split_whitespace().map(|s| s.into()).collect();
            oauth.insert("scopes".into(), scopes.into());
        }
        let out = serde_json::to_vec(&root).context("failed to serialize live credentials")?;
        atomic_write_600(&path, out).context("failed to write live .credentials.json")?;
        Ok(LiveWriteBack::Written)
    })
}

/// [`force_link_profile_credentials`] with the caller's judgment re-verified
/// INSIDE the state flock, immediately before the mutation (RESCUE-2c). The
/// reclaim's `still_unchanged` re-check used to run outside the lock, leaving
/// a syscall-wide window in which a concurrently landed CC login could be
/// destroyed by the unconditional relink. Returns `false` (no-op) when the
/// re-check fails — the caller treats that as "superseded, start over".
pub(crate) fn force_link_profile_credentials_if(
    name: &str,
    still_unchanged: &dyn Fn() -> bool,
) -> Result<bool> {
    with_state_lock(|| {
        if !still_unchanged() {
            return Ok(false);
        }
        force_link_profile_credentials(name).map(|()| true)
    })
}

/// True when the live `.credentials.json` currently parses to such a logged-out
/// shell. An unreadable or non-JSON file reads `false` — it may be a Claude
/// Code write in progress, and "possibly a login" keeps the same protection as
/// a real one (the divergence guards stay armed).
pub(crate) fn live_credentials_are_shell() -> bool {
    matches!(
        read_claude_credentials(),
        Ok(Some(live)) if live_login_is_empty(&live)
    )
}

/// The unsaved-credentials gate shared by every switch / defer / divergence-prompt
/// path: the live login diverges from what a switch to `active` installs AND holds
/// a login worth protecting. Three diverging states carry nothing unsaved and are
/// exempt — a first-login adoption (captured on switch, not stranded), a logged-out
/// shell (blank tokens, see [`live_credentials_are_shell`]), and a login already
/// saved in the profile's store (its content is captured, so re-installing loses
/// no login, see [`live_login_is_stored`]). Routing every gate through this one
/// predicate keeps the exemptions from drifting apart. The underlying reads
/// propagate their error; a boolean gate maps that to `false` with `.unwrap_or(false)`.
pub(crate) fn live_diverged_and_unsaved(active: &str) -> Result<bool> {
    Ok(
        matches!(classify_credentials_link(active)?, LinkState::Diverged)
            && !is_first_login(active)?
            && !live_credentials_are_shell()
            && !live_login_is_stored(active),
    )
}

/// macOS: mirror a profile's stored OAuth login into the Keychain so Claude Code
/// (which reads the Keychain, not the file) actually switches account. No-op when
/// the profile has no stored `credentials.json` (a base_url profile, whose
/// endpoint+token come from `settings.json`, or an OAuth profile not yet logged
/// in) — the existing Keychain login is left untouched in that case.
///
/// Runs after the symlink swap and is `?`-fatal: a failure leaves the file layer
/// switched while CC still reads the old Keychain login. Loud + recoverable —
/// both writes are idempotent, so retrying the switch re-runs the pair.
#[cfg(target_os = "macos")]
fn keychain_write_source(path: &Path) -> Result<()> {
    // CLA-SPLIT: callers pass the already-resolved install source so the
    // symlink target and the Keychain content come from ONE resolution — a
    // session-token.json vanishing between two stats can't split them.
    if !path.exists() {
        return Ok(());
    }
    let creds: ClaudeCredentials = read_json_file(path)?;
    crate::keychain::keychain_write(&creds)
}

#[cfg(unix)]
pub(crate) fn create_symlink(target: &Path, link: &Path) -> Result<()> {
    std::os::unix::fs::symlink(target, link).context("failed to create credential symlink")
}

#[cfg(windows)]
pub(crate) fn create_symlink(target: &Path, link: &Path) -> Result<()> {
    match std::os::windows::fs::symlink_file(target, link) {
        Ok(()) => Ok(()),
        Err(_) => std::fs::copy(target, link)
            .map(|_| ())
            .context("failed to copy credentials"),
    }
}

#[cfg(not(any(unix, windows)))]
pub(crate) fn create_symlink(target: &Path, link: &Path) -> Result<()> {
    std::fs::copy(target, link)
        .map(|_| ())
        .context("failed to copy credentials")
}

/// Symlink `~/.claude/.credentials.json` → profile's `credentials.json` (copy on
/// Windows). Refuses to overwrite a non-matching regular file — that would silently
/// drop a CC re-login the user hasn't resolved yet.
pub(crate) fn link_profile_credentials(name: &str) -> Result<()> {
    with_state_lock(|| {
        let link = claude_credentials_path()?;
        let target = install_source_path(name)?;

        if let Ok(meta) = link.symlink_metadata()
            && !meta.file_type().is_symlink()
        {
            let live_bytes = std::fs::read(&link).ok();
            let target_bytes = std::fs::read(&target).ok();
            if live_bytes != target_bytes {
                anyhow::bail!(
                    "refusing to replace .credentials.json: live file differs from profile '{name}'; {} first",
                    crate::format::RESOLVE_IN_TUI
                );
            }
        }

        // macOS Keychain write FIRST (timeout-sweep 2026-07-18): it is the
        // realistic failure on this path (locked keychain / unanswered ACL
        // prompt → the 20 s subprocess kill), and it must fail BEFORE any
        // local mutation. The old order swapped the symlink first, so a
        // failed write stranded link→new / Keychain→old with no rollback.
        // Claude Code reads the Keychain — failing here leaves every surface
        // consistently on the previous login.
        #[cfg(target_os = "macos")]
        if target.exists() && crate::keychain::enabled() {
            keychain_write_source(&target)?;
        }

        if link.symlink_metadata().is_ok() {
            std::fs::remove_file(&link).context("failed to remove old .credentials.json")?;
        }
        if target.exists() {
            if let Some(parent) = link.parent() {
                std::fs::create_dir_all(parent)?;
            }
            create_symlink(&target, &link)?;
        }

        Ok(())
    })
}

pub(crate) fn clear_claude_credentials() -> Result<()> {
    with_state_lock(|| {
        let link = claude_credentials_path()?;
        if link.symlink_metadata().is_ok() {
            std::fs::remove_file(&link).context("failed to remove .credentials.json")?;
        }
        // macOS: also drop the live Keychain login so Claude Code can't spend the
        // account (parity with removing the credential file). This deletes
        // whatever the item holds at that moment — possibly a chain CC rotated
        // after our last capture, or a login clauth never wrote. The write-only
        // design can't snapshot it first (reading Claude's item prompts on every
        // call), so that tail is lost and needs a re-login; see keychain.rs.
        #[cfg(target_os = "macos")]
        if crate::keychain::enabled() {
            crate::keychain::keychain_delete()?;
        }
        Ok(())
    })
}

pub(crate) struct ClaudeEndpoint {
    pub(crate) base_url: Option<String>,
    pub(crate) api_key: Option<String>,
}

pub(crate) fn read_claude_endpoint_config() -> Result<ClaudeEndpoint> {
    let path = claude_settings_path()?;
    if !path.exists() {
        return Ok(ClaudeEndpoint {
            base_url: None,
            api_key: None,
        });
    }
    let settings: serde_json::Value = read_json_file(&path)?;
    // An api-key profile now surfaces its key via the top-level `apiKeyHelper`
    // (the env-var path is gone except as a stale residual from an un-migrated
    // settings.json). Derive the value from the helper's profile name, which
    // pins the key in `config.toml` (the source of truth). A helper string
    // whose last token fails `validate_profile_name`'s charset yields None —
    // the function never panics on a hand-edited or corrupted helper.
    let api_key = settings["env"]["ANTHROPIC_AUTH_TOKEN"]
        .as_str()
        .map(str::to_owned)
        .or_else(|| {
            settings
                .get("apiKeyHelper")
                .and_then(|v| v.as_str())
                .and_then(profile_name_from_helper)
                .and_then(|name| crate::profile::load_profile(&name).ok())
                .and_then(|p| p.api_key)
        });
    Ok(ClaudeEndpoint {
        base_url: settings["env"]["ANTHROPIC_BASE_URL"]
            .as_str()
            .map(str::to_owned),
        api_key,
    })
}

/// Extract the profile name from a `apiKeyHelper` command string of the form
/// `<exe> __api-key <profile>` (each token shell-quoted). The exe may itself
/// be shell-quoted with internal spaces (`'/home/uwu clxdy/bin/clauth'`), so
/// `split_whitespace` can yield more than three tokens — the parser locates
/// the literal `__api-key` subcommand token and takes the NEXT token as the
/// profile name, requiring it to be the LAST token (no trailing flags) and
/// to pass `validate_profile_name`'s charset (`[A-Za-z0-9_.@+-]+`, no leading
/// dot). A foreign helper that happens to contain `__api-key` followed by a
/// profile-shaped token still parses — acceptable because clauth only writes
/// this string itself, and the subcommand name is unusual enough not to
/// collide in practice. A hand-edited or corrupted helper that fails any of
/// the above yields `None` rather than risk a phantom profile lookup that
/// returns the wrong account's key into [`capture_snapshot`].
fn profile_name_from_helper(helper: &str) -> Option<String> {
    let mut tokens = helper.split_whitespace();
    while let Some(tok) = tokens.next() {
        if tok != API_KEY_HELPER_SUBCMD {
            continue;
        }
        // The token immediately after `__api-key` is the profile name; a
        // following token means the shape is `<exe> __api-key <profile>
        // <extra>` (a future flag, a typo), which is not ours.
        let name = tokens.next()?;
        if tokens.next().is_some() {
            return None;
        }
        let valid = !name.is_empty()
            && !name.starts_with('.')
            && name.bytes().all(|b| {
                b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'@' | b'+' | b'-')
            });
        return valid.then(|| name.to_string());
    }
    None
}

/// The Setup-tab field that owns a clauth-managed env key, phrased for the
/// collision prompt (`'X' is already set by …`). These are the keys clauth
/// derives from a profile's endpoint + model-tier fields; a custom env entry
/// equal to one of them would override the field's value in `settings.json`.
/// `None` when the key is not clauth-managed.
pub(crate) fn managed_env_key_label(key: &str) -> Option<&'static str> {
    Some(match key {
        "ANTHROPIC_BASE_URL" => "the base url field",
        "ANTHROPIC_AUTH_TOKEN" => "the api key field",
        "ANTHROPIC_DEFAULT_OPUS_MODEL" => "the opus model field",
        "ANTHROPIC_DEFAULT_SONNET_MODEL" => "the sonnet model field",
        "ANTHROPIC_DEFAULT_HAIKU_MODEL" => "the haiku model field",
        "CLAUDE_CODE_SUBAGENT_MODEL" => "the subagent model field",
        _ => return None,
    })
}

/// Keys present in the live `~/.claude/settings.json` `env` object. Empty when
/// the file is absent or carries no `env` block. Used to detect a custom env key
/// that already exists in the inherited base settings.
pub(crate) fn claude_settings_env_keys() -> Result<Vec<String>> {
    let path = claude_settings_path()?;
    if !path.exists() {
        return Ok(Vec::new());
    }
    let settings: serde_json::Value = read_json_file(&path)?;
    Ok(settings["env"]
        .as_object()
        .map(|env| env.keys().cloned().collect())
        .unwrap_or_default())
}

/// Patch `settings.json` `env` with profile's endpoint keys and env map;
/// strip `prev_env_keys` the new profile doesn't carry to clear stale entries.
pub(crate) fn apply_profile_to_claude_settings(
    profile: &Profile,
    prev_env_keys: &[String],
) -> Result<()> {
    with_state_lock(|| apply_profile_to_claude_settings_inner(profile, prev_env_keys))
}

fn apply_profile_to_claude_settings_inner(
    profile: &Profile,
    prev_env_keys: &[String],
) -> Result<()> {
    let path = claude_settings_path()?;

    let has_anything = profile.base_url.is_some()
        || profile.api_key.is_some()
        || !profile.env.is_empty()
        || !profile.models.is_empty()
        || !prev_env_keys.is_empty();
    if !has_anything && !path.exists() {
        return Ok(());
    }

    let content = build_claude_settings_json(Some(&path), profile, prev_env_keys)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // TECH-9 #16: settings.json can embed a third-party api_key (as
    // ANTHROPIC_AUTH_TOKEN) — 0o600, never the umask-default 0o644 this wrote before
    // in a world-traversable ~/.claude.
    atomic_write_600(&path, content).context("failed to write settings.json")
}

/// The hidden `clauth __api-key <profile>` subcommand name embedded in CC's
/// `apiKeyHelper`. The helper string is rebuilt from `env::current_exe()` on
/// every `build_claude_settings_json` run; a long-lived process (daemon/TUI)
/// that rebuilds after an in-place self-update sees Linux's `<path> (deleted)`
/// form, which `build_api_key_helper_command` strips back to the installed path.
const API_KEY_HELPER_SUBCMD: &str = "__api-key";

/// Build the `apiKeyHelper` command string CC runs per request to mint an auth
/// value for an api-key profile. The hidden subcommand reads
/// `Profile::api_key` from `config.toml` (0o600) and prints it to stdout.
///
/// CC runs the value through the system shell (`/bin/sh` on macOS/Linux,
/// `cmd` on Windows — per the Claude Code settings docs), so each token is
/// shell-escaped by [`shell_quote`]. The profile name is constrained by
/// `actions::validate_profile_name` to `[A-Za-z0-9_.@+-]+` with no leading
/// dot — entirely within the safe-char set, so it round-trips unquoted; the
/// helper-quoting exists for the exe path, which may contain spaces
/// (`/Applications/...`, `C:\Program Files\...`).
fn build_api_key_helper_command(exe: &Path, profile_name: &str) -> String {
    let exe_cow = exe.to_string_lossy();
    // A long-lived process (daemon/TUI) that rebuilds settings after the
    // in-place self-updater swapped the binary sees Linux `current_exe()`
    // return `<path> (deleted)`; the replacement lives at the same `<path>`,
    // so drop the marker to keep the helper pointing at the installed binary.
    let exe_str = exe_cow.strip_suffix(" (deleted)").unwrap_or(&exe_cow);
    format!(
        "{} {} {}",
        shell_quote(exe_str),
        shell_quote(API_KEY_HELPER_SUBCMD),
        shell_quote(profile_name),
    )
}

/// Quote `s` for the shell CC runs `apiKeyHelper` under. A safe-char run
/// (`[A-Za-z0-9_./:@=,+%-]`, matching everything `validate_profile_name`
/// allows plus a typical Unix exe path) is left unquoted; everything else is
/// wrapped — POSIX single-quoting on Unix (with `'\''` for an embedded
/// `'`), best-effort double-quoting on Windows for `cmd /c`. `cmd`'s quoting
/// is genuinely ambiguous (it's whitespace-split with `"` as a toggle, not a
/// real escape grammar); the safe-char fast path sidesteps it for the common
/// case, and the double-quote branch covers a spaces-in-path exe well enough
/// for `cmd /c "EXE SUB ARG"` to split three tokens.
fn shell_quote(s: &str) -> String {
    if s.is_empty() {
        return "''".to_string();
    }
    let safe = s.bytes().all(|b| {
        b.is_ascii_alphanumeric()
            || matches!(
                b,
                b'_' | b'.' | b'/' | b':' | b'@' | b'=' | b',' | b'+' | b'-' | b'%'
            )
    });
    if safe {
        return s.to_string();
    }
    #[cfg(unix)]
    {
        // POSIX single-quote; `'\''` closes, escapes, and reopens the quote.
        let mut out = String::with_capacity(s.len() + 2);
        out.push('\'');
        for c in s.chars() {
            if c == '\'' {
                out.push_str("'\\''");
            } else {
                out.push(c);
            }
        }
        out.push('\'');
        out
    }
    #[cfg(windows)]
    {
        // Best-effort cmd quoting — wrap in `"..."`, escaping embedded `"` and
        // `\`. Good enough for `cmd /c "<exe-with-spaces> <sub> <profile>"`.
        let mut out = String::with_capacity(s.len() + 2);
        out.push('"');
        for c in s.chars() {
            match c {
                '"' => out.push_str("\\\""),
                '\\' => out.push_str("\\\\"),
                _ => out.push(c),
            }
        }
        out.push('"');
        out
    }
    #[cfg(not(any(unix, windows)))]
    {
        s.to_string()
    }
}

/// Build the merged settings.json content. `prev_env_keys` are stripped before
/// the new profile's env is applied; pass `&[]` on start to leave existing keys.
/// Also writes the profile's model config — the top-level `model` setting and
/// the `ANTHROPIC_DEFAULT_*_MODEL` / `CLAUDE_CODE_SUBAGENT_MODEL` env keys —
/// each set when present and removed when unset, so a switch never inherits the
/// previous profile's model routing.
///
/// `base` is the settings file to merge onto; `None` (or a missing path) starts
/// from an empty object — used for an isolated runtime that must carry no
/// operator settings.
pub(crate) fn build_claude_settings_json(
    base: Option<&Path>,
    profile: &Profile,
    prev_env_keys: &[String],
) -> Result<String> {
    let mut settings: serde_json::Value = match base {
        Some(p) if p.exists() => read_json_file(p)?,
        _ => serde_json::json!({}),
    };

    if settings.get("env").is_none() {
        settings["env"] = serde_json::json!({});
    }

    let env = settings["env"]
        .as_object_mut()
        .context("settings.json `env` is not an object")?;

    for key in prev_env_keys {
        if !profile.env.contains_key(key) {
            env.remove(key);
        }
    }

    match profile.base_url.as_deref() {
        Some(url) => {
            env.insert("ANTHROPIC_BASE_URL".into(), url.into());
        }
        None => {
            env.remove("ANTHROPIC_BASE_URL");
        }
    }
    // Always clear `env.ANTHROPIC_AUTH_TOKEN`. An api-key profile now mints
    // the key per request via the top-level `apiKeyHelper` (written below, after
    // the env borrow ends), and a non-api-key profile must not inherit the
    // previous profile's token.
    env.remove("ANTHROPIC_AUTH_TOKEN");

    // Model-tier and subagent overrides — clauth-owned env keys, always set or
    // cleared deterministically so a switch never inherits the prior profile's.
    let model_env = [
        ("ANTHROPIC_DEFAULT_OPUS_MODEL", &profile.models.opus),
        ("ANTHROPIC_DEFAULT_SONNET_MODEL", &profile.models.sonnet),
        ("ANTHROPIC_DEFAULT_HAIKU_MODEL", &profile.models.haiku),
        ("CLAUDE_CODE_SUBAGENT_MODEL", &profile.models.subagent),
    ];
    for (key, value) in model_env {
        match value {
            Some(v) => {
                env.insert(key.into(), v.clone().into());
            }
            None => {
                env.remove(key);
            }
        }
    }

    // Profile env last: explicit ANTHROPIC_* entries win over base_url/api_key.
    for (k, v) in &profile.env {
        env.insert(k.clone(), v.clone().into());
    }

    // Top-level `model` setting (not env). The `env` borrow above has ended, so
    // `settings` is free to mutate again.
    let obj = settings
        .as_object_mut()
        .context("settings.json is not an object")?;
    match profile.models.default.as_deref() {
        Some(model) => {
            obj.insert("model".into(), model.into());
        }
        None => {
            obj.remove("model");
        }
    }

    // An api-key profile mints its key via CC's top-level `apiKeyHelper`
    // instead of `env.ANTHROPIC_AUTH_TOKEN` (cleared above). The key then
    // leaves the settings.json `env` block AND the spawned CC process's own
    // env: CC runs the helper per request through the system shell and sends
    // its stdout as both `X-Api-Key` and `Authorization: Bearer` (see
    // `docs/internals.md`). The helper reads the key from `config.toml`
    // (0o600, the source of truth) via a hidden subcommand, so the raw key
    // never reaches the runtime settings.json. A profile with no api_key (a
    // whitespace-only or control-char-poisoned key is one `api_key_for_profile`
    // and `validate_api_key` also refuse to mint) removes any stale helper so a
    // switch can't inherit it, and never wires a helper that would only fail at
    // mint — symmetric with the fail-closed behavior at the other end.
    let has_api_key = profile
        .api_key
        .as_deref()
        .map(str::trim)
        .filter(|k| !k.is_empty())
        .is_some_and(|k| validate_api_key(k).is_ok());
    if has_api_key {
        let exe = env::current_exe().context("resolving current_exe for apiKeyHelper")?;
        obj.insert(
            "apiKeyHelper".into(),
            build_api_key_helper_command(&exe, &profile.name).into(),
        );
    } else {
        obj.remove("apiKeyHelper");
    }

    serde_json::to_string_pretty(&settings).context("failed to serialize settings.json")
}

/// Save live `.credentials.json` into the active profile. No-op on divergence
/// (would silently overwrite stored identity); divergence is resolved via
/// `force_snapshot_active_credentials` after user confirmation. First-login
/// on a credential-less profile is adopted instead.
///
/// CAP-1: the decision and the write share ONE read of the live file. The old
/// shape classified the link, then re-read the file to capture — a foreign
/// login landing between the two (a running `claude`'s own refresh writes back
/// whatever chain it holds, Keychain first, mirror file next) was captured
/// into a profile it does not belong to (2026-07-12: 'ax-backup' held
/// 'ax-main''s chain this way and the pair double-polled one account into a
/// rate-limit pin). Deciding on exactly the bytes that get written closes the
/// window: an equal-token live refreshes the store with those same bytes, a
/// differing one is the divergence case (never captured unattended —
/// same-account rotations are the identity-guarded adopt leg's job), and only
/// a completed first login adopts the bytes it examined.
pub(crate) fn snapshot_active_credentials(config: &mut AppConfig) -> Result<()> {
    with_state_lock(|| {
        let Some(active) = config.state.active_profile.clone() else {
            return Ok(());
        };
        let live = match read_claude_credentials() {
            Ok(live) => live,
            // Unreadable/partial live file — never capture what we can't parse.
            Err(_) => return Ok(()),
        };
        let Some(live) = live else {
            // No live login at all: leave the store untouched. A missing live
            // file (logout, TOCTOU delete, dangling symlink) is not a login;
            // clearing the store here would destroy a chain the account still
            // owns (the same invariant the force path holds).
            return Ok(());
        };
        let stored_path = install_source_path(&active)?;
        if !stored_path.exists() {
            // First login on a credential-less profile: adopt only a COMPLETED
            // login (a partial write adopts nothing), using these bytes. A
            // logged-out shell (oauth block present, blank tokens) is not a
            // login either — adopting it would delete the live file (with its
            // foreign keys, e.g. mcpOAuth) to relink an install source a blank
            // profile doesn't have.
            if live.claude_ai_oauth.is_some() && !live_login_is_empty(&live) {
                return adopt_first_login_bytes(config, &active, live);
            }
            return Ok(());
        }
        let same_token = read_json_file::<ClaudeCredentials>(&stored_path)
            .ok()
            .is_some_and(|stored| {
                stored.access_token().is_some_and(|t| !t.is_empty())
                    && stored.access_token() == live.access_token()
            });
        if same_token {
            // CLA-SPLIT: a live slot holding the profile's static session
            // token carries nothing to snapshot — and writing it into
            // `profile.credentials` would clobber the clauth-private usage
            // OAuth pair. Leave both stores untouched.
            if has_session_token(&active) {
                return Ok(());
            }
            // Same access token ⇒ same rotation state: refresh the store with
            // the bytes just read (CC rewrites the mirror with identical
            // tokens but sometimes fresher metadata).
            return save_live_credentials(config, &active, Some(live));
        }
        // Diverged (or an unparseable store): keep the stored identity.
        Ok(())
    })
}

/// Store the live `.credentials.json` into the profile then replace it with a
/// symlink. Must only be called after `is_first_login` returns true; the gate
/// is re-verified here on the same read that gets written (CAP-1), so a live
/// file that changed since the caller's check adopts nothing wrong.
pub(crate) fn adopt_first_login(config: &mut AppConfig, active: &str) -> Result<()> {
    with_state_lock(|| {
        let Ok(Some(live)) = read_claude_credentials() else {
            return Ok(());
        };
        if live.claude_ai_oauth.is_none() || install_source_path(active)?.exists() {
            return Ok(()); // partial write, or no longer a first login
        }
        adopt_first_login_bytes(config, active, live)
    })
}

/// Adopt an already-read-and-verified first login: save exactly those bytes,
/// relink, and anchor the profile's identity to the login just captured.
fn adopt_first_login_bytes(
    config: &mut AppConfig,
    active: &str,
    live: ClaudeCredentials,
) -> Result<()> {
    save_live_credentials(config, active, Some(live))?;
    force_link_profile_credentials(active)?;
    // CAP-1: a capture is an identity event — the anchor moves with the store.
    crate::profile_cache::refresh_account_anchor(active);
    Ok(())
}

/// Write pre-read live credentials into `active`'s store (`None` clears it).
/// Callers decide WHAT to write; this never re-reads the live file, so the
/// bytes written are the bytes the caller's checks saw.
fn save_live_credentials(
    config: &mut AppConfig,
    active: &str,
    credentials: Option<ClaudeCredentials>,
) -> Result<()> {
    if let Some(profile) = config.find_mut(active) {
        profile.credentials = credentials;
        save_profile(profile)?;
    }
    Ok(())
}

/// Snapshot the live `.credentials.json` into the active profile unconditionally.
/// The caller has confirmed the capture (the CLI `[Y/n]`, the TUI divergence
/// Overwrite action, or an MCP Overwrite default) — so this may change which
/// ACCOUNT the profile holds. CAP-1: the identity anchor moves with the store
/// (or is dropped when no local identity hint exists), so the identity-guarded
/// adopt/follow paths never consult an anchor describing the pre-capture
/// account.
pub(crate) fn force_snapshot_active_credentials(config: &mut AppConfig) -> Result<()> {
    with_state_lock(|| {
        let Some(active) = config.state.active_profile.clone() else {
            return Ok(());
        };
        // CLA-SPLIT: a profile running on its static long-lived token carries
        // nothing to snapshot — the live slot holds the token (or a
        // session-side re-login), and capturing either into
        // `profile.credentials` would clobber the clauth-private usage OAuth
        // pair. The non-force snapshot already guards this; the confirmed
        // Overwrite must not be the one path that can destroy the pair.
        // `clauth login` is the supported way to refresh the usage pair.
        if has_session_token(&active) {
            return Ok(());
        }
        let live = read_claude_credentials()?;
        // Only a real live login is captured. A logged-out shell (blank tokens)
        // OR an absent live file (a TOCTOU delete in the modal-confirm window,
        // or a dangling symlink) is not a login; persisting either would
        // overwrite the stored chain with blanks or nothing — even under a
        // confirmed Overwrite, which the user meant as "capture the login",
        // not "destroy the store".
        let Some(live) = live else {
            return Ok(());
        };
        if live_login_is_empty(&live) {
            return Ok(());
        }
        save_live_credentials(config, &active, Some(live))?;
        crate::profile_cache::refresh_account_anchor(&active);
        Ok(())
    })
}

/// Re-link `.credentials.json` to `name`'s stored credentials, overwriting the live path.
pub(crate) fn force_link_profile_credentials(name: &str) -> Result<()> {
    with_state_lock(|| {
        let link = claude_credentials_path()?;
        let target = install_source_path(name)?;
        // Keychain first, mutations after — same ordering rationale as
        // `link_profile_credentials` (a failed write must strand nothing).
        #[cfg(target_os = "macos")]
        if target.exists() && crate::keychain::enabled() {
            keychain_write_source(&target)?;
        }
        if link.symlink_metadata().is_ok() {
            std::fs::remove_file(&link).context("failed to remove .credentials.json")?;
        }
        if target.exists() {
            if let Some(parent) = link.parent() {
                std::fs::create_dir_all(parent)?;
            }
            create_symlink(&target, &link)?;
        }
        Ok(())
    })
}

/// RESCUE-2: preserve a diverged live login before a user-ordered switch
/// discards it. Copies the regular-file live credentials into
/// `~/.clauth/quarantine/<epoch-ms>-<seq>-<active>.credentials.json` (0600)
/// so the forced switch is loss-free — a login clauth doesn't own stays
/// recoverable by hand instead of being destroyed. The newest 20 archives are
/// kept; older ones are pruned. Refuses a symlinked live path: a symlink is a
/// profile's own store, so there is nothing unsaved to archive (the
/// divergence resolved between the caller's check and this call).
pub(crate) fn archive_live_credentials(active: &str) -> Result<PathBuf> {
    with_state_lock(|| {
        let path = claude_credentials_path()?;
        let meta = path
            .symlink_metadata()
            .context("live .credentials.json vanished before archive")?;
        anyhow::ensure!(
            !meta.file_type().is_symlink(),
            "live .credentials.json is a profile's symlink — nothing unsaved to archive"
        );
        let bytes = std::fs::read(&path).context("failed to read live .credentials.json")?;
        let dir = crate::profile::clauth_dir()?.join("quarantine");
        std::fs::create_dir_all(&dir).context("failed to create quarantine dir")?;
        // The per-process sequence breaks same-millisecond collisions (two
        // archives in one ms would otherwise silently overwrite each other —
        // the exact loss the archive exists to prevent). Zero-padded so the
        // filename stays chronologically sortable past the epoch-ms prefix.
        static ARCHIVE_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let seq = ARCHIVE_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dest = dir.join(format!(
            "{}-{seq:04}-{active}.credentials.json",
            crate::usage::now_ms()
        ));
        atomic_write_600(&dest, bytes).context("failed to write quarantine copy")?;
        // Retention: an archive, not a landfill — keep the newest
        // QUARANTINE_KEEP copies (names sort chronologically), best-effort
        // prune the rest. Pruning failure never fails the switch.
        const QUARANTINE_KEEP: usize = 20;
        if let Ok(entries) = std::fs::read_dir(&dir) {
            let mut archived: Vec<PathBuf> = entries
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| {
                    p.file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(|n| n.ends_with(".credentials.json"))
                })
                .collect();
            archived.sort();
            if archived.len() > QUARANTINE_KEEP {
                for old in &archived[..archived.len() - QUARANTINE_KEEP] {
                    let _ = std::fs::remove_file(old);
                }
            }
        }
        Ok(dest)
    })
}

/// True when both sides have an OAuth block and access or refresh token differs.
/// Missing data on either side returns false (snapshot/skip is safer than guessing).
pub(crate) fn credentials_diverged(
    stored: Option<&ClaudeCredentials>,
    live: Option<&ClaudeCredentials>,
) -> bool {
    let Some(stored) = stored.and_then(|c| c.claude_ai_oauth.as_ref()) else {
        return false;
    };
    let Some(live) = live.and_then(|c| c.claude_ai_oauth.as_ref()) else {
        return false;
    };
    stored.access_token != live.access_token || stored.refresh_token != live.refresh_token
}

/// Replace the symlink at `.credentials.json` with a regular file (same bytes).
/// No-op if already a regular file or absent. Prevents CC writes from bleeding
/// into the profile's storage after the user disowns the active profile.
pub(crate) fn detach_credentials_link() -> Result<()> {
    with_state_lock(|| {
        let path = claude_credentials_path()?;
        let Ok(meta) = path.symlink_metadata() else {
            return Ok(());
        };
        if !meta.file_type().is_symlink() {
            return Ok(());
        }
        let content =
            std::fs::read(&path).context("failed to read .credentials.json before detach")?;
        std::fs::remove_file(&path).context("failed to remove .credentials.json symlink")?;
        atomic_write_600(&path, content).context("failed to write detached .credentials.json")?;
        Ok(())
    })
}

#[cfg(test)]
#[path = "../tests/inline/claude.rs"]
mod tests;
