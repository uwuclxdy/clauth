//! The codex credential engine: the `auth.json` model, the standby refresh
//! with codex's own discipline, and the last-known-good belt.
//!
//! Codex refresh tokens are SINGLE-USE rotating, and the server answers a
//! replay with `refresh_token_reused` — permanent, browser-re-login-only
//! (decision 7). Three rules fall out, and everything here serves one of
//! them:
//!
//! - **Single writer.** Every clauth-side rotation runs under the profile's
//!   [`RotationGuard`](crate::runtime::RotationGuard); codex-vs-codex is
//!   handled by codex's own reload-and-skip, clauth-vs-codex by the guard.
//! - **No replay.** A failed refresh persists nothing and the SAME token is
//!   never retried on the next tick — the attempt memo remembers the token a
//!   tick spent its shot on, and only the health kick (401-triggered, phase
//!   5's wire) may retry it, twice at most.
//! - **Stand down where codex is about to act.** Codex refreshes within five
//!   minutes of the access token's JWT expiry; for a profile with a LIVE
//!   codex session, clauth entering that window is a race it loses by
//!   construction, so it does not enter. A parked profile has no codex to
//!   defer to, and waiting there is how chains die at the wham 401.
//!
//! The belt (accepted on #51): every well-formed read of the store records a
//! last-known-good copy — atomic, 0600, never linked from any home, so it can
//! never be a second carrier — and a store that reads bad twice in a row
//! ACROSS a guard acquisition (i.e. not a live in-place write half-done) is
//! restored from it. The window this cannot close: a crash between codex's
//! truncate and its write of a newly ROTATED pair leaves the belt holding the
//! superseded token, and restoring that hits `refresh_token_reused` — the
//! same death doing nothing hits, so the belt is strictly better than
//! nothing and never worse.

use crate::profile::ProfileName;
use std::collections::HashMap;
use std::sync::Mutex;

use anyhow::{Context, Result};

use crate::logline::logline;
use crate::profile::profile_subpath;

/// Codex's own OAuth client id — the spec's verified refresh contract.
pub(crate) const CODEX_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
/// The token endpoint, shared by refresh (JSON body) and the authorization-
/// code exchange (form-urlencoded) — same URL, different encodings, a trap
/// the spec pins.
pub(crate) const CODEX_TOKEN_URL: &str = "https://auth.openai.com/oauth/token";

/// Codex's own pre-expiry refresh window (`manager.rs:180`): a LIVE session
/// starts refreshing inside five minutes of the access token's `exp`.
pub(crate) const CODEX_SELF_REFRESH_WINDOW_MS: i64 = 5 * 60 * 1000;

/// How far ahead of expiry the standby leg rotates a chain. Twice codex's own
/// window: a PARKED profile has nothing else to keep its chain alive, and a
/// live-session profile still gets a guard-serialized rotation safely outside
/// the window codex itself acts in.
pub(crate) const CODEX_STANDBY_LEAD_MS: i64 = 10 * 60 * 1000;

/// A parsed view over a codex `auth.json`. The FULL value is kept and
/// rewritten — read-modify-write, never a typed round-trip — so every key a
/// newer codex writes survives a clauth rotation (the store-rewrite rule the
/// credential-install seam names).
#[derive(Debug, Clone)]
pub(crate) struct CodexAuth {
    raw: serde_json::Value,
}

impl CodexAuth {
    /// Parse a store read. `Err` is "this read is BAD" — short, torn, or not
    /// an object — which callers treat as retry-then-belt, never as "the
    /// chain is gone" (the spec's blanket rule for a file codex writes in
    /// place).
    pub(crate) fn parse(bytes: &[u8]) -> Result<Self> {
        let raw: serde_json::Value =
            serde_json::from_slice(bytes).context("auth.json did not parse")?;
        anyhow::ensure!(raw.is_object(), "auth.json is not an object");
        Ok(Self { raw })
    }

    fn token_str(&self, key: &str) -> Option<&str> {
        self.raw.get("tokens")?.get(key)?.as_str()
    }

