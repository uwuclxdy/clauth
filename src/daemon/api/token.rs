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

use anyhow::{Context, Result};
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

/// `~/.clauth/auth_token.json`. `created_at` is informational — it answers "how
/// old is the credential I copied to that other box?" without a second file.
#[derive(Debug, Serialize, Deserialize)]
struct AuthTokenFile {
    schema: u64,
    token: String,
    created_at: String,
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
fn is_well_formed(token: &str) -> bool {
    token.len() == TOKEN_LEN
        && token
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// The stored token, or `None` when there is nothing usable on disk.
fn read_valid() -> Option<String> {
    let path = token_path().ok()?;
    let body = std::fs::read_to_string(&path).ok()?;
    let parsed: AuthTokenFile = serde_json::from_str(&body).ok()?;
    if parsed.schema > SCHEMA {
        // Written by a newer clauth. Refusing to reuse it would rotate the
        // operator's distributed token behind their back on a downgrade, so
        // take it if it still looks like a token and let the newer field set be.
        logline!(
            "clauth daemon: {TOKEN_FILE} is schema {} (this build knows {SCHEMA})",
            parsed.schema
        );
    }
    is_well_formed(&parsed.token).then_some(parsed.token)
}

fn write(token: &str) -> Result<()> {
    let file = AuthTokenFile {
        schema: SCHEMA,
        token: token.to_string(),
        created_at: epoch_secs_to_iso((now_ms() / 1000) as i64),
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
        if let Some(token) = read_valid() {
            return Ok(token);
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
