use std::env;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::lock::{StateLockHeld, with_state_lock};
use crate::logline::logline;
use crate::profile::{
    AppConfig, ClaudeCredentials, Profile, ProfileName, atomic_write, atomic_write_600, claude_dir,
    profile_dir, read_json_file, save_profile,
};

pub(crate) fn claude_credentials_path() -> Result<PathBuf> {
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
pub(crate) fn has_session_token(name: &ProfileName) -> bool {
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
pub(crate) fn session_token_status(name: &ProfileName) -> Option<SessionTokenStatus> {
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

/// The access token a switch would INSTALL for `name` from its sidecar, or
/// `None` when the split is disengaged (no sidecar, unparseable, or a
/// [`SessionTokenStatus::NotLongLived`] mis-fill that switches ignore). Gated on
/// the same predicate as [`has_session_token`], so a profile can never be
/// attributed by a token no switch would ever install — and that predicate is
/// content-classified, so a ROLLING stamp (refresh-less by construction, like
/// the mint) attributes here too: `clauth which` names a session running on a
/// rolling bearer the same way it names one on a static mint.
pub(crate) fn installed_session_token(name: &ProfileName) -> Option<String> {
    if !has_session_token(name) {
        return None;
    }
    let path = profile_dir(name).ok()?.join("session-token.json");
    let creds = read_json_file::<ClaudeCredentials>(&path).ok()?;
    creds
        .access_token()
        .filter(|t| !t.is_empty())
        .map(str::to_string)
}

/// Remove `name`'s long-lived sidecar, flipping [`install_source_path`] back to
/// `credentials.json`. Returns whether a file was actually removed, so a caller
/// can tell "cleared" from "there was nothing to clear". An absent sidecar is
/// success, not an error: the requested end state already holds.
pub(crate) fn clear_session_token(name: &ProfileName) -> Result<bool> {
    let path = profile_dir(name)?.join("session-token.json");
    with_state_lock(|_held| match std::fs::remove_file(&path) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(anyhow::Error::new(e).context("remove session-token.json")),
    })
}

/// Remove `name`'s preserved mint backup (`session-token.static.json`), the
/// clear's second file: "cleared the long-lived token" with a year-scale mint
/// still sitting in the backup slot would be false — on a rolling profile the
/// sidecar holds an hours-scale bearer and the backup holds the actual
/// long-lived credential the operator asked to remove. Same contract as
/// [`clear_session_token`]: returns whether a file was removed, absent is
/// success. A plain delete, not a quarantine — quarantine preserves EVIDENCE of
/// an anomaly, and an operator-confirmed removal of their own credential is the
/// one path where keeping the bytes on disk would defeat the command.
pub(crate) fn clear_static_backup(name: &ProfileName) -> Result<bool> {
    let path = profile_dir(name)?.join("session-token.static.json");
    with_state_lock(|_held| match std::fs::remove_file(&path) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(anyhow::Error::new(e).context("remove session-token.static.json")),
    })
}

/// Whether `name` holds a preserved mint backup (`session-token.static.json`).
/// A single stat: the Setup tab's clear row uses it to DISCLOSE that the
/// backup goes with a clear, and to stay reachable while the backup is the
/// only long-lived piece left.
pub(crate) fn has_static_backup(name: &ProfileName) -> bool {
    profile_dir(name).is_ok_and(|d| d.join("session-token.static.json").exists())
}

/// A cheap identity of `name`'s on-disk credential state: (mtime, length) of
/// `credentials.json`, `session-token.json`, and `session-token.static.json`,
/// `None` per absent file. Every recovery the re-login-shaped scheduler
/// leashes prescribe lands as a write to one of these three (a browser
/// re-login replaces `credentials.json`, a `--setup-token` re-mint or a
/// hand-restore replaces the sidecar or the backup), and every writer in this
/// codebase goes through an atomic tempfile + rename, so a change is always a
/// fresh mtime. Metadata only — no locks, no reads: a mid-write observation
/// just changes again on the next look, which is the correct answer anyway.
pub(crate) fn credential_fingerprint(
    name: &ProfileName,
) -> [Option<(std::time::SystemTime, u64)>; 3] {
    let Ok(dir) = profile_dir(name) else {
        return [None, None, None];
    };
    [
        "credentials.json",
        "session-token.json",
        "session-token.static.json",
    ]
    .map(|f| {
        std::fs::metadata(dir.join(f))
            .ok()
            .and_then(|m| Some((m.modified().ok()?, m.len())))
    })
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

/// CLA-ROLL: what `session-token.json` holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SidecarKind {
    /// A `claude setup-token` mint: year-scale, the setup-token scope pair,
    /// worth preserving.
    Mint,
    /// A bearer stamped from the usage chain: hours-scale, the chain's full
    /// grant, and never to be snapshotted as a backup.
    Rolling,
    /// A rotating pair — a mis-fill, the one state the split exists to detect.
    /// Checked FIRST, because a mis-fill is by construction a copy of
    /// `credentials.json` and so carries the chain's full scopes: without this
    /// arm the scope test would read it `Rolling`, and every surface keying on
    /// that (`status.json` above all, where a reader has no second source)
    /// would publish routine-maintenance truth over the exact failure the
    /// DANGER rendering exists for.
    Misfilled,
}

/// CLA-ROLL: classify a sidecar's content EXACTLY, from the one signal every
/// sidecar ever written carries — its scope set. [`write_session_token`] stamps
/// [`SETUP_TOKEN_SCOPES`]; [`stamp_rolling_token`] clones the usage chain's
/// grant, and a chain fit to BE a usage chain can never carry the mint's set:
/// `/api/oauth/usage` 403s exactly those scopes (the #52 root cause), so a
/// chain that has polled usage even once proves it holds something more. This
/// is the same discrimination the split itself rests on, read back.
///
/// Rolling ⇔ a plan stamp (`subscriptionType`, which no mint write ever sets)
/// or any scope beyond the setup pair. Everything else — the pair, a subset,
/// an absent list — reads `Mint`, because THIS classifier's failure direction
/// decides what gets overwritten without a backup, and only
/// [`stamp_rolling_token`] (which refuses to write a mint-classifying bearer)
/// can put rolling content on disk. Expiry is deliberately not consulted: a
/// hand-edited `expiresAt` (which `SETUP_TOKEN_ASSUMED_LIFETIME_MS`'s doc
/// invites) must never reclassify a credential.
pub(crate) fn sidecar_kind_of(oauth: &crate::profile::OAuthToken) -> SidecarKind {
    // A refresh token is a content FACT that pre-empts the scope inference: a
    // mis-fill carries the chain's scopes and must classify as what it is,
    // never as a rolling bearer.
    if oauth.refresh_token.is_some() {
        return SidecarKind::Misfilled;
    }
    if oauth.subscription_type.is_some() {
        return SidecarKind::Rolling;
    }
    let beyond_mint = oauth.scopes.as_ref().is_some_and(|s| {
        s.iter()
            .any(|sc| !SETUP_TOKEN_SCOPES.contains(&sc.as_str()))
    });
    if beyond_mint {
        SidecarKind::Rolling
    } else {
        SidecarKind::Mint
    }
}

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
pub(crate) fn write_session_token(name: &ProfileName, token: &str, now_ms: i64) -> Result<i64> {
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
    with_state_lock(|_held| atomic_write_600(&path, &bytes).context("write session-token.json"))?;
    Ok(expires_at)
}

/// CLA-ROLL: write `name`'s `session-token.json` from the usage chain's
/// just-persisted OAuth fields — the access token as a plan-gated-model-capable bearer
/// with the chain's REAL expiry, full scopes, and `subscriptionType`, and NO
/// refresh token (the classifier stays [`SessionTokenStatus::LongLived`], so
/// every split guard keeps working unmodified; sessions get nothing to rotate,
/// the refresh chain stays clauth-private). The honest expiry is deliberate: a
/// dead roll must LOOK dead on every surface, never a far-future stamp sitting
/// over a token that died hours ago — a display that reads comfortable while
/// the credential behind it is gone is how one of these goes unnoticed.
///
/// Before the FIRST roll overwrites a genuine static mint, the mint is
/// preserved at `session-token.static.json` ([`preserve_static_mint`]) so
/// switching back to the static token — or a terminally dead chain — can restore Sonnet-cap
/// service instead of signing sessions out.
pub(crate) fn stamp_rolling_token(
    name: &ProfileName,
    chain: &crate::profile::OAuthToken,
) -> Result<()> {
    let rolled = rolling_projection(chain);
    // [`sidecar_kind_of`]'s totality is enforced HERE, at the only writer of
    // rolling content: a chain whose recorded grant would classify as a mint
    // (no plan stamp AND nothing beyond the setup scope pair) cannot produce a
    // bearer this code could later tell from a real mint — and a chain shaped
    // like that could never have polled usage anyway (#52: those scopes 403),
    // so refusing to roll it costs nothing real.
    if sidecar_kind_of(&rolled) == SidecarKind::Mint {
        anyhow::bail!(
            "'{name}' usage chain's recorded grant is indistinguishable from a setup-token \
             mint (no scope beyond the setup pair, no subscription type) · refusing to stamp \
             a rolling bearer that could later be preserved as the static mint. Re-run \
             `clauth login {name}` to record the chain's real grant"
        );
    }
    let sidecar = ClaudeCredentials {
        claude_ai_oauth: Some(rolled),
    };
    let bytes = serde_json::to_vec_pretty(&sidecar).context("serialize rolling session token")?;
    let path = profile_dir(name)?.join("session-token.json");
    with_state_lock(|_held| {
        preserve_static_mint(name)?;
        atomic_write_600(&path, &bytes).context("write rolling session-token.json")
    })
}

/// The refresh-less projection of a usage chain — the EXACT content a rolling
/// stamp writes: the chain's bearer, scopes, and plan stamp with the refresh
/// token dropped. One constructor, so the pre-stamp classification
/// (`roll_from_stored_chain`'s `GrantUnusable` verdict) and the stamp itself
/// can never drift apart on what "rolled" means.
pub(crate) fn rolling_projection(chain: &crate::profile::OAuthToken) -> crate::profile::OAuthToken {
    crate::profile::OAuthToken {
        access_token: chain.access_token.clone(),
        refresh_token: None,
        expires_at: chain.expires_at,
        scopes: chain.scopes.clone(),
        subscription_type: chain.subscription_type.clone(),
    }
}

/// Copy a genuine static mint aside to `session-token.static.json` before the
/// roll overwrites it. Idempotent across rolls: once preserved, later sidecar
/// contents are rolling values and touch nothing. A live backup stands unless
/// the sidecar holds a genuinely FRESHER mint (a later stamped expiry — the
/// shape only an explicit re-mint produces), which replaces it; a sidecar
/// that is absent or holds a rolling/mis-filled value has no mint to
/// preserve. Callers hold the state flock.
fn preserve_static_mint(name: &ProfileName) -> Result<()> {
    let dir = profile_dir(name)?;
    let sidecar = dir.join("session-token.json");
    let backup = dir.join("session-token.static.json");
    // ONE read backs both the decision and the bytes written. Reading twice
    // let a mis-fill land in between, so a rotating pair could be validated as
    // absent and then snapshotted as "the mint".
    //
    // Only NotFound reads as "no mint to preserve". Any other read failure is
    // LOUD and aborts the caller's roll: this function's verdict decides
    // whether `stamp_rolling_token` may overwrite the sidecar, and a transient
    // `EIO`/`EACCES` swallowed here would destroy a genuine mint with no
    // backup written and nothing left for `clauth static-token` to restore —
    // the one direction that is unrecoverable.
    let raw = match std::fs::read(&sidecar) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e).context("read session-token.json before overwriting it"),
    };
    // Parse failures stay quiet by design: bytes that don't hold a login have
    // no mint to preserve (`session_token_status` disengages the split on the
    // same read), and preserving them as "the mint" would make the degrade
    // ladder restore garbage.
    let Ok(creds) = serde_json::from_slice::<ClaudeCredentials>(&raw) else {
        return Ok(());
    };
    let Some(oauth) = creds.claude_ai_oauth.as_ref() else {
        return Ok(());
    };
    // Exactly one kind is worth keeping. A mint is preserved whatever its
    // remaining life (under the old horizon heuristic a mint in its final
    // month was destroyed with nothing kept, precisely when a backup was about
    // to matter most); a rolling bearer is never snapshotted whatever its
    // stamped expiry; and a mis-fill classifies as itself — the classifier's
    // refresh-token arm, not a sibling check here — so a rotating pair can
    // never become "the mint" either.
    if sidecar_kind_of(oauth) != SidecarKind::Mint {
        return Ok(());
    }
    // Whether the slot's current holder stands. A LIVE backup whose stamped
    // expiry is AT LEAST the sidecar mint's stands (idempotence: rolls after
    // the first change nothing); anything else is REPLACED by the mint about
    // to be superseded. Two failure shapes taught this rule its two halves:
    // a DEAD file left in the slot permanently blocked every future mint from
    // being preserved (re-mint, re-arm, and the fresh mint was destroyed on
    // the next roll with only the dead backup left to restore) — and a live
    // but OLDER backup did the same thing one notch subtler: with the flag
    // off, `clauth login --setup-token` writes the sidecar alone, so the
    // fresh year-scale mint sat only there, and "an existing backup is never
    // replaced" let the next roll destroy it while preserving the stale one.
    // Liveness comes from [`classify_backup_bytes`], the SAME rule every
    // restore path reads the file with; the freshness comparison treats a
    // missing expiry stamp as unbounded. Read errors other than NotFound are
    // loud, same rule as the sidecar read above.
    let sidecar_exp = oauth.expires_at;
    match std::fs::read(&backup) {
        Ok(bytes) => {
            let now = crate::usage::now_ms() as i64;
            let stands = matches!(classify_backup_bytes(&bytes, now), BackupVerdict::LiveMint)
                && serde_json::from_slice::<ClaudeCredentials>(&bytes)
                    .ok()
                    .and_then(|c| c.claude_ai_oauth)
                    .is_some_and(|held| match (held.expires_at, sidecar_exp) {
                        (None, _) => true,
                        (Some(_), None) => false,
                        (Some(held_exp), Some(new_exp)) => held_exp >= new_exp,
                    });
            if stands {
                return Ok(());
            }
            // A displaced holder that never was a mint is EVIDENCE (whatever
            // wrote it, the disposal rule matches `live_backup_bytes`); a
            // displaced dead or stale mint is just a superseded credential
            // and is overwritten in place.
            if matches!(classify_backup_bytes(&bytes, now), BackupVerdict::NoMint) {
                quarantine_file_locked(name, &backup, "session-token.static.json")?;
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).context("read session-token.static.json before replacing it"),
    }
    atomic_write_600(&backup, raw).context("write session-token.static.json")
}

