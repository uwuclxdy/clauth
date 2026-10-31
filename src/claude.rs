use std::collections::{BTreeSet, HashMap};
use std::env;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};

use crate::lock::{StateLockHeld, with_state_lock};
use crate::lockorder::{RankedMutex, rank};
use crate::logline::logline;
use crate::profile::{
    AppConfig, ClaudeCredentials, Profile, ProfileName, atomic_write, atomic_write_600, claude_dir,
    profile_dir, read_json_file, save_profile,
};
use crate::usage::{
    ForgetPlan, OAuthIdentity, forget_disk_plan, note_forget_plan, oauth_changed,
    read_before_identities, setup_token_plan, throughput_identity,
};

pub(crate) fn claude_credentials_path() -> Result<PathBuf> {
    Ok(claude_dir()?.join(".credentials.json"))
}

pub(crate) fn claude_settings_path() -> Result<PathBuf> {
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
    let (removed, plan) = with_state_lock(|_held| {
        let plan = setup_token_plan(name, None);
        // The sidecar can hold the account's design login; `credentials.json`
        // becomes the install source once it goes, so the keys move there first,
        // and a design login the live file alone holds is saved into the
        // install source before that: the sidecar, while its token is still
        // the live one.
        sync_live_extra_into_install_source(name);
        hand_sidecar_keys_to_store(&path);
        let removed = match std::fs::remove_file(&path) {
            Ok(()) => true,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
            Err(e) => return Err(anyhow::Error::new(e).context("remove session-token.json")),
        };
        // As in `write_session_token`: the readings go once the token does.
        forget_disk_plan(name, plan);
        Ok((removed, plan))
    })?;
    note_forget_plan(name, plan);
    Ok(removed)
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

/// Where `name`'s preserved mint backup sits. One derivation for both readers,
/// so they cannot disagree about WHICH file answers the question. They differ
/// only on an unresolvable home, deliberately: the restore verb propagates that
/// as a failure, the render paths below cannot fail and take the absence.
pub(crate) fn static_backup_path(name: &ProfileName) -> Result<PathBuf> {
    Ok(profile_dir(name)?.join("session-token.static.json"))
}

/// Whether `name` holds a preserved mint backup (`session-token.static.json`).
/// A single stat: the Setup tab's clear row uses it to DISCLOSE that the
/// backup goes with a clear, and to stay reachable while the backup is the
/// only long-lived piece left.
pub(crate) fn has_static_backup(name: &ProfileName) -> bool {
    static_backup_path(name).is_ok_and(|p| p.exists())
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
            "that doesn't look like a `claude setup-token` mint (expected an sk-ant-... value)"
        );
    }
    if token.starts_with("sk-ant-api") {
        anyhow::bail!(
            "that looks like an API key (sk-ant-api...), not a `claude setup-token` mint. \
             Installing it as the session bearer signs sessions out on first use. Capture an \
             API key with `clauth login <name> --base-url <url> --api-key <key>` instead"
        );
    }
    if token.chars().any(char::is_whitespace) {
        anyhow::bail!("the pasted token contains whitespace, likely a partial or padded paste");
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
    validate_credential_chars(key, "api key")
}

/// [`validate_api_key`]'s check for the OAuth access token `clauth __oauth-token`
/// prints: the same header-injection boundary, with the noun the surface names.
pub(crate) fn validate_oauth_token(token: &str) -> Result<()> {
    validate_credential_chars(token, "oauth token")
}

/// The shared check behind [`validate_api_key`] and [`validate_oauth_token`]:
/// refuse a credential holding whitespace or a control char, since CC forwards
/// the helper's stdout verbatim as a header value and an interior CRLF would
/// inject one. `noun` is caller-specific — the two surfaces name the credential
/// they print differently.
fn validate_credential_chars(value: &str, noun: &str) -> Result<()> {
    if value.chars().any(|c| c.is_control() || c.is_whitespace()) {
        anyhow::bail!(
            "{noun} contains whitespace or control characters, likely a bad paste or an edited config"
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
            ..crate::profile::OAuthToken::default_extra()
        }),
    };
    let bytes = serde_json::to_vec_pretty(&sidecar).context("serialize session token")?;
    let path = profile_dir(name)?.join("session-token.json");
    let plan = with_state_lock(|_held| {
        let plan = setup_token_plan(name, Some(token));
        // A re-mint relinks nothing, so a live file Claude Code wrote beside
        // the mint this replaces stops matching any store; saved first, the
        // keep below carries it onto the new mint. Over a mis-fill, whose keys
        // the keep never takes, it waits in `credentials.json` instead.
        sync_live_extra_into_install_source(name);
        let bytes = keep_non_login_in_sidecar(&bytes, &path);
        atomic_write_600(&path, bytes).context("write session-token.json")?;
        // The old token's readings go once the new one is on disk: a failed
        // write keeps the token, and its readings with it. Still inside the
        // hold, so a reload that re-seeds from disk finds them gone.
        forget_disk_plan(name, plan);
        Ok(plan)
    })?;
    note_forget_plan(name, plan);
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
             mint (no scope beyond the setup pair, no subscription type). Refusing to stamp \
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
        // While the sidecar still holds the bearer this stamp replaces, a live
        // file Claude Code wrote beside that bearer (a `/design-login`) is still
        // recognisably this account's; after the stamp no sync matches it again.
        // Saved first into the install source (`credentials.json` before the
        // first roll), the keep below carries it onto the new bearer.
        sync_live_extra_into_install_source(name);
        // The bearer this stamp replaces, read in the same hold as the write:
        // the one a session's per-session item may hold a copy of.
        let replaced = read_json_file::<ClaudeCredentials>(&path)
            .ok()
            .and_then(|c| c.claude_ai_oauth);
        preserve_static_mint(name)?;
        let bytes = keep_non_login_in_sidecar(&bytes, &path);
        atomic_write_600(&path, bytes).context("write rolling session-token.json")?;
        if let Some(revoked) = revoked_by_stamp(
            replaced.as_ref(),
            sidecar
                .claude_ai_oauth
                .as_ref()
                .map_or("", |o| o.access_token.as_str()),
        ) {
            crate::runtime::note_revoked_rolling_bearer(name, revoked);
        }
        Ok(())
    })
}

/// The refresh-less projection of a usage chain — the EXACT content a rolling
/// stamp writes: the chain's bearer, scopes, plan stamp, and rate-limit tier
/// with the refresh token dropped. One constructor, so the pre-stamp
/// classification (`roll_from_stored_chain`'s `GrantUnusable` verdict) and the
/// stamp itself can never drift apart on what "rolled" means.
pub(crate) fn rolling_projection(chain: &crate::profile::OAuthToken) -> crate::profile::OAuthToken {
    let mut rolled = crate::profile::OAuthToken {
        access_token: chain.access_token.clone(),
        refresh_token: None,
        expires_at: chain.expires_at,
        scopes: chain.scopes.clone(),
        subscription_type: chain.subscription_type.clone(),
        // A fresh mint from clauth's own chain, not a rewrite over a prior
        // store, so it starts with no outside-written keys to keep. Claude
        // Code adds its own on its first save into the sidecar, and the
        // rolling re-stamp replaces them until that next save. The one key
        // carried over is the rate-limit tier: Claude Code reads it at startup
        // (#78), so a bearer without it runs as an untiered account.
        ..crate::profile::OAuthToken::default_extra()
    };
    if let Some(tier) = chain.rate_limit_tier() {
        rolled.set_rate_limit_tier(tier.to_string());
    }
    rolled
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
            ..crate::profile::OAuthToken::default_extra()
        }),
    };
    let bytes = serde_json::to_vec_pretty(&sidecar).context("serialize session token")?;
    let dir = profile_dir(name)?;
    let plan = with_state_lock(|_held| {
        let plan = setup_token_plan(name, Some(token));
        // As in `write_session_token`: what the live file holds beside the
        // bearer this replaces is saved before it stops matching.
        sync_live_extra_into_install_source(name);
        // Both files from the SAME bytes, the sidecar's non-login keys
        // included: the backup is this account's own file too, and the roll's
        // `preserve_static_mint` already copies the sidecar into it raw.
        let sidecar = dir.join("session-token.json");
        let bytes = keep_non_login_in_sidecar(&bytes, &sidecar);
        atomic_write_600(&sidecar, &bytes).context("write session-token.json")?;
        // As in `write_session_token`: the readings go once the sidecar holds
        // the new token.
        forget_disk_plan(name, plan);
        atomic_write_600(&dir.join("session-token.static.json"), &bytes)
            .context("write session-token.static.json")?;
        Ok(plan)
    })?;
    note_forget_plan(name, plan);
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
    let (outcome, plan) = with_state_lock(|_held| {
        if !matches!(
            session_token_status(name),
            Some(SessionTokenStatus::NotLongLived)
        ) {
            return Ok((HealOutcome::NotMisfilled, ForgetPlan::default()));
        }
        // The clock gate matters here too: healing a mis-fill by installing an
        // EXPIRED mint trades a disengaged-but-working vanilla posture for a
        // credential that signs sessions out on first use. No live backup →
        // the mis-fill stays, loudly, exactly as if there were no backup.
        let Some(bytes) = live_backup_bytes(name, &backup)? else {
            return Ok((HealOutcome::NoLiveBackup, ForgetPlan::default()));
        };
        // Without a pair the mint is the OAuth half's identity, so putting it
        // back forgets what the mis-fill's identity read, as a re-mint does.
        let plan = setup_token_plan(name, mint_token(&bytes).as_deref());
        // A mis-fill is never installed, so the live file holds the login of
        // `credentials.json`, which stops being installed once the mint is
        // back: what the live file holds beside it is saved there first.
        sync_live_extra_into_install_source(name);
        // The backup goes back whole and nothing of the mis-fill joins it: the
        // keys beside a mis-filled login can be another account's (its org id,
        // device token, gateway), and they are evidence, quarantined with it.
        quarantine_file_locked(name, &sidecar, "session-token.json")?;
        atomic_write_600(&sidecar, bytes).context("restore session-token.json")?;
        // As in `write_session_token`: the readings go once the mint is back.
        forget_disk_plan(name, plan);
        std::fs::remove_file(&backup).context("remove consumed static backup")?;
        Ok((HealOutcome::Healed, plan))
    })?;
    note_forget_plan(name, plan);
    Ok(outcome)
}

/// The access token a preserved mint's bytes hold, for the forget plan of a
/// writer that puts it back.
fn mint_token(bytes: &[u8]) -> Option<String> {
    serde_json::from_slice::<ClaudeCredentials>(bytes)
        .ok()?
        .access_token()
        .map(str::to_string)
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
    let guard = match crate::runtime::RotationGuard::acquire(name) {
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
    // The stamp below reads the stored chain: a staged pair lands first.
    if let Err(refused) = crate::oauth::land_stage_for_install(name, &guard) {
        crate::logline::logline!(
            "clauth: start-time rolling-token arming for '{name}' skipped ({})",
            refused.text_with_status()
        );
        return;
    }
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
                "clauth: '{name}' preserved static mint has itself expired. Not restoring it. \
                 Re-mint with `clauth login {name} --setup-token`"
            );
            Ok(None)
        }
        BackupVerdict::NoMint => {
            quarantine_file_locked(name, backup, "session-token.static.json")?;
            std::fs::remove_file(backup).context("remove quarantined static backup")?;
            crate::logline::logline!(
                "clauth: '{name}' preserved static backup does not hold a mint. Quarantined \
                 under the profile's quarantine/ dir. Re-mint with \
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
    let plan = with_state_lock(|_held| {
        let Some(bytes) = live_backup_bytes(name, &backup)? else {
            return Ok(None);
        };
        // Without a pair the sidecar's token is the OAuth half's identity, so
        // putting the mint back forgets the rolling token's readings, as a
        // re-mint does.
        let plan = setup_token_plan(name, mint_token(&bytes).as_deref());
        // What the live file holds beside the login this replaces stops
        // matching once the mint is back, so it is saved first, into the
        // install source: the sidecar's bearer, or over a mis-fill (quarantined
        // below, never written into) `credentials.json`, where it waits, since
        // the keep takes nothing there.
        sync_live_extra_into_install_source(name);
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
        // Over a mis-fill this keeps nothing, like the heal. A mis-fill is a
        // login that parses as a rotating pair; a sidecar whose login does not
        // parse is not recognised as one, here or by the quarantine above, and
        // its keys are kept like the account's own. Over this account's
        // own rolling bearer it keeps the bearer's non-login keys over the
        // backup's: the backup is the sidecar as the first roll or the last
        // re-mint found it, and every sync since landed in the sidecar, so the
        // sidecar's copy is the newer one. Ceiling: a key the backup holds and
        // the sidecar lost comes back with the backup.
        let bytes = keep_non_login_in_sidecar(&bytes, &sidecar);
        atomic_write_600(&sidecar, bytes).context("restore session-token.json")?;
        // As in `write_session_token`: the readings go once the mint is back.
        forget_disk_plan(name, plan);
        std::fs::remove_file(&backup).context("remove consumed static backup")?;
        Ok(Some(plan))
    })?;
    if let Some(plan) = plan {
        note_forget_plan(name, plan);
    }
    Ok(plan.is_some())
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

/// CLA-ROLL: whether a rolling sidecar's bearer has fallen behind its usage
/// chain: the sidecar classifies as a rolling bearer, the chain holds a
/// rotating pair whose access token is still live at `now`, and the two
/// access tokens differ. Every stamp copies the
/// chain's access token, so after any landed rotation they are equal; a
/// difference means the chain rotated without the stamp reaching the sidecar,
/// and the server revoked the bearer the sidecar still serves. PURE.
pub(crate) fn rolling_bearer_behind_chain(
    sidecar: &crate::profile::OAuthToken,
    chain: &crate::profile::OAuthToken,
    now: i64,
) -> bool {
    sidecar_kind_of(sidecar) == SidecarKind::Rolling
        && chain.refresh_token.is_some()
        && chain.expires_at.is_some_and(|exp| exp > now)
        && !chain.access_token.is_empty()
        && sidecar.access_token != chain.access_token
}

/// The rolling bearer a stamp writing `stamped` revoked: the one it replaced
/// (`replaced`, the sidecar's block before the write), when that was a rolling
/// bearer clauth wrote ([`SidecarKind::Rolling`]) and differs from the new
/// one. A mint, a mis-fill, an absent sidecar or an unchanged bearer yields
/// `None`: nothing a session can hold a copy of was revoked by this stamp.
/// PURE.
pub(crate) fn revoked_by_stamp(
    replaced: Option<&crate::profile::OAuthToken>,
    stamped: &str,
) -> Option<String> {
    let replaced = replaced?;
    (sidecar_kind_of(replaced) == SidecarKind::Rolling
        && !replaced.access_token.is_empty()
        && replaced.access_token != stamped)
        .then(|| replaced.access_token.clone())
}

/// The item side of [`copied_bearer_repoint`]'s rule: whether the item's
/// `claudeAiOauth` is exactly `revoked` and refreshless ([`sidecar_kind_of`]
/// is not [`SidecarKind::Misfilled`], so `refreshToken: ""` counts as a
/// refresh token). An item failing this has nothing to re-point, now or
/// later. PURE.
#[cfg_attr(
    not(target_os = "macos"),
    allow(
        dead_code,
        reason = "the only caller is the macOS copied-bearer re-point; the rule is pinned on every platform"
    )
)]
pub(crate) fn item_holds_revoked(item: &serde_json::Value, revoked: &str) -> bool {
    if revoked.is_empty() {
        return false;
    }
    let Some(held) = item
        .get("claudeAiOauth")
        .and_then(|b| serde_json::from_value::<crate::profile::OAuthToken>(b.clone()).ok())
    else {
        return false;
    };
    held.access_token == revoked && sidecar_kind_of(&held) != SidecarKind::Misfilled
}

/// Whether a session's per-session Keychain item holds a copy of `revoked`,
/// the rolling bearer its member's last stamp replaced, and should take the
/// sidecar's current one. True only when the item holds that exact bearer
/// ([`item_holds_revoked`]), and the sidecar now holds a live rolling bearer
/// that differs from it and is not itself behind its chain
/// (`sidecar_behind_chain`, from [`rolling_sidecar_behind_chain`]). Matching
/// the exact revoked bearer, not any differing one, leaves alone a static
/// mint, another account's bearer and anything clauth did not write for this
/// member. PURE.
#[cfg_attr(
    not(target_os = "macos"),
    allow(
        dead_code,
        reason = "the only caller is the macOS copied-bearer re-point; the rule is pinned on every platform"
    )
)]
pub(crate) fn copied_bearer_repoint(
    item: &serde_json::Value,
    revoked: &str,
    sidecar: &serde_json::Value,
    now: i64,
    sidecar_behind_chain: bool,
) -> bool {
    if sidecar_behind_chain || !item_holds_revoked(item, revoked) {
        return false;
    }
    let Some(current) = sidecar
        .get("claudeAiOauth")
        .and_then(|b| serde_json::from_value::<crate::profile::OAuthToken>(b.clone()).ok())
    else {
        return false;
    };
    sidecar_kind_of(&current) == SidecarKind::Rolling
        && current.expires_at.is_some_and(|exp| exp > now)
        && !current.access_token.is_empty()
        && current.access_token != revoked
}

/// Replace `item`'s copied bearer with `sidecar`'s block when
/// [`copied_bearer_repoint`] holds, reporting whether it changed; every other
/// key of the item (`mcpOAuth` among them) is kept as it was. PURE.
#[cfg_attr(
    not(target_os = "macos"),
    allow(
        dead_code,
        reason = "the only caller is the macOS copied-bearer re-point; the rule is pinned on every platform"
    )
)]
pub(crate) fn repoint_copied_bearer(
    item: &mut serde_json::Value,
    revoked: &str,
    sidecar: &serde_json::Value,
    now: i64,
    sidecar_behind_chain: bool,
) -> bool {
    if !copied_bearer_repoint(item, revoked, sidecar, now, sidecar_behind_chain) {
        return false;
    }
    let (Some(obj), Some(block)) = (item.as_object_mut(), sidecar.get("claudeAiOauth")) else {
        return false;
    };
    obj.insert("claudeAiOauth".to_string(), block.clone());
    true
}

