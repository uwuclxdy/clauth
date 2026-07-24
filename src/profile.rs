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

use crate::lock::with_state_lock;
use crate::logline::logline;
use crate::providers::{Provider, ThirdPartyStats};
use crate::usage::{FetchStatus, UsageInfo};

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
    /// Which CLI this profile's credentials belong to. See [`Harness`].
    pub(crate) harness: Harness,
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
    /// CLA-FEED: the daemon feeds this profile's `session-token.json` with the
    /// usage chain's current access token on every rotation (full scopes +
    /// `subscriptionType`, no refresh token — sessions get Fable-capable
    /// bearers while the refresh chain stays clauth-private). Off — the
    /// default — keeps the sidecar exactly what was captured (static mint).
    pub(crate) session_feed: bool,
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
    pub(crate) credentials: Option<ClaudeCredentials>,
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
            harness: Harness::default(),
            base_url,
            api_key,
            auto_start: false,
            env: BTreeMap::new(),
            models: ModelSettings::default(),
            fallback_threshold: None,
            weekly_threshold: None,
            last_resort: false,
            session_feed: false,
            max_auto_spend: None,
            check_weekly: true,
            check_scoped: true,
            bell_threshold: None,
            disabled: false,
            credentials: None,
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
    /// UI. Endpoint routing stays on [`Profile::is_oauth`]: a `base_url` decides
    /// where requests go regardless of what else is stored.
    pub(crate) fn login_is_oauth(&self) -> bool {
        self.credentials.is_some() || self.is_oauth()
    }

    pub(crate) fn is_third_party(&self) -> bool {
        self.provider.is_some()
    }

    pub(crate) fn is_codex(&self) -> bool {
        self.harness == Harness::Codex
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
}

/// Which CLI a profile's credentials belong to (CDX-1). Persisted in the
/// profile's config.toml as `harness = "codex"`; absent = Claude, so every
/// pre-CDX config loads unchanged. Immutable after creation — a profile never
/// converts across harnesses (delete + recreate instead), and the two live
/// credential stores (`~/.claude` + Keychain vs `~/.codex/auth.json`) have
/// independent active slots (`AppState::active_profile` vs
/// `AppState::active_codex_profile`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Harness {
    #[default]
    Claude,
    Codex,
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
/// clock in the tree (`tui::render::format::clock_label`).
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

/// Stored at ~/.clauth/profiles.toml — ordering and active marker only.
/// Credentials and endpoint config live in per-profile subdirectories.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct AppState {
    pub(crate) active_profile: Option<ProfileName>,
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
    /// (schema 1, `wiki/daemon.md`), so renaming it would break a published read
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
    /// Opt-in: rotate the ACTIVE, Keychain-installed profile ahead of its
    /// access-token expiry instead of waiting for a 401 (rotation coherence,
    /// #1). Off by default — stock clauth stays strictly lazy. Adoption plus
    /// mirror-on-rotate already provide the correctness; the early refresh is
    /// an optimization (fewer live-mirror adopt events) some setups may want.
    /// See `usage::scheduler::proactive_rotation_due`.
    #[serde(default, skip_serializing_if = "is_false")]
    pub(crate) preemptive_rotation: bool,
    /// CDX-6: poll `wham/usage` for codex profiles' rate-limit windows with
    /// their stored access tokens — per-account live usage without a running
    /// session (the passive JSONL/proxy legs only see accounts that run).
    /// Default ON (AX decision 2026-07-22, reversing feasibility §2.5's ban —
    /// see `codex::poll`); the off switch exists because the endpoint is a
    /// private API that may change or gate without notice.
    #[serde(default = "default_codex_usage_poll", skip_serializing_if = "is_true")]
    pub(crate) codex_usage_poll: bool,
    /// Opt-in: on an `--isolated` `clauth start`, lift the run's transcripts out
    /// of the throwaway `runtime-isolated/projects/` store into the global
    /// `~/.claude/projects/` before the runtime is GC'd — so the session stays
    /// resumable and its tokens count in the Tokens tab. Off by default: stock
    /// clauth discards an isolated store on teardown, byte-for-byte. A per-run
    /// `--rescue`/`--no-rescue` flag overrides this toggle. See `start::run`.
    #[serde(default, skip_serializing_if = "is_false")]
    pub(crate) auto_rescue: bool,
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
    /// Chain-wide weekly (7d) exhaustion SOFT line, percent — past it an account
    /// counts as exhausted for the switch trigger + candidate acceptance. `None`
    /// = [`DEFAULT_WEEKLY_SWITCH_PCT`]. Read through
    /// [`AppState::weekly_switch_threshold_pct`], which resets hand-edited
    /// garbage to the default. Global (not per-member like the 5h threshold):
    /// the line protects the CHAIN — a wrong hop strands days. The wrap-off
    /// `Off` decision ignores this soft line and keys on the 100% hard cap
    /// (`WEEKLY_HARD_BLOCK_PCT` in `fallback.rs`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) weekly_switch_threshold: Option<f64>,
    /// The codex-harness active slot (CDX-1) — which profile's chain currently
    /// lives in `~/.codex/auth.json`. Independent of `active_profile` (claude):
    /// the two CLIs have separate live credential stores, so switching one
    /// never unlinks the other. Omitted from profiles.toml until first used.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) active_codex_profile: Option<ProfileName>,
    /// Codex-harness fallback chain (CDX-4 walks it; CDX-1 only stores +
    /// validates). Chains are per-harness: a codex profile can never enter
    /// `fallback_chain` and vice versa — the claude auto-switch path would
    /// otherwise install a profile with no claude credentials.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) codex_fallback_chain: Vec<ProfileName>,
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
}

