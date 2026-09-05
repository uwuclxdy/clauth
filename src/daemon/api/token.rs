//! The REST API's bearer token: generate once, persist, reuse.
//!
//! Stored at `~/.clauth/auth_token.json` (0600, like every other secret in the
//! tree) so the operator copies it to the client machine once and it survives
//! every restart. 32 CSPRNG bytes hashed with SHA-256 and hex-encoded — 64
//! characters, 256 bits of entropy.
//!
//! The running server holds only the token's DIGEST ([`AuthToken`]), never the
//! plaintext: a bearer token is a password, and the process that checks it has
//! no reason to keep a copy that a core dump could yield. Only
//! `clauth daemon --print-token` reads the plaintext back, and only to print it.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use crate::lock::with_state_lock;
use crate::logline::logline;
use crate::profile::{atomic_write_600, clauth_dir};
use crate::usage::{epoch_secs_to_iso, now_ms};

/// Peer of `status.json` / `clauthd.lock` in `~/.clauth`.
const TOKEN_FILE: &str = "auth_token.json";
/// Hex chars in a SHA-256 digest. The wire format's whole length check.
const TOKEN_LEN: usize = 64;
/// Bumped only on a breaking change to the file's shape, like `status.json`.
const SCHEMA: u64 = 1;

/// What this build's token is allowed to do: everything the API exposes — the
/// feed, the switch, and the mirror.
///
/// The one value there is, and written now on purpose. A later build that wants
/// a read-only token for a wall display, or a mirror-only one for a replica,
/// then adds a value to a field every deployed file already carries, instead of
/// bumping the schema and migrating them. Costing one line today buys that.
const CONTROL_TIER: &str = "control";

/// `~/.clauth/auth_token.json`. `created_at` is informational — it answers "how
/// old is the credential I copied to that other box?" without a second file.
#[derive(Debug, Serialize, Deserialize)]
struct AuthTokenFile {
    schema: u64,
    token: String,
    created_at: String,
    /// See [`CONTROL_TIER`]. A file written before the field existed reads as
    /// `control`, which is exactly what it was, so no existing deployment is
    /// rotated by the upgrade.
    #[serde(default = "control_tier")]
    tier: String,
}

fn control_tier() -> String {
    CONTROL_TIER.to_string()
}

/// A loaded token, reduced to its digest at construction.
///
/// Deliberately carries no `Display`/`Debug` of the secret and no accessor for
/// the plaintext: the only thing a holder can do is [`verify`](Self::verify).
#[derive(Clone)]
pub(crate) struct AuthToken {
    digest: [u8; 32],
}

impl std::fmt::Debug for AuthToken {
    /// Never renders the digest. A digest is not the token, but it is still a
    /// verifier — a log line carrying one lets an attacker confirm a guess
    /// offline.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AuthToken(<redacted>)")
    }
}

impl AuthToken {
    pub(crate) fn from_plaintext(token: &str) -> Self {
        Self {
            digest: digest_of(token),
        }
    }

    /// Constant-time check of a presented bearer token.
    ///
    /// Compares DIGESTS rather than the strings themselves, which makes the
    /// comparison length-independent for free: a presented token of the wrong
    /// length hashes to 32 bytes like any other, so neither its length nor the
    /// position of its first wrong byte is observable in the timing. A plain
    /// `==` on the strings would leak both.
    pub(crate) fn verify(&self, presented: &str) -> bool {
        bool::from(self.digest.ct_eq(&digest_of(presented)))
    }
}

fn digest_of(s: &str) -> [u8; 32] {
    Sha256::digest(s.as_bytes()).into()
}

fn token_path() -> Result<std::path::PathBuf> {
    Ok(clauth_dir()?.join(TOKEN_FILE))
}

/// A fresh token: 32 CSPRNG bytes through SHA-256, hex-encoded.
///
/// The hash is not there to protect the seed (nothing sees it); it is what
/// fixes the token at exactly 64 hex characters regardless of how the seed is
/// drawn, so the length check in [`read_valid`] is a real check.
fn generate() -> Result<String> {
    let mut seed = [0u8; 32];
    getrandom::fill(&mut seed).map_err(|e| anyhow::anyhow!("CSPRNG failure: {e}"))?;
    Ok(hex::encode(<[u8; 32]>::from(Sha256::digest(seed))))
}