/// CLA-ROLL: capture a fresh mint into the sidecar AND stamp it as the static
/// backup, in ONE state-flock section from the SAME serialized bytes — the
/// re-mint path on a rolling-token profile. A two-step (write sidecar, then
/// read it back into the backup) leaves a window where a concurrent rotation
/// roll replaces the mint with an hours-horizon token that then gets
/// snapshotted as "the mint", which a later restore would then install as a
/// dead credential. Returns the stamped expiry like [`write_session_token`].
pub(crate) fn write_session_token_with_backup(
    name: &ProfileName,
    token: &str,
    now_ms: i64,
) -> Result<i64> {
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
    with_state_lock(|_held| {
        atomic_write_600(&dir.join("session-token.json"), &bytes)
            .context("write session-token.json")?;
        atomic_write_600(&dir.join("session-token.static.json"), &bytes)
            .context("write session-token.static.json")
    })?;
    Ok(expires_at)
}

/// What [`heal_misfilled_sidecar`] found — distinguished because the caller
/// acts differently on each, and one `false` folding them made the install
/// gate send a sidecar a concurrent repair had already fixed down the vanilla
/// path, and its log claim "no static backup exists" cover a backup that was
/// sitting right there, expired.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HealOutcome {
    /// Evidence quarantined, preserved mint restored over the pair.
    Healed,
    /// Re-checked under the lock and the sidecar is not mis-filled — a
    /// concurrent repair (or whatever writes the sidecar) beat us. Nothing to
    /// heal; the sidecar's current content decides what happens next.
    NotMisfilled,
    /// The sidecar IS mis-filled and no live mint backup exists to restore:
    /// absent, expired, or not a mint at all ([`live_backup_bytes`] logged
    /// which). The split stays disengaged until an operator re-captures.
    NoLiveBackup,
}

