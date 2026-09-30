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
//!   never retried on the routine leg — a DURABLE memo (a fingerprint file
//!   beside the store) remembers the token a tick spent its shot on, so even
//!   a daemon restart, which forgets every in-memory map, does not re-send a
//!   token the server may have already consumed. Only the health kick
//!   (401-triggered) may retry past the memo, one forced attempt per kick and
//!   two consecutive kicks at most. A verdict the server calls terminal
//!   leaves a quarantine record beside the memo, bound to the token it
//!   judged, and every surface that would hand the chain to a session — the
//!   walk, the feed, start, switch — reads it for as long as that token is
//!   the one in the store: browser re-login is the only exit.
//! - **Stand down where a live carrier holds the chain.** Under real symlinks
//!   a live session reads the very file a rotation writes, so the only race is
//!   codex's own five-minute pre-expiry window; under the fake transport the
//!   session holds a SEPARATE copy, so any rotation desyncs it and clauth
//!   stands down for the whole live session. A PARKED profile has no carrier
//!   to defer to — waiting there is how chains die at the wham 401.
//!
//! The belt (accepted on #51): every well-formed read of the store records a
//! last-known-good copy — atomic, 0600, never linked from any home, so it can
//! never be a second carrier — and it is restored only after the store reads
//! bad continuously for a real wall-clock window (past any single codex
//! truncate+write) with NO live session, since a live session IS the writer.
//! Those two gates are what make it never-worse-than-nothing: a slow write is
//! never mistaken for a corrupt file, and its rotated pair is never stomped by
//! the pre-rotation belt. The residual window is a crash between codex's
//! truncate and its write of a newly rotated pair with no session live — the
//! belt then holds the superseded token, which fails exactly the way doing
//! nothing fails.

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

