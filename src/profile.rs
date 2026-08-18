use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
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

use crate::lock::{StateLockHeld, with_state_lock};
use crate::logline::logline;
use crate::providers::{Provider, ThirdPartyStats};
use crate::usage::{FetchStatus, UsageInfo};

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
}

#[derive(Debug, Clone)]
pub(crate) struct Profile {
    pub(crate) name: ProfileName,
    pub(crate) base_url: Option<String>,
    pub(crate) api_key: Option<String>,
    /// Fires a 1-token Haiku ping each 30s tick while no 5h window is active.
    pub(crate) auto_start: bool,
    /// Extra env vars merged into `settings.json`'s `env` block while active; cleared on switch.
    pub(crate) env: BTreeMap<String, String>,
    /// Per-account Claude Code model configuration, written into this profile's
    /// runtime `settings.json` (and the live `~/.claude` settings while active).
    pub(crate) models: ModelSettings,
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
    /// CLA-ROLL: the daemon re-stamps this profile's `session-token.json` with the
    /// usage chain's current access token on every rotation (full scopes +
    /// `subscriptionType`, no refresh token — sessions get plan-gated-model
    /// bearers while the refresh chain stays clauth-private). Off — the
    /// default — keeps the sidecar exactly what was captured (static mint).
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
    /// Utilization % at/above which a bell toast fires in the overview tab.
    /// None = no bell for this profile.
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
    pub(crate) usage: Option<UsageInfo>,
    pub(crate) fetch_status: Option<FetchStatus>,
    /// Recognised third-party provider (derived from base_url).
    pub(crate) provider: Option<Provider>,
    /// Provider-specific usage data (e.g. DeepSeek balance).
    pub(crate) third_party_usage: Option<ThirdPartyStats>,
}