/// [`rolling_bearer_behind_chain`] over `name`'s files on disk: the
/// `session-token.json` sidecar against the `credentials.json` chain, now.
/// Either one absent or unreadable reads `false`.
pub(crate) fn rolling_sidecar_behind_chain(name: &ProfileName) -> bool {
    let Ok(dir) = profile_dir(name) else {
        return false;
    };
    let read = |file: &str| {
        read_json_file::<ClaudeCredentials>(&dir.join(file))
            .ok()
            .and_then(|c| c.claude_ai_oauth)
    };
    match (read("session-token.json"), read("credentials.json")) {
        (Some(sidecar), Some(chain)) => {
            rolling_bearer_behind_chain(&sidecar, &chain, crate::usage::now_ms() as i64)
        }
        _ => false,
    }
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
    match (crate::canonical::identity(a), crate::canonical::identity(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

/// The live slot's file-layer follower (#104): the Linux counterpart of the
/// macOS Keychain mirror. After a credential write changes what the live slot
/// should hold (a re-stamp, a rotation), a REGULAR-file live slot holding a
/// login clauth wrote is updated in place — Claude Code replaced the symlink
/// with a regular file (its temp+rename write), and without this the revoked
/// bearer keeps serving. A symlink needs nothing (the store write just
/// landed); `Missing` has nothing to write; a login clauth does not recognize
/// is left for the TUI — the same recognition gate as the Keychain mirror.
#[derive(Debug)]
pub(crate) enum LiveSlotFollow {
    /// The live login was one clauth wrote; the file now holds `incoming`.
    Followed,
    /// The live file holds a different login (a real CC re-login). Untouched.
    NotOurs,
    /// The live file exists but does not parse — possibly a mid-write login.
    /// Unlike the Keychain mirror (whose truncated bytes are almost always
    /// clauth's own cut-off write), clauth never writes this file
    /// non-atomically, so there is nothing of ours to heal. Untouched.
    /// Carries the position alone, never the values the parser would quote.
    Unreadable(Option<crate::format::ParsePosition>),
    /// No live file, or a live symlink (which follows the store write itself).
    Noop,
}

/// Follow `incoming` into a regular-file live slot when its login is one of
/// `candidates` (the bearer(s) clauth wrote or is replacing) or a logged-out
/// shell. The write replaces the `claudeAiOauth` block alone — every other
/// block (`mcpOAuth` included) survives, the file-layer `Keep::Everything`.
/// The write is atomic; callers hold their own locks, as with the mirror. The
/// recognize-then-write gap is the same accepted race the mirror carries: the
/// next poll's content check self-heals a lost interleaving with a publish.
pub(crate) fn follow_live_file_login(
    candidates: &[&str],
    incoming: &crate::profile::OAuthToken,
) -> Result<LiveSlotFollow> {
    let link = claude_credentials_path()?;
    let meta = match link.symlink_metadata() {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(LiveSlotFollow::Noop),
        Err(e) => return Err(e).context("failed to stat .credentials.json"),
    };
    if meta.file_type().is_symlink() {
        return Ok(LiveSlotFollow::Noop);
    }
    let bytes = std::fs::read(&link).context("failed to read .credentials.json")?;
    let live = match serde_json::from_slice::<ClaudeCredentials>(&bytes) {
        Ok(live) => live,
        Err(e) => {
            return Ok(LiveSlotFollow::Unreadable(
                crate::format::json_parse_position(&e),
            ));
        }
    };
    let recognized =
        live_login_is_empty(&live) || live.access_token().is_some_and(|t| candidates.contains(&t));
    if !recognized {
        return Ok(LiveSlotFollow::NotOurs);
    }
    let mut value: serde_json::Value = serde_json::from_slice(&bytes).map_err(|e| {
        crate::format::parse_error(".credentials.json", crate::format::json_parse_position(&e))
    })?;
    value["claudeAiOauth"] =
        serde_json::to_value(incoming).context("failed to serialize the incoming login")?;
    atomic_write_600(
        &link,
        serde_json::to_vec(&value).context("failed to serialize .credentials.json")?,
    )
    .context("failed to follow the live login")?;
    Ok(LiveSlotFollow::Followed)
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

/// What the Keychain mirror does when the profile's install source holds no
/// readable login (an api-key or third-party profile, or a store that does not
/// parse or cannot be read). Chosen by the caller, because only the caller knows
/// whether it meant to change which account is live.
#[cfg(any(test, target_os = "macos"))]
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

/// What the macOS Keychain mirror does for one resolved install source, as a
/// pure decision split from the macOS-only side effects so the routing is
/// testable on every platform.
#[cfg(any(test, target_os = "macos"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KeychainMirrorAction {
    /// The source holds a login: install it into the Keychain item.
    Install,
    /// The source holds no readable login and the caller may sign the item out.
    SignOut,
    /// The source holds no readable login and the caller must leave the item.
    Leave,
}

/// Route one (linkable, absent) input to the mirror's action: a linkable
/// source installs whatever the read boundary accepts, and an unreadable one
/// takes the same absent arm a missing store does under the caller's
/// [`AbsentSource`] — so a switch to a torn store never reaches the
/// parse-refusing read boundary.
#[cfg(any(test, target_os = "macos"))]
fn keychain_mirror_verdict(linkable: bool, absent: AbsentSource) -> KeychainMirrorAction {
    if linkable {
        KeychainMirrorAction::Install
    } else {
        match absent {
            AbsentSource::SignOut => KeychainMirrorAction::SignOut,
            AbsentSource::Leave => KeychainMirrorAction::Leave,
        }
    }
}

/// macOS: mirror what the file layer just installed into the Keychain, which is
/// where Claude Code actually reads its login. `path` is the install source the
/// swap resolved: its whole JSON object becomes the item, so the incoming
/// account's own blocks travel with its login and the outgoing account's do not
/// (`keychain::keychain_install` carries the MCP-server logins across).
///
/// Runs BEFORE the symlink swap. A running Claude Code notices a switch by the
/// link target's mtime changing, then re-reads the Keychain and memoizes the
/// new mtime; it never looks again until the mtime moves once more. Swapping
/// the link first opened a window (seconds, for a switch under load) in which a
/// session that sent a request saw the new mtime, read the OLD login out of the
/// item, and stayed on the account the switch left for the rest of its life
/// (2026-09-24: two busy sessions kept spending a 5h-spent account). With the
/// item written first, any reload the link triggers reads the new login.
///
/// `?`-fatal: a failure here leaves both layers on the old account, and every
/// write is idempotent, so retrying the switch re-runs it. The inverse - this
/// write landing and the publish below failing - leaves the item switched
/// while the link is not: live sessions never re-read (no mtime moved), fresh
/// ones resolve the item, and a retry completes the switch.
///
/// The item it replaces may hold a login no store has saved (a bare `claude`
/// refreshed the account it runs on there): the write then leaves the item in
/// place and says so ([`bare_item_may_be_replaced`]; `placed` is the key of
/// an item the caller settled), returning [`ItemWrite::Refused`], which the
/// caller turns into a failed switch before it publishes the link.
#[cfg(target_os = "macos")]
fn keychain_mirror_source(
    path: &Path,
    linkable: bool,
    absent: AbsentSource,
    preserve: bool,
    placed: Option<&[u8; 32]>,
) -> Result<ItemWrite> {
    // CLA-SPLIT: callers pass the already-resolved install source and its
    // linkability, so the symlink target, the Keychain content and the absent
    // arm all come from ONE resolution: a session-token.json vanishing between
    // two stats can't split them.
    let refused = std::cell::Cell::new(false);
    let may_replace = |item: &serde_json::Value| {
        let may = bare_item_may_be_replaced(item, placed);
        refused.set(!may);
        may
    };
    let written = match keychain_mirror_verdict(linkable, absent) {
        KeychainMirrorAction::Install => {
            let store = checked_store_at(path)?;
            // What the install replaces belongs to the account whose login it
            // holds: the outgoing one on a switch, this one on a relink. Its
            // non-login keys are saved there before the write, whether or not
            // the write then lands.
            crate::keychain::keychain_install(&store, |item| {
                if preserve {
                    sync_item_into_its_store(item, Some(path));
                }
                may_replace(item)
            })?
        }
        // Both other outcomes complete the switch: an actually-signed-out item,
        // and the locked-keychain skip, whose event line `sign_out_at` already
        // raised — the item keeps serving the departed account until the switch
        // is re-run on an unlocked keychain, which is recoverable where the
        // pre-fix blind delete was not.
        KeychainMirrorAction::SignOut => {
            crate::keychain::keychain_sign_out(preserve, may_replace, |item| {
                preserve && sync_item_into_its_store(item, None)
            })?
        }
        KeychainMirrorAction::Leave => false,
    };
    Ok(match (refused.get(), written) {
        (true, _) => ItemWrite::Refused,
        (false, true) => ItemWrite::Landed,
        (false, false) => ItemWrite::Left,
    })
}

/// What [`keychain_mirror_source`] did to the bare item.
#[cfg(target_os = "macos")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ItemWrite {
    /// The item holds the source's login now, or is signed out for an absent
    /// source under [`AbsentSource::SignOut`].
    Landed,
    /// Left as it was: a locked keychain, or [`AbsentSource::Leave`] over an
    /// absent source.
    Left,
    /// The writer guard found a login no store saved and left it whole.
    Refused,
}

/// macOS: install the store `path` holds into the NAMESPACED Keychain item for
/// `config_dir` — the multi-session swap executor's Keychain half. The executor
/// repoints the session's credential link and this writes what a session's CC
/// actually reads (it resolves the Keychain first, namespaced per
/// `CLAUDE_CONFIG_DIR`). Same typed boundary check as the bare-item mirror
/// ([`keychain_mirror_source`]); no absent-source arm, because the swap's own
/// precondition already refused a member with no credential store.
///
/// Runs AFTER the state-flock hold that moved the link (a `security` subprocess
/// must never span the flock) and is loud-not-fatal on failure: the file layer
/// is swapped and the write is idempotent. A transient failure (a locked
/// keychain) is retried: the swap's write on the session's swap polls, spaced
/// by a doubling backoff, and the start seed's write on its credential ticks;
/// any other failure waits for a later swap onto another member. Callers pair
/// it with [`carry_session_item_into`] first.
#[cfg(target_os = "macos")]
pub(crate) fn keychain_mirror_source_for_config_dir(
    path: &Path,
    config_dir: &Path,
    owned: &crate::runtime::namespaced_keychain_ledger::OwnedKeychainWrite,
) -> Result<()> {
    let store = checked_store_at(path)?;
    crate::keychain::keychain_install_for_config_dir(&store, config_dir, owned)
}

/// macOS: carry the pair the session's Claude Code left in its per-config-dir
/// Keychain item back into `store`, the install source of the member the
/// session is leaving — the Keychain twin of the file drain
/// (`sync_credentials_unlocked`), run before a swap overwrites that item. CC
/// keeps its refreshed pair ONLY in the item: it deletes the runtime file once
/// it has migrated, so the swap's item write would otherwise destroy the
/// outgoing member's only live pair.
///
/// The item is not unconditionally fresher: the STORE moves too (a `clauth
/// login` recapture, a switch-away snapshot), so the winner is the side whose
/// login expires later — CC's refresh and a recapture both advance expiry —
/// and a tie keeps the store. The compare and the write share ONE state-flock
/// hold, so a store write landing between them cannot be reverted. The file
/// drain's CLA-SPLIT guard carries over unchanged: a differing item over a
/// static session token is a session-side re-login, never adopted over the
/// token. An unparseable item carries nothing and heals instead: the swap's
/// write replaces the truncated bytes rather than skipping inertly.
///
/// The read (a `security` subprocess) runs OUTSIDE the state flock.
///
/// Ownership of the item's login is never proven first: neither clauth-written
/// stores nor CC-written items carry a top-level account anchor on real blobs,
/// and a skipped rescue overwrites the account's only live refresh token — so
/// any item whose login expires later is adopted.
#[cfg(target_os = "macos")]
pub(crate) fn carry_session_item_into(store: &Path, config_dir: &Path) -> Result<()> {
    let blob = match crate::keychain::read_config_dir_item(config_dir) {
        Ok(Some(blob)) => blob,
        Ok(None) => return Ok(()),
        Err(e) if crate::keychain::read_failed_unparseable(&e) => {
            logline!(
                "clauth: the per-session Keychain item is not valid JSON; the swap's write \
                 replaces it rather than carrying its bytes"
            );
            return Ok(());
        }
        Err(e) => return Err(e),
    };
    let bytes = serde_json::to_vec(&blob).context("failed to serialize the Keychain item")?;
    let parsed: ClaudeCredentials = serde_json::from_slice(&bytes).map_err(|e| {
        crate::format::parse_refusal(
            format!(
                "the Keychain item for {} is not Claude credentials",
                config_dir.display()
            ),
            crate::format::json_parse_position(&e),
        )
    })?;
    if parsed.claude_ai_oauth.is_none() {
        return Ok(());
    }
    crate::lock::with_state_lock(|_held| {
        let store_bytes = std::fs::read(store).ok();
        if store_bytes.as_deref() == Some(bytes.as_slice()) {
            return Ok(());
        }
        if store.file_name().is_some_and(|f| f == "session-token.json") {
            logline!(
                "clauth: kept the static session token (a session-side re-login \
                 found in the per-session Keychain item is never adopted over it)"
            );
            return Ok(());
        }
        let store_value = store_bytes
            .as_deref()
            .and_then(|sb| serde_json::from_slice::<serde_json::Value>(sb).ok());
        if !item_login_outranks_store(&blob, store_value.as_ref()) {
            // The store moved after the item was last written (a recapture, a
            // switch-away snapshot): keep it, the carry must not revert it.
            return Ok(());
        }
        if !session_item_carry_gate(store, &parsed)? {
            return Ok(());
        }
        let store_creds = store_bytes
            .as_deref()
            .and_then(|sb| serde_json::from_slice::<ClaudeCredentials>(sb).ok());
        crate::profile::atomic_write_600(store, &bytes)
            .with_context(|| format!("failed to write {}", store.display()))?;
        // Mark the `/profile` reading stale only when the carried LOGIN changed,
        // never for a rewrite that touched only other keys.
        if crate::profile::login_changed(store_creds.as_ref(), &parsed) {
            crate::usage::mark_profile_read_stale(store);
        }
        Ok(())
    })
}

/// The spent-token gate of [`carry_session_item_into`]: `Ok(true)` lets the
/// item's login be written over `store`, `Ok(false)` keeps the store (an item
/// still holding a token the server consumed is a rewind, the rule the file
/// drain keeps), and `Err` refuses the carry when the record beside the store
/// cannot be read: the swap then stays inert instead of writing the incoming
/// member's pair over an item that may hold the account's only live refresh
/// token. PURE of the Keychain so it is pinned on every platform.
#[cfg_attr(
    not(target_os = "macos"),
    allow(
        dead_code,
        reason = "the only caller is the macOS session-item carry; the rule is pinned on every platform"
    )
)]
pub(crate) fn session_item_carry_gate(store: &Path, item: &ClaudeCredentials) -> Result<bool> {
    match crate::oauth::session_login_copy(store, item) {
        crate::oauth::SessionLoginCopy::Copyable => Ok(true),
        crate::oauth::SessionLoginCopy::Spent => Ok(false),
        crate::oauth::SessionLoginCopy::Unknown => anyhow::bail!(
            "cannot read the spent-token record beside {}, so whether the per-session Keychain \
             item's refresh token was already consumed is unknown: the item is left as it is; \
             check permissions on ~/.clauth",
            store.display()
        ),
    }
}

/// Whether the per-session Keychain item's login outranks the store's for the
/// carry: strictly-later expiry decides alone — no real blob carries a
/// top-level account anchor to gate on, and rescue wins over wrong-account
/// refusal — so a side that parses to no login or no expiry reads as zero,
/// which the other side beats. PURE so the rule is pinned on every platform;
/// the gated carry that consults it is macOS-only.
#[cfg_attr(
    not(target_os = "macos"),
    allow(
        dead_code,
        reason = "the only caller is the macOS session-item carry; the rule is pinned on every platform"
    )
)]
pub(crate) fn item_login_outranks_store(
    item: &serde_json::Value,
    store: Option<&serde_json::Value>,
) -> bool {
    let expiry = |side: Option<&serde_json::Value>| {
        side.and_then(|v| serde_json::from_value::<ClaudeCredentials>(v.clone()).ok())
            .and_then(|c| c.claude_ai_oauth)
            .and_then(|o| o.expires_at)
            .unwrap_or(0)
    };
    expiry(Some(item)) > expiry(store)
}

// ── the bare Keychain item holding another account's login (GH #106) ──────
//
// A bare `claude` still running on an account clauth switched away from keeps
// refreshing it and writes the successor pair into the bare item. That pair is
// the account's only live refresh token: no store holds it, and an item write
// over it strands the account until a browser re-login. Everything below but
// the `security` glue is platform-neutral, so the rules are pinned on every
// platform; the macOS callers are the daemon leg and the three item writers.

/// Whose login the bare item holds, by token equality against the profile
/// stores alone (no network, no `security` call).
#[cfg_attr(
    not(target_os = "macos"),
    allow(
        dead_code,
        reason = "the callers are the macOS item writers and daemon leg; the rule is pinned on every platform"
    )
)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BareItemLogin {
    /// No login, or a blank logged-out shell: nothing to save or restore.
    Inert,
    /// The active profile's store alone holds the item's login.
    ActiveOwn,
    /// One other profile's store alone holds the item's login.
    Foreign { owner: ProfileName },
    /// No store holds it, or two do. A Claude Code refresh mints a whole
    /// successor pair, so the active account's own successor lands here too:
    /// only the `/profile` probe can name its account.
    Unmatched,
}

/// What the daemon leg does about the bare item ([`bare_item_correction`]).
#[cfg_attr(
    not(target_os = "macos"),
    allow(
        dead_code,
        reason = "the callers are the macOS item writers and daemon leg; the rule is pinned on every platform"
    )
)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BareItemCorrection {
    /// The item holds the active login, or none.
    Nothing,
    /// Save the item's login into `owner`, then put the active login back.
    Rescue { owner: ProfileName },
    /// The item holds the active account's own successor: save it into the
    /// active store and never write the store's older pair over it.
    AdoptIntoActive,
    /// No single owner: write nothing anywhere and say so.
    Warn,
}

/// What [`carry_bare_item_login_into`] did to the owner's store.
#[cfg_attr(
    not(target_os = "macos"),
    allow(
        dead_code,
        reason = "the callers are the macOS item writers and daemon leg; the rule is pinned on every platform"
    )
)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BareItemCarry {
    /// The store holds the item's login now.
    Saved,
    /// The store keeps its own login: it expires no earlier than the item's,
    /// or the store's spent record names the item's refresh token.
    Kept,
    /// The carry cannot tell whether the item's refresh token is live (an
    /// unreadable spent record, a rotation staged or pending beside the store,
    /// no refresh token to carry): the item may hold the account's only live
    /// login.
    Refused,
}

/// [`BareItemLogin`] for `item` over `stores`, `(profile, store value)` pairs
/// where one profile may appear once per store file it keeps. With no active
/// profile, every saved login is another account's.
#[cfg_attr(
    not(target_os = "macos"),
    allow(
        dead_code,
        reason = "the callers are the macOS item writers and daemon leg; the rule is pinned on every platform"
    )
)]
pub(crate) fn classify_bare_item_login<'a>(
    item: &serde_json::Value,
    active: impl Into<Option<&'a ProfileName>>,
    stores: &[(ProfileName, serde_json::Value)],
) -> BareItemLogin {
    let active = active.into();
    match bare_item_token_holders(item, stores).as_deref() {
        None => BareItemLogin::Inert,
        Some([only]) if Some(*only) == active => BareItemLogin::ActiveOwn,
        Some([only]) => BareItemLogin::Foreign {
            owner: (*only).clone(),
        },
        Some(_) => BareItemLogin::Unmatched,
    }
}

/// The profiles whose store holds the item's login, each once, or `None` when
/// the item holds no login. The `find_matching_oauth_profile` tiers: a
/// rotating pair matches on its refresh token alone (its access token
/// rotates), a mint, which carries no refresh token, on its access token.
fn bare_item_token_holders<'a>(
    item: &serde_json::Value,
    stores: &'a [(ProfileName, serde_json::Value)],
) -> Option<Vec<&'a ProfileName>> {
    let access = login_access_token(item)?;
    let (key, token) = match login_refresh_token(item) {
        Some(refresh) => ("refreshToken", refresh),
        None => ("accessToken", access),
    };
    let mut holders: Vec<&ProfileName> = Vec::new();
    for (name, store) in stores {
        let held = store
            .get("claudeAiOauth")
            .and_then(|login| login.get(key))
            .and_then(serde_json::Value::as_str)
            == Some(token);
        if held && !holders.contains(&name) {
            holders.push(name);
        }
    }
    Some(holders)
}

/// The non-empty refresh token of `store`'s login.
fn login_refresh_token(store: &serde_json::Value) -> Option<&str> {
    store
        .get("claudeAiOauth")?
        .get("refreshToken")?
        .as_str()
        .filter(|token| !token.is_empty())
}

/// The one profile whose cached account id is `id`. A blank id never matches
/// (two blanks are no identity), and two profiles cached with one id name
/// nobody: neither store can be told apart as the login's home.
#[cfg_attr(
    not(target_os = "macos"),
    allow(
        dead_code,
        reason = "the callers are the macOS item writers and daemon leg; the rule is pinned on every platform"
    )
)]
pub(crate) fn profile_for_account_id<'a>(
    id: &str,
    anchors: &'a [(ProfileName, Option<crate::profile::AccountId>)],
) -> Option<&'a ProfileName> {
    let mut owners = bare_item_claimants(id, anchors);
    match (owners.next(), owners.next()) {
        (Some(one), None) => Some(one),
        _ => None,
    }
}

/// Every profile cached with account id `id`, in roster order. A blank id
/// names nobody.
fn bare_item_claimants<'a>(
    id: &str,
    anchors: &'a [(ProfileName, Option<crate::profile::AccountId>)],
) -> impl Iterator<Item = &'a ProfileName> {
    let id = id.trim().to_string();
    anchors
        .iter()
        .filter(move |(_, anchor)| {
            !id.is_empty() && anchor.as_deref().is_some_and(|a| a.trim() == id)
        })
        .map(|(name, _)| name)
}

/// The correction for `login`, given the profile the `/profile` probe named
/// for an [`BareItemLogin::Unmatched`] item (`None`: no probe, a failed one,
/// or an id no single profile is cached with).
#[cfg_attr(
    not(target_os = "macos"),
    allow(
        dead_code,
        reason = "the callers are the macOS item writers and daemon leg; the rule is pinned on every platform"
    )
)]
pub(crate) fn bare_item_correction<'a>(
    login: &BareItemLogin,
    probed: Option<&ProfileName>,
    active: impl Into<Option<&'a ProfileName>>,
) -> BareItemCorrection {
    match (login, probed) {
        (BareItemLogin::Inert | BareItemLogin::ActiveOwn, _) => BareItemCorrection::Nothing,
        (BareItemLogin::Foreign { owner }, _) => BareItemCorrection::Rescue {
            owner: owner.clone(),
        },
        (BareItemLogin::Unmatched, Some(owner)) if Some(owner) == active.into() => {
            BareItemCorrection::AdoptIntoActive
        }
        (BareItemLogin::Unmatched, Some(owner)) => BareItemCorrection::Rescue {
            owner: owner.clone(),
        },
        (BareItemLogin::Unmatched, None) => BareItemCorrection::Warn,
    }
}