impl AppState {
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
    /// honoring a hand-edited 40.0 as 50) — an out-of-band value hand-edited
    /// into profiles.toml must not silently disable the weekly gate (rationale
    /// in `fallback.rs`).
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

/// CDX-6 codex usage polling defaults ON (AX decision 2026-07-22); the flag
/// exists as the kill switch for a private-API dependency.
fn default_codex_usage_poll() -> bool {
    true
}

/// A spent budget stops spending unless the operator says otherwise, so this
/// defaults ON — unlike `switch_off_when_spent`, whose default keeps you signed in because
/// staying costs nothing there.
fn default_switch_off_when_budget_spent() -> bool {
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
            active_profile: None,
            profiles: Vec::new(),
            fallback_chain: Vec::new(),
            switch_off_when_spent: false,
            auth_broken: Vec::new(),
            burn_aware_switching: false,
            spend_budget_switching: false,
            switch_off_when_budget_spent: default_switch_off_when_budget_spent(),
            preemptive_rotation: false,
            codex_usage_poll: default_codex_usage_poll(),
            auto_rescue: false,
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
            active_codex_profile: None,
            codex_fallback_chain: Vec::new(),
            burn_switch_floor_pct: None,
            burn_horizon_cap_ms: None,
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
    pub(crate) fn is_active(&self, name: &str) -> bool {
        self.state.active_profile.as_deref() == Some(name)
    }

    /// Codex-slot counterpart of [`AppConfig::is_active`] — true when `name`'s
    /// chain currently lives in `~/.codex/auth.json`. The two slots are
    /// independent by design (see [`Harness`]).
    pub(crate) fn is_active_codex(&self, name: &str) -> bool {
        self.state.active_codex_profile.as_deref() == Some(name)
    }

    /// True when `name`'s last OAuth refresh was rejected as revoked/invalid
    /// (AUTH-1). Such a profile is skipped by the fallback chain walk.
    pub(crate) fn is_auth_broken(&self, name: &str) -> bool {
        self.state.auth_broken.iter().any(|n| n.as_str() == name)
    }

    /// Mark or clear `name`'s auth-broken flag. Returns `true` when the set
    /// actually changed, so the caller can skip a redundant `save_app_state`.
    /// Pure in-memory mutation — the caller persists via `save_app_state`.
    pub(crate) fn set_auth_broken(&mut self, name: &str, broken: bool) -> bool {
        let present = self.is_auth_broken(name);
        if broken && !present {
            self.state.auth_broken.push(name.into());
            true
        } else if !broken && present {
            self.state.auth_broken.retain(|n| n.as_str() != name);
            true
        } else {
            false
        }
    }

    pub(crate) fn find(&self, name: &str) -> Option<&Profile> {
        self.profiles.iter().find(|p| p.name == name)
    }

    pub(crate) fn find_mut(&mut self, name: &str) -> Option<&mut Profile> {
        self.profiles.iter_mut().find(|p| p.name == name)
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

    /// Add a profile, or REPLACE an existing one with the same name (upsert).
    /// Re-authenticating an existing profile (`clauth login <existing>`) captures
    /// through here; append-only would leave a duplicate `Profile` + a duplicate
    /// name in `state.profiles`. The name in `state.profiles` is left untouched on
    /// replace (it is already present and kept in sync with `self.profiles`).
    pub(crate) fn add(&mut self, profile: Profile) {
        if let Some(existing) = self.profiles.iter_mut().find(|p| p.name == profile.name) {
            *existing = profile;
            return;
        }
        self.state.profiles.push(profile.name.clone());
        self.profiles.push(profile);
    }

    pub(crate) fn remove(&mut self, name: &str) {
        self.profiles.retain(|p| p.name != name);
        self.state.profiles.retain(|n| n.as_str() != name);
        self.state.fallback_chain.retain(|n| n.as_str() != name);
        self.state
            .codex_fallback_chain
            .retain(|n| n.as_str() != name);
        self.state.auth_broken.retain(|n| n.as_str() != name);
        if self.is_active(name) {
            self.state.active_profile = None;
        }
        if self.is_active_codex(name) {
            self.state.active_codex_profile = None;
        }
    }

    /// Resync `state.profiles` from in-memory list to fix length drift from partial saves.
    pub(crate) fn sync_state_profiles(&mut self) {
        self.state.profiles = self.profiles.iter().map(|p| p.name.clone()).collect();
    }

    /// Replace `old` with `new` in every name list and the active marker.
    pub(crate) fn rename_all_occurrences(&mut self, old: &str, new: &str) {
        if let Some(profile) = self.find_mut(old) {
            profile.name = new.into();
        }
        if let Some(slot) = self.state.profiles.iter_mut().find(|n| n.as_str() == old) {
            *slot = new.into();
        }
        if let Some(slot) = self
            .state
            .fallback_chain
            .iter_mut()
            .find(|n| n.as_str() == old)
        {
            *slot = new.into();
        }
        if let Some(slot) = self
            .state
            .codex_fallback_chain
            .iter_mut()
            .find(|n| n.as_str() == old)
        {
            *slot = new.into();
        }
        if let Some(slot) = self
            .state
            .auth_broken
            .iter_mut()
            .find(|n| n.as_str() == old)
        {
            *slot = new.into();
        }
        if self.is_active(old) {
            self.state.active_profile = Some(new.into());
        }
        if self.is_active_codex(old) {
            self.state.active_codex_profile = Some(new.into());
        }
    }
}

/// Per-account model knobs written into the profile's Claude Code `settings.json`.
/// `default` is the `model` setting; `opus`/`sonnet`/`haiku` are the
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
    pub(crate) subagent: Option<String>,
}

impl ModelSettings {
    pub(crate) fn is_empty(&self) -> bool {
        self.default.is_none()
            && self.opus.is_none()
            && self.sonnet.is_none()
            && self.haiku.is_none()
            && self.subagent.is_none()
    }
}

#[derive(Debug, Serialize, Deserialize, Default, PartialEq)]
struct ProfileConfig {
    #[serde(default)]
    harness: Harness,
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
    #[serde(default)]
    session_feed: bool,
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

pub(crate) fn home_dir() -> Result<PathBuf> {
    #[cfg(test)]
    if let Some(path) = HOME_OVERRIDE.lock().ok().and_then(|g| g.clone()) {
        return Ok(path);
    }
    dirs::home_dir().context("cannot determine home directory")
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
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) struct ReloadFingerprint {
    profiles_toml_mtime: Option<SystemTime>,
    /// `(profile dir name, config.toml mtime, session-token.json mtime)`, each
    /// mtime `None` when the file is absent, sorted by name so readdir order
    /// can't spuriously flip equality. The sidecar rides here because a
    /// `login --setup-token` re-mint touches nothing else — without it the hot
    /// reload never sees a new/changed long-lived token.
    config_mtimes: Vec<(String, Option<SystemTime>, Option<SystemTime>)>,
}

/// Pure filesystem stat of the reload triggers. Holds NO locks — `config` sits
/// high in the rank hierarchy, so this must stay lock-free — and fails soft: a
/// readdir/stat error contributes the empty value instead of erroring.
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
            let mtime_of = |file: &str| {
                std::fs::metadata(entry.path().join(file))
                    .and_then(|m| m.modified())
                    .ok()
            };
            config_mtimes.push((
                name,
                mtime_of("config.toml"),
                mtime_of("session-token.json"),
            ));
        }
    }
    config_mtimes.sort();
    ReloadFingerprint {
        profiles_toml_mtime,
        config_mtimes,
    }
}