/// CLA-ROLL: heal a mis-filled sidecar on a rolling-token profile — quarantine
/// the evidence (the profile's own `quarantine/` dir), restore the preserved
/// static mint over it.
pub(crate) fn heal_misfilled_sidecar(name: &ProfileName) -> Result<HealOutcome> {
    let dir = profile_dir(name)?;
    let sidecar = dir.join("session-token.json");
    let backup = dir.join("session-token.static.json");
    with_state_lock(|_held| {
        if !matches!(
            session_token_status(name),
            Some(SessionTokenStatus::NotLongLived)
        ) {
            return Ok(HealOutcome::NotMisfilled);
        }
        // The clock gate matters here too: healing a mis-fill by installing an
        // EXPIRED mint trades a disengaged-but-working vanilla posture for a
        // credential that signs sessions out on first use. No live backup →
        // the mis-fill stays, loudly, exactly as if there were no backup.
        let Some(bytes) = live_backup_bytes(name, &backup)? else {
            return Ok(HealOutcome::NoLiveBackup);
        };
        quarantine_file_locked(name, &sidecar, "session-token.json")?;
        atomic_write_600(&sidecar, bytes).context("restore session-token.json")?;
        std::fs::remove_file(&backup).context("remove consumed static backup")?;
        Ok(HealOutcome::Healed)
    })
}

/// CLA-ROLL: quarantine a mis-filled sidecar and REMOVE it (leaving the
/// sidecar absent) — the CLI `clauth rolling-token <p>` pre-clear, where overwriting
/// is explicit operator intent but the evidence still goes to quarantine
/// first. `Ok(true)` when a mis-fill was cleared.
pub(crate) fn quarantine_misfilled_sidecar(name: &ProfileName) -> Result<bool> {
    let dir = profile_dir(name)?;
    let sidecar = dir.join("session-token.json");
    with_state_lock(|_held| {
        if !matches!(
            session_token_status(name),
            Some(SessionTokenStatus::NotLongLived)
        ) {
            return Ok(false);
        }
        quarantine_file_locked(name, &sidecar, "session-token.json")?;
        std::fs::remove_file(&sidecar).context("remove mis-filled session-token.json")?;
        Ok(true)
    })
}

/// Copy a credential file's bytes into the profile's own `quarantine/` dir
/// before the caller overwrites or removes it — timestamp + sequence, suffixed
/// with the quarantined file's basename, so the evidence of whatever
/// mis-filled it survives the repair. Callers hold the state flock.
fn quarantine_file_locked(name: &ProfileName, path: &Path, suffix: &str) -> Result<()> {
    let bytes = std::fs::read(path).context("read credential file for quarantine")?;
    // UNDER THE PROFILE, not a global `~/.clauth/quarantine/`. What lands here
    // can be a rotating pair — that is what makes a sidecar a mis-fill — and
    // a global dir means `clauth delete <name>` leaves that profile's refresh
    // tokens on disk after removing everything else it owns. Here
    // `delete_profile`'s existing `remove_dir_all` sweeps it with the rest, and
    // the profile name stops being part of a filename that has to be parsed
    // back out.
    //
    // NOT `create_dir_all`: it would land 0755 at the usual umask.
    // `atomic_write_600` below `mkdir_700`s a missing parent for exactly this.
    let dir = profile_dir(name)?.join("quarantine");
    static QUARANTINE_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = QUARANTINE_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dest = dir.join(format!("{}-{seq:04}.{suffix}", crate::usage::now_ms()));
    atomic_write_600(&dest, bytes).context("write quarantined credential file")
}

/// CLA-ROLL: best-effort arming at session start (`clauth start` resolves its
/// credentials through [`install_source_path`], never `ensure_installable`) —
/// a rolling-token profile whose sidecar is absent or stale is stamped from the
/// DISK-loaded chain when comfortably live, so a session launched inside an
/// arming window never copies the rotating pair. Never touches a
/// NotLongLived mis-fill, never
/// spends a refresh, and never fails the caller — a stamping hiccup must not
/// block a session start (the vanilla fallback still works; the daemon heals
/// the sidecar on its next rotation).
pub(crate) fn arm_rolling_from_disk(name: &ProfileName) {
    arm_rolling_from_disk_synced(name, || {});
}