/// The line after a writer saved the bare item's login into `owner`, which
/// had not saved it.
fn bare_item_saved_line(owner: &str) -> String {
    format!(
        "the macOS Keychain login was {owner}'s, written by a Claude Code still running on it; \
         saved it into {owner}"
    )
}

/// The daemon leg's line after it saved another account's login out of the
/// bare item and put the active one back.
#[cfg_attr(
    not(target_os = "macos"),
    allow(
        dead_code,
        reason = "the callers are the macOS item writers and daemon leg; the rule is pinned on every platform"
    )
)]
pub(crate) fn foreign_item_rescued_line(owner: &str, active: &str) -> String {
    format!("{} and put {active} back", bare_item_saved_line(owner))
}

/// What the daemon leg's restore leaves in the bare item, as its lines end.
fn bare_item_restore_tail(active: Option<&ProfileName>) -> String {
    match active {
        None => "signed the Keychain out, since no clauth account is active".to_string(),
        Some(active) if !bare_item_active_has_login(active) => {
            format!("signed the Keychain out, since {active} has no login to put back")
        }
        Some(active) => format!("put {active} back"),
    }
}

/// Whether `active` stores a login the restore can put back, rather than
/// sign the item out (an api-key or third-party profile, or a store that
/// does not parse): the restore's own [`linkable_install_source`] verdict.
fn bare_item_active_has_login(active: &ProfileName) -> bool {
    install_source_path(active).is_ok_and(|path| linkable_install_source(&path))
}

/// The line for a bare item holding a login no single clauth account claims,
/// from the daemon leg and from a writer that left the item in place.
const BARE_ITEM_UNCLAIMED_LINE: &str =
    "the macOS Keychain holds a login no clauth account claims; left it in place";

/// The line for a bare item whose owner's store cannot take its login now.
fn bare_item_owner_busy_line(owner: &str) -> String {
    format!(
        "the macOS Keychain holds {owner}'s login, but {owner}'s saved login is mid-rotation or \
         unreadable; left it in place, trying again in 15 minutes"
    )
}

fn log_bare_item_unclaimed() {
    logline!(
        "{}: {BARE_ITEM_UNCLAIMED_LINE}",
        crate::logline::role(crate::logline::stamped())
    );
}

/// The refusal of a writer that would overwrite a login no store saved, and
/// whose owner the lookup could not name: `cause` says why.
fn bare_item_unplaced_refusal(
    writer: BareItemWriter,
    cause: impl std::fmt::Display,
) -> anyhow::Error {
    anyhow::anyhow!(
        "the macOS Keychain holds a login no clauth account has saved yet, and clauth could not \
         look up its owner ({cause}); not {}, so that login is not lost{}",
        writer.verb(),
        writer.retry_tail("once online")
    )
}

/// The refusal of a writer whose guard found the bare item holding a login no
/// store saved: the item changed after the entry's settle, or the entry never
/// settled (a startup or poll relink nobody is there to ask about).
#[cfg_attr(
    not(target_os = "macos"),
    allow(
        dead_code,
        reason = "the callers are the macOS item writers; the text is pinned on every platform"
    )
)]
fn bare_item_changed_refusal(writer: BareItemWriter) -> anyhow::Error {
    anyhow::anyhow!(
        "the macOS Keychain holds a login no clauth account has saved; not {}, so that login is \
         not lost; save it first with:  clauth capture <name>",
        writer.verb()
    )
}

/// The roster the bare-item rules search: profiles.toml's, or every profile
/// directory on disk when it does not load (the `sync_item_into_its_store`
/// fallback). The fallback serves the search alone: claiming an owner's
/// [`crate::profile::ProfileId`], which every carry needs, reads profiles.toml
/// and refuses while it does not load.
fn bare_item_roster() -> Vec<ProfileName> {
    crate::profile::load_app_state().map_or_else(
        |_| crate::profile::profile_names_on_disk(),
        |state| state.profiles,
    )
}

/// Every store file each roster profile keeps, as JSON: the rotating pair's
/// `credentials.json` and a mint's `session-token.json`. A file that does not
/// read or parse is left out, which can only make the item look unsaved.
fn bare_item_stores(roster: &[ProfileName]) -> Vec<(ProfileName, serde_json::Value)> {
    let mut stores = Vec::new();
    for name in roster {
        let Ok(dir) = profile_dir(name) else {
            continue;
        };
        for file in ["credentials.json", "session-token.json"] {
            if let Some(value) = std::fs::read(dir.join(file))
                .ok()
                .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            {
                stores.push((name.clone(), value));
            }
        }
    }
    stores
}

/// Whether any roster profile's spent record names `refresh`: a token the
/// server consumed is dead, so writing over it strands nothing.
fn bare_item_refresh_spent(roster: &[ProfileName], refresh: &str) -> bool {
    roster
        .iter()
        .any(|name| crate::oauth::SpentRecord::read(name).spent(refresh) == Some(true))
}

/// The cached account id of every roster profile, for the `/profile` probe's
/// answer to name its owner ([`profile_for_account_id`]).
fn bare_item_anchors(
    roster: &[ProfileName],
) -> Vec<(ProfileName, Option<crate::profile::AccountId>)> {
    roster
        .iter()
        .map(|name| {
            let anchor = crate::profile_cache::load_profile_cache::<crate::profile::AccountId>(
                name,
                crate::profile_cache::ACCOUNT_ID_CACHE_FILE,
            );
            (name.clone(), anchor)
        })
        .collect()
}

/// Whether `store`, one of `name`'s store files, holds a login while `name`
/// has no cached account id: that profile may be the owner of any login.
fn bare_item_uncached_holder(
    name: &ProfileName,
    store: &serde_json::Value,
    anchors: &[(ProfileName, Option<crate::profile::AccountId>)],
) -> bool {
    login_access_token(store).is_some()
        && anchors.iter().any(|(anchored, anchor)| {
            anchored == name && anchor.as_deref().is_none_or(|a| a.trim().is_empty())
        })
}

/// Look up the account of each login-holding profile with no cached id
/// through its own unexpired access token, and cache what `probe` answers in
/// `anchors` and on disk ([`cache_looked_up_anchor`]). A failed lookup, or a
/// login replaced since `stores` was read, leaves the profile uncached; a
/// [`BareItemLookupPending`] stops the walk and passes through for the
/// caller to retry. Answers each cached profile with the
/// [`crate::profile::ProfileId`] it held as its anchor was cached, for
/// [`bare_item_owner_account`].
fn bare_item_fill_anchors(
    stores: &[(ProfileName, serde_json::Value)],
    anchors: &mut [(ProfileName, Option<crate::profile::AccountId>)],
    probe: &mut impl FnMut(&str) -> Result<crate::profile::AccountId>,
) -> Result<Vec<(ProfileName, crate::profile::ProfileId)>> {
    let mut filled = Vec::new();
    for (name, store) in stores {
        let Some(access) = login_access_token(store) else {
            continue;
        };
        if bare_item_expired(store) || !bare_item_uncached_holder(name, store, anchors) {
            continue;
        }
        let id = match probe(access) {
            Ok(id) if !id.trim().is_empty() => id,
            Ok(_) => continue,
            Err(e) if is_bare_item_lookup_pending(&e) => return Err(e),
            Err(_) => continue,
        };
        let Some(account) = cache_looked_up_anchor(name, access, &id) else {
            continue;
        };
        for (anchored, anchor) in anchors.iter_mut() {
            if anchored == name {
                *anchor = Some(id.clone());
            }
        }
        filled.push((name.clone(), account));
    }
    Ok(filled)
}

/// Write `id`, the account `/profile` answered for `access`, as `name`'s
/// identity anchor under the state flock, and only while `access` is still
/// one of `name`'s stored logins: a re-login since the lookup must never
/// inherit the old login's account. The same hold claims `name`'s
/// [`crate::profile::ProfileId`], so the anchor and the account a carry lands
/// by are one profile's even if `name` is recreated before the carry. `None`
/// when it wrote nothing, a failed claim and a refused or timed-out flock
/// wait included.
fn cache_looked_up_anchor(
    name: &ProfileName,
    access: &str,
    id: &str,
) -> Option<crate::profile::ProfileId> {
    with_state_lock(|held| {
        let still_held = bare_item_stores(std::slice::from_ref(name))
            .iter()
            .any(|(_, store)| login_access_token(store) == Some(access));
        if !still_held {
            return Ok(None);
        }
        let account = crate::profile::claim_profile_id(name)?;
        crate::usage::reconcile_identity_anchor_to_held(name, id, held);
        Ok(Some(account))
    })
    .ok()
    .flatten()
}

/// The [`crate::profile::ProfileId`] a carry into `owner` lands by: the one
/// `owner` held when the fill cached its anchor, since the fill's later
/// lookups give a delete and recreate time to land; else the one it holds
/// now, since no lookup ran after its anchor was read.
#[cfg_attr(
    not(target_os = "macos"),
    allow(
        dead_code,
        reason = "the callers are the macOS item writers and daemon leg; the rule is pinned on every platform"
    )
)]
fn bare_item_owner_account(
    owner: &ProfileName,
    filled: &[(ProfileName, crate::profile::ProfileId)],
) -> Result<crate::profile::ProfileId> {
    match filled.iter().find(|(name, _)| name == owner) {
        Some((_, account)) => Ok(account.clone()),
        None => crate::profile::claim_profile_id(owner),
    }
}

thread_local! {
    /// The key of the bare-item login an entry point's settle found safe to
    /// replace, for every writer guard that entry then runs on this thread
    /// ([`BareItemPlacement`]). The writers sit under shared switch, sign-out
    /// and relink code with many callers that never settle; this hands the
    /// verdict down without threading it through each of them.
    static BARE_ITEM_PLACED: std::cell::Cell<Option<[u8; 32]>> = const { std::cell::Cell::new(None) };
}

/// While alive, every [`bare_item_may_be_replaced`] on this thread also lets
/// the writer replace the login `placed` names: the item an entry point's
/// [`settle_bare_item_before_write`] judged before it took any lock. Restores
/// the enclosing placement on drop, a panic unwind included.
#[cfg_attr(
    not(target_os = "macos"),
    allow(
        dead_code,
        reason = "the entry points settle the macOS Keychain item only"
    )
)]
pub(crate) struct BareItemPlacement {
    prior: Option<[u8; 32]>,
}

#[cfg_attr(
    not(target_os = "macos"),
    allow(
        dead_code,
        reason = "the entry points settle the macOS Keychain item only"
    )
)]
impl BareItemPlacement {
    pub(crate) fn enter(placed: Option<[u8; 32]>) -> Self {
        Self {
            prior: BARE_ITEM_PLACED.replace(placed),
        }
    }
}

impl Drop for BareItemPlacement {
    fn drop(&mut self) {
        BARE_ITEM_PLACED.set(self.prior);
    }
}

/// Whether an item writer may replace the bare item it just read: `false`
/// only when the item carries a refresh token no store holds, no spent record
/// names, and neither the caller (`placed`, the
/// [`crate::usage::identity_key`] of the item's access token) nor the
/// enclosing [`BareItemPlacement`] settled. An item with no refresh token has
/// nothing single-use to strand. Logs the unclaimed line on `false`. Callers
/// hold the state flock: this reads store files only.
#[cfg_attr(
    not(target_os = "macos"),
    allow(
        dead_code,
        reason = "the callers are the macOS item writers and daemon leg; the rule is pinned on every platform"
    )
)]
pub(crate) fn bare_item_may_be_replaced(
    item: &serde_json::Value,
    placed: Option<&[u8; 32]>,
) -> bool {
    let (Some(access), Some(refresh)) = (login_access_token(item), login_refresh_token(item))
    else {
        return true;
    };
    let key = crate::usage::identity_key(access);
    let roster = bare_item_roster();
    let held = bare_item_token_holders(item, &bare_item_stores(&roster))
        .is_some_and(|holders| !holders.is_empty());
    if held
        || bare_item_refresh_spent(&roster, refresh)
        || placed == Some(&key)
        || BARE_ITEM_PLACED.get() == Some(key)
    {
        return true;
    }
    log_bare_item_unclaimed();
    false
}

/// Save the bare item's login into `owner`'s `credentials.json` under one
/// state-flock hold, only while `owner` still holds `account`, the
/// [`crate::profile::ProfileId`] claimed when the owner was named: the
/// bare-item twin of [`carry_session_item_into`]'s store side, its gates in
/// its order (an equal login, [`item_login_outranks_store`], the spent
/// record), behind the stand-down every other writer of that file keeps for
/// a rotation staged (`credentials.json.staged`) or pending (`.pending`)
/// beside it: a stage's minted pair is lost once its base changes under it.
/// Writes the login alone: the item is whatever account's Claude Code last
/// wrote it, so the keys beside its login may be another account's, and the
/// store keeps its own. Takes the item as already read, so it spends no
/// `security` call.
#[cfg_attr(
    not(target_os = "macos"),
    allow(
        dead_code,
        reason = "the callers are the macOS item writers and daemon leg; the rule is pinned on every platform"
    )
)]
pub(crate) fn carry_bare_item_login_into(
    owner: &ProfileName,
    account: &crate::profile::ProfileId,
    item: &serde_json::Value,
) -> Result<BareItemCarry> {
    let Ok(parsed) = serde_json::from_value::<ClaudeCredentials>(item.clone()) else {
        return Ok(BareItemCarry::Refused);
    };
    if parsed.refresh_token().is_none_or(str::is_empty) {
        return Ok(BareItemCarry::Refused);
    }
    let store = &profile_dir(owner)?.join("credentials.json");
    with_state_lock(|held| {
        crate::profile::require_profile_id(owner, account, held)?;
        if crate::profile::rotation_staged_beside(store)
            || store
                .with_file_name(crate::oauth::STAGE_FILE)
                .symlink_metadata()
                .is_ok()
        {
            return Ok(BareItemCarry::Refused);
        }
        let store_value = std::fs::read(store)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok());
        let held_login = store_value.as_ref().and_then(|v| v.get("claudeAiOauth"));
        if held_login.is_some() && held_login == item.get("claudeAiOauth") {
            return Ok(BareItemCarry::Saved);
        }
        if !item_login_outranks_store(item, store_value.as_ref()) {
            return Ok(BareItemCarry::Kept);
        }
        match crate::oauth::session_login_copy(store, &parsed) {
            crate::oauth::SessionLoginCopy::Copyable => {}
            crate::oauth::SessionLoginCopy::Spent => return Ok(BareItemCarry::Kept),
            crate::oauth::SessionLoginCopy::Unknown => return Ok(BareItemCarry::Refused),
        }
        let store_creds =
            store_value.and_then(|v| serde_json::from_value::<ClaudeCredentials>(v).ok());
        let bytes = crate::profile::serialize_credentials_preserving_extra(&parsed, store)?;
        atomic_write_600(store, bytes)
            .with_context(|| format!("failed to write {}", store.display()))?;
        if crate::profile::login_changed(store_creds.as_ref(), &parsed) {
            crate::usage::mark_profile_read_stale(store);
        }
        Ok(BareItemCarry::Saved)
    })
}

/// Which writer settles the bare item before it writes: its refusals name
/// the operation they stopped.
#[cfg_attr(
    not(target_os = "macos"),
    allow(
        dead_code,
        reason = "the callers are the macOS item writers; the rule is pinned on every platform"
    )
)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BareItemWriter {
    /// A switch, and the guarded relink's own refusal (a rename, a first
    /// capture, a boot reconcile).
    Switch,
    /// A switch-off's sign-out.
    SignOut,
    /// The startup relink of the active account.
    Relink,
}

impl BareItemWriter {
    /// The operation a refusal says it stopped.
    fn verb(self) -> &'static str {
        match self {
            Self::Switch => "switching",
            Self::SignOut => "signing out",
            Self::Relink => "relinking",
        }
    }

    /// The operation a prompt offers to go on with.
    pub(crate) fn action(self) -> &'static str {
        match self {
            Self::Switch => "switch",
            Self::SignOut => "sign out",
            Self::Relink => "relink",
        }
    }

    /// A refusal's retry advice: a startup relink retries on its own, with
    /// no switch to run again.
    fn retry_tail(self, when: &str) -> String {
        match self {
            Self::Switch | Self::SignOut => format!("; run the switch again {when}"),
            Self::Relink => String::new(),
        }
    }
}

/// What [`settle_bare_item_before_write`] leaves the writer: a line to show
/// the user, and the key of an item it may replace though no store holds it
/// ([`bare_item_may_be_replaced`]'s `placed`).
#[cfg_attr(
    not(target_os = "macos"),
    allow(
        dead_code,
        reason = "the callers are the macOS item writers; the rule is pinned on every platform"
    )
)]
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct BareItemSettled {
    pub(crate) notice: Option<String>,
    pub(crate) placed: Option<[u8; 32]>,
}

/// What [`settle_bare_item_before_write`] decided: the writer may go on, or
/// the item holds a login no store saved that the settle cannot place, so
/// only the user can say what happens to it.
#[cfg_attr(
    not(target_os = "macos"),
    allow(
        dead_code,
        reason = "the callers are the macOS item writers; the rule is pinned on every platform"
    )
)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BareItemSettle {
    Settled(BareItemSettled),
    NeedsChoice(BareItemUnplaced),
}

/// Why the settle could not place an unsaved bare-item login.
#[cfg_attr(
    not(target_os = "macos"),
    allow(
        dead_code,
        reason = "the callers are the macOS item writers; the rule is pinned on every platform"
    )
)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BareItemUnplaced {
    /// A login-holding profile has no cached account id, so it may be the
    /// owner.
    NotCached,
    /// The first two profiles, in roster order, cached with the item's
    /// account id.
    SharedBy(ProfileName, ProfileName),
    /// Every login-holding profile is cached, none with the item's account.
    UnknownAccount,
    /// The access token expired by its own clock, or `/profile` refused it
    /// (401): the owner cannot be looked up until Claude Code refreshes it.
    Expired,
}

#[cfg_attr(
    not(target_os = "macos"),
    allow(
        dead_code,
        reason = "the callers are the macOS item writers; the rule is pinned on every platform"
    )
)]
impl BareItemUnplaced {
    pub(crate) fn reason(&self) -> String {
        match self {
            Self::NotCached => "its account's id is not cached yet".to_string(),
            Self::SharedBy(first, second) => {
                format!("two profiles share its account ({first}, {second})")
            }
            Self::Expired => "its access token expired or was revoked".to_string(),
            Self::UnknownAccount => "no clauth account is logged in to its account".to_string(),
        }
    }

    /// The line a prompt shows under its choices, if any: a refreshed login
    /// can be looked up, an expired one cannot.
    pub(crate) fn tip(&self) -> Option<&'static str> {
        match self {
            Self::Expired => {
                Some("tip: start `claude` once so it refreshes the login, then run this again")
            }
            Self::NotCached | Self::SharedBy(..) | Self::UnknownAccount => None,
        }
    }

    pub(crate) fn question(&self) -> String {
        format!(
            "the macOS Keychain holds a login no clauth account has saved ({}); what should \
             clauth do with it?",
            self.reason()
        )
    }

    /// The refusal of a caller with nobody to ask.
    pub(crate) fn refusal(&self, writer: BareItemWriter) -> anyhow::Error {
        anyhow::anyhow!(
            "the macOS Keychain holds a login no clauth account has saved ({}); not {}, so that \
             login is not lost; save it first with:  clauth capture <name>",
            self.reason(),
            writer.verb()
        )
    }
}

/// The bare item's login, alone, when it carries a refresh token no store
/// holds and no spent record names (an expired one included): the login a
/// capture saves. The keys beside it may be another account's, so they stay
/// out.
#[cfg_attr(
    not(target_os = "macos"),
    allow(
        dead_code,
        reason = "the callers are the macOS capture and item writers; the rule is pinned on every platform"
    )
)]
pub(crate) fn unsaved_bare_item_login(item: &serde_json::Value) -> Option<ClaudeCredentials> {
    let refresh = login_refresh_token(item)?;
    let roster = bare_item_roster();
    let held = !bare_item_token_holders(item, &bare_item_stores(&roster))?.is_empty();
    if held || bare_item_refresh_spent(&roster, refresh) {
        return None;
    }
    let login = serde_json::json!({ "claudeAiOauth": item.get("claudeAiOauth")? });
    serde_json::from_value(login).ok()
}