    pub(crate) fn refresh_token(&self) -> Option<&str> {
        self.token_str("refresh_token").filter(|t| !t.is_empty())
    }

    pub(crate) fn access_token(&self) -> Option<&str> {
        self.token_str("access_token").filter(|t| !t.is_empty())
    }

    pub(crate) fn account_id(&self) -> Option<&str> {
        self.token_str("account_id").filter(|t| !t.is_empty())
    }

    /// The access token's `exp`, in epoch ms — an UNVERIFIED read of the JWT
    /// payload (clauth schedules by it, never trusts it for auth). `None` for
    /// a token that is not a parseable JWT, which callers treat the way codex
    /// itself does: fall back to age-based judgment rather than failing.
    pub(crate) fn access_exp_ms(&self) -> Option<i64> {
        jwt_exp_ms(self.access_token()?)
    }

    /// The rotated pair folded in: the three token slots and `last_refresh`,
    /// everything else byte-preserved. Mirrors codex's own `persist_tokens`.
    pub(crate) fn with_rotated(mut self, tok: &CodexTokenResponse, now_rfc3339: String) -> Self {
        if let Some(obj) = self.raw.as_object_mut() {
            let tokens = obj.entry("tokens").or_insert_with(|| serde_json::json!({}));
            if let Some(t) = tokens.as_object_mut() {
                if let Some(id) = &tok.id_token {
                    t.insert("id_token".into(), serde_json::json!(id));
                }
                t.insert("access_token".into(), serde_json::json!(tok.access_token));
                t.insert("refresh_token".into(), serde_json::json!(tok.refresh_token));
            }
            obj.insert("last_refresh".into(), serde_json::json!(now_rfc3339));
        }
        self
    }

    pub(crate) fn to_bytes(&self) -> Vec<u8> {
        // Compact like codex's own writer; the exact formatting is not a
        // contract, key survival is.
        serde_json::to_vec(&self.raw).unwrap_or_default()
    }
}

/// The decoded JWT payload, unverified. No signature check anywhere in this
/// module — payloads feed schedules and identity LABELS, never an
/// authorization decision.
pub(crate) fn jwt_payload(jwt: &str) -> Option<serde_json::Value> {
    let payload = jwt.split('.').nth(1)?;
    let bytes = base64url_decode_nopad(payload)?;
    serde_json::from_slice(&bytes).ok()
}

/// `exp` (epoch ms) out of an unverified JWT payload.
pub(crate) fn jwt_exp_ms(jwt: &str) -> Option<i64> {
    jwt_payload(jwt)?.get("exp")?.as_i64()?.checked_mul(1000)
}

/// Base64url (no padding) decode — the JWT alphabet. Hand-rolled because the
/// tree deliberately carries no base64 crate; the ENCODER lives in
/// `oauth_login::base64url_nopad`, and this is its inverse.
fn base64url_decode_nopad(s: &str) -> Option<Vec<u8>> {
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
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len() * 3 / 4);
    for chunk in bytes.chunks(4) {
        let mut acc: u32 = 0;
        for (i, &c) in chunk.iter().enumerate() {
            acc |= val(c)? << (18 - 6 * i);
        }
        let emit = match chunk.len() {
            4 => 3,
            3 => 2,
            2 => 1,
            _ => return None,
        };
        let all = [(acc >> 16) as u8, (acc >> 8) as u8, acc as u8];
        out.extend_from_slice(&all[..emit]);
    }
    Some(out)
}

/// A refresh outcome's failure half, classified the way codex's own manager
/// classifies the server's `error` field (spec, verified against
/// rust-v0.145.0): `refresh_token_reused` is PERMANENT — the replay answer —
/// `refresh_token_expired`/`refresh_token_invalidated` need a browser
/// re-login, and everything else is a retry-later.
#[derive(Debug)]
pub(crate) enum CodexRefreshError {
    /// The single-use token was already spent — a second carrier exists or a
    /// reply was lost. Browser re-login only.
    Reused,
    /// The chain aged out or was revoked server-side. Browser re-login.
    Dead(&'static str),
    /// Network/5xx/429 — nothing is known to have been consumed.
    Transient(String),
}

impl std::fmt::Display for CodexRefreshError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CodexRefreshError::Reused => f.write_str("refresh token already used"),
            CodexRefreshError::Dead(kind) => write!(f, "chain is dead ({kind})"),
            CodexRefreshError::Transient(e) => write!(f, "transient: {e}"),
        }
    }
}