/// The injected closure runs after the pre-guard pre-filter and immediately
/// before `RotationGuard::acquire` — a sync point for the regression test that
/// holds the guard while this thread parks, so "the write uses the post-guard
/// re-read" is pinned by construction rather than by a sleep long enough to
/// probably lose a race. Production passes a no-op.
fn arm_rolling_from_disk_synced(name: &ProfileName, pre_guard_done: impl FnOnce()) {
    const ROLLING_ARM_GRACE_MS: i64 = 30 * 60 * 1000;
    let profile = match crate::profile::load_profile(name) {
        Ok(profile) => profile,
        Err(e) => {
            // Loud, not a bare return: never-fail-the-start is the contract,
            // but a session that silently runs on the rotating pair because
            // ~/.clauth could not be read is the failure this leg exists to
            // prevent, and it deserves a trace.
            crate::logline::logline!(
                "clauth: start-time rolling-token arming for '{name}' skipped (could not load the profile: {e:#}); the session runs on whatever the sidecar holds"
            );
            return;
        }
    };
    if !profile.rolling_token {
        return;
    }
    let now = crate::usage::now_ms() as i64;
    match session_token_status(name) {
        // Mis-fill: evidence stays; NotLongLived semantics apply elsewhere.
        Some(SessionTokenStatus::NotLongLived) => return,
        // A comfortably live rolling token (or a healthy mint) needs nothing.
        Some(SessionTokenStatus::LongLived(exp))
            if exp.is_none_or(|e| now + ROLLING_ARM_GRACE_MS < e) =>
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
        .is_none_or(|e| now + ROLLING_ARM_GRACE_MS >= e)
    {
        return; // chain itself stale — the daemon's guarded refresh will stamp
    }
    // Everything above is a CHEAP PRE-FILTER on a pre-guard read — it decides
    // only whether this is worth serializing for, never what gets written.
    pre_guard_done();
    // `RotationGuard::acquire` BLOCKS on the flock, so arriving at the `else`
    // is a filesystem or permissions problem under `~/.clauth`, not contention:
    // the ordinary interleaving is that we WAIT here while the daemon's
    // rotation lands, and only then proceed.
    let _guard = match crate::runtime::RotationGuard::acquire(name) {
        Ok(guard) => guard,
        Err(e) => {
            // `acquire` BLOCKS, so failing is a filesystem or permissions
            // fault under ~/.clauth, never contention — the one situation an
            // operator has to hear about, because every later arming attempt
            // fails the same way.
            crate::logline::logline!(
                "clauth: start-time rolling-token arming for '{name}' skipped (rotation lock: {e:#})"
            );
            return;
        }
    };
    // So the token above is now potentially the one that rotation just
    // superseded. Re-read under the guard and stamp from THAT — the same rule
    // `oauth.rs` states for the refresher: a pre-guard snapshot can go stale
    // the moment a sibling rotation runs, and the value it would install here
    // is a bearer with less life than the sidecar it replaces (or, if a refresh
    // invalidates its predecessor, a dead one with no refresh path behind it).
    let fresh = match crate::profile::load_profile(name) {
        Ok(fresh) => fresh,
        Err(e) => {
            crate::logline::logline!(
                "clauth: start-time rolling-token arming for '{name}' skipped (post-guard profile re-read failed: {e:#})"
            );
            return;
        }
    };
    // The FLAG is part of that re-read: a `static-token --clear` can hold this
    // same guard, disarm the profile, take the sidecar and the preserved mint,
    // and release — all while this thread parks. Stamping from the pre-guard
    // routing would land a fresh rolling bearer on the profile the operator
    // just cleared, with the flag now off so nothing ever re-stamps it: a
    // dies-in-hours credential with no exit.
    if !fresh.rolling_token {
        return;
    }
    let Some(oauth) = fresh
        .credentials
        .as_ref()
        .and_then(|c| c.claude_ai_oauth.as_ref())
    else {
        return;
    };
    // The clock is re-taken with the state: `acquire` can block for a full
    // rotation round trip, and judging post-guard freshness against the
    // pre-guard `now` biases toward "fresh" by exactly that wait.
    let now = crate::usage::now_ms() as i64;
    // Re-check the freshness gate too: the rotation we waited for may have
    // already re-stamped the sidecar through the rotation hook, in which case
    // there is nothing left to do and no reason to write.
    if matches!(
        session_token_status(name),
        Some(SessionTokenStatus::LongLived(exp)) if exp.is_none_or(|e| now + ROLLING_ARM_GRACE_MS < e)
    ) {
        return;
    }
    // And the chain-staleness gate: the pre-guard pass proved a comfortable
    // chain existed THEN. The chain re-read under the guard is the one about
    // to be stamped, and stamping a bearer already inside the grace window
    // installs a token with less life than a session can rely on — the
    // daemon's guarded refresh owns that case.
    if oauth
        .expires_at
        .is_none_or(|e| now + ROLLING_ARM_GRACE_MS >= e)
    {
        return;
    }
    if let Err(e) = stamp_rolling_token(name, oauth) {
        crate::logline::logline!(
            "clauth: start-time rolling-token arming for '{name}' failed: {e:#}"
        );
    }
}

/// How much life a backup must keep to be worth restoring. Claude Code starts
/// refreshing a credential once it is inside FIVE minutes of expiry, and a
/// refresh-less mint cannot answer that — a backup restored with less life
/// than CC's own threshold lands in a client already trying to refresh it,
/// consuming the backup only to sign the session out moments later.
pub(crate) const BACKUP_EXPIRY_GRACE_MS: i64 = 5 * 60 * 1000;

/// What preserved-backup bytes hold — THE one rule every consumer of
/// `session-token.static.json` shares. [`preserve_static_mint`]'s
/// keep-or-replace guard and [`live_backup_bytes`]'s restore verdict read the
/// same file, and two hand-rolled checks let a shape one side treated as live
/// and the other as dead fall between them: a parseable file with no
/// `claudeAiOauth` block restored as "the mint", after which
/// `has_session_token` went false and sessions got the rotating pair.
enum BackupVerdict {
    /// A genuine mint with life left. No stamped expiry reads as alive — the
    /// writers always stamp one, but a hand-placed mint without a clock is
    /// still a mint.
    LiveMint,
    /// A genuine mint whose stamped `expiresAt` is inside
    /// [`BACKUP_EXPIRY_GRACE_MS`] of now.
    Expired,
    /// Not a mint: unparseable bytes, no `claudeAiOauth` block, or content
    /// that classifies as anything but a mint ([`sidecar_kind_of`]) — a
    /// rotating pair or a rolling bearer must never be restorable as "the
    /// mint", whatever wrote it into the slot.
    NoMint,
}

fn classify_backup_bytes(bytes: &[u8], now: i64) -> BackupVerdict {
    let Ok(creds) = serde_json::from_slice::<ClaudeCredentials>(bytes) else {
        return BackupVerdict::NoMint;
    };
    let Some(oauth) = creds.claude_ai_oauth else {
        return BackupVerdict::NoMint;
    };
    if sidecar_kind_of(&oauth) != SidecarKind::Mint {
        return BackupVerdict::NoMint;
    }
    if oauth
        .expires_at
        .is_some_and(|exp| exp <= now + BACKUP_EXPIRY_GRACE_MS)
    {
        BackupVerdict::Expired
    } else {
        BackupVerdict::LiveMint
    }
}

/// The preserved backup's bytes — IF it still holds a live mint. `Ok(None)`
/// otherwise, with the file's fate depending on what it holds:
///
///   * absent → nothing to do;
///   * a mint aged past its stamped `expiresAt` → left on disk as evidence
///     (restoring it installs a credential that signs every session out on
///     first use — the Incident C shape — and CONSUMING the backup to do it
///     also destroys whatever life the sidecar's current bearer has left);
///   * not a mint at all → QUARANTINED to the profile's `quarantine/` dir and
///     the slot cleared. A slot-holder that can never restore is worse than an
///     empty slot: `clauth static-token` flips the flag off before it reads
///     the file, so a permanent error here left the operator's prescribed
///     re-mint running with the flag off — the no-backup write path — and the
///     file survived to fail the next attempt identically.
///
/// Read failures are loud: this file is the mint's only other copy, and
/// "could not read it" must never be reported as "it does not exist". Callers
/// hold the state flock (the quarantine move needs it).
fn live_backup_bytes(name: &ProfileName, backup: &Path) -> Result<Option<Vec<u8>>> {
    let bytes = match std::fs::read(backup) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).context("read session-token.static.json"),
    };
    match classify_backup_bytes(&bytes, crate::usage::now_ms() as i64) {
        BackupVerdict::LiveMint => Ok(Some(bytes)),
        BackupVerdict::Expired => {
            crate::logline::logline!(
                "clauth: '{name}' preserved static mint has itself expired — not restoring it; \
                 re-mint with `clauth login {name} --setup-token`"
            );
            Ok(None)
        }
        BackupVerdict::NoMint => {
            quarantine_file_locked(name, backup, "session-token.static.json")?;
            std::fs::remove_file(backup).context("remove quarantined static backup")?;
            crate::logline::logline!(
                "clauth: '{name}' preserved static backup does not hold a mint — quarantined \
                 under the profile's quarantine/ dir; re-mint with \
                 `clauth login {name} --setup-token`"
            );
            Ok(None)
        }
    }
}

/// Restore the preserved static mint over the rolling sidecar (the rolling token switched off, or
/// the usage chain died terminally). `Ok(true)` when a backup existed and was
/// restored; `Ok(false)` when there was nothing to restore (the sidecar is
/// left as-is — a last rolling token keeps serving until its real expiry).
pub(crate) fn restore_static_mint(name: &ProfileName) -> Result<bool> {
    let dir = profile_dir(name)?;
    let backup = dir.join("session-token.static.json");
    let sidecar = dir.join("session-token.json");
    with_state_lock(|_held| {
        let Some(bytes) = live_backup_bytes(name, &backup)? else {
            return Ok(false);
        };
        // A mis-filled sidecar about to be overwritten is EVIDENCE, exactly
        // as it is on the heal and CLI pre-clear paths — this was the one
        // repair that destroyed the rotating pair silently instead of moving
        // it aside first.
        if matches!(
            session_token_status(name),
            Some(SessionTokenStatus::NotLongLived)
        ) {
            quarantine_file_locked(name, &sidecar, "session-token.json")?;
        }
        atomic_write_600(&sidecar, bytes).context("restore session-token.json")?;
        std::fs::remove_file(&backup).context("remove consumed static backup")?;
        Ok(true)
    })
}