/// Whether the item's access token expired by its own `expiresAt` (epoch
/// ms). An absent or unparseable one reads as live, so the probe decides.
fn bare_item_expired(item: &serde_json::Value) -> bool {
    let Some(expires_at) = item
        .get("claudeAiOauth")
        .and_then(|login| login.get("expiresAt"))
        .and_then(serde_json::Value::as_i64)
    else {
        return false;
    };
    i64::try_from(crate::usage::now_ms()).is_ok_and(|now| expires_at <= now)
}

/// The `/profile` lookup of an off-loop settle is still running on its
/// worker: the caller retries later, and the next attempt settles off the
/// memoized answer. Never shown: the drain that gets it defers quietly.
#[cfg_attr(
    not(target_os = "macos"),
    allow(
        dead_code,
        reason = "the caller is the macOS daemon drain; the rule is pinned on every platform"
    )
)]
#[derive(Debug)]
pub(crate) struct BareItemLookupPending;

impl std::fmt::Display for BareItemLookupPending {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the macOS Keychain login's owner lookup is still running")
    }
}

impl std::error::Error for BareItemLookupPending {}

/// Whether a settle error is [`BareItemLookupPending`].
#[cfg_attr(
    not(target_os = "macos"),
    allow(
        dead_code,
        reason = "the caller is the macOS daemon drain; the rule is pinned on every platform"
    )
)]
pub(crate) fn is_bare_item_lookup_pending(e: &anyhow::Error) -> bool {
    e.is::<BareItemLookupPending>()
}

/// Save an unsaved bare-item login into its owner before a switch, sign-out
/// or guarded relink writes over it. The caller holds no state flock: `probe`
/// is the `/profile` lookup (HTTP), and the carry takes its own short hold.
///
/// An item no store needs to save first (no refresh token, a spent one, one
/// a store holds) settles with no probe. An expired one asks with no probe,
/// as does one `/profile` refuses (401: revoked, or a clock behind the
/// server's). Otherwise the probe names the owner: its store takes the login,
/// or already holds a later one (the writer may then replace the item,
/// `placed`); a store mid-rotation, or a failed lookup, refuses the write so
/// the login is not lost. A login-holding profile with no cached account id
/// may be the owner, so before asking, the probe looks up each such
/// profile's own unexpired access token and caches the id it answers. With
/// no single owner the settle asks ([`BareItemSettle::NeedsChoice`]):
/// replacing the item could kill the account it belongs to. A probe
/// answering [`BareItemLookupPending`] passes through as itself.
#[cfg_attr(
    not(target_os = "macos"),
    allow(
        dead_code,
        reason = "the callers are the macOS item writers; the rule is pinned on every platform"
    )
)]
pub(crate) fn settle_bare_item_before_write(
    item: &serde_json::Value,
    writer: BareItemWriter,
    mut probe: impl FnMut(&str) -> Result<crate::profile::AccountId>,
) -> Result<BareItemSettle> {
    let settled = |settled: BareItemSettled| Ok(BareItemSettle::Settled(settled));
    let (Some(access), Some(refresh)) = (login_access_token(item), login_refresh_token(item))
    else {
        return settled(BareItemSettled::default());
    };
    let roster = bare_item_roster();
    let stores = bare_item_stores(&roster);
    let held = bare_item_token_holders(item, &stores).is_some_and(|holders| !holders.is_empty());
    if held || bare_item_refresh_spent(&roster, refresh) {
        return settled(BareItemSettled::default());
    }
    if bare_item_expired(item) {
        return Ok(BareItemSettle::NeedsChoice(BareItemUnplaced::Expired));
    }
    let id = match probe(access) {
        Ok(id) => id,
        Err(e) if is_bare_item_lookup_pending(&e) => return Err(e),
        Err(e) if e.is::<crate::usage::ProfileUnauthorized>() => {
            return Ok(BareItemSettle::NeedsChoice(BareItemUnplaced::Expired));
        }
        Err(e) => return Err(bare_item_unplaced_refusal(writer, format!("{e:#}"))),
    };
    let key = crate::usage::identity_key(access);
    let mut anchors = bare_item_anchors(&roster);
    let filled = if bare_item_claimants(&id, &anchors).next().is_none() {
        bare_item_fill_anchors(&stores, &mut anchors, &mut probe)?
    } else {
        Vec::new()
    };
    let mut claimants = bare_item_claimants(&id, &anchors);
    let owner = match (claimants.next(), claimants.next()) {
        (Some(owner), None) => owner,
        (Some(first), Some(second)) => {
            return Ok(BareItemSettle::NeedsChoice(BareItemUnplaced::SharedBy(
                first.clone(),
                second.clone(),
            )));
        }
        (None, _) => {
            let uncached = stores
                .iter()
                .any(|(name, store)| bare_item_uncached_holder(name, store, &anchors));
            return Ok(BareItemSettle::NeedsChoice(if uncached {
                BareItemUnplaced::NotCached
            } else {
                BareItemUnplaced::UnknownAccount
            }));
        }
    };
    let carried = bare_item_owner_account(owner, &filled)
        .and_then(|account| carry_bare_item_login_into(owner, &account, item))
        .with_context(|| format!("saving the macOS Keychain login into {owner}"))?;
    match carried {
        BareItemCarry::Saved => settled(BareItemSettled {
            notice: Some(bare_item_saved_line(owner.as_str())),
            placed: None,
        }),
        BareItemCarry::Kept => settled(BareItemSettled {
            notice: None,
            placed: Some(key),
        }),
        BareItemCarry::Refused => Err(bare_item_owner_refusal(owner, writer)),
    }
}

/// The refusal of a writer whose item belongs to `owner`, whose store cannot
/// take the login now.
fn bare_item_owner_refusal(owner: &ProfileName, writer: BareItemWriter) -> anyhow::Error {
    anyhow::anyhow!(
        "the macOS Keychain holds {owner}'s login, but {owner}'s saved login is mid-rotation or \
         unreadable; not {}, so that login is not lost{}",
        writer.verb(),
        writer.retry_tail("in a minute")
    )
}

/// What the save a user picked for an unsaved bare-item login does: carry it
/// into the profile already cached with its account, or capture it into a
/// new profile, with the account id the lookup proved for it, if any.
#[cfg_attr(
    not(target_os = "macos"),
    allow(
        dead_code,
        reason = "the callers are the macOS prompt and TUI modal; the rule is pinned on every platform"
    )
)]
#[derive(Debug, Clone)]
pub(crate) enum BareItemSave {
    /// Nothing to save: the item's login is saved, spent, or gone.
    Nothing,
    /// The one profile cached with the login's account.
    Owner(ProfileName),
    /// The first two profiles, in roster order, cached with the login's
    /// account: the user picks which one takes it, since the login may be
    /// either one's chain.
    Owners(ProfileName, ProfileName),
    /// No profile is cached with the login's account: a new one takes it.
    NewProfile {
        login: ClaudeCredentials,
        account: Option<crate::profile::AccountId>,
    },
}

/// [`BareItemSave`] for `item`. The account id comes from the probe memo
/// alone (a token's account never changes), so this spends no HTTP: an item
/// never probed, or whose probe failed, saves into a new unproven profile.
#[cfg_attr(
    not(target_os = "macos"),
    allow(
        dead_code,
        reason = "the callers are the macOS prompt and TUI modal; the rule is pinned on every platform"
    )
)]
pub(crate) fn bare_item_save(item: &serde_json::Value) -> BareItemSave {
    let (Some(login), Some(access)) = (unsaved_bare_item_login(item), login_access_token(item))
    else {
        return BareItemSave::Nothing;
    };
    let account = match bare_item_probe_memo(&crate::usage::identity_key(access)) {
        Some(Ok(id)) => Some(id),
        Some(Err(_)) | None => None,
    };
    if let Some(id) = &account {
        let anchors = bare_item_anchors(&bare_item_roster());
        let mut claimants = bare_item_claimants(id, &anchors);
        match (claimants.next(), claimants.next()) {
            (Some(owner), None) => return BareItemSave::Owner(owner.clone()),
            (Some(first), Some(second)) => {
                return BareItemSave::Owners(first.clone(), second.clone());
            }
            (None, _) => {}
        }
    }
    BareItemSave::NewProfile { login, account }
}

/// Carry the item's login into `owner` for [`BareItemSave::Owner`], or the
/// one the user picked of [`BareItemSave::Owners`], while `owner` still holds
/// `account`, the [`crate::profile::ProfileId`] claimed when the save named
/// it: a store that already holds a later login keeps it, and one that
/// cannot take it refuses as the settle would. A kept store answers the key
/// the writer may replace the item by, as the settle's own carry does (the
/// item strands nothing), since settling again cannot place an item two
/// profiles claim; a saved one answers `None`, and the settle again finds the
/// item held.
#[cfg_attr(
    not(target_os = "macos"),
    allow(
        dead_code,
        reason = "the callers are the macOS prompt and TUI modal; the rule is pinned on every platform"
    )
)]
pub(crate) fn save_bare_item_into(
    owner: &ProfileName,
    account: &crate::profile::ProfileId,
    item: &serde_json::Value,
    writer: BareItemWriter,
) -> Result<Option<[u8; 32]>> {
    let carried = carry_bare_item_login_into(owner, account, item)
        .with_context(|| format!("saving the macOS Keychain login into {owner}"))?;
    match carried {
        BareItemCarry::Saved => Ok(None),
        BareItemCarry::Kept => Ok(bare_item_login_key(item)),
        BareItemCarry::Refused => Err(bare_item_owner_refusal(owner, writer)),
    }
}

/// The [`crate::profile::ProfileId`] a bare-item save into `owner` lands by,
/// claimed as the save names `owner`, before any wait.
pub(crate) fn claim_bare_item_owner(owner: &ProfileName) -> Result<crate::profile::ProfileId> {
    crate::profile::claim_profile_id(owner)
        .with_context(|| format!("saving the macOS Keychain login into {owner}"))
}

/// The [`crate::usage::identity_key`] of the item's access token: which
/// login the item holds, for a writer guard or a question about it.
pub(crate) fn bare_item_login_key(item: &serde_json::Value) -> Option<[u8; 32]> {
    login_access_token(item).map(crate::usage::identity_key)
}

/// The key a writer guard needs to replace `item` once the user chose to
/// ([`BareItemPlacement::enter`]); `None` when the item holds nothing to
/// strand.
#[cfg_attr(
    not(target_os = "macos"),
    allow(
        dead_code,
        reason = "the callers are the macOS prompt and TUI modal; the rule is pinned on every platform"
    )
)]
pub(crate) fn bare_item_replacement(item: &serde_json::Value) -> Option<[u8; 32]> {
    unsaved_bare_item_login(item)?;
    bare_item_login_key(item)
}

/// The settle without HTTP, for a caller that must not wait on the network
/// (the daemon's run loop): `/profile` answers come from the probe memo, and
/// a token with none kicks `kick` (a detached lookup that fills the memo)
/// and answers [`BareItemLookupPending`] for the caller to retry. A memoized
/// failure refuses as a failed lookup does.
#[cfg_attr(
    not(target_os = "macos"),
    allow(
        dead_code,
        reason = "the caller is the macOS daemon drain; the rule is pinned on every platform"
    )
)]
pub(crate) fn settle_bare_item_off_loop(
    item: &serde_json::Value,
    writer: BareItemWriter,
    mut kick: impl FnMut(&str),
) -> Result<BareItemSettle> {
    settle_bare_item_before_write(item, writer, |access| {
        match bare_item_probe_memo(&crate::usage::identity_key(access)) {
            Some(Ok(id)) => Ok(id),
            Some(Err(failure)) => Err(failure.into_error()),
            None => {
                kick(access);
                Err(BareItemLookupPending.into())
            }
        }
    })
}

/// How long a found account id for a bare-item token is kept for
/// [`settle_bare_item_off_loop`] and [`bare_item_save`]: the leg's window. An
/// account id never changes for a token.
const BARE_ITEM_PROBE_MEMO_TTL_MS: u64 = BARE_ITEM_PROBE_SUPPRESS_TTL_MS;

/// How long a failed lookup is kept: long enough that the daemon's drain,
/// retrying every few seconds, refuses off it instead of kicking a lookup per
/// attempt, short enough that one transient failure (a network blip, a 5xx)
/// is looked up again inside the drain's retry window rather than blocking
/// every auto-switch for the found-id window.
const BARE_ITEM_PROBE_FAILURE_TTL_MS: u64 = 30 * 1000;

/// A memoized failed `/profile` lookup: its cause, and whether the endpoint
/// refused the token (401), which the settle asks about rather than refuses.
#[derive(Debug, Clone, PartialEq, Eq)]
struct BareItemProbeFailure {
    cause: String,
    unauthorized: bool,
}

impl BareItemProbeFailure {
    fn of(e: &anyhow::Error) -> Self {
        Self {
            cause: format!("{e:#}"),
            unauthorized: e.is::<crate::usage::ProfileUnauthorized>(),
        }
    }

    fn into_error(self) -> anyhow::Error {
        if self.unauthorized {
            crate::usage::ProfileUnauthorized.into()
        } else {
            anyhow::anyhow!("{}", self.cause)
        }
    }
}

/// A memoized `/profile` answer: the account id, or the failure.
type BareItemProbed = std::result::Result<crate::profile::AccountId, BareItemProbeFailure>;

/// Per item access-token hash, the epoch-ms its answer expires at, then the
/// answer.
type BareItemProbeMemo = HashMap<[u8; 32], (u64, BareItemProbed)>;

/// [`BareItemProbeMemo`] for the process. Same leaf rank as the oauth probe
/// memos; never held with another lock.
static BARE_ITEM_PROBED: LazyLock<RankedMutex<BareItemProbeMemo, rank::IdentityMemo>> =
    LazyLock::new(|| RankedMutex::new(HashMap::new()));

fn bare_item_probe_memo(key: &[u8; 32]) -> Option<BareItemProbed> {
    let now = crate::usage::now_ms();
    let probed = BARE_ITEM_PROBED.lock().ok()?;
    probed
        .get(key)
        .filter(|(until, _)| now < *until)
        .map(|(_, answer)| answer.clone())
}

fn memoize_bare_item_probe(key: [u8; 32], answer: BareItemProbed) {
    let now = crate::usage::now_ms();
    let ttl = if answer.is_ok() {
        BARE_ITEM_PROBE_MEMO_TTL_MS
    } else {
        BARE_ITEM_PROBE_FAILURE_TTL_MS
    };
    if let Ok(mut probed) = BARE_ITEM_PROBED.lock() {
        probed.retain(|_, (until, _)| now < *until);
        probed.insert(key, (now + ttl, answer));
    }
}

/// The `/profile` lookup for a bare-item token, recorded in the probe memo.
/// A remembered account id is reused (it cannot change); a remembered
/// failure is tried again, since a user retrying asks for a fresh lookup.
#[cfg_attr(
    not(target_os = "macos"),
    allow(
        dead_code,
        reason = "the callers are the macOS item writers; the rule is pinned on every platform"
    )
)]
fn probe_bare_item_memoized(
    access: &str,
    probe: impl FnOnce(&str) -> Result<crate::profile::AccountId>,
) -> Result<crate::profile::AccountId> {
    let key = crate::usage::identity_key(access);
    if let Some(Ok(id)) = bare_item_probe_memo(&key) {
        return Ok(id);
    }
    let answer = probe(access);
    memoize_bare_item_probe(
        key,
        match &answer {
            Ok(id) => Ok(id.clone()),
            Err(e) => Err(BareItemProbeFailure::of(e)),
        },
    );
    answer
}

/// How long a bare-item login the leg could not place waits before the leg
/// spends another `/profile` probe or warns about it again: the
/// `STORED_PROBE_SUPPRESS_TTL_MS` window, for the same reason (a token that
/// proved nothing once proves nothing every 30 s).
const BARE_ITEM_PROBE_SUPPRESS_TTL_MS: u64 = 15 * 60 * 1000;

/// Per item access-token hash, the earliest epoch-ms the leg may handle that
/// login again. Same leaf rank as the oauth probe memos; never held with
/// another lock.
static BARE_ITEM_PROBE_SUPPRESSED: LazyLock<
    RankedMutex<HashMap<[u8; 32], u64>, rank::IdentityMemo>,
> = LazyLock::new(|| RankedMutex::new(HashMap::new()));

fn bare_item_probe_due(key: &[u8; 32]) -> bool {
    let now = crate::usage::now_ms();
    let Ok(suppressed) = BARE_ITEM_PROBE_SUPPRESSED.lock() else {
        return true;
    };
    suppressed
        .get(key)
        .is_none_or(|not_before| now >= *not_before)
}

fn suppress_bare_item_probe(key: &[u8; 32]) {
    if let Ok(mut suppressed) = BARE_ITEM_PROBE_SUPPRESSED.lock() {
        suppressed.insert(
            *key,
            crate::usage::now_ms() + BARE_ITEM_PROBE_SUPPRESS_TTL_MS,
        );
    }
}

/// The daemon leg over the bare item, as already read: save another account's
/// login into its owner, then put `active`'s back through `restore` (which
/// signs the item out for a keyless or absent `active`); adopt the active
/// account's own successor into its store and restore nothing; restore over
/// a login a store already holds or the server already consumed; with no
/// single owner, write nothing and say so. A login-holding profile with no
/// cached account id may be the owner, so before calling the item
/// unclaimed the leg looks up each such profile's own unexpired access token
/// as the settle does ([`bare_item_fill_anchors`]). `probe` resolves an access token
/// to its account id (`usage::probe_account_uuid`); `restore` writes the
/// active login into the item, handed the key of an item it may replace
/// though no store holds it ([`bare_item_may_be_replaced`]), and reports
/// whether it landed.
///
/// One token is handled at most once per [`BARE_ITEM_PROBE_SUPPRESS_TTL_MS`]
/// unless its item was put right: every arm that leaves it as it was spends
/// the window, so a standing item costs one probe and one line per window.
///
/// Runs with no state flock held: the probe is HTTP, and each store write
/// takes its own short hold ([`carry_bare_item_login_into`]).
#[cfg_attr(
    not(target_os = "macos"),
    allow(
        dead_code,
        reason = "the callers are the macOS item writers and daemon leg; the rule is pinned on every platform"
    )
)]
pub(crate) fn correct_bare_item(
    item: &serde_json::Value,
    active: Option<&ProfileName>,
    mut probe: impl FnMut(&str) -> Result<crate::profile::AccountId>,
    restore: impl Fn(Option<&[u8; 32]>) -> Result<bool>,
) {
    let role = crate::logline::role(crate::logline::stamped());
    let Some(access) = login_access_token(item) else {
        return;
    };
    let roster = bare_item_roster();
    let stores = bare_item_stores(&roster);
    let login = classify_bare_item_login(item, active, &stores);
    if matches!(login, BareItemLogin::Inert | BareItemLogin::ActiveOwn) {
        return;
    }
    let key = crate::usage::identity_key(access);
    if !bare_item_probe_due(&key) {
        return;
    }
    let tail = bare_item_restore_tail(active);
    let putting_back = active.filter(|active| bare_item_active_has_login(active));
    let put_back = |placed: Option<&[u8; 32]>| {
        let restored = match putting_back {
            Some(active) => restore(placed)
                .with_context(|| format!("putting {active}'s login back into the macOS Keychain")),
            None => restore(placed),
        };
        match restored {
            Ok(true) => return true,
            Ok(false) => {}
            Err(e) => logline!("{role}: {e:#}"),
        }
        suppress_bare_item_probe(&key);
        false
    };
    let left_as_saved = |owner: &ProfileName| {
        logline!(
            "{role}: the macOS Keychain held {owner}'s login, which {owner} already has saved or \
             has replaced with a newer one; {tail}"
        );
    };
    let leave = |line: &str| {
        suppress_bare_item_probe(&key);
        logline!("{role}: {line}");
    };
    if let BareItemLogin::Foreign { owner } = &login {
        if put_back(None) {
            left_as_saved(owner);
        }
        return;
    }
    let holders = bare_item_token_holders(item, &stores).unwrap_or_default();
    // Two stores hold it: it is saved, though neither can be told apart as
    // its home, so nothing is carried and the restore strands nothing.
    if holders.len() > 1 {
        put_back(None);
        return;
    }
    // A mint no store holds carries nothing single-use to strand.
    let Some(refresh) = login_refresh_token(item) else {
        return;
    };
    if bare_item_refresh_spent(&roster, refresh) {
        if put_back(None) {
            logline!(
                "{role}: the macOS Keychain held a login that no longer works (its token was \
                 already rotated); {tail}"
            );
        }
        return;
    }
    // `/profile` answers 401 for an expired access token for good, and the
    // login may be its account's only live one: Claude Code's next refresh
    // makes it lookable again.
    if bare_item_expired(item) {
        return leave(
            "the macOS Keychain holds an expired login no clauth account has saved; left it in \
             place until Claude Code refreshes it",
        );
    }
    let id = match probe(access) {
        Ok(id) => id,
        Err(e) => {
            return leave(&format!(
                "the macOS Keychain holds a login no clauth account has saved, and clauth could \
                 not look up its owner ({e:#}); left it in place, trying again in 15 minutes"
            ));
        }
    };
    let mut anchors = bare_item_anchors(&roster);
    let filled = if bare_item_claimants(&id, &anchors).next().is_none() {
        // The leg's lookup runs inline and never answers pending, the fill's
        // only error; a holder whose lookup failed stays uncached.
        bare_item_fill_anchors(&stores, &mut anchors, &mut probe).unwrap_or_default()
    } else {
        Vec::new()
    };
    let probed = profile_for_account_id(&id, &anchors);
    let (owner, adopt) = match bare_item_correction(&login, probed, active) {
        BareItemCorrection::Rescue { owner } => (owner, false),
        BareItemCorrection::AdoptIntoActive => match active {
            Some(active) => (active.clone(), true),
            None => return leave(BARE_ITEM_UNCLAIMED_LINE),
        },
        BareItemCorrection::Nothing | BareItemCorrection::Warn => {
            return leave(BARE_ITEM_UNCLAIMED_LINE);
        }
    };
    let carried = bare_item_owner_account(&owner, &filled)
        .and_then(|account| carry_bare_item_login_into(&owner, &account, item))
        .with_context(|| format!("saving the macOS Keychain login into {owner}"));
    match (carried, adopt) {
        (Err(e), _) => leave(&format!("{e:#}")),
        (Ok(BareItemCarry::Refused), _) => leave(&bare_item_owner_busy_line(owner.as_str())),
        (Ok(BareItemCarry::Saved), true) => {
            logline!(
                "{role}: the macOS Keychain holds {owner}'s own login, refreshed by Claude Code; \
                 saved it into {owner}"
            );
        }
        (Ok(BareItemCarry::Kept), true) => {
            if put_back(Some(&key)) {
                logline!(
                    "{role}: the macOS Keychain held an older login of {owner}'s; put its \
                     current one back"
                );
            }
        }
        (Ok(BareItemCarry::Saved), false) => match putting_back {
            Some(active) if put_back(None) => {
                logline!(
                    "{role}: {}",
                    foreign_item_rescued_line(owner.as_str(), active.as_str())
                );
            }
            None if put_back(None) => {
                logline!(
                    "{role}: {} and {tail}",
                    bare_item_saved_line(owner.as_str())
                );
            }
            // The save landed though the restore did not: say so on its own.
            Some(_) | None => logline!("{role}: {}", bare_item_saved_line(owner.as_str())),
        },
        (Ok(BareItemCarry::Kept), false) => {
            if put_back(Some(&key)) {
                left_as_saved(&owner);
            }
        }
    }
}