/// True for the exact shape [`generate`] emits. Anything else is treated as no
/// token at all rather than trusted: a hand-edited or truncated file must not
/// silently become a weaker credential than the one the operator thinks is
/// installed.
pub(crate) fn is_well_formed(token: &str) -> bool {
    token.len() == TOKEN_LEN
        && token
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// The stored token, or `Ok(None)` when there is nothing usable on disk.
///
/// `Err` is reserved for the one case that must NOT be resolved by writing: a
/// `tier` this build does not know. Everything else — absent, unparseable,
/// truncated — is `Ok(None)`, which the caller is free to replace.
///
/// Quiet on purpose. [`current_or`] calls this once per request, so a line here
/// about a bad file would be a line per request; the replacement is announced by
/// [`load_or_create`], which is the only place a replacement actually happens.
fn read_valid() -> Result<Option<String>> {
    let path = token_path()?;
    let Ok(body) = std::fs::read_to_string(&path) else {
        return Ok(None);
    };
    let Ok(parsed) = serde_json::from_str::<AuthTokenFile>(&body) else {
        return Ok(None);
    };
    if parsed.schema > SCHEMA {
        // Written by a newer clauth. Refusing to reuse it would rotate the
        // operator's distributed token behind their back on a downgrade, so
        // take it if it still looks like a token and let the newer field set be.
        logline!(
            "clauth daemon: {TOKEN_FILE} is schema {} (this build knows {SCHEMA})",
            parsed.schema
        );
    }
    // A tier this build does not know is the one thing worth refusing to start
    // over. Serving it as `control` would silently promote a token a newer build
    // deliberately restricted, and replacing it would revoke, from a downgrade,
    // a credential the operator distributed on purpose. Neither is ours to pick.
    if parsed.tier != CONTROL_TIER {
        bail!(
            "{} carries tier {:?}, which this build does not know (it serves only {CONTROL_TIER:?}). \
             Run the clauth that wrote it, or delete the file to mint a fresh control token",
            path.display(),
            parsed.tier
        );
    }
    Ok(is_well_formed(&parsed.token).then_some(parsed.token))
}

/// The token as it stands on disk RIGHT NOW, falling back to `spawned` when the
/// file cannot be read.
///
/// `api::spawn` reads the token once and nothing re-read it, so
/// `clauth daemon --rotate-token` wrote a new token that the running daemon
/// never saw: it went on accepting the OLD one and 401ing the new one until
/// restart, while `--help`, `wiki/Daemon.md` and `SECURITY.md` all described it
/// as the response to a leaked bearer. That is the one control that has to work,
/// and `--rotate-token` conflicts with both `--replace` and `--listen`, so there
/// was no single command that rotated and restarted.
///
/// Read per request rather than cached behind an mtime check. The whole traffic
/// is a tray polling every 30s and a replica renewing a held request every 50s,
/// so one ~100-byte read and a SHA-256 per request is not a cost worth a cache,
/// a mutex, and a lock rank to avoid.
///
/// An unreadable or malformed file keeps `spawned` rather than locking every
/// client out: that is a broken deployment, and refusing everyone is strictly
/// worse than continuing on the token the daemon started with. It does mean a
/// DELETED file stops revoking — but deleting it needs the same filesystem
/// access that could read the token in the first place.
pub(crate) fn current_or(spawned: &AuthToken) -> AuthToken {
    read_valid()
        .ok()
        .flatten()
        .map_or_else(|| spawned.clone(), |t| AuthToken::from_plaintext(&t))
}

fn write(token: &str) -> Result<()> {
    let file = AuthTokenFile {
        schema: SCHEMA,
        token: token.to_string(),
        created_at: epoch_secs_to_iso((now_ms() / 1000) as i64),
        tier: control_tier(),
    };
    let path = token_path()?;
    atomic_write_600(&path, serde_json::to_vec_pretty(&file)?)
        .with_context(|| format!("failed to write {}", path.display()))
}

/// The persisted token, generating and storing one on first use.
///
/// Runs under the cross-process state flock so two instances starting together
/// cannot each generate a token and leave the loser's copy — already handed to
/// a client — silently invalid.
pub(crate) fn load_or_create() -> Result<String> {
    with_state_lock(|_| {
        let path = token_path()?;
        if let Some(token) = read_valid()? {
            return Ok(token);
        }
        // A file that EXISTED and was not usable is being replaced, and every
        // client holding the old token starts getting 401s. Silently is the
        // wrong way to do that: the operator's symptom would be a tray that
        // stopped working with nothing anywhere saying why.
        if path.exists() {
            logline!(
                "clauth daemon: {TOKEN_FILE} was unusable (bad JSON, or a token that is not \
                 64 lowercase hex characters) and has been replaced. Every client holding the \
                 old token now gets 401s — re-copy it with `clauth daemon --print-token`"
            );
        }
        let token = generate()?;
        write(&token)?;
        Ok(token)
    })
}

/// Replace the stored token with a fresh one. Every client holding the old one
/// starts getting 401s, which is the point.
pub(crate) fn rotate() -> Result<String> {
    with_state_lock(|_| {
        let token = generate()?;
        write(&token)?;
        Ok(token)
    })
}

#[cfg(test)]
#[path = "../../../tests/inline/daemon_api_token.rs"]
mod tests;