/// CLA-ROLL: what the sidecar holds right now, with the token behind it, for
/// the arming report and every rendering surface. `None` when there is no
/// readable sidecar at all.
///
/// The kind comes from [`sidecar_kind_of`] — the same exact classification
/// every other reader uses — so the CLI can tell "armed a rolling bearer" from
/// "the gate degraded and left the mint in place", two outcomes that both
/// arrive as `AuthGate::Ready` and both leave `has_session_token` true. A
/// mis-fill comes back as [`SidecarKind::Misfilled`], never laundered into
/// `Rolling` by its chain-shaped scopes — the state agrees with
/// [`session_token_status`]'s `NotLongLived` on the same bytes.
pub(crate) fn sidecar_summary(
    name: &ProfileName,
) -> Option<(SidecarKind, crate::profile::OAuthToken)> {
    let path = profile_dir(name).ok()?.join("session-token.json");
    let creds = read_json_file::<ClaudeCredentials>(&path).ok()?;
    let oauth = creds.claude_ai_oauth?;
    Some((sidecar_kind_of(&oauth), oauth))
}

/// The file a switch INSTALLS as the live login: the profile's
/// `session-token.json` when present ([`has_session_token`]), else its
/// `credentials.json` — which is exactly the pre-split behavior, so profiles
/// without the sidecar are byte-identical to before.
pub(crate) fn install_source_path(name: &ProfileName) -> Result<PathBuf> {
    let dir = profile_dir(name)?;
    // Content-aware, not a bare existence check (#53 review): a sidecar that
    // isn't genuinely long-lived must not become the install source — see
    // [`SessionTokenStatus::NotLongLived`].
    if has_session_token(name) {
        return Ok(dir.join("session-token.json"));
    }
    Ok(dir.join("credentials.json"))
}