/// macOS: the bare item as a writer is about to find it, read outside any
/// state-flock hold under its own `security` budget; `None` for no item, or
/// for a read that failed (a locked keychain, a spent budget), where the
/// writer keeps its own handling of an unreadable item.
#[cfg(target_os = "macos")]
pub(crate) fn read_bare_keychain_item() -> Option<serde_json::Value> {
    crate::lock::with_keychain_budget(crate::lock::SUBPROCESS_BUDGET, || {
        crate::keychain::read_blob_for_service(CLAUDE_KEYCHAIN_SERVICE)
    })
    .ok()
    .flatten()
}

/// macOS: [`settle_bare_item_before_write`] over the bare item as it is now,
/// for a switch, sign-out or guarded-relink entry point to run before it
/// takes the config guard or the state flock. Hand a settled `placed` to the
/// write through [`BareItemPlacement::enter`] and show its `notice`; a
/// [`BareItemSettle::NeedsChoice`] is the caller's to ask about, or to refuse
/// with [`BareItemUnplaced::refusal`] where nobody can answer.
#[cfg(target_os = "macos")]
pub(crate) fn settle_bare_keychain_item(writer: BareItemWriter) -> Result<BareItemSettle> {
    let settled = Ok(BareItemSettle::Settled(BareItemSettled::default()));
    if !crate::keychain::enabled() {
        return settled;
    }
    let Some(item) = read_bare_keychain_item() else {
        return settled;
    };
    settle_bare_item_before_write(&item, writer, |access| {
        probe_bare_item_memoized(access, crate::usage::probe_account_uuid)
    })
}

/// Whether a drain may spend its item read out of the Keychain budget its
/// tick armed (`left`, `None` when none is armed): only while one `security`
/// call (`per_call`) still fits. The install mirror's own clamp refuses a
/// window the read spent.
#[cfg_attr(
    not(target_os = "macos"),
    allow(
        dead_code,
        reason = "the caller is the macOS daemon drain; the rule is pinned on every platform"
    )
)]
pub(crate) fn bare_item_read_affordable(
    left: Option<std::time::Duration>,
    per_call: std::time::Duration,
) -> bool {
    left.is_none_or(|left| left >= per_call)
}

/// macOS: [`settle_bare_item_off_loop`] over the bare item as it is now, for
/// the daemon's run loop: its `/profile` lookup runs on
/// [`probe_bare_item_detached`]'s worker, and until it answers this fails
/// with [`BareItemLookupPending`], as it does without reading the item while
/// the tick's Keychain budget cannot afford the read
/// ([`bare_item_read_affordable`]). With nobody to ask, an item it cannot
/// place refuses ([`BareItemUnplaced::refusal`]).
#[cfg(target_os = "macos")]
pub(crate) fn settle_bare_keychain_item_off_loop(
    writer: BareItemWriter,
) -> Result<BareItemSettled> {
    if !crate::keychain::enabled() {
        return Ok(BareItemSettled::default());
    }
    if !bare_item_read_affordable(
        crate::lock::armed_budget_remaining(),
        crate::keychain::SECURITY_TIMEOUT,
    ) {
        return Err(BareItemLookupPending.into());
    }
    let Some(item) = read_bare_keychain_item() else {
        return Ok(BareItemSettled::default());
    };
    match settle_bare_item_off_loop(&item, writer, probe_bare_item_detached)? {
        BareItemSettle::Settled(settled) => Ok(settled),
        BareItemSettle::NeedsChoice(unplaced) => Err(unplaced.refusal(writer)),
    }
}

/// Clears its in-flight flag when a detached worker ends, a panic or a
/// failed spawn included, so one bad run never shuts the latch for good.
#[cfg(target_os = "macos")]
struct InFlight(&'static std::sync::atomic::AtomicBool);

#[cfg(target_os = "macos")]
impl Drop for InFlight {
    fn drop(&mut self) {
        self.0.store(false, std::sync::atomic::Ordering::Release);
    }
}

/// Set while a [`probe_bare_item_detached`] worker runs.
#[cfg(target_os = "macos")]
static BARE_ITEM_PROBE_IN_FLIGHT: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// macOS: the `/profile` lookup for `access` on a named detached worker,
/// its answer recorded in the probe memo, so the daemon's run loop never
/// waits on the request throttle or the HTTP deadline. A lookup still
/// running is left to finish: the caller retries off the memo.
#[cfg(target_os = "macos")]
fn probe_bare_item_detached(access: &str) {
    use std::sync::atomic::Ordering;

    if BARE_ITEM_PROBE_IN_FLIGHT.swap(true, Ordering::AcqRel) {
        return;
    }
    let claim = InFlight(&BARE_ITEM_PROBE_IN_FLIGHT);
    let access = access.to_string();
    let spawned = std::thread::Builder::new()
        .name("clauth-bare-item-probe".to_string())
        .spawn(move || {
            let _claim = claim;
            let _ = probe_bare_item_memoized(&access, crate::usage::probe_account_uuid);
        });
    if let Err(e) = spawned {
        logline!(
            "{}: {}",
            crate::logline::role(crate::logline::stamped()),
            bare_item_probe_unstarted_line(&e)
        );
    }
}

/// The owner lookup's line when its worker thread cannot start.
#[cfg_attr(
    not(target_os = "macos"),
    allow(
        dead_code,
        reason = "the caller is the macOS owner lookup; the text is pinned on every platform"
    )
)]
fn bare_item_probe_unstarted_line(e: &std::io::Error) -> String {
    format!("could not start the macOS Keychain owner lookup: {e}")
}

/// macOS: the daemon's ~30 s leg over the bare item ([`correct_bare_item`]):
/// read it fresh, outside any state-flock hold, against the active profile
/// on disk now. A read that fails (a locked keychain, a spent budget) skips
/// the leg until the next one. Runs on [`correct_bare_keychain_item_detached`]'s
/// worker.
#[cfg(target_os = "macos")]
fn correct_bare_keychain_item() {
    if !crate::keychain::enabled() {
        return;
    }
    let Ok(state) = crate::profile::load_app_state() else {
        return;
    };
    let active = state.active_profile;
    let Some(item) = read_bare_keychain_item() else {
        return;
    };
    correct_bare_item(
        &item,
        active.as_ref(),
        |access| probe_bare_item_memoized(access, crate::usage::probe_account_uuid),
        |placed| restore_bare_item(active.as_ref(), placed),
    );
}

/// Set while a [`correct_bare_keychain_item_detached`] worker runs.
#[cfg(target_os = "macos")]
static BARE_ITEM_LEG_IN_FLIGHT: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// macOS: [`correct_bare_keychain_item`] on a named detached worker, so its
/// blocking calls (the `security` read, the `/profile` probe behind the
/// shared request throttle, the store and item writes) never hold the
/// daemon's run loop past its watchdog. A leg still running when the next is
/// due is left to finish: the next one starts on a later tick.
#[cfg(target_os = "macos")]
pub(crate) fn correct_bare_keychain_item_detached() {
    use std::sync::atomic::Ordering;

    if !crate::keychain::enabled() || BARE_ITEM_LEG_IN_FLIGHT.swap(true, Ordering::AcqRel) {
        return;
    }
    let claim = InFlight(&BARE_ITEM_LEG_IN_FLIGHT);
    let spawned = std::thread::Builder::new()
        .name("clauth-bare-keychain-item".to_string())
        .spawn(move || {
            let _claim = claim;
            correct_bare_keychain_item();
        });
    if let Err(e) = spawned {
        logline!(
            "{}: {}",
            crate::logline::role(crate::logline::stamped()),
            bare_item_leg_unstarted_line(&e)
        );
    }
}

/// The daemon leg's line when its worker thread cannot start.
#[cfg_attr(
    not(target_os = "macos"),
    allow(
        dead_code,
        reason = "the caller is the macOS daemon leg; the text is pinned on every platform"
    )
)]
fn bare_item_leg_unstarted_line(e: &impl std::fmt::Display) -> String {
    format!("could not start the macOS Keychain check: {e}")
}

/// macOS: put `active`'s login back into the bare item, or sign the item out
/// when `active` stores none or there is no active profile, skipped when a
/// switch landed since the leg read the active profile. A rotation staged
/// for `active` lands first, under its rotation guard, so the item never
/// takes a pair whose refresh token was already sent.
///
/// The `security` read, write and verify run inside the state-flock hold,
/// like a switch's: the hold is what orders this write against a switch's,
/// so a peer waits up to the subprocess budget on a stuck keychain, as it
/// does behind a switch.
#[cfg(target_os = "macos")]
fn restore_bare_item(active: Option<&ProfileName>, placed: Option<&[u8; 32]>) -> Result<bool> {
    // Any store file, a torn one included, may have a stage beside it that
    // must land before the mirror reads the store.
    let _rotation = match active {
        Some(active) if install_source_path(active).is_ok_and(|path| path.exists()) => {
            let guard = crate::runtime::RotationGuard::acquire(active)?;
            crate::oauth::land_stage_for_install(active, &guard)
                .map_err(|refused| anyhow::anyhow!("{}", refused.text_with_status()))?;
            Some(guard)
        }
        Some(_) | None => None,
    };
    with_state_lock(|_held| {
        let state = crate::profile::load_app_state()?;
        if state.active_profile.as_ref() != active {
            return Ok(false);
        }
        let preserve = preserve_non_login_keys();
        let Some(active) = active else {
            let refused = std::cell::Cell::new(false);
            let may_replace = |item: &serde_json::Value| {
                let may = bare_item_may_be_replaced(item, placed);
                refused.set(!may);
                may
            };
            let signed_out = crate::keychain::keychain_sign_out(preserve, may_replace, |item| {
                preserve && sync_item_into_its_store(item, None)
            })?;
            return Ok(signed_out && !refused.get());
        };
        let target = install_source_path(active)?;
        let linkable = linkable_install_source(&target);
        // `SignOut`: an active profile storing no login leaves the item
        // serving nobody, never the account it held.
        keychain_mirror_source(&target, linkable, AbsentSource::SignOut, preserve, placed)
            .map(|written| written == ItemWrite::Landed)
    })
}

/// The Keychain service name Claude Code reads its OAuth login from on macOS:
/// the bare `Claude Code-credentials` item a global `claude` resolves, because
/// its config dir IS `~/.claude`. The macOS keychain module aliases this as its
/// `SERVICE` constant; the literal lives here so the pure naming rules beside
/// it — which derive and recognize the per-config-dir twins from this name —
/// stay cross-platform, where they can be pinned (the
/// `item_login_outranks_store` split: the module that shells out against
/// these names compiles on macOS alone).
pub(crate) const CLAUDE_KEYCHAIN_SERVICE: &str = "Claude Code-credentials";

/// The namespaced Keychain service Claude Code derives for a config dir it
/// runs under (`CLAUDE_CONFIG_DIR`): [`CLAUDE_KEYCHAIN_SERVICE`], a `-`, then
/// the first 8 hex chars of the SHA-256 of the dir's path — CC's own
/// `sha256(configDir).toString('hex').slice(0, 8)`, which is why a `clauth
/// start` session's CC reads this twin and never the bare item. The session
/// seed, the swap executor and the stale-runtime GC all derive the same name
/// for one runtime tree through this rule.
///
/// PURE over its input — no canonicalize, no subprocess — so the naming rule
/// is pinned on every platform; the macOS derivation is canonicalize plus
/// this fn, and its subprocess callers are unreachable under `cfg(test)`.
#[cfg_attr(
    not(target_os = "macos"),
    allow(
        dead_code,
        reason = "the only production caller is the macOS Keychain layer; the naming rule is pinned on every platform"
    )
)]
pub(crate) fn namespaced_keychain_service(canonical_config_dir: &Path) -> String {
    let path_str = canonical_config_dir.to_string_lossy();
    let hash = Sha256::digest(path_str.as_bytes());
    format!(
        "{CLAUDE_KEYCHAIN_SERVICE}-{:02x}{:02x}{:02x}{:02x}",
        hash[0], hash[1], hash[2], hash[3]
    )
}

/// Whether `service` names a per-config-dir twin: the bare service, a `-`,
/// then EXACTLY eight LOWERCASE hex digits — the only shape
/// [`namespaced_keychain_service`] produces (node's `toString('hex')` and
/// Rust's `{:02x}` are both lowercase-only). The stale-runtime GC's delete
/// refuses everything else, the bare item itself (the operator's global
/// `claude` login, which no GC decision explains) most importantly, so a
/// caller handing over a name it derived nowhere cannot reach an item it
/// cannot account for. PURE so the guard is pinned on every platform.
#[cfg_attr(
    not(target_os = "macos"),
    allow(
        dead_code,
        reason = "the only production caller is the macOS Keychain layer; the guard is pinned on every platform"
    )
)]
pub(crate) fn is_namespaced_keychain_service(service: &str) -> bool {
    let Some(suffix) = service.strip_prefix(CLAUDE_KEYCHAIN_SERVICE) else {
        return false;
    };
    let Some(hex) = suffix.strip_prefix('-') else {
        return false;
    };
    hex.len() == 8 && hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// The census decision over one `security dump-keychain` text: the NAMESPACED
/// services it lists that clauth's durable ownership ledger authorizes and no
/// existing runtime dir explains. Shape is only a parser guard: a foreign
/// `CLAUDE_CONFIG_DIR` item never enters the ledger or live set and is therefore
/// left untouched. `owned` is loaded fail-closed by the caller; an unreadable or
/// malformed ledger reaches no decision at all.
///
/// The only lines it reads are the generic-password service attribute, the
/// shape `security dump-keychain` prints as `0x00000007 <blob>="<name>"` for a
/// printable value and `0x00000007 <blob>=0x<hex>  "<name>"` when the tool
/// adds the hex form; the first quoted span is the service either way. A
/// `<NULL>` value, the account attribute (`0x00000008`) and every other line
/// contribute nothing. PURE over text so the decision is pinned on every
/// platform; the `security` I/O it feeds is macOS-only
/// (`keychain::census_namespaced_items`).
#[cfg_attr(
    not(target_os = "macos"),
    allow(
        dead_code,
        reason = "the only production caller is the macOS Keychain census; the decision is pinned on every platform"
    )
)]
pub(crate) fn census_orphan_keychain_services(
    dump: &str,
    live: &BTreeSet<String>,
    owned: &BTreeSet<String>,
) -> Vec<String> {
    let mut orphans = BTreeSet::new();
    for line in dump.lines() {
        let Some(value) = line.trim_start().strip_prefix("0x00000007 <blob>=") else {
            continue;
        };
        let Some(service) = value
            .split_once('"')
            .and_then(|(_, rest)| rest.split_once('"').map(|(name, _)| name))
        else {
            continue;
        };
        if is_namespaced_keychain_service(service)
            && owned.contains(service)
            && !live.contains(service)
        {
            orphans.insert(service.to_string());
        }
    }
    orphans.into_iter().collect()
}

/// What a salvage-then-delete namespaced collection observed about the item's
/// bytes, so the collector names the salvage on its event line without
/// fabricating one. Carries no raw bytes: the preserved path alone, the error
/// rendered, never the payload.
#[derive(Debug)]
pub(crate) enum SalvageOutcome {
    /// The item was already gone when the salvage read ran.
    Absent,
    /// The raw bytes were quarantined first; the path names the file.
    Preserved(PathBuf),
    /// The salvage read or quarantine failed — a refused or prompted read, an
    /// unwritable quarantine — named, never read as success.
    Failed(anyhow::Error),
}

/// The shared destructive-collection core for a namespaced Keychain item:
/// salvage any readable bytes through the caller's `quarantine`, then delete —
/// in that order, never the reverse — and report what the salvage saw. The
/// delete lands whatever the salvage did: the salvage preserves evidence, it
/// does not refuse the collection, and a failed one is named rather than
/// fabricated as success. `read`, `quarantine` and `delete` are injected so the
/// ORDER is pinned on every platform while the real `/usr/bin/security` legs
/// compile on macOS alone (the `census_orphan_keychain_services` split).
/// Guarded on the service SHAPE like every delete-by-name in this family, so a
/// caller passing a name it derived nowhere cannot reach an item it cannot
/// account for.
#[cfg_attr(
    not(target_os = "macos"),
    allow(
        dead_code,
        reason = "the real legs are the macOS Keychain collectors; the ordering decision is pinned on every platform"
    )
)]
pub(crate) fn salvage_delete_namespaced_item_with(
    service: &str,
    account: &str,
    read: impl FnOnce(&str, &str) -> Result<Option<String>>,
    quarantine: impl FnOnce(&str, &str) -> Result<PathBuf>,
    delete: impl FnOnce(&str, &str) -> Result<()>,
) -> Result<SalvageOutcome> {
    anyhow::ensure!(
        is_namespaced_keychain_service(service),
        "refusing to delete Keychain item `{service}` through the salvage path: it is not a \
         per-config-dir item name"
    );
    let salvage = match read(service, account) {
        Ok(Some(raw)) => match quarantine(service, &raw) {
            Ok(path) => SalvageOutcome::Preserved(path),
            Err(e) => SalvageOutcome::Failed(e),
        },
        Ok(None) => SalvageOutcome::Absent,
        Err(e) => SalvageOutcome::Failed(e),
    };
    delete(service, account)?;
    Ok(salvage)
}

/// How an event line names what the salvage observed, shared by both
/// destructive collectors so their wording cannot drift apart.
#[cfg_attr(
    not(target_os = "macos"),
    allow(
        dead_code,
        reason = "the real legs are the macOS Keychain collectors; the operator-facing wording is pinned on every platform"
    )
)]
pub(crate) fn salvage_tail(outcome: &SalvageOutcome) -> String {
    match outcome {
        SalvageOutcome::Absent => "the item was already absent when the salvage read it".into(),
        SalvageOutcome::Preserved(path) => {
            format!("its raw bytes were preserved first at {}", path.display())
        }
        SalvageOutcome::Failed(e) => format!(
            "its raw bytes could not be preserved first ({e:#}); the Keychain may have refused the \
             read or required an interaction prompt"
        ),
    }
}