/// The rotated pair a successful refresh returns.
#[derive(Debug, Clone, serde::Deserialize)]
pub(crate) struct CodexTokenResponse {
    pub(crate) id_token: Option<String>,
    pub(crate) access_token: String,
    pub(crate) refresh_token: String,
}

/// The refresh body — the spec's verified contract, exactly three fields:
/// JSON `{client_id, grant_type, refresh_token}`. Split out so the shape is
/// pinned as a value (the local stub records only request paths).
fn refresh_request_body(refresh_token: &str) -> serde_json::Value {
    serde_json::json!({
        "client_id": CODEX_CLIENT_ID,
        "grant_type": "refresh_token",
        "refresh_token": refresh_token,
    })
}

/// One live refresh against `url` — split from [`refresh_codex_chain`] so
/// tests drive the wire shape against a local stub.
pub(crate) fn refresh_codex_chain_at(
    url: &str,
    refresh_token: &str,
) -> std::result::Result<CodexTokenResponse, CodexRefreshError> {
    let body = refresh_request_body(refresh_token);
    let result = crate::oauth::http_agent()
        .post(url)
        .header("Content-Type", "application/json")
        .send(body.to_string());
    match result {
        Ok(mut resp) => {
            let status = resp.status().as_u16();
            let text = resp.body_mut().read_to_string().unwrap_or_default();
            if (200..300).contains(&status) {
                serde_json::from_str::<CodexTokenResponse>(&text).map_err(|e| {
                    CodexRefreshError::Transient(format!("token response did not parse: {e}"))
                })
            } else {
                Err(classify_refresh_failure(status, &text))
            }
        }
        Err(e) => Err(CodexRefreshError::Transient(e.to_string())),
    }
}

pub(crate) fn refresh_codex_chain(
    refresh_token: &str,
) -> std::result::Result<CodexTokenResponse, CodexRefreshError> {
    refresh_codex_chain_at(CODEX_TOKEN_URL, refresh_token)
}

/// Classify a non-2xx refresh reply. The `error` field is matched over the
/// whole body text rather than a parsed shape: the server has answered both
/// `{"error": "..."}` and nested variants, and a misparse must never turn a
/// PERMANENT verdict into a retry loop that replays a spent token.
fn classify_refresh_failure(status: u16, body: &str) -> CodexRefreshError {
    if body.contains("refresh_token_reused") {
        return CodexRefreshError::Reused;
    }
    if body.contains("refresh_token_expired") {
        return CodexRefreshError::Dead("expired");
    }
    if body.contains("refresh_token_invalidated") {
        return CodexRefreshError::Dead("invalidated");
    }
    if (400..500).contains(&status) && status != 429 {
        // An unrecognized 4xx on a single-use chain: treat as dead rather
        // than replay a token the server may have consumed.
        return CodexRefreshError::Dead("rejected");
    }
    CodexRefreshError::Transient(format!("HTTP {status}"))
}

// ── the attempt memo (no-replay) ─────────────────────────────────────────────

/// The refresh token each profile last spent a standby attempt on, by
/// fingerprint. An entry blocks the ROUTINE leg from replaying that token;
/// a successful rotation changes the token and the memo no longer matches;
/// the health kick consumes the entry deliberately. In-memory: the rule is
/// per-tick discipline, and a daemon restart retrying once is the claude
/// path's tolerated shape, not this one's steady state.
static ATTEMPTED: Mutex<Option<HashMap<String, String>>> = Mutex::new(None);

