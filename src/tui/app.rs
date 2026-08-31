//! Application state, keymap, and tick logic.
//!
//! Layout invariants:
//!   - Overview: read-only account list; `profile_cursor` is shared with Usage
//!     and Config so the highlight follows across tab switches.
//!   - Config: master-detail — account list + `+ new` row + inline editor
//!     (`config_draft`). No popups for create / edit / rename / delete.
//!   - Fallback: master-detail — ordered chain + `+ add` on the left; inline
//!     threshold stepper / remove / add-picker on the right. No popups.
//!   - Modals stack: top of `modals` owns input; events reach the screen below
//!     only when the stack is empty.

use std::collections::{HashMap, HashSet, VecDeque};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::Result;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::actions::{
    CaptureSnapshot, EnvKeyCollision, capture_into_profile, capture_snapshot, classify_env_key,
    clear_profile_api_key, clear_profile_credentials, create_blank_profile,
    create_profile_from_login, delete_profile, duplicate_profile, edit_profile_endpoint,
    edit_profile_env, edit_profile_model, edit_profile_preset, find_matching_oauth_profile,
    overwrite_captured_profile, rename_profile, reorder_profile, rotation_guard_for_mutation,
    switch_off, switch_profile, validate_profile_name,
};
use crate::claude::{
    LinkState, adopt_first_login, classify_credentials_link, claude_settings_env_keys,
    credentials_diverged, detach_credentials_link, force_link_profile_credentials,
    force_snapshot_active_credentials, is_first_login, link_profile_credentials,
    live_credentials_are_shell, read_claude_credentials, snapshot_active_credentials,
};
use crate::fallback::{DEFAULT_THRESHOLD, SwitchAction, auto_switch_if_needed, threshold_for};
use crate::format::format_pct;
use crate::lock::with_state_lock;
use crate::lockorder::{RankedGuard, RankedMutex};
use crate::oauth;
use crate::profile::{
    AppConfig, ClockFormat, ConfigHandle, ConsoleSite, DivergenceChoice, HerdrSettings,
    MAX_REFRESH_INTERVAL_MS, MAX_WEEKLY_SWITCH_PCT, MIN_REFRESH_INTERVAL_MS, MIN_WEEKLY_SWITCH_PCT,
    ModelSettings, PopupWidth, Profile, ProfileName, ReloadFingerprint, ResetDisplay, ThemeName,
    load_config, reload_fingerprint, save_app_state, save_profile,
};
use crate::status::{self, Incident, StatusEvent};
use crate::tui::theme;
use crate::update::{self, UpdateEvent};
use crate::usage::{
    ActivityStore, FetchStatus, KickBlocks, LastFetchedAt, NextRefreshPerProfile, OpResult,
    OpResultReceiver, OpResultSender, PendingSwitch, PendingSwitchOff, PollStreaks,
    ProfileActivity, RefetchQueue, StartupReceiver, StartupSender, StartupSignal, StatusStore,
    SuppressedGenericStore, ThirdPartyList, ThirdPartyStatusStore, ThirdPartyUsageStore, TokenList,
    UsageInfo, UsageStore, any_busy, bootstrap_fetch, bootstrap_third_party, clear_activity,
    collect_oauth_seed_names, collect_third_party_entries, collect_tokens, is_idle, mark_activity,
    now_ms, spawn_refresher, switch_gate_in_flight,
};

// ── Shared input field ────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub(crate) struct InputState {
    pub(crate) value: String,
    /// Caret position in UTF-8 byte offset. Always falls on a char boundary.
    pub(crate) cursor: usize,
}

impl InputState {
    pub(crate) fn new(initial: &str) -> Self {
        Self {
            value: initial.to_string(),
            cursor: initial.len(),
        }
    }

    pub(crate) fn insert(&mut self, ch: char) {
        self.value.insert(self.cursor, ch);
        self.cursor += ch.len_utf8();
    }

    pub(crate) fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let prev = self.value[..self.cursor]
            .char_indices()
            .next_back()
            .map(|(i, _)| i)
            .unwrap_or(0);
        self.value.replace_range(prev..self.cursor, "");
        self.cursor = prev;
    }

    pub(crate) fn delete(&mut self) {
        if self.cursor >= self.value.len() {
            return;
        }
        let next = self.value[self.cursor..]
            .char_indices()
            .nth(1)
            .map(|(i, _)| self.cursor + i)
            .unwrap_or(self.value.len());
        self.value.replace_range(self.cursor..next, "");
    }

    pub(crate) fn left(&mut self) {
        if self.cursor == 0 {
            return;
        }
        self.cursor = self.value[..self.cursor]
            .char_indices()
            .next_back()
            .map(|(i, _)| i)
            .unwrap_or(0);
    }

    pub(crate) fn right(&mut self) {
        if self.cursor >= self.value.len() {
            return;
        }
        self.cursor = self.value[self.cursor..]
            .char_indices()
            .nth(1)
            .map(|(i, _)| self.cursor + i)
            .unwrap_or(self.value.len());
    }

    /// Delete the word (run of non-spaces, plus any trailing spaces) left of the
    /// caret — the `ctrl+w` editor verb.
    pub(crate) fn delete_word(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let head = &self.value[..self.cursor];
        let trimmed = head.trim_end_matches(' ');
        let start = trimmed.rfind(' ').map(|i| i + 1).unwrap_or(0);
        self.value.replace_range(start..self.cursor, "");
        self.cursor = start;
    }

    pub(crate) fn home(&mut self) {
        self.cursor = 0;
    }

    pub(crate) fn end(&mut self) {
        self.cursor = self.value.len();
    }

    pub(crate) fn trimmed(&self) -> &str {
        self.value.trim()
    }

    pub(crate) fn trimmed_some(&self) -> Option<String> {
        let t = self.trimmed();
        (!t.is_empty()).then(|| t.to_string())
    }
}

// ── Modals ────────────────────────────────────────────────────────────────────

/// One interactive line in the Fallback tab's detail pane for a chain member.
/// `Threshold` is a stepper (±5 on `+`/`-`); `CheckWeekly`/`CheckScoped`/
/// `LastResort`/`Preferred` are boolean toggles (space/⏎, per the
/// enumerated-row grammar);
/// `Remove` arms then confirms. The chain-global wrap-off setting lives on the
/// program-wide Config tab, not here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FallbackRow {
    Threshold,
    /// Per-account override of the chain-wide weekly (7d) switch line
    /// (`Profile::weekly_threshold`; the Config tab's `weekly limit` stays
    /// the default). ⏎ opens an inline editor; an EMPTY commit clears the
    /// override back to the chain-wide value. Inert (dimmed, no-op) while the
    /// member's `check_weekly` gate is off — the line isn't judged there.
    WeeklyAt,
    /// Whether auto-switching checks this account's aggregate weekly usage
    /// (`Profile::check_weekly`, on by default). Off, only the 100% hard cap
    /// blocks it.
    CheckWeekly,
    /// Whether auto-switching checks this account's per-model weekly windows,
    /// e.g. "7d fable" (`Profile::check_scoped`, on by default). Off, the
    /// account stays in rotation for use with other models.
    CheckScoped,
    LastResort,
    /// Whether this member is the chain's preferred (home) account
    /// (`Profile::preferred`, off by default). A pure on/off toggle, exclusive
    /// across the chain (a radio, like `LastResort`) and mutually exclusive with
    /// it. Marking it opts the chain into returning here once it reads clear and
    /// fresh again.
    Preferred,
    /// Dollar ceiling on what the chain may spend of this member's
    /// pay-as-you-go budget unattended (`Profile::max_auto_spend`, $0 default).
    /// Inert unless `AppState::spend_budget_switching` is also on — see
    /// `fallback::spend_room`. ⏎ opens an inline editor.
    MaxSpend,
    Remove,
}

/// One editable line in the Setup tab's detail pane. Built per selection by
/// [`config_rows`]: auto-start only for OAuth; trailing row is `delete` or
/// `create`. `Name`/`BaseUrl`/`ApiKey` are text rows; the rest are toggles/actions.
/// `EnvEntry(i)` indexes the profile's sorted custom-env snapshot; `EnvAdd` is the
/// trailing `+ add env` row. Both appear only for existing accounts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConfigRow {
    Name,
    /// OAuth-only auto-start toggle. `config_rows` renders it in the second
    /// slot (right below `Name`); declared here so the enum tracks that order.
    AutoStart,
    BaseUrl,
    ApiKey,
    /// Default model (CC `model` setting). Hybrid: space cycles aliases, ⏎ types a custom value.
    Model,
    /// `ANTHROPIC_DEFAULT_OPUS_MODEL` — full id, free text.
    OpusModel,
    /// `ANTHROPIC_DEFAULT_SONNET_MODEL` — full id, free text.
    SonnetModel,
    /// `ANTHROPIC_DEFAULT_HAIKU_MODEL` — full id, free text.
    HaikuModel,
    /// `ANTHROPIC_DEFAULT_FABLE_MODEL` — full id, free text.
    FableModel,
    /// `CLAUDE_CODE_SUBAGENT_MODEL` — full id, free text.
    SubagentModel,
    /// The `+ model override` reveal row. Shown only while the unset alias
    /// overrides are collapsed; ⏎ expands opus/sonnet/haiku/fable/subagent inline.
    ModelOverrideAdd,
    /// A custom `key = value` env entry, indexed into the profile's sorted env
    /// snapshot. ⏎ edits its VALUE. There is no removal action: an emptied value
    /// saves as an empty string, so the key stays in `settings.json` until the
    /// account's `config.toml` is edited by hand.
    EnvEntry(usize),
    /// The `+ add env` row — ⏎ opens a key editor that runs the collision check.
    EnvAdd,
    /// Browser OAuth login: mint fresh tokens into this account (or, on the
    /// `+ new` form, create the account from the login). Async — runs on a worker.
    Login,
    /// Drop this account's stored OAuth credentials, keeping the profile shell.
    DeleteCreds,
    /// CLA-SPLIT escape hatch: delete this account's `session-token.json`, so
    /// switches go back to installing its own `credentials.json`. Shown only
    /// while a sidecar exists (`claude::session_token_status`, so a MIS-FILLED
    /// one gets the row too — that state needs an exit as much as a live mint),
    /// and dimmed + inert when the account stores no other login, since
    /// clearing would leave it with no credentials at all. Arms on the first
    /// Space/⏎ and confirms on the second, like `Disabled`'s disable direction:
    /// it changes what every future switch installs and, on the active account,
    /// moves a running session's credentials.
    ClearSessionToken,
    /// User-disabled account-action row (`Profile::disabled`), same class as
    /// `Delete`: enabling fires the shared
    /// `actions::enable_profile` immediately (harmless), disabling arms on the
    /// first Space/⏎ and confirms on the second, via `actions::disable_profile`.
    /// Dimmed and inert while the account is active or holds a live `clauth
    /// start` session — the same gate the CLI enforces.
    Disabled,
    Delete,
    Create,
}

impl ConfigRow {
    /// Text rows capture keystrokes and commit on ⏎; the rest act on ⏎. The
    /// `Model` row is hybrid (space cycles, ⏎ opens a custom field), and the env
    /// rows seed their buffer before editing, so all three are driven out of band
    /// and are deliberately not text rows here.
    pub(crate) fn is_text(self) -> bool {
        matches!(
            self,
            ConfigRow::Name
                | ConfigRow::BaseUrl
                | ConfigRow::ApiKey
                | ConfigRow::OpusModel
                | ConfigRow::SonnetModel
                | ConfigRow::HaikuModel
                | ConfigRow::FableModel
                | ConfigRow::SubagentModel
        )
    }
}

/// The `model` row alias cycle (space advances it). `None` renders as `default`
/// (no `model` key); a custom id set via ⏎ is outside this list.
pub(crate) const MODEL_PRESETS: [&str; 4] = ["opus", "sonnet", "haiku", "opusplan"];

/// The weekly-line preset ladder: one source for the Config row's segmented
/// control AND `step_weekly_threshold`'s cycle. 100 reproduces the old
/// hard-cap behavior (switch only once the API already refuses).
pub(crate) const WEEKLY_PRESETS: [f64; 4] = [90.0, 95.0, 98.0, 100.0];

/// Presets for the burn-aware early-switch floor (percent). The projection may
/// not switch below the chosen value, so wasted headroom is capped at
/// `100 - floor`. Default 98 (see [`crate::profile::DEFAULT_BURN_FLOOR_PCT`]).
pub(crate) const BURN_FLOOR_PRESETS: [f64; 4] = [97.0, 98.0, 99.0, 100.0];

/// Presets for the burn-aware projection horizon cap (ms). The projection looks
/// ahead by `min(refresh_interval, this)`; a shorter cap keeps the switch
/// nearer 100. Default 60 s (see [`crate::profile::DEFAULT_BURN_HORIZON_MS`]).
pub(crate) const BURN_HORIZON_PRESETS: [u64; 4] = [30_000, 45_000, 60_000, 90_000];

/// One row on the program-wide Config tab. These back real persisted globals in
/// [`AppState`] — no decorative toggles. ⏎/space cycles or flips in place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GlobalConfigRow {
    /// Color-depth tier: `full` (truecolor) / `compatible` (xterm-256).
    /// Persists to `[theme]` and live-swaps the active palette.
    Theme,
    /// Shape of every reset countdown (`AppState.reset_display`, issue #39):
    /// `relative` (stock) / `clock` / `both`. ⏎/space cycles.
    ResetShape,
    /// Notation for the wall-clock half of a reset stamp
    /// (`AppState.clock_format`): `24h` / `12h`. Dimmed + inert while
    /// `reset display` is `relative`, since nothing renders a clock then.
    ClockNotation,
    /// Chain-wide "when spent" behavior (`AppState.switch_off_when_spent`) — surfaced here as
    /// a program-wide default alongside the Fallback detail row.
    SwitchOffWhenSpent,
    /// Chain-wide weekly (7d) exhaustion line
    /// (`AppState.weekly_switch_threshold`, default 98) — space steps presets,
    /// ⏎ opens the custom-value editor (50–100, decimals allowed).
    WeeklyThreshold,
    /// Global refresh interval; space cycles presets in-place, ⏎ opens the
    /// custom-value editor (10–3600 s).
    RefreshInterval,
    /// Default action when CC overwrites the credentials symlink. ⏎/space cycles.
    DivergenceDefault,
    /// Opt-in burn-aware auto-switch (`AppState.burn_aware_switching`, issue #8
    /// follow-up b) — off by default, projects the ACTIVE profile's
    /// utilization ahead of the next poll instead of the static threshold.
    BurnAware,
    /// Burn-aware early-switch floor (`AppState.burn_switch_floor_pct`) — space
    /// cycles [`BURN_FLOOR_PRESETS`]. Dimmed + inert unless burn-aware is on.
    BurnFloor,
    /// Burn-aware projection horizon cap (`AppState.burn_horizon_cap_ms`) —
    /// space cycles [`BURN_HORIZON_PRESETS`]. Dimmed + inert unless burn-aware
    /// is on.
    BurnHorizon,
    /// Opt-in master switch for real-money fallback
    /// (`AppState.spend_budget_switching`) — off by default. On, the chain may
    /// hop to a member whose subscription is spent but whose account still has
    /// pay-as-you-go budget, bounded by that member's `max auto-spend` ceiling
    /// (Fallback tab, $0 by default). BOTH halves are needed, so flipping this
    /// alone still spends nothing.
    SpendBudget,
    /// What happens once a billing account has spent its `max auto-spend`
    /// budget (`AppState.switch_off_when_budget_spent`, default switch-off). Deliberately
    /// NOT `SwitchOffWhenSpent`: that row answers "the chain is out of free quota", where
    /// staying costs nothing, and this one answers "the money ran out", where
    /// staying is the spending.
    SwitchOffWhenBudgetSpent,
    /// Preemptive rotation (`AppState.preemptive_rotation`) — ON by default.
    /// Rotates a profile ahead of token expiry rather than waiting for a 401,
    /// with enough lead that the running `claude` never reaches its own
    /// refresh threshold. Off falls back to rotating only on rejection.
    PreemptiveRotation,
    /// Whether the background usage fetch still polls spent (100%-capped)
    /// accounts (`AppState.refresh_spent_accounts`, default on). Off skips a
    /// spent account until its window resets — a fetch-leg optimization only,
    /// never a switch/fallback input. ENUMERATED on/off, ⏎ mirrors space.
    RefreshSpentAccounts,
}

/// Inline editor state for the Config detail pane. Built on entry, torn down
/// when focus returns to the account list.
#[derive(Debug, Clone)]
pub(crate) struct ConfigDraft {
    /// `None` while creating; `Some(name)` while editing. Existing accounts
    /// commit per-field on ⏎; new drafts buffer until the `create` row fires.
    pub(crate) editing_name: Option<String>,
    pub(crate) name: InputState,
    pub(crate) base_url: InputState,
    pub(crate) api_key: InputState,
    pub(crate) model: InputState,
    pub(crate) opus_model: InputState,
    pub(crate) sonnet_model: InputState,
    pub(crate) haiku_model: InputState,
    pub(crate) fable_model: InputState,
    pub(crate) subagent_model: InputState,
    /// Value buffer for the env entry currently being edited (seeded on entry).
    /// Shared across `EnvEntry` rows since only one is active at a time.
    pub(crate) env_value: InputState,
    /// Key buffer for the `+ add env` row's key editor.
    pub(crate) env_new_key: InputState,
    /// `Some(row)` while a text row (or the `model` custom field) owns the keyboard.
    pub(crate) active: Option<ConfigRow>,
    /// The account-action row (`Delete`, `ClearSessionToken`, or `Disabled` in
    /// the disable direction only — enabling is immediate, never armed)
    /// currently armed by a first ⏎; a second ⏎ on that same row confirms. Any
    /// cursor move, or the confirm itself, disarms back to `None`.
    pub(crate) armed_action: Option<ConfigRow>,
    /// API-account re-login is in flight: committing the base-url field advances
    /// to the api-key field (re-enter both, mirroring `login --base-url --api-key`)
    /// instead of ending the edit. Cleared on the api-key commit or any ⎋.
    pub(crate) relogin_chain: bool,
    /// `+ model override` reveal state. Draft-scoped: a fresh draft starts
    /// collapsed (set overrides still render; unset ones hide behind the chip).
    pub(crate) overrides_expanded: bool,
    /// A `+ new`-form browser login, held in memory until `create account`
    /// consumes it (capture-then-commit). Carries the probed account uuid
    /// alongside the mint so the anchor is seeded under the name the create
    /// actually commits — the draft's name is still editable until then. Dropped
    /// with the draft; never set on an existing account's draft.
    pub(crate) captured_login: Option<Box<crate::oauth_login::LoginOutcome>>,
}

impl ConfigDraft {
    /// The edit buffer behind a text, `Model`, or actively-edited env row; `None`
    /// for toggle/action rows. The env buffers resolve only while their row is the
    /// active one — an idle `EnvEntry` renders from the read-only snapshot instead.
    pub(crate) fn field(&self, row: ConfigRow) -> Option<&InputState> {
        Some(match row {
            ConfigRow::Name => &self.name,
            ConfigRow::BaseUrl => &self.base_url,
            ConfigRow::ApiKey => &self.api_key,
            ConfigRow::Model => &self.model,
            ConfigRow::OpusModel => &self.opus_model,
            ConfigRow::SonnetModel => &self.sonnet_model,
            ConfigRow::HaikuModel => &self.haiku_model,
            ConfigRow::FableModel => &self.fable_model,
            ConfigRow::SubagentModel => &self.subagent_model,
            ConfigRow::EnvEntry(_) if self.active == Some(row) => &self.env_value,
            ConfigRow::EnvAdd if self.active == Some(ConfigRow::EnvAdd) => &self.env_new_key,
            ConfigRow::AutoStart
            | ConfigRow::ModelOverrideAdd
            | ConfigRow::EnvEntry(_)
            | ConfigRow::EnvAdd
            | ConfigRow::Login
            | ConfigRow::DeleteCreds
            | ConfigRow::ClearSessionToken
            | ConfigRow::Disabled
            | ConfigRow::Delete
            | ConfigRow::Create => return None,
        })
    }

    pub(crate) fn field_mut(&mut self, row: ConfigRow) -> Option<&mut InputState> {
        Some(match row {
            ConfigRow::Name => &mut self.name,
            ConfigRow::BaseUrl => &mut self.base_url,
            ConfigRow::ApiKey => &mut self.api_key,
            ConfigRow::Model => &mut self.model,
            ConfigRow::OpusModel => &mut self.opus_model,
            ConfigRow::SonnetModel => &mut self.sonnet_model,
            ConfigRow::HaikuModel => &mut self.haiku_model,
            ConfigRow::FableModel => &mut self.fable_model,
            ConfigRow::SubagentModel => &mut self.subagent_model,
            ConfigRow::EnvEntry(_) if self.active == Some(row) => &mut self.env_value,
            ConfigRow::EnvAdd if self.active == Some(ConfigRow::EnvAdd) => &mut self.env_new_key,
            ConfigRow::AutoStart
            | ConfigRow::ModelOverrideAdd
            | ConfigRow::EnvEntry(_)
            | ConfigRow::EnvAdd
            | ConfigRow::Login
            | ConfigRow::DeleteCreds
            | ConfigRow::ClearSessionToken
            | ConfigRow::Disabled
            | ConfigRow::Delete
            | ConfigRow::Create => return None,
        })
    }
}

#[derive(Debug, Clone)]
pub(crate) struct ConfirmState {
    pub(crate) message: String,
    pub(crate) detail: Option<String>,
    /// Highlighted choice: false = cancel, true = confirm.
    pub(crate) choice: bool,
    pub(crate) on_confirm: ConfirmAction,
}

#[derive(Debug, Clone)]
pub(crate) enum ConfirmAction {
    /// `bool` = `from_divergence`, carried through for deferred-detach semantics.
    CaptureConflict(Box<CaptureSnapshot>, bool),
    /// Capture-name collision (issue #7): the typed name already belongs to
    /// another profile. `String` = that profile's existing (canonical-cased)
    /// name, `bool` = `from_divergence`, both carried through to
    /// `overwrite_captured_profile` the same way `CaptureConflict` carries them
    /// to `capture_into_profile`.
    CaptureOverwrite(Box<CaptureSnapshot>, String, bool),
    /// Divergence "save elsewhere" → a chosen (non-active) profile. `String` =
    /// that profile. Saves the live login into it and force-links it active —
    /// the guarded relink can't resolve a CC-written divergence (the on-disk
    /// byte formats differ), so this path forces the live link onto the target.
    AdoptDivergence(Box<CaptureSnapshot>, String),
    Switch(String),
    /// Confirm before discarding CC's freshly-written credentials and relinking.
    DiscardDivergence(String),
    /// Force-rotate all refresh tokens; active sessions may be logged out.
    RotateAll,
    /// Force-rotate one account's refresh token (action-menu "rotate access
    /// token" on the focused account).
    RotateOne(String),
    /// Disable one account (action-menu "disable account" off the Setup pane,
    /// standing in for the row's arm-then-confirm).
    DisableOne(String),
    /// Plugin tab: write the `mcpServers.clauth` entry into `~/.claude.json`.
    /// Reversible local write — non-destructive, so it keeps the plain button.
    WireMcpServers,
    /// Plugin tab: relink `~/.claude/.credentials.json` to the active profile's
    /// own stored credentials (repair a `missing` link). Spends no token — it only
    /// re-points at creds the profile already holds — so it keeps the plain button.
    RelinkCredentials(String),
    /// Setup tab: drop a profile's stored OAuth credentials, keeping the shell.
    BlankCredentials(String),
    /// Setup `+ new` draft: a login already stashed a mint (the `✓ logged in`
    /// row), so re-running would silently replace it. Confirm first, then
    /// re-dispatch `start_login`. `bool` = `is_new`, carried to the restart.
    RestartLogin(String, bool),
    /// Delete row on a profile with a live `clauth start` session: the unforced
    /// guard in `delete_profile` refuses this, so confirm the deauth risk here
    /// and re-run the delete with `force`.
    DeleteLiveSession(String),
    /// Fallback tab `+ add`: the candidate would mix api-key and oauth accounts
    /// in the chain. Confirm carries the add through; cancel returns to the
    /// picker. Non-destructive — the member can be removed after adding.
    AddChainCandidate(String),
    /// Setup menu `save as preset`: a custom preset already holds the typed
    /// name. `(preset name, source account)` — confirming re-saves over it.
    OverwritePreset(String, String),
    /// Setup menu `apply preset`: the target account already carries a base url
    /// or model settings the preset would replace. `(account, preset name)` —
    /// the preset is re-read at apply, so what lands is what is on disk now.
    ApplyPresetOver(String, String),
    /// Preset picker `d`: drop a custom preset from disk.
    DeletePreset(String),
    /// Info-only modal: an action the user asked for is refused for a reason
    /// they should read (rotating a macOS profile whose running session holds
    /// its login in a Keychain entry clauth cannot write). Confirming just
    /// dismisses; `run_confirm_action` does nothing.
    Acknowledge,
    /// Plugin tab: run `crate::herdr::heal` on the named config file.
    HealHerdrConfig(std::path::PathBuf),
    /// Plugin tab herdr options: flip the `delegate row text` knob, persist it,
    /// then heal herdr's config so the sidebar row matches the new knob. The
    /// heal is what the confirm gates — it rewrites herdr's own file.
    HerdrDelegateRowText(std::path::PathBuf),
    /// Plugin tab: install the clauth plugin through agentgear at user scope.
    /// A write into CC's plugin registry (driven via the `claude` CLI), so it
    /// keeps the confirm modal like every other mutating fix.
    InstallPlugin,
}

/// One-field name prompt shared by the Setup menu's two naming actions. The
/// variant carries what the typed name is FOR, so the modal itself stays a plain
/// input and every validation rule lives in the handler that consumes it.
#[derive(Debug, Clone)]
pub(crate) struct NamePromptForm {
    pub(crate) input: InputState,
    pub(crate) action: NamePromptAction,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum NamePromptAction {
    /// Copy this account onto a new one under the typed name.
    DuplicateProfile(String),
    /// Save this account's base url + models as a preset under the typed name.
    SavePreset(String),
}

impl NamePromptAction {
    /// Modal title, and the blurb under it — both read off the same variant so a
    /// new action can't ship with the other one's copy.
    pub(crate) fn title(&self) -> &'static str {
        match self {
            Self::DuplicateProfile(_) => "DUPLICATE",
            Self::SavePreset(_) => "SAVE PRESET",
        }
    }

    pub(crate) fn blurb(&self) -> String {
        match self {
            Self::DuplicateProfile(source) => {
                format!("copies every setting of '{source}' except its login.")
            }
            Self::SavePreset(source) => {
                format!("stores '{source}'s base url and model settings under this name.")
            }
        }
    }
}

/// Preset picker for `apply preset`. Built-ins lead the list (see
/// [`crate::presets::list_presets`]), so the cursor's index is the list's index
/// and nothing has to track the split.
#[derive(Debug, Clone)]
pub(crate) struct PresetPickerForm {
    /// The account the pick is stamped onto.
    pub(crate) target: String,
    pub(crate) presets: Vec<crate::presets::Preset>,
    pub(crate) cursor: usize,
}

#[derive(Debug, Clone)]
pub(crate) struct CaptureNameForm {
    pub(crate) snapshot: Box<CaptureSnapshot>,
    pub(crate) input: InputState,
    /// Set when initiated by `NewProfile` divergence. Detach + deactivate of
    /// the prior profile is deferred to the success arm so cancel leaves it intact.
    pub(crate) from_divergence: bool,
}

/// Credential-divergence prompt. Raised on demand (the <kbd>d</kbd> key from
/// the non-blocking banner) and when an action that REQUIRES resolution is
/// attempted (switch / switch-off / plugin fix) — never auto-pushed at startup
/// or from the 1Hz poll, so a divergence can't lock the whole TUI
/// (`divergence_pending` + the banner carry the signal instead).
#[derive(Debug, Clone)]
pub(crate) struct DivergenceForm {
    pub(crate) active: String,
    /// The profile the live login was identified as belonging to
    /// ([`crate::actions::identify_live_login_owner`], local evidence), when
    /// that profile is NOT the active one. Surfaces a first-class "switch to
    /// it" action — the near-always-right resolution for a CC re-login into a
    /// known account, which the generic three options all get wrong.
    pub(crate) sibling: Option<ProfileName>,
    pub(crate) cursor: usize,
}

/// The non-blocking divergence signal behind the accounts-pane banner: set by
/// the 1Hz poll / startup reconcile, cleared the moment the live link matches
/// again. <kbd>d</kbd> opens the resolver from it.
#[derive(Debug, Clone)]
pub(crate) struct DivergenceNotice {
    pub(crate) active: String,
    /// Locally identified owner of the live login when it is a NON-active
    /// profile (see [`DivergenceForm::sibling`]); drives the banner wording.
    pub(crate) sibling: Option<ProfileName>,
    /// SipHash of the live access token at identification time — the memo that
    /// keeps the 1Hz poll from re-running the owner lookup while the live login
    /// is unchanged.
    pub(crate) fingerprint: Option<u64>,
}

impl DivergenceNotice {
    /// Banner copy for the one system banner: names the live login's owner when
    /// known, else the generic mismatch, ending in the `d` affordance. Lowercase
    /// fragments, mid-dot separators (cloudy-tui banner copy).
    pub(crate) fn banner_message(&self) -> String {
        match &self.sibling {
            Some(owner) => format!(
                "live login is '{owner}' · not the active '{}' · press d to resolve",
                self.active
            ),
            None => format!(
                "live login no longer matches '{}' · press d to resolve",
                self.active
            ),
        }
    }
}

/// One row on the Divergence prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DivergenceAction {
    /// The live login belongs to this (non-active) profile: capture it there
    /// and make that profile active — the [`ConfirmAction::AdoptDivergence`]
    /// path the "save elsewhere" picker already uses, promoted to the top.
    SwitchToOwner(String),
    Choice(DivergenceChoice),
}

impl DivergenceForm {
    pub(crate) fn actions(&self) -> Vec<DivergenceAction> {
        let mut v = Vec::with_capacity(4);
        if let Some(owner) = &self.sibling {
            v.push(DivergenceAction::SwitchToOwner(owner.to_string()));
        }
        v.extend([
            DivergenceAction::Choice(DivergenceChoice::Overwrite),
            DivergenceAction::Choice(DivergenceChoice::NewProfile),
            DivergenceAction::Choice(DivergenceChoice::Discard),
        ]);
        v
    }
}

/// "Save the live login to another profile" picker, opened from the Divergence
/// modal's second action. Row 0 is `+ new profile`; rows 1.. are existing
/// profiles (the active/diverged one excluded). A refresh-token match is
/// pre-selected so re-logging an account you already hold is one keypress.
#[derive(Debug, Clone)]
pub(crate) struct DivergenceTargetForm {
    pub(crate) targets: Vec<String>,
    pub(crate) cursor: usize,
}

/// A choice on the env-key collision prompt (3 vertical options).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EnvCollisionChoice {
    /// Add the custom field anyway — its value overrides the colliding source.
    Overwrite,
    /// Leave the existing value untouched; don't add. Jumps to the existing
    /// custom field when the collision is with one.
    KeepExisting,
    /// Back out, no change.
    Cancel,
}

/// Prompt shown when a freshly typed custom env key already exists in one of the
/// three sources ([`EnvKeyCollision`]). Modeled on [`DivergenceForm`] — a message
/// plus arrow-selected options. `cancel` is the default-focused (safe) choice.
#[derive(Debug, Clone)]
pub(crate) struct EnvCollisionForm {
    pub(crate) profile: String,
    pub(crate) key: String,
    /// Human reason for the collision (`set by the base url field`, …).
    pub(crate) reason: String,
    /// Sorted `EnvEntry` index of the colliding own-field, for `keep existing`.
    pub(crate) existing_idx: Option<usize>,
    pub(crate) cursor: usize,
}

impl EnvCollisionForm {
    pub(crate) fn options() -> [EnvCollisionChoice; 3] {
        [
            EnvCollisionChoice::Overwrite,
            EnvCollisionChoice::KeepExisting,
            EnvCollisionChoice::Cancel,
        ]
    }
}

/// One entry in the action menu.
#[derive(Debug, Clone)]
pub(crate) struct ActionItem {
    pub(crate) label: &'static str,
    /// Single-letter shortcut auto-assigned by the hotkey algorithm; `None` when
    /// all candidate characters were already taken.
    pub(crate) hotkey: Option<char>,
    /// The logical action to fire when this item is selected.
    pub(crate) action: ActionMenuAction,
}

/// Logical actions the action menu can dispatch. Each variant maps 1-to-1 to
/// an existing key handler so there is no duplicated dispatch logic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ActionMenuAction {
    // Global
    NewAccount,
    RefreshUsage,
    /// Force-refresh every account — what the global `r` does off the Usage and
    /// Tokens tabs, and the only way to reach it from Usage (where `r` is the
    /// focused account's own refresh).
    RefreshAll,
    RotateTokens,
    /// The focused account's disabled flag, off the Setup pane (Overview and
    /// Usage, which carry no row for it).
    DisableProfile,
    EnableProfile,
    /// Open the focused account's provider console — where its api key is
    /// minted. Offered only for a recognised third-party endpoint, since that
    /// is the only case clauth knows a page for.
    OpenProviderConsole,
    // Setup tab — all three act on the focused account and none has a key.
    /// Copy every setting of the focused account onto a new one, credentials
    /// excluded. Prompts for the new name.
    Duplicate,
    /// Store the focused account's base url + model settings as a named
    /// [`crate::presets::Preset`]. Prompts for the name.
    SaveAsPreset,
    /// Stamp a preset's base url + model settings onto the focused account.
    /// Opens the picker.
    ApplyPreset,
    // Status tab
    RefreshStatus,
    OpenIncidentLink,
    // Usage
    ToggleEstimates,
    TogglePace,
    // Tokens
    TokensPeriodLifetime,
    TokensPeriodDaily,
    TokensPeriodWeekly,
    TokensPeriodMonthly,
    TokensShowAll,
    TokensShowClaude,
    TokensShowOthers,
    ToggleCountCache,
    ReloadTokenStats,
}

/// State for the action-menu modal.
#[derive(Debug, Clone)]
pub(crate) struct ActionMenuState {
    /// Account-scoped items first, then the tab-global ones. One flat list so
    /// the cursor and the hotkey scan stay index-free of the split; the render
    /// draws the group rule at `scoped_len`.
    pub(crate) items: Vec<ActionItem>,
    /// How many leading `items` act on [`ActionMenuState::context`].
    pub(crate) scoped_len: usize,
    /// The focused account, named in the title bar so the scoped items don't
    /// each need a suffix. `None` whenever the scoped group is empty — a name
    /// on a menu of tab-global actions would claim a scope that isn't there.
    pub(crate) context: Option<String>,
    pub(crate) cursor: usize,
}

impl ActionMenuState {
    /// Build and assign hotkeys in source order per the SKILL.md algorithm.
    /// Reserved keys: `a` `x` `?` `q`.
    pub(crate) fn new(
        scoped: Vec<ActionMenuAction>,
        global: Vec<ActionMenuAction>,
        context: Option<String>,
    ) -> Self {
        const RESERVED: &[char] = &['a', 'x', '?', 'q'];
        let scoped_len = scoped.len();
        let context = (scoped_len > 0).then_some(context).flatten();
        let mut claimed: Vec<char> = Vec::new();
        let items = scoped
            .into_iter()
            .chain(global)
            .map(|action| {
                let label = action.label();
                // An explicit override wins when free; else scan the first 3 alpha
                // chars of the label per the SKILL.md algorithm.
                let hotkey = action
                    .preferred_hotkey()
                    .filter(|c| !RESERVED.contains(c) && !claimed.contains(c))
                    .or_else(|| {
                        label
                            .chars()
                            .filter(|c| c.is_alphabetic())
                            .map(|c| c.to_lowercase().next().unwrap_or(c))
                            .take(3)
                            .find(|c| !RESERVED.contains(c) && !claimed.contains(c))
                    })
                    .inspect(|c| claimed.push(*c));
                ActionItem {
                    label,
                    hotkey,
                    action,
                }
            })
            .collect();
        Self {
            items,
            scoped_len,
            context,
            cursor: 0,
        }
    }
}

impl ActionMenuAction {
    /// Explicit hotkey override, taking priority over the label scan when free.
    /// `rotate tokens` pins `t` (mnemonic + matches the global rotate key) instead
    /// of falling to `o` after `refresh usage` claims `r`.
    fn preferred_hotkey(&self) -> Option<char> {
        match self {
            Self::RotateTokens => Some('t'),
            Self::ToggleEstimates => Some('e'),
            Self::TogglePace => Some('p'),
            // `r` belongs to the focused account's own refresh, and `e`/`p` to
            // the Usage page keys above, so refresh-all takes the third letter
            // of its label — pinned rather than scanned so it doesn't move with
            // the tab. (Uppercase `R` is out: hotkeys are case-insensitive.)
            Self::RefreshAll => Some('f'),
            // One letter across both halves of the disable/enable pair, so the
            // key doesn't move when the account's state flips.
            Self::DisableProfile | Self::EnableProfile => Some('d'),
            // Mirror the Tokens tab's page keys so the menu teaches them.
            Self::ToggleCountCache => Some('c'),
            Self::ReloadTokenStats => Some('r'),
            // Period rows all start with "period: ", so pin each to its
            // distinguishing word instead of the label scan.
            Self::TokensPeriodLifetime => Some('l'),
            Self::TokensPeriodDaily => Some('d'),
            Self::TokensPeriodWeekly => Some('w'),
            Self::TokensPeriodMonthly => Some('m'),
            _ => None,
        }
    }

    pub(crate) fn label(&self) -> &'static str {
        match self {
            Self::NewAccount => "new account",
            Self::RefreshUsage => "refresh usage",
            Self::RefreshAll => "refresh all accounts",
            Self::RotateTokens => "rotate access token",
            Self::DisableProfile => "disable account",
            Self::EnableProfile => "enable account",
            Self::OpenProviderConsole => "open provider console",
            Self::Duplicate => "duplicate account",
            Self::SaveAsPreset => "save as preset",
            Self::ApplyPreset => "apply preset",
            Self::RefreshStatus => "refresh status",
            Self::OpenIncidentLink => "open in browser",
            Self::ToggleEstimates => "toggle estimates",
            Self::TogglePace => "toggle pace marker",
            Self::TokensPeriodLifetime => "period: lifetime",
            Self::TokensPeriodDaily => "period: daily",
            Self::TokensPeriodWeekly => "period: weekly",
            Self::TokensPeriodMonthly => "period: monthly",
            Self::TokensShowAll => "show all models",
            Self::TokensShowClaude => "show claude models",
            Self::TokensShowOthers => "show other models",
            Self::ToggleCountCache => "toggle cache counting",
            Self::ReloadTokenStats => "reload stats",
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) enum Modal {
    Confirm(ConfirmState),
    /// Credential divergence prompt.
    Divergence(DivergenceForm),
    CaptureName(CaptureNameForm),
    /// One-field name prompt behind the Setup menu's `duplicate account` and
    /// `save as preset`.
    NamePrompt(NamePromptForm),
    /// Setup menu `apply preset`: pick which preset stamps the account.
    PresetPicker(PresetPickerForm),
    /// Divergence "save elsewhere": pick which profile the live login lands in.
    DivergenceTarget(DivergenceTargetForm),
    Help,
    /// Context-sensitive action menu opened by `a`.
    ActionMenu(ActionMenuState),
    /// Custom env key collides with an existing source; overwrite/keep/cancel.
    EnvCollision(EnvCollisionForm),
    /// In-flight browser login progress; renders live from [`App::login`].
    /// esc/q collapse it to the footer indicator — the login keeps running.
    Login,
}

// ── Toasts ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ToastKind {
    Info,
    Success,
    Warning,
    Danger,
}

#[derive(Debug, Clone)]
pub(crate) struct Toast {
    pub(crate) kind: ToastKind,
    pub(crate) body: String,
    pub(crate) born: Instant,
}

const ROTATE_ALL_MSG: &str = "rotate all access tokens?";
/// macOS refuses to rotate an account with a running session (it can't reach the
/// credential that session reads), so the two hosts promise different things.
#[cfg(target_os = "macos")]
const ROTATE_ALL_DETAIL: &str = "accounts with a live clauth start session are skipped.";
#[cfg(not(target_os = "macos"))]
const ROTATE_ALL_DETAIL: &str = "running sessions pick up the new tokens on their next request.";

/// The macOS-only single-rotate refusal, in both the shapes it is surfaced in:
/// an acknowledge modal from the action menu, and a toast if a session starts
/// between arming the confirm and running it.
///
/// Defined on every platform though only macOS reads them (the branch is
/// `cfg!`, so it still compiles here) — that keeps the strings reviewable and
/// exact-match pinnable from a Linux run.
const ROTATE_LIVE_SESSION_MSG: &str = "has a live clauth start session";
const ROTATE_LIVE_SESSION_DETAIL: &str = "macos keeps its login in a keychain entry clauth can't write, so rotating would sign the \
     session out.";
const ROTATE_LIVE_SESSION_TOAST: &str = "macos keeps its login where clauth can't rotate it";

/// What disabling costs, for the action menu's confirm. The Setup row states the
/// same thing in its own hint (`config::row_hint`) before arming.
const DISABLE_DETAIL: &str =
    "it drops out of auto-switch, usage polling, and status until re-enabled.";
const TOAST_CAPACITY: usize = 3;
const TOAST_TTL_NORMAL: Duration = Duration::from_secs(3);
const TOAST_TTL_DANGER: Duration = Duration::from_secs(6);

// ── Tabs ──────────────────────────────────────────────────────────────────────

/// Top-level views (⇥/⇧⇥/←→). All tabs share background workers; only the
/// body and keymap differ.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Tab {
    /// Accounts table + at-a-glance usage.
    Overview,
    /// Per-account usage breakdown.
    Usage,
    /// Global token usage across past Claude Code sessions (from `~/.claude`).
    Tokens,
    /// Per-account settings (endpoint, rename, auto-start, delete).
    Setup,
    /// Fallback chain editor — ordering and per-member thresholds.
    Fallback,
    /// Program-wide settings: theme tier and global defaults.
    Config,
    /// Claude service status feed (incidents from status.claude.com).
    Status,
    /// Claude Code integration health: MCP wiring, plugin install, per-profile runtime.
    Plugin,
}

impl Tab {
    pub(crate) const ALL: [Tab; 8] = [
        Tab::Overview,
        Tab::Usage,
        Tab::Tokens,
        Tab::Setup,
        Tab::Fallback,
        Tab::Config,
        Tab::Status,
        Tab::Plugin,
    ];

    pub(crate) fn title(self) -> &'static str {
        match self {
            Tab::Overview => "Overview",
            Tab::Usage => "Usage",
            Tab::Tokens => "Tokens",
            Tab::Setup => "Setup",
            Tab::Fallback => "Fallback",
            Tab::Config => "Config",
            Tab::Status => "Status",
            Tab::Plugin => "Plugin",
        }
    }

    pub(crate) fn index(self) -> usize {
        Tab::ALL.iter().position(|t| *t == self).unwrap_or(0)
    }

    pub(crate) fn next(self) -> Tab {
        Tab::ALL[(self.index() + 1) % Tab::ALL.len()]
    }

    pub(crate) fn prev(self) -> Tab {
        Tab::ALL[(self.index() + Tab::ALL.len() - 1) % Tab::ALL.len()]
    }
}

/// Which view the Tokens tab shows. `Dashboard` is the landing page (totals +
/// charts); `Models` is the descend-into per-model master-detail breakdown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TokenView {
    Dashboard,
    Models,
}

/// Display filter over the Tokens tab's grouped model list (top-models card +
/// the Models view), set from the tab's action menu. Session-only — a lens,
/// not a setting. The aggregate cards (today/total/daily/…) stay unfiltered:
/// the daily trend has no per-model split, so filtering only some cards would
/// let "today" contradict the trend next to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum TokenFilter {
    #[default]
    All,
    Claude,
    Others,
}

impl TokenFilter {
    pub(crate) fn matches(self, model: &str) -> bool {
        match self {
            TokenFilter::All => true,
            TokenFilter::Claude => crate::tokens::is_anthropic(model),
            TokenFilter::Others => !crate::tokens::is_anthropic(model),
        }
    }

    /// Title-right meta badge for the filtered model surfaces; `None` when off.
    pub(crate) fn badge(self) -> Option<&'static str> {
        match self {
            TokenFilter::All => None,
            TokenFilter::Claude => Some("claude only"),
            TokenFilter::Others => Some("others only"),
        }
    }
}

/// Time-window lens over the Tokens tab, cycled with `t` or set from the
/// action menu. Session-only — a lens, not a setting, like [`TokenFilter`].
/// `Lifetime` (the default) is the untouched all-time dashboard; the other
/// three scope the cards to today / this calendar week / this calendar month.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum TokenPeriod {
    #[default]
    Lifetime,
    Daily,
    Weekly,
    Monthly,
}

impl TokenPeriod {
    /// Cycle order for the `t` key, wrapping.
    pub(crate) fn next(self) -> Self {
        match self {
            TokenPeriod::Lifetime => TokenPeriod::Daily,
            TokenPeriod::Daily => TokenPeriod::Weekly,
            TokenPeriod::Weekly => TokenPeriod::Monthly,
            TokenPeriod::Monthly => TokenPeriod::Lifetime,
        }
    }

    /// Title-right meta badge naming the scoped window; `None` when lifetime.
    pub(crate) fn badge(self) -> Option<&'static str> {
        match self {
            TokenPeriod::Lifetime => None,
            TokenPeriod::Daily => Some("today"),
            TokenPeriod::Weekly => Some("this week"),
            TokenPeriod::Monthly => Some("this month"),
        }
    }

    /// Like [`Self::badge`] but always names the lens — `lifetime` included —
    /// so the lens-bearing surfaces never render an empty badge slot (a badge
    /// that only sometimes appears reads as an anomaly, not a lens).
    pub(crate) fn lens_badge(self) -> &'static str {
        self.badge().unwrap_or("lifetime")
    }

    /// Calendar bucket for the trend cards; `None` = per-day rows.
    pub(crate) fn bucket(self) -> Option<crate::tokens::Bucket> {
        match self {
            TokenPeriod::Weekly => Some(crate::tokens::Bucket::Week),
            TokenPeriod::Monthly => Some(crate::tokens::Bucket::Month),
            TokenPeriod::Lifetime | TokenPeriod::Daily => None,
        }
    }
}

/// Which Config pane owns the cursor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConfigFocus {
    Profiles,
    Actions,
}

/// Which Fallback pane has focus. `Chain`: ordered list (↑↓, ⇧↑↓ reorders, ⏎
/// enters). `Detail`: threshold stepper + remove row, or add-candidate picker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FallbackFocus {
    Chain,
    Detail,
}

// ── Status tab ─────────────────────────────────────────────────────────────────

/// Which Status pane has focus. `List`: the incident selector (↑↓ + ⏎ descends).
/// `Detail`: the selected incident's timeline (↑↓ scrolls).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StatusFocus {
    List,
    Detail,
}

/// UI-thread-only state for the Status tab. Fed by [`StatusEvent`]s drained in
/// `on_tick`; never shared with the background thread (which only sends events).
#[derive(Debug)]
pub(crate) struct StatusState {
    /// Incidents in feed order (newest first).
    pub(crate) incidents: Vec<Incident>,
    /// Wall-clock ms of the last successful fetch / cache load; `None` until any
    /// event arrives. Drives the "cached / stale" age cue.
    pub(crate) fetched_at_ms: Option<u64>,
    /// True when the last event was `Cached` (startup cache or fetch-failed
    /// fallback) — render the data as cached rather than fresh.
    pub(crate) cached: bool,
    /// Set only when a fetch failed with nothing cached to show.
    pub(crate) error: Option<String>,
    /// A manual refresh is in flight → the panel title shows a spinner.
    pub(crate) fetching: bool,
    /// Selected incident index in `incidents`.
    pub(crate) cursor: usize,
    pub(crate) focus: StatusFocus,
    /// Scroll offset (lines) into the detail timeline.
    pub(crate) detail_scroll: u16,
    /// Max valid `detail_scroll` from the last detail render (`total - viewport`).
    /// The render pass owns the real geometry, so it writes the bound here (via
    /// `&App` interior mutability) and the key handler clamps against it — that
    /// stops a held ↓ from inflating `detail_scroll` toward `u16::MAX`.
    pub(crate) detail_max_scroll: std::cell::Cell<u16>,
    /// Newest incident id already signalled, so a refresh only toasts genuinely
    /// new incidents (not the initial load).
    pub(crate) seen_latest: Option<String>,
}

impl Default for StatusState {
    fn default() -> Self {
        Self {
            incidents: Vec::new(),
            fetched_at_ms: None,
            cached: false,
            error: None,
            fetching: false,
            cursor: 0,
            focus: StatusFocus::List,
            detail_scroll: 0,
            detail_max_scroll: std::cell::Cell::new(0),
            seen_latest: None,
        }
    }
}

impl StatusState {
    /// The incident currently under the cursor, if any.
    pub(crate) fn selected(&self) -> Option<&Incident> {
        self.incidents.get(self.cursor)
    }

    /// Worst impact among active incidents: critical > major > minor > maintenance.
    /// Returns [`crate::status::Impact::None`] when nothing is active.
    pub(crate) fn worst_active_impact(&self) -> crate::status::Impact {
        self.incidents
            .iter()
            .filter(|i| i.is_active())
            .map(|i| &i.impact)
            .max_by_key(|i| i.severity())
            .cloned()
            .unwrap_or(crate::status::Impact::None)
    }
}

/// An incident is active when its lifecycle status is not terminal
/// (`resolved` / `completed`). Thin wrapper over [`Incident::is_active`] for the
/// render layer.
pub(crate) fn incident_is_active(incident: &Incident) -> bool {
    incident.is_active()
}

// ── Plugin tab ─────────────────────────────────────────────────────────────────

/// Which Plugin pane has focus. `List`: the checks + profiles selector (↑↓ moves,
/// ⏎ descends, `f` fixes). `Detail`: the selected row's readout (↑↓ scrolls).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PluginFocus {
    List,
    Detail,
}

/// Health bucket for a row's status dot — the same success / warning / danger
/// buckets as the header `● status.claude.ai` dot, plus a neutral `Idle` for a
/// profile that is neither linked nor running a live session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Health {
    Ok,
    Warn,
    Danger,
    Idle,
}

/// A one-key fix offered on the selected row. `WireMcpServers` writes the manual
/// entry (a [`ConfirmAction`]); `RepairDivergence` re-raises the existing
/// divergence resolver for the named (active) profile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PluginFix {
    WireMcpServers,
    RepairDivergence(String),
    /// Relink a `missing` active-profile credential link to its own stored creds.
    RelinkCredentials(String),
    /// Append the keybinding + sidebar row to herdr's config (the config half of
    /// `clauth herdr install`). `PathBuf` = the resolved config file.
    HealHerdrConfig(std::path::PathBuf),
    /// Install the clauth plugin through agentgear (user scope, embedded tree).
    /// Replaces the copy-paste `/plugin` hint the row used to show.
    InstallPlugin,
}

/// A computed integration-check row (global, profile-independent).
#[derive(Debug, Clone)]
pub(crate) struct Check {
    pub(crate) label: &'static str,
    pub(crate) health: Health,
    /// Full readout for the detail pane, one entry per line. (Checks are
    /// dot-only in the list — the dot color carries the verdict, the readout
    /// lives here — so there is no separate terse value.)
    pub(crate) detail: Vec<String>,
    pub(crate) fix: Option<PluginFix>,
}

/// UI-thread-only state for the Plugin tab. Recomputed synchronously on tab focus
/// and on `r`; there is no background thread (all reads are local FS/`PATH`;
/// `claude --version` is one cached subprocess gated by [`PluginState::cc_version`]).
#[derive(Debug)]
pub(crate) struct PluginState {
    pub(crate) focus: PluginFocus,
    /// Cursor over the integration checks (`0..checks.len()`).
    pub(crate) cursor: usize,
    pub(crate) detail_scroll: u16,
    /// Max valid `detail_scroll` from the last render (`&App` interior mutability,
    /// clamped by the key handler — same pattern as `StatusState`).
    pub(crate) detail_max_scroll: std::cell::Cell<u16>,
    /// True while a `claude --version` probe is in flight (title spinner). The
    /// probe is synchronous, so this is mostly belt-and-suspenders for the spec.
    pub(crate) fetching: bool,
    pub(crate) error: Option<String>,
    pub(crate) checks: Vec<Check>,
    /// Cached `claude --version`: `None` = unprobed, `Some(None)` = probed and
    /// missing/unparseable, `Some(Some(v))` = the version string. Probed only
    /// on an explicit `r` — construction and a tab switch never spawn the
    /// subprocess, so the first paint never blocks on it.
    pub(crate) cc_version: Option<Option<String>>,
    /// Cached `clauth mcp` discovery handshake: `None` = unprobed. Re-probed only
    /// on `r` (heavier than the others — it boots the real server), never on a tab
    /// switch or the per-tick refresh.
    pub(crate) mcp_boot: Option<crate::plugin_probe::McpProbe>,
    /// Cached herdr probe: `None` = unprobed, `Some(None)` = herdr does not
    /// resolve (no row), `Some(Some(p))` = the probe. Probed at construction in
    /// herdr mode (`HERDR_ENV=1` proves herdr is present) and re-probed on `r`;
    /// it spawns three subprocesses, so a tab switch and the per-tick refresh
    /// reuse the cached value.
    pub(crate) herdr: Option<Option<crate::herdr::HerdrProbe>>,
    /// The `clauth mcp` job store as of the last refresh, newest first: what the
    /// delegates pane draws. Re-read on the same cadence as the checks, because
    /// the server writing it is a DIFFERENT process, so there is nothing to
    /// subscribe to and no event to wait for. Read-only: the TUI never writes
    /// this store, and never sweeps it.
    ///
    /// The cost is one readdir plus a parse per file. No count cap bounds that
    /// from here: retention is the two TTLs (`jobs::DONE_TTL_MS` /
    /// `jobs::RUNNING_TTL_MS`), applied only by the startup sweep inside a
    /// `clauth mcp` process, which the TUI neither runs nor depends on having
    /// run, and the count spans both record spellings. What the directory
    /// really holds is whatever the last such startup expired, plus every job
    /// written since. Measured against the 1 s tick with 256 records over
    /// ~5 MB of envelopes, best of 5 warm: **~3 ms release, ~20 ms debug**.
    /// Written as a bound rather than to two figures on purpose — two
    /// independent runs on this box landed 2.2 ms and 3.2 ms release, neither on
    /// a quiet machine, so a point figure would read as stale to the next person
    /// who re-derives it and misses by half. What binds is the order: single-
    /// digit ms in release, roughly ten times that in debug.
    pub(crate) delegates: Vec<crate::mcp::jobs::StoredJob>,
    /// Cursor over the herdr detail's options rows ([`HERDR_OPTIONS`]). The
    /// herdr detail is the one detail pane that takes per-row focus: while it
    /// is descended, ↑↓ walks these rows instead of scrolling the prose.
    pub(crate) herdr_options_cursor: usize,
    /// Typed editor for the `tag refresh` stepper — the Config-tab
    /// refresh-interval editor's shape. `None` = not editing.
    pub(crate) herdr_tag_draft: Option<InputState>,
    /// The herdr config verdict from the last recompute (a cheap file re-read
    /// that rides the per-tick refresh), cached so the render and the key
    /// handler never re-read the file per frame. `None` = unreadable or no
    /// config path. The options section reads it to decide whether the
    /// `delegate row text` row can write.
    pub(crate) herdr_config: Option<crate::herdr::ConfigStatus>,
}

/// Selector index the `herdr` check occupies once it renders: `about`,
/// `mcp servers`, `plugin`, `herdr`, `runtime`. The landing row in herdr mode;
/// when the construction probe does not resolve herdr, the same index rests
/// on `runtime`, the last row — the cursor clamp in
/// `recompute_plugin_checks` keeps it valid either way.
const HERDR_SELECTOR_ROW: usize = 3;

impl Default for PluginState {
    fn default() -> Self {
        Self {
            focus: PluginFocus::List,
            cursor: 0,
            detail_scroll: 0,
            detail_max_scroll: std::cell::Cell::new(0),
            fetching: false,
            error: None,
            checks: Vec::new(),
            cc_version: None,
            mcp_boot: None,
            herdr: None,
            delegates: Vec::new(),
            herdr_options_cursor: 0,
            herdr_tag_draft: None,
            herdr_config: None,
        }
    }
}

impl PluginState {
    /// Total selectable rows (the integration checks).
    pub(crate) fn row_count(&self) -> usize {
        self.checks.len()
    }

    /// The check under the cursor.
    pub(crate) fn selected_check(&self) -> Option<&Check> {
        self.checks.get(self.cursor)
    }

    /// The fix offered by the row under the cursor, if any.
    pub(crate) fn selected_fix(&self) -> Option<&PluginFix> {
        self.selected_check().and_then(|check| check.fix.as_ref())
    }
}

/// One editable row in the herdr detail's options section, in tab order. The
/// values live in `AppState.herdr` (profiles.toml) and persist immediately on
/// every change — there is no separate save step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HerdrOption {
    PopupWidth,
    PaneTag,
    TagRefresh,
    BorderLabel,
    DelegateDot,
    DelegateRowText,
}

/// The options section's row order — also the ↑↓ walk order.
pub(crate) const HERDR_OPTIONS: [HerdrOption; 6] = [
    HerdrOption::PopupWidth,
    HerdrOption::PaneTag,
    HerdrOption::TagRefresh,
    HerdrOption::BorderLabel,
    HerdrOption::DelegateDot,
    HerdrOption::DelegateRowText,
];

impl HerdrOption {
    /// The form-row label, lowercase per the contract.
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::PopupWidth => "popup width",
            Self::PaneTag => "pane tag",
            Self::TagRefresh => "tag refresh",
            Self::BorderLabel => "border label",
            Self::DelegateDot => "delegate dot",
            Self::DelegateRowText => "delegate row text",
        }
    }
}

// ── Footer alert ─────────────────────────────────────────────────────────────

/// A transient message that replaces the hint bar in place until dismissed.
/// `x` clears it (after all toasts are gone — see dismissal precedence).
/// Add variants as new conditions arise; keep one active at a time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum FooterAlert {
    /// `! <message>` in `WARNING` color — user must act or acknowledge.
    Warn(String),
}

// ── Banner ────────────────────────────────────────────────────────────────────

/// Severity for the full-width body banner. Ordering matters: `Danger`
/// outranks `Warning`, so when two conditions could both be live the
/// higher-severity banner wins (see `update_banner`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BannerSeverity {
    Warning,
    Danger,
}

/// A sticky system-wide condition rendered as a full-width row at the top of
/// the body (below the header, above the screen content). Computed from app
/// state each frame; not stored as a notification and not user-dismissable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Banner {
    pub(crate) severity: BannerSeverity,
    pub(crate) message: String,
}

// ── Overview list items ───────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy)]
pub(crate) enum MainItemKind {
    Profile(usize),
}

// ── Login session ─────────────────────────────────────────────────────────────

/// An in-flight browser OAuth login. The worker blocks up to 180s in
/// `oauth_login::login_with`; the UI stays live and applies the result in
/// `on_tick`. `generation` discards a stale result from a login the user
/// superseded (esc-cancel or a fresh login start).
pub(crate) struct LoginSession {
    pub(crate) name: String,
    /// true → the mint lands in the `+ new` draft (capture-then-commit);
    /// false → re-login an existing profile in place (overwrite).
    pub(crate) is_new: bool,
    pub(crate) generation: u64,
    /// The authorize URL once the worker announces it; shown in the login modal.
    pub(crate) url: Option<String>,
    /// Live milestone for the modal's stage line.
    pub(crate) stage: LoginStage,
}

/// Where an in-flight login currently sits, mapped from
/// [`crate::oauth_login::LoginProgress`] worker events.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LoginStage {
    /// Waiting for the user to finish the browser round-trip.
    WaitingBrowser,
    /// Callback landed; exchanging the code for tokens.
    ExchangingCode,
    /// Tokens minted; verifying them against the API.
    Verifying,
}

/// Worker→UI login channel payload: the announced URL or a stage bump.
pub(crate) enum LoginEvent {
    Url(String),
    Stage(LoginStage),
}

/// What a finished login worker produced. Both flows are a browser round-trip
/// announced through the same [`LoginEvent`] channel and drawn by the same
/// modal, and they diverge only at apply time: an Anthropic login replaces the
/// profile's credentials, an Alibaba console login replaces its usage session
/// and touches nothing else.
///
/// The drain routes on this payload rather than on anything recorded in
/// [`LoginSession`], so a session and its result cannot disagree about which
/// flow ran.
pub(crate) enum LoginResult {
    Oauth(Box<crate::oauth_login::LoginOutcome>),
    Console(Box<crate::alibaba_login::ConsoleLoginOutcome>),
}

// ── App ───────────────────────────────────────────────────────────────────────

pub(crate) struct App {
    /// Shared config; locked by the UI thread and the background refresher.
    /// Release before HTTP to avoid stalling the UI.
    pub(crate) config: ConfigHandle,

    pub(crate) usage_store: UsageStore,
    pub(crate) usage_status: StatusStore,
    pub(crate) usage_tokens: TokenList,
    pub(crate) next_refresh_per_profile: NextRefreshPerProfile,
    /// Per-profile activity state; read by the render loop for spinners, written
    /// by workers. Never held across HTTP.
    pub(crate) activity: ActivityStore,
    /// Drained in `on_tick`; each result clears the activity slot and surfaces errors.
    pub(crate) op_results: OpResultReceiver,
    /// Sender side; cloned into workers so they can report without holding a lock.
    pub(crate) op_sender: OpResultSender,
    /// Off-thread AUTH-1 switch-gate answers; drained in `on_tick` by
    /// `drain_switch_gates`, which completes or refuses the pending switch.
    pub(crate) switch_gates: std::sync::mpsc::Receiver<SwitchGateResult>,
    /// Sender side; cloned into each switch-gate worker.
    pub(crate) switch_gate_tx: std::sync::mpsc::Sender<SwitchGateResult>,
    /// Startup signals from reconcile/bootstrap workers; drained in `on_tick`.
    pub(crate) startup_results: StartupReceiver,
    /// Sender side for startup workers.
    pub(crate) startup_sender: StartupSender,
    pub(crate) last_fetched: LastFetchedAt,
    /// Per-profile consecutive-failure counters (429s + transient refresh
    /// failures) driving exponential usage-fetch backoff.
    pub(crate) poll_streaks: PollStreaks,
    /// Per-profile kick-429 blocks (the messages endpoint rejecting the 5h
    /// auto-start kick) — feeds the usage tab's blocked pill.
    pub(crate) kick_blocks: KickBlocks,
    /// Scheduler-posted auto-switch decisions; drained in `on_tick`.
    pub(crate) pending_switch: PendingSwitch,
    /// Set by the scheduler when the whole chain is spent; `on_tick` drains it
    /// and turns off all accounts.
    pub(crate) pending_switch_off: PendingSwitchOff,
    pub(crate) refetch_queue: RefetchQueue,

    pub(crate) third_party_tokens: ThirdPartyList,
    pub(crate) third_party_usage_store: ThirdPartyUsageStore,
    pub(crate) third_party_status: ThirdPartyStatusStore,
    pub(crate) tab: Tab,
    /// Running inside a herdr pane (`HERDR_ENV=1` at `cmd_tui`): the header
    /// carries a `[ herdr ]` tag and the TUI lands on the Plugin tab's herdr
    /// row. Read-only after construction — the mode is decided once at launch.
    pub(crate) herdr_mode: bool,
    pub(crate) modals: Vec<Modal>,
    /// First help-modal row on screen (↑↓ scrolls). Reset when the modal opens.
    pub(crate) help_scroll: u16,
    /// Max valid `help_scroll` from the last help render (`total - viewport`).
    /// Interior mutability because the render pass takes `&App`; clamping there
    /// stops a held ↓ from inflating `help_scroll` past the end.
    pub(crate) help_max_scroll: std::cell::Cell<u16>,

    /// Selected account index, shared across Overview/Usage/Setup tabs.
    /// On Setup may also rest on the trailing `+ new` row (== profile_count).
    pub(crate) profile_cursor: usize,
    /// Which Setup pane has focus.
    pub(crate) config_focus: ConfigFocus,
    /// Cursor into the detail rows on the Setup tab's right pane.
    pub(crate) config_action_cursor: usize,
    /// Inline editor for the Config detail pane; `Some` only while Actions has focus.
    pub(crate) config_draft: Option<ConfigDraft>,
    /// Cursor into `chain_items()` on the Fallback left pane.
    pub(crate) chain_cursor: usize,
    /// Which Fallback pane has focus.
    pub(crate) fallback_focus: FallbackFocus,
    /// Cursor into the Fallback right pane (member rows or add-candidate list).
    pub(crate) fallback_detail_cursor: usize,
    /// First ⏎ on remove arms it; second confirms. Cursor move or focus change disarms.
    pub(crate) fallback_armed_remove: bool,
    /// `Some` while the threshold field is open (⏎ opens, owns keyboard).
    /// `+`/`-` still step the value when `None`.
    pub(crate) fallback_threshold_draft: Option<InputState>,
    /// In-flight value for the member's `max auto-spend` field (`None` = not
    /// editing). Same lifecycle as `fallback_threshold_draft`.
    pub(crate) fallback_max_spend_draft: Option<InputState>,
    /// The `weekly at` override editor's buffer, or None (not editing). Same
    /// lifecycle as `fallback_threshold_draft`; an EMPTY commit clears the
    /// member's override.
    pub(crate) fallback_weekly_draft: Option<InputState>,
    /// Cursor into [`GLOBAL_CONFIG_ROWS`] on the program-wide Config tab.
    pub(crate) global_config_cursor: usize,
    /// `Some` while the refresh-interval custom-value field is open (⏎ opens,
    /// owns keyboard). Space/`+`/`-` still cycle the presets when `None`.
    pub(crate) refresh_interval_draft: Option<InputState>,
    /// In-flight custom value for the Config tab's weekly-threshold editor
    /// (`None` = not editing). Same lifecycle as `refresh_interval_draft`.
    pub(crate) weekly_threshold_draft: Option<InputState>,

    pub(crate) toasts: VecDeque<Toast>,
    /// Whether the terminal is currently too short for the normal layout (< 14 rows).
    /// Tracked across frames so the "too small" toast fires only on the transition in.
    pub(crate) compact: bool,
    /// Startup update check result; drained in `on_tick`. Silent on errors.
    pub(crate) update_results: std::sync::mpsc::Receiver<UpdateEvent>,
    /// Join handle for the update check thread; joined on TUI exit for clean shutdown.
    pub(crate) update_handle: Option<JoinHandle<()>>,

    /// In-flight browser OAuth login (Setup tab); `None` when idle.
    pub(crate) login: Option<LoginSession>,
    /// Monotonic login id; bumped on each start so a superseded worker's result
    /// is discarded when it lands.
    pub(crate) login_generation: u64,
    pub(crate) login_event_rx: std::sync::mpsc::Receiver<(u64, LoginEvent)>,
    pub(crate) login_event_tx: std::sync::mpsc::Sender<(u64, LoginEvent)>,
    pub(crate) login_result_rx:
        std::sync::mpsc::Receiver<(u64, std::result::Result<LoginResult, String>)>,
    pub(crate) login_result_tx:
        std::sync::mpsc::Sender<(u64, std::result::Result<LoginResult, String>)>,

    /// Claude status feed state; UI-thread-only (no shared lock).
    pub(crate) status: StatusState,
    /// Status feed events from the background thread; drained in `on_tick`.
    pub(crate) status_events: std::sync::mpsc::Receiver<StatusEvent>,
    /// Manual-refresh signal to the status thread; a `()` triggers a refetch.
    pub(crate) status_refresh: std::sync::mpsc::Sender<()>,

    /// `● daemon` header-dot state: daemon presence + `status.json` health,
    /// re-probed on a throttled cadence in `on_tick`. UI-thread-only.
    pub(crate) daemon_health: crate::daemon::DaemonHealth,
    /// Throttle for the `daemon_health` flock probe; construct probes once
    /// itself, so this starts stamped over a real answer. Seeding a placeholder
    /// instead would render `Absent` — "no daemon runs" — as fact for the whole
    /// first interval, and the first paint happens before any `on_tick`.
    pub(crate) last_daemon_probe: Instant,
    /// Single-fetcher lease (#27), shared with the scheduler tick. The bootstrap
    /// switch one-shot runs only if THIS instance holds it.
    pub(crate) fetch_lease: Arc<crate::daemon::FetchLease>,

    /// Plugin tab state; UI-thread-only, recomputed on focus + `r` (no thread).
    pub(crate) plugin: PluginState,

    /// Global token-usage stats read from `~/.claude` (stats-cache + recent
    /// transcript top-up); `None` until the loader posts its first result.
    pub(crate) token_stats: Option<crate::tokens::TokenStats>,
    /// Set when the loader reported a failure and no stats are cached — the tab
    /// shows an error instead of a perpetual "parsing stats-cache.json".
    pub(crate) tokens_failed: bool,
    /// True while the JSONL transcript top-up is known to be in flight: set when
    /// a `Base` first seeds the tab or on a manual reload, cleared on the next
    /// `Loaded`/`Failed`. Silent periodic refreshes never light it (their `Base`
    /// hits an already-populated tab). Drives the loading spinners on the tab.
    pub(crate) tokens_topping_up: bool,
    /// Latest `(done, total)` transcript-sweep count from the loader; rendered
    /// only while `tokens_topping_up` (silent periodic sweeps also emit it),
    /// cleared on `Loaded`/`Failed` and on a manual reload.
    pub(crate) tokens_progress: Option<(usize, usize)>,
    /// Which view the Tokens tab is showing (Dashboard landing vs Models detail).
    pub(crate) token_view: TokenView,
    /// Cursor into the grouped model list on the Tokens `Models` view.
    pub(crate) token_model_cursor: usize,
    /// Model filter over the Tokens tab's model surfaces (action-menu driven).
    pub(crate) token_filter: TokenFilter,
    pub(crate) token_period: TokenPeriod,
    /// Token-stats load results from the background loader; drained in `on_tick`.
    pub(crate) tokens_events: std::sync::mpsc::Receiver<crate::tokens::TokensEvent>,
    /// Manual-refresh signal to the token loader; a `()` triggers a reload.
    pub(crate) tokens_refresh: std::sync::mpsc::Sender<()>,

    /// Model price table for the Tokens tab's API-equivalent cost lens; `None`
    /// until the pricing loader posts a result (and `—` is shown meanwhile).
    pub(crate) price_table: Option<crate::pricing::PriceTable>,
    /// Set when the pricing loader reported a failure and no table is cached —
    /// the cost lens reads `rates unavailable` instead of a perpetual
    /// `rates loading`.
    pub(crate) price_failed: bool,
    /// Pricing load results from the background loader; drained in `on_tick`.
    pub(crate) pricing_events: std::sync::mpsc::Receiver<crate::pricing::PricingEvent>,
    /// Manual-refresh signal to the pricing loader; a `()` triggers a refetch.
    pub(crate) pricing_refresh: std::sync::mpsc::Sender<()>,

    pub(crate) last_reload_fp: ReloadFingerprint,
    /// Origin of the ambient-animation phase clock; read through [`App::anim_ms`].
    pub(crate) started_at: Instant,
    /// Pinned animation phase for render tests that must sample a specific point
    /// of the wave. Backdating `started_at` instead would subtract from
    /// `Instant::now()`, which panics on a host whose uptime is shorter than the
    /// offset. Never compiled into the binary.
    #[cfg(test)]
    pub(crate) anim_phase_ms: Option<u64>,
    /// Tick counter; advances the activity spinner frame each `on_tick`.
    pub(crate) tick_count: u64,
    pub(crate) quit: bool,
    /// First `q` at top level arms this; second `q` confirms quit.
    pub(crate) armed_quit: bool,
    /// Live footer alert; replaces the hint bar in place while `Some`.
    /// Cleared on disarm, tab switch, or explicit `x` dismiss.
    pub(crate) footer_alert: Option<FooterAlert>,
    /// Active banner; recomputed each tick from sticky system conditions.
    /// `None` when no condition holds (banner row absent from layout).
    pub(crate) banner: Option<Banner>,
    /// Last time the 1Hz divergence poll ran. `None` means un-sampled, which the
    /// gate reads as due — the state a fixture writes to force a poll, since
    /// backdating an `Instant` panics on a host booted more recently than the
    /// interval.
    pub(crate) last_divergence_check: Option<Instant>,
    /// The non-blocking divergence banner's backing signal: `Some` while the
    /// live login no longer matches the active profile, `None` the moment the
    /// link is clean again. Set/refreshed by the 1Hz poll and startup
    /// reconcile; <kbd>d</kbd> opens the resolver from it. In-memory only — a
    /// restart re-evaluates.
    pub(crate) divergence_pending: Option<DivergenceNotice>,
    /// Throttle for the Plugin tab's per-tick live refresh (session counts + link
    /// state); recompute fires at most once per `PLUGIN_REFRESH_INTERVAL`.
    pub(crate) last_plugin_refresh: Instant,
    /// Set once reconcile reports back; gates bootstrap spawn.
    pub(crate) reconcile_done: bool,
    /// Set once `spawn_bootstrap` is dispatched; prevents double-dispatch.
    pub(crate) bootstrap_started: bool,
    /// Tunable per-profile refresh interval; the scheduler reads this each tick.
    pub(crate) refresh_interval: Arc<AtomicU64>,
    /// Set before bootstrap spawn, cleared on every worker exit path.
    /// `ConfirmAction::RotateAll` checks this alongside `any_busy` to block a
    /// concurrent rotate-all from racing the bootstrap's relink + initial fetch.
    pub(crate) bootstrap_active: Arc<AtomicBool>,
    /// Signal to the scheduler thread that the UI is shutting down.
    pub(crate) shutting_down: Arc<AtomicBool>,
    /// Per-tab background-event indicator; `None` = no pending activity.
    /// Set when a background event fires on a tab that isn't currently active;
    /// cleared in `switch_tab` when the user visits that tab.
    pub(crate) tab_activity: [Option<ToastKind>; Tab::ALL.len()],

    /// Per-profile flag: has the bell been fired for the current crossing.
    /// Reset when utilization drops back below threshold.
    pub(crate) bell_fired: HashMap<String, bool>,

    /// Cached parsed usage history per profile from usage_history.jsonl. The
    /// file itself belongs to the scheduler's fetch path (`apply_outcome`),
    /// which may be running in another process, so this side is read-only.
    pub(crate) history_cache: HashMap<String, Vec<(u64, UsageInfo)>>,
    /// Last-known mtime per profile history file, for cache invalidation.
    pub(crate) history_mtimes: HashMap<String, std::time::SystemTime>,

    /// Cached long-lived-token status per profile, keyed by name (absent when a
    /// profile has no sidecar). Read by the Overview render for the `⊘` danger
    /// marker, so it needn't stat each `session-token.json` per frame. Seeded
    /// at construct and refreshed on config reload; the expiry stamp it
    /// carries is compared to live `now_ms` at render time, so the clock
    /// ticking past expiry never needs a re-read — only an add / delete /
    /// re-mint does, which `reload_fingerprint` now catches.
    pub(crate) session_tokens: HashMap<String, crate::claude::SessionTokenStatus>,
    /// Live `clauth start` sessions per account, for the Overview `active`
    /// column and the Fallback tab's compact equivalent. Cached because
    /// collecting it is a readdir plus an `open` + `try_lock` per row per marker
    /// layout, and both surfaces read it every frame. Refreshed by
    /// [`poll_live_sessions`] rather than on a config reload: sessions come and
    /// go without touching config.
    pub(crate) live_sessions: crate::live_sessions::LiveTally,
    /// Throttle for the per-tick re-tally; construct seeds the tally itself, so
    /// this starts stamped. `None` means un-sampled, which the gate reads as due
    /// — the state a fixture writes to force a re-tally, since backdating an
    /// `Instant` panics on a host booted more recently than the interval.
    last_live_sessions_refresh: Option<Instant>,
}

/// Read every named profile's long-lived-token status for the Overview cache.
/// Reads each `session-token.json` off disk, so callers gather the names under a
/// brief config guard and call this with the guard already dropped — the read
/// never happens while the config lock is held.
fn collect_session_tokens(names: &[String]) -> HashMap<String, crate::claude::SessionTokenStatus> {
    names
        .iter()
        .filter_map(|n| {
            // ONE sidecar read per profile, the same derivation `build_snap`
            // uses: Misfilled ⇔ NotLongLived, everything readable else is
            // LongLived. (The cache held the `SidecarKind` too while the
            // Overview wore a type tag; upstream 5dde024 removed the tag, and
            // the kind went with it.)
            let (kind, oauth) = crate::claude::sidecar_summary(&ProfileName::from(n.clone()))?;
            let status = match kind {
                crate::claude::SidecarKind::Misfilled => {
                    crate::claude::SessionTokenStatus::NotLongLived
                }
                _ => crate::claude::SessionTokenStatus::LongLived(oauth.expires_at),
            };
            Some((n.clone(), status))
        })
        .collect()
}

/// Cloned `Arc`s bundled for [`spawn_refresher`] and the bootstrap worker;
/// carries no lock rank and is safe to construct while holding any lock.
struct WorkerHandles {
    config: ConfigHandle,
    usage_tokens: TokenList,
    usage_store: UsageStore,
    usage_status: StatusStore,
    refresh_interval: Arc<AtomicU64>,
    next_refresh_per_profile: NextRefreshPerProfile,
    activity: ActivityStore,
    last_fetched: LastFetchedAt,
    poll_streaks: PollStreaks,
    kick_blocks: KickBlocks,
    pending_switch: PendingSwitch,
    pending_switch_off: PendingSwitchOff,
    refetch_queue: RefetchQueue,
    third_party_tokens: ThirdPartyList,
    third_party_usage_store: ThirdPartyUsageStore,
    third_party_status: ThirdPartyStatusStore,
    shutting_down: Arc<AtomicBool>,
    fetch_lease: Arc<crate::daemon::FetchLease>,
    bootstrap_active: Arc<AtomicBool>,
    startup_sender: StartupSender,
}

impl WorkerHandles {
    /// Clone every `Arc` the background workers share with the UI thread out of
    /// `app`.
    fn from_app(app: &App) -> Self {
        Self {
            config: Arc::clone(&app.config),
            usage_tokens: Arc::clone(&app.usage_tokens),
            usage_store: Arc::clone(&app.usage_store),
            usage_status: Arc::clone(&app.usage_status),
            refresh_interval: Arc::clone(&app.refresh_interval),
            next_refresh_per_profile: Arc::clone(&app.next_refresh_per_profile),
            activity: Arc::clone(&app.activity),
            last_fetched: Arc::clone(&app.last_fetched),
            poll_streaks: Arc::clone(&app.poll_streaks),
            kick_blocks: Arc::clone(&app.kick_blocks),
            pending_switch: Arc::clone(&app.pending_switch),
            pending_switch_off: Arc::clone(&app.pending_switch_off),
            refetch_queue: Arc::clone(&app.refetch_queue),
            third_party_tokens: Arc::clone(&app.third_party_tokens),
            third_party_usage_store: Arc::clone(&app.third_party_usage_store),
            third_party_status: Arc::clone(&app.third_party_status),
            shutting_down: Arc::clone(&app.shutting_down),
            fetch_lease: Arc::clone(&app.fetch_lease),
            bootstrap_active: Arc::clone(&app.bootstrap_active),
            startup_sender: app.startup_sender.clone(),
        }
    }
}

impl App {
    pub(crate) fn new(config: AppConfig) -> Self {
        let usage_store: UsageStore = Arc::new(RankedMutex::new(HashMap::new()));
        let usage_status: StatusStore = Arc::new(RankedMutex::new(HashMap::new()));
        let usage_tokens: TokenList = Arc::new(RankedMutex::new(collect_tokens(&config)));
        let next_refresh_per_profile: NextRefreshPerProfile =
            Arc::new(RankedMutex::new(HashMap::new()));
        let activity: ActivityStore = Arc::new(RankedMutex::new(HashMap::new()));
        let (op_sender, op_results) = std::sync::mpsc::channel::<OpResult>();
        let (switch_gate_tx, switch_gates) = std::sync::mpsc::channel::<SwitchGateResult>();
        let (startup_sender, startup_results) = std::sync::mpsc::channel::<StartupSignal>();
        let last_fetched: LastFetchedAt = Arc::new(RankedMutex::new(HashMap::new()));
        let poll_streaks: PollStreaks = Arc::new(RankedMutex::new(HashMap::new()));
        let kick_blocks: KickBlocks = Arc::new(RankedMutex::new(HashMap::new()));
        let pending_switch: PendingSwitch = Arc::new(RankedMutex::new(HashSet::new()));
        let pending_switch_off: PendingSwitchOff = Arc::new(RankedMutex::new(false));
        let refetch_queue: RefetchQueue = Arc::new(RankedMutex::new(HashSet::new()));
        let third_party_tokens: ThirdPartyList = Arc::new(RankedMutex::new(
            collect_third_party_entries(&config.profiles),
        ));
        let third_party_usage_store: ThirdPartyUsageStore =
            Arc::new(RankedMutex::new(HashMap::new()));
        let third_party_status: ThirdPartyStatusStore = Arc::new(RankedMutex::new(HashMap::new()));
        let refresh_interval = Arc::new(AtomicU64::new(config.state.refresh_interval_ms));

        let mut history_cache: HashMap<String, Vec<(u64, UsageInfo)>> = HashMap::new();
        let mut history_mtimes: HashMap<String, std::time::SystemTime> = HashMap::new();
        for profile in &config.profiles {
            let name = &profile.name;
            let data = crate::profile::load_usage_history(name);
            if !data.is_empty() {
                if let Ok(path) = crate::profile::profile_history_path(name)
                    && let Ok(meta) = std::fs::metadata(&path)
                    && let Ok(mtime) = meta.modified()
                {
                    history_mtimes.insert(name.to_string(), mtime);
                }
                history_cache.insert(name.to_string(), data);
            }
        }

        // Kick the best-effort update check; verdict lands in `update_results`, toasted from `on_tick`.
        let (update_sender, update_results) = std::sync::mpsc::channel::<UpdateEvent>();
        let update_handle = update::spawn(update_sender);

        // Status feed worker: streams incidents over `status_events`; a `()` on
        // `status_refresh` triggers a manual refetch. The channels are always
        // created (so the drains stay inert), but the thread is skipped under
        // `cfg!(test)`: a detached worker could outlive a test's `HOME_OVERRIDE`
        // scope and its cache write would then resolve the real `~/.clauth`.
        let (status_sender, status_events) = std::sync::mpsc::channel::<StatusEvent>();
        let (status_refresh, status_refresh_rx) = std::sync::mpsc::channel::<()>();
        if cfg!(test) {
            // Drop the worker's ends so they aren't flagged unused; the stored
            // `status_refresh` sender simply has no receiver in tests.
            drop((status_sender, status_refresh_rx));
        } else {
            status::spawn(status_sender, status_refresh_rx);
        }

        // Token-usage loader: reads `~/.claude/stats-cache.json` + recent
        // transcripts off the UI thread. Same test-skip rationale as the status
        // worker — a detached thread could outlive a test's `HOME_OVERRIDE`. The
        // `~/.claude` path is resolved once here so the worker never re-resolves
        // `home_dir()` (which would race the override).
        let (tokens_sender, tokens_events) =
            std::sync::mpsc::channel::<crate::tokens::TokensEvent>();
        let (tokens_refresh, tokens_refresh_rx) = std::sync::mpsc::channel::<()>();
        if cfg!(test) {
            drop((tokens_sender, tokens_refresh_rx));
        } else if let Ok(claude_dir) = crate::profile::claude_dir() {
            let clauth_dir = crate::profile::clauth_dir().ok();
            crate::tokens::spawn(tokens_sender, tokens_refresh_rx, claude_dir, clauth_dir);
        } else {
            drop((tokens_sender, tokens_refresh_rx));
        }

        // Pricing loader: fetches the ai-pricelog index rates for the Tokens
        // tab's cost lens, disk-cached under `~/.clauth`. Same test-skip
        // rationale as the status/token workers — a detached thread could
        // outlive a test's `HOME_OVERRIDE` and write the real `~/.clauth`.
        let (pricing_sender, pricing_events) =
            std::sync::mpsc::channel::<crate::pricing::PricingEvent>();
        let (pricing_refresh, pricing_refresh_rx) = std::sync::mpsc::channel::<()>();
        if cfg!(test) {
            drop((pricing_sender, pricing_refresh_rx));
        } else {
            crate::pricing::spawn(pricing_sender, pricing_refresh_rx);
        }

        let (login_event_tx, login_event_rx) = std::sync::mpsc::channel();
        let (login_result_tx, login_result_rx) = std::sync::mpsc::channel();

        // Seed the token cache and the live tally before `config` moves into the
        // handle below.
        let live_sessions = crate::live_sessions::LiveTally::collect(&config);
        let session_tokens = collect_session_tokens(
            &config
                .profiles
                .iter()
                .map(|p| p.name.to_string())
                .collect::<Vec<_>>(),
        );

        Self {
            config: Arc::new(RankedMutex::new(config)),
            usage_store,
            usage_status,
            usage_tokens,
            next_refresh_per_profile,
            activity,
            op_results,
            op_sender,
            switch_gates,
            switch_gate_tx,
            startup_results,
            startup_sender,
            last_fetched,
            poll_streaks,
            kick_blocks,
            pending_switch,
            pending_switch_off,
            refetch_queue,
            third_party_tokens,
            third_party_usage_store,
            third_party_status,
            tab: Tab::Overview,
            herdr_mode: false,
            modals: Vec::new(),
            help_scroll: 0,
            help_max_scroll: std::cell::Cell::new(0),
            profile_cursor: 0,
            config_focus: ConfigFocus::Profiles,
            config_action_cursor: 0,
            fallback_focus: FallbackFocus::Chain,
            fallback_detail_cursor: 0,
            fallback_armed_remove: false,
            fallback_threshold_draft: None,
            fallback_max_spend_draft: None,
            fallback_weekly_draft: None,
            global_config_cursor: 0,
            refresh_interval_draft: None,
            weekly_threshold_draft: None,
            config_draft: None,
            chain_cursor: 0,
            toasts: VecDeque::new(),
            compact: false,
            update_results,
            update_handle,
            login: None,
            login_generation: 0,
            login_event_rx,
            login_event_tx,
            login_result_rx,
            login_result_tx,
            status: StatusState::default(),
            status_events,
            status_refresh,
            daemon_health: crate::daemon::daemon_health(),
            last_daemon_probe: Instant::now(),
            fetch_lease: Arc::new(crate::daemon::FetchLease::new()),
            plugin: PluginState::default(),
            token_stats: None,
            tokens_failed: false,
            tokens_topping_up: false,
            tokens_progress: None,
            token_view: TokenView::Dashboard,
            token_model_cursor: 0,
            token_filter: TokenFilter::default(),
            token_period: TokenPeriod::default(),
            tokens_events,
            tokens_refresh,
            price_table: None,
            price_failed: false,
            pricing_events,
            pricing_refresh,
            last_reload_fp: reload_fingerprint(),
            started_at: Instant::now(),
            #[cfg(test)]
            anim_phase_ms: None,
            tick_count: 0,
            quit: false,
            armed_quit: false,
            footer_alert: None,
            banner: None,
            last_divergence_check: Some(Instant::now()),
            divergence_pending: None,
            last_plugin_refresh: Instant::now(),
            reconcile_done: false,
            bootstrap_started: false,
            refresh_interval,
            bootstrap_active: Arc::new(AtomicBool::new(false)),
            shutting_down: Arc::new(AtomicBool::new(false)),
            tab_activity: [None; Tab::ALL.len()],
            bell_fired: HashMap::new(),
            history_cache,
            history_mtimes,
            session_tokens,
            live_sessions,
            last_live_sessions_refresh: Some(Instant::now()),
        }
    }

    /// herdr-mode landing, applied at construction (before the first paint):
    /// the Plugin tab with the herdr selector row under the cursor. The herdr
    /// probe runs here too — `HERDR_ENV=1` proves herdr is present, and each
    /// of its three subprocesses is bounded at `herdr::PROBE_TIMEOUT` (2 s,
    /// worst case 6 s total) — so the landing row is real at first paint
    /// instead of waiting for `r`; the cursor clamp inside the recompute
    /// below keeps the landing row valid when herdr does not resolve. The
    /// `claude --version` probe stays `r`-gated: construction must not block
    /// the first paint on a spawn. Nothing else changes — no key handling, no
    /// focus stealing after construction.
    pub(crate) fn with_herdr_mode(mut self, herdr_mode: bool) -> Self {
        self.herdr_mode = herdr_mode;
        if herdr_mode {
            self.tab = Tab::Plugin;
            self.plugin.cursor = HERDR_SELECTOR_ROW;
            // Skipped under test (a spawned probe would read the real
            // registry); the landing test injects the probe instead.
            self.plugin.herdr = Some(if cfg!(test) {
                None
            } else {
                crate::herdr::probe()
            });
            recompute_plugin_checks(&mut self, false);
        }
        self
    }

    /// Phase clock every ambient animation keys off: milliseconds since the TUI
    /// opened, wrapped by each effect's own period.
    pub(crate) fn anim_ms(&self) -> u64 {
        #[cfg(test)]
        if let Some(ms) = self.anim_phase_ms {
            return ms;
        }
        self.started_at.elapsed().as_millis() as u64
    }

    /// Lock the shared AppConfig. Order: AppConfig outer, `with_state_lock` inner.
    pub(crate) fn config(&self) -> RankedGuard<'_, AppConfig> {
        Self::lock_config(&self.config)
    }

    /// The `config` lock for a worker holding only the cloned [`ConfigHandle`]
    /// — same lock and poison policy, without an `&App`.
    pub(crate) fn lock_config(config: &ConfigHandle) -> RankedGuard<'_, AppConfig> {
        #[expect(clippy::expect_used, reason = "mutex poisoning is unrecoverable")]
        config.lock().expect("config mutex poisoned")
    }

    /// Spawn the bootstrap on a background thread (never blocks first paint).
    /// Re-links credentials, then seeds usage from disk — OAuth caches via
    /// `bootstrap_fetch`, api-key/provider caches via `bootstrap_third_party`, both
    /// gated on the cache being fresher than one refresh interval and stamped at the
    /// cache mtime so the refresh cadence resumes across the restart — so the UI
    /// shows last-known numbers instantly while the scheduler refreshes on the normal
    /// cadence; profiles with no fresh cache are left for the scheduler to fetch in
    /// the background. No proactive token rotation (401-recovery is lazy). Posts
    /// `StartupSignal::BootstrapDone` when done; the UI thread then rebuilds the
    /// token snapshot, starts the scheduler, applies usage, and runs the startup
    /// auto-switch.
    pub(crate) fn spawn_bootstrap(&self) {
        let h = WorkerHandles::from_app(self);
        let done = BootstrapDoneGuard {
            bootstrap_active: h.bootstrap_active,
            startup_sender: h.startup_sender,
        };

        spawn_worker(move || {
            let _done = done;

            // Re-establish the credentials symlink (shutdown replaced it with
            // a plain file); without this, CC refreshes bypass the profile.
            let active = Self::lock_config(&h.config)
                .state
                .active_profile
                .as_ref()
                .cloned();
            if let Some(active) = active {
                let _ = link_profile_credentials(&active);
            }

            // `seed_names` is the display superset (disabled included) for the
            // cache seed; `snapshot` is the enabled-only work-list that drives
            // the `Queued` marking below — a disabled profile is seeded but never
            // queued for a fetch it will never get.
            let (seed_names, snapshot, third_party) = {
                let cfg = Self::lock_config(&h.config);
                (
                    collect_oauth_seed_names(&cfg),
                    collect_tokens(&cfg),
                    collect_third_party_entries(&cfg.profiles),
                )
            };
            let interval_ms = h.refresh_interval.load(Ordering::Relaxed);
            bootstrap_fetch(
                &h.usage_store,
                &h.usage_status,
                &h.last_fetched,
                &seed_names,
                interval_ms,
            );
            bootstrap_third_party(
                &h.third_party_usage_store,
                &h.third_party_status,
                &h.last_fetched,
                &third_party,
                interval_ms,
            );

            // Seeded-but-due and stale/missing profiles are fetched by the
            // scheduler's first tick (started in `finish_bootstrap`); 5h windows
            // are armed by the windowless scan in `on_tick` after bootstrap
            // clears `bootstrap_active` — no startup-only fetch or kick pass.

            // Profiles already due on the first tick — never-fetched (no cache) or
            // a cache older than one interval — are marked Queued now so the
            // overview timer and usage header show a pending spinner from first
            // paint instead of a stale `0s` countdown over the past deadline (a
            // never-fetched profile's deadline is `0 + interval`, always in the
            // past). The first tick re-marks (idempotent); each worker flips itself
            // to Fetching when its request fires and clears on landing.
            let now = now_ms();
            let due_now: Vec<String> = match h.last_fetched.lock() {
                Ok(lf) => snapshot
                    .iter()
                    .map(|e| e.name.to_string())
                    .chain(third_party.iter().map(|e| e.name.to_string()))
                    .filter(|n| {
                        lf.get(n)
                            .is_none_or(|t| t.as_millis().saturating_add(interval_ms) <= now)
                    })
                    .collect(),
                Err(_) => Vec::new(),
            };
            for name in &due_now {
                mark_activity(
                    &h.activity,
                    &ProfileName::from(name.clone()),
                    ProfileActivity::Queued,
                );
            }
        });
    }

    /// Recency-weighted burn rate (%/h) for `name`'s 5h window, computed from
    /// the in-memory `history_cache` — never touches disk. Shared by the
    /// Overview ETA line (`render/overview.rs`) and the burn-aware auto-switch
    /// one-shot below so neither the render pass nor this UI-thread check ever
    /// reads `usage_history.jsonl` while holding the config guard;
    /// `fallback::burn_rate_for_profile` is the disk-reading twin used off the
    /// render/UI thread (the scheduler tick, after locks are dropped).
    pub(crate) fn active_burn_rate(
        &self,
        name: &ProfileName,
        usage_info: &UsageInfo,
    ) -> Option<f64> {
        let five_h = usage_info.five_hour.as_ref().map(|w| ("5h", w))?;
        crate::usage::compute_burn_rates_from_history(
            self.history_cache
                .get(name.as_str())
                .map(|v| v.as_slice())
                .unwrap_or(&[]),
            std::slice::from_ref(&five_h),
            crate::usage::BURN_LOOKBACK_MS,
            crate::usage::BURN_MIN_SAMPLES,
            crate::usage::BURN_GAP_CUT_MS,
        )
        .remove("5h")
        .flatten()
    }

    /// UI-thread tail of bootstrap: rebuilds token snapshot, starts scheduler,
    /// applies usage, runs startup auto-switch. No HTTP.
    fn finish_bootstrap(&mut self) {
        self.refresh_tokens();
        self.start_scheduler();
        self.apply_usage();
        // Single-fetcher lease (#27): the startup switch one-shot is a switch
        // DECISION, so it may run only if THIS instance is the fetcher. Acquire
        // the shared lease now — the scheduler's first tick sleeps a cadence
        // before its own acquire, so the UI thread wins it here when it's free;
        // if another instance holds it, that instance owns every switch decision
        // and a one-shot here would race it (the #27 thrash).
        if !self.fetch_lease.acquire() {
            return;
        }
        let switched = {
            let mut cfg = self.config();
            // Run the startup one-shot on Fresh data only. A Cached seed's numbers
            // are unverified — stale in either direction — so switching on them
            // risks acting on a window the account no longer has. Stale profiles
            // are due on the scheduler's first tick, which fetches then
            // auto-switches off the corrected numbers.
            let active_profile = cfg.state.active_profile.as_ref().and_then(|n| cfg.find(n));
            let active_fresh =
                active_profile.is_some_and(|p| p.fetch_status == Some(FetchStatus::Fresh));
            if active_fresh {
                // In-memory rate only (`history_cache`) — never a disk read
                // while the config guard is held; see `active_burn_rate`.
                let rate = active_profile.and_then(|p| {
                    let usage = p.usage.as_ref()?;
                    self.active_burn_rate(&p.name, usage)
                });
                auto_switch_if_needed(&mut cfg, rate).ok().flatten()
            } else {
                None
            }
        };
        match switched {
            Some(SwitchAction::To(target)) => {
                // Destination-based, as in the pending-switch drain: landing on
                // the home account reads as a return whether the return pass or
                // an exhaustion walk onto a clear preferred put us there.
                let returned = self
                    .config()
                    .find(&ProfileName::from(target.clone()))
                    .is_some_and(|p| p.preferred);
                let msg = if returned {
                    format!("returned to preferred account '{target}'")
                } else {
                    format!("auto-switched to '{target}'")
                };
                self.toast(ToastKind::Warning, msg);
            }
            Some(SwitchAction::Off) => {
                self.refresh_tokens();
                self.toast(
                    ToastKind::Warning,
                    "all accounts spent\nswitched off to halt usage".to_string(),
                );
            }
            None => {}
        }
    }

    /// Bundle scheduler `Arc`s and launch the background refresher.
    fn start_scheduler(&self) {
        let h = WorkerHandles::from_app(self);
        // Session-scoped suppressed-generic set: rebuilt fresh each TUI launch,
        // dropped on exit. Purely scheduler-internal — the App never touches it
        // (manual refresh clears suppression via the shared forced queue).
        let suppressed_generic: SuppressedGenericStore = Arc::new(RankedMutex::new(HashMap::new()));
        spawn_refresher(
            h.config,
            h.usage_tokens,
            h.usage_store,
            h.usage_status,
            h.refresh_interval,
            h.next_refresh_per_profile,
            h.activity,
            h.last_fetched,
            h.poll_streaks,
            h.kick_blocks,
            h.pending_switch,
            h.pending_switch_off,
            h.refetch_queue,
            h.third_party_tokens,
            h.third_party_usage_store,
            h.third_party_status,
            suppressed_generic,
            h.shutting_down,
            // Single-fetcher lease (#27): the TUI competes for `usage-fetch.lock`
            // like any instance, standing its refresher down while another holds
            // it. Shared with `finish_bootstrap` so its startup switch one-shot
            // only runs when this TUI is the fetcher.
            h.fetch_lease,
        );
    }

    pub(crate) fn apply_usage(&mut self) {
        // On poisoned lock keep the prior value — a blank map would blind auto-switch permanently.
        // Third-party stores BEFORE OAuth stores: ranks 270/280 < 300/350.
        let bells;
        let history_names;
        {
            let third_party_map = self.third_party_usage_store.lock().ok();
            let third_party_status_map = self.third_party_status.lock().ok();
            let info_map = self.usage_store.lock().ok();
            let status_map = self.usage_status.lock().ok();
            let mut cfg = self.config();
            for p in &mut cfg.profiles {
                if let Some(s) = info_map.as_ref() {
                    p.usage = s.get(p.name.as_str()).cloned();
                }
                // OAuth fetch_status takes precedence; third-party only when no OAuth status.
                if let Some(s) = status_map.as_ref()
                    && s.contains_key(p.name.as_str())
                {
                    p.fetch_status = s.get(p.name.as_str()).copied();
                } else if let Some(s) = third_party_status_map.as_ref() {
                    p.fetch_status = s.get(p.name.as_str()).copied();
                }
                if let Some(s) = third_party_map.as_ref()
                    && s.contains_key(p.name.as_str())
                {
                    p.third_party_usage = s.get(p.name.as_str()).cloned();
                }
            }

            bells = cfg
                .profiles
                .iter()
                .map(|p| {
                    let util = p
                        .usage
                        .as_ref()
                        .and_then(|u| u.five_hour.as_ref())
                        .map(|u| u.utilization);
                    let fresh = p.fetch_status == Some(FetchStatus::Fresh);
                    (p.name.to_string(), p.bell_threshold, util, fresh)
                })
                .collect::<Vec<_>>();

            // Collect while cfg is still alive.
            history_names = cfg
                .profiles
                .iter()
                .filter(|p| p.usage.is_some())
                .map(|p| p.name.to_string())
                .collect::<Vec<_>>();
        }
        for (name, threshold, util, fresh) in bells {
            // Ring or clear only on a live read — a synthetic/stale window (e.g.
            // a just-kicked 0%) must not clear a real bell or fire a false one.
            // Non-fresh profiles keep their prior bell state.
            if !fresh {
                continue;
            }
            if let Some(t) = threshold
                && let Some(u) = util
            {
                if u >= t {
                    if !self.bell_fired.contains_key(&name) {
                        self.toast(
                            ToastKind::Warning,
                            format!("bell: {name} at {}", format_pct(u)),
                        );
                        self.set_tab_activity(Tab::Overview, ToastKind::Warning);
                        self.bell_fired.insert(name, true);
                    }
                } else {
                    self.bell_fired.remove(&name);
                }
            }
        }

        // Re-read any history log that changed on disk. The file is written by
        // whichever process holds the fetch lease — this one or a headless
        // daemon — so an mtime bump is the only signal that new samples landed.
        for name in &history_names {
            if let Ok(path) = crate::profile::profile_history_path(&ProfileName::from(name.clone()))
                && let Ok(mtime) = path.metadata().and_then(|m| m.modified())
                && self.history_mtimes.get(name) != Some(&mtime)
            {
                self.history_cache.insert(
                    name.clone(),
                    crate::profile::load_usage_history(&ProfileName::from(name.clone())),
                );
                self.history_mtimes.insert(name.clone(), mtime);
            }
        }
    }

    /// Reload config if state mtime changed. Returns true on reload.
    pub(crate) fn reload_if_state_changed(&mut self) -> bool {
        let current = reload_fingerprint();
        if current == self.last_reload_fp {
            return false;
        }
        if let Ok(fresh) = load_config() {
            *self.config() = fresh;
            self.last_reload_fp = current;
            // Collect under the config guard, then DROP it before locking the
            // token mutexes — TOKENS/THIRD_PARTY rank OUTSIDE CONFIG, so writing
            // them while config is held inverts the global lock order (same shape
            // as `refresh_tokens`).
            let (tokens, third_party, names) = {
                let cfg = self.config();
                let tokens = collect_tokens(&cfg);
                let third_party = collect_third_party_entries(&cfg.profiles);
                let names: Vec<String> = cfg.profiles.iter().map(|p| p.name.to_string()).collect();
                (tokens, third_party, names)
            };
            #[expect(clippy::expect_used, reason = "mutex poisoning is unrecoverable")]
            {
                *self
                    .usage_tokens
                    .lock()
                    .expect("usage_tokens mutex poisoned") = tokens;
                *self
                    .third_party_tokens
                    .lock()
                    .expect("third_party_tokens mutex poisoned") = third_party;
            }
            self.session_tokens = collect_session_tokens(&names);
            true
        } else {
            false
        }
    }

    pub(crate) fn refresh_tokens(&self) {
        // Drop `config` lock before taking `usage_tokens` — folding them would
        // invert lock order (TOKENS is outer of CONFIG).
        let tokens = collect_tokens(&self.config());
        let third_party = collect_third_party_entries(&self.config().profiles);
        #[expect(clippy::expect_used, reason = "mutex poisoning is unrecoverable")]
        {
            *self
                .usage_tokens
                .lock()
                .expect("usage_tokens mutex poisoned") = tokens;
            *self
                .third_party_tokens
                .lock()
                .expect("third_party_tokens mutex poisoned") = third_party;
        }
    }

    /// Queue every profile for an immediate re-fetch (Overview `r`). Reuses each
    /// profile's cached plan/tier — only the single-profile refresh re-pulls
    /// `/profile`, keeping the global refresh light on the rate-limited host.
    pub(crate) fn manual_refresh(&self) {
        #[expect(clippy::expect_used, reason = "mutex poisoning is unrecoverable")]
        let names: Vec<ProfileName> = self
            .usage_tokens
            .lock()
            .expect("usage_tokens mutex poisoned")
            .iter()
            .map(|e| e.name.clone())
            .collect();
        for name in names {
            self.enqueue_refetch(&name, false);
        }
    }

    /// Queue a single profile for an immediate re-fetch (Usage `r` / action
    /// menu), also re-pulling its `/profile` (plan / tier).
    pub(crate) fn manual_refresh_one(&self, name: &ProfileName) {
        self.enqueue_refetch(name, true);
    }

    /// Mark a profile for an immediate re-fetch. `refresh_plan` expires the
    /// `/profile` TTL so the next fetch re-pulls plan/tier — set for an explicit
    /// single-profile refresh, cleared for the bulk refresh-all.
    fn enqueue_refetch(&self, name: &ProfileName, refresh_plan: bool) {
        // Light a pending spinner immediately so the UI reflects the keypress.
        // Only when idle — don't clobber an in-flight switch/refresh marker. The
        // next tick's worker flips Queued→Fetching when its request fires; a name
        // no leg owns is cleared by the tick's orphan sweep.
        if is_idle(&self.activity, name) {
            mark_activity(&self.activity, name, ProfileActivity::Queued);
        }
        if refresh_plan {
            crate::usage::expire_profile_ttl(name);
        }
        if let Ok(mut q) = self.refetch_queue.lock() {
            q.insert(name.to_string());
        }
    }

    pub(crate) fn toast(&mut self, kind: ToastKind, body: impl Into<String>) {
        if self.toasts.len() >= TOAST_CAPACITY {
            self.toasts.pop_front();
        }
        self.toasts.push_back(Toast {
            kind,
            body: body.into(),
            born: Instant::now(),
        });
    }

    /// Disarm the 2-step quit and clear its accompanying footer alert.
    /// Only call this when the armed-quit sequence is interrupted (key switch,
    /// tab change, etc.). For `x`-dismiss of an alert produced by a non-quit
    /// condition, clear `footer_alert` directly instead.
    pub(crate) fn disarm_quit(&mut self) {
        self.armed_quit = false;
        self.footer_alert = None;
    }

    pub(crate) fn prune_toasts(&mut self) {
        while let Some(front) = self.toasts.front() {
            let ttl = if front.kind == ToastKind::Danger {
                TOAST_TTL_DANGER
            } else {
                TOAST_TTL_NORMAL
            };
            if front.born.elapsed() >= ttl {
                self.toasts.pop_front();
            } else {
                break;
            }
        }
    }

    /// Called each frame with the current terminal height. Tracks compact
    /// state; the matching warning banner is recomputed from this flag in
    /// `update_banner` each tick, so it self-clears on resize (no toast).
    pub(crate) fn update_compact(&mut self, terminal_height: u16) {
        self.compact = terminal_height < 14;
    }

    /// Mark a background tab with an activity color. Only sets if `tab` is not
    /// the currently-active tab; visiting the tab (via `switch_tab`) clears it.
    /// Higher-severity kinds override lower ones (Danger > Warning > Success > Info).
    pub(crate) fn set_tab_activity(&mut self, tab: Tab, kind: ToastKind) {
        if tab == self.tab {
            return;
        }
        let idx = tab.index();
        let prev = self.tab_activity[idx];
        let severity = |k: ToastKind| match k {
            ToastKind::Danger => 3,
            ToastKind::Warning => 2,
            ToastKind::Success => 1,
            ToastKind::Info => 0,
        };
        if prev.is_none_or(|p| severity(kind) > severity(p)) {
            self.tab_activity[idx] = Some(kind);
        }
    }

    // ── Main list ────────────────────────────────────────────────────────────

    pub(crate) fn main_items(&self) -> Vec<MainItemKind> {
        (0..self.config().profiles.len())
            .map(MainItemKind::Profile)
            .collect()
    }

    pub(crate) fn profile_count(&self) -> usize {
        self.config().profiles.len()
    }

    pub(crate) fn profile_name_at(&self, idx: usize) -> Option<ProfileName> {
        self.config().profiles.get(idx).map(|p| p.name.clone())
    }

    /// Clamp `profile_cursor` to `0..profile_count`.
    pub(crate) fn clamp_profile_cursor(&mut self) {
        let max = self.profile_count().saturating_sub(1);
        self.profile_cursor = self.profile_cursor.min(max);
    }

    pub(crate) fn current_main_item(&self) -> Option<MainItemKind> {
        self.main_items().get(self.profile_cursor).copied()
    }
}

// ── Token snapshot ────────────────────────────────────────────────────────────

// ── Startup reconciliation ────────────────────────────────────────────────────

/// Startup credential reconciliation, non-blocking. Compares the live
/// `~/.claude/.credentials.json` to the active profile's stored creds inline.
/// No divergence → snapshot + `ReconcileDone` immediately.
///
/// On divergence we never probe the stored chain via OAuth refresh — a refresh
/// *spends* the single-use refresh token server-side and would rotate the stored
/// identity on every diverged startup. Instead the Divergence modal hands the
/// verdict to the user. None of its actions spend the stored token (Overwrite
/// takes live creds, NewProfile captures them, Discard relinks as-is); a stale
/// access token is refreshed lazily on the next fetch.
pub(super) fn reconcile_startup(app: &mut App) {
    let Some(active) = app.config().state.active_profile.as_ref().cloned() else {
        let _ = app.startup_sender.send(StartupSignal::ReconcileDone);
        return;
    };

    // Read live credentials under the state lock to avoid torn snapshots.
    let live = with_state_lock(|_held| Ok(read_claude_credentials().ok().flatten()))
        .ok()
        .flatten();
    let diverged = {
        let cfg = app.config();
        let stored = cfg.find(&active).and_then(|p| p.credentials.as_ref());
        credentials_diverged(stored, live.as_ref())
    };

    if !diverged {
        let mut cfg = app.config();
        let _ = snapshot_active_credentials(&mut cfg);
        let _ = app.startup_sender.send(StartupSignal::ReconcileDone);
        return;
    }

    // Diverged: hand the verdict to the user. No network, no FS write here.
    let _ = app
        .startup_sender
        .send(StartupSignal::ReconcileNeedsPrompt {
            active: active.to_string(),
        });
}

// ── Event handling ────────────────────────────────────────────────────────────

/// True while `app.tab`'s descend/ascend sub-focus screen is active (Setup's
/// Actions pane, Fallback's Detail pane, Status/Plugin's Detail pane, Tokens'
/// Models view) — the state where `q`/`esc` ascend instead of arming quit /
/// no-op. Single source of truth shared by the `q` handler and the footer's
/// `q back` / `q quit` label; the help-modal esc-row test iterates it too, so
/// a new sub-focus tab without an `esc` help row fails CI.
pub(crate) fn has_sub_focus(app: &App) -> bool {
    (app.tab == Tab::Setup && app.config_focus == ConfigFocus::Actions)
        || (app.tab == Tab::Fallback && app.fallback_focus == FallbackFocus::Detail)
        || (app.tab == Tab::Status && app.status.focus == StatusFocus::Detail)
        || (app.tab == Tab::Plugin && app.plugin.focus == PluginFocus::Detail)
        || (app.tab == Tab::Tokens && app.token_view == TokenView::Models)
}

pub(crate) fn handle_key(app: &mut App, key: KeyEvent) {
    if key.kind != KeyEventKind::Press {
        return;
    }

    if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
        app.quit = true;
        app.shutting_down.store(true, Ordering::SeqCst);
        return;
    }

    if !app.modals.is_empty() {
        handle_modal_key(app, key);
        return;
    }

    // Config text field capturing keystrokes owns keyboard (like a modal)
    // so typing into a name can't fire global shortcuts.
    if app.tab == Tab::Setup
        && app.config_focus == ConfigFocus::Actions
        && app
            .config_draft
            .as_ref()
            .is_some_and(|d| d.active.is_some())
    {
        handle_config_edit_key(app, key);
        return;
    }

    // Same for the threshold editor: owns keyboard so digits can't trip globals.
    if app.tab == Tab::Fallback
        && app.fallback_focus == FallbackFocus::Detail
        && app.fallback_threshold_draft.is_some()
    {
        handle_fallback_threshold_edit_key(app, key);
        return;
    }

    // Same for the `max auto-spend` editor.
    if app.tab == Tab::Fallback
        && app.fallback_focus == FallbackFocus::Detail
        && app.fallback_max_spend_draft.is_some()
    {
        handle_fallback_max_spend_edit_key(app, key);
        return;
    }

    // Same for the per-member `weekly at` override editor.
    if app.tab == Tab::Fallback
        && app.fallback_focus == FallbackFocus::Detail
        && app.fallback_weekly_draft.is_some()
    {
        handle_fallback_weekly_edit_key(app, key);
        return;
    }

    // Same for the Config-tab refresh-interval custom-value editor.
    if app.tab == Tab::Config && app.refresh_interval_draft.is_some() {
        handle_refresh_interval_edit_key(app, key);
        return;
    }

    // And the Config-tab weekly-threshold custom-value editor.
    if app.tab == Tab::Config && app.weekly_threshold_draft.is_some() {
        handle_weekly_threshold_edit_key(app, key);
        return;
    }

    // And the Plugin-tab herdr tag-refresh editor (same capture shape: typing
    // owns the keyboard so digits can't trip global shortcuts).
    if app.tab == Tab::Plugin && app.plugin.herdr_tag_draft.is_some() {
        handle_herdr_tag_edit_key(app, key);
        return;
    }

    // Esc/q abandon an in-flight login (the detached worker's result is
    // discarded by the generation guard). Catching q here keeps it symmetric
    // with esc — otherwise q would ascend out of the Setup form (orphaning the
    // draft that receives the mint) or silently arm the 2-step quit under a
    // footer whose login line never yields to the armed alert. Sits BELOW the
    // editor captures so typing a literal `q` into a field can't cancel.
    if app.login.is_some() && matches!(key.code, KeyCode::Esc | KeyCode::Char('q')) {
        app.login = None;
        app.toast(ToastKind::Info, "login canceled");
        return;
    }

    match key.code {
        KeyCode::Right | KeyCode::Tab => {
            app.disarm_quit();
            switch_tab(app, app.tab.next());
            return;
        }
        KeyCode::Left | KeyCode::BackTab => {
            app.disarm_quit();
            switch_tab(app, app.tab.prev());
            return;
        }
        KeyCode::Char('?') => {
            app.disarm_quit();
            app.help_scroll = 0;
            app.modals.push(Modal::Help);
            return;
        }
        KeyCode::Char('d') => {
            // Open the divergence resolver from the banner (no-op when the
            // live link is clean).
            if let Some(notice) = app.divergence_pending.clone() {
                app.disarm_quit();
                open_divergence_modal(app, &notice.active);
            }
            return;
        }
        KeyCode::Char('a') => {
            app.disarm_quit();
            let state = build_action_menu(app);
            if !state.items.is_empty() {
                app.modals.push(Modal::ActionMenu(state));
            }
            return;
        }
        KeyCode::Char('r') => {
            app.disarm_quit();
            if app.tab == Tab::Status {
                trigger_status_refresh(app);
                return;
            }
            // Plugin checks re-run synchronously; `r` also re-probes `claude --version`.
            if app.tab == Tab::Plugin {
                recompute_plugin_checks(app, true);
                app.toast(ToastKind::Info, "re-running plugin checks");
                return;
            }
            if app.tab == Tab::Tokens {
                reload_token_stats(app);
                return;
            }
            if app.tab == Tab::Usage {
                queue_focused_usage_refresh(app);
            } else {
                app.manual_refresh();
                app.toast(ToastKind::Info, "refreshing every account");
            }
            return;
        }
        KeyCode::Char('t') => {
            app.disarm_quit();
            // Tokens claims `t` for its period lens (as `r` reloads there);
            // rotate-all stays reachable from every other tab.
            if app.tab == Tab::Tokens {
                set_token_period(app, app.token_period.next());
                return;
            }
            app.modals.push(Modal::Confirm(ConfirmState {
                message: ROTATE_ALL_MSG.to_string(),
                detail: Some(ROTATE_ALL_DETAIL.to_string()),
                choice: false,
                on_confirm: ConfirmAction::RotateAll,
            }));
            return;
        }
        KeyCode::Char('n') => {
            app.disarm_quit();
            start_new_account(app);
            return;
        }
        // Esc backs out of sub-focus; no-op at the top level.
        KeyCode::Esc => {
            app.disarm_quit();
            if app.tab == Tab::Setup && app.config_focus == ConfigFocus::Actions {
                leave_config_detail(app);
            } else if app.tab == Tab::Fallback && app.fallback_focus == FallbackFocus::Detail {
                leave_fallback_detail(app);
            } else if app.tab == Tab::Status && app.status.focus == StatusFocus::Detail {
                app.status.focus = StatusFocus::List;
            } else if app.tab == Tab::Plugin && app.plugin.focus == PluginFocus::Detail {
                app.plugin.focus = PluginFocus::List;
            } else if app.tab == Tab::Tokens && app.token_view == TokenView::Models {
                app.token_view = TokenView::Dashboard;
            }
            // At top level, Esc is a no-op — ctrl+c or `q q` to quit.
            return;
        }
        // First `q` at the top level arms the 2-step quit; second confirms.
        // When there is a sub-focus to back out of, `q` ascends instead.
        KeyCode::Char('q') => {
            if has_sub_focus(app) {
                app.disarm_quit();
                if app.tab == Tab::Setup {
                    leave_config_detail(app);
                } else if app.tab == Tab::Fallback {
                    leave_fallback_detail(app);
                } else if app.tab == Tab::Tokens {
                    app.token_view = TokenView::Dashboard;
                } else if app.tab == Tab::Plugin {
                    app.plugin.focus = PluginFocus::List;
                } else {
                    app.status.focus = StatusFocus::List;
                }
            } else if app.armed_quit {
                app.quit = true;
                app.shutting_down.store(true, Ordering::SeqCst);
            } else {
                app.armed_quit = true;
                app.footer_alert = Some(FooterAlert::Warn("press q again to quit".to_string()));
            }
            return;
        }
        KeyCode::Char('x') => {
            // Dismissal precedence: toasts first, then footer alert.
            // `x` dismisses the alert directly; quit disarming is a side effect
            // only when armed_quit was the producer — not a general alert clear.
            if !app.toasts.is_empty() {
                app.toasts.pop_front();
            } else if app.footer_alert.is_some() {
                app.footer_alert = None;
                if app.armed_quit {
                    app.armed_quit = false;
                }
            }
            return;
        }
        _ => {
            app.disarm_quit();
        }
    }

    match app.tab {
        Tab::Overview => handle_overview_key(app, key),
        Tab::Usage => handle_usage_key(app, key),
        Tab::Tokens => handle_tokens_key(app, key),
        Tab::Setup => handle_config_key(app, key),
        Tab::Fallback => handle_fallback_key(app, key),
        Tab::Config => handle_global_config_key(app, key),
        Tab::Status => handle_status_key(app, key),
        Tab::Plugin => handle_plugin_key(app, key),
    }
}

/// The Tokens model rows for the active period lens — filtered and ranked
/// exactly as rendered, the single source for both the render and the cursor
/// math. Lifetime keeps the grouped ("others"-folded) rows; scoped periods
/// list raw per-model aggregates (a period's list is short, no tail fold).
pub(crate) fn token_period_models(app: &App) -> Vec<crate::tokens::PeriodModel> {
    use crate::tokens::PeriodModel;
    let Some(stats) = app.token_stats.as_ref() else {
        return Vec::new();
    };
    let mut rows: Vec<PeriodModel> = if let Some(bucket) = app.token_period.bucket() {
        let (from, to) = crate::tokens::current_bucket_bounds(&crate::tokens::today_date(), bucket);
        crate::tokens::period_models(&stats.daily_models, &from, &to)
    } else if app.token_period == TokenPeriod::Daily {
        stats
            .today
            .as_ref()
            .map(|t| {
                // Daily-lens rows carry today's per-hour buckets (unlike
                // lifetime rows, whose aggregate holds no dates), so the
                // top-models and detail views price the same hourly rates
                // the today card does.
                use crate::pricing::HourTokens;
                let mut hours: HashMap<&str, [HourTokens; 24]> = t
                    .model_hours
                    .iter()
                    .map(|m| (m.model.as_str(), m.hours))
                    .collect();
                t.models
                    .iter()
                    .map(|m| PeriodModel {
                        model: m.model.clone(),
                        in_out: m.in_out(),
                        split: m.clone(),
                        split_complete: true,
                        days: vec![crate::tokens::PeriodDay {
                            date: t.date.clone(),
                            split: m.clone(),
                            hours: hours.remove(m.model.as_str()),
                        }],
                    })
                    .collect()
            })
            .unwrap_or_default()
    } else {
        crate::tokens::group_models(&stats.models)
            .iter()
            .map(PeriodModel::from_full)
            .collect()
    };
    rows.retain(|m| app.token_filter.matches(&m.model));
    let basis = crate::tokens::effective_cache_basis(&rows, app.config().state.count_cache);
    rows.sort_unstable_by_key(|m| std::cmp::Reverse(m.metric(basis)));
    rows
}

/// Number of rows in the Tokens `Models` master list under the active
/// period + filter lenses.
fn token_model_count(app: &App) -> usize {
    token_period_models(app).len()
}

/// Tokens `r` / action-menu reload: re-read the on-disk stats + recent
/// transcripts and refetch model prices for the cost lens.
fn reload_token_stats(app: &mut App) {
    let _ = app.tokens_refresh.send(());
    let _ = app.pricing_refresh.send(());
    // A user-triggered reload is a foreground refresh — light the loading
    // spinners until the loader's next `Loaded`/`Failed` clears them.
    app.tokens_topping_up = true;
    app.tokens_progress = None;
    app.toast(ToastKind::Info, "reloading token usage");
}

/// Tokens tab: `c` toggles cache-counting (persisted) on either view (`t`'s
/// period cycle is claimed by the global key arm); Dashboard descends to
/// Models on ⏎; Models moves the model cursor with ↑↓ (ascend handled by the
/// global esc/q).
fn handle_tokens_key(app: &mut App, key: KeyEvent) {
    if key.code == KeyCode::Char('c') {
        toggle_count_cache(app);
        return;
    }
    match app.token_view {
        TokenView::Dashboard => {
            // Dashboard is a fixed grid (no scroll); ⏎ descends into Models.
            if key.code == KeyCode::Enter && token_model_count(app) > 0 {
                app.token_view = TokenView::Models;
                app.token_model_cursor = 0;
            }
        }
        TokenView::Models => {
            let len = token_model_count(app);
            match key.code {
                KeyCode::Up if len > 0 => {
                    app.token_model_cursor = (app.token_model_cursor + len - 1).rem_euclid(len);
                }
                KeyCode::Down if len > 0 => {
                    app.token_model_cursor = (app.token_model_cursor + 1).rem_euclid(len);
                }
                _ => {}
            }
        }
    }
}

/// Switch the active tab and reset that tab's cursor to a sensible default.
fn switch_tab(app: &mut App, tab: Tab) {
    app.tab = tab;
    app.tab_activity[tab.index()] = None;
    app.config_draft = None;
    // Clamp cursor: a Config `+ new` selection must land on a real account.
    app.clamp_profile_cursor();
    match tab {
        Tab::Overview | Tab::Usage => {}
        Tab::Tokens => {
            // Land on the dashboard; keep the model cursor reset for descend.
            app.token_view = TokenView::Dashboard;
            app.token_model_cursor = 0;
        }
        Tab::Setup => {
            app.config_focus = ConfigFocus::Profiles;
            app.config_action_cursor = 0;
        }
        Tab::Fallback => {
            app.chain_cursor = chain_cursor_for_profile(app);
            sync_profile_from_chain(app);
            app.fallback_focus = FallbackFocus::Chain;
            app.fallback_detail_cursor = 0;
            app.fallback_armed_remove = false;
            app.fallback_threshold_draft = None;
        }
        Tab::Config => {
            app.global_config_cursor = 0;
            app.refresh_interval_draft = None;
            app.weekly_threshold_draft = None;
        }
        Tab::Status => {
            // Keep the incident cursor; reset focus to the list per the contract.
            app.status.focus = StatusFocus::List;
        }
        Tab::Plugin => {
            app.plugin.focus = PluginFocus::List;
            app.plugin.cursor = 0;
            app.plugin.detail_scroll = 0;
            // Entering the tab clears an open tag editor the same way the
            // Config tab clears its drafts; `switch_tab` matches the
            // destination, so the clear runs on entry, not on exit.
            app.plugin.herdr_tag_draft = None;
            // Recompute on focus; the cached `claude --version` is not re-probed.
            recompute_plugin_checks(app, false);
        }
    }
}

/// Move `profile_cursor` by `delta`, wrapping in `0..len`.
fn step_profile_cursor(app: &mut App, delta: i32, len: usize) {
    if len == 0 {
        return;
    }
    app.profile_cursor = (app.profile_cursor as i32 + delta).rem_euclid(len as i32) as usize;
}

fn handle_overview_key(app: &mut App, key: KeyEvent) {
    let count = app.profile_count();
    match key.code {
        KeyCode::Up if key.modifiers.contains(KeyModifiers::SHIFT) => reorder_main_cursor(app, -1),
        KeyCode::Down if key.modifiers.contains(KeyModifiers::SHIFT) => reorder_main_cursor(app, 1),
        KeyCode::Up => step_profile_cursor(app, -1, count),
        KeyCode::Down => step_profile_cursor(app, 1, count),
        KeyCode::Enter => activate_main_item(app),
        _ => {}
    }
}

/// Usage tab: up/down picks the account. Read-only pane.
fn handle_usage_key(app: &mut App, key: KeyEvent) {
    let count = app.profile_count();
    match key.code {
        KeyCode::Up => step_profile_cursor(app, -1, count),
        KeyCode::Down => step_profile_cursor(app, 1, count),
        KeyCode::Char('e') => toggle_show_estimates(app),
        KeyCode::Char('p') => toggle_show_pace(app),
        _ => {}
    }
}

/// Status tab keymap. List focus: ↑↓ moves the incident cursor (wrapping), ⏎
/// descends into the timeline (only when an incident exists). Detail focus: ↑↓
/// scrolls the timeline (clamped to content by the render pass).
fn handle_status_key(app: &mut App, key: KeyEvent) {
    match app.status.focus {
        StatusFocus::List => {
            let len = app.status.incidents.len();
            match key.code {
                KeyCode::Up if len > 0 => {
                    app.status.cursor = (app.status.cursor + len - 1).rem_euclid(len.max(1));
                    app.status.detail_scroll = 0;
                }
                KeyCode::Down if len > 0 => {
                    app.status.cursor = (app.status.cursor + 1).rem_euclid(len.max(1));
                    app.status.detail_scroll = 0;
                }
                KeyCode::Enter if len > 0 => {
                    app.status.focus = StatusFocus::Detail;
                    app.status.detail_scroll = 0;
                }
                _ => {}
            }
        }
        StatusFocus::Detail => match key.code {
            KeyCode::Up => {
                // Clamp before stepping, for the mirror of the ↓ case: a shorter
                // incident (or a taller terminal) shrinks the bound under an
                // offset that outlived it, and stepping down from the stale
                // value costs one press per row while the render clamps the
                // display, so ↑ reads as dead.
                let max = app.status.detail_max_scroll.get();
                app.status.detail_scroll = app.status.detail_scroll.min(max).saturating_sub(1);
            }
            KeyCode::Down => {
                // Clamp against the last render's content height so a held ↓ can't
                // run the offset past the end (which would make ↑ look dead).
                let max = app.status.detail_max_scroll.get();
                app.status.detail_scroll = app.status.detail_scroll.saturating_add(1).min(max);
            }
            _ => {}
        },
    }
}

/// Signal the status thread to refetch and light the title spinner.
fn trigger_status_refresh(app: &mut App) {
    let _ = app.status_refresh.send(());
    app.status.fetching = true;
    app.toast(ToastKind::Info, "refreshing status");
}

/// Plugin tab keymap. List focus: ↑↓ moves the cursor (wrapping over both
/// groups), ⏎ descends to the detail pane, `f` applies the selected row's fix.
/// Detail focus: the herdr detail walks its focusable options rows, every
/// other detail scrolls (clamped by the render pass); `f` still fixes.
fn handle_plugin_key(app: &mut App, key: KeyEvent) {
    match app.plugin.focus {
        PluginFocus::List => {
            let len = app.plugin.row_count();
            match key.code {
                KeyCode::Up if len > 0 => {
                    app.plugin.cursor = (app.plugin.cursor + len - 1) % len;
                    app.plugin.detail_scroll = 0;
                }
                KeyCode::Down if len > 0 => {
                    app.plugin.cursor = (app.plugin.cursor + 1) % len;
                    app.plugin.detail_scroll = 0;
                }
                KeyCode::Enter if len > 0 => {
                    app.plugin.focus = PluginFocus::Detail;
                    app.plugin.detail_scroll = 0;
                }
                KeyCode::Char('f') => apply_plugin_fix(app),
                _ => {}
            }
        }
        PluginFocus::Detail => {
            if app
                .plugin
                .selected_check()
                .is_some_and(|c| c.label == "herdr")
            {
                handle_herdr_options_key(app, key);
            } else {
                match key.code {
                    KeyCode::Up => {
                        // Clamp before stepping — see the Status detail pane's ↑ arm.
                        let max = app.plugin.detail_max_scroll.get();
                        app.plugin.detail_scroll =
                            app.plugin.detail_scroll.min(max).saturating_sub(1);
                    }
                    KeyCode::Down => {
                        let max = app.plugin.detail_max_scroll.get();
                        app.plugin.detail_scroll =
                            app.plugin.detail_scroll.saturating_add(1).min(max);
                    }
                    KeyCode::Char('f') => apply_plugin_fix(app),
                    _ => {}
                }
            }
        }
    }
}

/// Apply the selected row's fix. `WireMcpServers` opens a confirm modal; a
/// diverged active profile re-raises the existing 3-way divergence resolver.
fn apply_plugin_fix(app: &mut App) {
    let Some(fix) = app.plugin.selected_fix().cloned() else {
        return;
    };
    match fix {
        PluginFix::WireMcpServers => {
            app.disarm_quit();
            app.modals.push(Modal::Confirm(ConfirmState {
                message: "wire clauth into claude code's mcpServers?".to_string(),
                detail: Some(
                    "writes the clauth entry into ~/.claude.json; other fields are preserved."
                        .to_string(),
                ),
                choice: false,
                on_confirm: ConfirmAction::WireMcpServers,
            }));
        }
        PluginFix::RepairDivergence(name) => {
            app.disarm_quit();
            open_divergence_modal(app, &name);
        }
        PluginFix::RelinkCredentials(name) => {
            app.disarm_quit();
            app.modals.push(Modal::Confirm(ConfirmState {
                message: format!("relink ~/.claude credentials to '{name}'?"),
                detail: Some(
                    "re-points .credentials.json at the account's own stored tokens; spends nothing.".to_string(),
                ),
                choice: false,
                on_confirm: ConfirmAction::RelinkCredentials(name),
            }));
        }
        PluginFix::HealHerdrConfig(path) => {
            app.disarm_quit();
            app.modals.push(Modal::Confirm(ConfirmState {
                message: "add the keybinding and sidebar row to herdr's config?".to_string(),
                detail: Some(
                    "writes them into herdr's config.toml and validates the result with `herdr config check`.".to_string(),
                ),
                choice: false,
                on_confirm: ConfirmAction::HealHerdrConfig(path),
            }));
        }
        PluginFix::InstallPlugin => {
            app.disarm_quit();
            app.modals.push(Modal::Confirm(ConfirmState {
                message: "install the clauth plugin into claude code?".to_string(),
                detail: Some(
                    "runs claude's own plugin installer at user scope; your other plugins and settings are untouched."
                        .to_string(),
                ),
                choice: false,
                on_confirm: ConfirmAction::InstallPlugin,
            }));
        }
    }
}

/// The herdr detail's options keymap: ↑↓ walks the six rows (wrapping), space
/// and ⏎ activate the focused row, `+`/`-` step the tag-refresh stepper, `f`
/// still applies the check's fix. Only the herdr detail routes here — every
/// other detail keeps the scroll-only keymap.
fn handle_herdr_options_key(app: &mut App, key: KeyEvent) {
    let cursor = app.plugin.herdr_options_cursor;
    let rows = HERDR_OPTIONS.len();
    match key.code {
        KeyCode::Up => app.plugin.herdr_options_cursor = (cursor + rows - 1) % rows,
        KeyCode::Down => app.plugin.herdr_options_cursor = (cursor + 1) % rows,
        KeyCode::Char('f') => apply_plugin_fix(app),
        KeyCode::Enter | KeyCode::Char(' ') => activate_herdr_option(app, HERDR_OPTIONS[cursor]),
        KeyCode::Char('+') => step_herdr_tag_refresh(app, 1),
        KeyCode::Char('-') => step_herdr_tag_refresh(app, -1),
        _ => {}
    }
}

/// Apply space/⏎ to the focused options row: cycle, toggle, open the tag
/// editor, or confirm the delegate-row rewrite. Every change persists
/// immediately through `save_app_state` (the `cycle_theme` shape).
fn activate_herdr_option(app: &mut App, row: HerdrOption) {
    match row {
        HerdrOption::PopupWidth => cycle_herdr_popup_width(app),
        HerdrOption::PaneTag => {
            toggle_herdr_bool(app, |h| h.pane_tag = !h.pane_tag);
            push_herdr_knob_change();
        }
        HerdrOption::TagRefresh => begin_herdr_tag_edit(app),
        HerdrOption::BorderLabel => {
            toggle_herdr_bool(app, |h| h.border_label = !h.border_label);
            push_herdr_knob_change();
        }
        HerdrOption::DelegateDot => toggle_herdr_bool(app, |h| h.delegate_dot = !h.delegate_dot),
        // Inert while herdr's config does not read+parse: the write this row
        // triggers goes through the parse, so offering it there is a promise
        // the heal cannot keep. The render draws the row faint with the reason.
        HerdrOption::DelegateRowText => {
            if herdr_config_writable(app) {
                open_herdr_row_text_confirm(app);
            }
        }
    }
}

/// True while the herdr config behind the options section reads AND parses —
/// the states where `heal` can actually rewrite the file. `None` (unreadable /
/// no config path) and `parsed == false` both read as not writable. Shared by
/// the key handler and the render so the inert row and the no-op key agree.
pub(crate) fn herdr_config_writable(app: &App) -> bool {
    app.plugin.herdr_config.as_ref().is_some_and(|c| c.parsed)
}

/// `+`/`-` on the tag-refresh stepper: ±1 second, floored at 1. A no-op on
/// every other row.
fn step_herdr_tag_refresh(app: &mut App, delta: i64) {
    if HERDR_OPTIONS[app.plugin.herdr_options_cursor] != HerdrOption::TagRefresh {
        return;
    }
    {
        let mut cfg = app.config();
        let cur = cfg.state.herdr.tag_watch_secs;
        cfg.state.herdr.tag_watch_secs = if delta > 0 {
            cur.saturating_add(1)
        } else {
            cur.saturating_sub(1).max(1)
        };
        let _ = save_app_state(&cfg.state);
    }
    app.last_reload_fp = reload_fingerprint();
}

/// Flip one herdr bool knob and persist immediately (the `cycle_theme` save
/// shape — no separate save step).
fn toggle_herdr_bool(app: &mut App, flip: impl FnOnce(&mut HerdrSettings)) {
    {
        let mut cfg = app.config();
        flip(&mut cfg.state.herdr);
        let _ = save_app_state(&cfg.state);
    }
    app.last_reload_fp = reload_fingerprint();
}

/// Push a `pane_tag`/`border_label` change onto the live herdr panes right
/// now, instead of letting it sit until the next watcher tick
/// (`tag_watch_secs` away, 5 s by default): re-run `report-profile.sh` once
/// per pane, the way `watch-profile.sh` re-runs it. The off direction clears
/// its artifact through the same re-run, so it no longer waits for the tick
/// either. Only the two knobs that script reads trigger this —
/// `delegate_dot`'s gate lives in the MCP server process (a separate binary a
/// TUI cannot hot-patch), and `tag_refresh` belongs to watchers that already
/// hold their own interval, so both keep their current behavior.
///
/// Gated on the plugin-pane environment (`HERDR_ENV=1` plus the injected
/// binary and plugin-root paths): no herdr command exposes the plugin root,
/// so this gate is the only channel, and a bare `clauth` TUI inside a herdr
/// pane silently skips the push too. Best-effort — a failure anywhere is
/// silent, and the change still lands with the next report (a watcher tick,
/// or the next agent-event hook for panes without one).
fn push_herdr_knob_change() {
    if std::env::var("HERDR_ENV").as_deref() != Ok("1") {
        return;
    }
    let Ok(bin) = std::env::var("HERDR_BIN_PATH") else {
        return;
    };
    let Ok(root) = std::env::var("HERDR_PLUGIN_ROOT") else {
        return;
    };
    let script = std::path::PathBuf::from(&root)
        .join("report-profile.sh")
        .to_string_lossy()
        .into_owned();
    spawn_worker(move || {
        let Some(panes) = herdr_pane_ids(&bin) else {
            return;
        };
        for pane in panes {
            rerun_pane_report(&script, &pane);
        }
    });
}

/// `bin pane list` → pane ids, parsed leniently: an unknown payload shape (a
/// herdr release moving a field) reads as no panes rather than a wrong one.
/// Bounded at `crate::herdr::PROBE_TIMEOUT` — a hung herdr delays the push
/// worker, never the key handling.
fn herdr_pane_ids(bin: &str) -> Option<Vec<String>> {
    let out = crate::herdr::bounded_output(bin, &["pane", "list"], &[])?;
    if !out.status.success() {
        return None;
    }
    let root: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&out.stdout)).ok()?;
    let panes = root.get("result")?.get("panes")?.as_array()?;
    Some(
        panes
            .iter()
            .filter_map(|pane| pane.get("pane_id")?.as_str().map(str::to_string))
            .collect(),
    )
}

/// Re-run `report-profile.sh` for one pane with the pane id set and the
/// event/context JSON cleared — the exact `watch-profile.sh` invocation. The
/// script's pidfile gate skips the watcher spawn while a live watch exists,
/// so re-running it alongside the live watchers is safe.
fn rerun_pane_report(script: &str, pane: &str) {
    let _ = crate::herdr::bounded_output(
        script,
        &[],
        &[
            ("HERDR_PANE_ID", std::ffi::OsStr::new(pane)),
            ("HERDR_PLUGIN_EVENT_JSON", std::ffi::OsStr::new("")),
            ("HERDR_PLUGIN_CONTEXT_JSON", std::ffi::OsStr::new("")),
        ],
    );
}

/// Advance the popup-width knob to the next shape, wrapping past `split-top`
/// back to `fit` — space always cycles forward, never clamps.
fn cycle_herdr_popup_width(app: &mut App) {
    {
        let mut cfg = app.config();
        cfg.state.herdr.popup_width = match cfg.state.herdr.popup_width {
            PopupWidth::Fit => PopupWidth::Half,
            PopupWidth::Half => PopupWidth::SplitRight,
            PopupWidth::SplitRight => PopupWidth::SplitTop,
            PopupWidth::SplitTop => PopupWidth::Fit,
        };
        let _ = save_app_state(&cfg.state);
    }
    app.last_reload_fp = reload_fingerprint();
}

/// Open the inline typed editor for the tag-refresh stepper, seeded with the
/// current value in whole seconds. ⏎ commits, ⎋ discards — the Config-tab
/// refresh-interval editor's mechanism.
fn begin_herdr_tag_edit(app: &mut App) {
    let secs = app.config().state.herdr.tag_watch_secs;
    app.plugin.herdr_tag_draft = Some(InputState::new(&secs.to_string()));
}

/// Keystrokes while the herdr tag-refresh editor is open: ⏎ saves, ⎋ discards.
fn handle_herdr_tag_edit_key(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Esc => app.plugin.herdr_tag_draft = None,
        KeyCode::Enter => commit_herdr_tag_edit(app),
        _ => {
            if let Some(input) = app.plugin.herdr_tag_draft.as_mut() {
                apply_input_edit(input, key);
            }
        }
    }
}

/// Parse and persist the typed tag-refresh seconds. A value under 1s keeps the
/// draft open so the inline Invalid-input treatment stays on screen — no toast.
fn commit_herdr_tag_edit(app: &mut App) {
    let Some(raw) = app.plugin.herdr_tag_draft.as_ref().map(|i| i.trimmed()) else {
        return;
    };
    let Some(secs) = parse_herdr_tag_secs(raw) else {
        return;
    };
    {
        let mut cfg = app.config();
        cfg.state.herdr.tag_watch_secs = secs;
        let _ = save_app_state(&cfg.state);
    }
    app.last_reload_fp = reload_fingerprint();
    app.plugin.herdr_tag_draft = None;
}

/// A typed tag-refresh value is valid only as a whole number of seconds, at
/// least 1 — shared by the commit path and the render's inline check.
pub(crate) fn parse_herdr_tag_secs(raw: &str) -> Option<u64> {
    let secs = raw.parse::<u64>().ok()?;
    (secs >= 1).then_some(secs)
}

/// Open the confirm before flipping `delegate row text`: the flip rewrites
/// herdr's own config (the row `clauth herdr install` appended), so it carries
/// the same confirm gate as the `[f]` heal. The copy names the delegate token
/// so the confirm says what it will write; cancel is the default choice.
fn open_herdr_row_text_confirm(app: &mut App) {
    let Some(path) = app
        .plugin
        .herdr
        .as_ref()
        .and_then(|probe| probe.as_ref())
        .and_then(|probe| probe.config_path.clone())
    else {
        return;
    };
    let turning_on = !app.config().state.herdr.delegate_row_text;
    app.disarm_quit();
    app.modals.push(Modal::Confirm(ConfirmState {
        message: if turning_on {
            "add the delegate token to herdr's sidebar row?".to_string()
        } else {
            "drop the delegate token from herdr's sidebar row?".to_string()
        },
        detail: Some(if turning_on {
            "writes $clauth_delegate into the row clauth added in herdr's config.toml, validated by `herdr config check`; a hand-owned row is left alone with a note."
                .to_string()
        } else {
            "rewrites the row clauth added in herdr's config.toml without $clauth_delegate, validated by `herdr config check`; a hand-owned row is left alone with a note."
                .to_string()
        }),
        choice: false,
        on_confirm: ConfirmAction::HerdrDelegateRowText(path),
    }));
}

/// Componentwise numeric comparison of two dotted version strings. `None` when
/// either side is not a dotted run of numbers — an unrecognisable version is
/// unknown, not stale, so a caller stays quiet rather than warning on a shape it
/// has never seen.
fn compare_versions(a: &str, b: &str) -> Option<std::cmp::Ordering> {
    let parts = |s: &str| -> Option<Vec<u64>> {
        s.split('.').map(|part| part.parse::<u64>().ok()).collect()
    };
    Some(parts(a)?.cmp(&parts(b)?))
}

/// `probed >= min` componentwise. Either side absent or unparseable reads as
/// satisfied: a false warn on an unrecognisable version is worse than staying
/// quiet.
fn version_satisfies(probed: Option<&str>, min: Option<&str>) -> bool {
    let (Some(probed), Some(min)) = (probed, min) else {
        return true;
    };
    match compare_versions(probed, min) {
        Some(ord) => ord != std::cmp::Ordering::Less,
        None => true,
    }
}

/// The Plugin tab's `herdr` row: the installed herdr's clauth plugin plus the
/// keybinding/sidebar config `clauth herdr install` adds. Pure so the verdict
/// logic unit-tests without an `App`; the caller supplies the probe and the
/// config readout (`None` when the config file could not be read at all).
pub(crate) fn herdr_check(
    probe: &crate::herdr::HerdrProbe,
    config: Option<&crate::herdr::ConfigStatus>,
) -> Check {
    let mut detail = Vec::new();
    detail.push(match &probe.version {
        Some(version) => format!("herdr: {version}"),
        None => "herdr: unknown".to_string(),
    });

    let mut danger = false;
    let mut warn = false;
    let mut fix = None;

    // Indented, because herdr's own prose carries colons ("manifest unavailable: No such file or directory") and `detail_line` splits the first `": "` into a key column: left flush, a warning renders as a field named after its first clause and widens that column for every real field above it.
    if let Some(error) = &probe.error {
        danger = true;
        detail.push(format!("  {error}"));
    }

    if let Some(entry) = &probe.entry {
        // `herdr plugin list --json` spells a linked plugin's source `local`, measured against 0.8.0; `link` is the verb that creates one, not the kind it reports.
        let local = entry.source_kind.as_deref() == Some("local");

        if !entry.warnings.is_empty() {
            danger = true;
        }
        if !entry.enabled {
            warn = true;
            detail.push("plugin: disabled".to_string());
        } else if local {
            detail.push("plugin: linked (local)".to_string());
        } else {
            detail.push("plugin: installed (github)".to_string());
        }
        if local && let Some(root) = &entry.plugin_root {
            detail.push(format!("root: {root}"));
        }
        for warning in &entry.warnings {
            detail.push(format!("  {warning}"));
        }
        if !version_satisfies(probe.version.as_deref(), entry.min_herdr_version.as_deref()) {
            warn = true;
            detail.push(format!(
                "plugin needs herdr {} or newer",
                entry.min_herdr_version.as_deref().unwrap_or("unknown")
            ));
        }

        let parsed = config.is_some_and(|c| c.parsed);
        let templated = config.is_some_and(|c| c.sidebar == crate::herdr::SidebarState::Templated);
        match config {
            Some(c) if !c.parsed => {
                warn = true;
                detail.push("herdr's config doesn't parse".to_string());
            }
            Some(c) => {
                match &c.bound_key {
                    Some(key) => detail.push(format!("key: {key}")),
                    None => {
                        warn = true;
                        detail.push("key: not bound".to_string());
                    }
                }
                if c.sidebar == crate::herdr::SidebarState::Templated {
                    detail.push("sidebar: templated".to_string());
                } else {
                    warn = true;
                    detail.push("sidebar: not templated".to_string());
                }
            }
            None => {
                warn = true;
                detail.push("herdr's config can't be read".to_string());
            }
        }

        if parsed && (config.and_then(|c| c.bound_key.as_deref()).is_none() || !templated) {
            detail.push(String::new());
            detail.push("[f] add the keybinding and sidebar row to herdr's config".to_string());
            fix = probe.config_path.clone().map(PluginFix::HealHerdrConfig);
        }
    } else if probe.error.is_none() {
        warn = true;
        detail.push("plugin: not installed".to_string());
        detail.push(String::new());
        detail.push("  clauth herdr install".to_string());
    }

    let health = if danger {
        Health::Danger
    } else if warn {
        Health::Warn
    } else {
        Health::Ok
    };

    Check {
        label: "herdr",
        health,
        detail,
        fix,
    }
}

/// Recompute the Plugin tab's integration checks; the last (`runtime`) folds every
/// profile into one summary. Every read is a local FS/`PATH` check; `claude
/// --version` runs only when `refresh_version` is set or the cached result is
/// absent. Synchronous — no background thread.
fn recompute_plugin_checks(app: &mut App, refresh_version: bool) {
    use crate::plugin_probe as probe;

    app.plugin.error = None;

    // CC version is cached; only `r` probes. Construction and a tab switch
    // leave it unprobed rather than spawning `claude --version` synchronously
    // — construction must not block the first paint. Skipped under test so
    // the suite never spawns the real `claude` binary.
    if refresh_version {
        app.plugin.fetching = true;
        app.plugin.cc_version = Some(if cfg!(test) {
            None
        } else {
            probe::cc_version()
        });
        app.plugin.fetching = false;
    }

    let mut checks: Vec<Check> = Vec::with_capacity(4);

    // about — clauth's data dir + PATH resolution (CC spawns `clauth mcp` by
    // name, so resolution is load-bearing) and the Claude Code version. Combined
    // health: clauth missing is danger (server can't start), CC missing is warn.
    let clauth_path = probe::on_path("clauth");
    let mut about_detail = vec![format!(
        "data: {}",
        crate::profile::clauth_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "\u{2014}".to_string())
    )];
    match &clauth_path {
        Some(path) => about_detail.push(format!("path: {}", path.display())),
        None => {
            about_detail.push("path: not on PATH".to_string());
            about_detail.push(
                "claude code spawns clauth mcp by name, so the server won't start".to_string(),
            );
            about_detail.push("install clauth so its bin directory is on PATH".to_string());
        }
    }
    match &app.plugin.cc_version {
        Some(Some(version)) => about_detail.push(format!("claude: {version}")),
        Some(None) => {
            about_detail.push("claude: not found".to_string());
            about_detail.push("claude --version failed or claude is not on PATH".to_string());
            about_detail.push("install claude code so the claude binary resolves".to_string());
        }
        // Unprobed is not missing: the probe is `r`-gated, so before the first
        // `r` the row names the key that fills the line in instead of claiming
        // a binary is absent.
        None => about_detail.push("claude: press r to probe".to_string()),
    }
    checks.push(Check {
        label: "about",
        health: if clauth_path.is_none() {
            Health::Danger
        } else if matches!(app.plugin.cc_version, Some(None)) {
            // Probed and missing is the only version verdict that warns; an
            // unprobed version is not evidence of anything.
            Health::Warn
        } else {
            Health::Ok
        },
        detail: about_detail,
        fix: None,
    });

    // `clauth mcp` boot self-probe — `r`-gated only (heavier than the other reads:
    // it spawns the real server). Cleared when clauth no longer resolves so a stale
    // "boots" can't linger. Skipped under test so the suite never boots the server.
    if refresh_version {
        app.plugin.fetching = true;
        app.plugin.mcp_boot = if clauth_path.is_some() {
            Some(if cfg!(test) {
                probe::McpProbe::Ok
            } else {
                probe::mcp_boots()
            })
        } else {
            None
        };
        app.plugin.fetching = false;
    }
    let mcp_boot = app.plugin.mcp_boot.clone();

    // herdr probe — three subprocesses, so it is `r`-gated like `mcp_boot`.
    // Skipped under test rather than set to a fixed value, so a test that
    // injected a probe keeps it.
    if refresh_version && !cfg!(test) {
        app.plugin.herdr = Some(crate::herdr::probe());
    }

    // "global" == active in every project: a CC `user`-scope plugin install. A
    // `local`/`project` install (or a `./.mcp.json`) binds clauth to one repo.
    let records = probe::installed_records();
    let installed = !records.is_empty();
    let plugin_global = records.iter().any(|r| r.scope.as_deref() == Some("user"));

    // mcpServers wiring — a plugin install OR a manual `mcpServers.clauth` entry.
    // Globally wired = a `user`-scope plugin or the `~/.claude.json` entry; a
    // project-scope plugin or a `./.mcp.json` wires this repo only, so it warns and
    // offers the same global write fix as a missing wiring does.
    let wiring = probe::manual_mcp_wiring();
    let wired = installed || wiring != probe::McpWiring::None;
    let manual_global = wiring == probe::McpWiring::GlobalConfig;
    // A manual `~/.claude.json` entry whose command/args no longer match the
    // canonical launch line reads as wired but won't start the current server.
    // Only the operative manual entry matters — a `user`-scope plugin install
    // supersedes it, so drift under one is moot.
    let drifted = manual_global && !plugin_global && probe::global_entry_drifted() == Some(true);
    let globally_wired = plugin_global || (manual_global && !drifted);
    let project_only = wired && !globally_wired && !drifted;
    let source = if plugin_global {
        "source: plugin install (user)"
    } else if manual_global {
        "source: ~/.claude.json (manual)"
    } else if installed {
        "source: plugin install (project)"
    } else if wiring == probe::McpWiring::ProjectFile {
        "source: ./.mcp.json (manual)"
    } else {
        "source: none"
    };
    let mut mcp_detail = vec![
        format!("present: {}", if wired { "yes" } else { "no" }),
        source.to_string(),
    ];
    match &mcp_boot {
        Some(probe::McpProbe::Ok) => mcp_detail.push("server: boots".to_string()),
        Some(probe::McpProbe::Failed(reason)) => {
            mcp_detail.push(format!("server: failed ({reason})"));
        }
        None => {}
    }
    let needs_wire = !globally_wired || drifted;
    if needs_wire {
        mcp_detail.push(String::new());
        if drifted {
            mcp_detail.push("entry doesn't match the current launch line".to_string());
        } else if project_only {
            mcp_detail.push("wired for this project only, not global".to_string());
        }
        mcp_detail.push("[f] wire mcpServers into ~/.claude.json".to_string());
    }
    let boot_failed = matches!(mcp_boot, Some(probe::McpProbe::Failed(_)));
    checks.push(Check {
        label: "mcp servers",
        health: if boot_failed {
            Health::Danger
        } else if needs_wire {
            Health::Warn
        } else {
            Health::Ok
        },
        detail: mcp_detail,
        fix: needs_wire.then_some(PluginFix::WireMcpServers),
    });

    // plugin install record — installed-only verdict (CC exposes no clean per-scope
    // "enabled" boolean, so v1 reports presence + scope, not enabled/disabled).
    let marketplace = probe::marketplace_known();
    let plugin_check = if let Some(record) = records.first() {
        let scope = record.scope.as_deref();
        let mut detail = vec![format!("installed: yes ({})", scope.unwrap_or("?"))];
        if let Some(version) = &record.version {
            detail.push(format!("version: {version}"));
        }
        if let Some(sha) = &record.git_commit_sha {
            detail.push(format!(
                "commit: {}",
                sha.chars().take(7).collect::<String>()
            ));
        }
        if let Some(at) = &record.installed_at {
            detail.push(format!(
                "installed at: {}",
                at.split('T').next().unwrap_or(at)
            ));
        }
        if let Some(project) = &record.project_path {
            detail.push(format!("project: {project}"));
        }
        if let Some(repo) = marketplace.as_ref().and_then(|m| m.repo.as_ref()) {
            detail.push(format!("marketplace: {repo}"));
        }
        if plugin_global {
            Check {
                label: "plugin",
                health: Health::Ok,
                detail,
                fix: None,
            }
        } else {
            detail.push(String::new());
            detail.push("installed for this project only, not global".to_string());
            detail.push("[f] install globally (user scope)".to_string());
            Check {
                label: "plugin",
                health: Health::Warn,
                detail,
                fix: Some(PluginFix::InstallPlugin),
            }
        }
    } else {
        let known = marketplace.is_some();
        let mut detail = vec![format!(
            "installed: no ({})",
            if known {
                "marketplace known"
            } else {
                "marketplace unknown"
            }
        )];
        if let Some(repo) = marketplace.as_ref().and_then(|m| m.repo.as_ref()) {
            detail.push(format!("marketplace: {repo}"));
        }
        detail.push(String::new());
        detail.push("[f] install the clauth plugin".to_string());
        Check {
            label: "plugin",
            health: Health::Warn,
            detail,
            fix: Some(PluginFix::InstallPlugin),
        }
    };
    checks.push(plugin_check);

    // herdr — the installed herdr's clauth plugin + keybinding/sidebar config.
    // The probe is cached (`r`-gated, three subprocesses); the config read is a
    // cheap `fs::read_to_string` that rides the tick, using the path the probe
    // already resolved. No row when herdr does not resolve or was never probed.
    if let Some(Some(probe)) = &app.plugin.herdr {
        let config = probe
            .config_path
            .as_deref()
            .map(crate::herdr::read_config)
            .and_then(Result::ok)
            .map(|text| crate::herdr::config_status(&text));
        checks.push(herdr_check(probe, config.as_ref()));
        // The options section reads this verdict (writable vs inert) off the
        // cache — recompute already paid for the file read, so neither the
        // render nor the key handler re-reads per frame.
        app.plugin.herdr_config = config;
    } else {
        // No herdr row renders without a probe, so its verdict must not
        // outlive the probe (a stale writable verdict would arm the
        // delegate-row confirm against a file that no longer resolves).
        app.plugin.herdr_config = None;
    }

    // runtime — fold every profile's live sessions / credential link / token
    // freshness into one summary row. Snapshot the names under the config lock,
    // then drop it before the FS reads (`classify_credentials_link`) so no lock
    // is held across I/O.
    struct Snap {
        name: ProfileName,
        active: bool,
        expires_at: Option<i64>,
    }
    let snaps: Vec<Snap> = {
        let cfg = app.config();
        cfg.profiles
            .iter()
            .map(|p| Snap {
                name: p.name.clone(),
                active: cfg.is_active(&p.name),
                expires_at: p.access_token_expires_at(),
            })
            .collect()
    };

    let now_secs = (crate::usage::now_ms() / 1000) as i64;
    let total = snaps.len();
    let mut live_sessions: usize = 0;
    let mut live_profiles: usize = 0;
    let mut live_names: Vec<String> = Vec::new();
    let mut rate_limited_names: Vec<String> = Vec::new();
    // The active profile's link readout plus the one fix it can offer. Divergence
    // and missing-link are meaningful only for the active profile — its creds are
    // the ones linked into ~/.claude — so non-active profiles only contribute
    // their live-session and rate-limit signal.
    let mut active_name: Option<String> = None;
    let mut active_link = "\u{2014}";
    let mut active_expires = "\u{2014}".to_string();
    let mut active_fix: Option<PluginFix> = None;
    let mut active_bad = false; // diverged / missing / unknown link

    // `r` means re-probe everything, so it re-collects rather than rendering the
    // age the tick left. Sited next to the read, not at the top: the version
    // probe above spawns `claude`, and a tally taken before that is already as
    // stale as the subprocess is slow.
    if refresh_version {
        app.last_live_sessions_refresh = None;
        poll_live_sessions(app);
    }

    // Otherwise the tick's fleet tally, never a second sweep: this row and the
    // Overview's `live` column answer one question, and two independent reads of
    // it can disagree inside a frame.
    let tally = &app.live_sessions;
    for snap in snaps {
        let instances = tally.member(&snap.name).sessions;
        live_sessions += instances;
        if instances > 0 {
            live_profiles += 1;
            live_names.push(if instances > 1 {
                format!("{} · {instances}", snap.name)
            } else {
                snap.name.to_string()
            });
        }
        // Observed delegate throughput (MCP `delegate`); a recent rate-limit on any
        // exercised model warns even when the credential link is healthy.
        let throughput = crate::throughput::summary(&snap.name, now_secs);
        if throughput.iter().any(|t| t.rate_limited_recent) {
            rate_limited_names.push(snap.name.to_string());
        }

        if !snap.active {
            continue;
        }
        active_name = Some(snap.name.to_string());

        // A classify error (broken symlink mid-read, perms) must not read as
        // healthy: surface it as a warn with an `unknown` link label rather than
        // silently dropping to idle/ok.
        let link_result = classify_credentials_link(&snap.name);
        let link = link_result.as_ref().ok().copied();
        let link_err = link_result.is_err();
        let diverged = matches!(link, Some(LinkState::Diverged));
        let missing = matches!(link, Some(LinkState::Missing));
        // A `missing` link is repairable only when the profile still holds stored
        // creds to relink to; with none it needs a fresh login, not a relink.
        let stored_creds = crate::claude::install_source_path(&snap.name)
            .map(|p| p.exists())
            .unwrap_or(false);

        active_link = if link_err {
            "unknown"
        } else {
            match link {
                Some(LinkState::LinkedTo) => "linked",
                Some(LinkState::Diverged) => "diverged",
                Some(LinkState::Missing) => "missing",
                None => "\u{2014}",
            }
        };
        active_bad = link_err || diverged || missing;
        active_fix = if diverged {
            Some(PluginFix::RepairDivergence(snap.name.to_string()))
        } else if missing && stored_creds {
            Some(PluginFix::RelinkCredentials(snap.name.to_string()))
        } else {
            None
        };
        // Access-token freshness as a relative span; `—` when no OAuth expiry is
        // known (third-party / api-key profiles).
        active_expires = match snap.expires_at {
            Some(ms) => {
                let secs = ms / 1000 - (crate::usage::now_ms() / 1000) as i64;
                if secs <= 0 {
                    "expired".to_string()
                } else {
                    crate::usage::humanize_duration(secs)
                }
            }
            None => "\u{2014}".to_string(),
        };
    }

    // Health: a bad active link (diverged/missing/unknown) or any recent delegate
    // rate-limit warns; an active `linked` creds link or any live session is ok;
    // otherwise the fleet is idle (neutral, not green).
    let runtime_health = if active_bad || !rate_limited_names.is_empty() {
        Health::Warn
    } else if active_link == "linked" || live_sessions > 0 {
        Health::Ok
    } else {
        Health::Idle
    };

    let link_line = match &active_name {
        Some(_) if active_expires != "\u{2014}" => format!("{active_link} · {active_expires}"),
        Some(_) => active_link.to_string(),
        None => "\u{2014}".to_string(),
    };
    // Runtime health only — config (type / model / overrides) lives on the Setup
    // tab. This row answers "how many live sessions, is the active credential link
    // healthy, and how fresh is its token?".
    let mut runtime_detail = vec![format!("accounts: {total}")];
    // A zero is hidden rather than printed, matching the Overview cell and the
    // Fallback card — the row says nothing when there is nothing to say. `live`
    // is the one noun every session-counting surface uses (the Overview column
    // header, the Fallback card's key), so the figure reads the same everywhere.
    if live_sessions > 0 {
        runtime_detail.push(format!(
            "live: {live_sessions} across {live_profiles} account{}",
            crate::format::plural(live_profiles)
        ));
    }
    // Name each account carrying a live session as an indented sub-line, so the
    // spread is concrete rather than just a tally.
    for name in &live_names {
        runtime_detail.push(format!("  {name}"));
    }
    runtime_detail.push(format!(
        "active: {}",
        active_name.as_deref().unwrap_or("\u{2014}")
    ));
    runtime_detail.push(format!("link: {link_line}"));
    if !rate_limited_names.is_empty() {
        // "rate-limited" sits in the value so `value_tone` warns on it (the key is
        // a plain label).
        runtime_detail.push(format!(
            "delegate: rate-limited ({})",
            rate_limited_names.join(", ")
        ));
    }
    match &active_fix {
        Some(PluginFix::RepairDivergence(_)) => {
            runtime_detail.push(String::new());
            runtime_detail.push("[f] repair credentials".to_string());
        }
        Some(PluginFix::RelinkCredentials(_)) => {
            runtime_detail.push(String::new());
            runtime_detail.push("[f] relink credentials".to_string());
        }
        // The other fixes are produced on their own check rows (wire / herdr /
        // install), never on this one; `None` is a healthy or inactive link.
        // Named, so a new `PluginFix` variant fails the build instead of a
        // silent no-op.
        Some(
            PluginFix::WireMcpServers | PluginFix::HealHerdrConfig(_) | PluginFix::InstallPlugin,
        ) => {}
        None => {}
    }
    checks.push(Check {
        label: "runtime",
        health: runtime_health,
        detail: runtime_detail,
        fix: active_fix,
    });

    app.plugin.checks = checks;

    // The delegates pane's data. Read here rather than on its own timer so it
    // rides the cadence this tab already documents (tab focus, `r`, and the 1 s
    // tick while focused) — a `clauth mcp` run is a different process, so a
    // watcher would buy freshness this tab has never promised.
    //
    // `list_banded`, the same call `clauth jobs` and `monitor`'s listing make,
    // so the pane's row order is not a second derivation of one. It arrives
    // banded and the renderer sorts nothing.
    app.plugin.delegates = crate::mcp::jobs::list_banded(crate::usage::now_ms());

    // Keep the cursor in range after the check set changes.
    let max = app.plugin.row_count().saturating_sub(1);
    if app.plugin.cursor > max {
        app.plugin.cursor = max;
    }
}

/// Open the selected incident's page in the default browser (detached).
fn open_incident_link(app: &mut App) {
    let Some(link) = app.status.selected().map(|i| i.link.clone()) else {
        return;
    };
    if link.is_empty() {
        app.toast(ToastKind::Info, "no link for this incident");
        return;
    }
    match crate::platform::open_url(&link) {
        Ok(()) => app.toast(ToastKind::Info, "opening in browser"),
        Err(_) => app.toast(ToastKind::Danger, "failed to open browser"),
    }
}

/// Request switch to profile at `idx`; no-ops if already active.
fn request_switch_to(app: &mut App, idx: usize) {
    let cfg = app.config();
    let Some(profile) = cfg.profiles.get(idx) else {
        return;
    };
    let name = profile.name.clone();
    // `switch_profile` already refuses a disabled target (shared guard), but a
    // disabled row must never even offer the confirm — never selectable, not
    // just never landed.
    if cfg.is_active(&name) || profile.is_disabled() {
        return;
    }
    drop(cfg);
    app.modals.push(Modal::Confirm(ConfirmState {
        message: format!("switch to '{name}'?"),
        detail: None,
        choice: true,
        on_confirm: ConfirmAction::Switch(name.to_string()),
    }));
}

fn activate_main_item(app: &mut App) {
    let Some(item) = app.current_main_item() else {
        return;
    };
    match item {
        MainItemKind::Profile(idx) => request_switch_to(app, idx),
    }
}

fn reorder_main_cursor(app: &mut App, delta: i32) {
    let Some(MainItemKind::Profile(idx)) = app.current_main_item() else {
        return;
    };
    let new_idx = match delta.signum() {
        -1 if idx > 0 => idx - 1,
        1 if idx + 1 < app.config().profiles.len() => idx + 1,
        _ => return,
    };
    let result = {
        let mut cfg = app.config();
        reorder_profile(&mut cfg, idx, new_idx)
    };
    if let Err(e) = result {
        app.toast(ToastKind::Danger, format!("reorder failed\n{e}"));
        return;
    }
    if delta < 0 && app.profile_cursor > 0 {
        app.profile_cursor -= 1;
    } else if delta > 0 {
        app.profile_cursor += 1;
    }
}

/// One off-thread AUTH-1 switch-gate answer, posted by `spawn_switch_gate`'s
/// worker and drained by `drain_switch_gates`.
pub(crate) struct SwitchGateResult {
    name: ProfileName,
    gate: oauth::AuthGate,
}

/// Switch the active profile to `name`. The AUTH-1 pre-install gate
/// (`ensure_installable`) may refresh the target's token over HTTP, so it runs
/// off the UI thread; `drain_switch_gates` completes the relink when it
/// answers. The already-active target skips the gate — nothing new to install
/// (`switch_profile` no-ops on `is_active`), and gating it races a live
/// `claude` refreshing through the symlink — the same exemption as the
/// CLI/MCP paths.
fn perform_switch(app: &mut App, name: &ProfileName) {
    let active = app.config().state.active_profile.as_ref().cloned();
    if active.as_deref() == Some(name) {
        finalize_switch(app, name);
        return;
    }
    spawn_switch_gate(app, name.clone(), oauth::refresh_result);
}

/// Run `ensure_installable` for `name` off the UI thread and post the answer
/// to the switch-gate channel. The `Switching` activity mark is the pending
/// state: it shows the spinner and blocks further switches until the drain
/// clears it. `refresher` is injected so tests gate offline; under `cfg(test)`
/// the gate runs synchronously — a detached worker would race the test's
/// `HomeSandbox` (the gate persists `auth_broken` transitions to home paths).
fn spawn_switch_gate<F>(app: &mut App, name: ProfileName, refresher: F)
where
    F: Fn(&str, Option<&str>) -> std::result::Result<oauth::TokenResponse, oauth::RefreshError>
        + Send
        + 'static,
{
    mark_activity(&app.activity, &name, ProfileActivity::Switching);
    let config = Arc::clone(&app.config);
    let sender = app.switch_gate_tx.clone();
    let gate = move || {
        let gate = oauth::ensure_installable(&config, &name, refresher);
        let _ = sender.send(SwitchGateResult { name, gate });
    };
    if cfg!(test) {
        gate()
    } else {
        spawn_worker(gate)
    }
}

/// True when the live credentials diverge from the stored chain and it's not a
/// first-login adoption (must be reconciled before clearing/relinking). A
/// logged-out shell is exempt — an empty login needs no reconciling.
fn active_diverged_unsaved(active: &ProfileName) -> bool {
    crate::claude::live_diverged_and_unsaved(active).unwrap_or(false)
}

/// Toast and raise the Divergence prompt for `active` (`verb` = blocked action).
fn prompt_divergence(app: &mut App, active: String, verb: &str) {
    app.toast(
        ToastKind::Warning,
        format!("'{active}' has unsaved claude code credentials\nresolve before {verb}"),
    );
    open_divergence_modal(app, &active);
}

/// Push the Divergence prompt, first identifying (locally) which profile the
/// live login belongs to so the near-always-right "switch to it" action can
/// lead the menu. The active profile owning its own newer login is NOT a
/// sibling (that is the adopt path's self-healing domain).
fn open_divergence_modal(app: &mut App, active: &str) {
    let sibling = {
        let cfg = app.config();
        crate::actions::identify_live_login_owner(&cfg).filter(|owner| owner != active)
    };
    app.modals.push(Modal::Divergence(DivergenceForm {
        active: active.to_string(),
        sibling,
        cursor: 0,
    }));
}

/// Complete a switch on the UI thread: divergence guard, relink via
/// `switch_profile`, token-snapshot refresh. No HTTP — the AUTH-1 gate has
/// already answered (or the target is the already-active exemption) by the
/// time this runs.
fn finalize_switch(app: &mut App, name: &ProfileName) {
    // Guard a diverged outgoing active: `switch_profile` would no-op the
    // snapshot and then `link_profile_credentials` would bail on the regular
    // file, stranding the fresh `/login` chain. Raise the Divergence modal so
    // the user cleans up first; first-login adoption stays a clean switch.
    let outgoing = app.config().state.active_profile.as_ref().cloned();
    if let Some(active) = outgoing
        && active != *name
        && active_diverged_unsaved(&active)
    {
        clear_activity(&app.activity, name);
        prompt_divergence(app, active.to_string(), "switching");
        return;
    }
    let result = {
        let mut cfg = app.config();
        switch_profile(&mut cfg, name)
    };
    clear_activity(&app.activity, name);
    match result {
        Ok(()) => {
            app.refresh_tokens();
            app.last_reload_fp = reload_fingerprint();
            app.toast(ToastKind::Success, format!("switched to '{name}'"));
        }
        Err(e) => app.toast(ToastKind::Danger, format!("switch failed\n{e}")),
    }
}

/// Turn off all accounts on the UI thread. Mirrors `finalize_switch`'s
/// divergence guard: an unsaved `/login` must be resolved before clearing live
/// credentials. No HTTP.
fn perform_switch_off(app: &mut App) {
    let Some(active) = app.config().state.active_profile.as_ref().cloned() else {
        return;
    };
    if active_diverged_unsaved(&active) {
        prompt_divergence(app, active.to_string(), "switching off");
        return;
    }
    let result = {
        let mut cfg = app.config();
        switch_off(&mut cfg)
    };
    match result {
        Ok(()) => {
            app.refresh_tokens();
            app.last_reload_fp = reload_fingerprint();
            app.toast(
                ToastKind::Warning,
                "all accounts spent\nswitched off to halt usage".to_string(),
            );
        }
        Err(e) => app.toast(ToastKind::Danger, format!("switch-off failed\n{e}")),
    }
}

/// Read the live login into a snapshot, or toast + `None` when there's nothing
/// to capture. An all-empty snapshot would persist a credential-less profile
/// behind a success toast (issue #1 — on macOS CC keeps its login in the
/// Keychain, so the credentials file is absent).
fn capture_live_or_toast(app: &mut App) -> Option<CaptureSnapshot> {
    let snapshot = match capture_snapshot() {
        Ok(s) => s,
        Err(e) => {
            app.toast(ToastKind::Danger, format!("capture failed\n{e}"));
            return None;
        }
    };
    let has_oauth = snapshot
        .credentials
        .as_ref()
        .is_some_and(|c| c.claude_ai_oauth.is_some());
    if !has_oauth && snapshot.base_url.is_none() && snapshot.api_key.is_none() {
        // The Keychain caveat only makes sense on macOS, where a live login can
        // hide in the Keychain clauth doesn't read; elsewhere it's noise.
        let msg = if cfg!(target_os = "macos") {
            "no live login found\nnothing to capture (macos keychain isn't supported yet)"
        } else {
            "no live login found\nnothing to capture"
        };
        app.toast(ToastKind::Danger, msg);
        return None;
    }
    Some(snapshot)
}

fn begin_capture(app: &mut App, from_divergence: bool) {
    let Some(snapshot) = capture_live_or_toast(app) else {
        return;
    };
    let existing_match = {
        let cfg = app.config();
        find_matching_oauth_profile(&cfg, snapshot.credentials.as_ref())
    };
    if let Some(existing) = existing_match {
        app.modals.push(Modal::Confirm(ConfirmState {
            message: format!("these credentials already belong to '{existing}'."),
            detail: Some("capture anyway?".to_string()),
            choice: false,
            on_confirm: ConfirmAction::CaptureConflict(Box::new(snapshot), from_divergence),
        }));
        return;
    }
    app.modals.push(Modal::CaptureName(CaptureNameForm {
        snapshot: Box::new(snapshot),
        input: InputState::new(""),
        from_divergence,
    }));
}

// ── Chain screen ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy)]
pub(crate) enum ChainItemKind {
    Member(usize),
    Add,
}

pub(crate) fn chain_items(app: &App) -> Vec<ChainItemKind> {
    let cfg = app.config();
    let mut items: Vec<ChainItemKind> = cfg
        .state
        .fallback_chain
        .iter()
        .enumerate()
        .map(|(i, _)| ChainItemKind::Member(i))
        .collect();
    // A disabled, unchained profile is never an add-picker candidate (see
    // `chain_candidates`) — excluding it here too keeps `+ add` from showing
    // when it would open onto an empty picker.
    let any_unchained = cfg
        .profiles
        .iter()
        .any(|p| !p.is_disabled() && !cfg.state.fallback_chain.iter().any(|c| c == &p.name));
    if any_unchained {
        items.push(ChainItemKind::Add);
    }
    items
}

/// Detail rows for a chain member: threshold stepper, last-resort/preferred
/// toggles, remove.
pub(crate) const FALLBACK_ROWS: [FallbackRow; 8] = [
    FallbackRow::Threshold,
    FallbackRow::WeeklyAt,
    FallbackRow::CheckWeekly,
    FallbackRow::CheckScoped,
    FallbackRow::LastResort,
    FallbackRow::Preferred,
    FallbackRow::MaxSpend,
    FallbackRow::Remove,
];

/// Rows on the program-wide Config tab, in display order. Related knobs sit
/// together instead of interleaving halt above detection; [`GlobalConfigRow::band`]
/// names each run, and the renderer turns a band change into an eyebrow header.
pub(crate) const GLOBAL_CONFIG_ROWS: [GlobalConfigRow; 14] = [
    GlobalConfigRow::Theme,
    GlobalConfigRow::ResetShape,
    GlobalConfigRow::ClockNotation,
    GlobalConfigRow::DivergenceDefault,
    GlobalConfigRow::RefreshInterval,
    GlobalConfigRow::RefreshSpentAccounts,
    GlobalConfigRow::PreemptiveRotation,
    GlobalConfigRow::WeeklyThreshold,
    GlobalConfigRow::BurnAware,
    GlobalConfigRow::BurnFloor,
    GlobalConfigRow::BurnHorizon,
    GlobalConfigRow::SwitchOffWhenSpent,
    GlobalConfigRow::SpendBudget,
    GlobalConfigRow::SwitchOffWhenBudgetSpent,
];

impl GlobalConfigRow {
    /// The concern this row belongs to. Rows sharing a band are contiguous in
    /// [`GLOBAL_CONFIG_ROWS`]; the Config renderer opens each run with an eyebrow
    /// header, so a band that stops being contiguous would render twice — which
    /// is what `config_bands_stay_contiguous` pins.
    pub(crate) fn band(self) -> &'static str {
        match self {
            GlobalConfigRow::Theme
            | GlobalConfigRow::ResetShape
            | GlobalConfigRow::ClockNotation => "appearance",
            GlobalConfigRow::DivergenceDefault
            | GlobalConfigRow::RefreshInterval
            | GlobalConfigRow::RefreshSpentAccounts
            | GlobalConfigRow::PreemptiveRotation => "scheduler",
            GlobalConfigRow::WeeklyThreshold
            | GlobalConfigRow::BurnAware
            | GlobalConfigRow::BurnFloor
            | GlobalConfigRow::BurnHorizon
            | GlobalConfigRow::SwitchOffWhenSpent => "auto-switch",
            GlobalConfigRow::SpendBudget | GlobalConfigRow::SwitchOffWhenBudgetSpent => {
                "extra usage"
            }
        }
    }
}

/// Config tab keymap (enumerated rows only, per the unified value-row grammar):
/// ↑↓ walks rows; space cycles every row's value forward, wrapping the top
/// value back to the first; ⏎ opens the refresh-interval and weekly-threshold
/// custom-value editors and otherwise mirrors space. No row here binds `+`/`-`
/// (that's reserved for the Fallback tab's continuous `rotate at` threshold).
fn handle_global_config_key(app: &mut App, key: KeyEvent) {
    let last = GLOBAL_CONFIG_ROWS.len() - 1;
    app.global_config_cursor = app.global_config_cursor.min(last);
    match key.code {
        KeyCode::Up => {
            app.global_config_cursor = if app.global_config_cursor == 0 {
                last
            } else {
                app.global_config_cursor - 1
            };
        }
        KeyCode::Down => {
            app.global_config_cursor = if app.global_config_cursor >= last {
                0
            } else {
                app.global_config_cursor + 1
            };
        }
        KeyCode::Char(' ') => {
            run_global_config_row(app, GLOBAL_CONFIG_ROWS[app.global_config_cursor]);
        }
        KeyCode::Enter => {
            let row = GLOBAL_CONFIG_ROWS[app.global_config_cursor];
            if row == GlobalConfigRow::RefreshInterval {
                begin_refresh_interval_edit(app);
            } else if row == GlobalConfigRow::WeeklyThreshold {
                begin_weekly_threshold_edit(app);
            } else {
                run_global_config_row(app, row);
            }
        }
        _ => {}
    }
}

/// Apply space (or ⏎ on non-refresh rows) on a Config-tab row: cycle the theme
/// tier, flip wrap-off, or step the refresh interval forward through presets
/// (wrapping past the top preset back to the first).
fn run_global_config_row(app: &mut App, row: GlobalConfigRow) {
    match row {
        GlobalConfigRow::Theme => cycle_theme(app),
        GlobalConfigRow::ResetShape => cycle_reset_display(app),
        // Inert while the countdown is relative (rendered dimmed): no surface
        // draws a clock then, so the notation would decide nothing.
        GlobalConfigRow::ClockNotation => {
            if app.config().state.reset_display().shows_clock() {
                cycle_clock_format(app);
            }
        }
        GlobalConfigRow::DivergenceDefault => cycle_divergence_default(app),
        GlobalConfigRow::SwitchOffWhenSpent => toggle_wrap_off(app),
        GlobalConfigRow::WeeklyThreshold => step_weekly_threshold(app),
        GlobalConfigRow::RefreshInterval => step_refresh_interval(app),
        GlobalConfigRow::BurnAware => toggle_burn_aware_switching(app),
        // Inert while burn-aware is off (rendered dimmed): the floor/cap only
        // shape the projection, which the static path never runs.
        GlobalConfigRow::BurnFloor => {
            if app.config().state.burn_aware_switching {
                step_burn_floor(app);
            }
        }
        GlobalConfigRow::BurnHorizon => {
            if app.config().state.burn_aware_switching {
                step_burn_horizon(app);
            }
        }
        GlobalConfigRow::SpendBudget => toggle_spend_budget_switching(app),
        // Inert while spend budget is off (rendered dimmed): decides no halt when
        // nothing spends, so it stays a true disabled row — the key is a no-op.
        GlobalConfigRow::SwitchOffWhenBudgetSpent => {
            if app.config().state.spend_budget_switching {
                toggle_budget_wrap_off(app);
            }
        }
        GlobalConfigRow::PreemptiveRotation => toggle_preemptive_rotation(app),
        GlobalConfigRow::RefreshSpentAccounts => toggle_refresh_spent_accounts(app),
    }
}

/// Cycle the active theme tier, persist it to `[theme]`, and live-swap the
/// palette so the next frame renders in the new tier without a restart.
fn cycle_theme(app: &mut App) {
    let next = match theme::tier() {
        theme::Tier::Full => theme::Tier::Compatible,
        theme::Tier::Compatible => theme::Tier::Full,
    };
    let name = match next {
        theme::Tier::Full => ThemeName::Full,
        theme::Tier::Compatible => ThemeName::Compatible,
    };
    {
        let mut cfg = app.config();
        cfg.state.theme = Some(name);
        let _ = save_app_state(&cfg.state);
    }
    app.last_reload_fp = reload_fingerprint();
    theme::set_tier(next);
}

/// Fallback footer hint derived from current focus + selection + edit state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FallbackHint {
    Empty,
    ChainMember,
    ChainAdd,
    DetailThreshold,
    DetailThresholdEdit,
    DetailWeeklyAt,
    DetailWeeklyAtEdit,
    DetailCheckWeekly,
    DetailCheckScoped,
    DetailLastResort,
    DetailPreferred,
    DetailMaxSpend,
    DetailMaxSpendEdit,
    DetailRemove,
    DetailRemoveArmed,
    DetailAdd,
}

/// Resolve the Fallback tab's footer hint.
pub(crate) fn fallback_hint(app: &App) -> FallbackHint {
    if chain_items(app).is_empty() {
        return FallbackHint::Empty;
    }
    match app.fallback_focus {
        FallbackFocus::Chain => match selected_chain_member(app) {
            Some(_) => FallbackHint::ChainMember,
            None => FallbackHint::ChainAdd,
        },
        FallbackFocus::Detail => {
            if selected_chain_member(app).is_none() {
                return FallbackHint::DetailAdd;
            }
            if app.fallback_threshold_draft.is_some() {
                return FallbackHint::DetailThresholdEdit;
            }
            if app.fallback_max_spend_draft.is_some() {
                return FallbackHint::DetailMaxSpendEdit;
            }
            if app.fallback_weekly_draft.is_some() {
                return FallbackHint::DetailWeeklyAtEdit;
            }
            let cursor = app.fallback_detail_cursor.min(FALLBACK_ROWS.len() - 1);
            match FALLBACK_ROWS[cursor] {
                FallbackRow::Threshold => FallbackHint::DetailThreshold,
                FallbackRow::WeeklyAt => FallbackHint::DetailWeeklyAt,
                FallbackRow::CheckWeekly => FallbackHint::DetailCheckWeekly,
                FallbackRow::CheckScoped => FallbackHint::DetailCheckScoped,
                FallbackRow::LastResort => FallbackHint::DetailLastResort,
                FallbackRow::Preferred => FallbackHint::DetailPreferred,
                FallbackRow::MaxSpend => FallbackHint::DetailMaxSpend,
                FallbackRow::Remove if app.fallback_armed_remove => FallbackHint::DetailRemoveArmed,
                FallbackRow::Remove => FallbackHint::DetailRemove,
            }
        }
    }
}

/// Fallback tab keymap; delegates to chain or detail handler.
fn handle_fallback_key(app: &mut App, key: KeyEvent) {
    match app.fallback_focus {
        FallbackFocus::Chain => handle_fallback_chain_key(app, key),
        FallbackFocus::Detail => handle_fallback_detail_key(app, key),
    }
}

/// Returns the chain row that should be highlighted when entering the fallback tab.
/// If the currently-selected profile (`profile_cursor`) is a member of the chain,
/// returns its position there; otherwise 0.
fn chain_cursor_for_profile(app: &App) -> usize {
    let cfg = app.config();
    let selected_name = cfg
        .profiles
        .get(app.profile_cursor)
        .map(|p| p.name.as_str());
    if let Some(name) = selected_name
        && let Some(pos) = cfg.state.fallback_chain.iter().position(|c| c == name)
    {
        return pos;
    }
    0
}

/// If the current `chain_cursor` points at a `Member`, sync `profile_cursor` to that
/// member's index in the profile list. Leaves `profile_cursor` unchanged on `Add` or
/// when the name is not found (e.g. empty chain).
fn sync_profile_from_chain(app: &mut App) {
    let chain_pos = app.chain_cursor;
    let profile_idx = {
        let cfg = app.config();
        match cfg.state.fallback_chain.get(chain_pos) {
            Some(name) => cfg.profiles.iter().position(|p| p.name == *name),
            None => None,
        }
    };
    if let Some(idx) = profile_idx {
        app.profile_cursor = idx;
    }
}

fn handle_fallback_chain_key(app: &mut App, key: KeyEvent) {
    let last = chain_items(app).len().saturating_sub(1);
    match key.code {
        KeyCode::Up if key.modifiers.contains(KeyModifiers::SHIFT) => reorder_chain_member(app, -1),
        KeyCode::Down if key.modifiers.contains(KeyModifiers::SHIFT) => {
            reorder_chain_member(app, 1)
        }
        KeyCode::Up => {
            app.chain_cursor = if app.chain_cursor == 0 {
                last
            } else {
                app.chain_cursor - 1
            };
            sync_profile_from_chain(app);
        }
        KeyCode::Down => {
            app.chain_cursor = if app.chain_cursor >= last {
                0
            } else {
                app.chain_cursor + 1
            };
            sync_profile_from_chain(app);
        }
        KeyCode::Enter => enter_fallback_detail(app),
        _ => {}
    }
}

pub(crate) fn next_divergence_default(
    current: Option<DivergenceChoice>,
) -> Option<DivergenceChoice> {
    match current {
        None => Some(DivergenceChoice::Overwrite),
        Some(DivergenceChoice::Overwrite) => Some(DivergenceChoice::NewProfile),
        Some(DivergenceChoice::NewProfile) => Some(DivergenceChoice::Discard),
        Some(DivergenceChoice::Discard) => None,
    }
}

/// Reset-shape cycle order, wrapping back to the stock relative form.
pub(crate) fn next_reset_display(current: ResetDisplay) -> ResetDisplay {
    match current {
        ResetDisplay::Relative => ResetDisplay::Clock,
        ResetDisplay::Clock => ResetDisplay::Both,
        ResetDisplay::Both => ResetDisplay::Relative,
    }
}

fn cycle_reset_display(app: &mut App) {
    let next = next_reset_display(app.config().state.reset_display());
    {
        let mut cfg = app.config();
        cfg.state.reset_display = Some(next);
        let _ = save_app_state(&cfg.state);
    }
    app.last_reload_fp = reload_fingerprint();
}

fn cycle_clock_format(app: &mut App) {
    let next = match app.config().state.clock_format() {
        ClockFormat::H24 => ClockFormat::H12,
        ClockFormat::H12 => ClockFormat::H24,
    };
    {
        let mut cfg = app.config();
        cfg.state.clock_format = Some(next);
        let _ = save_app_state(&cfg.state);
    }
    app.last_reload_fp = reload_fingerprint();
}

fn cycle_divergence_default(app: &mut App) {
    let next = next_divergence_default(app.config().state.default_divergence);
    {
        let mut cfg = app.config();
        cfg.state.default_divergence = next;
        let _ = save_app_state(&cfg.state);
    }
    app.last_reload_fp = reload_fingerprint();
}

fn toggle_wrap_off(app: &mut App) {
    {
        let mut cfg = app.config();
        cfg.state.switch_off_when_spent = !cfg.state.switch_off_when_spent;
        let _ = save_app_state(&cfg.state);
    }
    app.last_reload_fp = reload_fingerprint();
}

/// Flip the opt-in burn-aware auto-switch mode (issue #8 follow-up b). Shares
/// `switch_off_when_spent`'s persistence shape exactly: mutate the shared `AppConfig`,
/// `save_app_state`, bump `last_reload_fp` — no separate propagation to the
/// scheduler is needed since both `next_target` and `next_auto_switch_target`
/// read the flag straight off the same shared `config` (`snapshot_chain`
/// mirrors `switch_off_when_spent`'s copy into `ChainSnapshot`).
fn toggle_burn_aware_switching(app: &mut App) {
    {
        let mut cfg = app.config();
        cfg.state.burn_aware_switching = !cfg.state.burn_aware_switching;
        let _ = save_app_state(&cfg.state);
    }
    app.last_reload_fp = reload_fingerprint();
}

/// Step the burn-aware floor forward through [`BURN_FLOOR_PRESETS`] (space on
/// the Config row), wrapping past the top back to the first — the same
/// segmented-control grammar as the weekly row. Only reachable while burn-aware
/// is on (the row is inert otherwise). Persists the `Option` field so an unset
/// default keeps `skip_serializing_if` omitting it until the first change.
fn step_burn_floor(app: &mut App) {
    let current = app.config().state.burn_switch_floor_pct();
    let next = BURN_FLOOR_PRESETS
        .iter()
        .copied()
        .find(|&p| p > current)
        .unwrap_or(BURN_FLOOR_PRESETS[0]);
    {
        let mut cfg = app.config();
        cfg.state.burn_switch_floor_pct = Some(next);
        let _ = save_app_state(&cfg.state);
    }
    app.last_reload_fp = reload_fingerprint();
}

/// Step the burn-aware horizon cap forward through [`BURN_HORIZON_PRESETS`].
/// Same grammar + persistence as [`step_burn_floor`].
fn step_burn_horizon(app: &mut App) {
    let current = app.config().state.burn_horizon_cap_ms();
    let next = BURN_HORIZON_PRESETS
        .iter()
        .copied()
        .find(|&p| p > current)
        .unwrap_or(BURN_HORIZON_PRESETS[0]);
    {
        let mut cfg = app.config();
        cfg.state.burn_horizon_cap_ms = Some(next);
        let _ = save_app_state(&cfg.state);
    }
    app.last_reload_fp = reload_fingerprint();
}

/// Flip the master half of the real-money opt-in
/// (`AppState.spend_budget_switching`). Same persistence shape as
/// `toggle_burn_aware_switching`; both walk twins read the flag off the shared
/// config (the UI twin directly, the scheduler's through `snapshot_chain`), so
/// no separate propagation is needed. Flipping this on alone still spends
/// nothing — every member's ceiling defaults to $0.
fn toggle_spend_budget_switching(app: &mut App) {
    {
        let mut cfg = app.config();
        cfg.state.spend_budget_switching = !cfg.state.spend_budget_switching;
        let _ = save_app_state(&cfg.state);
    }
    app.last_reload_fp = reload_fingerprint();
}

/// Flip what happens once a billing account has spent its budget
/// (`AppState.switch_off_when_budget_spent`). Same persistence shape as
/// `toggle_spend_budget_switching`.
fn toggle_budget_wrap_off(app: &mut App) {
    {
        let mut cfg = app.config();
        cfg.state.switch_off_when_budget_spent = !cfg.state.switch_off_when_budget_spent;
        let _ = save_app_state(&cfg.state);
    }
    app.last_reload_fp = reload_fingerprint();
}

/// Flip preemptive rotation (on by default, and profile-wide — not per-account).
/// Same persistence shape as `toggle_burn_aware_switching`; the scheduler reads
/// the flag off the shared config each rotation-leg entry, so no separate
/// propagation is needed.
fn toggle_preemptive_rotation(app: &mut App) {
    {
        let mut cfg = app.config();
        cfg.state.preemptive_rotation = !cfg.state.preemptive_rotation;
        let _ = save_app_state(&cfg.state);
    }
    app.last_reload_fp = reload_fingerprint();
}

/// Flip whether the background fetch polls spent (100%-capped) accounts
/// (`refresh_spent_accounts`, default on). Same persistence shape as
/// `toggle_preemptive_rotation`; the scheduler reads the flag off the shared
/// config each tick, so no separate propagation is needed.
fn toggle_refresh_spent_accounts(app: &mut App) {
    {
        let mut cfg = app.config();
        cfg.state.refresh_spent_accounts = !cfg.state.refresh_spent_accounts;
        let _ = save_app_state(&cfg.state);
    }
    app.last_reload_fp = reload_fingerprint();
}

fn toggle_show_estimates(app: &mut App) {
    {
        let mut cfg = app.config();
        cfg.state.show_estimates = !cfg.state.show_estimates;
        let _ = save_app_state(&cfg.state);
    }
    app.last_reload_fp = reload_fingerprint();
}

/// Toggle whether the Tokens tab counts cache in its token figures (persisted).
fn toggle_count_cache(app: &mut App) {
    {
        let mut cfg = app.config();
        cfg.state.count_cache = !cfg.state.count_cache;
        let _ = save_app_state(&cfg.state);
    }
    app.last_reload_fp = reload_fingerprint();
}

fn toggle_show_pace(app: &mut App) {
    {
        let mut cfg = app.config();
        cfg.state.show_pace = !cfg.state.show_pace;
        let _ = save_app_state(&cfg.state);
    }
    app.last_reload_fp = reload_fingerprint();
}

/// Advance the global refresh interval to the next-greater preset, wrapping
/// past the top back to the first — space always cycles forward, never clamps.
/// A custom off-ladder value lands on the next preset above it, not one past it.
fn step_refresh_interval(app: &mut App) {
    const PRESETS: [u64; 6] = [15_000, 30_000, 60_000, 90_000, 120_000, 300_000];
    let current = app.refresh_interval.load(Ordering::Relaxed);
    let new_interval = PRESETS
        .iter()
        .copied()
        .find(|&p| p > current)
        .unwrap_or(PRESETS[0]);
    app.refresh_interval.store(new_interval, Ordering::Relaxed);
    {
        let mut cfg = app.config();
        cfg.state.refresh_interval_ms = new_interval;
        let _ = save_app_state(&cfg.state);
    }
    app.last_reload_fp = reload_fingerprint();
}

/// Open the inline custom-value editor for the global refresh interval, seeded
/// with the current value in whole seconds. ⏎ commits, ⎋ discards.
fn begin_refresh_interval_edit(app: &mut App) {
    let secs = app.refresh_interval.load(Ordering::Relaxed) / 1000;
    app.refresh_interval_draft = Some(InputState::new(&secs.to_string()));
}

/// Keystrokes while the refresh-interval field is open: ⏎ saves, ⎋ discards.
fn handle_refresh_interval_edit_key(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Esc => app.refresh_interval_draft = None,
        KeyCode::Enter => commit_refresh_interval_edit(app),
        _ => {
            if let Some(input) = app.refresh_interval_draft.as_mut() {
                apply_input_edit(input, key);
            }
        }
    }
}

/// Parse and persist the typed custom interval. Invalid input keeps the draft
/// open so the Config card's inline Invalid-input treatment (DANGER value +
/// `└ 10–3600 s` tooltip) stays on screen until corrected — no toast.
fn commit_refresh_interval_edit(app: &mut App) {
    let Some(raw) = app.refresh_interval_draft.as_ref().map(|i| i.trimmed()) else {
        return;
    };
    let Some(ms) = parse_refresh_secs(raw) else {
        return;
    };
    app.refresh_interval.store(ms, Ordering::Relaxed);
    {
        let mut cfg = app.config();
        cfg.state.refresh_interval_ms = ms;
        let _ = save_app_state(&cfg.state);
    }
    app.last_reload_fp = reload_fingerprint();
    app.refresh_interval_draft = None;
}

/// A typed custom interval is valid only as a whole number of **seconds** that
/// lands in `MIN_REFRESH_INTERVAL_MS..=MAX_REFRESH_INTERVAL_MS` once scaled to
/// milliseconds. Shared by the commit path and the Config card's inline check.
pub(crate) fn parse_refresh_secs(raw: &str) -> Option<u64> {
    let ms = raw.parse::<u64>().ok()?.checked_mul(1000)?;
    (MIN_REFRESH_INTERVAL_MS..=MAX_REFRESH_INTERVAL_MS)
        .contains(&ms)
        .then_some(ms)
}

/// Step the weekly exhaustion line forward through the preset ladder (space on
/// the Config row), wrapping past the top back to the first — the same
/// segmented-control grammar as the refresh row. Presets mirror
/// `WEEKLY_PRESETS` in `render/global_config.rs`.
fn step_weekly_threshold(app: &mut App) {
    let current = app.config().state.weekly_switch_threshold_pct();
    let next = WEEKLY_PRESETS
        .iter()
        .copied()
        .find(|&p| p > current)
        .unwrap_or(WEEKLY_PRESETS[0]);
    {
        let mut cfg = app.config();
        cfg.state.weekly_switch_threshold = Some(next);
        let _ = save_app_state(&cfg.state);
    }
    app.last_reload_fp = reload_fingerprint();
}

/// Open the inline custom-value editor for the weekly line, seeded with the
/// current value. ⏎ commits, ⎋ discards.
fn begin_weekly_threshold_edit(app: &mut App) {
    let current = app.config().state.weekly_switch_threshold_pct();
    app.weekly_threshold_draft = Some(InputState::new(&format_weekly_pct(current)));
}

/// Keystrokes while the weekly-threshold field is open: ⏎ saves, ⎋ discards.
fn handle_weekly_threshold_edit_key(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Esc => app.weekly_threshold_draft = None,
        KeyCode::Enter => commit_weekly_threshold_edit(app),
        _ => {
            if let Some(input) = app.weekly_threshold_draft.as_mut() {
                apply_input_edit(input, key);
            }
        }
    }
}

/// Parse and persist the typed custom weekly line. Invalid input keeps the
/// draft open with the inline Invalid-input treatment, same as the refresh
/// editor — no toast.
fn commit_weekly_threshold_edit(app: &mut App) {
    let Some(raw) = app.weekly_threshold_draft.as_ref().map(|i| i.trimmed()) else {
        return;
    };
    let Some(pct) = parse_weekly_pct(raw) else {
        return;
    };
    {
        let mut cfg = app.config();
        cfg.state.weekly_switch_threshold = Some(pct);
        let _ = save_app_state(&cfg.state);
    }
    app.last_reload_fp = reload_fingerprint();
    app.weekly_threshold_draft = None;
}

/// A typed custom weekly line is valid as a finite percent (decimals allowed)
/// within `MIN_WEEKLY_SWITCH_PCT..=MAX_WEEKLY_SWITCH_PCT`. Shared by the
/// commit path and the Config card's inline check.
pub(crate) fn parse_weekly_pct(raw: &str) -> Option<f64> {
    let pct = raw.parse::<f64>().ok()?;
    (pct.is_finite() && (MIN_WEEKLY_SWITCH_PCT..=MAX_WEEKLY_SWITCH_PCT).contains(&pct))
        .then_some(pct)
}

/// Render a percent without a trailing `.0` on whole numbers — `98`, `97.5`.
pub(crate) fn format_weekly_pct(pct: f64) -> String {
    if pct.fract() == 0.0 {
        format!("{pct:.0}")
    } else {
        format!("{pct}")
    }
}

/// Right-pane keymap for a member: ↑↓ walks rows, `+`/`-` steps the threshold,
/// ⏎/space on remove arms then confirms. Delegates to add picker on `+ add`.
fn handle_fallback_detail_key(app: &mut App, key: KeyEvent) {
    if selected_chain_member(app).is_none() {
        handle_fallback_add_key(app, key);
        return;
    }
    let last = FALLBACK_ROWS.len() - 1;
    app.fallback_detail_cursor = app.fallback_detail_cursor.min(last);
    let row = FALLBACK_ROWS[app.fallback_detail_cursor];
    match key.code {
        KeyCode::Up => {
            app.fallback_armed_remove = false;
            app.fallback_detail_cursor = if app.fallback_detail_cursor == 0 {
                last
            } else {
                app.fallback_detail_cursor - 1
            };
        }
        KeyCode::Down => {
            app.fallback_armed_remove = false;
            app.fallback_detail_cursor = if app.fallback_detail_cursor >= last {
                0
            } else {
                app.fallback_detail_cursor + 1
            };
        }
        // Continuous fields only — `max spend` (a dollar ceiling) has no
        // natural step unit and stays typed-only.
        KeyCode::Char('+' | '=') if row == FallbackRow::Threshold => adjust_threshold(app, 5.0),
        KeyCode::Char('-' | '_') if row == FallbackRow::Threshold => adjust_threshold(app, -5.0),
        KeyCode::Char('+' | '=') if row == FallbackRow::WeeklyAt => adjust_weekly(app, 5.0),
        KeyCode::Char('-' | '_') if row == FallbackRow::WeeklyAt => adjust_weekly(app, -5.0),
        KeyCode::Enter | KeyCode::Char(' ') => {
            run_fallback_row(app, row);
        }
        _ => {}
    }
}

/// `+ add` detail: a candidate picker. ↑↓ walks, ⏎ adds and re-homes focus.
fn handle_fallback_add_key(app: &mut App, key: KeyEvent) {
    let candidates = chain_candidates(app);
    if candidates.is_empty() {
        leave_fallback_detail(app);
        return;
    }
    let last = candidates.len() - 1;
    app.fallback_detail_cursor = app.fallback_detail_cursor.min(last);
    match key.code {
        KeyCode::Up => {
            app.fallback_detail_cursor = if app.fallback_detail_cursor == 0 {
                last
            } else {
                app.fallback_detail_cursor - 1
            };
        }
        KeyCode::Down => {
            app.fallback_detail_cursor = if app.fallback_detail_cursor >= last {
                0
            } else {
                app.fallback_detail_cursor + 1
            };
        }
        KeyCode::Enter | KeyCode::Char(' ') => {
            let name = ProfileName::from(candidates[app.fallback_detail_cursor].clone());
            let would_mix = {
                let cfg = app.config();
                chain_would_mix(&cfg, &name)
            };
            if would_mix {
                app.modals.push(Modal::Confirm(ConfirmState {
                    message: "mixing api-key and oauth accounts can leave sessions stuck on the \
                              api account."
                        .into(),
                    detail: Some("api → oauth switches may not work until cc restarts.".into()),
                    choice: false,
                    on_confirm: ConfirmAction::AddChainCandidate(name.to_string()),
                }));
            } else {
                commit_chain_add(app, &name);
            }
        }
        _ => {}
    }
}

/// Add `name` to the chain, toast, and re-home focus. Shared by the direct
/// add path and the `AddChainCandidate` confirm callback.
fn commit_chain_add(app: &mut App, name: &ProfileName) {
    add_chain_candidate(app, name);
    app.toast(ToastKind::Success, format!("added '{name}' to chain"));
    // When the picker empties, `+ add` disappears — land on the new member.
    let remaining = chain_candidates(app);
    if remaining.is_empty() {
        leave_fallback_detail(app);
        app.chain_cursor = chain_items(app).len().saturating_sub(1);
    } else {
        app.fallback_detail_cursor = app.fallback_detail_cursor.min(remaining.len() - 1);
    }
}

/// Chain index under the cursor, or `None` on `+ add`.
fn selected_chain_member(app: &App) -> Option<usize> {
    match chain_items(app).get(app.chain_cursor).copied() {
        Some(ChainItemKind::Member(i)) => Some(i),
        _ => None,
    }
}

/// Profiles not yet in the chain (add-picker candidates). A disabled account
/// is excluded — the fallback-chain editing UI must never offer adding one
/// (it still sits in `fallback_chain` untouched if it was already a member
/// before being disabled; only new additions are blocked here).
pub(crate) fn chain_candidates(app: &App) -> Vec<String> {
    let cfg = app.config();
    cfg.profiles
        .iter()
        .filter(|p| !p.is_disabled() && !cfg.state.fallback_chain.iter().any(|c| c == &p.name))
        .map(|p| p.name.to_string())
        .collect()
}

/// Whether adding `name` to the fallback chain would mix api-key and oauth
/// accounts. CC's env-var handling breaks the api-key → oauth switch direction,
/// so a mixed chain can leave a running bare
/// `claude` session stuck on the api-key account until restart. Fires only
/// when a homogeneous chain would gain its other kind — silent on empty,
/// already-mixed, and same-kind adds.
pub(crate) fn chain_would_mix(cfg: &AppConfig, name: &ProfileName) -> bool {
    let Some(candidate) = cfg.find(name) else {
        return false;
    };
    let mut members = cfg.state.fallback_chain.iter().filter_map(|n| cfg.find(n));
    let Some(first) = members.next() else {
        return false;
    };
    let chain_kind = first.api_key.is_some();
    // A chain that already holds both kinds can't have a mix "created" by a
    // new add, so the modal would be noise.
    if members.any(|p| p.api_key.is_some() != chain_kind) {
        return false;
    }
    candidate.api_key.is_some() != chain_kind
}

/// Enter right pane for the selected chain item. No-op on `+ add` when empty.
fn enter_fallback_detail(app: &mut App) {
    match chain_items(app).get(app.chain_cursor).copied() {
        Some(ChainItemKind::Member(_)) => {}
        Some(ChainItemKind::Add) if !chain_candidates(app).is_empty() => {}
        _ => return,
    }
    app.fallback_detail_cursor = 0;
    app.fallback_armed_remove = false;
    app.fallback_focus = FallbackFocus::Detail;
}

/// Return focus to the chain list, clearing any armed remove or live edit.
fn leave_fallback_detail(app: &mut App) {
    app.fallback_focus = FallbackFocus::Chain;
    app.fallback_armed_remove = false;
    app.fallback_detail_cursor = 0;
    app.fallback_threshold_draft = None;
    app.fallback_weekly_draft = None;
}

/// ⇧↑↓: move the selected member up/down, cursor follows. No-op on `+ add`
/// or at boundary. Chain index == cursor for members (they precede `+ add`).
fn reorder_chain_member(app: &mut App, delta: i32) {
    let Some(pos) = selected_chain_member(app) else {
        return;
    };
    let target = pos as i32 + delta;
    {
        let mut cfg = app.config();
        if target < 0 || target as usize >= cfg.state.fallback_chain.len() {
            return;
        }
        cfg.state.fallback_chain.swap(pos, target as usize);
        let _ = save_app_state(&cfg.state);
    }
    app.chain_cursor = target as usize;
}

/// ⏎/space on a member detail row: threshold opens inline editor; remove arms
/// then deletes on second press.
fn run_fallback_row(app: &mut App, row: FallbackRow) {
    match row {
        FallbackRow::Threshold => {
            if let Some(current) = selected_threshold(app) {
                app.fallback_threshold_draft = Some(InputState::new(&format!("{current:.0}")));
            }
        }
        FallbackRow::WeeklyAt => {
            // Inert while the member's weekly gate is off (rendered dimmed):
            // the line isn't judged there, so typing one would silently do
            // nothing — same no-op contract as the spend-budget-off ceiling.
            if let Some((override_pct, check_weekly)) = selected_weekly_override(app)
                && check_weekly
            {
                let seed = override_pct.map(|v| format!("{v:.0}")).unwrap_or_default();
                app.fallback_weekly_draft = Some(InputState::new(&seed));
            }
        }
        FallbackRow::CheckWeekly => toggle_member_flag(app, MemberFlag::CheckWeekly),
        FallbackRow::CheckScoped => toggle_member_flag(app, MemberFlag::CheckScoped),
        FallbackRow::LastResort => toggle_last_resort(app),
        FallbackRow::Preferred => toggle_preferred(app),
        FallbackRow::MaxSpend => {
            // Inert while spend budget is off (rendered dimmed): opening the editor
            // would let a ceiling be typed that does nothing, so no-op.
            let armed = app.config().state.spend_budget_switching;
            if armed && let Some(current) = selected_max_spend(app) {
                app.fallback_max_spend_draft = Some(InputState::new(&format!("{current:.2}")));
            }
        }
        FallbackRow::Remove => {
            if app.fallback_armed_remove {
                remove_chain_member(app);
            } else {
                app.fallback_armed_remove = true;
            }
        }
    }
}

/// Keystrokes while the threshold field is open: ⏎ saves, ⎋ discards.
fn handle_fallback_threshold_edit_key(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Esc => app.fallback_threshold_draft = None,
        KeyCode::Enter => commit_threshold_edit(app),
        _ => {
            if let Some(input) = app.fallback_threshold_draft.as_mut() {
                apply_input_edit(input, key);
            }
        }
    }
}

/// Parse and persist the typed threshold (0..=100). Invalid input keeps the
/// draft open so the inline Invalid-input treatment (DANGER value + `└ max is N`
/// tooltip, rendered by the detail card) stays on screen until corrected — no toast.
fn commit_threshold_edit(app: &mut App) {
    let Some(raw) = app.fallback_threshold_draft.as_ref().map(|i| i.trimmed()) else {
        return;
    };
    let Some(value) = parse_threshold(raw) else {
        return;
    };
    write_threshold(app, value);
    app.fallback_threshold_draft = None;
}

/// A typed threshold is valid only as a number in `0..=100`. Shared by the
/// commit path and the detail card's inline Invalid-input check.
pub(crate) fn parse_threshold(raw: &str) -> Option<f64> {
    raw.parse::<f64>()
        .ok()
        .filter(|v| (0.0..=100.0).contains(v))
}

/// Keystrokes while the `weekly at` override field is open: ⏎ saves, ⎋ discards.
fn handle_fallback_weekly_edit_key(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Esc => app.fallback_weekly_draft = None,
        KeyCode::Enter => commit_weekly_edit(app),
        _ => {
            if let Some(input) = app.fallback_weekly_draft.as_mut() {
                apply_input_edit(input, key);
            }
        }
    }
}

/// Parse and persist the typed override. Invalid input keeps the draft open
/// (same no-toast treatment as the threshold editor); an EMPTY commit clears
/// the override back to the chain-wide default.
fn commit_weekly_edit(app: &mut App) {
    let Some(raw) = app.fallback_weekly_draft.as_ref().map(|i| i.trimmed()) else {
        return;
    };
    let Some(value) = parse_weekly_override(raw) else {
        return;
    };
    write_weekly_override(app, value);
    app.fallback_weekly_draft = None;
}

/// A typed override is a number in `0..=100`, or EMPTY — the explicit
/// clear-back-to-default. `Some(None)` = clear, `Some(Some(v))` = set,
/// `None` = invalid. Shared by the commit path and the detail card's inline
/// Invalid-input check.
pub(crate) fn parse_weekly_override(raw: &str) -> Option<Option<f64>> {
    if raw.is_empty() {
        return Some(None);
    }
    parse_weekly_pct(raw).map(Some)
}

/// The selected member's `(weekly_threshold override, check_weekly)`, or
/// `None` on `+ add`.
fn selected_weekly_override(app: &App) -> Option<(Option<f64>, bool)> {
    let pos = selected_chain_member(app)?;
    let cfg = app.config();
    let name = cfg.state.fallback_chain.get(pos)?;
    cfg.find(name).map(|p| (p.weekly_threshold, p.check_weekly))
}

/// Write (or clear) the selected member's weekly-line override and persist.
fn write_weekly_override(app: &mut App, value: Option<f64>) {
    let Some(pos) = selected_chain_member(app) else {
        return;
    };
    let save_err = {
        let mut cfg = app.config();
        let Some(name) = cfg.state.fallback_chain.get(pos).cloned() else {
            return;
        };
        match cfg.find_mut(&name) {
            Some(profile) => {
                profile.weekly_threshold = value;
                save_profile(profile).err()
            }
            None => None,
        }
    };
    if let Some(e) = save_err {
        app.toast(ToastKind::Danger, format!("save failed\n{e}"));
    }
}

/// Keystrokes while the `max auto-spend` field is open: ⏎ saves, ⎋ discards.
fn handle_fallback_max_spend_edit_key(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Esc => app.fallback_max_spend_draft = None,
        KeyCode::Enter => commit_max_spend_edit(app),
        _ => {
            if let Some(input) = app.fallback_max_spend_draft.as_mut() {
                apply_input_edit(input, key);
            }
        }
    }
}

/// Parse and persist the typed ceiling. Invalid input keeps the draft open, the
/// same no-toast treatment the threshold editor uses.
fn commit_max_spend_edit(app: &mut App) {
    let Some(raw) = app.fallback_max_spend_draft.as_ref().map(|i| i.trimmed()) else {
        return;
    };
    let Some(value) = parse_max_spend(raw) else {
        return;
    };
    write_max_spend(app, value);
    app.fallback_max_spend_draft = None;
}

/// A typed ceiling is valid only as a finite, non-negative number of dollars.
/// `is_finite` is the load-bearing half: `"inf"` parses as a perfectly good
/// `f64`, and an infinite ceiling means unbounded unattended spending
/// (`fallback::spend_room`). Shared by the commit path and the detail card's
/// inline Invalid-input check.
pub(crate) fn parse_max_spend(raw: &str) -> Option<f64> {
    raw.parse::<f64>()
        .ok()
        .filter(|v| v.is_finite() && *v >= 0.0)
}

/// This member's ceiling in dollars, or `None` on `+ add`. Unset reads as $0 —
/// the never-spend default.
fn selected_max_spend(app: &App) -> Option<f64> {
    let pos = selected_chain_member(app)?;
    let cfg = app.config();
    let name = cfg.state.fallback_chain.get(pos)?;
    Some(cfg.find(name).and_then(|p| p.max_auto_spend).unwrap_or(0.0))
}

/// Write the ceiling for the selected member and persist.
fn write_max_spend(app: &mut App, value: f64) {
    let Some(pos) = selected_chain_member(app) else {
        return;
    };
    let save_err = {
        let mut cfg = app.config();
        let Some(name) = cfg.state.fallback_chain.get(pos).cloned() else {
            return;
        };
        match cfg.find_mut(&name) {
            Some(profile) => {
                profile.max_auto_spend = Some(value);
                save_profile(profile).err()
            }
            None => None,
        }
    };
    if let Some(e) = save_err {
        app.toast(ToastKind::Danger, format!("save failed\n{e}"));
    }
}

/// Effective threshold for the selected member, or `None` on `+ add`.
fn selected_threshold(app: &App) -> Option<f64> {
    let pos = selected_chain_member(app)?;
    let cfg = app.config();
    let name = cfg.state.fallback_chain.get(pos)?;
    cfg.find(name).map(threshold_for)
}

/// Write threshold for the selected member and persist.
fn write_threshold(app: &mut App, value: f64) {
    let Some(pos) = selected_chain_member(app) else {
        return;
    };
    let save_err = {
        let mut cfg = app.config();
        let Some(name) = cfg.state.fallback_chain.get(pos).cloned() else {
            return;
        };
        match cfg.find_mut(&name) {
            Some(profile) => {
                profile.fallback_threshold = Some(value);
                save_profile(profile).err()
            }
            None => None,
        }
    };
    if let Some(e) = save_err {
        app.toast(ToastKind::Danger, format!("save failed\n{e}"));
    }
}

/// Step the threshold by `delta`, clamped to 0..=100, and persist.
fn adjust_threshold(app: &mut App, delta: f64) {
    if let Some(current) = selected_threshold(app) {
        write_threshold(app, (current + delta).clamp(0.0, 100.0));
    }
}

/// Step the selected member's `weekly at` override by `delta`, clamped to
/// `MIN_WEEKLY_SWITCH_PCT..=MAX_WEEKLY_SWITCH_PCT`, and persist. Mirrors
/// `run_fallback_row`'s `check_weekly` guard: inert while the member's weekly
/// gate is off (rendered dimmed), same no-op contract as a dimmed row's ⏎.
/// An unset override bases the nudge on the chain-wide resolved default so
/// the value visibly moves off what the dimmed-default row already shows.
fn adjust_weekly(app: &mut App, delta: f64) {
    let Some((override_pct, check_weekly)) = selected_weekly_override(app) else {
        return;
    };
    if !check_weekly {
        return;
    }
    let base = override_pct.unwrap_or_else(|| app.config().state.weekly_switch_threshold_pct());
    let next = (base + delta).clamp(MIN_WEEKLY_SWITCH_PCT, MAX_WEEKLY_SWITCH_PCT);
    write_weekly_override(app, Some(next));
}

/// ⏎/space on the `last resort` row: flip `Profile::last_resort` and persist.
/// The chain has ONE parking spot, so turning the mark on here clears it on
/// every other profile (radio). The target saves first (the user's intent);
/// each cleared profile then saves on its own, and a failed clear reverts only
/// that profile — the chain walk tolerates a transiently double-marked chain
/// (first marked member after the active wins), so nothing lies about disk.
/// The `refresh_tokens()` kick is not load-bearing here (the chain snapshot
/// reads the shared config directly, unlike `auto_start` which lives in the
/// `TokenList`); it's kept so every per-profile toggle path re-derives the
/// scheduler snapshot the same way.
/// Which per-member usage gate a toggle row flips (`Profile::check_weekly` /
/// `Profile::check_scoped`).
#[derive(Clone, Copy)]
enum MemberFlag {
    CheckWeekly,
    CheckScoped,
}

/// Flip a per-member usage gate and persist it; rolls back in memory when the
/// save fails so the UI never shows a state the disk doesn't hold (same
/// contract as [`toggle_last_resort`], minus its exclusivity move).
fn toggle_member_flag(app: &mut App, flag: MemberFlag) {
    let Some(pos) = selected_chain_member(app) else {
        return;
    };
    let failed = {
        let mut cfg = app.config();
        let Some(name) = cfg.state.fallback_chain.get(pos).cloned() else {
            return;
        };
        let Some(profile) = cfg.find_mut(&name) else {
            return;
        };
        fn field(p: &mut crate::profile::Profile, flag: MemberFlag) -> &mut bool {
            match flag {
                MemberFlag::CheckWeekly => &mut p.check_weekly,
                MemberFlag::CheckScoped => &mut p.check_scoped,
            }
        }
        *field(profile, flag) = !*field(profile, flag);
        match save_profile(profile) {
            Ok(()) => None,
            Err(e) => {
                *field(profile, flag) = !*field(profile, flag);
                Some(e)
            }
        }
    };
    match failed {
        None => app.refresh_tokens(),
        Some(e) => app.toast(ToastKind::Danger, format!("save failed\n{e}")),
    }
}

fn toggle_last_resort(app: &mut App) {
    enum Outcome {
        Missing,
        Saved { moved_from: Option<String> },
        SaveFailed(anyhow::Error),
    }
    let Some(pos) = selected_chain_member(app) else {
        return;
    };
    let outcome = {
        let mut cfg = app.config();
        let Some(name) = cfg.state.fallback_chain.get(pos).cloned() else {
            return;
        };
        match cfg.find_mut(&name) {
            None => Outcome::Missing,
            Some(profile) => {
                profile.last_resort = !profile.last_resort;
                let now_on = profile.last_resort;
                // Never both on one profile: "park here to the end" and "always
                // come home here" are contradictory. Clear preferred before the
                // single save so the pair persists atomically; remember the
                // prior value so a save failure rolls back both.
                let cleared_preferred = now_on && profile.preferred;
                if now_on {
                    profile.preferred = false;
                }
                match save_profile(profile) {
                    Ok(()) => {
                        let mut moved_from = None;
                        if now_on {
                            for p in cfg
                                .profiles
                                .iter_mut()
                                .filter(|p| p.last_resort && p.name != name)
                            {
                                p.last_resort = false;
                                match save_profile(p) {
                                    Ok(()) => {
                                        moved_from.get_or_insert_with(|| p.name.to_string());
                                    }
                                    Err(_) => p.last_resort = true,
                                }
                            }
                        }
                        Outcome::Saved { moved_from }
                    }
                    Err(e) => {
                        if let Some(p) = cfg.find_mut(&name) {
                            p.last_resort = !now_on;
                            if cleared_preferred {
                                p.preferred = true;
                            }
                        }
                        Outcome::SaveFailed(e)
                    }
                }
            }
        }
    };
    match outcome {
        Outcome::Missing => {}
        Outcome::Saved { moved_from } => {
            if let Some(prev) = moved_from {
                app.toast(ToastKind::Info, format!("last resort moved from '{prev}'"));
            }
            app.refresh_tokens();
        }
        Outcome::SaveFailed(e) => app.toast(ToastKind::Danger, format!("save failed\n{e}")),
    }
}

/// Mark the selected member as the chain's preferred (home) account, or clear
/// it. Twin of [`toggle_last_resort`]: exclusive across the chain (target-first
/// radio, per-sibling rollback) with a `"preferred moved from '{prev}'"` toast,
/// and reciprocally mutually exclusive — turning preferred on clears
/// last_resort on the same profile.
fn toggle_preferred(app: &mut App) {
    enum Outcome {
        Missing,
        Saved { moved_from: Option<String> },
        SaveFailed(anyhow::Error),
    }
    let Some(pos) = selected_chain_member(app) else {
        return;
    };
    let outcome = {
        let mut cfg = app.config();
        let Some(name) = cfg.state.fallback_chain.get(pos).cloned() else {
            return;
        };
        match cfg.find_mut(&name) {
            None => Outcome::Missing,
            Some(profile) => {
                profile.preferred = !profile.preferred;
                let now_on = profile.preferred;
                // Reciprocal of the clear in `toggle_last_resort` — the two
                // flags never coexist on one profile.
                let cleared_last_resort = now_on && profile.last_resort;
                if now_on {
                    profile.last_resort = false;
                }
                match save_profile(profile) {
                    Ok(()) => {
                        let mut moved_from = None;
                        if now_on {
                            for p in cfg
                                .profiles
                                .iter_mut()
                                .filter(|p| p.preferred && p.name != name)
                            {
                                p.preferred = false;
                                match save_profile(p) {
                                    Ok(()) => {
                                        moved_from.get_or_insert_with(|| p.name.to_string());
                                    }
                                    Err(_) => p.preferred = true,
                                }
                            }
                        }
                        Outcome::Saved { moved_from }
                    }
                    Err(e) => {
                        if let Some(p) = cfg.find_mut(&name) {
                            p.preferred = !now_on;
                            if cleared_last_resort {
                                p.last_resort = true;
                            }
                        }
                        Outcome::SaveFailed(e)
                    }
                }
            }
        }
    };
    match outcome {
        Outcome::Missing => {}
        Outcome::Saved { moved_from } => {
            if let Some(prev) = moved_from {
                app.toast(ToastKind::Info, format!("preferred moved from '{prev}'"));
            }
            app.refresh_tokens();
        }
        Outcome::SaveFailed(e) => app.toast(ToastKind::Danger, format!("save failed\n{e}")),
    }
}

/// Add a profile to the chain (seeding default threshold if unset) and persist.
fn add_chain_candidate(app: &mut App, name: &ProfileName) {
    let mut cfg = app.config();
    if let Some(profile) = cfg.find_mut(name)
        && profile.fallback_threshold.is_none()
    {
        profile.fallback_threshold = Some(DEFAULT_THRESHOLD);
        let _ = save_profile(profile);
    }
    cfg.state.fallback_chain.push(name.clone());
    let _ = save_app_state(&cfg.state);
}

/// Remove the selected member, persist, and return focus to the list.
fn remove_chain_member(app: &mut App) {
    let Some(pos) = selected_chain_member(app) else {
        return;
    };
    let name = {
        let mut cfg = app.config();
        let Some(name) = cfg.state.fallback_chain.get(pos).cloned() else {
            return;
        };
        cfg.state.fallback_chain.retain(|n| n != &name);
        let _ = save_app_state(&cfg.state);
        name
    };
    leave_fallback_detail(app);
    let items_len = chain_items(app).len();
    if app.chain_cursor >= items_len {
        app.chain_cursor = items_len.saturating_sub(1);
    }
    app.toast(ToastKind::Info, format!("removed '{name}' from chain"));
}

// ── Modal handling ────────────────────────────────────────────────────────────

fn handle_modal_key(app: &mut App, key: KeyEvent) {
    let Some(top) = app.modals.last().cloned() else {
        return;
    };
    match top {
        // The keymap outgrows a short terminal, so ↑↓ scrolls it — the same
        // binding every other scrollable surface here uses. BOTH arms clamp
        // against the bound the last render published: growing the terminal
        // shrinks the range under a `help_scroll` that outlived it, and an
        // unclamped step down would then need one press per stale row before
        // the view moved at all (the render clamps the display meanwhile, so
        // the key just looks dead).
        Modal::Help => match key.code {
            KeyCode::Up => {
                let max = app.help_max_scroll.get();
                app.help_scroll = app.help_scroll.min(max).saturating_sub(1);
            }
            KeyCode::Down => {
                let max = app.help_max_scroll.get();
                app.help_scroll = app.help_scroll.saturating_add(1).min(max);
            }
            KeyCode::Esc | KeyCode::Enter | KeyCode::Char('?' | 'q') => {
                app.modals.pop();
            }
            _ => {}
        },
        Modal::Confirm(_) => handle_confirm_key(app, key),
        Modal::Divergence(_) => handle_divergence_key(app, key),
        Modal::CaptureName(_) => handle_capture_name_key(app, key),
        Modal::NamePrompt(_) => handle_name_prompt_key(app, key),
        Modal::PresetPicker(_) => handle_preset_picker_key(app, key),
        Modal::DivergenceTarget(_) => handle_divergence_target_key(app, key),
        Modal::ActionMenu(_) => handle_action_menu_key(app, key),
        Modal::EnvCollision(_) => handle_env_collision_key(app, key),
        Modal::Login => match key.code {
            // Re-fire the browser open. The URL exists once the worker announced
            // it; before that there is nothing to open, so `r` is a no-op.
            KeyCode::Char('r') | KeyCode::Char('R') => {
                if let Some(url) = app.login.as_ref().and_then(|s| s.url.clone()) {
                    match crate::platform::open_url(&url) {
                        Ok(()) => app.toast(ToastKind::Info, "opening your browser…"),
                        Err(_) => app.toast(ToastKind::Danger, "couldn't open the browser"),
                    }
                }
            }
            // Collapse to the footer indicator; the login keeps running (the
            // generation is untouched). A real cancel is the top-level esc
            // once collapsed; ⏎ on the login row re-expands.
            KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q') => {
                app.modals.pop();
            }
            _ => {}
        },
    }
}

/// Build the action menu for the current screen/focus context: what acts on the
/// focused account, then what acts on the tab.
pub(crate) fn build_action_menu(app: &App) -> ActionMenuState {
    use ActionMenuAction::*;
    let mut scoped: Vec<ActionMenuAction> = Vec::new();
    let mut actions: Vec<ActionMenuAction> = Vec::new();
    let mut context: Option<String> = None;

    match app.tab {
        Tab::Overview => {
            context = push_account_scope(app, &mut scoped);
            actions.push(RefreshAll);
            actions.push(NewAccount);
        }
        Tab::Usage => {
            context = push_account_scope(app, &mut scoped);
            actions.push(RefreshAll);
            actions.push(ToggleEstimates);
            actions.push(TogglePace);
        }
        // Tokens: the period + model-filter lenses (minus the ones already
        // active) plus the page keys (`c` cache basis, `r` reload) for
        // discoverability.
        Tab::Tokens => {
            for (period, action) in [
                (TokenPeriod::Lifetime, TokensPeriodLifetime),
                (TokenPeriod::Daily, TokensPeriodDaily),
                (TokenPeriod::Weekly, TokensPeriodWeekly),
                (TokenPeriod::Monthly, TokensPeriodMonthly),
            ] {
                if app.token_period != period {
                    actions.push(action);
                }
            }
            if app.token_filter != TokenFilter::All {
                actions.push(TokensShowAll);
            }
            if app.token_filter != TokenFilter::Claude {
                actions.push(TokensShowClaude);
            }
            if app.token_filter != TokenFilter::Others {
                actions.push(TokensShowOthers);
            }
            actions.push(ToggleCountCache);
            actions.push(ReloadTokenStats);
        }
        // Both halves of the Setup tab configure the SAME focused account, so
        // they carry the same three items. Every per-row action already has a
        // key (the detail pane is a list of actions and ⏎ runs the row); what
        // is left is the whole-account work no row states: copy it, save its
        // endpoint + models as a template, stamp one back onto it. On `+ new`
        // there is no source account to duplicate or save, but a preset can
        // still stamp the draft's endpoint + model fields.
        Tab::Setup => {
            if let Some((name, _, _)) = focused_account(app) {
                context = Some(name.to_string());
                scoped.push(Duplicate);
                scoped.push(SaveAsPreset);
                scoped.push(ApplyPreset);
                // The api-key row is on this tab, so the page that key comes
                // from belongs next to it.
                if focused_provider_console(app).is_some() {
                    scoped.push(OpenProviderConsole);
                }
            } else if app.profile_cursor >= app.profile_count() {
                context = app
                    .config_draft
                    .as_ref()
                    .and_then(|d| d.name.trimmed_some());
                scoped.push(ApplyPreset);
            }
        }
        // Fallback: every action the chain and its detail rows carry is bound to
        // a key of its own (⏎, ⇧↑↓, space, +/-), so `a` offers nothing.
        Tab::Fallback => {}
        Tab::Config => {}
        Tab::Status => {
            actions.push(RefreshStatus);
            if app.status.selected().is_some() {
                actions.push(OpenIncidentLink);
            }
        }
        // Plugin: no action menu — `r` re-runs checks, `f` fixes, ⏎/esc navigate.
        Tab::Plugin => {}
    }

    ActionMenuState::new(scoped, actions, context)
}

/// Push what the account under the cursor can be told to do, returning the name
/// the menu titles that group with. Nothing to push when the cursor sits past
/// the accounts (an empty Overview), which is also what leaves the title bare.
fn push_account_scope(app: &App, scoped: &mut Vec<ActionMenuAction>) -> Option<String> {
    let (name, _, _) = focused_account(app)?;
    scoped.push(ActionMenuAction::RefreshUsage);
    scoped.push(ActionMenuAction::RotateTokens);
    scoped.push(disabled_toggle_action(app));
    if focused_provider_console(app).is_some() {
        scoped.push(ActionMenuAction::OpenProviderConsole);
    }
    Some(name.to_string())
}

/// The console page the focused account's endpoint mints its api key on.
/// `None` past the accounts, on an OAuth account, and on an endpoint no
/// provider claims. Both the menu's gate and the handler resolve through this
/// one function, so what is offered and what opens cannot drift apart.
fn focused_provider_console(app: &App) -> Option<&'static str> {
    app.config().profiles.get(app.profile_cursor)?.console_url()
}

/// Open the focused account's provider console in the default browser
/// (detached), mirroring [`open_incident_link`].
fn open_provider_console(app: &mut App) {
    let Some(url) = focused_provider_console(app) else {
        app.toast(ToastKind::Info, "no console page for this endpoint");
        return;
    };
    match crate::platform::open_url(url) {
        Ok(()) => app.toast(ToastKind::Info, "opening in browser"),
        Err(_) => app.toast(ToastKind::Danger, "failed to open browser"),
    }
}

/// The focused account's disable-row action, labeled by the state it would
/// leave behind — the same flip the Setup row's own label carries.
fn disabled_toggle_action(app: &App) -> ActionMenuAction {
    let currently_disabled = app
        .config()
        .profiles
        .get(app.profile_cursor)
        .is_some_and(Profile::is_disabled);
    if currently_disabled {
        ActionMenuAction::EnableProfile
    } else {
        ActionMenuAction::DisableProfile
    }
}

fn handle_action_menu_key(app: &mut App, key: KeyEvent) {
    if let KeyCode::Char(ch) = key.code {
        let ch_lower = ch.to_lowercase().next().unwrap_or(ch);
        let Some(Modal::ActionMenu(state)) = app.modals.last() else {
            return;
        };
        let action = state
            .items
            .iter()
            .find(|item| item.hotkey == Some(ch_lower))
            .map(|item| item.action.clone());
        if let Some(action) = action {
            app.modals.pop();
            dispatch_action_menu_action(app, action);
            return;
        }
    }

    match key.code {
        KeyCode::Esc | KeyCode::Char('q') => {
            app.modals.pop();
        }
        KeyCode::Up => {
            let Some(Modal::ActionMenu(state)) = app.modals.last_mut() else {
                return;
            };
            let len = state.items.len();
            if len > 0 {
                state.cursor = (state.cursor + len - 1) % len;
            }
        }
        KeyCode::Down => {
            let Some(Modal::ActionMenu(state)) = app.modals.last_mut() else {
                return;
            };
            let len = state.items.len();
            if len > 0 {
                state.cursor = (state.cursor + 1) % len;
            }
        }
        KeyCode::Enter => {
            let Some(Modal::ActionMenu(state)) = app.modals.last() else {
                return;
            };
            let action = state
                .items
                .get(state.cursor)
                .map(|item| item.action.clone());
            app.modals.pop();
            if let Some(action) = action {
                dispatch_action_menu_action(app, action);
            }
        }
        _ => {}
    }
}

/// The account under the cursor as `(name, has_oauth_login,
/// usage_cache_is_third_party)`. `profile_cursor` is shared across Overview,
/// Usage, and Setup, so this resolves the focused account on any of them.
/// `None` when the cursor sits past the profile list (e.g. `+ new`).
///
/// The OAuth bool is credential typing ([`Profile::login_is_oauth`]), not endpoint
/// routing: the actions it gates (rotate, refresh) act on the stored token chain,
/// so a hybrid holding a real pair behind a `base_url` can rotate it, and an
/// endpoint-only profile cannot.
///
/// The third bool is the shared cache selector, not `is_third_party`: the
/// refresh gate asks whether the account has a usage fetch leg, and the
/// third-party leg fetches generic api-key endpoints too — `is_third_party`
/// would refuse a litellm row the scheduler refreshes every cadence.
fn focused_account(app: &App) -> Option<(ProfileName, bool, bool)> {
    let cfg = app.config();
    cfg.profiles.get(app.profile_cursor).map(|p| {
        (
            p.name.clone(),
            p.login_is_oauth(),
            p.usage_cache_is_third_party(),
        )
    })
}

/// Queue a usage refetch for the account under the cursor, or toast why not.
/// One shape for both entry points (Usage `r`, the action menu's refresh
/// entry): the OAuth arm and the third-party-cache arm both queue
/// [`App::manual_refresh_one`] — the same refetch the cadence legs would run —
/// and only an endpoint-only account with no credential either leg can use
/// falls through to the nothing-to-refresh toast.
fn queue_focused_usage_refresh(app: &mut App) {
    match focused_account(app) {
        Some((name, true, _)) | Some((name, _, true)) => {
            app.manual_refresh_one(&name);
            app.toast(ToastKind::Info, format!("refreshing '{name}'"));
        }
        Some((name, false, false)) => {
            app.toast(ToastKind::Info, format!("'{name}' has no usage to refresh"));
        }
        None => {}
    }
}

/// Dispatch a selected action menu item to its handler.
fn dispatch_action_menu_action(app: &mut App, action: ActionMenuAction) {
    match action {
        ActionMenuAction::NewAccount => start_new_account(app),
        ActionMenuAction::RefreshUsage => queue_focused_usage_refresh(app),
        ActionMenuAction::RotateTokens => match focused_account(app) {
            // macOS refuses this rotation (`runtime::rotation_blocked_for`), so
            // say why up front instead of arming a confirm that no-ops.
            Some((name, true, _)) if crate::runtime::rotation_blocked_for(&name) => {
                app.modals.push(Modal::Confirm(ConfirmState {
                    message: format!("'{name}' {ROTATE_LIVE_SESSION_MSG}"),
                    detail: Some(ROTATE_LIVE_SESSION_DETAIL.to_string()),
                    choice: true,
                    on_confirm: ConfirmAction::Acknowledge,
                }));
            }
            Some((name, true, _)) => {
                app.modals.push(Modal::Confirm(ConfirmState {
                    message: format!("rotate access token for '{name}'?"),
                    detail: None,
                    choice: false,
                    on_confirm: ConfirmAction::RotateOne(name.to_string()),
                }));
            }
            Some((name, _, _)) => {
                app.toast(ToastKind::Info, format!("'{name}' has no tokens to rotate"));
            }
            None => {}
        },
        ActionMenuAction::RefreshAll => {
            app.manual_refresh();
            app.toast(ToastKind::Info, "refreshing every account");
        }
        ActionMenuAction::DisableProfile | ActionMenuAction::EnableProfile => {
            toggle_focused_account_disabled(app);
        }
        ActionMenuAction::OpenProviderConsole => open_provider_console(app),
        ActionMenuAction::Duplicate => prompt_duplicate_profile(app),
        ActionMenuAction::SaveAsPreset => prompt_save_preset(app),
        ActionMenuAction::ApplyPreset => open_preset_picker(app),
        ActionMenuAction::RefreshStatus => trigger_status_refresh(app),
        ActionMenuAction::OpenIncidentLink => open_incident_link(app),
        ActionMenuAction::ToggleEstimates => toggle_show_estimates(app),
        ActionMenuAction::TogglePace => toggle_show_pace(app),
        ActionMenuAction::TokensPeriodLifetime => set_token_period(app, TokenPeriod::Lifetime),
        ActionMenuAction::TokensPeriodDaily => set_token_period(app, TokenPeriod::Daily),
        ActionMenuAction::TokensPeriodWeekly => set_token_period(app, TokenPeriod::Weekly),
        ActionMenuAction::TokensPeriodMonthly => set_token_period(app, TokenPeriod::Monthly),
        ActionMenuAction::TokensShowAll => set_token_filter(app, TokenFilter::All),
        ActionMenuAction::TokensShowClaude => set_token_filter(app, TokenFilter::Claude),
        ActionMenuAction::TokensShowOthers => set_token_filter(app, TokenFilter::Others),
        ActionMenuAction::ToggleCountCache => toggle_count_cache(app),
        ActionMenuAction::ReloadTokenStats => reload_token_stats(app),
    }
}

/// Swap the Tokens model filter and re-clamp the Models cursor — the filtered
/// list can be shorter than where the cursor sat.
fn set_token_filter(app: &mut App, filter: TokenFilter) {
    app.token_filter = filter;
    let len = token_model_count(app);
    if app.token_model_cursor >= len {
        app.token_model_cursor = len.saturating_sub(1);
    }
}

/// Swap the Tokens period lens and re-clamp the Models cursor — the scoped
/// list can be shorter than where the cursor sat.
fn set_token_period(app: &mut App, period: TokenPeriod) {
    app.token_period = period;
    let len = token_model_count(app);
    if app.token_model_cursor >= len {
        app.token_model_cursor = len.saturating_sub(1);
    }
}

/// Setup tab keymap. Left: ↑↓ + ⏎ enters detail. Right: ↑↓ walks rows, ⏎
/// edits/toggles/arms/creates. Esc (global) returns to list.
fn handle_config_key(app: &mut App, key: KeyEvent) {
    let sel_len = app.profile_count() + 1; // includes trailing `+ new` row
    app.profile_cursor = app.profile_cursor.min(sel_len - 1);

    match app.config_focus {
        ConfigFocus::Profiles => match key.code {
            KeyCode::Up => step_profile_cursor(app, -1, sel_len),
            KeyCode::Down => step_profile_cursor(app, 1, sel_len),
            KeyCode::Enter => enter_config_detail(app),
            _ => {}
        },
        ConfigFocus::Actions => {
            let rows = config_rows(app);
            if rows.is_empty() {
                app.config_focus = ConfigFocus::Profiles;
                app.config_draft = None;
                return;
            }
            let last = rows.len() - 1;
            app.config_action_cursor = app.config_action_cursor.min(last);
            match key.code {
                KeyCode::Up => {
                    disarm_armed_action(app);
                    app.config_action_cursor = if app.config_action_cursor == 0 {
                        last
                    } else {
                        app.config_action_cursor - 1
                    };
                }
                KeyCode::Down => {
                    disarm_armed_action(app);
                    app.config_action_cursor = if app.config_action_cursor >= last {
                        0
                    } else {
                        app.config_action_cursor + 1
                    };
                }
                KeyCode::Enter => run_config_row(app, rows[app.config_action_cursor]),
                KeyCode::Char(' ') => {
                    let row = rows[app.config_action_cursor];
                    // Space cycles the `model` alias in place; ⏎ opens its custom
                    // field instead. Every other row treats space like ⏎.
                    if row == ConfigRow::Model {
                        cycle_model(app);
                    } else {
                        run_config_row(app, row);
                    }
                }
                _ => {}
            }
        }
    }
}

/// Detail rows for the current selection. `+ new` → create form; account →
/// settings (auto-start only for OAuth).
pub(crate) fn config_rows(app: &App) -> Vec<ConfigRow> {
    let cfg = app.config();
    let draft = app.config_draft.as_ref();
    if app.profile_cursor >= cfg.profiles.len() {
        // `+ new` create form. The api key only means something once a base url
        // makes this an API account, so it stays hidden until one is typed.
        let mut rows = vec![ConfigRow::Name, ConfigRow::BaseUrl];
        if draft.is_some_and(|d| !d.base_url.value.trim().is_empty()) {
            rows.push(ConfigRow::ApiKey);
        }
        // Base model only — the alias overrides + env rows stay existing-account-only.
        rows.push(ConfigRow::Model);
        if draft.is_none_or(|d| d.base_url.value.trim().is_empty()) {
            rows.push(ConfigRow::Login);
        }
        rows.push(ConfigRow::Create);
        return rows;
    }
    let profile = cfg.profiles.get(app.profile_cursor);
    // `is_api` tracks the base-url buffer live (draft wins): api key shows only
    // in API mode, auto-start (OAuth-only) only when it's empty, so both flip the
    // instant a base url is typed.
    let is_api = match draft {
        Some(d) => !d.base_url.value.trim().is_empty(),
        None => profile.map(|p| !p.is_oauth()).unwrap_or(false),
    };
    let mut rows = vec![ConfigRow::Name];
    // auto-start sits right below name (OAuth-only; mutually exclusive with api key).
    if !is_api {
        rows.push(ConfigRow::AutoStart);
    }
    rows.push(ConfigRow::BaseUrl);
    if is_api {
        rows.push(ConfigRow::ApiKey);
    }
    rows.push(ConfigRow::Model);

    // Alias overrides collapse: render the ones already set, tuck the rest behind
    // a single `+ model override` reveal until ⏎ expands them (draft-scoped).
    let expanded = draft.is_some_and(|d| d.overrides_expanded);
    let override_set = |row: ConfigRow| match draft {
        Some(d) => d.field(row).is_some_and(|i| !i.value.trim().is_empty()),
        None => {
            let v = match row {
                ConfigRow::OpusModel => profile.and_then(|p| p.models.opus.as_deref()),
                ConfigRow::SonnetModel => profile.and_then(|p| p.models.sonnet.as_deref()),
                ConfigRow::HaikuModel => profile.and_then(|p| p.models.haiku.as_deref()),
                ConfigRow::FableModel => profile.and_then(|p| p.models.fable.as_deref()),
                ConfigRow::SubagentModel => profile.and_then(|p| p.models.subagent.as_deref()),
                _ => None,
            };
            v.is_some_and(|s| !s.trim().is_empty())
        }
    };
    let mut any_collapsed = false;
    for row in [
        ConfigRow::OpusModel,
        ConfigRow::SonnetModel,
        ConfigRow::HaikuModel,
        ConfigRow::FableModel,
        ConfigRow::SubagentModel,
    ] {
        if expanded || override_set(row) {
            rows.push(row);
        } else {
            any_collapsed = true;
        }
    }
    if any_collapsed {
        rows.push(ConfigRow::ModelOverrideAdd);
    }

    // One row per custom env entry (sorted), then the `+ add env` row. Indices
    // must match the sorted env snapshot used by the renderer and the commit path.
    let env_count = profile.map(|p| p.env.len()).unwrap_or(0);
    rows.extend((0..env_count).map(ConfigRow::EnvEntry));
    rows.push(ConfigRow::EnvAdd);
    // Log in / re-login, then log out once a credential exists — for both OAuth
    // (browser mint) and API (base url + api key) accounts. "Has a credential"
    // reads the OAuth token or the api key depending on the account's credential
    // typing (`login_is_oauth`, not the endpoint-shaped `is_api`): the log-out
    // row acts on what's on disk, so a hybrid's token can't be hidden behind
    // either a base url or an uncommitted draft.
    rows.push(ConfigRow::Login);
    let has_creds = if profile.is_some_and(|p| p.login_is_oauth()) {
        profile.and_then(|p| p.credentials.as_ref()).is_some()
    } else {
        profile
            .and_then(|p| p.api_key.as_deref())
            .is_some_and(|k| !k.trim().is_empty())
    };
    if has_creds {
        rows.push(ConfigRow::DeleteCreds);
    }
    // CLA-SPLIT escape hatch, beside the log-out row because it drops the OTHER
    // credential a split profile holds. Gated on ANY long-lived piece existing
    // — the sidecar (`session_token_status`, so a mis-fill gets the same
    // exit), the preserved mint backup, or a set `rolling_token` flag — never
    // on the split being ENGAGED: a flag with no sidecar is exactly the state
    // where hiding the row leaves the daemon to re-create what the operator
    // has no surface left to remove (the CLI's widened nothing-to-clear gate,
    // rendered). The no-other-login refusal is a dim, not a hide — see
    // `run_config_row`.
    if profile.is_some_and(|p| {
        crate::claude::session_token_status(&p.name).is_some()
            || p.rolling_token
            || crate::claude::has_static_backup(&p.name)
    }) {
        rows.push(ConfigRow::ClearSessionToken);
    }
    // The account-actions group's tail: disable/enable (reversible) one severity
    // notch below delete (irreversible), which always stays the very last row.
    rows.push(ConfigRow::Disabled);
    rows.push(ConfigRow::Delete);
    rows
}

/// Enter the detail pane, seeding a draft for the current selection.
fn enter_config_detail(app: &mut App) {
    app.config_action_cursor = 0;
    if app.profile_cursor >= app.profile_count() {
        app.config_draft = Some(build_draft_new());
    } else if let Some(name) = app.profile_name_at(app.profile_cursor) {
        app.config_draft = Some(build_draft_existing(app, &name));
    } else {
        return;
    }
    app.config_focus = ConfigFocus::Actions;
}

/// Jump to the `+ new` create form (global `n`).
fn start_new_account(app: &mut App) {
    switch_tab(app, Tab::Setup);
    app.profile_cursor = app.profile_count();
    app.config_action_cursor = 0;
    app.config_draft = Some(build_draft_new());
    app.config_focus = ConfigFocus::Actions;
}

fn build_draft_new() -> ConfigDraft {
    ConfigDraft {
        editing_name: None,
        name: InputState::new(""),
        base_url: InputState::new(""),
        api_key: InputState::new(""),
        model: InputState::new(""),
        opus_model: InputState::new(""),
        sonnet_model: InputState::new(""),
        haiku_model: InputState::new(""),
        fable_model: InputState::new(""),
        subagent_model: InputState::new(""),
        env_value: InputState::new(""),
        env_new_key: InputState::new(""),
        active: None,
        armed_action: None,
        relogin_chain: false,
        overrides_expanded: false,
        captured_login: None,
    }
}

fn build_draft_existing(app: &App, name: &ProfileName) -> ConfigDraft {
    let cfg = app.config();
    let profile = cfg.find(name);
    let m = profile.map(|p| p.models.clone()).unwrap_or_default();
    ConfigDraft {
        editing_name: Some(name.to_string()),
        name: InputState::new(name),
        base_url: InputState::new(profile.and_then(|p| p.base_url.as_deref()).unwrap_or("")),
        api_key: InputState::new(profile.and_then(|p| p.api_key.as_deref()).unwrap_or("")),
        model: InputState::new(m.default.as_deref().unwrap_or("")),
        opus_model: InputState::new(m.opus.as_deref().unwrap_or("")),
        sonnet_model: InputState::new(m.sonnet.as_deref().unwrap_or("")),
        haiku_model: InputState::new(m.haiku.as_deref().unwrap_or("")),
        fable_model: InputState::new(m.fable.as_deref().unwrap_or("")),
        subagent_model: InputState::new(m.subagent.as_deref().unwrap_or("")),
        env_value: InputState::new(""),
        env_new_key: InputState::new(""),
        active: None,
        armed_action: None,
        relogin_chain: false,
        overrides_expanded: false,
        captured_login: None,
    }
}

/// Back out of the Setup detail pane, dropping the draft. A `+ new` draft
/// holding a minted login loses it with the form — say so instead of
/// discarding a real browser round-trip silently.
fn leave_config_detail(app: &mut App) {
    let mint_dropped = app
        .config_draft
        .as_ref()
        .is_some_and(|d| d.editing_name.is_none() && d.captured_login.is_some());
    app.config_focus = ConfigFocus::Profiles;
    app.config_draft = None;
    if mint_dropped {
        app.toast(
            ToastKind::Warning,
            "closing the form dropped the login you just captured",
        );
    }
}

/// Disarm whichever account-action row is armed when the cursor moves off it.
fn disarm_armed_action(app: &mut App) {
    if let Some(d) = app.config_draft.as_mut() {
        d.armed_action = None;
    }
}

/// ⏎/space on a detail row: text → capture, toggle → flip, delete → arm/confirm, create → commit.
fn run_config_row(app: &mut App, row: ConfigRow) {
    // Env rows seed their buffer before editing, so they're driven out of band.
    match row {
        ConfigRow::EnvEntry(i) => {
            enter_env_value_edit(app, i);
            return;
        }
        ConfigRow::EnvAdd => {
            enter_env_add_edit(app);
            return;
        }
        ConfigRow::ModelOverrideAdd => {
            // Reveal the unset alias-override rows inline; no buffer to seed.
            if let Some(d) = app.config_draft.as_mut() {
                d.overrides_expanded = true;
            }
            return;
        }
        _ => {}
    }
    // Text rows plus the `model` row's custom field open an inline editor.
    if row.is_text() || row == ConfigRow::Model {
        if let Some(d) = app.config_draft.as_mut() {
            d.active = Some(row);
            if let Some(input) = d.field_mut(row) {
                input.end();
            }
        }
        return;
    }
    let name = app
        .config_draft
        .as_ref()
        .and_then(|d| d.editing_name.clone())
        .map(ProfileName::from);
    match row {
        ConfigRow::Disabled => {
            if let Some(name) = name {
                let gated =
                    app.config().is_active(&name) || crate::runtime::has_live_session(&name);
                if gated {
                    return;
                }
                let currently_disabled = app.config().find(&name).is_some_and(Profile::is_disabled);
                let armed = app
                    .config_draft
                    .as_ref()
                    .is_some_and(|d| d.armed_action == Some(ConfigRow::Disabled));
                // Enabling is harmless — immediate, no arm. Disabling has real
                // operational impact (drops the account from auto-switch/
                // polling/status mid-flight), so it reuses Delete's
                // press-to-arm → confirm class instead of firing on one ⏎.
                if currently_disabled || armed {
                    toggle_profile_disabled(app, &name);
                    // Clear rather than leave a stale arm: unlike Delete (whose
                    // confirm removes the row entirely), this row survives its
                    // own toggle, so a later disable must re-arm from scratch.
                    disarm_armed_action(app);
                } else {
                    arm_action(app, ConfigRow::Disabled);
                }
            }
        }
        ConfigRow::AutoStart => {
            if let Some(name) = name {
                toggle_auto_start(app, &name);
            }
        }
        ConfigRow::Delete => {
            let armed = app
                .config_draft
                .as_ref()
                .is_some_and(|d| d.armed_action == Some(ConfigRow::Delete));
            match (armed, name) {
                (true, Some(name)) => perform_delete(app, &name),
                _ => arm_action(app, ConfigRow::Delete),
            }
        }
        ConfigRow::Create => commit_new_account(app),
        ConfigRow::Login => {
            // New-form draft has no editing_name → validate the typed name now and
            // create-on-mint; an existing draft re-logs in place.
            let editing = app
                .config_draft
                .as_ref()
                .and_then(|d| d.editing_name.clone());
            match login_row_flow(app, editing.as_deref()) {
                LoginRowFlow::Console { site, region } => {
                    let name = editing.unwrap_or_default();
                    start_console_login(app, name, site, region);
                    return;
                }
                LoginRowFlow::ApiKey => {
                    start_api_relogin(app);
                    return;
                }
                LoginRowFlow::OauthMint => {}
            }
            let target = match editing {
                Some(name) => Some((name, false)),
                None => {
                    let typed = app
                        .config_draft
                        .as_ref()
                        .map(|d| d.name.trimmed().to_string())
                        .unwrap_or_default();
                    let validation = {
                        let cfg = app.config();
                        validate_profile_name(&typed, &cfg.names(), None)
                    };
                    match validation {
                        Ok(()) => Some((typed, true)),
                        Err(e) => {
                            app.toast(ToastKind::Danger, format!("{e}"));
                            None
                        }
                    }
                }
            };
            if let Some((name, is_new)) = target {
                // A stashed mint (the `✓ logged in` done-state) makes ⏎ a
                // stash-replacing re-login; gate it so it can't drop the capture
                // silently. Only the `+ new` draft ever holds a stash.
                let has_stash = app
                    .config_draft
                    .as_ref()
                    .is_some_and(|d| d.captured_login.is_some());
                if has_stash {
                    app.modals.push(Modal::Confirm(ConfirmState {
                        message: "replace the captured login?".to_string(),
                        detail: Some("the login you already captured will be dropped".to_string()),
                        choice: false,
                        on_confirm: ConfirmAction::RestartLogin(name, is_new),
                    }));
                } else {
                    start_login(app, name, is_new);
                }
            }
        }
        ConfigRow::DeleteCreds => {
            if let Some(name) = name {
                let is_api = {
                    let cfg = app.config();
                    cfg.find(&name)
                        .map(|p| !p.login_is_oauth())
                        .unwrap_or(false)
                };
                let detail = if is_api {
                    "blanks the api key; keeps the base url, model, and env. re-login any time."
                } else {
                    "blanks the login; keeps the account, model, env, and chain slot. re-login any time."
                };
                app.modals.push(Modal::Confirm(ConfirmState {
                    message: format!("log out of '{name}'?"),
                    detail: Some(detail.to_string()),
                    choice: false,
                    on_confirm: ConfirmAction::BlankCredentials(name.to_string()),
                }));
            }
        }
        ConfigRow::ClearSessionToken => {
            if let Some(name) = name {
                // Same refusal the CLI makes (`cmd_static_token_clear`), on the
                // same condition: with no other login stored, clearing would
                // strip the account's last credential — but only a stored PIECE
                // (a sidecar or a preserved mint) is a credential to strip. A
                // flag-only account disarms without touching one, so it clears
                // here exactly as the CLI does, instead of a silent return
                // behind a row the widening made visible.
                let stores_other_login = {
                    let cfg = app.config();
                    cfg.find(&name)
                        .is_some_and(|p| p.credentials.is_some() || p.api_key.is_some())
                };
                let holds_credential_piece = crate::claude::session_token_status(&name).is_some()
                    || crate::claude::has_static_backup(&name);
                if holds_credential_piece && !stores_other_login {
                    return;
                }
                let armed = app
                    .config_draft
                    .as_ref()
                    .is_some_and(|d| d.armed_action == Some(ConfigRow::ClearSessionToken));
                // Arm/confirm rather than a modal: this is a row-level
                // destructive action, and it retargets every future switch.
                if armed {
                    perform_clear_session_token(app, &name);
                    disarm_armed_action(app);
                } else {
                    arm_action(app, ConfigRow::ClearSessionToken);
                }
            }
        }
        _ => {}
    }
}

/// Confirmed `clear long-lived token`: drop the sidecar, then relink when the
/// account is ACTIVE. The relink is not optional there — the live
/// `~/.claude/.credentials.json` is a symlink INTO the profile store aimed at
/// `session-token.json`, so removing the target alone leaves a dangling link
/// and Claude Code reads no credentials at all. An idle account is left alone:
/// force-linking it would repoint the live slot at a profile nobody switched to.
///
/// Same full-exit contract as the CLI's `static-token --clear`, or the two
/// surfaces fight the daemon differently: the `rolling_token` flag goes FIRST
/// (a set flag re-stamps a fresh bearer over the removal on the daemon's next
/// scan), the preserved mint backup goes too (a "cleared" long-lived token
/// with a year-scale mint still in `session-token.static.json` is not
/// cleared), and a mis-filled sidecar is quarantined as evidence before the
/// removal, exactly as every other repair path does.
///
/// The rotation guard is the same load-bearing piece it is on the CLI path —
/// a rotation in flight that still saw the flag set would re-stamp the
/// sidecar AFTER the removal — but a UI thread must never park behind a
/// timeout-less flock, so this takes `try_acquire` and fails LOUDLY into a
/// toast when a rotation holds the profile, instead of silently losing the
/// race.
///
/// The relink has TWO outcomes and the toast names which one happened. The row
/// is refused only when clearing would strip the account's LAST credential — a
/// stored piece (sidecar or preserved mint) with neither a login nor an api key
/// behind it; a flag-only account disarms regardless, touching no credential.
/// An api-key account clears fine onto an ABSENT install source: the forcing
/// relink then removes the live slot and, on macOS, signs the Keychain out
/// rather than installing anything. Both were reported as "relinked its own
/// login". The refusal is re-checked from DISK under the guard: the in-memory
/// gate in `run_config_row` reads a snapshot `reload_fingerprint` can never
/// invalidate (it does not stat `credentials.json`), so an out-of-band log-out
/// would otherwise slip past it forever.
///
/// Deliberately does NOT stamp `last_reload_fp`: `reload_fingerprint` folds each
/// sidecar's write time in, so leaving it stale is what makes the next tick
/// reload and rebuild `session_tokens` (the Overview `⊘` marker's cache). The
/// local `remove` below only spares that marker one tick of staleness.
fn perform_clear_session_token(app: &mut App, name: &ProfileName) {
    let _guard = match crate::runtime::RotationGuard::try_acquire(name) {
        Ok(Some(guard)) => guard,
        Ok(None) => {
            // Rendered from the shared cause, not spelled here: one condition
            // reaching an operator from a third surface in a third wording is
            // what sends someone looking for three different problems.
            app.toast(
                ToastKind::Danger,
                crate::format::Transient::new(
                    crate::format::Cause::RotationLockHeld(name.to_string()),
                    crate::format::Retry::Stated,
                )
                .text(),
            );
            return;
        }
        Err(e) => {
            app.toast(ToastKind::Danger, format!("clear failed\n{e}"));
            return;
        }
    };
    // Everything the ACTIONS key off is re-read under the guard — the CLI's
    // own rule, and stricter here: the TUI's config is a tick-stale snapshot,
    // and `reload_fingerprint` does not stat `credentials.json` at all, so an
    // out-of-band log-out leaves the snapshot claiming a stored login
    // INDEFINITELY, not for one tick. `run_config_row`'s gate fed the row's
    // rendering and nothing else.
    let on_disk = match crate::profile::load_profile(name) {
        Ok(p) => p,
        Err(e) => {
            app.toast(ToastKind::Danger, format!("clear failed\n{e}"));
            return;
        }
    };
    let holds_credential_piece = crate::claude::session_token_status(name).is_some()
        || crate::claude::has_static_backup(name);
    if holds_credential_piece && on_disk.credentials.is_none() && on_disk.api_key.is_none() {
        app.toast(
            ToastKind::Danger,
            format!("'{name}' stores no other login anymore · log in first, then clear"),
        );
        return;
    }
    let rolling_armed = on_disk.rolling_token;
    if rolling_armed {
        // The PERSIST is written from the same under-guard disk read, never
        // the in-memory snapshot — an external daemon's rotation may have
        // landed since the last reload, and `save_profile` writes the whole
        // profile back. The renderer's in-memory flag flips only AFTER that
        // write lands: flipped first, a failed save leaves the config lying
        // (`reload_fingerprint` never corrects it — config mtime did not move)
        // until an unrelated save makes the lie durable, and the end state is
        // a live rolling sidecar nothing re-stamps.
        let mut disarmed = on_disk.clone();
        disarmed.rolling_token = false;
        if let Err(e) = save_profile(&disarmed) {
            app.toast(ToastKind::Danger, format!("clear failed\n{e}"));
            return;
        }
        let mut cfg = app.config();
        if let Some(p) = cfg.find_mut(name) {
            p.rolling_token = false;
        }
    }
    if let Err(e) = crate::claude::quarantine_misfilled_sidecar(name)
        .and_then(|_| crate::claude::clear_session_token(name))
    {
        app.toast(ToastKind::Danger, format!("clear failed\n{e}"));
        return;
    }
    app.session_tokens.remove(name.as_str());
    // Read AFTER the clear, so it names the store the relink actually finds:
    // `install_source_path` falls back to `credentials.json` only once the
    // sidecar is gone. An api-key account has none, so it is signed out rather
    // than relinked, and the toast used to claim the opposite.
    let has_login = crate::claude::has_stored_oauth_login(name);
    // `active` comes off DISK for the same reason every other fact here does.
    // This process is not the only thing that switches: `daemon::tick` and
    // `fallback` both reach `actions::switch_profile`, so the active account
    // moves with no keypress at all, and the snapshot behind the row is a tick
    // old at best. Acting on a stale one relinks the account the operator just
    // switched AWAY from, or skips the relink on the one that IS active and
    // leaves its live slot a dangling symlink into the file just removed. A
    // config that will not parse is a larger failure than this decision, so
    // that arm keeps the old in-memory read rather than inventing a verdict.
    let active = crate::profile::load_config()
        .map(|c| c.is_active(name))
        .unwrap_or_else(|_| app.config().is_active(name));
    if active && let Err(e) = force_link_profile_credentials(name) {
        app.toast(ToastKind::Danger, format!("relink failed\n{e}"));
        return;
    }
    // No `refresh_tokens()`: `collect_tokens` reads `Profile::credentials`, which
    // a sidecar clear never touches.
    let tail = match (active, has_login) {
        (true, true) => ", relinked its own login",
        (true, false) if on_disk.api_key.is_some() => {
            ", signed Claude Code out; it runs on its api key"
        }
        // Reachable only on the flag-only widening (a stored piece with no
        // other login is refused above): nothing backs this account now, and
        // claiming an api key it does not hold would be the toast's own lie.
        (true, false) => ", signed Claude Code out; it stores no login at all",
        (false, true) => "; the next switch installs its own login",
        (false, false) if on_disk.api_key.is_some() => {
            "; it stores no login, so it runs on its api key"
        }
        (false, false) => "; it stores no login at all · log in before switching to it",
    };
    let rolled = if rolling_armed {
        " · re-stamping off"
    } else {
        ""
    };
    // The backup goes LAST, after the relink: failing between the sidecar
    // removal and the relink would leave an active profile's live slot a
    // dangling symlink under a toast reading "clear failed", a broken login
    // reported as nothing-happened. From here the live slot is already sound,
    // so a failed backup removal is exactly what its toast says and no more.
    //
    // `tail` and `rolled` are composed ABOVE so this arm can carry them: the
    // sign-out and the disarm both already happened and are durable, and a
    // failure message that reports neither is the same silence the postscripts
    // exist to refuse. The CLI keeps them for free by printing before its own
    // removal.
    let backup_removed = match crate::claude::clear_static_backup(name) {
        Ok(removed) => removed,
        Err(e) => {
            app.toast(
                ToastKind::Danger,
                format!(
                    "cleared the long-lived token for '{name}'{tail}{rolled}, but the preserved \
                     mint remains\n{e}"
                ),
            );
            return;
        }
    };
    // Unconditional on the removal, like the CLI's `clear_backup_postscript`:
    // the pre-action disclosure lives in the row hint, but a hint is not a
    // report, and a year-scale credential destroyed with nothing saying so is
    // the silence the `rolled` suffix already refuses for the flag.
    let swept = if backup_removed {
        " · the preserved mint is gone"
    } else {
        ""
    };
    app.toast(
        ToastKind::Success,
        format!("cleared the long-lived token for '{name}'{tail}{rolled}{swept}"),
    );
}

/// API-account "re-login": re-enter the base url + api key inline, mirroring the
/// CLI's `login --base-url --api-key`. Seeds both buffers from the stored values
/// and opens the base-url editor; committing it advances to the api-key editor
/// (the `relogin_chain`), and committing that persists both via `commit_endpoint`.
fn start_api_relogin(app: &mut App) {
    let name = app
        .config_draft
        .as_ref()
        .and_then(|d| d.editing_name.clone())
        .map(ProfileName::from);
    let (base, key) = {
        let cfg = app.config();
        let p = name.as_ref().and_then(|n| cfg.find(n));
        (
            p.and_then(|p| p.base_url.clone()).unwrap_or_default(),
            p.and_then(|p| p.api_key.clone()).unwrap_or_default(),
        )
    };
    if let Some(d) = app.config_draft.as_mut() {
        d.base_url = InputState::new(&base);
        d.api_key = InputState::new(&key);
        d.base_url.end();
        d.relogin_chain = true;
        d.active = Some(ConfigRow::BaseUrl);
    }
}

/// Kick a browser OAuth login on a worker. `is_new` → the mint lands in the
/// `+ new` draft when it arrives; else an existing profile is overwritten
/// (divergence-gated in `apply_login`). A second ⏎ while one is in flight
/// re-expands the progress modal instead of starting another login.
fn start_login(app: &mut App, name: String, is_new: bool) {
    if let Some(session) = app.login.as_ref() {
        // A ⏎ aimed at a different account can't start a second login — say
        // so instead of silently re-showing the in-flight session's modal.
        if session.name != name || session.is_new != is_new {
            app.toast(
                ToastKind::Warning,
                format!("a login for '{}' is already in progress", session.name),
            );
        }
        open_login_modal(app);
        return;
    }
    app.login_generation += 1;
    let generation = app.login_generation;
    app.login = Some(LoginSession {
        name,
        is_new,
        generation,
        url: None,
        stage: LoginStage::WaitingBrowser,
    });
    let event_tx = app.login_event_tx.clone();
    let result_tx = app.login_result_tx.clone();
    spawn_worker(move || {
        let res = crate::oauth_login::login_with(|progress| {
            use crate::oauth_login::LoginProgress;
            let event = match progress {
                LoginProgress::AuthorizeUrl(url) => LoginEvent::Url(url.to_string()),
                LoginProgress::ExchangingCode => LoginEvent::Stage(LoginStage::ExchangingCode),
                LoginProgress::Verifying => LoginEvent::Stage(LoginStage::Verifying),
            };
            let _ = event_tx.send((generation, event));
        });
        // A toast, not stderr: the canned line without the HTTP status. The
        // status is in `~/.clauth/clauth.log` via the exchange's `logline!`.
        let _ = result_tx.send((
            generation,
            res.map(|o| LoginResult::Oauth(Box::new(o)))
                .map_err(|e| e.user_message()),
        ));
    });
    open_login_modal(app);
}

/// Which of the three flows the `log in` row runs. An account can satisfy more
/// than one of these predicates, so the ORDER is the behaviour: a Model Studio
/// account is api-typed by `login_is_oauth`, and reading the api-key arm first
/// would route every one of them to the api-key re-entry with nothing else
/// failing. Encoded here rather than as a chain of early returns in the row so
/// the order is a thing a test can pin — driving the row itself binds a
/// loopback listener and opens a browser.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LoginRowFlow {
    /// Capture this account's Alibaba console session.
    Console {
        site: ConsoleSite,
        region: &'static str,
    },
    /// Re-enter an existing api-key account's base url + key inline.
    ApiKey,
    /// Mint an Anthropic login in the browser. Also the `+ new` form's flow,
    /// which has no account to read a type off yet.
    OauthMint,
}

/// Resolve the `log in` row's flow for the draft's account (`None` on the
/// `+ new` form, which can only mint).
fn login_row_flow(app: &App, editing: Option<&str>) -> LoginRowFlow {
    let Some(name) = editing else {
        return LoginRowFlow::OauthMint;
    };
    let name = ProfileName::from(name);
    if let Some((site, region)) = console_login_target(app, &name) {
        return LoginRowFlow::Console { site, region };
    }
    let cfg = app.config();
    match cfg.find(&name) {
        Some(p) if !p.login_is_oauth() => LoginRowFlow::ApiKey,
        _ => LoginRowFlow::OauthMint,
    }
}

/// Which console the `log in` row would capture a session from for `name`, or
/// `None` when that row runs one of its other two flows (an api-key re-entry or
/// an Anthropic browser mint).
///
/// Split out of the row so the decision is readable without starting a browser
/// round-trip: driving the row itself binds a loopback listener and opens a
/// browser, which is not something a test may do.
///
/// The verdict itself is [`Profile::console_login_target`], shared with the row's
/// hint and label so the copy cannot describe a different flow than the one ⏎
/// runs.
fn console_login_target(app: &App, name: &ProfileName) -> Option<(ConsoleSite, &'static str)> {
    let cfg = app.config();
    cfg.find(name)?.console_login_target()
}

/// Kick the Alibaba console login on a worker — the same browser round-trip and
/// the same progress modal as [`start_login`], capturing that account's usage
/// session instead of an Anthropic credential.
///
/// Never `is_new`: the console site and region are read off the profile's
/// endpoint, so an account without one has no console to open, and the create
/// form has no endpoint yet. That is why the CLI's own route is
/// create-with-preset, then log in.
fn start_console_login(app: &mut App, name: String, site: ConsoleSite, region: &'static str) {
    if let Some(session) = app.login.as_ref() {
        // Same shape as `start_login`'s guard, `is_new` included: a `+ new`
        // draft's login is a different session even when the typed name matches,
        // so re-showing its modal without a word would read as this row's own
        // login having started.
        if session.name != name || session.is_new {
            app.toast(
                ToastKind::Warning,
                format!("a login for '{}' is already in progress", session.name),
            );
        }
        open_login_modal(app);
        return;
    }
    app.login_generation += 1;
    let generation = app.login_generation;
    app.login = Some(LoginSession {
        name,
        is_new: false,
        generation,
        url: None,
        stage: LoginStage::WaitingBrowser,
    });
    let event_tx = app.login_event_tx.clone();
    let result_tx = app.login_result_tx.clone();
    spawn_worker(move || {
        let res = crate::alibaba_login::login_with(site, region, |url| {
            let _ = event_tx.send((generation, LoginEvent::Url(url.to_string())));
        });
        let _ = result_tx.send((
            generation,
            res.map(|o| LoginResult::Console(Box::new(o)))
                // `{e:#}` keeps the whole `anyhow` chain: the outermost context
                // here is a bare "loopback accept failed" whose io cause is the
                // only part that says what to do about it. No constructor in
                // `alibaba_login` embeds the token or an upstream body.
                .map_err(|e| format!("{e:#}")),
        ));
    });
    open_login_modal(app);
}

/// Show the login progress modal (no-op when already open).
fn open_login_modal(app: &mut App) {
    if !app.modals.iter().any(|m| matches!(m, Modal::Login)) {
        app.modals.push(Modal::Login);
    }
}

/// Drop the login progress modal (login finished, failed, or was canceled).
fn close_login_modal(app: &mut App) {
    app.modals.retain(|m| !matches!(m, Modal::Login));
}

/// Arm an account-action row (`Delete`, `ClearSessionToken`, or `Disabled` in
/// the disable direction) on its first ⏎.
fn arm_action(app: &mut App, row: ConfigRow) {
    if let Some(d) = app.config_draft.as_mut() {
        d.armed_action = Some(row);
    }
}

/// Keystrokes while a text row is active: ⏎ commits, ⎋ reverts.
fn handle_config_edit_key(app: &mut App, key: KeyEvent) {
    let Some(active) = app.config_draft.as_ref().and_then(|d| d.active) else {
        return;
    };
    match key.code {
        KeyCode::Esc => cancel_config_edit(app, active),
        KeyCode::Enter => commit_config_field(app, active),
        _ => {
            if let Some(d) = app.config_draft.as_mut()
                && let Some(input) = d.field_mut(active)
            {
                apply_input_edit(input, key);
            }
        }
    }
}

/// ⎋ inside a field: existing accounts revert from the live profile;
/// new drafts keep the typed value. Either way, editing ends.
fn cancel_config_edit(app: &mut App, field: ConfigRow) {
    let editing_name = app
        .config_draft
        .as_ref()
        .and_then(|d| d.editing_name.clone())
        .map(ProfileName::from);
    if let Some(name) = editing_name {
        let value = {
            let cfg = app.config();
            row_committed_value(cfg.find(&name), &name, field)
        };
        if let Some(d) = app.config_draft.as_mut()
            && let Some(input) = d.field_mut(field)
        {
            *input = InputState::new(&value);
        }
    }
    if let Some(d) = app.config_draft.as_mut() {
        // ⎋ mid re-login abandons the whole base-url → api-key chain, not just the
        // current field.
        d.relogin_chain = false;
        d.active = None;
    }
}

/// The persisted value behind a buffered row, used to revert on ⎋ and to reseed
/// the buffer after a commit. Toggle/action rows have no buffer → empty string.
fn row_committed_value(profile: Option<&Profile>, name: &ProfileName, row: ConfigRow) -> String {
    match row {
        ConfigRow::Name => name.to_string(),
        ConfigRow::BaseUrl => profile.and_then(|p| p.base_url.clone()).unwrap_or_default(),
        ConfigRow::ApiKey => profile.and_then(|p| p.api_key.clone()).unwrap_or_default(),
        ConfigRow::Model => profile
            .and_then(|p| p.models.default.clone())
            .unwrap_or_default(),
        ConfigRow::OpusModel => profile
            .and_then(|p| p.models.opus.clone())
            .unwrap_or_default(),
        ConfigRow::SonnetModel => profile
            .and_then(|p| p.models.sonnet.clone())
            .unwrap_or_default(),
        ConfigRow::HaikuModel => profile
            .and_then(|p| p.models.haiku.clone())
            .unwrap_or_default(),
        ConfigRow::FableModel => profile
            .and_then(|p| p.models.fable.clone())
            .unwrap_or_default(),
        ConfigRow::SubagentModel => profile
            .and_then(|p| p.models.subagent.clone())
            .unwrap_or_default(),
        // The saved value of the i-th sorted env entry; reverts a value edit on ⎋.
        ConfigRow::EnvEntry(i) => profile
            .and_then(|p| p.env.values().nth(i).cloned())
            .unwrap_or_default(),
        ConfigRow::AutoStart
        | ConfigRow::ModelOverrideAdd
        | ConfigRow::EnvAdd
        | ConfigRow::Login
        | ConfigRow::DeleteCreds
        | ConfigRow::ClearSessionToken
        | ConfigRow::Disabled
        | ConfigRow::Delete
        | ConfigRow::Create => String::new(),
    }
}

/// ⏎ inside a field: new draft buffers; existing accounts persist per field.
fn commit_config_field(app: &mut App, field: ConfigRow) {
    let is_new = app
        .config_draft
        .as_ref()
        .map(|d| d.editing_name.is_none())
        .unwrap_or(true);
    if is_new {
        if let Some(d) = app.config_draft.as_mut() {
            d.active = None;
        }
        return;
    }
    match field {
        ConfigRow::Name => commit_rename(app),
        ConfigRow::BaseUrl | ConfigRow::ApiKey => commit_endpoint(app),
        ConfigRow::Model
        | ConfigRow::OpusModel
        | ConfigRow::SonnetModel
        | ConfigRow::HaikuModel
        | ConfigRow::FableModel
        | ConfigRow::SubagentModel => commit_model_field(app, field),
        ConfigRow::EnvEntry(i) => commit_env_value(app, i),
        ConfigRow::EnvAdd => commit_env_new_key(app),
        _ => {
            if let Some(d) = app.config_draft.as_mut() {
                d.active = None;
            }
        }
    }
}

/// ⏎ on a model field: fold the trimmed buffer into the profile's
/// [`ModelSettings`], persist, then reseed the buffer from the saved value.
fn commit_model_field(app: &mut App, field: ConfigRow) {
    let Some(name) = app
        .config_draft
        .as_ref()
        .and_then(|d| d.editing_name.clone())
        .map(ProfileName::from)
    else {
        return;
    };
    let raw = app
        .config_draft
        .as_ref()
        .and_then(|d| d.field(field))
        .map(|i| i.trimmed().to_string())
        .unwrap_or_default();
    let mut models = {
        let cfg = app.config();
        cfg.find(&name)
            .map(|p| p.models.clone())
            .unwrap_or_default()
    };
    apply_model_field(&mut models, field, &raw);
    let result = {
        let mut cfg = app.config();
        edit_profile_model(&mut cfg, &name, models)
    };
    match result {
        Ok(()) => {
            let value = {
                let cfg = app.config();
                row_committed_value(cfg.find(&name), &name, field)
            };
            if let Some(d) = app.config_draft.as_mut() {
                if let Some(input) = d.field_mut(field) {
                    *input = InputState::new(&value);
                }
                d.active = None;
            }
        }
        Err(e) => app.toast(ToastKind::Danger, format!("model update failed\n{e}")),
    }
}

/// Fold a trimmed buffer into the matching [`ModelSettings`] field. Empty input
/// clears it (scalar → `None`).
fn apply_model_field(models: &mut ModelSettings, field: ConfigRow, raw: &str) {
    let scalar = (!raw.is_empty()).then(|| raw.to_string());
    match field {
        ConfigRow::Model => models.default = scalar,
        ConfigRow::OpusModel => models.opus = scalar,
        ConfigRow::SonnetModel => models.sonnet = scalar,
        ConfigRow::HaikuModel => models.haiku = scalar,
        ConfigRow::FableModel => models.fable = scalar,
        ConfigRow::SubagentModel => models.subagent = scalar,
        // The other rows carry no `ModelSettings` field and never route here
        // (`commit_model_field` is the only caller). Named, so a new
        // `ConfigRow` variant fails the build instead of a silent no-op.
        ConfigRow::Name
        | ConfigRow::AutoStart
        | ConfigRow::BaseUrl
        | ConfigRow::ApiKey
        | ConfigRow::ModelOverrideAdd
        | ConfigRow::EnvEntry(_)
        | ConfigRow::EnvAdd
        | ConfigRow::Login
        | ConfigRow::DeleteCreds
        | ConfigRow::ClearSessionToken
        | ConfigRow::Disabled
        | ConfigRow::Delete
        | ConfigRow::Create => {}
    }
}

/// Space on the `model` row: advance the alias cycle and persist. A custom value
/// (set via ⏎) is outside the cycle, so the first space resets it to `default`.
/// `editing_name` is `None` on the still-unsaved `+ new` draft — there's no
/// profile yet to persist into, so it just advances the buffer in place; the
/// value is folded in on `commit_new_account`.
fn cycle_model(app: &mut App) {
    let editing_name = app
        .config_draft
        .as_ref()
        .and_then(|d| d.editing_name.clone())
        .map(ProfileName::from);
    let Some(name) = editing_name else {
        if let Some(d) = app.config_draft.as_mut() {
            let next = next_model_preset(d.model.trimmed_some().as_deref()).unwrap_or_default();
            d.model = InputState::new(&next);
        }
        return;
    };
    let mut models = {
        let cfg = app.config();
        cfg.find(&name)
            .map(|p| p.models.clone())
            .unwrap_or_default()
    };
    models.default = next_model_preset(models.default.as_deref());
    let result = {
        let mut cfg = app.config();
        edit_profile_model(&mut cfg, &name, models)
    };
    match result {
        Ok(()) => {
            let value = {
                let cfg = app.config();
                cfg.find(&name)
                    .and_then(|p| p.models.default.clone())
                    .unwrap_or_default()
            };
            if let Some(d) = app.config_draft.as_mut() {
                d.model = InputState::new(&value);
            }
        }
        Err(e) => app.toast(ToastKind::Danger, format!("model update failed\n{e}")),
    }
}

/// Advance the `model` alias cycle: default → opus → sonnet → haiku → opusplan →
/// default. A value outside [`MODEL_PRESETS`] (a custom id) collapses to default.
fn next_model_preset(current: Option<&str>) -> Option<String> {
    match current {
        None => Some(MODEL_PRESETS[0].to_string()),
        Some(cur) => match MODEL_PRESETS.iter().position(|p| *p == cur) {
            Some(i) if i + 1 < MODEL_PRESETS.len() => Some(MODEL_PRESETS[i + 1].to_string()),
            _ => None,
        },
    }
}

/// ⏎ on an `EnvEntry`: seed the shared value buffer from the entry's saved value
/// and open the inline value editor.
fn enter_env_value_edit(app: &mut App, i: usize) {
    let Some(name) = app
        .config_draft
        .as_ref()
        .and_then(|d| d.editing_name.clone())
        .map(ProfileName::from)
    else {
        return;
    };
    let value = {
        let cfg = app.config();
        cfg.find(&name)
            .and_then(|p| p.env.values().nth(i).cloned())
            .unwrap_or_default()
    };
    if let Some(d) = app.config_draft.as_mut() {
        d.env_value = InputState::new(&value);
        d.active = Some(ConfigRow::EnvEntry(i));
    }
}

/// ⏎ on `+ add env`: open an empty key editor (commit runs the collision check).
fn enter_env_add_edit(app: &mut App) {
    if let Some(d) = app.config_draft.as_mut() {
        d.env_new_key = InputState::new("");
        d.active = Some(ConfigRow::EnvAdd);
    }
}

/// ⏎ in an env value editor: fold the trimmed buffer into the entry (key
/// unchanged), persist via [`edit_profile_env`], then reseed from the saved value.
fn commit_env_value(app: &mut App, i: usize) {
    let Some(name) = app
        .config_draft
        .as_ref()
        .and_then(|d| d.editing_name.clone())
        .map(ProfileName::from)
    else {
        return;
    };
    let value = app
        .config_draft
        .as_ref()
        .map(|d| d.env_value.trimmed().to_string())
        .unwrap_or_default();
    let new_env = {
        let cfg = app.config();
        cfg.find(&name).map(|p| {
            let mut env = p.env.clone();
            if let Some(key) = p.env.keys().nth(i).cloned() {
                env.insert(key, value.clone());
            }
            env
        })
    };
    let Some(new_env) = new_env else {
        if let Some(d) = app.config_draft.as_mut() {
            d.active = None;
        }
        return;
    };
    let result = {
        let mut cfg = app.config();
        edit_profile_env(&mut cfg, &name, new_env)
    };
    match result {
        Ok(()) => {
            let saved = {
                let cfg = app.config();
                cfg.find(&name)
                    .and_then(|p| p.env.values().nth(i).cloned())
                    .unwrap_or_default()
            };
            if let Some(d) = app.config_draft.as_mut() {
                d.env_value = InputState::new(&saved);
                d.active = None;
            }
        }
        Err(e) => app.toast(ToastKind::Danger, format!("env update failed\n{e}")),
    }
}

/// ⏎ in the add-env key editor: validate, run the 3-source collision check, and
/// either prompt (overwrite/keep/cancel) or add the key and drop into value-edit.
fn commit_env_new_key(app: &mut App) {
    let Some(name) = app
        .config_draft
        .as_ref()
        .and_then(|d| d.editing_name.clone())
        .map(ProfileName::from)
    else {
        return;
    };
    let key = app
        .config_draft
        .as_ref()
        .map(|d| d.env_new_key.trimmed().to_string())
        .unwrap_or_default();
    if key.is_empty() {
        if let Some(d) = app.config_draft.as_mut() {
            d.active = None;
        }
        return;
    }
    // A settings.json env key is a shell-style name; spaces or `=` are never valid.
    if key.contains(char::is_whitespace) || key.contains('=') {
        app.toast(
            ToastKind::Danger,
            "env key can't contain spaces or '='".to_string(),
        );
        return; // keep the editor open so the user can fix it
    }
    let base_env_keys = claude_settings_env_keys().unwrap_or_default();
    let collision = {
        let cfg = app.config();
        cfg.find(&name)
            .and_then(|p| classify_env_key(p, &base_env_keys, &key))
    };
    if let Some(d) = app.config_draft.as_mut() {
        d.active = None;
    }
    match collision {
        Some(c) => app.modals.push(Modal::EnvCollision(env_collision_form(
            name.to_string(),
            key,
            c,
        ))),
        None => env_add_commit(app, &name, &key),
    }
}

/// Add (when new) the custom key with an empty value, then focus its row and open
/// the value editor. An existing key (overwrite chosen on the prompt) is edited in
/// place — never re-blanked.
fn env_add_commit(app: &mut App, name: &ProfileName, key: &str) {
    let exists = {
        let cfg = app.config();
        cfg.find(name)
            .map(|p| p.env.contains_key(key))
            .unwrap_or(false)
    };
    if !exists {
        let new_env = {
            let cfg = app.config();
            cfg.find(name).map(|p| {
                let mut env = p.env.clone();
                env.insert(key.to_string(), String::new());
                env
            })
        };
        let Some(new_env) = new_env else {
            return;
        };
        if let Err(e) = {
            let mut cfg = app.config();
            edit_profile_env(&mut cfg, name, new_env)
        } {
            app.toast(ToastKind::Danger, format!("env update failed\n{e}"));
            return;
        }
    }
    let idx = {
        let cfg = app.config();
        cfg.find(name)
            .and_then(|p| p.env.keys().position(|k| k == key))
    };
    let Some(idx) = idx else {
        return;
    };
    if let Some(row_pos) = env_entry_row_index(app, idx) {
        app.config_action_cursor = row_pos;
    }
    enter_env_value_edit(app, idx);
}

/// Position of `EnvEntry(sorted_idx)` in the current detail-row list, if present.
fn env_entry_row_index(app: &App, sorted_idx: usize) -> Option<usize> {
    config_rows(app)
        .iter()
        .position(|r| *r == ConfigRow::EnvEntry(sorted_idx))
}

/// Build the collision prompt for a candidate key against its colliding source.
/// `reason` is a noun phrase ("the base url field", …) so both the prompt message
/// (`used by {reason}`) and the overwrite detail (`overrides {reason}`) read right.
fn env_collision_form(
    profile: String,
    key: String,
    collision: EnvKeyCollision,
) -> EnvCollisionForm {
    let (reason, existing_idx) = match collision {
        EnvKeyCollision::Managed(label) => (label.to_string(), None),
        EnvKeyCollision::ProfileField(idx) => (
            "another custom field on this account".to_string(),
            Some(idx),
        ),
        EnvKeyCollision::BaseSettings => ("your ~/.claude/settings.json".to_string(), None),
    };
    EnvCollisionForm {
        profile,
        key,
        reason,
        existing_idx,
        // Default to the safe choice (cancel), per the modal cancel-default rule.
        cursor: EnvCollisionForm::options().len() - 1,
    }
}

fn handle_env_collision_key(app: &mut App, key: KeyEvent) {
    let Some(Modal::EnvCollision(state)) = app.modals.last_mut() else {
        return;
    };
    let last = EnvCollisionForm::options().len() - 1;
    match key.code {
        KeyCode::Up => {
            state.cursor = if state.cursor == 0 {
                last
            } else {
                state.cursor - 1
            };
        }
        KeyCode::Down => {
            state.cursor = if state.cursor >= last {
                0
            } else {
                state.cursor + 1
            };
        }
        KeyCode::Esc | KeyCode::Char('q') => {
            app.modals.pop();
        }
        KeyCode::Enter | KeyCode::Char(' ') => {
            let choice = EnvCollisionForm::options()[state.cursor.min(last)];
            let name = ProfileName::from(state.profile.clone());
            let pending = state.key.clone();
            let existing_idx = state.existing_idx;
            app.modals.pop();
            run_env_collision_choice(app, &name, &pending, existing_idx, choice);
        }
        _ => {}
    }
}

fn run_env_collision_choice(
    app: &mut App,
    name: &ProfileName,
    key: &str,
    existing_idx: Option<usize>,
    choice: EnvCollisionChoice,
) {
    match choice {
        EnvCollisionChoice::Overwrite => env_add_commit(app, name, key),
        EnvCollisionChoice::KeepExisting => {
            // For an own-field clash, jump to the existing entry; else just dismiss.
            if let Some(idx) = existing_idx
                && let Some(row_pos) = env_entry_row_index(app, idx)
            {
                app.config_action_cursor = row_pos;
            }
        }
        EnvCollisionChoice::Cancel => {}
    }
}

fn commit_rename(app: &mut App) {
    let Some(d) = app.config_draft.as_ref() else {
        return;
    };
    let Some(old) = d.editing_name.clone() else {
        return;
    };
    let new = d.name.trimmed().to_string();
    if new == old {
        if let Some(d) = app.config_draft.as_mut() {
            d.active = None;
        }
        return;
    }
    let validation = {
        let cfg = app.config();
        validate_profile_name(&new, &cfg.names(), Some(old.as_str()))
    };
    if let Err(e) = validation {
        app.toast(ToastKind::Danger, format!("{e}"));
        return;
    }
    // The rotation guard is taken BEFORE `app.config()`: ROTATION ranks outside
    // `Config`, so acquiring it under the config guard is the inversion
    // `lockorder` asserts on.
    let old_pn = ProfileName::from(old.clone());
    let new_pn = ProfileName::from(new.clone());
    let result = rotation_guard_for_mutation(&old_pn).and_then(|rotation| {
        let mut cfg = app.config();
        rename_profile(&mut cfg, &old_pn, &new_pn, &rotation)
    });
    match result {
        Ok(()) => {
            app.refresh_tokens();
            app.last_reload_fp = reload_fingerprint();
            if let Some(d) = app.config_draft.as_mut() {
                d.editing_name = Some(new.clone());
                d.name = InputState::new(&new);
                d.active = None;
            }
            app.toast(ToastKind::Success, format!("renamed '{old}' → '{new}'"));
        }
        Err(e) => app.toast(ToastKind::Danger, format!("rename failed\n{e}")),
    }
}

fn commit_endpoint(app: &mut App) {
    let Some(d) = app.config_draft.as_ref() else {
        return;
    };
    let Some(name) = d.editing_name.clone().map(ProfileName::from) else {
        return;
    };
    let active_field = d.active;
    let base_url = d.base_url.trimmed_some();
    // An API key only makes sense with a base URL; drop it for OAuth.
    let api_key = if base_url.is_some() {
        d.api_key.trimmed_some()
    } else {
        None
    };
    let result = {
        let mut cfg = app.config();
        edit_profile_endpoint(&mut cfg, &name, base_url, api_key)
    };
    match result {
        Ok(()) => {
            // Reseed from the saved profile (API key may have been dropped).
            let (base, key) = {
                let cfg = app.config();
                let p = cfg.find(&name);
                (
                    p.and_then(|p| p.base_url.clone()).unwrap_or_default(),
                    p.and_then(|p| p.api_key.clone()).unwrap_or_default(),
                )
            };
            if let Some(d) = app.config_draft.as_mut() {
                d.base_url = InputState::new(&base);
                d.api_key = InputState::new(&key);
                // Re-login chain: after the base-url step, advance into the
                // api-key editor instead of ending (only while it's still an API
                // account — an emptied base url flipped it to OAuth, key dropped).
                if d.relogin_chain && active_field == Some(ConfigRow::BaseUrl) && !base.is_empty() {
                    d.api_key.end();
                    d.active = Some(ConfigRow::ApiKey);
                } else {
                    d.relogin_chain = false;
                    d.active = None;
                }
            }
        }
        Err(e) => app.toast(ToastKind::Danger, format!("edit failed\n{e}")),
    }
}

fn commit_new_account(app: &mut App) {
    let Some(d) = app.config_draft.as_ref() else {
        return;
    };
    let name = d.name.trimmed().to_string();
    let base_url = d.base_url.trimmed_some();
    let api_key = d.api_key.trimmed_some();
    let model = d.model.trimmed_some();
    // A draft-held mint only makes sense for an OAuth create; a typed base url
    // flipped the form to API mode (login row hidden), so the mint is dropped.
    let captured = if base_url.is_none() {
        d.captured_login.clone()
    } else {
        None
    };
    let mint_discarded = base_url.is_some() && d.captured_login.is_some();
    let validation = {
        let cfg = app.config();
        validate_profile_name(&name, &cfg.names(), None)
    };
    if let Err(e) = validation {
        app.toast(ToastKind::Danger, format!("{e}"));
        return;
    }
    let api_key = if base_url.is_some() { api_key } else { None };
    let result = {
        let mut cfg = app.config();
        match captured {
            // The draft parked the login's uuid until this moment fixed the
            // name; `create_profile_from_login` anchors it on the commit.
            Some(login) => create_profile_from_login(
                &mut cfg,
                name.clone(),
                model,
                login.credentials,
                login.account_uuid,
            ),
            None => create_blank_profile(&mut cfg, name.clone(), base_url, api_key, model),
        }
    };
    match result {
        Ok(()) => {
            if mint_discarded {
                app.toast(
                    ToastKind::Info,
                    "base url set\nthe captured oauth login was discarded",
                );
            }
            app.refresh_tokens();
            app.last_reload_fp = reload_fingerprint();
            let new_idx = app
                .config()
                .profiles
                .iter()
                .position(|p| p.name == name)
                .unwrap_or(0);
            app.profile_cursor = new_idx;
            app.config_focus = ConfigFocus::Profiles;
            app.config_draft = None;
            app.toast(ToastKind::Success, format!("created '{name}'"));
        }
        Err(e) => app.toast(ToastKind::Danger, format!("create failed\n{e}")),
    }
}

fn perform_delete(app: &mut App, name: &ProfileName) {
    // The unforced guard in `delete_profile` refuses a live-session profile,
    // which would otherwise dead-end the TUI on a danger toast with no way
    // forward. Confirm the deauth risk instead of attempting (and failing)
    // the unforced delete first.
    if crate::runtime::has_live_session(name) {
        app.modals.push(Modal::Confirm(ConfirmState {
            message: format!("delete '{name}' anyway?"),
            detail: Some(
                "this account has a live clauth start session; deleting it may log that \
                 session out."
                    .to_string(),
            ),
            choice: false,
            on_confirm: ConfirmAction::DeleteLiveSession(name.to_string()),
        }));
        return;
    }
    finish_delete(app, name, false);
}

fn finish_delete(app: &mut App, name: &ProfileName, force: bool) {
    // Before `app.config()`, for the reason the rename path states.
    let result = rotation_guard_for_mutation(name).and_then(|rotation| {
        let mut cfg = app.config();
        delete_profile(&mut cfg, name, force, &rotation)
    });
    match result {
        Ok(()) => {
            app.refresh_tokens();
            app.last_reload_fp = reload_fingerprint();
            app.config_focus = ConfigFocus::Profiles;
            app.config_draft = None;
            app.clamp_profile_cursor();
            app.toast(ToastKind::Success, format!("deleted '{name}'"));
        }
        Err(e) => app.toast(ToastKind::Danger, format!("delete failed\n{e}")),
    }
}

/// The action menu's `disable account` / `enable account` off the Setup pane,
/// where there is no row to arm. Disabling routes through the confirm modal
/// instead (the row's own arm-then-confirm, in the shape a modal can carry);
/// enabling is harmless, so it fires straight away, exactly as the row does.
///
/// The two gates the row renders inert speak up here: a menu carries no dimmed
/// item, so a silent no-op would read as a broken pick.
fn toggle_focused_account_disabled(app: &mut App) {
    let Some((name, _, _)) = focused_account(app) else {
        return;
    };
    if app.config().is_active(&name) {
        app.toast(
            ToastKind::Warning,
            format!("'{name}' is the active account\nswitch away before disabling it"),
        );
        return;
    }
    if crate::runtime::has_live_session(&name) {
        app.toast(
            ToastKind::Warning,
            format!("'{name}' has a live session\nclose it before disabling"),
        );
        return;
    }
    if app.config().find(&name).is_some_and(Profile::is_disabled) {
        toggle_profile_disabled(app, &name);
        return;
    }
    app.modals.push(Modal::Confirm(ConfirmState {
        message: format!("disable '{name}'?"),
        detail: Some(DISABLE_DETAIL.to_string()),
        choice: false,
        on_confirm: ConfirmAction::DisableOne(name.to_string()),
    }));
}

// ── Setup menu: duplicate + presets ───────────────────────────────────────────

/// `duplicate account`: ask for the new name. The copy itself runs on ⏎, so
/// esc costs nothing.
fn prompt_duplicate_profile(app: &mut App) {
    let Some((name, _, _)) = focused_account(app) else {
        return;
    };
    app.modals.push(Modal::NamePrompt(NamePromptForm {
        input: InputState::new(""),
        action: NamePromptAction::DuplicateProfile(name.to_string()),
    }));
}

/// `save as preset`: ask for the preset name.
fn prompt_save_preset(app: &mut App) {
    let Some((name, _, _)) = focused_account(app) else {
        return;
    };
    app.modals.push(Modal::NamePrompt(NamePromptForm {
        input: InputState::new(""),
        action: NamePromptAction::SavePreset(name.to_string()),
    }));
}

/// `apply preset`: open the picker. The list always holds the built-ins, so
/// it is never empty and needs no empty state. On `+ new` the target is the
/// draft's name buffer (empty until the user types one); the preset's fields
/// are stamped into the draft, not a saved profile.
fn open_preset_picker(app: &mut App) {
    let target = if let Some((name, _, _)) = focused_account(app) {
        name.to_string()
    } else if app.profile_cursor >= app.profile_count() {
        if app.config_draft.is_none() {
            app.config_draft = Some(build_draft_new());
        }
        app.config_draft
            .as_ref()
            .map(|d| d.name.trimmed().to_string())
            .unwrap_or_default()
    } else {
        return;
    };
    app.modals.push(Modal::PresetPicker(PresetPickerForm {
        target,
        presets: crate::presets::list_presets(),
        cursor: 0,
    }));
}

/// ⏎ on the name prompt. The name is validated for the action it carries —
/// a duplicate against the profile roster, a preset against the preset store —
/// and a failure toasts with the modal still open so the name can be fixed.
fn handle_name_prompt_key(app: &mut App, key: KeyEvent) {
    let Some(Modal::NamePrompt(form)) = app.modals.last_mut() else {
        return;
    };
    match key.code {
        // Esc closes; `q` is a valid name character and falls through to the
        // input editor. (The other modals' `q` shortcut doesn't apply to a
        // text field.)
        KeyCode::Esc => {
            app.modals.pop();
        }
        KeyCode::Enter => {
            let name = form.input.trimmed().to_string();
            let action = form.action.clone();
            match action {
                NamePromptAction::DuplicateProfile(source) => {
                    let validation = {
                        let cfg = app.config();
                        validate_profile_name(&name, &cfg.names(), None)
                    };
                    if let Err(e) = validation {
                        app.toast(ToastKind::Danger, format!("{e}"));
                        return;
                    }
                    app.modals.pop();
                    duplicate_profile_into(
                        app,
                        &ProfileName::from(source),
                        &ProfileName::from(name),
                    );
                }
                NamePromptAction::SavePreset(source) => {
                    // The preset store is its own namespace, so the roster is
                    // empty here; only the charset + built-in rules apply.
                    if let Err(e) = validate_profile_name(&name, &[], None) {
                        app.toast(ToastKind::Danger, format!("{e}"));
                        return;
                    }
                    if crate::presets::is_builtin(&name) {
                        app.toast(
                            ToastKind::Danger,
                            format!("'{name}' is a built-in preset\npick another name"),
                        );
                        return;
                    }
                    app.modals.pop();
                    if crate::presets::preset_exists(&name) {
                        app.modals.push(Modal::Confirm(ConfirmState {
                            message: format!("preset '{name}' already exists."),
                            detail: Some(
                                "overwrite it with this account's base url and model settings?"
                                    .to_string(),
                            ),
                            choice: false,
                            on_confirm: ConfirmAction::OverwritePreset(name, source),
                        }));
                        return;
                    }
                    save_preset_from(app, &source, &name);
                }
            }
        }
        _ => apply_input_edit(&mut form.input, key),
    }
}

fn handle_preset_picker_key(app: &mut App, key: KeyEvent) {
    let Some(Modal::PresetPicker(state)) = app.modals.last_mut() else {
        return;
    };
    let last = state.presets.len().saturating_sub(1);
    match key.code {
        KeyCode::Esc | KeyCode::Char('q') => {
            app.modals.pop();
        }
        KeyCode::Up => {
            state.cursor = if state.cursor == 0 {
                last
            } else {
                state.cursor - 1
            };
        }
        KeyCode::Down => {
            state.cursor = if state.cursor >= last {
                0
            } else {
                state.cursor + 1
            };
        }
        // Drop a custom preset. Built-ins have no file to drop, so the pick says
        // so rather than no-opping: a list carries no dimmed row here. The toast
        // fires with the picker still mounted so the user can pick another.
        KeyCode::Char('d') => {
            let Some(preset) = state.presets.get(state.cursor.min(last)).cloned() else {
                return;
            };
            if preset.builtin {
                app.toast(
                    ToastKind::Info,
                    format!("'{}' is built in and always available", preset.name),
                );
                return;
            }
            app.modals.pop();
            app.modals.push(Modal::Confirm(ConfirmState {
                message: format!("delete preset '{}'?", preset.name),
                detail: Some("accounts already stamped from it are untouched.".to_string()),
                choice: false,
                on_confirm: ConfirmAction::DeletePreset(preset.name),
            }));
        }
        KeyCode::Enter => {
            let picked = state.presets.get(state.cursor.min(last)).cloned();
            let target = state.target.clone();
            app.modals.pop();
            let Some(preset) = picked else {
                return;
            };
            // Naming the fields that would change is the whole point of the
            // warning: "this overwrites settings" says nothing a user can act on.
            let clobbered = {
                let cfg = app.config();
                cfg.find(&ProfileName::from(target.clone()))
                    .map(preset_clobbers)
                    .unwrap_or_default()
            };
            if clobbered.is_empty() {
                apply_preset_to(app, &target, &preset.name);
                return;
            }
            app.modals.push(Modal::Confirm(ConfirmState {
                message: format!("apply '{}' over '{target}'?", preset.name),
                detail: Some(format!("replaces {}.", clobbered.join(", "))),
                choice: false,
                on_confirm: ConfirmAction::ApplyPresetOver(target, preset.name),
            }));
        }
        _ => {}
    }
}

/// Which of the target's own fields an apply would land on, in row order and
/// spelled the way the Setup rows are. Empty means the preset writes into a
/// blank endpoint + model block and can go straight through. Every set field
/// counts: the apply REPLACES both blocks wholesale, so a field the preset
/// leaves unset is cleared rather than kept.
fn preset_clobbers(profile: &Profile) -> Vec<&'static str> {
    let mut out = Vec::new();
    if profile.base_url.is_some() {
        out.push("base url");
    }
    let m = &profile.models;
    for (label, set) in [
        ("model", m.default.is_some()),
        ("opus", m.opus.is_some()),
        ("sonnet", m.sonnet.is_some()),
        ("haiku", m.haiku.is_some()),
        ("fable", m.fable.is_some()),
        ("subagent", m.subagent.is_some()),
    ] {
        if set {
            out.push(label);
        }
    }
    out
}

/// Stamp `preset` onto `target`. A saved account is written in a single locked
/// transaction ([`edit_profile_preset`]) so a failure leaves the whole profile on
/// its prior state. On `+ new` (cursor past the roster) the target names the
/// unsaved draft, not a profile on disk — the preset's fields are written into
/// the draft's input buffers directly and committed when the create form fires.
/// Rebuilds the draft afterwards for a saved account, since every buffer it
/// holds is stale.
///
/// The preset is re-read here rather than carried from the picker: a confirm can
/// sit open across an edit to the store, and what the user confirmed was a NAME.
/// The account's own api key is preserved — a template names an endpoint, never
/// the credential for one.
fn apply_preset_to(app: &mut App, target: &str, preset: &str) {
    let Some(preset) = crate::presets::load_preset(preset) else {
        app.toast(ToastKind::Danger, format!("preset '{preset}' is gone"));
        return;
    };
    // `+ new`: target names the draft. Stamp the input buffers and return —
    // nothing is on disk until the create form commits.
    if app.profile_cursor >= app.profile_count() {
        stamp_draft_from_preset(app, &preset);
        return;
    }
    let target = ProfileName::from(target);
    let result = {
        let mut cfg = app.config();
        edit_profile_preset(
            &mut cfg,
            &target,
            preset.base_url.clone(),
            preset.models.clone(),
        )
    };
    match result {
        Ok(()) => {
            app.refresh_tokens();
            app.last_reload_fp = reload_fingerprint();
            if app.config_draft.is_some() {
                app.config_draft = Some(build_draft_existing(app, &target));
            }
            app.toast(
                ToastKind::Success,
                format!("applied '{}' to '{target}'", preset.name),
            );
        }
        Err(e) => app.toast(ToastKind::Danger, format!("apply preset failed\n{e}")),
    }
}

/// Write `preset`'s fields into the `+ new` draft's input buffers. Only fields
/// the preset carries are touched — a draft is a work in progress, so clearing
/// a buffer the user had typed (because the preset left that field unset) would
/// lose their work for no reason. The model-override fields the preset does
/// carry land in the corresponding tier row.
fn stamp_draft_from_preset(app: &mut App, preset: &crate::presets::Preset) {
    let Some(draft) = app.config_draft.as_mut() else {
        return;
    };
    if let Some(url) = preset.base_url.as_deref() {
        draft.base_url = InputState::new(url);
    }
    if let Some(m) = preset.models.default.as_deref() {
        draft.model = InputState::new(m);
    }
    for (value, field) in [
        (preset.models.opus.as_deref(), &mut draft.opus_model),
        (preset.models.sonnet.as_deref(), &mut draft.sonnet_model),
        (preset.models.haiku.as_deref(), &mut draft.haiku_model),
        (preset.models.fable.as_deref(), &mut draft.fable_model),
        (preset.models.subagent.as_deref(), &mut draft.subagent_model),
    ] {
        if let Some(m) = value {
            *field = InputState::new(m);
        }
    }
    let name = preset.name.clone();
    app.toast(
        ToastKind::Success,
        format!("stamped '{name}' onto the new account"),
    );
}

/// Save `source`'s base url + models under `preset`.
fn save_preset_from(app: &mut App, source: &str, preset: &str) {
    let fields = {
        let cfg = app.config();
        cfg.find(&ProfileName::from(source))
            .map(|p| (p.base_url.clone(), p.models.clone()))
    };
    let Some((base_url, models)) = fields else {
        app.toast(ToastKind::Danger, format!("'{source}' is gone"));
        return;
    };
    match crate::presets::save_preset(preset, &base_url, &models) {
        Ok(()) => app.toast(ToastKind::Success, format!("saved preset '{preset}'")),
        Err(e) => app.toast(ToastKind::Danger, format!("save preset failed\n{e}")),
    }
}

/// Copy `source` onto a new account named `new_name` and select it.
fn duplicate_profile_into(app: &mut App, source: &ProfileName, new_name: &ProfileName) {
    let result = {
        let mut cfg = app.config();
        duplicate_profile(&mut cfg, source, new_name.to_string())
    };
    match result {
        Ok(()) => {
            app.refresh_tokens();
            app.last_reload_fp = reload_fingerprint();
            let idx = app
                .config()
                .profiles
                .iter()
                .position(|p| p.name == *new_name)
                .unwrap_or(0);
            app.profile_cursor = idx;
            app.config_action_cursor = 0;
            if app.config_focus == ConfigFocus::Actions {
                app.config_draft = Some(build_draft_existing(app, new_name));
            }
            app.toast(
                ToastKind::Success,
                format!("duplicated '{source}' as '{new_name}'"),
            );
        }
        Err(e) => app.toast(ToastKind::Danger, format!("duplicate failed\n{e}")),
    }
}

/// Flip `name`'s `Profile::disabled` flag (Setup `disabled` row). Inert while
/// `name` is the active profile or holds a live `clauth start` session — the
/// same gate `actions::disable_profile` itself enforces, checked here TOO so
/// the row stays truly inert (silent no-op, matching the dimmed-row cloudy-
/// tui contract): `disable_profile`'s own refusal is a real `bail!`, and
/// without this early return every press would surface a red danger toast
/// instead of doing nothing, which is what a dimmed/disabled row is supposed
/// to mean. `disable_profile`/`enable_profile` persist into the live shared
/// `AppConfig` directly (`app.config()`), so the flip renders next frame with
/// no reload round-trip; `refresh_tokens` rebuilds the scheduler's per-profile
/// work lists (`collect_tokens`/`collect_third_party_entries` both filter on
/// `is_disabled`) the same way `toggle_auto_start` does, since a per-profile
/// `config.toml` write doesn't bump the mtime the periodic reload watches.
fn toggle_profile_disabled(app: &mut App, name: &ProfileName) {
    let currently_disabled = app.config().find(name).is_some_and(Profile::is_disabled);
    let gated = app.config().is_active(name) || crate::runtime::has_live_session(name);
    if gated {
        return;
    }
    let result = {
        let mut cfg = app.config();
        if currently_disabled {
            crate::actions::enable_profile(&mut cfg, name)
        } else {
            crate::actions::disable_profile(&mut cfg, name)
        }
    };
    match result {
        Ok(true) => {
            app.refresh_tokens();
            let verb = if currently_disabled {
                "enabled"
            } else {
                "disabled"
            };
            app.toast(ToastKind::Success, format!("{verb} '{name}'"));
        }
        Ok(false) => {}
        Err(e) => app.toast(ToastKind::Danger, format!("toggle failed\n{e}")),
    }
}

fn toggle_auto_start(app: &mut App, name: &ProfileName) {
    enum Outcome {
        NotOAuth,
        Saved(bool),
        SaveFailed(anyhow::Error),
        Missing,
    }
    let outcome = {
        let mut cfg = app.config();
        match cfg.find_mut(name) {
            None => Outcome::Missing,
            Some(profile) if !profile.is_oauth() => Outcome::NotOAuth,
            Some(profile) => {
                profile.auto_start = !profile.auto_start;
                let now_on = profile.auto_start;
                match save_profile(profile) {
                    Ok(()) => Outcome::Saved(now_on),
                    Err(e) => {
                        if let Some(p) = cfg.find_mut(name) {
                            p.auto_start = !now_on;
                        }
                        Outcome::SaveFailed(e)
                    }
                }
            }
        }
    };
    match outcome {
        Outcome::Missing => {}
        Outcome::NotOAuth => app.toast(
            ToastKind::Warning,
            "auto-start only works on oauth accounts",
        ),
        Outcome::Saved(_now_on) => {
            // Rebuild the scheduler's token snapshot so the new `auto_start` flag
            // reaches the fetch leg's window-lapsed gate. The per-profile
            // config.toml write doesn't bump the profiles.toml mtime that
            // `reload_if_state_changed` watches, so without this the toggle would
            // lag until the next unrelated snapshot rebuild. The periodic tick
            // then opens a window if this profile now lacks a live one.
            app.refresh_tokens();
        }
        Outcome::SaveFailed(e) => {
            app.toast(ToastKind::Danger, format!("save failed\n{e}"));
        }
    }
}

fn handle_confirm_key(app: &mut App, key: KeyEvent) {
    let Some(Modal::Confirm(state)) = app.modals.last_mut() else {
        return;
    };
    match key.code {
        KeyCode::Left | KeyCode::Right | KeyCode::Tab => {
            state.choice = !state.choice;
        }
        KeyCode::Char('y') => state.choice = true,
        KeyCode::Char('n') => state.choice = false,
        KeyCode::Esc | KeyCode::Char('q') => {
            app.modals.pop();
        }
        KeyCode::Enter | KeyCode::Char(' ') => {
            let confirmed = state.choice;
            let action = state.on_confirm.clone();
            app.modals.pop();
            if confirmed {
                run_confirm_action(app, action);
            }
        }
        _ => {}
    }
}

/// Run `crate::herdr::heal` with the knob the state now holds and surface the
/// outcome: success, non-empty refusal notes (warning), or failure (danger).
/// Shared by the `[f]` fix and the `delegate row text` options row — the knob
/// rides the heal the way `install` reads it, so the row written matches the
/// `delegate_row_text` set in the TUI.
fn run_herdr_heal(app: &mut App, path: &std::path::Path) {
    let delegate_row_text = app.config().state.herdr.delegate_row_text;
    match crate::herdr::heal(
        path,
        crate::herdr::DEFAULT_KEY,
        &crate::herdr::herdr_bin(),
        delegate_row_text,
    ) {
        Ok(notes) if notes.is_empty() => {
            // Name the knob state the write landed: the confirm copy for the
            // off direction says "drop the delegate token", so a success toast
            // reading "added the keybinding and sidebar row" would claim the
            // opposite of what the user confirmed.
            app.toast(
                ToastKind::Success,
                format!(
                    "wrote herdr's config: keybinding + sidebar row (delegate token {})",
                    if delegate_row_text { "on" } else { "off" }
                ),
            );
            recompute_plugin_checks(app, false);
        }
        Ok(notes) => {
            // Non-empty notes = pieces clauth refused to touch (a table it
            // cannot extend by appending). Reporting success over those is a
            // lie, so the notes surface verbatim.
            app.toast(
                ToastKind::Warning,
                format!("herdr's config needs attention\n{}", notes.join("\n")),
            );
            recompute_plugin_checks(app, false);
        }
        Err(e) => app.toast(ToastKind::Danger, format!("herdr config fix failed\n{e}")),
    }
}

fn run_confirm_action(app: &mut App, action: ConfirmAction) {
    match action {
        ConfirmAction::CaptureConflict(snapshot, from_divergence) => {
            app.modals.push(Modal::CaptureName(CaptureNameForm {
                snapshot,
                input: InputState::new(""),
                from_divergence,
            }));
        }
        ConfirmAction::CaptureOverwrite(snapshot, name, from_divergence) => {
            // Same deferred detach as the non-colliding path: only disown the
            // prior active profile once the overwrite is actually confirmed,
            // so a cancel (handled entirely by `handle_confirm_key` popping
            // without calling this) leaves it untouched.
            if from_divergence {
                let _ = detach_credentials_link();
                let mut cfg = app.config();
                let _ = with_state_lock(|held| {
                    cfg.state.set_active(None, held);
                    save_app_state(&cfg.state)
                });
            }
            let name = ProfileName::from(name);
            let result = {
                let mut cfg = app.config();
                overwrite_captured_profile(&mut cfg, &name, *snapshot)
            };
            match result {
                Ok(()) => {
                    app.refresh_tokens();
                    app.last_reload_fp = reload_fingerprint();
                    app.toast(
                        ToastKind::Success,
                        format!("overwrote '{name}' with the captured login"),
                    );
                }
                Err(e) => app.toast(ToastKind::Danger, format!("overwrite failed\n{e}")),
            }
        }
        ConfirmAction::AdoptDivergence(snapshot, name) => {
            // `name` is never the active profile (the picker lists only others),
            // so `overwrite_captured_profile` just rewrites its stored creds and
            // skips its own guarded relink. Then force the live link onto it and
            // make it active — the divergence is resolved onto the target.
            let name = ProfileName::from(name);
            // Captured before the activation replaces the marker, through the
            // one helper that also answers for a CLEARED marker — this arm can
            // run after a `switch_off` left the departed account's `[env]` in
            // the live file.
            let outgoing_env_keys = crate::actions::outgoing_env_keys(&app.config());
            let result = {
                let mut cfg = app.config();
                overwrite_captured_profile(&mut cfg, &name, *snapshot).and_then(|()| {
                    with_state_lock(|held| {
                        cfg.state.set_active(Some(name.clone()), held);
                        save_app_state(&cfg.state)
                    })
                })
            };
            let result = result
                .and_then(|()| force_link_profile_credentials(&name))
                .and_then(|()| {
                    // This arm activates OUTSIDE `overwrite_captured_profile`,
                    // so the settings write its own activate arms make has to
                    // happen here too: an endpoint that never reaches
                    // `settings.json` routes the live session at Anthropic
                    // under a profile that says otherwise.
                    let cfg = app.config();
                    let Some(profile) = cfg.find(&name) else {
                        return Ok(());
                    };
                    crate::claude::apply_profile_to_claude_settings(profile, &outgoing_env_keys)
                });
            match result {
                Ok(()) => {
                    app.refresh_tokens();
                    app.last_reload_fp = reload_fingerprint();
                    app.toast(
                        ToastKind::Success,
                        format!("saved the login into '{name}', now active"),
                    );
                }
                Err(e) => app.toast(ToastKind::Danger, format!("save failed\n{e}")),
            }
        }
        ConfirmAction::Switch(name) => {
            let name = ProfileName::from(name);
            if switch_gate_in_flight(&app.activity) {
                app.toast(
                    ToastKind::Warning,
                    "another switch is still in flight\ntry again in a moment",
                );
                return;
            }
            if !is_idle(&app.activity, &name) {
                app.toast(
                    ToastKind::Warning,
                    format!("'{name}' is already busy\ntry again in a moment"),
                );
                return;
            }
            perform_switch(app, &name);
        }
        ConfirmAction::DiscardDivergence(name) => run_discard_divergence(app, &name),
        ConfirmAction::RotateAll => {
            // Refuse if anything is in-flight. Bootstrap is a whole-worker
            // signal separate from per-profile activity; the flag covers the
            // gap between the last Refreshing slot clearing and the next fetch.
            if app.bootstrap_active.load(Ordering::SeqCst) || any_busy(&app.activity) {
                app.toast(
                    ToastKind::Warning,
                    "rotate-all skipped\nanother op is still in flight",
                );
                return;
            }
            let config = Arc::clone(&app.config);
            let refetch = Arc::clone(&app.refetch_queue);
            let activity = Arc::clone(&app.activity);
            let sender = app.op_sender.clone();
            spawn_worker(move || {
                let _ = oauth::refresh_all(&config, true, &refetch, &activity, &sender);
            });
            app.toast(ToastKind::Info, "rotating all tokens");
        }
        ConfirmAction::RotateOne(name) => {
            // Backstop for a session that started between arming the confirm and
            // running it: on macOS `rotate_one_inner` refuses, and an unguarded
            // "rotating 'X'" toast would be a success-shaped message for a no-op.
            let name = ProfileName::from(name);
            if crate::runtime::rotation_blocked_for(&name) {
                app.toast(
                    ToastKind::Warning,
                    format!("'{name}' {ROTATE_LIVE_SESSION_MSG}\n{ROTATE_LIVE_SESSION_TOAST}"),
                );
                return;
            }
            // The per-profile RotationGuard inside rotate_one serialises against a
            // live session's own refresh, so unlike RotateAll this doesn't need the
            // global any_busy gate — an unacquirable guard surfaces as a Danger
            // toast (contention blocks inside `acquire`, it never lands there).
            let config = Arc::clone(&app.config);
            let refetch = Arc::clone(&app.refetch_queue);
            let activity = Arc::clone(&app.activity);
            let sender = app.op_sender.clone();
            let target = name.clone();
            spawn_worker(move || {
                oauth::rotate_one(&config, &target, &refetch, &activity, &sender);
            });
            app.toast(ToastKind::Info, format!("rotating '{name}'"));
        }
        // Re-checked inside `toggle_profile_disabled`: an account can go active
        // or open a session between arming this confirm and running it.
        ConfirmAction::DisableOne(name) => toggle_profile_disabled(app, &ProfileName::from(name)),
        ConfirmAction::WireMcpServers => {
            match crate::plugin_probe::wire_mcp_server() {
                Ok(()) => {
                    app.toast(ToastKind::Success, "wired clauth into ~/.claude.json");
                    // Reflect the new wiring in the rows without a fresh version probe.
                    recompute_plugin_checks(app, false);
                }
                Err(e) => app.toast(ToastKind::Danger, format!("wire failed\n{e}")),
            }
        }
        ConfirmAction::RelinkCredentials(name) => {
            let name = ProfileName::from(name);
            match force_link_profile_credentials(&name) {
                Ok(()) => {
                    app.refresh_tokens();
                    app.toast(
                        ToastKind::Success,
                        format!("relinked credentials to '{name}'"),
                    );
                    recompute_plugin_checks(app, false);
                }
                Err(e) => app.toast(ToastKind::Danger, format!("relink failed\n{e}")),
            }
        }
        ConfirmAction::HealHerdrConfig(path) => run_herdr_heal(app, &path),
        ConfirmAction::HerdrDelegateRowText(path) => {
            // Persist the flipped knob FIRST: `run_herdr_heal` reads it back off
            // the state (the way `install` reads it), so heal writes the row the
            // user just asked for. Cancel never reaches here — the modal pops
            // without running the action.
            {
                let mut cfg = app.config();
                cfg.state.herdr.delegate_row_text = !cfg.state.herdr.delegate_row_text;
                let _ = save_app_state(&cfg.state);
            }
            app.last_reload_fp = reload_fingerprint();
            run_herdr_heal(app, &path);
        }
        ConfirmAction::InstallPlugin => match crate::plugin_host::install() {
            // A no-op from a row that read "not installed" means the backend
            // never ran — `claude` not on PATH is the reachable case (agentgear
            // skips an undetected backend). A green success toast over a
            // skipped install is a lie, so the warning names it instead.
            Ok(agentgear::Outcome::NoOp) => {
                app.toast(
                    ToastKind::Warning,
                    "plugin install made no changes\ninstall claude code first, then try again",
                );
                recompute_plugin_checks(app, false);
            }
            // Installed / Repaired / Adopted / Updated, or anything agentgear
            // adds later: a real change happened, so the success toast is
            // earned. (`Outcome` is #[non_exhaustive], so the wildcard is the
            // contract, not a shortcut.)
            Ok(outcome) => {
                app.toast(ToastKind::Success, format!("clauth plugin {outcome}"));
                // Reflect the fresh install in the rows without a version probe.
                recompute_plugin_checks(app, false);
            }
            Err(e) => app.toast(ToastKind::Danger, format!("install failed\n{e}")),
        },
        ConfirmAction::BlankCredentials(name) => {
            let name = ProfileName::from(name);
            let result = {
                let mut cfg = app.config();
                // OAuth accounts drop the token via the shared clearer; API
                // accounts drop only the api key, keeping the base-url shell.
                let is_oauth = cfg.find(&name).map(|p| p.login_is_oauth()).unwrap_or(true);
                if is_oauth {
                    clear_profile_credentials(&mut cfg, &name)
                } else {
                    clear_profile_api_key(&mut cfg, &name)
                }
            };
            match result {
                Ok(()) => {
                    app.refresh_tokens();
                    app.last_reload_fp = reload_fingerprint();
                    app.toast(ToastKind::Success, format!("logged out of '{name}'"));
                }
                Err(e) => app.toast(ToastKind::Danger, format!("log out failed\n{e}")),
            }
        }
        ConfirmAction::RestartLogin(name, is_new) => start_login(app, name, is_new),
        ConfirmAction::DeleteLiveSession(name) => {
            finish_delete(app, &ProfileName::from(name), true)
        }
        ConfirmAction::AddChainCandidate(name) => commit_chain_add(app, &ProfileName::from(name)),
        ConfirmAction::OverwritePreset(preset, source) => save_preset_from(app, &source, &preset),
        ConfirmAction::ApplyPresetOver(target, preset) => apply_preset_to(app, &target, &preset),
        ConfirmAction::DeletePreset(preset) => match crate::presets::delete_preset(&preset) {
            Ok(()) => app.toast(ToastKind::Success, format!("deleted preset '{preset}'")),
            Err(e) => app.toast(ToastKind::Danger, format!("delete preset failed\n{e}")),
        },
        ConfirmAction::Acknowledge => {}
    }
}

fn handle_divergence_key(app: &mut App, key: KeyEvent) {
    let Some(Modal::Divergence(state)) = app.modals.last_mut() else {
        return;
    };
    let actions = state.actions();
    let last = actions.len() - 1;
    match key.code {
        KeyCode::Up => {
            state.cursor = if state.cursor == 0 {
                last
            } else {
                state.cursor - 1
            };
        }
        KeyCode::Down => {
            state.cursor = if state.cursor >= last {
                0
            } else {
                state.cursor + 1
            };
        }
        KeyCode::Esc | KeyCode::Char('q') => {
            // Just close — the prompt is on-demand now; the non-blocking
            // banner keeps carrying the signal until the divergence resolves.
            app.modals.pop();
        }
        KeyCode::Enter | KeyCode::Char(' ') => {
            let action = actions[state.cursor.min(last)].clone();
            let active = state.active.clone();
            app.modals.pop();
            match action {
                DivergenceAction::SwitchToOwner(owner) => {
                    // Same confirmed path as the "save elsewhere" picker: the
                    // live login is captured into its own profile, which then
                    // becomes active. Nothing is lost, nothing else rewritten.
                    let Some(snapshot) = capture_live_or_toast(app) else {
                        return;
                    };
                    app.modals.push(Modal::Confirm(ConfirmState {
                        message: format!("switch to '{owner}'? the live login is its account."),
                        detail: Some(format!(
                            "the login is saved into '{owner}' and '{owner}' becomes the active \
                             account. the running claude is untouched."
                        )),
                        choice: false,
                        on_confirm: ConfirmAction::AdoptDivergence(Box::new(snapshot), owner),
                    }));
                }
                DivergenceAction::Choice(choice) => run_divergence_choice(app, &active, choice),
            }
        }
        _ => {}
    }
}

fn run_divergence_choice(app: &mut App, active: &str, choice: DivergenceChoice) {
    match choice {
        DivergenceChoice::Overwrite => {
            let snapshot_result = {
                let mut cfg = app.config();
                force_snapshot_active_credentials(&mut cfg)
            };
            if let Err(e) = snapshot_result {
                app.toast(ToastKind::Danger, format!("overwrite failed\n{e}"));
                return;
            }
            if let Err(e) = force_link_profile_credentials(&ProfileName::from(active)) {
                app.toast(ToastKind::Danger, format!("relink failed\n{e}"));
                return;
            }
            app.refresh_tokens();
            app.toast(
                ToastKind::Success,
                format!("saved live credentials into '{active}'"),
            );
        }
        DivergenceChoice::NewProfile => open_divergence_target_picker(app),
        DivergenceChoice::Discard => {
            app.modals.push(Modal::Confirm(ConfirmState {
                message: format!("discard the new login and restore '{active}'?"),
                detail: Some(
                    "claude code's freshly written credentials will be overwritten with the account's stored tokens.".to_string(),
                ),
                choice: false,
                on_confirm: ConfirmAction::DiscardDivergence(active.to_string()),
            }));
        }
    }
}

fn run_discard_divergence(app: &mut App, active: &str) {
    if let Err(e) = force_link_profile_credentials(&ProfileName::from(active)) {
        app.toast(ToastKind::Danger, format!("discard failed\n{e}"));
        return;
    }
    app.toast(
        ToastKind::Warning,
        format!("discarded new login\nrestored '{active}'"),
    );
}

/// Open the "save elsewhere" picker from the Divergence modal. Lists every
/// profile except the active (diverged) one; a refresh-token match is
/// pre-selected. With no other profile to pick, drops straight into a
/// new-profile capture.
fn open_divergence_target_picker(app: &mut App) {
    let (targets, preselect) = {
        let cfg = app.config();
        let active = cfg.state.active_profile.as_deref();
        let targets: Vec<String> = cfg
            .profiles
            .iter()
            .map(|p| p.name.as_str().to_string())
            .filter(|n| Some(n.as_str()) != active)
            .collect();
        let live = read_claude_credentials().ok().flatten();
        let preselect = find_matching_oauth_profile(&cfg, live.as_ref())
            .and_then(|m| targets.iter().position(|n| n.as_str() == m.as_str()))
            .map_or(0, |i| i + 1);
        (targets, preselect)
    };
    if targets.is_empty() {
        begin_capture(app, true);
        return;
    }
    app.modals
        .push(Modal::DivergenceTarget(DivergenceTargetForm {
            targets,
            cursor: preselect,
        }));
}

/// Rows: 0 = `+ new profile` (→ capture-name), 1.. = existing profiles
/// (→ overwrite-confirm on that profile). Both routes carry `from_divergence`,
/// so the prior active is detached only once the action is confirmed and a
/// cancel leaves everything as-is.
fn handle_divergence_target_key(app: &mut App, key: KeyEvent) {
    let Some(Modal::DivergenceTarget(form)) = app.modals.last_mut() else {
        return;
    };
    let last = form.targets.len(); // row 0 is "+ new"; rows 1..=len are profiles
    match key.code {
        KeyCode::Up => {
            form.cursor = if form.cursor == 0 {
                last
            } else {
                form.cursor - 1
            };
        }
        KeyCode::Down => {
            form.cursor = if form.cursor >= last {
                0
            } else {
                form.cursor + 1
            };
        }
        KeyCode::Esc | KeyCode::Char('q') => {
            app.modals.pop();
        }
        KeyCode::Enter | KeyCode::Char(' ') => {
            let cursor = form.cursor;
            let Some(Modal::DivergenceTarget(form)) = app.modals.pop() else {
                return;
            };
            if cursor == 0 {
                begin_capture(app, true);
                return;
            }
            let Some(target) = form.targets.get(cursor - 1).cloned() else {
                return;
            };
            let Some(snapshot) = capture_live_or_toast(app) else {
                return;
            };
            // Names no surviving endpoint, unlike the browser-login confirm:
            // this snapshot is a LIVE read (`capture_snapshot` reads the live
            // settings' endpoint), so what it carries describes the OUTGOING
            // active profile, not this target. An endpoint-carrying active
            // profile fills both fields and the target's pair is replaced by
            // it; an OAuth-active one fills neither and the preserve arm keeps
            // the target's. The outcome turns on an account this prompt is not
            // about, so it promises nothing about either field.
            app.modals.push(Modal::Confirm(ConfirmState {
                message: format!("save the live login into '{target}'?"),
                detail: Some(format!(
                    "'{target}' becomes the active account; its old credentials are replaced. usage history, env, and model settings are kept."
                )),
                choice: false,
                on_confirm: ConfirmAction::AdoptDivergence(Box::new(snapshot), target),
            }));
        }
        _ => {}
    }
}

fn handle_capture_name_key(app: &mut App, key: KeyEvent) {
    let Some(Modal::CaptureName(form)) = app.modals.last_mut() else {
        return;
    };
    match key.code {
        KeyCode::Esc | KeyCode::Char('q') => {
            app.modals.pop();
        }
        KeyCode::Enter => {
            let name = form.input.trimmed().to_string();
            // Chars/empty-only check here — the duplicate-name branch of
            // `validate_profile_name` is skipped (empty `existing`) so a
            // collision falls through to the canonical_name lookup below
            // instead of dead-ending with an "already exists" error.
            if let Err(e) = validate_profile_name(&name, &[], None) {
                app.toast(ToastKind::Danger, format!("{e}"));
                return;
            }
            let collision = {
                let cfg = app.config();
                cfg.canonical_name(&name)
            };
            let Some(Modal::CaptureName(form)) = app.modals.pop() else {
                return;
            };
            let CaptureNameForm {
                snapshot,
                from_divergence,
                ..
            } = form;
            if let Some(existing) = collision {
                // Issue #7: typing an existing profile's name used to dead-end
                // with an error. Route to the same confirm-modal machinery as
                // every other destructive action instead of a picker/new modal.
                app.modals.push(Modal::Confirm(ConfirmState {
                    message: format!("account '{existing}' already exists."),
                    detail: Some(
                        "overwrite its credentials with the captured login? usage history, env, and model settings are kept.".to_string(),
                    ),
                    choice: false,
                    on_confirm: ConfirmAction::CaptureOverwrite(
                        snapshot,
                        existing,
                        from_divergence,
                    ),
                }));
                return;
            }
            // Divergence capture: detach + deactivate only after name is
            // confirmed so `capture_into_profile` sees `active_profile.is_none()`
            // and links the new one. On Esc this never runs.
            if from_divergence {
                let _ = detach_credentials_link();
                let mut cfg = app.config();
                let _ = with_state_lock(|held| {
                    cfg.state.set_active(None, held);
                    save_app_state(&cfg.state)
                });
            }
            let result = {
                let mut cfg = app.config();
                capture_into_profile(&mut cfg, name.clone(), *snapshot)
            };
            match result {
                Ok(()) => {
                    app.refresh_tokens();
                    app.last_reload_fp = reload_fingerprint();
                    app.toast(ToastKind::Success, format!("captured '{name}'"));
                }
                Err(e) => app.toast(ToastKind::Danger, format!("capture failed\n{e}")),
            }
        }
        _ => apply_input_edit(&mut form.input, key),
    }
}

fn apply_input_edit(input: &mut InputState, key: KeyEvent) {
    match key.code {
        KeyCode::Char('w') if key.modifiers.contains(KeyModifiers::CONTROL) => input.delete_word(),
        KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => input.insert(c),
        KeyCode::Backspace => input.backspace(),
        KeyCode::Delete => input.delete(),
        KeyCode::Left => input.left(),
        KeyCode::Right => input.right(),
        KeyCode::Home => input.home(),
        KeyCode::End => input.end(),
        _ => {}
    }
}

// ── Per-tick maintenance ──────────────────────────────────────────────────────

/// Apply queued status-feed events to `app.status`. Surfaces a new-incident cue
/// (toast on the Status tab, tab-activity color elsewhere) and a manual-refresh
/// failure toast. Manual refresh always clears `fetching`.
fn drain_status_events(app: &mut App) {
    while let Ok(ev) = app.status_events.try_recv() {
        match ev {
            StatusEvent::Fetched {
                incidents,
                fetched_at_ms,
            } => {
                let was_manual = app.status.fetching;
                apply_status_incidents(app, incidents, fetched_at_ms, false, was_manual);
            }
            StatusEvent::Cached {
                incidents,
                fetched_at_ms,
            } => {
                // A cache fallback after a manual refresh means the live fetch
                // failed — surface that as a danger toast.
                let was_manual = app.status.fetching;
                apply_status_incidents(app, incidents, fetched_at_ms, true, false);
                if was_manual {
                    app.status.fetching = false;
                    app.toast(ToastKind::Danger, "status refresh failed\nshowing cached");
                }
            }
            StatusEvent::Failed(msg) => {
                let was_manual = app.status.fetching;
                app.status.fetching = false;
                // Keep an error only while nothing is loaded; avoid toast spam on
                // the steady-state retry loop. A manual failure does toast.
                if app.status.incidents.is_empty() {
                    app.status.error = Some(msg);
                }
                if was_manual {
                    app.toast(ToastKind::Danger, "status refresh failed");
                }
            }
        }
    }
}

/// Replace the incident list, clear the spinner, clamp the cursor, and run the
/// new-incident signal. `cached` marks the data as a cache load / fallback.
/// `manual` is true when this `Fetched` answers a manual refresh (toasts there).
fn apply_status_incidents(
    app: &mut App,
    mut incidents: Vec<Incident>,
    fetched_at_ms: u64,
    cached: bool,
    manual: bool,
) {
    let prev_selected_id = app.status.selected().map(|i| i.id.clone());

    app.status.fetching = false;
    app.status.cached = cached;
    app.status.fetched_at_ms = Some(fetched_at_ms);
    if !cached {
        app.status.error = None;
    }

    let newest_id = incidents.first().map(|i| i.id.clone());
    if let Some(newest) = &newest_id
        && app.status.seen_latest.as_ref() != Some(newest)
    {
        let is_initial = app.status.seen_latest.is_none();
        // Only signal genuinely new incidents, never the initial load.
        if !is_initial && let Some(incident) = incidents.first() {
            let severity = if incident_is_active(incident) {
                ToastKind::Warning
            } else {
                ToastKind::Info
            };
            if app.tab == Tab::Status {
                let title = crate::format::truncate(&incident.title, 40);
                app.toast(severity, format!("new incident: {title}"));
            } else {
                app.set_tab_activity(Tab::Status, severity);
            }
        }
        app.status.seen_latest = newest_id.clone();
    }
    let _ = manual;

    incidents.sort_by(|a, b| match (a.is_active(), b.is_active()) {
        (true, false) => std::cmp::Ordering::Less,
        (false, true) => std::cmp::Ordering::Greater,
        _ => b.started_ms.cmp(&a.started_ms),
    });
    app.status.incidents = incidents;

    if app.status.incidents.is_empty() {
        app.status.cursor = 0;
    } else if app.status.cursor >= app.status.incidents.len() {
        app.status.cursor = app.status.incidents.len() - 1;
    }
    if app.status.selected().map(|i| i.id.clone()) != prev_selected_id {
        app.status.detail_scroll = 0;
    }
}

/// Apply queued token-stats loads. A `Failed` keeps the last good snapshot so a
/// transient parse miss never blanks the tab.
fn drain_tokens_events(app: &mut App) {
    while let Ok(ev) = app.tokens_events.try_recv() {
        match ev {
            // Base seeds the first paint only; on a refresh the prior topped-up
            // snapshot stays visible until the new Loaded lands, so the cadence
            // never flickers the tab back to base-only data.
            crate::tokens::TokensEvent::Base(stats) => {
                app.tokens_failed = false;
                if app.token_stats.is_none() {
                    app.token_stats = Some(*stats);
                    // First paint from the cache: the transcript top-up is now in
                    // flight, so light the loading spinners until `Loaded` lands.
                    app.tokens_topping_up = true;
                }
            }
            crate::tokens::TokensEvent::Progress { done, total } => {
                app.tokens_progress = Some((done, total));
            }
            crate::tokens::TokensEvent::Loaded(stats) => {
                app.tokens_failed = false;
                app.tokens_topping_up = false;
                app.tokens_progress = None;
                app.token_stats = Some(*stats);
                // Re-clamp the model cursor in case the grouped list shrank.
                let len = token_model_count(app);
                if len > 0 && app.token_model_cursor >= len {
                    app.token_model_cursor = len - 1;
                }
            }
            // Surface failure only when there is nothing to show — a transient
            // read error mid-session keeps the last good snapshot.
            crate::tokens::TokensEvent::Failed => {
                app.tokens_topping_up = false;
                app.tokens_progress = None;
                if app.token_stats.is_none() {
                    app.tokens_failed = true;
                }
            }
        }
    }
}

fn drain_pricing_events(app: &mut App) {
    while let Ok(ev) = app.pricing_events.try_recv() {
        match ev {
            crate::pricing::PricingEvent::Loaded(table) => {
                app.price_table = Some(*table);
                app.price_failed = false;
            }
            // Surface failure only when there is nothing to show — a transient
            // fetch error mid-session keeps the last good table.
            crate::pricing::PricingEvent::Failed => {
                if app.price_table.is_none() {
                    app.price_failed = true;
                }
            }
        }
    }
}

/// Drain the login worker: track URL/stage events, and on a result apply it
/// (stash or overwrite) — discarding a stale result from a superseded login.
fn drain_login_events(app: &mut App) {
    while let Ok((generation, event)) = app.login_event_rx.try_recv() {
        if let Some(session) = app.login.as_mut()
            && session.generation == generation
        {
            match event {
                LoginEvent::Url(url) => session.url = Some(url),
                LoginEvent::Stage(stage) => session.stage = stage,
            }
        }
    }
    while let Ok((generation, result)) = app.login_result_rx.try_recv() {
        if app.login.as_ref().map(|s| s.generation) != Some(generation) {
            continue; // superseded login; drop it
        }
        let Some(session) = app.login.take() else {
            continue;
        };
        close_login_modal(app);
        match result {
            Ok(LoginResult::Oauth(creds)) => apply_login(app, session, *creds),
            Ok(LoginResult::Console(outcome)) => apply_console_login(app, session, *outcome),
            Err(e) => app.toast(
                ToastKind::Danger,
                format!("login for '{}' failed\n{e}", session.name),
            ),
        }
    }
}

/// Fold a completed login into the app on the UI thread. A new-account login
/// stashes the mint into the live `+ new` draft — the profile is created only
/// when `create account` fires (capture-then-commit). A re-login re-checks
/// existence at apply time and, since fresh creds replacing stored ones IS a
/// divergence, gates the overwrite on `AppState.default_divergence`: only an
/// `Overwrite` default applies silently; anything else asks first.
fn apply_login(app: &mut App, session: LoginSession, outcome: crate::oauth_login::LoginOutcome) {
    if session.is_new {
        // The `+ new` form may have been closed during the browser round-trip;
        // the mint lives only in the draft, so without one it is dropped. The
        // anchor waits for `create account`: the draft's name is still editable,
        // so the profile this login belongs to has no final name yet.
        let stashed = match app
            .config_draft
            .as_mut()
            .filter(|d| d.editing_name.is_none())
        {
            Some(draft) => {
                draft.captured_login = Some(Box::new(outcome));
                true
            }
            None => false,
        };
        if stashed {
            // Land the cursor on `create account`, the one step left.
            if let Some(idx) = config_rows(app)
                .iter()
                .position(|r| *r == ConfigRow::Create)
            {
                app.config_action_cursor = idx;
            }
            app.toast(ToastKind::Success, "logged in\ncreate account saves it");
        } else {
            app.toast(
                ToastKind::Warning,
                "login finished but the new-account form is no longer open\nlog in again",
            );
        }
        return;
    }

    let (exists, has_creds) = {
        let cfg = app.config();
        let profile = cfg.find(&ProfileName::from(session.name.clone()));
        (
            profile.is_some(),
            profile.and_then(|p| p.credentials.as_ref()).is_some(),
        )
    };
    if !exists {
        app.toast(
            ToastKind::Danger,
            format!("login failed\nprofile '{}' no longer exists", session.name),
        );
        return;
    }
    // The uuid rides the snapshot: this may land in a confirm modal instead of
    // committing now, and only the commit may seed the anchor.
    let snapshot = CaptureSnapshot {
        credentials: Some(outcome.credentials),
        base_url: None,
        api_key: None,
        account_uuid: outcome.account_uuid,
    };
    // No stored creds → nothing diverges; adopt silently (mirrors the
    // first-login adopt in `poll_credentials_divergence`).
    let apply_now = !has_creds
        || matches!(
            app.config().state.default_divergence,
            Some(DivergenceChoice::Overwrite)
        );
    if !apply_now {
        // Names the endpoint only where the preserve arm will actually keep it
        // — the same rule `reauth_confirm_object` enforces on the CLI prompt,
        // asked of the same predicate. This snapshot is credential-only, so
        // the arm turns on the stored profile alone.
        let keeps_endpoint = app
            .config()
            .find(&ProfileName::from(session.name.clone()))
            .is_some_and(crate::claude::has_own_inference_endpoint);
        app.modals.push(Modal::Confirm(ConfirmState {
            message: format!("replace the stored credentials for '{}'?", session.name),
            detail: Some(
                if keeps_endpoint {
                    "a fresh browser login finished for this account. the old tokens are dropped; chain slot, env, model settings, and its endpoint and api key stay."
                } else {
                    "a fresh browser login finished for this account. the old tokens are dropped; chain slot, env, and model settings stay."
                }
                .to_string(),
            ),
            choice: false,
            on_confirm: ConfirmAction::CaptureOverwrite(Box::new(snapshot), session.name, false),
        }));
        return;
    }
    let result = {
        let mut cfg = app.config();
        overwrite_captured_profile(&mut cfg, &ProfileName::from(session.name.clone()), snapshot)
    };
    match result {
        Ok(()) => {
            app.refresh_tokens();
            app.last_reload_fp = reload_fingerprint();
            app.toast(ToastKind::Success, format!("logged in '{}'", session.name));
        }
        Err(e) => app.toast(ToastKind::Danger, format!("login failed\n{e}")),
    }
}

/// Fold a captured Alibaba console session into its profile. Replaces the usage
/// session and NOTHING else — not the api key, which the callback also returns
/// scoped to a workspace rather than to this plan (`alibaba_login`'s module doc
/// has the reason it is dropped).
///
/// Re-checks the account at apply time the way [`apply_login`] does, and on the
/// same reasoning: a browser round-trip is long enough for the profile to be
/// deleted or repointed at a different endpoint underneath it, and a session is
/// only meaningful on the console its endpoint is administered from. There is no
/// divergence gate, because a lapsed session cannot be refreshed and re-running
/// this login is the routine repair rather than an overwrite to guard.
fn apply_console_login(
    app: &mut App,
    session: LoginSession,
    outcome: crate::alibaba_login::ConsoleLoginOutcome,
) {
    let name = ProfileName::from(session.name);
    let (exists, target) = {
        let cfg = app.config();
        let profile = cfg.find(&name);
        (
            profile.is_some(),
            profile.and_then(Profile::console_login_target),
        )
    };
    if !exists {
        app.toast(
            ToastKind::Danger,
            format!("console login failed\nprofile '{name}' no longer exists"),
        );
        return;
    }
    // The SITE, not just "still Alibaba". A session is minted on one console
    // front and is meaningless on the other, and the usage fetch keys on the
    // credential's OWN site rather than on `base_url` — so an endpoint moved
    // between two Model Studio hosts mid-round-trip would not degrade to a dead
    // session, it would quietly report the other plan's quota under this name.
    // Compared against what the console ANSWERED with, which is what got
    // stored, rather than against what the login opened.
    if target.map(|(site, _)| site) != Some(outcome.console.site) {
        app.toast(
            ToastKind::Danger,
            format!("console login failed\n'{name}' no longer points at the console this session came from"),
        );
        return;
    }
    let result = {
        let mut cfg = app.config();
        crate::actions::store_console_login(&mut cfg, &name, outcome.console)
    };
    match result {
        Ok(()) => {
            // The stored session is what the usage leg fetches with, and
            // `store_console_login` drops the cache the old one filled, so ask
            // for the figures the new one can actually read.
            app.refresh_tokens();
            app.manual_refresh_one(&name);
            // The window is the surprising part and the CLI's own summary leads
            // with it: the 48h runs from the aliyun browser sign-in, so a login
            // inherits what is left of it and can be worth minutes.
            app.toast(
                ToastKind::Success,
                format!(
                    "console session captured for '{name}'\nit expires 48h after your aliyun sign-in, not after this login"
                ),
            );
        }
        Err(e) => app.toast(ToastKind::Danger, format!("console login failed\n{e}")),
    }
}

pub(crate) fn on_tick(app: &mut App) {
    app.tick_count = app.tick_count.wrapping_add(1);

    while let Ok(ev) = app.update_results.try_recv() {
        match ev {
            UpdateEvent::Installed(v) => {
                app.toast(
                    ToastKind::Success,
                    format!("updated to v{v}\nrestart to apply"),
                );
            }
            UpdateEvent::Available(v) => {
                app.toast(
                    ToastKind::Info,
                    format!("update available: v{v}\nreinstall with cargo install clauth"),
                );
            }
        }
    }

    drain_op_results(app);
    drain_status_events(app);
    drain_tokens_events(app);
    drain_pricing_events(app);
    drain_login_events(app);

    if app.reload_if_state_changed() {
        app.clamp_profile_cursor();
    }
    app.apply_usage();

    drain_switch_gates(app);

    let auto_switch_targets: Vec<String> = app
        .pending_switch
        .lock()
        .map(|mut g| g.drain().collect())
        .unwrap_or_default();
    for name in auto_switch_targets {
        let name = ProfileName::from(name);
        if switch_gate_in_flight(&app.activity) || !is_idle(&app.activity, &name) {
            continue;
        }
        // The label keys on the DESTINATION, not on which pass fired: any move
        // landing on the preferred (home) account reads as a homecoming. The
        // healthy-active return pass is the usual source, but the exhaustion walk
        // can also land here when the preferred is the only clear member left —
        // both are genuinely "now on home", so the destination-based label holds
        // without threading the cause through `SwitchAction`.
        let returning = app.config().find(&name).is_some_and(|p| p.preferred);
        let msg = if returning {
            format!("returning to preferred account '{name}'")
        } else {
            format!("auto-switching to '{name}'")
        };
        app.toast(ToastKind::Warning, msg);
        app.set_tab_activity(Tab::Overview, ToastKind::Warning);
        perform_switch(app, &name);
    }

    drain_pending_switch_off(app);

    drain_startup_signals(app);
    maybe_spawn_bootstrap(app);

    poll_credentials_divergence(app);
    // Before the plugin refresh, which folds the tally into its runtime row and
    // would otherwise render this tick against the previous one's fleet.
    poll_live_sessions(app);
    poll_plugin_refresh(app);
    poll_daemon_health(app);

    update_banner(app);
    app.prune_toasts();
}

/// Re-probe the daemon presence + `status.json` health for the `● daemon`
/// header dot, at most once a second (a flock try-lock + `status.json` read is
/// cheap but not free, and the dot changes on a human timescale).
fn poll_daemon_health(app: &mut App) {
    const DAEMON_PROBE_INTERVAL: Duration = Duration::from_secs(1);
    if app.last_daemon_probe.elapsed() < DAEMON_PROBE_INTERVAL {
        return;
    }
    app.last_daemon_probe = Instant::now();
    app.daemon_health = crate::daemon::daemon_health();
}

/// Re-tally the live-session registry for the Overview `live` column, the
/// Fallback member card and the Plugin tab's `runtime` row, at most once a
/// second — a readdir plus an `open` + `try_lock` per row is cheap but not
/// per-frame cheap, and a session starting or exiting is a human-timescale
/// event. Ungated by tab: all three read it, and a snapshot a second stale on
/// arrival would show the wrong fleet for that second.
fn poll_live_sessions(app: &mut App) {
    const LIVE_SESSIONS_INTERVAL: Duration = Duration::from_secs(1);
    if app
        .last_live_sessions_refresh
        .is_some_and(|t| t.elapsed() < LIVE_SESSIONS_INTERVAL)
    {
        return;
    }
    app.last_live_sessions_refresh = Some(Instant::now());
    let tally = crate::live_sessions::LiveTally::collect(&app.config());
    app.live_sessions = tally;
}

/// Plugin tab live refresh: re-run the cheap local checks (session counts + link
/// state) at most once per interval while the tab is focused and no modal is open,
/// so a session started elsewhere shows up without a manual `r`. Never re-probes
/// `claude --version` or `clauth mcp` — both stay `r`-gated.
fn poll_plugin_refresh(app: &mut App) {
    const PLUGIN_REFRESH_INTERVAL: Duration = Duration::from_secs(1);

    if app.tab != Tab::Plugin || !app.modals.is_empty() {
        return;
    }
    if app.last_plugin_refresh.elapsed() < PLUGIN_REFRESH_INTERVAL {
        return;
    }
    app.last_plugin_refresh = Instant::now();
    recompute_plugin_checks(app, false);
}

/// Recompute the sticky banner from current app state. Called every tick.
/// Winner ordering: `DANGER` > `WARNING`; ties broken by whichever condition
/// is checked first. One condition per `if` block in priority order.
fn update_banner(app: &mut App) {
    // Profiles exist but none is active — either switch-off-all (set by
    // `perform_switch_off`) or a profile that was never linked. Claim "all
    // spent" only while some profile still shows a live spent window; without
    // that evidence (e.g. a credential-less sole profile) the honest wording is
    // "no active profile" (issue #2 read the generic banner as a stuck limit).
    // The evidence is the HARD cap, not the configurable soft line: `Off` (what
    // clears the active in the first place) keys on the cap, and a soft-blocked
    // member still serves — calling it spent would be a lie the code disagrees with.
    let cfg = app.config();
    let no_active = !cfg.profiles.is_empty() && cfg.state.active_profile.is_none();
    let any_spent = no_active && cfg.profiles.iter().any(crate::fallback::is_exhausted_hard);
    drop(cfg);

    // Divergence outranks the compact-size nudge (both WARNING): a live-login
    // mismatch is an account-integrity condition to act on, and `d` resolves it
    // even in a small terminal. `no_active` (DANGER) still wins, and it implies
    // no divergence anyway (the poll clears the notice without an active profile).
    let divergence_msg = app
        .divergence_pending
        .as_ref()
        .map(DivergenceNotice::banner_message);

    app.banner = if no_active {
        let message = if any_spent {
            "all accounts spent · switch to an account to resume"
        } else {
            "no active account · select one to resume"
        };
        Some(Banner {
            severity: BannerSeverity::Danger,
            message: message.to_string(),
        })
    } else if let Some(message) = divergence_msg {
        Some(Banner {
            severity: BannerSeverity::Warning,
            message,
        })
    } else if app.compact {
        // Terminal too small for the full layout. Lower severity than the
        // all-spent danger above, so danger wins the tie.
        Some(Banner {
            severity: BannerSeverity::Warning,
            message: "terminal too small · enlarge for full layout".to_string(),
        })
    } else {
        None
    };
}

/// Drain worker op results: clear activity slots, toast errors/successes,
/// rebuild token snapshot on success.
fn drain_op_results(app: &mut App) {
    let mut needs_token_snapshot_rebuild = false;
    while let Ok(OpResult { name, outcome }) = app.op_results.try_recv() {
        if let Ok(mut a) = app.activity.lock()
            && a.get(&name).copied() == Some(ProfileActivity::Refreshing)
        {
            a.remove(&name);
        }
        match outcome {
            Ok(()) => {
                needs_token_snapshot_rebuild = true;
                app.toast(ToastKind::Info, format!("rotated token for '{name}'"));
                app.set_tab_activity(Tab::Usage, ToastKind::Info);
            }
            Err(e) => {
                app.toast(
                    ToastKind::Danger,
                    format!("refresh for '{name}' failed\n{e}"),
                );
                app.set_tab_activity(Tab::Usage, ToastKind::Danger);
            }
        }
    }
    if needs_token_snapshot_rebuild {
        app.refresh_tokens();
    }
}

/// Complete switches whose off-thread AUTH-1 gate answered: clear the pending
/// mark, then relink (`Ready`/`Refreshed`) or refuse with the login hint
/// (`Broken`) / a retry hint (`Transient`). Mirrors `drain_pending_switch_off`'s
/// modal guard — completion can raise the Divergence prompt, which must not
/// stack under an open modal.
fn drain_switch_gates(app: &mut App) {
    if !app.modals.is_empty() {
        return;
    }
    while let Ok(SwitchGateResult { name, gate }) = app.switch_gates.try_recv() {
        clear_activity(&app.activity, &name);
        match gate {
            oauth::AuthGate::Ready => finalize_switch(app, &name),
            oauth::AuthGate::Refreshed => {
                app.refresh_tokens();
                finalize_switch(app, &name);
            }
            oauth::AuthGate::Broken => app.toast(
                ToastKind::Danger,
                crate::format::login_expired(&name).toast(),
            ),
            oauth::AuthGate::Transient(e) => app.toast(
                ToastKind::Danger,
                crate::format::refresh_transient(&name, &e).toast(),
            ),
        }
    }
}

/// Drain the wrap-off flag. Only drain when no modal is open — `perform_switch_off`
/// may raise a Divergence prompt; consuming the flag while one is open would
/// let the scheduler re-set it and stack duplicates.
fn drain_pending_switch_off(app: &mut App) {
    if !app.modals.is_empty() {
        return;
    }
    let switch_off_pending = app
        .pending_switch_off
        .lock()
        .map(|mut g| std::mem::replace(&mut *g, false))
        .unwrap_or(false);
    if switch_off_pending {
        perform_switch_off(app);
    }
}

/// Drain startup signals from reconcile/bootstrap workers.
fn drain_startup_signals(app: &mut App) {
    while let Ok(signal) = app.startup_results.try_recv() {
        match signal {
            StartupSignal::ReconcileDone => {
                app.reconcile_done = true;
            }
            StartupSignal::ReconcileNeedsPrompt { active } => {
                app.reconcile_done = true;
                resolve_or_note_divergence(app, &active);
            }
            StartupSignal::BootstrapDone => {
                app.finish_bootstrap();
            }
        }
    }
}

/// Spawn bootstrap once reconcile is done and no modal is open.
fn maybe_spawn_bootstrap(app: &mut App) {
    if app.bootstrap_started || !app.reconcile_done || !app.modals.is_empty() {
        return;
    }
    app.bootstrap_started = true;
    app.bootstrap_active.store(true, Ordering::SeqCst);
    app.spawn_bootstrap();
}

/// 1Hz check: keep [`App::divergence_pending`] (the non-blocking banner) in
/// sync with whether `.credentials.json` matches the active profile's stored
/// creds. Never pushes the modal — a divergence must not lock the whole TUI out
/// of browsing usage; <kbd>d</kbd> opens the resolver on demand, and
/// switch-shaped actions raise it themselves when resolution is actually
/// required. First-login adoption resolves automatically here, as does a
/// configured `default_divergence` within its owner gate
/// ([`resolve_or_note_divergence`]).
fn poll_credentials_divergence(app: &mut App) {
    const POLL_INTERVAL: Duration = Duration::from_secs(1);

    if app
        .last_divergence_check
        .is_some_and(|t| t.elapsed() < POLL_INTERVAL)
    {
        return;
    }
    app.last_divergence_check = Some(Instant::now());

    if !app.modals.is_empty() {
        return;
    }
    let Some(active) = app.config().state.active_profile.as_ref().cloned() else {
        app.divergence_pending = None;
        return;
    };
    if !matches!(
        classify_credentials_link(&active).ok(),
        Some(LinkState::Diverged)
    ) {
        app.divergence_pending = None; // resolved; a later divergence re-flags
        return;
    }
    // A logged-out shell is nothing to resolve: don't nag, and never let a
    // `default_divergence` Overwrite capture its blank tokens over the stored chain.
    if live_credentials_are_shell() {
        app.divergence_pending = None;
        return;
    }
    // First login on a credential-less profile: adopt silently, don't prompt.
    if is_first_login(&active).unwrap_or(false) {
        let result = {
            let mut cfg = app.config();
            adopt_first_login(&mut cfg, &active)
        };
        match result {
            Ok(()) => {
                app.refresh_tokens();
                app.last_reload_fp = reload_fingerprint();
                app.toast(ToastKind::Success, format!("saved login into '{active}'"));
            }
            Err(e) => app.toast(ToastKind::Danger, format!("adopt failed\n{e}")),
        }
        return;
    }
    // A login already saved in the active profile's store holds nothing unsaved —
    // the next switch re-installs it, losing no login — so it must not raise the
    // banner. This clears clauth's own symlink AND the macOS regular-file mirror CC
    // writes over it. The switch/defer gates apply the same exemption through
    // `live_diverged_and_unsaved`; the poll must adopt a first login before this
    // point, so it checks the predicate directly here instead.
    if crate::claude::live_login_is_stored(&active) {
        app.divergence_pending = None;
        return;
    }
    resolve_or_note_divergence(app, active.as_ref());
}

/// Apply the configured `default_divergence`, or flag the non-blocking banner
/// when nothing is configured — the shared tail of the startup reconcile and the
/// 1Hz poll.
///
/// The default is owner-gated: it may only resolve a login no stored SIBLING
/// owns. Owner-blind, an `Overwrite`/`NewProfile` default captures a sibling
/// profile's re-login into the active profile — credential misattribution with
/// no user say. A sibling-owned divergence falls through to the banner, whose
/// "switch to it" action is the resolution that case actually wants.
///
/// The banner is never a modal: the user opens the resolver with <kbd>d</kbd>
/// when they want it, and any switch-shaped action raises it itself. Startup and
/// the poll must never lock the TUI.
fn resolve_or_note_divergence(app: &mut App, active: &str) {
    if note_divergence(app, active) {
        return;
    }
    let default = app.config().state.default_divergence;
    if let Some(choice) = default {
        // The default IS the resolution, so drop the banner it would otherwise
        // paint for the tick between resolving and the next poll's re-classify.
        app.divergence_pending = None;
        run_divergence_choice(app, active, choice);
    }
}

/// Set/refresh the non-blocking divergence banner; returns whether a SIBLING
/// profile owns the live login. The (local) owner lookup runs only when the live
/// login actually changed — the fingerprint memo keeps the 1Hz poll to a single
/// file read in the steady state.
fn note_divergence(app: &mut App, active: &str) -> bool {
    let fingerprint = live_creds_fingerprint();
    if let Some(p) = &app.divergence_pending
        && p.active == active
        && p.fingerprint == fingerprint
    {
        return p.sibling.is_some();
    }
    let sibling = {
        let cfg = app.config();
        crate::actions::identify_live_login_owner(&cfg).filter(|owner| owner != active)
    };
    let sibling_owned = sibling.is_some();
    app.divergence_pending = Some(DivergenceNotice {
        active: active.to_string(),
        sibling,
        fingerprint,
    });
    sibling_owned
}

/// SipHash of the live access token — a cheap identity for "which login sits in
/// `~/.claude/.credentials.json` right now". It changes on every re-login and
/// every refresh, so the banner's owner lookup re-runs exactly when the creds
/// change. `None` when no readable OAuth login is present.
fn live_creds_fingerprint() -> Option<u64> {
    use std::hash::{DefaultHasher, Hash, Hasher};
    let creds = read_claude_credentials().ok().flatten()?;
    let token = creds.access_token().filter(|t| !t.is_empty())?;
    let mut hasher = DefaultHasher::new();
    token.hash(&mut hasher);
    Some(hasher.finish())
}

/// Clears `bootstrap_active` and posts `BootstrapDone` when dropped — success
/// or panic, the scheduler and UI thread are never left blocked on a crashed
/// bootstrap.
struct BootstrapDoneGuard {
    bootstrap_active: Arc<AtomicBool>,
    startup_sender: StartupSender,
}

impl Drop for BootstrapDoneGuard {
    fn drop(&mut self) {
        self.bootstrap_active.store(false, Ordering::SeqCst);
        let _ = self.startup_sender.send(StartupSignal::BootstrapDone);
    }
}

/// Handles of every [`spawn_worker`] thread, so a test can join them BEFORE its
/// `HomeSandbox` drops. `HOME_OVERRIDE` is a process-global cleared on that drop,
/// so a detached worker that outlives it resolves the operator's REAL `$HOME` and
/// takes real locks under `~/.clauth` — `RotationGuard::acquire` alone does
/// `mkdir_700` + a blocking flock. Serialized by `profile::HOME_TEST_LOCK`, which
/// every sandboxed test holds, so the registry is effectively exclusive.
/// Never compiled into the binary.
#[cfg(test)]
static TEST_WORKERS: std::sync::Mutex<Vec<std::thread::JoinHandle<()>>> =
    std::sync::Mutex::new(Vec::new());

/// Join every worker spawned so far. Any test whose action reaches
/// [`spawn_worker`] must call this while its sandbox is still alive.
#[cfg(test)]
pub(crate) fn join_test_workers() {
    let handles: Vec<_> = TEST_WORKERS
        .lock()
        .map(|mut w| std::mem::take(&mut *w))
        .unwrap_or_default();
    for handle in handles {
        let _ = handle.join();
    }
}

/// Spawn a background worker, catching panics so a single thread crash never
/// takes down the process. Callers clone only the Arcs their closure needs.
fn spawn_worker<F>(f: F)
where
    F: FnOnce() + Send + 'static,
{
    let handle = std::thread::spawn(move || {
        let _ = catch_unwind(AssertUnwindSafe(f));
    });
    #[cfg(test)]
    if let Ok(mut workers) = TEST_WORKERS.lock() {
        workers.push(handle);
    }
    #[cfg(not(test))]
    drop(handle);
}

// ── Shutdown ──────────────────────────────────────────────────────────────────

/// Snapshot active credentials and detach the symlink. After shutdown, external
/// writes to `.credentials.json` land in the standalone file, not the profile.
pub(crate) fn shutdown(app: &mut App) -> Result<()> {
    app.shutting_down.store(true, Ordering::SeqCst);
    let wait_start = Instant::now();
    while wait_start.elapsed() < Duration::from_millis(2000) {
        if !any_busy(&app.activity) {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    // Wait up to 5 s for the update check to finish; let it detach if it's
    // still running (a slow network shouldn't delay TUI exit indefinitely).
    if let Some(handle) = app.update_handle.take() {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !handle.is_finished() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        drop(handle);
    }
    {
        let mut cfg = app.config();
        let _ = snapshot_active_credentials(&mut cfg);
        let _ = save_app_state(&cfg.state);
    }
    let _ = detach_credentials_link();
    Ok(())
}

#[cfg(test)]
#[path = "../../tests/inline/tui_app.rs"]
mod tests;