/// How an event line names the ownership-row retirement after a collected
/// delete, shared by both destructive collectors.
#[cfg_attr(
    not(target_os = "macos"),
    allow(
        dead_code,
        reason = "the real legs are the macOS Keychain collectors; the operator-facing wording is pinned on every platform"
    )
)]
pub(crate) fn retirement_tail(retirement: &Result<()>) -> String {
    match retirement {
        Ok(()) => "the ownership row was retired".to_string(),
        Err(e) => format!(
            "retiring the ownership row failed ({e:#}); it stays ledgered for a later census"
        ),
    }
}

/// How an event line names the in-flight record's clearing after a collected
/// delete, shared by both destructive collectors so their wording cannot
/// drift apart.
#[cfg_attr(
    not(target_os = "macos"),
    allow(
        dead_code,
        reason = "the real legs are the macOS Keychain collectors; the operator-facing wording is pinned on every platform"
    )
)]
pub(crate) fn in_flight_tail(cleared: &Result<()>) -> String {
    match cleared {
        Ok(()) => "its in-flight record was cleared".to_string(),
        Err(e) => format!(
            "clearing its in-flight record failed ({e:#}); the staleness sweep removes it, \
             refusing same-service writes until it does"
        ),
    }
}

/// What a `security(1)` exit status means, as far as this codebase has
/// measured. The OSStatus rides the exit status as its low byte
/// (`osstatus & 0xFF`: −25300 → 44, measured on the read leg; −25308 → 36;
/// −25293 → 51), so the classification is over the code the tool reports.
///
/// PURE and cross-platform — pinned here, consumed by the macOS module that
/// shells out (the `namespaced_keychain_service` split) — so every surface
/// that renders or acts on a `security` failure shares one vocabulary instead
/// of each re-deriving a cause from a bare exit number.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SecurityExitClass {
    /// Exit 36, `errSecInteractionNotAllowed` (−25308): the keychain is locked
    /// or the context cannot show an interaction prompt — over ssh a gated
    /// operation is REFUSED, never prompted. The one TRANSIENT class: it clears
    /// the moment the keychain is unlocked.
    InteractionNotAllowed,
    /// Exit 44, `errSecItemNotFound` (−25300): no item matches — the read
    /// leg's existing "absent" tolerance, `EXIT_ITEM_NOT_FOUND` in the macOS
    /// module.
    ItemNotFound,
    /// Every other code, 51 (`errSecAuthFailed`, the write-suppression
    /// fixture) included: transient-or-not is unmeasured, so no cause is
    /// claimed and no special handling is taken.
    Unclassified,
}

/// See [`SecurityExitClass`]. PURE so the table is pinned on every platform.
#[cfg_attr(
    not(target_os = "macos"),
    allow(
        dead_code,
        reason = "the only production caller is the macOS error builder; the table is pinned on every platform"
    )
)]
pub(crate) fn classify_security_exit(code: i32) -> SecurityExitClass {
    match code {
        36 => SecurityExitClass::InteractionNotAllowed,
        44 => SecurityExitClass::ItemNotFound,
        _ => SecurityExitClass::Unclassified,
    }
}

impl SecurityExitClass {
    /// The cause line this class renders on a failure surface: a HARDCODED
    /// literal keyed on the classified code, never the tool's stderr — the
    /// write arm must keep its suppression (a write's stderr can echo the
    /// value being written), so the one code whose diagnostic lives in
    /// exactly those withheld bytes gets its cause named here instead. `None`
    /// for the classes no surface has a measured cause for.
    #[cfg_attr(
        not(target_os = "macos"),
        allow(
            dead_code,
            reason = "the only production caller is the macOS error builder; the literal is pinned on every platform"
        )
    )]
    pub(crate) fn cause(self) -> Option<&'static str> {
        match self {
            SecurityExitClass::InteractionNotAllowed => Some(
                "the keychain is locked or cannot show a prompt (errSecInteractionNotAllowed). It \
                 clears once the keychain is unlocked. Retry once it has",
            ),
            SecurityExitClass::ItemNotFound | SecurityExitClass::Unclassified => None,
        }
    }
}

/// Whether the sign-out's failed-read branch may take its destructive arm —
/// deleting the item whole. A locked keychain's read says nothing about the
/// item's bytes, so treating it as empty destroyed a live login in the field
/// (2026-09-12: an ssh session's exit 36 reached the delete); the classified
/// transient skips the delete instead and says so on the event line. The skip
/// is the recoverable side — a re-run of the switch or clear signs out for
/// real once the keychain unlocks — where the delete is not. Every other
/// failed read keeps the documented degrade: delete, with the bytes
/// quarantined whenever the read brought any back. PURE so the skip rule is
/// pinned on every platform; `sign_out_at` itself is macOS-only.
#[cfg_attr(
    not(target_os = "macos"),
    allow(
        dead_code,
        reason = "the only production caller is the macOS sign-out; the skip rule is pinned on every platform"
    )
)]
pub(crate) fn failed_read_degrades_to_delete(class: SecurityExitClass) -> bool {
    !matches!(class, SecurityExitClass::InteractionNotAllowed)
}

/// Typed check at the boundary, then hand the untyped object to the installer:
/// the login must be PRESENT and parse as a login, or CC is handed a credential
/// it cannot read and nothing here would have said so. Presence is its own
/// clause because the field is an `Option`, so `{}` and a store holding only
/// `mcpOAuth` parse clean. The Value is what gets written, since the typed
/// shape models the login alone and would drop the store's siblings.
#[cfg(target_os = "macos")]
fn checked_store_at(path: &Path) -> Result<serde_json::Value> {
    let store: serde_json::Value = read_json_file(path)?;
    let parsed = serde_json::from_value::<ClaudeCredentials>(store.clone()).map_err(|e| {
        crate::format::parse_refusal(
            format!(
                "install source is not Claude credentials: {}",
                path.display()
            ),
            crate::format::json_parse_position(&e),
        )
    })?;
    anyhow::ensure!(
        parsed.claude_ai_oauth.is_some(),
        "refusing to install a credential store that holds no login: {}",
        path.display()
    );
    Ok(store)
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

/// Point `link` at `target` without ever leaving the path absent: stage the
/// pointer at a staging sibling, then rename it over whatever is there.
///
/// The unlink-then-create this replaced left `~/.claude/.credentials.json`
/// missing whenever the create failed after the unlink had landed, which is
/// reachable on Windows: [`create_symlink`] falls back to copying `target`
/// there, and a copy loses to anyone holding `target` open for writing. Staging
/// first moves that failure ahead of the swap, where it costs nothing.
pub(crate) fn publish_credential_link(link: &Path, target: &Path) -> Result<()> {
    if let Some(parent) = link.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = crate::profile::tmp_sibling(link);
    let _ = std::fs::remove_file(&tmp);
    create_symlink(target, &tmp)?;
    let mut renamed = std::fs::rename(&tmp, link);
    if renamed.is_err() && clear_readonly(link) {
        renamed = std::fs::rename(&tmp, link);
    }
    match renamed {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e).with_context(|| format!("failed to publish {}", link.display()))
        }
    }
}

/// Drop a read-only attribute from `path`, reporting whether it dropped one.
///
/// Windows only. It keeps [`publish_credential_link`] from refusing a switch
/// that the unlink-then-create before it would have completed: a read-only
/// destination refuses `rename` there with os error 5, while `remove_file`
/// clears the attribute on its way past. Nothing clauth writes is read-only, so
/// the operator or a backup tool is what sets it.
#[cfg(windows)]
#[expect(
    clippy::permissions_set_readonly_false,
    reason = "the lint is about unix, where clearing readonly grants group and other write; this arm is windows-only, where it clears one file attribute"
)]
fn clear_readonly(path: &Path) -> bool {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return false;
    };
    let mut perms = meta.permissions();
    if !perms.readonly() {
        return false;
    }
    perms.set_readonly(false);
    std::fs::set_permissions(path, perms).is_ok()
}

#[cfg(not(windows))]
fn clear_readonly(_path: &Path) -> bool {
    false
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
/// other side of the line [`CARRIED_CREDENTIAL_KEYS`] draws. What a sign-out
/// drops with `preserve_non_login_keys` off; on, it drops
/// [`LOGIN_CREDENTIAL_KEYS`].
const ACCOUNT_SCOPED_CREDENTIAL_KEYS: [&str; 5] = [
    "claudeAiOauth",
    "organizationUuid",
    "trustedDeviceToken",
    "enterpriseGateway",
    "designOauth",
];

/// The login and the keys that authenticate or identify its account: what
/// `preserve_non_login_keys` never preserves, because they are the login's own
/// and are only right beside the exact login they came with. Every other key is
/// a non-login key. `enterpriseGateway` is a credential of its own (a gateway
/// JWT beside an IdP refresh token, read out of Claude Code 2.1.283's gateway
/// refresh and logout paths), so a store keeping it after a sign-out would go on
/// authenticating the departed account through the gateway;
/// `organizationUuid` and `trustedDeviceToken` name that account and its device
/// trust, so one kept beside a later login of another account is a mixed
/// identity that never self-corrects. Preservation therefore never syncs, keeps
/// or leaves these behind: a store gains them only with the login they belong
/// to. The one key [`ACCOUNT_SCOPED_CREDENTIAL_KEYS`] adds is `designOauth`,
/// Claude Design's own login, which serves no inference and is the key
/// preservation exists for. Ceiling: a key Claude Code adds later that
/// identifies the account is preserved like any other until it is named here.
const LOGIN_CREDENTIAL_KEYS: [&str; 4] = [
    "claudeAiOauth",
    "organizationUuid",
    "trustedDeviceToken",
    "enterpriseGateway",
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
/// With `keep_non_login` only [`LOGIN_CREDENTIAL_KEYS`] go, so a design login
/// stays in the store and a store holding it is written back rather than
/// deleted: on macOS the Keychain item can be that login's only copy.
///
/// `keep_non_login` is the caller's decision for this store, not the
/// `preserve_non_login_keys` reading: [`sign_out_keeps_non_login_keys`] makes
/// it, true only with the setting on and no store of the account having taken
/// the keys. Passing the setting alone would leave a design login in the item
/// that its account's store also holds, two copies of one rotating chain. With
/// the decision true it holds for every sign-out, not only a switch onto an
/// account that stores no login: a wrap-off, the Setup tab's `log out` on the
/// active account, deleting the active profile, and clearing a long-lived token
/// onto an account that stores no login keep it too.
/// A blob that is not an object carries nothing worth preserving.
#[cfg_attr(
    not(target_os = "macos"),
    allow(
        dead_code,
        reason = "the only caller is the macOS Keychain sign-out; the rule is pinned on every platform"
    )
)]
pub(crate) fn strip_account_credentials(
    blob: &mut serde_json::Value,
    keep_non_login: bool,
) -> SignOut {
    let Some(obj) = blob.as_object_mut() else {
        return SignOut::Delete;
    };
    let dropped_keys: &[&str] = if keep_non_login {
        &LOGIN_CREDENTIAL_KEYS
    } else {
        &ACCOUNT_SCOPED_CREDENTIAL_KEYS
    };
    let mut dropped = false;
    for key in dropped_keys {
        dropped |= obj.remove(*key).is_some();
    }
    match (dropped, obj.is_empty()) {
        (_, true) => SignOut::Delete,
        (true, false) => SignOut::Write,
        (false, false) => SignOut::Nothing,
    }
}

/// Whether a sign-out keeps the design login in the store it strips: with
/// `preserve_non_login_keys` on, unless its account's own store already took
/// it (`saved_elsewhere`), which leaves one copy of a rotating chain rather
/// than two. PURE, so the rule is pinned on every platform; the macOS Keychain
/// sign-out is its caller.
#[cfg_attr(
    not(target_os = "macos"),
    allow(
        dead_code,
        reason = "the only caller is the macOS Keychain sign-out; the rule is pinned on every platform"
    )
)]
pub(crate) fn sign_out_keeps_non_login_keys(preserve: bool, saved_elsewhere: bool) -> bool {
    preserve && !saved_elsewhere
}

/// The [`CARRIED_CREDENTIAL_KEYS`] a per-session Keychain item holds, for the
/// refreshless swap hand-off: `None` when it holds none or is not an object,
/// so the hand-off deletes the item with nothing to save. PURE so the rule is
/// pinned on every platform; the macOS hand-off is its caller.
#[cfg_attr(
    not(target_os = "macos"),
    allow(
        dead_code,
        reason = "the only caller is the macOS refreshless swap hand-off; the rule is pinned on every platform"
    )
)]
pub(crate) fn carried_keys_of(
    item: &serde_json::Value,
) -> Option<serde_json::Map<String, serde_json::Value>> {
    let obj = item.as_object()?;
    let carried: serde_json::Map<String, serde_json::Value> = CARRIED_CREDENTIAL_KEYS
        .iter()
        .filter_map(|key| Some(((*key).to_string(), obj.get(*key)?.clone())))
        .collect();
    (!carried.is_empty()).then_some(carried)
}

/// Merge `carried` (from [`carried_keys_of`]) into `store`, reporting whether
/// anything changed, by the same rule every switch carry uses
/// ([`copy_non_login_keys`] through [`fresher_copy`]): per MCP server, a
/// server only one side holds is kept and for one both hold the copy that
/// expires later stays, so another session's fresher refresh in the shared
/// store is never rewound by this item's older copy. The login is never
/// touched. A store that is not an object is refused: there is no credential
/// store to hand the session over to.
#[cfg_attr(
    not(target_os = "macos"),
    allow(
        dead_code,
        reason = "the only caller is the macOS refreshless swap hand-off; the rule is pinned on every platform"
    )
)]
pub(crate) fn merge_carried_into_store(
    store: &mut serde_json::Value,
    carried: &serde_json::Map<String, serde_json::Value>,
) -> Result<bool> {
    if !store.is_object() {
        anyhow::bail!("the store is not a JSON object");
    }
    Ok(copy_non_login_keys(
        store,
        &serde_json::Value::Object(carried.clone()),
    ))
}

/// What the refreshless swap hand-off does with the per-session item once the
/// carry into the incoming member's store has run (or had nothing to carry).
/// Either way the item goes: Claude Code resolves the item before any file, so
/// an item left holding only `mcpOAuth` reads as "Not logged in" and the
/// session never reaches the store its `.credentials.json` now points at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(
    not(target_os = "macos"),
    allow(
        dead_code,
        reason = "the only consumer is the macOS refreshless swap hand-off; the rule is pinned on every platform"
    )
)]
pub(crate) enum HandOff {
    /// The store holds what the item carried, or it carried nothing: delete.
    Delete,
    /// The carry failed: quarantine the item's bytes, then delete, so the MCP
    /// logins stay recoverable without stranding the session.
    QuarantineThenDelete,
}

/// See [`HandOff`]. PURE so the rule is pinned on every platform.
#[cfg_attr(
    not(target_os = "macos"),
    allow(
        dead_code,
        reason = "the only caller is the macOS refreshless swap hand-off; the rule is pinned on every platform"
    )
)]
pub(crate) fn hand_off_after_carry(carry_landed: bool) -> HandOff {
    if carry_landed {
        HandOff::Delete
    } else {
        HandOff::QuarantineThenDelete
    }
}

/// macOS: write [`carried_keys_of`] an item into `store`, the incoming
/// member's install source the session's `.credentials.json` now points at,
/// merged by [`merge_carried_into_store`]. Read and write share ONE state-flock
/// hold, the way [`carry_session_item_into`] writes a store, so a re-stamp
/// landing between them cannot be reverted. An absent or unparseable store
/// refuses: the caller then quarantines the item rather than lose what it
/// carried.
#[cfg(target_os = "macos")]
pub(crate) fn carry_item_keys_into_store(
    store: &Path,
    carried: &serde_json::Map<String, serde_json::Value>,
) -> Result<()> {
    crate::lock::with_state_lock(|_held| {
        let bytes =
            std::fs::read(store).with_context(|| format!("failed to read {}", store.display()))?;
        let mut value: serde_json::Value = serde_json::from_slice(&bytes)
            .with_context(|| format!("{} is not valid JSON", store.display()))?;
        if !merge_carried_into_store(&mut value, carried)
            .with_context(|| format!("{} is not a credential store", store.display()))?
        {
            return Ok(());
        }
        let out = serde_json::to_vec_pretty(&value).context("failed to serialize the store")?;
        crate::profile::atomic_write_600(store, out)
            .with_context(|| format!("failed to write {}", store.display()))
    })
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
/// A no-op when either file is unreadable. With `preserve_non_login_keys` off
/// also a no-op when `target` is the static-token sidecar: every sidecar writer
/// then rebuilds that file from its token alone, so a block carried there is
/// dropped at the next re-mint and sits on disk for nothing until then. With it
/// on the writers keep it ([`keep_non_login_in_sidecar`]), and the carry has to
/// reach the sidecar: the per-account sync puts the outgoing account's MCP
/// logins into its own sidecar, and without the carry refreshing them there a
/// switch back would put a copy live whose refresh token another account's
/// session has since spent.
fn carry_live_extra_into(
    link: &Path,
    target: &Path,
    name: &ProfileName,
    preserve: bool,
) -> Result<()> {
    if !preserve
        && target
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

/// Run [`carry_live_extra_into`] for a switch onto `name`, reporting a failure
/// instead of raising it. Preserving MCP logins is a convenience; completing the
/// switch is not, so an unwritable profile directory must not strand the
/// operator on the outgoing account.
fn carry_live_extra_best_effort(link: &Path, target: &Path, name: &ProfileName, preserve: bool) {
    if let Err(e) = carry_live_extra_into(link, target, name, preserve) {
        logline!(
            "clauth: switched to '{name}' but could not carry its MCP server logins: {e:#}. \
             Re-authenticate any MCP server that reports a signed-out session"
        );
    }
}

/// Merge [`CARRIED_CREDENTIAL_KEYS`] from the credential at `link` into the store
/// at `target` before a relink installs it, reporting whether it wrote. The
/// fresher copy stays per key and per server ([`fresher_copy`]): a key only
/// `link` holds is added, one it lacks is left as `target` has it, nothing is
/// deleted, and the login is never touched.
///
/// [`carry_live_extra_into`]'s sibling for a live file that is no account's login
/// (a static token or a logged-out shell a session saved over the link): its MCP
/// logins belong to no Claude account, so they are kept, but an older copy never
/// lands over the store's newer one, whose refresh token a peer session may
/// already have spent. Only the carried keys cross: the file can be a stale
/// row's copy of another account, and a key that authenticates or names one
/// beside the store's login would carry that account's identity in.
///
/// A no-op when either side is absent or unreadable ([`read_store_for_merge`]),
/// into a `session-token.json` target with `preserve_non_login_keys` off (every
/// sidecar writer then rebuilds that file from its token alone), and while a
/// rotation is staged beside the target ([`crate::profile::rotation_staged_beside`]),
/// whose staged pair the write would discard.
///
/// A carried key lands only as a JSON object: a null or any other value says
/// nothing about the servers, so it carries nothing. Where both sides hold an
/// object, the store's copy stays unless the file's is strictly fresher — the
/// file's expiry later than the store's, or present where the store has none —
/// since the file sat unheld while peer sessions wrote the store through their
/// links. A tie, or no expiry on either side, keeps the store's, and a server
/// only one side holds survives either way.
fn carry_live_extra_fresher_into(link: &Path, target: &Path, preserve: bool) -> Result<bool> {
    if !preserve
        && target
            .file_name()
            .is_some_and(|n| n == "session-token.json")
    {
        return Ok(false);
    }
    let purpose = "this relink goes ahead without the MCP logins the saved login may hold";
    let (Some(live), Some(mut stored)) = (
        read_store_for_merge(link, purpose),
        read_store_for_merge(target, purpose),
    ) else {
        return Ok(false);
    };
    let (Some(live_obj), Some(stored_obj)) = (live.as_object(), stored.as_object_mut()) else {
        return Ok(false);
    };
    let mut changed = false;
    for key in CARRIED_CREDENTIAL_KEYS {
        if let Some(value) = live_obj.get(key).filter(|value| value.is_object()) {
            changed |= merge_fresher_key(stored_obj, key, value, false);
        }
    }
    if !changed {
        return Ok(false);
    }
    if crate::profile::rotation_staged_beside(target) {
        let purpose = "the shell's MCP logins were not merged into its store, and the relink that \
             follows takes the shell's file away";
        logline!(
            "clauth: a rotation is staged beside {}, so {purpose}",
            target.display()
        );
        return Ok(false);
    }
    atomic_write_600(target, serde_json::to_string_pretty(&stored)?)
        .context("failed to merge the session's MCP server logins into its store")?;
    Ok(true)
}

/// Run [`carry_live_extra_fresher_into`], reporting a failure instead of raising
/// it: keeping MCP logins is a convenience, and the relink it rides on is not.
pub(crate) fn carry_live_extra_fresher_best_effort(link: &Path, target: &Path, preserve: bool) {
    if let Err(e) = carry_live_extra_fresher_into(link, target, preserve) {
        logline!(
            "clauth: could not merge the session's MCP server logins into {}: {e:#}. \
             Re-authenticate any MCP server that reports a signed-out session",
            target.display()
        );
    }
}

/// Whether every key of a credential store other than the login stays with the
/// account whose store it is (`preserve_non_login_keys` in profiles.toml,
/// default on). Read off disk at the moment it matters, the way every persist
/// leg re-reads the record, so a daemon holding a config from before an edit
/// acts on what the file says now. A profiles.toml that does not load reads ON:
/// a broken hand-edit then keeps keys rather than stripping them, on every side
/// that reads this (the sidecar writers, the syncs, the Keychain sign-out).
pub(crate) fn preserve_non_login_keys() -> bool {
    crate::profile::load_app_state().map_or(true, |state| state.preserve_non_login_keys)
}

/// The non-empty access token of `store`'s login, the line [`classify_link_at`]
/// and the link guard draw: two blank logins are two logged-out shells, never
/// the same account.
fn login_access_token(store: &serde_json::Value) -> Option<&str> {
    store
        .get("claudeAiOauth")?
        .get("accessToken")?
        .as_str()
        .filter(|token| !token.is_empty())
}

/// Read a credential store for a merge, `None` when there is nothing to merge
/// with. An absent file is silent; one that is present but unreadable or not
/// JSON says so on the event line, `purpose` naming what the skip costs, so a
/// merge skipped over it does not let its keys die unlogged with the next write.
fn read_store_for_merge(path: &Path, purpose: &str) -> Option<serde_json::Value> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => {
            logline!(
                "clauth: could not read {} ({e}), so {purpose}",
                path.display()
            );
            return None;
        }
    };
    match serde_json::from_slice(&bytes) {
        Ok(value) => Some(value),
        Err(e) => {
            let frame = crate::format::parse_refusal(
                format!("{} is not valid JSON", path.display()),
                crate::format::json_parse_position(&e),
            )
            .to_string();
            logline!("clauth: {frame}, so {purpose}");
            None
        }
    }
}