fn token_fingerprint(token: &str) -> String {
    use sha2::Digest as _;
    let mut h = sha2::Sha256::new();
    h.update(token.as_bytes());
    let out = h.finalize();
    out.iter().take(8).map(|b| format!("{b:02x}")).collect()
}

fn attempted_matches(name: &str, token: &str) -> bool {
    ATTEMPTED
        .lock()
        .ok()
        .and_then(|g| {
            g.as_ref()
                .and_then(|m| m.get(name).map(|fp| *fp == token_fingerprint(token)))
        })
        .unwrap_or(false)
}

fn mark_attempted(name: &str, token: &str) {
    if let Ok(mut g) = ATTEMPTED.lock() {
        g.get_or_insert_with(HashMap::new)
            .insert(name.to_string(), token_fingerprint(token));
    }
}

fn clear_attempted(name: &str) {
    if let Ok(mut g) = ATTEMPTED.lock()
        && let Some(m) = g.as_mut()
    {
        m.remove(name);
    }
}

// ── the last-known-good belt ────────────────────────────────────────────────

/// The belt file beside the store. Never linked from any home, never written
/// by codex — it cannot become a second carrier; it is only ever read to
/// repair a store that is already unusable.
fn lkg_path(name: &str) -> Result<std::path::PathBuf> {
    profile_subpath(&ProfileName::from(name), "auth.lkg.json")
}

/// Record a well-formed store read into the belt (atomic, 0600, only when
/// the bytes moved).
pub(crate) fn record_lkg(name: &str, bytes: &[u8]) {
    let Ok(path) = lkg_path(name) else { return };
    let same = std::fs::read(&path).map(|b| b == bytes).unwrap_or(false);
    if !same && let Err(e) = crate::profile::atomic_write_600(&path, bytes) {
        logline!("clauth: codex last-known-good write for '{name}' failed: {e}");
    }
}

/// Bad-read strikes per profile — a strike lands only on a read that stayed
/// bad UNDER the rotation guard (a live in-place write cannot span that), and
/// the second strike restores.
static BAD_READS: Mutex<Option<HashMap<String, u32>>> = Mutex::new(None);

fn bump_bad_read(name: &str) -> u32 {
    let mut g = match BAD_READS.lock() {
        Ok(g) => g,
        Err(_) => return 0,
    };
    let m = g.get_or_insert_with(HashMap::new);
    let n = m.entry(name.to_string()).or_insert(0);
    *n += 1;
    *n
}

fn clear_bad_reads(name: &str) {
    if let Ok(mut g) = BAD_READS.lock()
        && let Some(m) = g.as_mut()
    {
        m.remove(name);
    }
}

/// Restore the store from the belt — caller holds the rotation guard.
fn restore_from_lkg(name: &str) -> Result<()> {
    let bytes = std::fs::read(lkg_path(name)?).context("no last-known-good copy")?;
    CodexAuth::parse(&bytes).context("the last-known-good copy does not parse")?;
    let store = profile_subpath(&ProfileName::from(name), "auth.json")?;
    crate::profile::atomic_write_600(&store, &bytes)
        .with_context(|| format!("failed to restore {}", store.display()))
}

// ── the health kick ─────────────────────────────────────────────────────────

/// Profiles whose usage poll met a 401 — drained by the standby leg, which
/// force-refreshes them past the age gate AND the attempt memo, ONE attempt
/// per kick, at most [`KICK_BREAKER`] consecutive kicks. Reset by a
/// successful poll (phase 5's wire). Unrelated to the claude 5h-window kick,
/// by name and by mechanism.
static KICKED: Mutex<Option<HashMap<String, KickState>>> = Mutex::new(None);

#[derive(Default)]
struct KickState {
    /// A queued kick not yet spent on a forced attempt.
    pending: bool,
    /// Kicks received since the last successful poll.
    strikes: u32,
}

const KICK_BREAKER: u32 = 2;