/// Whether a switch to `name` would install an OAuth login once its long-lived
/// sidecar is gone: the `credentials.json` [`install_source_path`] falls back to.
///
/// Read off the FILE rather than `Profile::credentials`, because the file is what
/// the relink branches on. The clear paths (`clauth static-token --clear`, the
/// Setup tab's row) are refused only when clearing would strip a profile's last
/// credential — a stored piece with neither a login nor an api key behind it —
/// so an api-key profile clears fine and lands on an ABSENT install
/// source: the relink then removes the live slot and, on macOS, signs the Keychain
/// out. Their copy claimed a relink onto a stored OAuth login regardless, which is
/// why this exists rather than each surface guessing — a message derived from a
/// different fact than the action drifts from it silently.
pub(crate) fn has_stored_oauth_login(name: &ProfileName) -> bool {
    profile_dir(name).is_ok_and(|dir| dir.join("credentials.json").exists())
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

pub(crate) fn classify_credentials_link(active: &ProfileName) -> Result<LinkState> {
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
        // Not our symlink. On macOS, Claude Code rewrites ~/.claude/.credentials.json
        // as a regular-file mirror of the Keychain after every run, clobbering the
        // symlink we created. That is NOT divergence when the credential is unchanged
        // — only a genuine re-login / token rotation (different access token) is.
        // Compare content instead of trusting symlink identity so an ordinary switch
        // doesn't falsely prompt to capture credentials that already match the profile.
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
pub(crate) fn is_first_login(active: &ProfileName) -> Result<bool> {
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
/// `session-token.json` ([`clear_session_token`] flips it back) while the live slot still
/// holds the previous source's content — a stale slot the next switch
/// re-installs, not an unsaved login. Both stores are checked because the flip
/// can leave the slot holding either the OAuth login or the static mint.
/// Without this exemption every unattended switch fails "unsaved credentials;
/// resolve in the TUI" until its retry TTL, and the TUI prompts about
/// credentials that are fully saved (observed live 2026-07-21 on the macOS
/// fork as a symlink; recurs there as a regular file after any CC session).
pub(crate) fn live_login_is_stored(active: &ProfileName) -> bool {
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

/// True when the credentials hold NO usable login: no OAuth block, or one whose
/// access AND refresh tokens are both absent/blank — Claude Code's logged-out
/// shell (it blanks the tokens and zeroes `expiresAt` when its own refresh
/// dies, keeping unrelated keys like `mcpOAuth`). A shell still classifies
/// [`LinkState::Diverged`], but there is no login in it to protect.
pub(crate) fn live_login_is_empty(creds: &ClaudeCredentials) -> bool {
    creds.access_token().filter(|t| !t.is_empty()).is_none()
        && creds.refresh_token().filter(|t| !t.is_empty()).is_none()
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
pub(crate) fn live_diverged_and_unsaved(active: &ProfileName) -> Result<bool> {
    Ok(
        matches!(classify_credentials_link(active)?, LinkState::Diverged)
            && !is_first_login(active)?
            && !live_credentials_are_shell()
            && !live_login_is_stored(active),
    )
}

/// What the Keychain mirror does when the profile stores NO login at all (an
/// api-key or third-party profile, whose endpoint + token come from
/// `settings.json`). Chosen by the caller, because only the caller knows whether
/// it meant to change which account is live.
#[cfg(target_os = "macos")]
enum AbsentSource {
    /// Sign the item out. The forcing relink alone: its caller has decided this
    /// profile's credentials are what the live slot must hold, and it deletes
    /// that slot a few lines up. CC resolves the Keychain FIRST, so leaving the
    /// item there kept the operator authenticated as the account they just
    /// switched away from, with the file layer reading switched while every
    /// request still spent the old account.
    SignOut,
    /// Leave the item alone, which is what every relink did before the mirror
    /// learned to sign out. The guarded relink alone, and it is not a
    /// conservatism: `rename_profile`, the first-ever `login` capture, and the
    /// daemon's and TUI's boot reconcile all reach that path with nothing
    /// switching, so a sign-out there would destroy a bare `claude` login clauth
    /// never captured and cannot put back. `switch_profile`'s uncaptured-relogin
    /// branch routes here too, deliberately: refusing beats dropping when a live
    /// login is unsaved.
    Leave,
}

/// macOS: mirror what the file layer just installed into the Keychain, which is
/// where Claude Code actually reads its login. `path` is the install source the
/// swap resolved: its whole JSON object becomes the item, so the incoming
/// account's own blocks travel with its login and the outgoing account's do not
/// (`keychain::keychain_install` carries the MCP-server logins across).
///
/// Runs after the symlink swap and is `?`-fatal: a failure leaves the file layer
/// switched while CC still reads the old Keychain login. Loud and recoverable,
/// since every write here is idempotent, so retrying the switch re-runs it.
#[cfg(target_os = "macos")]
fn keychain_mirror_source(path: &Path, absent: AbsentSource) -> Result<()> {
    // CLA-SPLIT: callers pass the already-resolved install source so the
    // symlink target and the Keychain content come from ONE resolution: a
    // session-token.json vanishing between two stats can't split them. This
    // `exists` is a SECOND stat, though, and under `SignOut` its false branch is
    // destructive where it used to be inert. Both stats sit inside the state
    // flock, so only a writer that is not clauth can win that race.
    if !path.exists() {
        return match absent {
            AbsentSource::SignOut => crate::keychain::keychain_sign_out(),
            AbsentSource::Leave => Ok(()),
        };
    }
    let store: serde_json::Value = read_json_file(path)?;
    // Typed check at the boundary, then install the untyped object: the login
    // must be PRESENT and parse as a login, or CC is handed a credential it
    // cannot read and nothing here would have said so. Presence is its own
    // clause because the field is an `Option`, so `{}` and a store holding only
    // `mcpOAuth` parse clean. The Value is what gets written, since the typed
    // shape models the login alone and would drop the store's siblings.
    let parsed = serde_json::from_value::<ClaudeCredentials>(store.clone()).with_context(|| {
        format!(
            "install source is not Claude credentials: {}",
            path.display()
        )
    })?;
    anyhow::ensure!(
        parsed.claude_ai_oauth.is_some(),
        "refusing to install a credential store that holds no login: {}",
        path.display()
    );
    crate::keychain::keychain_install(&store)
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

/// Credential-store keys a switch carries forward onto the incoming profile.
/// `mcpOAuth` holds Claude Code's per-MCP-server logins, minted per (server,
/// endpoint) against the server itself, so they belong to no Claude account and
/// an account switch that dropped them logged the operator out of every MCP
/// server.
///
/// An ALLOWLIST rather than "every key that is not the login": Claude Code keeps
/// this store as one object it rewrites wholesale, so a key it adds later would
/// otherwise start crossing accounts with nobody having decided it should, and
/// an account-scoped one (a device token, an org id) is exactly what must not.
/// Ceiling: renaming the key upstream turns the carry into a silent no-op. The
/// upgrade path is a startup probe that reports an unrecognised non-login key in
/// the live store, which is only worth building once a second key exists.
const CARRIED_CREDENTIAL_KEYS: [&str; 1] = ["mcpOAuth"];

/// The credential-store keys that belong to ONE Claude account, so a sign-out
/// drops them and nothing carries them onto another account's login. Claude Code
/// deletes exactly these five on logout and preserves everything else
/// (read out of its own logout path), which is the
/// other side of the line [`CARRIED_CREDENTIAL_KEYS`] draws.
const ACCOUNT_SCOPED_CREDENTIAL_KEYS: [&str; 5] = [
    "claudeAiOauth",
    "organizationUuid",
    "trustedDeviceToken",
    "enterpriseGateway",
    "designOauth",
];

/// Copy [`CARRIED_CREDENTIAL_KEYS`] from `live` onto `target`, reporting whether
/// anything changed. A key `live` holds overwrites `target`'s copy; one it lacks
/// leaves `target`'s alone; the login is never touched.
///
/// The value-level core of [`carry_live_extra_into`], shared with the macOS
/// Keychain mirror, whose live slot is a Keychain item rather than a file. Both
/// callers import from ANOTHER account's blob, which is why this takes an
/// allowlist where `profile::preserve_extra_blocks` keeps everything.
pub(crate) fn carry_live_extra_over(
    target: &mut serde_json::Map<String, serde_json::Value>,
    live: &serde_json::Map<String, serde_json::Value>,
) -> bool {
    let mut changed = false;
    for key in CARRIED_CREDENTIAL_KEYS {
        let Some(value) = live.get(key) else {
            continue;
        };
        if target.get(key) != Some(value) {
            target.insert(key.to_string(), value.clone());
            changed = true;
        }
    }
    changed
}

/// What a sign-out found in a credential store, and so what its caller owes the
/// store it read from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(
    not(target_os = "macos"),
    allow(
        dead_code,
        reason = "the only caller is the macOS Keychain sign-out; the rule is pinned on every platform"
    )
)]
pub(crate) enum SignOut {
    /// Nothing that belongs to no account survives the strip: delete the store
    /// outright rather than leave an empty husk where a clean absence was.
    Delete,
    /// Account-scoped keys were dropped and something else survives: write the
    /// stripped blob back.
    Write,
    /// The store held no account-scoped key, so it is already signed out and a
    /// write would land identical bytes. Load-bearing rather than tidy: the
    /// daemon and the TUI relink the active profile on a tick, and a store that
    /// is a Keychain item costs a subprocess per write.
    Nothing,
}

/// Drop every [`ACCOUNT_SCOPED_CREDENTIAL_KEYS`] entry from `blob`, keeping what
/// belongs to no Claude account, and report what the caller owes its store.
/// A blob that is not an object carries nothing worth preserving.
#[cfg_attr(
    not(target_os = "macos"),
    allow(
        dead_code,
        reason = "the only caller is the macOS Keychain sign-out; the rule is pinned on every platform"
    )
)]
pub(crate) fn strip_account_credentials(blob: &mut serde_json::Value) -> SignOut {
    let Some(obj) = blob.as_object_mut() else {
        return SignOut::Delete;
    };
    let mut dropped = false;
    for key in ACCOUNT_SCOPED_CREDENTIAL_KEYS {
        dropped |= obj.remove(key).is_some();
    }
    match (dropped, obj.is_empty()) {
        (_, true) => SignOut::Delete,
        (true, false) => SignOut::Write,
        (false, false) => SignOut::Nothing,
    }
}

/// Copy [`CARRIED_CREDENTIAL_KEYS`] from the live credential onto `target`
/// before it becomes the live credential, so an account switch keeps MCP-server
/// auth. A key the live file holds overwrites the target's copy; one the live
/// file lacks is left as the target has it. The login is never touched.
///
/// Accepted ceiling: this can add and overwrite, never delete. A stale block in
/// a store that was not live when the operator revoked it goes live again on
/// switch-in. Per-server logouts do propagate, because Claude Code keeps the
/// `mcpOAuth` object and deletes only the entry, so the live file carries the
/// shrunken object forward. Pruning instead would wipe real logins the first
/// time a freshly-logged-in account became live, which is the worse trade. The
/// upgrade path is a clauth-owned canonical copy the per-store copies reconcile
/// against, which is only worth its moving parts once revocation is a surface.
///
/// A no-op when either file is unreadable or `target` is the static-token
/// sidecar: [`write_session_token`] rebuilds that file from the mint alone, so a
/// block carried there is dropped at the next re-mint and sits on disk for
/// nothing until then.
fn carry_live_extra_into(link: &Path, target: &Path, name: &ProfileName) -> Result<()> {
    if target
        .file_name()
        .is_some_and(|n| n == "session-token.json")
    {
        return Ok(());
    }
    let Ok(live) = read_json_file::<serde_json::Value>(link) else {
        return Ok(());
    };
    let Ok(mut stored) = read_json_file::<serde_json::Value>(target) else {
        // No store to carry into. An api-key or third-party profile keeps no
        // credentials file, so the relink below removes the live slot and
        // nothing recreates what it held. Park it against the day this profile
        // has a store again, rather than returning and dropping it.
        park_mcp_logins(name, &live);
        return Ok(());
    };
    let (Some(live_obj), Some(stored_obj)) = (live.as_object(), stored.as_object_mut()) else {
        return Ok(());
    };
    if carry_live_extra_over(stored_obj, live_obj) {
        atomic_write_600(target, serde_json::to_string_pretty(&stored)?)
            .context("failed to carry MCP server logins into profile credentials")?;
    }
    Ok(())
}

/// Run [`carry_live_extra_into`] for a switch onto `name`, reporting a failure
/// instead of raising it. Preserving MCP logins is a convenience; completing the
/// switch is not, so an unwritable profile directory must not strand the
/// operator on the outgoing account.
/// Copy [`CARRIED_CREDENTIAL_KEYS`] out of `source` into `name`'s parked store.
///
/// Nothing is written when `source` carries none of them, so an absent parked
/// file and an empty one never both mean "parked, and there was nothing" —
/// [`restore_parked_mcp_logins`] would re-attach the empty one as if it were a
/// login set. Best-effort like the carry itself: `write_profile_cache` swallows
/// its own failures, and keeping MCP logins is a convenience where completing
/// the capture or switch that triggered this is not.
fn park_mcp_logins(name: &ProfileName, source: &serde_json::Value) {
    let Some(obj) = source.as_object() else {
        return;
    };
    let parked: serde_json::Map<String, serde_json::Value> = CARRIED_CREDENTIAL_KEYS
        .iter()
        .filter_map(|key| obj.get(*key).map(|v| ((*key).to_string(), v.clone())))
        .collect();
    if parked.is_empty() {
        return;
    }
    crate::profile_cache::write_profile_cache(
        name,
        crate::profile_cache::MCP_LOGINS_FILE,
        &serde_json::Value::Object(parked),
    );
}

/// Park `name`'s MCP-server logins out of a credential store about to be
/// removed. `save_profile` deletes that file whenever a profile stops storing a
/// login (a recapture onto a third-party endpoint, a blanked OAuth login), and
/// where the live slot is clauth's symlink that file IS what the slot resolves
/// to — so the logins are already unreachable by the time any relink runs, and
/// the carry above never sees them. Reading the STORE rather than the live slot
/// is what makes this behave the same on a host that copies the slot instead.
pub(crate) fn park_mcp_logins_from_store(name: &ProfileName, store: &Path) {
    if let Ok(existing) = read_json_file::<serde_json::Value>(store) {
        park_mcp_logins(name, &existing);
    }
}

/// Merge `name`'s parked MCP-server logins back into a credential store it has
/// just regained, then drop the parked copy. The parked block wins outright
/// over the store's: a capture writes the login alone and carries no `mcpOAuth`
/// at all, so there is nothing newer for it to lose to. The parked copy is
/// dropped only once the merged write has landed, so a failure here costs a
/// retry rather than the logins.
///
/// Re-filtered through [`CARRIED_CREDENTIAL_KEYS`] on the way back in: the park
/// already filtered, so this only bounds what a hand-edited parked file can put
/// into a credential store.
pub(crate) fn restore_parked_mcp_logins(name: &ProfileName, store: &Path) {
    let Some(parked) = crate::profile_cache::load_profile_cache::<serde_json::Value>(
        name,
        crate::profile_cache::MCP_LOGINS_FILE,
    ) else {
        return;
    };
    let (Some(parked_obj), Ok(mut stored)) = (
        parked.as_object(),
        read_json_file::<serde_json::Value>(store),
    ) else {
        return;
    };
    let Some(stored_obj) = stored.as_object_mut() else {
        return;
    };
    for key in CARRIED_CREDENTIAL_KEYS {
        if let Some(value) = parked_obj.get(key) {
            stored_obj.insert(key.to_string(), value.clone());
        }
    }
    let landed = serde_json::to_string_pretty(&stored)
        .ok()
        .is_some_and(|bytes| atomic_write_600(store, bytes).is_ok());
    if landed {
        crate::profile_cache::remove_profile_cache(name, crate::profile_cache::MCP_LOGINS_FILE);
    }
}

fn carry_live_extra_best_effort(link: &Path, target: &Path, name: &ProfileName) {
    if let Err(e) = carry_live_extra_into(link, target, name) {
        logline!(
            "clauth: switched to '{name}' but could not carry its MCP server logins: {e:#}. \
             Re-authenticate any MCP server that reports a signed-out session"
        );
    }
}

/// Symlink `~/.claude/.credentials.json` → profile's `credentials.json` (copy on
/// Windows). Refuses to overwrite a non-matching regular file — that would silently
/// drop a CC re-login the user hasn't resolved yet.
pub(crate) fn link_profile_credentials(name: &ProfileName) -> Result<()> {
    with_state_lock(|_held| {
        let link = claude_credentials_path()?;
        let target = install_source_path(name)?;

        if let Ok(meta) = link.symlink_metadata() {
            if !meta.file_type().is_symlink() {
                // A matching LOGIN clears the file, whatever else differs: the
                // non-login blocks are carried across below
                // (`carry_live_extra_into`), so a live file differing from the
                // profile only in those is not an unresolved re-login. Non-empty
                // and equal is the same test `classify_link_at` applies, blank
                // clause included: two blanks are two logged-out shells, never a
                // match. Anything the login test cannot clear falls back to the
                // byte compare this replaced, so a file too torn to parse and one
                // holding no login block at all keep refusing exactly as before.
                // Without that fallback both sides read `None`, compare equal, and
                // the live file is removed with nothing to relink.
                let login_of = |p: &Path| {
                    read_json_file::<ClaudeCredentials>(p)
                        .ok()
                        .and_then(|c| c.access_token().map(str::to_string))
                        .filter(|t| !t.is_empty())
                };
                let logins_match = match (login_of(&link), login_of(&target)) {
                    (Some(live), Some(stored)) => live == stored,
                    _ => false,
                };
                if !logins_match && std::fs::read(&link).ok() != std::fs::read(&target).ok() {
                    anyhow::bail!(
                        "refusing to replace .credentials.json: live file differs from profile '{name}'; {} first",
                        crate::format::RESOLVE_IN_TUI
                    );
                }
            }
            // Preserve the live MCP-server logins onto the incoming profile
            // before the swap, so switching accounts keeps them intact.
            carry_live_extra_best_effort(&link, &target, name);
            std::fs::remove_file(&link).context("failed to remove old .credentials.json")?;
        }

        if target.exists() {
            if let Some(parent) = link.parent() {
                std::fs::create_dir_all(parent)?;
            }
            create_symlink(&target, &link)?;
        }
        // macOS: make the switch real, since Claude Code reads the Keychain.
        // `Leave` because this is the GUARDED relink: it is also what a rename,
        // a first-ever capture, and the daemon's and TUI's boot reconcile call,
        // none of which is changing accounts.
        #[cfg(target_os = "macos")]
        if crate::keychain::enabled() {
            keychain_mirror_source(&target, AbsentSource::Leave)?;
        }

        Ok(())
    })
}

pub(crate) fn clear_claude_credentials() -> Result<()> {
    with_state_lock(|_held| {
        let link = claude_credentials_path()?;
        if link.symlink_metadata().is_ok() {
            std::fs::remove_file(&link).context("failed to remove .credentials.json")?;
        }
        // macOS: also sign the Keychain out so Claude Code can't spend the
        // account (parity with removing the credential file). Whatever login the
        // item held goes, possibly a chain CC rotated after our last capture,
        // which is lost and needs a re-login. What survives is the MCP-server
        // logins, which belong to no account and would otherwise be collateral
        // of every wrap-off; an item left holding nothing else is deleted.
        #[cfg(target_os = "macos")]
        if crate::keychain::enabled() {
            crate::keychain::keychain_sign_out()?;
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
                .and_then(|name| crate::profile::load_profile(&ProfileName::from(name)).ok())
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
        "ANTHROPIC_DEFAULT_FABLE_MODEL" => "the fable model field",
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
    with_state_lock(|_held| apply_profile_to_claude_settings_inner(profile, prev_env_keys))
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
    atomic_write(&path, content).context("failed to write settings.json")
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
fn build_api_key_helper_command(exe: &Path, profile_name: &ProfileName) -> String {
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

/// Whether `profile` carries a usable api key: trimmed, non-empty, and free of
/// whitespace/control chars. Hoisted from [`build_claude_settings_json`], which
/// gates its `apiKeyHelper` wiring on exactly this test — sharing the function
/// keeps the delegate guard from drifting from the real wiring.
fn has_usable_api_key(profile: &Profile) -> bool {
    profile
        .api_key
        .as_deref()
        .map(str::trim)
        .filter(|k| !k.is_empty())
        .is_some_and(|k| validate_api_key(k).is_ok())
}

/// Whether [`build_claude_settings_json`] wires an inference auth source into
/// the spawned `claude`: a usable api key (minted per request via the
/// `apiKeyHelper` it writes) or a profile `env` entry carrying
/// `ANTHROPIC_AUTH_TOKEN` / `ANTHROPIC_API_KEY` (it clears the former from the
/// settings env block, then applies `profile.env` LAST, so an explicit entry is
/// what the spawned process sees). The delegate and fan-out guards refuse a
/// recognised third-party profile this returns `false` for.
///
/// Deliberately NOT [`crate::usage::third_party_credentialed`], the usage
/// fetch predicate: that one treats Alibaba's console session as a credential,
/// and the console session authenticates the quota gateway only, never
/// inference. A keyless Alibaba profile fails THIS test.
///
/// The load boundary reads the same env half:
/// `crate::profile`'s `effective_base_url` keeps a managed `base_url` behind a
/// stored pair when an `[env]` auth entry is present, so an endpoint the
/// preserve arm keeps (`has_own_inference_endpoint` below) survives the next
/// `load_profile`. Change one and the other must follow.
pub(crate) fn has_inference_auth(profile: &Profile) -> bool {
    has_usable_api_key(profile)
        || profile.env.iter().any(|(k, v)| {
            matches!(k.as_str(), "ANTHROPIC_AUTH_TOKEN" | "ANTHROPIC_API_KEY")
                && !v.trim().is_empty()
        })
}

/// Whether this account's inference runs on its OWN endpoint and credential,
/// independent of any stored OAuth chain: it routes somewhere
/// ([`Profile::routing_endpoint`], so an `[env] ANTHROPIC_BASE_URL` counts like
/// a managed `base_url`) and has something to authenticate there with.
///
/// The single predicate behind two decisions the owner ruled on the same day,
/// deliberately shared so they cannot drift apart: a browser re-login PRESERVES
/// such an account's endpoint + key instead of wiping them
/// (`actions::overwrite_captured_profile`), and a dead OAuth chain does NOT
/// refuse its `delegate` (`mcp::preflight_target`), because the chain it lost
/// feeds usage polling and nothing else.
///
/// Never keyed on a RECOGNISED provider: whether clauth has a usage integration
/// for a host says nothing about whether inference works against it, and most
/// endpoints in use resolve to `provider: None`. The keyless refusal is a
/// different question and keeps its own `is_third_party` scope — an
/// unrecognised endpoint may be a local model that needs no key at all.
pub(crate) fn has_own_inference_endpoint(profile: &Profile) -> bool {
    profile
        .routing_endpoint()
        .map(str::trim)
        .is_some_and(|u| !u.is_empty())
        && has_inference_auth(profile)
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
        ("ANTHROPIC_DEFAULT_FABLE_MODEL", &profile.models.fable),
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
    // its stdout as both `X-Api-Key` and `Authorization: Bearer`. The helper
    // reads the key from `config.toml`
    // (0o600, the source of truth) via a hidden subcommand, so the raw key
    // never reaches the runtime settings.json. A profile with no api_key (a
    // whitespace-only or control-char-poisoned key is one `api_key_for_profile`
    // and `validate_api_key` also refuse to mint) removes any stale helper so a
    // switch can't inherit it, and never wires a helper that would only fail at
    // mint — symmetric with the fail-closed behavior at the other end.
    let has_api_key = has_usable_api_key(profile);
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
pub(crate) fn snapshot_active_credentials(config: &mut AppConfig) -> Result<()> {
    with_state_lock(|held| {
        let Some(active) = config.state.active_profile.as_ref().cloned() else {
            return Ok(());
        };
        if matches!(classify_credentials_link(&active)?, LinkState::Diverged) {
            if is_first_login(&active)? {
                adopt_first_login(config, &active)?;
            }
            return Ok(());
        }
        snapshot_active_credentials_unchecked(config, &active, held)
    })
}

/// Store the live `.credentials.json` into the profile then replace it with a
/// symlink. Must only be called after `is_first_login` returns true.
///
/// The store write is checked rather than assumed: its sink writes nothing when
/// `active` names a profile the config does not carry, and this is the one
/// caller that would then relink a profile with no install source. On macOS that
/// forcing relink signs the Keychain out, so a silent no-op here would delete a
/// genuine Claude Code login from BOTH the live file and the Keychain, having
/// captured it into neither. Refusing costs an adopt; the alternative costs the
/// login.
pub(crate) fn adopt_first_login(config: &mut AppConfig, active: &ProfileName) -> Result<()> {
    with_state_lock(|held| {
        snapshot_active_credentials_unchecked(config, active, held)?;
        anyhow::ensure!(
            install_source_path(active)?.exists(),
            "refusing to relink '{active}': the live login was not captured into it"
        );
        force_link_profile_credentials(active)
    })
}

fn snapshot_active_credentials_unchecked(
    config: &mut AppConfig,
    active: &ProfileName,
    held: &StateLockHeld,
) -> Result<()> {
    // CLA-SPLIT: a profile whose live slot holds its static session token carries
    // nothing to snapshot, and capturing the live file into `profile.credentials`
    // would clobber the clauth-private usage OAuth pair. The guard lives at this
    // shared sink so every caller is covered: both the divergence-modal
    // "overwrite" and the CLI reconciled switch reach here via
    // `force_snapshot_active_credentials`. `adopt_first_login` never hits it for
    // a session-token profile (the install source exists, so `is_first_login` is
    // false), so the guard is a safe no-op on that path.
    if has_session_token(active) {
        return Ok(());
    }
    // Fresh, not just the in-memory active marker: the active profile may have
    // been deleted or renamed out-of-process since this caller's config was
    // loaded (a daemon switch/switch-off holds a stale config between reloads).
    // `save_profile` would recreate the deleted directory, so consult the
    // on-disk list before writing. Callers run this under the state flock, so
    // the read is stable.
    if !crate::profile::is_configured(active)? {
        return Ok(());
    }
    let credentials = read_claude_credentials()?;
    // Only a real live login is captured. A logged-out shell (blank tokens) OR an
    // absent live file (a TOCTOU delete in the modal-confirm window, or a
    // dangling symlink) is not a login; persisting either would overwrite the
    // stored chain with blanks or nothing. This shared sink is the last gate
    // before every force-capture caller writes (modal Overwrite, CLI reconciled
    // switch, reconcile_startup's default_divergence, adopt), so the invariant
    // belongs here, not in each caller.
    let Some(credentials) = credentials else {
        return Ok(());
    };
    if live_login_is_empty(&credentials) {
        return Ok(());
    }
    if let Some(profile) = config.find_mut(active) {
        profile.set_credentials(Some(credentials), held);
        save_profile(profile)?;
    }
    Ok(())
}

/// Snapshot the live `.credentials.json` into the active profile unconditionally.
pub(crate) fn force_snapshot_active_credentials(config: &mut AppConfig) -> Result<()> {
    with_state_lock(|held| {
        let Some(active) = config.state.active_profile.as_ref().cloned() else {
            return Ok(());
        };
        snapshot_active_credentials_unchecked(config, &active, held)
    })
}

/// Re-link `.credentials.json` to `name`'s stored credentials, overwriting the live path.
pub(crate) fn force_link_profile_credentials(name: &ProfileName) -> Result<()> {
    with_state_lock(|_held| {
        let link = claude_credentials_path()?;
        let target = install_source_path(name)?;
        if link.symlink_metadata().is_ok() {
            // Preserve the live MCP-server logins onto the incoming profile
            // before the swap, so switching accounts keeps them intact.
            carry_live_extra_best_effort(&link, &target, name);
            std::fs::remove_file(&link).context("failed to remove .credentials.json")?;
        }
        if target.exists() {
            if let Some(parent) = link.parent() {
                std::fs::create_dir_all(parent)?;
            }
            create_symlink(&target, &link)?;
        }
        // macOS: make the switch real, since Claude Code reads the Keychain.
        // `SignOut` because this is the FORCING relink, which every switch path
        // takes: the caller has decided this profile's credentials are what the
        // live slot holds, so a profile storing none must stop the item serving
        // the account the switch just left.
        #[cfg(target_os = "macos")]
        if crate::keychain::enabled() {
            keychain_mirror_source(&target, AbsentSource::SignOut)?;
        }
        Ok(())
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
    with_state_lock(|_held| {
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