/// Copy every non-login key of `source` onto `target` (every key but
/// [`LOGIN_CREDENTIAL_KEYS`]), reporting whether anything changed. A key only
/// `source` holds is added; one it lacks is left as `target` has it; where both
/// hold one, [`fresher_copy`] decides what stays. A refresh moves a login's
/// expiry forward, so an older copy (a live file a `clauth start` session's
/// refresh has since overtaken, a backup's) never lands over a newer one, whose
/// refresh token the older one's may already have been spent for. Nothing is
/// ever deleted, and the login and its identity keys are never touched, on
/// either side.
fn copy_non_login_keys(target: &mut serde_json::Value, source: &serde_json::Value) -> bool {
    let (Some(target), Some(source)) = (target.as_object_mut(), source.as_object()) else {
        return false;
    };
    let mut changed = false;
    for (key, value) in source {
        if LOGIN_CREDENTIAL_KEYS.contains(&key.as_str()) {
            continue;
        }
        changed |= merge_fresher_key(target, key, value, true);
    }
    changed
}

/// Merge one key of `source` into `target`, [`fresher_copy`] deciding where both
/// hold a copy and `source`'s landing whole where only it does, reporting whether
/// `target` changed. `source_on_tie` picks the copy that stays where the
/// expiries tie or neither side carries one; a copy with an expiry still beats
/// one without, whichever side. `true` for [`copy_non_login_keys`], whose source
/// is a live file a refresh moves forward, `false` for
/// [`carry_live_extra_fresher_into`], whose file sat unheld while peer sessions
/// wrote the store.
fn merge_fresher_key(
    target: &mut serde_json::Map<String, serde_json::Value>,
    key: &str,
    value: &serde_json::Value,
    source_on_tie: bool,
) -> bool {
    let merged = match target.get(key) {
        Some(held) => {
            let (held, source) = if source_on_tie {
                (held, value)
            } else {
                (value, held)
            };
            fresher_copy(held, source)
        }
        None => value.clone(),
    };
    if target.get(key) == Some(&merged) {
        return false;
    }
    target.insert(key.to_string(), merged);
    true
}

/// What stays of a key both sides hold: `held`, the target's copy, and
/// `source`'s. A token (an object with `expiresAt`, `accessToken` or
/// `refreshToken`, a design login) is one value and never mixed: the copy
/// that expires later stays, `source`'s on a tie, and one without an expiry
/// never replaces one that has one. Two objects that are not tokens are merged
/// one level deep, keeping every entry either side holds: an entry that is a
/// token follows the same rule, any other entry takes `source`'s copy, and an
/// entry only the target holds stays. That is what the MCP logins, a map of
/// tokens with one per server, need: a server only the target holds stays, so
/// a per-server logout reaches the store with the next switch-in's carry, not
/// through this. Where either side is not an object, `source`'s copy wins
/// whole.
fn fresher_copy(held: &serde_json::Value, source: &serde_json::Value) -> serde_json::Value {
    if let Some(kept) = fresher_token(held, source) {
        return kept;
    }
    match (held.as_object(), source.as_object()) {
        (Some(held_map), Some(source_map)) => {
            let mut merged = held_map.clone();
            for (entry, value) in source_map {
                let kept = held_map
                    .get(entry)
                    .and_then(|held_entry| fresher_token(held_entry, value))
                    .unwrap_or_else(|| value.clone());
                merged.insert(entry.clone(), kept);
            }
            serde_json::Value::Object(merged)
        }
        _ => source.clone(),
    }
}

/// [`fresher_copy`]'s token rule, `None` when neither side is a token.
fn fresher_token(
    held: &serde_json::Value,
    source: &serde_json::Value,
) -> Option<serde_json::Value> {
    let is_token = |value: &serde_json::Value| {
        value.as_object().is_some_and(|obj| {
            ["expiresAt", "accessToken", "refreshToken"]
                .iter()
                .any(|field| obj.contains_key(*field))
        })
    };
    match (is_token(held), is_token(source)) {
        (false, false) => return None,
        // A token is never replaced by something that is none (an emptied
        // object), which would drop a login no copy replaced.
        (true, false) => return Some(held.clone()),
        _ => {}
    }
    let expiry = |value: &serde_json::Value| {
        value.get("expiresAt").and_then(|at| {
            at.as_i64()
                .or_else(|| at.as_f64().map(|at| at.trunc() as i64))
        })
    };
    Some(match (expiry(held), expiry(source)) {
        (Some(held_at), Some(source_at)) if held_at > source_at => held.clone(),
        (Some(_), None) => held.clone(),
        _ => source.clone(),
    })
}

/// The names of `value`'s non-login keys, for an event line (names only).
fn non_login_key_names(value: &serde_json::Value) -> Vec<&str> {
    value
        .as_object()
        .map(|obj| {
            obj.keys()
                .map(String::as_str)
                .filter(|key| !LOGIN_CREDENTIAL_KEYS.contains(key))
                .collect()
        })
        .unwrap_or_default()
}

/// The bytes a static-token sidecar writer publishes at `sidecar`: `fresh`, the
/// file rebuilt from a token, plus every non-login key the current sidecar
/// holds, a `/design-login` a sync put there among them. Every sidecar writer
/// rebuilds the file from its token alone, so without this a re-mint, a rolling
/// re-stamp or a restore drops what the account kept beside its login. The
/// keep-everything rule an account's own `credentials.json` gets on every token
/// write ([`crate::profile::preserve_extra_blocks`]), minus the login's identity
/// keys ([`LOGIN_CREDENTIAL_KEYS`]): those belong to the login being replaced,
/// and a re-mint can be another account's token.
///
/// With no sidecar yet (the first mint on this profile, or the first roll) the
/// keys come from the profile's `credentials.json`, the same account's other
/// store, so a design login saved while the profile ran on its OAuth login is
/// installed with the new token too. Every caller first saves the live file
/// into the install source ([`sync_live_extra_into_install_source`]), that
/// `credentials.json` on this path, so what only the live file held beside the
/// login being replaced is read here too.
///
/// `fresh` unchanged when `preserve_non_login_keys` is off, and when the
/// current sidecar is a mis-fill: the keys beside a mis-filled login can be
/// another account's, and a mis-fill's contents are evidence, not this
/// account's data. A present file that does not read or parse is skipped with
/// an event line ([`read_store_for_merge`]); keeping keys is a convenience, and
/// the write it rides on is not. Callers hold the state flock, so the read and
/// their write see the same file.
fn keep_non_login_in_sidecar(fresh: &[u8], sidecar: &Path) -> Vec<u8> {
    if !preserve_non_login_keys() {
        return fresh.to_vec();
    }
    let source = if sidecar.symlink_metadata().is_ok() {
        sidecar.to_path_buf()
    } else {
        sidecar.with_file_name("credentials.json")
    };
    let Some(current) = read_store_for_merge(
        &source,
        "this write goes ahead without the non-login keys it may hold (a design login, MCP \
         logins)",
    ) else {
        return fresh.to_vec();
    };
    let misfilled = serde_json::from_value::<ClaudeCredentials>(current.clone())
        .ok()
        .and_then(|creds| creds.claude_ai_oauth)
        .is_some_and(|oauth| sidecar_kind_of(&oauth) == SidecarKind::Misfilled);
    if source == sidecar && misfilled {
        return fresh.to_vec();
    }
    let Ok(mut rebuilt) = serde_json::from_slice::<serde_json::Value>(fresh) else {
        return fresh.to_vec();
    };
    if !copy_non_login_keys(&mut rebuilt, &current) {
        return fresh.to_vec();
    }
    serde_json::to_vec_pretty(&rebuilt).unwrap_or_else(|_| fresh.to_vec())
}

/// `store` with `source`'s non-login keys merged over it, or `None` when
/// nothing is to be written: `source`'s login is not `store`'s (the same
/// non-empty access token), or the merge changes nothing. The rule every
/// per-account sync applies, the live file's ([`sync_live_extra_into`]) and the
/// macOS Keychain item's ([`sync_item_into_its_store`]). PURE, so it is pinned
/// on every platform.
pub(crate) fn non_login_keys_onto_own_store(
    source: &serde_json::Value,
    store: &serde_json::Value,
) -> Option<serde_json::Value> {
    non_login_keys_for_login(source, store, login_access_token(store)?)
}

/// `store` with `source`'s non-login keys merged over it when `source`'s login
/// is `login`, `None` when it is not or nothing changes. The store's own login
/// for every sync but the one after a rotation, which matches the live file
/// against the login the rotation just replaced.
fn non_login_keys_for_login(
    source: &serde_json::Value,
    store: &serde_json::Value,
    login: &str,
) -> Option<serde_json::Value> {
    if login_access_token(source) != Some(login) {
        return None;
    }
    let mut synced = store.clone();
    copy_non_login_keys(&mut synced, source).then_some(synced)
}

/// Sync the live credential's non-login keys into `store`, the store of the
/// account the live login belongs to, reporting whether it wrote. The
/// per-account half of preservation: Claude Code saves a `/design-login` (and
/// anything else it adds beside the login) into the live file, and on Linux its
/// staged write replaces clauth's symlink with a regular file, so without this
/// the next relink onto the store, a switch's or the daemon's and TUI's boot
/// reconcile's, discards what only that file held.
///
/// Only when the live login IS `store`'s ([`non_login_keys_onto_own_store`]): a
/// live file carrying another account's login is not this account's data, and
/// what crosses accounts is the carry's decision alone
/// ([`CARRIED_CREDENTIAL_KEYS`]). A no-op when the live slot resolves to
/// `store` itself, and when either side is absent; a present side that does
/// not parse is skipped with an event line. Stands down while a rotation is
/// staged beside a `credentials.json` store
/// ([`crate::profile::rotation_staged_beside`]): a store written after the
/// stage reads as the newer commit, and the load would discard the staged pair.
/// The keys then stay only in the live file: a caller that relinks it away
/// right after (a switch, the boot reconcile, the adoption) loses them with it,
/// and one that leaves it in place (a rotation's sync, a sidecar writer's
/// pre-save) leaves them there for a later sync, which carries them only while
/// the live login still matches the store's.
///
/// Accepted ceiling, the carry's own: this adds and overwrites, never deletes,
/// so a key Claude Code removed from the live file stays in the store. Where
/// both hold a copy, the fresher stays ([`copy_non_login_keys`]).
pub(crate) fn sync_live_extra_into(link: &Path, store: &Path) -> Result<bool> {
    sync_live_extra_matching(link, store, None)
}

/// [`sync_live_extra_into`] with the live login it must match: `None` for the
/// store's own, `Some` for the login a rotation just replaced in the store.
fn sync_live_extra_matching(link: &Path, store: &Path, login: Option<&str>) -> Result<bool> {
    if paths_equivalent(link, store) {
        return Ok(false);
    }
    let purpose = "the live credential's non-login keys (a design login, MCP logins) were not \
                   saved into the account's store";
    let (Some(live), Some(stored)) = (
        read_store_for_merge(link, purpose),
        read_store_for_merge(store, purpose),
    ) else {
        return Ok(false);
    };
    let synced = match login {
        Some(login) => non_login_keys_for_login(&live, &stored, login),
        None => non_login_keys_onto_own_store(&live, &stored),
    };
    let Some(synced) = synced else {
        return Ok(false);
    };
    if crate::profile::rotation_staged_beside(store) {
        logline!(
            "clauth: a rotation is staged beside {}, so {purpose}",
            store.display()
        );
        return Ok(false);
    }
    atomic_write_600(store, serde_json::to_string_pretty(&synced)?)
        .context("failed to save the live credential's non-login keys into its store")?;
    Ok(true)
}

/// After clauth rotated `name`'s chain: the live file Claude Code wrote beside
/// the login the rotation replaced (`previous_login`, a `/design-login`) is
/// still this account's, but its login no longer matches the store's, so no
/// later sync would recognise it. Matched against the replaced login instead,
/// and saved beside the rotated pair. Runs once the store holds the rotated
/// pair (the stage exists to make that pair durable before anything else
/// writes), under the rotation's state-flock hold. Best-effort, logged.
pub(crate) fn sync_live_extra_after_rotation(name: &ProfileName, previous_login: &str) {
    if !preserve_non_login_keys() {
        return;
    }
    let (Ok(link), Ok(store)) = (claude_credentials_path(), install_source_path(name)) else {
        return;
    };
    if let Err(e) = sync_live_extra_matching(&link, &store, Some(previous_login)) {
        logline!(
            "clauth: could not save the live credential's non-login keys into {} after \
             rotating '{name}': {e:#}",
            store.display()
        );
    }
}

/// Run [`sync_live_extra_into`], reporting a failure instead of raising it:
/// keeping a design login is a convenience, and completing the relink, switch
/// or rotation it rides on is not.
pub(crate) fn sync_live_extra_best_effort(link: &Path, store: &Path) {
    sync_live_extra_matching_best_effort(link, store, None);
}

/// [`sync_live_extra_best_effort`] with the live login it must match: `None`
/// for the store's own (the plain [`sync_live_extra_into`]), `Some` for the
/// login an adoption's relink found replaced in the store, its only `Some`
/// caller (rule 2's older login). One log text serves both. The `match` is a
/// reachability constraint, not dispatch: the rotation path keeps its own
/// message ([`sync_live_extra_after_rotation`]), so its `None` arm is the one
/// way the binary still reaches [`sync_live_extra_into`].
pub(crate) fn sync_live_extra_matching_best_effort(link: &Path, store: &Path, login: Option<&str>) {
    let synced = match login {
        Some(login) => sync_live_extra_matching(link, store, Some(login)),
        None => sync_live_extra_into(link, store),
    };
    if let Err(e) = synced {
        logline!(
            "clauth: could not save the live credential's non-login keys into {}: {e:#}. A design \
             login or MCP login made since the last relink may need signing in again",
            store.display()
        );
    }
}

/// Save the non-login keys of a Keychain item a relink or sign-out is replacing
/// into the store of the account whose login it holds, reporting whether that
/// store now holds them: the Keychain half of the switch-away sync. On macOS
/// Claude Code keeps its credential store in the item rather than the file
/// (measured on 2.1.283 against a `security` stand-in: it read its login from
/// the item and neither read nor created `.credentials.json`; the regular-file
/// mirror `classify_link_at` describes is not what that build did), so a
/// `/design-login` lives there alone, and a switch's install writes the
/// incoming store over it with only the MCP-server logins carried.
///
/// The owner is the store holding the item's login (the same non-empty access
/// token), looked for in this order: `relinked`, the store a relink installs;
/// the on-disk active profile's, still the outgoing account while a switch
/// relinks; the one roster profile whose store does. A profiles.toml that does
/// not load falls back to every profile directory on disk. No owner, or two
/// stores claiming one login, saves nothing and says so; so does an item that
/// holds a design login and no login at all, which the install is about to
/// drop. clauth rotates the chain it mirrors into the item and writes the store
/// first, so the two agree in the normal state; after a refresh Claude Code
/// made inside the item no store matches (not measured). Callers hold the state
/// flock and pass the item as read before the write, so this spends no
/// `security` call of its own. Best-effort: a failure costs the keys, never the
/// switch.
#[cfg_attr(
    not(target_os = "macos"),
    allow(
        dead_code,
        reason = "the callers are the macOS Keychain relink and sign-out; the owner search is pinned on every platform"
    )
)]
pub(crate) fn sync_item_into_its_store(item: &serde_json::Value, relinked: Option<&Path>) -> bool {
    let unsaved: Vec<&str> = non_login_key_names(item)
        .into_iter()
        .filter(|key| !CARRIED_CREDENTIAL_KEYS.contains(key))
        .collect();
    let Some(login) = login_access_token(item) else {
        if relinked.is_some() && !unsaved.is_empty() {
            logline!(
                "clauth: the macOS Keychain item held {} with no login beside it, so no \
                 account's store takes it and the install replaces it",
                unsaved.join(", ")
            );
        }
        return false;
    };
    let purpose = "the macOS Keychain item's non-login keys (a design login) were not saved \
                   into its account's store";
    let owns = |store: &Path| {
        read_store_for_merge(store, purpose)
            .filter(|value| login_access_token(value) == Some(login))
            .map(|value| (store.to_path_buf(), value))
    };
    let owner = relinked.and_then(owns).or_else(|| {
        let names = match crate::profile::load_app_state() {
            Ok(state) => {
                let active = state
                    .active_profile
                    .as_ref()
                    .and_then(|name| install_source_path(name).ok())
                    .and_then(|store| owns(&store));
                if active.is_some() {
                    return active;
                }
                state.profiles
            }
            Err(_) => crate::profile::profile_names_on_disk(),
        };
        let mut owners = names
            .iter()
            .filter_map(|name| install_source_path(name).ok())
            .filter_map(|store| owns(&store));
        match (owners.next(), owners.next()) {
            (Some(one), None) => Some(one),
            _ => None,
        }
    });
    let Some((store, value)) = owner else {
        if !unsaved.is_empty() {
            logline!(
                "clauth: no single profile holds the macOS Keychain item's login, so {} it held \
                 beside the login was not saved into an account's store",
                unsaved.join(", ")
            );
        }
        return false;
    };
    let Some(synced) = non_login_keys_onto_own_store(item, &value) else {
        return true;
    };
    if crate::profile::rotation_staged_beside(&store) {
        logline!(
            "clauth: a rotation is staged beside {}, so {purpose}",
            store.display()
        );
        return false;
    }
    let written = serde_json::to_string_pretty(&synced)
        .map_err(anyhow::Error::from)
        .and_then(|bytes| atomic_write_600(&store, bytes).map_err(anyhow::Error::from));
    match written {
        Ok(()) => true,
        Err(e) => {
            logline!("clauth: {purpose}: {e:#}");
            false
        }
    }
}

