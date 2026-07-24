//! Read-only lens over a codex `auth.json` (CDX-1 §0.3). The file is NEVER
//! reserialized through this type — codex adds fields fast (`agent_identity`,
//! `personal_access_token`, `bedrock_api_key` arrived recently) and dropping
//! unmodeled fields corrupts logins, so every store/switch/adopt copies raw
//! BYTES and this lens only answers questions about them. Identity, plan and
//! email all come from the stored JWTs — zero network.
//!
//! Schema reference: openai/codex @ `cbc83d9` / codex-cli 0.144.4
//! (`AuthDotJson` + `TokenData`); details in docs/codex-support/feasibility.md
//! §2.1/§2.4.

use serde_json::Value;

pub(crate) struct CodexAuthFile {
    raw: Value,
}

impl CodexAuthFile {
    /// Parse auth.json bytes. Errors on non-JSON / non-object content —
    /// callers decide whether that means "quarantine" or "leave alone".
    pub(crate) fn parse(bytes: &[u8]) -> anyhow::Result<Self> {
        let raw: Value =
            serde_json::from_slice(bytes).map_err(|e| anyhow::anyhow!("invalid auth.json: {e}"))?;
        anyhow::ensure!(raw.is_object(), "auth.json is not a JSON object");
        Ok(Self { raw })
    }

    fn token_str(&self, key: &str) -> Option<&str> {
        self.raw
            .get("tokens")?
            .get(key)?
            .as_str()
            .filter(|s| !s.is_empty())
    }

    /// The identity anchor. codex itself refuses to refresh across a changed
    /// `tokens.account_id`, so it is authoritative; the id_token claim is the
    /// fallback for files minted before the top-level copy existed.
    pub(crate) fn account_id(&self) -> Option<String> {
        if let Some(id) = self.token_str("account_id") {
            return Some(id.to_string());
        }
        let claims = jwt_claims(self.token_str("id_token")?)?;
        claims
            .get("https://api.openai.com/auth")?
            .get("chatgpt_account_id")?
            .as_str()
            .map(str::to_string)
    }

    pub(crate) fn access_token(&self) -> Option<&str> {
        self.token_str("access_token")
    }

    pub(crate) fn refresh_token(&self) -> Option<&str> {
        self.token_str("refresh_token")
    }

    /// True when the file holds anything worth protecting — an access or a
    /// refresh token. A tokenless shell can be overwritten without loss.
    pub(crate) fn has_login(&self) -> bool {
        self.access_token().is_some() || self.refresh_token().is_some()
    }

    /// Account email, from the id_token claims (top-level `email`, falling
    /// back to the `https://api.openai.com/profile` claim object).
    pub(crate) fn email(&self) -> Option<String> {
        let claims = jwt_claims(self.token_str("id_token")?)?;
        if let Some(email) = claims.get("email").and_then(Value::as_str) {
            return Some(email.to_string());
        }
        claims
            .get("https://api.openai.com/profile")?
            .get("email")?
            .as_str()
            .map(str::to_string)
    }

    /// Plan tier (`free|plus|pro|business|enterprise|edu`), from the id_token
    /// `https://api.openai.com/auth` claim.
    pub(crate) fn plan(&self) -> Option<String> {
        let claims = jwt_claims(self.token_str("id_token")?)?;
        claims
            .get("https://api.openai.com/auth")?
            .get("chatgpt_plan_type")?
            .as_str()
            .map(str::to_string)
    }

    /// Access-token expiry in epoch ms, from the JWT `exp` claim (unix secs).
    pub(crate) fn access_token_exp_ms(&self) -> Option<i64> {
        let claims = jwt_claims(self.access_token()?)?;
        claims.get("exp")?.as_i64()?.checked_mul(1000)
    }

    /// When the chain last advanced, in epoch ms — the top-level
    /// `last_refresh` RFC 3339 stamp codex resets on every refresh. Drives
    /// the CDX-3 standby keep-alive line. `None` when absent/unparseable.
    pub(crate) fn last_refresh_ms(&self) -> Option<u64> {
        let s = self.raw.get("last_refresh")?.as_str()?;
        let secs = crate::usage::iso_to_epoch_secs(s)?;
        u64::try_from(secs).ok()?.checked_mul(1000)
    }

    /// SipHash of the access token — cheap identity for "which login is this",
    /// mirroring `claude::live_credentials_fingerprint`. `None` when the file
    /// holds no non-empty access token.
    pub(crate) fn fingerprint(&self) -> Option<u64> {
        use std::hash::{DefaultHasher, Hash, Hasher};
        let token = self.access_token()?;
        let mut hasher = DefaultHasher::new();
        token.hash(&mut hasher);
        Some(hasher.finish())
    }
}

/// Decode a JWT's payload segment into its claims. No signature verification —
/// this only ever runs on tokens clauth itself stored, purely to read identity
/// metadata locally. Lenient: any malformed input → `None`.
pub(crate) fn jwt_claims(token: &str) -> Option<Value> {
    let payload = token.split('.').nth(1)?;
    let bytes = base64url_decode(payload)?;
    serde_json::from_slice(&bytes).ok()
}

/// Minimal base64url (RFC 4648 §5) decoder, padding-optional — JWTs are
/// unpadded. Kept dependency-free to match `oauth_login`'s encode-side
/// `base64url_nopad`; rejects any character outside the alphabet.
fn base64url_decode(input: &str) -> Option<Vec<u8>> {
    fn val(c: u8) -> Option<u32> {
        match c {
            b'A'..=b'Z' => Some((c - b'A') as u32),
            b'a'..=b'z' => Some((c - b'a' + 26) as u32),
            b'0'..=b'9' => Some((c - b'0' + 52) as u32),
            b'-' => Some(62),
            b'_' => Some(63),
            _ => None,
        }
    }
    let trimmed = input.trim_end_matches('=');
    let mut out = Vec::with_capacity(trimmed.len() * 3 / 4);
    for chunk in trimmed.as_bytes().chunks(4) {
        match chunk.len() {
            1 => return None, // a lone 6 bits can't form a byte
            n => {
                let mut acc: u32 = 0;
                for &c in chunk {
                    acc = (acc << 6) | val(c)?;
                }
                acc <<= 6 * (4 - n) as u32;
                let bytes = acc.to_be_bytes();
                // 4 chars → 3 bytes, 3 → 2, 2 → 1 (bytes[0] is always padding).
                out.extend_from_slice(&bytes[1..n]);
            }
        }
    }
    Some(out)
}

#[cfg(test)]
#[path = "../../tests/inline/codex_auth.rs"]
mod tests;