impl Profile {
    pub(crate) fn new(name: String, base_url: Option<String>, api_key: Option<String>) -> Self {
        let provider = base_url.as_deref().and_then(Provider::from_base_url);
        Self {
            name: name.into(),
            base_url,
            api_key,
            auto_start: false,
            env: BTreeMap::new(),
            models: ModelSettings::default(),
            fallback_threshold: None,
            weekly_threshold: None,
            last_resort: false,
            preferred: false,
            rolling_token: false,
            max_auto_spend: None,
            check_weekly: true,
            check_scoped: true,
            bell_threshold: None,
            disabled: false,
            console: None,
            credentials: slot(None),
            usage: None,
            fetch_status: None,
            provider,
            third_party_usage: None,
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

    /// Whether this account's usage figures live in `third_party_cache.json`
    /// rather than the OAuth `usage_cache.json` — the question every reader of a
    /// cached figure asks, and the one the scheduler answers by writing.
    ///
    /// True for a recognised provider AND for a generic api-key endpoint, whose
    /// discovered usage is cached the same way: `third_party_entry_for` builds a
    /// `ThirdPartyTarget::Generic` whenever `provider` is `None` and a
    /// `base_url` is set, so that leg genuinely fetches and caches for it.
    /// Keying a reader on [`Profile::is_third_party`] instead answers a
    /// DIFFERENT question and renders a generic account, refreshed hourly, as
    /// never fetched.
    pub(crate) fn usage_cache_is_third_party(&self) -> bool {
        usage_cache_is_third_party(
            self.provider,
            self.base_url.as_deref(),
            self.api_key.as_deref(),
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
    /// clobber a concurrent rotation.
    pub(crate) fn set_credentials(
        &mut self,
        credentials: Option<ClaudeCredentials>,
        _held: &StateLockHeld,
    ) {
        self.credentials.set(credentials, _held);
    }

    /// In-place access to the stored pair for the rotation persist leg, which
    /// rewrites token fields under the same hold the witness proves.
    pub(crate) fn credentials_mut(
        &mut self,
        _held: &StateLockHeld,
    ) -> Option<&mut ClaudeCredentials> {
        #[cfg(not(test))]
        {
            self.credentials.0.as_mut()
        }
        #[cfg(test)]
        {
            self.credentials.as_mut()
        }
    }
}

/// Theme tier stored in `profiles.toml`. Serialized as a lowercase string so
/// the file stays human-readable: `theme = "full"` / `theme = "compatible"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum ThemeName {
    Full,
    Compatible,
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
/// Plugin tab's herdr-options form rows, read by the plugin scripts through
/// `clauth herdr config get <key>` — so the on-disk shape is also a published
/// read contract. The `[herdr]` table itself may be absent (defaults) or
/// partial: a missing field fills from [`Default`] rather than erroring.
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
        }
    }
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
    /// The on-disk key stays `switch_off_when_spent`: it is also a `status.json` field
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
    /// profile projects its utilization at the next poll (current + recent
    /// burn rate × refresh interval) instead of comparing against the static
    /// per-profile threshold — switching exactly when it would otherwise
    /// cross 100% before the scheduler notices. Falls back to the static
    /// threshold check when no burn rate is available yet. Off by default:
    /// the static threshold stays the default auto-switch behavior (issue #8
    /// follow-up b). Candidate selection and `soonest_resume` are unaffected
    /// either way — see `fallback::is_exhausted_active`.
    #[serde(default, skip_serializing_if = "is_false")]
    pub(crate) burn_aware_switching: bool,
    /// Opt-in master switch for spending real money: when on, the auto-switch
    /// chain may pick a member whose subscription windows are spent but whose
    /// account still has pay-as-you-go budget, bounded by that member's
    /// `Profile::max_auto_spend` ceiling. Off by default, and every ceiling
    /// defaults to `$0`, so BOTH halves must be set before a cent is spent
    /// unattended. Spend-armed members rank below every subscription member
    /// with free quota — see `fallback::next_target`.
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
    /// When false, the background usage fetch skips accounts already pinned at
    /// their 100% window cap (spent) until the window resets — a spent window
    /// can't change until then, so re-polling only burns quota + poll load.
    /// Default true (poll every account every interval — today's behavior). A
    /// forced `r` refresh and a never-fetched account still poll once (a reset
    /// is only observed by polling). Fetch-leg only: never touches
    /// switch/fallback predicates. See `usage::scheduler` + `windows_maxed`.
    #[serde(default = "default_refresh_spent", skip_serializing_if = "is_true")]
    pub(crate) refresh_spent_accounts: bool,
    /// Config-file theme override. CLI `--theme` flag takes priority; auto-
    /// detect applies when this is `None` and no flag was passed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) theme: Option<ThemeName>,
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
    /// may fire (`burn_aware_switching` only). The projection replaces the
    /// static threshold with "would cross 100% before the next poll", and on a
    /// small window (Pro) the window-relative burn %/h reads high, so the
    /// projection trips from well below 100 — this caps the wasted headroom at
    /// `100 - floor` on every tier. `None` = [`DEFAULT_BURN_FLOOR_PCT`]. Read
    /// through [`AppState::burn_switch_floor_pct`], which resets a hand-edited
    /// out-of-band value to the default. Inert unless burn-aware is on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) burn_switch_floor_pct: Option<f64>,
    /// Burn-aware horizon cap (ms): the projection looks ahead by
    /// `min(refresh_interval, this)` instead of the full refresh interval, so a
    /// long poll cadence can't balloon the early-switch margin (it scales
    /// linearly with the look-ahead). `None` = [`DEFAULT_BURN_HORIZON_MS`]. Read
    /// through [`AppState::burn_horizon_cap_ms`]. Inert unless burn-aware is on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) burn_horizon_cap_ms: Option<u64>,
    /// herdr-mode knobs (popup width, pane tag, the delegate dot). Omitted from
    /// the file while every knob is at its default, so an untouched
    /// profiles.toml gains no `[herdr]` block on the next save.
    #[serde(default, skip_serializing_if = "herdr_is_default")]
    pub(crate) herdr: HerdrSettings,
}

fn herdr_is_default(herdr: &HerdrSettings) -> bool {
    *herdr == HerdrSettings::default()
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

    /// The effective reset-countdown shape (unset = the stock relative form).
    pub(crate) fn reset_display(&self) -> ResetDisplay {
        self.reset_display.unwrap_or_default()
    }

    /// The effective wall-clock notation (unset = 24-hour).
    pub(crate) fn clock_format(&self) -> ClockFormat {
        self.clock_format.unwrap_or_default()
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
    /// is only ever compared against — and floored by — the refresh interval.
    pub(crate) fn burn_horizon_cap_ms(&self) -> u64 {
        self.burn_horizon_cap_ms
            .filter(|v| (MIN_REFRESH_INTERVAL_MS..=MAX_REFRESH_INTERVAL_MS).contains(v))
            .unwrap_or(DEFAULT_BURN_HORIZON_MS)
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

fn is_true(b: &bool) -> bool {
    *b
}

fn is_false(b: &bool) -> bool {
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

/// Default burn-aware floor (percent). 98 mirrors the weekly default: a safe
/// backstop that never lets a projected switch waste more than 2% of the
/// window, while the horizon cap does the common-case reclaiming. Tune up for
/// tighter margins (more window used, small rate-limit risk), 100 = only ever
/// switch at the cap.
pub(crate) const DEFAULT_BURN_FLOOR_PCT: f64 = 98.0;

/// Lowest configurable burn-aware floor. Below this the projection may switch
/// so far from 100 that the poll-lag margin it exists to protect is gone.
pub(crate) const MIN_BURN_FLOOR_PCT: f64 = 90.0;

/// Highest configurable burn-aware floor — 100 makes the projection fire only
/// once utilization is already at the cap.
pub(crate) const MAX_BURN_FLOOR_PCT: f64 = 100.0;

/// Default burn-aware horizon cap (60 s). Under the default 90 s cadence this
/// shrinks the projected look-ahead below the full interval, reclaiming most of
/// the early-switch margin while keeping a poll-lag cushion. Bounded by the
/// refresh interval either way (`min(interval, cap)`).
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
            spend_budget_switching: false,
            switch_off_when_budget_spent: default_switch_off_when_budget_spent(),
            preemptive_rotation: default_preemptive_rotation(),
            refresh_spent_accounts: true,
            theme: None,
            reset_display: None,
            clock_format: None,
            show_estimates: true,
            show_pace: false,
            count_cache: false,
            refresh_interval_ms: default_refresh_interval(),
            default_divergence: None,
            weekly_switch_threshold: None,
            burn_switch_floor_pct: None,
            burn_horizon_cap_ms: None,
            herdr: HerdrSettings::default(),
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

    /// True when `name`'s last OAuth refresh was rejected as revoked/invalid
    /// (AUTH-1). Such a profile is skipped by the fallback chain walk.
    pub(crate) fn is_auth_broken(&self, name: &ProfileName) -> bool {
        self.state.is_auth_broken(name)
    }

    /// Mark or clear `name`'s auth-broken flag. Returns `true` when the set
    /// actually changed, so the caller can skip a redundant `save_app_state`.
    /// Pure in-memory mutation — the caller persists via `save_app_state`.
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
        self.state.profiles.retain(|n| n != name);
        self.state.fallback_chain.retain(|n| n != name);
        self.state.auth_broken.retain(|n| n != name);
        if self.is_active(name) {
            self.state.set_active(None, held);
        }
    }

    /// Resync `state.profiles` from in-memory list to fix length drift from partial saves.
    pub(crate) fn sync_state_profiles(&mut self) {
        self.state.profiles = self.profiles.iter().map(|p| p.name.clone()).collect();
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
        if let Some(slot) = self.state.profiles.iter_mut().find(|n| **n == *old) {
            *slot = new.clone();
        }
        if let Some(slot) = self.state.fallback_chain.iter_mut().find(|n| **n == *old) {
            *slot = new.clone();
        }
        if let Some(slot) = self.state.auth_broken.iter_mut().find(|n| **n == *old) {
            *slot = new.clone();
        }
        if self.is_active(old) {
            self.state.set_active(Some(new.clone()), held);
        }
    }
}

/// Per-account model knobs written into the profile's Claude Code `settings.json`.
/// `default` is the `model` setting; `opus`/`sonnet`/`haiku`/`fable` are the
/// `ANTHROPIC_DEFAULT_*_MODEL` env overrides; `subagent` is
/// `CLAUDE_CODE_SUBAGENT_MODEL`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
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
}

impl ModelSettings {
    pub(crate) fn is_empty(&self) -> bool {
        self.default.is_none()
            && self.opus.is_none()
            && self.sonnet.is_none()
            && self.haiku.is_none()
            && self.fable.is_none()
            && self.subagent.is_none()
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
#[derive(Debug, Default, Serialize, Deserialize, PartialEq)]
struct ConsoleConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    site: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    region: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Default, PartialEq)]
struct ProfileConfig {
    base_url: Option<String>,
    api_key: Option<String>,
    #[serde(default, alias = "kick_timer")]
    auto_start: bool,
    #[serde(default)]
    env: BTreeMap<String, String>,
    #[serde(default)]
    models: ModelSettings,
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
    bell_threshold: Option<f64>,
    #[serde(default)]
    disabled: bool,
    /// `[console]` — the Alibaba Model Studio console session. A TABLE, so it
    /// must render after every scalar key in `render_config_toml`.
    #[serde(default)]
    console: ConsoleConfig,
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
    /// `(profile dir name, config.toml mtime, session-token.json write time)`,
    /// each `None` when the file is absent, sorted by name so readdir order can't
    /// spuriously flip equality. The sidecar rides here because a
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
    config_mtimes: Vec<(String, Option<SystemTime>, Option<SystemTime>)>,
}

/// Filesystem stat of the reload triggers, plus the swap receipt beside a
/// `session-token.json` that exists — never that file's contents, since this runs
/// per TUI frame. Holds NO locks — `config` sits high in the rank hierarchy, so
/// this must stay lock-free — and fails soft: a readdir/stat error contributes
/// the empty value instead of erroring.
pub(crate) fn reload_fingerprint() -> ReloadFingerprint {
    let profiles_toml_mtime = app_state_mtime();
    let mut config_mtimes: Vec<(String, Option<SystemTime>, Option<SystemTime>)> = Vec::new();
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
            config_mtimes.push((name, config_mtime, token));
        }
    }
    config_mtimes.sort();
    ReloadFingerprint {
        profiles_toml_mtime,
        codex_toml_mtime: crate::codex_profiles::codex_state_mtime(),
        config_mtimes,
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
        - 2 * 24 * 60 * 60 * 1000;

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

/// Open a profile's `usage_history.jsonl` for append, creating it 0o600 on Unix.
/// The log records per-profile utilization samples under `~/.clauth`, so it
/// rides the owner-only invariant rather than the process umask.
fn history_append_file(path: &Path) -> std::io::Result<std::fs::File> {
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
    let Ok(next_json) = serde_json::to_string(next) else {
        return;
    };
    let bridge_json = prev.and_then(|p| serde_json::to_string(p).ok());
    let unchanged = match &bridge_json {
        Some(json) => json == &next_json,
        None => load_usage_history(name)
            .last()
            .and_then(|(_, info)| serde_json::to_string(info).ok())
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
    let ts = crate::usage::now_ms();
    let mut body = match &bridge_json {
        Some(json) => line(ts.saturating_sub(1), json),
        None => String::new(),
    };
    body.push_str(&line(ts, &next_json));

    match history_append_file(&path) {
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

fn profile_credentials_pending_path(name: &ProfileName) -> Result<PathBuf> {
    profile_subpath(name, "credentials.json.pending")
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

pub(crate) fn atomic_write(path: &Path, content: impl AsRef<[u8]>) -> std::io::Result<()> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    if !dir.exists() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = tmp_sibling(path);
    std::fs::write(&tmp, content)?;
    match std::fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

/// Like [`atomic_write`] but creates the temp file with mode 0o600 (Unix only)
/// so the file is never world-readable even for the instant before the rename.
/// On non-Unix this is identical to [`atomic_write`].
pub(crate) fn atomic_write_600(path: &Path, content: impl AsRef<[u8]>) -> std::io::Result<()> {
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
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)?;
        f.write_all(content.as_ref())?;
    }
    #[cfg(not(unix))]
    std::fs::write(&tmp, content)?;
    match std::fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
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
/// `usage-fetch.lock`, session PID files, `rotation-locks/<name>.lock`) routes
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
/// dirs, 0o600 files). Installs created before the invariant carry umask modes
/// no writer revisits once the bytes stop changing, so [`load_config`] runs
/// this on every entry point. Symlinks are skipped and never traversed: a
/// shared-mode runtime is full of links into the operator's `~/.claude`, and
/// following one would chmod a file clauth does not own. Best-effort per entry
/// — a chmod failure on one path never aborts the walk or the load.
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
        let want = if is_dir { 0o700 } else { 0o600 };
        if meta.permissions().mode() & 0o777 != want {
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

pub(crate) fn read_json_file<T: DeserializeOwned>(path: &Path) -> Result<T> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    serde_json::from_str(&content).with_context(|| format!("failed to parse {}", path.display()))
}

pub(crate) fn read_toml_file<T: DeserializeOwned>(path: &Path) -> Result<T> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    toml::from_str(&content).with_context(|| format!("failed to parse {}", path.display()))
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
/// the NEW same-name account (e.g. a rotation/adopt persisting a pair minted
/// from the old chain). Only a full [`reload_fingerprint`] drift check closes
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
    let path = app_state_path()?;
    if !path.exists() {
        return Ok(AppState::default());
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
    Ok(state)
}

pub(crate) fn save_app_state(state: &AppState) -> Result<()> {
    with_state_lock(|_held| {
        mkdir_700(&clauth_dir()?)?;
        atomic_write_600(&app_state_path()?, toml::to_string_pretty(state)?)
            .context("failed to write profiles.toml")
    })
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
    with_state_lock(|_held| {
        let mut state = load_app_state()?;
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
/// The OAuth-bearer leak needs BOTH a stored pair AND no api key: CC would send
/// that bearer to the third-party base_url. Gate on the pair so a PURE api
/// account (no pair) with a cleared key keeps its base_url shell and stays
/// re-loginable (`clear_profile_api_key`). Normalized at the LOAD boundary, same
/// discipline as the `max_auto_spend` case. This governs the managed base_url
/// FIELD only: clauth never copies `ANTHROPIC_BASE_URL` into `profile.env`, so an
/// env override is always operator-authored and is never normalized here —
/// normalize the state clauth authors, not an explicit one.
///
/// "Never normalized" is not "never read". An env override still ROUTES the
/// account (`build_claude_settings_json` applies `profile.env` last), so a
/// caller asking where requests go must ask [`stored_endpoint`], which reads
/// both sources. This function alone answers only the managed half, which is
/// why a `None` from it does not mean Anthropic.
///
/// One rule, three readers: [`load_profile`],
/// [`stored_usage_cache_is_third_party`], which answers the same question
/// without the side effects, and [`stored_endpoint`]'s managed half.
fn effective_base_url(
    configured: Option<String>,
    has_credentials: bool,
    api_key: Option<&str>,
) -> Option<String> {
    let has_usable_key = api_key.map(str::trim).is_some_and(|k| !k.is_empty());
    match configured {
        Some(_) if has_credentials && !has_usable_key => None,
        other => other,
    }
}

/// Whether an account's usage figures live in `third_party_cache.json` rather
/// than the OAuth `usage_cache.json`, given the three stored fields that decide
/// it. THE answer to "which cache holds this account's figures": every reader of
/// a cached figure asks this, and [`load_profile`]'s own seeding branch — the
/// step that decides whether a `Profile` carries `third_party_usage` at all — is
/// the producer, so it calls this rather than spelling the rule again.
///
/// A recognised provider, or a generic api-key endpoint whose discovered usage
/// the same leg caches. `api_key.is_some()` rather than a trimmed-non-empty
/// test, deliberately: whether a blank key can AUTHENTICATE a fetch
/// (`third_party_credentialed`) is a different question from where the figures
/// would be written.
fn usage_cache_is_third_party(
    provider: Option<Provider>,
    base_url: Option<&str>,
    api_key: Option<&str>,
) -> bool {
    provider.is_some() || (base_url.is_some() && api_key.is_some())
}

/// [`Profile::usage_cache_is_third_party`] for a caller holding only a name,
/// read from `config.toml` plus one stat. A config clauth cannot read or parse
/// proves no endpoint, so it answers `false` and its reader falls back to the
/// OAuth cache, whose absence renders `unknown` — the honest reading when clauth
/// cannot classify.
///
/// Deliberately NOT `load_profile(name)`: that path recovers a staged rotation,
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
/// `third_party_cache.json` for what is really an OAuth account. Only
/// [`crate::mcp::digest`]'s sample can observe it — every other caller runs
/// `load_config` first, and `recover_pending_credentials` consumes the sidecar —
/// and it costs at most one digest call reporting no refresh. Pinned, in that
/// direction, by `the_lock_free_third_party_read_agrees_with_a_full_load`.
pub(crate) fn stored_usage_cache_is_third_party(name: &ProfileName) -> bool {
    let Ok(config_path) = profile_config_path(name) else {
        return false;
    };
    let Ok(raw) = std::fs::read_to_string(&config_path) else {
        return false;
    };
    let Ok(config) = toml::from_str::<ProfileConfig>(&raw) else {
        return false;
    };
    let has_credentials = profile_credentials_path(name).is_ok_and(|p| p.exists());
    let base_url = effective_base_url(config.base_url, has_credentials, config.api_key.as_deref());
    usage_cache_is_third_party(
        base_url.as_deref().and_then(Provider::from_base_url),
        base_url.as_deref(),
        config.api_key.as_deref(),
    )
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
    let Ok(config_path) = profile_config_path(name) else {
        return None;
    };
    let Ok(raw) = std::fs::read_to_string(&config_path) else {
        return None;
    };
    let Ok(config) = toml::from_str::<ProfileConfig>(&raw) else {
        return None;
    };
    let has_credentials = profile_credentials_path(name).is_ok_and(|p| p.exists());
    let base_url = effective_base_url(config.base_url, has_credentials, config.api_key.as_deref());
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
/// caller qualifies a figure it need not have.
pub(crate) fn stored_endpoint(name: &ProfileName) -> StoredEndpoint {
    let Ok(config_path) = profile_config_path(name) else {
        return StoredEndpoint::Unknown;
    };
    let Ok(raw) = std::fs::read_to_string(&config_path) else {
        return StoredEndpoint::Unknown;
    };
    let Ok(config) = toml::from_str::<ProfileConfig>(&raw) else {
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
    let has_credentials = profile_credentials_path(name).is_ok_and(|p| p.exists());
    match effective_base_url(config.base_url, has_credentials, config.api_key.as_deref()) {
        Some(url) => StoredEndpoint::Custom(url),
        None => StoredEndpoint::Anthropic,
    }
}

pub(crate) fn load_profile(name: &ProfileName) -> Result<Profile> {
    let config_path = profile_config_path(name)?;
    let raw_config = match std::fs::read_to_string(&config_path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e).with_context(|| format!("failed to read {name}/config.toml")),
    };
    let config: ProfileConfig = if raw_config.trim().is_empty() {
        ProfileConfig::default()
    } else {
        toml::from_str(&raw_config)
            .with_context(|| format!("failed to parse {name}/config.toml"))?
    };

    let cred_path = profile_credentials_path(name)?;
    let credentials = if cred_path.exists() {
        Some(read_json_file(&cred_path)?)
    } else {
        None
    };
    // Adopt a staged rotation that never committed (crash/failed write between OAuth response and save).
    let credentials = recover_pending_credentials(name, credentials);

    let base_url = effective_base_url(
        config.base_url,
        credentials.is_some(),
        config.api_key.as_deref(),
    );

    let provider = base_url.as_deref().and_then(Provider::from_base_url);
    // Seed third-party usage from disk for every account whose figures live in
    // that cache — the predicate is named there so no reader has to restate it.
    let third_party_usage =
        if usage_cache_is_third_party(provider, base_url.as_deref(), config.api_key.as_deref()) {
            crate::profile_cache::load_profile_cache::<crate::providers::ThirdPartyStats>(
                name,
                crate::profile_cache::THIRD_PARTY_CACHE_FILE,
            )
        } else {
            None
        };

    let profile = Profile {
        name: name.clone(),
        base_url,
        api_key: config.api_key,
        auto_start: config.auto_start,
        env: config.env,
        models: config.models,
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
        bell_threshold: finite_pct(config.bell_threshold),
        disabled: config.disabled,
        console: console_credential(&config.console),
        credentials: slot(credentials),
        usage: None,
        fetch_status: None,
        provider,
        third_party_usage,
    };

    maybe_rewrite_config_toml(&config_path, &raw_config, &profile);

    Ok(profile)
}

/// Refresh config.toml when its semantic content drifts from what we'd render
/// today. Comment-only or whitespace-only differences shouldn't trigger a
/// rewrite — the TUI reloads on every state-file change and we don't want to
/// thrash disk on every reload.
fn maybe_rewrite_config_toml(config_path: &Path, raw_config: &str, profile: &Profile) {
    let rendered = render_config_toml(profile);
    let needs_rewrite = match toml::from_str::<ProfileConfig>(&rendered) {
        Ok(canonical) => {
            let on_disk = ProfileConfig {
                base_url: profile.base_url.clone(),
                api_key: profile.api_key.clone(),
                auto_start: profile.auto_start,
                env: profile.env.clone(),
                models: profile.models.clone(),
                fallback_threshold: profile.fallback_threshold,
                weekly_threshold: profile.weekly_threshold,
                last_resort: profile.last_resort,
                preferred: profile.preferred,
                rolling_token: profile.rolling_token,
                max_auto_spend: profile.max_auto_spend,
                // Default-on booleans render as commented examples when on, so
                // the canonical form is `None` (unset) — an explicit `= true`
                // on disk normalizes away on the next rewrite.
                check_weekly: (!profile.check_weekly).then_some(false),
                check_scoped: (!profile.check_scoped).then_some(false),
                bell_threshold: profile.bell_threshold,
                disabled: profile.disabled,
                console: console_config(profile.console.as_ref()),
            };
            canonical != on_disk
        }
        Err(_) => raw_config != rendered,
    };
    if needs_rewrite {
        let _ = with_state_lock(|_held| {
            // config.toml can carry `api_key` — same 0600 rule as save_profile.
            let _ = atomic_write_600(config_path, &rendered);
            Ok(())
        });
    }
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
fn serialize_credentials_preserving_extra(
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

pub(crate) fn save_profile(profile: &Profile) -> Result<()> {
    with_state_lock(|_held| {
        mkdir_700(&profile_dir(&profile.name)?)?;

        // credentials.json BEFORE config.toml: single-use refresh token must
        // not be lost to a config.toml write failure.
        let cred_path = profile_credentials_path(&profile.name)?;
        match profile.credentials.as_ref() {
            Some(creds) => {
                let bytes = serialize_credentials_preserving_extra(creds, &cred_path)?;
                atomic_write_600(&cred_path, bytes).context("failed to write credentials.json")?;
                // This profile has a store again, so whatever it parked when it
                // last lost one goes back where every reader looks for it.
                crate::claude::restore_parked_mcp_logins(&profile.name, &cred_path);
            }
            None if cred_path.exists() => {
                // Read the MCP-server logins out before the only file carrying
                // them goes. This is the chokepoint rather than each caller
                // because every path that stops a profile storing a login lands
                // here: a recapture onto a third-party endpoint, and blanking an
                // OAuth login both do.
                crate::claude::park_mcp_logins_from_store(&profile.name, &cred_path);
                std::fs::remove_file(&cred_path).context("failed to remove credentials.json")?;
            }
            None => {}
        }

        atomic_write_600(
            &profile_config_path(&profile.name)?,
            render_config_toml(profile),
        )
        .context("failed to write config.toml")?;

        Ok(())
    })
}

/// Write rotated credentials to a sidecar BEFORE `save_profile`. Single-use
/// refresh tokens can't be lost to a crash mid-save; `load_profile` adopts
/// this sidecar on next start if the commit never landed.
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

/// Adopt the rotation sidecar when it's at least as new as `credentials.json`
/// (commit failed or process died mid-save). A stale sidecar is discarded.
fn recover_pending_credentials(
    name: &ProfileName,
    loaded: Option<ClaudeCredentials>,
) -> Option<ClaudeCredentials> {
    let Ok(pending_path) = profile_credentials_pending_path(name) else {
        return loaded;
    };
    let Ok(pending_meta) = pending_path.symlink_metadata() else {
        return loaded; // no sidecar — the common case
    };
    let recovered = (|| -> Option<ClaudeCredentials> {
        let bytes = std::fs::read(&pending_path).ok()?;
        let pending: ClaudeCredentials = serde_json::from_slice(&bytes).ok()?;
        pending.claude_ai_oauth.as_ref()?; // must carry an oauth block to matter
        let cred_path = profile_credentials_path(name).ok()?;
        // Clean success → credentials.json strictly newer → discard.
        // Failed/interrupted commit → sidecar newer, tied, or no
        // credentials.json at all → adopt. A tie means staging and committing
        // landed in one mtime tick; of the two ways to be wrong, dropping a
        // rotation that may never have landed is the unrecoverable one.
        //
        // The committed side's WRITE time, not its raw mtime: a per-session swap
        // stamps a store it repoints to without writing it, and reading that
        // stamp as a commit discards a sidecar staged moments earlier.
        let adopt = match crate::profile_cache::effective_write_time(&cred_path) {
            Some(cred_mtime) => pending_meta
                .modified()
                .map(|p| p >= cred_mtime)
                .unwrap_or(true),
            None => true,
        };
        if !adopt {
            return None;
        }
        // Through the preserving serializer, not the staged bytes: staging holds
        // the rotated login alone, so writing it raw would drop every non-login
        // block the store carries. The recovery leg is the one write that reaches
        // the store without going through `save_profile`.
        let _ = with_state_lock(|_held| {
            let body = serialize_credentials_preserving_extra(&pending, &cred_path)?;
            atomic_write_600(&cred_path, body).map_err(Into::into)
        });
        Some(pending)
    })();
    let _ = std::fs::remove_file(&pending_path);
    recovered.or(loaded)
}

pub(crate) fn load_config() -> Result<AppConfig> {
    mkdir_700(&profiles_root()?)?;
    // Every entry point loads config early, so this is the tree-wide chokepoint
    // that retightens an install created before the owner-only invariant.
    if let Ok(dir) = clauth_dir() {
        enforce_clauth_perms(&dir);
    }
    let state = load_app_state()?;
    let profiles = state
        .profiles
        .iter()
        .map(load_profile)
        .collect::<Result<Vec<_>>>()?;
    Ok(AppConfig { state, profiles })
}

/// Renders config.toml with set values uncommented and unset ones as commented examples.
fn render_config_toml(profile: &Profile) -> String {
    fn toml_str(s: &str) -> String {
        toml::Value::String(s.to_string()).to_string()
    }

    let mut out = String::from("# clauth profile configuration\n\n");

    out.push_str("# Base URL for an API-endpoint profile. Leave commented for an OAuth\n");
    out.push_str("# (Pro / Max / Team / Enterprise) profile.\n");
    match profile.base_url.as_deref() {
        Some(v) => out.push_str(&format!("base_url = {}\n", toml_str(v))),
        None => out.push_str("# base_url = \"https://api.anthropic.com\"\n"),
    }
    out.push('\n');

    out.push_str("# API key for the endpoint. Only used when base_url is set.\n");
    match profile.api_key.as_deref() {
        Some(v) => out.push_str(&format!("api_key = {}\n", toml_str(v))),
        None => out.push_str("# api_key = \"sk-ant-...\"\n"),
    }
    out.push('\n');

    out.push_str("# Auto-start the 5-hour usage window for this profile. clauth fires a\n");
    out.push_str("# 1-token Haiku ping at launch and on every 30s refresh while there's\n");
    out.push_str("# no running window. ~0.001¢ per ping. OAuth profiles only.\n");
    out.push_str("# Old name `kick_timer = true` is still accepted.\n");
    if profile.auto_start {
        out.push_str("auto_start = true\n");
    } else {
        out.push_str("# auto_start = true\n");
    }
    out.push('\n');

    out.push_str("# 5-hour utilization percentage at/above which clauth will auto-switch\n");
    out.push_str("# off this profile, provided the profile is also a member of the\n");
    out.push_str("# fallback chain configured in ~/.clauth/profiles.toml. Range 0..=100.\n");
    match profile.fallback_threshold {
        Some(v) => out.push_str(&format!("fallback_threshold = {v}\n")),
        None => out.push_str("# fallback_threshold = 95.0\n"),
    }
    out.push('\n');

    out.push_str("# Per-account override of the chain-wide weekly (7d) switch line (the\n");
    out.push_str("# Config tab's `weekly limit`, default 98). Governs when auto-switching\n");
    out.push_str("# treats this account's week — aggregate and per-model — as spent.\n");
    out.push_str("# Commented = follow the chain-wide value. Range 0..=100.\n");
    match profile.weekly_threshold {
        Some(v) => out.push_str(&format!("weekly_threshold = {v}\n")),
        None => out.push_str("# weekly_threshold = 98.0\n"),
    }
    out.push('\n');

    out.push_str("# Marks this profile as the fallback chain's last resort. Once the\n");
    out.push_str("# auto-switch walk lands here with no other member having headroom, it\n");
    out.push_str("# parks instead of turning off all accounts. Independent of\n");
    out.push_str("# fallback_threshold, this profile still switches away at its own\n");
    out.push_str("# threshold whenever another chain member has headroom.\n");
    if profile.last_resort {
        out.push_str("last_resort = true\n");
    } else {
        out.push_str("# last_resort = true\n");
    }
    out.push('\n');

    out.push_str("# Marks this profile as the fallback chain's preferred (home) account.\n");
    out.push_str("# At most one member is preferred; the toggle is a radio. Once live work\n");
    out.push_str("# has drifted off it and it reads clear and fresh again, the daemon walks\n");
    out.push_str("# the active account — and every following session — back to it. Mutually\n");
    out.push_str("# exclusive with last_resort.\n");
    if profile.preferred {
        out.push_str("preferred = true\n");
    } else {
        out.push_str("# preferred = true\n");
    }
    out.push('\n');

    out.push_str("# CLA-ROLL: re-stamp this profile's session-token.json with the usage\n");
    out.push_str("# chain's current access token on every rotation (plan-gated models\n");
    out.push_str("# work in sessions, refresh chain stays clauth-private). Managed by\n");
    out.push_str("# `clauth rolling-token <profile>` / `clauth static-token <profile>`.\n");
    if profile.rolling_token {
        out.push_str("rolling_token = true\n");
    } else {
        out.push_str("# rolling_token = true\n");
    }
    out.push('\n');

    out.push_str("# Ceiling in US dollars on what the fallback chain may spend of this\n");
    out.push_str("# account's pay-as-you-go budget unattended. Needs `spend_budget_switching`\n");
    out.push_str("# on in profiles.toml AND pay-as-you-go enabled on the account; 0 (the\n");
    out.push_str("# default) never spends. The chain stops using this account once its\n");
    out.push_str("# spend reaches 90% of this or of the account's own cap, whichever is\n");
    out.push_str("# lower — parking on a `last_resort` member if the chain has one, else\n");
    out.push_str("# per `switch_off_when_budget_spent`.\n");
    match profile.max_auto_spend {
        Some(v) => out.push_str(&format!("max_auto_spend = {v}\n")),
        None => out.push_str("# max_auto_spend = 5.0\n"),
    }
    out.push('\n');

    out.push_str("# Whether auto-switching checks this account's aggregate weekly (7d)\n");
    out.push_str("# usage against the weekly limit. Set false to keep this account in\n");
    out.push_str("# rotation across the soft weekly band; the 100% hard cap always blocks.\n");
    if profile.check_weekly {
        out.push_str("# check_weekly = false\n");
    } else {
        out.push_str("check_weekly = false\n");
    }
    out.push('\n');

    out.push_str("# Whether auto-switching checks this account's per-model weekly windows\n");
    out.push_str("# (e.g. \"7d fable\") against the weekly limit. Set false to ignore them\n");
    out.push_str("# and keep this account in rotation for use with other models.\n");
    if profile.check_scoped {
        out.push_str("# check_scoped = false\n");
    } else {
        out.push_str("check_scoped = false\n");
    }
    out.push('\n');

    out.push_str("# 5-hour utilization percentage at/above which clauth fires a bell\n");
    out.push_str("# notification in the overview tab. Range 0..=100.\n");
    match profile.bell_threshold {
        Some(v) => out.push_str(&format!("bell_threshold = {v}\n")),
        None => out.push_str("# bell_threshold = 95.0\n"),
    }
    out.push('\n');

    out.push_str("# Disable this account: it becomes invisible to the fallback chain, the\n");
    out.push_str("# usage/rotation scheduler, and the daemon status feed (by default), while\n");
    out.push_str("# its profile directory and credentials stay on disk untouched.\n");
    if profile.disabled {
        out.push_str("disabled = true\n");
    } else {
        out.push_str("# disabled = true\n");
    }
    out.push('\n');

    // Tables last: every key after a `[table]` header belongs to that table, so
    // this block and the two below must follow every scalar key above.
    out.push_str("# Alibaba Model Studio console session. The ONLY credential that can read\n");
    out.push_str("# Token Plan quota — the api key authenticates inference and is never read\n");
    out.push_str("# by the quota surface. Captured by `clauth login <name>` on an Alibaba\n");
    out.push_str("# profile. It expires 48h after your aliyun browser sign-in, NOT after the\n");
    out.push_str("# login: re-running the login inherits whatever is left of that window,\n");
    out.push_str("# which can be minutes. A full window needs a fresh console sign-in first.\n");
    out.push_str("# site: domestic | international.\n");
    match profile.console.as_ref() {
        Some(c) => {
            out.push_str("[console]\n");
            out.push_str(&format!("token = {}\n", toml_str(&c.token)));
            out.push_str(&format!("site = {}\n", toml_str(c.site.as_str())));
            out.push_str(&format!("region = {}\n", toml_str(&c.region)));
        }
        None => {
            out.push_str("# [console]\n");
            out.push_str("# token = \"...\"\n");
            out.push_str("# site = \"international\"\n");
            out.push_str("# region = \"ap-southeast-1\"\n");
        }
    }
    out.push('\n');

    out.push_str("# Per-account Claude Code model configuration, written into this profile's\n");
    out.push_str("# settings.json. `default` is the `model` setting (an alias like `opusplan`\n");
    out.push_str("# or a full id like `claude-opus-4-8[1m]`); `opus`/`sonnet`/`haiku`/`fable`\n");
    out.push_str("# pin what those aliases resolve to (ANTHROPIC_DEFAULT_*_MODEL); `subagent`\n");
    out.push_str("# forces the subagent model (CLAUDE_CODE_SUBAGENT_MODEL).\n");
    let m = &profile.models;
    let scalars = [
        ("default", &m.default),
        ("opus", &m.opus),
        ("sonnet", &m.sonnet),
        ("haiku", &m.haiku),
        ("fable", &m.fable),
        ("subagent", &m.subagent),
    ];
    if scalars.iter().all(|(_, v)| v.is_none()) {
        out.push_str("# [models]\n");
        out.push_str("# default = \"opusplan\"\n");
    } else {
        out.push_str("[models]\n");
        for (k, v) in scalars {
            if let Some(v) = v {
                out.push_str(&format!("{k} = {}\n", toml_str(v)));
            }
        }
    }
    out.push('\n');

    out.push_str("# Extra env vars merged into ~/.claude/settings.json's env block while\n");
    out.push_str("# this profile is active. Cleared on switch to another profile.\n");
    if profile.env.is_empty() {
        out.push_str("# [env]\n");
        out.push_str("# HTTP_PROXY = \"http://localhost:8080\"\n");
    } else {
        out.push_str("[env]\n");
        for (k, v) in &profile.env {
            out.push_str(&format!("{k} = {}\n", toml_str(v)));
        }
    }

    out
}

#[cfg(test)]
#[path = "../tests/inline/profile.rs"]
mod tests;