fn profiles_root() -> Result<PathBuf> {
    Ok(clauth_dir()?.join("profiles"))
}

fn app_state_path() -> Result<PathBuf> {
    Ok(clauth_dir()?.join("profiles.toml"))
}

pub(crate) fn profile_dir(name: &str) -> Result<PathBuf> {
    Ok(profiles_root()?.join(name))
}

pub(crate) fn profile_subpath(name: &str, sub: &str) -> Result<PathBuf> {
    Ok(profile_dir(name)?.join(sub))
}

fn profile_config_path(name: &str) -> Result<PathBuf> {
    profile_subpath(name, "config.toml")
}

fn profile_credentials_path(name: &str) -> Result<PathBuf> {
    profile_subpath(name, "credentials.json")
}

pub(crate) fn profile_history_path(name: &str) -> Result<PathBuf> {
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
/// Called at startup only — rewrites the file in-place when there's anything
/// to remove. No-op when the file is missing, unparseable, or already within
/// the retention window.
pub(crate) fn prune_usage_history(name: &str) {
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

/// Load all parsed entries from a profile's usage_history.jsonl.
/// Returns chronological (timestamp_ms, UsageInfo) pairs.
pub(crate) fn load_usage_history(name: &str) -> Vec<(u64, UsageInfo)> {
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

fn profile_credentials_pending_path(name: &str) -> Result<PathBuf> {
    profile_subpath(name, "credentials.json.pending")
}

/// Core tempfile+rename writer. `mode = Some(m)` creates the temp file at that
/// Unix mode (and any dir it must create at 0o700, since a 0o600 file under a
/// world-readable dir still leaks via the dir entry); `None` uses the default
/// umask mode and `create_dir_all` for a missing parent. `durable = true`
/// `fsync`s the file before the rename and best-effort `fsync`s the parent dir
/// after, so a crash/power-loss can't leave a zero-length or lost file (TECH-9
/// #5). `durable = false` is the fast path reserved for the 1/s `status.json`
/// write, which is a rebuildable cache — durability there would fsync every
/// second for no correctness gain.
fn write_and_rename(
    path: &Path,
    content: &[u8],
    mode: Option<u32>,
    durable: bool,
) -> std::io::Result<()> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    if !dir.exists() {
        match mode {
            Some(_) => mkdir_700(dir)?,
            None => std::fs::create_dir_all(dir)?,
        }
    }
    let file_name = path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".to_string());
    let tmp = dir.join(format!(".{file_name}.tmp.{}", std::process::id()));
    // Clear any stale temp so `create_new` lands on a fresh inode — guarantees
    // the requested mode is applied at creation, never inherited from a looser file.
    if tmp.exists() {
        std::fs::remove_file(&tmp)?;
    }
    let mut f = {
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        if let Some(m) = mode {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(m);
        }
        // A `create_new` failure — notably `AlreadyExists` from a concurrent
        // same-PID writer racing the identical temp path — propagates WITHOUT
        // deleting the temp: it belongs to that other writer, not to us, and
        // removing it would sabotage the rename it is about to perform.
        opts.open(&tmp)?
    };
    // The temp is ours now, so clean it up on any write/sync failure — a partial
    // write must never leak under the 0700 dir (the rename arm cleans up its own).
    let write_result = {
        use std::io::Write;
        f.write_all(content).and_then(|()| {
            // Flush data+metadata to disk BEFORE the rename so the rename can
            // never expose a zero-length file after a crash.
            if durable { f.sync_all() } else { Ok(()) }
        })
    };
    drop(f);
    if let Err(e) = write_result {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    match std::fs::rename(&tmp, path) {
        Ok(()) => {
            if durable {
                // Best-effort: fsync the directory so the rename itself is durable.
                if let Ok(d) = std::fs::File::open(dir) {
                    let _ = d.sync_all();
                }
            }
            Ok(())
        }
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

/// Tempfile + rename write; readers always see old or new, never partial.
/// Durable (fsync before rename) — see [`write_and_rename`].
pub(crate) fn atomic_write(path: &Path, content: impl AsRef<[u8]>) -> std::io::Result<()> {
    write_and_rename(path, content.as_ref(), None, true)
}

/// Like [`atomic_write`] but creates the temp file with mode 0o600 (Unix only)
/// so the file is never world-readable even for the instant before the rename.
/// Durable. On non-Unix the mode is ignored.
pub(crate) fn atomic_write_600(path: &Path, content: impl AsRef<[u8]>) -> std::io::Result<()> {
    write_and_rename(path, content.as_ref(), Some(0o600), true)
}

/// Like [`atomic_write_600`] but NON-durable — the fast path for the 1/s
/// `status.json` write, a rebuildable menu-bar cache where an fsync every second
/// buys no correctness (TECH-9 #5 keeps this the only un-synced writer).
pub(crate) fn atomic_write_600_fast(path: &Path, content: impl AsRef<[u8]>) -> std::io::Result<()> {
    write_and_rename(path, content.as_ref(), Some(0o600), false)
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
/// `usage-fetch.lock`, session PID files, `rotation.lock`) routes through here
/// so no lock is born at the
/// process umask — the file itself carries nothing secret, but a blanket
/// owner-only tree is the invariant the perms test can check without an
/// exceptions list.
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

fn load_app_state() -> Result<AppState> {
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
    with_state_lock(|| {
        mkdir_700(&clauth_dir()?)?;
        atomic_write_600(&app_state_path()?, toml::to_string_pretty(state)?)
            .context("failed to write profiles.toml")
    })
}

/// Atomically read-modify-write the persisted `AppState` INSIDE the state flock:
/// reload the on-disk state, apply `delta` (which must touch ONLY the field(s)
/// this caller owns), and write the merged result. Returns the merged state so the
/// caller can re-sync its in-memory copy.
///
/// This is the cross-process lost-update fix (TECH-7, finding #1). The blind
/// [`save_app_state`] rewrites the WHOLE `profiles.toml` from a snapshot loaded
/// OUTSIDE the flock, so a writer that changed one field (e.g. a switch setting
/// `active_profile`) silently clobbers a concurrent writer's change to a DIFFERENT
/// field (e.g. a `clauth login` that appended a profile) — the appended profile is
/// permanently orphaned. Reloading inside the flock and applying a narrow delta
/// makes the two writers commute: each preserves the other's field.
pub(crate) fn update_app_state(delta: impl FnOnce(&mut AppState)) -> Result<AppState> {
    with_state_lock(|| {
        let mut state = load_app_state()?;
        delta(&mut state);
        mkdir_700(&clauth_dir()?)?;
        atomic_write_600(&app_state_path()?, toml::to_string_pretty(&state)?)
            .context("Failed to write profiles.toml")?;
        Ok(state)
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

pub(crate) fn load_profile(name: &str) -> Result<Profile> {
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

    // The OAuth-bearer leak needs BOTH a stored pair AND no api key: CC would
    // send that bearer to the third-party base_url. Gate on the pair so a PURE
    // api account (no pair) with a cleared key keeps its base_url shell and
    // stays re-loginable (`clear_profile_api_key`). Normalize at the LOAD
    // boundary, same discipline as the `max_auto_spend` case below. This governs
    // the managed base_url FIELD only: clauth never copies `ANTHROPIC_BASE_URL`
    // into `profile.env`, so an env override is always operator-authored and is
    // left untouched — normalize the state clauth authors, not an explicit one.
    let has_usable_key = config
        .api_key
        .as_deref()
        .map(str::trim)
        .is_some_and(|k| !k.is_empty());
    let base_url = match config.base_url {
        Some(_) if credentials.is_some() && !has_usable_key => None,
        other => other,
    };

    let provider = base_url.as_deref().and_then(Provider::from_base_url);
    // Seed third-party usage from disk for recognised providers AND generic
    // api-key endpoints (whose discovered usage is cached the same way).
    let third_party_usage =
        if provider.is_some() || (base_url.is_some() && config.api_key.is_some()) {
            crate::profile_cache::load_profile_cache::<crate::providers::ThirdPartyStats>(
                name,
                crate::profile_cache::THIRD_PARTY_CACHE_FILE,
            )
        } else {
            None
        };

    let profile = Profile {
        name: name.into(),
        harness: config.harness,
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
        session_feed: config.session_feed,
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
        credentials,
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
                harness: profile.harness,
                base_url: profile.base_url.clone(),
                api_key: profile.api_key.clone(),
                auto_start: profile.auto_start,
                env: profile.env.clone(),
                models: profile.models.clone(),
                fallback_threshold: profile.fallback_threshold,
                weekly_threshold: profile.weekly_threshold,
                last_resort: profile.last_resort,
                session_feed: profile.session_feed,
                max_auto_spend: profile.max_auto_spend,
                // Default-on booleans render as commented examples when on, so
                // the canonical form is `None` (unset) — an explicit `= true`
                // on disk normalizes away on the next rewrite.
                check_weekly: (!profile.check_weekly).then_some(false),
                check_scoped: (!profile.check_scoped).then_some(false),
                bell_threshold: profile.bell_threshold,
                disabled: profile.disabled,
            };
            canonical != on_disk
        }
        Err(_) => raw_config != rendered,
    };
    if needs_rewrite {
        let _ = with_state_lock(|| {
            // TECH-9 #15: config.toml may carry a third-party api_key — 0o600, never
            // the umask-default 0o644 the old `atomic_write` produced on this path.
            let _ = atomic_write_600(config_path, &rendered);
            Ok(())
        });
    }
}

pub(crate) fn save_profile(profile: &Profile) -> Result<()> {
    with_state_lock(|| {
        mkdir_700(&profile_dir(&profile.name)?)?;

        // credentials.json BEFORE config.toml: single-use refresh token must
        // not be lost to a config.toml write failure.
        let cred_path = profile_credentials_path(&profile.name)?;
        match &profile.credentials {
            Some(creds) => atomic_write_600(&cred_path, serde_json::to_string_pretty(creds)?)
                .context("failed to write credentials.json")?,
            None if cred_path.exists() => {
                std::fs::remove_file(&cred_path).context("failed to remove credentials.json")?
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
pub(crate) fn stage_rotated_credentials(name: &str, creds: &ClaudeCredentials) -> Result<()> {
    with_state_lock(|| {
        mkdir_700(&profile_dir(name)?)?;
        atomic_write_600(
            &profile_credentials_pending_path(name)?,
            serde_json::to_string_pretty(creds)?,
        )
        .context("failed to stage rotated credentials")
    })
}

pub(crate) fn clear_staged_credentials(name: &str) {
    if let Ok(path) = profile_credentials_pending_path(name) {
        let _ = std::fs::remove_file(path);
    }
}

/// Adopt the rotation sidecar when it's at least as new as `credentials.json`
/// (commit failed or process died mid-save). A stale sidecar is discarded.
fn recover_pending_credentials(
    name: &str,
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
        let adopt = match cred_path.metadata().and_then(|m| m.modified()) {
            Ok(cred_mtime) => pending_meta
                .modified()
                .map(|p| p >= cred_mtime)
                .unwrap_or(true),
            Err(_) => true,
        };
        if !adopt {
            return None;
        }
        let _ = with_state_lock(|| atomic_write_600(&cred_path, &bytes).map_err(Into::into));
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
        .map(|n| load_profile(n))
        .collect::<Result<Vec<_>>>()?;
    Ok(AppConfig { state, profiles })
}

/// Renders config.toml with set values uncommented and unset ones as commented examples.
fn render_config_toml(profile: &Profile) -> String {
    fn toml_str(s: &str) -> String {
        toml::Value::String(s.to_string()).to_string()
    }

    let mut out = String::from("# clauth profile configuration\n\n");

    out.push_str("# Which CLI this profile's credentials belong to: \"claude\" (default) or\n");
    out.push_str("# \"codex\". Set at profile creation — never edit by hand; a profile does\n");
    out.push_str("# not convert across harnesses.\n");
    match profile.harness {
        Harness::Codex => out.push_str("harness = \"codex\"\n"),
        Harness::Claude => out.push_str("# harness = \"claude\"\n"),
    }
    out.push('\n');

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

    out.push_str("# CLA-FEED: feed this profile's session-token.json with the usage\n");
    out.push_str("# chain's current access token on every rotation (Fable-capable\n");
    out.push_str("# sessions, refresh chain stays clauth-private). Managed by\n");
    out.push_str("# `clauth feed <profile> on|off`.\n");
    if profile.session_feed {
        out.push_str("session_feed = true\n");
    } else {
        out.push_str("# session_feed = true\n");
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

    out.push_str("# Per-account Claude Code model configuration, written into this profile's\n");
    out.push_str("# settings.json. `default` is the `model` setting (an alias like `opusplan`\n");
    out.push_str("# or a full id like `claude-opus-4-8[1m]`); `opus`/`sonnet`/`haiku` pin what\n");
    out.push_str("# those aliases resolve to (ANTHROPIC_DEFAULT_*_MODEL); `subagent` forces the\n");
    out.push_str("# subagent model (CLAUDE_CODE_SUBAGENT_MODEL).\n");
    let m = &profile.models;
    let scalars = [
        ("default", &m.default),
        ("opus", &m.opus),
        ("sonnet", &m.sonnet),
        ("haiku", &m.haiku),
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