/// Move the sidecar's non-login keys into the profile's `credentials.json`
/// before the sidecar goes (a cleared long-lived token), so a design login the
/// account kept there stays installed once `credentials.json` is the install
/// source again. The same account's other store, so no login check, but never
/// out of a mis-fill, whose keys are evidence; the staged-rotation stand-down
/// holds as for every write of that file. Callers hold the state flock.
/// Best-effort, logged.
fn hand_sidecar_keys_to_store(sidecar: &Path) {
    if !preserve_non_login_keys() {
        return;
    }
    let store = sidecar.with_file_name("credentials.json");
    let purpose = "the cleared token's non-login keys (a design login, MCP logins) were not \
                   moved into credentials.json";
    let Some(held) = read_store_for_merge(sidecar, purpose) else {
        return;
    };
    let misfilled = serde_json::from_value::<ClaudeCredentials>(held.clone())
        .ok()
        .and_then(|creds| creds.claude_ai_oauth)
        .is_some_and(|oauth| sidecar_kind_of(&oauth) == SidecarKind::Misfilled);
    if misfilled {
        return;
    }
    let Some(mut stored) = read_store_for_merge(&store, purpose) else {
        let held_keys = non_login_key_names(&held);
        if !held_keys.is_empty() {
            logline!(
                "clauth: {} held {} beside the cleared token and the profile stores no \
                 credentials.json to take them, so they go with the token",
                sidecar.display(),
                held_keys.join(", ")
            );
        }
        return;
    };
    if !copy_non_login_keys(&mut stored, &held) {
        return;
    }
    if crate::profile::rotation_staged_beside(&store) {
        logline!(
            "clauth: a rotation is staged beside {}, so {purpose}",
            store.display()
        );
        return;
    }
    let written = serde_json::to_string_pretty(&stored)
        .map_err(anyhow::Error::from)
        .and_then(|bytes| atomic_write_600(&store, bytes).map_err(anyhow::Error::from));
    if let Err(e) = written {
        logline!("clauth: {purpose}: {e:#}");
    }
}

/// Whether the resolved install source `target` holds a login worth linking:
/// the file exists AND parses as credentials. A store that cannot be read
/// answers "none" here, exactly like a logged-out account with no store, so a
/// switch to it links nothing instead of publishing a link the next switch away
/// would fail reading through.
fn linkable_install_source(target: &Path) -> bool {
    target.exists() && read_json_file::<ClaudeCredentials>(target).is_ok()
}

/// Whether the live credential file is a symlink to an unreadable store: the
/// active profile's torn `credentials.json`. In that state the key-preserving
/// syncs would only re-warn over the same torn bytes the torn-store notice
/// already named, so they are skipped. A detached regular live file that cannot
/// be read (a Claude Code write in progress) still syncs and warns — its keys
/// may exist and be about to die.
fn live_link_is_unreadable(link: &Path) -> bool {
    link.symlink_metadata()
        .is_ok_and(|m| m.file_type().is_symlink())
        && read_json_file::<ClaudeCredentials>(link).is_err()
}

/// Symlink `~/.claude/.credentials.json` → profile's `credentials.json` (copy on
/// Windows). Refuses to overwrite a non-matching regular file — that would silently
/// drop a CC re-login the user hasn't resolved yet.
pub(crate) fn link_profile_credentials(name: &ProfileName) -> Result<()> {
    with_state_lock(|_held| {
        let link = claude_credentials_path()?;
        let target = install_source_path(name)?;
        // Decided once for the file publish and the Keychain mirror alike: an
        // unreadable install source takes the mirror's absent arm, never its
        // parse-refusing read, so a switch to a torn store links nothing on
        // every platform.
        let linkable = linkable_install_source(&target);
        // Loaded once for the file sync and the Keychain mirror alike, so one
        // relink cannot act on two different answers across an edit landing
        // between two reads.
        let preserve = preserve_non_login_keys();
        let live_unreadable = live_link_is_unreadable(&link);

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
            // The live file is this account's when its login is the target's,
            // which is the daemon's and the TUI's boot reconcile: keep what
            // Claude Code saved beside the login (a `/design-login`) in the
            // account's own store, the static-token sidecar included, before
            // the relink below replaces the file.
            if preserve && !live_unreadable {
                sync_live_extra_best_effort(&link, &target);
            }
            // Preserve the live MCP-server logins onto the incoming profile
            // before the swap, so switching accounts keeps them intact.
            if !live_unreadable {
                carry_live_extra_best_effort(&link, &target, name, preserve);
            }
        }

        // macOS: make the switch real, since Claude Code reads the Keychain.
        // `Leave` because this is the GUARDED relink: it is also what a rename,
        // a first-ever capture, and the daemon's and TUI's boot reconcile call,
        // none of which is changing accounts. Before the link, never after: see
        // [`keychain_mirror_source`].
        #[cfg(target_os = "macos")]
        if crate::keychain::enabled()
            && keychain_mirror_source(&target, linkable, AbsentSource::Leave, preserve, None)?
                == ItemWrite::Refused
        {
            return Err(bare_item_changed_refusal(BareItemWriter::Switch));
        }
        if linkable {
            publish_credential_link(&link, &target)?;
        } else if link.symlink_metadata().is_ok() {
            std::fs::remove_file(&link).context("failed to remove old .credentials.json")?;
        }

        Ok(())
    })
}

pub(crate) fn clear_claude_credentials() -> Result<()> {
    with_state_lock(|_held| {
        let link = claude_credentials_path()?;
        // macOS: also sign the Keychain out so Claude Code can't spend the
        // account (parity with removing the credential file). The login the
        // item held goes, unless no store has saved it (a chain CC rotated
        // after our last capture): then the item is left whole, since the
        // sign-out would strand it, and the sign-out fails before the file
        // goes. What survives a sign-out is the MCP-server logins, which
        // belong to no account and would otherwise be collateral of every
        // wrap-off, and with `preserve_non_login_keys` on a design login as
        // well; an item left holding nothing else is deleted.
        #[cfg(target_os = "macos")]
        if crate::keychain::enabled() {
            let preserve = preserve_non_login_keys();
            let refused = std::cell::Cell::new(false);
            let may_replace = |item: &serde_json::Value| {
                let may = bare_item_may_be_replaced(item, None);
                refused.set(!may);
                may
            };
            crate::keychain::keychain_sign_out(preserve, may_replace, |item| {
                preserve && sync_item_into_its_store(item, None)
            })?;
            if refused.get() {
                return Err(bare_item_changed_refusal(BareItemWriter::SignOut));
            }
        }
        if link.symlink_metadata().is_ok() {
            std::fs::remove_file(&link).context("failed to remove .credentials.json")?;
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
                .and_then(|name| {
                    crate::profile::read_api_key(&ProfileName::from(name))
                        .ok()
                        .flatten()
                })
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

/// The runtime overlay's model sources, split by SLOT rather than flattened:
/// the top-level `model` and `env.ANTHROPIC_MODEL` are headline candidates a
/// `--model` flag or an explicit `model` argument replaces, while the
/// `fallbackModel` entries are the overlay's tier of the demand's one fallback
/// chain (a higher settings source's list replaces them: an absent key reads
/// `None`, an empty or non-array value reads `Some(vec![])`, i.e. no fallback),
/// and `env.CLAUDE_CODE_SUBAGENT_MODEL` joins the demand whatever the headline;
/// the top-level `agent` names the session's agent, the settings-tier leg of
/// the agent chain. The four `ANTHROPIC_DEFAULT_*_MODEL` keys carry the
/// `[models]` alias remap, read here so the remap resolves what the child
/// actually sees. Read off the SAME overlay `build_claude_settings_json`
/// writes, so the two cannot drift on the builder or base choice.
#[derive(Debug, Default)]
pub(crate) struct RuntimeModelSources {
    pub(crate) model: Option<String>,
    pub(crate) agent: Option<String>,
    pub(crate) fallbacks: Option<Vec<String>>,
    pub(crate) env_anthropic: Option<String>,
    pub(crate) env_subagent: Option<String>,
    pub(crate) env_opus: Option<String>,
    pub(crate) env_sonnet: Option<String>,
    pub(crate) env_haiku: Option<String>,
    pub(crate) env_fable: Option<String>,
}

impl RuntimeModelSources {
    /// The `env` value this source set carries for `key`, across the model env
    /// keys the demand resolver reads. `None` for a key the struct does not
    /// hold, so a caller folding several sources fills each key from the first
    /// source that set it.
    pub(crate) fn env_value(&self, key: &str) -> Option<String> {
        match key {
            "ANTHROPIC_MODEL" => self.env_anthropic.clone(),
            "CLAUDE_CODE_SUBAGENT_MODEL" => self.env_subagent.clone(),
            "ANTHROPIC_DEFAULT_OPUS_MODEL" => self.env_opus.clone(),
            "ANTHROPIC_DEFAULT_SONNET_MODEL" => self.env_sonnet.clone(),
            "ANTHROPIC_DEFAULT_HAIKU_MODEL" => self.env_haiku.clone(),
            "ANTHROPIC_DEFAULT_FABLE_MODEL" => self.env_fable.clone(),
            _ => None,
        }
    }
}

/// The model slots a settings object carries, read off the keys Claude Code
/// itself reads: top-level `model`, `agent`, `fallbackModel`, and the model
/// `env` keys. Shared by the overlay reader, the project settings tiers and the
/// `--settings` passthrough tier, so the three fold the same keys.
pub(crate) fn model_sources_from_json(settings: &serde_json::Value) -> RuntimeModelSources {
    let env = |key: &str| settings["env"][key].as_str().map(str::to_owned);
    RuntimeModelSources {
        model: settings["model"].as_str().map(str::to_owned),
        agent: settings["agent"]
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned),
        fallbacks: settings.get("fallbackModel").map(|fb| {
            fb.as_array()
                .map(|fb| {
                    fb.iter()
                        .filter_map(|v| v.as_str())
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default()
        }),
        env_anthropic: env("ANTHROPIC_MODEL"),
        env_subagent: env("CLAUDE_CODE_SUBAGENT_MODEL"),
        env_opus: env("ANTHROPIC_DEFAULT_OPUS_MODEL"),
        env_sonnet: env("ANTHROPIC_DEFAULT_SONNET_MODEL"),
        env_haiku: env("ANTHROPIC_DEFAULT_HAIKU_MODEL"),
        env_fable: env("ANTHROPIC_DEFAULT_FABLE_MODEL"),
    }
}

/// The split-source form of the runtime overlay's model slots — same build,
/// same overlay, structured so a caller judging the headline alone does not
/// have to pick it out of a flattened list.
pub(crate) fn runtime_model_sources(
    base: Option<&Path>,
    profile: &Profile,
    stale_env_keys: &[String],
) -> RuntimeModelSources {
    let empty = RuntimeModelSources::default();
    let Ok(json) = build_claude_settings_json(base, profile, stale_env_keys) else {
        return empty;
    };
    let Ok(settings) = serde_json::from_str::<serde_json::Value>(&json) else {
        return empty;
    };
    model_sources_from_json(&settings)
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

/// Build the `apiKeyHelper` command string CC runs per request to obtain an
/// auth value for an api-key profile. The hidden subcommand reads
/// `Profile::api_key` from `config.toml` (0o600) and prints it to stdout —
/// the key is static, and nothing on this path mints or rotates it.
///
/// CC runs the value through the system shell (`/bin/sh` on macOS/Linux,
/// `cmd` on Windows — per the Claude Code settings docs), so each token is
/// shell-escaped by [`shell_quote`]. The profile name is constrained by
/// `actions::validate_profile_name` to `[A-Za-z0-9_.@+-]+` with no leading
/// dot — entirely within the safe-char set, so it round-trips unquoted; the
/// helper-quoting exists for the exe path, which may contain spaces
/// (`/Applications/...`, `C:\Program Files\...`).
fn build_api_key_helper_command(exe: &Path, profile_name: &ProfileName) -> String {
    let exe = crate::platform::installed_exe_path(exe);
    format!(
        "{} {} {}",
        shell_quote(&exe.to_string_lossy()),
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
/// [`crate::oauth::third_party_dead_chain_copy`] shares it: the split sentence
/// claims the key still works, and a profile this returns false for with no
/// env-carried key has no key it could.
pub(crate) fn has_usable_api_key(profile: &Profile) -> bool {
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

/// Whether the outgoing snapshot would read the active profile's own unreadable
/// `credentials.json` through the live link: the active profile loaded with an
/// unreadable store AND the live login resolves to that same store. Compared
/// against `credentials.json`, never the install source, so a long-lived
/// sidecar profile linked to `session-token.json` still snapshots. The
/// canonical-path comparison answers the store half for a symlink (Linux); on
/// macOS a regular-file Keychain mirror resolves to itself rather than the
/// store, so the live file is left to the caller's divergence handling.
fn active_unreadable_store_is_linked(config: &AppConfig) -> Result<bool> {
    let Some(active) = config.state.active_profile.as_ref() else {
        return Ok(false);
    };
    let Some(profile) = config.find(active) else {
        return Ok(false);
    };
    if profile.unreadable_credentials_store.is_none() {
        return Ok(false);
    }
    let live = claude_credentials_path()?;
    let store = profile_dir(active)?.join("credentials.json");
    Ok(paths_equivalent(&live, &store))
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
        // The live link points at the active profile's torn store: reading
        // through it would fail the snapshot (and the switch riding on it)
        // over bytes the torn-store notice already named. Skip only then; a
        // detached live file is left to the caller's divergence handling (the
        // CLI reconcile captures it).
        if active_unreadable_store_is_linked(config)? {
            return Ok(());
        }
        if matches!(classify_credentials_link(&active)?, LinkState::Diverged) {
            if is_first_login(&active)? {
                adopt_first_login(config, &active)?;
            }
            return Ok(());
        }
        snapshot_active_credentials_unchecked(config, &active, held, false)?;
        Ok(())
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
        // Re-verified under the same hold the snapshot sink runs in: the
        // startup cascade checks the predicate unlocked, and a switch landing
        // in between must not be overwritten by a boot adopt.
        anyhow::ensure!(
            is_first_login(active)?,
            "refusing to adopt for '{active}': the live login is no longer a first login"
        );
        snapshot_active_credentials_unchecked(config, active, held, true)?;
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
    remove_anchor: bool,
) -> Result<bool> {
    // CLA-SPLIT: a profile whose live slot holds its static session token carries
    // nothing to snapshot, and capturing the live file into `profile.credentials`
    // would clobber the clauth-private usage OAuth pair. The guard lives at this
    // shared sink so every caller is covered: both the divergence-modal
    // "overwrite" and the CLI reconciled switch reach here via
    // `force_snapshot_active_credentials`. `adopt_first_login` never hits it for
    // a session-token profile (the install source exists, so `is_first_login` is
    // false), so the guard is a safe no-op on that path.
    //
    // The login stays out, but the keys beside it are this account's: with
    // `preserve_non_login_keys` on, whatever Claude Code saved beside the static
    // token in the live file (a `/design-login`) is synced into the sidecar,
    // which is the switch-away half of per-account preservation. Only when the
    // live login IS the sidecar's ([`sync_live_extra_into`]).
    if has_session_token(active) {
        sync_live_extra_into_install_source(active);
        return Ok(false);
    }
    // Fresh, not just the in-memory active marker: the active profile may have
    // been deleted or renamed out-of-process since this caller's config was
    // loaded (a daemon switch/switch-off holds a stale config between reloads).
    // `save_profile` would recreate the deleted directory, so consult the
    // on-disk list before writing. Callers run this under the state flock, so
    // the read is stable.
    if !crate::profile::is_configured(active)? {
        return Ok(false);
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
        return Ok(false);
    };
    if live_login_is_empty(&credentials) {
        return Ok(false);
    }
    let Some(profile) = config.find_mut(active) else {
        return Ok(false);
    };
    // A disk-read login proves no uuid, so an anchored profile's identity moves
    // (an Unproven re-login forgets once); the non-force LinkedTo snapshot's
    // bytes are the store's own, so nothing is forgotten.
    let plan = if remove_anchor {
        let before = read_before_identities(active);
        let after_oauth = OAuthIdentity::for_values(None, credentials.access_token());
        // A hybrid's delegates still run on the key, so its throughput record
        // survives; a pure OAuth spawn's record follows the login's identity.
        let after_throughput = throughput_identity(
            &after_oauth,
            before.base_url.as_deref(),
            before.api_key.as_deref(),
            before.env_token.as_deref(),
        );
        let plan = ForgetPlan {
            oauth: oauth_changed(&before.oauth, &after_oauth),
            throughput: before.throughput != after_throughput,
            ..ForgetPlan::default()
        };
        forget_disk_plan(active, plan);
        crate::profile_cache::remove_profile_cache(
            active,
            crate::profile_cache::ACCOUNT_ID_CACHE_FILE,
        );
        crate::oauth::retire_unreadable_spent_record(active);
        Some(plan)
    } else {
        None
    };
    profile.set_credentials(Some(credentials), held);
    save_profile(profile)?;
    let oauth_changed = match plan {
        Some(plan) => {
            note_forget_plan(active, plan);
            plan.oauth
        }
        None => false,
    };
    // The capture above models the login alone, so a key only the live
    // file holds (a `/design-login`) would be dropped here and gone with
    // the relink that follows: sync it into the store the login was just
    // captured into.
    sync_live_extra_into_install_source(active);
    Ok(oauth_changed)
}

/// With `preserve_non_login_keys` on, sync the live file's non-login keys into
/// `name`'s install source ([`sync_live_extra_into`]): the store the live login
/// is `name`'s own in, the sidecar while it holds a long-lived login and
/// `credentials.json` otherwise. A mis-filled sidecar is never the install
/// source, so nothing is written into that evidence. The snapshot sink's sync,
/// and the save every sidecar writer makes under the state flock before it
/// replaces or removes the login the live file was written beside (a re-mint,
/// a rolling stamp, a restore, a heal, a clear), after which no sync matches
/// that file again. Infallible like the rest of the preservation, so a path
/// that cannot resolve costs the keys, never the switch or write it runs for.
pub(crate) fn sync_live_extra_into_install_source(name: &ProfileName) {
    if !preserve_non_login_keys() {
        return;
    }
    if let (Ok(link), Ok(store)) = (claude_credentials_path(), install_source_path(name)) {
        sync_live_extra_best_effort(&link, &store);
    }
}

/// Snapshot the live `.credentials.json` into the active profile unconditionally.
/// Returns whether the OAuth half's stored identity changed, so a caller can
/// expire the `/profile` clock only when it did.
pub(crate) fn force_snapshot_active_credentials(config: &mut AppConfig) -> Result<bool> {
    with_state_lock(|held| {
        let Some(active) = config.state.active_profile.as_ref().cloned() else {
            return Ok(false);
        };
        // The snapshot reads the live login off disk, so its uuid is unproven:
        // the shared sink removes the anchor before the store write.
        snapshot_active_credentials_unchecked(config, &active, held, true)
    })
}

/// Re-link `.credentials.json` to `name`'s stored credentials, overwriting the live path.
pub(crate) fn force_link_profile_credentials(name: &ProfileName) -> Result<()> {
    with_state_lock(|_held| {
        let link = claude_credentials_path()?;
        let target = install_source_path(name)?;
        // One linkability decision for both layers, as in
        // `link_profile_credentials`.
        let linkable = linkable_install_source(&target);
        // One load for both layers, as in `link_profile_credentials`.
        let preserve = preserve_non_login_keys();
        let live_unreadable = live_link_is_unreadable(&link);
        if link.symlink_metadata().is_ok() {
            // A relink of the account the live login already belongs to (an
            // adopt, a re-capture of the active profile) keeps what the live
            // file holds beside the login; a switch's live login is the
            // outgoing account's, which the snapshot sink synced before this.
            if preserve && !live_unreadable {
                sync_live_extra_best_effort(&link, &target);
            }
            // Preserve the live MCP-server logins onto the incoming profile
            // before the swap, so switching accounts keeps them intact.
            if !live_unreadable {
                carry_live_extra_best_effort(&link, &target, name, preserve);
            }
        }
        // macOS: make the switch real, since Claude Code reads the Keychain.
        // `SignOut` because this is the FORCING relink, which every switch path
        // takes: the caller has decided this profile's credentials are what the
        // live slot holds, so a profile storing none must stop the item serving
        // the account the switch just left. Before the link, never after: see
        // [`keychain_mirror_source`].
        #[cfg(target_os = "macos")]
        if crate::keychain::enabled()
            && keychain_mirror_source(&target, linkable, AbsentSource::SignOut, preserve, None)?
                == ItemWrite::Refused
        {
            return Err(bare_item_changed_refusal(BareItemWriter::Switch));
        }
        if linkable {
            publish_credential_link(&link, &target)?;
        } else if link.symlink_metadata().is_ok() {
            std::fs::remove_file(&link).context("failed to remove .credentials.json")?;
        }
        Ok(())
    })
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
        // No unlink first: `atomic_write_600` publishes through a staging
        // sibling and a rename, and a rename REPLACES a symlink destination
        // rather than following it. Unlinking beforehand only opened a window
        // where a failed write left the path with nothing in it.
        let content =
            std::fs::read(&path).context("failed to read .credentials.json before detach")?;
        atomic_write_600(&path, content).context("failed to write detached .credentials.json")?;
        Ok(())
    })
}

#[cfg(test)]
#[path = "../tests/inline/claude.rs"]
mod tests;

#[cfg(test)]
#[path = "../tests/inline/claude_bare_item.rs"]
mod bare_item_tests;