/// A wham/usage 401 for `name`: queue ONE forced refresh. The codex usage
/// leg — the next commit in this series — is the production caller; until it
/// lands, the standby tests drive the whole kick path.
#[allow(
    dead_code,
    reason = "wired by the codex usage leg's 401 arm, the next commit in this series"
)]
pub(crate) fn kick_codex(name: &str) {
    if let Ok(mut g) = KICKED.lock() {
        let m = g.get_or_insert_with(HashMap::new);
        let s = m.entry(name.to_string()).or_default();
        s.pending = true;
        s.strikes = s.strikes.saturating_add(1);
    }
}

/// A successful poll for `name` — the breaker resets.
pub(crate) fn kick_reset(name: &str) {
    if let Ok(mut g) = KICKED.lock()
        && let Some(m) = g.as_mut()
    {
        m.remove(name);
    }
}

/// Consume `name`'s pending kick, if the breaker allows it: each kick buys
/// exactly one forced attempt, and past [`KICK_BREAKER`] consecutive kicks
/// nothing fires — a chain two forced refreshes did not heal needs a
/// re-login, not a third replay. The over-breaker entry stays so the state
/// is visible until a successful poll clears it.
fn take_kick(name: &str) -> bool {
    let Ok(mut g) = KICKED.lock() else {
        return false;
    };
    let Some(s) = g.as_mut().and_then(|m| m.get_mut(name)) else {
        return false;
    };
    if s.pending && s.strikes <= KICK_BREAKER {
        s.pending = false;
        return true;
    }
    false
}

// ── the standby leg ─────────────────────────────────────────────────────────

/// What one profile's standby pass decided — returned so the daemon leg and
/// the tests read the same verdicts.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum StandbyOutcome {
    /// Nothing to do: no store, no chain, not due, or memo-blocked.
    Idle,
    /// Stood down: a live codex session owns the pre-expiry window.
    StoodDown,
    /// A rotation ran and persisted.
    Rotated,
    /// A rotation ran and failed; the memo now blocks the token.
    Failed,
    /// The store read bad twice under the guard and the belt restored it.
    Restored,
}