/// codex's `TOKEN_REFRESH_INTERVAL` (`manager.rs:181`): the fallback age at
/// which a chain with an UNREADABLE access-token `exp` is due — matching
/// codex's own behavior rather than treating an unparseable JWT as due every
/// tick (which would spend a single-use token per second).
pub(crate) const CODEX_TOKEN_REFRESH_INTERVAL_MS: i64 = 8 * 24 * 60 * 60 * 1000;

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

    /// The account's fedramp routing flag, read from the stored tokens where
    /// codex's own client reads it (`login/src/token_data.rs`): its backend
    /// client adds `X-OpenAI-Fedramp: true` when the flag is set. A store
    /// without the flag is an ordinary account.
    pub(crate) fn is_fedramp(&self) -> bool {
        self.raw
            .get("tokens")
            .and_then(|t| t.get("chatgpt_account_is_fedramp"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
    }

    /// The id_token's `chatgpt_plan_type` claim, nested where
    /// `codex_login::chatgpt_account_id` reads the account id, through
    /// [`plan_word`]. The FALLBACK plan label for an account no poll has
    /// answered for yet (settled question 5): stale the moment a plan changes,
    /// unverified like every payload read here, and never an authorization
    /// input.
    pub(crate) fn id_token_plan(&self) -> Option<String> {
        plan_word(
            jwt_payload(self.token_str("id_token")?)?
                .get("https://api.openai.com/auth")?
                .get("chatgpt_plan_type")?
                .as_str()?,
        )
    }

    /// `last_refresh` as epoch ms — codex writes it as an RFC-3339 stamp. The
    /// fallback schedule signal when the access token's JWT `exp` is
    /// unreadable, exactly as codex's own manager falls back to it.
    pub(crate) fn last_refresh_ms(&self) -> Option<i64> {
        let raw = self.raw.get("last_refresh")?.as_str()?;
        chrono::DateTime::parse_from_rfc3339(raw)
            .ok()
            .map(|dt| dt.timestamp_millis())
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
    pub(crate) fn with_rotated(mut self, tok: &CodexTokenResponse, now_rfc3339: &str) -> Self {
        if let Some(obj) = self.raw.as_object_mut() {
            let tokens = obj.entry("tokens").or_insert_with(|| serde_json::json!({}));
            if let Some(t) = tokens.as_object_mut() {
                if let Some(id) = &tok.id_token {
                    t.insert("id_token".into(), serde_json::json!(id));
                }
                t.insert("access_token".into(), serde_json::json!(tok.access_token));
                t.insert("refresh_token".into(), serde_json::json!(tok.refresh_token));
            }
        }
        self.with_last_refresh(now_rfc3339)
    }

    /// `last_refresh` re-stamped to `now_rfc3339`, everything else
    /// byte-preserved: the stamp codex's `persist_tokens` writes on every chain
    /// event, which a capture is too.
    pub(crate) fn with_last_refresh(mut self, now_rfc3339: &str) -> Self {
        if let Some(obj) = self.raw.as_object_mut() {
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

/// The one normalizer for a ChatGPT plan word, wherever it was read (the
/// `wham/usage` `plan_type`, the id_token's `chatgpt_plan_type` claim):
/// trimmed and lowercased, and an empty word is no word, so every reader
/// falls back the same way on `""` as on an absent key.
pub(crate) fn plan_word(raw: &str) -> Option<String> {
    let plan = raw.trim().to_ascii_lowercase();
    (!plan.is_empty()).then_some(plan)
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
/// re-login, and everything else is a retry-later. Which verdicts quarantine
/// the chain is [`CodexRefreshError::quarantine_kind`]'s answer, not the
/// variant's.
#[derive(Debug)]
pub(crate) enum CodexRefreshError {
    /// The single-use token was already spent — a second carrier exists or a
    /// reply was lost. Browser re-login only.
    Reused,
    /// The chain aged out or was revoked server-side (`expired`,
    /// `invalidated`), or the token endpoint answered an unrecognized 4xx
    /// (`rejected`). Browser re-login for the first two.
    Dead(&'static str),
    /// Network/5xx/429 — nothing is known to have been consumed.
    Transient(String),
}

impl CodexRefreshError {
    /// The quarantine kind this verdict earns, or `None` for one that must
    /// not lock the chain out of start, switch and the walk: `Transient` by
    /// definition, and `Dead("rejected")` because an unrecognized 4xx is a
    /// statement about the front door (a WAF 403, a moved endpoint's 404, a
    /// contract-drift 400), never about the chain — it hits every profile at
    /// once, and a re-login line cannot help. `rejected` keeps what it had
    /// before the record existed: the memo blocks the routine leg, a kick may
    /// force one attempt, the breaker bounds the kicks. The three verdicts the
    /// server spells about the chain itself quarantine: `reused`, `expired`,
    /// `invalidated`.
    fn quarantine_kind(&self) -> Option<&'static str> {
        match self {
            CodexRefreshError::Reused => Some("reused"),
            CodexRefreshError::Dead(kind @ ("expired" | "invalidated")) => Some(kind),
            CodexRefreshError::Dead(_) | CodexRefreshError::Transient(_) => None,
        }
    }
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

/// Test-only [`CODEX_TOKEN_URL`] override: [`standby_tick`] hardwires the
/// production refresher, so its wire is unreachable offline without one.
/// Serialized by `profile::HOME_TEST_LOCK`. Never compiled into the binary.
#[cfg(test)]
static TOKEN_URL_OVERRIDE: Mutex<Option<String>> = Mutex::new(None);

#[cfg(test)]
pub(crate) fn set_token_url_override(url: &str) {
    if let Ok(mut guard) = TOKEN_URL_OVERRIDE.lock() {
        *guard = Some(url.to_string());
    }
}

#[cfg(test)]
pub(crate) fn clear_token_url_override() {
    if let Ok(mut guard) = TOKEN_URL_OVERRIDE.lock() {
        *guard = None;
    }
}

fn token_url() -> std::borrow::Cow<'static, str> {
    #[cfg(test)]
    if let Some(url) = TOKEN_URL_OVERRIDE.lock().ok().and_then(|g| g.clone()) {
        return std::borrow::Cow::Owned(url);
    }
    std::borrow::Cow::Borrowed(CODEX_TOKEN_URL)
}

pub(crate) fn refresh_codex_chain(
    refresh_token: &str,
) -> std::result::Result<CodexTokenResponse, CodexRefreshError> {
    refresh_codex_chain_at(&token_url(), refresh_token)
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

/// The first 8 bytes of a token's SHA-256, hex: what the memo and the
/// quarantine record hold in place of the token itself.
pub(crate) fn token_fingerprint(token: &str) -> String {
    use sha2::Digest as _;
    let mut h = sha2::Sha256::new();
    h.update(token.as_bytes());
    let out = h.finalize();
    out.iter().take(8).map(|b| format!("{b:02x}")).collect()
}

/// The no-replay memo is DURABLE — a small file beside the store holding the
/// fingerprint of the token a tick spent its shot on. In-memory alone would
/// re-arm the replay decision 7 calls permanent: a daemon restart forgets the
/// map, reads the OLD token still in the store (a failed refresh persisted
/// nothing), and re-sends it — but if the earlier attempt actually reached
/// the server and rotated (a lost response is indistinguishable from a
/// never-sent request), that re-send is a replay and the chain dies
/// permanently. Persisting the fingerprint closes that window across the one
/// thing that forgets: a restart. Always written and read under the rotation
/// guard, so there is no concurrent writer.
fn attempt_memo_path(name: &str) -> Result<std::path::PathBuf> {
    profile_subpath(&ProfileName::from(name), "auth.attempt")
}

fn attempted_matches(name: &str, token: &str) -> bool {
    let Ok(path) = attempt_memo_path(name) else {
        return false;
    };
    std::fs::read_to_string(&path)
        .map(|fp| fp.trim() == token_fingerprint(token))
        .unwrap_or(false)
}

/// Land the memo, or say which path refused. Fail-CLOSED at the call site: a
/// memo that did not reach disk protects nothing across a restart, so the
/// token it would have covered must not go on the wire.
fn mark_attempted(name: &str, token: &str) -> Result<()> {
    let path = attempt_memo_path(name)?;
    crate::profile::atomic_write_600(&path, token_fingerprint(token))
        .with_context(|| format!("failed to write the no-replay memo {}", path.display()))
}

fn clear_attempted(name: &str) {
    if let Ok(path) = attempt_memo_path(name) {
        let _ = std::fs::remove_file(path);
    }
}

/// Retire any no-replay memo for `name` — for an out-of-band store write
/// (capture/login) that installs a fresh chain the old memo has no claim on.
pub(crate) fn forget_attempt(name: &str) {
    clear_attempted(name);
}

// ── the quarantine record ───────────────────────────────────────────────────

/// A terminal verdict on a chain, bound to the refresh token it judged: `kind`
/// is `reused` (the replay answer), `expired` or `invalidated` — the three the
/// server spells about the chain itself ([`CodexRefreshError::quarantine_kind`]
/// says why `rejected` is not one) — or `lost`, clauth's own: the wire
/// accepted a rotation whose pair never reached the store, so the token the
/// store still holds is spent server-side. `at` is the RFC-3339 stamp of the
/// pass that heard it, `token_fingerprint` the judged token's, so the record
/// speaks only while that token is the one in the store. The codex twin of
/// `AppState::auth_broken`, kept as a file beside the store like the memo and
/// the belt rather than a slot in `codex-profiles.toml`: the verdict lands
/// under the rotation guard, on the standby thread, where the state flock is
/// not held.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct CodexQuarantine {
    pub(crate) kind: String,
    pub(crate) at: String,
    pub(crate) token_fingerprint: String,
}

fn quarantine_path(name: &str) -> Result<std::path::PathBuf> {
    profile_subpath(&ProfileName::from(name), "auth.quarantine.json")
}

/// The one reader every consumer goes through — the chain walk's `broken`
/// exclusion, the published `auth_status`, and the start/switch refusals.
/// `None` is "no verdict": never written, cleared by a rotation or a fresh
/// chain, a record that does not parse, or a record about a token the store
/// no longer holds — codex's own `codex login` through a still-linked slot
/// lands a fresh chain by a path that calls no clear, and the old chain's
/// verdict has no claim on it. A store that cannot be read keeps the record
/// standing: the verdict is about a chain that may still be there.
pub(crate) fn read_quarantine(name: &str) -> Option<CodexQuarantine> {
    let bytes = std::fs::read(quarantine_path(name).ok()?).ok()?;
    let record: CodexQuarantine = serde_json::from_slice(&bytes).ok()?;
    let current =
        read_store_auth(name).and_then(|auth| auth.refresh_token().map(token_fingerprint));
    match current {
        Some(fingerprint) if fingerprint != record.token_fingerprint => None,
        _ => Some(record),
    }
}

/// Written under the rotation guard the pass already holds, for the token
/// the pass judged. Best-effort like the belt: a failed write leaves the
/// chain viable to the walk, which the memo then keeps off the routine leg
/// and the breaker bounds. A standing record of the same kind on the same
/// token is kept, so `since` names the first verdict rather than the latest
/// kick's re-confirmation of it.
fn mark_quarantined(name: &str, kind: &str, at: &str, token: &str) {
    if read_quarantine(name).is_some_and(|q| q.kind == kind) {
        return;
    }
    let record = CodexQuarantine {
        kind: kind.to_string(),
        at: at.to_string(),
        token_fingerprint: token_fingerprint(token),
    };
    if let Ok(path) = quarantine_path(name)
        && let Err(e) =
            crate::profile::atomic_write_600(&path, serde_json::to_vec(&record).unwrap_or_default())
    {
        logline!("clauth: codex quarantine record write for '{name}' failed: {e}");
    }
}

/// Test seam for the Overview's broken marker: plant a quarantine record
/// against `name`'s CURRENT refresh token, so [`read_quarantine`] answers
/// `Some` exactly as a live verdict would. `at` is a fixed stamp — the
/// renderer reads only the record's presence.
#[cfg(test)]
pub(crate) fn quarantine_for_test(name: &str, kind: &str, token: &str) {
    mark_quarantined(name, kind, "1970-01-01T00:00:00Z", token);
}

/// Retire `name`'s quarantine record — a rotation that landed, or a
/// capture/login that installed a fresh chain the verdict has no claim on.
pub(crate) fn clear_quarantine(name: &str) {
    if let Ok(path) = quarantine_path(name) {
        let _ = std::fs::remove_file(path);
    }
}

/// Refuse `name` as a start or switch target while its quarantine record
/// stands, naming the one exit that is valid for every profile: the browser
/// mint. The bare capture is a no-op for an adopted slot (it finds the link
/// already naming this profile and captures nothing), and no refresh can
/// revive a chain the server declared dead.
pub(crate) fn refuse_if_quarantined(name: &str) -> Result<()> {
    if let Some(q) = read_quarantine(name) {
        anyhow::bail!(
            "'{name}': codex chain is broken ({} since {}), run `clauth login {name} --codex \
             --browser`",
            q.kind,
            q.at
        );
    }
    Ok(())
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

/// Bad-read strikes per profile, each carrying the wall-clock of the FIRST
/// strike. A strike lands only on a read that stayed bad under the rotation
/// guard, but the guard is not real confirmation on its own — codex takes no
/// such lock, and on a slow/NFS home one truncate+write can straddle several
/// adjacent ticks, so two microsecond-apart re-reads can BOTH fall inside one
/// in-flight write. The restore therefore also requires a real elapsed
/// interval and no live session (see [`BELT_CONFIRM_MS`] /
/// [`restore_confirmed`]).
static BAD_READS: Mutex<Option<HashMap<String, (u32, i64)>>> = Mutex::new(None);

/// The store must read bad continuously for at least this long before the belt
/// restores — generously past any single codex truncate+write, so a slow
/// write is never mistaken for a corrupt file.
const BELT_CONFIRM_MS: i64 = 30_000;

/// Register a confirmed-bad read at `now_ms`, returning `(strikes, first_ms)`.
fn bump_bad_read(name: &str, now_ms: i64) -> (u32, i64) {
    let mut g = match BAD_READS.lock() {
        Ok(g) => g,
        Err(_) => return (0, now_ms),
    };
    let m = g.get_or_insert_with(HashMap::new);
    let e = m.entry(name.to_string()).or_insert((0, now_ms));
    e.0 += 1;
    *e
}

fn clear_bad_reads(name: &str) {
    if let Ok(mut g) = BAD_READS.lock()
        && let Some(m) = g.as_mut()
    {
        m.remove(name);
    }
}

/// Whether a restore is now safe: at least two strikes, a real wall-clock gap
/// since the first (so an in-flight write has long since landed), and NO live
/// codex session — a live session IS the writer, and stomping its in-place
/// write with the pre-rotation belt is the one way the belt is worse than
/// doing nothing.
fn restore_confirmed(name: &str, strikes: u32, first_ms: i64, now_ms: i64) -> bool {
    strikes >= 2
        && now_ms - first_ms >= BELT_CONFIRM_MS
        && !crate::runtime::has_live_session(&ProfileName::from(name))
}

/// Restore the store from the belt — caller holds the rotation guard, and
/// [`restore_confirmed`] has ruled out a live writer.
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

/// The profile store's chain, read-only and guard-free. The usage leg needs the
/// access token and the account id to poll `wham/usage`, and reading takes no
/// rotation guard because it writes nothing — a torn read from codex's in-place
/// writer simply yields `None` and the poll is skipped for one interval, which
/// is the same "short or unparseable is RETRY, never gone" rule every reader
/// here follows.
pub(crate) fn read_store_auth(name: &str) -> Option<CodexAuth> {
    let store = profile_subpath(&ProfileName::from(name), "auth.json").ok()?;
    CodexAuth::parse(&std::fs::read(&store).ok()?).ok()
}

/// The plan label a surface publishes for `name`: `cached_plan` — the
/// `wham/usage` `plan_type` the last poll cached — is authoritative, and the
/// store's id_token claim stands in only while no poll has answered (settled
/// question 5). One rule for every surface that shows a codex tier.
pub(crate) fn plan_label(name: &str, cached_plan: Option<&str>) -> Option<String> {
    cached_plan
        .map(str::to_string)
        .or_else(|| read_store_auth(name)?.id_token_plan())
}

/// A wham/usage 401 for `name`: queue ONE forced refresh. The codex usage leg's
/// 401 arm is the production caller.
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

/// Whether `name` has a pending kick the breaker would still honor — a PEEK,
/// consuming nothing. The decision path peeks first (to know a memo-blocked
/// or not-yet-due chain is still worth pursuing) and only [`take_kick`]s once
/// it is actually about to hit the wire, so a stand-down or a busy guard
/// never burns the one forced attempt.
fn kick_available(name: &str) -> bool {
    KICKED
        .lock()
        .ok()
        .and_then(|g| {
            g.as_ref()
                .and_then(|m| m.get(name).map(|s| s.pending && s.strikes <= KICK_BREAKER))
        })
        .unwrap_or(false)
}

/// Whether `name`'s breaker has tripped: a kick still pending that
/// [`take_kick`] will never honor. The chain walk's `kick_rejected` reads this
/// — an account whose polls keep 401ing past two forced refreshes cannot serve
/// a session, whatever its cached window says — and only a successful poll
/// ([`kick_reset`]) clears it. A peek, consuming nothing.
pub(crate) fn kick_breaker_tripped(name: &str) -> bool {
    KICKED
        .lock()
        .ok()
        .and_then(|g| {
            g.as_ref()
                .and_then(|m| m.get(name).map(|s| s.pending && s.strikes > KICK_BREAKER))
        })
        .unwrap_or(false)
}

/// Hand back a kick [`take_kick`] consumed for an attempt that never reached
/// the wire (the memo did not land): the forced attempt is still owed, and the
/// strike was already counted when the kick arrived.
fn restore_kick(name: &str) {
    if let Ok(mut g) = KICKED.lock()
        && let Some(s) = g.as_mut().and_then(|m| m.get_mut(name))
    {
        s.pending = true;
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

/// Latch so a chain clauth cannot service logs its plight once, not every
/// tick (the daemon log would otherwise truncate). Keyed by profile and
/// plight — a store unreadable AND unrestorable, a memo path that refuses the
/// write — and cleared when that plight heals: a good read, a memo that lands.
static PLIGHT_WARNED: Mutex<Option<std::collections::HashSet<(String, &'static str)>>> =
    Mutex::new(None);

fn plight_warn_once(name: &str, plight: &'static str) -> bool {
    PLIGHT_WARNED
        .lock()
        .map(|mut g| {
            g.get_or_insert_with(Default::default)
                .insert((name.to_string(), plight))
        })
        .unwrap_or(false)
}

fn clear_plight_warn(name: &str, plight: &'static str) {
    if let Ok(mut g) = PLIGHT_WARNED.lock()
        && let Some(s) = g.as_mut()
    {
        s.remove(&(name.to_string(), plight));
    }
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
    /// A rotation ran and failed; the memo now blocks the token, and a
    /// server-spelled terminal verdict left a quarantine record (an
    /// unrecognized 4xx leaves none: `quarantine_kind`).
    Failed,
    /// The store read bad twice under the guard and the belt restored it.
    Restored,
    /// The no-replay memo could not be written, so nothing was sent; a kick
    /// this pass took is handed back.
    MemoFailed,
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
            clear_plight_warn(name, "corrupt");
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
                    let (strikes, first_ms) = bump_bad_read(name, now_ms);
                    if restore_confirmed(name, strikes, first_ms, now_ms) {
                        return match restore_from_lkg(name) {
                            Ok(()) => {
                                clear_bad_reads(name);
                                clear_plight_warn(name, "corrupt");
                                logline!(
                                    "clauth: '{name}' codex auth.json read bad for \
                                     {}s — restored the last-known-good copy",
                                    (now_ms - first_ms) / 1000
                                );
                                StandbyOutcome::Restored
                            }
                            Err(e) => {
                                // Latched: this line would otherwise repeat every
                                // tick until a re-login and truncate the log.
                                if plight_warn_once(name, "corrupt") {
                                    logline!(
                                        "clauth: '{name}' codex auth.json is unreadable and the \
                                         belt could not restore it: {e:#}"
                                    );
                                }
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

    // The age gate: due when the access token is inside the standby lead, or
    // — with an UNREADABLE exp — when `last_refresh` is older than codex's own
    // 8-day fallback interval. A pathological token with neither signal is NOT
    // due every tick (that would spend a single-use token per second); a 401
    // kick forces it if it is actually dead. A kick bypasses the gate and the
    // memo.
    let due = match auth.access_exp_ms() {
        Some(exp) => exp - now_ms < CODEX_STANDBY_LEAD_MS,
        None => auth
            .last_refresh_ms()
            .is_some_and(|lr| now_ms - lr >= CODEX_TOKEN_REFRESH_INTERVAL_MS),
    };
    let routine = due && !attempted_matches(name, &refresh_token);
    // Peek — do not consume — so a stand-down or a busy guard below never
    // burns the one forced attempt the kick buys.
    if !routine && !kick_available(name) {
        return StandbyOutcome::Idle;
    }

    // The stand-down. A rotation is unsafe whenever a live carrier holds the
    // same chain and could replay the token we spend. Two live-session cases:
    //  - fake transport: the session holds a physically SEPARATE copy of
    //    auth.json (decision 8's second-carrier case), so ANY rotation of the
    //    store desyncs it — stand down regardless of the window;
    //  - real symlinks: the session reads the very file we write (codex
    //    reloads before spending), so only codex's own pre-expiry window is a
    //    race — stand down just inside it.
    // A stand-down consumes no kick: a chain we cannot safely touch is not one
    // two forced attempts should count against.
    if crate::runtime::has_live_session(&ProfileName::from(name)) {
        let in_window = auth
            .access_exp_ms()
            .is_some_and(|exp| exp - now_ms < CODEX_SELF_REFRESH_WINDOW_MS);
        if crate::runtime::profile_uses_fake_transport(name) || in_window {
            return StandbyOutcome::StoodDown;
        }
    }

    let Ok(guard) = crate::runtime::RotationGuard::try_acquire(&ProfileName::from(name)) else {
        return StandbyOutcome::Idle;
    };
    let Some(_guard) = guard else {
        return StandbyOutcome::Idle;
    };

    // Test seam: a live codex rotates the store in exactly this window (after
    // the pre-guard capture, before the post-guard re-read), which the
    // re-read below must catch. Nothing outside tests sets this.
    #[cfg(test)]
    run_pre_reread_hook(name);

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
        // Somebody rotated in the window — the chain is fresh, nothing to do,
        // and a pending kick was chasing a now-stale 401, so retire it.
        kick_reset(name);
        return StandbyOutcome::Idle;
    }

    // Commit to the wire now — and only now consume the kick, iff this is a
    // FORCED attempt (a routine due-and-unmemoed pass needs none).
    let forced = if routine { false } else { take_kick(name) };
    if !routine && !forced {
        // The kick evaporated between the peek and here (a sibling tick took
        // it, or the breaker tripped) — nothing to do.
        return StandbyOutcome::Idle;
    }

    // No token reaches the wire without its memo on disk: a memo that failed
    // to land is exactly the restart hole the memo exists to close, so the
    // pass ends here rather than sending unprotected. The kick it took is
    // handed back — the forced attempt is still owed.
    if let Err(e) = mark_attempted(name, &fresh_token) {
        if forced {
            restore_kick(name);
        }
        // Latched: a path that stays unwritable would otherwise say so every
        // tick, since the chain stays due until the memo lands.
        if plight_warn_once(name, "memo") {
            logline!("clauth: codex refresh for '{name}' not sent: {e:#}");
        }
        return StandbyOutcome::MemoFailed;
    }
    clear_plight_warn(name, "memo");
    match refresher(&fresh_token) {
        Ok(tok) => {
            let rotated = auth.with_rotated(&tok, &now_rfc3339);
            match crate::profile::atomic_write_600(&store, rotated.to_bytes()) {
                Ok(()) => {
                    record_lkg(name, &rotated.to_bytes());
                    clear_attempted(name);
                    clear_quarantine(name);
                    // The kick's breaker is NOT reset here. A successful
                    // rotation only proves the TOKEN rotated — if the 401 that
                    // kicked us has a non-token cause (a suspended account),
                    // the next poll 401s again and re-kicks, and resetting
                    // here would force a fresh rotation every cycle forever,
                    // burning the single-use chain. Only a successful POLL
                    // (phase 5's wire) clears the breaker; a persistently
                    // 401ing account trips it after two forced attempts.
                    logline!("clauth: rotated codex chain for '{name}'");
                    StandbyOutcome::Rotated
                }
                Err(e) => {
                    // The rotated pair could not land: the OLD token is spent
                    // server-side and the new one exists only in memory. A
                    // terminal verdict this pass already knows, recorded
                    // against the token the store still holds so the walk,
                    // the feed and start/switch stop handing the chain out
                    // before a kick-forced retry earns the server's `reused`.
                    mark_quarantined(name, "lost", &now_rfc3339, &fresh_token);
                    logline!(
                        "clauth: '{name}' codex rotation succeeded on the wire but the \
                         store write failed ({e}); the chain is spent, run \
                         `clauth login {name} --codex --browser`"
                    );
                    StandbyOutcome::Failed
                }
            }
        }
        Err(e) => {
            // A terminal verdict is recorded, never merely logged: the walk,
            // the feed and the start/switch refusals all read the record, and
            // without it a dead chain stays viable to every one of them.
            if let Some(kind) = e.quarantine_kind() {
                mark_quarantined(name, kind, &now_rfc3339, &fresh_token);
            }
            logline!("clauth: codex refresh for '{name}' failed: {e}");
            StandbyOutcome::Failed
        }
    }
}

/// Test seam for the post-guard re-read: a hook, keyed by profile name, run
/// once between the pre-guard token capture and the post-guard re-read to
/// simulate a live codex rotating the store in that window.
#[cfg(test)]
pub(crate) type PreRereadHook = std::sync::Arc<dyn Fn() + Send + Sync>;

#[cfg(test)]
static PRE_REREAD_HOOK: Mutex<Option<HashMap<String, PreRereadHook>>> = Mutex::new(None);

#[cfg(test)]
pub(crate) fn set_pre_reread_hook(name: &str, f: PreRereadHook) {
    if let Ok(mut g) = PRE_REREAD_HOOK.lock() {
        g.get_or_insert_with(HashMap::new)
            .insert(name.to_string(), f);
    }
}

#[cfg(test)]
fn run_pre_reread_hook(name: &str) {
    let hook = PRE_REREAD_HOOK
        .lock()
        .ok()
        .and_then(|mut g| g.as_mut().and_then(|m| m.remove(name)));
    if let Some(f) = hook {
        f();
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
