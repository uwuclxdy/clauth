use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use chrono::{Datelike, Local, Weekday};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

/// Choice for auto-resolving credential divergence without the modal prompt.
/// Persisted in `AppState.default_divergence`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum DivergenceChoice {
    Overwrite,
    NewProfile,
    Discard,
}

use crate::harness::Harness;
use crate::lock::{StateLockHeld, with_state_lock};
use crate::logline::logline;
use crate::providers::{Provider, ThirdPartyStats};
use crate::usage::{UsageInfo, WalletSample};

/// A slot that reads like its inner value but writes only through the
/// witness-gated [`SlotOps::set`]. `active_profile` and `credentials` are
/// read-modify-written cross-process, so every write must run under the state
/// flock — [`AppState::set_active`] and [`Profile::set_credentials`] take the
/// [`StateLockHeld`] witness `with_state_lock` hands out, and a plain
/// `slot = value` assignment of a real value no longer compiles: the inner is
/// private to this module. `slot = Default::default()` still compiles — a
/// witness-free clear to the default that `#[serde(default)]` on
/// `active_profile` needs the derive for — and is the one write left outside
/// the witness.
///
/// Under `cfg(test)` the type is an alias for `T`, so fixtures build in-memory
/// states without staging a flock hold and existing test literals compile
/// unchanged. The witness signatures survive both builds; the gate's non-test
/// clippy leg is what enforces the write contract.
///
/// `DerefMut` is deliberately absent: it would let `*slot = value` assign the
/// inner without a witness. The `Debug` impl delegates so a derived log of
/// `AppState`/`Profile` renders exactly as it did before the wrapper.
#[cfg(not(test))]
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub(crate) struct LockedSlot<T>(T);

#[cfg(test)]
pub(crate) type LockedSlot<T> = T;

/// The write path and the move-out read, implemented for both shapes of
/// [`LockedSlot`] so the writers' bodies need no `cfg` splits.
pub(crate) trait SlotOps<T> {
    /// The one sanctioned write path: requires the state-flock witness, which
    /// is only handed out inside `with_state_lock`.
    fn set(&mut self, value: T, held: &StateLockHeld);

    /// Move the inner value out. Reads are not gated — the witness governs
    /// writes, and every reader of a cached credential already serializes its
    /// own snapshot under the config mutex.
    fn into_inner(self) -> T;
}

#[cfg(not(test))]
impl<T> SlotOps<T> for LockedSlot<T> {
    fn set(&mut self, value: T, _held: &StateLockHeld) {
        self.0 = value;
    }

    fn into_inner(self) -> T {
        self.0
    }
}

#[cfg(test)]
impl<T> SlotOps<Option<T>> for Option<T> {
    fn set(&mut self, value: Option<T>, _held: &StateLockHeld) {
        *self = value;
    }

    fn into_inner(self) -> Option<T> {
        self
    }
}

/// Build a slot value inside this module — the only construction path outside
/// serde, [`Default`], and [`Clone`]. Private so production code cannot mint a
/// slot to assign.
#[cfg(not(test))]
fn slot<T>(value: T) -> LockedSlot<T> {
    LockedSlot(value)
}

#[cfg(test)]
fn slot<T>(value: T) -> T {
    value
}

#[cfg(not(test))]
impl<T: std::fmt::Debug> std::fmt::Debug for LockedSlot<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

#[cfg(not(test))]
impl<T> std::ops::Deref for LockedSlot<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.0
    }
}

/// Newtype over `String` (transparent on disk). Makes every name-list mutation
/// compiler-checked — a rename that misses a `Vec` or the active marker is a
/// type error, not silent data drift. Derefs to `str` for existing lookups.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub(crate) struct ProfileName(String);

impl ProfileName {
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::ops::Deref for ProfileName {
    type Target = str;
    fn deref(&self) -> &str {
        &self.0
    }
}

impl std::borrow::Borrow<str> for ProfileName {
    fn borrow(&self) -> &str {
        &self.0
    }
}

impl AsRef<str> for ProfileName {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ProfileName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<String> for ProfileName {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl From<&str> for ProfileName {
    fn from(s: &str) -> Self {
        Self(s.to_string())
    }
}

impl PartialEq<str> for ProfileName {
    fn eq(&self, other: &str) -> bool {
        self.0 == other
    }
}

impl PartialEq<&str> for ProfileName {
    fn eq(&self, other: &&str) -> bool {
        self.0 == *other
    }
}

impl PartialEq<ProfileName> for str {
    fn eq(&self, other: &ProfileName) -> bool {
        self == other.0
    }
}

impl PartialEq<String> for ProfileName {
    fn eq(&self, other: &String) -> bool {
        &self.0 == other
    }
}

/// Newtype over `String` for a Claude account uuid — same pattern as
/// `ProfileName`, so the compiler catches `&str` / `AccountId` argument swaps
/// in identity comparisons. `#[serde(transparent)]` keeps the existing
/// `account_id.json` format (a bare JSON string) unchanged.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub(crate) struct AccountId(String);

impl AccountId {
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::ops::Deref for AccountId {
    type Target = str;
    fn deref(&self) -> &str {
        &self.0
    }
}

impl std::borrow::Borrow<str> for AccountId {
    fn borrow(&self) -> &str {
        &self.0
    }
}

impl AsRef<str> for AccountId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for AccountId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<String> for AccountId {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl From<&str> for AccountId {
    fn from(s: &str) -> Self {
        Self(s.to_string())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ClaudeCredentials {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) claude_ai_oauth: Option<OAuthToken>,
}

impl ClaudeCredentials {
    pub(crate) fn refresh_token(&self) -> Option<&str> {
        self.claude_ai_oauth.as_ref()?.refresh_token.as_deref()
    }

    pub(crate) fn access_token(&self) -> Option<&str> {
        Some(self.claude_ai_oauth.as_ref()?.access_token.as_str())
    }

    /// Epoch-ms the access token expires at, when known. Gates the auto-start
    /// kick's rotate-on-429: only a clock-expired token is worth rotating.
    pub(crate) fn access_token_expires_at(&self) -> Option<i64> {
        self.claude_ai_oauth.as_ref()?.expires_at
    }

    /// The granted OAuth scopes, space-joined — what Claude Code echoes in the
    /// `scope` field of a refresh request. `None` when unset or empty so the
    /// refresh path can fall back to the standard scope set.
    pub(crate) fn scopes_joined(&self) -> Option<String> {
        let scopes = self.claude_ai_oauth.as_ref()?.scopes.as_ref()?;
        if scopes.is_empty() {
            return None;
        }
        Some(scopes.join(" "))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct OAuthToken {
    pub(crate) access_token: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) refresh_token: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) expires_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) scopes: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) subscription_type: Option<String>,
    /// Every key of the login block this struct does not name, kept verbatim on
    /// both boundaries of every store rewrite. Claude Code writes fields of its
    /// own into `claudeAiOauth` (`rateLimitTier`, `refreshTokenExpiresAt`,
    /// `clientId`) and re-emits them only on a token save, so a typed model
    /// that silently dropped them made every clauth write of the store sticky
    /// (issue #75): the parse lost them before any merge could see them, and
    /// the serialize never emitted them. This is the login block's share of
    /// [`preserve_extra_blocks`]'s rule — the store's own value is the record,
    /// the model is a patch onto it — and it fixes the parse boundary too,
    /// which no write-side merge can reach: a first capture has no disk bytes
    /// to merge from.
    #[serde(flatten, skip_serializing_if = "serde_json::Map::is_empty")]
    pub(crate) extra: serde_json::Map<String, serde_json::Value>,
}

/// Claude Code's own key for the account's rate-limit tier inside `claudeAiOauth`
/// (`default_claude_max_5x` on a Team Premium seat). Claude Code stamps it from
/// `/profile` at login and reads it back as a feature-flag targeting attribute,
/// so a login block without it evaluates the server's plan-gated flags as an
/// untiered account. Lives in [`OAuthToken::extra`]: the field is Claude Code's,
/// not part of the model clauth owns.
pub(crate) const RATE_LIMIT_TIER_KEY: &str = "rateLimitTier";

impl OAuthToken {
    /// The stamped rate-limit tier, when the login block carries one.
    pub(crate) fn rate_limit_tier(&self) -> Option<&str> {
        self.extra.get(RATE_LIMIT_TIER_KEY).and_then(|v| v.as_str())
    }

    /// Stamp the rate-limit tier Claude Code would have written itself.
    pub(crate) fn set_rate_limit_tier(&mut self, tier: String) {
        self.extra.insert(
            RATE_LIMIT_TIER_KEY.to_string(),
            serde_json::Value::String(tier),
        );
    }

    /// `..Self::default_extra()` — the struct-update tail for every constructor
    /// minting a login from clauth's own flow, where no outside writer has put
    /// anything into the block yet. `Default` is deliberately not derived: the
    /// named fields carry no defaults (`access_token` has none), so this named
    /// constructor is the one spelling of "starts empty".
    pub(crate) fn default_extra() -> Self {
        Self {
            access_token: String::new(),
            refresh_token: None,
            expires_at: None,
            scopes: None,
            subscription_type: None,
            extra: serde_json::Map::new(),
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct Profile {
    pub(crate) name: ProfileName,
    pub(crate) base_url: Option<String>,
    pub(crate) api_key: Option<String>,
    /// Fires a 1-token Haiku ping each 30s tick while no 5h window is active.
    pub(crate) auto_start: bool,
    /// Opt-in usage probe for an account `/usage` cannot read: a
    /// `claude setup-token` login with no OAuth pair stored. A 1-token Haiku
    /// message every [`Profile::usage_probe_interval_ms`], read for the usage
    /// its response headers carry; it may start the 5h window, on a plan
    /// that has one. Inert while a pair is stored, since `/usage` reads
    /// that account.
    pub(crate) usage_probe: bool,
    /// The probe's cadence, inside [`MIN_USAGE_PROBE_INTERVAL_MS`]
    /// ..= [`MAX_USAGE_PROBE_INTERVAL_MS`] once loaded.
    pub(crate) usage_probe_interval_ms: u64,
    /// Extra env vars merged into `settings.json`'s `env` block while active.
    /// A switch away removes each one, or puts back the value the user's
    /// `~/.claude/settings.json` held under that key before it.
    pub(crate) env: BTreeMap<String, String>,
    /// Per-account Claude Code model configuration, written into this profile's
    /// runtime `settings.json` (and the live `~/.claude` settings while active).
    pub(crate) models: ModelSettings,
    /// Per-account Claude Code reasoning effort, written as
    /// `env.CLAUDE_CODE_EFFORT_LEVEL` in `settings.json` — the carrier a
    /// running CC 2.1.317 re-reads live; clauth never writes `effortLevel`.
    /// Unset = the key written empty.
    pub(crate) effort: Option<String>,
    /// Utilization % to auto-switch off at (fallback chain only). None = use default.
    pub(crate) fallback_threshold: Option<f64>,
    /// Per-account override of the chain-wide weekly (7d) switch line
    /// (`AppState::weekly_switch_threshold_pct`, the Config tab's `weekly
    /// limit`). None — the default — follows the chain-wide value. Applies to
    /// the aggregate 7d judgment (while `check_weekly` is on) AND to the
    /// per-model `weekly_scoped` windows (while `check_scoped` is on).
    pub(crate) weekly_threshold: Option<f64>,
    /// Chain-walk terminal stop (fallback chain only): once the auto-switch
    /// picker lands here with nothing else viable, it parks instead of turning
    /// off all accounts. Independent of `fallback_threshold` — this profile
    /// still switches away at its own threshold when another member has
    /// headroom (issue #8 follow-up: a threshold no longer doubles as a sink
    /// marker).
    pub(crate) last_resort: bool,
    /// The operator's home account (fallback chain only, at most one across the
    /// chain — the toggle is a radio, like `last_resort`). Opt-in return: once
    /// live work has drifted off it onto a later member and it reads clear and
    /// fresh again, the daemon walks the active — and every following session —
    /// back to it. Mutually exclusive with `last_resort`: a member marked one
    /// clears the other, since "park here to the end" and "always come home
    /// here" are contradictory verdicts. Default off. See
    /// `fallback::next_auto_switch_target`'s return-to-preferred pass.
    pub(crate) preferred: bool,
    /// Weekdays on which this account is the home account, read in the
    /// machine's local zone. Empty — the default — leaves `preferred` in
    /// charge. A named day is claimed against every account, not just this
    /// one, and `preferred` keeps the days no list claims: see
    /// [`AppConfig::is_home_on`], which is where the question is actually
    /// answered and which only lets serving chain members claim. Lets one account own the weekend without an external
    /// job rewriting `config.toml` twice a day.
    pub(crate) preferred_days: Vec<Weekday>,
    /// CLA-ROLL: the daemon re-stamps this profile's `session-token.json` with the
    /// usage chain's current access token on every rotation (full scopes,
    /// `subscriptionType` and `rateLimitTier`, no refresh token — sessions get
    /// plan-gated-model bearers while the refresh chain stays clauth-private).
    /// Off — the default — keeps the sidecar exactly what was captured (static
    /// mint).
    pub(crate) rolling_token: bool,
    /// Ceiling in US dollars on what the auto-switch chain may spend of this
    /// account's pay-as-you-go budget on its own (fallback chain only, and only
    /// while `AppState::spend_budget_switching` is on). `None`/`0` — the
    /// default — means the chain never picks this member for spend reasons, so
    /// stock behavior costs nothing. See `fallback::spend_armed`.
    pub(crate) max_auto_spend: Option<f64>,
    /// Whether auto-switching checks this account's aggregate weekly (7d)
    /// usage against the soft weekly line (fallback chain only). Off, only the
    /// hard cap (100%) blocks it — the account stays in rotation across the
    /// soft band. Default on (stock behavior).
    pub(crate) check_weekly: bool,
    /// Whether auto-switching checks this account's per-model weekly windows
    /// (e.g. "7d fable") against the weekly line (fallback chain only). On, a
    /// scoped window past the line keeps this account out of rotation — for
    /// sessions on that model it is as dead as a spent week. Off, scoped
    /// windows are ignored here: the account stays in rotation for use with
    /// other models. Default on.
    pub(crate) check_scoped: bool,
    /// Whether auto-switching checks this account's usage-credit cap
    /// (fallback chain only). On, a spent cap keeps this account out of the
    /// walk's target passes; off, the cap is ignored there. Default on.
    pub(crate) check_credit_cap: bool,
    /// 5h utilization % at/above which the alert fires: the TUI toast and the
    /// daemon's `bell_command`.
    /// None = no alert for this profile.
    pub(crate) bell_threshold: Option<f64>,
    /// USER CHOICE (not the auto-quarantine `AppState::auth_broken`): when
    /// true, this account is invisible to every operational surface — the
    /// fallback-chain walk, the usage/rotation scheduler, and the daemon
    /// status feed by default — while its profile directory and stored
    /// credentials stay on disk untouched. It still sits in `fallback_chain`
    /// on disk; only the walk skips it. Default off. See `Profile::is_disabled`.
    pub(crate) disabled: bool,
    /// Alibaba Model Studio console session, when one has been captured. The
    /// api key can't read quota, so this is what the Alibaba usage fetch runs
    /// on; `None` means the account renders no usage until a console login.
    pub(crate) console: Option<ConsoleCredential>,
    pub(crate) credentials: LockedSlot<Option<ClaudeCredentials>>,
    /// Recorded at load: the profile's `session-token.json` sidecar holds a
    /// genuine long-lived login ([`crate::claude::has_session_token`]). Loaded
    /// state like `credentials`, never serialized — only the loader reads the
    /// sidecar, so every walk site reads the fact off this field instead of a
    /// second per-member disk read. A struct literal / [`Profile::new`] leaves
    /// it `false` ("no sidecar").
    pub(crate) has_long_lived_sidecar: bool,
    /// Recorded at load: the profile's committed `credentials.json` exists but
    /// cannot be read, and no `.staged`/`.pending` sidecar supplied the pair.
    /// Loaded state like `has_long_lived_sidecar`, never serialized — the
    /// account reads logged out while its config and the torn bytes stay on
    /// disk. Carries the reason so the notice and the refusal can name it
    /// (parse failure vs the OS read error). A struct literal / [`Profile::new`]
    /// leaves it `None`.
    pub(crate) unreadable_credentials_store: Option<UnreadableStoreReason>,
    /// Recorded at load beside `has_long_lived_sidecar`: the digest of the
    /// setup token a switch would install from the sidecar, the credential a
    /// pairless account's sessions run on, so a refused one is matched against
    /// it with no disk read under the config guard. A re-mint moves the
    /// sidecar's write time, which reloads every holder. A struct literal /
    /// [`Profile::new`] leaves it `None`.
    pub(crate) setup_token_digest: Option<u64>,
    /// Recognised third-party provider (derived from base_url).
    pub(crate) provider: Option<Provider>,
    /// What `config.toml` typed for the endpoint keys the load derives, beside
    /// the derived values as of the last load or credentials change; in memory
    /// only. [`Profile::new`] and a struct literal hold `Default` (nothing
    /// typed).
    pub(crate) typed_endpoint: TypedEndpoint,
    /// The `[proxy]` record `config.toml` held when this profile loaded, read
    /// once there so the cache selector never parses the file per call. Never
    /// serialized: the table stays the one on-disk record, carried through every
    /// rewrite as an unmodelled key, and a writer of it sets this beside it.
    pub(crate) proxy: Option<ProxyAccount>,
    /// Recorded at load beside `proxy`: `config.toml` holds a `[proxy]` table
    /// that is no record ([`ProxyClaim::Unreadable`]), so `proxy` is `None`
    /// while the table still names an upstream account. Never serialized; a
    /// writer of the record clears it. A struct literal / [`Profile::new`]
    /// leaves it `false`.
    pub(crate) has_unreadable_proxy_record: bool,
}

/// The endpoint keys as typed, beside what clauth derived from them. `base_url`
/// is derived under the login state (`effective_base_url`), `[console]`
/// through `console_credential`; a save writes the typed value back unless the
/// caller moved the derived one off its snapshot, so a value clauth hid is
/// never read as the caller's edit.
#[derive(Clone, Default)]
pub(crate) struct TypedEndpoint {
    base_url: Option<String>,
    console: ConsoleConfig,
    derived_base_url: Option<String>,
    derived_console: Option<ConsoleCredential>,
}

impl std::fmt::Debug for TypedEndpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The typed `[console]` token is a credential: only its presence prints.
        f.debug_struct("TypedEndpoint")
            .field("base_url", &self.base_url)
            .field(
                "console_token",
                &self.console.token.as_ref().map(|_| "<redacted>"),
            )
            .field("console_site", &self.console.site)
            .field("console_region", &self.console.region)
            .field("derived_base_url", &self.derived_base_url)
            .field("derived_console", &self.derived_console)
            .finish()
    }
}

/// The one judgment behind both login reads — [`Profile::holds_login`] over
/// loaded state and [`stored_login`] over the committed store — so the
/// two cannot drift apart. Any one of the three facts is a login.
fn holds_login_facts(stored_pair: bool, long_lived_sidecar: bool, inference_auth: bool) -> bool {
    stored_pair || long_lived_sidecar || inference_auth
}

impl Profile {
    pub(crate) fn new(name: String, base_url: Option<String>, api_key: Option<String>) -> Self {
        let provider = base_url.as_deref().and_then(Provider::from_base_url);
        Self {
            name: name.into(),
            base_url,
            api_key,
            auto_start: false,
            usage_probe: false,
            usage_probe_interval_ms: DEFAULT_USAGE_PROBE_INTERVAL_MS,
            env: BTreeMap::new(),
            models: ModelSettings::default(),
            effort: None,
            fallback_threshold: None,
            weekly_threshold: None,
            last_resort: false,
            preferred: false,
            preferred_days: Vec::new(),
            rolling_token: false,
            max_auto_spend: None,
            check_weekly: true,
            check_scoped: true,
            check_credit_cap: true,
            bell_threshold: None,
            disabled: false,
            console: None,
            credentials: slot(None),
            has_long_lived_sidecar: false,
            unreadable_credentials_store: None,
            setup_token_digest: None,
            provider,
            typed_endpoint: TypedEndpoint::default(),
            proxy: None,
            has_unreadable_proxy_record: false,
        }
    }

    pub(crate) fn is_oauth(&self) -> bool {
        self.base_url.is_none()
    }

    /// Credential typing: which stored credential the login / log-out surfaces
    /// act on. A profile can hold both an OAuth pair and a `base_url` (capture
    /// reads the two live files independently; setting an endpoint never drops
    /// stored credentials), and on such a hybrid the pair is the thing a log out
    /// has to clear — otherwise a live token sits on disk behind a logged-out
    /// UI. Endpoint routing is a different question and no method here answers
    /// it: [`Profile::is_oauth`] reads the managed `base_url` field alone; an
    /// operator-authored `[env] ANTHROPIC_BASE_URL` routes requests even with
    /// no `base_url` set. A caller asking where requests go asks
    /// [`stored_endpoint`], which reads both sources, or
    /// [`Profile::routing_endpoint`] for a profile already in hand.
    pub(crate) fn login_is_oauth(&self) -> bool {
        self.credentials.is_some() || self.is_oauth()
    }

    /// "Holds a login" — the ONE predicate every automatic walk reads to keep
    /// a login-less member from being linked. A stored OAuth pair (a
    /// `claude_ai_oauth` block in `credentials`, the shape [`load_profile`]
    /// puts there), a long-lived `session-token.json` sidecar (recorded on
    /// [`Profile::has_long_lived_sidecar`] at load), or a usable inference
    /// credential ([`crate::claude::has_inference_auth`]: an api key or an
    /// `[env]` inference token). Loaded-state only — no disk read — so a walk
    /// never re-reads `session-token.json` per member.
    pub(crate) fn holds_login(&self) -> bool {
        holds_login_facts(
            self.credentials
                .as_ref()
                .is_some_and(|c| c.claude_ai_oauth.is_some()),
            self.has_long_lived_sidecar,
            crate::claude::has_inference_auth(self),
        )
    }

    /// The endpoint a LOADED profile routes its requests to, env half first:
    /// the same two sources and the same precedence [`stored_endpoint`] reads
    /// off disk, answered off the profile in hand. The order mirrors the
    /// producer: `build_claude_settings_json` writes the managed `base_url`
    /// into the settings env block and applies `profile.env` last, so an
    /// explicit `ANTHROPIC_BASE_URL` there is what the spawned `claude` reads.
    /// An entry that is blank once trimmed is no override, the same test
    /// [`stored_endpoint`] applies.
    pub(crate) fn routing_endpoint(&self) -> Option<&str> {
        self.env
            .get("ANTHROPIC_BASE_URL")
            .map(|v| v.trim())
            .filter(|v| !v.is_empty())
            .or(self.base_url.as_deref())
    }

    /// Whether this account's endpoint is one clauth has a TYPED integration
    /// for. Answers "is this a recognised provider", nothing else — a generic
    /// api-key endpoint is `false` here while still being an api-key account in
    /// every other sense. For "where do this account's usage figures live", ask
    /// [`Profile::usage_cache_is_third_party`], which is a wider set.
    pub(crate) fn is_third_party(&self) -> bool {
        self.provider.is_some()
    }

    /// Whether this account's inference needs a key it does not carry
    /// ([`crate::claude::has_inference_auth`]), so a spawned `claude` would die
    /// on an empty envelope. Needing one: a recognised provider's endpoint, or
    /// a proxy account ([`Profile::proxy`]) routed to a loopback bind
    /// ([`Profile::routing_endpoint`]), which its proxy serves on the
    /// inference key it minted alone. An unrecognised endpoint needs none here,
    /// since it may be a local model taking no key, and so does a proxy account
    /// whose `[env] ANTHROPIC_BASE_URL` routes it elsewhere. A proxy record the
    /// load boundary re-read as an OAuth login (a stored pair, no key) loses
    /// its managed bind, so it is keyless only where that override still
    /// routes it to one.
    pub(crate) fn is_keyless(&self) -> bool {
        let needs_key = self.is_third_party()
            || (self.proxy.is_some()
                && self
                    .routing_endpoint()
                    .and_then(crate::proxy::loopback_port)
                    .is_some());
        needs_key && !crate::claude::has_inference_auth(self)
    }

    /// Whether this account's usage figures live in `third_party_cache.json`
    /// rather than the OAuth `usage_cache.json` — the question every reader of a
    /// cached figure asks, and the one the scheduler answers by writing.
    ///
    /// True for a recognised provider AND for a generic api-key endpoint, whose
    /// discovered usage is cached the same way: `third_party_entry_for` builds a
    /// `ThirdPartyTarget::Generic` whenever `provider` is `None` and a
    /// `base_url` is set, so that leg genuinely fetches and caches for it.
    /// True too for a proxy account on its loopback bind, keyed or keyless,
    /// by the record it loaded with ([`Profile::proxy`]): a keyless one is
    /// never polled, and its last copy still renders, aging to stale.
    /// A stored pair with an env token serving inference and nothing that leg
    /// could fetch with answers false: the chain feeds usage polling for such
    /// an account (the env-token account's inference spends on the token
    /// while the pair serves usage), and the OAuth leg writes the figures
    /// this reader should read. A proxy account of that shape answers false
    /// too, though no leg polls it ([`Profile::spends_oauth_chain`]).
    /// Keying a reader on [`Profile::is_third_party`] instead answers a
    /// DIFFERENT question and renders a generic account, refreshed hourly, as
    /// never fetched.
    pub(crate) fn usage_cache_is_third_party(&self) -> bool {
        usage_cache_is_third_party(
            self.provider,
            self.base_url.as_deref(),
            self.api_key.as_deref(),
            self.credentials.is_some(),
            &self.env,
            // The record as `load_profile` read it, so no call reads the file.
            || self.proxy.is_some(),
        )
    }

    /// The vendor console page where this account's api key is minted, for a
    /// surface offering to open it. `None` for an OAuth account and for an
    /// endpoint no provider claims, neither of which clauth knows a page for.
    ///
    /// Reads the two fields that were derived together, so the page always
    /// belongs to the endpoint the account actually calls.
    pub(crate) fn console_url(&self) -> Option<&'static str> {
        self.provider?.console_url(self.base_url.as_deref()?)
    }

    /// The console front this account's usage session is captured from, or
    /// `None` when its usage needs no console. `Some` is what makes a login on
    /// this account the CONSOLE login rather than an api-key or OAuth one.
    ///
    /// One predicate for every surface that asks: the CLI's login gate, the TUI
    /// row that runs it, and that row's own hint and label. They disagreed once
    /// already — the row ran the console flow while its hint still described the
    /// api-key re-entry.
    pub(crate) fn console_login_target(&self) -> Option<(ConsoleSite, &'static str)> {
        if self.provider != Some(Provider::Alibaba) {
            return None;
        }
        crate::providers::alibaba::site_and_region(self.base_url.as_deref()?)
    }

    /// User-disabled (see [`Profile::disabled`]) — never `auth_broken`'s
    /// auto-quarantine, always an operator's own choice.
    pub(crate) fn is_disabled(&self) -> bool {
        self.disabled
    }

    /// Whether this account has a Claude Code login clauth may use: poll
    /// `/usage` on its access token, kick a window on it, refresh, rotate,
    /// adopt or stamp it, probe with its setup token, or quarantine it on a
    /// refusal. Never for a proxy account ([`Profile::proxy`]): it sends every
    /// request with the key its proxy minted and never uses the chain, so a
    /// pair stored beside its record stays on disk untouched. A `[proxy]`
    /// table clauth cannot read ([`Profile::has_unreadable_proxy_record`]) is a
    /// proxy account here too, as it is to the deletes and the re-login. The
    /// one decision the OAuth leg's poll pair (`usage::usage_poll_pair`, which
    /// seeds the scheduler's work list), the usage probe
    /// (`usage::usage_probe_admits`), the live-mirror adopt
    /// (`oauth::try_adopt_live_rotation`), the switch gate
    /// (`oauth::oauth_shape`), its rolling stamp
    /// (`oauth::roll_from_stored_chain`), the start-time rolling stamp
    /// (`claude::arm_rolling_from_disk_synced`), rotate-all
    /// (`oauth::rotation_candidates`), the rotate worker
    /// (`oauth::rotate_one_inner`), the auto-start opt-in
    /// ([`Profile::has_effective_auto_start`]: the switch kick
    /// `oauth::prime_window`, the auto-start queue
    /// `usage::auto_start_queue_members`, the Usage tab's auto-start countdown,
    /// the Config tab's `auto-start queue` row and its key, the scheduler's
    /// weekly-reset mark `usage::scheduler::drain_oauth_completions`), the token-mode
    /// commands and the TUI clear's confirm
    /// (`actions::refuse_token_mode_on_proxy`), `clauth limit-reset`
    /// (`actions::refuse_limit_reset_on_proxy`), the status feed's refresh
    /// countdown (`daemon::status_json::build_profile_entries`), the Usage
    /// tab's empty body, the Setup tab's auto-start, usage-probe and clear
    /// rows (`tui::app::config_rows`), its auto-start toggle, `build_snap`'s
    /// log-out hint, the session env (`claude::session_env`) and the TUI's
    /// rotate action read. Memory can lag the record on disk, so every write,
    /// spend, mark, adopt and delete also takes the decision off the disk
    /// through [`stored_proxy_claim_failing_closed`]: the stage landing every
    /// lander shares (`oauth::land_pair`), the send rule every refresh asks
    /// (`oauth::send_rule`), the kick (`oauth::auto_start_kick`), the rolling
    /// stamp, the live-mirror adopt, the weekly-reset mark's confirmation, the
    /// Claude Code login adopts (`claude::adopt_skips_proxy_account`), a
    /// session's drain (`runtime::keeps_login_off_proxy_store`) and the
    /// deletes.
    pub(crate) fn spends_oauth_chain(&self) -> bool {
        self.proxy.is_none() && !self.has_unreadable_proxy_record
    }

    /// Whether this account's stored `auto_start` opts it into anything: the
    /// flag on an account that spends its own OAuth chain. A proxy account
    /// carrying the flag (set before it became one, or by a hand edit) opts
    /// into no kick and no queue seat, so it never makes the queue look
    /// populated either. Surfaces that show or edit the stored flag itself
    /// read [`Profile::auto_start`] instead.
    pub(crate) fn has_effective_auto_start(&self) -> bool {
        self.auto_start && self.spends_oauth_chain()
    }

    pub(crate) fn refresh_token(&self) -> Option<&str> {
        self.credentials.as_ref()?.refresh_token()
    }

    pub(crate) fn access_token(&self) -> Option<&str> {
        self.credentials.as_ref()?.access_token()
    }

    pub(crate) fn access_token_expires_at(&self) -> Option<i64> {
        self.credentials.as_ref()?.access_token_expires_at()
    }

    /// Granted OAuth scopes space-joined (see [`ClaudeCredentials::scopes_joined`]).
    pub(crate) fn scopes_joined(&self) -> Option<String> {
        self.credentials.as_ref()?.scopes_joined()
    }

    /// Replace the stored credential set. Requires the state-flock witness:
    /// the slot is read-modify-written cross-process, so an unlocked write can
    /// clobber a concurrent rotation. Any install or blanking replaces whatever
    /// bytes the disk held, so the unreadable-store fact clears with it.
    ///
    /// `base_url` follows the login state, so the store is written first and
    /// then re-derived, the way the next load derives it: a login hides a
    /// keyless URL and a logout makes it live, in memory as on disk.
    pub(crate) fn set_credentials(
        &mut self,
        credentials: Option<ClaudeCredentials>,
        held: &StateLockHeld,
    ) {
        self.unreadable_credentials_store = None;
        self.credentials.set(credentials, held);
        self.rederive_base_url();
    }

    /// Derive `base_url` and `provider` again from the typed value after a
    /// change to what decides them — the login, `api_key`, `[env]` — the way
    /// the next load derives them: a keyless URL under a login hides in memory
    /// as on disk, and an inference token beside it makes it live again.
    pub(crate) fn rederive_base_url(&mut self) {
        self.fold_base_url_edit();
        // A torn store counts as a login, as the load counts it.
        let has_login = self.credentials.is_some() || self.unreadable_credentials_store.is_some();
        let base_url = self.derived_base_url(has_login);
        if base_url != self.base_url {
            self.provider = base_url.as_deref().and_then(Provider::from_base_url);
        }
        self.typed_endpoint.derived_base_url.clone_from(&base_url);
        self.base_url = base_url;
    }

    /// Set `base_url` to the caller's own value. A value that differs from what
    /// clauth derived becomes the typed value; one equal to it leaves the typed
    /// value as it was, so re-committing a URL clauth hides keeps the line. A
    /// caller replacing the endpoint outright wants
    /// [`Profile::replace_base_url`].
    pub(crate) fn set_base_url(&mut self, base_url: Option<String>) {
        self.base_url = base_url;
        self.fold_endpoint_edits();
    }

    /// Replace `base_url` with a value the caller states outright (a captured
    /// endpoint): it becomes the typed value even where it equals the derived
    /// one, so a snapshot carrying a key and no URL drops the stored host
    /// instead of pairing its key with it.
    pub(crate) fn replace_base_url(&mut self, base_url: Option<String>) {
        self.set_base_url(base_url.clone());
        self.typed_endpoint.base_url = base_url;
    }

    /// Set the console session to the caller's own value, as
    /// [`Profile::set_base_url`] does for `base_url`.
    pub(crate) fn set_console(&mut self, console: Option<ConsoleCredential>) {
        self.console = console;
        self.fold_endpoint_edits();
    }

    /// The `base_url` this profile routes to once its login state is
    /// `has_credentials`, derived from what a save would write, the way the
    /// next load derives it.
    pub(crate) fn derived_base_url(&self, has_credentials: bool) -> Option<String> {
        effective_base_url(
            self.saved_base_url(),
            has_credentials,
            self.api_key.as_deref(),
            &self.env,
        )
    }

    /// The `base_url` a save writes: the in-memory value when the caller moved
    /// it off the derived snapshot, else the typed one.
    pub(crate) fn saved_base_url(&self) -> Option<String> {
        if self.base_url == self.typed_endpoint.derived_base_url {
            self.typed_endpoint.base_url.clone()
        } else {
            self.base_url.clone()
        }
    }

    /// The `[console]` table a save writes, by the same rule as
    /// [`Profile::saved_base_url`].
    fn saved_console(&self) -> ConsoleConfig {
        if self.console == self.typed_endpoint.derived_console {
            self.typed_endpoint.console.clone()
        } else {
            console_config(self.console.as_ref())
        }
    }

    /// Make a caller's edit of `base_url` the typed value and its new snapshot.
    fn fold_base_url_edit(&mut self) {
        self.typed_endpoint.base_url = self.saved_base_url();
        self.typed_endpoint
            .derived_base_url
            .clone_from(&self.base_url);
    }

    /// Make a caller's edit of an endpoint key the typed value and its new
    /// snapshot.
    fn fold_endpoint_edits(&mut self) {
        self.fold_base_url_edit();
        self.typed_endpoint.console = self.saved_console();
        self.typed_endpoint
            .derived_console
            .clone_from(&self.console);
    }
}

/// Theme tier a `profiles.toml` theme name maps to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ThemeName {
    Full,
    Compatible,
}

impl ThemeName {
    const ALL: [ThemeName; 2] = [ThemeName::Full, ThemeName::Compatible];

    fn as_str(self) -> &'static str {
        match self {
            ThemeName::Full => "full",
            ThemeName::Compatible => "compatible",
        }
    }
}

/// Theme selection in `profiles.toml`: the scalar `theme = "full"` or the table
/// `[theme] name = "full"`. The form the file held is kept, so a save never
/// rewrites an older reader's scalar into a form it refuses; a pick writes the
/// table, and a name this build does not know rides every save verbatim.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged, expecting = "a theme name, or a [theme] table with a name")]
pub(crate) enum ThemeSetting {
    Scalar(String),
    Table { name: String },
}

impl AppState {
    /// The configured theme tier, `None` when the file names none this build
    /// knows.
    pub(crate) fn theme_tier(&self) -> Option<ThemeName> {
        let name = match self.theme.as_ref()? {
            ThemeSetting::Scalar(name) | ThemeSetting::Table { name } => name,
        };
        ThemeName::ALL
            .into_iter()
            .find(|tier| tier.as_str() == name.as_str())
    }

    /// Record a theme pick for the next save.
    pub(crate) fn set_theme(&mut self, name: ThemeName) {
        self.theme = Some(ThemeSetting::Table {
            name: name.as_str().to_string(),
        });
    }
}

/// How a usage window's reset renders across the TUI (`AppState.reset_display`,
/// issue #39). `Relative` is the shipped default and the pre-setting behavior,
/// byte for byte.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum ResetDisplay {
    /// `resets in 40m` — countdown only.
    #[default]
    Relative,
    /// `resets at 21:20` — wall-clock stamp only.
    Clock,
    /// `resets in 40m (21:20)` — both.
    Both,
}

impl ResetDisplay {
    /// Whether this mode renders a wall-clock stamp — the gate on the `clock`
    /// Config row and on the wider overview reset column.
    pub(crate) fn shows_clock(self) -> bool {
        matches!(self, ResetDisplay::Clock | ResetDisplay::Both)
    }
}

/// Wall-clock notation for the stamp [`ResetDisplay`] renders
/// (`AppState.clock_format`). Defaults to 24-hour, matching the only other
/// clock in the tree (`crate::format::local_stamp`, the status tab's incident
/// stamps, which are fixed 24-hour by the prose-stamp ruling).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum ClockFormat {
    /// `21:20`
    #[default]
    #[serde(rename = "24h")]
    H24,
    /// `9:20pm`
    #[serde(rename = "12h")]
    H12,
}

/// The tab a launch opens on: the Config tab's `home tab` row, persisted as a
/// top-level `home_tab` key in profiles.toml beside `theme` /
/// `reset_display` / `clock_format`. Read by the TUI at construction; the
/// first herdr launch overrides it (Services tab, herdr row selected, detail
/// open) and then marks the landing done in `[herdr] first_landing_done`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum HomeTab {
    #[default]
    Overview,
    Usage,
    Tokens,
    Setup,
    Fallback,
    Config,
    Status,
    /// The retired spelling `plugin` deserializes as this tab (the renamed
    /// `PopupWidth::Fit`'s `#[serde(alias = "full")]` precedent), so a
    /// profiles.toml written before the tab became Services still loads.
    #[serde(alias = "plugin")]
    Services,
}

impl HomeTab {
    /// Every main tab, in the cycle order the `home tab` row steps through.
    pub(crate) const ALL: [HomeTab; 8] = [
        HomeTab::Overview,
        HomeTab::Usage,
        HomeTab::Tokens,
        HomeTab::Setup,
        HomeTab::Fallback,
        HomeTab::Config,
        HomeTab::Status,
        HomeTab::Services,
    ];

    /// The on-disk spelling, doubled as the cycle row's chip label.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            HomeTab::Overview => "overview",
            HomeTab::Usage => "usage",
            HomeTab::Tokens => "tokens",
            HomeTab::Setup => "setup",
            HomeTab::Fallback => "fallback",
            HomeTab::Config => "config",
            HomeTab::Status => "status",
            HomeTab::Services => "services",
        }
    }
}

/// What shape `open-pane.sh` opens the herdr entrypoint in, one of the
/// `[herdr]` knobs in profiles.toml. Serialized as a lowercase string so the
/// file stays human-readable: `popup_width = "fit"`.
///
/// The four-value set is fit, half, split-right, split-top. The retired
/// `full` deserializes as [`PopupWidth::Fit`] — the owner's ruling, since
/// fit and full resolved identically below the 540-col cap — so a
/// profiles.toml written before the merge still loads.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum PopupWidth {
    /// Full width on terminals ≤ 540 cols, else a 540-col centered popup.
    #[default]
    #[serde(alias = "full")]
    Fit,
    /// herdr's own default half-size.
    Half,
    /// A real split pane right of the focused pane.
    #[serde(rename = "split-right")]
    SplitRight,
    /// A real split pane directly above the focused pane.
    #[serde(rename = "split-top")]
    SplitTop,
}

impl PopupWidth {
    /// The `clauth herdr config get popup_width` spelling.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            PopupWidth::Fit => "fit",
            PopupWidth::Half => "half",
            PopupWidth::SplitRight => "split-right",
            PopupWidth::SplitTop => "split-top",
        }
    }
}

/// The herdr knobs, persisted under `[herdr]` in profiles.toml. Written by the
/// Services tab's herdr-options form rows, read by the plugin scripts through
/// `clauth herdr config get <key>` and by the TUI at launch — so the on-disk
/// shape is also a published read contract. The `[herdr]` table itself may be
/// absent (defaults) or partial: a missing field fills from [`Default`]
/// rather than erroring.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct HerdrSettings {
    /// Which open shape `open-pane.sh` resolves per open: popup sizing for
    /// `fit`/`half`, a real split pane for `split-right`/`split-top`.
    pub(crate) popup_width: PopupWidth,
    /// Publish the `clauth=$profile` pane-metadata token (the sidebar tag).
    pub(crate) pane_tag: bool,
    /// The per-pane tag watcher interval, seconds.
    pub(crate) tag_watch_secs: u64,
    /// Also publish `--display-agent "$profile"` so split-pane borders name
    /// the account.
    pub(crate) border_label: bool,
    /// The `clauth mcp` server reports `clauth_delegate=working|idle` pane
    /// metadata while delegates run.
    pub(crate) delegate_dot: bool,
    /// The sidebar row `clauth herdr install` appends gains the
    /// `$clauth_delegate` token, so a running delegate reads as text.
    pub(crate) delegate_row_text: bool,
    /// Whether a herdr restore that dropped launch options refuses the
    /// conversation's first prompt with the restore notice.
    /// Read by the hook only; the TUI toggle persists it without pushing
    /// anything live.
    pub(crate) restore_notice: bool,
    /// Set once the first herdr-mode landing fires, so that landing happens
    /// exactly once; every later launch — herdr mode included — opens the
    /// top-level `home_tab` instead.
    pub(crate) first_landing_done: bool,
}

impl Default for HerdrSettings {
    fn default() -> Self {
        Self {
            popup_width: PopupWidth::Fit,
            pane_tag: true,
            tag_watch_secs: 5,
            border_label: false,
            delegate_dot: true,
            delegate_row_text: false,
            restore_notice: true,
            first_landing_done: false,
        }
    }
}

/// The daemon's API knobs, persisted under `[serve]` in profiles.toml. Like
/// [`HerdrSettings`], the table may be absent (defaults) or partial: a missing
/// field fills from [`Default`] rather than erroring.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct ServeSettings {
    /// The daemon-wide switch: whether `POST /api/v1/sessions` is served at all
    /// (default off). Each calling device also needs its own `sessions` grant.
    pub(crate) session_creation: bool,
    /// The exact origins whose pages may read the API (default empty: no CORS
    /// header and no origin gate).
    #[serde(
        deserialize_with = "tolerant_origins",
        skip_serializing_if = "AppOrigins::is_empty"
    )]
    pub(crate) app_origins: AppOrigins,
    /// The app page a pairing link opens (default unset: the single
    /// `app_origins` entry, else no link).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) app_url: Option<String>,
    /// The `host:port` a pairing link names for this daemon, for a reverse
    /// proxy or another name (default unset: the address a `--listen` daemon
    /// publishes in `status.json`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) public_address: Option<String>,
}

/// `[serve] app_origins` as the file holds it. A hand edit of the wrong type
/// must not stop every clauth command, so [`tolerant_origins`] keeps what is
/// not a string in `unreadable` for the API to log: this runs on every
/// `profiles.toml` read, hooks and the MCP server included, and a line from
/// here would land on their stderr once per process.
#[derive(Debug, Clone, Default, Serialize)]
#[serde(transparent)]
pub(crate) struct AppOrigins {
    /// Every string, verbatim, valid or not: a save never rewrites one.
    pub(crate) entries: Vec<String>,
    /// Each value that is no string in a list, as text.
    #[serde(skip)]
    pub(crate) unreadable: Vec<String>,
}

impl AppOrigins {
    fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// `unreadable` is a note about one read and renders nothing, so a table
/// holding only that stays default.
impl PartialEq for AppOrigins {
    fn eq(&self, other: &Self) -> bool {
        self.entries == other.entries
    }
}

impl Eq for AppOrigins {}

fn tolerant_origins<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<AppOrigins, D::Error> {
    let mut origins = AppOrigins::default();
    match toml::Value::deserialize(deserializer)? {
        toml::Value::Array(items) => {
            for item in items {
                match item {
                    toml::Value::String(entry) => origins.entries.push(entry),
                    other => origins.unreadable.push(other.to_string()),
                }
            }
        }
        other => origins.unreadable.push(other.to_string()),
    }
    Ok(origins)
}

/// Why `entry` cannot count as an origin, or `None` when it can: exactly
/// `scheme://host[:port]`, the form a browser serializes, so anything else
/// could only ever fail to match or be read as more than one site. `http` is
/// loopback alone: a page there has no secure context, and on any other host a
/// network peer can write its script.
pub(crate) fn origin_entry_fault(entry: &str) -> Option<&'static str> {
    if entry.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Some("it holds whitespace or a control character");
    }
    if entry.contains('*') {
        return Some("it holds a wildcard");
    }
    let Some((scheme, authority)) = entry.split_once("://") else {
        return Some("it is not scheme://host[:port]");
    };
    let default_port = match scheme {
        "https" => "443",
        "http" => "80",
        _ => return Some("its scheme is not https or http"),
    };
    if authority.contains('/') {
        return Some("it has a path or a trailing slash");
    }
    if authority.contains(['?', '#']) {
        return Some("it has a query or a fragment");
    }
    if authority.contains('@') {
        return Some("it has userinfo");
    }
    let host_end = if authority.starts_with('[') {
        authority.find(']').map_or(authority.len(), |at| at + 1)
    } else {
        authority.find(':').unwrap_or(authority.len())
    };
    let (host, port) = authority.split_at(host_end);
    if !origin_host_is_lowercase_name(host) {
        return Some("its host is empty, not lowercase, or malformed");
    }
    if !port.is_empty() {
        let Some(digits) = port.strip_prefix(':') else {
            return Some("it is not scheme://host[:port]");
        };
        if digits == default_port {
            return Some("it names its scheme's default port");
        }
        let as_sent = digits.bytes().all(|b| b.is_ascii_digit())
            && !digits.starts_with('0')
            && digits.parse::<u16>().is_ok();
        if !as_sent {
            return Some("its port is not a number a browser would send");
        }
    }
    if scheme == "http" && !matches!(host, "localhost" | "127.0.0.1" | "[::1]") {
        return Some("it is plain http on a host other than loopback");
    }
    None
}

/// Why `url` cannot be the app page a pairing link opens (`[serve] app_url`),
/// or `None` when it can: an absolute `https://` URL whose origin passes
/// [`origin_entry_fault`], with no fragment, since the link's own fragment
/// carries the address and the code.
pub(crate) fn app_url_fault(url: &str) -> Option<&'static str> {
    if url.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Some("it holds whitespace or a control character");
    }
    if url.contains('#') {
        return Some("it has a fragment, which the link's own fragment replaces");
    }
    let Some((scheme, rest)) = url.split_once("://") else {
        return Some("it is not an absolute https:// URL");
    };
    if scheme != "https" {
        return Some("its scheme is not https");
    }
    let authority = rest
        .split_once('/')
        .map_or(rest, |(authority, _)| authority);
    origin_entry_fault(&format!("{scheme}://{authority}"))
}

/// Why `address` cannot name the daemon in a pairing link (`[serve]
/// public_address`), or `None` when it can: exactly `host:port`, the host as an
/// origin's and the port a number a browser would send.
pub(crate) fn public_address_fault(address: &str) -> Option<&'static str> {
    if address.contains("://") {
        return Some("it has a scheme; it takes host:port alone");
    }
    let Some((host, port)) = address.rsplit_once(':') else {
        return Some("it is not host:port");
    };
    if !origin_host_is_lowercase_name(host) {
        return Some("its host is empty, not lowercase, or malformed");
    }
    let as_sent = port.bytes().all(|b| b.is_ascii_digit())
        && !port.starts_with('0')
        && port.parse::<u16>().is_ok();
    if !as_sent {
        return Some("its port is not a number from 1 to 65535");
    }
    None
}

/// A DNS name or a dotted address in lowercase, or a bracketed IPv6 address.
fn origin_host_is_lowercase_name(host: &str) -> bool {
    if host.chars().any(char::is_uppercase) {
        return false;
    }
    if let Some(address) = host.strip_prefix('[') {
        return address
            .strip_suffix(']')
            .is_some_and(|address| address.parse::<std::net::Ipv6Addr>().is_ok());
    }
    !host.is_empty()
        && host.bytes().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'-' | b'_')
        })
}

/// The auto-updater knob, persisted under `[update]` in profiles.toml. Like
/// [`HerdrSettings`] and [`ServeSettings`], the table may be absent (defaults)
/// or partial: a missing field fills from [`Default`] rather than erroring.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct UpdateSettings {
    /// Run the background update work (default on): the binary check on
    /// launch and the daemon's remote herdr reinstall. `CLAUTH_NO_UPDATE=1`
    /// still disables every leg even when this is on — the env kill-switch
    /// stays authoritative.
    pub(crate) auto_update: bool,
}

impl Default for UpdateSettings {
    fn default() -> Self {
        Self { auto_update: true }
    }
}

/// The Claude Code plugin's knobs, persisted under `[plugin]` in profiles.toml
/// and edited from the Services tab's plugin card. Like [`UpdateSettings`],
/// the table may be absent (defaults) or partial.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct PluginSettings {
    /// The one wake `clauth mcp-await-job` sends a session idle
    /// `AWAIT_IDLE_WAKE_SECS` on a 1 h prompt cache while a delegate of its
    /// own still runs (default on). Off, the wait runs on to the job's end.
    pub(crate) idle_wake: bool,
}

impl Default for PluginSettings {
    fn default() -> Self {
        Self { idle_wake: true }
    }
}

/// How the fallback-chain walk orders the candidates WITHIN one accept pass
/// (`AppState.walk_order`, issue #86). `Chain` (the default) is today's walk
/// byte for byte: first accept in chain position, starting one slot after the
/// active and wrapping. `SoonestWeeklyReset` reorders each accept pass by the
/// soonest-resetting weekly window, so the member whose quota expires soonest
/// drains first and less expires unspent; `MostWeeklyHeadroom` reorders it by
/// the least-used week, so hops spread across the chain instead of always
/// landing on the next slot. The pass ladder (free quota > serving sink >
/// spend-armed > dead sink > halt) and every exclusion are untouched — the
/// mode decides only WHERE among the members a pass already accepts to land.
/// Serialized as a lowercase string with explicit hyphenated spellings:
/// `walk_order = "soonest-weekly-reset"`, `walk_order = "most-weekly-headroom"`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum WalkOrder {
    /// Today's chain-position walk, byte for byte.
    #[default]
    Chain,
    /// Order each accept pass by the soonest-resetting weekly window.
    #[serde(rename = "soonest-weekly-reset")]
    SoonestWeeklyReset,
    /// Order each accept pass by the least-used weekly window. Hop targets
    /// only: unlike `SoonestWeeklyReset` it never walks a healthy active.
    #[serde(rename = "most-weekly-headroom")]
    MostWeeklyHeadroom,
}

/// Stored at ~/.clauth/profiles.toml — ordering and active marker only.
/// Credentials and endpoint config live in per-profile subdirectories.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct AppState {
    #[serde(default)]
    pub(crate) active_profile: LockedSlot<Option<ProfileName>>,
    pub(crate) profiles: Vec<ProfileName>,
    #[serde(default)]
    pub(crate) fallback_chain: Vec<ProfileName>,
    /// When true and the whole chain is exhausted of SUBSCRIPTION quota,
    /// auto-switch clears live credentials and unsets the active profile
    /// instead of staying put. Its money twin is
    /// [`AppState::switch_off_when_budget_spent`] — see that field for why the
    /// two are separate.
    ///
    /// The on-disk key stays `wrap_off`: it is also a `status.json` field
    /// (schema 1, `wiki/Daemon.md`), so renaming it would break a published read
    /// contract and every existing profiles.toml. The Rust name says what it
    /// does; the serde name is the compatibility surface. Don't "align" them.
    #[serde(rename = "wrap_off", default)]
    pub(crate) switch_off_when_spent: bool,
    /// Profiles quarantined after a *permanent* OAuth refresh rejection (AUTH-1 /
    /// Incident C) — a transient network/5xx blip never lands here. Excluded from
    /// the fallback chain walk and refused as a switch target so a dead token is
    /// never installed into the Keychain (which would log out every running
    /// `claude`); cleared on a successful refresh or `clauth login`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) auth_broken: Vec<ProfileName>,
    /// When true, the fallback-chain auto-switch decision for the ACTIVE
    /// profile also projects its utilization (current + recent burn rate ×
    /// `min(poll interval, burn_horizon_cap_ms)`, the poll interval being the
    /// active's usage-probe interval where the probe reads it) beside the static
    /// per-profile threshold check, which always applies — switching early
    /// when the projection would reach 100%, never later than static. With no
    /// burn rate yet, or on a third-party provider window, the static check
    /// alone decides. Off by default. Candidate selection and `soonest_resume`
    /// are unaffected either way — the ACTIVE-side check is the walk's own.
    #[serde(default, skip_serializing_if = "is_false")]
    pub(crate) burn_aware_switching: bool,
    /// Which member the fallback-chain walk lands on WITHIN one accept pass
    /// (issue #86): `chain` (the default) walks by chain position exactly as
    /// it always has; `soonest-weekly-reset` lands each pass on the accepted
    /// member whose weekly window resets soonest, so less quota expires
    /// unspent; `most-weekly-headroom` on the one whose week is least used.
    /// `None` = the [`WalkOrder`] default, so an untouched
    /// profiles.toml carries neither this key nor the setting and walks
    /// byte-identically to before the key existed. Read through
    /// [`AppState::walk_order`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) walk_order: Option<WalkOrder>,
    /// The program the daemon runs when an account's 5h usage crosses its
    /// `bell_threshold`, `%s` standing for the bell's text; split by POSIX
    /// quoting rules and run without a shell (`bell::run_command`). `None` =
    /// no headless bell. Only a hand edit of profiles.toml sets it: no TUI row
    /// and no API route writes a program to run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) bell_command: Option<String>,
    /// Opt-in master switch for spending real money: when on, the auto-switch
    /// chain may pick a member whose subscription windows are spent but whose
    /// account still has pay-as-you-go budget, bounded by that member's
    /// `Profile::max_auto_spend` ceiling. Off by default, and every ceiling
    /// defaults to `$0`, so BOTH halves must be set before a cent is spent
    /// unattended. Spend-armed members rank below every subscription member
    /// with free quota — see the fallback walk's spend pass.
    #[serde(default, skip_serializing_if = "is_false")]
    pub(crate) spend_budget_switching: bool,
    /// What to do once a billing account has spent its `max_auto_spend` budget:
    /// `true` (the default) switches everything off, `false` stays on it.
    ///
    /// Separate from [`AppState::switch_off_when_spent`] on purpose. That one
    /// answers "the chain ran out of SUBSCRIPTION quota", where staying costs
    /// nothing but rate-limit errors. Here staying IS the spending, so the same
    /// words mean opposite things and an operator can legitimately want opposite
    /// answers: stay on active when the quota runs out, switch off when the money
    /// does. Defaults to switching off so `max auto-spend` is a real cap rather
    /// than an entry gate. See `fallback::budget_spent`.
    /// Unlike its `wrap_off` twin this key needs no compatibility spelling: it
    /// has never shipped, so the on-disk name is just the field name. That is
    /// why the pair looks lopsided in profiles.toml.
    #[serde(
        default = "default_switch_off_when_budget_spent",
        skip_serializing_if = "is_true"
    )]
    pub(crate) switch_off_when_budget_spent: bool,
    /// Rotate a profile ahead of its access-token expiry instead of waiting for
    /// a 401. Default ON: the lead clears the running `claude`'s own refresh
    /// threshold, so clauth's stored pair stays the live one instead of lagging
    /// a chain the session advanced. Off falls back to rotating only on
    /// rejection. See `usage::scheduler::proactive_rotation_due`.
    #[serde(
        default = "default_preemptive_rotation",
        skip_serializing_if = "is_true"
    )]
    pub(crate) preemptive_rotation: bool,
    /// Keep every key of a Claude credential store other than the login and
    /// its identity keys (`claude::LOGIN_CREDENTIAL_KEYS`) with the account
    /// whose store it is: a `/design-login`, the MCP-server logins, whatever
    /// Claude Code adds beside the login. Default ON. The live file's copy is
    /// synced into the account's own store on every relink and at the
    /// switch-away snapshot, and a static token's writers keep what the sidecar
    /// holds. Nothing crosses to another account through this: the switch carry
    /// stays `mcpOAuth` alone (`claude::CARRIED_CREDENTIAL_KEYS`).
    /// Off is the behavior before the key existed. Read off disk where it is
    /// used (`claude::preserve_non_login_keys`), where an unreadable file counts
    /// as on.
    #[serde(
        default = "default_preserve_non_login_keys",
        skip_serializing_if = "is_true"
    )]
    pub(crate) preserve_non_login_keys: bool,
    /// Keep every account's MCP-server logins (`mcpOAuth`) in step: an MCP
    /// login belongs to no Claude account, so a fresh one made on any account
    /// or inside any live session is copied to every account's credential
    /// store that holds a Claude login or MCP logins (an account with no such
    /// store gets none), where that store holds a copy of the same login or
    /// none (`claude::sync_mcp_pass`; an account's own separately made login
    /// is kept, and a login signed out of is removed everywhere); a running
    /// session takes it at its next switch. With it on, a switch's carry keeps
    /// the incoming account's own logins too. The daemon
    /// does it on its tick and a swap's hand-off does it at once, neither into
    /// a store a running session reads through its link. Default ON; off leaves
    /// each account with its own copies (the carries' usable-first rule still
    /// applies). Read off disk where it is used (`claude::sync_mcp_logins`),
    /// where an unreadable file counts as on; the daemon tick gates the pass
    /// off the config it reloads.
    #[serde(default = "default_sync_mcp_logins", skip_serializing_if = "is_true")]
    pub(crate) sync_mcp_logins: bool,
    /// When false, the background usage fetch skips accounts already pinned at
    /// their 100% window cap (spent) until the window resets — a spent window
    /// can't change until then, so re-polling only burns quota + poll load.
    /// Default true (poll every account every interval — today's behavior). A
    /// forced `r` refresh and a never-fetched account still poll once (a reset
    /// is only observed by polling). Fetch-leg only: never touches
    /// switch/fallback predicates. See `usage::scheduler` + `windows_maxed`.
    #[serde(default = "default_refresh_spent", skip_serializing_if = "is_true")]
    pub(crate) refresh_spent_accounts: bool,
    /// Interleave the `auto_start` auto-start kick across accounts, so their 5h
    /// windows open at least `5h / N` less a tick's tolerance apart instead of
    /// all at once, and a freshly reset account is within reach every cycle
    /// (`usage::auto_start_queue`).
    ///
    /// Default OFF: nobody's behaviour changes on upgrade. For N >= 2 the
    /// toggle visibly changes what `auto_start` does — windows stop resetting
    /// together, and a burst across every account at once waits up to
    /// `(N-1) * gap` for the last auto-start (nothing breaks: real usage still
    /// opens a window on demand). Accounts that never opted into `auto_start`
    /// are untouched either way.
    #[serde(default, skip_serializing_if = "is_false")]
    pub(crate) auto_start_queue: bool,
    /// Config-file theme override. CLI `--theme` flag takes priority; auto-
    /// detect applies when this is `None` and no flag was passed. Read through
    /// [`AppState::theme_tier`], written through [`AppState::set_theme`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) theme: Option<ThemeSetting>,
    /// Shape of every reset countdown in the TUI. `None` = the
    /// [`ResetDisplay`] default, so an untouched profiles.toml carries neither
    /// this key nor [`AppState::clock_format`] and renders exactly as it did
    /// before the setting existed. Read through [`AppState::reset_display`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) reset_display: Option<ResetDisplay>,
    /// Notation for the wall-clock half of a reset stamp. Inert while
    /// `reset_display` is `Relative` (nothing renders a clock then). `None` =
    /// the [`ClockFormat`] default; read through [`AppState::clock_format`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) clock_format: Option<ClockFormat>,
    /// The tab every launch opens on (the Config tab's `home tab` row). The
    /// first herdr launch overrides it — that one landing opens the Plugin
    /// tab with the herdr row's detail descended. `None` = the [`HomeTab`]
    /// default, so an untouched profiles.toml carries neither this key nor
    /// the setting and lands exactly as it did before the key existed. Read
    /// through [`AppState::home_tab`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) home_tab: Option<HomeTab>,
    /// Holds the header mascot on its rest pose, and the header gauge's account
    /// name and the Overview's provider names flat (the Config tab's
    /// `reduce motion` row); spinners and the saving shimmers keep moving, since
    /// they mark work in flight. `false` is skipped, so an untouched
    /// profiles.toml carries no key.
    #[serde(default, skip_serializing_if = "is_false")]
    pub(crate) reduce_motion: bool,
    /// When false, burn-rate estimates ("34.4 %/h · 1h 56m left") are hidden
    /// in the Usage tab even when data is available.
    #[serde(default = "default_show_estimates", skip_serializing_if = "is_true")]
    pub(crate) show_estimates: bool,
    /// When true, the Usage tab overlays an ideal-pace `│` marker on each window
    /// bar (off by default). Toggled from the Usage action menu.
    #[serde(default, skip_serializing_if = "is_false")]
    pub(crate) show_pace: bool,
    /// When true, the Tokens tab counts cache tokens in every "tokens" figure
    /// (total throughput); when false (default), figures are in+out only — the
    /// basis that matches the daily trend. Toggled with `c` on the Tokens tab.
    #[serde(default, skip_serializing_if = "is_false")]
    pub(crate) count_cache: bool,
    #[serde(default = "default_refresh_interval")]
    pub(crate) refresh_interval_ms: u64,
    /// Context-window nudge threshold, tokens: at or past it, the hook's
    /// context leg (`hook_context`) tells the conversation once per value.
    /// `None` = off. Read through
    /// [`AppState::context_nudge_threshold_tokens`], which resets a
    /// hand-edited out-of-band value to off. Valid range
    /// [`MIN_CONTEXT_NUDGE_TOKENS`]..=[`MAX_CONTEXT_NUDGE_TOKENS`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) context_nudge_threshold_tokens: Option<u64>,
    /// Default action when credential divergence is detected. `None` = show the
    /// Divergence modal (current behavior).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) default_divergence: Option<DivergenceChoice>,
    /// Chain-wide weekly (7d) exhaustion line, percent — past it an account
    /// counts as exhausted in BOTH walk directions (switch trigger + candidate
    /// acceptance); the wrap-off `Off` decision ignores it and keys on the
    /// 100% hard cap (`WEEKLY_HARD_BLOCK_PCT` in `fallback.rs`). `None` =
    /// [`DEFAULT_WEEKLY_SWITCH_PCT`]. Read through
    /// [`AppState::weekly_switch_threshold_pct`], which resets hand-edited
    /// garbage to the default. Global (not per-member like the 5h
    /// threshold): the line protects the CHAIN — a wrong hop strands days.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) weekly_switch_threshold: Option<f64>,
    /// Burn-aware floor: the lowest 5h utilization at which a projected switch
    /// may fire (`burn_aware_switching` only). The projection adds "would
    /// reach 100% within the look-ahead" beside the static threshold, and on a
    /// small window (Pro) the window-relative burn %/h reads high, so the
    /// projection trips from well below 100 — this caps the wasted headroom at
    /// `100 - min(floor, threshold)` on every tier, and a floor at or above
    /// the threshold does nothing. `None` = [`DEFAULT_BURN_FLOOR_PCT`]. Read
    /// through [`AppState::burn_switch_floor_pct`], which resets a hand-edited
    /// out-of-band value to the default. Inert unless burn-aware is on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) burn_switch_floor_pct: Option<f64>,
    /// Burn-aware horizon cap (ms): the projection looks ahead by
    /// `min(poll interval, this)` instead of the full poll interval (the
    /// refresh interval, or the active's usage-probe interval where the probe
    /// reads it), so a long poll cadence can't balloon the early-switch margin
    /// (it scales linearly with the look-ahead). `None` =
    /// [`DEFAULT_BURN_HORIZON_MS`]. Read through
    /// [`AppState::burn_horizon_cap_ms`]. Inert unless burn-aware is on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) burn_horizon_cap_ms: Option<u64>,
    /// The `[herdr]` table: the herdr-plugin knobs plus the first-landing
    /// marker. Omitted from the file while every key is at its default, so an
    /// untouched profiles.toml gains no `[herdr]` block on the next save.
    #[serde(default, skip_serializing_if = "herdr_is_default")]
    pub(crate) herdr: HerdrSettings,
    /// The daemon's `[serve]` table. Omitted from the file while at its
    /// default, so an untouched profiles.toml gains no `[serve]` block on the
    /// next save.
    #[serde(default, skip_serializing_if = "serve_is_default")]
    pub(crate) serve: ServeSettings,
    /// The `[update]` table: the auto-updater toggle. Omitted from the file
    /// while at its default, so an untouched profiles.toml gains no `[update]`
    /// block on the next save; an explicit `auto_update = false` serializes.
    #[serde(default, skip_serializing_if = "update_is_default")]
    pub(crate) update: UpdateSettings,
    /// The `[plugin]` table: the delegate idle-wake toggle. Omitted from the
    /// file while at its default, like `[update]`.
    #[serde(default, skip_serializing_if = "plugin_is_default")]
    pub(crate) plugin: PluginSettings,
    /// The built-in bind-table digest the TUI last offered to move accounts
    /// onto. `None` = never offered, so the next TUI launch with a non-empty
    /// offer list re-offers. The CLI never reads or writes it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) preset_binds_offered: Option<String>,
    /// [`AppState::set_active`] ran on this copy, so [`save_app_state`]
    /// moves `~/.clauth/base-env.json`'s recorded account with the marker it
    /// saves: a marker move by this clauth never reads as one by an older
    /// clauth that predates the record. Never serialized.
    #[serde(skip)]
    pub(crate) active_moved: bool,
}

fn herdr_is_default(herdr: &HerdrSettings) -> bool {
    *herdr == HerdrSettings::default()
}

fn serve_is_default(serve: &ServeSettings) -> bool {
    *serve == ServeSettings::default()
}

fn update_is_default(update: &UpdateSettings) -> bool {
    *update == UpdateSettings::default()
}

fn plugin_is_default(plugin: &PluginSettings) -> bool {
    *plugin == PluginSettings::default()
}

impl AppState {
    /// Whether `name` is on the persisted quarantine list.
    pub(crate) fn is_auth_broken(&self, name: &ProfileName) -> bool {
        self.auth_broken.iter().any(|n| n == name)
    }

    /// Set the active-profile marker. Requires the state-flock witness: the
    /// marker is read-modify-written cross-process, so an unlocked write can
    /// lose a concurrent switch.
    pub(crate) fn set_active(&mut self, active: Option<ProfileName>, _held: &StateLockHeld) {
        self.active_profile.set(active, _held);
        self.active_moved = true;
    }

    /// Mark or clear `name`'s auth-broken flag in this state. Returns `true`
    /// when the list actually changed. Pure in-memory mutation — the caller
    /// decides what to persist.
    pub(crate) fn set_auth_broken(&mut self, name: &ProfileName, broken: bool) -> bool {
        let present = self.is_auth_broken(name);
        if broken && !present {
            self.auth_broken.push(name.clone());
            true
        } else if !broken && present {
            self.auth_broken.retain(|n| n != name);
            true
        } else {
            false
        }
    }

    /// Drop `name` from every name list and clear the active marker when it
    /// named `name`. State-only mirror of [`AppConfig::remove`] so a caller can
    /// apply the delete onto a freshly read state rather than its stale copy.
    pub(crate) fn remove(&mut self, name: &ProfileName, held: &StateLockHeld) {
        self.profiles.retain(|n| n != name);
        self.fallback_chain.retain(|n| n != name);
        self.auth_broken.retain(|n| n != name);
        if self.active_profile.as_ref() == Some(name) {
            self.set_active(None, held);
        }
    }

    /// Replace `old` with `new` in every name list and the active marker.
    /// State-only mirror of [`AppConfig::rename_all_occurrences`] so a caller
    /// can apply the rename onto a freshly read state rather than its stale
    /// copy.
    pub(crate) fn rename_all_occurrences(
        &mut self,
        old: &ProfileName,
        new: &ProfileName,
        held: &StateLockHeld,
    ) {
        if let Some(slot) = self.profiles.iter_mut().find(|n| **n == *old) {
            *slot = new.clone();
        }
        if let Some(slot) = self.fallback_chain.iter_mut().find(|n| **n == *old) {
            *slot = new.clone();
        }
        if let Some(slot) = self.auth_broken.iter_mut().find(|n| **n == *old) {
            *slot = new.clone();
        }
        if self.active_profile.as_ref() == Some(old) {
            self.set_active(Some(new.clone()), held);
        }
    }

    /// The effective reset-countdown shape (unset = the stock relative form).
    pub(crate) fn reset_display(&self) -> ResetDisplay {
        self.reset_display.unwrap_or_default()
    }

    /// The effective walk-order mode (unset = [`WalkOrder::Chain`], the
    /// stock chain-position walk).
    pub(crate) fn walk_order(&self) -> WalkOrder {
        self.walk_order.unwrap_or_default()
    }

    /// The effective wall-clock notation (unset = 24-hour).
    pub(crate) fn clock_format(&self) -> ClockFormat {
        self.clock_format.unwrap_or_default()
    }

    /// The effective landing tab (unset = [`HomeTab`] default, overview).
    pub(crate) fn home_tab(&self) -> HomeTab {
        self.home_tab.unwrap_or_default()
    }

    /// The effective weekly exhaustion line: the configured value when it sits
    /// inside [`MIN_WEEKLY_SWITCH_PCT`]`..=`[`MAX_WEEKLY_SWITCH_PCT`], else the
    /// DEFAULT (a reset, not a clamp-to-nearest-bound: fail-safe high beats
    /// honoring a hand-edited `40.0` as `50`) — an out-of-band value edited
    /// into profiles.toml must not silently disable the weekly gate
    /// (rationale in `fallback.rs`).
    pub(crate) fn weekly_switch_threshold_pct(&self) -> f64 {
        self.weekly_switch_threshold
            .filter(|v| (MIN_WEEKLY_SWITCH_PCT..=MAX_WEEKLY_SWITCH_PCT).contains(v))
            .unwrap_or(DEFAULT_WEEKLY_SWITCH_PCT)
    }

    /// The effective burn-aware floor, resetting an out-of-band hand-edit to the
    /// default (same fail-safe reset-not-clamp rationale as
    /// [`AppState::weekly_switch_threshold_pct`]).
    pub(crate) fn burn_switch_floor_pct(&self) -> f64 {
        self.burn_switch_floor_pct
            .filter(|v| (MIN_BURN_FLOOR_PCT..=MAX_BURN_FLOOR_PCT).contains(v))
            .unwrap_or(DEFAULT_BURN_FLOOR_PCT)
    }

    /// The effective burn-aware horizon cap (ms), resetting an out-of-band
    /// hand-edit to the default. Shares the refresh-interval band since the cap
    /// only ever bounds the look-ahead from above together with the poll
    /// interval (`min(poll interval, cap)`; the poll interval being the refresh
    /// interval, or a probe-read active's usage-probe interval).
    pub(crate) fn burn_horizon_cap_ms(&self) -> u64 {
        self.burn_horizon_cap_ms
            .filter(|v| (MIN_REFRESH_INTERVAL_MS..=MAX_REFRESH_INTERVAL_MS).contains(v))
            .unwrap_or(DEFAULT_BURN_HORIZON_MS)
    }

    /// The effective context-nudge threshold: an out-of-band hand-edit reads
    /// as off (reset-to-`None`, never clamp — the same fail-safe rationale as
    /// [`AppState::weekly_switch_threshold_pct`], off having no default to
    /// reset to).
    pub(crate) fn context_nudge_threshold_tokens(&self) -> Option<u64> {
        self.context_nudge_threshold_tokens
            .filter(|v| (MIN_CONTEXT_NUDGE_TOKENS..=MAX_CONTEXT_NUDGE_TOKENS).contains(v))
    }
}

fn default_show_estimates() -> bool {
    true
}

fn default_refresh_spent() -> bool {
    true
}

/// A spent budget stops spending unless the operator says otherwise, so this
/// defaults ON — unlike `switch_off_when_spent`, whose default keeps you signed in because
/// staying costs nothing there.
fn default_switch_off_when_budget_spent() -> bool {
    true
}

fn default_preemptive_rotation() -> bool {
    true
}

fn default_preserve_non_login_keys() -> bool {
    true
}

fn default_sync_mcp_logins() -> bool {
    true
}

fn is_true(b: &bool) -> bool {
    *b
}

/// `skip_serializing_if` guard: serialize a bool field only when true. Shared
/// with [`crate::providers::UsageBar`]'s `inference_quota` mark.
pub(crate) fn is_false(b: &bool) -> bool {
    !*b
}

/// Default interval new profiles.toml uses. Kept in one place — everything else
/// references this constant.
pub(crate) const DEFAULT_REFRESH_INTERVAL_MS: u64 = 90_000;

/// Minimum allowed `refresh_interval_ms`. The Anthropic API is rate-limited;
/// sub-10 s intervals serve no purpose and can trigger 429s.
pub(crate) const MIN_REFRESH_INTERVAL_MS: u64 = 10_000;

/// Maximum settable `refresh_interval_ms` (1 h). Past this the background usage
/// view is effectively stale; the Config-tab custom-value editor caps here.
pub(crate) const MAX_REFRESH_INTERVAL_MS: u64 = 3_600_000;

fn default_refresh_interval() -> u64 {
    DEFAULT_REFRESH_INTERVAL_MS
}

/// Default `usage_probe_interval_ms` (90 s).
pub(crate) const DEFAULT_USAGE_PROBE_INTERVAL_MS: u64 = 90_000;

/// Floor of `usage_probe_interval_ms`. Every probe is an inference request on
/// the account, so the floor sits well above the usage poll's.
pub(crate) const MIN_USAGE_PROBE_INTERVAL_MS: u64 = 60_000;

/// Ceiling of `usage_probe_interval_ms`, so a probed account gets a burn rate.
/// Each landing is stamped up to `interval/4` past the interval, the history
/// writer bridges every change 1 ms before it, and the burn rate cuts its
/// series at the last gap between two equal readings longer than
/// [`crate::usage::BURN_GAP_CUT_MS`]: past that, two points are left, below
/// [`crate::usage::BURN_MIN_SAMPLES`]. At three quarters of the cut,
/// `interval + interval/4` stays a sixteenth of it short, room for the tick,
/// the request slot and the round trip.
pub(crate) const MAX_USAGE_PROBE_INTERVAL_MS: u64 = crate::usage::BURN_GAP_CUT_MS * 3 / 4;
const _: () = assert!(
    MAX_USAGE_PROBE_INTERVAL_MS + MAX_USAGE_PROBE_INTERVAL_MS / 4 < crate::usage::BURN_GAP_CUT_MS
);

/// A stored probe interval as the profile carries it: absent is the default,
/// anything outside the band is clamped into it.
fn usage_probe_interval(stored: Option<u64>) -> u64 {
    stored.map_or(DEFAULT_USAGE_PROBE_INTERVAL_MS, |ms| {
        ms.clamp(MIN_USAGE_PROBE_INTERVAL_MS, MAX_USAGE_PROBE_INTERVAL_MS)
    })
}

/// Default chain-wide weekly (7d) exhaustion line (percent). Why 98 and not
/// the API's 100% refusal cap: topping out the week bricks an account for
/// days, so the hop must fire while there is still room to land it — the
/// full rationale lives on the gate in `fallback.rs`.
pub(crate) const DEFAULT_WEEKLY_SWITCH_PCT: f64 = 98.0;

/// Lowest configurable weekly line. Below this the chain thrashes: most
/// members spend half their week above the line, so every hop immediately
/// re-triggers.
pub(crate) const MIN_WEEKLY_SWITCH_PCT: f64 = 50.0;

/// Highest configurable weekly line — 100 reproduces the pre-2026-07-12
/// hard-cap behavior (switch only once the API already refuses).
pub(crate) const MAX_WEEKLY_SWITCH_PCT: f64 = 100.0;

/// Bounds of [`AppState::context_nudge_threshold_tokens`], tokens.
pub(crate) const MIN_CONTEXT_NUDGE_TOKENS: u64 = 50_000;
pub(crate) const MAX_CONTEXT_NUDGE_TOKENS: u64 = 2_000_000;

/// Default burn-aware floor (percent), level with the weekly default. It sits
/// above the default member threshold (95), so stock settings switch exactly
/// like static; only a member threshold above 98 gives the projection room,
/// and then a projected switch wastes at most 2% of the window. Lower it for
/// an earlier projected switch (less window used, less rate-limit risk); at
/// or above a member's threshold it does nothing for that member.
pub(crate) const DEFAULT_BURN_FLOOR_PCT: f64 = 98.0;

/// Lowest configurable burn-aware floor. Below this the projection may switch
/// so far from 100 that the poll-lag margin it exists to protect is gone.
pub(crate) const MIN_BURN_FLOOR_PCT: f64 = 90.0;

/// Highest configurable burn-aware floor — 100 sits at or above every member
/// threshold, so the projection never switches earlier than static.
pub(crate) const MAX_BURN_FLOOR_PCT: f64 = 100.0;

/// Default burn-aware horizon cap (60 s). Under the default 90 s cadence this
/// cuts the projected look-ahead, and with it the early-switch margin, by a
/// third, while keeping a poll-lag cushion. Bounded by the
/// poll interval either way (`min(interval, cap)`; the usage-probe interval on
/// a probe-read active).
pub(crate) const DEFAULT_BURN_HORIZON_MS: u64 = 60_000;

impl Default for AppState {
    fn default() -> Self {
        Self {
            active_profile: slot(None),
            profiles: Vec::new(),
            fallback_chain: Vec::new(),
            switch_off_when_spent: false,
            auth_broken: Vec::new(),
            burn_aware_switching: false,
            walk_order: None,
            bell_command: None,
            spend_budget_switching: false,
            switch_off_when_budget_spent: default_switch_off_when_budget_spent(),
            preemptive_rotation: default_preemptive_rotation(),
            preserve_non_login_keys: default_preserve_non_login_keys(),
            sync_mcp_logins: default_sync_mcp_logins(),
            refresh_spent_accounts: true,
            auto_start_queue: false,
            theme: None,
            reset_display: None,
            clock_format: None,
            home_tab: None,
            reduce_motion: false,
            show_estimates: true,
            show_pace: false,
            count_cache: false,
            refresh_interval_ms: default_refresh_interval(),
            context_nudge_threshold_tokens: None,
            default_divergence: None,
            weekly_switch_threshold: None,
            burn_switch_floor_pct: None,
            burn_horizon_cap_ms: None,
            herdr: HerdrSettings::default(),
            serve: ServeSettings::default(),
            update: UpdateSettings::default(),
            plugin: PluginSettings::default(),
            preset_binds_offered: None,
            active_moved: false,
        }
    }
}

/// `Clone` is what lets a reader snapshot the config and drop the lock before
/// doing disk work with it (`daemon::write_status`); CONFIG outranks the locks
/// that work takes.
#[derive(Clone)]
pub(crate) struct AppConfig {
    pub(crate) state: AppState,
    pub(crate) profiles: Vec<Profile>,
}

/// Ranked in lock order: inner of `usage_store`, outer of the state flock.
pub(crate) type ConfigHandle =
    std::sync::Arc<crate::lockorder::RankedMutex<AppConfig, crate::lockorder::rank::Config>>;

impl AppConfig {
    pub(crate) fn is_active(&self, name: &ProfileName) -> bool {
        self.state.active_profile.as_ref() == Some(name)
    }

    /// Whether `name` is the home account on `day`, decided across the whole
    /// profile list rather than per profile.
    ///
    /// A day list claims its days EXCLUSIVELY: on a day some profile names,
    /// only the profiles naming it are home, and a bare `preferred = true`
    /// elsewhere stands down for that day. Resolving this per profile instead
    /// would leave the flag claiming all seven, so the two-account split an
    /// operator actually wants — weekdays here, weekends there — would need a
    /// list on both sides, and getting one wrong reads as first-match luck in
    /// `fallback.rs` rather than as a mistake.
    ///
    /// On a day nobody names, `preferred` decides exactly as before.
    ///
    /// Only chain members the walk would actually visit are ever home. A
    /// profile off the chain, or one `walk_excluded` skips (unresolvable,
    /// auth-broken, disabled), never serves — so its list must not stand the
    /// flag down and leave the day with nobody home, and its own flag must not
    /// mark it home on the days no list claims. Reading the chain rather than
    /// `profiles` follows the spend warning, which is on the chain for the
    /// same reason.
    pub(crate) fn is_home_on(
        &self,
        name: &ProfileName,
        day: Weekday,
        key_rejected: &HashSet<ProfileName>,
    ) -> bool {
        // An account the walk would never visit is home on NO day: a list on it
        // claims nothing, and its flag decides nothing either. One guard at the
        // entry rather than one per branch — the gap this closes was exactly a
        // branch that did not repeat the check, and a third branch would repeat
        // the gap. Redundant on the claimed branch, where `day_listers` has
        // already applied it; the redundancy is what makes the omission
        // impossible.
        if !crate::fallback::serves_the_chain(self, name, key_rejected) {
            return false;
        }
        let mut listers = self.day_listers(day, key_rejected);
        match listers.next() {
            // Home on a claimed day IS the claimant set, asked of the scan
            // rather than re-derived beside it. A second predicate drifts:
            // `walk_excluded` alone reads an off-chain account as eligible, so
            // a healthy non-member with a matching list answered home here
            // while the scan refused it the same claim.
            Some(first) => first == name || listers.any(|n| n == name),
            None => self.find(name).is_some_and(|p| p.preferred),
        }
    }

    /// Chain members that name `day` and could actually serve it, in chain
    /// order. Empty when the day is unclaimed, which is what hands it back to
    /// `preferred`. `key_rejected` is the key-rejected set the caller read once
    /// outside the config guard.
    pub(crate) fn day_listers<'a>(
        &'a self,
        day: Weekday,
        key_rejected: &'a HashSet<ProfileName>,
    ) -> impl Iterator<Item = &'a ProfileName> + 'a {
        self.state
            .fallback_chain
            .iter()
            .filter(move |n| !crate::fallback::walk_excluded(self, n, key_rejected))
            .filter(move |n| {
                self.find(n)
                    .is_some_and(|p| p.preferred_days.contains(&day))
            })
    }

    /// The day-list collision notice for `day`: what to log and toast when more
    /// than one chain member that could serve names the same day. `None` on the
    /// ordinary zero-or-one claimant.
    ///
    /// Nothing is broken by a collision — the return pass takes the first
    /// claimant that reads clear — but the operator wrote two lines expecting
    /// one home, so the state is worth saying out loud once.
    ///
    /// The message doubles as its callers' once-gate key: it names the day and
    /// the claimants in chain order, so it changes exactly when the midnight
    /// rollover or a config edit changes what is being warned about, and stays
    /// byte-equal across every tick in between.
    pub(crate) fn day_claim_collision(
        &self,
        day: Weekday,
        key_rejected: &HashSet<ProfileName>,
    ) -> Option<String> {
        let names: Vec<String> = self
            .day_listers(day, key_rejected)
            .map(|n| format!("'{n}'"))
            .collect();
        if names.len() < 2 {
            return None;
        }
        Some(format!(
            "{} accounts claim {}: {}. The chain returns to whichever of them reads clear first",
            names.len(),
            day.to_string().to_ascii_lowercase(),
            names.join(", "),
        ))
    }

    /// The passed-over-lister notice for `day`: what to say when an account
    /// names the day but could not serve it, so its line does nothing.
    /// `None` when every lister could serve, which is the ordinary case.
    ///
    /// The editor warns at save time, but a list goes inert LATER too — the
    /// account leaves the chain, is disabled, or its login breaks — and a
    /// hand-edited `config.toml` never passes the editor at all. Neither
    /// reaches the operator without a tick-time notice.
    ///
    /// Same gate-key scheme as [`AppConfig::day_claim_collision`]: the day, the
    /// blocked accounts in profile-list order, and what became of the day are
    /// all in the message.
    pub(crate) fn day_claim_passed_over(
        &self,
        day: Weekday,
        key_rejected: &HashSet<ProfileName>,
    ) -> Option<String> {
        let blocked: Vec<String> = self
            .profiles
            .iter()
            .filter(|p| p.preferred_days.contains(&day))
            .filter_map(|p| {
                crate::fallback::day_claim_blocker(self, &p.name, key_rejected)
                    .map(|why| format!("'{}' ({why})", p.name))
            })
            .collect();
        if blocked.is_empty() {
            return None;
        }
        let named = day.to_string().to_ascii_lowercase();
        // What happened to the day, not just that a line is inert: a carried
        // day still has somebody home and reads as a stray line, while an
        // uncarried one has quietly fallen back to the flag.
        let tail = match self.day_listers(day, key_rejected).next() {
            Some(carrier) => format!("'{carrier}' carries it"),
            None => format!("Nothing else claims {named}, so `preferred` decides it"),
        };
        let subject = if blocked.len() == 1 {
            "the list on"
        } else {
            "the lists on"
        };
        Some(format!(
            "{named}: {subject} {} cannot claim it. {tail}",
            blocked.join(", ")
        ))
    }

    /// Today's day-list notices in the machine's local zone, in a fixed order.
    ///
    /// Each entry is its own gate key, so a caller holding the previous set
    /// emits only what is new rather than repainting the rest — a second list
    /// arriving must not re-toast a collision the operator has already read.
    pub(crate) fn day_claim_notices_today(
        &self,
        key_rejected: &HashSet<ProfileName>,
    ) -> Vec<String> {
        let day = Local::now().weekday();
        [
            self.day_claim_collision(day, key_rejected),
            self.day_claim_passed_over(day, key_rejected),
        ]
        .into_iter()
        .flatten()
        .collect()
    }

    /// One notice per stored account whose name the commands clauth prints
    /// cannot address, the claude roster then the codex one, each in roster
    /// order. A name both rosters hold — or one listed twice — is one account,
    /// so it is noticed at its first occurrence alone: the claude roster's,
    /// the account a bare name resolves to and the only one Setup can rename,
    /// while the codex row for the same name is a hand edit. New names refuse
    /// both shapes at creation; these are the accounts stored before that, or
    /// by hand.
    pub(crate) fn name_notices<'a>(
        &'a self,
        codex: impl IntoIterator<Item = &'a ProfileName>,
    ) -> Vec<NameNotice> {
        let mut notices: Vec<NameNotice> = Vec::new();
        for (name, harness) in self
            .profiles
            .iter()
            .map(|profile| (&profile.name, Harness::Claude))
            .chain(codex.into_iter().map(|name| (name, Harness::Codex)))
        {
            let Some(notice) = NameNotice::of(name, harness) else {
                continue;
            };
            if notices.iter().any(|seen| seen.name == notice.name) {
                continue;
            }
            notices.push(notice);
        }
        notices
    }

    /// [`AppConfig::is_home_on`] for today in the machine's local zone. Called
    /// per chain build rather than at load: the fingerprint that drives a hot
    /// reload is built from `config.toml` mtimes, and midnight moves no file.
    pub(crate) fn is_home_today(
        &self,
        name: &ProfileName,
        key_rejected: &HashSet<ProfileName>,
    ) -> bool {
        self.is_home_on(name, Local::now().weekday(), key_rejected)
    }

    /// True when `name`'s last OAuth refresh was rejected as revoked/invalid
    /// (AUTH-1). Such a profile is skipped by the fallback chain walk.
    pub(crate) fn is_auth_broken(&self, name: &ProfileName) -> bool {
        self.state.is_auth_broken(name)
    }

    /// Mark or clear `name`'s auth-broken flag. Returns `true` when the set
    /// actually changed. Pure in-memory mutation — the caller persists via
    /// `set_auth_broken_persisted`.
    pub(crate) fn set_auth_broken(&mut self, name: &ProfileName, broken: bool) -> bool {
        self.state.set_auth_broken(name, broken)
    }

    pub(crate) fn find(&self, name: &ProfileName) -> Option<&Profile> {
        self.profiles.iter().find(|p| p.name == *name)
    }

    pub(crate) fn find_mut(&mut self, name: &ProfileName) -> Option<&mut Profile> {
        self.profiles.iter_mut().find(|p| p.name == *name)
    }

    pub(crate) fn names(&self) -> Vec<&str> {
        self.profiles.iter().map(|p| p.name.as_str()).collect()
    }

    /// Every stored profile with [`Profile::is_disabled`] false — the view every
    /// operational surface (fallback walk, scheduler, daemon status feed) reads.
    /// [`AppConfig::profiles`]/[`AppConfig::names`] stay the full list; the TUI
    /// still needs every profile, disabled ones included.
    pub(crate) fn enabled_profiles(&self) -> impl Iterator<Item = &Profile> {
        self.profiles.iter().filter(|p| !p.is_disabled())
    }

    /// Case-insensitive name lookup; returns the canonical-cased name on match.
    pub(crate) fn canonical_name(&self, query: &str) -> Option<String> {
        self.names()
            .into_iter()
            .find(|n| n.eq_ignore_ascii_case(query))
            .map(str::to_string)
    }

    pub(crate) fn add(&mut self, profile: Profile) {
        self.state.profiles.push(profile.name.clone());
        self.profiles.push(profile);
    }

    pub(crate) fn remove(&mut self, name: &ProfileName, held: &StateLockHeld) {
        self.profiles.retain(|p| p.name != *name);
        self.state.remove(name, held);
    }

    /// Replace `old` with `new` in every name list and the active marker.
    pub(crate) fn rename_all_occurrences(
        &mut self,
        old: &ProfileName,
        new: &ProfileName,
        held: &StateLockHeld,
    ) {
        if let Some(profile) = self.find_mut(old) {
            profile.name = new.clone();
        }
        self.state.rename_all_occurrences(old, new, held);
    }
}

/// Per-account model knobs written into the profile's Claude Code `settings.json`.
/// `default` is the `model` setting; `opus`/`sonnet`/`haiku`/`fable` are the
/// `ANTHROPIC_DEFAULT_*_MODEL` env overrides; `subagent` is
/// `CLAUDE_CODE_SUBAGENT_MODEL`; `advisor` is the top-level `advisorModel`
/// setting, the model Claude Code's server-side advisor tool runs on.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ModelSettings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) default: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) opus: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) sonnet: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) haiku: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) fable: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) subagent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) advisor: Option<String>,
}

impl ModelSettings {
    pub(crate) fn is_empty(&self) -> bool {
        self.default.is_none()
            && self.opus.is_none()
            && self.sonnet.is_none()
            && self.haiku.is_none()
            && self.fable.is_none()
            && self.subagent.is_none()
            && self.advisor.is_none()
    }

    /// The stored id for a row named by its settings key (`default`, `opus`,
    /// `sonnet`, `haiku`, `fable`, `subagent`, `advisor`). `None` outside that set.
    pub(crate) fn field(&self, row: &str) -> Option<&String> {
        match row {
            "default" => self.default.as_ref(),
            "opus" => self.opus.as_ref(),
            "sonnet" => self.sonnet.as_ref(),
            "haiku" => self.haiku.as_ref(),
            "fable" => self.fable.as_ref(),
            "subagent" => self.subagent.as_ref(),
            "advisor" => self.advisor.as_ref(),
            _ => None,
        }
    }

    /// Set one row by settings key; an unknown key writes nothing.
    pub(crate) fn set_field(&mut self, row: &str, value: Option<String>) {
        match row {
            "default" => self.default = value,
            "opus" => self.opus = value,
            "sonnet" => self.sonnet = value,
            "haiku" => self.haiku = value,
            "fable" => self.fable = value,
            "subagent" => self.subagent = value,
            "advisor" => self.advisor = value,
            _ => {}
        }
    }
}

/// Which Alibaba Model Studio front the console session belongs to. The two
/// sites are separate deployments with separate console hosts and separate
/// gateway actions, so a token minted on one is meaningless on the other.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum ConsoleSite {
    /// `bailian.console.aliyun.com` — the mainland-China front.
    #[default]
    Domestic,
    /// `modelstudio.console.alibabacloud.com` — the international front.
    International,
}

impl ConsoleSite {
    /// The canonical spelling written to `config.toml`.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Domestic => "domestic",
            Self::International => "international",
        }
    }

    /// Parse a stored or callback-supplied site. `None` for anything outside
    /// the known spellings, so a caller can fall back to the site it already
    /// knows rather than silently routing to the other deployment. `intl` is
    /// accepted because that is how the international hosts spell themselves.
    pub(crate) fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "domestic" | "cn" | "china" => Some(Self::Domestic),
            "international" | "intl" => Some(Self::International),
            _ => None,
        }
    }
}

/// A profile's Alibaba Model Studio **console** session: the only credential
/// that can read Token Plan quota. The api key authenticates inference and is
/// never read by the quota surface, so this is a second, independent secret.
///
/// It expires 48 hours after the operator's aliyun BROWSER sign-in — not after
/// the login that captured it, which merely inherits the rest of that window
/// (two tokens minted ~4h apart carry the same create/expire stamps). There is
/// no refresh path, so its death is an ordinary state rather than an error, and
/// a re-login is not guaranteed to buy much — see
/// [`crate::usage::FetchStatus::AuthExpired`].
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct ConsoleCredential {
    /// Bearer token for the OneConsole gateway. Redacted from [`fmt::Debug`]
    /// because `Profile` derives `Debug` and a stray `{:?}` would otherwise put
    /// a live session on a log line.
    pub(crate) token: String,
    pub(crate) site: ConsoleSite,
    /// Console region id (`cn-beijing`, `ap-southeast-1`). Sent verbatim as the
    /// gateway's `region` form field, so it stays a string rather than an enum:
    /// an unknown region still reaches the endpoint that can answer for it.
    pub(crate) region: String,
}

impl std::fmt::Debug for ConsoleCredential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConsoleCredential")
            .field("token", &"<redacted>")
            .field("site", &self.site)
            .field("region", &self.region)
            .finish()
    }
}

/// On-disk `[console]` table. Every field is optional so an absent table, a
/// half-filled one, and a hand-edited one all parse; [`load_profile`]
/// normalizes it into an `Option<ConsoleCredential>` at the load boundary.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
struct ConsoleConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    site: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    region: Option<String>,
}

/// A profile's own identity, apart from its name: random, written as `id` into
/// its `config.toml` by the create ([`save_new_profile`]), carried by a rename
/// (the dir moves whole) and gone with a delete. A login names its account by
/// this at start and lands only where the name still holds it, so an account
/// deleted and recreated, or renamed into the name, during the wait receives
/// nothing. The codex roster keys its own the same way.
///
/// [`ProfileConfig`] does not model it: the config writers read it off disk
/// under the flock, so a stale in-memory profile never writes an old one back,
/// and a writer that renders without it (an older clauth) carries it as an
/// unmodelled key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub(crate) struct ProfileId(String);

impl ProfileId {
    pub(crate) fn mint() -> Result<Self> {
        let mut bytes = [0u8; 16];
        getrandom::fill(&mut bytes).map_err(|e| anyhow::anyhow!("CSPRNG failure: {e}"))?;
        Ok(Self(hex::encode(bytes)))
    }
}

#[derive(Debug, Serialize, Deserialize, Default, PartialEq)]
struct ProfileConfig {
    base_url: Option<String>,
    api_key: Option<String>,
    #[serde(default, alias = "kick_timer")]
    auto_start: bool,
    #[serde(default)]
    usage_probe: bool,
    #[serde(default)]
    usage_probe_interval_ms: Option<u64>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    #[serde(default)]
    models: ModelSettings,
    #[serde(default)]
    effort: Option<String>,
    #[serde(default)]
    fallback_threshold: Option<f64>,
    #[serde(default)]
    weekly_threshold: Option<f64>,
    #[serde(default)]
    last_resort: bool,
    /// CLA-ROLL. No alias for the pre-rename `session_feed` spelling: no
    /// released clauth ever wrote that key, so carrying it here would be a
    /// permanent legacy alias for something that never shipped. Installs that
    /// ran the feature branch under its old name re-run
    /// `clauth rolling-token <profile>` once after upgrading.
    #[serde(default)]
    rolling_token: bool,
    #[serde(default)]
    preferred: bool,
    /// Weekday names behind [`Profile::preferred_days`]. Strings rather than a
    /// typed enum so a typo costs one entry instead of the whole profile —
    /// `load_profile` drops what it cannot read, and the canonical rewrite then
    /// drops it from disk, which is the visible signal it was not understood.
    #[serde(default)]
    preferred_days: Vec<String>,
    #[serde(default)]
    max_auto_spend: Option<f64>,
    /// `Option` (not `bool`) so the derived `Default` and an absent key agree:
    /// `None` means "unset", which resolves to the on default at the load
    /// boundary — a plain `#[serde(default)] bool` would silently default OFF.
    #[serde(default)]
    check_weekly: Option<bool>,
    #[serde(default)]
    check_scoped: Option<bool>,
    #[serde(default)]
    check_credit_cap: Option<bool>,
    #[serde(default)]
    bell_threshold: Option<f64>,
    #[serde(default)]
    disabled: bool,
    /// `[console]` — the Alibaba Model Studio console session. A TABLE, so it
    /// must render after every scalar key in `render_config_toml`.
    #[serde(default)]
    console: ConsoleConfig,
}

/// The `[proxy]` table as a raw config.toml holds it, parsed on the READ side
/// only. The record is deliberately UNMODELLED in [`ProfileConfig`]: a table
/// `ProfileConfig` holds but `render_config_toml` never writes is dropped by
/// the next rewrite, while a table neither holds stays where it sits through
/// every in-place `config.toml` write. That is what keeps every pre-existing
/// state file and fixture byte-identical: the record reaches the runtime
/// [`Profile`] only as its never-serialized `proxy` field.
#[derive(Debug, Deserialize, Default, PartialEq)]
struct ProxyTable {
    #[serde(default)]
    service: Option<String>,
    #[serde(default)]
    account: Option<String>,
}

/// The typed proxy-account record a profile's `[proxy]` table carries: which
/// proxy owns the account and the account's id there. `clauth delete` keys the
/// upstream delete off it, the settings engine keys its PATCH off it, and the
/// usage arm keys its per-account read off it — the registry bind match alone
/// decides the arm, this record decides the account.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProxyAccount {
    pub(crate) service: crate::proxy::Service,
    pub(crate) account: String,
}

/// [`ProxyTable`] as the typed record. Both keys must be present and the
/// service must still parse, so a hand-truncated or misnamed record reads as
/// no record — the profile falls back to today's generic-endpoint arm rather
/// than half-claiming an account.
fn proxy_record(table: &ProxyTable) -> Option<ProxyAccount> {
    let service = table
        .service
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())?;
    let account = table
        .account
        .as_deref()
        .map(str::trim)
        .filter(|a| !a.is_empty())?;
    Some(ProxyAccount {
        service: crate::proxy::Service::parse(service).ok()?,
        account: account.to_string(),
    })
}

/// What `name`'s config.toml says about a proxy account.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ProxyClaim {
    /// No `[proxy]` table (or no config clauth can read).
    None,
    Account(ProxyAccount),
    /// A `[proxy]` table is there but is no record: a missing or blank key, a
    /// service name that does not parse, or a config.toml whose TOML no longer
    /// parses around a `[proxy]` header. The CLI loads every profile before it
    /// asks, which refuses that last file first; the shared delete
    /// ([`crate::actions::delete_profile`]) reads it under a config the TUI
    /// loaded before the edit, and refuses it here.
    Unreadable,
    /// No reading of config.toml can say whether it holds a `[proxy]` table:
    /// the file cannot be read, or its TOML does not parse and its text holds
    /// `proxy` or a backslash on no `[proxy]` header line of its own, which
    /// reads [`ProxyClaim::Unreadable`] first. Only
    /// [`stored_proxy_claim_failing_closed`] answers it; [`stored_proxy_claim`]
    /// reads such a file as [`ProxyClaim::None`].
    Unclassifiable,
}

impl ProxyClaim {
    /// The record, `None` for no record and for one clauth cannot read alike
    /// ([`stored_proxy_record`]).
    fn into_record(self) -> Option<ProxyAccount> {
        match self {
            Self::Account(record) => Some(record),
            Self::None | Self::Unreadable | Self::Unclassifiable => None,
        }
    }
}

/// The proxy claim `name`'s config.toml carries, read lock-free off one file
/// read — the [`stored_provider`] shape, for the same reason: callers hold no
/// [`Profile`] or must not take the state flock for a read-only question. The
/// re-login tells [`ProxyClaim::Unreadable`] from [`ProxyClaim::None`]: a
/// table clauth cannot read still names an upstream account; each landing it
/// guards then refuses a config.toml this reads as [`ProxyClaim::None`]
/// without being able to tell, on its id check or claim
/// ([`require_profile_id`], [`claim_profile_id`]). Any
/// other guard on a write, a spend or a delete reads
/// [`stored_proxy_claim_failing_closed`].
pub(crate) fn stored_proxy_claim(name: &ProfileName) -> ProxyClaim {
    // A config clauth cannot read claims nothing, as an empty one.
    proxy_claim_in(&read_profile_config_text(name).unwrap_or_default())
}

/// [`stored_proxy_claim`] for a guard deciding whether clauth may write into,
/// spend, mark, adopt into or delete `name` as an account that is no proxy
/// account: what [`stored_proxy_claim`] reads as [`ProxyClaim::None`] because
/// it cannot tell is [`ProxyClaim::Unclassifiable`] here. A missing file
/// claims nothing, as [`load_profile`] reads it as an empty config: the
/// record's one writer ([`write_proxy_record`]) edits the file in place, so no
/// record exists without it.
pub(crate) fn stored_proxy_claim_failing_closed(name: &ProfileName) -> ProxyClaim {
    match profile_config_path(name) {
        Ok(path) => proxy_claim_failing_closed_at(&path),
        Err(_) => ProxyClaim::Unclassifiable,
    }
}

/// [`stored_proxy_claim_failing_closed`] for the profile whose credential
/// store is `store`, read off the `config.toml` beside it: a session's drain
/// knows its store's path, not a name to resolve under the home.
pub(crate) fn proxy_claim_beside_failing_closed(store: &Path) -> ProxyClaim {
    proxy_claim_failing_closed_at(&store.with_file_name("config.toml"))
}

/// The fail-closed proxy claim of the `config.toml` at `path`.
fn proxy_claim_failing_closed_at(path: &Path) -> ProxyClaim {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(_) => return ProxyClaim::Unclassifiable,
    };
    match parsed_proxy_claim(&raw) {
        Some(claim) => claim,
        None if has_proxy_header_line(&raw) => ProxyClaim::Unreadable,
        None if may_name_proxy_key(&raw) => ProxyClaim::Unclassifiable,
        None => ProxyClaim::None,
    }
}

/// Whether a `config.toml` text that does not parse may still name a `proxy`
/// key. TOML keys compare byte for byte, so such a key spells `proxy` out or
/// escapes it with a backslash. A text that does not parse has no grammar to
/// tell a key from a comment or a string, so either one anywhere counts: a
/// plain account whose broken config only mentions one stays refused until
/// the file is fixed. A text with a `[proxy]` header line never gets here: it
/// reads [`ProxyClaim::Unreadable`] first.
fn may_name_proxy_key(raw: &str) -> bool {
    raw.contains("proxy") || raw.contains('\\')
}

/// The proxy claim a `config.toml` text carries, as [`stored_proxy_claim`]
/// reads it.
fn proxy_claim_in(raw: &str) -> ProxyClaim {
    match parsed_proxy_claim(raw) {
        Some(claim) => claim,
        None if has_proxy_header_line(raw) => ProxyClaim::Unreadable,
        None => ProxyClaim::None,
    }
}

/// Whether a `config.toml` text that does not parse still opens a `[proxy]`
/// table on a line of its own.
fn has_proxy_header_line(raw: &str) -> bool {
    raw.lines().any(|line| line.trim() == "[proxy]")
}

/// The proxy claim of a `config.toml` text that parses as TOML; `None` when it
/// does not.
fn parsed_proxy_claim(raw: &str) -> Option<ProxyClaim> {
    #[cfg(test)]
    PROXY_CLAIM_READS.with(|reads| reads.set(reads.get() + 1));
    let table = raw.parse::<toml::Table>().ok()?;
    Some(match table.get("proxy") {
        None => ProxyClaim::None,
        Some(proxy) => proxy
            .clone()
            .try_into::<ProxyTable>()
            .ok()
            .as_ref()
            .and_then(proxy_record)
            .map_or(ProxyClaim::Unreadable, ProxyClaim::Account),
    })
}

#[cfg(test)]
thread_local! {
    /// The proxy-claim reads on this thread ([`stored_proxy_claim`] and the
    /// name-keyed classifier's record read alike), so a pin can see which
    /// paths parse a config.toml for a proxy record.
    static PROXY_CLAIM_READS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// How many proxy claims were read on this thread.
#[cfg(test)]
pub(crate) fn proxy_claim_reads() -> usize {
    PROXY_CLAIM_READS.with(std::cell::Cell::get)
}

/// The proxy-account record `name`'s config.toml carries; `None` for no
/// record and for one clauth cannot read alike, which every reader but the
/// delete treats as the generic-endpoint profile it then is.
pub(crate) fn stored_proxy_record(name: &ProfileName) -> Option<ProxyAccount> {
    stored_proxy_claim(name).into_record()
}

/// Write `name`'s proxy-account record into its `config.toml` in place — the
/// ONE write path for the record. A file already holding a `proxy` table (a
/// block table, an inline table, or dotted `proxy.*` keys) gets its `service`
/// and `account` values replaced where they sit (a `service` or `account` that
/// is itself a table is replaced by the value); a file without one gets the
/// table appended after its last item; a profile with no file yet gets one
/// holding the record. An existing record is replaced (the re-bind login's
/// re-mint). A file that is not TOML refuses with the load's own parse error
/// and is never written over; a file holding any CRLF line is written CRLF
/// throughout.
pub(crate) fn write_proxy_record(name: &ProfileName, record: &ProxyAccount) -> Result<()> {
    let path = profile_config_path(name)?;
    let raw = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        // A missing file reads as empty: the append below writes one holding
        // the record, so the create and the append stay one path.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => {
            return Err(e).with_context(|| format!("failed to read {name}/config.toml"));
        }
    };
    let lf = raw.replace("\r\n", "\n");
    let Ok(mut doc) = lf.parse::<toml_edit::DocumentMut>() else {
        // A file that is not TOML refuses with the load's own parse error;
        // the bytes on disk are never written over.
        return Err(ConfigParseRefusal::new(name.as_str(), &raw).into());
    };
    let service = toml::Value::from(record.service.as_str());
    let account = toml::Value::from(record.account.as_str());
    let set = |t: &mut dyn toml_edit::TableLike| {
        set_state_entry(t, "service", &service);
        set_state_entry(t, "account", &account);
    };
    let item = doc.get_mut("proxy");
    let non_table = item.as_ref().is_some_and(|i| !i.is_table_like());
    if let Some(table) = item.and_then(toml_edit::Item::as_table_like_mut) {
        set(table);
    } else {
        // A `proxy` value that is no table cannot hold the record: removed
        // with its comments kept, and the table appended, as a modelled table
        // does over a value where it belongs.
        if non_table {
            remove_key_keeping_comments(&mut doc, "proxy", &[], "");
        }
        append_root_table(&mut doc, "proxy", set);
    }
    let mut out = doc.to_string();
    if raw.contains("\r\n") {
        // Normalize before the expand, as the config save does: a multi-line
        // string's own `\r\n` survives the parse, and a blanket expand writes
        // a bare `\r` the file no longer parses.
        out = out.replace("\r\n", "\n").replace('\n', "\r\n");
    }
    atomic_write_600(&path, out).context("failed to write the proxy record")
}

/// Test-only home-dir override. Redirects all reads/writes away from real `~/.clauth`.
/// Never compiled into the binary.
#[cfg(test)]
static HOME_OVERRIDE: std::sync::Mutex<Option<PathBuf>> = std::sync::Mutex::new(None);

/// Serializes tests that redirect `home_dir()`: `HOME_OVERRIDE` and `$HOME` feed
/// the same resolution, so overlapping redirects bleed between parallel tests.
/// `testutil::HomeSandbox` and runtime's `with_fake_home` acquire it as RAII
/// guards.
#[cfg(test)]
pub(crate) static HOME_TEST_LOCK: crate::lockorder::RankedMutex<
    (),
    crate::lockorder::rank::HomeTest,
> = crate::lockorder::RankedMutex::new(());

#[cfg(test)]
pub(crate) fn set_home_override(path: PathBuf) {
    if let Ok(mut guard) = HOME_OVERRIDE.lock() {
        *guard = Some(path);
    }
}

#[cfg(test)]
pub(crate) fn clear_home_override() {
    if let Ok(mut guard) = HOME_OVERRIDE.lock() {
        *guard = None;
    }
}

/// Whether some live guard is currently redirecting [`home_dir`]. The question
/// it answers is "is a sandbox alive to own what I am about to register", not
/// "does THIS thread hold it": every redirect (`testutil::HomeSandbox`,
/// runtime's `with_fake_home`) sets the override for exactly its own lifetime
/// while holding `HOME_TEST_LOCK`, so a set override means one of them is live.
#[cfg(test)]
pub(crate) fn home_override_active() -> bool {
    HOME_OVERRIDE.lock().is_ok_and(|g| g.is_some())
}

/// The home every `~/.clauth` and `~/.claude` path is built from. Under `cfg(test)`
/// the override is the ONLY answer: falling back to the operator's real home there
/// writes into their live tree and takes the `~/.clauth/.lock` a running clauth
/// holds, so the test fails on contention it never staged while disturbing the
/// operator it never meant to touch. Panicking names the test that forgot its
/// sandbox at the moment it reaches for the home, which a returned `Err` would not:
/// callers that read a home path through `.ok()` swallow one and go quiet again.
pub(crate) fn home_dir() -> Result<PathBuf> {
    #[cfg(test)]
    {
        match HOME_OVERRIDE.lock().ok().and_then(|g| g.clone()) {
            Some(path) => Ok(path),
            None => panic!(
                "test resolved the operator's real home; hold a `testutil::HomeSandbox` \
                 across the whole test, background threads included"
            ),
        }
    }
    #[cfg(not(test))]
    {
        dirs::home_dir().context("cannot determine home directory")
    }
}

pub(crate) fn clauth_dir() -> Result<PathBuf> {
    Ok(home_dir()?.join(".clauth"))
}

pub(crate) fn claude_dir() -> Result<PathBuf> {
    Ok(home_dir()?.join(".claude"))
}

pub(crate) fn app_state_mtime() -> Option<SystemTime> {
    let path = app_state_path().ok()?;
    std::fs::metadata(&path).ok()?.modified().ok()
}

/// Everything a full config reload depends on, so an edit to a per-account
/// `config.toml` (which never touches `profiles.toml`) is still detected. Every
/// profile dir contributes `(name, its config.toml mtime or None)`; folding
/// EVERY mtime — not just the newest — means an edit to any config.toml flips the
/// fingerprint even when its mtime doesn't advance the max (a clock step back, an
/// mtime-preserving restore, two edits within one coarse mtime tick). The
/// `(name, None)` entries make a config.toml appearing/vanishing, or a whole
/// profile dir being added/removed, shift it too.
/// `Hash` is for a caller that must PERSIST this cheaply — `hook_note` stores it
/// in a JSON record and this is not a serde type. Hashing is sound for that use
/// because every consumer asks the same question the `Eq` impl does, "did any of
/// this move", and never reads a component back.
#[derive(Clone, PartialEq, Eq, Debug, Hash)]
pub(crate) struct ReloadFingerprint {
    profiles_toml_mtime: Option<SystemTime>,
    /// `codex-profiles.toml`'s mtime — a codex switch or chain edit writes that
    /// file and nothing else, so without this stat a codex state change would
    /// never shift the fingerprint. The dir walk below already covers a codex
    /// profile's `config.toml`; this is the only codex-specific stat needed.
    codex_toml_mtime: Option<SystemTime>,
    /// `proxies.toml`'s mtime — a registry edit decides which usage arm (the
    /// proxy's, or the generic endpoint scan) a proxy account's entry is built
    /// on, and touches no profile file, so without this stat a fixed row
    /// would wait for some unrelated reload to rebuild the entries.
    proxies_toml_mtime: Option<SystemTime>,
    /// `(profile dir name, config.toml mtime, session-token.json write time,
    /// credentials.json write time)`, each `None` when the file is absent, sorted
    /// by name so readdir order can't spuriously flip equality. The store rides
    /// here because a landed rotation writes it and nothing else, and a peer
    /// holding the old pair in memory must reload it. The sidecar rides here because a
    /// `login --setup-token` re-mint touches nothing else — without it the hot
    /// reload never sees a new/changed long-lived token. Its WRITE time, not its
    /// raw mtime: for a token-mode member that file is the store a swap stamps,
    /// and a bare stamp is not a config change. Reading the mtime rather than the
    /// contents also keeps the sidecar's bearer off this path, which runs per
    /// TUI frame.
    ///
    /// A CHOSEN consequence, not an accident (CLA-ROLL review): a rolling
    /// re-stamp genuinely rewrites the sidecar every couple of hours, so each
    /// re-stamp moves this fingerprint and triggers one full `load_config` in
    /// the TUI and the daemon. Telling a re-stamp apart from a re-mint would
    /// need content reads on this per-frame path, and the reload it costs is a
    /// milliseconds-scale walk per rolling profile per ~2h — accepted over
    /// giving the fingerprint eyes into credential bytes.
    config_mtimes: Vec<ProfileStamps>,
    /// `(profile dir name, account_id.json write time, auth.json write time)` —
    /// the identity files the reload compare keys on, in a SECOND per-name vec so
    /// the config tuple above keeps its shape. An anchor-only write (a re-anchor)
    /// or an auth.json-only write (codex rewriting its store in place) flips the
    /// fingerprint through this.
    identity_mtimes: Vec<IdentityStamps>,
}

/// One profile dir's entry in [`ReloadFingerprint::config_mtimes`].
type ProfileStamps = (
    String,
    Option<SystemTime>,
    Option<SystemTime>,
    Option<SystemTime>,
);

/// One profile dir's identity-file stamps in
/// [`ReloadFingerprint::identity_mtimes`]: the anchor and the codex `auth.json`,
/// each through [`crate::profile_cache::effective_write_time`].
type IdentityStamps = (String, Option<SystemTime>, Option<SystemTime>);

/// Filesystem stat of the reload triggers, plus the touch receipt beside each
/// store stamped through [`crate::profile_cache::effective_write_time`] (the
/// session-token, credentials, anchor and codex-auth files) — never the files'
/// contents, since this runs per TUI frame. Holds NO locks — `config` sits high
/// in the rank hierarchy, so this must stay lock-free — and fails soft: a
/// readdir/stat error contributes the empty value instead of erroring.
pub(crate) fn reload_fingerprint() -> ReloadFingerprint {
    let profiles_toml_mtime = app_state_mtime();
    let mut config_mtimes: Vec<ProfileStamps> = Vec::new();
    let mut identity_mtimes: Vec<IdentityStamps> = Vec::new();
    if let Ok(root) = profiles_root()
        && let Ok(entries) = std::fs::read_dir(&root)
    {
        for entry in entries.flatten() {
            if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            let config_mtime = std::fs::metadata(entry.path().join("config.toml"))
                .and_then(|m| m.modified())
                .ok();
            let token = crate::profile_cache::effective_write_time(
                &entry.path().join("session-token.json"),
            );
            let store =
                crate::profile_cache::effective_write_time(&entry.path().join("credentials.json"));
            let anchor = crate::profile_cache::effective_write_time(
                &entry
                    .path()
                    .join(crate::profile_cache::ACCOUNT_ID_CACHE_FILE),
            );
            let auth = crate::profile_cache::effective_write_time(&entry.path().join("auth.json"));
            config_mtimes.push((name.clone(), config_mtime, token, store));
            identity_mtimes.push((name, anchor, auth));
        }
    }
    config_mtimes.sort();
    identity_mtimes.sort();
    ReloadFingerprint {
        profiles_toml_mtime,
        codex_toml_mtime: crate::codex_profiles::codex_state_mtime(),
        proxies_toml_mtime: crate::proxy::registry_path()
            .ok()
            .and_then(|path| std::fs::metadata(path).ok())
            .and_then(|meta| meta.modified().ok()),
        config_mtimes,
        identity_mtimes,
    }
}

fn profiles_root() -> Result<PathBuf> {
    Ok(clauth_dir()?.join("profiles"))
}

fn app_state_path() -> Result<PathBuf> {
    Ok(clauth_dir()?.join("profiles.toml"))
}

pub(crate) fn profile_dir(name: &ProfileName) -> Result<PathBuf> {
    Ok(profiles_root()?.join(name.as_str()))
}

/// Every profile directory on disk, by name: the roster a reader falls back to
/// when profiles.toml does not load and it still has to attribute a credential
/// to its account (the macOS Keychain owner search). An unreadable profiles
/// root reads as no profiles.
pub(crate) fn profile_names_on_disk() -> Vec<ProfileName> {
    let Ok(entries) = profiles_root().and_then(|root| Ok(std::fs::read_dir(root)?)) else {
        return Vec::new();
    };
    entries
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_type().is_ok_and(|t| t.is_dir()))
        .filter_map(|entry| entry.file_name().into_string().ok())
        .map(ProfileName::from)
        .collect()
}

pub(crate) fn profile_subpath(name: &ProfileName, sub: &str) -> Result<PathBuf> {
    Ok(profile_dir(name)?.join(sub))
}

fn profile_config_path(name: &ProfileName) -> Result<PathBuf> {
    profile_subpath(name, "config.toml")
}

fn profile_credentials_path(name: &ProfileName) -> Result<PathBuf> {
    profile_subpath(name, "credentials.json")
}

pub(crate) fn profile_history_path(name: &ProfileName) -> Result<PathBuf> {
    Ok(profile_dir(name)?.join("usage_history.jsonl"))
}

/// One line from usage_history.jsonl.
#[derive(Deserialize)]
struct HistoryLine {
    ts: u64,
    #[serde(rename = "name")]
    _name: String,
    usage: UsageInfo,
}

/// Retention window shared by both per-profile series logs
/// (`usage_history.jsonl`, `wallet_history.jsonl`).
const HISTORY_RETENTION_MS: u64 = 2 * 24 * 60 * 60 * 1000;

/// Prune a profile's usage_history.jsonl to keep at most 2 days of entries.
/// Rewrites the file in place when there's anything to remove; no-op when it is
/// missing, unparseable, or already within the retention window.
///
/// Not a hot-path call: a full read + parse + rewrite per profile. The scheduler
/// runs it at startup and then on a coarse cadence (`HISTORY_PRUNE_INTERVAL_MS`),
/// under the fetch lease so the rewrite never races an append.
pub(crate) fn prune_usage_history(name: &ProfileName) {
    let Ok(path) = profile_history_path(name) else {
        return;
    };
    let Ok(content) = std::fs::read_to_string(&path) else {
        return;
    };
    let cutoff = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
        - HISTORY_RETENTION_MS;

    let mut kept: Vec<&str> = Vec::new();
    let mut pruned: usize = 0;
    for line in content.lines() {
        if let Ok(entry) = serde_json::from_str::<HistoryLine>(line) {
            if entry.ts >= cutoff {
                kept.push(line);
            } else {
                pruned += 1;
            }
        }
    }

    if pruned > 0 {
        let body = kept.join("\n");
        let body = if body.is_empty() { body } else { body + "\n" };
        // 0o600: the rename swaps the inode, so a plain write would revert the
        // history log (re-created 0o600 by the appender) to the umask.
        if let Err(e) = atomic_write_600(&path, body) {
            logline!("clauth: failed to prune usage history for {name}: {e}");
        }
    }
}

/// Open an append-only log under `~/.clauth` (a profile's
/// `usage_history.jsonl` / `wallet_history.jsonl`, the gateway's
/// `gateway.log`), creating it 0o600 on Unix, so it rides the owner-only
/// invariant rather than the process umask.
pub(crate) fn open_append_600(path: &Path) -> std::io::Result<std::fs::File> {
    let mut opts = std::fs::OpenOptions::new();
    opts.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    opts.open(path)
}

/// Append one live usage sample for `name` to its `usage_history.jsonl` — the
/// durable series both burn-rate readers replay ([`load_usage_history`]).
///
/// `prev` is the sample this one replaces, as the caller's shared store held it.
/// When it differs, a bridge line stamps it one ms before the new sample so an
/// idle stretch keeps its temporal density instead of replaying as one long
/// ramp. An unchanged sample writes nothing, so the log grows only when the
/// numbers actually moved.
///
/// With no `prev` the file's own last entry stands in for that comparison, so
/// nothing can re-append a line the log already holds. This is the cold-fill
/// case — a profile with no `usage_cache.json` for the startup bootstrap to
/// seed the store from — not the ordinary restart, where the seed makes `prev`
/// the cached value and the bridge is written normally. That branch never
/// bridges: with no seeded value, the span it would cross is unmeasured.
///
/// Both lines go out in one `write_all` so an append can never land interleaved.
/// Best-effort like [`prune_usage_history`]: a failure is logged, never fatal.
///
/// Appends are serialized by the fetch lease (one writer per tick), but the
/// retention trim is NOT under it: [`prune_usage_history`] rewrites through a
/// rename, so a starting process's startup trim can still drop an append that
/// lands mid-rewrite. One telemetry sample, bounded by the rewrite duration —
/// a sidecar lock on both sides is the upgrade path if that ever stops being
/// acceptable (flocking the log itself cannot work: the rename swaps the inode).
pub(crate) fn append_usage_sample(name: &ProfileName, prev: Option<&UsageInfo>, next: &UsageInfo) {
    append_usage_sample_at(name, prev, next, crate::usage::now_ms());
}

/// Time-injected body of [`append_usage_sample`] — the seam that lets a test
/// seed a history file through the REAL writer, bridge lines included, at
/// controlled timestamps. The queue classifier's pins go through here so they
/// cover the shape the writer actually produces rather than a handwritten
/// fixture (review round 2: the fixtures omitted bridges, and the classifier
/// self-confirmed on real files while its tests stayed green).
pub(crate) fn append_usage_sample_at(
    name: &ProfileName,
    prev: Option<&UsageInfo>,
    next: &UsageInfo,
    ts: u64,
) {
    // `fetched_at` is a cache-age clock, not a usage figure: two live reads of
    // the same numbers differ only in that stamp, so the unchanged check (and
    // the lines it writes) exclude it. Otherwise a quiet account would append
    // one line per poll instead of only when the numbers moved.
    let mut next_body = next.clone();
    next_body.fetched_at = None;
    let Ok(next_json) = serde_json::to_string(&next_body) else {
        return;
    };
    let bridge_json = prev.and_then(|p| {
        let mut p = p.clone();
        p.fetched_at = None;
        serde_json::to_string(&p).ok()
    });
    let unchanged = match &bridge_json {
        Some(json) => json == &next_json,
        None => load_usage_history(name)
            .last()
            .and_then(|(_, info)| {
                let mut info = info.clone();
                info.fetched_at = None;
                serde_json::to_string(&info).ok()
            })
            .is_some_and(|json| json == next_json),
    };
    if unchanged {
        return;
    }

    let Ok(path) = profile_history_path(name) else {
        return;
    };
    if let Some(dir) = path.parent()
        && let Err(e) = mkdir_700(dir)
    {
        logline!("clauth: failed to create the profile dir for {name}: {e}");
        return;
    }
    let name_json = serde_json::to_string(name).unwrap_or_else(|_| format!("\"{name}\""));
    let line =
        |ts: u64, usage: &str| format!("{{\"ts\":{ts},\"name\":{name_json},\"usage\":{usage}}}\n");
    let mut body = match &bridge_json {
        Some(json) => line(ts.saturating_sub(1), json),
        None => String::new(),
    };
    body.push_str(&line(ts, &next_json));

    match open_append_600(&path) {
        Ok(mut file) => {
            use std::io::Write;
            if let Err(e) = file.write_all(body.as_bytes()) {
                logline!("clauth: failed to append usage history for {name}: {e}");
            }
        }
        Err(e) => logline!("clauth: failed to open usage history for {name}: {e}"),
    }
}

/// Load all parsed entries from a profile's usage_history.jsonl.
/// Returns chronological (timestamp_ms, UsageInfo) pairs.
pub(crate) fn load_usage_history(name: &ProfileName) -> Vec<(u64, UsageInfo)> {
    let Ok(path) = profile_history_path(name) else {
        return vec![];
    };
    let Ok(content) = std::fs::read_to_string(&path) else {
        return vec![];
    };
    let mut entries: Vec<(u64, UsageInfo)> = content
        .lines()
        .filter_map(|line| {
            let entry: HistoryLine = serde_json::from_str(line).ok()?;
            Some((entry.ts, entry.usage))
        })
        .collect();
    entries.sort_by_key(|(ts, _)| *ts);
    entries
}

pub(crate) fn profile_wallet_history_path(name: &ProfileName) -> Result<PathBuf> {
    Ok(profile_dir(name)?.join("wallet_history.jsonl"))
}

/// Append this landing fetch's wallet readings to `wallet_history.jsonl` —
/// the durable balance series the wallet-burn rate replays
/// (`usage::burn::compute_wallet_rate_from_history_at`).
///
/// Mirrors [`append_usage_sample`]: per wallet, an unchanged reading (same
/// amount under the same `(label, currency)` identity) writes nothing, and a
/// changed one writes a bridge line stamping the previous amount 1 ms before
/// the new reading so an idle stretch keeps its temporal density instead of
/// replaying as one long ramp. Every wallet a provider reports is recorded —
/// the rate renders the funded one's, but a wallet that drains to zero and
/// later refills still needs its own series. One `write_all` per fetch so an
/// append can never land interleaved; best-effort, a failure is logged,
/// never fatal.
pub(crate) fn append_wallet_readings(name: &ProfileName, stats: &ThirdPartyStats) {
    append_wallet_readings_at(name, stats, crate::usage::now_ms());
}

/// Time-injected body of [`append_wallet_readings`] — the seam that lets a
/// test seed the series through the REAL writer, bridge lines included, at
/// controlled timestamps. Same role [`append_usage_sample_at`] plays for the
/// window series.
pub(crate) fn append_wallet_readings_at(name: &ProfileName, stats: &ThirdPartyStats, ts: u64) {
    let wallets = crate::providers::balance_wallets(&stats.rows);
    if wallets.is_empty() {
        return;
    }
    // Last recorded reading per `(label, currency)` identity: the unchanged
    // check and the bridge compare against each wallet's OWN prior reading,
    // never a bare file tail (a multi-wallet provider interleaves its
    // wallets' lines in one file).
    let prior: HashMap<(String, String), WalletSample> = load_wallet_history(name)
        .into_iter()
        .map(|s| ((s.label.clone(), s.currency.clone()), s))
        .collect();
    let mut body = String::new();
    for wallet in &wallets {
        let prev = prior.get(&(wallet.label.clone(), wallet.currency.clone()));
        if prev.is_some_and(|p| p.amount == wallet.amount) {
            continue; // unchanged reading: the log grows only when numbers moved
        }
        if let Some(p) = prev {
            extend_wallet_line(
                &mut body,
                ts.saturating_sub(1),
                &p.label,
                p.amount,
                &p.currency,
            );
        }
        extend_wallet_line(
            &mut body,
            ts,
            &wallet.label,
            wallet.amount,
            &wallet.currency,
        );
    }
    if body.is_empty() {
        return;
    }
    let Ok(path) = profile_wallet_history_path(name) else {
        return;
    };
    if let Some(dir) = path.parent()
        && let Err(e) = mkdir_700(dir)
    {
        logline!("clauth: failed to create the profile dir for {name}: {e}");
        return;
    }
    match open_append_600(&path) {
        Ok(mut file) => {
            use std::io::Write;
            if let Err(e) = file.write_all(body.as_bytes()) {
                logline!("clauth: failed to append wallet history for {name}: {e}");
            }
        }
        Err(e) => logline!("clauth: failed to open wallet history for {name}: {e}"),
    }
}

/// Serialize one series line onto `body`; skipped only on a serialization
/// failure, which this plain-data struct cannot produce.
fn extend_wallet_line(body: &mut String, ts: u64, label: &str, amount: f64, currency: &str) {
    if let Ok(json) = serde_json::to_string(&WalletSample {
        ts,
        label: label.to_string(),
        amount,
        currency: currency.to_string(),
    }) {
        body.push_str(&json);
        body.push('\n');
    }
}

/// Load all parsed entries from a profile's wallet_history.jsonl, sorted by
/// timestamp.
pub(crate) fn load_wallet_history(name: &ProfileName) -> Vec<WalletSample> {
    let Ok(path) = profile_wallet_history_path(name) else {
        return vec![];
    };
    let Ok(content) = std::fs::read_to_string(&path) else {
        return vec![];
    };
    let mut entries: Vec<WalletSample> = content
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect();
    entries.sort_by_key(|s| s.ts);
    entries
}

/// Prune a profile's wallet_history.jsonl to keep at most
/// [`HISTORY_RETENTION_MS`] of entries — the same window and rewrite shape
/// [`prune_usage_history`] applies to the usage series.
pub(crate) fn prune_wallet_history(name: &ProfileName) {
    let Ok(path) = profile_wallet_history_path(name) else {
        return;
    };
    let Ok(content) = std::fs::read_to_string(&path) else {
        return;
    };
    let cutoff = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
        - HISTORY_RETENTION_MS;

    let mut kept: Vec<&str> = Vec::new();
    let mut pruned: usize = 0;
    for line in content.lines() {
        if let Ok(entry) = serde_json::from_str::<WalletSample>(line) {
            if entry.ts >= cutoff {
                kept.push(line);
            } else {
                pruned += 1;
            }
        }
    }

    if pruned > 0 {
        let body = kept.join("\n");
        let body = if body.is_empty() { body } else { body + "\n" };
        // 0o600: the rename swaps the inode, so a plain write would revert the
        // history log (re-created 0o600 by the appender) to the umask.
        if let Err(e) = atomic_write_600(&path, body) {
            logline!("clauth: failed to prune wallet history for {name}: {e}");
        }
    }
}

fn profile_credentials_pending_path(name: &ProfileName) -> Result<PathBuf> {
    profile_subpath(name, "credentials.json.pending")
}

/// Whether `store` is a profile's `credentials.json` with a rotation staged
/// beside it that no load has resolved yet. A writer of that file outside the
/// rotation stands down while this holds, as the tier backfill does: a store
/// written after the stage reads as the newer commit, and
/// `recover_pending_credentials` then discards a rotated pair that may never
/// have landed.
pub(crate) fn rotation_staged_beside(store: &Path) -> bool {
    store.file_name().is_some_and(|f| f == "credentials.json")
        && store
            .with_file_name("credentials.json.pending")
            .symlink_metadata()
            .is_ok()
}

/// Tempfile + rename write; readers always see old or new, never partial.
/// Makes a staging name unique per WRITER, not merely per process. Two threads
/// of one process can aim an atomic write at ONE destination — two
/// `ProfileRuntime` watchdogs sharing a fake-symlink runtime tree — and a
/// pid-only name puts both on one staging path: one renames it away while the
/// other still holds an fd, so the loser's remaining writes land in the live
/// destination, non-atomically, and its own rename then fails ENOENT.
static TMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Staging path for an atomic publish of `path`: a hidden sibling, so the rename
/// stays within one directory (and therefore one filesystem).
pub(crate) fn tmp_sibling(path: &Path) -> PathBuf {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let file_name = path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".to_string());
    dir.join(format!(
        ".{file_name}.tmp.{}.{}",
        std::process::id(),
        TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ))
}

/// Read a regular file with a hard byte ceiling: stat before open (a FIFO is
/// refused instead of freezing the open), re-check the opened handle, and
/// bound the read with `take(cap + 1)`. Returns the UTF-8 text or a plain
/// detail string (an io error's own Display, "not a regular file", or the cap
/// refusal) — callers render their own copy around it. Shared by the MCP
/// prompt reader and the cc-exemption settings reader.
pub(crate) fn read_capped_file(path: &Path, cap: u64) -> std::result::Result<String, String> {
    let meta = std::fs::metadata(path).map_err(|e| e.to_string())?;
    if !meta.is_file() {
        return Err("not a regular file".to_string());
    }
    let file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let meta = file.metadata().map_err(|e| e.to_string())?;
    if !meta.is_file() {
        return Err("not a regular file".to_string());
    }
    let size = meta.len();
    if size > cap {
        return Err(format!("{size} bytes over the {cap} byte cap"));
    }
    read_validated_handle(file, cap)
}

/// Read an already-opened, type-checked handle with a hard byte ceiling. A
/// file can grow past the cap between a size check and the read; `take` bounds
/// the read to cap + 1, and a read that actually hit the bound is refused
/// instead of silently truncating. Invalid UTF-8 is refused at the byte
/// offset, never lossily decoded.
pub(crate) fn read_validated_handle(
    file: std::fs::File,
    cap: u64,
) -> std::result::Result<String, String> {
    let mut reader = std::io::Read::take(file, cap + 1);
    let mut buf = Vec::new();
    std::io::Read::read_to_end(&mut reader, &mut buf).map_err(|e| e.to_string())?;
    if buf.len() > cap as usize {
        return Err(format!("grew past the {cap} byte cap during the read"));
    }
    let text = std::str::from_utf8(&buf)
        .map_err(|e| format!("invalid UTF-8 at byte offset {}", e.valid_up_to()))?;
    Ok(text.to_string())
}

pub(crate) fn atomic_write(path: &Path, content: impl AsRef<[u8]>) -> std::io::Result<()> {
    atomic_write_with(path, |f| {
        use std::io::Write;
        f.write_all(content.as_ref())
    })
}

/// [`atomic_write`] with the content written by `write` into the temp file
/// once it is open. The temp is removed when `write` or the rename fails.
pub(crate) fn atomic_write_with(
    path: &Path,
    write: impl FnOnce(&mut std::fs::File) -> std::io::Result<()>,
) -> std::io::Result<()> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    if !dir.exists() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = tmp_sibling(path);
    let f = std::fs::File::create(&tmp)?;
    publish_temp(&tmp, path, f, write, None)
}

/// Write the open temp `f` at `tmp` by `write`, close it and rename it onto
/// `path`; the temp is removed when either step fails, so a failing writer
/// never leaves one behind (each temp name is used once). With `modified` the
/// closed temp takes that mtime before the rename.
fn publish_temp(
    tmp: &Path,
    path: &Path,
    mut f: std::fs::File,
    write: impl FnOnce(&mut std::fs::File) -> std::io::Result<()>,
    modified: Option<std::time::SystemTime>,
) -> std::io::Result<()> {
    let written = write(&mut f);
    drop(f);
    if let Err(e) = written {
        let _ = std::fs::remove_file(tmp);
        return Err(e);
    }
    if let Some(at) = modified {
        // Through a handle of its own, opened once the writing one closed:
        // Windows settles a written file's last write time when its writing
        // handle closes, after a time set through that same handle.
        let handle = std::fs::OpenOptions::new().write(true).open(tmp);
        #[cfg(test)]
        let handle = handle.map(|file| DatingHandle {
            file,
            fault: take_dating_fault(path),
        });
        let dated = handle.and_then(|file| file.set_modified(at));
        if let Err(e) = dated {
            let _ = std::fs::remove_file(tmp);
            return Err(e);
        }
    }
    match land(tmp, path) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(tmp);
            Err(e)
        }
    }
}

/// Like [`atomic_write`] but creates the temp file with mode 0o600 (Unix only)
/// so the file is never world-readable even for the instant before the rename,
/// and a stale temp at its name is removed first, so the mode is applied at
/// creation.
pub(crate) fn atomic_write_600(path: &Path, content: impl AsRef<[u8]>) -> std::io::Result<()> {
    atomic_write_600_dated(path, content, None)
}

/// [`atomic_write_600`] whose file lands already carrying the mtime
/// `modified`: the temp file takes it before the rename, which moves the name
/// and keeps the file's times, so no reader sees the new bytes under any
/// other mtime, and a write that fails leaves the old file's bytes and mtime
/// as they were.
pub(crate) fn atomic_write_600_dated(
    path: &Path,
    content: impl AsRef<[u8]>,
    modified: Option<std::time::SystemTime>,
) -> std::io::Result<()> {
    atomic_write_600_dated_with(
        path,
        |f| {
            use std::io::Write;
            f.write_all(content.as_ref())
        },
        modified,
    )
}

/// [`atomic_write_600`] with the content written by `write` into the temp
/// file once it is open, so a writer refused at the temp's creation (a
/// read-only or unwritable dir) never pays for building the content. The
/// temp is removed when `write` or the rename fails.
pub(crate) fn atomic_write_600_with(
    path: &Path,
    write: impl FnOnce(&mut std::fs::File) -> std::io::Result<()>,
) -> std::io::Result<()> {
    atomic_write_600_dated_with(path, write, None)
}

/// [`atomic_write_600_with`] landing with the mtime `modified` when given
/// ([`atomic_write_600_dated`]).
fn atomic_write_600_dated_with(
    path: &Path,
    write: impl FnOnce(&mut std::fs::File) -> std::io::Result<()>,
    modified: Option<std::time::SystemTime>,
) -> std::io::Result<()> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    if !dir.exists() {
        // A 0o600 file under a world-readable dir still leaks via the dir entry;
        // any dir this helper must create is 0o700 to keep the secret contained.
        mkdir_700(dir)?;
    }
    let tmp = tmp_sibling(path);
    // Clear any stale temp so `create_new` lands on a fresh inode — guarantees
    // the 0o600 mode is applied at creation, never inherited from a looser file.
    // Unique per writer, so this can only fire on a leftover from a crashed
    // process whose pid was recycled, never on a live sibling's staging file.
    if tmp.exists() {
        std::fs::remove_file(&tmp)?;
    }
    #[cfg(unix)]
    let f = {
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)?
    };
    #[cfg(not(unix))]
    let f = std::fs::File::create(&tmp)?;
    publish_temp(&tmp, path, f, write, modified)
}

/// [`publish_temp`]'s rename. In a test build the file is read at the instant
/// it lands, inside this step, so whatever the writer does after the rename
/// shows as a landed file that lacks it ([`set_landed_hook`]).
fn land(tmp: &Path, path: &Path) -> std::io::Result<()> {
    std::fs::rename(tmp, path)?;
    #[cfg(test)]
    run_landed_hook(path);
    Ok(())
}

/// A test's reader at the instant a dated write lands at a path, run once.
#[cfg(test)]
pub(crate) type LandedHook = std::sync::Arc<dyn Fn(&Path) + Send + Sync>;

#[cfg(test)]
static LANDED_HOOKS: std::sync::Mutex<Option<HashMap<PathBuf, LandedHook>>> =
    std::sync::Mutex::new(None);

#[cfg(test)]
pub(crate) fn set_landed_hook(path: &Path, f: LandedHook) {
    if let Ok(mut hooks) = LANDED_HOOKS.lock() {
        hooks
            .get_or_insert_with(HashMap::new)
            .insert(path.to_path_buf(), f);
    }
}

#[cfg(test)]
fn run_landed_hook(path: &Path) {
    let hook = LANDED_HOOKS
        .lock()
        .ok()
        .and_then(|mut hooks| hooks.as_mut().and_then(|hooks| hooks.remove(path)));
    if let Some(f) = hook {
        f(path);
    }
}

/// The paths whose next dated write fails at its dating step, as a failed
/// `set_modified` would; each fault fires once.
#[cfg(test)]
static DATING_FAULTS: std::sync::Mutex<Option<HashSet<PathBuf>>> = std::sync::Mutex::new(None);

#[cfg(test)]
pub(crate) fn fail_next_dating(path: &Path) {
    if let Ok(mut faults) = DATING_FAULTS.lock() {
        faults
            .get_or_insert_with(HashSet::new)
            .insert(path.to_path_buf());
    }
}

/// The dated writer's dating handle in a test build: an armed fault is the
/// result its `set_modified` answers, in place of the real call's, so the
/// writer's handling of that result is what the fault exercises.
#[cfg(test)]
struct DatingHandle {
    file: std::fs::File,
    fault: std::io::Result<()>,
}

#[cfg(test)]
impl DatingHandle {
    fn set_modified(self, at: std::time::SystemTime) -> std::io::Result<()> {
        self.fault.and_then(|()| self.file.set_modified(at))
    }
}

#[cfg(test)]
fn take_dating_fault(path: &Path) -> std::io::Result<()> {
    let planted = DATING_FAULTS
        .lock()
        .ok()
        .is_some_and(|mut faults| faults.as_mut().is_some_and(|faults| faults.remove(path)));
    if planted {
        Err(std::io::Error::other("planted dating fault"))
    } else {
        Ok(())
    }
}

/// Create `path` as a directory (recursively) with mode 0o700 on Unix,
/// or the default mode on non-Unix.
pub(crate) fn mkdir_700(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(path)
    }
    #[cfg(not(unix))]
    std::fs::create_dir_all(path)
}

/// Open an owner-only advisory-lock/state file (`read+write`, create without
/// truncating so a sibling's held lock survives the race) at mode 0o600. Every
/// `~/.clauth` lock file (`.lock`, `clauthd.lock`, `clauthd-standby.lock`,
/// `usage-fetch.lock`, session PID files, `rotation-locks/<name>.lock`,
/// `limit-reset-locks/<name>.lock`) routes
/// through here so no lock is born at the process umask — the file itself
/// carries nothing secret, but a blanket owner-only tree is the invariant the
/// perms test can check without an exceptions list.
pub(crate) fn open_state_file(path: &Path) -> std::io::Result<std::fs::File> {
    let mut opts = std::fs::OpenOptions::new();
    opts.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    opts.open(path)
}

/// Retighten an existing `~/.clauth` tree to the owner-only invariant (0o700
/// dirs, 0o600 files, 0o700 for a file whose owner exec bit is set).
/// Installs created before the invariant carry umask modes no writer revisits
/// once the bytes stop changing, so [`load_config`] runs this on every entry
/// point. Symlinks are skipped and never traversed: a shared-mode runtime is
/// full of links into the operator's `~/.claude`, and following one would
/// chmod a file clauth does not own. Best-effort per entry — a chmod failure
/// on one path never aborts the walk or the load.
pub(crate) fn enforce_clauth_perms(root: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let Ok(meta) = root.symlink_metadata() else {
            return;
        };
        if meta.file_type().is_symlink() {
            return;
        }
        let is_dir = meta.is_dir();
        let mode = meta.permissions().mode() & 0o777;
        // A file keeps an owner exec bit it already had: Claude Code writes
        // runtime wrappers like `chrome/chrome-native-host` 0755 and the
        // browser execs them, so forcing 0600 breaks the install. Only the
        // owner's own bit carries over; no group or other bit ever does.
        let want = if is_dir {
            0o700
        } else {
            0o600 | (mode & 0o100)
        };
        if mode != want {
            let _ = std::fs::set_permissions(root, std::fs::Permissions::from_mode(want));
        }
        // A codex home's CONTENTS are codex's own: the PATH-alias helper
        // binaries codex plants there carry exec bits a blanket 0600 would
        // strip. The home node itself keeps the 0700 invariant (just applied
        // above); the walk stops at its threshold rather than skipping the
        // node, and `auth.json`'s writer owns that file's 0600 itself instead
        // of relying on this sweep to retighten it.
        if is_dir && crate::runtime::is_codex_home_path(root) {
            return;
        }
        if is_dir && let Ok(entries) = std::fs::read_dir(root) {
            for entry in entries.flatten() {
                enforce_clauth_perms(&entry.path());
            }
        }
    }
    #[cfg(not(unix))]
    let _ = root;
}

/// Clear the group and other bits on the `~/.claude` dirs clauth's rescue and
/// store-link writers create — the rescue's [`crate::sessions::SIDECAR_TREES`],
/// the store-link dirs [`crate::runtime::SHARED_STORES`], and each direct child of
/// `projects/` (CC's and clauth's alike) — so the session-id listing those
/// trees expose stays owner-only. A dir those writers created under an older
/// build keeps its umask mode forever (nothing revisits it once the bytes stop
/// changing); [`load_config`] runs this on every entry point, beside the
/// `~/.clauth` walk.
///
/// Not every `~/.claude` dir a clauth writer makes is in the set: the fake-mode
/// mirror's other names and the nested rescue dirs under a named tree are born
/// 0700 via [`mkdir_700`] and stay outside it, and
/// [`crate::claude::publish_credential_link`] and
/// `apply_profile_to_claude_settings_inner` create `~/.claude` itself with
/// `create_dir_all` at the umask. The set is fixed and non-recursive: nothing
/// below the named dirs, never a file. A symlink for a named dir itself is
/// never followed (the open refuses it; the owner-read-off fallback is by
/// path, see [`tighten_owner_dir_by_path`]), but the `projects/` children are
/// listed and opened by path, so a swap of `projects/` for a link between the
/// listing gate and a child's open is followed — `O_NOFOLLOW` guards only the
/// last component, and closing the gap needs an `openat` walk std lacks
/// (the swap needs write on `~/.claude`: the owner alone unless that dir is
/// group- or other-writable). Best-effort: a chmod
/// failure on one path never aborts the load.
pub(crate) fn enforce_claude_dirs_perms() {
    #[cfg(unix)]
    {
        let Ok(claude) = claude_dir() else {
            return;
        };
        for name in crate::sessions::SIDECAR_TREES
            .iter()
            .copied()
            .chain(crate::runtime::SHARED_STORES.iter().map(|(name, _)| *name))
        {
            tighten_owner_dir(&claude.join(name));
        }
        let projects = claude.join("projects");
        // `read_dir` follows a symlink, so gate the listing on the node itself
        // being a real dir — a linked `projects/` must list nothing, not the
        // operator's children through the link.
        if let Ok(meta) = projects.symlink_metadata()
            && !meta.file_type().is_symlink()
            && meta.is_dir()
            && let Ok(entries) = std::fs::read_dir(&projects)
        {
            for entry in entries.flatten() {
                tighten_owner_dir(&entry.path());
            }
        }
    }
}

/// Clear the group and other bits on a real directory, decided by opening it
/// no-follow (`O_NOFOLLOW | O_DIRECTORY`) and chmodding the open handle: the
/// open itself refuses a symlink and a non-dir, and the handle pins the node,
/// so a path swapped for a link between the open and the chmod is never
/// followed. Owner bits and setgid stay as found — a 0500 dir is never widened
/// to 0700. A dir whose owner read bit is off (e.g. 0377) fails that open and
/// takes the [`tighten_owner_dir_by_path`] fallback.
#[cfg(unix)]
fn tighten_owner_dir(path: &Path) {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let dir = match std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_DIRECTORY)
        .open(path)
    {
        Ok(dir) => dir,
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            tighten_owner_dir_by_path(path);
            return;
        }
        Err(_) => return,
    };
    let Ok(mode) = dir.metadata().map(|m| m.permissions().mode()) else {
        return;
    };
    if mode & 0o077 != 0 {
        let _ = dir.set_permissions(std::fs::Permissions::from_mode(mode & !0o077));
    }
}

/// By-path retighten for a dir whose owner read bit is off: the no-follow open
/// needs owner read and fails `EACCES` (e.g. 0377), yet the group and other
/// bits are exactly what must go. `chmod` needs no read on the target, so this
/// re-checks the node is a real dir and clears them by path — a swap between
/// that check and the by-path chmod is followed (the swap needs write on the
/// parent: the owner alone unless it is group- or other-writable).
#[cfg(unix)]
fn tighten_owner_dir_by_path(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return;
    };
    if meta.file_type().is_symlink() || !meta.is_dir() {
        return;
    }
    let mode = meta.permissions().mode();
    if mode & 0o077 != 0 {
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode & !0o077));
    }
}

pub(crate) fn read_json_file<T: DeserializeOwned>(path: &Path) -> Result<T> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    serde_json::from_str(&content).map_err(|e| {
        crate::format::parse_error(path.display(), crate::format::json_parse_position(&e))
    })
}

/// [`read_json_file`] for a path the live-session count runs every tick: a
/// regular file only and bounded ([`crate::runtime::read_regular_capped`]),
/// since a blocking `open` on a FIFO never returns and a device read never
/// ends. A non-regular or unreadable file
/// reads `Err`, the same arm an unreadable file takes; the callers map it to
/// "no sidecar".
pub(crate) fn read_json_file_capped<T: DeserializeOwned>(path: &Path) -> Result<T> {
    let bytes = crate::runtime::read_regular_capped(path, crate::runtime::RECORD_READ_CAP)
        .ok()
        .flatten()
        .ok_or_else(|| anyhow::anyhow!("failed to read {}", path.display()))?;
    serde_json::from_slice(&bytes).map_err(|e| {
        crate::format::parse_error(path.display(), crate::format::json_parse_position(&e))
    })
}

pub(crate) fn read_toml_file<T: DeserializeOwned>(path: &Path) -> Result<T> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    toml::from_str(&content).map_err(|e| {
        crate::format::parse_error(
            path.display(),
            crate::format::toml_parse_position(&content, &e),
        )
    })
}

/// The configured active profile, read from `profiles.toml` alone. For callers
/// that need only this one field on a poll loop (the MCP digest samples at
/// 200ms while waiting): a full `load_config` there reads every profile's
/// config and credentials for one string.
pub(crate) fn active_profile_name() -> Option<ProfileName> {
    load_app_state()
        .ok()
        .and_then(|s| s.active_profile.into_inner())
}

/// `[plugin] idle_wake`, read lock-free from `profiles.toml`'s `[plugin]` table
/// alone, for the await hook's idle-wake decision: a hand-written file holding
/// only that table reads too. A file or table that does not read keeps the
/// wake on: only a readable `false` turns it off.
pub(crate) fn idle_wake_enabled() -> bool {
    #[derive(Deserialize)]
    struct PluginTable {
        #[serde(default)]
        plugin: PluginSettings,
    }
    app_state_path()
        .and_then(|path| read_toml_file::<PluginTable>(&path))
        .map_or(true, |t| t.plugin.idle_wake)
}

/// Whether the on-disk profile list still carries `name`, read from
/// `profiles.toml` alone and writing nothing.
///
/// For a caller that must ask this while holding the state flock. [`load_config`]
/// answers the same question, but per profile it adopts a pending credential
/// sidecar and rewrites `config.toml`, and it chmod-walks the whole `~/.clauth`
/// tree first. The standing ruling that rules it out there is not a cost
/// argument: the state flock is a cross-process serialization point nothing
/// holds across IO. That is why [`crate::lockorder::rank::ProfileTtl`] ranks
/// OUTSIDE the flock rather than inside it, and why `hook_note::ScopeLock` runs
/// its own `load_config` half BEFORE acquiring rather than under the hold —
/// same call, same question, already answered twice.
///
/// It deliberately does NOT agree with `load_config` everywhere, because the
/// file is what decides membership: a listed name whose `config.toml` is
/// unreadable fails `load_config` outright and answers `true` here. So a
/// PER-PROFILE read failure never turns into a refusal downstream, which is the
/// direction a refusal gate wants. `profiles.toml` itself is the opposite and
/// deliberately so — an unparseable account list propagates `Err`, because a
/// record that cannot be read is not a record a session should be started
/// against. Case-exact, matching [`AppConfig::find`].
///
/// Residual (name-keyed, deliberate): this answers "is a profile by this name
/// on disk", which cannot tell "still present" from "deleted, then re-logged-in
/// under the same name". A stale-chain gate consulting it can therefore act on
/// the NEW same-name account (e.g. an adopt persisting a pair from the old
/// chain; the rotation persist refuses a store its chain does not own, so it
/// never lands there). Only a full [`reload_fingerprint`] drift check closes
/// that, and no caller has needed the extra read; treat it as a documented
/// boundary, not an oversight.
pub(crate) fn is_configured(name: &ProfileName) -> Result<bool> {
    Ok(load_app_state()?.profiles.iter().any(|n| n == name))
}

/// The claude roster, read from `profiles.toml` alone — the name list without
/// the per-profile config/credential reads `load_config` does. For the
/// cross-harness name-uniqueness check, which needs both rosters from disk and
/// neither's profiles.
pub(crate) fn claude_roster_names() -> Result<Vec<ProfileName>> {
    Ok(load_app_state()?.profiles)
}

pub(crate) fn load_app_state() -> Result<AppState> {
    Ok(read_app_state()?.unwrap_or_default())
}

/// `profiles.toml`, `None` when it is missing, so a caller that cares can tell
/// a first run from a good read in one step.
pub(crate) fn read_app_state() -> Result<Option<AppState>> {
    let path = app_state_path()?;
    if !path.exists() {
        return Ok(None);
    }
    let mut state: AppState = read_toml_file(&path)?;
    state.refresh_interval_ms = state.refresh_interval_ms.max(MIN_REFRESH_INTERVAL_MS);
    // Normalize a hand-edited weekly line here, not on read alone: left raw the
    // out-of-band value survives every save and any direct field read trusts
    // it. Through the accessor so the band and its reset-to-default (never
    // clamp-to-nearest-bound) semantics stay defined in one place; an unset
    // field stays unset so `skip_serializing_if` keeps omitting it.
    if state.weekly_switch_threshold.is_some() {
        state.weekly_switch_threshold = Some(state.weekly_switch_threshold_pct());
    }
    // Same on-disk normalization for the burn-aware tunables: an out-of-band
    // hand-edit must not survive to the next save or a direct field read.
    if state.burn_switch_floor_pct.is_some() {
        state.burn_switch_floor_pct = Some(state.burn_switch_floor_pct());
    }
    if state.burn_horizon_cap_ms.is_some() {
        state.burn_horizon_cap_ms = Some(state.burn_horizon_cap_ms());
    }
    if state.context_nudge_threshold_tokens.is_some() {
        state.context_nudge_threshold_tokens = state.context_nudge_threshold_tokens();
    }
    Ok(Some(state))
}

/// The fresh-read proof for [`save_app_state`]. Sealed inside this module: only
/// [`FreshState`] (and, in test builds, an [`AppState`] a fixture authored) can
/// be handed to a save. The proof says the value came from a load inside a
/// flock hold, not what it holds: a caller can still overwrite it through
/// [`DerefMut`](std::ops::DerefMut).
mod fresh {
    pub(crate) trait Sealed {}
}

/// What [`save_app_state`] accepts. Closed by the private [`fresh::Sealed`]
/// supertrait, so no caller outside this module can implement it for a stale
/// snapshot of its own.
pub(crate) trait SaveState: fresh::Sealed {
    fn app_state(&self) -> &AppState;
}

/// An [`AppState`] read fresh from `profiles.toml` inside a state flock hold,
/// carrying the proof that [`save_app_state`] requires. [`Deref`]/[`DerefMut`]
/// keep field and method access the same as a bare [`AppState`], so a caller
/// can still overwrite the fresh value through [`DerefMut`].
///
/// It borrows the hold it was read under, so it cannot outlive that hold's
/// closure: a read in one hold saved after it, across a slow step, does not
/// compile (`with_state_lock(|h| load_fresh_state(h))` fails with "lifetime
/// may not live long enough", measured 2026-10-29). No test pins that, since a
/// compile failure cannot run in this suite.
pub(crate) struct FreshState<'h> {
    state: AppState,
    _hold: std::marker::PhantomData<&'h StateLockHeld>,
}

impl fresh::Sealed for FreshState<'_> {}
impl SaveState for FreshState<'_> {
    fn app_state(&self) -> &AppState {
        &self.state
    }
}

#[cfg(test)]
impl fresh::Sealed for AppState {}
#[cfg(test)]
impl SaveState for AppState {
    fn app_state(&self) -> &AppState {
        self
    }
}

impl FreshState<'_> {
    pub(crate) fn into_inner(self) -> AppState {
        self.state
    }
}

impl std::ops::Deref for FreshState<'_> {
    type Target = AppState;
    fn deref(&self) -> &AppState {
        &self.state
    }
}

impl std::ops::DerefMut for FreshState<'_> {
    fn deref_mut(&mut self) -> &mut AppState {
        &mut self.state
    }
}

/// Read `profiles.toml` fresh inside the caller's state flock hold, returning
/// the value with the proof [`save_app_state`] requires. The proof says the
/// value came from a load inside this hold, and lives no longer than it; a
/// caller can still overwrite it through [`DerefMut`].
pub(crate) fn load_fresh_state<'h>(_held: &'h StateLockHeld) -> Result<FreshState<'h>> {
    Ok(FreshState {
        state: load_app_state()?,
        _hold: std::marker::PhantomData,
    })
}

/// Test seam: [`save_app_state`] writes fail the way the read-only `~/.clauth`
/// leg would (`atomic_write_600`'s temp create refuses), so a test can land the
/// profile save and fail only the state write. Armed through the RAII
/// [`FailAppStateSave`] guard, cleared on drop so an arm that never reached a
/// save cannot leak into a sibling test.
#[cfg(test)]
static FAIL_APP_STATE_SAVE: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Test-only RAII: arm the [`save_app_state`] failure for the guard's lifetime,
/// clearing it on drop even if the test panics. It BORROWS the
/// [`HomeSandbox`](crate::testutil::HomeSandbox) for the reason
/// `testutil::EndpointSandbox` does: the override is a process-global serialized
/// by `HOME_TEST_LOCK`, which the home sandbox holds, so dropping the home first
/// is E0505 at compile time instead of a race nothing checks.
#[cfg(test)]
pub(crate) struct FailAppStateSave<'a>(std::marker::PhantomData<&'a crate::testutil::HomeSandbox>);

#[cfg(test)]
impl<'a> FailAppStateSave<'a> {
    pub(crate) fn new(_home: &'a crate::testutil::HomeSandbox) -> Self {
        FAIL_APP_STATE_SAVE.store(true, std::sync::atomic::Ordering::Relaxed);
        Self(std::marker::PhantomData)
    }
}

#[cfg(test)]
impl Drop for FailAppStateSave<'_> {
    fn drop(&mut self) {
        FAIL_APP_STATE_SAVE.store(false, std::sync::atomic::Ordering::Relaxed);
    }
}

/// Test seam: [`update_profile_config`]'s next write fails the way a read-only
/// profile dir would, after its fresh read landed, so a test can fail the save
/// alone. Armed through [`FailProfileConfigSave`], [`FailAppStateSave`]'s twin.
#[cfg(test)]
static FAIL_PROFILE_CONFIG_SAVE: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Test-only RAII: arm the [`update_profile_config`] write failure for the
/// guard's lifetime, borrowing the home sandbox as [`FailAppStateSave`] does.
#[cfg(test)]
pub(crate) struct FailProfileConfigSave<'a>(
    std::marker::PhantomData<&'a crate::testutil::HomeSandbox>,
);

#[cfg(test)]
impl<'a> FailProfileConfigSave<'a> {
    pub(crate) fn new(_home: &'a crate::testutil::HomeSandbox) -> Self {
        FAIL_PROFILE_CONFIG_SAVE.store(true, std::sync::atomic::Ordering::Relaxed);
        Self(std::marker::PhantomData)
    }
}

#[cfg(test)]
impl Drop for FailProfileConfigSave<'_> {
    fn drop(&mut self) {
        FAIL_PROFILE_CONFIG_SAVE.store(false, std::sync::atomic::Ordering::Relaxed);
    }
}

pub(crate) fn save_app_state<S: SaveState>(state: &S) -> Result<()> {
    #[cfg(test)]
    if FAIL_APP_STATE_SAVE.swap(false, std::sync::atomic::Ordering::Relaxed) {
        return Err(anyhow::anyhow!(
            "failed to write profiles.toml: Permission denied (os error 13)"
        ));
    }
    let state = state.app_state();
    with_state_lock(|held| {
        let path = app_state_path()?;
        let raw = read_state_file(&path)?;
        // The edit runs before anything is written, so a file the writer
        // refuses leaves the marker record untouched too.
        let edited = raw
            .as_deref()
            .map(|raw| edit_state_in_place(&path, raw, state))
            .transpose()?;
        // The record first: a failed record write leaves the marker as it
        // was, and only a crash between the two writes leaves them apart.
        if state.active_moved {
            let replaced = edited
                .as_ref()
                .and_then(|(_, disk)| disk.as_ref())
                .and_then(|disk| disk.active_profile.as_ref().cloned());
            crate::claude::note_active_marker(
                state.active_profile.as_ref(),
                replaced.as_ref(),
                held,
            )?;
        }
        write_state_file(&path, state, raw, edited.map(|(out, _)| out))
    })
}

/// A typed state file the in-place writer edits: `profiles.toml`,
/// `codex-profiles.toml` or `proxies.toml`.
pub(crate) trait StateFile: Serialize + serde::de::DeserializeOwned {
    /// The file's name, as its refusals and write errors print it.
    const LABEL: &'static str;

    /// The list keys the type cannot read without: a key read alone sits
    /// beside these, empty.
    const REQUIRED_LISTS: &'static [&'static str] = &[];

    /// Write a table the file does not hold yet into it, as a block table.
    fn insert_new_table(doc: &mut toml_edit::DocumentMut, key: &str, new: &toml::Table) {
        append_root_table(doc, key, |t| {
            for (sub, v) in new {
                t.insert(sub, toml_edit::Item::Value(state_edit_value(v)));
            }
        });
    }
}

impl StateFile for AppState {
    const LABEL: &'static str = "profiles.toml";
    const REQUIRED_LISTS: &'static [&'static str] = &["profiles"];
}

/// `state` saved over the file at `path`: edited in place over the file's own
/// bytes, rendered whole where there is no file. The caller holds the state
/// flock, so the raw read and the typed state belong to one hold.
pub(crate) fn save_state_in_place<T: StateFile>(path: &Path, state: &T) -> Result<()> {
    let raw = read_state_file(path)?;
    let edited = raw
        .as_deref()
        .map(|raw| edit_state_in_place(path, raw, state))
        .transpose()?;
    write_state_file(path, state, raw, edited.map(|(out, _)| out))
}

/// A state file's raw bytes, `None` when it does not exist.
fn read_state_file(path: &Path) -> Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(raw) => Ok(Some(raw)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("failed to read {}", path.display())),
    }
}

/// Write `edited` over `path` unless it equals the file's own bytes; with no
/// file to edit, the whole render of `state`.
fn write_state_file<T: StateFile>(
    path: &Path,
    state: &T,
    raw: Option<String>,
    edited: Option<String>,
) -> Result<()> {
    let out = match (raw, edited) {
        (Some(raw), Some(out)) if out == raw => return Ok(()),
        (_, Some(out)) => out,
        (_, None) => toml::to_string_pretty(state)
            .with_context(|| format!("failed to render {}", T::LABEL))?,
    };
    mkdir_700(&clauth_dir()?)?;
    atomic_write_600(path, out).with_context(|| format!("failed to write {}", T::LABEL))
}

/// A state file as it is on disk with `state` written into it in place: only
/// the keys whose typed value moved are touched, so comments, key order, list
/// layout and every key this clauth does not model stay as written (a newer
/// clauth's keys included). Returns the edited bytes and the typed state the
/// file held, `None` when it holds no readable state (an empty file).
///
/// Two shapes refuse and write nothing: a file that is not TOML (the load's
/// own parse error, position only) and a modelled key clauth cannot read into
/// its type ([`ConfigEditRefusal`], naming the key). Old and new are compared
/// as typed values, so a spelling the model reads as the same value (a serde
/// variant alias) stays as written until that setting changes.
fn edit_state_in_place<T: StateFile>(
    path: &Path,
    raw: &str,
    state: &T,
) -> Result<(String, Option<T>)> {
    let lf = raw.replace("\r\n", "\n");
    let typed = toml::from_str::<T>(raw);
    let parsed = lf
        .parse::<toml_edit::DocumentMut>()
        .ok()
        .zip(lf.parse::<toml::Table>().ok());
    let Some((mut doc, literal)) = parsed else {
        let position = match &typed {
            Err(e) => crate::format::toml_parse_position(raw, e),
            Ok(_) => None,
        };
        return Err(ConfigParseRefusal::at(path.display(), position).into());
    };
    let disk = match typed {
        Ok(disk) => Some(disk),
        Err(_) => {
            if let Some(key) = unreadable_state_key::<T>(&doc, &literal) {
                return Err(ConfigEditRefusal::new(T::LABEL, &key).into());
            }
            // No single key explains the failure: a file missing a required
            // key (an empty one) holds no state to diff against, so every
            // modelled key is written in.
            None
        }
    };
    let old = match &disk {
        Some(disk) => state_table(disk)?,
        None => toml::Table::new(),
    };
    let rendered = doc.to_string();
    apply_state_edits::<T>(&mut doc, &old, &state_table(state)?, &literal);
    let mut out = doc.to_string();
    if out == rendered {
        // The edit list changed nothing, so the file already holds this state:
        // hand back its own bytes (a missing final newline, mixed line endings)
        // and the save's raw comparison skips the write.
        return Ok((raw.to_string(), disk));
    }
    if raw.contains("\r\n") {
        // The config.toml writer's rule: normalize, then expand, so a CRLF
        // file stays CRLF, inserted lines included.
        out = out.replace("\r\n", "\n").replace('\n', "\r\n");
    }
    Ok((out, disk))
}

/// `state` as the typed table a save writes: the serializer's own view, so a
/// key at its `skip_serializing_if` default is absent.
fn state_table<T: StateFile>(state: &T) -> Result<toml::Table> {
    let render_failed = || format!("failed to render {}", T::LABEL);
    match toml::Value::try_from(state).with_context(render_failed)? {
        toml::Value::Table(t) => Ok(t),
        _ => Err(anyhow::anyhow!(render_failed())),
    }
}

/// `key = value` read alone as the file's type (beside the empty lists the
/// type requires), or `None` when the model cannot read it.
fn read_state_key_alone<T: StateFile>(key: &str, value: &toml::Value) -> Option<T> {
    let mut t = toml::Table::new();
    for required in T::REQUIRED_LISTS {
        t.insert((*required).to_string(), toml::Value::Array(Vec::new()));
    }
    t.insert(key.to_string(), value.clone());
    toml::Value::Table(t).try_into::<T>().ok()
}

/// Whether two values of `key` read as the same typed value: the comparison
/// that keeps an alias spelling as written when the model reads it as the
/// value the save holds.
fn reads_the_same<T: StateFile>(key: &str, a: &toml::Value, b: &toml::Value) -> bool {
    let view = |v| {
        read_state_key_alone::<T>(key, v)
            .and_then(|s| state_table(&s).ok())
            .map(|mut t| t.remove(key))
    };
    match (view(a), view(b)) {
        (Some(a), Some(b)) => a == b,
        _ => false,
    }
}

/// The first key, in file order, whose value the file's type cannot read on
/// its own: the key a shape refusal names.
fn unreadable_state_key<T: StateFile>(
    doc: &toml_edit::DocumentMut,
    literal: &toml::Table,
) -> Option<String> {
    doc.iter()
        .map(|(key, _)| key)
        .find(|key| {
            literal
                .get(*key)
                .is_some_and(|v| read_state_key_alone::<T>(key, v).is_none())
        })
        .map(str::to_string)
}

/// The per-key edit list of a state save: every top-level key whose typed
/// value moved between `old` (the file) and `new` (the save), and nothing
/// else. `literal` is the file's own untyped view, for the keys `old` omits.
fn apply_state_edits<T: StateFile>(
    doc: &mut toml_edit::DocumentMut,
    old: &toml::Table,
    new: &toml::Table,
    literal: &toml::Table,
) {
    for (key, n) in new {
        let o = old.get(key);
        if o == Some(n)
            || (o.is_none()
                && literal
                    .get(key)
                    .is_some_and(|l| reads_the_same::<T>(key, l, n)))
        {
            continue;
        }
        match n {
            toml::Value::Table(n) => {
                edit_state_table::<T>(doc, key, o.and_then(toml::Value::as_table), n, literal);
            }
            n => set_root_state_value(doc, key, n),
        }
    }
    for (key, o) in old {
        if new.contains_key(key) {
            continue;
        }
        match o.as_table() {
            Some(o) if doc.get(key).is_some_and(toml_edit::Item::is_table_like) => {
                let subkeys: Vec<&str> = o.keys().map(String::as_str).collect();
                unset_table(doc, key, &subkeys, &[], "");
            }
            _ => remove_key_keeping_comments(doc, key, &[], ""),
        }
    }
}

/// A modelled table written into the file: edited subkey by subkey where the
/// file already holds it in any table form (block, dotted or inline, the form
/// kept), else inserted as a new block table. A value where the table belongs
/// (the scalar `theme = "full"` a pick turns into `[theme]`) is removed with
/// its comments kept, and the table takes its place.
fn edit_state_table<T: StateFile>(
    doc: &mut toml_edit::DocumentMut,
    key: &str,
    old: Option<&toml::Table>,
    new: &toml::Table,
    literal: &toml::Table,
) {
    if !doc.get(key).is_some_and(toml_edit::Item::is_table_like) {
        if doc.contains_key(key) {
            remove_key_keeping_comments(doc, key, &[], "");
        }
        T::insert_new_table(doc, key, new);
        return;
    }
    let literal = literal.get(key).and_then(toml::Value::as_table);
    let alone = |sub: &str, v: &toml::Value| {
        toml::Value::Table([(sub.to_string(), v.clone())].into_iter().collect())
    };
    for (sub, n) in new {
        let o = old.and_then(|t| t.get(sub));
        if o == Some(n)
            || (old.is_none()
                && literal
                    .and_then(|t| t.get(sub))
                    .is_some_and(|l| reads_the_same::<T>(key, &alone(sub, l), &alone(sub, n))))
        {
            continue;
        }
        if let Some(t) = doc
            .get_mut(key)
            .and_then(toml_edit::Item::as_table_like_mut)
        {
            set_state_entry(t, sub, n);
        }
    }
    for sub in old.into_iter().flat_map(toml::Table::keys) {
        if !new.contains_key(sub) {
            remove_subkey_keeping_comments(doc, key, sub);
        }
    }
}

/// A modelled root value written into the file: in place where the file holds
/// a value, through the root-section append where it holds none, and in place
/// of a table where the file holds one there.
fn set_root_state_value(doc: &mut toml_edit::DocumentMut, key: &str, value: &toml::Value) {
    match doc.get(key) {
        Some(item) if item.is_value() => set_state_entry(doc.as_table_mut(), key, value),
        Some(_) => {
            remove_key_keeping_comments(doc, key, &[], "");
            append_root_value(doc, key, state_edit_value(value));
        }
        None => append_root_value(doc, key, state_edit_value(value)),
    }
}

/// One entry of a table set to `value`: a list of names is edited element by
/// element, any other value replaced keeping its decor, and an absent key
/// inserted at the table's end.
fn set_state_entry(t: &mut dyn toml_edit::TableLike, key: &str, value: &toml::Value) {
    let names = value.as_array().and_then(|a| {
        a.iter()
            .map(toml::Value::as_str)
            .collect::<Option<Vec<&str>>>()
    });
    match t.get_mut(key) {
        Some(item) if item.is_value() => {
            if let (Some(names), Some(arr)) = (names, item.as_array_mut())
                && arr.iter().all(toml_edit::Value::is_str)
            {
                edit_name_list(arr, &names);
                return;
            }
            set_value_preserving_decor(t, key, state_edit_value(value));
        }
        Some(_) => {
            t.remove(key);
            t.insert(key, toml_edit::Item::Value(state_edit_value(value)));
        }
        None => {
            t.insert(key, toml_edit::Item::Value(state_edit_value(value)));
        }
    }
}

/// A typed value as the value the writer inserts: a list it creates is one
/// line, a table inline (a block table is built by its own caller).
pub(crate) fn state_edit_value(v: &toml::Value) -> toml_edit::Value {
    match v {
        toml::Value::String(s) => s.as_str().into(),
        toml::Value::Integer(i) => (*i).into(),
        toml::Value::Float(f) => (*f).into(),
        toml::Value::Boolean(b) => (*b).into(),
        toml::Value::Datetime(d) => (*d).into(),
        toml::Value::Array(a) => toml_edit::Value::Array(a.iter().map(state_edit_value).collect()),
        toml::Value::Table(t) => toml_edit::Value::InlineTable(
            t.iter()
                .map(|(k, v)| (k.as_str(), state_edit_value(v)))
                .collect(),
        ),
    }
}

/// Which target slot a list edit fills: an element the two lists share in
/// order, a name that moved in from another position, a new name replacing a
/// removed one (taking its decor), or a new name with no removal to replace.
#[derive(Clone, Copy)]
enum ListSlot {
    Kept,
    Moved,
    Paired(usize),
    New,
}

/// A note written after an element's comma: the comment text and the line
/// break it sat before, kept apart so a re-attach into a one-line list can
/// mint a break of its own.
struct CommaNote {
    text: String,
    line_break: String,
}

/// Split the same-line comment off the head of `text` when the first line
/// holds one: the note (with its line break held apart) and the rest of the
/// text. `None` when the first line holds no `#`.
fn strip_head_note(text: &str) -> Option<(CommaNote, &str)> {
    let nl = text.find('\n')?;
    let before = &text[..nl];
    if !before.contains('#') {
        return None;
    }
    // The document reaches the editor LF-normalized, so a lifted break is
    // always "\n"; the writer re-expands it on a CRLF file.
    Some((
        CommaNote {
            text: before.to_string(),
            line_break: "\n".to_string(),
        },
        &text[nl + 1..],
    ))
}

/// Edit `arr` into `target` with the fewest element moves: the longest run the
/// two share in order stays put with its decor, a name that moved carries its
/// own comment to its new place, and a new name takes the list's own element
/// layout (its own line in a multi-line list, a `, ` in a one-line one).
///
/// Within the gap between two kept elements, a name leaving the list (not
/// moving elsewhere) and a name new to it pair off in order, the pair being one
/// value replacement that keeps the removed element's whole decor; a comment
/// above an unpaired removal moves onto the next element.
///
/// A note written after an element's comma is that element's, though toml_edit
/// parks it in the next element's prefix (or the trailing after the last
/// comma, or the last element's suffix when there is no trailing comma): each
/// such note is lifted off before the decor moves and put back right after
/// its owner's comma wherever the owner lands.
///
/// A comment on the open bracket's line is the list's, though toml_edit parks
/// it in the first element's prefix (or the trailing of an empty list): it is
/// lifted the same way and put back right after `[`.
fn edit_name_list(arr: &mut toml_edit::Array, target: &[&str]) {
    let cur: Vec<String> = arr
        .iter()
        .filter_map(|v| v.as_str().map(str::to_string))
        .collect();
    if cur.iter().map(String::as_str).eq(target.iter().copied()) {
        return;
    }
    // Lift each note after an element's comma off the decor it parked in,
    // keyed by owner name plus occurrence so two equal names never lend each
    // other a note. Every stripped decor keeps its line break.
    let mut seen = std::collections::HashMap::<String, usize>::new();
    let cur_keys: Vec<(String, usize)> = cur
        .iter()
        .map(|name| {
            let n = seen.entry(name.clone()).or_default();
            let key = (name.clone(), *n);
            *n += 1;
            key
        })
        .collect();
    let mut notes: std::collections::HashMap<(String, usize), CommaNote> =
        std::collections::HashMap::new();
    for (i, key) in cur_keys.iter().enumerate() {
        if i + 1 < arr.len() {
            let note = arr.get_mut(i + 1).and_then(|v| {
                let prefix = decor_prefix(v.decor()).to_string();
                strip_head_note(&prefix).map(|(note, rest)| {
                    v.decor_mut()
                        .set_prefix(format!("{}{}", note.line_break, rest));
                    note
                })
            });
            if let Some(note) = note {
                notes.insert(key.clone(), note);
            }
        } else if arr.trailing_comma() {
            let trailing = arr.trailing().as_str().unwrap_or("").to_string();
            let note = strip_head_note(&trailing).map(|(note, rest)| {
                arr.set_trailing(format!("{}{}", note.line_break, rest));
                note
            });
            if let Some(note) = note {
                notes.insert(key.clone(), note);
            }
        } else {
            let note = arr.get_mut(i).and_then(|v| {
                let suffix = decor_suffix(v.decor()).to_string();
                strip_head_note(&suffix).map(|(note, rest)| {
                    v.decor_mut()
                        .set_suffix(format!("{}{}", note.line_break, rest));
                    note
                })
            });
            if let Some(note) = note {
                notes.insert(key.clone(), note);
            }
        }
    }
    // A comment on the open bracket's line labels the list, though toml_edit
    // parks it in the first element's prefix head (or the trailing of an
    // empty list): held apart, it stays on the bracket's line whichever
    // element ends up first.
    let open_note = arr
        .get_mut(0)
        .and_then(|v| {
            let prefix = decor_prefix(v.decor()).to_string();
            strip_head_note(&prefix).map(|(note, rest)| {
                v.decor_mut()
                    .set_prefix(format!("{}{}", note.line_break, rest));
                note
            })
        })
        .or_else(|| {
            let trailing = arr.trailing().as_str().unwrap_or("").to_string();
            strip_head_note(&trailing).map(|(note, rest)| {
                arr.set_trailing(format!("{}{}", note.line_break, rest));
                note
            })
        });
    // The whitespace closing a list with no trailing comma rides the last
    // element's suffix, and so does a comment line after the last element:
    // held as the list's trailing text, both stay at the end whichever
    // element ends up last.
    if !arr.trailing_comma()
        && let Some(last) = arr.len().checked_sub(1)
    {
        let trailing = arr.trailing().as_str().unwrap_or("").to_string();
        let moved = arr.get_mut(last).and_then(|v| {
            let suffix = decor_suffix(v.decor()).to_string();
            (!suffix.is_empty() && (suffix.trim().is_empty() || suffix.contains('#'))).then(|| {
                v.decor_mut().set_suffix("");
                suffix
            })
        });
        if let Some(suffix) = moved {
            arr.set_trailing(format!("{suffix}{trailing}"));
        }
    }
    let prefixes: Vec<String> = arr
        .iter()
        .map(|v| decor_prefix(v.decor()).to_string())
        .collect();
    let first_layout = prefixes.first().map(|p| layout_of(p)).unwrap_or_default();
    let layout = prefixes
        .iter()
        .skip(1)
        .next_back()
        .or_else(|| prefixes.first().filter(|p| p.contains('\n')))
        .map_or_else(|| " ".to_string(), |p| layout_of(p));

    let (kept_cur, kept_target) = shared_run(&cur, target);
    // Count each target name, then spend one per kept element and one per
    // element that leaves its place: what remains is the names new to the list.
    let mut moves: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    for name in target {
        *moves.entry(name).or_default() += 1;
    }
    for (i, name) in cur.iter().enumerate() {
        if kept_cur[i] {
            spend_one(&mut moves, name);
        }
    }
    let mut moving = vec![false; cur.len()];
    for i in (0..cur.len()).rev() {
        if !kept_cur[i] && spend_one(&mut moves, &cur[i]) {
            moving[i] = true;
        }
    }
    let mut supply: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    for (i, name) in cur.iter().enumerate() {
        if moving[i] {
            *supply.entry(name.as_str()).or_default() += 1;
        }
    }
    let mut slots = vec![ListSlot::New; target.len()];
    for (j, name) in target.iter().enumerate() {
        if kept_target[j] {
            slots[j] = ListSlot::Kept;
        } else if spend_one(&mut supply, name) {
            slots[j] = ListSlot::Moved;
        }
    }
    // Pair, gap by gap between the elements both lists keep, a name leaving
    // the list with a new name: the pair replaces one value in the same place.
    let kept_at: Vec<usize> = (0..cur.len()).filter(|&i| kept_cur[i]).collect();
    let target_at: Vec<usize> = (0..target.len()).filter(|&j| kept_target[j]).collect();
    for gap in 0..=kept_at.len() {
        let cur_lo = if gap == 0 { 0 } else { kept_at[gap - 1] + 1 };
        let cur_hi = kept_at.get(gap).copied().unwrap_or(cur.len());
        let target_lo = if gap == 0 { 0 } else { target_at[gap - 1] + 1 };
        let target_hi = target_at.get(gap).copied().unwrap_or(target.len());
        let leaving = (cur_lo..cur_hi).filter(|&i| !moving[i]);
        let fresh: Vec<usize> = (target_lo..target_hi)
            .filter(|&j| matches!(slots[j], ListSlot::New))
            .collect();
        for (i, j) in leaving.zip(fresh) {
            slots[j] = ListSlot::Paired(i);
        }
    }
    let mut pair_of = vec![None; cur.len()];
    for (j, slot) in slots.iter().enumerate() {
        if let ListSlot::Paired(i) = slot {
            pair_of[*i] = Some(j);
        }
    }
    // Remove every element the target does not keep, in reverse so the indexes
    // stay the file's own. A paired removal keeps its decor for its
    // replacement, a moved one for its new place, and only an unpaired one
    // leaves its comment behind.
    let mut moved_values: Vec<(String, toml_edit::Value)> = Vec::new();
    let mut paired_values: Vec<Option<toml_edit::Value>> =
        (0..target.len()).map(|_| None).collect();
    for i in (0..cur.len()).rev() {
        if kept_cur[i] {
            continue;
        }
        if moving[i] {
            moved_values.push((cur[i].clone(), remove_list_element(arr, i, false)));
        } else if let Some(j) = pair_of[i] {
            paired_values[j] = Some(remove_list_element(arr, i, false));
        } else {
            // An unpaired removal leaves its lifted note behind as a comment
            // line of its own where it sat: riding the suffix, the carry
            // renders it at the list's element indent.
            if let Some(note) = notes.remove(&cur_keys[i])
                && let Some(v) = arr.get_mut(i)
            {
                // The note lands after the comments the element already
                // holds in its suffix, keeping the file's own order.
                let suffix = decor_suffix(v.decor()).to_string();
                v.decor_mut()
                    .set_suffix(format!("{suffix}{}{}", note.text, note.line_break));
            }
            remove_list_element(arr, i, true);
        }
    }
    // Pool the notes of the elements that survive as themselves, in file
    // order per name: the re-attach hands them to that name's target slots
    // in target order, so equal names keep their notes in file order while
    // a renamed copy keeps its own (still keyed in `notes`).
    let mut pool: std::collections::HashMap<String, Vec<CommaNote>> =
        std::collections::HashMap::new();
    for i in 0..cur.len() {
        if (kept_cur[i] || moving[i])
            && let Some(note) = notes.remove(&cur_keys[i])
        {
            pool.entry(cur[i].clone()).or_default().push(note);
        }
    }
    for (j, name) in target.iter().enumerate() {
        let slot = slots[j];
        match slot {
            ListSlot::Kept => {}
            ListSlot::Moved => {
                let value = moved_values
                    .iter()
                    .position(|(n, _)| n == name)
                    .map(|p| moved_values.remove(p).1);
                insert_list_element(arr, j, value, name, false, &layout, &first_layout);
            }
            ListSlot::Paired(_) => {
                let value = paired_values[j]
                    .take()
                    .map(|removed| replacement_value(name, &removed));
                insert_list_element(arr, j, value, name, true, &layout, &first_layout);
            }
            ListSlot::New => {
                insert_list_element(arr, j, None, name, false, &layout, &first_layout);
            }
        }
    }
    // Put each lifted note back right after its owner's comma: at the head of
    // the next element's prefix, of the trailing when the owner is last with a
    // trailing comma, of its own suffix when it is last without one.
    for j in 0..target.len() {
        let note = match slots[j] {
            ListSlot::Paired(i) => notes.remove(&cur_keys[i]),
            ListSlot::New => continue,
            _ => pool.get_mut(target[j]).and_then(|v| {
                if v.is_empty() {
                    None
                } else {
                    Some(v.remove(0))
                }
            }),
        };
        if let Some(note) = note {
            if j + 1 < target.len() {
                if let Some(next) = arr.get_mut(j + 1) {
                    let prefix = decor_prefix(next.decor()).to_string();
                    // A comment ends its line, so a prefix that does not
                    // start on a new line (a one-line list) needs one before
                    // the value, or the comment swallows it.
                    let line_break = if prefix.starts_with('\n') {
                        ""
                    } else {
                        note.line_break.as_str()
                    };
                    next.decor_mut()
                        .set_prefix(format!("{}{}{}", note.text, line_break, prefix));
                }
            } else if arr.trailing_comma() {
                let trailing = arr.trailing().as_str().unwrap_or("").to_string();
                // A close hugging the comma (`"b",]`) leaves no break between
                // the note and the `]` unless the note's own is minted.
                let line_break = if trailing.contains('\n') {
                    ""
                } else {
                    note.line_break.as_str()
                };
                arr.set_trailing(format!("{}{}{}", note.text, line_break, trailing));
            } else {
                let trailing = arr.trailing().as_str().unwrap_or("").to_string();
                if let Some(v) = arr.get_mut(j) {
                    let suffix = decor_suffix(v.decor()).to_string();
                    // The close newline rides the suffix or the trailing: when
                    // both hold none (a close on the last name's line, `"b"]`)
                    // the note mints its own, or the comment swallows the `]`.
                    let line_break = if suffix.contains('\n') || trailing.contains('\n') {
                        ""
                    } else {
                        note.line_break.as_str()
                    };
                    v.decor_mut()
                        .set_suffix(format!("{}{}{}", note.text, line_break, suffix));
                }
            }
        }
    }
    // The held-apart bracket comment goes back right after `[`: onto the
    // first element's prefix head, minting a line break when the list grew
    // out of an empty one, or onto the trailing when the list emptied.
    if let Some(note) = open_note {
        match arr.get_mut(0) {
            Some(first) => {
                let prefix = decor_prefix(first.decor()).to_string();
                let line_break = if prefix.starts_with('\n') {
                    ""
                } else {
                    note.line_break.as_str()
                };
                first
                    .decor_mut()
                    .set_prefix(format!("{}{}{}", note.text, line_break, prefix));
            }
            None => {
                let trailing = arr.trailing().as_str().unwrap_or("").to_string();
                let line_break = if trailing.starts_with('\n') {
                    ""
                } else {
                    note.line_break.as_str()
                };
                arr.set_trailing(format!("{}{}{}", note.text, line_break, trailing));
            }
        }
    }
    debug_assert!(
        notes.is_empty(),
        "every lifted note is re-attached or carried"
    );
    debug_assert!(
        pool.values().all(Vec::is_empty),
        "every pooled note finds a target slot"
    );
}

/// Take one unit of `name`'s remaining count: `false` when none is left.
fn spend_one(counts: &mut std::collections::HashMap<&str, usize>, name: &str) -> bool {
    counts
        .get_mut(name)
        .is_some_and(|n| match n.checked_sub(1) {
            Some(left) => {
                *n = left;
                true
            }
            None => false,
        })
}

/// A new name wearing a removed element's decor: one value replaced by another
/// in the same place.
fn replacement_value(name: &str, removed: &toml_edit::Value) -> toml_edit::Value {
    let mut v = toml_edit::Value::from(name);
    v.decor_mut().set_prefix(decor_prefix(removed.decor()));
    v.decor_mut().set_suffix(decor_suffix(removed.decor()));
    v
}

/// Which elements of `cur` and `target` lie on a longest common subsequence:
/// the elements a list edit leaves where they are.
fn shared_run(cur: &[String], target: &[&str]) -> (Vec<bool>, Vec<bool>) {
    let (n, m) = (cur.len(), target.len());
    let mut len = vec![vec![0usize; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            len[i][j] = if cur[i] == target[j] {
                len[i + 1][j + 1] + 1
            } else {
                len[i + 1][j].max(len[i][j + 1])
            };
        }
    }
    let mut kept_cur = vec![false; n];
    let mut kept_target = vec![false; m];
    let (mut i, mut j) = (0, 0);
    while i < n && j < m {
        if cur[i] == target[j] {
            kept_cur[i] = true;
            kept_target[j] = true;
            i += 1;
            j += 1;
        } else if len[i + 1][j] >= len[i][j + 1] {
            i += 1;
        } else {
            j += 1;
        }
    }
    (kept_cur, kept_target)
}

/// Remove element `idx`. With `carry`, every comment the element holds moves
/// to where the element sat: the block above it verbatim onto the element
/// after it (or the list's end) and its same-line comment onto its own line
/// at the list's element indent, leading newlines dropped so no blank line is
/// left behind. Without, the comments leave with the element. An element
/// taking first place takes the old first's layout.
fn remove_list_element(arr: &mut toml_edit::Array, idx: usize, carry: bool) -> toml_edit::Value {
    let removed = arr.remove(idx);
    let lead = decor_prefix(removed.decor()).to_string();
    let own = decor_suffix(removed.decor()).to_string();
    let comment = {
        let mut comment = lead
            .rfind('\n')
            .filter(|_| carry && lead.contains('#'))
            .map(|nl| lead[..=nl].to_string())
            .unwrap_or_default();
        if carry && own.contains('#') {
            let indent = layout_of(&lead);
            let indent = if indent.is_empty() {
                " "
            } else {
                indent.as_str()
            };
            let continuation = indent.trim_start_matches('\n');
            for line in own.lines().map(str::trim).filter(|l| l.starts_with('#')) {
                comment.push_str(if comment.is_empty() {
                    indent
                } else {
                    continuation
                });
                comment.push_str(line);
                comment.push('\n');
            }
        }
        comment
    };
    if let Some(next) = arr.get_mut(idx) {
        let existing = decor_prefix(next.decor()).to_string();
        if !comment.is_empty() {
            next.decor_mut()
                .set_prefix(format!("{comment}{}", existing.trim_start_matches('\n')));
        } else if idx == 0 {
            next.decor_mut()
                .set_prefix(format!("{}{}", layout_of(&lead), existing.trim_start()));
        }
    } else if !comment.is_empty() {
        let trailing = arr.trailing().as_str().unwrap_or("").to_string();
        arr.set_trailing(format!("{comment}{}", trailing.trim_start_matches('\n')));
    }
    removed
}

/// Insert `name` at `idx`: a moved element keeps the comment above it, a
/// replacement keeps the whole decor it carries, and any other element takes
/// the list's layout for that place.
fn insert_list_element(
    arr: &mut toml_edit::Array,
    idx: usize,
    value: Option<toml_edit::Value>,
    name: &str,
    keep_decor: bool,
    layout: &str,
    first_layout: &str,
) {
    let own_comment = value
        .as_ref()
        .is_some_and(|v| decor_prefix(v.decor()).contains('#'));
    let mut v = value.unwrap_or_else(|| {
        let mut v = toml_edit::Value::from(name);
        v.decor_mut().set_suffix("");
        v
    });
    if !keep_decor && !own_comment {
        v.decor_mut()
            .set_prefix(if idx == 0 { first_layout } else { layout });
    }
    if idx == 0
        && let Some(first) = arr.get_mut(0)
        && decor_prefix(first.decor()) == first_layout
    {
        first.decor_mut().set_prefix(layout);
    }
    arr.insert_formatted(idx, v);
}

/// An element prefix's layout without its comments: the line break and indent
/// before the value, or the space of a one-line list.
fn layout_of(prefix: &str) -> String {
    match prefix.rfind('\n') {
        Some(nl) => format!("\n{}", &prefix[nl + 1..]),
        None => prefix.to_string(),
    }
}

pub(crate) fn decor_prefix(decor: &toml_edit::Decor) -> &str {
    decor.prefix().and_then(|r| r.as_str()).unwrap_or("")
}

fn decor_suffix(decor: &toml_edit::Decor) -> &str {
    decor.suffix().and_then(|r| r.as_str()).unwrap_or("")
}

/// Set or clear `name`'s persisted `auth_broken` flag against the CURRENT
/// on-disk state, not an in-memory snapshot. A daemon leg that holds a stale
/// `AppConfig` (a CLI delete/rename/login landed since its last reload) must
/// not re-serialize the whole stale list over the change: reading profiles.toml
/// fresh and writing only the flag back preserves every concurrent account
/// mutation and can never resurrect a deleted profile's row.
///
/// A `broken` set for a name the on-disk list no longer carries is a no-op —
/// nothing is quarantined that does not exist. Returns whether the flag
/// actually changed on disk.
///
/// Same name-keyed residual as [`is_configured`]: this cannot distinguish a
/// profile that is still present from one deleted and re-logged-in under the
/// same name.
pub(crate) fn set_auth_broken_persisted(name: &ProfileName, broken: bool) -> Result<bool> {
    with_state_lock(|held| {
        let mut state = load_fresh_state(held)?;
        if broken && !state.profiles.iter().any(|n| n == name) {
            return Ok(false);
        }
        if !state.set_auth_broken(name, broken) {
            return Ok(false);
        }
        save_app_state(&state)?;
        Ok(true)
    })
}

/// Fresh read-modify-write of `profiles.toml` under the state flock: load the
/// current state, apply `f` with the flock witness, save, and return the saved
/// state. A closure that changes nothing writes nothing, so a no-op never
/// touches the file or moves every peer's reload fingerprint.
pub(crate) fn update_app_state(f: impl FnOnce(&mut AppState, &StateLockHeld)) -> Result<AppState> {
    with_state_lock(|held| {
        let mut state = load_fresh_state(held)?;
        let before = toml::to_string_pretty(&*state).context("failed to render profiles.toml")?;
        f(&mut state, held);
        if toml::to_string_pretty(&*state).context("failed to render profiles.toml")? != before {
            save_app_state(&state)?;
        }
        Ok(state.into_inner())
    })
}

/// A hand-editable percent field, normalized at the LOAD boundary so the
/// on-disk value is never a live trap for a direct reader (the 2026-07-14
/// weekly-line lesson). `nan` and `inf` are both valid TOML floats and both
/// survive `clamp`; a `NaN` threshold then reads false against every `>=` it
/// gates, silently disabling itself, and renders back out as `NaN`, which TOML
/// rejects. That bricks the next load of the file this module just rewrote, so
/// anything non-finite reads as unset (same shape as `max_auto_spend`'s guard).
fn finite_pct(raw: Option<f64>) -> Option<f64> {
    raw.filter(|v| v.is_finite()).map(|v| v.clamp(0.0, 100.0))
}

/// `[console]` → a usable credential, normalized at the LOAD boundary so no
/// reader below ever meets a half-filled table. A blank or absent token means
/// no session at all (the two are the same state to every consumer); an unset
/// or unrecognised `site`/`region` takes the vendor default rather than failing
/// the profile load, because a hand-edit there costs one re-login and not a
/// bricked account.
fn console_credential(raw: &ConsoleConfig) -> Option<ConsoleCredential> {
    let token = raw
        .token
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty())?;
    let region = raw
        .region
        .as_deref()
        .map(str::trim)
        .filter(|r| !r.is_empty())
        .unwrap_or(crate::providers::alibaba::DEFAULT_REGION);
    Some(ConsoleCredential {
        token: token.to_string(),
        site: raw
            .site
            .as_deref()
            .and_then(ConsoleSite::parse)
            .unwrap_or_default(),
        region: region.to_string(),
    })
}

/// Inverse of [`console_credential`] for the rewrite-drift comparison: the
/// table `render_config_toml` emits for this credential.
fn console_config(cred: Option<&ConsoleCredential>) -> ConsoleConfig {
    match cred {
        Some(c) => ConsoleConfig {
            token: Some(c.token.clone()),
            site: Some(c.site.as_str().to_string()),
            region: Some(c.region.clone()),
        },
        None => ConsoleConfig::default(),
    }
}

/// The managed endpoint a profile actually routes to, given what its
/// `config.toml` stores and whether it holds an OAuth pair.
///
/// The OAuth-bearer leak needs BOTH a stored pair AND no other inference
/// auth: with nothing else, CC would send that bearer to the third-party
/// base_url. Gate on the pair so a PURE api account (no pair) with a cleared
/// key keeps its base_url shell and stays re-loginable
/// (`clear_profile_api_key`). An `[env] ANTHROPIC_AUTH_TOKEN` /
/// `ANTHROPIC_API_KEY` entry counts too, the same env reading
/// [`crate::claude::has_inference_auth`] applies: the settings writer clears
/// its own token and applies `profile.env` LAST, so the env token is what the
/// spawned process authenticates with and the bearer never reaches the
/// endpoint; a proxy account's usable key leaves the entry out, whatever its
/// value, and sends itself instead. Both boundaries must count the same ENV shapes — the
/// preserve arm (`actions::overwrite_captured_profile`) keys on
/// [`crate::claude::has_own_inference_endpoint`], which is built on
/// [`crate::claude::has_inference_auth`], so an endpoint that predicate
/// preserves must survive the load. The api-key halves deliberately differ:
/// the preserve side validates the key ([`crate::claude::validate_api_key`],
/// the test that wires the `apiKeyHelper`), while this keeps a trim-non-empty
/// key so the CLI's keyless-refusal surface has an endpoint to name its fix
/// (`rolling_token_on_a_flagged_third_party_hybrid_names_the_split_state`).
/// Normalized at the LOAD boundary, same discipline as the `max_auto_spend`
/// case. This governs the managed base_url FIELD only: clauth never copies
/// `ANTHROPIC_BASE_URL` into `profile.env`, so an env override is always
/// operator-authored and is never normalized here — normalize the state
/// clauth authors, not an explicit one.
///
/// "Never normalized" is not "never read". An env override still ROUTES the
/// account (`build_claude_settings_json` applies `profile.env` last), so a
/// caller asking where requests go must ask [`stored_endpoint`], which reads
/// both sources. This function alone answers only the managed half, which is
/// why a `None` from it does not mean Anthropic.
///
/// One rule, four readers: [`load_profile`], and
/// [`stored_usage_cache_is_third_party`], [`stored_provider`] and
/// [`stored_endpoint`]'s managed half, which answer the same question without
/// the side effects.
pub(crate) fn effective_base_url(
    configured: Option<String>,
    has_credentials: bool,
    api_key: Option<&str>,
    env: &BTreeMap<String, String>,
) -> Option<String> {
    let has_usable_key = api_key.map(str::trim).is_some_and(|k| !k.is_empty());
    let has_env_token = env_has_inference_token(env);
    match configured {
        Some(_) if has_credentials && !has_usable_key && !has_env_token => None,
        other => other,
    }
}

/// Whether `env` carries an inference auth token: the env half of
/// [`crate::claude::has_inference_auth`] — the predicate the preserve arm's
/// gate ([`crate::claude::has_own_inference_endpoint`]) is built on — spelled
/// at the load boundary, same keys, same trimmed-non-empty test, so the env
/// halves cannot drift apart again. The one load-side spelling, shared by the
/// two load-side predicates that count it ([`effective_base_url`] and
/// [`usage_cache_is_third_party`]).
fn env_has_inference_token(env: &BTreeMap<String, String>) -> bool {
    inference_env_token(env).is_some()
}

/// The inference env token, read the same way [`env_has_inference_token`] reads
/// it; value-shaped so the digest and the boolean cannot drift.
pub(crate) fn inference_env_token(env: &BTreeMap<String, String>) -> Option<String> {
    ["ANTHROPIC_AUTH_TOKEN", "ANTHROPIC_API_KEY"]
        .iter()
        .find_map(|k| env.get(*k).map(|v| v.trim()).filter(|v| !v.is_empty()))
        .map(str::to_string)
}

/// Whether an account's usage figures live in `third_party_cache.json` rather
/// than the OAuth `usage_cache.json`, given the stored fields that decide it.
/// THE answer to "which cache holds this account's figures": every reader of
/// a cached figure asks this, and [`load_profile`]'s own seeding branch — the
/// step that decides whether a `Profile` carries `third_party_usage` at all — is
/// the producer, so it calls this rather than spelling the rule again.
///
/// A recognised provider, or a generic api-key endpoint whose discovered usage
/// the same leg caches. A stored pair with an env token serving inference and
/// nothing the third-party leg could fetch with answers OAuth-cache instead:
/// the chain is what feeds usage polling for such an account (the ruling's
/// model — the env-token account's inference spends on the token while the
/// pair serves usage), and a third-party reading would point every usage
/// surface at a cache nothing writes. Scoped to the env-token shape: a
/// pair+provider hybrid with no key and no env token keeps the provider arm
/// (pinned by the `which` tier test and the TUI's hybrid typing), however
/// unreachable that shape is past the load boundary, which drops its endpoint
/// for the bearer leak. The fetchability test mirrors
/// [`crate::usage::third_party_credentialed`], the fetch leg's own gate,
/// spelled off the fields because the lock-free readers hold no [`Profile`].
/// `api_key.is_some()` rather than a trimmed-non-empty test for the generic
/// disjunct, deliberately: whether a blank key can AUTHENTICATE a fetch is a
/// different question from where the figures would be written.
fn usage_cache_is_third_party(
    provider: Option<Provider>,
    base_url: Option<&str>,
    api_key: Option<&str>,
    has_credentials: bool,
    env: &BTreeMap<String, String>,
    proxy_recorded: impl FnOnce() -> bool,
) -> bool {
    let fetchable = matches!(provider, Some(Provider::Alibaba))
        || api_key.map(str::trim).is_some_and(|k| !k.is_empty());
    if has_credentials && !fetchable && env_has_inference_token(env) {
        return false;
    }
    // A proxy account's figures live in the third-party cache by RECORD, not
    // by key presence: a keyless (re-mint pending) proxy account is never
    // polled, and the copy its last poll wrote is still its figures. The usage
    // arm reads a proxy only on the loopback bind its `base_url` names
    // (`proxy_target_for`), so the record is asked for there alone, after the
    // pure test, the way that arm asks; off it, the account reads as before.
    provider.is_some()
        || base_url.is_some_and(|url| {
            api_key.is_some() || (crate::proxy::loopback_port(url).is_some() && proxy_recorded())
        })
}

/// [`Profile::usage_cache_is_third_party`] for a caller holding only a name,
/// read from one `config.toml` read and parse plus one stat; a keyless
/// loopback endpoint parses the same bytes once more for its `[proxy]`
/// record, and no answer reads the file twice. A config clauth cannot read or parse
/// proves no endpoint, so it answers `false` and its reader falls back to the
/// OAuth cache, whose absence renders `unknown` — the honest reading when clauth
/// cannot classify.
///
/// Deliberately NOT `load_profile(name)`: that path recovers a legacy `.pending`
/// rotation sidecar,
/// which takes the cross-process state flock and rewrites `credentials.json`. A
/// caller holding a leaf lock (the MCP digest samples at 5 Hz under
/// `rank::McpDigest`) would invert the lock order, and no caller asking a
/// read-only question should mutate a credential store.
///
/// **The divergence that buys, named in its direction**: this reads the
/// COMMITTED `credentials.json` only, so a profile whose pair exists solely as a
/// staged `credentials.pending` sidecar reads here as having NO credentials.
/// [`effective_base_url`] then KEEPS a `base_url` that a full load would drop
/// (the pair would reach the endpoint), so this answers `true` where
/// `load_profile` answers `false`, and a reader watches
/// `third_party_cache.json` for what is really an OAuth account. An env-token
/// profile keeps the ENDPOINT on both readers (the pair never reaches it),
/// but the classification still disagrees: this read sees no pair and answers
/// third-party where the adopting load answers OAuth-cache. Only the
/// caller that skips the full load, [`crate::mcp::digest`]'s sample, can
/// observe it; every other caller runs
/// `load_config` first, and `recover_pending_credentials` consumes the sidecar.
/// The MCP `load_usage` (the roster rank and the delegate pre-flight) is a
/// lock-free read like this one, but both its callers run `load_config` first,
/// so the staged pair is gone before it reads — the divergence costs at most
/// one digest call reporting no refresh. Pinned, in that
/// direction, by
/// `a_staged_pair_is_the_one_state_the_lock_free_read_reads_differently` (both
/// spellings), and pinned agreeing everywhere else by
/// `the_lock_free_third_party_read_agrees_with_a_full_load` and, for a
/// keyless proxy account, `both_readers_read_a_keyless_proxy_account_as_third_party`.
pub(crate) fn stored_usage_cache_is_third_party(name: &ProfileName) -> bool {
    let Some(raw) = read_profile_config_text(name) else {
        return false;
    };
    let Ok(config) = toml::from_str::<ProfileConfig>(&raw) else {
        return false;
    };
    let has_credentials = stored_credentials_file_exists(name);
    let base_url = effective_base_url(
        config.base_url,
        has_credentials,
        config.api_key.as_deref(),
        &config.env,
    );
    usage_cache_is_third_party(
        base_url.as_deref().and_then(Provider::from_base_url),
        base_url.as_deref(),
        config.api_key.as_deref(),
        has_credentials,
        &config.env,
        || matches!(proxy_claim_in(&raw), ProxyClaim::Account(_)),
    )
}

/// Read and parse `name`'s `config.toml`, lock-free — the prologue every
/// name-keyed lock-free reader shares. `None` when the config cannot be read or
/// parsed.
fn read_profile_config(name: &ProfileName) -> Option<ProfileConfig> {
    toml::from_str::<ProfileConfig>(&read_profile_config_text(name)?).ok()
}

/// `name`'s `config.toml` text, lock-free; `None` when it cannot be read.
fn read_profile_config_text(name: &ProfileName) -> Option<String> {
    std::fs::read_to_string(profile_config_path(name).ok()?).ok()
}

/// The [`ProfileId`] `name`'s `config.toml` holds now, off disk.
pub(crate) fn stored_profile_id(name: &ProfileName) -> Option<ProfileId> {
    profile_id_at(&profile_config_path(name).ok()?)
}

/// The [`ProfileId`] a login, capture or adopt starting on `name` lands by,
/// minted into its `config.toml` when an older clauth created it without one.
pub(crate) fn claim_profile_id(name: &ProfileName) -> Result<ProfileId> {
    with_state_lock(|_held| {
        // The roster first: a leftover directory no account owns is no one's.
        if !claude_roster_names()?.contains(name) {
            anyhow::bail!("profile '{name}' no longer exists");
        }
        if let Some(id) = stored_profile_id(name) {
            return Ok(id);
        }
        let id = ProfileId::mint()?;
        write_config_render(&read_profile(name)?, Some(&id), false)?;
        Ok(id)
    })
}

/// `name`'s profile read fresh, only while it is still the account `account`
/// was claimed from: what a CLI login shows and reuses after its claim.
pub(crate) fn read_claimed_profile(name: &ProfileName, account: &ProfileId) -> Result<Profile> {
    with_state_lock(|held| {
        require_profile_id(name, account, held)?;
        read_profile(name)
    })
}

/// Refuse unless `name` still holds the account `expected` was claimed from: a
/// rename away and a delete-then-recreate both leave another id, or none, under
/// the name, and a name dropped from the roster leaves a directory no account
/// owns. The caller holds the state flock across this and its write.
pub(crate) fn require_profile_id(
    name: &ProfileName,
    expected: &ProfileId,
    _held: &StateLockHeld,
) -> Result<()> {
    if !claude_roster_names()?.contains(name) || stored_profile_id(name).as_ref() != Some(expected)
    {
        anyhow::bail!("profile '{name}' no longer exists");
    }
    Ok(())
}

/// The recognised [`Provider`] for `name`, read lock-free off one `config.toml`
/// read the same way [`stored_usage_cache_is_third_party`] reads its own answer.
/// `None` for an unreadable config, an OAuth account, and a generic api-key
/// endpoint alike. `profile_windows_for` carries it so the headroom prose can
/// deny a 5h/7d limit from the PROVIDER rather than from one cached response's
/// bar count. Classified off the managed `base_url` only, matching the typed
/// integration an account is scheduled under — an operator-authored
/// `ANTHROPIC_BASE_URL` reroutes the request but does not change which provider
/// clauth typed it as.
pub(crate) fn stored_provider(name: &ProfileName) -> Option<Provider> {
    let config = read_profile_config(name)?;
    let has_credentials = stored_credentials_file_exists(name);
    let base_url = effective_base_url(
        config.base_url,
        has_credentials,
        config.api_key.as_deref(),
        &config.env,
    );
    base_url.as_deref().and_then(Provider::from_base_url)
}

/// Where an account's requests actually GO, for a caller holding only a name.
///
/// The question is "which endpoint answers this account's calls" — never "is
/// this a recognised provider" ([`Profile::is_third_party`]) and never "which
/// cache holds this account's figures"
/// ([`Profile::usage_cache_is_third_party`]). The three disagree on a generic
/// api-key endpoint and on a hybrid, so a reader that borrows the wrong one
/// inherits its question rather than its answer.
///
/// Nor is it [`Profile::is_oauth`], which reads the managed `base_url` field
/// alone: an account routes through an operator-authored
/// `[env] ANTHROPIC_BASE_URL` too, and does so even when the managed field is
/// empty. This type answers over BOTH sources. A caller holding a loaded
/// [`Profile`] asks [`Profile::routing_endpoint`] instead, which answers the
/// same question over the same two sources in the same order.
pub(crate) enum StoredEndpoint {
    /// Requests go to Anthropic's own endpoint: no `[env] ANTHROPIC_BASE_URL`
    /// and no effective managed `base_url`.
    Anthropic,
    /// Requests go to this endpoint.
    Custom(String),
    /// The stored config could not be read or parsed, so clauth cannot say.
    /// A distinct arm rather than a fallback to [`Self::Anthropic`], because
    /// every caller so far is deciding whether a figure may be presented as
    /// Anthropic's, and guessing that on an unreadable config asserts the one
    /// thing it has no evidence for.
    Unknown,
}

/// [`StoredEndpoint`] for `name`, off ONE read of its `config.toml`.
///
/// Both endpoint sources are consulted, and the profile `[env]` entry wins,
/// because that is the order the PRODUCER applies them:
/// [`crate::claude::build_claude_settings_json`] writes the managed `base_url`
/// into the settings env block and then inserts `profile.env` last, so an
/// explicit `ANTHROPIC_BASE_URL` there is what the spawned `claude` reads.
/// Deciding the precedence any other way would describe an endpoint no request
/// ever reaches. An entry that is blank once trimmed is no override, the same
/// test [`crate::claude::has_inference_auth`] applies to the env entries it
/// reads.
///
/// Read lock-free the same way [`stored_usage_cache_is_third_party`] reads its
/// own answer — deliberately NOT through [`load_profile`], whose
/// staged-rotation recovery takes the state flock and would invert the lock
/// order under the MCP layer's leaves.
///
/// It carries that read's documented divergence too, in the direction that
/// fails safe: a profile whose OAuth pair exists only as a staged
/// `credentials.pending` sidecar reads here as having none, so
/// [`effective_base_url`] KEEPS a `base_url` a full load would drop and the
/// caller qualifies a figure it need not have. A profile whose `[env]` carries
/// an auth token keeps the endpoint on both readers, so the divergence is the
/// no-env-auth shape alone.
pub(crate) fn stored_endpoint(name: &ProfileName) -> StoredEndpoint {
    let Some(config) = read_profile_config(name) else {
        return StoredEndpoint::Unknown;
    };
    if let Some(url) = config
        .env
        .get("ANTHROPIC_BASE_URL")
        .map(|v| v.trim())
        .filter(|v| !v.is_empty())
    {
        return StoredEndpoint::Custom(url.to_string());
    }
    let has_credentials = stored_credentials_file_exists(name);
    match effective_base_url(
        config.base_url,
        has_credentials,
        config.api_key.as_deref(),
        &config.env,
    ) {
        Some(url) => StoredEndpoint::Custom(url),
        None => StoredEndpoint::Anthropic,
    }
}

/// The identity-relevant endpoint fields of `name`'s stored `config.toml`, read
/// lock-free (never `load_profile`, which takes the state flock). `base_url` is
/// the EFFECTIVE managed base_url when [`stored_endpoint_identity`] reads it, and
/// the RAW managed field when a change site builds the struct for
/// [`crate::usage::endpoint_identity_plan`], which derives the effective form
/// itself; the api key, the console credential and the raw env map are the same
/// either way. `None` when the config cannot be read.
pub(crate) struct StoredEndpointIdentity {
    pub(crate) base_url: Option<String>,
    pub(crate) api_key: Option<String>,
    pub(crate) console: Option<ConsoleCredential>,
    pub(crate) env: BTreeMap<String, String>,
}

/// Whether `name`'s `credentials.json` exists (the throughput identity's
/// effective-base_url guard).
pub(crate) fn stored_credentials_file_exists(name: &ProfileName) -> bool {
    profile_credentials_path(name).is_ok_and(|p| p.exists())
}

/// [`StoredEndpointIdentity`] for `name`, off ONE read of its `config.toml`,
/// the same lock-free shape [`stored_endpoint`] / [`stored_provider`] use.
pub(crate) fn stored_endpoint_identity(name: &ProfileName) -> Option<StoredEndpointIdentity> {
    let config = read_profile_config(name)?;
    let has_credentials = stored_credentials_file_exists(name);
    let base_url = effective_base_url(
        config.base_url,
        has_credentials,
        config.api_key.as_deref(),
        &config.env,
    );
    Some(StoredEndpointIdentity {
        base_url,
        api_key: config.api_key,
        console: console_credential(&config.console),
        env: config.env,
    })
}

pub(crate) fn load_profile(name: &ProfileName) -> Result<Profile> {
    load_profile_with(name, true)
}

/// [`load_profile`]'s read-only form, used by the shared swap-target check and
/// the `clauth mcp` handshake: the same read, but never through
/// [`maybe_rewrite_config_toml`] and never through a legacy `.pending` sidecar's
/// write-through, both of which take the state flock — a `config.toml` a full
/// load would normalize is left as it is, and an adoptable sidecar is answered
/// from memory and left for the next ordinary load to land.
pub(crate) fn read_profile(name: &ProfileName) -> Result<Profile> {
    load_profile_with(name, false)
}

fn load_profile_with(name: &ProfileName, rewrite: bool) -> Result<Profile> {
    let mut source = read_profile_source(name)?;
    // A live `.staged` pair loads in place of the store; a legacy `.pending` that
    // never committed is adopted.
    source.credentials = recover_pending_credentials(name, source.credentials, rewrite);
    let (profile, config_path, raw_config) = build_profile(name, source);
    if rewrite {
        maybe_rewrite_config_toml(&config_path, &raw_config, &profile);
    }
    notice_profile_load(&profile);
    Ok(profile)
}

/// The thread the load notices may print on, if any. Off by default — the
/// TUI's stderr is its alternate screen, the daemon's is `daemon.log`, the MCP
/// server's is Claude Code's, a hook's is its caller's — so only the CLI
/// dispatch arms, and for the user-facing commands alone
/// ([`enable_load_notices`]). The arming thread is recorded because the flag
/// is process-wide while stderr belongs to the foreground thread: a `clauth
/// start` session's watchdog thread loads the roster mid-session, and a notice
/// printed there would reach `errln!` — which panics on a write error no
/// scheduler thread can answer for — on a screen Claude Code has already
/// painted.
static LOAD_NOTICES: std::sync::Mutex<Option<std::thread::ThreadId>> = std::sync::Mutex::new(None);

/// The key of every notice already printed, so however many times one process
/// reloads the roster, one torn store or one unaddressable name yields one
/// line.
static LOAD_NOTICED: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

fn load_notices_thread() -> std::sync::MutexGuard<'static, Option<std::thread::ThreadId>> {
    LOAD_NOTICES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Turn the load notices on for the calling thread, and no other. Called once
/// by the CLI dispatch, and only for the user-facing commands.
pub(crate) fn enable_load_notices() {
    *load_notices_thread() = Some(std::thread::current().id());
}

/// Whether this thread is the one [`enable_load_notices`] armed.
fn load_notices_armed_here() -> bool {
    *load_notices_thread() == Some(std::thread::current().id())
}

/// One load notice and the key it is deduplicated by.
struct LoadNotice {
    /// The identity the once-per-process set holds: a torn store keys by
    /// ACCOUNT, so a reason whose text moved between two loads (a parse
    /// failure, then an I/O failure) cannot print a second line; a name notice
    /// keys by its account name too, so the two rosters — whose copy differs
    /// by remedy — cannot print two lines for one name.
    key: String,
    line: String,
}

impl LoadNotice {
    /// A name notice keyed by ACCOUNT, so the line of whichever roster loads
    /// first is the only one printed.
    fn for_name(notice: NameNotice) -> Self {
        Self {
            key: format!("name:{}", notice.name),
            line: notice.cli_line(),
        }
    }
}

/// Print each notice once per process, skipping any key already told. No-op
/// unless [`enable_load_notices`] ran on THIS thread.
fn print_load_notices(notices: impl IntoIterator<Item = LoadNotice>) {
    if !load_notices_armed_here() {
        return;
    }
    let mut told = LOAD_NOTICED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    for notice in notices {
        if told.contains(&notice.key) {
            continue;
        }
        crate::out::errln!("{}", notice.line);
        told.push(notice.key);
    }
}

/// The stderr lines a loaded account warrants: its unreadable store, then a
/// name the printed commands cannot address.
fn notice_profile_load(profile: &Profile) {
    let store = unreadable_store_notice(profile).map(|line| LoadNotice {
        // A key no name-notice can equal: those all start with `name:`.
        key: format!("torn-store:{}", profile.name),
        line,
    });
    let name = NameNotice::of(&profile.name, Harness::Claude).map(LoadNotice::for_name);
    print_load_notices([store, name].into_iter().flatten());
}

/// The stderr lines a freshly loaded codex roster warrants: one per stored
/// name the printed commands cannot address, through the same arming thread
/// and once-per-process set as [`notice_profile_load`]. A name the claude
/// roster also holds gets the CLAUDE copy even here: the codex roster may be
/// the first one a command loads, and its account is the one a bare name
/// resolves to.
pub(crate) fn notice_codex_load(names: &[ProfileName]) {
    // Checked here, not only in `print_load_notices`: an unarmed load must not
    // read the claude roster for a line it will never print.
    if !load_notices_armed_here() {
        return;
    }
    let claude = load_app_state()
        .map(|state| state.profiles)
        .unwrap_or_default();
    print_load_notices(names.iter().filter_map(|name| {
        let harness = if claude.contains(name) {
            Harness::Claude
        } else {
            Harness::Codex
        };
        NameNotice::of(name, harness).map(LoadNotice::for_name)
    }));
}

/// Test-only RAII: arm the load notices for the guard's thread for its
/// lifetime, clearing them on drop so a test that loads a torn store cannot
/// leak the arming into a sibling test in the same process.
#[cfg(test)]
pub(crate) struct LoadNoticesArmed(());

#[cfg(test)]
impl LoadNoticesArmed {
    pub(crate) fn new() -> Self {
        enable_load_notices();
        Self(())
    }
}

#[cfg(test)]
impl Drop for LoadNoticesArmed {
    fn drop(&mut self) {
        *load_notices_thread() = None;
    }
}

/// Test-only: put the load notices back to their off default and clear the
/// already-printed set, so an in-process `dispatch()` test that armed cannot
/// leak the arming into a sibling test. Called by the test home sandbox, which
/// already resets the other process-global overrides.
#[cfg(test)]
pub(crate) fn reset_load_notices_for_test() {
    *load_notices_thread() = None;
    LOAD_NOTICED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clear();
}

/// What makes a stored account name unaddressable by the commands clauth
/// prints for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NameHazard {
    /// A leading `-` longer than a lone `-`: clap reads `clauth login -x` as
    /// a flag, while `clauth login -` parses as a value.
    LeadingDash,
    /// A clauth subcommand: a bare `clauth list` runs the command, never the
    /// switch.
    Subcommand,
}

/// A stored account whose name the printed commands cannot address, with the
/// copy each surface shows for it. Its equality is the TUI's and the daemon's
/// once-gate key: one account renamed, added or dropped changes the set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NameNotice {
    name: ProfileName,
    hazard: NameHazard,
    /// The roster that holds the name, which decides the remedy: a claude
    /// account is renamed in place, a codex one has no rename at all.
    harness: Harness,
}

impl NameNotice {
    /// The notice `name` warrants on `harness`'s roster, `None` for an
    /// ordinary name.
    pub(crate) fn of(name: &ProfileName, harness: Harness) -> Option<Self> {
        let hazard = if name.as_str() != "-" && name.starts_with('-') {
            NameHazard::LeadingDash
        } else if crate::cli::is_subcommand_name(name) {
            NameHazard::Subcommand
        } else {
            return None;
        };
        Some(Self {
            name: name.clone(),
            hazard,
            harness,
        })
    }

    /// The remedy a CLI line ends with, `name` already escaped: a claude
    /// account gets the runnable `clauth rename` command, with `--` ahead of a
    /// dash-led name so clap reads it as a value; a codex account has no
    /// rename, so its only fix is deleting it and logging in again under
    /// another name.
    fn cli_remedy(&self, name: &str) -> String {
        match (self.harness, self.hazard) {
            (Harness::Claude, NameHazard::LeadingDash) => {
                format!("rename it with `clauth rename -- {name} <new-name>`")
            }
            (Harness::Claude, NameHazard::Subcommand) => {
                format!("rename it with `clauth rename {name} <new-name>`")
            }
            (Harness::Codex, _) => "delete it and log in again under another name".to_string(),
        }
    }

    /// The remedy the toast's detail line ends with: the toast stands inside
    /// the TUI, so a claude account is pointed at the Setup tab.
    fn toast_remedy(&self) -> &'static str {
        match self.harness {
            Harness::Claude => "rename it on the Setup tab",
            Harness::Codex => "delete it and log in again under another name",
        }
    }

    /// The one stderr line, also the line a TUI or daemon log records. The
    /// name prints escaped where it holds a byte a name cannot
    /// ([`crate::format::escape_unless`]), so a hand-edited roster cannot drive
    /// the terminal or forge a line; a name of name chars is byte-identical.
    pub(crate) fn cli_line(&self) -> String {
        let name = crate::format::escape_unless(&self.name, crate::actions::is_name_char);
        let remedy = self.cli_remedy(&name);
        match self.hazard {
            NameHazard::LeadingDash => format!(
                "clauth: {name}: a name starting with '-' reads as a flag, so the commands \
                 clauth prints for it fail; {remedy}"
            ),
            NameHazard::Subcommand => format!(
                "clauth: {name}: a bare `clauth {name}` runs clauth's own {name} command, never \
                 a switch to this account; switch with `clauth switch {name}`, or {remedy}"
            ),
        }
    }

    /// The TUI toast body: the title line, then the detail line, the name
    /// escaped as in [`Self::cli_line`].
    pub(crate) fn toast_body(&self) -> String {
        let name = crate::format::escape_unless(&self.name, crate::actions::is_name_char);
        let remedy = self.toast_remedy();
        match self.hazard {
            NameHazard::LeadingDash => format!(
                "{name} starts with '-', so the commands clauth prints for it fail\n{remedy}"
            ),
            NameHazard::Subcommand => format!(
                "`clauth {name}` runs clauth's own command, not a switch to {name}\n\
                 switch with `clauth switch {name}`, or {remedy}"
            ),
        }
    }
}

/// Why a profile's committed `credentials.json` could not be read, recorded at
/// load so the notice and the refusal can name it instead of always blaming a
/// parse failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum UnreadableStoreReason {
    /// The file exists but does not parse as credentials JSON.
    Parse,
    /// The file could not be read at all; the OS error's `Display` is carried
    /// so the notice names the actual cause (a directory in its place, an I/O
    /// error).
    Io(String),
}

/// The parenthetical naming why a store cannot be read, shared by the notice
/// and the refusal so the two surfaces cannot drift. Parse failures keep the
/// file name alone; read failures add the OS error's own spelling.
fn unreadable_store_parenthetical(reason: &UnreadableStoreReason) -> String {
    match reason {
        UnreadableStoreReason::Parse => "credentials.json does not parse".to_string(),
        UnreadableStoreReason::Io(io) => format!("credentials.json: {io}"),
    }
}

/// The one-line stderr notice for a loaded profile whose store cannot be read,
/// `None` when the store reads (or is absent).
pub(crate) fn unreadable_store_notice(profile: &Profile) -> Option<String> {
    let reason = profile.unreadable_credentials_store.as_ref()?;
    Some(format!(
        "clauth: {name}: {} ({}), so it shows as logged out until `clauth login {name}`",
        crate::format::STORED_LOGIN_UNREADABLE,
        unreadable_store_parenthetical(reason),
        name = crate::format::escape_unless(&profile.name, crate::actions::is_name_char),
    ))
}

/// The refusal a re-key over an unreadable store raises, `None` when the store
/// reads. The CLI renders it through its usual error prefix.
pub(crate) fn unreadable_store_refusal(profile: &Profile) -> Option<String> {
    let reason = profile.unreadable_credentials_store.as_ref()?;
    Some(format!(
        "{name}: {} ({}); log in again with `clauth login {name}` first, or the re-key would drop it",
        crate::format::STORED_LOGIN_UNREADABLE,
        unreadable_store_parenthetical(reason),
        name = crate::format::escape_unless(&profile.name, crate::actions::is_name_char),
    ))
}

/// What a lock-free on-disk login read proves: the store holds a login, holds
/// none, or the read could not tell (a store that exists but cannot be read or
/// parsed). The drains drop a target only on [`StoredLogin::HoldsNone`] and
/// never link on [`StoredLogin::Unknown`]: a store that exists but cannot be
/// read or parsed proves nothing about absence, so it is never read as "no
/// credential" (the repo precedent: `src/usage/forget.rs` `SnapshotIdentities`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StoredLogin {
    /// A readable fact proves a login: a parsed stored pair, a parsed long-lived
    /// sidecar, or a parsed `config.toml` whose api key / `[env]` token passes
    /// [`crate::claude::has_inference_auth`].
    HoldsLogin,
    /// Every store that exists is readable and holds none of the three facts.
    HoldsNone,
    /// At least one store exists but cannot be read or parsed, and nothing
    /// readable proves a login. The disk proves nothing, so a caller must not
    /// read this as "no login".
    Unknown,
}

/// The long-lived sidecar fact plus its unreadable flag, off ONE read of
/// `session-token.json`: `(holds, unreadable)`. A parsed sidecar classifies the
/// same way [`crate::claude::session_token_status`] does (a refresh-token
/// sidecar is a mis-fill, not a login); an existing-but-unparseable sidecar is
/// `(false, true)` — unknown, not "no sidecar" — because a sidecar that exists
/// but cannot be parsed proves nothing about absence. An absent sidecar is
/// `(false, false)`.
fn stored_sidecar_login(name: &ProfileName) -> (bool, bool) {
    let Ok(path) = profile_subpath(name, "session-token.json") else {
        return (false, false);
    };
    if !path.exists() {
        return (false, false);
    }
    let Ok(creds) = read_json_file::<ClaudeCredentials>(&path) else {
        return (false, true);
    };
    let Some(oauth) = creds.claude_ai_oauth else {
        return (false, false);
    };
    (oauth.refresh_token.is_none(), false)
}

/// [`Profile::holds_login`] for a caller holding only a name, read lock-free
/// from the committed store — the SAME three facts the loaded-state read
/// answers, each off its existing lock-free reader: the stored OAuth pair
/// ([`crate::usage::read_oauth_for_compare`], which also reports an
/// unreadable `credentials.json`), a long-lived sidecar ([`stored_sidecar_login`]),
/// and `config.toml`'s api key + env fed to [`crate::claude::has_inference_auth`]
/// ([`stored_endpoint_identity`] `None` while the file exists is the unreadable
/// flag, the same check [`crate::usage::read_third_party_for_compare`] makes).
/// The holds judgment is [`holds_login_facts`], shared so the loaded read and
/// this read cannot drift.
///
/// Deliberately NOT `load_profile(name)`: that path recovers a legacy `.pending`
/// rotation sidecar, which takes the cross-process state flock and rewrites
/// `credentials.json`, and `maybe_rewrite_config_toml` may rewrite
/// `config.toml` — no caller asking a read-only question should mutate a
/// credential store. The TUI's own drain runs it on the UI thread, so it takes
/// no lock and does bounded single-file reads only.
pub(crate) fn stored_login(name: &ProfileName) -> StoredLogin {
    let (oauth, oauth_unreadable) = crate::usage::read_oauth_for_compare(name);
    let stored_pair = oauth.stored.is_some();
    let (long_lived_sidecar, sidecar_unreadable) = stored_sidecar_login(name);
    let endpoint = stored_endpoint_identity(name);
    let (inference_auth, config_unreadable) = match endpoint {
        Some(e) => {
            let mut profile = Profile::new(name.as_str().to_string(), None, e.api_key);
            profile.env = e.env;
            (crate::claude::has_inference_auth(&profile), false)
        }
        None => (false, profile_config_path(name).is_ok_and(|p| p.exists())),
    };

    if holds_login_facts(stored_pair, long_lived_sidecar, inference_auth) {
        StoredLogin::HoldsLogin
    } else if oauth_unreadable || sidecar_unreadable || config_unreadable {
        StoredLogin::Unknown
    } else {
        StoredLogin::HoldsNone
    }
}

/// The parsed source of a profile: one `config.toml` read, whose parse failure
/// still fails here, and one committed `credentials.json` read, whose unreadable
/// fact is captured as [`ProfileSource::store_unreadable`] rather than failing.
/// NO state flock, NO legacy `.pending` adoption, NO `config.toml` rewrite —
/// those are the legs [`load_profile`] adds on top.
struct ProfileSource {
    config_path: PathBuf,
    raw_config: String,
    config: ProfileConfig,
    credentials: Option<ClaudeCredentials>,
    /// The committed `credentials.json` exists but could not be read, and why.
    /// Kept apart from `credentials` because a `.staged`/`.pending` sidecar can
    /// still supply the pair, which clears the unreadable fact at
    /// [`build_profile`].
    store_unreadable: Option<UnreadableStoreReason>,
}

/// The `config.toml` half of [`read_profile_source`], split out for a caller
/// that must not depend on any other file of the profile.
fn read_profile_config_source(name: &ProfileName) -> Result<(PathBuf, String, ProfileConfig)> {
    let config_path = profile_config_path(name)?;
    let raw_config = match std::fs::read_to_string(&config_path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e).with_context(|| format!("failed to read {name}/config.toml")),
    };
    let config: ProfileConfig = if raw_config.trim().is_empty() {
        ProfileConfig::default()
    } else {
        toml::from_str(&raw_config).map_err(|e| {
            crate::format::parse_error(
                format!("{name}/config.toml"),
                crate::format::toml_parse_position(&raw_config, &e),
            )
        })?
    };
    Ok((config_path, raw_config, config))
}

fn read_profile_source(name: &ProfileName) -> Result<ProfileSource> {
    let (config_path, raw_config, config) = read_profile_config_source(name)?;

    let cred_path = profile_credentials_path(name)?;
    let (credentials, store_unreadable) = if cred_path.exists() {
        match read_json_file(&cred_path) {
            Ok(creds) => (Some(creds), None),
            Err(e) => {
                // `read_json_file`'s read error is `with_context` over the
                // `io::Error`; its parse error is a source-free `anyhow!`, so
                // the downcast alone tells the two apart. `read_to_string`
                // surfaces a cut multi-byte character as `InvalidData`, which
                // is a content tear, not an OS read failure, so it stays
                // Parse.
                let reason = match e.downcast_ref::<std::io::Error>() {
                    Some(io) if io.kind() != std::io::ErrorKind::InvalidData => {
                        UnreadableStoreReason::Io(io.to_string())
                    }
                    _ => UnreadableStoreReason::Parse,
                };
                (None, Some(reason))
            }
        }
    } else {
        (None, None)
    };
    Ok(ProfileSource {
        config_path,
        raw_config,
        config,
        credentials,
        store_unreadable,
    })
}

/// A profile's stored api_key, off its `config.toml` alone: the key lives
/// there, so a torn `credentials.json` beside it must never cost the account
/// its key. No lock and no write, unlike [`load_profile`], since Claude Code's
/// `apiKeyHelper` calls it. `Ok(None)` when the profile has no api_key (a
/// missing or blank `config.toml` included); the caller trims and validates.
pub(crate) fn read_api_key(name: &ProfileName) -> Result<Option<String>> {
    let (_, _, config) = read_profile_config_source(name)?;
    Ok(config.api_key)
}

/// Build the loaded [`Profile`] from parsed [`ProfileSource`], consuming the
/// source. Returns the profile plus the source's rewrite inputs (the config path
/// and raw text), since the caller still owes the [`maybe_rewrite_config_toml`]
/// leg.
fn build_profile(name: &ProfileName, source: ProfileSource) -> (Profile, PathBuf, String) {
    let ProfileSource {
        config_path,
        raw_config,
        config,
        credentials,
        store_unreadable,
    } = source;

    // An unreadable store reads logged out only when no sidecar supplied the
    // pair; an adopted `.pending`/`.staged` pair is a login, so the fact clears.
    let unreadable_credentials_store = if credentials.is_none() {
        store_unreadable
    } else {
        None
    };

    let base_url = effective_base_url(
        config.base_url.clone(),
        credentials.is_some() || unreadable_credentials_store.is_some(),
        config.api_key.as_deref(),
        &config.env,
    );

    let provider = base_url.as_deref().and_then(Provider::from_base_url);
    let console = console_credential(&config.console);
    let typed_endpoint = TypedEndpoint {
        base_url: config.base_url,
        console: config.console,
        derived_base_url: base_url.clone(),
        derived_console: console.clone(),
    };
    let claim = proxy_claim_in(&raw_config);
    let has_unreadable_proxy_record = claim == ProxyClaim::Unreadable;
    let proxy = claim.into_record();

    let profile = Profile {
        name: name.clone(),
        base_url,
        api_key: config.api_key,
        auto_start: config.auto_start,
        usage_probe: config.usage_probe,
        usage_probe_interval_ms: usage_probe_interval(config.usage_probe_interval_ms),
        env: config.env,
        models: config.models,
        effort: config.effort,
        fallback_threshold: finite_pct(config.fallback_threshold),
        // Reset-not-clamp, mirroring the chain-wide line this overrides
        // (`AppState::weekly_switch_threshold_pct`): an out-of-band hand-edit
        // (`0.98` — the fraction-vs-percent typo — or `nan`, `120`) follows
        // the chain default instead of clamping into a plausible-looking
        // near-zero line that silently weekly-blocks the account from about
        // 1% into its week.
        weekly_threshold: config
            .weekly_threshold
            .filter(|v| (MIN_WEEKLY_SWITCH_PCT..=MAX_WEEKLY_SWITCH_PCT).contains(v)),
        last_resort: config.last_resort,
        preferred: config.preferred,
        preferred_days: parse_preferred_days(&config.preferred_days),
        rolling_token: config.rolling_token,
        // Normalize at the LOAD boundary so the on-disk value is never a live
        // trap for a direct reader (the 2026-07-14 weekly-line lesson). `inf`
        // and `nan` are both valid TOML floats, and an infinite ceiling means
        // unlimited unattended spending — so anything non-finite reads as $0,
        // the never-spend default.
        max_auto_spend: config
            .max_auto_spend
            .map(|v| if v.is_finite() { v.max(0.0) } else { 0.0 }),
        check_weekly: config.check_weekly.unwrap_or(true),
        check_scoped: config.check_scoped.unwrap_or(true),
        check_credit_cap: config.check_credit_cap.unwrap_or(true),
        bell_threshold: finite_pct(config.bell_threshold),
        disabled: config.disabled,
        console,
        credentials: slot(credentials),
        has_long_lived_sidecar: crate::claude::has_session_token(name),
        unreadable_credentials_store,
        setup_token_digest: crate::claude::installed_session_token(name)
            .and_then(|token| crate::usage::OAuthIdentity::for_setup_token(&token).setup_token),
        provider,
        typed_endpoint,
        proxy,
        has_unreadable_proxy_record,
    };

    (profile, config_path, raw_config)
}

/// The canonical typed [`ProfileConfig`] for a loaded [`Profile`] — what the
/// save writer persists and what the load's drift comparison compares against.
/// One construction, so the two paths cannot drift. The endpoint keys clauth
/// derives at load go back as typed unless the caller moved them
/// ([`Profile::saved_base_url`]), so a freshly loaded profile is canonical in
/// them and the load never edits one.
fn canonical_profile_config(profile: &Profile) -> ProfileConfig {
    ProfileConfig {
        base_url: profile.saved_base_url(),
        api_key: profile.api_key.clone(),
        auto_start: profile.auto_start,
        usage_probe: profile.usage_probe,
        usage_probe_interval_ms: (profile.usage_probe_interval_ms
            != DEFAULT_USAGE_PROBE_INTERVAL_MS)
            .then_some(profile.usage_probe_interval_ms),
        env: profile.env.clone(),
        models: profile.models.clone(),
        effort: profile.effort.clone(),
        fallback_threshold: profile.fallback_threshold,
        weekly_threshold: profile.weekly_threshold,
        last_resort: profile.last_resort,
        preferred: profile.preferred,
        preferred_days: render_preferred_days(&profile.preferred_days),
        rolling_token: profile.rolling_token,
        max_auto_spend: profile.max_auto_spend,
        // Default-on booleans render as commented examples when on, so the
        // canonical form is `None` (unset) — an explicit `= true` on disk
        // normalizes away on the next rewrite.
        check_weekly: (!profile.check_weekly).then_some(false),
        check_scoped: (!profile.check_scoped).then_some(false),
        check_credit_cap: (!profile.check_credit_cap).then_some(false),
        bell_threshold: profile.bell_threshold,
        disabled: profile.disabled,
        console: profile.saved_console(),
    }
}

/// The normalizing write of a full load: when the raw file's typed
/// [`ProfileConfig`] differs from the canonical form derived from the loaded
/// [`Profile`], edit the file in place to the canonical value of only the keys
/// that differ. The comparison is typed-vs-typed, never raw bytes or a
/// `toml::Value` table, so an unmodelled key, a comment, a blank line, the key
/// order and the `kick_timer` alias never trigger a write. The
/// read-compare-write runs under the state flock and re-reads `config.toml`
/// inside it, writing only if still different, so a peer write landing between
/// the pre-flock read and the flock is never overwritten by a stale edit. The
/// write never fails the load: an edit the writer refuses loads with the
/// normalized in-memory value and writes nothing.
fn maybe_rewrite_config_toml(config_path: &Path, raw_config: &str, profile: &Profile) {
    if on_disk_profile_config(raw_config)
        .is_ok_and(|on_disk| on_disk == canonical_profile_config(profile))
    {
        return;
    }
    let name = profile.name.clone();
    let _ = with_state_lock(|_held| {
        // Re-read under the hold: a peer's write landed between the read and
        // this hold is kept, never rewound.
        let now = match std::fs::read_to_string(config_path) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(_) => return Ok(()),
        };
        if now != raw_config {
            // A peer wrote: normalize its fresh bytes from a profile rebuilt
            // from them, never from the pre-flock copy.
            let Ok(source) = read_profile_source(&name) else {
                return Ok(());
            };
            let (fresh_profile, _, fresh_raw) = build_profile(&name, source);
            if on_disk_profile_config(&fresh_raw)
                .is_ok_and(|on_disk| on_disk == canonical_profile_config(&fresh_profile))
            {
                return Ok(());
            }
            let _ = edit_existing_config_toml(
                config_path,
                &fresh_raw,
                &fresh_profile,
                ConfigEditMode::Load,
                None,
            );
            return Ok(());
        }
        // An edit the writer refuses (a non-plain or unreadable shape) is a
        // no-op for the load: the value is already normalized in memory.
        let _ =
            edit_existing_config_toml(config_path, raw_config, profile, ConfigEditMode::Load, None);
        Ok(())
    });
}

/// Serialize `creds` for the profile store at `cred_path`, re-attaching every
/// non-login top-level block the store already holds. [`ClaudeCredentials`]
/// models the Claude login alone, so a plain re-serialize drops those blocks on
/// every token write, `mcpOAuth` (the per-MCP-server logins) among them.
///
/// Everything already in the file is kept, unlike `claude.rs`'s carry, which
/// takes an allowlist. The two run in opposite directions: the carry IMPORTS
/// from another account's live file, where an unrecognised key is a key nobody
/// decided should cross, while this only rewrites a store over itself, where an
/// unrecognised key belongs to the account whose file it already is. Dropping it
/// here would lose data no other writer holds.
pub(crate) fn serialize_credentials_preserving_extra(
    creds: &ClaudeCredentials,
    cred_path: &Path,
) -> Result<String> {
    let mut value = serde_json::to_value(creds).context("failed to serialize credentials")?;
    let existing = read_json_file::<serde_json::Value>(cred_path).ok();
    preserve_extra_blocks(&mut value, existing.as_ref());
    serde_json::to_string_pretty(&value).context("failed to serialize credentials")
}

/// Re-attach onto `value` every non-login top-level block `existing` holds, the
/// value-level core of [`serialize_credentials_preserving_extra`]. Split out
/// because the macOS Keychain mirror rewrites CC's item over itself on a
/// rotation and owes it the same rule, while its `existing` arrives from a
/// subprocess rather than a path. A no-op when either side is not an object.
pub(crate) fn preserve_extra_blocks(
    value: &mut serde_json::Value,
    existing: Option<&serde_json::Value>,
) {
    let (Some(value_obj), Some(existing_obj)) = (
        value.as_object_mut(),
        existing.and_then(serde_json::Value::as_object),
    ) else {
        return;
    };
    for (key, extra) in existing_obj {
        if key == "claudeAiOauth" {
            continue;
        }
        value_obj.insert(key.clone(), extra.clone());
    }
}

/// The profile's stored claude OAuth access token, read from `credentials.json`
/// directly — never [`load_profile`] (it hands back a live stage's pair, and its
/// legacy `.pending` write-through adopts a pre-fix rotation's pair as if
/// committed). `None` on an absent or
/// unparseable store or a login-less chain. The `/profile` read calls this once
/// at request time, outside the state flock; its caller then re-checks the sent
/// token inside the flock ([`stored_token_matches`]).
pub(crate) fn stored_access_token(name: &ProfileName) -> Option<String> {
    let Ok(path) = profile_credentials_path(name) else {
        return None;
    };
    let Ok(creds) = read_json_file::<ClaudeCredentials>(&path) else {
        return None;
    };
    creds.claude_ai_oauth.map(|o| o.access_token)
}

/// The access token `fetched` answered for is STILL the profile's stored one —
/// the ONE shared guard the `/profile` read's two ride-alongs run under the state
/// flock, because that reading is evidence about the exact token that fetched it,
/// and a re-login or a concurrent rotation changes that token. `false` on an
/// absent/unparseable store or a moved chain.
pub(crate) fn stored_token_matches(name: &ProfileName, fetched_access_token: &str) -> bool {
    stored_access_token(name).as_deref() == Some(fetched_access_token)
}

/// Whether `incoming`'s claude OAuth login (access OR refresh token) differs from
/// what `stored` holds. An absent or unparseable `stored` counts as changed; a
/// rewrite that touches only other keys (an MCP-server token refresh) is
/// unchanged, so a copy-back that only moved those keys must not mark the
/// `/profile` reading stale.
pub(crate) fn login_changed(
    stored: Option<&ClaudeCredentials>,
    incoming: &ClaudeCredentials,
) -> bool {
    let Some(stored) = stored else {
        return true;
    };
    let Some(stored_oauth) = stored.claude_ai_oauth.as_ref() else {
        return true;
    };
    let Some(incoming_oauth) = incoming.claude_ai_oauth.as_ref() else {
        return false;
    };
    stored_oauth.access_token != incoming_oauth.access_token
        || stored_oauth.refresh_token != incoming_oauth.refresh_token
}

/// The #80 backfill: stamp the polled raw rate-limit tier into a stored chain
/// that predates login-time stamping, so a pre-#80 mint picks the key up
/// without a manual re-login (decision 1 of the #80 review).
/// Write-if-missing, and only onto the chain the tier was fetched for: the
/// access token the `/profile` body answered for must still be the stored one,
/// or the reading belongs to a superseded pair — the tier is evidence about
/// the exact token that fetched it, and a re-login or a concurrent rotation
/// changes that token.
///
/// Stands down while a legacy `.pending` rotation sidecar exists: writing the
/// pre-rotation pair would move `credentials.json` past the sidecar and get
/// the minted pair discarded by [`recover_pending_credentials`]. The write is
/// the credentials file alone — a background leg never rewrites `config.toml`
/// (its comments and unmodelled keys are the operator's to keep) — through the
/// preserving serializer, so a top-level block the model does not carry (an
/// MCP-server login) survives, the same write shape [`recover_pending_credentials`]
/// uses for its own write-through.
///
/// `Ok(true)` = stamped now; `Ok(false)` = nothing to write (off-roster, a
/// legacy `.pending` sidecar, no store, no chain, or an already-stamped tier); `Err` =
/// could not read or persist. The caller holds the state flock and has already
/// run the stored-token check (the `/profile` read's two ride-alongs share ONE
/// hold and ONE check).
pub(crate) fn stamp_rate_limit_tier_if_missing_held(
    name: &ProfileName,
    tier: &str,
    _held: &StateLockHeld,
) -> Result<bool> {
    // Fresh record membership, like every other persist leg: the poll's
    // work list can lag a concurrent delete/rename by a tick.
    if !is_configured(name)? {
        return Ok(false);
    }
    if profile_credentials_pending_path(name)?.exists() {
        return Ok(false);
    }
    let cred_path = profile_credentials_path(name)?;
    if !cred_path.exists() {
        return Ok(false);
    }
    let mut creds: ClaudeCredentials = read_json_file(&cred_path)?;
    let Some(oauth) = creds.claude_ai_oauth.as_mut() else {
        return Ok(false);
    };
    if oauth.rate_limit_tier().is_some() {
        return Ok(false);
    }
    oauth.set_rate_limit_tier(tier.to_string());
    let bytes = serialize_credentials_preserving_extra(&creds, &cred_path)?;
    atomic_write_600(&cred_path, bytes).context("failed to write credentials.json")?;
    Ok(true)
}

/// Standalone wrapper for [`stamp_rate_limit_tier_if_missing_held`]: take the
/// state flock and the stored-token check itself. The production `/profile` leg
/// calls the held body inside a combined one-flock ride-along; this is what the
/// tier-stamp tests drive.
#[cfg(test)]
pub(crate) fn stamp_rate_limit_tier_if_missing(
    name: &ProfileName,
    fetched_access_token: &str,
    tier: &str,
) -> Result<bool> {
    with_state_lock(|held| {
        if !stored_token_matches(name, fetched_access_token) {
            return Ok(false);
        }
        stamp_rate_limit_tier_if_missing_held(name, tier, held)
    })
}

/// The whole-profile save: the credential store, then `config.toml`, both from
/// `profile` as given. A test fixture's write: outside tests a login install,
/// adopt or clear saves through [`update_profile`] and a settings edit through
/// [`update_profile_config`], so no caller can persist a profile it read before
/// a peer's edit or delete.
#[cfg(test)]
pub(crate) fn save_profile(profile: &Profile) -> Result<()> {
    save_profile_with_id(profile, None)
}

/// Fresh read-modify-write of one profile's credential store and `config.toml`
/// under the state flock: `None` (nothing read, nothing written) when the
/// roster on disk no longer holds `name`; else `f` edits the profile re-read
/// off disk, which is then saved whole, the credential store before
/// `config.toml`, and comes back with `f`'s value.
/// So a login install, adopt or clear changes only the fields `f` sets, and
/// the caller mirrors those into memory once this returns.
pub(crate) fn update_profile<T>(
    name: &ProfileName,
    f: impl FnOnce(&mut Profile) -> Result<T>,
) -> Result<Option<(Profile, T)>> {
    with_state_lock(|_held| {
        if !is_configured(name)? {
            return Ok(None);
        }
        let mut fresh = load_profile(name)?;
        let out = f(&mut fresh)?;
        save_profile_with_id(&fresh, None)?;
        Ok(Some((fresh, out)))
    })
}

/// [`update_profile`] for a capture that sets the stored login alone, coming
/// back with the login it stored (`None` when the roster on disk no longer
/// holds `name`). A `config.toml` that does not parse is left as it is and the
/// credential store is still written: a hand-edit typo in the outgoing account
/// must not block every switch away from it, while a failed store write still
/// fails the capture.
pub(crate) fn update_profile_login(
    name: &ProfileName,
    credentials: ClaudeCredentials,
    held: &StateLockHeld,
) -> Result<Option<ClaudeCredentials>> {
    if !is_configured(name)? {
        return Ok(None);
    }
    // A parse failure carries no `io::Error` (`read_profile_source`'s split);
    // a read failure does, and fails below as `update_profile` would.
    if let Err(e) = read_profile_config_source(name)
        && e.downcast_ref::<std::io::Error>().is_none()
    {
        write_profile_credentials(name, Some(&credentials), held)?;
        return Ok(Some(credentials));
    }
    Ok(update_profile(name, |fresh| {
        fresh.set_credentials(Some(credentials), held);
        Ok(())
    })?
    .and_then(|(fresh, ())| fresh.credentials.into_inner()))
}

/// The whole-profile save for a create: the new account gets a fresh
/// [`ProfileId`], whatever a leftover directory under its name still holds.
pub(crate) fn save_new_profile(profile: &Profile) -> Result<()> {
    save_profile_with_id(profile, Some(ProfileId::mint()?))
}

fn save_profile_with_id(profile: &Profile, fresh: Option<ProfileId>) -> Result<()> {
    with_state_lock(|held| {
        mkdir_700(&profile_dir(&profile.name)?)?;
        // credentials.json BEFORE config.toml: single-use refresh token must
        // not be lost to a config.toml write failure.
        write_profile_credentials(&profile.name, profile.credentials.as_ref(), held)?;
        match fresh {
            Some(id) => write_config_render(profile, Some(&id), true),
            None => write_profile_config(profile),
        }
    })
}

/// The config leg of [`save_profile`] alone: `config.toml`, never the store.
/// A test fixture's whole-profile write: outside tests the one `config.toml`
/// settings writer is [`update_profile_config`], so no caller can persist a
/// profile it read before a peer's edit or delete.
#[cfg(test)]
pub(crate) fn save_profile_config(profile: &Profile) -> Result<()> {
    with_state_lock(|_held| {
        mkdir_700(&profile_dir(&profile.name)?)?;
        write_profile_config(profile)
    })
}

/// Fresh read-modify-write of one profile's `config.toml` under the state
/// flock: `None` (nothing read, nothing created) when the roster on disk no
/// longer holds `name`; else `f` edits the profile re-read off disk, which is
/// saved unless `f` errs or changed nothing a `config.toml` on disk holds, and
/// comes back with `f`'s value.
/// So a settings edit changes only its own fields, never rewinding a peer's
/// edit to another one or recreating a peer-deleted account's directory; the
/// caller mirrors its fields into memory once this returns.
pub(crate) fn update_profile_config<T>(
    name: &ProfileName,
    f: impl FnOnce(&mut Profile) -> Result<T>,
) -> Result<Option<(Profile, T)>> {
    with_state_lock(|_held| {
        if !is_configured(name)? {
            return Ok(None);
        }
        let mut fresh = load_profile(name)?;
        let before = profile_config_path(name)?
            .exists()
            .then(|| render_config_toml(&fresh));
        let out = f(&mut fresh)?;
        if before.as_deref() != Some(render_config_toml(&fresh).as_str()) {
            #[cfg(test)]
            if FAIL_PROFILE_CONFIG_SAVE.swap(false, std::sync::atomic::Ordering::Relaxed) {
                return Err(anyhow::anyhow!(
                    "failed to write config.toml: Permission denied (os error 13)"
                ));
            }
            mkdir_700(&profile_dir(name)?)?;
            write_profile_config(&fresh)?;
        }
        Ok(Some((fresh, out)))
    })
}

/// A save over a modelled key whose on-disk shape the writer does not edit.
/// In a profile's `config.toml`: an inline table, a dotted or implicit table, a
/// multi-line string, a multi-line array, a value of the wrong type, a table
/// where a value belongs or a value where a table belongs, or the `kick_timer`
/// alias beside `auto_start`. In a [`StateFile`], which edits every readable
/// shape in place: a key its model cannot read. Nothing is written; the file
/// and the key are named and the file is left byte for byte. A distinct type so
/// a caller can `downcast_ref` the refusal instead of matching its text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ConfigEditRefusal {
    file: String,
    key: String,
}

impl ConfigEditRefusal {
    fn new(file: impl Into<String>, key: &str) -> Self {
        Self {
            file: file.into(),
            key: key.to_string(),
        }
    }

    /// The key whose on-disk shape stopped the save, for a caller wording its
    /// own message around it.
    pub(crate) fn key(&self) -> &str {
        &self.key
    }
}

impl std::fmt::Display for ConfigEditRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} has `{}` in a shape clauth cannot read; fix or remove that line, then try again",
            self.file, self.key
        )
    }
}

impl std::error::Error for ConfigEditRefusal {}

/// A save refused because the file (`config.toml` or a [`StateFile`]) does not
/// parse as TOML. Its `Display` is that file's load error, `failed to parse
/// <label> at line L, column C`, byte for byte; the position rides on the type
/// so a caller can name it without matching the text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ConfigParseRefusal {
    message: String,
    position: Option<crate::format::ParsePosition>,
}

impl ConfigParseRefusal {
    fn new(profile: &str, raw: &str) -> Self {
        let position = on_disk_profile_config(raw)
            .err()
            .and_then(|e| crate::format::toml_parse_position(raw, &e));
        Self::at(format!("{profile}/config.toml"), position)
    }

    /// The refusal for the file the load names `label`.
    fn at(label: impl std::fmt::Display, position: Option<crate::format::ParsePosition>) -> Self {
        Self {
            message: crate::format::parse_error(label, position).to_string(),
            position,
        }
    }

    /// The parse position the load named, for a caller wording its own message.
    pub(crate) fn position(&self) -> Option<crate::format::ParsePosition> {
        self.position
    }
}

impl std::fmt::Display for ConfigParseRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ConfigParseRefusal {}

/// The file's own [`ProfileId`] rides every rewrite.
fn write_profile_config(profile: &Profile) -> Result<()> {
    let id = profile_id_at(&profile_config_path(&profile.name)?);
    write_config_render(profile, id.as_ref(), false)
}

/// The `id` key of the `config.toml` at `config_path`, read on its own so a
/// hand-edit breaking another key does not lose it.
fn profile_id_at(config_path: &Path) -> Option<ProfileId> {
    let raw = std::fs::read_to_string(config_path).ok()?;
    let table = raw.parse::<toml::Table>().ok()?;
    Some(ProfileId(table.get("id")?.as_str()?.to_string()))
}

/// `config.toml` from `profile` plus `id`, carrying every unmodelled key the
/// file already holds. The caller holds the state flock. `replace_unreadable`
/// is true only for a create claiming a fresh directory: a leftover config the
/// writer cannot read belongs to no account, so it is replaced instead of
/// refusing, and the create's undo still names it if the roster write then
/// fails.
fn write_config_render(
    profile: &Profile,
    id: Option<&ProfileId>,
    replace_unreadable: bool,
) -> Result<()> {
    let config_path = profile_config_path(&profile.name)?;
    let rendered = with_profile_id(render_config_toml(profile), id);
    let raw = match std::fs::read_to_string(&config_path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound || replace_unreadable => {
            return atomic_write_600(&config_path, rendered).context("failed to write config.toml");
        }
        Err(e) => {
            return Err(e).with_context(|| format!("failed to read {}/config.toml", profile.name));
        }
    };
    edit_existing_config_toml(&config_path, &raw, profile, ConfigEditMode::Save, id)
}

/// Which edit step a config write runs: the save writes every drifted key; the
/// load comments out a tuning value it ignores outright.
#[derive(Clone, Copy)]
enum ConfigEditMode {
    Save,
    Load,
}

/// The read-edit-write core of a config write over a file that already exists:
/// parse, refuse a non-plain or unreadable shape, apply the edit step, and
/// write atomically (a CRLF file stays CRLF). Shared by the save writer and the
/// normalizing load so the two cannot drift.
fn edit_existing_config_toml(
    config_path: &Path,
    raw: &str,
    profile: &Profile,
    mode: ConfigEditMode,
    id: Option<&ProfileId>,
) -> Result<()> {
    let lf = raw.replace("\r\n", "\n");
    let Ok(mut doc) = lf.parse::<toml_edit::DocumentMut>() else {
        // A file that is not TOML refuses with the load's own parse error: the
        // user's bytes are never replaced by the template, and the position
        // rides on a typed error the rollback warning names without matching
        // text.
        return Err(ConfigParseRefusal::new(profile.name.as_str(), raw).into());
    };
    let on_disk = on_disk_profile_config(raw);
    let canonical = canonical_profile_config(profile);
    if let Some(key) = non_plain_edit(&doc, on_disk.as_ref().ok(), &canonical) {
        return Err(ConfigEditRefusal::new(format!("{}: config.toml", profile.name), key).into());
    }
    let on_disk = match on_disk {
        Ok(on_disk) => on_disk,
        // A typed-parse failure no key's shape explains refuses as the load
        // does, so the edit step only ever runs over a file that parsed.
        Err(e) => {
            return Err(crate::format::parse_error(
                format!("{}/config.toml", profile.name),
                crate::format::toml_parse_position(raw, &e),
            ));
        }
    };
    apply_config_edits(&mut doc, &on_disk, &canonical, mode);
    if let Some(id) = id
        && doc.get("id").and_then(|v| v.as_str()) != Some(id.0.as_str())
    {
        doc["id"] = toml_edit::value(id.0.clone());
    }
    let mut out = doc.to_string();
    if raw.contains("\r\n") {
        // toml_edit normalizes line endings to `\n` on parse; a CRLF file stays
        // CRLF through the save, inserted lines included. A multi-line string's
        // own `\r\n` survives the parse, so normalize first, then expand, or the
        // blanket replace writes a bare `\r` the file no longer parses.
        out = out.replace("\r\n", "\n").replace('\n', "\r\n");
    }
    // A load whose edit leaves the bytes unchanged writes nothing: an edit that
    // computes the same bytes as the raw file is a no-op, not a rewrite that
    // bumps the mtime and flips the reload fingerprint.
    if matches!(mode, ConfigEditMode::Load) && out == raw {
        return Ok(());
    }
    atomic_write_600(config_path, out).context("failed to write config.toml")
}

/// `rendered` with `id = "…"` as its first key, under the header comment.
fn with_profile_id(rendered: String, id: Option<&ProfileId>) -> String {
    let Some(id) = id else {
        return rendered;
    };
    let line = format!("\n\nid = {}\n\n", toml::Value::String(id.0.clone()));
    rendered.replacen("\n\n", &line, 1)
}

/// A login write refused because another roster profile's `credentials.json`
/// already holds its refresh token: a single-use chain lives in one store, or
/// the first refresh through either store kills the other.
#[derive(Debug)]
pub(crate) struct ChainHeldElsewhere {
    pub(crate) owner: ProfileName,
}

impl std::fmt::Display for ChainHeldElsewhere {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "this login is already saved in '{}'", self.owner)
    }
}

impl std::error::Error for ChainHeldElsewhere {}

impl ChainHeldElsewhere {
    /// The refusal anywhere in `e`'s chain.
    pub(crate) fn find(e: &anyhow::Error) -> Option<&Self> {
        e.chain().find_map(|cause| cause.downcast_ref::<Self>())
    }
}

/// Refuse writing `credentials` into `store` while another roster profile's
/// `credentials.json` holds its refresh token. A login with no refresh token
/// passes, and so does a store that already holds this chain: rewriting it
/// (a re-capture, a tier stamp) puts it in no store it was not in. A store
/// that does not read or parse holds no chain this can see. The caller holds
/// the state flock, so the roster and the stores read here are the ones the
/// write lands beside.
pub(crate) fn refuse_chain_held_elsewhere(
    store: &Path,
    credentials: &ClaudeCredentials,
    _held: &StateLockHeld,
) -> Result<()> {
    match chain_holder_elsewhere(store, credentials)? {
        Some(owner) => Err(ChainHeldElsewhere { owner }.into()),
        None => Ok(()),
    }
}

/// The roster profile other than `store`'s whose `credentials.json` holds
/// `credentials`' refresh token, on [`refuse_chain_held_elsewhere`]'s rules.
/// Every store of `store`'s own profile (its `credentials.json` and its
/// `session-token.json` sidecar share one dir) is that profile's, never
/// another's. Reads only, so a caller without the state flock gets a
/// best-effort answer a peer's write can move.
pub(crate) fn chain_holder_elsewhere(
    store: &Path,
    credentials: &ClaudeCredentials,
) -> Result<Option<ProfileName>> {
    let Some(refresh) = credentials.refresh_token().filter(|t| !t.is_empty()) else {
        return Ok(None);
    };
    let holds = |path: &Path| {
        std::fs::read(path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<ClaudeCredentials>(&bytes).ok())
            .is_some_and(|stored| stored.refresh_token() == Some(refresh))
    };
    if holds(store) {
        return Ok(None);
    }
    // A caller can hand the store in its resolved spelling (a session tree's
    // link target), so the dirs compare by identity as well as by spelling.
    let own_dir = store.parent();
    let own_identity = own_dir.and_then(|dir| crate::canonical::identity(dir).ok());
    let is_own = |path: &Path| {
        let dir = path.parent();
        dir == own_dir
            || own_identity.is_some()
                && dir.and_then(|d| crate::canonical::identity(d).ok()) == own_identity
    };
    for name in load_app_state()?.profiles {
        let path = profile_credentials_path(&name)?;
        if !is_own(&path) && holds(&path) {
            return Ok(Some(name));
        }
    }
    Ok(None)
}

/// [`refuse_chain_held_elsewhere`] for `name`'s `credentials.json`, asked by a
/// writer before the first write it makes ahead of the store write (a
/// forget, a seed, a detach), so the refusal leaves everything as it was.
pub(crate) fn refuse_login_held_elsewhere(
    name: &ProfileName,
    credentials: Option<&ClaudeCredentials>,
    held: &StateLockHeld,
) -> Result<()> {
    match credentials {
        Some(credentials) => {
            refuse_chain_held_elsewhere(&profile_credentials_path(name)?, credentials, held)
        }
        None => Ok(()),
    }
}

/// The credential leg every store writer shares: `Some` writes the login
/// through the preserving serializer, so `mcpOAuth` and every other top-level
/// block the store holds survive, and restores whatever MCP logins the profile
/// parked; `None` parks those logins and removes the store. A login whose
/// chain another profile's store holds is refused before anything is written
/// ([`refuse_chain_held_elsewhere`]).
pub(crate) fn write_profile_credentials(
    name: &ProfileName,
    credentials: Option<&ClaudeCredentials>,
    held: &StateLockHeld,
) -> Result<()> {
    let cred_path = profile_credentials_path(name)?;
    match credentials {
        Some(creds) => {
            refuse_chain_held_elsewhere(&cred_path, creds, held)?;
            let bytes = serialize_credentials_preserving_extra(creds, &cred_path)?;
            atomic_write_600(&cred_path, bytes).context("failed to write credentials.json")?;
            // This profile has a store again, so whatever it parked when it
            // last lost one goes back where every reader looks for it.
            crate::claude::restore_parked_mcp_logins(name, &cred_path);
        }
        None if cred_path.exists() => {
            // Read the MCP-server logins out before the only file carrying
            // them goes. This is the chokepoint rather than each caller
            // because every path that stops a profile storing a login lands
            // here: a recapture onto a third-party endpoint, and blanking an
            // OAuth login both do.
            crate::claude::park_mcp_logins_from_store(name, &cred_path);
            std::fs::remove_file(&cred_path).context("failed to remove credentials.json")?;
        }
        None => {}
    }
    Ok(())
}

/// Write a legacy `credentials.json.pending` sidecar the way a binary predating
/// `credentials.json.staged` did: the fixture for the legacy load branch, which
/// production no longer writes.
#[cfg(test)]
pub(crate) fn stage_rotated_credentials(
    name: &ProfileName,
    creds: &ClaudeCredentials,
) -> Result<()> {
    with_state_lock(|_held| {
        mkdir_700(&profile_dir(name)?)?;
        atomic_write_600(
            &profile_credentials_pending_path(name)?,
            serde_json::to_string_pretty(creds)?,
        )
        .context("failed to stage rotated credentials")
    })
}

pub(crate) fn clear_staged_credentials(name: &ProfileName) {
    if let Ok(path) = profile_credentials_pending_path(name) {
        let _ = std::fs::remove_file(path);
    }
}

/// The RESOLVE half of [`recover_pending_credentials`]: what a full load would
/// end with, read-only. Returns the resolved credentials and whether the LAND
/// leg is owed (a `.pending` sidecar exists to adopt or discard). A live
/// `credentials.json.staged` resolves in place of the store; an adoptable
/// legacy `credentials.json.pending` resolves as the login; a stale one
/// resolves as the store's raw login and is still owed a removal. No state
/// flock, no write, no removal.
fn resolve_pending_credentials(
    name: &ProfileName,
    loaded: Option<&ClaudeCredentials>,
) -> (Option<ClaudeCredentials>, bool) {
    if let Some(staged) = crate::oauth::live_stage_credentials(name, loaded) {
        return (Some(staged), false);
    }
    let (Ok(pending_path), Ok(cred_path)) = (
        profile_credentials_pending_path(name),
        profile_credentials_path(name),
    ) else {
        return (loaded.cloned(), false);
    };
    if pending_path.symlink_metadata().is_err() {
        return (loaded.cloned(), false); // no sidecar — the common case
    }
    (
        adoptable_pending(&pending_path, &cred_path).or_else(|| loaded.cloned()),
        true,
    )
}

/// The LAND half of [`recover_pending_credentials`]: make the disk match what
/// [`resolve_pending_credentials`] judged. Called only when that resolve said a
/// land is due. A sidecar the rule already discards goes lock-free, as it
/// always did: a stale one must not make every load of this profile wait on the
/// flock. An adoptable one is re-decided inside the flock and the sidecar goes
/// only once the write-through it justified has landed in that same hold: a
/// sidecar deleted beside a write that never happened loses the only live pair.
fn land_pending_credentials(
    name: &ProfileName,
    loaded: Option<ClaudeCredentials>,
) -> Option<ClaudeCredentials> {
    let (Ok(pending_path), Ok(cred_path)) = (
        profile_credentials_pending_path(name),
        profile_credentials_path(name),
    ) else {
        return loaded;
    };
    if adoptable_pending(&pending_path, &cred_path).is_none() {
        let _ = std::fs::remove_file(&pending_path);
        return loaded;
    }
    let landed = with_state_lock(|_held| {
        let adopted = adoptable_pending(&pending_path, &cred_path);
        if let Some(pending) = &adopted {
            // Through the preserving serializer, not the staged bytes: staging
            // holds the rotated login alone, so writing it raw would drop every
            // non-login block the store carries.
            let body = serialize_credentials_preserving_extra(pending, &cred_path)?;
            atomic_write_600(&cred_path, body).context("failed to write credentials.json")?;
        }
        let _ = std::fs::remove_file(&pending_path);
        Ok(adopted)
    });
    match landed {
        Ok(adopted) => adopted.or(loaded),
        // No hold, or a write-through that failed: the sidecar stays for a
        // later load to land, and this load still answers with the pair the
        // rule judges live.
        Err(_) => adoptable_pending(&pending_path, &cred_path).or(loaded),
    }
}

/// What `load_profile` reads as the profile's credentials, given the store's
/// raw `loaded` value: the read-only [`resolve_pending_credentials`] verdict,
/// then the [`land_pending_credentials`] write-through + `.pending` removal
/// whenever a land is due and `rewrite` allows it. A live
/// `credentials.json.staged` (a rotation whose persist never landed,
/// `oauth::live_stage_credentials`) is the chain head and loads into memory
/// only: read lock-free, never written through, never deleted, since a loader
/// deciding outside the rotation guard raced installs and refused the
/// rotation's own persist. A legacy `credentials.json.pending` from an older
/// binary keeps its mtime rule.
///
/// `rewrite == false` is the no-rewrite read (the MCP handshake and the shared
/// swap-target check): the resolve verdict is answered read lock-free and the
/// `.pending` sidecar is left for the next ordinary load to land under the
/// flock, so this path never takes the state lock and never writes
/// `credentials.json`.
fn recover_pending_credentials(
    name: &ProfileName,
    loaded: Option<ClaudeCredentials>,
    rewrite: bool,
) -> Option<ClaudeCredentials> {
    let (resolved, land_due) = resolve_pending_credentials(name, loaded.as_ref());
    if !rewrite {
        return resolved;
    }
    if land_due {
        land_pending_credentials(name, loaded)
    } else {
        resolved
    }
}

/// The legacy sidecar's login when it is at least as new as the store (a
/// commit that failed or a process that died mid-save), else `None` (no login
/// in it, or a clean commit superseded it).
///
/// Clean success → credentials.json strictly newer → discard. Failed or
/// interrupted commit → sidecar newer, tied, or no credentials.json at all →
/// adopt. A tie means staging and committing landed in one mtime tick; of the
/// two ways to be wrong, dropping a rotation that may never have landed is the
/// unrecoverable one. The committed side's WRITE time, not its raw mtime: a
/// per-session swap stamps a store it repoints to without writing it, and
/// reading that stamp as a commit discards a sidecar staged moments earlier.
fn adoptable_pending(pending_path: &Path, cred_path: &Path) -> Option<ClaudeCredentials> {
    let pending_meta = pending_path.symlink_metadata().ok()?;
    let pending: ClaudeCredentials =
        serde_json::from_slice(&std::fs::read(pending_path).ok()?).ok()?;
    pending.claude_ai_oauth.as_ref()?;
    let adopt = match crate::profile_cache::effective_write_time(cred_path) {
        Some(cred_mtime) => pending_meta
            .modified()
            .map(|p| p >= cred_mtime)
            .unwrap_or(true),
        None => true,
    };
    adopt.then_some(pending)
}

pub(crate) fn load_config() -> Result<AppConfig> {
    load_config_with(true)
}

/// [`load_config`] for a caller that answers before it serves and must never
/// wait on the state flock — the `clauth mcp` handshake's `instructions` block.
/// Same reads, but each profile loads through
/// [`read_profile`], so neither a `config.toml` rewrite nor a
/// legacy `.pending` sidecar's write-through blocks the handshake on the flock.
pub(crate) fn load_config_without_rewrite() -> Result<AppConfig> {
    load_config_with(false)
}

fn load_config_with(rewrite: bool) -> Result<AppConfig> {
    mkdir_700(&profiles_root()?)?;
    // Every entry point loads config early, so this is the tree-wide chokepoint
    // that retightens an install created before the owner-only invariant.
    if let Ok(dir) = clauth_dir() {
        enforce_clauth_perms(&dir);
    }
    enforce_claude_dirs_perms();
    let state = load_app_state()?;
    let profiles = state
        .profiles
        .iter()
        .map(|name| {
            if rewrite {
                load_profile(name)
            } else {
                read_profile(name)
            }
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(AppConfig { state, profiles })
}

/// `preferred_days` entries → weekdays, keeping the written order and dropping
/// duplicates. Parsing is chrono's, which takes full names and three-letter
/// forms in any case (`Sat`, `saturday`). An entry that does not parse is
/// DROPPED rather than failing the load: one typo in a day list must not take
/// the profile with it, and the canonical rewrite then drops it from disk,
/// which is the visible signal that it was not understood.
fn parse_preferred_days(raw: &[String]) -> Vec<Weekday> {
    let mut out: Vec<Weekday> = Vec::new();
    for entry in raw {
        if let Ok(day) = entry.trim().parse::<Weekday>()
            && !out.contains(&day)
        {
            out.push(day);
        }
    }
    out
}

/// The canonical on-disk spelling: lowercase three-letter names, so a rewrite
/// of a hand-written `["Saturday", "SUN"]` settles instead of alternating.
pub(crate) fn render_preferred_days(days: &[Weekday]) -> Vec<String> {
    days.iter()
        .map(|d| d.to_string().to_ascii_lowercase())
        .collect()
}

/// Every modelled top-level scalar key, read by the completeness test and
/// pinned against `ProfileConfig`'s serialized root scalar key set, so a scalar
/// added to the model without a doc block and example fails a test.
#[cfg(test)]
const MODELLED_SCALARS: [&str; 18] = [
    "base_url",
    "api_key",
    "auto_start",
    "usage_probe",
    "usage_probe_interval_ms",
    "fallback_threshold",
    "weekly_threshold",
    "last_resort",
    "preferred",
    "preferred_days",
    "rolling_token",
    "max_auto_spend",
    "check_weekly",
    "check_scoped",
    "check_credit_cap",
    "bell_threshold",
    "disabled",
    "effort",
];

/// The template shape of one modelled top-level scalar key: the comment block
/// `render_config_toml` emits above it and the commented example line it emits
/// when the key is unset. The doc text is pushed through `out.push_str("# …")`
/// lines so the in-place writer and the render share one source and the glyph
/// test's exemption keeps covering the config-template prose.
#[rustfmt::skip]
fn push_scalar_doc(out: &mut String, key: &str) {
    match key {
        "base_url" => {
            out.push_str("# Base URL for an API-endpoint profile. Leave commented for an OAuth\n");
            out.push_str("# (Pro / Max / Ultra / Team / Enterprise) profile.\n");
        }
        "api_key" => {
            out.push_str("# API key for the endpoint. Only used when base_url is set.\n");
        }
        "auto_start" => {
            out.push_str("# Auto-start the 5-hour usage window for this profile. clauth fires a\n");
            out.push_str("# 1-token Haiku ping at launch and on every 30s refresh while there's\n");
            out.push_str("# no running window. ~0.001¢ per ping. OAuth profiles only.\n");
            out.push_str("# A plan with no 5-hour limit gets no ping.\n");
            out.push_str("# Old name `kick_timer = true` is still accepted.\n");
        }
        "usage_probe" => {
            out.push_str("# Probe this account's usage with a 1-token Haiku message, for a\n");
            out.push_str("# `claude setup-token` login that /usage cannot read (no OAuth pair\n");
            out.push_str("# stored). Every probe is an inference request and may start the 5h\n");
            out.push_str("# window. A plan with no 5-hour limit has none to start. Inert while an\n");
            out.push_str("# OAuth pair is stored. OAuth profiles only.\n");
        }
        "usage_probe_interval_ms" => {
            out.push_str(&format!(
                "# How often the usage probe runs, in milliseconds: {MIN_USAGE_PROBE_INTERVAL_MS} to\n\
                 # {MAX_USAGE_PROBE_INTERVAL_MS}. A value outside that range is clamped into it.\n"
            ));
        }
        "fallback_threshold" => {
            out.push_str(
                "# 5-hour utilization percentage at/above which clauth will auto-switch\n",
            );
            out.push_str("# off this profile, provided the profile is also a member of the\n");
            out.push_str(
                "# fallback chain configured in ~/.clauth/profiles.toml. Range 0..=100.\n",
            );
        }
        "weekly_threshold" => {
            out.push_str("# Per-account override of the chain-wide weekly (7d) switch line (the\n");
            out.push_str(
                "# Config tab's `weekly limit`, default 98). Governs when auto-switching\n",
            );
            out.push_str("# treats this account's week — aggregate and per-model — as spent.\n");
            out.push_str("# Commented = follow the chain-wide value. Range 0..=100.\n");
        }
        "last_resort" => {
            out.push_str("# Marks this profile as the fallback chain's last resort. Once the\n");
            out.push_str(
                "# auto-switch walk lands here with no other member having headroom, it\n",
            );
            out.push_str("# parks instead of turning off all accounts. Independent of\n");
            out.push_str("# fallback_threshold, this profile still switches away at its own\n");
            out.push_str("# threshold whenever another chain member has headroom.\n");
        }
        "preferred" => {
            out.push_str(
                "# Marks this profile as the fallback chain's preferred (home) account.\n",
            );
            out.push_str(
                "# At most one member is preferred; the toggle is a radio. Once live work\n",
            );
            out.push_str(
                "# has drifted off it and it reads clear and fresh again, the daemon walks\n",
            );
            out.push_str("# the active account — and every following session — back to it. Mutually\n");
            out.push_str("# exclusive with last_resort.\n");
        }
        "preferred_days" => {
            out.push_str(
                "# Weekdays this account is the home account, in local time. Empty (the\n",
            );
            out.push_str(
                "# default) leaves `preferred` above in charge every day. A non-empty list\n",
            );
            out.push_str("# CLAIMS those days against every account — a bare `preferred` elsewhere\n");
            out.push_str("# stands down on them — while `preferred` still decides the days no list\n");
            out.push_str("# claims, here and everywhere. Only chain members that could actually\n");
            out.push_str(
                "# serve claim: a list on a removed, disabled or auth-broken account is\n",
            );
            out.push_str(
                "# inert. Full names and three-letter forms both parse, and an entry that\n",
            );
            out.push_str("# does not is dropped on the next rewrite.\n");
        }
        "rolling_token" => {
            out.push_str("# CLA-ROLL: re-stamp this profile's session-token.json with the usage\n");
            out.push_str("# chain's current access token on every rotation (plan-gated models\n");
            out.push_str("# work in sessions, refresh chain stays clauth-private). Managed by\n");
            out.push_str("# `clauth rolling-token <profile>` / `clauth static-token <profile>`.\n");
        }
        "max_auto_spend" => {
            out.push_str("# Ceiling in US dollars on what the fallback chain may spend of this\n");
            out.push_str(
                "# account's pay-as-you-go budget unattended. Needs `spend_budget_switching`\n",
            );
            out.push_str(
                "# on in profiles.toml AND pay-as-you-go enabled on the account; 0 (the\n",
            );
            out.push_str("# default) never spends. The chain stops using this account once its\n");
            out.push_str("# spend reaches 90% of this or of the account's own cap, whichever is\n");
            out.push_str("# lower — parking on a `last_resort` member if the chain has one, else\n");
            out.push_str("# per `switch_off_when_budget_spent`.\n");
        }
        "check_weekly" => {
            out.push_str("# Whether auto-switching checks this account's aggregate weekly (7d)\n");
            out.push_str("# usage against the weekly limit. Set false to keep this account in\n");
            out.push_str(
                "# rotation across the soft weekly band; the 100% hard cap always blocks.\n",
            );
        }
        "check_scoped" => {
            out.push_str(
                "# Whether auto-switching checks this account's per-model weekly windows\n",
            );
            out.push_str(
                "# (e.g. \"7d fable\") against the weekly limit. Set false to ignore them\n",
            );
            out.push_str("# and keep this account in rotation for use with other models.\n");
        }
        "check_credit_cap" => {
            out.push_str(
                "# Whether auto-switching keeps off this account while its usage-credit cap\n",
            );
            out.push_str(
                "# is spent (Claude Code then refuses credit-billed models such as Fable).\n",
            );
            out.push_str("# Set false to keep it in rotation for other models.\n");
        }
        "bell_threshold" => {
            out.push_str(
                "# 5-hour utilization percentage at/above which clauth alerts: a TUI toast,\n",
            );
            out.push_str("# plus the daemon's bell_command when one is set. Range 0..=100.\n");
        }
        "disabled" => {
            out.push_str(
                "# Disable this account: it becomes invisible to the fallback chain, the\n",
            );
            out.push_str(
                "# usage/rotation scheduler, and the daemon status feed (by default), while\n",
            );
            out.push_str("# its profile directory and credentials stay on disk untouched.\n");
        }
        "effort" => {
            out.push_str("# Per-account reasoning effort: rides env CLAUDE_CODE_EFFORT_LEVEL\n");
            out.push_str("# (`effortLevel` is Claude Code's own key).\n");
        }
        _ => unreachable!("no doc block for modelled scalar {key}"),
    }
}

fn scalar_example(key: &str) -> String {
    match key {
        "base_url" => "# base_url = \"https://api.anthropic.com\"\n".to_string(),
        "api_key" => "# api_key = \"sk-ant-...\"\n".to_string(),
        "auto_start" => "# auto_start = true\n".to_string(),
        "usage_probe" => "# usage_probe = true\n".to_string(),
        "usage_probe_interval_ms" => {
            format!("# usage_probe_interval_ms = {DEFAULT_USAGE_PROBE_INTERVAL_MS}\n")
        }
        "fallback_threshold" => "# fallback_threshold = 95.0\n".to_string(),
        "weekly_threshold" => "# weekly_threshold = 98.0\n".to_string(),
        "last_resort" => "# last_resort = true\n".to_string(),
        "preferred" => "# preferred = true\n".to_string(),
        "preferred_days" => "# preferred_days = [\"sat\", \"sun\"]\n".to_string(),
        "rolling_token" => "# rolling_token = true\n".to_string(),
        "max_auto_spend" => "# max_auto_spend = 5.0\n".to_string(),
        "check_weekly" => "# check_weekly = false\n".to_string(),
        "check_scoped" => "# check_scoped = false\n".to_string(),
        "check_credit_cap" => "# check_credit_cap = false\n".to_string(),
        "bell_threshold" => "# bell_threshold = 95.0\n".to_string(),
        "disabled" => "# disabled = true\n".to_string(),
        "effort" => "# effort = \"high\"\n".to_string(),
        _ => unreachable!("no example for modelled scalar {key}"),
    }
}

fn scalar_doc(key: &str) -> String {
    let mut out = String::new();
    push_scalar_doc(&mut out, key);
    out
}

#[rustfmt::skip]
fn push_console_doc(out: &mut String) {
    out.push_str("# Alibaba Model Studio console session. The ONLY credential that can read\n");
    out.push_str("# Token Plan quota — the api key authenticates inference and is never read\n");
    out.push_str("# by the quota surface. Captured by `clauth login <name>` on an Alibaba\n");
    out.push_str("# profile. It expires 48h after your aliyun browser sign-in, NOT after the\n");
    out.push_str("# login: re-running the login inherits whatever is left of that window,\n");
    out.push_str("# which can be minutes. A full window needs a fresh console sign-in first.\n");
    out.push_str("# site: domestic | international.\n");
}

fn console_doc() -> String {
    let mut out = String::new();
    push_console_doc(&mut out);
    out
}

const CONSOLE_EXAMPLE: &str =
    "# [console]\n# token = \"...\"\n# site = \"international\"\n# region = \"ap-southeast-1\"\n";

#[rustfmt::skip]
fn push_models_doc(out: &mut String) {
    out.push_str("# Per-account Claude Code model configuration, written into this profile's\n");
    out.push_str("# settings.json. `default` is the `model` setting (an alias like `opusplan`\n");
    out.push_str("# or a full id like `claude-opus-4-8[1m]`); `opus`/`sonnet`/`haiku`/`fable`\n");
    out.push_str("# pin what those aliases resolve to (ANTHROPIC_DEFAULT_*_MODEL); `subagent`\n");
    out.push_str("# forces the subagent model (CLAUDE_CODE_SUBAGENT_MODEL); `advisor` is the\n");
    out.push_str("# server-side advisor model (the `advisorModel` setting).\n");
}

fn models_doc() -> String {
    let mut out = String::new();
    push_models_doc(&mut out);
    out
}

const MODELS_EXAMPLE: &str = "# [models]\n# default = \"opusplan\"\n";

#[rustfmt::skip]
fn push_env_doc(out: &mut String) {
    out.push_str("# Extra env vars merged into ~/.claude/settings.json's env block while\n");
    out.push_str("# this profile is active. A switch to another profile removes each one,\n");
    out.push_str("# or puts back the value ~/.claude/settings.json held before it.\n");
}

fn env_doc() -> String {
    let mut out = String::new();
    push_env_doc(&mut out);
    out
}

const ENV_EXAMPLE: &str = "# [env]\n# HTTP_PROXY = \"http://localhost:8080\"\n";

/// Renders config.toml with set values uncommented and unset ones as commented examples.
fn render_config_toml(profile: &Profile) -> String {
    fn toml_str(s: &str) -> String {
        toml::Value::String(s.to_string()).to_string()
    }

    let mut out = String::from("# clauth profile configuration\n\n");

    push_scalar_doc(&mut out, "base_url");
    match profile.saved_base_url().as_deref() {
        Some(v) => out.push_str(&format!("base_url = {}\n", toml_str(v))),
        None => out.push_str(&scalar_example("base_url")),
    }
    out.push('\n');

    push_scalar_doc(&mut out, "api_key");
    match profile.api_key.as_deref() {
        Some(v) => out.push_str(&format!("api_key = {}\n", toml_str(v))),
        None => out.push_str(&scalar_example("api_key")),
    }
    out.push('\n');

    push_scalar_doc(&mut out, "auto_start");
    if profile.auto_start {
        out.push_str("auto_start = true\n");
    } else {
        out.push_str(&scalar_example("auto_start"));
    }
    out.push('\n');

    push_scalar_doc(&mut out, "usage_probe");
    if profile.usage_probe {
        out.push_str("usage_probe = true\n");
    } else {
        out.push_str(&scalar_example("usage_probe"));
    }
    out.push('\n');

    push_scalar_doc(&mut out, "usage_probe_interval_ms");
    if profile.usage_probe_interval_ms == DEFAULT_USAGE_PROBE_INTERVAL_MS {
        out.push_str(&scalar_example("usage_probe_interval_ms"));
    } else {
        out.push_str(&format!(
            "usage_probe_interval_ms = {}\n",
            profile.usage_probe_interval_ms
        ));
    }
    out.push('\n');

    push_scalar_doc(&mut out, "fallback_threshold");
    match profile.fallback_threshold {
        Some(v) => out.push_str(&format!("fallback_threshold = {v}\n")),
        None => out.push_str(&scalar_example("fallback_threshold")),
    }
    out.push('\n');

    push_scalar_doc(&mut out, "weekly_threshold");
    match profile.weekly_threshold {
        Some(v) => out.push_str(&format!("weekly_threshold = {v}\n")),
        None => out.push_str(&scalar_example("weekly_threshold")),
    }
    out.push('\n');

    push_scalar_doc(&mut out, "last_resort");
    if profile.last_resort {
        out.push_str("last_resort = true\n");
    } else {
        out.push_str(&scalar_example("last_resort"));
    }
    out.push('\n');

    push_scalar_doc(&mut out, "preferred");
    if profile.preferred {
        out.push_str("preferred = true\n");
    } else {
        out.push_str(&scalar_example("preferred"));
    }
    out.push('\n');

    push_scalar_doc(&mut out, "preferred_days");
    if profile.preferred_days.is_empty() {
        out.push_str(&scalar_example("preferred_days"));
    } else {
        out.push_str("preferred_days = [");
        for (i, day) in render_preferred_days(&profile.preferred_days)
            .iter()
            .enumerate()
        {
            if i > 0 {
                out.push_str(", ");
            }
            out.push('"');
            out.push_str(day);
            out.push('"');
        }
        out.push_str("]\n");
    }
    out.push('\n');

    push_scalar_doc(&mut out, "rolling_token");
    if profile.rolling_token {
        out.push_str("rolling_token = true\n");
    } else {
        out.push_str(&scalar_example("rolling_token"));
    }
    out.push('\n');

    push_scalar_doc(&mut out, "max_auto_spend");
    match profile.max_auto_spend {
        Some(v) => out.push_str(&format!("max_auto_spend = {v}\n")),
        None => out.push_str(&scalar_example("max_auto_spend")),
    }
    out.push('\n');

    push_scalar_doc(&mut out, "check_weekly");
    if profile.check_weekly {
        out.push_str(&scalar_example("check_weekly"));
    } else {
        out.push_str("check_weekly = false\n");
    }
    out.push('\n');

    push_scalar_doc(&mut out, "check_scoped");
    if profile.check_scoped {
        out.push_str(&scalar_example("check_scoped"));
    } else {
        out.push_str("check_scoped = false\n");
    }
    out.push('\n');

    push_scalar_doc(&mut out, "check_credit_cap");
    if profile.check_credit_cap {
        out.push_str(&scalar_example("check_credit_cap"));
    } else {
        out.push_str("check_credit_cap = false\n");
    }
    out.push('\n');

    push_scalar_doc(&mut out, "bell_threshold");
    match profile.bell_threshold {
        Some(v) => out.push_str(&format!("bell_threshold = {v}\n")),
        None => out.push_str(&scalar_example("bell_threshold")),
    }
    out.push('\n');

    push_scalar_doc(&mut out, "disabled");
    if profile.disabled {
        out.push_str("disabled = true\n");
    } else {
        out.push_str(&scalar_example("disabled"));
    }
    out.push('\n');

    push_scalar_doc(&mut out, "effort");
    match profile.effort.as_deref() {
        Some(v) => out.push_str(&format!("effort = {}\n", toml_str(v))),
        None => out.push_str(&scalar_example("effort")),
    }
    out.push('\n');

    // Tables last: every key after a `[table]` header belongs to that table, so
    // this block and the two below must follow every scalar key above.
    push_console_doc(&mut out);
    match profile.console.as_ref() {
        Some(c) => {
            out.push_str("[console]\n");
            out.push_str(&format!("token = {}\n", toml_str(&c.token)));
            out.push_str(&format!("site = {}\n", toml_str(c.site.as_str())));
            out.push_str(&format!("region = {}\n", toml_str(&c.region)));
        }
        None => out.push_str(CONSOLE_EXAMPLE),
    }
    out.push('\n');

    push_models_doc(&mut out);
    let m = &profile.models;
    let scalars = [
        ("default", &m.default),
        ("opus", &m.opus),
        ("sonnet", &m.sonnet),
        ("haiku", &m.haiku),
        ("fable", &m.fable),
        ("subagent", &m.subagent),
        ("advisor", &m.advisor),
    ];
    if scalars.iter().all(|(_, v)| v.is_none()) {
        out.push_str(MODELS_EXAMPLE);
    } else {
        out.push_str("[models]\n");
        for (k, v) in scalars {
            if let Some(v) = v {
                out.push_str(&format!("{k} = {}\n", toml_str(v)));
            }
        }
    }
    out.push('\n');

    push_env_doc(&mut out);
    if profile.env.is_empty() {
        out.push_str(ENV_EXAMPLE);
    } else {
        out.push_str("[env]\n");
        for (k, v) in &profile.env {
            // The key encoder, never `toml_str`: a value encoder emits a
            // multi-line string for a newline or for both quote kinds, which
            // TOML refuses as a key.
            out.push_str(&format!(
                "{} = {}\n",
                toml_edit::Key::new(k.as_str()),
                toml_str(v)
            ));
        }
    }

    out
}

/// The site where a template example (or doc block) sits in a parsed document.
enum ExampleSite {
    /// The document's trailing comments, after the last table.
    Trailing,
    /// The leading comment of a root key (a scalar, or a table's header key).
    KeyPrefix(String),
    /// The leading comment above a root `[table]` header.
    TablePrefix(String),
}

/// The byte offset of `example` where it sits directly under one of the key's
/// doc blocks, the block starting a line of its own: only the example under a
/// doc block clauth wrote for that key is the key's, so a lookalike line, or
/// one under a doc block whose first line was edited, never matches.
fn find_example_line(hay: &str, example: &str, doc_blocks: &[String]) -> Option<usize> {
    hay.match_indices(example)
        .map(|(i, _)| i)
        .find(|&i| ends_with_any_doc_block(&hay[..i], doc_blocks))
}

/// Whether `text` ends with `doc_block` at the start of a line: the doc block
/// must start the text or sit right after a `\n`, so a comment whose first
/// line was prefixed (a longer line ending in the block's text) never matches.
fn ends_with_doc_block(text: &str, doc_block: &str) -> bool {
    text.strip_suffix(doc_block)
        .is_some_and(|above| above.is_empty() || above.ends_with('\n'))
}

/// Whether `text` ends with any doc block clauth wrote for a key, each matched
/// at the start of a line.
fn ends_with_any_doc_block(text: &str, doc_blocks: &[String]) -> bool {
    doc_blocks
        .iter()
        .any(|doc_block| ends_with_doc_block(text, doc_block))
}

/// The em dash older `config.toml` doc text used. Spelled as a char literal so
/// the CLI's ASCII-only guard (which skips `'\u{...}'`) reads this source as
/// ASCII while the generated comment keeps the typographic dash.
const EM_DASH: char = '\u{2014}';

/// The cent sign the `auto_start` doc uses, spelled like [`EM_DASH`] so the
/// older doc block in `known_doc_blocks` stays ASCII too.
const CENT: char = '\u{a2}';

/// Every `config.toml` doc block clauth has written for `key`, current first.
/// A file last saved before a doc rewording carries the older block; it is
/// still the key's own, so its example toggles in place. Set and unset both
/// match against this list.
fn known_doc_blocks(key: &str) -> Vec<String> {
    let mut docs = vec![doc_block_for(key)];
    match key {
        // The v0.18.0 form, before the Ultra tier.
        "base_url" => docs.push(
            "# Base URL for an API-endpoint profile. Leave commented for an OAuth\n\
             # (Pro / Max / Team / Enterprise) profile.\n"
                .to_string(),
        ),
        // The v0.18.0 form, before the no-5-hour-limit line.
        "auto_start" => docs.push(format!(
            "# Auto-start the 5-hour usage window for this profile. clauth fires a\n\
             # 1-token Haiku ping at launch and on every 30s refresh while there's\n\
             # no running window. ~0.001{CENT} per ping. OAuth profiles only.\n\
             # Old name `kick_timer = true` is still accepted.\n"
        )),
        // ce69b154 reworded the alert copy (90205d2d wrote the older block).
        "bell_threshold" => docs.push(
            "# 5-hour utilization percentage at/above which clauth fires a bell\n\
             # notification in the overview tab. Range 0..=100.\n"
                .to_string(),
        ),
        // d5ea42d5 added the fable tier to the alias list (5ab01076 wrote the
        // three-model form).
        "models" => {
            docs.push(
                "# Per-account Claude Code model configuration, written into this profile's\n\
                 # settings.json. `default` is the `model` setting (an alias like `opusplan`\n\
                 # or a full id like `claude-opus-4-8[1m]`); `opus`/`sonnet`/`haiku` pin what\n\
                 # those aliases resolve to (ANTHROPIC_DEFAULT_*_MODEL); `subagent` forces the\n\
                 # subagent model (CLAUDE_CODE_SUBAGENT_MODEL).\n"
                    .to_string(),
            );
            // The v0.18.1 form, before the advisor row.
            docs.push(
                "# Per-account Claude Code model configuration, written into this profile's\n\
                 # settings.json. `default` is the `model` setting (an alias like `opusplan`\n\
                 # or a full id like `claude-opus-4-8[1m]`); `opus`/`sonnet`/`haiku`/`fable`\n\
                 # pin what those aliases resolve to (ANTHROPIC_DEFAULT_*_MODEL); `subagent`\n\
                 # forces the subagent model (CLAUDE_CODE_SUBAGENT_MODEL).\n"
                    .to_string(),
            );
        }
        // 027afde6 renamed the wrap-off flag; 3076ec9b made the spend line a
        // stop-using cap; 5f840f3c wrote the original stop-at-90% text.
        "max_auto_spend" => {
            docs.push(format!(
                "# Ceiling in US dollars on what the fallback chain may spend of this\n\
                 # account's pay-as-you-go budget unattended. Needs `spend_budget_switching`\n\
                 # on in profiles.toml AND pay-as-you-go enabled on the account; 0 (the\n\
                 # default) never spends. The chain stops using this account once its\n\
                 # spend reaches 90% of this or of the account's own cap, whichever is\n\
                 # lower {EM_DASH} parking on a `last_resort` member if the chain has one, else\n\
                 # per `budget_wrap_off`.\n"
            ));
            docs.push(
                "# Ceiling in US dollars on what the fallback chain may spend of this\n\
                 # account's pay-as-you-go budget unattended. Needs `spend_budget_switching`\n\
                 # on in profiles.toml AND pay-as-you-go enabled on the account; 0 (the\n\
                 # default) never spends. The chain stops at 90% of this or the account's\n\
                 # own cap, whichever is lower.\n"
                    .to_string(),
            );
        }
        // 300cc6bf turned the em-dash after fallback_threshold into a comma
        // (d6b5bfa2 wrote the em-dash form).
        "last_resort" => docs.push(format!(
            "# Marks this profile as the fallback chain's last resort. Once the\n\
             # auto-switch walk lands here with no other member having headroom, it\n\
             # parks instead of turning off all accounts. Independent of\n\
             # fallback_threshold {EM_DASH} this profile still switches away at its own\n\
             # threshold whenever another chain member has headroom.\n"
        )),
        // 09a1dd3c reworded the switch-away clause (f4c9ca1e wrote the older
        // block).
        "env" => docs.push(
            "# Extra env vars merged into ~/.claude/settings.json's env block while\n\
             # this profile is active. Cleared on switch to another profile.\n"
                .to_string(),
        ),
        _ => {}
    }
    docs
}

fn doc_block_for(key: &str) -> String {
    match key {
        "console" => console_doc(),
        "models" => models_doc(),
        "env" => env_doc(),
        k => scalar_doc(k),
    }
}

/// The single search the in-place writer uses: root items in file order first,
/// the document trailing last, matching only the example line that sits
/// directly under a doc block clauth wrote for the key. Any other copy is the
/// user's prose, never a site.
fn locate_example(
    doc: &toml_edit::DocumentMut,
    example: &str,
    doc_blocks: &[String],
) -> Option<(ExampleSite, String, String)> {
    let keys: Vec<String> = doc.iter().map(|(k, _)| k.to_string()).collect();
    for k in &keys {
        let (key_prefix, table_prefix) = {
            let Some((key_handle, item)) = doc.get_key_value(k) else {
                continue;
            };
            let kp = key_handle
                .leaf_decor()
                .prefix()
                .and_then(|r| r.as_str())
                .map(str::to_string);
            let tp = item
                .as_table()
                .and_then(|t| t.decor().prefix())
                .and_then(|r| r.as_str())
                .map(str::to_string);
            (kp, tp)
        };
        if let Some(p) = &key_prefix
            && let Some(i) = find_example_line(p, example, doc_blocks)
        {
            return Some((
                ExampleSite::KeyPrefix(k.clone()),
                p[..i].to_string(),
                p[i + example.len()..].to_string(),
            ));
        }
        if let Some(p) = &table_prefix
            && let Some(i) = find_example_line(p, example, doc_blocks)
        {
            return Some((
                ExampleSite::TablePrefix(k.clone()),
                p[..i].to_string(),
                p[i + example.len()..].to_string(),
            ));
        }
    }
    let t = doc.trailing().as_str()?;
    let i = find_example_line(t, example, doc_blocks)?;
    Some((
        ExampleSite::Trailing,
        t[..i].to_string(),
        t[i + example.len()..].to_string(),
    ))
}

fn find_example(doc: &toml_edit::DocumentMut, example: &str, doc_blocks: &[String]) -> bool {
    locate_example(doc, example, doc_blocks).is_some()
}

fn set_value_preserving_decor(
    table: &mut dyn toml_edit::TableLike,
    key: &str,
    value: toml_edit::Value,
) {
    let Some(item) = table.get_mut(key) else {
        return;
    };
    let Some(old) = item.as_value() else { return };
    let decor = old.decor().clone();
    let mut v = value;
    *v.decor_mut() = decor;
    *item = toml_edit::Item::Value(v);
}

fn insert_root_value(
    doc: &mut toml_edit::DocumentMut,
    key: &str,
    value: toml_edit::Value,
    leaf_prefix: Option<String>,
) {
    let mut k = toml_edit::Key::new(key);
    if let Some(p) = leaf_prefix {
        k.leaf_decor_mut().set_prefix(p);
    }
    doc.as_table_mut()
        .insert_formatted(&k, toml_edit::Item::Value(value));
}

fn insert_root_value_before(
    doc: &mut toml_edit::DocumentMut,
    before_key: &str,
    key: &str,
    value: toml_edit::Value,
    leaf_prefix: Option<String>,
) {
    let table = doc.as_table_mut();
    let keys: Vec<String> = table.iter().map(|(k, _)| k.to_string()).collect();
    let Some(pos) = keys.iter().position(|k| k == before_key) else {
        let mut k = toml_edit::Key::new(key);
        if let Some(p) = leaf_prefix {
            k.leaf_decor_mut().set_prefix(p);
        }
        table.insert_formatted(&k, toml_edit::Item::Value(value));
        return;
    };
    let mut tail: Vec<(toml_edit::Key, toml_edit::Item)> = Vec::new();
    for k in &keys[pos..] {
        if let Some(e) = table.remove_entry(k) {
            tail.push(e);
        }
    }
    let mut k = toml_edit::Key::new(key);
    if let Some(p) = leaf_prefix {
        k.leaf_decor_mut().set_prefix(p);
    }
    table.insert_formatted(&k, toml_edit::Item::Value(value));
    for (k, item) in tail {
        table.insert_formatted(&k, item);
    }
}

/// An appended root table goes after the document's trailing comments, never
/// before them: the trailing text moves onto the table's header prefix so a
/// root-level trailing comment stays above the new header instead of landing
/// inside the table's body.
pub(crate) fn append_root_table(
    doc: &mut toml_edit::DocumentMut,
    key: &str,
    build: impl FnOnce(&mut dyn toml_edit::TableLike),
) {
    let tail = take_trailing_lines(doc);
    insert_root_table(doc, key, build, (!tail.is_empty()).then_some(tail), None);
}

/// An appended root scalar goes at the end of its section, never above the
/// document's trailing comments: with no header in the document, the trailing
/// text moves onto the key's prefix so a root-level trailing comment stays
/// above the new key; with a header, the root section ends before the first
/// one and the trailing text belongs to the last table, so nothing moves.
fn append_root_value(doc: &mut toml_edit::DocumentMut, key: &str, value: toml_edit::Value) {
    let tail = if doc.iter().any(|(_, item)| opens_a_header(item)) {
        String::new()
    } else {
        take_trailing_lines(doc)
    };
    insert_root_value(doc, key, value, (!tail.is_empty()).then_some(tail));
}

/// Whether a root item opens a `[table]` or `[[array]]` header line; a table
/// written as dotted keys (`env.A = "b"`) sits among the root keys and opens
/// none.
fn opens_a_header(item: &toml_edit::Item) -> bool {
    item.is_array_of_tables() || item.as_table().is_some_and(|t| !t.is_dotted())
}

/// The document's trailing text, taken off the document as whole lines: a last
/// comment with no final newline gets one, so an item placed after it never
/// lands inside that comment.
fn take_trailing_lines(doc: &mut toml_edit::DocumentMut) -> String {
    let mut tail = doc.trailing().as_str().unwrap_or("").to_string();
    doc.set_trailing("");
    if !tail.is_empty() && !tail.ends_with('\n') {
        tail.push('\n');
    }
    tail
}

pub(crate) fn insert_root_table(
    doc: &mut toml_edit::DocumentMut,
    key: &str,
    build: impl FnOnce(&mut dyn toml_edit::TableLike),
    header_prefix: Option<String>,
    position: Option<isize>,
) {
    let mut t = toml_edit::Table::new();
    build(&mut t);
    if let Some(p) = header_prefix {
        t.decor_mut().set_prefix(p);
    }
    t.set_position(position);
    doc.insert(key, toml_edit::Item::Table(t));
}

fn replace_example_with_value(
    doc: &mut toml_edit::DocumentMut,
    key: &str,
    value: toml_edit::Value,
    example: &str,
) {
    // A root key opens before the first table header: an example after any
    // header (a later table's prefix, or the trailing text after a table)
    // cannot become a key where it sits.
    let first_header = doc
        .iter()
        .find(|(_, item)| opens_a_header(item))
        .map(|(k, _)| k.to_string());
    let site =
        locate_example(doc, example, &known_doc_blocks(key)).filter(|(site, _, _)| match site {
            ExampleSite::KeyPrefix(_) => true,
            ExampleSite::TablePrefix(k) => first_header.as_deref() == Some(k.as_str()),
            ExampleSite::Trailing => first_header.is_none(),
        });
    let Some((site, before, after)) = site else {
        append_root_value(doc, key, value);
        return;
    };
    match site {
        ExampleSite::Trailing => {
            doc.set_trailing(after);
            insert_root_value(doc, key, value, Some(before));
        }
        ExampleSite::KeyPrefix(k) => {
            if let Some(mut km) = doc.as_table_mut().key_mut(&k) {
                km.leaf_decor_mut().set_prefix(after);
            }
            insert_root_value_before(doc, &k, key, value, Some(before));
        }
        ExampleSite::TablePrefix(k) => {
            if let Some(tm) = doc.get_mut(&k).and_then(|it| it.as_table_mut()) {
                tm.decor_mut().set_prefix(after);
            }
            insert_root_value(doc, key, value, Some(before));
        }
    }
}

fn replace_example_with_table(
    doc: &mut toml_edit::DocumentMut,
    key: &str,
    example: &str,
    build: impl FnOnce(&mut dyn toml_edit::TableLike),
) {
    let Some((site, before, after)) = locate_example(doc, example, &known_doc_blocks(key)) else {
        append_root_table(doc, key, build);
        return;
    };
    match site {
        ExampleSite::Trailing => {
            doc.set_trailing(after);
            insert_root_table(doc, key, build, Some(before), None);
        }
        ExampleSite::KeyPrefix(_) => {
            // The example block sits in a root scalar's prefix: a table cannot
            // open above a root key, so the block stays as written and the
            // table appends after the last table.
            append_root_table(doc, key, build);
        }
        ExampleSite::TablePrefix(k) => {
            let position = doc
                .get(&k)
                .and_then(toml_edit::Item::as_table)
                .and_then(toml_edit::Table::position);
            if let Some(tm) = doc.get_mut(&k).and_then(|it| it.as_table_mut()) {
                tm.decor_mut().set_prefix(after);
            }
            insert_root_table(doc, key, build, Some(before), position.map(|p| p - 1));
        }
    }
}

/// The comment on the same line as `item`'s value or header, trimmed and kept
/// as its own comment line; empty when there is none. A comment is a comment
/// whether it trails a value or a table header, so an unset keeps it.
fn same_line_comment(item: &toml_edit::Item) -> String {
    let suffix = match item {
        toml_edit::Item::Value(v) => v.decor().suffix(),
        toml_edit::Item::Table(t) => t.decor().suffix(),
        toml_edit::Item::ArrayOfTables(_) | toml_edit::Item::None => None,
    };
    suffix
        .and_then(|r| r.as_str())
        .map(str::trim)
        .filter(|s| s.starts_with('#'))
        .map(|s| format!("{s}\n"))
        .unwrap_or_default()
}

/// The comment lines inside a removed value, each trimmed onto its own line:
/// the text before the value, every list element's prefix and suffix, the
/// list's trailing text, and an inline table's trailing text. Empty when the
/// value holds no comment, so a removal without one stays byte-identical.
fn interior_comments(item: &toml_edit::Item) -> String {
    let mut parts: Vec<&str> = Vec::new();
    let Some(value) = item.as_value() else {
        return String::new();
    };
    parts.push(decor_prefix(value.decor()));
    if let toml_edit::Value::Array(a) = value {
        for v in a.iter() {
            parts.push(decor_prefix(v.decor()));
            parts.push(decor_suffix(v.decor()));
            if let toml_edit::Value::InlineTable(t) = v {
                parts.push(t.trailing().as_str().unwrap_or(""));
            }
        }
        parts.push(a.trailing().as_str().unwrap_or(""));
    } else if let toml_edit::Value::InlineTable(t) = value {
        parts.push(t.trailing().as_str().unwrap_or(""));
    }
    parts
        .iter()
        .flat_map(|p| p.lines())
        .map(str::trim)
        .filter(|l| l.starts_with('#'))
        .map(|l| format!("{l}\n"))
        .collect()
}

fn remove_key_keeping_comments(
    doc: &mut toml_edit::DocumentMut,
    key: &str,
    doc_blocks: &[String],
    example: &str,
) {
    let (lead, same_line, interior) = {
        let table = doc.as_table();
        let Some((k, item)) = table.get_key_value(key) else {
            return;
        };
        let lead = if let Some(t) = item.as_table() {
            let lead = t
                .decor()
                .prefix()
                .and_then(|r| r.as_str())
                .unwrap_or("")
                .to_string();
            // A table appended by the writer carries toml_edit's default `\n`
            // separator as its prefix; dropping the table must drop that blank
            // line too, or the set-then-unset cycle leaves it behind.
            if lead.trim().is_empty() {
                String::new()
            } else {
                lead
            }
        } else {
            k.leaf_decor()
                .prefix()
                .and_then(|r| r.as_str())
                .unwrap_or("")
                .to_string()
        };
        (lead, same_line_comment(item), interior_comments(item))
    };
    let next = {
        let keys: Vec<String> = doc.iter().map(|(k, _)| k.to_string()).collect();
        keys.iter()
            .position(|k| k == key)
            .and_then(|i| keys.get(i + 1))
            .cloned()
    };
    doc.as_table_mut().remove(key);
    let mut kept = lead;
    if ends_with_any_doc_block(&kept, doc_blocks) {
        kept.push_str(example);
    }
    kept.push_str(&interior);
    kept.push_str(&same_line);
    if let Some(next_key) = next {
        prepend_prefix(doc, &next_key, &kept);
    } else {
        let t = doc.trailing().as_str().unwrap_or("").to_string();
        doc.set_trailing(format!("{kept}{t}"));
    }
}

pub(crate) fn prepend_prefix(doc: &mut toml_edit::DocumentMut, key: &str, prefix: &str) {
    if let Some(t) = doc
        .get_mut(key)
        .and_then(toml_edit::Item::as_table_mut)
        .filter(|t| t.is_dotted())
    {
        prepend_to_first_leaf(t, prefix);
        return;
    }
    if let Some(t) = doc
        .get_mut(key)
        .and_then(toml_edit::Item::as_table_mut)
        .filter(|t| t.is_implicit())
    {
        prepend_to_first_header(t, prefix);
        return;
    }
    let header = match doc.get_mut(key) {
        Some(toml_edit::Item::Table(t)) => Some(t),
        Some(toml_edit::Item::ArrayOfTables(a)) => a.get_mut(0),
        _ => None,
    };
    if let Some(tm) = header {
        let existing = tm
            .decor()
            .prefix()
            .and_then(|r| r.as_str())
            .unwrap_or("")
            .to_string();
        tm.decor_mut().set_prefix(format!("{prefix}{existing}"));
    } else if let Some(mut km) = doc.as_table_mut().key_mut(key) {
        let existing = km
            .leaf_decor()
            .prefix()
            .and_then(|r| r.as_str())
            .unwrap_or("")
            .to_string();
        km.leaf_decor_mut()
            .set_prefix(format!("{prefix}{existing}"));
    }
}

/// The document position of the first header `item` renders: its own for a
/// header table, its first element's for an array of tables, and its earliest
/// child's for an implicit table, which renders no header of its own.
pub(crate) fn first_header_position(item: &toml_edit::Item) -> Option<isize> {
    match item {
        toml_edit::Item::Table(t) if t.is_implicit() => {
            t.iter().filter_map(|(_, c)| first_header_position(c)).min()
        }
        toml_edit::Item::Table(t) => t.position(),
        toml_edit::Item::ArrayOfTables(a) => a.get(0).and_then(toml_edit::Table::position),
        _ => None,
    }
}

/// An implicit table (`foo` of `[foo.bar]`) renders no decor of its own, so a
/// comment moved onto it goes on the first header rendered beneath it.
fn prepend_to_first_header(t: &mut toml_edit::Table, prefix: &str) {
    let Some(first) = t
        .iter()
        .filter_map(|(k, c)| first_header_position(c).map(|p| (p, k.to_string())))
        .min()
        .map(|(_, k)| k)
    else {
        return;
    };
    let target = match t.get_mut(&first) {
        Some(toml_edit::Item::Table(c)) if c.is_implicit() => {
            prepend_to_first_header(c, prefix);
            return;
        }
        Some(toml_edit::Item::Table(c)) => c,
        Some(toml_edit::Item::ArrayOfTables(a)) => match a.get_mut(0) {
            Some(c) => c,
            None => return,
        },
        _ => return,
    };
    let existing = target
        .decor()
        .prefix()
        .and_then(|r| r.as_str())
        .unwrap_or("")
        .to_string();
    target.decor_mut().set_prefix(format!("{prefix}{existing}"));
}

/// A dotted line (`env.A = "b"`) renders the prefix of its first leaf key,
/// never its table's own decor, so a comment moved onto it goes there.
fn prepend_to_first_leaf(t: &mut toml_edit::Table, prefix: &str) {
    let Some(first) = t.iter().next().map(|(k, _)| k.to_string()) else {
        return;
    };
    if let Some(inner) = t
        .get_mut(&first)
        .and_then(toml_edit::Item::as_table_mut)
        .filter(|inner| inner.is_dotted())
    {
        prepend_to_first_leaf(inner, prefix);
        return;
    }
    if let Some(mut km) = t.key_mut(&first) {
        let existing = km
            .leaf_decor()
            .prefix()
            .and_then(|r| r.as_str())
            .unwrap_or("")
            .to_string();
        km.leaf_decor_mut()
            .set_prefix(format!("{prefix}{existing}"));
    }
}

fn remove_subkey_keeping_comments(doc: &mut toml_edit::DocumentMut, table_key: &str, subkey: &str) {
    let (lead, inline_close) = {
        let Some(t) = doc.get(table_key).and_then(|i| i.as_table_like()) else {
            return;
        };
        let Some((k, item)) = t.get_key_value(subkey) else {
            return;
        };
        let inline = doc
            .get(table_key)
            .is_some_and(toml_edit::Item::is_inline_table);
        let comment_bearing = k
            .leaf_decor()
            .prefix()
            .and_then(|r| r.as_str())
            .is_some_and(|s| s.contains('#'))
            || item
                .as_value()
                .and_then(|v| v.decor().suffix())
                .and_then(|r| r.as_str())
                .is_some_and(|s| s.contains('#'));
        if inline && !comment_bearing {
            // An inline table key keeps its own separator, so a removed key's
            // whitespace prefix has nowhere to go. The whitespace that closed
            // the table dies with its last key, so that one moves to the
            // table's trailing instead of another key. A key whose own prefix
            // or value suffix holds a comment is not this case: it takes the
            // carry path below, which moves that comment onto the next key, or
            // past the table from the last one.
            let last = t.iter().last().is_some_and(|(last, _)| last == subkey);
            let close = if last {
                item.as_value()
                    .and_then(|v| v.decor().suffix())
                    .and_then(|r| r.as_str())
                    .unwrap_or("")
                    .to_string()
            } else {
                String::new()
            };
            (String::new(), Some(close))
        } else {
            let mut lead = k
                .leaf_decor()
                .prefix()
                .and_then(|r| r.as_str())
                .unwrap_or("")
                .to_string();
            lead.push_str(&same_line_comment(item));
            (lead, None)
        }
    };
    // The next subkey is the block-table carry target only; the inline path
    // returns through its own close handling. Computed before the removal so
    // the removed key still holds its place in the table.
    let next_sub = if inline_close.is_none() {
        let Some(t) = doc.get(table_key).and_then(|i| i.as_table_like()) else {
            return;
        };
        let keys: Vec<String> = t.iter().map(|(k, _)| k.to_string()).collect();
        keys.iter()
            .position(|k| k == subkey)
            .and_then(|i| keys.get(i + 1))
            .cloned()
    } else {
        None
    };
    {
        let Some(t) = doc.get_mut(table_key).and_then(|i| i.as_table_like_mut()) else {
            return;
        };
        t.remove(subkey);
    }
    if let Some(close) = inline_close {
        if !close.is_empty()
            && let Some(t) = doc
                .get_mut(table_key)
                .and_then(toml_edit::Item::as_inline_table_mut)
            && !t.trailing_comma()
        {
            let existing = t.trailing().as_str().unwrap_or("").to_string();
            t.set_trailing(format!("{close}{existing}"));
        }
        return;
    }
    if let Some(ns) = next_sub {
        let Some(t) = doc.get_mut(table_key).and_then(|i| i.as_table_like_mut()) else {
            return;
        };
        if let Some(inner) = t
            .get_mut(&ns)
            .and_then(toml_edit::Item::as_table_mut)
            .filter(|inner| inner.is_dotted())
        {
            prepend_to_first_leaf(inner, &lead);
        } else if let Some(mut km) = t.key_mut(&ns) {
            let existing = km
                .leaf_decor()
                .prefix()
                .and_then(|r| r.as_str())
                .unwrap_or("")
                .to_string();
            km.leaf_decor_mut().set_prefix(format!("{lead}{existing}"));
        }
    } else {
        let root_keys: Vec<String> = doc.iter().map(|(k, _)| k.to_string()).collect();
        if let Some(pos) = root_keys.iter().position(|k| k == table_key)
            && let Some(next_root) = root_keys.get(pos + 1)
        {
            prepend_prefix(doc, next_root, &lead);
        } else {
            let t = doc.trailing().as_str().unwrap_or("").to_string();
            doc.set_trailing(format!("{lead}{t}"));
        }
    }
}

fn set_subkey_string_in_table(t: &mut dyn toml_edit::TableLike, key: &str, value: &str) {
    let v = toml_edit::Value::from(value.to_string());
    if t.contains_key(key) {
        set_value_preserving_decor(t, key, v);
    } else {
        t.insert(key, toml_edit::Item::Value(v));
    }
}

fn set_subkey_string(doc: &mut toml_edit::DocumentMut, table_key: &str, key: &str, value: &str) {
    let Some(t) = doc.get_mut(table_key).and_then(|i| i.as_table_like_mut()) else {
        return;
    };
    set_subkey_string_in_table(t, key, value);
}

fn set_scalar(doc: &mut toml_edit::DocumentMut, key: &str, value: toml_edit::Value) {
    if doc.contains_key(key) {
        set_value_preserving_decor(doc.as_table_mut(), key, value);
        return;
    }
    let example = scalar_example(key);
    replace_example_with_value(doc, key, value, &example);
}

fn unset_scalar(doc: &mut toml_edit::DocumentMut, key: &str) {
    if !doc.contains_key(key) {
        return;
    }
    let example = scalar_example(key);
    remove_key_keeping_comments(doc, key, &known_doc_blocks(key), &example);
}

fn set_table(
    doc: &mut toml_edit::DocumentMut,
    key: &str,
    example: &str,
    build: impl FnOnce(&mut dyn toml_edit::TableLike),
) {
    if doc.contains_key(key) {
        if let Some(t) = doc.get_mut(key).and_then(|i| i.as_table_like_mut()) {
            build(t);
        }
    } else if find_example(doc, example, &known_doc_blocks(key)) {
        replace_example_with_table(doc, key, example, build);
    } else {
        append_root_table(doc, key, build);
    }
}

/// Remove every modelled subkey through the keep-the-comments removal, then
/// drop the table only once it holds no key at all (an unknown subkey keeps
/// it). The header's trailing comment and every comment inside stay, as lines,
/// where the table was.
fn unset_table(
    doc: &mut toml_edit::DocumentMut,
    key: &str,
    subkeys: &[&str],
    doc_blocks: &[String],
    example: &str,
) {
    for subkey in subkeys {
        remove_subkey_keeping_comments(doc, key, subkey);
    }
    if doc
        .get(key)
        .and_then(toml_edit::Item::as_table_like)
        .is_some_and(toml_edit::TableLike::is_empty)
    {
        remove_key_keeping_comments(doc, key, doc_blocks, example);
    }
}

fn preferred_days_value(days: &[String]) -> toml_edit::Value {
    let mut arr = toml_edit::Array::new();
    for d in days {
        arr.push(d.clone());
    }
    toml_edit::Value::from(arr)
}

const MODELS_FIELDS: [&str; 7] = [
    "default", "opus", "sonnet", "haiku", "fable", "subagent", "advisor",
];

fn models_fields(m: &ModelSettings) -> [(&'static str, Option<&String>); 7] {
    MODELS_FIELDS.map(|k| (k, m.field(k)))
}

fn build_models_table(t: &mut dyn toml_edit::TableLike, m: &ModelSettings) {
    for (k, v) in models_fields(m) {
        if let Some(v) = v {
            set_subkey_string_in_table(t, k, v);
        }
    }
}

fn edit_models_existing(doc: &mut toml_edit::DocumentMut, m: &ModelSettings) {
    for (k, v) in models_fields(m) {
        match v {
            Some(v) => set_subkey_string(doc, "models", k, v),
            None => remove_subkey_keeping_comments(doc, "models", k),
        }
    }
}

fn edit_string_scalar(
    doc: &mut toml_edit::DocumentMut,
    od: &Option<String>,
    ca: &Option<String>,
    key: &str,
) {
    if od == ca {
        return;
    }
    match ca {
        Some(v) => set_scalar(doc, key, toml_edit::Value::from(v.clone())),
        None => unset_scalar(doc, key),
    }
}

fn edit_f64_scalar(doc: &mut toml_edit::DocumentMut, od: Option<f64>, ca: Option<f64>, key: &str) {
    if od == ca {
        return;
    }
    match ca {
        Some(v) => set_scalar(doc, key, toml_edit::Value::from(v)),
        None => unset_scalar(doc, key),
    }
}

fn edit_u64_scalar(doc: &mut toml_edit::DocumentMut, od: Option<u64>, ca: Option<u64>, key: &str) {
    if od == ca {
        return;
    }
    match ca {
        // TOML integers are i64; the load clamps the interval into
        // MIN..=MAX_USAGE_PROBE_INTERVAL_MS, so the cast is lossless.
        Some(v) => set_scalar(doc, key, toml_edit::Value::from(v as i64)),
        None => unset_scalar(doc, key),
    }
}

fn edit_bool_scalar(doc: &mut toml_edit::DocumentMut, od: bool, ca: bool, key: &str) {
    if od == ca {
        return;
    }
    if ca {
        set_scalar(doc, key, toml_edit::Value::from(true));
    } else {
        unset_scalar(doc, key);
    }
}

fn edit_opt_bool_scalar(
    doc: &mut toml_edit::DocumentMut,
    od: Option<bool>,
    ca: Option<bool>,
    key: &str,
) {
    if od == ca {
        return;
    }
    match ca {
        Some(_) => set_scalar(doc, key, toml_edit::Value::from(false)),
        None => unset_scalar(doc, key),
    }
}

const CONSOLE_FIELDS: [&str; 3] = ["token", "site", "region"];

fn edit_console(doc: &mut toml_edit::DocumentMut, od: &ConsoleConfig, ca: &ConsoleConfig) {
    if od == ca {
        return;
    }
    if ca.token.is_some() || ca.site.is_some() || ca.region.is_some() {
        if doc.contains_key("console") {
            for (k, v) in [
                ("token", ca.token.as_deref()),
                ("site", ca.site.as_deref()),
                ("region", ca.region.as_deref()),
            ] {
                if let Some(v) = v {
                    set_subkey_string(doc, "console", k, v);
                }
            }
        } else {
            set_table(doc, "console", CONSOLE_EXAMPLE, |t| {
                for (k, v) in [
                    ("token", ca.token.as_deref()),
                    ("site", ca.site.as_deref()),
                    ("region", ca.region.as_deref()),
                ] {
                    if let Some(v) = v {
                        set_subkey_string_in_table(t, k, v);
                    }
                }
            });
        }
    } else {
        unset_table(
            doc,
            "console",
            &CONSOLE_FIELDS,
            &known_doc_blocks("console"),
            CONSOLE_EXAMPLE,
        );
    }
}

fn edit_models(doc: &mut toml_edit::DocumentMut, od: &ModelSettings, ca: &ModelSettings) {
    if od == ca {
        return;
    }
    if ca.is_empty() {
        unset_table(
            doc,
            "models",
            &MODELS_FIELDS,
            &known_doc_blocks("models"),
            MODELS_EXAMPLE,
        );
    } else if doc.contains_key("models") {
        edit_models_existing(doc, ca);
    } else {
        set_table(doc, "models", MODELS_EXAMPLE, |t| build_models_table(t, ca));
    }
}

fn edit_env(
    doc: &mut toml_edit::DocumentMut,
    od: &BTreeMap<String, String>,
    ca: &BTreeMap<String, String>,
) {
    if od == ca {
        return;
    }
    if ca.is_empty() {
        let keys: Vec<&str> = od.keys().map(String::as_str).collect();
        unset_table(doc, "env", &keys, &known_doc_blocks("env"), ENV_EXAMPLE);
    } else if doc.contains_key("env") {
        for k in od.keys() {
            if !ca.contains_key(k) {
                remove_subkey_keeping_comments(doc, "env", k);
            }
        }
        for (k, v) in ca {
            set_subkey_string(doc, "env", k, v);
        }
    } else {
        set_table(doc, "env", ENV_EXAMPLE, |t| {
            for (k, v) in ca {
                set_subkey_string_in_table(t, k, v);
            }
        });
    }
}

/// The on-disk config typed whole: `Ok` when the file parses into
/// [`ProfileConfig`], `Err` when it holds a modelled key clauth cannot read
/// (a wrong type, or the `kick_timer` alias beside `auto_start`, which serde
/// counts as one field). An `Err` file still reaches the shape check, which
/// then refuses on the first unreadable key and never runs the
/// non-plain/`edited` comparison over a defaulted view; one the shape check
/// cannot attribute to a key refuses with the load's own parse error.
fn on_disk_profile_config(raw: &str) -> Result<ProfileConfig, toml::de::Error> {
    toml::from_str::<ProfileConfig>(raw)
}

/// What a modelled key's on-disk shape means for the save writer.
#[derive(Clone, Copy, PartialEq, Eq)]
enum KeyShape {
    /// Plain: the writer edits it in place (a one-line scalar of the right
    /// type, or a block table whose modelled subkeys are one-line strings).
    Plain,
    /// Readable but not plain: an inline table, a dotted or implicit table, a
    /// multi-line string, or a multi-line array. The save refuses only when it
    /// would edit this key.
    NonPlain,
    /// Clauth cannot read the key into its field type (the wrong kind or
    /// type). The save refuses whenever this key sits in the file.
    Unreadable,
}

/// The scalar type a modelled scalar field reads from TOML.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ScalarKind {
    Str,
    F64,
    U64,
    Bool,
    OptBool,
    StrArray,
}

/// The modelled scalar a top-level key names, with its read type; the
/// `kick_timer` alias reads as the bool `auto_start` does.
fn modelled_scalar(key: &str) -> Option<(ScalarKind, &'static str)> {
    Some(match key {
        "base_url" => (ScalarKind::Str, "base_url"),
        "api_key" => (ScalarKind::Str, "api_key"),
        "effort" => (ScalarKind::Str, "effort"),
        "fallback_threshold" => (ScalarKind::F64, "fallback_threshold"),
        "weekly_threshold" => (ScalarKind::F64, "weekly_threshold"),
        "max_auto_spend" => (ScalarKind::F64, "max_auto_spend"),
        "bell_threshold" => (ScalarKind::F64, "bell_threshold"),
        "auto_start" => (ScalarKind::Bool, "auto_start"),
        "usage_probe" => (ScalarKind::Bool, "usage_probe"),
        "usage_probe_interval_ms" => (ScalarKind::U64, "usage_probe_interval_ms"),
        "kick_timer" => (ScalarKind::Bool, "kick_timer"),
        "last_resort" => (ScalarKind::Bool, "last_resort"),
        "preferred" => (ScalarKind::Bool, "preferred"),
        "rolling_token" => (ScalarKind::Bool, "rolling_token"),
        "disabled" => (ScalarKind::Bool, "disabled"),
        "check_weekly" => (ScalarKind::OptBool, "check_weekly"),
        "check_scoped" => (ScalarKind::OptBool, "check_scoped"),
        "check_credit_cap" => (ScalarKind::OptBool, "check_credit_cap"),
        "preferred_days" => (ScalarKind::StrArray, "preferred_days"),
        _ => return None,
    })
}

/// Whether a string value is written in a multi-line `"""` or `'''` form. The
/// decoded content is the same as a one-line string, so the form is read off
/// the value's rendered representation, minus the whitespace the key/value
/// separator leaves on its prefix.
fn is_multiline_string(v: &toml_edit::Value) -> bool {
    if !v.is_str() {
        return false;
    }
    let repr = v.to_string();
    let repr = repr.trim_start();
    repr.starts_with("\"\"\"") || repr.starts_with("'''")
}

fn scalar_shape(item: &toml_edit::Item, kind: ScalarKind) -> KeyShape {
    let Some(v) = item.as_value() else {
        return KeyShape::Unreadable; // a table or array-of-tables where a value belongs
    };
    match kind {
        ScalarKind::Str => {
            if v.is_str() {
                if is_multiline_string(v) {
                    KeyShape::NonPlain
                } else {
                    KeyShape::Plain
                }
            } else {
                KeyShape::Unreadable
            }
        }
        ScalarKind::F64 => {
            if v.is_integer() || v.is_float() {
                KeyShape::Plain
            } else {
                KeyShape::Unreadable
            }
        }
        ScalarKind::U64 => {
            if v.as_integer().is_some_and(|i| i >= 0) {
                KeyShape::Plain
            } else {
                KeyShape::Unreadable
            }
        }
        ScalarKind::Bool | ScalarKind::OptBool => {
            if v.is_bool() {
                KeyShape::Plain
            } else {
                KeyShape::Unreadable
            }
        }
        ScalarKind::StrArray => {
            if v.as_array().is_some_and(|a| a.iter().all(|e| e.is_str())) {
                // The form is read off the value's own representation, the way
                // `is_multiline_string` reads a string's form: an array written
                // across several lines is not a one-line value.
                if v.to_string().contains('\n') {
                    KeyShape::NonPlain
                } else {
                    KeyShape::Plain
                }
            } else {
                KeyShape::Unreadable
            }
        }
    }
}

/// The shape of a table's relevant subkeys: every relevant subkey must be a
/// one-line string for the table to count as plain.
fn table_shape(item: &toml_edit::Item, subkeys: &[&str], all: bool) -> KeyShape {
    let Some(t) = item.as_table_like() else {
        return KeyShape::Unreadable; // a value (or none) where a table belongs
    };
    let mut shape = KeyShape::Plain;
    for (k, sub) in t.iter() {
        if !all && !subkeys.contains(&k) {
            continue;
        }
        match subkey_shape(sub) {
            KeyShape::Unreadable => return KeyShape::Unreadable,
            KeyShape::NonPlain => shape = KeyShape::NonPlain,
            KeyShape::Plain => {}
        }
    }
    shape
}

fn subkey_shape(item: &toml_edit::Item) -> KeyShape {
    let Some(v) = item.as_value() else {
        return KeyShape::Unreadable; // a nested table or array where a value belongs
    };
    if v.is_str() {
        if is_multiline_string(v) {
            KeyShape::NonPlain
        } else {
            KeyShape::Plain
        }
    } else {
        KeyShape::Unreadable
    }
}

/// The shape of a modelled top-level key, with its static name, or `None` for
/// a key clauth does not model.
fn key_shape(key: &str, item: &toml_edit::Item) -> Option<(KeyShape, &'static str)> {
    if let Some((kind, name)) = modelled_scalar(key) {
        return Some((scalar_shape(item, kind), name));
    }
    let (subkeys, all, name) = match key {
        "console" => (CONSOLE_FIELDS.as_slice(), false, "console"),
        "models" => (MODELS_FIELDS.as_slice(), false, "models"),
        "env" => (&[][..], true, "env"), // every env subkey is a modelled variable
        _ => return None,
    };
    let inner = table_shape(item, subkeys, all);
    let shape = if item.is_inline_table() || item.as_table().is_some_and(|t| t.is_implicit()) {
        // An inline table or a table with no `[table]` header (a dotted key, or
        // the parent of a nested table) stays non-plain however plain its
        // subkeys are.
        match inner {
            KeyShape::Unreadable => KeyShape::Unreadable,
            _ => KeyShape::NonPlain,
        }
    } else {
        inner
    };
    Some((shape, name))
}

/// Whether the save would edit `key`: the same comparison the edit step uses
/// to decide, listed once per modelled field so the refusal mirrors the writer.
fn edited(key: &str, on_disk: &ProfileConfig, canonical: &ProfileConfig) -> bool {
    match key {
        "base_url" => on_disk.base_url != canonical.base_url,
        "api_key" => on_disk.api_key != canonical.api_key,
        "auto_start" => on_disk.auto_start != canonical.auto_start,
        "usage_probe" => on_disk.usage_probe != canonical.usage_probe,
        "usage_probe_interval_ms" => {
            on_disk.usage_probe_interval_ms != canonical.usage_probe_interval_ms
        }
        "fallback_threshold" => on_disk.fallback_threshold != canonical.fallback_threshold,
        "weekly_threshold" => on_disk.weekly_threshold != canonical.weekly_threshold,
        "last_resort" => on_disk.last_resort != canonical.last_resort,
        "preferred" => on_disk.preferred != canonical.preferred,
        "preferred_days" => on_disk.preferred_days != canonical.preferred_days,
        "rolling_token" => on_disk.rolling_token != canonical.rolling_token,
        "max_auto_spend" => on_disk.max_auto_spend != canonical.max_auto_spend,
        "check_weekly" => on_disk.check_weekly != canonical.check_weekly,
        "check_scoped" => on_disk.check_scoped != canonical.check_scoped,
        "check_credit_cap" => on_disk.check_credit_cap != canonical.check_credit_cap,
        "bell_threshold" => on_disk.bell_threshold != canonical.bell_threshold,
        "disabled" => on_disk.disabled != canonical.disabled,
        "effort" => on_disk.effort != canonical.effort,
        "console" => on_disk.console != canonical.console,
        "models" => on_disk.models != canonical.models,
        "env" => on_disk.env != canonical.env,
        _ => false,
    }
}

/// The first modelled key, in file order, whose on-disk shape the save cannot
/// leave as written: an unreadable key, or a readable-but-non-plain key the
/// save would edit. When the typed parse failed (`on_disk` is `None`), only the
/// unreadable arm and the `kick_timer` pair run — the non-plain/`edited` test
/// never compares a defaulted view. `None` when every edit is over a plain shape.
fn non_plain_edit(
    doc: &toml_edit::DocumentMut,
    on_disk: Option<&ProfileConfig>,
    canonical: &ProfileConfig,
) -> Option<&'static str> {
    for (key, item) in doc.iter() {
        if key == "kick_timer"
            && doc.get("auto_start").is_some_and(|it| {
                key_shape("auto_start", it).is_some_and(|(s, _)| s != KeyShape::Unreadable)
            })
        {
            // The pre-rename alias beside a readable `auto_start`: serde counts
            // the alias and the field as one `auto_start`, so a file holding both
            // cannot parse. A mistyped `auto_start` is named on its own, in file
            // order, since removing the alias alone would not fix the file.
            return Some("kick_timer");
        }
        let Some((shape, name)) = key_shape(key, item) else {
            continue;
        };
        match shape {
            KeyShape::Unreadable => return Some(name),
            KeyShape::NonPlain => {
                if let Some(on_disk) = on_disk
                    && edited(key, on_disk, canonical)
                {
                    return Some(name);
                }
            }
            KeyShape::Plain => {}
        }
    }
    None
}

/// The one per-key edit list both writes share, so a key added here reaches the
/// save and the load alike. The mode picks the arms that differ: the load
/// comments out a threshold it ignores and keeps a typo'd weekday's line.
fn apply_config_edits(
    doc: &mut toml_edit::DocumentMut,
    on_disk: &ProfileConfig,
    canonical: &ProfileConfig,
    mode: ConfigEditMode,
) {
    let od = on_disk;
    edit_string_scalar(doc, &od.base_url, &canonical.base_url, "base_url");
    edit_string_scalar(doc, &od.api_key, &canonical.api_key, "api_key");
    edit_console(doc, &od.console, &canonical.console);
    if od.auto_start != canonical.auto_start {
        if doc.contains_key("kick_timer") {
            remove_key_keeping_comments(doc, "kick_timer", &[], "");
        }
        if canonical.auto_start {
            set_scalar(doc, "auto_start", toml_edit::Value::from(true));
        } else {
            unset_scalar(doc, "auto_start");
        }
    }
    edit_bool_scalar(doc, od.usage_probe, canonical.usage_probe, "usage_probe");
    edit_u64_scalar(
        doc,
        od.usage_probe_interval_ms,
        canonical.usage_probe_interval_ms,
        "usage_probe_interval_ms",
    );
    match mode {
        ConfigEditMode::Save => {
            edit_f64_scalar(
                doc,
                od.fallback_threshold,
                canonical.fallback_threshold,
                "fallback_threshold",
            );
            edit_f64_scalar(
                doc,
                od.weekly_threshold,
                canonical.weekly_threshold,
                "weekly_threshold",
            );
        }
        ConfigEditMode::Load => {
            edit_threshold_load(
                doc,
                od.fallback_threshold,
                canonical.fallback_threshold,
                "fallback_threshold",
            );
            edit_threshold_load(
                doc,
                od.weekly_threshold,
                canonical.weekly_threshold,
                "weekly_threshold",
            );
        }
    }
    edit_bool_scalar(doc, od.last_resort, canonical.last_resort, "last_resort");
    edit_bool_scalar(doc, od.preferred, canonical.preferred, "preferred");
    match mode {
        ConfigEditMode::Save => {
            if od.preferred_days != canonical.preferred_days {
                if canonical.preferred_days.is_empty() {
                    unset_scalar(doc, "preferred_days");
                } else {
                    set_scalar(
                        doc,
                        "preferred_days",
                        preferred_days_value(&canonical.preferred_days),
                    );
                }
            }
        }
        ConfigEditMode::Load => {
            edit_preferred_days_load(doc, &od.preferred_days, &canonical.preferred_days);
        }
    }
    edit_bool_scalar(
        doc,
        od.rolling_token,
        canonical.rolling_token,
        "rolling_token",
    );
    edit_f64_scalar(
        doc,
        od.max_auto_spend,
        canonical.max_auto_spend,
        "max_auto_spend",
    );
    edit_opt_bool_scalar(doc, od.check_weekly, canonical.check_weekly, "check_weekly");
    edit_opt_bool_scalar(doc, od.check_scoped, canonical.check_scoped, "check_scoped");
    edit_opt_bool_scalar(
        doc,
        od.check_credit_cap,
        canonical.check_credit_cap,
        "check_credit_cap",
    );
    match mode {
        ConfigEditMode::Save => {
            edit_f64_scalar(
                doc,
                od.bell_threshold,
                canonical.bell_threshold,
                "bell_threshold",
            );
        }
        ConfigEditMode::Load => {
            edit_threshold_load(
                doc,
                od.bell_threshold,
                canonical.bell_threshold,
                "bell_threshold",
            );
        }
    }
    edit_bool_scalar(doc, od.disabled, canonical.disabled, "disabled");
    edit_string_scalar(doc, &od.effort, &canonical.effort, "effort");
    edit_models(doc, &od.models, &canonical.models);
    edit_env(doc, &od.env, &canonical.env);
}

/// A threshold the load clamps is written clamped; one it ignores outright (a
/// non-finite value, or a `weekly_threshold` outside 50..=100) is commented out
/// in place.
fn edit_threshold_load(
    doc: &mut toml_edit::DocumentMut,
    od: Option<f64>,
    ca: Option<f64>,
    key: &str,
) {
    if od == ca {
        return;
    }
    if ca.is_none() {
        comment_out_scalar(doc, key);
    } else {
        edit_f64_scalar(doc, od, ca, key);
    }
}

/// A typo'd weekday keeps the whole typed line commented above the fixed list;
/// any other drift (a case or duplicate spelling) is written in place.
fn edit_preferred_days_load(doc: &mut toml_edit::DocumentMut, od: &[String], ca: &[String]) {
    if od == ca {
        return;
    }
    if has_typoed_weekday(od) {
        if ca.is_empty() {
            comment_out_scalar(doc, "preferred_days");
        } else {
            comment_out_and_set_preferred_days(doc, preferred_days_value(ca));
        }
    } else if ca.is_empty() {
        unset_scalar(doc, "preferred_days");
    } else {
        set_scalar(doc, "preferred_days", preferred_days_value(ca));
    }
}

/// Whether an entry of the on-disk weekday list does not parse: the load drops
/// it, so the line is commented out rather than rewritten in place.
fn has_typoed_weekday(raw: &[String]) -> bool {
    raw.iter().any(|e| e.trim().parse::<Weekday>().is_err())
}

/// Comment a modelled scalar out in place: the key's own line becomes `# ` plus
/// the line's text (key, separator, value, trailing comment), its leading
/// comments stay, and the key is removed so the file stops claiming the value.
fn comment_out_scalar(doc: &mut toml_edit::DocumentMut, key: &str) {
    let Some((kept, next)) = take_comment_out_line(doc, key) else {
        return;
    };
    if let Some(next_key) = next {
        prepend_prefix(doc, &next_key, &kept);
    } else {
        let t = doc.trailing().as_str().unwrap_or("").to_string();
        doc.set_trailing(format!("{kept}{t}"));
    }
}

/// Remove `key`, returning the text that replaces it in place — its leading
/// comments (the doc block) followed by its own line commented out — plus the
/// next root key, if any, the caller places the text before.
fn take_comment_out_line(
    doc: &mut toml_edit::DocumentMut,
    key: &str,
) -> Option<(String, Option<String>)> {
    let (lead, comment, next) = {
        let table = doc.as_table();
        let (k, item) = table.get_key_value(key)?;
        let v = item.as_value()?;
        let lead = k
            .leaf_decor()
            .prefix()
            .and_then(|r| r.as_str())
            .unwrap_or("")
            .to_string();
        // The key's own line text: the key name, its separator suffix, the `=`,
        // then the value's own rendered text. `Value`'s `Display` already
        // carries its decor (the separator space and any trailing comment), so
        // what was typed stays visible byte for byte.
        let key_suffix = k
            .leaf_decor()
            .suffix()
            .and_then(|r| r.as_str())
            .unwrap_or(" ");
        let comment = format!("# {}{}={}", k, key_suffix, v);
        let keys: Vec<String> = doc.iter().map(|(k, _)| k.to_string()).collect();
        let next = keys
            .iter()
            .position(|x| x == key)
            .and_then(|i| keys.get(i + 1))
            .cloned();
        (lead, comment, next)
    };
    doc.as_table_mut().remove(key);
    let mut kept = lead;
    kept.push_str(&comment);
    kept.push('\n');
    Some((kept, next))
}

/// A typo'd `preferred_days` with some valid entries: the typed line stays as a
/// comment above the fixed list, which becomes the key where the typed line was.
fn comment_out_and_set_preferred_days(doc: &mut toml_edit::DocumentMut, value: toml_edit::Value) {
    let Some((kept, next)) = take_comment_out_line(doc, "preferred_days") else {
        return;
    };
    match next {
        Some(next_key) => {
            insert_root_value_before(doc, &next_key, "preferred_days", value, Some(kept));
        }
        None => insert_root_value(doc, "preferred_days", value, Some(kept)),
    }
}

#[cfg(test)]
#[path = "../tests/inline/profile.rs"]
mod tests;