/// One profile's standby pass. `refresher` is injected so every path is
/// testable offline; production passes [`refresh_codex_chain`].
pub(crate) fn standby_pass(
    name: &str,
    now_ms: i64,
    now_rfc3339: String,
    refresher: &dyn Fn(&str) -> std::result::Result<CodexTokenResponse, CodexRefreshError>,
) -> StandbyOutcome {
    let Ok(store) = profile_subpath(&ProfileName::from(name), "auth.json") else {
        return StandbyOutcome::Idle;
    };
    let bytes = match std::fs::read(&store) {
        Ok(b) => b,
        Err(_) => return StandbyOutcome::Idle,
    };
    let auth = match CodexAuth::parse(&bytes) {
        Ok(a) => {
            clear_bad_reads(name);
            record_lkg(name, &bytes);
            a
        }
        Err(_) => {
            // Re-read under the guard: a live in-place write cannot span the
            // acquisition, so a read that is STILL bad is a real casualty.
            let Ok(_guard) = crate::runtime::RotationGuard::try_acquire(&ProfileName::from(name))
            else {
                return StandbyOutcome::Idle;
            };
            let Some(_held) = _guard else {
                return StandbyOutcome::Idle;
            };
            match std::fs::read(&store).ok().and_then(|b| {
                let parsed = CodexAuth::parse(&b).ok()?;
                Some((b, parsed))
            }) {
                Some((b, _)) => {
                    clear_bad_reads(name);
                    record_lkg(name, &b);
                    return StandbyOutcome::Idle;
                }
                None => {
                    if bump_bad_read(name) >= 2 {
                        return match restore_from_lkg(name) {
                            Ok(()) => {
                                clear_bad_reads(name);
                                logline!(
                                    "clauth: '{name}' codex auth.json was unreadable twice — \
                                     restored the last-known-good copy"
                                );
                                StandbyOutcome::Restored
                            }
                            Err(e) => {
                                logline!(
                                    "clauth: '{name}' codex auth.json is unreadable and the \
                                     belt could not restore it: {e:#}"
                                );
                                StandbyOutcome::Idle
                            }
                        };
                    }
                    return StandbyOutcome::Idle;
                }
            }
        }
    };

    let Some(refresh_token) = auth.refresh_token().map(str::to_string) else {
        return StandbyOutcome::Idle;
    };

    let kicked = take_kick(name);
    // The age gate: due when the access token is inside the standby lead (or
    // its exp is unreadable — no schedule beats a dead chain), bypassed by a
    // kick. The memo blocks a routine replay; a kick spends its strike on
    // exactly that retry.
    let due = match auth.access_exp_ms() {
        Some(exp) => exp - now_ms < CODEX_STANDBY_LEAD_MS,
        None => true,
    };
    if !kicked && (!due || attempted_matches(name, &refresh_token)) {
        return StandbyOutcome::Idle;
    }

    // The stand-down, scoped exactly as accepted on #51: only a profile with
    // a LIVE codex session has a codex to lose the race to. The liveness
    // marker layout answers for codex sessions with no harness awareness.
    if crate::runtime::has_live_session(&ProfileName::from(name))
        && auth
            .access_exp_ms()
            .is_some_and(|exp| exp - now_ms < CODEX_SELF_REFRESH_WINDOW_MS)
    {
        return StandbyOutcome::StoodDown;
    }

    let Ok(guard) = crate::runtime::RotationGuard::try_acquire(&ProfileName::from(name)) else {
        return StandbyOutcome::Idle;
    };
    let Some(_guard) = guard else {
        return StandbyOutcome::Idle;
    };

    // Re-read under the guard: a live codex may have rotated while we
    // decided. Only the post-guard token feeds the wire.
    let bytes = match std::fs::read(&store) {
        Ok(b) => b,
        Err(_) => return StandbyOutcome::Idle,
    };
    let auth = match CodexAuth::parse(&bytes) {
        Ok(a) => a,
        Err(_) => return StandbyOutcome::Idle,
    };
    let Some(fresh_token) = auth.refresh_token().map(str::to_string) else {
        return StandbyOutcome::Idle;
    };
    if fresh_token != refresh_token {
        // Somebody rotated in the window — the chain is fresh, nothing to do.
        return StandbyOutcome::Idle;
    }

    mark_attempted(name, &fresh_token);
    match refresher(&fresh_token) {
        Ok(tok) => {
            let rotated = auth.with_rotated(&tok, now_rfc3339);
            match crate::profile::atomic_write_600(&store, rotated.to_bytes()) {
                Ok(()) => {
                    record_lkg(name, &rotated.to_bytes());
                    clear_attempted(name);
                    if kicked {
                        kick_reset(name);
                    }
                    logline!("clauth: rotated codex chain for '{name}'");
                    StandbyOutcome::Rotated
                }
                Err(e) => {
                    // The rotated pair could not land: the OLD token is spent
                    // server-side and the new one exists only in memory. Say
                    // exactly that — this is the one failure worth shouting.
                    logline!(
                        "clauth: '{name}' codex rotation SUCCEEDED on the wire but the \
                         store write failed ({e}); the chain will read as reused until \
                         a re-login — run `codex login` + `clauth login {name} --codex`"
                    );
                    StandbyOutcome::Failed
                }
            }
        }
        Err(e) => {
            logline!("clauth: codex refresh for '{name}' failed: {e}");
            StandbyOutcome::Failed
        }
    }
}

/// The daemon's codex leg: one standby pass per codex profile. Self-contained
/// — reads the codex roster itself, so the claude scheduler's state stays
/// untouched (per-harness independence, decision 4).
pub(crate) fn standby_tick(now_ms: i64, now_rfc3339: &str) {
    let Ok(state) = crate::codex_profiles::CodexState::load() else {
        return;
    };
    for name in state.profiles() {
        let _ = standby_pass(
            name.as_str(),
            now_ms,
            now_rfc3339.to_string(),
            &refresh_codex_chain,
        );
    }
}

#[cfg(test)]
#[path = "../tests/inline/codex_auth.rs"]
mod tests;
