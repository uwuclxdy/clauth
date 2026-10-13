//! Application state, keymap, and tick logic.
//!
//! Layout invariants:
//!   - Overview: selectable Claude and Codex rows; `overview_cursor` maps onto
//!     the visible filter, while Claude selections sync `profile_cursor` with Usage and Setup.
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
use chrono::Weekday;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::actions::{
    CaptureSnapshot, ChainEditRefusal, ChainRefusal, EnvKeyCollision, capture_into_profile,
    capture_snapshot, classify_env_key, clear_profile_api_key, clear_profile_credentials,
    create_blank_profile, create_profile_from_login, delete_profile, duplicate_profile,
    edit_profile_endpoint, edit_profile_env, edit_profile_model, edit_profile_preset,
    find_matching_oauth_profile, overwrite_captured_profile, rename_profile, reorder_profile,
    rotation_guard_for_mutation, set_chain_order, set_member_threshold, set_wrap_off,
    snapshot_is_empty, switch_off, switch_profile, validate_foreign_harness_free,
    validate_name_chars, validate_profile_name,
};
use crate::claude::{
    LinkState, adopt_first_login, classify_credentials_link, claude_settings_env_keys,
    detach_credentials_link, force_link_profile_credentials, force_snapshot_active_credentials,
    is_first_login, link_profile_credentials, live_credentials_are_shell, read_claude_credentials,
    snapshot_active_credentials,
};
use crate::daemon::gateway::{Answerer, GatewayState};
use crate::fallback::{
    DEFAULT_THRESHOLD, MAX_THRESHOLD, MIN_THRESHOLD, SwitchAction, auto_switch_if_needed,
    parse_threshold, threshold_for,
};
use crate::format::format_threshold_tokens;
use crate::harness::Harness;
use crate::lock::with_state_lock;
use crate::lockorder::{RankedGuard, RankedMutex};
use crate::oauth;
use crate::profile::{
    AppConfig, ClockFormat, ConfigHandle, ConsoleSite, DivergenceChoice, HerdrSettings, HomeTab,
    MAX_CONTEXT_NUDGE_TOKENS, MAX_REFRESH_INTERVAL_MS, MAX_WEEKLY_SWITCH_PCT,
    MIN_CONTEXT_NUDGE_TOKENS, MIN_REFRESH_INTERVAL_MS, MIN_WEEKLY_SWITCH_PCT, ModelSettings,
    PopupWidth, Profile, ProfileName, ReloadFingerprint, ResetDisplay, ThemeName, WalkOrder,
    load_config, reload_fingerprint, save_app_state, save_profile_config,
};
use crate::profile_cache::{USAGE_CACHE_FILE, load_profile_cache, profile_cache_mtime_ms};
use crate::profile_json::{stale_after_ms, usage_cache_file};
use crate::status::{self, Incident, StatusEvent};
use crate::tui::render::prose::{self, cmd_lit};
use crate::tui::render::{ListView, follow_scroll_offset};
use crate::tui::theme;
use crate::update::{self, UpdateEvent};
use crate::usage::{
    ActivityStore, FetchLeg, FetchStatus, KickBlocks, LastFetchedAt, NextRefreshPerProfile,
    OpResult, OpResultReceiver, OpResultSender, PendingSwitch, PendingSwitchOff,
    PendingSwitchTarget, PollStreaks, ProfileActivity, RefetchQueue, StartupReceiver,
    StartupSender, StartupSignal, StatusStore, SuppressedAuthExpiredStore, ThirdPartyBroken,
    ThirdPartyList, ThirdPartyStatusStore, ThirdPartyStreaks, ThirdPartyUsageStore, TokenList,
    UsageInfo, UsageStore, any_busy, bootstrap_fetch, bootstrap_third_party, clear_activity,
    collect_oauth_seed_names, collect_third_party_entries, collect_tokens, is_idle, mark_activity,
    now_ms, spawn_refresher, switch_gate_in_flight, windows_maxed,
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

// ── Note editor ───────────────────────────────────────────────────────────────

/// The note editor's multi-line draft. The stored text is `lines` joined by
/// `\n`; the caret is `(row, col)` in char columns, converted to display cells
/// at render (a wide char takes two cells).
#[derive(Debug, Clone)]
pub(crate) struct NoteBuffer {
    lines: Vec<String>,
    row: usize,
    col: usize,
}

impl NoteBuffer {
    pub(crate) fn new(text: &str) -> Self {
        let lines: Vec<String> = if text.is_empty() {
            vec![String::new()]
        } else {
            text.split('\n').map(str::to_string).collect()
        };
        let row = lines.len() - 1;
        let col = lines[row].chars().count();
        Self { lines, row, col }
    }

    pub(crate) fn text(&self) -> String {
        self.lines.join("\n")
    }

    pub(crate) fn row_count(&self) -> usize {
        self.lines.len()
    }

    pub(crate) fn line(&self, index: usize) -> &str {
        &self.lines[index]
    }

    /// Caret row and DISPLAY column (wide chars count two cells), for the
    /// native cursor position.
    pub(crate) fn cursor_pos(&self) -> (usize, usize) {
        (self.row, display_col(&self.lines[self.row], self.col))
    }

    fn line_len(&self) -> usize {
        self.lines[self.row].chars().count()
    }

    fn clamp_col(&mut self) {
        self.col = self.col.min(self.line_len());
    }

    fn cursor_byte(&self) -> usize {
        let line = &self.lines[self.row];
        line.char_indices()
            .nth(self.col)
            .map(|(i, _)| i)
            .unwrap_or(line.len())
    }

    pub(crate) fn insert_char(&mut self, ch: char) {
        let at = self.cursor_byte();
        self.lines[self.row].insert(at, ch);
        self.col += 1;
    }

    pub(crate) fn insert_newline(&mut self) {
        let at = self.cursor_byte();
        let rest = self.lines[self.row].split_off(at);
        self.lines.insert(self.row + 1, rest);
        self.row += 1;
        self.col = 0;
    }

    pub(crate) fn backspace(&mut self) {
        if self.col > 0 {
            let prev = self.lines[self.row]
                .char_indices()
                .nth(self.col - 1)
                .map(|(i, _)| i)
                .unwrap_or(0);
            let at = self.cursor_byte();
            self.lines[self.row].replace_range(prev..at, "");
            self.col -= 1;
        } else if self.row > 0 {
            let line = self.lines.remove(self.row);
            self.row -= 1;
            self.col = self.line_len();
            self.lines[self.row].push_str(&line);
        }
    }

    pub(crate) fn delete(&mut self) {
        if self.col < self.line_len() {
            let at = self.cursor_byte();
            let line = &self.lines[self.row];
            let next = line[at..]
                .char_indices()
                .nth(1)
                .map(|(i, _)| at + i)
                .unwrap_or(line.len());
            self.lines[self.row].replace_range(at..next, "");
        } else if self.row + 1 < self.lines.len() {
            let next = self.lines.remove(self.row + 1);
            self.lines[self.row].push_str(&next);
        }
    }

    pub(crate) fn left(&mut self) {
        if self.col > 0 {
            self.col -= 1;
        } else if self.row > 0 {
            self.row -= 1;
            self.col = self.line_len();
        }
    }

    pub(crate) fn right(&mut self) {
        if self.col < self.line_len() {
            self.col += 1;
        } else if self.row + 1 < self.lines.len() {
            self.row += 1;
            self.col = 0;
        }
    }

    pub(crate) fn up(&mut self) {
        if self.row > 0 {
            self.row -= 1;
            self.clamp_col();
        }
    }

    pub(crate) fn down(&mut self) {
        if self.row + 1 < self.lines.len() {
            self.row += 1;
            self.clamp_col();
        }
    }

    pub(crate) fn home(&mut self) {
        self.col = 0;
    }

    pub(crate) fn end(&mut self) {
        self.col = self.line_len();
    }

    /// Delete the word (run of non-spaces, plus any preceding spaces) left of
    /// the caret within the line; at a line start, joins the line up.
    pub(crate) fn delete_word(&mut self) {
        if self.col == 0 {
            self.backspace();
            return;
        }
        let chars: Vec<char> = self.lines[self.row].chars().collect();
        let mut start = self.col;
        while start > 0 && chars[start - 1].is_whitespace() {
            start -= 1;
        }
        while start > 0 && !chars[start - 1].is_whitespace() {
            start -= 1;
        }
        let from_byte = chars[..start].iter().map(|c| c.len_utf8()).sum::<usize>();
        let at = self.cursor_byte();
        self.lines[self.row].replace_range(from_byte..at, "");
        self.col = start;
    }
}

/// Display width of the first `cols` chars of a line: wide (CJK/fullwidth/
/// emoji-range) chars take two cells, everything else one. The usage pane's
/// rows are ASCII-bound; the editor's caret is what must land right on the
/// lines that are not.
fn display_col(line: &str, cols: usize) -> usize {
    line.chars().take(cols).map(char_display_width).sum()
}

fn char_display_width(c: char) -> usize {
    let u = c as u32;
    if matches!(u,
        0x1100..=0x115F
            | 0x2E80..=0x303E
            | 0x3041..=0x33FF
            | 0x3400..=0x4DBF
            | 0x4E00..=0x9FFF
            | 0xA000..=0xA4CF
            | 0xAC00..=0xD7A3
            | 0xF900..=0xFAFF
            | 0xFE10..=0xFE19
            | 0xFE30..=0xFE6F
            | 0xFF00..=0xFF60
            | 0xFFE0..=0xFFE6
            | 0x1F300..=0x1F64F
            | 0x1F900..=0x1F9FF
            | 0x20000..=0x3FFFD,
    ) {
        2
    } else {
        1
    }
}

/// The usage tab's note editor state. The account is captured at open, so a
/// config reload mid-edit can never retarget the draft.
#[derive(Debug, Clone)]
pub(crate) struct NoteEditor {
    pub(crate) profile: crate::profile::ProfileName,
    pub(crate) buf: NoteBuffer,
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
    /// The weekdays this member is home (`Profile::preferred_days`): a cycle row
    /// over `PREFERRED_DAY_PRESETS` at rest, a multi-select chip row once ⏎
    /// descends into it (`CardEdit::Days`). A list matching no preset trails the
    /// run in its canonical spelling and is one more stop in the cycle while the
    /// card stays open (`App::fallback_day_stop`).
    PreferredDays,
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
    /// `Profile::bell_threshold`, labelled `alert at`: the 5h % the bell rings
    /// at. ⏎ types a value (empty turns it off), `+`/`-` step it by 5.
    BellThreshold,
    BaseUrl,
    ApiKey,
    /// Default model (CC `model` setting). Hybrid: space cycles aliases, ⏎ types a custom value.
    Model,
    /// `ANTHROPIC_DEFAULT_FABLE_MODEL` — full id, free text.
    FableModel,
    /// `ANTHROPIC_DEFAULT_OPUS_MODEL` — full id, free text.
    OpusModel,
    /// `ANTHROPIC_DEFAULT_SONNET_MODEL` — full id, free text.
    SonnetModel,
    /// `ANTHROPIC_DEFAULT_HAIKU_MODEL` — full id, free text.
    HaikuModel,
    /// `CLAUDE_CODE_SUBAGENT_MODEL` — full id, free text.
    SubagentModel,
    /// The `+ model override` reveal row. Shown only while the unset alias
    /// overrides are collapsed; ⏎ expands fable/opus/sonnet/haiku/subagent inline.
    ModelOverrideAdd,
    /// A custom `key = value` env entry, indexed into the profile's sorted env
    /// snapshot. ⏎ edits its VALUE. An emptied value REMOVES the key — the
    /// unset, never a blank string — so the entry drops from `settings.json`
    /// on the next settings apply (`commit_env_value`).
    EnvEntry(usize),
    /// The `+ add env` row — ⏎ opens a key editor that runs the collision check.
    EnvAdd,
    /// Browser OAuth login: mint fresh tokens into this account (or, on the
    /// `+ new` form, create the account from the login). Async — runs on a worker.
    Login,
    /// `+ new`-form only: stash the live login Claude Code is using now into
    /// the draft, like [`ConfigRow::Login`] stashes its mint; `create account`
    /// commits it. Renders only while the live login is real and unowned
    /// (`App::unsaved_live_login`).
    CaptureLogin,
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
    /// and are deliberately not text rows here. `BellThreshold` is a text row
    /// whose `+`/`-` at rest step it without opening the editor.
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
                | ConfigRow::BellThreshold
        )
    }
}

/// The `model` row alias cycle (space advances it). `None` renders as `default`
/// (no `model` key); a custom id set via ⏎ is outside this list.
pub(crate) const MODEL_PRESETS: [&str; 5] = ["fable", "opus", "sonnet", "haiku", "opusplan"];

/// The alias-override rows, in the order Setup lists them.
const OVERRIDE_ROWS: [ConfigRow; 5] = [
    ConfigRow::FableModel,
    ConfigRow::OpusModel,
    ConfigRow::SonnetModel,
    ConfigRow::HaikuModel,
    ConfigRow::SubagentModel,
];

/// The weekly-line preset ladder: one source for the Config row's segmented
/// control AND `step_weekly_threshold`'s cycle. 100 reproduces the old
/// hard-cap behavior (switch only once the API already refuses).
pub(crate) const WEEKLY_PRESETS: [f64; 4] = [90.0, 95.0, 98.0, 100.0];

/// `Weekday::Mon..=Sun`: the order the day picker lays out its chips and
/// writes a toggled list in.
pub(crate) const WEEKDAYS_ALL: [Weekday; 7] = [
    Weekday::Mon,
    Weekday::Tue,
    Weekday::Wed,
    Weekday::Thu,
    Weekday::Fri,
    Weekday::Sat,
    Weekday::Sun,
];

const DAYS_WEEKDAYS: [Weekday; 5] = [
    Weekday::Mon,
    Weekday::Tue,
    Weekday::Wed,
    Weekday::Thu,
    Weekday::Fri,
];
const DAYS_WEEKENDS: [Weekday; 2] = [Weekday::Sat, Weekday::Sun];

/// The `preferred days` preset ladder: one source for the row's display AND
/// `step_preferred_days`' cycle, in cycle order. Space walks it forward and
/// wraps past the last rung onto the card's remembered custom list, else back
/// to `never`; a set matching no rung steps to `never`.
pub(crate) const PREFERRED_DAY_PRESETS: [(&str, &[Weekday]); 4] = [
    ("never", &[]),
    ("weekdays", &DAYS_WEEKDAYS),
    ("weekends", &DAYS_WEEKENDS),
    ("every day", &WEEKDAYS_ALL),
];

/// The preset rung a day set matches, or `None` for a custom set. Order-blind:
/// `["sun", "sat"]` is the `weekends` rung whatever the file wrote.
pub(crate) fn preferred_days_preset(days: &[Weekday]) -> Option<usize> {
    PREFERRED_DAY_PRESETS.iter().position(|(_, preset)| {
        preset.len() == days.len() && days.iter().all(|d| preset.contains(d))
    })
}

/// The preset rung's days at `index`, or the empty set — the `never` value.
pub(crate) fn preferred_days_at(index: usize) -> Vec<Weekday> {
    PREFERRED_DAY_PRESETS
        .get(index)
        .map(|(_, days)| days.to_vec())
        .unwrap_or_default()
}

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
    /// The tab every launch opens on (`AppState.home_tab`): space/⏎
    /// cycles the eight tabs in [`Tab::ALL`] order. The first herdr launch
    /// overrides it — that one landing opens the Services tab with the herdr
    /// row's detail descended.
    HomeTab,
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
    /// Context-window nudge threshold in tokens
    /// (`AppState.context_nudge_threshold_tokens`, default off): at or past it,
    /// the context hook tells a running session once per value. Space cycles
    /// off → 300k → 400k → 600k → 900k → off; ⏎ opens the custom-value editor
    /// (50k-2M tokens, trailing `k` allowed).
    ContextNudge,
    /// Default action when CC overwrites the credentials symlink. ⏎/space cycles.
    DivergenceDefault,
    /// Keep every key of a credential store other than the login and its
    /// identity keys with the account whose store it is
    /// (`AppState.preserve_non_login_keys`) — ON by default. A `/design-login`
    /// then stays with the account it was made on across switches and relinks.
    /// Read off disk where it is used, so a flip applies to the next relink.
    /// ENUMERATED on/off, ⏎ mirrors space.
    PreserveNonLoginKeys,
    /// Opt-in burn-aware auto-switch (`AppState.burn_aware_switching`, issue #8
    /// follow-up b) — off by default, projects the ACTIVE profile's
    /// utilization ahead of the next poll instead of the static threshold.
    BurnAware,
    /// Walk-order mode (`AppState.walk_order`, issue #86): `chain` (default —
    /// today's chain-position walk) / `soonest weekly reset` / `most weekly
    /// headroom`. Space cycles.
    /// Decides WHERE each accept pass lands; `switch mode` decides WHEN the
    /// active is left, so the two stay orthogonal.
    WalkOrder,
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
    /// Background self-update (`AppState.update.auto_update`) — ON by default.
    /// The binary updater reads it at next launch, the daemon's herdr leg on
    /// its next reload, a new MCP server at startup (`check on launch`; no
    /// live process is cancelled). The row renders the persisted value and
    /// stays editable even under `CLAUTH_NO_UPDATE=1`, which still disables
    /// every leg until the env var goes. ENUMERATED on/off, ⏎ mirrors space.
    AutoUpdate,
    /// Whether the `auto_start` auto-start kick is interleaved across accounts, so
    /// their 5h windows open `5h / N` apart instead of all at once
    /// (`AppState.auto_start_queue`, default OFF — for one account the queue is
    /// a no-op, so the operator opts in). ENUMERATED on/off, ⏎ mirrors space.
    AutoStartQueue,
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
    pub(crate) bell_threshold: InputState,
    /// Value buffer for the env entry currently being edited (seeded on entry).
    /// Shared across `EnvEntry` rows since only one is active at a time.
    pub(crate) env_value: InputState,
    /// Key buffer for the `+ add env` row's key editor.
    pub(crate) env_new_key: InputState,
    /// Key of an env entry added this draft whose value has had no commit
    /// since the add. Escape on that entry's editor removes the row (cancel
    /// semantics) instead of keeping the empty placeholder; any value commit
    /// on the key clears it.
    pub(crate) env_just_added: Option<String>,
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
    /// The override rows this draft opened with a value. They stay listed for
    /// the draft's life, so emptying one never pulls the row out from under
    /// the cursor; a fresh draft tucks an unset one behind the chip again.
    pub(crate) overrides_shown: Vec<ConfigRow>,
    /// A `+ new`-form login stash, held in memory until `create account`
    /// consumes it (capture-then-commit): the browser mint `+ login` produced,
    /// or the live login `+ capture current login` snapshotted. Carries the
    /// probed account uuid alongside the credentials so the anchor is seeded
    /// under the name the create actually commits — the draft's name is still
    /// editable until then. Dropped with the draft; never set on an existing
    /// account's draft.
    pub(crate) captured_login: Option<DraftLogin>,
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
            ConfigRow::BellThreshold => &self.bell_threshold,
            ConfigRow::EnvEntry(_) if self.active == Some(row) => &self.env_value,
            ConfigRow::EnvAdd if self.active == Some(ConfigRow::EnvAdd) => &self.env_new_key,
            ConfigRow::AutoStart
            | ConfigRow::ModelOverrideAdd
            | ConfigRow::EnvEntry(_)
            | ConfigRow::EnvAdd
            | ConfigRow::Login
            | ConfigRow::CaptureLogin
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
            ConfigRow::BellThreshold => &mut self.bell_threshold,
            ConfigRow::EnvEntry(_) if self.active == Some(row) => &mut self.env_value,
            ConfigRow::EnvAdd if self.active == Some(ConfigRow::EnvAdd) => &mut self.env_new_key,
            ConfigRow::AutoStart
            | ConfigRow::ModelOverrideAdd
            | ConfigRow::EnvEntry(_)
            | ConfigRow::EnvAdd
            | ConfigRow::Login
            | ConfigRow::CaptureLogin
            | ConfigRow::DeleteCreds
            | ConfigRow::ClearSessionToken
            | ConfigRow::Disabled
            | ConfigRow::Delete
            | ConfigRow::Create => return None,
        })
    }
}

/// What a `+ new` draft's stash holds: the browser mint `+ login` finished
/// with, or the live login `+ capture current login` snapshotted off
/// `~/.claude/.credentials.json`. One slot — the second stash source replaces
/// the first, gated on a confirm.
#[derive(Debug, Clone)]
pub(crate) enum DraftLogin {
    Mint(Box<crate::oauth_login::LoginOutcome>),
    LiveLogin(Box<CaptureSnapshot>),
}

#[derive(Debug, Clone)]
pub(crate) struct ConfirmState {
    pub(crate) message: String,
    pub(crate) detail: Option<String>,
    /// Highlighted choice: false = cancel, true = confirm.
    pub(crate) choice: bool,
    /// Body scroll state for an overflowing confirm body: `↑`/`↓` scroll one
    /// line, `PageUp`/`PageDown` one page, while the buttons stay pinned. The
    /// render publishes the bound and the viewport; the key handler clamps.
    pub(crate) scroll: ConfirmScroll,
    pub(crate) on_confirm: ConfirmAction,
}

/// The scroll a confirm modal's body carries. `offset` is the key handler's
/// own state; `max_scroll` and `viewport` are what the last render drew, so a
/// resize changes the page step on the next press. `Default` keeps the many
/// one-line confirms at the top of their (fitting) bodies.
#[derive(Debug, Clone, Default)]
pub(crate) struct ConfirmScroll {
    pub(crate) offset: u16,
    pub(crate) max_scroll: std::cell::Cell<u16>,
    pub(crate) viewport: std::cell::Cell<u16>,
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
    /// Services tab: write the `mcpServers.clauth` entry into `~/.claude.json`.
    /// Reversible local write — non-destructive, so it keeps the plain button.
    WireMcpServers,
    /// Setup tab: drop a profile's stored OAuth credentials, keeping the shell.
    BlankCredentials(String),
    /// Setup `+ new` draft: a login already stashed a mint (the `✓ logged in`
    /// row), so re-running would silently replace it. Confirm first, then
    /// re-dispatch `start_login`. `bool` = `is_new`, carried to the restart.
    RestartLogin(String, bool),
    /// Setup `+ new` draft: `+ capture current login` pressed while a browser
    /// mint is already stashed. Confirm, then stash the snapshot in its place.
    CaptureOverMintStash(Box<CaptureSnapshot>),
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
    /// Services tab: run `crate::herdr::heal` on the named config file.
    HealHerdrConfig(std::path::PathBuf),
    /// Services tab herdr options: flip the `delegate row text` knob, persist it,
    /// then heal herdr's config so the sidebar row matches the new knob. The
    /// heal is what the confirm gates — it rewrites herdr's own file.
    HerdrDelegateRowText(std::path::PathBuf),
    /// Services tab: install the clauth plugin through agentgear at user scope.
    /// A write into CC's plugin registry (driven via the `claude` CLI), so it
    /// keeps the confirm modal like every other mutating fix.
    InstallPlugin,
    /// Services tab: adopt a found standalone shunt config (TOML only), running
    /// the probe and store plan off the UI thread.
    AdoptConfig(std::path::PathBuf),
    /// Services tab: move a stopped standalone's account stores in. The plan
    /// the confirm counted rides here, so the move runs exactly what the card
    /// showed (never the cache re-read at yes); the move re-plans and refuses
    /// `PlanChanged` on any drift.
    MoveStoresIn(crate::gateway::StoreMovePlan),
    /// Services tab: add clauth's admin write key to the adopted config's
    /// existing `[server.admin]` table.
    AddAdminKey,
    /// Services tab: add a `[server.admin]` table carrying clauth's key (the
    /// gateway restarts once to register its admin routes).
    AddAdminTable,
    /// The shared daemon-start confirm's `start`: spawn `<this binary> daemon`
    /// detached.
    StartDaemon,
    /// `stop shunt?`'s `stop`: the transient hold, if the verb still applies.
    StopShunt,
    /// `restart shunt?`'s `restart`: save a bind edit whose restart the confirm
    /// gates (only while the gateway is supervised and running).
    RestartBind {
        key: ShuntFieldKey,
        edit: Box<crate::gateway_edit::Edit>,
    },
    /// `rename upstream|model …?`'s `rename`: save a rename whose cascade the
    /// confirm counted.
    RenameField {
        key: ShuntFieldKey,
        edit: Box<crate::gateway_edit::Edit>,
    },
    /// `remove upstream|model|route …?`'s `remove`: run the entry's remove
    /// through the save worker. The edit is built when the confirm OPENS (the
    /// entry as it reads then, a route's `RouteView` included), so confirming
    /// runs that edit — never a lookup by position at confirm time. A route the
    /// shift moved under it then refuses through the engine's `EntryChanged`.
    RemoveEntry {
        key: ShuntEntryKey,
        edit: Box<crate::gateway_edit::Edit>,
    },
    /// `remove <profile>'s client token?`'s `remove`: revoke the profile's
    /// client token through the save worker. `record` is the adopted gateway
    /// read when the confirm opened.
    RemoveToken {
        record: crate::gateway::GatewayRecord,
        profile: String,
    },
    /// `point <profile> at the shunt gateway?`'s `overwrite`: mint the profile's
    /// client token, then point the profile at the gateway (`url`). `replace`
    /// is the confirm's third body line (`None` = nothing to replace), built
    /// when the confirm opened.
    AddToken {
        record: crate::gateway::GatewayRecord,
        profile: String,
        url: String,
        replace: Option<String>,
    },
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
    pub(crate) scroll: std::cell::Cell<usize>,
    pub(crate) viewport: std::cell::Cell<usize>,
    pub(crate) starts: std::cell::RefCell<Vec<usize>>,
    pub(crate) block_ends: std::cell::RefCell<Vec<usize>>,
    pub(crate) total: std::cell::Cell<usize>,
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
    pub(crate) scroll: std::cell::Cell<usize>,
    pub(crate) viewport: std::cell::Cell<usize>,
    pub(crate) starts: std::cell::RefCell<Vec<usize>>,
    pub(crate) block_ends: std::cell::RefCell<Vec<usize>>,
    pub(crate) total: std::cell::Cell<usize>,
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
    /// fragments, mid-dot separators.
    pub(crate) fn banner_message(&self) -> String {
        match &self.sibling {
            Some(owner) => format!(
                "live login is '{owner}' · not the active '{}' · press {} to resolve",
                self.active,
                prose::key("d")
            ),
            None => format!(
                "live login no longer matches '{}' · press {} to resolve",
                self.active,
                prose::key("d")
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
    pub(crate) scroll: std::cell::Cell<usize>,
    pub(crate) viewport: std::cell::Cell<usize>,
    pub(crate) starts: std::cell::RefCell<Vec<usize>>,
    pub(crate) block_ends: std::cell::RefCell<Vec<usize>>,
    pub(crate) total: std::cell::Cell<usize>,
}

/// The `+ add token` profile picker: one row per claude profile, in roster
/// order, each carrying its `(name, has_token)` — whether the profile already
/// holds a token, so the row reads `alice  has a token`. C1 has no synthetic
/// `+ new profile` row, so the cursor is the profile's own index
/// (`0..targets.len()`); C2 adds `+ new profile` as row 0 and shifts every
/// profile down one.
#[derive(Debug, Clone)]
pub(crate) struct TokenPickerForm {
    pub(crate) targets: Vec<(String, bool)>,
    pub(crate) cursor: usize,
    pub(crate) scroll: std::cell::Cell<usize>,
    pub(crate) viewport: std::cell::Cell<usize>,
    pub(crate) starts: std::cell::RefCell<Vec<usize>>,
    pub(crate) block_ends: std::cell::RefCell<Vec<usize>>,
    pub(crate) total: std::cell::Cell<usize>,
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
    pub(crate) scroll: std::cell::Cell<usize>,
    pub(crate) reviewing_overwrite: bool,
    pub(crate) confirm_overwrite: bool,
    pub(crate) max_scroll: std::cell::Cell<u16>,
    pub(crate) viewport: std::cell::Cell<usize>,
    pub(crate) overwrite_needs_review: std::cell::Cell<bool>,
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
    /// `clauth daemon`, detached from the TUI, offered while none runs.
    StartDaemon,
    /// `--replace`'s termination with no successor, offered while one runs.
    StopDaemon,
    /// The `start shunt` verb; the effect re-derives from the gateway slot at
    /// dispatch (jump to the card, the daemon-start confirm, or a hold release).
    StartShunt,
    /// The `stop shunt` verb: a transient hold while a fresh daemon runs the
    /// gateway.
    StopShunt,
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

/// What a `start daemon` / `stop daemon` worker reports, once.
#[derive(Debug)]
pub(crate) enum DaemonControlResult {
    Start(std::result::Result<crate::daemon::StartOutcome, String>),
    Stop(std::result::Result<crate::daemon::DaemonStop, String>),
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
    pub(crate) scroll: std::cell::Cell<usize>,
    /// Visible menu rows from the last render (the page step).
    pub(crate) viewport: std::cell::Cell<usize>,
    /// Chunked row starts/ends per item and the total row count, published by
    /// the render so the page step accounts for the group-rule line.
    pub(crate) starts: std::cell::RefCell<Vec<usize>>,
    pub(crate) block_ends: std::cell::RefCell<Vec<usize>>,
    pub(crate) total: std::cell::Cell<usize>,
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
            scroll: std::cell::Cell::new(0),
            viewport: std::cell::Cell::new(0),
            starts: std::cell::RefCell::new(Vec::new()),
            block_ends: std::cell::RefCell::new(Vec::new()),
            total: std::cell::Cell::new(0),
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
            Self::StartDaemon => "start daemon",
            Self::StopDaemon => "stop daemon",
            Self::StartShunt => "start shunt",
            Self::StopShunt => "stop shunt",
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
    /// `+ add token`: pick which claude profile gets a client token.
    TokenPicker(TokenPickerForm),
    Help,
    /// Context-sensitive action menu opened by `a`.
    ActionMenu(ActionMenuState),
    /// Custom env key collides with an existing source; overwrite/keep/cancel.
    EnvCollision(EnvCollisionForm),
    /// In-flight login progress; renders live from [`App::login`], the inline
    /// code field ([`LoginSession::paste_field`]) included. esc/q collapse it
    /// to the footer indicator — the login keeps running.
    Login,
    /// Read-only scrollable text modal for the shunt card's `check output`:
    /// the masked stderr of the last failed check.
    CheckOutput(String),
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
const TOAST_TTL_NORMAL: Duration = Duration::from_secs(5);
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
    /// Services: the shunt gateway, delegates, the Claude Code plugin and herdr.
    Services,
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
        Tab::Services,
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
            Tab::Services => "Services",
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

impl From<Tab> for HomeTab {
    fn from(tab: Tab) -> Self {
        match tab {
            Tab::Overview => HomeTab::Overview,
            Tab::Usage => HomeTab::Usage,
            Tab::Tokens => HomeTab::Tokens,
            Tab::Setup => HomeTab::Setup,
            Tab::Fallback => HomeTab::Fallback,
            Tab::Config => HomeTab::Config,
            Tab::Status => HomeTab::Status,
            Tab::Services => HomeTab::Services,
        }
    }
}

impl From<HomeTab> for Tab {
    fn from(tab: HomeTab) -> Self {
        match tab {
            HomeTab::Overview => Tab::Overview,
            HomeTab::Usage => Tab::Usage,
            HomeTab::Tokens => Tab::Tokens,
            HomeTab::Setup => Tab::Setup,
            HomeTab::Fallback => Tab::Fallback,
            HomeTab::Config => Tab::Config,
            HomeTab::Status => Tab::Status,
            HomeTab::Services => Tab::Services,
        }
    }
}

/// Which view the Tokens tab shows. `Dashboard` is the landing page (totals +
/// charts); `Models` is the descend-into per-model master-detail breakdown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TokenView {
    Dashboard,
    Models,
}

/// One card of the Tokens dashboard. Focus is stored as this (not an index)
/// so it keeps meaning across a resize that changes the layout's reading order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CardKind {
    First,
    Total,
    Trend,
    Models,
    Comp,
    Hour,
    Activity,
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
    pub(crate) selector: ListView,
    /// Scroll offset (lines) into the detail timeline.
    pub(crate) detail_scroll: u16,
    /// Max valid `detail_scroll` from the last detail render (`total - viewport`).
    /// The render pass owns the real geometry, so it writes the bound here (via
    /// `&App` interior mutability) and the key handler clamps against it — that
    /// stops a held ↓ from inflating `detail_scroll` toward `u16::MAX`.
    pub(crate) detail_max_scroll: std::cell::Cell<u16>,
    /// Visible detail-timeline rows from the last render (the page step).
    pub(crate) detail_viewport: std::cell::Cell<u16>,
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
            selector: ListView::default(),
            detail_scroll: 0,
            detail_max_scroll: std::cell::Cell::new(0),
            detail_viewport: std::cell::Cell::new(0),
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

// ── Services tab ───────────────────────────────────────────────────────────────

/// Which Services pane has focus. `List`: the row selector (↑↓ moves, ⏎
/// descends, `f` descends onto the row's first fix line). `Detail`: the
/// selected row's readout (↑↓ scrolls, or walks the plugin problems / herdr
/// options).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ServicesFocus {
    List,
    Detail,
}

/// Health bucket for a row's status dot — the same success / warning / danger
/// buckets as the header `● status.claude.ai` dot, plus a neutral `Idle` for a
/// row that is neither running nor healthy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Health {
    Ok,
    Warn,
    Danger,
    Idle,
}

/// A fix offered on a service row. `WireMcpServers` writes the manual
/// entry (a [`ConfirmAction`]); the verb the footer's `↵` hint and the
/// `f  <verb>` line under the problem render is [`fix_verb`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ServiceFix {
    WireMcpServers,
    /// Append the keybinding + sidebar row to herdr's config (the config half of
    /// `clauth herdr install`). `PathBuf` = the resolved config file.
    HealHerdrConfig(std::path::PathBuf),
    /// Install the clauth plugin through agentgear (user scope, embedded tree).
    InstallPlugin,
    /// Adopt a found standalone shunt config. `PathBuf` = the discovered TOML.
    AdoptConfig(std::path::PathBuf),
    /// Move a stopped standalone's account stores into clauth's managed stores.
    MoveStoresIn,
    /// Add clauth's admin write key to the adopted config's `[server.admin]`.
    AddAdminKey,
    /// Add a `[server.admin]` table carrying clauth's key.
    AddAdminTable,
}

/// One focusable line inside a service row's detail. When the detail pane is
/// descended, ↑↓ and `f` walk these problems and `↵` opens the focused one's
/// confirm; the footer's `↵ <verb>` follows the focus.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Problem {
    /// The detail-line index of this problem's `f  <verb>` line.
    pub(crate) line: usize,
    pub(crate) fix: ServiceFix,
}

/// One actionable line on the `shunt` card: a fix (`f  <verb>`), the `enabled`
/// toggle row, the `check output` line, or a config-section stop (an entry row
/// whose `↵`/`space` toggles it expanded, or a field row whose `↵`/`space` opens
/// it and `space` cycles a cycle row). Descended, ↑↓ pages the card one line at
/// a time; an actionable line takes focus as it scrolls into view, read-only
/// rows never do, and `space`/`↵` act only on the focused one (`↵` on a fix
/// line opens its confirm; `f` walks between fix lines).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ShuntFocus {
    /// An `f  <verb>` line at this detail index.
    Fix { line: usize, fix: ServiceFix },
    /// The `enabled` toggle row at this detail index.
    Enabled { line: usize },
    /// The `check output` line at this detail index (a held failed-check
    /// stderr; ↵/space opens its modal).
    CheckOutput { line: usize },
    /// A config-section entry row (an upstream, model or route), whose `↵`/
    /// `space` toggles it expanded. Not a `Check::detail` line: the render
    /// draws the config section after the detail lines.
    Entry { key: ShuntEntryKey },
    /// A config-section field row (an editable `key value` line, or a cycle
    /// row), whose `↵` opens it and `space` cycles a cycle row.
    Field { key: ShuntFieldKey },
    /// A `+ add <kind>` action row ending its group, whose `↵`/`space` opens
    /// (or folds) that kind's blank draft in place.
    Add { kind: ShuntAddKind },
    /// The `save <kind>` row of an open draft, whose `↵` runs the add.
    SaveAdd { kind: ShuntAddKind },
    /// The `remove <kind>` row ending an expanded entry, whose `↵` opens the
    /// remove confirm.
    Remove { key: ShuntEntryKey },
    /// A client-token action-chip row, whose `↵` opens the remove confirm.
    Token { key: String },
    /// The `+ add token` row ending the client-token group, whose `↵`/`space`
    /// opens the profile picker.
    AddToken,
}

impl ShuntFocus {
    /// The detail-line index the focusable sits on; `None` on a config row,
    /// which has no `Check::detail` line.
    pub(crate) fn line(&self) -> Option<usize> {
        match self {
            ShuntFocus::Fix { line, .. }
            | ShuntFocus::Enabled { line }
            | ShuntFocus::CheckOutput { line } => Some(*line),
            ShuntFocus::Entry { .. }
            | ShuntFocus::Field { .. }
            | ShuntFocus::Add { .. }
            | ShuntFocus::SaveAdd { .. }
            | ShuntFocus::Remove { .. }
            | ShuntFocus::Token { .. }
            | ShuntFocus::AddToken => None,
        }
    }

    /// The fix this line applies, `None` on every config-section row.
    pub(crate) fn fix(&self) -> Option<&ServiceFix> {
        match self {
            ShuntFocus::Fix { fix, .. } => Some(fix),
            ShuntFocus::Enabled { .. }
            | ShuntFocus::CheckOutput { .. }
            | ShuntFocus::Entry { .. }
            | ShuntFocus::Field { .. }
            | ShuntFocus::Add { .. }
            | ShuntFocus::SaveAdd { .. }
            | ShuntFocus::Remove { .. }
            | ShuntFocus::Token { .. }
            | ShuntFocus::AddToken => None,
        }
    }

    /// The line's kind, the identity focus keeps across a resize or recompute.
    pub(crate) fn kind(&self) -> ShuntFocusLine {
        match self {
            ShuntFocus::Enabled { .. } => ShuntFocusLine::Enabled,
            ShuntFocus::CheckOutput { .. } => ShuntFocusLine::CheckOutput,
            ShuntFocus::Entry { key } => ShuntFocusLine::Entry(key.clone()),
            ShuntFocus::Field { key } => ShuntFocusLine::Field(key.clone()),
            ShuntFocus::Add { kind } => ShuntFocusLine::Add(*kind),
            ShuntFocus::SaveAdd { kind } => ShuntFocusLine::SaveAdd(*kind),
            ShuntFocus::Remove { key } => ShuntFocusLine::Remove(key.clone()),
            ShuntFocus::Token { key } => ShuntFocusLine::Token(key.clone()),
            ShuntFocus::AddToken => ShuntFocusLine::AddToken,
            ShuntFocus::Fix {
                fix: ServiceFix::AdoptConfig(_),
                ..
            } => ShuntFocusLine::Adopt,
            ShuntFocus::Fix {
                fix: ServiceFix::MoveStoresIn,
                ..
            } => ShuntFocusLine::Move,
            ShuntFocus::Fix {
                fix: ServiceFix::AddAdminKey | ServiceFix::AddAdminTable,
                ..
            } => ShuntFocusLine::Admin,
            // No other fix line appears on the shunt card.
            ShuntFocus::Fix { .. } => unreachable!("no other fix appears on the shunt card"),
        }
    }
}

/// The identity of the shunt card's focused actionable line, by its kind. The
/// key handler owns it; the render reads it and re-settles it (the descend
/// rule) when a resize or a card change pushed the held line out of view. A
/// resize or a card change keeps focus on the same line while that kind still
/// exists, so the line index ([`ShuntFocus::line`]) is deliberately not part
/// of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ShuntFocusLine {
    /// The `enabled` toggle row.
    Enabled,
    /// The `f  adopt config` line on the absent card.
    Adopt,
    /// The `f  add admin key` / `f  add admin table` line.
    Admin,
    /// The `f  move stores in` line.
    Move,
    /// The `check output` line (a held failed-check stderr).
    CheckOutput,
    /// A config-section entry row, by its identity.
    Entry(ShuntEntryKey),
    /// A config-section field row, by its `(entry, field)` identity.
    Field(ShuntFieldKey),
    /// A `+ add <kind>` row, by its kind.
    Add(ShuntAddKind),
    /// The `save <kind>` row of an open draft, by its kind.
    SaveAdd(ShuntAddKind),
    /// A `remove <kind>` row, by its entry's identity.
    Remove(ShuntEntryKey),
    /// A client-token row, by the profile name.
    Token(String),
    /// The `+ add token` row.
    AddToken,
}

/// A config-section entry's identity: the group (the variant) plus the
/// name-or-id and its occurrence index among entries of that kind sharing that
/// name-or-id (shunt keeps duplicate upstream names and duplicate map-less
/// model ids valid, so the name alone would trap the walk between two of
/// them), or the route's 1-based position. This is what focus keeps across a
/// recompute or resize, and what the expanded set is keyed by — never a line
/// index, which a reorder would land on another entry.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum ShuntEntryKey {
    Upstream {
        name: String,
        occurrence: usize,
    },
    /// `(id, occurrence index)` among models sharing that id.
    Model {
        id: String,
        occurrence: usize,
    },
    Route(usize),
}

/// A config field's identity on the shunt card: the entry it belongs to (a
/// server field is keyed on its own, the variant alone) plus the field within
/// it. Field rows are keyed by `(entry key, field)` — never a line index — so
/// focus follows the edited row across a recompute: a rename follows the new
/// name and the expanded set's key moves with it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum ShuntFieldKey {
    Bind,
    DefaultProvider,
    Upstream {
        name: String,
        occurrence: usize,
        field: UpstreamFieldKind,
    },
    Model {
        id: String,
        occurrence: usize,
        field: ModelFieldKind,
    },
    Route {
        position: usize,
        field: RouteFieldKind,
    },
    /// A field of an open add form's draft (keyed by the kind and its field).
    Draft {
        kind: ShuntAddKind,
        field: ShuntDraftField,
    },
}

/// A field of an expanded upstream row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum UpstreamFieldKind {
    Name,
    Provider,
    Kind,
    BaseUrl,
    AuthMode,
    AuthAccount,
    Accounts,
    AuthEnv,
    AuthHeader,
    Effort,
    ServiceTier,
}

/// A field of an expanded model row.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum ModelFieldKind {
    Id,
    DisplayName,
    /// One `upstream_model` entry, keyed by its provider.
    UpstreamModel(String),
}

/// A field of an expanded route row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum RouteFieldKind {
    Model,
    Provider,
    UpstreamModel,
    Effort,
    ServiceTier,
}

/// The kind of a `+ add` form on the shunt card's config section (and its
/// `save <kind>` row): one of the three editable groups.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum ShuntAddKind {
    Upstream,
    Model,
    Route,
}

impl ShuntAddKind {
    /// The kind word every approved copy spells: `upstream` / `model` / `route`.
    pub(crate) fn word(self) -> &'static str {
        match self {
            ShuntAddKind::Upstream => "upstream",
            ShuntAddKind::Model => "model",
            ShuntAddKind::Route => "route",
        }
    }

    /// The `+ add <kind>` row's label.
    pub(crate) fn add_label(self) -> String {
        format!("+ add {}", self.word())
    }

    /// The `save <kind>` row's label.
    pub(crate) fn save_label(self) -> String {
        format!("save {}", self.word())
    }

    /// The `remove <kind>` row's label.
    pub(crate) fn remove_label(self) -> String {
        format!("remove {}", self.word())
    }

    /// The config-section group index of this kind, in the card's order
    /// (SERVER, UPSTREAMS, MODELS, ROUTES).
    pub(crate) fn group(self) -> usize {
        match self {
            ShuntAddKind::Upstream => 1,
            ShuntAddKind::Model => 2,
            ShuntAddKind::Route => 3,
        }
    }
}

/// A field of an open add form's draft, keyed by the kind and the field
/// within it. It reuses the entry field-kind enums: the upstream draft's
/// fields are [`UpstreamFieldKind`]'s, the model draft's [`ModelFieldKind`]'s,
/// the route draft's [`RouteFieldKind`]'s.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum ShuntDraftField {
    Upstream(UpstreamFieldKind),
    Model(ModelFieldKind),
    Route(RouteFieldKind),
}

/// The open add form's committed draft for one upstream: every value is the
/// typed (or picked) value, `None` = unset (renders `—` and is left out of
/// the add). A draft commit lands here, in memory, never in the file; only
/// `save upstream` writes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct UpstreamDraft {
    pub(crate) name: Option<String>,
    pub(crate) provider: Option<String>,
    pub(crate) kind: Option<String>,
    pub(crate) base_url: Option<String>,
    pub(crate) auth_mode: Option<String>,
    pub(crate) account: Option<String>,
    /// The comma-separated account names, exactly as the accounts row parses
    /// them.
    pub(crate) accounts: Option<String>,
    pub(crate) env: Option<String>,
    pub(crate) header: Option<String>,
    pub(crate) effort: Option<String>,
    pub(crate) service_tier: Option<String>,
    /// A refusal rendered on the `└` line under `save upstream` (a required
    /// field left blank, or the engine's own sentence).
    pub(crate) refusal: Option<String>,
}

/// The open add form's committed draft for one model.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ModelDraft {
    pub(crate) id: Option<String>,
    pub(crate) display_name: Option<String>,
    /// Provider name → model slug; `None` = blank (left out of the add). The
    /// rows derive from the config's current provider list; a committed value
    /// is looked up by provider name here.
    pub(crate) upstream_model: Vec<(String, Option<String>)>,
    /// A refusal rendered on the `└` line under `save model`.
    pub(crate) refusal: Option<String>,
}

/// The open add form's committed draft for one route.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct RouteDraft {
    pub(crate) model: Option<String>,
    pub(crate) provider: Option<String>,
    pub(crate) upstream_model: Option<String>,
    pub(crate) effort: Option<String>,
    pub(crate) service_tier: Option<String>,
    /// A refusal rendered on the `└` line under `save route`.
    pub(crate) refusal: Option<String>,
}

/// The open add forms: at most one draft per kind, each surviving the card's
/// recompute and any other row's save.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ShuntDrafts {
    pub(crate) upstream: Option<UpstreamDraft>,
    pub(crate) model: Option<ModelDraft>,
    pub(crate) route: Option<RouteDraft>,
}

impl ShuntDrafts {
    /// Whether the `+ add <kind>` draft is open.
    pub(crate) fn is_open(&self, kind: ShuntAddKind) -> bool {
        match kind {
            ShuntAddKind::Upstream => self.upstream.is_some(),
            ShuntAddKind::Model => self.model.is_some(),
            ShuntAddKind::Route => self.route.is_some(),
        }
    }

    /// Open (or reseed) the `+ add <kind>` draft in place, dropping any
    /// committed values and refusal from a previous open of the same kind. A
    /// model draft's `upstream_model` rows derive from the config's current
    /// provider list at render and save time, so opening it seeds nothing.
    pub(crate) fn open(&mut self, kind: ShuntAddKind) {
        match kind {
            ShuntAddKind::Upstream => self.upstream = Some(UpstreamDraft::default()),
            ShuntAddKind::Model => self.model = Some(ModelDraft::default()),
            ShuntAddKind::Route => self.route = Some(RouteDraft::default()),
        }
    }

    /// Fold (close) the `+ add <kind>` draft, dropping it and its refusal.
    pub(crate) fn fold(&mut self, kind: ShuntAddKind) {
        match kind {
            ShuntAddKind::Upstream => self.upstream = None,
            ShuntAddKind::Model => self.model = None,
            ShuntAddKind::Route => self.route = None,
        }
    }

    /// The refusal under the `save <kind>` row, if one shows.
    pub(crate) fn refusal(&self, kind: ShuntAddKind) -> Option<&str> {
        match kind {
            ShuntAddKind::Upstream => self.upstream.as_ref().and_then(|d| d.refusal.as_deref()),
            ShuntAddKind::Model => self.model.as_ref().and_then(|d| d.refusal.as_deref()),
            ShuntAddKind::Route => self.route.as_ref().and_then(|d| d.refusal.as_deref()),
        }
    }

    /// Set the refusal under the `save <kind>` row.
    pub(crate) fn set_refusal(&mut self, kind: ShuntAddKind, sentence: String) {
        match kind {
            ShuntAddKind::Upstream => {
                if let Some(draft) = &mut self.upstream {
                    draft.refusal = Some(sentence);
                }
            }
            ShuntAddKind::Model => {
                if let Some(draft) = &mut self.model {
                    draft.refusal = Some(sentence);
                }
            }
            ShuntAddKind::Route => {
                if let Some(draft) = &mut self.route {
                    draft.refusal = Some(sentence);
                }
            }
        }
    }

    /// Clear the refusal under the `save <kind>` row of `kind`.
    pub(crate) fn clear_refusal(&mut self, kind: ShuntAddKind) {
        match kind {
            ShuntAddKind::Upstream => {
                if let Some(draft) = &mut self.upstream {
                    draft.refusal = None;
                }
            }
            ShuntAddKind::Model => {
                if let Some(draft) = &mut self.model {
                    draft.refusal = None;
                }
            }
            ShuntAddKind::Route => {
                if let Some(draft) = &mut self.route {
                    draft.refusal = None;
                }
            }
        }
    }
}

/// The shunt card's open field editor (or a cycle row's pending pick). `None`
/// at rest, when a field row shows `❯` + its value.
#[derive(Debug, Clone)]
pub(crate) enum ShuntFieldEdit {
    /// A typed row's open editor — a typed row's `↵`, or a cycle row's `↵` at
    /// rest, which opens the custom-value editor. `input` holds the typed text;
    /// `saved` the value the editor opened on (`None` = unset), so `esc`
    /// restores it exactly.
    Typed {
        input: InputState,
        saved: Option<String>,
    },
    /// A cycle row's pending pick after `space` moved it: `picked` is the
    /// unsaved pick (the row shows `✎`, nothing written), `saved` the file's
    /// value. `↵` saves the pick, `esc` restores `saved`.
    Pick {
        saved: Option<String>,
        picked: String,
    },
}

/// The row a config save is keyed by: a field row, a `save <kind>` row, or a
/// `remove <kind>` row. The one [`ServicesState::saving`] predicate reads it,
/// so the spinner, the lock and every footer hint can never disagree about
/// which row is busy.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum ShuntSaveKey {
    Field(ShuntFieldKey),
    SaveAdd(ShuntAddKind),
    Remove(ShuntEntryKey),
    /// A client-token row's remove, keyed by the profile name.
    Token(String),
    /// The `+ add token` row's add.
    AddToken,
}

/// The row whose caret slot shows the `SUCCESS` `✓` after a successful save:
/// a field row, or an entry row (an add's new entry).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum ShuntOkKey {
    Field(ShuntFieldKey),
    Entry(ShuntEntryKey),
    /// A client-token row, keyed by the profile name (an add's new row).
    Token(String),
}

/// One config save in flight: the row's key (so the render spinner/shimmer the
/// right row) and when it started (the `✓` holds for about a second).
#[derive(Debug, Clone)]
pub(crate) struct ShuntSave {
    pub(crate) key: ShuntSaveKey,
    pub(crate) started: std::time::Instant,
    /// The value being saved (`None` = unset), which the saving row renders
    /// under the shimmer crest (the editor stays open with the typed text, but
    /// the row shows the value the save checks).
    pub(crate) value: Option<String>,
    /// The entry a `save <kind>` lands under, fixed when the save STARTS from
    /// the `Edit` being saved (the new name/id, or the route's position in the
    /// pre-save view). The outcome drain reads this, never the draft or the
    /// live view, so a fold or a recompute mid-check cannot move the `✓`.
    pub(crate) entry: Option<ShuntEntryKey>,
}

/// Where an unmanaged client-token value came from, for the card's dim source
/// word trailing an unmanaged token row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ShuntTokenSource {
    EnvFile,
    DaemonEnv,
}

impl ShuntTokenSource {
    /// The dim source word: `env file` / `daemon env`.
    pub(crate) fn word(self) -> &'static str {
        match self {
            ShuntTokenSource::EnvFile => "env file",
            ShuntTokenSource::DaemonEnv => "daemon env",
        }
    }
}

/// The client-token section's data, read beside the config view on the
/// recompute and cached on the card state: the stored tokens in name order,
/// the unmanaged client names and where they came from, the variable when the
/// unmanaged value does not parse, the store read failure's sentence (`error`,
/// the group then renders `tokens  not read` + the sentence), and the unmanaged
/// (client-auth) read failure's sentence (`unmanaged_error`, the stored rows
/// stay and the group renders `other clients  not read` + the sentence). Each
/// keeps `+ add token`.
#[derive(Debug, Clone, Default)]
pub(crate) struct ShuntTokens {
    pub(crate) stored: Vec<String>,
    pub(crate) unmanaged: Vec<(String, ShuntTokenSource)>,
    pub(crate) unparsed: Option<(String, ShuntTokenSource)>,
    pub(crate) error: Option<String>,
    pub(crate) unmanaged_error: Option<String>,
}

/// The shunt card's open field editor: which field, its edit state, and a
/// refusal's own sentence (rendered on the `└` DANGER line under the field,
/// the editor kept open with the typed text). `None` at rest.
#[derive(Debug, Clone)]
pub(crate) struct ShuntFieldEditState {
    pub(crate) key: ShuntFieldKey,
    pub(crate) edit: ShuntFieldEdit,
    pub(crate) refusal: Option<String>,
}

/// A remove's `EditRefusal`, rendered on the `└` line under the entry's
/// `remove <kind>` row while the entry is still expanded. It retires when
/// focus leaves the row, when the entry collapses, or when a recompute reads a
/// view that differs from the one it was raised on (a hand-edit).
#[derive(Debug, Clone)]
pub(crate) struct ShuntRemoveRefusal {
    pub(crate) key: ShuntEntryKey,
    pub(crate) sentence: String,
    /// The config view the refusal was raised on, so a recompute whose view
    /// differs retires it.
    pub(crate) view: Box<crate::gateway_edit::ConfigView>,
}

/// How long a field's caret slot shows the `SUCCESS` `✓` after its save lands.
pub(crate) const SHUNT_OK_MS: u64 = 1000;

/// Which edit's refused `shunt check` the card's `check output` line holds:
/// clauth's admin entry turning up present answers an admin edit's refusal,
/// never a config save's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CheckOutputSource {
    AdminEdit,
    ConfigSave,
}

/// A refused `shunt check`'s stderr held for the `check output` line, with the
/// kind of edit it refused.
#[derive(Debug, Clone)]
pub(crate) struct HeldCheckOutput {
    pub(crate) source: CheckOutputSource,
    pub(crate) stderr: crate::gateway::CheckStderr,
}

/// The adopted config read through [`crate::gateway_edit::read_view`] on the
/// recompute. The render draws the card's config section from it.
#[derive(Debug, Clone)]
pub(crate) enum ShuntConfigRead {
    Ok(Box<crate::gateway_edit::ConfigView>),
    /// The config's own read/parse failed with one of the card's reason words.
    Unread(UnreadConfig),
    /// The config reads but the editor's view refuses its shape: the card
    /// renders `config  not editable` and the refusal's own sentence.
    Refused(String),
}

/// A computed service row. `fix` is the row's first fix line's fix (its
/// presence marks the row in the selector and gates `f`); `problems` is the
/// detail-pane focus walk (empty on rows whose detail has no per-problem
/// focus); `shunt_focus` is the `shunt` card's own focus walk (a mix of fix
/// lines and the `enabled` row).
#[derive(Debug, Clone)]
pub(crate) struct Check {
    pub(crate) label: &'static str,
    pub(crate) health: Health,
    /// Full readout for the detail pane, one entry per line. (Rows are
    /// dot-only in the list — the dot color carries the verdict, the readout
    /// lives here — so there is no separate terse value.)
    pub(crate) detail: Vec<String>,
    pub(crate) fix: Option<ServiceFix>,
    pub(crate) problems: Vec<Problem>,
    /// The `shunt` detail's actionable lines (the fix lines and the `enabled`
    /// row), in card order. Empty elsewhere.
    pub(crate) shunt_focus: Vec<ShuntFocus>,
    /// On the `shunt` card, the detail index where its action section begins
    /// (the blank line before the `enabled` row, or the first `f  adopt
    /// config` line on an absent card). Lines at/after it render verbatim with
    /// per-line focus, instead of the shared `key: value` splitter — the MOVE
    /// PLAN copy holds a literal `: `. `None` elsewhere.
    pub(crate) shunt_action_start: Option<usize>,
}

impl Check {
    /// How many `f  <verb>` lines the detail shows: the plugin and herdr
    /// problems plus the shunt card's fix stops, the lines `f` walks.
    pub(crate) fn fix_line_count(&self) -> usize {
        self.problems.len()
            + self
                .shunt_focus
                .iter()
                .filter(|stop| stop.fix().is_some())
                .count()
    }
}

/// The one verb per fix, used identically in the detail's dim `f  <verb>` line
/// and the footer's `↵ <verb>` hint. One verb per fix, never the bracketed
/// `[f] repair credentials` anti-pattern.
pub(crate) fn fix_verb(fix: &ServiceFix) -> &'static str {
    match fix {
        ServiceFix::WireMcpServers => "wire mcp server",
        ServiceFix::InstallPlugin => "install plugin",
        ServiceFix::HealHerdrConfig(_) => "heal herdr config",
        ServiceFix::AdoptConfig(_) => "adopt config",
        ServiceFix::MoveStoresIn => "move stores in",
        ServiceFix::AddAdminKey => "add admin key",
        ServiceFix::AddAdminTable => "add admin table",
    }
}

/// The `f  <verb>` detail line for a fix.
pub(crate) fn fix_line(fix: &ServiceFix) -> String {
    format!("f  {}", fix_verb(fix))
}

/// One Services probe that must never hold a frame: at most one run in
/// flight, its result landing through [`Self::drain`] on the tick.
#[derive(Debug)]
pub(crate) struct ProbeWorker<T> {
    /// What the worker runs, or `None` under test, so no test spawns the real
    /// probe unless it opts in with a stub (the `App::clipboard` seam's shape).
    pub(crate) prober: Option<fn() -> T>,
    /// A run is in flight; a second never starts beside it.
    pub(crate) running: bool,
    /// A re-probe asked for while a run was in flight: one follow-up run
    /// starts when that run's result drains, since the in-flight run may
    /// predate the change the re-probe was asked to see.
    rerun: bool,
    pub(crate) tx: std::sync::mpsc::Sender<T>,
    rx: std::sync::mpsc::Receiver<T>,
}

impl<T: Default + Send + 'static> ProbeWorker<T> {
    fn new(prober: fn() -> T) -> Self {
        let (tx, rx) = std::sync::mpsc::channel();
        Self {
            prober: (!cfg!(test)).then_some(prober),
            running: false,
            rerun: false,
            tx,
            rx,
        }
    }

    /// Start a run, or queue one follow-up behind the run in flight: the
    /// explicit re-probe (`r`), whose result must postdate the call.
    fn restart(&mut self) {
        if self.running && self.prober.is_some() {
            self.rerun = true;
        } else {
            self.start();
        }
    }

    /// Start a run unless one is in flight. A prober that panics lands as
    /// `T::default()`, so `running` never sticks.
    fn start(&mut self) {
        let Some(prober) = self.prober else {
            return;
        };
        if self.running {
            return;
        }
        self.running = true;
        let tx = self.tx.clone();
        spawn_worker(move || {
            let _ = tx.send(catch_unwind(prober).unwrap_or_default());
        });
    }

    /// The newest result that landed since the last drain, if any. A landing
    /// with a follow-up queued by [`Self::restart`] starts it and is dropped:
    /// that run may predate the change the re-probe was asked to see, so only
    /// the follow-up's result reaches the caller.
    fn drain(&mut self) -> Option<T> {
        let mut landed = None;
        while let Ok(result) = self.rx.try_recv() {
            landed = Some(result);
            self.running = false;
        }
        if landed.is_some() && std::mem::take(&mut self.rerun) {
            self.start();
            return None;
        }
        landed
    }
}

/// Which env the store plan, the pool read and the standalone readout resolve
/// against ([`crate::gateway::inherited_env`]): this process's own with no
/// daemon, else the env record of the daemon holding the singleton, which
/// [`crate::gateway::inherited_env`] matches by pid and start time. A
/// fresh/stale flap keeps the source. A daemon restarted between two reads
/// moves it, though no read saw it absent, and so does the record a starting
/// daemon writes after it stamps its pid.
#[derive(Debug, Clone, PartialEq, Eq)]
enum PlanEnvSource {
    Own,
    Daemon {
        /// The pid sidecar; `None` while unreadable.
        pid: Option<u32>,
        /// The identity the env record names; `None` with no record.
        record: Option<crate::gateway::DaemonIdentity>,
    },
}

impl PlanEnvSource {
    fn of(health: crate::daemon::DaemonHealth) -> Self {
        match health {
            crate::daemon::DaemonHealth::Absent => Self::Own,
            crate::daemon::DaemonHealth::Stale | crate::daemon::DaemonHealth::Fresh => {
                Self::Daemon {
                    pid: crate::daemon::holder_pid(),
                    record: crate::gateway::recorded_daemon_identity(),
                }
            }
        }
    }
}

/// The cached standalone-store move plan, computed on a worker (the plan
/// spawns `ps`/`powershell` on macOS/Windows).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) enum MovePlanOutcome {
    /// A plan that moves at least one file — the MOVE PLAN section shows.
    Plan(crate::gateway::StoreMovePlan),
    /// The plan refused: the refusal's `Display`, ready to render as
    /// `refused  <…>`.
    Refused(String),
    /// A plan that moves nothing and does not refuse — no MOVE PLAN section.
    Nothing,
    /// No record, or the record/plan could not be read — no MOVE PLAN section.
    #[default]
    Unavailable,
}

/// The cached codex-pool read, computed on a worker (the pool read blocks up
/// to ~24 s).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PoolOutcome {
    /// The providers needing their own pool login, in gateway order.
    Providers(Vec<String>),
    /// The read failed: the error's `Display`, the approved `pool  <error>`
    /// sentence.
    Error(String),
}

impl Default for PoolOutcome {
    /// A panicked pool read shows no note and no error.
    fn default() -> Self {
        PoolOutcome::Providers(Vec::new())
    }
}

/// A [`ConfigHandle`] carried through the action worker to a token add's
/// endpoint write. `RankedMutex` has no `Debug`, so the wrapper prints a
/// placeholder; the job's `Debug` names every other field.
#[derive(Clone)]
pub(crate) struct JobConfig(pub(crate) ConfigHandle);

impl std::fmt::Debug for JobConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("JobConfig(<config>)")
    }
}

/// One confirmed shunt-card action, handed to the action worker so the slow
/// call never holds a frame.
#[derive(Debug, Clone)]
pub(crate) enum ShuntActionJob {
    Adopt {
        found: std::path::PathBuf,
    },
    Move {
        record: crate::gateway::GatewayRecord,
        confirmed: crate::gateway::StoreMovePlan,
    },
    AddAdminKey {
        record: crate::gateway::GatewayRecord,
    },
    AddAdminTable {
        record: crate::gateway::GatewayRecord,
    },
    /// `stop shunt`: write the gateway hold naming the daemon that holds the
    /// singleton now.
    Hold,
    /// `start shunt` on a held gateway: remove the hold.
    RemoveHold,
    /// Save one config row (a field, a `save <kind>`, or a `remove <kind>`)
    /// through the E1 engine (`apply_edit`), off the UI thread. `key` rides
    /// back in the outcome so the row can spinner/`✓` it; `confirmed` names a
    /// save that went through the `restart shunt?` confirm.
    ConfigEdit {
        record: crate::gateway::GatewayRecord,
        edit: Box<crate::gateway_edit::Edit>,
        key: ShuntSaveKey,
        confirmed: bool,
    },
    /// Revoke one profile's client token through the engine, off the UI thread.
    RemoveToken {
        record: crate::gateway::GatewayRecord,
        profile: String,
    },
    /// Mint a client token for `profile`, then point it at the gateway (`url`),
    /// off the UI thread. `config` is the shared handle the endpoint write
    /// locks.
    AddToken {
        record: crate::gateway::GatewayRecord,
        profile: String,
        url: String,
        config: JobConfig,
    },
}

/// The result of one shunt-card action, drained on the tick into a toast and a
/// recompute.
#[derive(Debug, Clone, Default)]
pub(crate) enum ShuntActionOutcome {
    Adopted {
        path: String,
    },
    /// A standalone shunt answered at the adopt probe: nothing written.
    AdoptRefused {
        version: String,
        addr: std::net::SocketAddr,
    },
    AdoptFailed {
        error: String,
    },
    Moved {
        n: usize,
    },
    /// The stores moved, but writing the record's `disabled = false` failed:
    /// the success toast still shows, then the house `save failed` toast.
    MovedEnableFailed {
        n: usize,
        error: String,
    },
    MoveFailed {
        error: String,
    },
    AdminAdded,
    AdminFailed {
        error: String,
        /// The failed check's stderr, held for the shunt card's `check output`
        /// modal. `None` on a refusal that ran no check (or a non-typed error).
        check_output: Option<crate::gateway::CheckStderr>,
        /// The adopted config path this edit ran against. The drain holds
        /// `check_output` only while it equals the current record's config, so
        /// a re-adopt landing while the check ran never shows another
        /// gateway's output on the card.
        config: std::path::PathBuf,
    },
    /// `stop shunt` landed: the hold file names the running daemon.
    Held,
    HoldFailed {
        error: String,
    },
    /// `start shunt` on a held gateway released the hold.
    Released,
    ReleaseFailed {
        error: String,
    },
    /// The worker panicked, or no action was started: no toast, just a
    /// recompute so the card reads the record's real state.
    #[default]
    NoOp,
    /// A config save landed: the `✓` shows on the row, and (for a confirmed
    /// written bind) the restart toast. `value` is what was saved (`None` =
    /// unset, the bind for the restart toast); `restart_only` names a bind
    /// save; `confirmed` says it went through the restart confirm.
    ConfigSaved {
        key: ShuntSaveKey,
        written: bool,
        restart_only: bool,
        value: Option<String>,
        confirmed: bool,
    },
    /// The engine refused the edit on the input or the file's shape: its own
    /// sentence renders on the `└` DANGER line under the row (the field's, the
    /// `save <kind>`'s or the `remove <kind>`'s), the draft/editor kept open.
    ConfigRefused {
        key: ShuntSaveKey,
        sentence: String,
    },
    /// `shunt check` (or the landing) refused: the compact detail line and the
    /// held `check output`, as the admin edit surfaces them.
    ConfigCheckFailed {
        error: String,
        check_output: Option<crate::gateway::CheckStderr>,
        config: std::path::PathBuf,
    },
    /// An engine bug or an untyped failure: `save failed` + the error chain.
    ConfigFailed {
        error: String,
    },
    /// A client-token remove landed (`removed` = true) or was already gone.
    TokenRemoved {
        profile: String,
        removed: bool,
    },
    /// The remove refused (the last-token guard or a read failure): the
    /// refusal's own sentence, on the `└` line while the row is in view, else
    /// a `remove failed` toast.
    TokenRemoveRefused {
        profile: String,
        sentence: String,
    },
    /// A remove failed untyped: `remove failed` + the error chain.
    TokenRemoveFailed {
        error: String,
    },
    /// A client-token add landed: the profile's row appears, focus on it.
    TokenAdded {
        profile: String,
    },
    /// The add refused (a bad name, a name the base uses, an unparsed base, or
    /// an untyped failure): `add failed` + the message.
    TokenAddRefused {
        error: String,
    },
    /// The mint landed but the profile write failed: the token row stays,
    /// `add failed` + either `the token is stored but <profile> was not
    /// changed: <e:#>` (the disk profile still has its old base url) or
    /// `<profile> points at the gateway, but <e:#>` (the save landed and only
    /// the active-profile settings write failed).
    TokenAddedProfileFailed {
        error: String,
    },
}

/// The one-shot worker behind the shunt card's confirmed actions: at most one
/// action runs at a time, its result landing through the tick's drain. The
/// same `prober` seam as [`ProbeWorker`] — `None` under test unless a stub is
/// installed — and `running` is the gate that makes a second press of any
/// shunt-card action start nothing.
#[derive(Debug)]
pub(crate) struct ShuntActionWorker {
    pub(crate) prober: Option<fn(ShuntActionJob) -> ShuntActionOutcome>,
    pub(crate) running: bool,
    pub(crate) tx: std::sync::mpsc::Sender<ShuntActionOutcome>,
    rx: std::sync::mpsc::Receiver<ShuntActionOutcome>,
}

impl ShuntActionWorker {
    fn new(prober: fn(ShuntActionJob) -> ShuntActionOutcome) -> Self {
        let (tx, rx) = std::sync::mpsc::channel();
        Self {
            prober: (!cfg!(test)).then_some(prober),
            running: false,
            tx,
            rx,
        }
    }

    /// Start a job unless one is in flight; a prober that panics lands as the
    /// `NoOp` default, so `running` never sticks.
    fn start(&mut self, job: ShuntActionJob) {
        let Some(prober) = self.prober else {
            return;
        };
        if self.running {
            return;
        }
        self.running = true;
        let tx = self.tx.clone();
        spawn_worker(move || {
            let _ = tx.send(catch_unwind(AssertUnwindSafe(|| prober(job))).unwrap_or_default());
        });
    }

    /// The newest result that landed since the last drain, if any.
    fn drain(&mut self) -> Option<ShuntActionOutcome> {
        let mut landed = None;
        while let Ok(result) = self.rx.try_recv() {
            landed = Some(result);
            self.running = false;
        }
        landed
    }
}

/// UI-thread-only state for the Services tab. Recomputed synchronously on tab
/// focus and on `r` (the reads are local FS/`PATH`; `claude --version` is one
/// cached subprocess gated by [`ServicesState::cc_version`]); the herdr and
/// standalone-shunt probes run on workers ([`ProbeWorker`]).
#[derive(Debug)]
pub(crate) struct ServicesState {
    pub(crate) focus: ServicesFocus,
    /// Cursor over the service rows (`0..checks.len()`).
    pub(crate) cursor: usize,
    pub(crate) selector: ListView,
    pub(crate) form_offset: std::cell::Cell<usize>,
    pub(crate) detail_scroll: u16,
    pub(crate) diagnostic_focus: bool,
    pub(crate) displayed_scroll: std::cell::Cell<u16>,
    pub(crate) detail_viewport: std::cell::Cell<u16>,
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
    /// resolve (no row), `Some(Some(p))` = the probe. The first recompute
    /// (entering the tab, or construction on the first herdr landing) starts
    /// it on a worker, since it spawns three subprocesses, and every `r`
    /// starts it or queues one follow-up behind the run in flight;
    /// the result lands through `drain_service_probes`, and until then the cached
    /// value renders. A tab switch and the per-tick refresh reuse it.
    pub(crate) herdr: Option<Option<crate::herdr::HerdrProbe>>,
    /// The worker running [`crate::herdr::probe`].
    pub(crate) herdr_probe: ProbeWorker<Option<crate::herdr::HerdrProbe>>,
    /// Cached standalone-shunt readout, for the `shunt` row while no gateway
    /// is adopted: `None` = unprobed. Probed on a worker (config discovery plus
    /// one `/health` round trip) the first time the row reads `absent`; every
    /// `r` starts it or queues one follow-up behind the run in flight; a tab
    /// switch and the per-tick refresh reuse it, except that a claim that a
    /// shunt answers is re-probed while the tab shows it
    /// ([`STANDALONE_RECHECK_INTERVAL`]).
    pub(crate) standalone: Option<StandaloneShunt>,
    /// The worker running [`standalone_probe`].
    pub(crate) standalone_probe: ProbeWorker<StandaloneShunt>,
    /// When the shown claim was last re-probed.
    pub(crate) last_standalone_recheck: Instant,
    /// The gateway record on disk, read once per recompute (a cheap local
    /// read). `None` = no record, or one that cannot load. Every shunt-card
    /// action and the `enabled` row read this, never the file per frame.
    pub(crate) shunt_record: Option<crate::gateway::GatewayRecord>,
    /// The adopted config's editor view, read through
    /// [`crate::gateway_edit::read_view`] on the recompute while a record
    /// exists. `None` = no record (no config section). The render draws the
    /// card's config groups from it.
    pub(crate) shunt_config: Option<ShuntConfigRead>,
    /// Which config entry rows are expanded, keyed by the entry's identity.
    /// Dropped keys whose entry left the config are pruned on the recompute.
    pub(crate) shunt_expanded: HashSet<ShuntEntryKey>,
    /// The shunt card's open field editor; `None` at rest.
    pub(crate) shunt_field_edit: Option<ShuntFieldEditState>,
    /// One field save in flight; the row renders the spinner + shimmer +
    /// `checking` while it runs.
    pub(crate) shunt_save: Option<ShuntSave>,
    /// The field whose save just landed and when — its caret slot shows `✓`
    /// for about a second, then the normal caret.
    pub(crate) shunt_ok: Option<(ShuntOkKey, Instant)>,
    /// The open add forms (at most one per kind), each surviving the card's
    /// recompute and any other row's save.
    pub(crate) shunt_drafts: ShuntDrafts,
    /// A remove's `EditRefusal`, rendered on the `└` line under the entry's
    /// `remove <kind>` row while the entry is still expanded. RefCell so the
    /// focus setter (which takes `&self`, the render's contract) can retire it
    /// the moment focus leaves the remove row.
    pub(crate) shunt_remove_refusal: std::cell::RefCell<Option<ShuntRemoveRefusal>>,
    /// The client-token section's data, read beside the config view on the
    /// recompute (the render reads this cache, never the store per frame).
    pub(crate) shunt_tokens: ShuntTokens,
    /// A token remove's refusal (the last-token guard), as `(profile,
    /// sentence)`, rendered on the `└` line under the token row. RefCell so the
    /// focus setter can retire it the moment focus leaves the row.
    pub(crate) shunt_token_refusal: std::cell::RefCell<Option<(String, String)>>,
    /// The cached standalone-store move plan (`None` = not yet planned). The
    /// plan runs on a worker — it spawns `ps`/`powershell` on macOS/Windows —
    /// on tab entry, on `r`, and after any shunt-card action lands.
    pub(crate) move_plan: Option<MovePlanOutcome>,
    /// The worker running [`move_plan_probe`].
    pub(crate) move_plan_probe: ProbeWorker<MovePlanOutcome>,
    /// The adopted config's admin need, read on the recompute (a local read +
    /// parse, the herdr config re-read's shape). `None` = no record, or an
    /// unreadable config (no verb offered).
    pub(crate) admin_need: Option<crate::gateway::AdminNeed>,
    /// The stderr of the last admin edit or config save THIS TUI ran whose
    /// `shunt check` refused, held for the shunt card's `check output` modal.
    /// Cleared when an admin edit succeeds (`AdminAdded`), a later failure of
    /// either kind edited another config, the gateway record changes, or
    /// `admin_need` reads `None`; an admin edit's output also when a later
    /// admin edit fails with no check output or `admin_need` reads `Neither`,
    /// a config save's when a later save lands.
    /// The daemon's own restart refusals never land here.
    pub(crate) check_output: Option<HeldCheckOutput>,
    /// The cached codex-pool read (`None` = not read, or not due). The read
    /// runs on a worker while the slot is `healthy` and `admin_need` is
    /// `Neither`, and can block up to ~24 s.
    pub(crate) pool: Option<PoolOutcome>,
    /// The worker running [`pool_probe`].
    pub(crate) pool_probe: ProbeWorker<PoolOutcome>,
    /// The worker running one confirmed shunt-card action at a time.
    pub(crate) shunt_action: ShuntActionWorker,
    /// The worker running one config field save at a time, apart from
    /// [`Self::shunt_action`] so a running stop/start/adopt/admin action can
    /// neither block nor be blocked by a save. Its `running` is the one-save
    /// guard; a panicked save drains as a failed outcome that clears it.
    pub(crate) shunt_save_worker: ShuntActionWorker,
    /// The `shunt` detail's focused actionable line, by its kind. Owned by the
    /// key handler and the recompute; the render reads it to draw the caret,
    /// and re-settles it (the descend rule) where it publishes the geometry
    /// when a resize or a card change pushed the held line out of view.
    pub(crate) shunt_focus: std::cell::RefCell<Option<ShuntFocusLine>>,
    /// The rendered line index of each [`Check::shunt_focus`] stop (parallel to
    /// that list), published by the last render so the key handler can tell
    /// which stops are in view. A stop's rendered row is what the in-view test
    /// measures: a wrapped read-only sentence above a stop can shift it down.
    pub(crate) shunt_stop_rows: std::cell::RefCell<Vec<usize>>,
    /// The last rendered shunt pane height, published by the render the way
    /// [`ServicesState::detail_max_scroll`] is — the key handler's viewport.
    pub(crate) shunt_viewport: std::cell::Cell<u16>,
    /// The env source the last recompute saw, so a daemon start, stop or
    /// restart can restart the plan/pool workers and the standalone probe.
    last_plan_env_source: Option<PlanEnvSource>,
    /// The `clauth mcp` job store as of the last refresh, newest first: what the
    /// delegates detail draws. Re-read on the same cadence as the checks, because
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
    /// Cursor over the delegates detail's job list while it is descended.
    pub(crate) delegates_cursor: usize,
    /// The selected job's id, tracked across a refresh so a newer job inserting
    /// above the selection does not move it onto a different job.
    pub(crate) delegates_selected_id: Option<String>,
    /// The delegates detail's persisted scroll offset and published geometry,
    /// parallel to the member card's `fallback_detail_*`: the offset survives
    /// across frames, and the page keys step against the last render's viewport.
    pub(crate) delegates_offset: std::cell::Cell<usize>,
    pub(crate) delegates_viewport: std::cell::Cell<usize>,
    pub(crate) delegates_total: std::cell::Cell<usize>,
    pub(crate) delegates_starts: std::cell::RefCell<Vec<usize>>,
    pub(crate) delegates_block_ends: std::cell::RefCell<Vec<usize>>,
    /// True once the delegates detail has been descended at least once: from
    /// then on the blurred pane keeps the last-selected row's tint and offset.
    pub(crate) delegates_has_descended: bool,
    /// Cursor over the plugin detail's fixable problems ([`Check::problems`]).
    /// While the plugin detail is descended, ↑↓ and `f` walk these instead of
    /// scrolling the prose; `↵` opens the focused one's confirm.
    pub(crate) problem_cursor: usize,
    /// Cursor over the herdr detail's options rows ([`HERDR_OPTIONS`]). The
    /// herdr detail is the one detail pane that takes per-row focus: while it
    /// is descended, ↑↓ walks these rows instead of scrolling the prose.
    pub(crate) herdr_options_cursor: usize,
    /// Whether the herdr detail's focus sits on its `f  heal herdr config`
    /// line rather than on [`Self::herdr_options_cursor`]'s option. Read only
    /// while the check still offers that fix, so a heal that clears it hands
    /// focus back to the option.
    pub(crate) herdr_fix_focus: bool,
    /// Typed editor for the `tag refresh` stepper — the Config-tab
    /// refresh-interval editor's shape. `None` = not editing.
    pub(crate) herdr_tag_draft: Option<InputState>,
    /// The herdr config verdict from the last recompute (a cheap file re-read
    /// that rides the per-tick refresh), cached so the render and the key
    /// handler never re-read the file per frame. `None` = unreadable or no
    /// config path. The options section reads it to decide whether the
    /// `delegate row text` row can write.
    pub(crate) herdr_config: Option<crate::herdr::ConfigStatus>,
    /// A pending first-herdr-launch landing: the cursor lands on the `herdr` row
    /// in the recompute that adopts its probe, or never. Cleared there, or
    /// earlier when a modal opens, the tab changes, or the user moves the
    /// cursor, descends or fixes.
    pub(crate) land_on_herdr: bool,
}

impl Default for ServicesState {
    fn default() -> Self {
        Self {
            focus: ServicesFocus::List,
            cursor: 0,
            selector: ListView::default(),
            form_offset: std::cell::Cell::new(0),
            detail_scroll: 0,
            diagnostic_focus: false,
            displayed_scroll: std::cell::Cell::new(0),
            detail_viewport: std::cell::Cell::new(0),
            detail_max_scroll: std::cell::Cell::new(0),
            fetching: false,
            error: None,
            checks: Vec::new(),
            cc_version: None,
            mcp_boot: None,
            herdr: None,
            herdr_probe: ProbeWorker::new(crate::herdr::probe),
            standalone: None,
            standalone_probe: ProbeWorker::new(standalone_probe),
            last_standalone_recheck: Instant::now(),
            shunt_record: None,
            shunt_config: None,
            shunt_expanded: HashSet::new(),
            shunt_field_edit: None,
            shunt_save: None,
            shunt_ok: None,
            shunt_drafts: ShuntDrafts::default(),
            shunt_remove_refusal: std::cell::RefCell::new(None),
            shunt_tokens: ShuntTokens::default(),
            shunt_token_refusal: std::cell::RefCell::new(None),
            move_plan: None,
            move_plan_probe: ProbeWorker::new(move_plan_probe),
            admin_need: None,
            check_output: None,
            pool: None,
            pool_probe: ProbeWorker::new(pool_probe),
            shunt_action: ShuntActionWorker::new(run_shunt_action),
            shunt_save_worker: ShuntActionWorker::new(run_shunt_action),
            shunt_focus: std::cell::RefCell::new(None),
            shunt_stop_rows: std::cell::RefCell::new(Vec::new()),
            shunt_viewport: std::cell::Cell::new(0),
            last_plan_env_source: None,
            delegates: Vec::new(),
            delegates_cursor: 0,
            delegates_selected_id: None,
            delegates_offset: std::cell::Cell::new(0),
            delegates_viewport: std::cell::Cell::new(0),
            delegates_total: std::cell::Cell::new(0),
            delegates_starts: std::cell::RefCell::new(Vec::new()),
            delegates_block_ends: std::cell::RefCell::new(Vec::new()),
            delegates_has_descended: false,
            problem_cursor: 0,
            herdr_options_cursor: 0,
            herdr_fix_focus: false,
            herdr_tag_draft: None,
            herdr_config: None,
            land_on_herdr: false,
        }
    }
}

impl ServicesState {
    /// Total selectable rows (the service rows).
    pub(crate) fn row_count(&self) -> usize {
        self.checks.len()
    }

    /// The check under the cursor.
    pub(crate) fn selected_check(&self) -> Option<&Check> {
        self.checks.get(self.cursor)
    }

    /// Whether the row under the cursor shows at least one fix line, the rows
    /// `f` walks and the selector marks.
    pub(crate) fn selected_has_fix(&self) -> bool {
        self.selected_check()
            .is_some_and(|check| check.fix.is_some())
    }

    /// The fix whose line holds the detail's focus, the one `↵` opens: the
    /// plugin's focused problem, the herdr heal line while focus sits on it,
    /// or the shunt card's focused fix line. `None` with the list focused or
    /// while a page key hid the plugin/herdr action focus. The footer's
    /// `↵ <verb>` follows this.
    pub(crate) fn focused_fix(&self) -> Option<&ServiceFix> {
        if self.focus == ServicesFocus::List {
            return None;
        }
        let check = self.selected_check()?;
        match check.label {
            "shunt" => self.focused_shunt_focus().and_then(ShuntFocus::fix),
            _ if self.diagnostic_focus => None,
            "herdr" => self
                .herdr_fix_focus
                .then(|| check.problems.first().map(|p| &p.fix))
                .flatten(),
            _ => check.problems.get(self.problem_cursor).map(|p| &p.fix),
        }
    }

    /// Whether `f` would move focus: the row shows a fix line other than the
    /// one already focused. The footer's `f next fix` reads this.
    pub(crate) fn f_moves(&self) -> bool {
        let lines = self.selected_check().map_or(0, Check::fix_line_count);
        lines > 1 || (lines == 1 && self.focused_fix().is_none())
    }

    /// The shunt detail's focused actionable line, resolved by the focus
    /// identity the key handler owns and gated by the one in-view predicate
    /// every acting key and the footer consult: the resolved stop holds only
    /// while its rendered row is on screen (the geometry the last render
    /// published). A resize or a card change that pushes the focused line out
    /// of view re-settles focus to the first actionable line in view (the
    /// descend rule) where the render publishes the geometry, so this only
    /// reads `None` when no actionable line is in view.
    pub(crate) fn focused_shunt_focus(&self) -> Option<&ShuntFocus> {
        let kind = self.shunt_focus_kind()?;
        let stops = &self.selected_check()?.shunt_focus;
        let i = stops.iter().position(|s| s.kind() == kind)?;
        let rows = self.shunt_stop_rows.borrow();
        if rows.len() != stops.len() {
            // No geometry published for this card yet (a key before the first
            // frame, or a unit test that skips the render): with nothing to
            // measure, the by-kind identity is the answer. The in-view gate
            // only applies over a published geometry.
            return Some(&stops[i]);
        }
        let scroll = self.detail_scroll.min(self.detail_max_scroll.get());
        shunt_in_view(scroll, self.shunt_viewport.get() as usize, rows[i]).then_some(&stops[i])
    }

    /// Whether a save for `key` is still checking (its own worker in flight).
    /// The one predicate every render, key-handler and footer site reads, so a
    /// hint, a spinner, the editor lock and a re-open guard can never disagree
    /// about which row is busy.
    pub(crate) fn saving(&self, key: &ShuntSaveKey) -> bool {
        self.shunt_save.as_ref().is_some_and(|s| s.key == *key)
    }

    /// The held focus identity, cloned out of the interior-mutability cell.
    pub(crate) fn shunt_focus_kind(&self) -> Option<ShuntFocusLine> {
        self.shunt_focus.borrow().clone()
    }

    /// Set the held focus identity. The moment focus leaves a remove row, that
    /// row's refusal retires (its sentence names a comment only meaningful
    /// while the user still sits on the row that raised it).
    pub(crate) fn set_shunt_focus(&self, kind: Option<ShuntFocusLine>) {
        self.shunt_focus.replace(kind.clone());
        let left_row = self
            .shunt_remove_refusal
            .borrow()
            .as_ref()
            .is_some_and(|refusal| {
                kind.as_ref() != Some(&ShuntFocusLine::Remove(refusal.key.clone()))
            });
        if left_row {
            self.shunt_remove_refusal.take();
        }
        let left_token = self
            .shunt_token_refusal
            .borrow()
            .as_ref()
            .is_some_and(|(profile, _)| {
                kind.as_ref() != Some(&ShuntFocusLine::Token(profile.clone()))
            });
        if left_token {
            self.shunt_token_refusal.take();
        }
    }

    /// The descend rule applied where the geometry is published: the render
    /// calls this over the freshly rendered rows so a resize or a card change
    /// re-settles focus to the first actionable line in view, else none. A
    /// held line pushed out of view re-settles regardless of pane focus; a
    /// focus that is none re-settles only while the detail pane holds focus —
    /// with the list focused, ascend keeps the identity and a none focus is
    /// the key handler's own state, never settled here.
    pub(crate) fn re_settle_shunt_focus(
        &self,
        stops: &[ShuntFocus],
        rows: &[usize],
        scroll: u16,
        viewport: usize,
    ) {
        if let Some(kind) = self.shunt_focus_kind() {
            if stops
                .iter()
                .position(|s| s.kind() == kind)
                .and_then(|i| rows.get(i))
                .is_some_and(|&r| shunt_in_view(scroll, viewport, r))
            {
                return;
            }
        } else if self.focus != ServicesFocus::Detail {
            return;
        }
        self.set_shunt_focus(shunt_first_in_view(stops, rows, scroll, viewport));
    }

    /// Re-settle the delegates selection against the current job list: the
    /// selection follows the job id across a refresh, and clamps to the last
    /// job when that id left the list. Called after every `delegates` refresh.
    pub(crate) fn settle_delegates_selection(&mut self) {
        let len = self.delegates.len();
        if len == 0 {
            self.delegates_cursor = 0;
            self.delegates_selected_id = None;
            return;
        }
        if let Some(id) = self.delegates_selected_id.clone()
            && let Some(pos) = self.delegates.iter().position(|j| j.record.job_id == id)
        {
            self.delegates_cursor = pos;
            return;
        }
        self.delegates_cursor = self.delegates_cursor.min(len - 1);
        self.delegates_selected_id = self
            .delegates
            .get(self.delegates_cursor)
            .map(|j| j.record.job_id.clone());
    }

    /// Whether `↵` on the row under the cursor descends into its detail: every
    /// row except a delegates row, which descends only while it lists a job.
    /// The one predicate the key handler and the footer both read.
    pub(crate) fn enter_descends(&self) -> bool {
        !self
            .selected_check()
            .is_some_and(|c| c.label == "delegates")
            || !self.delegates.is_empty()
    }

    /// Whether ↑↓/PageUp/PageDown move the delegates selection (≥2 jobs; a
    /// single job wraps to itself). The footer derives its descended hints
    /// from this, so a press that cannot move anything advertises nothing.
    pub(crate) fn delegates_selection_moves(&self) -> bool {
        self.delegates.len() > 1
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MainItemKind {
    Profile(usize),
    Codex(usize),
}

/// The Overview's visible harness filter; selection maps onto the rows it shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum HarnessFilter {
    #[default]
    All,
    Claude,
    Codex,
}

impl HarnessFilter {
    /// `c` cycles: both → claude → codex → both.
    pub(crate) fn next(self) -> Self {
        match self {
            HarnessFilter::All => HarnessFilter::Claude,
            HarnessFilter::Claude => HarnessFilter::Codex,
            HarnessFilter::Codex => HarnessFilter::All,
        }
    }
    pub(crate) fn shows_claude(self) -> bool {
        !matches!(self, HarnessFilter::Codex)
    }
    pub(crate) fn shows_codex(self) -> bool {
        !matches!(self, HarnessFilter::Claude)
    }
    /// Harness name for the accounts panel's left meta slot; `None` while both
    /// harnesses show, so the unfiltered panel renders no left slot.
    pub(crate) fn label_name(self) -> Option<&'static str> {
        match self {
            HarnessFilter::All => None,
            HarnessFilter::Claude => Some("claude"),
            HarnessFilter::Codex => Some("codex"),
        }
    }
}

/// One codex account as the Overview renders it — name, plan, the two windows
/// and whether its chain is quarantined. The windows come from the per-profile
/// usage cache the codex leg writes; the plan from that cache, else from the
/// store's id_token claim; `broken` from the quarantine record beside the
/// store. Deliberately NOT a `Profile`: synthesizing one would put a record
/// with no credentials into every claude path that walks `config.profiles`.
#[derive(Debug, Clone)]
pub(crate) struct CodexRow {
    pub(crate) name: ProfileName,
    pub(crate) active: bool,
    pub(crate) broken: bool,
    pub(crate) plan: Option<String>,
    pub(crate) five_hour: Option<crate::usage::UsageWindow>,
    pub(crate) seven_day: Option<crate::usage::UsageWindow>,
    /// Banked usage-limit resets off the same cache, floored at one
    /// available: `None` (or zero) renders no chip on the row.
    pub(crate) resets: Option<i64>,
}

/// Read the codex roster into the [`App::codex_rows`] snapshot. Lock-free: the
/// roster is one small TOML and each reading is the profile's own cache file
/// plus the small files beside its store (the quarantine record, and the store
/// itself on a cache miss for the plan), the same set `status --json` reads
/// with no daemon running.
pub(crate) fn codex_rows() -> Vec<CodexRow> {
    let Ok(state) = crate::codex_profiles::CodexState::load() else {
        return Vec::new();
    };
    let active = state.active_profile().cloned();
    state
        .profiles()
        .iter()
        .map(|name| {
            let cached: Option<crate::usage::UsageInfo> = crate::profile_cache::load_profile_cache(
                name,
                crate::profile_cache::USAGE_CACHE_FILE,
            );
            CodexRow {
                name: name.clone(),
                active: active.as_ref().is_some_and(|a| a == name),
                broken: crate::codex_auth::read_quarantine(name.as_str()).is_some(),
                plan: crate::codex_auth::plan_label(
                    name.as_str(),
                    cached
                        .as_ref()
                        .and_then(|u| u.plan.as_ref())
                        .and_then(|p| p.codex_plan.as_deref()),
                ),
                five_hour: cached.as_ref().and_then(|u| u.five_hour.clone()),
                seven_day: cached.as_ref().and_then(|u| u.seven_day.clone()),
                resets: cached
                    .as_ref()
                    .and_then(|u| u.codex_reset_credits)
                    .filter(|count| *count > 0),
            }
        })
        .collect()
}

// ── Login session ─────────────────────────────────────────────────────────────

/// An in-flight login. The worker blocks up to the login bound in
/// `oauth_login` for the first door — the browser callback or a pasted code —
/// while the UI stays live and applies the result in `on_tick`. `generation`
/// discards a stale result from a login the user superseded (esc-cancel or a
/// fresh login start).
pub(crate) struct LoginSession {
    pub(crate) name: String,
    /// true → the mint lands in the `+ new` draft (capture-then-commit);
    /// false → re-login an existing profile in place (overwrite).
    pub(crate) is_new: bool,
    pub(crate) generation: u64,
    /// The URL `r` re-opens: an OAuth login's browser link, known at start; the
    /// console login's page, once its worker announces it.
    pub(crate) url: Option<String>,
    /// Live milestone for the modal's stage line.
    pub(crate) stage: LoginStage,
    /// The door that delivered the code, as the worker reported it; `Browser`
    /// until then. The modal's stage copy follows it, so it is never set
    /// from the UI's own paste: the browser callback may have won first.
    pub(crate) method: LoginMethod,
    /// An OAuth login's paste door. `None` for the console login, which has no
    /// hosted link and no code to paste, so `c` and `p` are inert there.
    pub(crate) paste: Option<PasteDoor>,
    /// The login modal's inline code field, open (`Some`) from `p` until esc,
    /// a good submit, or the door closing. It lives here, not on the modal, so
    /// it dies with the session and never outlives the door it feeds. The
    /// typed bytes are a bearer-grade secret until exchanged: `LoginSession`
    /// derives no `Debug`, so they cannot ride a `{:?}`.
    pub(crate) paste_field: Option<InputState>,
}

/// The paste half of an OAuth login: the links `c` copies and a paste is
/// checked against, and the sender a parsed code goes down to the worker.
pub(crate) struct PasteDoor {
    pub(crate) links: crate::oauth_login::LoginLinks,
    pub(crate) tx: std::sync::mpsc::Sender<crate::oauth_login::ManualCode>,
}

impl LoginSession {
    /// The paste door while it can still win: the login is waiting on its
    /// first code. Once a door delivered one there is nothing to race, and the
    /// console login never had a door. Every `c`/`p` affordance gates on this.
    pub(crate) fn open_door(&self) -> Option<&PasteDoor> {
        match self.stage {
            LoginStage::WaitingBrowser => self.paste.as_ref(),
            LoginStage::ExchangingCode(_) | LoginStage::Verifying => None,
        }
    }
}

// Re-exported so the render code and the tests keep addressing it as this
// module's type: the CLI's login reports the same two doors off the same enum.
pub(crate) use crate::oauth_login::LoginMethod;

/// Where an in-flight login currently sits, mapped from
/// [`crate::oauth_login::LoginProgress`] worker events.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LoginStage {
    /// Waiting for the first door: the browser round-trip, or a pasted code.
    WaitingBrowser,
    /// A code landed through this door; exchanging it for tokens.
    ExchangingCode(LoginMethod),
    /// Tokens minted; verifying them against the API.
    Verifying,
}

/// Worker→UI login channel payload: the console login's announced URL, or a
/// stage bump (the first of which names the door that delivered the code).
pub(crate) enum LoginEvent {
    Url(String),
    Stage(LoginStage),
}

/// What a finished login worker produced. Both flows (the two-door OAuth
/// login, the Alibaba console) report through the same [`LoginEvent`] channel
/// and are drawn by the same modal, and they diverge only at apply time: an
/// Anthropic login replaces the profile's credentials, an Alibaba console login
/// replaces its usage session and touches nothing else.
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
    /// Interleaved auto-start queue (`usage::auto_start_queue`): the anchor the 5h-window
    /// queue spaces against, plus per-profile election health. Held here for
    /// the same reason as `kick_blocks` — the Fallback card's detail render
    /// reads it every frame, and a per-frame history replay is not an option.
    /// Shared with the refresher this TUI spawns, so what the queue chip shows
    /// is what the gate is actually using.
    pub(crate) auto_start_queue: crate::usage::AutoStartQueueState,
    /// Scheduler-posted auto-switch decisions; drained in `on_tick`.
    pub(crate) pending_switch: PendingSwitch,
    /// Set by the scheduler when the whole chain is spent; `on_tick` drains it
    /// and turns off all accounts.
    pub(crate) pending_switch_off: PendingSwitchOff,
    pub(crate) refetch_queue: RefetchQueue,

    pub(crate) third_party_tokens: ThirdPartyList,
    pub(crate) third_party_usage_store: ThirdPartyUsageStore,
    pub(crate) third_party_status: ThirdPartyStatusStore,
    pub(crate) third_party_streaks: ThirdPartyStreaks,
    pub(crate) third_party_broken: ThirdPartyBroken,
    /// Per-name stat of the durable key-rejection record the last time
    /// [`sync_broken_verdicts`] checked it (`None` = the record was absent).
    /// The stat is the read gate: a per-second stat per profile is cheap, a
    /// per-second read of every record is not.
    pub(crate) broken_verdict_mtimes: HashMap<String, Option<std::time::SystemTime>>,
    /// [`sync_broken_verdicts`]'s once-per-second gate (`None` = run on the
    /// first tick).
    pub(crate) last_broken_verdict_sync: Option<Instant>,
    pub(crate) tab: Tab,
    /// Running inside a herdr pane (`HERDR_ENV=1` at `cmd_tui`): the header
    /// carries a `[ herdr ]` tag and the TUI lands on the Services tab's herdr
    /// row. Read-only after construction — the mode is decided once at launch.
    pub(crate) herdr_mode: bool,
    pub(crate) modals: Vec<Modal>,
    /// First help-modal row on screen (↑↓ scrolls). Reset when the modal opens.
    pub(crate) help_scroll: u16,
    /// Max valid `help_scroll` from the last help render (`total - viewport`).
    /// Interior mutability because the render pass takes `&App`; clamping there
    /// stops a held ↓ from inflating `help_scroll` past the end.
    pub(crate) help_max_scroll: std::cell::Cell<u16>,
    /// First `check output`-modal row on screen (↑↓ scrolls). Reset on open.
    pub(crate) check_output_scroll: u16,
    /// Max valid `check_output_scroll` from the last `check output` render.
    pub(crate) check_output_max_scroll: std::cell::Cell<u16>,
    /// Visible `check output`-modal rows from the last render — the key
    /// handler's page step, published like `help_viewport`.
    pub(crate) check_output_viewport: std::cell::Cell<u16>,
    /// Visible help-modal rows from the last render — the key handler's page
    /// step (`PageUp`/`PageDown`), published like `help_max_scroll`.
    pub(crate) help_viewport: std::cell::Cell<u16>,

    /// Selected account index, shared across Overview/Usage/Setup tabs.
    /// On Setup may also rest on the trailing `+ new` row (== profile_count).
    pub(crate) profile_cursor: usize,
    pub(crate) overview_selector: ListView,
    pub(crate) overview_cursor: usize,
    pub(crate) usage_selector: ListView,
    pub(crate) usage_detail_focus: StatusFocus,
    pub(crate) usage_detail_scroll: usize,
    pub(crate) usage_detail_max_scroll: std::cell::Cell<usize>,
    pub(crate) usage_detail_viewport: std::cell::Cell<usize>,
    pub(crate) setup_selector: ListView,
    pub(crate) chain_selector: ListView,
    pub(crate) models_selector: ListView,
    pub(crate) setup_detail_offset: std::cell::Cell<usize>,
    pub(crate) setup_detail_viewport: std::cell::Cell<usize>,
    pub(crate) setup_detail_row_starts: std::cell::RefCell<Vec<usize>>,
    pub(crate) setup_detail_block_ends: std::cell::RefCell<Vec<usize>>,
    pub(crate) setup_detail_tips: std::cell::RefCell<Vec<usize>>,
    pub(crate) setup_detail_total: std::cell::Cell<usize>,
    pub(crate) fallback_detail_offset: std::cell::Cell<usize>,
    pub(crate) fallback_detail_viewport: std::cell::Cell<usize>,
    pub(crate) fallback_detail_row_starts: std::cell::RefCell<Vec<usize>>,
    pub(crate) fallback_detail_block_ends: std::cell::RefCell<Vec<usize>>,
    pub(crate) fallback_detail_tips: std::cell::RefCell<Vec<usize>>,
    pub(crate) fallback_detail_total: std::cell::Cell<usize>,
    pub(crate) global_config_offset: std::cell::Cell<usize>,
    pub(crate) global_config_viewport: std::cell::Cell<usize>,
    pub(crate) global_config_row_starts: std::cell::RefCell<Vec<usize>>,
    pub(crate) global_config_block_ends: std::cell::RefCell<Vec<usize>>,
    pub(crate) global_config_tips: std::cell::RefCell<Vec<usize>>,
    pub(crate) global_config_total: std::cell::Cell<usize>,
    /// Which harness the Overview lists (`c` cycles). A view filter only — see
    /// [`HarnessFilter`].
    pub(crate) harness_filter: HarnessFilter,
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
    /// The member card's one open edit, pinned to the member it opened on, or
    /// `None` at rest.
    pub(crate) fallback_edit: Option<MemberEdit<CardEdit>>,
    /// The custom day list `space` last stepped past on the open card, kept as
    /// one more stop in the preset cycle until the card closes. UI state only:
    /// never saved, dropped with the card.
    pub(crate) fallback_day_stop: Option<MemberEdit<Vec<Weekday>>>,
    /// Cursor into [`GLOBAL_CONFIG_ROWS`] on the program-wide Config tab.
    pub(crate) global_config_cursor: usize,
    /// `Some` while the refresh-interval custom-value field is open (⏎ opens,
    /// owns keyboard). Space/`+`/`-` still cycle the presets when `None`.
    pub(crate) refresh_interval_draft: Option<InputState>,
    /// In-flight custom value for the Config tab's context-nudge editor
    /// (`None` = not editing). Same lifecycle as `refresh_interval_draft`.
    pub(crate) context_nudge_draft: Option<InputState>,
    /// In-flight custom value for the Config tab's weekly-threshold editor
    /// (`None` = not editing). Same lifecycle as `refresh_interval_draft`.
    pub(crate) weekly_threshold_draft: Option<InputState>,
    /// The selected account's note (the usage tab's `notes:` row). Loaded on
    /// cursor moves, tab switches with a clamped cursor, and saves — never per
    /// frame.
    pub(crate) note_text: Option<String>,
    /// The open note editor, while `n` has one open on the usage tab.
    pub(crate) note_editor: Option<NoteEditor>,

    pub(crate) toasts: VecDeque<Toast>,
    /// Whether the terminal is currently too short for the normal layout (< 14 rows).
    /// Tracked across frames so the "too small" toast fires only on the transition in.
    pub(crate) compact: bool,
    /// Startup update check result; drained in `on_tick`. Silent on errors.
    pub(crate) update_results: std::sync::mpsc::Receiver<UpdateEvent>,
    /// Join handle for the update check thread; joined on TUI exit for clean shutdown.
    pub(crate) update_handle: Option<JoinHandle<()>>,

    /// In-flight login worker of any method (Setup tab); `None` when idle.
    pub(crate) login: Option<LoginSession>,
    /// What the login modal's `c` hands the hosted link to: OSC 52 on the
    /// terminal's stdout. Replaceable so a test keeps the escape off the
    /// terminal running the suite, which would take the fixture's link onto
    /// its clipboard.
    pub(crate) clipboard: fn(&str) -> std::io::Result<()>,
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

    /// `[ daemon ]` header-chip state: daemon presence + `status.json` health,
    /// re-probed on a throttled cadence in `on_tick`. UI-thread-only.
    pub(crate) daemon_health: crate::daemon::DaemonHealth,
    /// Throttle for the `daemon_health` flock probe; construct probes once
    /// itself, so this starts stamped over a real answer. Seeding a placeholder
    /// instead would render `Absent` — "no daemon runs" — as fact for the whole
    /// first interval, and the first paint happens before any `on_tick`.
    pub(crate) last_daemon_probe: Instant,
    /// The gateway slot's state under `daemon_health`, re-read beside it and
    /// whenever a daemon control, a shunt action or a Services recompute
    /// lands ([`sync_gateway_state`]). UI-thread-only.
    pub(crate) gateway_state: GatewayState,
    /// Whether the last [`App::set_gateway_state`] saw a gateway that would
    /// run with no daemon to run it; its rise raises the no-daemon toast.
    pub(crate) gateway_unrun: bool,
    /// The worker asking whether a gateway a daemon left behind still serves
    /// ([`crate::daemon::gateway::left_behind_gateway_runs`]), which picks the
    /// no-daemon toast's copy.
    pub(crate) left_behind_probe: ProbeWorker<bool>,
    /// A `start daemon` / `stop daemon` worker is running; the action menu
    /// offers neither verb until its outcome lands. UI-thread-only.
    pub(crate) daemon_control_busy: bool,
    /// Those workers' outcomes; drained in `on_tick`.
    pub(crate) daemon_control_rx: std::sync::mpsc::Receiver<DaemonControlResult>,
    /// Sender side; cloned into each worker.
    pub(crate) daemon_control_tx: std::sync::mpsc::Sender<DaemonControlResult>,
    /// Single-fetcher lease (#27), shared with the scheduler tick. The bootstrap
    /// switch one-shot runs only if THIS instance holds it.
    pub(crate) fetch_lease: Arc<crate::daemon::FetchLease>,

    /// Services tab state; UI-thread-only, recomputed on focus + `r` (no thread).
    pub(crate) services: ServicesState,

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
    pub(crate) model_detail_focus: StatusFocus,
    pub(crate) model_detail_scroll: u16,
    pub(crate) model_detail_max_scroll: std::cell::Cell<u16>,
    pub(crate) model_detail_viewport: std::cell::Cell<u16>,
    /// Model filter over the Tokens tab's model surfaces (action-menu driven).
    pub(crate) token_filter: TokenFilter,
    pub(crate) token_period: TokenPeriod,
    /// Focused card on the Tokens dashboard. Held across period/filter changes
    /// and across a resize (it names the card, not a layout slot).
    pub(crate) token_dash_focus: CardKind,
    /// First fully-visible card (index into the published reading order),
    /// reconciled each frame by [`follow_dash_focus`] and held in a `Cell` so
    /// the render pass can publish its correction (precedent: `token_dash_viewport`).
    pub(crate) token_dash_first: std::cell::Cell<usize>,
    /// Dashboard card reading order, published by the render pass so the key
    /// handler can walk `↑`/`↓` in the same order the frame drew.
    pub(crate) token_dash_order: std::cell::Cell<[CardKind; 7]>,
    /// Dashboard card `(top, bottom)` rows in reading order, published by the
    /// render pass for the key handler's whole-card scroll; empty until the
    /// first dashboard frame draws.
    pub(crate) token_dash_rows: std::cell::RefCell<Vec<(u16, u16)>>,
    /// Dashboard viewport height (body rows), published by the render pass.
    pub(crate) token_dash_viewport: std::cell::Cell<u16>,
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
    /// The live credentials hold a login no profile owns (a login at all, and
    /// `find_matching_oauth_profile` misses) — the condition the `+ new`
    /// form's `+ capture current login` row renders on. Recomputed on
    /// construct, on entering the Setup tab, on config reload (external
    /// changes), and at every in-TUI mutation that pins `last_reload_fp`
    /// forward instead of reloading (switch, capture, create, delete, login,
    /// logout).
    pub(crate) unsaved_live_login: bool,
    /// Origin of the ambient-animation phase clock; read through [`App::anim_ms`].
    pub(crate) started_at: Instant,
    /// Pinned animation phase for render tests that must sample a specific point
    /// of the wave. Backdating `started_at` instead would subtract from
    /// `Instant::now()`, which panics on a host whose uptime is shorter than the
    /// offset. Never compiled into the binary.
    #[cfg(test)]
    pub(crate) anim_phase_ms: Option<u64>,
    /// The day-list notices last surfaced, empty while the lists are ordinary.
    /// Holding the MESSAGES rather than a flag is what makes the gate right on
    /// both axes `AppConfig::day_claim_collision` documents: each string
    /// carries the day and the accounts, so it goes stale at midnight and on a
    /// config edit, and is byte-equal on every tick between. Holding the SET
    /// rather than one string is what keeps a second notice from repainting
    /// the first.
    pub(crate) day_claim_notices: Vec<String>,
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
    /// Throttle for the Services tab's per-tick live refresh (job store +
    /// wiring + herdr config); recompute fires at most once per
    /// `SERVICES_REFRESH_INTERVAL`.
    pub(crate) last_services_refresh: Instant,
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

    /// The 5h bell's latch: which accounts rang for their current crossing.
    pub(crate) bell_fired: crate::bell::BellLatch,

    /// Cached parsed usage history per profile from usage_history.jsonl. The
    /// file itself belongs to the scheduler's fetch path (`apply_outcome`),
    /// which may be running in another process, so this side is read-only.
    pub(crate) history_cache: HashMap<String, Vec<(u64, UsageInfo)>>,
    /// Last-seen content fingerprint (byte length, tail hash) per profile
    /// history file, for cache invalidation.
    pub(crate) history_fp: HashMap<String, (u64, u64)>,

    /// Cached parsed wallet series per profile from wallet_history.jsonl —
    /// the balance readings the wallet-burn rate replays. Same discipline as
    /// [`Self::history_cache`]: the third-party fetch leg is the only writer,
    /// this side is read-only and re-reads on a fingerprint change.
    pub(crate) wallet_cache: HashMap<String, Vec<crate::usage::WalletSample>>,
    /// Last-seen content fingerprint per profile wallet-history file.
    wallet_fp: HashMap<String, (u64, u64)>,

    /// Cached long-lived-token status per profile, keyed by name (absent when a
    /// profile has no sidecar). Read by the Overview render for the `⊘` danger
    /// marker, so it needn't stat each `session-token.json` per frame. Seeded
    /// at construct and refreshed on config reload; the expiry stamp it
    /// carries is compared to live `now_ms` at render time, so the clock
    /// ticking past expiry never needs a re-read — only an add / delete /
    /// re-mint does, which `reload_fingerprint` now catches.
    pub(crate) session_tokens: HashMap<String, crate::claude::SessionTokenStatus>,
    /// Live sessions per account, for the Overview `live` column, the Fallback
    /// member card and the header's fleet count. Cached because collecting it
    /// is a readdir plus an `open` + `try_lock` per row per marker layout, and
    /// every one of those surfaces reads it every frame. Refreshed by
    /// [`poll_live_sessions`] rather than on a config reload: sessions come and
    /// go without touching config.
    pub(crate) live_sessions: crate::live_sessions::LiveTally,
    /// Throttle for the per-tick re-tally; construct seeds the tally itself, so
    /// this starts stamped. `None` means un-sampled, which the gate reads as due
    /// — the state a fixture writes to force a re-tally, since backdating an
    /// `Instant` panics on a host booted more recently than the interval.
    last_live_sessions_refresh: Option<Instant>,
    /// The widest the header's `[ N live ]` chip has read `N`, in cells: the
    /// chip renders at least this wide for the TUI's lifetime.
    pub(crate) live_count_width: usize,
    /// The codex roster as the Overview lists it and the header counts it.
    /// Cached like `live_sessions`: [`codex_rows`] is a roster read plus a few
    /// small files per account, the header draws on every tab every frame, and
    /// the roster moves on a human timescale (a CLI verb in another terminal).
    /// One snapshot for both surfaces is also what keeps the header's count
    /// equal to the rows the Overview draws.
    pub(crate) codex_rows: Vec<CodexRow>,
    /// Throttle for the per-tick codex re-read; same contract as
    /// `last_live_sessions_refresh`.
    last_codex_rows_refresh: Option<Instant>,
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
    auto_start_queue: crate::usage::AutoStartQueueState,
    pending_switch: PendingSwitch,
    pending_switch_off: PendingSwitchOff,
    refetch_queue: RefetchQueue,
    third_party_tokens: ThirdPartyList,
    third_party_usage_store: ThirdPartyUsageStore,
    third_party_status: ThirdPartyStatusStore,
    third_party_streaks: ThirdPartyStreaks,
    third_party_broken: ThirdPartyBroken,
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
            auto_start_queue: Arc::clone(&app.auto_start_queue),
            pending_switch: Arc::clone(&app.pending_switch),
            pending_switch_off: Arc::clone(&app.pending_switch_off),
            refetch_queue: Arc::clone(&app.refetch_queue),
            third_party_tokens: Arc::clone(&app.third_party_tokens),
            third_party_usage_store: Arc::clone(&app.third_party_usage_store),
            third_party_status: Arc::clone(&app.third_party_status),
            third_party_streaks: Arc::clone(&app.third_party_streaks),
            third_party_broken: Arc::clone(&app.third_party_broken),
            shutting_down: Arc::clone(&app.shutting_down),
            fetch_lease: Arc::clone(&app.fetch_lease),
            bootstrap_active: Arc::clone(&app.bootstrap_active),
            startup_sender: app.startup_sender.clone(),
        }
    }
}

/// A cheap content fingerprint of a series log: its byte length folded with a
/// hash of its last 256 bytes. The re-read gates key on this instead of the
/// mtime, because NTFS quantizes file times on windows — an append can land
/// with a byte-identical `LastWriteTime` (measured 2026-09-13: 4/10 real-box
/// runs) — and the writer is whichever process holds the fetch lease, so the
/// mtime is the wrong signal. `None` when the file cannot be read, which
/// skips the re-read exactly like the old unreadable-mtime path did.
fn series_fingerprint(path: &std::path::Path) -> Option<(u64, u64)> {
    let mut file = std::fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    use std::hash::Hasher;
    use std::io::{Read, Seek, SeekFrom};
    // Clamped to the file's own start: End(-256) on a file under 256 bytes
    // seeks before position 0 and errors (measured: EINVAL), which would read
    // as "unreadable" and silently skip the re-read.
    file.seek(SeekFrom::Start(len.saturating_sub(256))).ok()?;
    let mut tail = Vec::with_capacity(256);
    file.read_to_end(&mut tail).ok()?;
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    std::hash::Hash::hash(&tail, &mut hasher);
    Some((len, hasher.finish()))
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
        let auto_start_queue = crate::usage::new_auto_start_queue_state();
        let pending_switch: PendingSwitch = Arc::new(RankedMutex::new(HashSet::new()));
        let pending_switch_off: PendingSwitchOff = Arc::new(RankedMutex::new(false));
        let refetch_queue: RefetchQueue = Arc::new(RankedMutex::new(HashSet::new()));
        let third_party_tokens: ThirdPartyList = Arc::new(RankedMutex::new(
            collect_third_party_entries(&config.profiles),
        ));
        let third_party_usage_store: ThirdPartyUsageStore =
            Arc::new(RankedMutex::new(HashMap::new()));
        let third_party_status: ThirdPartyStatusStore = Arc::new(RankedMutex::new(HashMap::new()));
        let third_party_streaks: ThirdPartyStreaks = Arc::new(RankedMutex::new(HashMap::new()));
        let third_party_broken: ThirdPartyBroken = Arc::new(RankedMutex::new(HashMap::new()));
        // Seed the live mirror from the durable verdict so a fingerprint-bound
        // key rejection survives a restart (see `durable_key_rejected_seed`).
        if let Ok(mut broken) = third_party_broken.lock() {
            broken.extend(Self::durable_key_rejected_seed(&config));
        }
        let refresh_interval = Arc::new(AtomicU64::new(config.state.refresh_interval_ms));

        let mut history_cache: HashMap<String, Vec<(u64, UsageInfo)>> = HashMap::new();
        let mut history_fp: HashMap<String, (u64, u64)> = HashMap::new();
        for profile in &config.profiles {
            let name = &profile.name;
            let data = crate::profile::load_usage_history(name);
            if !data.is_empty() {
                if let Ok(path) = crate::profile::profile_history_path(name)
                    && let Some(fp) = series_fingerprint(&path)
                {
                    history_fp.insert(name.to_string(), fp);
                }
                history_cache.insert(name.to_string(), data);
            }
        }

        // The wallet series mirrors it: same read-only discipline, same
        // fingerprint invalidation, only the file and the type differ — and
        // only third-party profiles carry one, so the loop stats no OAuth
        // profile's absent file.
        let mut wallet_cache: HashMap<String, Vec<crate::usage::WalletSample>> = HashMap::new();
        let mut wallet_fp: HashMap<String, (u64, u64)> = HashMap::new();
        for profile in config
            .profiles
            .iter()
            .filter(|p| p.usage_cache_is_third_party())
        {
            let name = &profile.name;
            let data = crate::profile::load_wallet_history(name);
            if !data.is_empty() {
                if let Ok(path) = crate::profile::profile_wallet_history_path(name)
                    && let Some(fp) = series_fingerprint(&path)
                {
                    wallet_fp.insert(name.to_string(), fp);
                }
                wallet_cache.insert(name.to_string(), data);
            }
        }

        // Kick the best-effort update check; verdict lands in `update_results`, toasted from `on_tick`.
        let (update_sender, update_results) = std::sync::mpsc::channel::<UpdateEvent>();
        let update_handle = update::spawn(update_sender, config.state.update.auto_update);

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

        let (daemon_control_tx, daemon_control_rx) = std::sync::mpsc::channel();
        let (login_event_tx, login_event_rx) = std::sync::mpsc::channel();
        let (login_result_tx, login_result_rx) = std::sync::mpsc::channel();

        // Seed the token cache and the live tally before `config` moves into the
        // handle below.
        let live_sessions = crate::live_sessions::LiveTally::collect(&config);
        let daemon_health = crate::daemon::daemon_health();
        let session_tokens = collect_session_tokens(
            &config
                .profiles
                .iter()
                .map(|p| p.name.to_string())
                .collect::<Vec<_>>(),
        );
        let initial_note = config
            .profiles
            .first()
            .and_then(|p| crate::profile_notes::load_note(&p.name));

        let mut app = Self {
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
            auto_start_queue,
            pending_switch,
            pending_switch_off,
            refetch_queue,
            third_party_tokens,
            third_party_usage_store,
            third_party_status,
            third_party_streaks,
            third_party_broken,
            broken_verdict_mtimes: HashMap::new(),
            last_broken_verdict_sync: None,
            tab: Tab::Overview,
            harness_filter: HarnessFilter::default(),
            herdr_mode: false,
            modals: Vec::new(),
            help_scroll: 0,
            help_max_scroll: std::cell::Cell::new(0),
            check_output_scroll: 0,
            check_output_max_scroll: std::cell::Cell::new(0),
            check_output_viewport: std::cell::Cell::new(0),
            help_viewport: std::cell::Cell::new(0),
            profile_cursor: 0,
            overview_selector: ListView::default(),
            overview_cursor: 0,
            usage_selector: ListView::default(),
            usage_detail_focus: StatusFocus::List,
            usage_detail_scroll: 0,
            usage_detail_max_scroll: std::cell::Cell::new(0),
            usage_detail_viewport: std::cell::Cell::new(0),
            setup_selector: ListView::default(),
            chain_selector: ListView::default(),
            models_selector: ListView::default(),
            setup_detail_offset: std::cell::Cell::new(0),
            setup_detail_viewport: std::cell::Cell::new(0),
            setup_detail_row_starts: std::cell::RefCell::new(Vec::new()),
            setup_detail_block_ends: std::cell::RefCell::new(Vec::new()),
            setup_detail_tips: std::cell::RefCell::new(Vec::new()),
            setup_detail_total: std::cell::Cell::new(0),
            fallback_detail_offset: std::cell::Cell::new(0),
            fallback_detail_viewport: std::cell::Cell::new(0),
            fallback_detail_row_starts: std::cell::RefCell::new(Vec::new()),
            fallback_detail_block_ends: std::cell::RefCell::new(Vec::new()),
            fallback_detail_tips: std::cell::RefCell::new(Vec::new()),
            fallback_detail_total: std::cell::Cell::new(0),
            global_config_offset: std::cell::Cell::new(0),
            global_config_viewport: std::cell::Cell::new(0),
            global_config_row_starts: std::cell::RefCell::new(Vec::new()),
            global_config_block_ends: std::cell::RefCell::new(Vec::new()),
            global_config_tips: std::cell::RefCell::new(Vec::new()),
            global_config_total: std::cell::Cell::new(0),
            config_focus: ConfigFocus::Profiles,
            config_action_cursor: 0,
            fallback_focus: FallbackFocus::Chain,
            fallback_detail_cursor: 0,
            fallback_edit: None,
            fallback_day_stop: None,
            global_config_cursor: 0,
            refresh_interval_draft: None,
            context_nudge_draft: None,
            weekly_threshold_draft: None,
            note_text: initial_note,
            note_editor: None,
            config_draft: None,
            chain_cursor: 0,
            toasts: VecDeque::new(),
            compact: false,
            update_results,
            update_handle,
            login: None,
            clipboard: crate::platform::copy_to_clipboard_osc52,
            login_generation: 0,
            login_event_rx,
            login_event_tx,
            login_result_rx,
            login_result_tx,
            status: StatusState::default(),
            status_events,
            status_refresh,
            daemon_health,
            gateway_state: crate::daemon::gateway_slot(daemon_health).0.state,
            gateway_unrun: false,
            left_behind_probe: ProbeWorker::new(crate::daemon::gateway::left_behind_gateway_runs),
            last_daemon_probe: Instant::now(),
            daemon_control_busy: false,
            daemon_control_rx,
            daemon_control_tx,
            fetch_lease: Arc::new(crate::daemon::FetchLease::new()),
            services: ServicesState::default(),
            token_stats: None,
            tokens_failed: false,
            tokens_topping_up: false,
            tokens_progress: None,
            token_view: TokenView::Dashboard,
            token_model_cursor: 0,
            model_detail_focus: StatusFocus::List,
            model_detail_scroll: 0,
            model_detail_max_scroll: std::cell::Cell::new(0),
            model_detail_viewport: std::cell::Cell::new(0),
            token_filter: TokenFilter::default(),
            token_period: TokenPeriod::default(),
            token_dash_focus: CardKind::First,
            token_dash_first: std::cell::Cell::new(0),
            token_dash_order: std::cell::Cell::new([
                CardKind::First,
                CardKind::Total,
                CardKind::Trend,
                CardKind::Models,
                CardKind::Comp,
                CardKind::Hour,
                CardKind::Activity,
            ]),
            token_dash_rows: std::cell::RefCell::new(Vec::new()),
            token_dash_viewport: std::cell::Cell::new(0),
            tokens_events,
            tokens_refresh,
            price_table: None,
            price_failed: false,
            pricing_events,
            pricing_refresh,
            last_reload_fp: reload_fingerprint(),
            unsaved_live_login: false,
            started_at: Instant::now(),
            #[cfg(test)]
            anim_phase_ms: None,
            day_claim_notices: Vec::new(),
            tick_count: 0,
            quit: false,
            armed_quit: false,
            footer_alert: None,
            banner: None,
            last_divergence_check: Some(Instant::now()),
            divergence_pending: None,
            last_services_refresh: Instant::now(),
            reconcile_done: false,
            bootstrap_started: false,
            refresh_interval,
            bootstrap_active: Arc::new(AtomicBool::new(false)),
            shutting_down: Arc::new(AtomicBool::new(false)),
            tab_activity: [None; Tab::ALL.len()],
            bell_fired: crate::bell::BellLatch::default(),
            history_cache,
            history_fp,
            wallet_cache,
            wallet_fp,
            session_tokens,
            live_count_width: live_count_text(live_sessions.total()).chars().count(),
            live_sessions,
            last_live_sessions_refresh: Some(Instant::now()),
            codex_rows: codex_rows(),
            last_codex_rows_refresh: Some(Instant::now()),
        };
        app.refresh_unsaved_live_login();
        app
    }

    /// The `ThirdPartyBroken` seed read off the durable per-credential verdict:
    /// one `(name, fingerprint)` per profile whose record matches the
    /// credential it holds right now (`fallback::third_party_key_rejected` —
    /// never a lapsed Alibaba console session, which stays usage-only). This
    /// is what keeps a fingerprint-bound key rejection visible after a TUI
    /// restart or in a stood-down TUI until a fetch recreates it; the
    /// intersection in [`App::key_rejected_names`] still drops the name the
    /// moment the credential changes.
    fn durable_key_rejected_seed(config: &AppConfig) -> HashMap<String, u64> {
        config
            .profiles
            .iter()
            .filter(|p| crate::fallback::third_party_key_rejected(p, &p.name))
            .filter_map(|p| {
                crate::usage::profile_credential_fingerprint(p).map(|fp| (p.name.to_string(), fp))
            })
            .collect()
    }

    /// The live key-rejected member set, read once per render (never per walk
    /// candidate): the broken fingerprint map intersected with the CURRENT
    /// config profiles (the same `profile_credential_fingerprint` the scheduler
    /// scans use), so a repair that landed in the config drops the name the
    /// moment the render reads it — never gated on the separately refreshed
    /// `ThirdPartyList`.
    pub(crate) fn key_rejected_names(&self) -> HashSet<ProfileName> {
        let broken = self
            .third_party_broken
            .lock()
            .map(|m| m.clone())
            .unwrap_or_default();
        let cfg = self.config();
        crate::usage::current_key_rejected_names(&broken, &cfg.profiles)
            .into_iter()
            .collect()
    }

    /// Landing, armed at construction. The FIRST herdr launch opens the
    /// Services tab and, once the herdr probe lands, puts the herdr selector row
    /// under the cursor with its detail pane descended; construction marks the
    /// landing done in `[herdr] first_landing_done` — once, forever. Every other
    /// launch — a plain TUI, and herdr after the first — opens the top-level
    /// `home_tab` (default overview); the herdr header tag is unaffected. The
    /// first landing's recompute starts the herdr probe on its worker, so the
    /// first paint never waits on its three subprocesses; the pending intent
    /// lands on the herdr row by label in the recompute that adopts the probe,
    /// or never. The `claude --version` probe stays `r`-gated: construction
    /// must not block the first paint on a spawn. A modal opening, an ↑↓
    /// cursor move, ↵, `f` on the list, or a tab switch clears the pending
    /// intent, so focus is never stolen once the user has acted.
    pub(crate) fn with_herdr_mode(mut self, herdr_mode: bool) -> Self {
        self.herdr_mode = herdr_mode;
        let first_landing = herdr_mode && !self.config().state.herdr.first_landing_done;
        if first_landing {
            self.tab = Tab::Services;
            // Land on the herdr row by label once its probe resolves.
            self.services.land_on_herdr = true;
            recompute_services_checks(&mut self, false, ShuntRefresh::Keep);
            {
                let mut cfg = self.config();
                cfg.state.herdr.first_landing_done = true;
                let _ = save_app_state(&cfg.state);
            }
            // Adopt the marker write like every other in-TUI save: without the
            // bump, the app's own profiles.toml rewrite reads as an external
            // change and re-runs a full config reload on the first tick.
            self.last_reload_fp = reload_fingerprint();
        } else {
            let home: Tab = self.config().state.home_tab().into();
            self.tab = home;
        }
        self
    }

    /// The production write path for the tally after construction: it also widens
    /// the header chip's held count width, so a count shrinking by a digit
    /// moves nothing right of the chip.
    pub(crate) fn set_live_sessions(&mut self, tally: crate::live_sessions::LiveTally) {
        self.live_count_width = self
            .live_count_width
            .max(live_count_text(tally.total()).chars().count());
        self.live_sessions = tally;
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
            let due_now: Vec<crate::usage::LegKey> = match h.last_fetched.lock() {
                Ok(lf) => snapshot
                    .iter()
                    .map(|e| FetchLeg::OAuth.key(e.name.clone()))
                    .chain(
                        third_party
                            .iter()
                            .map(|e| FetchLeg::ThirdParty.key(e.name.clone())),
                    )
                    .filter(|key| {
                        lf.get(key).is_none_or(|stamp| {
                            stamp.as_millis().saturating_add(interval_ms) <= now
                        })
                    })
                    .collect(),
                Err(_) => Vec::new(),
            };
            for key in &due_now {
                crate::usage::mark_fetch_activity(&h.activity, key, ProfileActivity::Queued);
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

    /// The in-memory burn rate for `profile`, or `None` when the profile is
    /// third-party-classified — a hybrid never pairs its OAuth burn history with
    /// its provider window (maintainer ruling 2026-09-10), so its windows are judged by the
    /// static threshold, never a cross-pool projection. The one guard the
    /// Overview ETA and the startup one-shot both read.
    pub(crate) fn burn_rate_for(&self, profile: &Profile, usage_info: &UsageInfo) -> Option<f64> {
        if profile.usage_cache_is_third_party() {
            return None;
        }
        self.active_burn_rate(&profile.name, usage_info)
    }

    /// The startup one-shot's burn rate for `profile`: its `Profile.usage`
    /// reading, or `None` when there is none. The one-shot reads its rate
    /// through this step, never `active_burn_rate` directly — a
    /// third-party-classified profile still refuses the OAuth burn pairing via
    /// [`Self::burn_rate_for`] (maintainer ruling 2026-09-10).
    fn startup_rate(&self, profile: &Profile) -> Option<f64> {
        let usage = profile.usage.as_ref()?;
        self.burn_rate_for(profile, usage)
    }

    /// The funded wallet's burn rate for `profile`, computed from the
    /// in-memory `wallet_cache` — never touches disk. The wallet twin of
    /// [`Self::active_burn_rate`]: the Usage tab's balance row and the
    /// overview drains line read it here, so neither render pass reads
    /// `wallet_history.jsonl` while holding the config guard.
    pub(crate) fn wallet_rate_for(&self, profile: &Profile) -> Option<crate::usage::WalletRate> {
        let stats = profile.third_party_usage.as_ref()?;
        crate::usage::funded_wallet_rate(
            self.wallet_cache
                .get(profile.name.as_str())
                .map(|v| v.as_slice())
                .unwrap_or(&[]),
            &stats.rows,
        )
    }

    /// The profile's peak-rate state off the live price table, sampled now:
    /// the recognized provider's own store rows — peak/off-peak is a property
    /// of the provider, never of the pinned models. `None` when no table is
    /// loaded or the profile has no store-backed provider (OAuth, generic
    /// endpoints, OpenRouter) — the Usage tab's `pricing` row and the
    /// overview's `▲` marker then do not render.
    pub(crate) fn peak_state_for(&self, profile: &Profile) -> Option<crate::pricing::PeakState> {
        self.price_table
            .as_ref()
            .and_then(|t| t.peak_state_for_profile(profile.provider, now_ms() as i64 / 1000))
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
        // Run the startup one-shot on Fresh data only. A Cached seed's numbers
        // are unverified — stale in either direction — so switching on them
        // risks acting on a window the account no longer has. Stale profiles
        // are due on the scheduler's first tick, which fetches then
        // auto-switches off the corrected numbers.
        let switched = {
            let cfg = self.config();
            let active_profile = cfg.state.active_profile.as_ref().and_then(|n| cfg.find(n));
            let active_fresh =
                active_profile.is_some_and(|p| p.fetch_status == Some(FetchStatus::Fresh));
            if active_fresh {
                // In-memory rate only (`history_cache`) — never a disk read
                // while the config guard is held; see `active_burn_rate`.
                let rate = active_profile.and_then(|p| self.startup_rate(p));
                drop(cfg);
                auto_switch_if_needed(&self.config, rate).ok().flatten()
            } else {
                None
            }
        };
        match switched {
            Some(SwitchAction::To(target)) => {
                // Destination-based, as in the pending-switch drain: landing on
                // the home account reads as a return whether the return pass or
                // an exhaustion walk onto a clear preferred put us there.
                let key_rejected = self.key_rejected_names();
                let returned = self
                    .config()
                    .is_home_today(&ProfileName::from(target.clone()), &key_rejected);
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
        // Session-scoped suppressed-auth-expired set: rebuilt fresh each TUI launch,
        // dropped on exit. Purely scheduler-internal — the App never touches it
        // (manual refresh clears suppression via the shared forced queue).
        let suppressed_auth_expired: SuppressedAuthExpiredStore =
            Arc::new(RankedMutex::new(HashMap::new()));
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
            h.auto_start_queue,
            h.pending_switch,
            h.pending_switch_off,
            h.refetch_queue,
            h.third_party_tokens,
            h.third_party_usage_store,
            h.third_party_status,
            h.third_party_streaks,
            h.third_party_broken,
            suppressed_auth_expired,
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
        let wallet_names;
        {
            let third_party_map = self.third_party_usage_store.lock().ok();
            let third_party_status_map = self.third_party_status.lock().ok();
            let info_map = self.usage_store.lock().ok();
            let status_map = self.usage_status.lock().ok();
            let mut cfg = self.config();
            let now = now_ms();
            let interval_ms = cfg.state.refresh_interval_ms;
            let refresh_spent_accounts = cfg.state.refresh_spent_accounts;
            for p in &mut cfg.profiles {
                // The TUI's per-account reading is the picker's whole record,
                // the same dichotomy the walk's picker and the disk readers
                // key on (`usage_cache_is_third_party`): a third-party account
                // (pure or hybrid) takes its provider stats' `to_usage_info()`
                // — never the OAuth leg's entry, so a stale OAuth reading under
                // a logged-out hybrid's name cannot survive, and no OAuth plan
                // or spend reaches a hybrid's `Profile.usage`. Assign only when
                // the store the classification needs is present — a poisoned
                // lock keeps the prior reading.
                let side_ok = if p.usage_cache_is_third_party() {
                    third_party_map.is_some()
                } else {
                    info_map.is_some()
                };
                if side_ok {
                    p.usage = crate::fallback::picked_usage(
                        p.usage_cache_is_third_party(),
                        info_map.as_ref().and_then(|s| s.get(p.name.as_str())),
                        third_party_map
                            .as_ref()
                            .and_then(|s| s.get(p.name.as_str())),
                    );
                }
                // Fetch_status follows the picker's side too, so the TUI's
                // freshness (the startup one-shot gate and the status pill)
                // agrees with the scheduler's `decision_fresh_any`: a
                // third-party-classified account reads its third-party status;
                // every other account is OAuth-first with the third-party
                // fallback.
                if p.usage_cache_is_third_party() {
                    if let Some(s) = third_party_status_map.as_ref() {
                        p.fetch_status = s.get(p.name.as_str()).copied();
                    }
                } else if let Some(s) = status_map.as_ref()
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

                // #74 degraded cue: cache age past the derived threshold reads
                // stale, independent of fetch_status. Same threshold, same
                // maxed-window exemption, and same age source as
                // `status.json`'s `age_stale` arm — the exemption reads the
                // DISK cache (`load_profile_cache`), never the live store,
                // because the two can diverge on a spent account the
                // scheduler dropped from its due set: reading the store there
                // would publish the exact disagreement the exemption exists
                // to prevent. OAuth goes through the one age contract
                // (`oauth_age`), the same one `status.json` and the MCP payloads
                // read; third-party figures keep the cache mtime.
                let oauth_usage = if p.usage_cache_is_third_party() {
                    None
                } else {
                    load_profile_cache::<UsageInfo>(&p.name, USAGE_CACHE_FILE)
                };
                let spent_skipped = !refresh_spent_accounts
                    && oauth_usage
                        .as_ref()
                        .is_some_and(|u| windows_maxed(u, (now / 1000) as i64));
                let past_threshold = if p.usage_cache_is_third_party() {
                    profile_cache_mtime_ms(&p.name, usage_cache_file(p))
                        .is_some_and(|at| now.saturating_sub(at) > stale_after_ms(interval_ms))
                } else {
                    crate::profile_json::oauth_age(oauth_usage.as_ref(), now).is_stale(
                        stale_after_ms(interval_ms),
                        oauth_usage
                            .as_ref()
                            .is_some_and(crate::profile_json::publishes_a_live_window),
                    )
                };
                p.usage_stale = !spent_skipped && past_threshold;
            }

            bells = cfg
                .profiles
                .iter()
                .map(|p| {
                    // The same reading the daemon's bell judges: a lapsed 5h
                    // window is no figure, and an aged one is not fresh.
                    let util = p
                        .usage
                        .as_ref()
                        .and_then(|u| u.five_hour.as_ref())
                        .filter(|w| crate::profile_json::window_row_is_live(w))
                        .map(|u| u.utilization);
                    let fresh = p.fetch_status == Some(FetchStatus::Fresh) && !p.usage_stale;
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
            // Only third-party profiles carry a wallet series, so the refresh
            // walk below stats no OAuth profile's absent file.
            wallet_names = cfg
                .profiles
                .iter()
                .filter(|p| p.usage_cache_is_third_party())
                .map(|p| p.name.to_string())
                .collect::<Vec<_>>();
        }
        for (name, threshold, util, fresh) in bells {
            if let Some(message) = self.bell_fired.observe(&name, threshold, util, fresh) {
                self.toast(ToastKind::Warning, message);
                self.set_tab_activity(Tab::Overview, ToastKind::Warning);
            }
        }

        // Re-read any history log that changed on disk. The file is written by
        // whichever process holds the fetch lease — this one or a headless
        // daemon — so its CONTENT is the only reliable signal that new samples
        // landed: NTFS quantizes file times on windows and a rapid append can
        // land with a byte-identical mtime (measured 2026-09-13: 4/10 real-box
        // runs), so an mtime gate silently serves a stale series there.
        for name in &history_names {
            if let Ok(path) = crate::profile::profile_history_path(&ProfileName::from(name.clone()))
                && let Some(fp) = series_fingerprint(&path)
                && self.history_fp.get(name) != Some(&fp)
            {
                self.history_cache.insert(
                    name.clone(),
                    crate::profile::load_usage_history(&ProfileName::from(name.clone())),
                );
                self.history_fp.insert(name.clone(), fp);
            }
        }
        // The wallet series re-reads the same way.
        for name in &wallet_names {
            if let Ok(path) =
                crate::profile::profile_wallet_history_path(&ProfileName::from(name.clone()))
                && let Some(fp) = series_fingerprint(&path)
                && self.wallet_fp.get(name) != Some(&fp)
            {
                self.wallet_cache.insert(
                    name.clone(),
                    crate::profile::load_wallet_history(&ProfileName::from(name.clone())),
                );
                self.wallet_fp.insert(name.clone(), fp);
            }
        }
    }

    /// Refresh `unsaved_live_login` off the live credentials + current config.
    /// Flips come from the live file moving (a login minted elsewhere), the
    /// profile set changing, or a switch swapping the live file — every site
    /// that can produce one calls this (see the field's list).
    pub(crate) fn refresh_unsaved_live_login(&mut self) {
        self.unsaved_live_login = capture_snapshot().is_ok_and(|snap| {
            !snapshot_is_empty(&snap)
                && find_matching_oauth_profile(&self.config(), snap.credentials.as_ref()).is_none()
        });
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
            let (tokens, third_party, names, broken_seed) = {
                let cfg = self.config();
                let tokens = collect_tokens(&cfg);
                let third_party = collect_third_party_entries(&cfg.profiles);
                let names: Vec<String> = cfg.profiles.iter().map(|p| p.name.to_string()).collect();
                let broken_seed = Self::durable_key_rejected_seed(&cfg);
                (tokens, third_party, names, broken_seed)
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
                // Union-add the durable seed: it can only ADD names whose record
                // matches the CURRENT fingerprint, and a re-keyed name still
                // drops at the intersection in `key_rejected_names`. A recovered
                // name can flicker back in for at most one sync interval — the
                // fetcher's error arm updates the live map before it clears the
                // durable record, so a sync landing in that gap re-reads the
                // still-standing record — and the next `sync_broken_verdicts`
                // pass drops it again.
                if let Ok(mut broken) = self.third_party_broken.lock() {
                    broken.extend(broken_seed);
                }
            }
            self.session_tokens = collect_session_tokens(&names);
            self.refresh_unsaved_live_login();
            repin_member_edit(self);
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
        // Light the selected cache leg immediately. Account-scoped refresh/switch
        // work stays untouched and outranks this marker in the render helper.
        let leg = {
            let config = self.config();
            config.find(name).map(crate::usage::FetchLeg::for_profile)
        };
        if let Some(leg) = leg {
            crate::usage::mark_fetch_activity(
                &self.activity,
                &leg.key(name.clone()),
                ProfileActivity::Queued,
            );
        }
        if refresh_plan {
            crate::usage::expire_profile_ttl(name);
        }
        if let Ok(mut q) = self.refetch_queue.lock() {
            q.insert(name.to_string());
        }
    }

    /// Every modal opens here. Opening one drops a still-pending first herdr
    /// landing: the landing happens on the tick its probe drains or never, so
    /// it can neither move the view behind a modal nor after the user's own
    /// action closed one.
    pub(crate) fn open_modal(&mut self, modal: Modal) {
        self.services.land_on_herdr = false;
        self.modals.push(modal);
    }

    /// Every write of [`App::gateway_state`] after construction lands here.
    /// No daemon (never a stale one, whose supervisor still runs) under the
    /// record-only `unobserved` slot means the gateway would run and nothing
    /// will start it: entering that state starts the left-behind probe, whose
    /// landing toasts once ([`drain_left_behind_probe`]).
    pub(crate) fn set_gateway_state(&mut self, state: GatewayState) {
        self.gateway_state = state;
        let unrun = self.daemon_health == crate::daemon::DaemonHealth::Absent
            && state == GatewayState::Unobserved;
        if unrun && !self.gateway_unrun {
            self.left_behind_probe.restart();
        }
        self.gateway_unrun = unrun;
    }

    pub(crate) fn toast(&mut self, kind: ToastKind, body: impl Into<String>) {
        if self.toasts.len() >= TOAST_CAPACITY {
            self.toasts.pop_front();
        }
        self.toasts.push_back(Toast {
            kind,
            body: prose::mark_commands(&body.into()),
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
        self.toasts.retain(|toast| {
            let ttl = if toast.kind == ToastKind::Danger {
                TOAST_TTL_DANGER
            } else {
                TOAST_TTL_NORMAL
            };
            toast.born.elapsed() < ttl
        });
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
        let mut rows = Vec::new();
        if self.harness_filter.shows_claude() {
            rows.extend((0..self.profile_count()).map(MainItemKind::Profile));
        }
        if self.harness_filter.shows_codex() {
            rows.extend((0..self.codex_rows.len()).map(MainItemKind::Codex));
        }
        rows
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
        self.clamp_overview_cursor();
        // Every reload path lands here (a roster change can move or remove the
        // selected account), so the note tracks the selection for free.
        self.reload_selected_note();
    }

    /// Re-read the selected account's note from disk.
    pub(crate) fn reload_selected_note(&mut self) {
        self.note_text = self
            .profile_name_at(self.profile_cursor)
            .and_then(|name| crate::profile_notes::load_note(&name));
    }

    pub(crate) fn current_main_item(&self) -> Option<MainItemKind> {
        self.main_items().get(self.overview_cursor).copied()
    }

    fn clamp_overview_cursor(&mut self) {
        self.overview_cursor = self
            .overview_cursor
            .min(self.main_items().len().saturating_sub(1));
    }

    fn sync_overview_profile_selection(&mut self) {
        if let Some(MainItemKind::Profile(idx)) = self.current_main_item() {
            self.profile_cursor = idx;
            self.reload_selected_note();
        }
    }
}

// ── Token snapshot ────────────────────────────────────────────────────────────

// ── Startup reconciliation ────────────────────────────────────────────────────

/// Startup credential reconciliation, non-blocking. Judges the live
/// `~/.claude/.credentials.json` through the SAME cascade the 1Hz poll uses
/// ([`reconcile_decision`]): against the install source, never the
/// clauth-private usage pair — a refreshless live login (a rolling bearer, a
/// static mint) is no divergence (#104). No divergence → snapshot +
/// `ReconcileDone` immediately.
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

    let verdict = reconcile_decision(app, &active);

    if matches!(verdict, DivergenceVerdict::Clean) {
        let mut cfg = app.config();
        let _ = snapshot_active_credentials(&mut cfg);
    }
    let _ = app
        .startup_sender
        .send(if matches!(verdict, DivergenceVerdict::Clean) {
            StartupSignal::ReconcileDone
        } else {
            StartupSignal::ReconcileNeedsPrompt {
                active: active.to_string(),
            }
        });
}

// ── Event handling ────────────────────────────────────────────────────────────

/// True while `app.tab`'s descend/ascend sub-focus screen is active (Setup's
/// Actions pane, Fallback's Detail pane, Status/Services' Detail pane, Tokens'
/// Models view) — the state where `q`/`esc` ascend instead of arming quit /
/// no-op. Single source of truth shared by the `q` handler and the footer's
/// `q back` / `q quit` label; the help-modal esc-row test iterates it too, so
/// a new sub-focus tab without an `esc` help row fails CI.
pub(crate) fn has_sub_focus(app: &App) -> bool {
    (app.tab == Tab::Usage && app.usage_detail_focus == StatusFocus::Detail)
        || (app.tab == Tab::Setup && app.config_focus == ConfigFocus::Actions)
        || (app.tab == Tab::Fallback && app.fallback_focus == FallbackFocus::Detail)
        || (app.tab == Tab::Status && app.status.focus == StatusFocus::Detail)
        || (app.tab == Tab::Services && app.services.focus == ServicesFocus::Detail)
        || (app.tab == Tab::Tokens && app.token_view == TokenView::Models)
}

/// True while focus is descended into the delegates detail: `←` ascends and
/// `→` is inert there, the master-detail arrow-trap, instead of switching tabs.
fn services_delegates_descended(app: &App) -> bool {
    app.tab == Tab::Services
        && app.services.focus == ServicesFocus::Detail
        && app
            .services
            .selected_check()
            .is_some_and(|c| c.label == "delegates")
}

/// What owns the keyboard this frame: every key it claims ([`KeyOwner::claims`]),
/// `←`/`→` included, reaches it before any tab switch or global binding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum KeyOwner {
    /// The modal stack: its top modal takes every key.
    Modal,
    /// A Setup detail row's inline field on an existing account.
    SetupField,
    /// A Setup detail row's inline field on the `+ new` form, where esc keeps
    /// the typed value (`cancel_config_edit` only reverts a saved account).
    NewAccountField,
    /// A typed field on the Fallback member card: `rotate at`, `weekly at`
    /// or `max spend`.
    MemberField,
    /// The Fallback card's `preferred days` chip picker.
    DayPicker,
    /// The Config tab's `refresh` custom-value field.
    RefreshInterval,
    /// The Config tab's `context nudge` custom-value field.
    ContextNudge,
    /// The Config tab's `weekly limit` custom-value field.
    WeeklyThreshold,
    /// The Services tab's herdr tag-refresh field.
    HerdrTag,
    /// The shunt card's open config field editor (a typed field or a pending
    /// cycle pick).
    ShuntField,
    /// The usage tab's note editor (the `n`-opened draft on the selected
    /// account).
    NoteEditor,
}

impl KeyOwner {
    /// Whether this owner takes `code`, or lets it through to the global keys.
    /// The chip picker binds neither `?` nor `x`, so help and toast dismissal
    /// work mid-pick; a typed field takes both, since they are data there. A
    /// shunt editor whose row is saving is locked — only the editing keys are
    /// inert, so `?` (help) and `x` (dismiss a toast) fall through while `↑↓`
    /// and `q` (back) stay claimed and act in the editor's own handler (`q`
    /// closes the editor and ascends, so a locked editor never outlives the
    /// detail focus).
    pub(crate) fn claims(self, app: &App, code: KeyCode) -> bool {
        if self == Self::DayPicker && matches!(code, KeyCode::Char('?' | 'x')) {
            return false;
        }
        if self == Self::ShuntField
            && app
                .services
                .shunt_field_edit
                .as_ref()
                .is_some_and(|state| app.services.saving(&ShuntSaveKey::Field(state.key.clone())))
            && matches!(code, KeyCode::Char('?' | 'x'))
        {
            return false;
        }
        true
    }
}

/// What owns the keyboard this frame, or `None` when keys take their tab and
/// global senses. The one answer both [`handle_key`] routes by and the footer
/// draws from, so the two cannot disagree about who holds `←`/`→`.
pub(crate) fn keyboard_owner(app: &App) -> Option<KeyOwner> {
    if !app.modals.is_empty() {
        return Some(KeyOwner::Modal);
    }
    match app.tab {
        Tab::Setup => {
            let draft = app
                .config_draft
                .as_ref()
                .filter(|d| app.config_focus == ConfigFocus::Actions && d.active.is_some())?;
            Some(match draft.editing_name {
                Some(_) => KeyOwner::SetupField,
                None => KeyOwner::NewAccountField,
            })
        }
        Tab::Fallback if app.fallback_focus == FallbackFocus::Detail => {
            match app.fallback_edit.as_ref().map(|e| &e.state) {
                Some(CardEdit::Threshold(_) | CardEdit::Weekly(_) | CardEdit::MaxSpend(_)) => {
                    Some(KeyOwner::MemberField)
                }
                Some(CardEdit::Days(_)) => Some(KeyOwner::DayPicker),
                Some(CardEdit::ArmedRemove) | None => None,
            }
        }
        Tab::Config if app.refresh_interval_draft.is_some() => Some(KeyOwner::RefreshInterval),
        Tab::Config if app.context_nudge_draft.is_some() => Some(KeyOwner::ContextNudge),
        Tab::Config if app.weekly_threshold_draft.is_some() => Some(KeyOwner::WeeklyThreshold),
        Tab::Services if app.services.herdr_tag_draft.is_some() => Some(KeyOwner::HerdrTag),
        Tab::Services if app.services.shunt_field_edit.is_some() => Some(KeyOwner::ShuntField),
        Tab::Usage if app.note_editor.is_some() => Some(KeyOwner::NoteEditor),
        Tab::Overview
        | Tab::Usage
        | Tab::Tokens
        | Tab::Fallback
        | Tab::Config
        | Tab::Status
        | Tab::Services => None,
    }
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

    // A modal, then an open editor, owns the keyboard: typed digits and
    // letters can't trip global shortcuts, and ←/→ never switch the tab. A key
    // the owner does not claim falls through to the global senses below.
    if let Some(owner) = keyboard_owner(app).filter(|owner| owner.claims(app, key.code)) {
        match owner {
            KeyOwner::Modal => handle_modal_key(app, key),
            KeyOwner::SetupField | KeyOwner::NewAccountField => handle_config_edit_key(app, key),
            KeyOwner::MemberField => handle_member_field_key(app, key),
            KeyOwner::DayPicker => handle_day_picker_key(app, key),
            KeyOwner::RefreshInterval => handle_refresh_interval_edit_key(app, key),
            KeyOwner::ContextNudge => handle_context_nudge_edit_key(app, key),
            KeyOwner::WeeklyThreshold => handle_weekly_threshold_edit_key(app, key),
            KeyOwner::HerdrTag => handle_herdr_tag_edit_key(app, key),
            KeyOwner::ShuntField => handle_shunt_field_edit_key(app, key),
            KeyOwner::NoteEditor => handle_note_editor_key(app, key),
        }
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
        KeyCode::Left if app.tab == Tab::Usage && app.usage_detail_focus == StatusFocus::Detail => {
            app.usage_detail_focus = StatusFocus::List;
            return;
        }
        KeyCode::Left
            if app.tab == Tab::Tokens
                && app.token_view == TokenView::Models
                && app.model_detail_focus == StatusFocus::Detail =>
        {
            app.model_detail_focus = StatusFocus::List;
            return;
        }
        KeyCode::Left if services_delegates_descended(app) => {
            leave_shunt_detail(app);
            return;
        }
        KeyCode::Right
            if (app.tab == Tab::Usage && app.usage_detail_focus == StatusFocus::Detail)
                || (app.tab == Tab::Tokens
                    && app.token_view == TokenView::Models
                    && app.model_detail_focus == StatusFocus::Detail) =>
        {
            return;
        }
        KeyCode::Right if services_delegates_descended(app) => {
            return;
        }
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
            app.open_modal(Modal::Help);
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
        // Overview only: it is the one screen listing accounts, so the filter
        // has nothing to mean anywhere else. Guarded rather than swallowed so
        // `c` still reaches the per-tab dispatch (Tokens binds it too).
        KeyCode::Char('c') if app.tab == Tab::Overview => {
            app.disarm_quit();
            let selected = app.current_main_item();
            app.harness_filter = app.harness_filter.next();
            app.overview_cursor = selected
                .and_then(|item| app.main_items().iter().position(|row| *row == item))
                .unwrap_or(0);
            app.sync_overview_profile_selection();
            app.overview_selector.offset.set(0);
            return;
        }
        KeyCode::Char('a') => {
            app.disarm_quit();
            let state = build_action_menu(app);
            if !state.items.is_empty() {
                app.open_modal(Modal::ActionMenu(state));
            }
            return;
        }
        KeyCode::Char('r') => {
            app.disarm_quit();
            if app.tab == Tab::Status {
                trigger_status_refresh(app);
                return;
            }
            // Service checks re-run synchronously; `r` also re-probes `claude --version`.
            if app.tab == Tab::Services {
                recompute_services_checks(app, true, ShuntRefresh::Keep);
                app.toast(ToastKind::Info, "re-running service checks");
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
            app.open_modal(Modal::Confirm(ConfirmState {
                message: ROTATE_ALL_MSG.to_string(),
                detail: Some(ROTATE_ALL_DETAIL.to_string()),
                choice: false,
                scroll: ConfirmScroll::default(),
                on_confirm: ConfirmAction::RotateAll,
            }));
            return;
        }
        KeyCode::Char('n') => {
            app.disarm_quit();
            // The usage tab claims `n` for the account's note editor (footer +
            // help modal advertise it there); the empty roster keeps the empty
            // state's `n to create one` promise. Everywhere else: new account.
            if app.tab == Tab::Usage && app.profile_count() > 0 {
                open_note_editor(app);
            } else {
                start_new_account(app);
            }
            return;
        }
        // Esc backs out of sub-focus; no-op at the top level.
        KeyCode::Esc => {
            app.disarm_quit();
            if app.tab == Tab::Usage && app.usage_detail_focus == StatusFocus::Detail {
                app.usage_detail_focus = StatusFocus::List;
            } else if app.tab == Tab::Tokens
                && app.token_view == TokenView::Models
                && app.model_detail_focus == StatusFocus::Detail
            {
                app.model_detail_focus = StatusFocus::List;
            } else if app.tab == Tab::Setup && app.config_focus == ConfigFocus::Actions {
                leave_config_detail(app);
            } else if app.tab == Tab::Fallback && app.fallback_focus == FallbackFocus::Detail {
                leave_fallback_detail(app);
            } else if app.tab == Tab::Status && app.status.focus == StatusFocus::Detail {
                app.status.focus = StatusFocus::List;
            } else if app.tab == Tab::Services && app.services.focus == ServicesFocus::Detail {
                if !shunt_esc_folds_entry(app) {
                    leave_shunt_detail(app);
                }
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
                if app.tab == Tab::Usage {
                    app.usage_detail_focus = StatusFocus::List;
                } else if app.tab == Tab::Tokens && app.model_detail_focus == StatusFocus::Detail {
                    app.model_detail_focus = StatusFocus::List;
                } else if app.tab == Tab::Setup {
                    leave_config_detail(app);
                } else if app.tab == Tab::Fallback {
                    leave_fallback_detail(app);
                } else if app.tab == Tab::Tokens {
                    app.token_view = TokenView::Dashboard;
                } else if app.tab == Tab::Services {
                    leave_shunt_detail(app);
                } else {
                    app.status.focus = StatusFocus::List;
                }
            } else if app.armed_quit {
                app.quit = true;
                app.shutting_down.store(true, Ordering::SeqCst);
            } else {
                app.armed_quit = true;
                app.footer_alert = Some(FooterAlert::Warn(format!(
                    "press {} again to quit",
                    prose::key("q")
                )));
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
        Tab::Services => handle_services_key(app, key),
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
pub(crate) fn token_model_count(app: &App) -> usize {
    token_period_models(app).len()
}

/// Keep detail focus on a model only while the active list can select one.
fn clamp_token_model_cursor(app: &mut App) {
    let len = token_model_count(app);
    if len == 0 {
        app.model_detail_focus = StatusFocus::List;
        app.model_detail_scroll = 0;
    }
    if app.token_model_cursor >= len {
        app.token_model_cursor = len.saturating_sub(1);
    }
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

/// Tokens tab: `c` toggles cache-counting on either view; the Dashboard moves
/// card focus with ↑/↓ (wrapping) and the page keys (stopping at the ends), and
/// ⏎ descends into Models. From Models, ⏎ descends into the read-only detail.
fn handle_tokens_key(app: &mut App, key: KeyEvent) {
    if key.code == KeyCode::Char('c') {
        toggle_count_cache(app);
        return;
    }
    match app.token_view {
        TokenView::Dashboard => match key.code {
            KeyCode::Enter if token_model_count(app) > 0 => {
                app.token_view = TokenView::Models;
                app.model_detail_focus = StatusFocus::List;
                app.model_detail_scroll = 0;
                app.token_model_cursor = 0;
            }
            KeyCode::Up => dash_move_focus(app, DashDir::Prev),
            KeyCode::Down => dash_move_focus(app, DashDir::Next),
            KeyCode::PageUp => dash_move_focus(app, DashDir::PageUp),
            KeyCode::PageDown => dash_move_focus(app, DashDir::PageDown),
            _ => {}
        },
        TokenView::Models => {
            if app.model_detail_focus == StatusFocus::Detail {
                let max = app.model_detail_max_scroll.get();
                let viewport = app.model_detail_viewport.get();
                match key.code {
                    KeyCode::Up => {
                        app.model_detail_scroll = app.model_detail_scroll.min(max).saturating_sub(1)
                    }
                    KeyCode::Down => {
                        app.model_detail_scroll =
                            app.model_detail_scroll.min(max).saturating_add(1).min(max)
                    }
                    KeyCode::PageUp => {
                        app.model_detail_scroll = page_up_scroll(
                            app.model_detail_scroll as usize,
                            max as usize,
                            viewport as usize,
                        ) as u16
                    }
                    KeyCode::PageDown => {
                        app.model_detail_scroll = page_down_scroll(
                            app.model_detail_scroll as usize,
                            max as usize,
                            viewport as usize,
                        ) as u16
                    }
                    _ => {}
                }
            } else {
                let len = token_model_count(app);
                match key.code {
                    KeyCode::Up if len > 0 => {
                        app.token_model_cursor = (app.token_model_cursor + len - 1).rem_euclid(len);
                        app.model_detail_scroll = 0;
                    }
                    KeyCode::Down if len > 0 => {
                        app.token_model_cursor = (app.token_model_cursor + 1).rem_euclid(len);
                        app.model_detail_scroll = 0;
                    }
                    KeyCode::PageUp if len > 0 => {
                        app.token_model_cursor =
                            selector_page_up(app.token_model_cursor, &app.models_selector);
                        app.model_detail_scroll = 0;
                    }
                    KeyCode::PageDown if len > 0 => {
                        app.token_model_cursor =
                            selector_page_down(app.token_model_cursor, &app.models_selector);
                        app.model_detail_scroll = 0;
                    }
                    KeyCode::Enter if len > 0 => {
                        app.model_detail_focus = StatusFocus::Detail;
                        app.model_detail_scroll = 0;
                    }
                    _ => {}
                }
            }
        }
    }
}

/// Direction a dashboard key moves card focus.
enum DashDir {
    Prev,
    Next,
    PageUp,
    PageDown,
}

/// Move dashboard card focus one step (or one page) and reconcile the
/// first-visible card, both through [`follow_dash_focus`].
fn dash_move_focus(app: &mut App, dir: DashDir) {
    let (new_focus, new_first) = {
        let order = app.token_dash_order.get();
        let rows = app.token_dash_rows.borrow();
        if rows.len() != 7 {
            return;
        }
        let viewport = app.token_dash_viewport.get();
        let Some(pos) = order.iter().position(|&k| k == app.token_dash_focus) else {
            return;
        };
        let starts: Vec<usize> = rows.iter().map(|&(top, _)| top as usize).collect();
        let next = match dir {
            DashDir::Prev => (pos + 6) % 7,
            DashDir::Next => (pos + 1) % 7,
            DashDir::PageUp => page_land_up(pos, &starts, viewport as usize),
            DashDir::PageDown => page_land_down(pos, &starts, viewport as usize),
        }
        .min(6);
        let focus = order[next];
        let first = follow_dash_focus(&order, &rows, viewport, focus, app.token_dash_first.get());
        (focus, first)
    };
    app.token_dash_focus = new_focus;
    app.token_dash_first.set(new_first);
}

/// Reconcile the persisted first-visible card against the laid-out geometry and
/// the focused card: it moves only when focus leaves the view. Pure — the key
/// path and the render pass both call it.
pub(crate) fn follow_dash_focus(
    order: &[CardKind; 7],
    rows: &[(u16, u16)],
    viewport: u16,
    focus: CardKind,
    first: usize,
) -> usize {
    if rows.is_empty() || viewport == 0 {
        return 0;
    }
    let last = rows.len() - 1;
    if rows[last].1 <= rows[0].0 + viewport {
        return 0; // grid fits: no scroll
    }
    let Some(mut focus_idx) = order.iter().position(|&k| k == focus) else {
        return first.min(last);
    };
    focus_idx = focus_idx.min(last);
    let mut first = dash_row_start(rows, first.min(last));
    let view_top = rows[first].0;
    let view_bottom = view_top + viewport;
    let (f_top, f_bottom) = rows[focus_idx];
    if f_top < view_top {
        first = dash_row_start(rows, focus_idx);
    } else if f_bottom > view_bottom {
        // Scroll down the minimum that reveals the focused card's bottom.
        let want = f_bottom.saturating_sub(viewport);
        let k = (0..=focus_idx)
            .find(|&k| rows[k].0 >= want)
            .unwrap_or(focus_idx);
        first = dash_row_start(rows, k);
    }
    first
}

/// Clamp the first-visible card after a resize: the view never starts past the
/// last card top that still fills the body (no blank tail while cards above
/// stay hidden), bounded so the focused card's bottom stays in view. Render-only.
pub(crate) fn clamp_dash_first(
    order: &[CardKind; 7],
    rows: &[(u16, u16)],
    viewport: u16,
    focus: CardKind,
    first: usize,
) -> usize {
    if rows.is_empty() || viewport == 0 {
        return 0;
    }
    let last = rows.len() - 1;
    if rows[last].1 <= rows[0].0 + viewport {
        return 0; // grid fits
    }
    let Some(mut focus_idx) = order.iter().position(|&k| k == focus) else {
        return first.min(last);
    };
    focus_idx = focus_idx.min(last);
    let first = dash_row_start(rows, first.min(last));
    if rows[last].1 >= rows[first].0 + viewport {
        return first; // already fills the body
    }
    let max_top = rows[last].1 - viewport;
    let focus_floor = rows[focus_idx].1.saturating_sub(viewport);
    let mut k = first;
    while k > 0 && rows[k].0 > max_top {
        k -= 1;
    }
    if rows[k].0 < focus_floor {
        while k < last && rows[k].0 < focus_floor {
            k += 1;
        }
    }
    dash_row_start(rows, k)
}

/// The first card of `i`'s visual row (side-by-side cards share a top).
fn dash_row_start(rows: &[(u16, u16)], mut i: usize) -> usize {
    while i > 0 && rows[i - 1].0 == rows[i].0 {
        i -= 1;
    }
    i
}

/// Switch the active tab and reset that tab's cursor to a sensible default.
fn switch_tab(app: &mut App, tab: Tab) {
    if tab == Tab::Overview && app.tab != Tab::Overview {
        app.overview_cursor = app.profile_cursor;
    }
    app.tab = tab;
    app.tab_activity[tab.index()] = None;
    app.config_draft = None;
    // Leaving the Services tab cancels a still-pending herdr landing: the user
    // has moved on, so a later recompute must not yank the cursor back.
    app.services.land_on_herdr = false;
    // Clamp cursor: a Config `+ new` selection must land on a real account.
    app.clamp_profile_cursor();
    match tab {
        Tab::Overview => {}
        Tab::Usage => {
            app.usage_detail_focus = StatusFocus::List;
            app.usage_detail_scroll = 0;
        }
        Tab::Tokens => {
            // Land on the dashboard; keep the model cursor reset for descend.
            app.token_view = TokenView::Dashboard;
            app.model_detail_focus = StatusFocus::List;
            app.model_detail_scroll = 0;
            app.token_model_cursor = 0;
        }
        Tab::Setup => {
            app.config_focus = ConfigFocus::Profiles;
            app.config_action_cursor = 0;
            // The `+ new` form's capture row renders off this flag, and a
            // login may have been minted (or saved) elsewhere since the last
            // Setup visit.
            app.refresh_unsaved_live_login();
        }
        Tab::Fallback => {
            app.chain_cursor = chain_cursor_for_profile(app);
            sync_profile_from_chain(app);
            leave_fallback_detail(app);
        }
        Tab::Config => {
            app.global_config_cursor = 0;
            app.refresh_interval_draft = None;
            app.context_nudge_draft = None;
            app.weekly_threshold_draft = None;
        }
        Tab::Status => {
            // Keep the incident cursor; reset focus to the list per the contract.
            app.status.focus = StatusFocus::List;
        }
        Tab::Services => {
            leave_shunt_detail(app);
            app.services.cursor = 0;
            app.services.detail_scroll = 0;
            // Entering the tab clears an open tag editor the same way the
            // Config tab clears its drafts; `switch_tab` matches the
            // destination, so the clear runs on entry, not on exit.
            app.services.herdr_tag_draft = None;
            // Recompute on focus; the cached `claude --version` is not
            // re-probed, but the shunt plan and pool read re-run so a card
            // left stale by an action elsewhere reads fresh on re-entry.
            recompute_services_checks(app, false, ShuntRefresh::Restart);
        }
    }
}

/// Move `profile_cursor` by `delta`, wrapping in `0..len`.
fn step_profile_cursor(app: &mut App, delta: i32, len: usize) {
    if len == 0 {
        return;
    }
    app.profile_cursor = (app.profile_cursor as i32 + delta).rem_euclid(len as i32) as usize;
    // The note belongs to the selection: the row must never show the previous
    // account's note under the newly selected header.
    app.reload_selected_note();
}

/// One page = the current viewport minus one overlap line (minimum 1), so a
/// page down keeps the old bottom line as the new top line. Every scrollable
/// surface steps by this; `↑`/`↓` stay one row/line and keep their wrapping.
pub(crate) fn page_step(viewport: usize) -> usize {
    viewport.saturating_sub(1).max(1)
}

/// The page-down landing row: the last row whose start line is within one page
/// below `cursor`'s start, at least one row forward, stopping at the last row.
/// Shared by [`page_down_cursor_by_rows`] and the Tokens dashboard page keys.
pub(crate) fn page_land_down(cursor: usize, starts: &[usize], viewport: usize) -> usize {
    if starts.is_empty() {
        return 0;
    }
    let last = starts.len() - 1;
    let cursor = cursor.min(last);
    if cursor >= last {
        return last;
    }
    let target = starts[cursor] + page_step(viewport);
    let mut next = cursor + 1;
    while next < last && starts[next + 1] <= target {
        next += 1;
    }
    next
}

/// The page-up landing row: the first row whose start line is within one page
/// above `cursor`'s start, at least one row back, stopping at the first row.
/// Shared by [`page_up_cursor_by_rows`] and the Tokens dashboard page keys.
pub(crate) fn page_land_up(cursor: usize, starts: &[usize], viewport: usize) -> usize {
    if starts.is_empty() {
        return 0;
    }
    let cursor = cursor.min(starts.len() - 1);
    if cursor == 0 {
        return 0;
    }
    let target = starts[cursor].saturating_sub(page_step(viewport));
    let mut prev = cursor - 1;
    while prev > 0 && starts[prev - 1] >= target {
        prev -= 1;
    }
    prev
}

/// Page down on a read-only scroll offset, clamped to the last render's bound.
pub(crate) fn page_down_scroll(scroll: usize, max: usize, viewport: usize) -> usize {
    scroll.min(max).saturating_add(page_step(viewport)).min(max)
}

/// Page up on a read-only scroll offset, clamped to the last render's bound.
pub(crate) fn page_up_scroll(scroll: usize, max: usize, viewport: usize) -> usize {
    scroll.min(max).saturating_sub(page_step(viewport))
}

/// Page down on a cursor list over its published row geometry: the cursor
/// lands on the last row whose start line is within one page (`viewport - 1`
/// lines) below the current row's start, at least one row forward, stopping at
/// the last row — walking back if the follow that landing produces would push
/// the old bottom line off the top (a focused row's tooltip moves with the
/// cursor). `starts[i]` is the rendered line where row `i` begins; `block_ends`
/// is the line its block reaches when focused; `offset`/`total` are the last
/// render's scroll and line count.
pub(crate) fn page_down_cursor_by_rows(
    cursor: usize,
    starts: &[usize],
    block_ends: &[usize],
    viewport: usize,
    offset: usize,
    total: usize,
) -> usize {
    if starts.is_empty() {
        return 0;
    }
    let cursor = cursor.min(starts.len() - 1);
    let mut next = page_land_down(cursor, starts, viewport);
    while next > cursor + 1 {
        let end = block_ends
            .get(next)
            .copied()
            .unwrap_or(starts[next] + 1)
            .max(starts[next] + 1);
        let predicted = follow_scroll_offset(total, viewport, (starts[next], end), offset);
        if predicted <= offset + page_step(viewport) {
            break;
        }
        next -= 1;
    }
    next
}

/// Page up mirror of [`page_down_cursor_by_rows`]: the cursor lands on the
/// first row whose start line is within one page above the current row's
/// start, at least one row back, stopping at the first row — walking forward
/// if the follow would push the old top line off the bottom. The landing row's
/// focused tooltip inserts `tips[prev]` lines above the old top line, so the
/// continuity bound is `offset + tip - page_step`, not `offset - page_step`.
pub(crate) fn page_up_cursor_by_rows(
    cursor: usize,
    starts: &[usize],
    block_ends: &[usize],
    tips: &[usize],
    viewport: usize,
    offset: usize,
    total: usize,
) -> usize {
    if starts.is_empty() {
        return 0;
    }
    let cursor = cursor.min(starts.len() - 1);
    if cursor == 0 {
        return 0;
    }
    let mut prev = page_land_up(cursor, starts, viewport);
    while prev < cursor - 1 {
        let end = block_ends
            .get(prev)
            .copied()
            .unwrap_or(starts[prev] + 1)
            .max(starts[prev] + 1);
        let predicted = follow_scroll_offset(total, viewport, (starts[prev], end), offset);
        let tip = tips.get(prev).copied().unwrap_or(0);
        if predicted
            >= offset
                .saturating_add(tip)
                .saturating_sub(page_step(viewport))
        {
            break;
        }
        prev += 1;
    }
    prev
}

/// Page down on a [`ListView`] cursor (single-line selector rows), reading the
/// geometry the render published.
fn selector_page_down(cursor: usize, list: &ListView) -> usize {
    let starts = list.starts.borrow();
    let block_ends = list.block_ends.borrow();
    page_down_cursor_by_rows(
        cursor,
        &starts,
        &block_ends,
        list.viewport.get(),
        list.offset.get(),
        list.total.get(),
    )
}

/// Page up on a [`ListView`] cursor, the mirror of [`selector_page_down`].
fn selector_page_up(cursor: usize, list: &ListView) -> usize {
    let starts = list.starts.borrow();
    let block_ends = list.block_ends.borrow();
    let tips = list.tips.borrow();
    page_up_cursor_by_rows(
        cursor,
        &starts,
        &block_ends,
        &tips,
        list.viewport.get(),
        list.offset.get(),
        list.total.get(),
    )
}

fn handle_overview_key(app: &mut App, key: KeyEvent) {
    let count = app.main_items().len();
    match key.code {
        KeyCode::Up if key.modifiers.contains(KeyModifiers::SHIFT) => reorder_main_cursor(app, -1),
        KeyCode::Down if key.modifiers.contains(KeyModifiers::SHIFT) => reorder_main_cursor(app, 1),
        KeyCode::Up if count > 0 => {
            app.overview_cursor = (app.overview_cursor + count - 1) % count;
            app.sync_overview_profile_selection();
        }
        KeyCode::Down if count > 0 => {
            app.overview_cursor = (app.overview_cursor + 1) % count;
            app.sync_overview_profile_selection();
        }
        KeyCode::PageUp if count > 0 => {
            app.overview_cursor = selector_page_up(app.overview_cursor, &app.overview_selector);
            app.sync_overview_profile_selection();
        }
        KeyCode::PageDown if count > 0 => {
            app.overview_cursor = selector_page_down(app.overview_cursor, &app.overview_selector);
            app.sync_overview_profile_selection();
        }
        KeyCode::Enter => activate_main_item(app),
        _ => {}
    }
}

/// Usage tab: account selection at rest, read-only detail scrolling on descent.
fn handle_usage_key(app: &mut App, key: KeyEvent) {
    if app.usage_detail_focus == StatusFocus::Detail {
        let max = app.usage_detail_max_scroll.get();
        let viewport = app.usage_detail_viewport.get();
        match key.code {
            KeyCode::Up => {
                app.usage_detail_scroll = app.usage_detail_scroll.min(max).saturating_sub(1)
            }
            KeyCode::Down => {
                app.usage_detail_scroll =
                    app.usage_detail_scroll.min(max).saturating_add(1).min(max)
            }
            KeyCode::PageUp => {
                app.usage_detail_scroll = page_up_scroll(app.usage_detail_scroll, max, viewport)
            }
            KeyCode::PageDown => {
                app.usage_detail_scroll = page_down_scroll(app.usage_detail_scroll, max, viewport)
            }
            KeyCode::Char('e') => toggle_show_estimates(app),
            KeyCode::Char('p') => toggle_show_pace(app),
            _ => {}
        }
        return;
    }
    let count = app.profile_count();
    match key.code {
        KeyCode::Up => {
            step_profile_cursor(app, -1, count);
            app.usage_detail_scroll = 0;
        }
        KeyCode::Down => {
            step_profile_cursor(app, 1, count);
            app.usage_detail_scroll = 0;
        }
        KeyCode::PageUp => {
            app.profile_cursor = selector_page_up(app.profile_cursor, &app.usage_selector);
            app.reload_selected_note();
            app.usage_detail_scroll = 0;
        }
        KeyCode::PageDown => {
            app.profile_cursor = selector_page_down(app.profile_cursor, &app.usage_selector);
            app.reload_selected_note();
            app.usage_detail_scroll = 0;
        }
        KeyCode::Enter if count > 0 => {
            app.usage_detail_focus = StatusFocus::Detail;
            app.usage_detail_scroll = 0;
        }
        KeyCode::Char('e') => toggle_show_estimates(app),
        KeyCode::Char('p') => toggle_show_pace(app),
        _ => {}
    }
}

/// Open the note editor on the selected account, seeded from disk.
fn open_note_editor(app: &mut App) {
    let Some(name) = app.profile_name_at(app.profile_cursor) else {
        return;
    };
    let text = crate::profile_notes::load_note(&name).unwrap_or_default();
    app.note_editor = Some(NoteEditor {
        profile: name,
        buf: NoteBuffer::new(&text),
    });
}

/// `⏎` in the note editor: persist the draft (an empty draft clears the note).
/// A failed save keeps the editor open — the draft is the only copy.
fn commit_note(app: &mut App) {
    let Some(editor) = app.note_editor.take() else {
        return;
    };
    let text = editor.buf.text();
    match crate::profile_notes::save_note(&editor.profile, &text) {
        Ok(()) => {
            app.note_text = crate::profile_notes::load_note(&editor.profile);
            app.toast(ToastKind::Success, "note saved");
        }
        Err(e) => {
            app.note_editor = Some(editor);
            app.toast(ToastKind::Danger, format!("couldn't save the note: {e}"));
        }
    }
}

/// The note editor's keymap: text edits, ⌃j newline, ⏎ save, esc cancel. Every
/// printable is data (a note may contain `q`, `?`, `x`), so the owner claims
/// all keys bar ctrl+c.
fn handle_note_editor_key(app: &mut App, key: KeyEvent) {
    enum NoteKey {
        Commit,
        Cancel,
        Char(char),
        Newline,
        Word,
        Backspace,
        Delete,
        Left,
        Right,
        Up,
        Down,
        Home,
        End,
    }
    let action = match key.code {
        KeyCode::Enter => NoteKey::Commit,
        KeyCode::Esc => NoteKey::Cancel,
        KeyCode::Char('j') if key.modifiers.contains(KeyModifiers::CONTROL) => NoteKey::Newline,
        KeyCode::Char('w') if key.modifiers.contains(KeyModifiers::CONTROL) => NoteKey::Word,
        KeyCode::Char(ch) => NoteKey::Char(ch),
        KeyCode::Backspace => NoteKey::Backspace,
        KeyCode::Delete => NoteKey::Delete,
        KeyCode::Left => NoteKey::Left,
        KeyCode::Right => NoteKey::Right,
        KeyCode::Up => NoteKey::Up,
        KeyCode::Down => NoteKey::Down,
        KeyCode::Home => NoteKey::Home,
        KeyCode::End => NoteKey::End,
        _ => return,
    };
    match action {
        NoteKey::Commit => commit_note(app),
        NoteKey::Cancel => app.note_editor = None,
        _ => {
            let Some(editor) = app.note_editor.as_mut() else {
                return;
            };
            let buf = &mut editor.buf;
            match action {
                NoteKey::Char(ch) => buf.insert_char(ch),
                NoteKey::Newline => buf.insert_newline(),
                NoteKey::Word => buf.delete_word(),
                NoteKey::Backspace => buf.backspace(),
                NoteKey::Delete => buf.delete(),
                NoteKey::Left => buf.left(),
                NoteKey::Right => buf.right(),
                NoteKey::Up => buf.up(),
                NoteKey::Down => buf.down(),
                NoteKey::Home => buf.home(),
                NoteKey::End => buf.end(),
                _ => {}
            }
        }
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
                KeyCode::PageUp if len > 0 => {
                    app.status.cursor = selector_page_up(app.status.cursor, &app.status.selector);
                    app.status.detail_scroll = 0;
                }
                KeyCode::PageDown if len > 0 => {
                    app.status.cursor = selector_page_down(app.status.cursor, &app.status.selector);
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
            KeyCode::PageUp => {
                let max = app.status.detail_max_scroll.get();
                app.status.detail_scroll = page_up_scroll(
                    app.status.detail_scroll as usize,
                    max as usize,
                    app.status.detail_viewport.get() as usize,
                ) as u16;
            }
            KeyCode::PageDown => {
                let max = app.status.detail_max_scroll.get();
                app.status.detail_scroll = page_down_scroll(
                    app.status.detail_scroll as usize,
                    max as usize,
                    app.status.detail_viewport.get() as usize,
                ) as u16;
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

/// Services tab keymap. List focus: ↑↓ moves the cursor (wrapping), ⏎ descends
/// to the detail pane (the `delegates` detail only while it lists a job), `f`
/// descends onto the selected row's first fix line. Detail focus: the plugin
/// detail walks its fixable problems, the herdr detail walks its heal line and
/// options rows, the delegates detail walks its jobs, every other detail
/// scrolls (clamped by the render pass); `f` walks to the next fix line and `↵`
/// on a focused one opens its confirm.
fn handle_services_key(app: &mut App, key: KeyEvent) {
    match app.services.focus {
        ServicesFocus::List => {
            let len = app.services.row_count();
            match key.code {
                KeyCode::Up if len > 0 => {
                    app.services.cursor = (app.services.cursor + len - 1) % len;
                    app.services.detail_scroll = 0;
                    app.services.diagnostic_focus = false;
                    app.services.land_on_herdr = false;
                }
                KeyCode::Down if len > 0 => {
                    app.services.cursor = (app.services.cursor + 1) % len;
                    app.services.detail_scroll = 0;
                    app.services.diagnostic_focus = false;
                    app.services.land_on_herdr = false;
                }
                KeyCode::PageUp if len > 0 => {
                    app.services.cursor =
                        selector_page_up(app.services.cursor, &app.services.selector);
                    app.services.detail_scroll = 0;
                    app.services.diagnostic_focus = false;
                    app.services.land_on_herdr = false;
                }
                KeyCode::PageDown if len > 0 => {
                    app.services.cursor =
                        selector_page_down(app.services.cursor, &app.services.selector);
                    app.services.detail_scroll = 0;
                    app.services.diagnostic_focus = false;
                    app.services.land_on_herdr = false;
                }
                KeyCode::Enter if len > 0 => {
                    // The user has acted: any still-pending herdr landing is
                    // cancelled, whether or not this descends.
                    app.services.land_on_herdr = false;
                    let delegates = app
                        .services
                        .selected_check()
                        .is_some_and(|c| c.label == "delegates");
                    // A delegates detail with no jobs has nothing to select, so
                    // `↵` does not descend into it (S2/S6 precedent: a pane
                    // with nothing to select advertises no inert `↵`).
                    if app.services.enter_descends() {
                        app.services.focus = ServicesFocus::Detail;
                        app.services.detail_scroll = 0;
                        app.services.diagnostic_focus = false;
                        app.services.herdr_fix_focus = false;
                        if delegates {
                            app.services.settle_delegates_selection();
                            app.services.delegates_offset.set(0);
                            app.services.delegates_has_descended = true;
                        } else {
                            settle_shunt_focus(app);
                        }
                    }
                }
                KeyCode::Char('f') => {
                    app.services.land_on_herdr = false;
                    walk_to_next_fix(app);
                }
                _ => {}
            }
        }
        ServicesFocus::Detail => {
            let label = app.services.selected_check().map(|c| c.label);
            if matches!(label, Some("herdr" | "plugin"))
                && matches!(key.code, KeyCode::PageUp | KeyCode::PageDown)
            {
                let max = app.services.detail_max_scroll.get();
                let viewport = app.services.detail_viewport.get();
                let origin = if app.services.diagnostic_focus {
                    app.services.detail_scroll.min(max)
                } else {
                    app.services.displayed_scroll.get().min(max)
                };
                app.services.diagnostic_focus = true;
                app.services.detail_scroll = if key.code == KeyCode::PageDown {
                    page_down_scroll(origin as usize, max as usize, viewport as usize) as u16
                } else {
                    page_up_scroll(origin as usize, max as usize, viewport as usize) as u16
                };
            } else if label == Some("herdr") {
                handle_herdr_options_key(app, key);
            } else if label == Some("plugin")
                && app
                    .services
                    .selected_check()
                    .is_some_and(|c| !c.problems.is_empty())
            {
                handle_plugin_problems_key(app, key);
            } else if label == Some("shunt") {
                handle_shunt_detail_key(app, key);
            } else if label == Some("delegates") {
                handle_delegates_detail_key(app, key);
            } else {
                match key.code {
                    KeyCode::Up => {
                        // Clamp before stepping — see the Status detail pane's ↑ arm.
                        let max = app.services.detail_max_scroll.get();
                        app.services.detail_scroll =
                            app.services.detail_scroll.min(max).saturating_sub(1);
                    }
                    KeyCode::Down => {
                        let max = app.services.detail_max_scroll.get();
                        app.services.detail_scroll =
                            app.services.detail_scroll.saturating_add(1).min(max);
                    }
                    KeyCode::PageUp => {
                        let max = app.services.detail_max_scroll.get();
                        app.services.detail_scroll = page_up_scroll(
                            app.services.detail_scroll as usize,
                            max as usize,
                            app.services.detail_viewport.get() as usize,
                        ) as u16;
                    }
                    KeyCode::PageDown => {
                        let max = app.services.detail_max_scroll.get();
                        app.services.detail_scroll = page_down_scroll(
                            app.services.detail_scroll as usize,
                            max as usize,
                            app.services.detail_viewport.get() as usize,
                        ) as u16;
                    }
                    _ => {}
                }
            }
        }
    }
}

/// `f` under the Services fix grammar (cloudy 2026-10-08, scoped to the
/// selected row 2026-10-12): focus moves to the selected row's next fix line
/// in detail order, wrapping, and from the list it descends onto the row's
/// first one. It never opens a fix; `↵` on the focused line does. A row with
/// no fix line leaves everything as it was.
fn walk_to_next_fix(app: &mut App) {
    if !app.services.selected_has_fix() {
        return;
    }
    let Some(check) = app.services.selected_check() else {
        return;
    };
    let label = check.label;
    let problems: Vec<usize> = check.problems.iter().map(|p| p.line).collect();
    let from_list = app.services.focus == ServicesFocus::List;
    if from_list {
        app.services.focus = ServicesFocus::Detail;
        app.services.detail_scroll = 0;
    }
    match label {
        "shunt" => shunt_walk_to_next_fix(app, from_list),
        "herdr" => {
            app.services.diagnostic_focus = false;
            app.services.herdr_fix_focus = true;
        }
        _ if problems.is_empty() => {}
        _ => {
            // A page key hid the walk focus: "next" starts from the top of
            // what the pane shows.
            let next = if from_list {
                0
            } else if app.services.diagnostic_focus {
                let top = app.services.displayed_scroll.get() as usize;
                problems.iter().position(|&line| line >= top).unwrap_or(0)
            } else {
                (app.services.problem_cursor + 1) % problems.len()
            };
            app.services.diagnostic_focus = false;
            app.services.problem_cursor = next;
        }
    }
}

/// The shunt card's half of [`walk_to_next_fix`]: the next fix stop after the
/// focused one in card order (the first one from the list, or the first at or
/// below the top of the view when nothing holds focus), wrapping, scrolled
/// just far enough to bring it into view.
fn shunt_walk_to_next_fix(app: &mut App, from_list: bool) {
    let Some(stops) = app.services.selected_check().map(|c| c.shunt_focus.clone()) else {
        return;
    };
    let fixes: Vec<usize> = (0..stops.len())
        .filter(|&i| stops[i].fix().is_some())
        .collect();
    let Some(&first) = fixes.first() else {
        return;
    };
    let rows = app.services.shunt_stop_rows.borrow().clone();
    let geometry = rows.len() == stops.len();
    let viewport = app.services.shunt_viewport.get() as usize;
    let max = app.services.detail_max_scroll.get();
    let scroll = app.services.detail_scroll.min(max);
    let target = if from_list {
        first
    } else {
        match app
            .services
            .focused_shunt_focus()
            .and_then(|f| stops.iter().position(|s| s == f))
        {
            Some(cur) => fixes.iter().copied().find(|&i| i > cur).unwrap_or(first),
            None if geometry => fixes
                .iter()
                .copied()
                .find(|&i| rows[i] >= scroll as usize)
                .unwrap_or(first),
            None => first,
        }
    };
    app.services.set_shunt_focus(Some(stops[target].kind()));
    if geometry && !shunt_in_view(scroll, viewport, rows[target]) {
        let row = rows[target];
        let scroll = if row < scroll as usize {
            row
        } else {
            row + 1 - viewport.max(1)
        };
        app.services.detail_scroll = scroll.min(max as usize) as u16;
    }
}

/// The plugin detail's problem walk: ↑↓ and `f` move over the fixable
/// problems (wrapping), `↵` opens the focused one's confirm. The render keeps
/// the focused line on screen; there is no manual scroll offset to maintain
/// here.
fn handle_plugin_problems_key(app: &mut App, key: KeyEvent) {
    let len = app
        .services
        .selected_check()
        .map(|c| c.problems.len())
        .unwrap_or(0);
    match key.code {
        KeyCode::Up if len > 0 => {
            app.services.diagnostic_focus = false;
            app.services.problem_cursor = (app.services.problem_cursor + len - 1) % len;
        }
        KeyCode::Down if len > 0 => {
            app.services.diagnostic_focus = false;
            app.services.problem_cursor = (app.services.problem_cursor + 1) % len;
        }
        KeyCode::Char('f') => walk_to_next_fix(app),
        KeyCode::Enter => apply_service_fix(app),
        _ => {}
    }
}

/// The delegates detail's keymap while descended: ↑↓ walks the job list
/// (wrapping), PageUp/PageDown page through the S8 page helper, stopping at
/// the ends. The selection follows the job id, so a refresh that inserts a
/// newer job above keeps the same job selected. No stop/cancel key exists —
/// stopping a delegate stays `monitor`'s job, and the pane binds nothing else.
fn handle_delegates_detail_key(app: &mut App, key: KeyEvent) {
    let len = app.services.delegates.len();
    if len == 0 {
        return;
    }
    app.services.delegates_cursor = app.services.delegates_cursor.min(len - 1);
    match key.code {
        KeyCode::Up => {
            app.services.delegates_cursor =
                (app.services.delegates_cursor + len - 1).rem_euclid(len);
            track_delegates_selection(app);
        }
        KeyCode::Down => {
            app.services.delegates_cursor = (app.services.delegates_cursor + 1).rem_euclid(len);
            track_delegates_selection(app);
        }
        KeyCode::PageUp => {
            let next = {
                let starts = app.services.delegates_starts.borrow();
                let block_ends = app.services.delegates_block_ends.borrow();
                page_up_cursor_by_rows(
                    app.services.delegates_cursor,
                    &starts,
                    &block_ends,
                    &[],
                    app.services.delegates_viewport.get(),
                    app.services.delegates_offset.get(),
                    app.services.delegates_total.get(),
                )
            };
            app.services.delegates_cursor = next;
            track_delegates_selection(app);
        }
        KeyCode::PageDown => {
            let next = {
                let starts = app.services.delegates_starts.borrow();
                let block_ends = app.services.delegates_block_ends.borrow();
                page_down_cursor_by_rows(
                    app.services.delegates_cursor,
                    &starts,
                    &block_ends,
                    app.services.delegates_viewport.get(),
                    app.services.delegates_offset.get(),
                    app.services.delegates_total.get(),
                )
            };
            app.services.delegates_cursor = next;
            track_delegates_selection(app);
        }
        _ => {}
    }
}

/// Mirror the delegates cursor onto the tracked job id (the refresh anchor).
fn track_delegates_selection(app: &mut App) {
    app.services.delegates_selected_id = app
        .services
        .delegates
        .get(app.services.delegates_cursor)
        .map(|j| j.record.job_id.clone());
}

/// The shunt detail's keymap: ↑↓ walks the actionable lines (landing on each
/// as it scrolls into view, one line at a time through the read-only text),
/// `f` walks to the next fix line, `↵` opens the focused fix's confirm, and
/// `space`/`↵` act on any other focused line (flip the `enabled` toggle, open
/// the `check output` modal, expand/collapse a config entry, or open/step a
/// config field editor — `space` cycles a cycle field).
/// Focus is the key handler's own state (a [`ShuntFocusLine`] identity); the
/// render reads it and re-settles it (the descend rule) when a resize or a
/// card change pushed the held line out of view. Only the shunt detail routes
/// here; the herdr options and plugin problems keep their own walks, every
/// other detail keeps the scroll-only keymap.
fn handle_shunt_detail_key(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Up => shunt_step(app, false),
        KeyCode::Down => shunt_step(app, true),
        KeyCode::PageUp => shunt_page(app, false),
        KeyCode::PageDown => shunt_page(app, true),
        KeyCode::Char('f') => walk_to_next_fix(app),
        KeyCode::Enter => match app.services.focused_shunt_focus() {
            Some(ShuntFocus::Enabled { .. }) => toggle_shunt_enabled(app),
            Some(ShuntFocus::CheckOutput { .. }) => open_check_output_modal(app),
            Some(ShuntFocus::Entry { key }) => {
                let key = key.clone();
                toggle_shunt_entry(app, &key);
            }
            Some(ShuntFocus::Field { key }) => {
                let key = key.clone();
                open_shunt_field_editor(app, &key);
            }
            Some(ShuntFocus::Add { kind }) => toggle_shunt_add(app, *kind),
            Some(ShuntFocus::SaveAdd { kind }) => commit_shunt_add(app, *kind),
            Some(ShuntFocus::Remove { key }) => {
                let key = key.clone();
                confirm_shunt_remove(app, &key);
            }
            Some(ShuntFocus::Token { key }) => {
                let key = key.clone();
                confirm_shunt_remove_token(app, &key);
            }
            Some(ShuntFocus::AddToken) => open_token_picker(app),
            Some(ShuntFocus::Fix { .. }) => apply_service_fix(app),
            None => {}
        },
        KeyCode::Char(' ') => match app.services.focused_shunt_focus() {
            Some(ShuntFocus::Enabled { .. }) => toggle_shunt_enabled(app),
            Some(ShuntFocus::CheckOutput { .. }) => open_check_output_modal(app),
            Some(ShuntFocus::Entry { key }) => {
                let key = key.clone();
                toggle_shunt_entry(app, &key);
            }
            Some(ShuntFocus::Field { key }) => {
                let key = key.clone();
                // `space` on a cycle row moves the pick; on a typed row it
                // opens the editor, like `↵`.
                if is_cycle_field(&key) {
                    step_shunt_pick(app, &key);
                } else {
                    open_shunt_field_editor(app, &key);
                }
            }
            Some(ShuntFocus::Add { kind }) => toggle_shunt_add(app, *kind),
            Some(ShuntFocus::AddToken) => open_token_picker(app),
            Some(ShuntFocus::SaveAdd { .. })
            | Some(ShuntFocus::Remove { .. })
            | Some(ShuntFocus::Token { .. }) => {}
            Some(ShuntFocus::Fix { .. }) | None => {}
        },
        _ => {}
    }
}

/// One `↑`/`↓` on the shunt card, over the geometry the last render published
/// ([`ServicesState::shunt_stop_rows`] + [`ServicesState::shunt_viewport`] +
/// [`ServicesState::detail_max_scroll`]): move focus to the next/previous
/// actionable line in card order, scrolling one rendered line at a time only
/// while that target is out of view, and never past either end. Read-only rows
/// never take focus; with no further actionable line a press scrolls (while
/// there is text to scroll) and focus stays on its line while that line is in
/// view.
fn shunt_step(app: &mut App, down: bool) {
    let Some(stops) = app.services.selected_check().map(|c| c.shunt_focus.clone()) else {
        return;
    };
    if stops.is_empty() {
        // A card with no actionable line still scrolls one rendered line per
        // ↑↓ (round 1 S14's behaviour): the read-only detail scrolls, and the
        // footer's `↑↓ scroll` is true exactly while this does something.
        let max = app.services.detail_max_scroll.get();
        app.services.detail_scroll = if down {
            app.services.detail_scroll.saturating_add(1).min(max)
        } else {
            app.services.detail_scroll.min(max).saturating_sub(1)
        };
        return;
    }
    let rows = app.services.shunt_stop_rows.borrow().clone();
    if rows.len() != stops.len() {
        // The render has not published this card's geometry yet (nothing drawn
        // to walk over).
        return;
    }
    let viewport = app.services.shunt_viewport.get() as usize;
    let max_scroll = app.services.detail_max_scroll.get();
    let mut scroll = app.services.detail_scroll.min(max_scroll);

    // Reconcile only a held identity that left the view (a resize can move it
    // out): the descend rule — the first actionable line in view — settles it,
    // and the step below walks from there. A none focus is NOT reconciled
    // here: `↓`/`↑` from none start from the target rule itself (the first
    // stop at/after the top of the view going down, the last before the bottom
    // going up), so the first visible line is never skipped.
    let focus = app.services.shunt_focus_kind().and_then(|kind| {
        stops
            .iter()
            .position(|s| s.kind() == kind)
            .filter(|&i| shunt_in_view(scroll, viewport, rows[i]))
            .map(|i| stops[i].kind())
    });
    let cur = focus
        .clone()
        .and_then(|kind| stops.iter().position(|s| s.kind() == kind));

    // The target stop: the one after/before the focused line, or (with no
    // focus) the first at/after the top of the view going down, the last
    // before the bottom of the view going up.
    let target = if down {
        match cur {
            Some(cur) => (cur + 1 < stops.len()).then_some(cur + 1),
            None => (0..stops.len()).find(|&i| rows[i] >= scroll as usize),
        }
    } else {
        match cur {
            Some(cur) => (cur > 0).then_some(cur - 1),
            None => (0..stops.len())
                .rev()
                .find(|&i| rows[i] < scroll as usize + viewport),
        }
    };

    // The current focus kept while its line is still in view after a scroll.
    let stops_ref = &stops;
    let rows_ref = &rows;
    let keep_focus = move |scroll: u16| {
        focus.filter(|kind| {
            stops_ref
                .iter()
                .position(|s| s.kind() == kind.clone())
                .is_some_and(|i| shunt_in_view(scroll, viewport, rows_ref[i]))
        })
    };

    match target {
        Some(t) if shunt_in_view(scroll, viewport, rows[t]) => {
            app.services.set_shunt_focus(Some(stops[t].kind()));
        }
        Some(_) => {
            // The target is out of view: scroll one line toward it, then take
            // it if that brought it into view, else keep the current focus
            // while its line is still visible.
            if down {
                scroll = scroll.saturating_add(1).min(max_scroll);
            } else {
                scroll = scroll.saturating_sub(1);
            }
            let target_taken = target
                .filter(|&t| shunt_in_view(scroll, viewport, rows[t]))
                .map(|t| stops[t].kind());
            app.services
                .set_shunt_focus(target_taken.or_else(|| keep_focus(scroll)));
        }
        None => {
            // No further actionable line: scroll one line while there is text
            // to scroll in this direction; at either end, nothing.
            if down {
                scroll = scroll.saturating_add(1).min(max_scroll);
            } else {
                scroll = scroll.saturating_sub(1);
            }
            app.services.set_shunt_focus(keep_focus(scroll));
        }
    }
    app.services.detail_scroll = scroll;
}

/// Whether a rendered row is inside the current viewport.
fn shunt_in_view(scroll: u16, viewport: usize, r: usize) -> bool {
    r >= scroll as usize && r < scroll as usize + viewport
}

/// One `PageUp`/`PageDown` on the shunt card: scroll the read-only detail one
/// page (`viewport - 1` lines), then re-pick the first actionable line in view
/// (or none) — the same rule a resize applies.
fn shunt_page(app: &mut App, down: bool) {
    let Some(stops) = app.services.selected_check().map(|c| c.shunt_focus.clone()) else {
        return;
    };
    let rows = app.services.shunt_stop_rows.borrow().clone();
    if rows.len() != stops.len() {
        // Geometry not published yet (a key before the first draw) — as in
        // `shunt_step`, there is nothing to walk over.
        return;
    }
    let max = app.services.detail_max_scroll.get();
    let viewport = app.services.shunt_viewport.get() as usize;
    // Compare the RENDERED offset (the stored scroll clamped to the last
    // render's bound), not the raw stored value: a taller terminal leaves the
    // stored scroll above the new bound, and a page over a stale number must
    // not re-pick focus while the rendered view stays put.
    let before = app.services.detail_scroll.min(max);
    let paged = if down {
        page_down_scroll(before as usize, max as usize, viewport) as u16
    } else {
        page_up_scroll(before as usize, max as usize, viewport) as u16
    };
    app.services.detail_scroll = paged;
    if paged == before {
        // The card fits, or is already at this end: the held focus stays.
        return;
    }
    app.services
        .set_shunt_focus(shunt_first_in_view(&stops, &rows, paged, viewport));
}

/// The shunt card's descend rule: the first actionable line in view, else
/// none.
fn shunt_first_in_view(
    stops: &[ShuntFocus],
    rows: &[usize],
    scroll: u16,
    viewport: usize,
) -> Option<ShuntFocusLine> {
    stops
        .iter()
        .zip(rows)
        .find(|(_, r)| shunt_in_view(scroll, viewport, **r))
        .map(|(s, _)| s.kind())
}

/// The shunt card's descend rule applied on descend: focus keeps its held
/// identity while that line is in view, else it becomes the first actionable
/// line in view (else none). Re-descending after an ascend therefore restores
/// the walk position instead of resetting to the top of the card.
fn settle_shunt_focus(app: &mut App) {
    let Some(stops) = app.services.selected_check().map(|c| c.shunt_focus.clone()) else {
        return;
    };
    let rows = app.services.shunt_stop_rows.borrow().clone();
    let viewport = app.services.shunt_viewport.get() as usize;
    let scroll = app
        .services
        .detail_scroll
        .min(app.services.detail_max_scroll.get());
    let held = app.services.shunt_focus_kind().and_then(|kind| {
        stops
            .iter()
            .position(|s| s.kind() == kind)
            .filter(|&i| i < rows.len() && shunt_in_view(scroll, viewport, rows[i]))
            .map(|i| stops[i].kind())
    });
    app.services
        .set_shunt_focus(held.or_else(|| shunt_first_in_view(&stops, &rows, scroll, viewport)));
}

/// Flip the record's `disabled` flag at once, reading and writing the value
/// ON DISK inside [`crate::gateway::GatewayRecord::update`] (under the state
/// flock), never the cached record. A failed write toasts the house
/// `save failed` + the error. The recompute below reloads the record from
/// disk and re-plans, so the row flips on the same frame.
fn toggle_shunt_enabled(app: &mut App) {
    if app.services.shunt_action.running {
        return;
    }
    match crate::gateway::GatewayRecord::update(|slot| {
        if let Some(held) = slot.as_mut() {
            held.disabled = !held.disabled;
        }
        Ok(())
    }) {
        Ok(()) => {}
        Err(e) => {
            app.toast(
                ToastKind::Danger,
                format!("save failed\n{}", escape_control(&error_toast_detail(&e))),
            );
        }
    }
    recompute_services_checks(app, false, ShuntRefresh::Restart);
}

/// Toggle one config entry row expanded/collapsed (the cloudy-tui disclosure
/// contract: `space` and `↵` both activate, independently per entry). The
/// recompute prunes an expanded key whose entry left the config, so a key
/// re-appearing after an edit arrives collapsed.
fn toggle_shunt_entry(app: &mut App, key: &ShuntEntryKey) {
    // A locked remove ignores expand/collapse (ruling 2): the entry's rows are
    // inert while its `remove <kind>` checks.
    if app.services.saving(&ShuntSaveKey::Remove(key.clone())) {
        return;
    }
    if !app.services.shunt_expanded.remove(key) {
        app.services.shunt_expanded.insert(key.clone());
    } else {
        // Collapsing an entry retires a refusal on its remove row.
        retire_remove_refusal(app, key);
    }
    rebuild_shunt_config_stops(app);
}

/// Retire a remove refusal whose entry is `key` (a collapse), if one shows.
fn retire_remove_refusal(app: &mut App, key: &ShuntEntryKey) {
    if app
        .services
        .shunt_remove_refusal
        .borrow()
        .as_ref()
        .is_some_and(|refusal| refusal.key == *key)
    {
        app.services.shunt_remove_refusal.take();
    }
}

/// Re-derive the card's cached config stops in the same call that changed the
/// expanded set, so the walk and the render agree at once (the render calls the
/// same [`config_field_stops`] over the same view and expanded set every frame).
/// The action-section stops (every stop with a detail line) are kept; the
/// config section is replaced from the one row model.
fn rebuild_shunt_config_stops(app: &mut App) {
    let Some(stops) = config_view(app).map(|view| {
        config_field_stops(
            view,
            &app.services.shunt_expanded,
            &app.services.shunt_drafts,
            &app.services.shunt_tokens,
        )
    }) else {
        return;
    };
    let Some(check) = app.services.checks.iter_mut().find(|c| c.label == "shunt") else {
        return;
    };
    check.shunt_focus.truncate(
        check
            .shunt_focus
            .iter()
            .take_while(|s| s.line().is_some())
            .count(),
    );
    check.shunt_focus.extend(stops);
}

// ── add and remove rows ─────────────────────────────────────────────────────

/// `↵`/`space` on `+ add <kind>`: open the blank draft in place, or fold it
/// when it is already open (the Setup `+ new` form's precedent, which `esc` at
/// rest on a draft row follows too). Focus stays on the `+ add` row.
fn toggle_shunt_add(app: &mut App, kind: ShuntAddKind) {
    // A locked draft ignores fold (ruling 2): `↵`/`space` on `+ add <kind>` are
    // inert while its `save <kind>` checks.
    if app.services.saving(&ShuntSaveKey::SaveAdd(kind)) {
        return;
    }
    let was_open = app.services.shunt_drafts.is_open(kind);
    if was_open {
        app.services.shunt_drafts.fold(kind);
    } else {
        app.services.shunt_drafts.open(kind);
    }
    // A folded draft closes its field editor too (a draft field's included).
    if was_open
        && let Some(state) = &app.services.shunt_field_edit
        && let ShuntFieldKey::Draft {
            kind: state_kind, ..
        } = state.key
        && state_kind == kind
    {
        app.services.shunt_field_edit = None;
    }
    rebuild_shunt_config_stops(app);
    app.services
        .set_shunt_focus(Some(ShuntFocusLine::Add(kind)));
}

/// `↵` on `save <kind>`: build the `New*` from the draft (a blank field is
/// left out) and run it through the save worker. A required field left blank
/// refuses before any worker runs, on the `└` line under `save <kind>`.
fn commit_shunt_add(app: &mut App, kind: ShuntAddKind) {
    if app.services.saving(&ShuntSaveKey::SaveAdd(kind)) {
        return;
    }
    let Some(view) = config_view(app).cloned() else {
        return;
    };
    match build_add_edit(&view, &app.services.shunt_drafts, kind) {
        AddDraft::Refuse(sentence) => {
            app.services.shunt_drafts.set_refusal(kind, sentence);
        }
        AddDraft::Edit(edit) => {
            start_shunt_save(app, ShuntSaveKey::SaveAdd(kind), *edit, false);
        }
    }
}

/// The `Edit::Add*` a draft builds, or a required-field refusal sentence.
enum AddDraft {
    Edit(Box<crate::gateway_edit::Edit>),
    Refuse(String),
}

/// Build the add edit from a draft: a blank field is `None` (left out), the
/// `accounts` list is comma-separated with spaces around commas trimmed (C2's
/// accounts row parse). A required field left blank refuses first.
fn build_add_edit(
    view: &crate::gateway_edit::ConfigView,
    drafts: &ShuntDrafts,
    kind: ShuntAddKind,
) -> AddDraft {
    use crate::gateway_edit::{Edit, NewAuth, NewModel, NewRoute, NewUpstream};
    match kind {
        ShuntAddKind::Upstream => {
            let Some(draft) = &drafts.upstream else {
                return AddDraft::Refuse("cannot add upstream: set its name first".to_string());
            };
            let Some(name) = draft.name.as_deref().filter(|name| !name.is_empty()) else {
                return AddDraft::Refuse("cannot add upstream: set its name first".to_string());
            };
            let auth = draft
                .auth_mode
                .as_deref()
                .filter(|mode| !mode.is_empty())
                .map(|mode| {
                    // Only the fields whose rows the current mode shows reach
                    // the add: a value typed under one mode then hidden by a
                    // switch must not ride into the written config.
                    let (account, accounts, env, header) = match auth_mode_rows(Some(mode)) {
                        AuthModeRows::ApiKey => {
                            (None, Vec::new(), draft.env.clone(), draft.header.clone())
                        }
                        AuthModeRows::OAuth => (
                            None,
                            split_accounts(draft.accounts.as_deref().unwrap_or("")),
                            None,
                            None,
                        ),
                        AuthModeRows::None => (None, Vec::new(), None, None),
                    };
                    NewAuth {
                        mode: Some(mode.to_string()),
                        account,
                        accounts,
                        env,
                        header,
                    }
                });
            AddDraft::Edit(Box::new(Edit::AddUpstream(NewUpstream {
                name: name.to_string(),
                provider: draft.provider.clone(),
                kind: draft.kind.clone(),
                base_url: draft.base_url.clone(),
                auth,
                effort: draft.effort.clone(),
                service_tier: draft.service_tier.clone(),
            })))
        }
        ShuntAddKind::Model => {
            let Some(draft) = &drafts.model else {
                return AddDraft::Refuse("cannot add model: set its id first".to_string());
            };
            let Some(id) = draft.id.as_deref().filter(|id| !id.is_empty()) else {
                return AddDraft::Refuse("cannot add model: set its id first".to_string());
            };
            let upstream_model = provider_names(view)
                .into_iter()
                .filter_map(|provider| {
                    draft
                        .upstream_model
                        .iter()
                        .find(|(name, _)| *name == provider)
                        .and_then(|(_, slug)| slug.clone())
                        .map(|slug| (provider, slug))
                })
                .collect();
            AddDraft::Edit(Box::new(Edit::AddModel(NewModel {
                id: id.to_string(),
                display_name: draft.display_name.clone(),
                upstream_model,
            })))
        }
        ShuntAddKind::Route => {
            let Some(draft) = &drafts.route else {
                return AddDraft::Refuse(
                    "cannot add a route: set its model and provider first".to_string(),
                );
            };
            let (Some(model), Some(provider)) = (
                draft.model.clone().filter(|model| !model.is_empty()),
                draft
                    .provider
                    .clone()
                    .filter(|provider| !provider.is_empty()),
            ) else {
                return AddDraft::Refuse(
                    "cannot add a route: set its model and provider first".to_string(),
                );
            };
            AddDraft::Edit(Box::new(Edit::AddRoute(NewRoute {
                model,
                provider,
                upstream_model: draft.upstream_model.clone(),
                effort: draft.effort.clone(),
                service_tier: draft.service_tier.clone(),
            })))
        }
    }
}

/// `↵` on `remove <kind>`: open the confirm modal (the C2 confirm kind reused,
/// no new modal kind, no body line), `cancel` default-focused, `remove` the
/// destructive button.
fn confirm_shunt_remove(app: &mut App, key: &ShuntEntryKey) {
    // A locked remove opens no second confirm (ruling 2): the `remove <kind>`
    // row is inert while its own remove checks.
    if app.services.saving(&ShuntSaveKey::Remove(key.clone())) {
        return;
    }
    let Some(view) = config_view(app).cloned() else {
        return;
    };
    // The edit is built here, from the entry as it reads now (a route's
    // `RouteView` included): confirming runs THIS edit, never a lookup at
    // confirm time, so a route the shift moved under it refuses `EntryChanged`
    // instead of removing the entry that now sits at its position.
    let Some(edit) = remove_edit(&view, key) else {
        // The route the key names left the config since its row rendered:
        // nothing to remove, and no confirm to open.
        return;
    };
    let message = remove_confirm_message(&view, key);
    app.disarm_quit();
    app.open_modal(Modal::Confirm(ConfirmState {
        message,
        detail: None,
        choice: false,
        scroll: ConfirmScroll::default(),
        on_confirm: ConfirmAction::RemoveEntry {
            key: key.clone(),
            edit: Box::new(edit),
        },
    }));
}

/// The remove confirm's message, the approved copy: `remove upstream
/// "<name>"?` / `remove model "<id>"?` / `remove the route at position <n>
/// (model "<m>")?`.
fn remove_confirm_message(view: &crate::gateway_edit::ConfigView, key: &ShuntEntryKey) -> String {
    match key {
        ShuntEntryKey::Upstream { name, .. } => {
            format!("remove upstream \"{}\"?", escape_control(name))
        }
        ShuntEntryKey::Model { id, .. } => format!("remove model \"{}\"?", escape_control(id)),
        ShuntEntryKey::Route(position) => {
            match entry_for_key(view, key) {
                Some(ConfigEntryRef::Route(route)) => remove_route_confirm_message(route),
                // The route the key names left the config since its row
                // rendered: name it by position alone (there is no model left
                // to quote).
                _ => format!("remove the route at position {}?", position),
            }
        }
    }
}

/// The remove confirm's message for a route, built from the route's own view:
/// `remove the route at position <n> (model "<m>")?`, or the position alone
/// when the route has no model (the engine's `RouteView::named` rule,
/// mirrored). Shared by the confirm at open and by the shift that moves an
/// open route confirm when an earlier route's remove lands.
fn remove_route_confirm_message(route: &crate::gateway_edit::RouteView) -> String {
    match route.model.as_ref().map(written_display) {
        Some(model) => format!(
            "remove the route at position {} (model \"{}\")?",
            route.position,
            escape_control(&model)
        ),
        None => format!("remove the route at position {}?", route.position),
    }
}

/// The `Edit::Remove*` one entry key produces; `None` when the route the key
/// names left the config since the confirm opened (a no-op remove).
fn remove_edit(
    view: &crate::gateway_edit::ConfigView,
    key: &ShuntEntryKey,
) -> Option<crate::gateway_edit::Edit> {
    use crate::gateway_edit::Edit;
    match key {
        ShuntEntryKey::Upstream { name, .. } => Some(Edit::RemoveUpstream { name: name.clone() }),
        ShuntEntryKey::Model { id, .. } => Some(Edit::RemoveModel { id: id.clone() }),
        ShuntEntryKey::Route(_) => match entry_for_key(view, key) {
            Some(ConfigEntryRef::Route(route)) => Some(Edit::RemoveRoute {
                route: route.clone(),
            }),
            _ => None,
        },
    }
}

// ── the config field editor ─────────────────────────────────────────────────

/// The entry a field row belongs to; `None` on a server or draft field.
fn field_entry(key: &ShuntFieldKey) -> Option<ShuntEntryKey> {
    match key {
        ShuntFieldKey::Bind | ShuntFieldKey::DefaultProvider | ShuntFieldKey::Draft { .. } => None,
        ShuntFieldKey::Upstream {
            name, occurrence, ..
        } => Some(ShuntEntryKey::Upstream {
            name: name.clone(),
            occurrence: *occurrence,
        }),
        ShuntFieldKey::Model { id, occurrence, .. } => Some(ShuntEntryKey::Model {
            id: id.clone(),
            occurrence: *occurrence,
        }),
        ShuntFieldKey::Route { position, .. } => Some(ShuntEntryKey::Route(*position)),
    }
}

/// Whether a field row's keys (`↵`, `space`, typing, the cycle pick) are inert
/// while its save — its own, its draft's `save <kind>`, or its entry's
/// `remove <kind>` — is checking. The one lock the field editor, the pick
/// stepper and the footer read, so a busy row's hint and lock can never
/// disagree.
pub(crate) fn field_locked(app: &App, key: &ShuntFieldKey) -> bool {
    match key {
        ShuntFieldKey::Draft { kind, .. } => app.services.saving(&ShuntSaveKey::SaveAdd(*kind)),
        _ => {
            app.services.saving(&ShuntSaveKey::Field(key.clone()))
                || field_entry(key)
                    .is_some_and(|entry| app.services.saving(&ShuntSaveKey::Remove(entry)))
        }
    }
}

/// Whether `key` is a rename field (upstream `name`, model `id`), whose save
/// dry-runs the cascade and confirms before writing.
fn is_rename_field(key: &ShuntFieldKey) -> bool {
    matches!(
        key,
        ShuntFieldKey::Upstream {
            field: UpstreamFieldKind::Name,
            ..
        } | ShuntFieldKey::Model {
            field: ModelFieldKind::Id,
            ..
        }
    )
}

/// Open one field's typed editor, seeded from the saved value (`None` = unset).
/// Both `↵` and `space` open a typed row; a cycle row's `↵` at rest opens the
/// same custom-value editor. A row whose save is still checking never reopens a
/// fresh editor over the file's value: `↵`/`space` are inert there, so a
/// refusal landing later toasts instead of describing text the editor no
/// longer shows.
fn open_shunt_field_editor(app: &mut App, key: &ShuntFieldKey) {
    if field_locked(app, key) {
        return;
    }
    let saved = config_view(app)
        .and_then(|view| shunt_field_value(view, &app.services.shunt_drafts, key).flatten());
    let input = InputState::new(saved.as_deref().unwrap_or(""));
    app.services.shunt_field_edit = Some(ShuntFieldEditState {
        key: key.clone(),
        edit: ShuntFieldEdit::Typed { input, saved },
        refusal: None,
    });
}

/// The next cycle pick after the current value, wrapping; a value outside the
/// set (or none) lands on the first option.
fn next_pick(options: &[&str], current: Option<&str>) -> String {
    match current.and_then(|c| options.iter().position(|o| *o == c)) {
        Some(i) => options[(i + 1) % options.len()].to_string(),
        None => options[0].to_string(),
    }
}

/// `space` on a cycle row: move the pick to the next option (nothing written),
/// or advance an already-pending pick. A row whose save is still checking never
/// opens a fresh pick (same inert rule as the typed editor).
fn step_shunt_pick(app: &mut App, key: &ShuntFieldKey) {
    if field_locked(app, key) {
        return;
    }
    let Some(value) =
        config_view(app).and_then(|view| shunt_field_value(view, &app.services.shunt_drafts, key))
    else {
        return;
    };
    let options = cycle_options(key);
    let (current, saved) = match &app.services.shunt_field_edit {
        Some(state) if state.key == *key => match &state.edit {
            ShuntFieldEdit::Pick { saved, picked } => (Some(picked.clone()), saved.clone()),
            _ => (value.clone(), value),
        },
        _ => (value.clone(), value),
    };
    let picked = next_pick(options, current.as_deref());
    app.services.shunt_field_edit = Some(ShuntFieldEditState {
        key: key.clone(),
        edit: ShuntFieldEdit::Pick { saved, picked },
        refusal: None,
    });
}

/// Keystrokes while a config field editor is open: `↵` commits, `esc` restores
/// the saved value and leaves the editor, `space` steps a pending cycle pick,
/// every other key edits the typed buffer (a pending pick takes neither).
fn handle_shunt_field_edit_key(app: &mut App, key: KeyEvent) {
    let Some(state) = app.services.shunt_field_edit.clone() else {
        return;
    };
    // While this row's save is checking, its editor is locked: no key edits
    // its text or closes it (the footer drops its edit hints), but focus may
    // still move away — the outcome then toasts instead of the `└` line. `↑↓`
    // step off the row (closing the editor) and `q` ascends to the list; both
    // stay claimed here so neither reaches a global binding that would leave
    // the editor open on a list-focused screen.
    if field_locked(app, &state.key) {
        match key.code {
            KeyCode::Up | KeyCode::Down => {
                app.services.shunt_field_edit = None;
                shunt_step(app, key.code == KeyCode::Down);
            }
            KeyCode::Char('q') => leave_shunt_detail(app),
            _ => {}
        }
        return;
    }
    match key.code {
        KeyCode::Esc => {
            app.services.shunt_field_edit = None;
        }
        KeyCode::Enter => commit_shunt_field(app, &state.key),
        KeyCode::Char(' ') if matches!(state.edit, ShuntFieldEdit::Pick { .. }) => {
            step_shunt_pick(app, &state.key);
        }
        _ => {
            if let Some(s) = app.services.shunt_field_edit.as_mut()
                && let ShuntFieldEdit::Typed { input, .. } = &mut s.edit
            {
                apply_input_edit(input, key);
            }
        }
    }
}

/// `↵` in a field editor: build the edit and either save it (through a confirm
/// for a bind restart or a cascading rename) or show the dry run's refusal.
fn commit_shunt_field(app: &mut App, key: &ShuntFieldKey) {
    let Some(state) = app.services.shunt_field_edit.clone() else {
        return;
    };
    let typed = match &state.edit {
        ShuntFieldEdit::Typed { input, .. } => input.value.clone(),
        ShuntFieldEdit::Pick { picked, .. } => picked.clone(),
    };
    // A draft field's commit lands in the draft, never the file: set the value
    // (empty unsets), clear the `save <kind>` refusal, close the editor, and
    // rebuild the stops (a committed auth mode can change the mode-dependent
    // row set).
    if let ShuntFieldKey::Draft { kind, field } = key {
        set_draft_field_value(&mut app.services.shunt_drafts, *kind, field, &typed);
        app.services.shunt_drafts.clear_refusal(*kind);
        app.services.shunt_field_edit = None;
        rebuild_shunt_config_stops(app);
        return;
    }
    let Some(record) = app.services.shunt_record.clone() else {
        return;
    };
    let Some(view) = config_view(app).cloned() else {
        return;
    };
    let Some(edit) = edit_for_field(&view, key, &typed) else {
        return;
    };

    // A rename dry-runs the same edit (no write, no check) and counts what
    // else it touches; any count confirms first.
    if is_rename_field(key) {
        match crate::gateway_edit::plan_cascade(&record, &edit) {
            Ok(Some(cascade)) if cascade_has_refs(&cascade, key) => {
                let counts = rename_counts_line(&cascade, key);
                let kind = match key {
                    ShuntFieldKey::Upstream { .. } => "upstream",
                    _ => "model",
                };
                let from = rename_from(key);
                app.disarm_quit();
                app.open_modal(Modal::Confirm(ConfirmState {
                    message: format!(
                        "rename {kind} \"{from}\" to \"{}\"?",
                        escape_control(&typed)
                    ),
                    detail: Some(format!("also renames it in {counts}.")),
                    choice: false,
                    scroll: ConfirmScroll::default(),
                    on_confirm: ConfirmAction::RenameField {
                        key: key.clone(),
                        edit: Box::new(edit),
                    },
                }));
                return;
            }
            Err(e) => {
                show_field_refusal(app, key, &e);
                return;
            }
            Ok(_) => {}
        }
    }

    // A bind save restarts the gateway: confirm while supervised and running
    // (and the value actually changed — an equal-value save writes nothing;
    // an emptied bind over an unset one is no change either).
    if key == &ShuntFieldKey::Bind {
        let changed = match &state.edit {
            ShuntFieldEdit::Typed { saved, .. } => saved.as_deref().unwrap_or("") != typed.as_str(),
            ShuntFieldEdit::Pick { .. } => true,
        };
        if changed && bind_save_needs_confirm(app) {
            let detail = bind_restart_detail(app, &typed);
            app.disarm_quit();
            app.open_modal(Modal::Confirm(ConfirmState {
                message: "restart shunt?".to_string(),
                detail: Some(detail),
                choice: false,
                scroll: ConfirmScroll::default(),
                on_confirm: ConfirmAction::RestartBind {
                    key: key.clone(),
                    edit: Box::new(edit),
                },
            }));
            return;
        }
    }

    start_shunt_save(app, ShuntSaveKey::Field(key.clone()), edit, false);
}

/// The rename confirm's `from`: the entry's current name/id, control-escaped
/// like every config-derived string a modal shows.
fn rename_from(key: &ShuntFieldKey) -> String {
    match key {
        ShuntFieldKey::Upstream { name, .. } => escape_control(name),
        ShuntFieldKey::Model { id, .. } => escape_control(id),
        _ => String::new(),
    }
}

/// Whether a rename's cascade touches any other reference (a zero-count rename
/// saves like any field).
fn cascade_has_refs(cascade: &crate::gateway_edit::Cascade, key: &ShuntFieldKey) -> bool {
    match key {
        ShuntFieldKey::Upstream { .. } => {
            cascade.routes
                + cascade.models
                + cascade.default_provider
                + cascade.route_prefixes
                + cascade.codex_endpoint
                + cascade.codex_routes
                > 0
        }
        ShuntFieldKey::Model { .. } => {
            cascade.route_models + cascade.routers + cascade.subagents > 0
        }
        _ => false,
    }
}

/// The rename confirm's count line: singular at 1, a kind at 0 left out, joined
/// by ` · `.
fn rename_counts_line(cascade: &crate::gateway_edit::Cascade, key: &ShuntFieldKey) -> String {
    let count = |n: usize, one: &str, many: &str| match n {
        0 => None,
        1 => Some(one.to_string()),
        n => Some(format!("{n} {many}")),
    };
    let mut parts = Vec::new();
    match key {
        ShuntFieldKey::Upstream { .. } => {
            parts.extend(count(cascade.routes, "1 route", "routes"));
            parts.extend(count(
                cascade.default_provider,
                "default_provider",
                "default_provider",
            ));
            parts.extend(count(cascade.models, "1 model mapping", "model mappings"));
            parts.extend(count(
                cascade.route_prefixes,
                "1 route prefix",
                "route prefixes",
            ));
            parts.extend(count(
                cascade.codex_endpoint,
                "codex_endpoint",
                "codex_endpoint",
            ));
            parts.extend(count(cascade.codex_routes, "1 codex route", "codex routes"));
        }
        ShuntFieldKey::Model { .. } => {
            parts.extend(count(cascade.route_models, "1 route", "routes"));
            parts.extend(count(cascade.routers, "1 router target", "router targets"));
            parts.extend(count(
                cascade.subagents,
                "1 subagent target",
                "subagent targets",
            ));
        }
        _ => {}
    }
    parts.join(" · ")
}

/// Whether a bind save needs the restart confirm: the gateway is supervised and
/// running (`starting`, `healthy` or `unhealthy`).
fn bind_save_needs_confirm(app: &App) -> bool {
    use crate::daemon::gateway::GatewayState;
    let (slot, supervised) = crate::daemon::gateway_slot(app.daemon_health);
    supervised
        && matches!(
            slot.state,
            GatewayState::Starting | GatewayState::Healthy | GatewayState::Unhealthy
        )
}

/// The live-session count clause both the `stop shunt?` and the `restart
/// shunt?` confirms lead with: `1 live session uses it; ` at one, `N live
/// sessions use it; ` above one, absent at 0 and when clauth cannot count.
fn live_sessions_clause(app: &App) -> String {
    match gateway_session_count(app) {
        Some(1) => "1 live session uses it; ".to_string(),
        Some(n) if n > 1 => format!("{n} live sessions use it; "),
        Some(_) | None => String::new(),
    }
}

/// The `restart shunt?` detail, its live-session count the `stop shunt?` way
/// (the count clause absent at 0 and when clauth cannot count). An emptied bind
/// reads the target as shunt's default, never a bare trailing "on .".
fn bind_restart_detail(app: &App, bind: &str) -> String {
    let target = if bind.is_empty() {
        format!("shunt's default, {}", crate::gateway::SHUNT_DEFAULT_BIND)
    } else {
        escape_control(bind)
    };
    format!(
        "bind is read at start; {}in-flight requests drain, then it starts on {target}.",
        live_sessions_clause(app)
    )
}

/// Show a save's failure: an [`EditRefusal`] renders its own sentence on the
/// field's `└` line (the editor kept open with the typed text); any other
/// failure — an engine bug or an untyped read error — toasts `save failed` +
/// the chain, never the `└` line.
fn show_field_refusal(app: &mut App, key: &ShuntFieldKey, err: &anyhow::Error) {
    if let Some(refusal) = err.downcast_ref::<crate::gateway_edit::EditRefusal>() {
        if let Some(state) = app.services.shunt_field_edit.as_mut()
            && state.key == *key
        {
            state.refusal = Some(refusal.to_string());
        }
    } else {
        app.toast(
            ToastKind::Danger,
            format!("save failed\n{}", escape_control(&error_toast_detail(err))),
        );
    }
}

/// Start `job` on the one save worker under the one-save guard (the same slot
/// every config save, token add and token remove shares; a second toasts the
/// guard). `value`/`entry` are the [`ShuntSave`] fields the config-save path
/// carries for its row; token jobs pass none. A field's editor stays open with
/// the typed text until the outcome lands.
fn start_on_save_worker(
    app: &mut App,
    key: ShuntSaveKey,
    value: Option<String>,
    entry: Option<ShuntEntryKey>,
    job: ShuntActionJob,
) {
    if app.services.shunt_save_worker.running {
        app.toast(
            ToastKind::Info,
            "a save is still checking\nwait for it, then save again",
        );
        return;
    }
    app.services.shunt_save = Some(ShuntSave {
        key,
        started: Instant::now(),
        value,
        entry,
    });
    app.services.shunt_save_worker.start(job);
}

/// Start the save worker for one config row (the outcome drains on the tick).
fn start_shunt_save(
    app: &mut App,
    key: ShuntSaveKey,
    edit: crate::gateway_edit::Edit,
    confirmed: bool,
) {
    let Some(record) = app.services.shunt_record.clone() else {
        return;
    };
    let entry = add_edit_entry_key(&edit, config_view(app));
    start_on_save_worker(
        app,
        key.clone(),
        edit_save_value(&edit),
        entry,
        ShuntActionJob::ConfigEdit {
            record,
            edit: Box::new(edit),
            key,
            confirmed,
        },
    );
}

/// Start a token remove on the one save worker (the same one-save slot every
/// config save, add, remove and token op shares; a second toasts the guard).
fn start_shunt_token_save(
    app: &mut App,
    key: ShuntSaveKey,
    record: crate::gateway::GatewayRecord,
    profile: String,
) {
    start_on_save_worker(
        app,
        key,
        None,
        None,
        ShuntActionJob::RemoveToken { record, profile },
    );
}

/// Start a token add on the one save worker: the `+ add token` row holds the
/// save slot while the mint and the profile write run.
fn start_shunt_token_add(
    app: &mut App,
    record: crate::gateway::GatewayRecord,
    profile: String,
    url: String,
) {
    let config = JobConfig(Arc::clone(&app.config));
    start_on_save_worker(
        app,
        ShuntSaveKey::AddToken,
        None,
        None,
        ShuntActionJob::AddToken {
            record,
            profile,
            url,
            config,
        },
    );
}

/// Ascend off the shunt detail: close any open field editor, retire a remove
/// refusal (its row is no longer the user's), and move focus to the list.
/// Every site that moves `services.focus` off the shunt detail (the
/// global `esc` and `q` ascend arms, the tab jump's Services reset, and the
/// locked editor's own `q`) goes through here, so the state "list focused,
/// editor open" cannot exist — an editor left open would keep owning the
/// keyboard on a pane that does not hold focus.
fn leave_shunt_detail(app: &mut App) {
    app.services.shunt_field_edit = None;
    app.services.shunt_remove_refusal.take();
    app.services.shunt_token_refusal.take();
    app.services.focus = ServicesFocus::List;
}

/// `esc` on a config row that is not editing: fold its entry (back to the
/// entry row), or fold its open draft (back to the `+ add` row) — the Setup
/// `+ new` form's precedent, whose `esc` at rest drops the draft. Returns
/// `true` when it folded; a server field row (or no config focus) leaves it to
/// ascend as today.
fn shunt_esc_folds_entry(app: &mut App) -> bool {
    let Some(focus) = app.services.focused_shunt_focus().cloned() else {
        return false;
    };
    let fold_draft = |app: &mut App, kind: ShuntAddKind| {
        app.services.shunt_drafts.fold(kind);
        rebuild_shunt_config_stops(app);
        app.services
            .set_shunt_focus(Some(ShuntFocusLine::Add(kind)));
    };
    match focus {
        ShuntFocus::Field { key } => {
            // A locked row's `esc` ascends to the list (no fold) — ruling 2.
            if field_locked(app, &key) {
                return false;
            }
            if let ShuntFieldKey::Draft { kind, .. } = &key {
                fold_draft(app, *kind);
                return true;
            }
            let Some(entry) = field_entry(&key) else {
                return false;
            };
            app.services.shunt_expanded.remove(&entry);
            retire_remove_refusal(app, &entry);
            rebuild_shunt_config_stops(app);
            app.services
                .set_shunt_focus(Some(ShuntFocusLine::Entry(entry)));
            true
        }
        ShuntFocus::SaveAdd { kind } => {
            if app.services.saving(&ShuntSaveKey::SaveAdd(kind)) {
                return false;
            }
            fold_draft(app, kind);
            true
        }
        ShuntFocus::Add { kind } if app.services.shunt_drafts.is_open(kind) => {
            if app.services.saving(&ShuntSaveKey::SaveAdd(kind)) {
                return false;
            }
            fold_draft(app, kind);
            true
        }
        ShuntFocus::Remove { key } => {
            // `esc` on the `remove <kind>` row folds its entry like C2's field
            // rows, unless the remove is locked (ruling 2): then it ascends.
            if app.services.saving(&ShuntSaveKey::Remove(key.clone())) {
                return false;
            }
            app.services.shunt_expanded.remove(&key);
            retire_remove_refusal(app, &key);
            rebuild_shunt_config_stops(app);
            app.services
                .set_shunt_focus(Some(ShuntFocusLine::Entry(key)));
            true
        }
        _ => false,
    }
}

/// Open the read-only `check output` modal over the held failed-check stderr,
/// which `crate::gateway::run_check` already masked. The body is split into its
/// real lines, ANSI CSI sequences stripped (shunt's tracing may emit SGR), and
/// each line control-escaped here; nothing is re-read or re-masked.
fn open_check_output_modal(app: &mut App) {
    let Some(held) = app.services.check_output.as_ref() else {
        return;
    };
    let body = stderr_display_lines(held.stderr.text()).join("\n");
    app.check_output_scroll = 0;
    app.open_modal(Modal::CheckOutput(body));
}

/// The held stderr as display lines: ANSI CSI sequences (`ESC [ … final`) that
/// shunt's tracing layer may emit are stripped, then each line is
/// control-escaped so no `\n` or escape byte reaches a cell. The modal's own
/// `.lines()` split is what turns the joined newlines back into rows.
fn stderr_display_lines(text: &str) -> Vec<String> {
    strip_ansi_csi(text).lines().map(escape_control).collect()
}

/// Strip ANSI CSI sequences (`ESC [ params/intermediates final`, params in
/// `0x20..=0x3F`, final in `0x40..=0x7E`) from `text`, so the modal shows
/// text, not terminal escape bytes. Any byte outside those ranges before the
/// final byte ends the sequence without eating text: the `ESC` is kept and
/// scanning resumes after it, so a malformed CSI never swallows a line.
fn strip_ansi_csi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(esc) = rest.find('\x1b') {
        out.push_str(&rest[..esc]);
        let after = &rest[esc + 1..];
        if let Some(body) = after.strip_prefix('[') {
            match body
                .char_indices()
                .find(|(_, c)| ('@'..='~').contains(c) || !('\u{20}'..='\u{3f}').contains(c))
            {
                Some((at, c)) if ('@'..='~').contains(&c) => rest = &body[at + c.len_utf8()..],
                _ => {
                    // No final byte, or a non-parameter byte: not a CSI.
                    out.push('\x1b');
                    rest = after;
                }
            }
        } else {
            out.push('\x1b');
            rest = after;
        }
    }
    out.push_str(rest);
    out
}

/// Apply the focused fix. Every mutating fix opens a confirm modal; a diverged
/// active profile's repair lives on the divergence resolver's own `d` prompt,
/// never on this tab. Only the four shunt fixes are gated by an in-flight shunt
/// action — a second shunt press starts nothing, while the plugin and herdr
/// fixes keep working.
fn apply_service_fix(app: &mut App) {
    let Some(fix) = app.services.focused_fix().cloned() else {
        return;
    };
    if app.services.shunt_action.running
        && matches!(
            fix,
            ServiceFix::AdoptConfig(_)
                | ServiceFix::MoveStoresIn
                | ServiceFix::AddAdminKey
                | ServiceFix::AddAdminTable
        )
    {
        return;
    }
    match fix {
        ServiceFix::WireMcpServers => {
            app.disarm_quit();
            app.open_modal(Modal::Confirm(ConfirmState {
                message: "wire clauth into claude code's mcpServers?".to_string(),
                detail: Some(
                    "writes the clauth entry into ~/.claude.json; other fields are preserved."
                        .to_string(),
                ),
                choice: false,
                scroll: ConfirmScroll::default(),
                on_confirm: ConfirmAction::WireMcpServers,
            }));
        }
        ServiceFix::HealHerdrConfig(path) => {
            app.disarm_quit();
            app.open_modal(Modal::Confirm(ConfirmState {
                message: "add the keybinding and sidebar row to herdr's config?".to_string(),
                detail: Some(
                    concat!(
                        "writes them into herdr's config.toml and validates the result with ",
                        cmd_lit!("herdr config check"),
                        "."
                    )
                    .to_string(),
                ),
                choice: false,
                scroll: ConfirmScroll::default(),
                on_confirm: ConfirmAction::HealHerdrConfig(path),
            }));
        }
        ServiceFix::InstallPlugin => {
            app.disarm_quit();
            app.open_modal(Modal::Confirm(ConfirmState {
                message: "install the clauth plugin into claude code?".to_string(),
                detail: Some(
                    "runs claude's own plugin installer at user scope and turns the plugin on; your other plugins and settings are untouched."
                        .to_string(),
                ),
                choice: false,
                scroll: ConfirmScroll::default(),
                on_confirm: ConfirmAction::InstallPlugin,
            }));
        }
        ServiceFix::AdoptConfig(path) => {
            app.disarm_quit();
            app.open_modal(Modal::Confirm(ConfirmState {
                message: format!("adopt {}?", escape_control(&path.to_string_lossy())),
                detail: Some("clauth runs shunt on this config and edits it in place.".to_string()),
                choice: false,
                scroll: ConfirmScroll::default(),
                on_confirm: ConfirmAction::AdoptConfig(path),
            }));
        }
        ServiceFix::MoveStoresIn => open_move_confirm(app),
        ServiceFix::AddAdminKey => {
            app.disarm_quit();
            app.open_modal(Modal::Confirm(ConfirmState {
                message: "add clauth's admin key to this config?".to_string(),
                detail: Some(
                    "clauth needs it to manage pool accounts and read their usage.".to_string(),
                ),
                choice: false,
                scroll: ConfirmScroll::default(),
                on_confirm: ConfirmAction::AddAdminKey,
            }));
        }
        ServiceFix::AddAdminTable => {
            app.disarm_quit();
            app.open_modal(Modal::Confirm(ConfirmState {
                message: "add a [server.admin] table with clauth's key?".to_string(),
                detail: Some("the gateway restarts once to load it.".to_string()),
                choice: false,
                scroll: ConfirmScroll::default(),
                on_confirm: ConfirmAction::AddAdminTable,
            }));
        }
    }
}

/// The move confirm: the two counts off the plan the card showed, carried in
/// the confirm action so the move runs exactly that plan. The `<K> stay
/// behind` line is absent at K = 0, and both lines take the singular at 1.
fn open_move_confirm(app: &mut App) {
    let plan = match &app.services.move_plan {
        Some(MovePlanOutcome::Plan(plan)) => plan.clone(),
        _ => return,
    };
    let n = plan.moved.len();
    let k = plan.kept.len();
    app.disarm_quit();
    app.open_modal(Modal::Confirm(ConfirmState {
        message: format!("move {n} {} into clauth?", account_files_word(n)),
        detail: (k > 0).then(|| format!("{k} {} behind, listed on the card.", stays_word(k))),
        choice: false,
        scroll: ConfirmScroll::default(),
        on_confirm: ConfirmAction::MoveStoresIn(plan),
    }));
}

/// The `account files`/`account file` noun, singular at 1 (the owner's copy).
fn account_files_word(n: usize) -> &'static str {
    if n == 1 {
        "account file"
    } else {
        "account files"
    }
}

/// The `stay`/`stays` verb, singular at 1 (the owner's copy).
fn stays_word(k: usize) -> &'static str {
    if k == 1 { "stays" } else { "stay" }
}

/// The herdr detail's keymap: ↑↓ walks the `f  heal herdr config` line (while
/// the check offers it, the stop above the first option) and the six option
/// rows, wrapping; `f` walks to the fix line; `↵` on it opens the heal
/// confirm; space and ⏎ activate a focused option, `+`/`-` step the
/// tag-refresh stepper. Only the herdr detail routes here — every other
/// detail keeps the scroll-only keymap.
fn handle_herdr_options_key(app: &mut App, key: KeyEvent) {
    let has_fix = app
        .services
        .selected_check()
        .is_some_and(|c| !c.problems.is_empty());
    if !has_fix {
        app.services.herdr_fix_focus = false;
    }
    let on_fix = app.services.herdr_fix_focus;
    let cursor = app.services.herdr_options_cursor;
    let rows = HERDR_OPTIONS.len();
    match key.code {
        KeyCode::Up => {
            app.services.diagnostic_focus = false;
            if on_fix {
                app.services.herdr_fix_focus = false;
                app.services.herdr_options_cursor = rows - 1;
            } else if has_fix && cursor == 0 {
                app.services.herdr_fix_focus = true;
            } else {
                app.services.herdr_options_cursor = (cursor + rows - 1) % rows;
            }
        }
        KeyCode::Down => {
            app.services.diagnostic_focus = false;
            if on_fix {
                app.services.herdr_fix_focus = false;
                app.services.herdr_options_cursor = 0;
            } else if has_fix && cursor == rows - 1 {
                app.services.herdr_fix_focus = true;
            } else {
                app.services.herdr_options_cursor = (cursor + 1) % rows;
            }
        }
        KeyCode::Char('f') => walk_to_next_fix(app),
        KeyCode::Enter if on_fix => apply_service_fix(app),
        KeyCode::Enter | KeyCode::Char(' ') if !app.services.diagnostic_focus && !on_fix => {
            activate_herdr_option(app, HERDR_OPTIONS[cursor]);
        }
        KeyCode::Char('+') if !app.services.diagnostic_focus && !on_fix => {
            step_herdr_tag_refresh(app, 1);
        }
        KeyCode::Char('-') if !app.services.diagnostic_focus && !on_fix => {
            step_herdr_tag_refresh(app, -1);
        }
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
    app.services.herdr_config.as_ref().is_some_and(|c| c.parsed)
}

/// `+`/`-` on the tag-refresh stepper: ±1 second, floored at 1. A no-op on
/// every other row.
fn step_herdr_tag_refresh(app: &mut App, delta: i64) {
    if HERDR_OPTIONS[app.services.herdr_options_cursor] != HerdrOption::TagRefresh {
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

/// `bin pane list` → pane ids, parsed by the one typed parser the pane
/// reporter shares: an unknown payload shape (a herdr release moving a field)
/// reads as no panes rather than a wrong one. Bounded at
/// `crate::herdr::PROBE_TIMEOUT` — a hung herdr delays the push worker, never
/// the key handling.
fn herdr_pane_ids(bin: &str) -> Option<Vec<String>> {
    let out = crate::herdr::bounded_output(bin, &["pane", "list"], &[])?;
    if !out.status.success() {
        return None;
    }
    crate::herdr::parse_pane_list(&out.stdout)
        .map(|panes| panes.into_iter().map(|pane| pane.pane_id).collect())
}

/// Re-run `report-profile.sh` for one pane with the pane id set and the
/// event/context JSON cleared, so the script reads the pane's live agent
/// itself. The script's pidfile gate skips the watcher spawn while a live
/// watch exists, so re-running it alongside the live watchers is safe.
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
    app.services.herdr_tag_draft = Some(InputState::new(&secs.to_string()));
}

/// Keystrokes while the herdr tag-refresh editor is open: ⏎ saves, ⎋ discards.
fn handle_herdr_tag_edit_key(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Esc => app.services.herdr_tag_draft = None,
        KeyCode::Enter => commit_herdr_tag_edit(app),
        _ => {
            if let Some(input) = app.services.herdr_tag_draft.as_mut() {
                apply_input_edit(input, key);
            }
        }
    }
}

/// Parse and persist the typed tag-refresh seconds. A value under 1s keeps the
/// draft open so the inline Invalid-input treatment stays on screen — no toast.
fn commit_herdr_tag_edit(app: &mut App) {
    let Some(raw) = app.services.herdr_tag_draft.as_ref().map(|i| i.trimmed()) else {
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
    app.services.herdr_tag_draft = None;
}

/// A typed tag-refresh value is valid only as a whole number of seconds, at
/// least 1 — shared by the commit path and the render's inline check.
pub(crate) fn parse_herdr_tag_secs(raw: &str) -> Option<u64> {
    let secs = raw.parse::<u64>().ok()?;
    (secs >= 1).then_some(secs)
}

/// Open the confirm before flipping `delegate row text`: the flip rewrites
/// herdr's own config (the row `clauth herdr install` appended), so it carries
/// the same confirm gate as the heal fix. The copy names the delegate token
/// so the confirm says what it will write; cancel is the default choice.
fn open_herdr_row_text_confirm(app: &mut App) {
    let Some(path) = app
        .services
        .herdr
        .as_ref()
        .and_then(|probe| probe.as_ref())
        .and_then(|probe| probe.config_path.clone())
    else {
        return;
    };
    let turning_on = !app.config().state.herdr.delegate_row_text;
    app.disarm_quit();
    app.open_modal(Modal::Confirm(ConfirmState {
        message: if turning_on {
            "add the delegate token to herdr's sidebar row?".to_string()
        } else {
            "drop the delegate token from herdr's sidebar row?".to_string()
        },
        detail: Some(if turning_on {
            concat!(
                "writes $clauth_delegate into the row clauth added in herdr's config.toml, validated by ",
                cmd_lit!("herdr config check"),
                "; a hand-owned row is left alone with a note."
            )
            .to_string()
        } else {
            concat!(
                "rewrites the row clauth added in herdr's config.toml without $clauth_delegate, validated by ",
                cmd_lit!("herdr config check"),
                "; a hand-owned row is left alone with a note."
            )
            .to_string()
        }),
        choice: false,
        scroll: ConfirmScroll::default(),
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

/// The Services tab's `herdr` row: the installed herdr's clauth plugin plus the
/// keybinding/sidebar config `clauth herdr install` adds, read root, plugin,
/// herdr version, key, sidebar, then the fix. Pure so the verdict
/// logic unit-tests without an `App`; the caller supplies the probe and the
/// config readout (`None` when the config file could not be read at all).
pub(crate) fn herdr_check(
    probe: &crate::herdr::HerdrProbe,
    config: Option<&crate::herdr::ConfigStatus>,
) -> Check {
    let mut detail = Vec::new();
    let herdr_line = match &probe.version {
        Some(version) => format!("herdr: {version}"),
        None => "herdr: unknown".to_string(),
    };

    let mut danger = false;
    let mut warn = false;
    let mut fixed = None;
    let mut problems: Vec<Problem> = Vec::new();

    // Indented, because herdr's own prose carries colons ("manifest unavailable: No such file or directory") and `detail_line` splits the first `": "` into a key column: left flush, a warning renders as a field named after its first clause and widens that column for every real field above it.
    let error_line = probe
        .error
        .as_ref()
        .map(|error| format!("  {}", prose::mark_commands(error)));
    if error_line.is_some() {
        danger = true;
    }

    if let Some(entry) = &probe.entry {
        // `herdr plugin list --json` spells a linked plugin's source `local`, measured against 0.8.0; `link` is the verb that creates one, not the kind it reports.
        let local = entry.source_kind.as_deref() == Some("local");

        if !entry.warnings.is_empty() {
            danger = true;
        }
        if local && let Some(root) = &entry.plugin_root {
            detail.push(format!("root: {root}"));
        }
        if !entry.enabled {
            warn = true;
            detail.push("plugin: disabled".to_string());
        } else if local {
            detail.push("plugin: linked (local)".to_string());
        } else {
            detail.push("plugin: installed (github)".to_string());
        }
        for warning in &entry.warnings {
            detail.push(format!("  {warning}"));
        }
        detail.push(herdr_line);
        detail.extend(error_line);
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
            detail.push("adds the keybinding and sidebar row to herdr's config".to_string());
            let line = detail.len();
            let fix = probe.config_path.clone().map(ServiceFix::HealHerdrConfig);
            if let Some(f) = &fix {
                detail.push(fix_line(f));
                problems.push(Problem {
                    line,
                    fix: f.clone(),
                });
            }
            fixed = fix;
        }
    } else if let Some(error_line) = error_line {
        detail.push(herdr_line);
        detail.push(error_line);
    } else {
        warn = true;
        detail.push("plugin: not installed".to_string());
        detail.push(herdr_line);
        detail.push(String::new());
        detail.push(format!("  {}", prose::cmd("clauth herdr install")));
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
        fix: fixed,
        problems,
        shunt_focus: Vec::new(),
        shunt_action_start: None,
    }
}

/// Whether a recompute re-runs the shunt card's plan and pool workers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ShuntRefresh {
    /// Cheap recompute: start a worker only when its cache is empty.
    Keep,
    /// Drop the plan/pool caches and restart their workers (respecting their
    /// gates), so "until it lands the card shows nothing for the plan" holds.
    Restart,
}

/// Recompute the Services tab's rows: `shunt` (the managed gateway's readout),
/// `delegates` (its detail is the job list), `plugin` (the four integration
/// readouts in one detail), `herdr`. Every read is a local FS/`PATH` check;
/// `claude --version` and the `clauth mcp` boot probe run only when
/// `refresh_version` is set. Synchronous, except the herdr probe, which it
/// starts on a worker when none is cached or on `r`, and the shunt plan/pool
/// workers, which [`ShuntRefresh::Restart`] re-runs on tab entry and after a
/// shunt action lands.
pub(crate) fn recompute_services_checks(app: &mut App, refresh_version: bool, shunt: ShuntRefresh) {
    use crate::plugin_probe as probe;

    app.services.error = None;

    // A daemon start, stop or restart moves the plan's env source (the running
    // daemon's record vs this process's own env), so the cached plan is invalid
    // the moment the source moves: restart the plan/pool workers exactly as
    // `Restart` does. The first recompute just records the source.
    let source = PlanEnvSource::of(app.daemon_health);
    let daemon_flipped = app
        .services
        .last_plan_env_source
        .as_ref()
        .is_some_and(|last| *last != source);
    app.services.last_plan_env_source = Some(source);
    let shunt = if daemon_flipped {
        ShuntRefresh::Restart
    } else {
        shunt
    };

    // CC version is cached; only `r` probes. Construction and a tab switch
    // leave it unprobed rather than spawning `claude --version` synchronously
    // — construction must not block the first paint. Skipped under test so
    // the suite never spawns the real `claude` binary.
    if refresh_version {
        app.services.fetching = true;
        app.services.cc_version = Some(if cfg!(test) {
            None
        } else {
            probe::cc_version()
        });
        app.services.fetching = false;
    }

    // `clauth mcp` boot self-probe — `r`-gated only (heavier than the other reads:
    // it spawns the real server). Cleared when clauth no longer resolves so a stale
    // "ok" can't linger. Skipped under test so the suite never boots the server.
    let clauth_path = probe::on_path("clauth");
    if refresh_version {
        app.services.fetching = true;
        app.services.mcp_boot = if clauth_path.is_some() {
            Some(if cfg!(test) {
                probe::McpProbe::Ok
            } else {
                probe::mcp_boots()
            })
        } else {
            None
        };
        app.services.fetching = false;
    }

    // herdr probe — three subprocesses, so it runs on a worker: the first
    // recompute with nothing cached starts it (the row appears with no key),
    // `r` re-probes.
    if refresh_version {
        app.services.herdr_probe.restart();
    } else if app.services.herdr.is_none() {
        app.services.herdr_probe.start();
    }

    let mut checks: Vec<Check> = Vec::with_capacity(4);

    // shunt row — the gateway's control surface: the daemon's feed when one is
    // fresh, else the record-only verdict. With no gateway adopted, the
    // standalone probe says what shunt would load and what answers. The record
    // and the admin need are cheap local reads on the recompute; the store
    // move plan and the codex pool read run on workers.
    let (gateway_slot, gateway_supervised) = crate::daemon::gateway_slot(app.daemon_health);
    app.set_gateway_state(gateway_slot.state);
    if gateway_slot.state == GatewayState::Absent {
        // The readout's bind comes from the env the adopt reads, which moves
        // with the env source.
        if refresh_version || daemon_flipped {
            app.services.standalone_probe.restart();
        } else if app.services.standalone.is_none() {
            app.services.standalone_probe.start();
        }
    }
    let record = crate::gateway::GatewayRecord::load().ok().flatten();
    if record != app.services.shunt_record {
        // The last refusal belonged to the record it edited against; a record
        // change (a re-adopt, a binary/env move) retires it.
        app.services.check_output = None;
    }
    app.services.shunt_record = record;
    // The adopted config's editor view: a cheap local read on the recompute,
    // like the record and the admin need. `None` while no record exists (the
    // absent card shows no config section); a config read/parse failure keeps
    // the groups off and renders one `config  not read (<reason>)` line, a
    // shape refusal `config  not editable` with its sentence. An env file the
    // view cannot read still reads the config (the view carries it). The
    // expanded set drops keys whose entry left the config, only over a read
    // view — a failed read says nothing about the entries.
    app.services.shunt_config = app.services.shunt_record.as_ref().map(|record| {
        match crate::gateway_edit::read_view(record) {
            Ok(view) => ShuntConfigRead::Ok(Box::new(view)),
            Err(e) => classify_shunt_config_read(&e),
        }
    });
    // The client-token section's data, read beside the config view (the same
    // per-tick cadence, the same "never spawn a process" UI-thread reads) and
    // cached so the render draws from it, never the store per frame.
    app.services.shunt_tokens = app
        .services
        .shunt_record
        .as_ref()
        .map(read_shunt_tokens)
        .unwrap_or_default();
    if let Some(ShuntConfigRead::Ok(view)) = &app.services.shunt_config {
        prune_shunt_expanded(view, &mut app.services.shunt_expanded);
    }
    // A remove refusal names a comment in the file it was raised on; a
    // recompute whose view differs (a hand-edit) retires it, so a stale DANGER
    // sentence never outlives the file it described.
    if app
        .services
        .shunt_remove_refusal
        .borrow()
        .as_ref()
        .is_some_and(|refusal| {
            matches!(&app.services.shunt_config, Some(ShuntConfigRead::Ok(view)) if view.as_ref() != refusal.view.as_ref())
        })
    {
        app.services.shunt_remove_refusal.take();
    }
    app.services.admin_need = app
        .services
        .shunt_record
        .as_ref()
        .and_then(|record| crate::gateway::admin_need(record.config()).ok());
    match app.services.admin_need {
        // The admin entry is present (this edit landed, another writer added
        // it, or a fresh adopt already had it): no admin refusal is left to
        // show. A config save's refusal is not answered by it and stays.
        Some(crate::gateway::AdminNeed::Neither) => {
            if app
                .services
                .check_output
                .as_ref()
                .is_some_and(|held| held.source == CheckOutputSource::AdminEdit)
            {
                app.services.check_output = None;
            }
        }
        // No record, or the config is unreadable: the line has no card to sit
        // under.
        None => app.services.check_output = None,
        Some(crate::gateway::AdminNeed::WriteKey | crate::gateway::AdminNeed::AdminTable) => {}
    }

    // The store move plan: on a worker, on tab entry, on `r`, and after any
    // action lands (the outcome drain passes [`ShuntRefresh::Restart`], which
    // drops the cache so the card shows nothing until the fresh plan lands).
    if shunt == ShuntRefresh::Restart {
        app.services.move_plan = None;
        app.services.pool = None;
    }
    if app.services.shunt_record.is_some() {
        if refresh_version || shunt == ShuntRefresh::Restart {
            app.services.move_plan_probe.restart();
        } else if app.services.move_plan.is_none() {
            app.services.move_plan_probe.start();
        }
    } else {
        app.services.move_plan = None;
    }

    // The codex pool note: on a worker, gated on the slot being `healthy` with
    // no admin step owed. Re-probed on tab entry, `r`, and whenever the slot
    // turns healthy (the recompute re-reads the slot each tick).
    let pool_due = gateway_slot.state == GatewayState::Healthy
        && app.services.admin_need == Some(crate::gateway::AdminNeed::Neither);
    if pool_due {
        if refresh_version || shunt == ShuntRefresh::Restart {
            app.services.pool_probe.restart();
        } else if app.services.pool.is_none() {
            app.services.pool_probe.start();
        }
    } else {
        app.services.pool = None;
    }

    let mut shunt = shunt_card(
        &gateway_slot,
        gateway_supervised,
        app.daemon_health == crate::daemon::DaemonHealth::Absent,
        app.services.standalone.as_ref(),
        app.services.shunt_record.as_ref(),
        app.services.admin_need,
        app.services.move_plan.as_ref(),
        app.services.pool.as_ref(),
        app.services.check_output.is_some(),
    );
    // The config rows join the focus walk after the action section, in card
    // order (server fields, then each entry and its expanded fields); the
    // render draws them in the same order, so the published geometry stays
    // parallel to this list.
    if let Some(ShuntConfigRead::Ok(view)) = &app.services.shunt_config {
        shunt.shunt_focus.extend(config_field_stops(
            view,
            &app.services.shunt_expanded,
            &app.services.shunt_drafts,
            &app.services.shunt_tokens,
        ));
    }
    checks.push(shunt);

    // The delegates detail names the profiles whose delegate traffic is
    // rate-limited. Snapshot the names under the config lock, then read each
    // profile's throughput cache outside it.
    let now_secs = (crate::usage::now_ms() / 1000) as i64;
    let profile_names: Vec<crate::profile::ProfileName> = {
        let cfg = app.config();
        cfg.profiles.iter().map(|p| p.name.clone()).collect()
    };
    let rate_limited: Vec<String> = profile_names
        .iter()
        .filter(|name| {
            crate::throughput::summary(name, now_secs)
                .iter()
                .any(|t| t.rate_limited_recent)
        })
        .map(|name| name.to_string())
        .collect();

    // delegates row — the job store is also its detail, so read it first and
    // derive the dot from what it holds now, not the previous tick's.
    app.services.delegates = crate::mcp::jobs::list_banded(crate::usage::now_ms());
    app.services.settle_delegates_selection();
    checks.push(delegates_check(&app.services.delegates, &rate_limited));

    // plugin row — the folded readout, over the cached probes + the PATH read.
    checks.push(plugin_check(
        clauth_path.as_deref(),
        app.services.cc_version.clone(),
        app.services.mcp_boot.clone(),
    ));

    // herdr — the installed herdr's clauth plugin + keybinding/sidebar config.
    // The probe is cached (its worker lands it, above); the config read is a
    // cheap `fs::read_to_string` that rides the tick, using the path the probe
    // already resolved. No row when herdr does not resolve or its probe has not
    // landed yet.
    if let Some(Some(probe)) = &app.services.herdr {
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
        app.services.herdr_config = config;
    } else {
        // No herdr row renders without a probe, so its verdict must not
        // outlive the probe (a stale writable verdict would arm the
        // delegate-row confirm against a file that no longer resolves).
        app.services.herdr_config = None;
    }

    app.services.checks = checks;

    // Keep the cursors in range after the row/problem sets change.
    let max = app.services.row_count().saturating_sub(1);
    if app.services.cursor > max {
        app.services.cursor = max;
    }
    let problems = app
        .services
        .selected_check()
        .map(|c| c.problems.len())
        .unwrap_or(0)
        .saturating_sub(1);
    if app.services.problem_cursor > problems {
        app.services.problem_cursor = problems;
    }

    // A delegates detail that emptied while descended has nothing left to
    // select: ascend to the selector so the footer and the pane focus cannot
    // stay stuck on a dead pane.
    if app.services.focus == ServicesFocus::Detail
        && app
            .services
            .selected_check()
            .is_some_and(|c| c.label == "delegates")
        && app.services.delegates.is_empty()
    {
        leave_shunt_detail(app);
    }

    // A pending herdr landing (the first herdr launch; its probe lands on a
    // worker) selects the herdr row the moment it appears — by label, not by a
    // stale index — and descends into its detail. The intent clears on
    // landing; until the probe resolves, the cursor parks on the last row, and
    // the drained probe settles it (`drain_service_probes`). Opening a modal
    // drops it (`App::open_modal`), so no modal is ever open here with it set.
    if app.services.land_on_herdr {
        if let Some(idx) = app.services.checks.iter().position(|c| c.label == "herdr") {
            app.services.cursor = idx;
            app.services.focus = ServicesFocus::Detail;
            app.services.land_on_herdr = false;
        } else {
            app.services.cursor = app.services.row_count().saturating_sub(1);
        }
    }

    // A card change keeps the shunt focus by identity while the kind still
    // exists (a landed action can drop the line that held it); the render
    // re-settles it (the descend rule) on the next frame when a change pushed
    // the line out of view, over the freshly rendered geometry.
    if let Some(kind) = app.services.shunt_focus_kind()
        && !app
            .services
            .selected_check()
            .is_some_and(|c| c.shunt_focus.iter().any(|s| s.kind() == kind))
    {
        app.services.set_shunt_focus(None);
    }
}

/// Adopt every finished Services probe and recompute once, so a row appears
/// (or changes) with no key. A pending first-herdr landing lands in that
/// recompute or never: a herdr probe that resolved without landing it (herdr
/// absent) drops the intent, so no later refresh can move the view.
fn drain_service_probes(app: &mut App) {
    let herdr = app.services.herdr_probe.drain();
    let standalone = app.services.standalone_probe.drain();
    let move_plan = app.services.move_plan_probe.drain();
    let pool = app.services.pool_probe.drain();
    let action = app.services.shunt_action.drain();
    let save = app.services.shunt_save_worker.drain();
    if herdr.is_none()
        && standalone.is_none()
        && move_plan.is_none()
        && pool.is_none()
        && action.is_none()
        && save.is_none()
    {
        return;
    }
    let herdr_landed = herdr.is_some();
    if let Some(probe) = herdr {
        app.services.herdr = Some(probe);
    }
    if let Some(readout) = standalone {
        app.services.standalone = Some(readout);
    }
    if let Some(plan) = move_plan {
        app.services.move_plan = Some(plan);
    }
    if let Some(readout) = pool {
        app.services.pool = Some(readout);
    }
    let action_landed = action.is_some() || save.is_some();
    if let Some(outcome) = action {
        handle_shunt_action_outcome(app, outcome);
    }
    if let Some(outcome) = save {
        // The save worker runs `ConfigEdit`, `RemoveToken` and `AddToken`,
        // none of which yields `NoOp`, so a `NoOp` here is a panicked save: it
        // drains as a failed outcome that clears the guard. A config edit's
        // panic can land only before the staged rename (the rename is the
        // write's last step and everything after it is pure), so nothing was
        // written holds there; a token job's panic can land after the store
        // write (a minted add), so its copy never claims nothing was written.
        let outcome = match outcome {
            ShuntActionOutcome::NoOp => {
                let token_job = app.services.shunt_save.as_ref().is_some_and(|s| {
                    matches!(s.key, ShuntSaveKey::Token(_) | ShuntSaveKey::AddToken)
                });
                if token_job {
                    ShuntActionOutcome::ConfigFailed {
                        error: "the save stopped unexpectedly; check the card".to_string(),
                    }
                } else {
                    ShuntActionOutcome::ConfigFailed {
                        error: "the save stopped unexpectedly; nothing was written".to_string(),
                    }
                }
            }
            other => other,
        };
        handle_shunt_action_outcome(app, outcome);
    }
    // After a shunt action lands (success OR failure) the plan and pool caches
    // are dropped and their workers restarted, so the next confirm counts a
    // fresh plan — never the stale one a failed move left behind.
    recompute_services_checks(
        app,
        false,
        if action_landed {
            ShuntRefresh::Restart
        } else {
            ShuntRefresh::Keep
        },
    );
    if herdr_landed {
        app.services.land_on_herdr = false;
    }
}

/// Whether a shunt refusal can show on its own `└` line: the Services tab is
/// shown, the detail pane holds focus, the `shunt` row is the selected
/// service, and the focus still rests on the row that started the save. Any
/// conjunct false (an `esc` ascend, a tab jump, another service picked, or a
/// focus move off the row) leaves the row off-screen — the refusal toasts
/// `save failed` + the sentence instead.
fn refusal_row_visible(app: &App, on_row: bool) -> bool {
    app.tab == Tab::Services
        && app.services.focus == ServicesFocus::Detail
        && app
            .services
            .selected_check()
            .is_some_and(|c| c.label == "shunt")
        && on_row
}

/// One shunt-card action's result: the toast, every error `Display` escaped
/// (it can interpolate an untrusted path), the moved count singular at 1. The
/// plan/pool caches are dropped by the drain's recompute (which restarts their
/// workers after any action).
fn handle_shunt_action_outcome(app: &mut App, outcome: ShuntActionOutcome) {
    match outcome {
        ShuntActionOutcome::Adopted { path } => {
            app.toast(ToastKind::Success, format!("adopted {path}"));
        }
        ShuntActionOutcome::AdoptRefused { version, addr } => {
            app.toast(
                ToastKind::Warning,
                format!("shunt {version} answers on {addr}\nstop it, then adopt again"),
            );
        }
        ShuntActionOutcome::AdoptFailed { error } => {
            app.toast(
                ToastKind::Danger,
                format!("adopt failed\n{}", escape_control(&error)),
            );
        }
        ShuntActionOutcome::Moved { n } => {
            app.toast(
                ToastKind::Success,
                format!("moved {n} {} into clauth", account_files_word(n)),
            );
        }
        ShuntActionOutcome::MovedEnableFailed { n, error } => {
            // The stores moved, so the success toast still shows; the record
            // write failed, so the house `save failed` toast follows it.
            app.toast(
                ToastKind::Success,
                format!("moved {n} {} into clauth", account_files_word(n)),
            );
            app.toast(
                ToastKind::Danger,
                format!("save failed\n{}", escape_control(&error)),
            );
        }
        ShuntActionOutcome::MoveFailed { error } => {
            app.toast(
                ToastKind::Danger,
                format!("move failed\n{}", escape_control(&error)),
            );
        }
        ShuntActionOutcome::AdminAdded => {
            // The one toast for both admin steps (the orchestrator's reading,
            // flagged in the report).
            app.services.check_output = None;
            app.toast(ToastKind::Success, "added clauth's admin key");
        }
        ShuntActionOutcome::AdminFailed {
            error,
            check_output,
            config,
        } => {
            app.toast(
                ToastKind::Danger,
                format!("admin key failed\n{}", escape_control(&error)),
            );
            hold_check_output(app, &config, CheckOutputSource::AdminEdit, check_output);
        }
        ShuntActionOutcome::Held => {
            app.toast(
                ToastKind::Success,
                "shunt stopped\nit stays off until the daemon restarts",
            );
        }
        ShuntActionOutcome::HoldFailed { error } => {
            app.toast(
                ToastKind::Danger,
                format!("shunt stop failed\n{}", escape_control(&error)),
            );
        }
        ShuntActionOutcome::Released => {
            app.toast(ToastKind::Success, "shunt starting");
        }
        ShuntActionOutcome::ReleaseFailed { error } => {
            app.toast(
                ToastKind::Danger,
                format!("shunt start failed\n{}", escape_control(&error)),
            );
        }
        ShuntActionOutcome::NoOp => {}
        ShuntActionOutcome::ConfigSaved {
            key,
            written,
            restart_only,
            value,
            confirmed,
        } => {
            // The entry a `save <kind>` lands under was fixed when the save
            // started and rides in the in-flight save; capture it before the
            // slot clears.
            let save_entry = app
                .services
                .shunt_save
                .as_ref()
                .and_then(|save| save.entry.clone());
            app.services.shunt_save = None;
            // A save landed (or matched the file and wrote nothing): the user
            // moved past the refused edit, so its output no longer explains
            // the card.
            if app
                .services
                .check_output
                .as_ref()
                .is_some_and(|held| held.source == CheckOutputSource::ConfigSave)
            {
                app.services.check_output = None;
            }
            match key {
                ShuntSaveKey::Field(field_key) => {
                    // The `✓` shows for about a second on the row that saved; a
                    // rename moves focus to the renamed field (the expanded
                    // set's key moves with it).
                    let new_key = config_view(app)
                        .and_then(|view| {
                            renamed_field_key(view, &field_key, value.as_deref().unwrap_or(""))
                        })
                        .unwrap_or(field_key.clone());
                    // A rename moves the expanded set's key with the renamed
                    // entry, so the new name's fields stay expanded and the
                    // focus follows.
                    if is_rename_field(&field_key) {
                        if let Some(old_entry) = field_entry(&field_key) {
                            app.services.shunt_expanded.remove(&old_entry);
                        }
                        if let Some(new_entry) = field_entry(&new_key) {
                            app.services.shunt_expanded.insert(new_entry);
                        }
                    }
                    app.services.shunt_ok =
                        Some((ShuntOkKey::Field(new_key.clone()), Instant::now()));
                    // A landing save touches only its own row: it closes the
                    // editor and moves focus only when they still name the
                    // saved row (a rename's new name included), never another
                    // editor the user opened or a row the user moved to while
                    // the save ran.
                    if app
                        .services
                        .shunt_field_edit
                        .as_ref()
                        .is_some_and(|state| state.key == field_key)
                    {
                        app.services.shunt_field_edit = None;
                    }
                    if app.services.focused_shunt_focus().is_some_and(|focus| {
                        focus
                            == &ShuntFocus::Field {
                                key: field_key.clone(),
                            }
                    }) {
                        app.services
                            .set_shunt_focus(Some(ShuntFocusLine::Field(new_key)));
                    }
                    if written && restart_only && confirmed {
                        // An emptied bind writes the key's removal, so shunt
                        // starts on its own default; a set bind restarts on the
                        // new address.
                        if let Some(bind) = value {
                            app.toast(
                                ToastKind::Success,
                                format!(
                                    "shunt restarting\nbind saved · it starts on {}",
                                    escape_control(&bind)
                                ),
                            );
                        } else {
                            app.toast(
                                ToastKind::Success,
                                format!(
                                    "shunt restarting\nbind removed · it starts on {}",
                                    crate::gateway::SHUNT_DEFAULT_BIND
                                ),
                            );
                        }
                    }
                }
                ShuntSaveKey::SaveAdd(kind) => {
                    // The draft closes, the new entry appears where the engine
                    // put it (collapsed), and that row's caret slot shows the
                    // `✓` for `SHUNT_OK_MS`. Focus moves to the new entry only
                    // while it still rests on this kind's own rows (the
                    // `+ add`, a draft field, or `save <kind>`) — never an
                    // editor the user opened or a row they moved to meanwhile.
                    let new_entry = save_entry.clone();
                    app.services.shunt_drafts.fold(kind);
                    if let Some(entry) = new_entry {
                        app.services.shunt_expanded.remove(&entry);
                        app.services.shunt_ok =
                            Some((ShuntOkKey::Entry(entry.clone()), Instant::now()));
                        if focus_on_add_kind(app, kind) {
                            app.services
                                .set_shunt_focus(Some(ShuntFocusLine::Entry(entry)));
                        }
                    }
                }
                ShuntSaveKey::Remove(entry_key) => {
                    // The entry leaves the card and the expanded set. Focus
                    // lands on the next entry row of that group, else the
                    // previous one, else its `+ add <kind>` row — but only
                    // while it still rests on the removed entry's own rows.
                    let focus_was_here = focus_on_removed_entry(app, &entry_key);
                    // Close a field editor open on the removed entry BEFORE
                    // re-keying: its key names the removed entry, and after a
                    // route shift a later route's new position could coincide.
                    if app
                        .services
                        .shunt_field_edit
                        .as_ref()
                        .is_some_and(|state| field_entry(&state.key).as_ref() == Some(&entry_key))
                    {
                        app.services.shunt_field_edit = None;
                    }
                    if let ShuntEntryKey::Route(position) = entry_key {
                        // A route remove shifts every later route down by one:
                        // re-key the route-positioned state so each still names
                        // the SAME route after the recompute. This also drops
                        // the removed route's own keys.
                        rekey_routes_after_remove(app, position);
                    } else {
                        app.services.shunt_expanded.remove(&entry_key);
                    }
                    app.services.shunt_remove_refusal.take();
                    if focus_was_here {
                        let focus = remove_focus_target(app, &entry_key);
                        app.services.set_shunt_focus(focus);
                    }
                }
                ShuntSaveKey::Token(_) | ShuntSaveKey::AddToken => {
                    unreachable!("a token save never yields ConfigSaved")
                }
            }
        }
        ShuntActionOutcome::ConfigRefused { key, sentence } => {
            app.services.shunt_save = None;
            match key {
                ShuntSaveKey::Field(field_key) => {
                    // The engine refused the input or the file's shape: the
                    // editor stays open with the typed text, the refusal on the
                    // `└` line. Once the user moved focus away (or opened
                    // another field), there is no editor for that row to write
                    // — the refusal toasts instead of reaching no surface.
                    if let Some(state) = app.services.shunt_field_edit.as_mut()
                        && state.key == field_key
                    {
                        state.refusal = Some(sentence);
                    } else {
                        app.toast(
                            ToastKind::Danger,
                            format!("save failed\n{}", escape_control(&sentence)),
                        );
                    }
                }
                ShuntSaveKey::SaveAdd(kind) => {
                    // The draft is kept; the refusal sits on the `└` line under
                    // the `save <kind>` row only while that row is on the
                    // focused card — the Services tab, the detail pane, the
                    // `shunt` row, and the kind's own rows (`+ add`, a draft
                    // field, `save`). Anything else has no surface to hold the
                    // sentence, so it toasts like the field arm.
                    if refusal_row_visible(app, focus_on_add_kind(app, kind)) {
                        app.services.shunt_drafts.set_refusal(kind, sentence);
                    } else {
                        app.toast(
                            ToastKind::Danger,
                            format!("save failed\n{}", escape_control(&sentence)),
                        );
                    }
                }
                ShuntSaveKey::Remove(entry_key) => {
                    // The refusal sits on the `└` line under the entry's
                    // `remove <kind>` row only while that row is on the focused
                    // card — the Services tab, the detail pane, the `shunt`
                    // row, and the focus still on the `remove` row. Landing
                    // anywhere else, it has no row to show under — it toasts
                    // like the field and add arms instead.
                    let on_row = app.services.shunt_focus_kind()
                        == Some(ShuntFocusLine::Remove(entry_key.clone()));
                    if refusal_row_visible(app, on_row) {
                        let view = config_view(app).cloned().map(Box::new);
                        if let Some(view) = view {
                            app.services
                                .shunt_remove_refusal
                                .replace(Some(ShuntRemoveRefusal {
                                    key: entry_key,
                                    sentence,
                                    view,
                                }));
                        }
                    } else {
                        app.toast(
                            ToastKind::Danger,
                            format!("save failed\n{}", escape_control(&sentence)),
                        );
                    }
                }
                ShuntSaveKey::Token(_) | ShuntSaveKey::AddToken => {
                    unreachable!("a token save never yields ConfigRefused")
                }
            }
        }
        ShuntActionOutcome::ConfigCheckFailed {
            error,
            check_output,
            config,
        } => {
            app.services.shunt_save = None;
            app.toast(
                ToastKind::Danger,
                format!("save failed\n{}", escape_control(&error)),
            );
            hold_check_output(app, &config, CheckOutputSource::ConfigSave, check_output);
        }
        ShuntActionOutcome::ConfigFailed { error } => {
            app.services.shunt_save = None;
            app.toast(
                ToastKind::Danger,
                format!("save failed\n{}", escape_control(&error)),
            );
        }
        ShuntActionOutcome::TokenRemoved { profile, removed } => {
            app.services.shunt_save = None;
            if removed {
                app.toast(
                    ToastKind::Success,
                    format!(
                        "removed {}'s client token\nthe gateway restarts once",
                        escape_control(&profile)
                    ),
                );
            }
            // The row leaves and focus lands on the next token row, else the
            // previous one, else `+ add token` — only while focus still rests
            // on the removed row.
            if focus_on_removed_token(app, &profile) {
                let focus = token_remove_focus_target(app, &profile);
                app.services.set_shunt_focus(focus);
            }
        }
        ShuntActionOutcome::TokenRemoveRefused { profile, sentence } => {
            app.services.shunt_save = None;
            // The refusal sits on the `└` line under the token row only while
            // that row is on the focused card — the Services tab, the detail
            // pane, the `shunt` row, and the focus still on the token row.
            // Landing anywhere else, it has no row to show under — it toasts
            // like the field and remove arms instead.
            let on_row = focus_on_removed_token(app, &profile);
            if refusal_row_visible(app, on_row) {
                app.services
                    .shunt_token_refusal
                    .replace(Some((profile, sentence)));
            } else {
                app.toast(
                    ToastKind::Danger,
                    format!("remove failed\n{}", escape_control(&sentence)),
                );
            }
        }
        ShuntActionOutcome::TokenRemoveFailed { error } => {
            app.services.shunt_save = None;
            app.toast(
                ToastKind::Danger,
                format!("remove failed\n{}", escape_control(&error)),
            );
        }
        ShuntActionOutcome::TokenAdded { profile } => {
            app.services.shunt_save = None;
            // A repointed credential retires the old provider's verdict pill
            // and throttle streak — the shared clear every credential-replacing
            // path runs (the `commit_endpoint` precedent), and the token
            // collection re-reads so the fetch leg schedules the re-keyed
            // profile's first fetch same-frame.
            app.refresh_tokens();
            clear_profile_status_chrome(app, &ProfileName::from(profile.as_str()));
            // The new token row appears in store order; focus lands on it with
            // the `✓` for `SHUNT_OK_MS`.
            app.services.shunt_ok = Some((ShuntOkKey::Token(profile.clone()), Instant::now()));
            app.services
                .set_shunt_focus(Some(ShuntFocusLine::Token(profile.clone())));
            app.toast(
                ToastKind::Success,
                format!(
                    "{} uses the gateway\nthe gateway restarts once to read its token",
                    escape_control(&profile)
                ),
            );
        }
        ShuntActionOutcome::TokenAddRefused { error } => {
            app.services.shunt_save = None;
            app.toast(
                ToastKind::Danger,
                format!("add failed\n{}", escape_control(&error)),
            );
        }
        ShuntActionOutcome::TokenAddedProfileFailed { error } => {
            // The token is stored, so the row stays; the copy already names
            // which of the two failure shapes it was (the disk profile read in
            // `run_add_client_token` decides), so the arm just toasts it.
            app.services.shunt_save = None;
            app.toast(
                ToastKind::Danger,
                format!("add failed\n{}", escape_control(&error)),
            );
        }
    }
}

/// Hold a failed `shunt check`'s stderr on the card, but only for the config
/// the edit ran against: a re-adopt that landed while the check ran must never
/// show another gateway's output on this card. A check-output-free refusal (or
/// a non-typed error) retires a held output of its own kind only.
fn hold_check_output(
    app: &mut App,
    config: &std::path::Path,
    source: CheckOutputSource,
    check_output: Option<crate::gateway::CheckStderr>,
) {
    let current = app
        .services
        .shunt_record
        .as_ref()
        .map(|record| record.config().to_path_buf());
    if Some(config) != current.as_deref() {
        app.services.check_output = None;
        return;
    }
    match check_output {
        Some(stderr) => app.services.check_output = Some(HeldCheckOutput { source, stderr }),
        // A refusal with no output answers only its own kind's held output.
        None => {
            if app
                .services
                .check_output
                .as_ref()
                .is_some_and(|held| held.source == source)
            {
                app.services.check_output = None;
            }
        }
    }
}

/// The entry key a `save <kind>` edit lands under, fixed at save START: for an
/// upstream/model, the (unique) name/id with occurrence 0 (the engine refused
/// a duplicate); for a route, the position after the pre-save view's routes.
/// `None` on any non-add edit. The outcome drain reads the key this captured,
/// never the draft or the live view.
fn add_edit_entry_key(
    edit: &crate::gateway_edit::Edit,
    view: Option<&crate::gateway_edit::ConfigView>,
) -> Option<ShuntEntryKey> {
    use crate::gateway_edit::Edit;
    match edit {
        Edit::AddUpstream(new) => Some(ShuntEntryKey::Upstream {
            name: new.name.clone(),
            occurrence: 0,
        }),
        Edit::AddModel(new) => Some(ShuntEntryKey::Model {
            id: new.id.clone(),
            occurrence: 0,
        }),
        Edit::AddRoute(_) => view.map(|view| ShuntEntryKey::Route(view.routes.len() + 1)),
        _ => None,
    }
}

/// The focus target after a successful remove: the next entry row of the same
/// group, else the previous one, else the group's `+ add <kind>` row. A route
/// remove shifts every later route down by one, so its "next" is the route now
/// at position p (the old p+1), computed against the pre-remove view.
fn remove_focus_target(app: &App, removed: &ShuntEntryKey) -> Option<ShuntFocusLine> {
    let view = config_view(app)?;
    match removed {
        ShuntEntryKey::Route(position) => {
            let has_next = view
                .routes
                .iter()
                .any(|route| route.position == *position + 1);
            if has_next {
                return Some(ShuntFocusLine::Entry(ShuntEntryKey::Route(*position)));
            }
            if *position > 1 {
                return Some(ShuntFocusLine::Entry(ShuntEntryKey::Route(position - 1)));
            }
            add_row_offered(view, ShuntAddKind::Route)
                .then_some(ShuntFocusLine::Add(ShuntAddKind::Route))
        }
        _ => {
            let kind = entry_kind(removed);
            let keys = config_entry_keys(view);
            let idx = keys.iter().position(|key| key == removed)?;
            let same_kind = |key: &ShuntEntryKey| entry_kind(key) == kind;
            if let Some(next) = keys.iter().skip(idx + 1).find(|key| same_kind(key)) {
                return Some(ShuntFocusLine::Entry(next.clone()));
            }
            if let Some(prev) = keys.iter().take(idx).rev().find(|key| same_kind(key)) {
                return Some(ShuntFocusLine::Entry(prev.clone()));
            }
            add_row_offered(view, kind).then_some(ShuntFocusLine::Add(kind))
        }
    }
}

/// Whether focus still rests on a `+ add <kind>` save's own rows: the `+ add`
/// row, one of its draft field rows, or its `save <kind>` row.
fn focus_on_add_kind(app: &App, kind: ShuntAddKind) -> bool {
    app.services
        .focused_shunt_focus()
        .is_some_and(|focus| match focus {
            ShuntFocus::Add { kind: k } | ShuntFocus::SaveAdd { kind: k } => *k == kind,
            ShuntFocus::Field {
                key: ShuntFieldKey::Draft { kind: k, .. },
            } => *k == kind,
            _ => false,
        })
}

/// Whether focus still rests on a removed entry's own rows: the entry row, one
/// of its field rows, or its `remove <kind>` row.
fn focus_on_removed_entry(app: &App, removed: &ShuntEntryKey) -> bool {
    app.services
        .focused_shunt_focus()
        .is_some_and(|focus| match focus {
            ShuntFocus::Entry { key } | ShuntFocus::Remove { key } => key == removed,
            ShuntFocus::Field { key } => field_entry(key).as_ref() == Some(removed),
            _ => false,
        })
}

/// Whether focus still rests on the token row whose remove landed.
fn focus_on_removed_token(app: &App, removed: &str) -> bool {
    app.services.shunt_focus_kind() == Some(ShuntFocusLine::Token(removed.to_string()))
}

/// The focus target after a successful token remove: the next stored token row,
/// else the previous one, else `+ add token` — read off the PRE-remove cached
/// list, so the removed row's neighbours are its neighbours as it left.
fn token_remove_focus_target(app: &App, removed: &str) -> Option<ShuntFocusLine> {
    let stored = &app.services.shunt_tokens.stored;
    let idx = stored.iter().position(|name| name == removed)?;
    if let Some(next) = stored.get(idx + 1) {
        return Some(ShuntFocusLine::Token(next.clone()));
    }
    if idx > 0 {
        return Some(ShuntFocusLine::Token(stored[idx - 1].clone()));
    }
    Some(ShuntFocusLine::AddToken)
}

/// `↵` on a token row: open the token-remove confirm. A locked row opens no
/// second confirm (ruling 2): the row is inert while its own remove checks.
fn confirm_shunt_remove_token(app: &mut App, profile: &str) {
    if app
        .services
        .saving(&ShuntSaveKey::Token(profile.to_string()))
    {
        return;
    }
    let Some(record) = app.services.shunt_record.clone() else {
        return;
    };
    app.disarm_quit();
    app.open_modal(Modal::Confirm(ConfirmState {
        message: format!("remove {}'s client token?", escape_control(profile)),
        detail: Some(format!(
            "{} keeps its api key, which stops working once the gateway restarts.",
            escape_control(profile)
        )),
        choice: false,
        scroll: ConfirmScroll::default(),
        on_confirm: ConfirmAction::RemoveToken {
            record,
            profile: profile.to_string(),
        },
    }));
}

/// `↵`/`space` on `+ add token`: open the profile picker, one row per claude
/// profile in roster order (the Overview's roster; codex profiles live in their
/// own store, never here). An empty roster opens nothing: the `n` hint names
/// the way to make one.
fn open_token_picker(app: &mut App) {
    if app.services.saving(&ShuntSaveKey::AddToken) {
        return;
    }
    let stored = &app.services.shunt_tokens.stored;
    let targets: Vec<(String, bool)> = {
        let cfg = app.config();
        cfg.profiles
            .iter()
            .map(|p| {
                let name = p.name.to_string();
                let has = stored.iter().any(|held| held == &name);
                (name, has)
            })
            .collect()
    };
    if targets.is_empty() {
        app.toast(
            ToastKind::Info,
            "no claude profile to give a token\npress n to create one",
        );
        return;
    }
    app.disarm_quit();
    app.open_modal(Modal::TokenPicker(TokenPickerForm {
        targets,
        cursor: 0,
        scroll: std::cell::Cell::new(0),
        viewport: std::cell::Cell::new(0),
        starts: std::cell::RefCell::new(Vec::new()),
        block_ends: std::cell::RefCell::new(Vec::new()),
        total: std::cell::Cell::new(0),
    }));
}

/// A picker pick: open the overwrite confirm naming what it replaces. The
/// gateway base url is read here (a bind read failure toasts `add failed`, no
/// confirm); the `replace` line follows the profile's current base url — an
/// api-key endpoint reads `it replaces <base> and its current api key.` (or
/// `it replaces <base>.` with no key), and a profile with no base url (an OAuth
/// subscription login) reads the OAuth copy. A profile that vanished from the
/// roster refuses with `add failed` / `<profile> no longer exists`, no confirm.
fn open_token_overwrite_confirm(app: &mut App, profile: String) {
    let Some(record) = app.services.shunt_record.clone() else {
        app.toast(
            ToastKind::Danger,
            format!("add failed\n{NO_ADOPTED_CONFIG}"),
        );
        return;
    };
    let url = match crate::gateway::gateway_base_url(&record) {
        Ok(url) => url,
        Err(e) => {
            app.toast(
                ToastKind::Danger,
                format!("add failed\n{}", escape_control(&error_toast_detail(&e))),
            );
            return;
        }
    };
    let found = {
        let cfg = app.config();
        cfg.profiles
            .iter()
            .find(|p| p.name.as_str() == profile)
            .map(|p| (p.base_url.clone(), p.api_key.is_some()))
    };
    let (base_url, has_key) = match found {
        Some(pair) => pair,
        None => {
            // A profile that vanished from the roster (the picker opened over
            // an older roster) refuses before any confirm, never minting.
            app.toast(
                ToastKind::Danger,
                format!("add failed\n{} no longer exists", escape_control(&profile)),
            );
            return;
        }
    };
    let replace = match (base_url.as_deref(), has_key) {
        (Some(base), true) => Some(format!(
            "it replaces {} and its current api key.",
            escape_control(base)
        )),
        (Some(base), false) => Some(format!("it replaces {}.", escape_control(base))),
        // An OAuth profile (no base url): the overwrite turns its subscription
        // login into a gateway consumer, so the third line names that and the
        // way back (the owner ruling 2026-10-12).
        (None, _) => Some(
            "it stops using its subscription login, which stays stored; clear its base url in Setup to go back."
                .to_string(),
        ),
    };
    app.disarm_quit();
    app.open_modal(Modal::Confirm(ConfirmState {
        message: format!("point {} at the shunt gateway?", escape_control(&profile)),
        detail: Some(format!(
            "its base url becomes {} and its api key a new client token.",
            escape_control(&url)
        )),
        choice: false,
        scroll: ConfirmScroll::default(),
        on_confirm: ConfirmAction::AddToken {
            record,
            profile,
            url,
            replace,
        },
    }));
}

/// Re-key every route-positioned piece of state after removing the route at
/// `position`: each `Route(n > position)` key moves down by one (the same
/// route's new position), and `Route(position)` drops (its entry left the
/// config). Upstreams and models are keyed by name/id and never shift.
fn rekey_routes_after_remove(app: &mut App, position: usize) {
    app.services.shunt_expanded = app
        .services
        .shunt_expanded
        .iter()
        .filter_map(|key| shift_entry_key(key, position))
        .collect();
    if let Some(state) = app.services.shunt_field_edit.clone() {
        app.services.shunt_field_edit =
            shift_field_key(&state.key, position).map(|key| ShuntFieldEditState {
                key,
                edit: state.edit,
                refusal: state.refusal,
            });
    }
    app.services.shunt_ok = app
        .services
        .shunt_ok
        .take()
        .and_then(|(key, at)| shift_ok_key(&key, position).map(|key| (key, at)));
    let focus = app
        .services
        .shunt_focus_kind()
        .and_then(|line| shift_focus_line(&line, position));
    app.services.set_shunt_focus(focus);
    // An open remove confirm's route key shifts with the other route-keyed
    // state (ruling 5's list gains the open confirm). A shifted route confirm
    // still names and removes its own route: the stored `RemoveRoute` edit
    // moves down with the key, and the message is rebuilt from the shifted
    // route so the copy names the route's new position. A confirm must never
    // be left able only to refuse.
    if let Some(Modal::Confirm(state)) = app.modals.last_mut()
        && let ConfirmAction::RemoveEntry { key, edit } = &mut state.on_confirm
        && let Some(shifted) = shift_entry_key(key, position)
    {
        *key = shifted;
        if let crate::gateway_edit::Edit::RemoveRoute { route } = edit.as_mut()
            && route.position > position
        {
            route.position -= 1;
            state.message = remove_route_confirm_message(route);
        }
    }
}

/// Shift one entry key past a removed route; `None` when it names the removed
/// route itself.
fn shift_entry_key(key: &ShuntEntryKey, position: usize) -> Option<ShuntEntryKey> {
    match key {
        ShuntEntryKey::Route(p) if *p > position => Some(ShuntEntryKey::Route(p - 1)),
        ShuntEntryKey::Route(p) if *p == position => None,
        other => Some(other.clone()),
    }
}

/// Shift one field key past a removed route; `None` when it names the removed
/// route itself.
fn shift_field_key(key: &ShuntFieldKey, position: usize) -> Option<ShuntFieldKey> {
    match key {
        ShuntFieldKey::Route { position: p, field } if *p > position => {
            Some(ShuntFieldKey::Route {
                position: p - 1,
                field: *field,
            })
        }
        ShuntFieldKey::Route { position: p, .. } if *p == position => None,
        other => Some(other.clone()),
    }
}

/// Shift one `✓` key past a removed route; `None` when it names the removed
/// route itself.
fn shift_ok_key(key: &ShuntOkKey, position: usize) -> Option<ShuntOkKey> {
    match key {
        ShuntOkKey::Entry(entry) => shift_entry_key(entry, position).map(ShuntOkKey::Entry),
        ShuntOkKey::Field(field) => shift_field_key(field, position).map(ShuntOkKey::Field),
        ShuntOkKey::Token(_) => Some(key.clone()),
    }
}

/// Shift one held focus identity past a removed route; `None` when it names the
/// removed route itself.
fn shift_focus_line(line: &ShuntFocusLine, position: usize) -> Option<ShuntFocusLine> {
    match line {
        ShuntFocusLine::Entry(entry) => shift_entry_key(entry, position).map(ShuntFocusLine::Entry),
        ShuntFocusLine::Field(field) => shift_field_key(field, position).map(ShuntFocusLine::Field),
        ShuntFocusLine::Remove(entry) => {
            shift_entry_key(entry, position).map(ShuntFocusLine::Remove)
        }
        other => Some(other.clone()),
    }
}

/// The `delegates` row: a selector whose detail is the job list. Green while a
/// job runs (running or blocking), dim when none — finished and orphaned rows
/// are not a running job. A profile's rate-limited delegate traffic names
/// itself in the detail; it never moves the dot (the dot is the two-state
/// running/none signal).
pub(crate) fn delegates_check(
    stored: &[crate::mcp::jobs::StoredJob],
    rate_limited: &[String],
) -> Check {
    use crate::mcp::jobs::JobPhase;
    let running = stored
        .iter()
        .any(|j| matches!(j.phase(), JobPhase::Running | JobPhase::Blocking));
    let mut detail = Vec::new();
    if !rate_limited.is_empty() {
        detail.push(format!(
            "delegate: rate-limited ({})",
            rate_limited.join(", ")
        ));
    }
    Check {
        label: "delegates",
        health: if running { Health::Ok } else { Health::Idle },
        detail,
        fix: None,
        problems: Vec::new(),
        shunt_focus: Vec::new(),
        shunt_action_start: None,
    }
}

/// What runs without clauth while no gateway is adopted: the config shunt's
/// own discovery would load, and the shunt answering `/health` on its bind.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct StandaloneShunt {
    /// The first config shunt would load from the TUI's cwd and env, TOML or
    /// YAML; `None` when discovery finds none.
    pub(crate) found: Option<std::path::PathBuf>,
    /// Why clauth could not read the bind shunt would listen on; `None` when
    /// it read one, or when no config and no env override set one.
    pub(crate) unread_bind: Option<UnreadBind>,
    /// Why the found config itself could not be read, whatever set the bind:
    /// an env bind hides this from `unread_bind`, and the adopt offer still
    /// needs it.
    pub(crate) unread_config: Option<UnreadConfig>,
    /// The running daemon holds the singleton but recorded no env, so the
    /// bind its adopt would read is unknown and the card offers no adopt.
    pub(crate) unread_env: bool,
    /// The version a shunt-shaped `/health` answer reported, and the address
    /// that answered; `None` when nothing shunt-shaped answered.
    pub(crate) answer: Option<(String, std::net::SocketAddr)>,
}

/// Why clauth could not read a standalone shunt's bind. The row names it, so
/// a shunt on that bind going unnamed has a stated reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum UnreadBind {
    /// The config is YAML, which clauth does not parse.
    Yaml,
    /// The config file could not be read.
    Unreadable,
    /// The config is not valid TOML.
    Unparsed,
    /// The value clauth refused, as written: a `${…}` reference, port 0, or
    /// something that is not an address; an env value leads with its name.
    Value(String),
}

impl UnreadBind {
    fn label(&self) -> String {
        match self {
            UnreadBind::Yaml => "yaml".to_string(),
            UnreadBind::Unreadable => "config unreadable".to_string(),
            UnreadBind::Unparsed => "config does not parse".to_string(),
            UnreadBind::Value(value) if value.is_empty() => "empty".to_string(),
            UnreadBind::Value(value) => value.clone(),
        }
    }
}

/// Why clauth could not read a found config at all. An adopt of it can only
/// fail, so the card offers none.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UnreadConfig {
    /// The config is YAML, which clauth does not parse.
    Yaml,
    /// The config file could not be read.
    Unreadable,
    /// The config is not valid TOML.
    Unparsed,
}

impl UnreadConfig {
    pub(crate) fn label(self) -> &'static str {
        match self {
            UnreadConfig::Yaml => "yaml",
            UnreadConfig::Unreadable => "unreadable",
            UnreadConfig::Unparsed => "does not parse",
        }
    }
}

impl From<UnreadConfig> for UnreadBind {
    fn from(unread: UnreadConfig) -> Self {
        match unread {
            UnreadConfig::Yaml => UnreadBind::Yaml,
            UnreadConfig::Unreadable => UnreadBind::Unreadable,
            UnreadConfig::Unparsed => UnreadBind::Unparsed,
        }
    }
}

/// The standalone readout from a discovery result, the `SHUNT_SERVER__BIND`
/// value and one `/health` probe: of the bind clauth reads off the found TOML
/// config and the env, else of shunt's default. A bind clauth cannot read
/// falls back to the default and is named in `unread_bind`, so the answer
/// line only ever states what answered where.
pub(crate) fn standalone_readout_from(
    discovered: anyhow::Result<Option<std::path::PathBuf>>,
    env_bind: Option<&str>,
    probe: impl Fn(std::net::SocketAddr) -> anyhow::Result<crate::gateway::Health>,
) -> StandaloneShunt {
    let (found, config_text, unread_config) = match discovered {
        Ok(Some(path)) => match std::fs::read_to_string(&path) {
            // The adopt's own check: a TOML name linked onto a YAML file, or a
            // config that does not parse (an env bind skips the bind's parse),
            // reads here but refuses there.
            Ok(text) => match crate::gateway::adoptable_record(path.clone()) {
                Ok(_) => (Some(path), text, None),
                Err(e) if e.is::<crate::gateway::NotToml>() => {
                    (Some(path), String::new(), Some(UnreadConfig::Yaml))
                }
                Err(e) if e.is::<crate::gateway::ConfigUnparsed>() => {
                    (Some(path), String::new(), Some(UnreadConfig::Unparsed))
                }
                Err(_) => (Some(path), String::new(), Some(UnreadConfig::Unreadable)),
            },
            Err(_) => (Some(path), String::new(), Some(UnreadConfig::Unreadable)),
        },
        Ok(None) => (None, String::new(), None),
        Err(e) => match e.downcast_ref::<crate::gateway::YamlConfig>() {
            Some(yaml) => (
                Some(yaml.path.clone()),
                String::new(),
                Some(UnreadConfig::Yaml),
            ),
            None => (None, String::new(), None),
        },
    };
    let resolved = match env_bind.map(crate::gateway::read_bind_env).transpose() {
        Ok(env) => crate::gateway::resolve_bind(&config_text, env.as_deref()),
        Err(refusal) => Err(refusal.into()),
    };
    let (addr, unread_bind) = match resolved {
        // An env override is the bind shunt uses whatever the config says.
        Ok(bind) => (
            bind.probe,
            unread_config
                .filter(|_| env_bind.is_none())
                .map(UnreadBind::from),
        ),
        Err(e) => {
            let reason = match env_bind {
                Some(value) => UnreadBind::Value(format!("{}={value}", crate::gateway::BIND_ENV)),
                None => match e.downcast_ref::<crate::gateway::ConfigBindRefused>() {
                    Some(refused) => UnreadBind::Value(refused.written.clone()),
                    None => UnreadBind::Unparsed,
                },
            };
            (crate::gateway::SHUNT_DEFAULT_BIND, Some(reason))
        }
    };
    let answer = match probe(addr) {
        Ok(crate::gateway::Health::Shunt { version }) => Some((version, addr)),
        Ok(crate::gateway::Health::Silent(_) | crate::gateway::Health::NotShunt { .. })
        | Err(_) => None,
    };
    StandaloneShunt {
        found,
        unread_bind,
        unread_config,
        unread_env: false,
        answer,
    }
}

/// [`standalone_readout_from`] over the bind env the adopt reads
/// ([`crate::gateway::inherited_bind_env`]): a refused one leaves the bind
/// unknown, so the readout probes the config's own bind and marks
/// `unread_env`. Every refusal that reader returns is `DaemonEnvUnrecorded`.
pub(crate) fn standalone_readout_with(
    discovered: anyhow::Result<Option<std::path::PathBuf>>,
    env_bind: anyhow::Result<Option<String>>,
    probe: impl Fn(std::net::SocketAddr) -> anyhow::Result<crate::gateway::Health>,
) -> StandaloneShunt {
    let mut readout = standalone_readout_from(
        discovered,
        env_bind.as_ref().ok().and_then(Option::as_deref),
        probe,
    );
    readout.unread_env = env_bind.is_err();
    readout
}

/// The readout over this process's cwd and the adopt's bind env: the
/// Services worker's prober.
fn standalone_probe() -> StandaloneShunt {
    standalone_readout_with(
        crate::gateway::discover_config(),
        crate::gateway::inherited_bind_env(),
        crate::gateway::probe_health,
    )
}

/// The `shunt` row: the managed gateway's status dot + readout, from the slot
/// `gateway_slot` resolves (the daemon's feed, or the record-only verdict).
/// The FIELD lines only — the action section (the `enabled` row, the admin
/// fix, the codex pool note and the MOVE PLAN) is appended by [`shunt_card`],
/// which also fills the focus walk. Pure over the slot plus the `supervised`
/// flag (whether the slot came from a fresh daemon's feed) so the verdict
/// logic unit-tests without touching `status.json`. With no gateway adopted
/// (`absent`) the state reads `not adopted` and the `standalone` readout, once
/// probed, names what runs without clauth.
pub(crate) fn shunt_check(
    slot: &crate::daemon::gateway::GatewaySlot,
    supervised: bool,
    standalone: Option<&StandaloneShunt>,
) -> Check {
    let mut detail = Vec::new();
    if let Some(binary) = &slot.binary {
        detail.push(format!("binary: {}", escape_control(binary)));
    }
    if let Some(config) = &slot.config {
        detail.push(format!("config: {}", escape_control(config)));
    }
    if let Some(version) = &slot.version {
        detail.push(format!("version: {}", escape_control(version)));
    }
    if slot.state == GatewayState::Absent {
        // The feed keeps `absent`; the row says what it means for the user.
        detail.push("state: not adopted".to_string());
        if let Some(path) = standalone.and_then(|s| s.found.as_ref()) {
            detail.push(format!(
                "found: {}",
                escape_control(&path.to_string_lossy())
            ));
        }
        if standalone.is_some_and(|s| s.unread_env) {
            detail.push("the running daemon recorded no environment; restart it".to_string());
        }
        let unread_bind = standalone.and_then(|s| s.unread_bind.as_ref());
        if let Some(unread) = unread_bind {
            detail.push(format!(
                "bind: not read ({})",
                escape_control(&unread.label())
            ));
        }
        // An env bind, read or refused, hides the config's own reason from
        // the bind line; this line names it, since the card offers no adopt.
        if let Some(unread) = standalone
            .and_then(|s| s.unread_config)
            .filter(|unread| unread_bind != Some(&UnreadBind::from(*unread)))
        {
            detail.push(format!("config: not read ({})", unread.label()));
        }
        if let Some((version, addr)) = standalone.and_then(|s| s.answer.as_ref()) {
            // A `:` in the version, followed by a space of its own or the one
            // after it, would make the detail renderer split this prose line
            // into a key and a value.
            detail.push(format!(
                "standalone shunt {} answers on {addr}",
                escape_control(version).replace(':', "\\u{3a}")
            ));
        }
    } else if slot.state == GatewayState::Held {
        // `held` is the feed's word; the card states what it means: the
        // daemon holds the gateway off until its next start.
        detail.push("state: stopped until the daemon restarts".to_string());
    } else {
        detail.push(format!("state: {}", gateway_state_word(slot.state)));
    }
    if let Some(reason) = &slot.reason {
        // A multi-line reason (a TOML parse error with its snippet) splits onto
        // one indented sub-line per further line instead of one garbled row; the
        // detail renderer truncates each line to the pane.
        let mut lines = reason.split('\n');
        if let Some(first) = lines.next() {
            detail.push(format!("reason: {}", escape_control(first)));
        }
        for line in lines {
            detail.push(format!("  {}", escape_control(line)));
        }
    }
    // A foreign answerer names what holds the port, so a red `foreign` dot
    // states its cause instead of leaving it to the state word alone.
    if slot.state == GatewayState::Foreign
        && let Some(answerer) = &slot.answerer
    {
        let word = match answerer {
            Answerer::Shunt => "shunt",
            Answerer::NotShunt => "not shunt",
            Answerer::NoAnswer => "no answer",
        };
        detail.push(format!("answerer: {word}"));
    }
    if let Some(pid) = slot.pid {
        detail.push(format!("pid: {pid}"));
    }
    if let Some(port) = slot.port {
        detail.push(format!("port: {port}"));
    }
    // `restarts` and `last exit` are figures only the daemon observed: on a
    // record-only slot they are literals nobody read, so they do not show.
    if supervised {
        detail.push(format!("restarts: {}", slot.restarts));
    }
    if supervised && let Some(exit) = &slot.last_exit {
        detail.push(format!("last exit: {exit}"));
    }
    Check {
        label: "shunt",
        health: gateway_health(slot.state),
        detail,
        fix: None,
        problems: Vec::new(),
        shunt_focus: Vec::new(),
        shunt_action_start: None,
    }
}

/// The gateway state's word — the same spelling `status.json` publishes,
/// except `held`, which the card renders as its meaning (see [`shunt_check`]).
fn gateway_state_word(state: GatewayState) -> &'static str {
    match state {
        GatewayState::Absent => "absent",
        GatewayState::Disabled => "disabled",
        GatewayState::Held => "held",
        GatewayState::NoConfig => "no_config",
        GatewayState::YamlRefused => "yaml_refused",
        GatewayState::Misconfigured => "misconfigured",
        GatewayState::BinaryMissing => "binary_missing",
        GatewayState::Foreign => "foreign",
        GatewayState::Starting => "starting",
        GatewayState::Healthy => "healthy",
        GatewayState::Unhealthy => "unhealthy",
        GatewayState::BelowFloor => "below_floor",
        GatewayState::Restarting => "restarting",
        GatewayState::Stopping => "stopping",
        GatewayState::Unobserved => "unobserved",
    }
}

/// The gateway state's dot bucket: green healthy; amber starting/unhealthy/
/// restarting/stopping; red the refusal states; dim absent/disabled/unobserved.
pub(crate) fn gateway_health(state: GatewayState) -> Health {
    match state {
        GatewayState::Healthy => Health::Ok,
        GatewayState::Starting
        | GatewayState::Unhealthy
        | GatewayState::Restarting
        | GatewayState::Stopping => Health::Warn,
        GatewayState::Misconfigured
        | GatewayState::BinaryMissing
        | GatewayState::Foreign
        | GatewayState::YamlRefused
        | GatewayState::BelowFloor
        | GatewayState::NoConfig => Health::Danger,
        GatewayState::Absent
        | GatewayState::Disabled
        | GatewayState::Held
        | GatewayState::Unobserved => Health::Idle,
    }
}

/// The full `shunt` card: [`shunt_check`]'s field lines, then the action
/// section — the blank line, the `enabled` toggle row, the admin fix (its
/// explanation line above it), the `check output` line while a failed check's
/// stderr is held, the codex pool note (or `pool` error), and the MOVE PLAN
/// section. Fills the card's [`Check::shunt_focus`] (the fix lines, the
/// `enabled` row and the `check output` row — the only actionable lines) and
/// its first fix line's [`Check::fix`].
///
/// `daemon_absent` is the header chip's no-daemon liveness (the same
/// [`crate::daemon::DaemonHealth::Absent`] the `[ daemon ]` chip reads):
/// alongside `record.disabled` and the slot states whose supervisor holds no
/// child, it is what "off" is derived from, never the feed's freshness.
#[allow(clippy::too_many_arguments)]
pub(crate) fn shunt_card(
    slot: &crate::daemon::gateway::GatewaySlot,
    supervised: bool,
    daemon_absent: bool,
    standalone: Option<&StandaloneShunt>,
    record: Option<&crate::gateway::GatewayRecord>,
    admin_need: Option<crate::gateway::AdminNeed>,
    move_plan: Option<&MovePlanOutcome>,
    pool: Option<&PoolOutcome>,
    check_output: bool,
) -> Check {
    let mut check = shunt_check(slot, supervised, standalone);

    // The "the daemon runs the gateway" note names the daemon only for a
    // record the gateway WOULD run on with no daemon to run it (the
    // `unobserved` verdict) — never for absent/disabled/no_config, where no
    // gateway would run at all. It rides the field lines, before the action
    // section, so the focus indices below stay true.
    if !supervised && slot.state == GatewayState::Unobserved {
        check.detail.push("the daemon runs the gateway".to_string());
    }

    if slot.state == GatewayState::Absent {
        // `f  adopt config` sits directly under the absent card's lines, and
        // only a readable TOML find whose bind clauth READ gets it: a YAML,
        // unreadable or unparsed find, an unreadable bind, or a daemon env clauth cannot
        // read offers nothing, since the adopt would only ever fail.
        let toml = standalone.and_then(|s| {
            if s.unread_bind.is_none() && s.unread_config.is_none() && !s.unread_env {
                s.found.as_ref()
            } else {
                None
            }
        });
        if let Some(path) = toml {
            check.shunt_action_start = Some(check.detail.len());
            let line = check.detail.len();
            check
                .detail
                .push(fix_line(&ServiceFix::AdoptConfig(path.clone())));
            check.shunt_focus.push(ShuntFocus::Fix {
                line,
                fix: ServiceFix::AdoptConfig(path.clone()),
            });
            check.fix = Some(ServiceFix::AdoptConfig(path.clone()));
        }
        return check;
    }

    let Some(record) = record else {
        return check;
    };

    // `off` = the managed gateway cannot be answering on its bind: the record
    // is `disabled`, no daemon holds the singleton, or the published slot is
    // one in which the supervisor holds no child. That set, derived from
    // `src/daemon/gateway.rs` at source: `idle_stops_child` names `absent`/
    // `disabled`/`no_config`/`held`; the Idle intents `yaml_refused` and
    // `misconfigured` publish only from the no-child path (a running child
    // keeps its live slot, never `misconfigured`), and `binary_missing`,
    // `below_floor` and `restarting` publish only after the child is gone
    // (a spawn miss, a refused-and-held stop, the exit backoff). A `foreign`
    // slot is never off (someone answers); a stale feed reads as unknown (a
    // wedged daemon's gateway may still answer), so no MOVE PLAN.
    let off = record.disabled
        || daemon_absent
        || matches!(
            slot.state,
            GatewayState::Disabled
                | GatewayState::Held
                | GatewayState::NoConfig
                | GatewayState::YamlRefused
                | GatewayState::Misconfigured
                | GatewayState::BinaryMissing
                | GatewayState::BelowFloor
                | GatewayState::Restarting
        );

    check.shunt_action_start = Some(check.detail.len());
    check.detail.push(String::new());

    // `enabled` toggle row.
    let line = check.detail.len();
    check.detail.push("enabled".to_string());
    check.shunt_focus.push(ShuntFocus::Enabled { line });

    // The admin fix, its explanation line directly above the `f` line.
    match admin_need {
        Some(crate::gateway::AdminNeed::WriteKey) => {
            check
                .detail
                .push("admin  clauth's key is not in [server.admin]".to_string());
            let line = check.detail.len();
            check.detail.push(fix_line(&ServiceFix::AddAdminKey));
            check.shunt_focus.push(ShuntFocus::Fix {
                line,
                fix: ServiceFix::AddAdminKey,
            });
        }
        Some(crate::gateway::AdminNeed::AdminTable) => {
            check
                .detail
                .push("admin  no [server.admin] table".to_string());
            let line = check.detail.len();
            check.detail.push(fix_line(&ServiceFix::AddAdminTable));
            check.shunt_focus.push(ShuntFocus::Fix {
                line,
                fix: ServiceFix::AddAdminTable,
            });
        }
        Some(crate::gateway::AdminNeed::Neither) | None => {}
    }

    // The failed check's output, after the admin fix lines (an admin edit's
    // sits right under the fix it came from; a config save's follows whatever
    // fix line the card shows): an actionable line whose ↵/space opens the
    // read-only masked-stderr modal.
    if check_output {
        let line = check.detail.len();
        check.detail.push("check output".to_string());
        check.shunt_focus.push(ShuntFocus::CheckOutput { line });
    }

    // The codex pool note, or the `pool` error in its place. Every provider
    // name escapes control/bidi characters. These lines are short (and the
    // `pool` error wraps), so they are not walk stops.
    match pool {
        Some(PoolOutcome::Providers(providers)) => {
            for provider in providers {
                check.detail.push(format!(
                    "codex  provider {} needs its own pool login",
                    escape_control(provider)
                ));
            }
        }
        Some(PoolOutcome::Error(error)) => {
            check
                .detail
                .push(format!("pool  {}", escape_control(error)));
        }
        None => {}
    }

    // The MOVE PLAN section, only while a move is due (off, and the plan moves
    // ≥1 file or refuses). A blank line precedes it (the herdr OPTIONS /
    // Config-band spacing rule).
    if off {
        match move_plan {
            Some(MovePlanOutcome::Plan(plan)) => {
                check.detail.push(String::new());
                check.detail.push("MOVE PLAN".to_string());
                for file in &plan.moved {
                    check
                        .detail
                        .push(format!("moves  {}", prose_path(&file.from)));
                }
                for kept in &plan.kept {
                    check.detail.push(kept_line(kept));
                }
                let line = check.detail.len();
                check.detail.push(fix_line(&ServiceFix::MoveStoresIn));
                check.shunt_focus.push(ShuntFocus::Fix {
                    line,
                    fix: ServiceFix::MoveStoresIn,
                });
            }
            Some(MovePlanOutcome::Refused(reason)) => {
                check.detail.push(String::new());
                check.detail.push("MOVE PLAN".to_string());
                check
                    .detail
                    .push(format!("refused  {}", escape_control(reason)));
            }
            _ => {}
        }
    }

    // The row's first fix line, never the `enabled` row.
    check.fix = check.shunt_focus.iter().find_map(ShuntFocus::fix).cloned();
    check
}

/// A `read_view` failure as the card's reason words: the config's own read
/// failure reads `unreadable`, a TOML parse failure `does not parse`; an
/// `EditRefusal` (a shape the editor does not read) carries the refusal's
/// sentence, rendered as `config  not editable` under it. The `yaml` word never
/// arises here: a loaded record's config was checked not to be YAML at adopt.
/// An env file failure never reaches this: the view carries it typed.
pub(crate) fn classify_shunt_config_read(e: &anyhow::Error) -> ShuntConfigRead {
    if e.is::<std::io::Error>() {
        return ShuntConfigRead::Unread(UnreadConfig::Unreadable);
    }
    if let Some(refusal) = e.downcast_ref::<crate::gateway_edit::EditRefusal>() {
        return ShuntConfigRead::Refused(refusal.to_string());
    }
    ShuntConfigRead::Unread(UnreadConfig::Unparsed)
}

/// The config section's entry rows in render order — the one row model both the
/// render and the focus walk derive from, so they cannot disagree for any view:
/// under `ProviderForm::Legacy` there are no upstream entries, to either. An
/// upstream and a model are each keyed by their name-or-id plus the occurrence
/// index among entries of that kind sharing it (shunt keeps duplicate upstream
/// names and duplicate map-less ids valid), so two same-name upstreams or
/// same-id models are distinct rows.
pub(crate) fn config_entry_keys(view: &crate::gateway_edit::ConfigView) -> Vec<ShuntEntryKey> {
    let mut keys = Vec::new();
    if let crate::gateway_edit::ProviderForm::Upstreams = view.form {
        for (i, u) in view.upstreams.iter().enumerate() {
            let name = entry_name(u.name.as_ref());
            let occurrence = view
                .upstreams
                .iter()
                .take(i)
                .filter(|earlier| entry_name(earlier.name.as_ref()) == name)
                .count();
            keys.push(ShuntEntryKey::Upstream { name, occurrence });
        }
    }
    for (i, m) in view.models.iter().enumerate() {
        let id = entry_name(m.id.as_ref());
        let occurrence = view
            .models
            .iter()
            .take(i)
            .filter(|earlier| entry_name(earlier.id.as_ref()) == id)
            .count();
        keys.push(ShuntEntryKey::Model { id, occurrence });
    }
    keys.extend(view.routes.iter().map(|r| ShuntEntryKey::Route(r.position)));
    keys
}

/// The read `ConfigView` the shunt card's config section renders from.
pub(crate) fn config_view(app: &App) -> Option<&crate::gateway_edit::ConfigView> {
    match &app.services.shunt_config {
        Some(ShuntConfigRead::Ok(view)) => Some(view.as_ref()),
        _ => None,
    }
}

/// The `BaseSource` an unmanaged client-token value came from, for the card's
/// dim source word.
fn token_source(source: crate::gateway_tokens::BaseSource) -> ShuntTokenSource {
    match source {
        crate::gateway_tokens::BaseSource::EnvFile(_) => ShuntTokenSource::EnvFile,
        crate::gateway_tokens::BaseSource::Inherited => ShuntTokenSource::DaemonEnv,
    }
}

/// A token read failure's sentence, for the `└` line under the group's
/// `tokens  not read` / `other clients  not read` line: a typed refusal's own
/// sentence, any other error its chain.
fn token_read_reason(e: &anyhow::Error) -> String {
    match e.downcast_ref::<crate::gateway_tokens::ClientTokenRefusal>() {
        Some(refusal) => refusal.to_string(),
        None => error_toast_detail(e),
    }
}

/// The client-token section's data, read beside the config view: the store's
/// names and the unmanaged client names (with their source) from
/// [`crate::gateway::client_auth`]. A store read failure records its sentence
/// in `error` (no stored rows); a client-auth read failure records its
/// sentence in `unmanaged_error` (the stored rows stay, the unmanaged rows read
/// `other clients  not read`). Each keeps the `+ add token` row.
fn read_shunt_tokens(record: &crate::gateway::GatewayRecord) -> ShuntTokens {
    let stored: Vec<String> = match crate::gateway_tokens::ClientTokens::load() {
        Ok(tokens) => tokens.names().map(str::to_string).collect(),
        Err(e) => {
            return ShuntTokens {
                error: Some(token_read_reason(&e)),
                ..ShuntTokens::default()
            };
        }
    };
    match crate::gateway::client_auth(record) {
        Ok(auth) => match auth.unmanaged {
            Some((crate::gateway_tokens::TokensState::Names(names), source)) => ShuntTokens {
                stored,
                unmanaged: names
                    .into_iter()
                    .map(|name| (name, token_source(source.clone())))
                    .collect(),
                ..ShuntTokens::default()
            },
            Some((crate::gateway_tokens::TokensState::Unparsed, source)) => ShuntTokens {
                stored,
                unparsed: Some((auth.variable, token_source(source))),
                ..ShuntTokens::default()
            },
            Some((crate::gateway_tokens::TokensState::Unset, _)) | None => ShuntTokens {
                stored,
                ..ShuntTokens::default()
            },
        },
        Err(e) => ShuntTokens {
            stored,
            unmanaged_error: Some(token_read_reason(&e)),
            ..ShuntTokens::default()
        },
    }
}

/// The config entry rows as focus stops, in card order (the render tests'
/// entry-only walk).
#[cfg(test)]
pub(crate) fn config_entry_stops(view: &crate::gateway_edit::ConfigView) -> Vec<ShuntFocus> {
    config_entry_keys(view)
        .into_iter()
        .map(|key| ShuntFocus::Entry { key })
        .collect()
}

/// The entry a [`ShuntEntryKey`] names, resolved by the key's own rule
/// (name-or-id plus its occurrence index, route position) — the one lookup the
/// render and the field model both go through, so a key and the row it names
/// can never disagree.
pub(crate) enum ConfigEntryRef<'a> {
    Upstream(&'a crate::gateway_edit::UpstreamView),
    Model(&'a crate::gateway_edit::ModelView),
    Route(&'a crate::gateway_edit::RouteView),
}

pub(crate) fn entry_for_key<'a>(
    view: &'a crate::gateway_edit::ConfigView,
    key: &ShuntEntryKey,
) -> Option<ConfigEntryRef<'a>> {
    match key {
        ShuntEntryKey::Upstream { name, occurrence } => view
            .upstreams
            .iter()
            .filter(|u| entry_name(u.name.as_ref()) == *name)
            .nth(*occurrence)
            .map(ConfigEntryRef::Upstream),
        ShuntEntryKey::Model { id, occurrence } => view
            .models
            .iter()
            .filter(|m| entry_name(m.id.as_ref()) == *id)
            .nth(*occurrence)
            .map(ConfigEntryRef::Model),
        ShuntEntryKey::Route(position) => view
            .routes
            .iter()
            .find(|r| r.position == *position)
            .map(ConfigEntryRef::Route),
    }
}

/// The field rows one expanded entry renders, in render order — the single row
/// model the render and the focus walk both derive from, so they cannot
/// disagree for any view. The auth sub-rows follow the MODE (never the auth's
/// spelling): `api_key` takes `auth env` + `auth header`, the four pool OAuth
/// modes (`claude_oauth`, `chatgpt_oauth`, `kimi_oauth`, `antigravity_oauth`)
/// take `account` (only when set) + `accounts`, every other mode or none takes
/// neither. A model's `upstream_model` rows are keyed by their provider.
pub(crate) fn entry_field_keys(
    view: &crate::gateway_edit::ConfigView,
    key: &ShuntEntryKey,
) -> Vec<ShuntFieldKey> {
    match entry_for_key(view, key) {
        Some(ConfigEntryRef::Upstream(u)) => upstream_field_keys(key, u),
        Some(ConfigEntryRef::Model(m)) => model_field_keys(key, m),
        Some(ConfigEntryRef::Route(r)) => route_field_keys(r),
        None => Vec::new(),
    }
}

/// The auth sub-rows one auth mode shows — the single row model shared by
/// C2's existing-entry rows, the draft's rows, and the add the draft builds,
/// so the form never writes a key whose row the current mode does not show.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AuthModeRows {
    /// `api_key`: `auth env` + `auth header`.
    ApiKey,
    /// A pool OAuth mode: `account` (only when set, for an existing entry) +
    /// `accounts`.
    OAuth,
    /// Any other mode, or none: no sub-rows.
    None,
}

/// The sub-rows a mode name shows, per [`AuthModeRows`].
fn auth_mode_rows(mode: Option<&str>) -> AuthModeRows {
    match mode {
        Some("api_key") => AuthModeRows::ApiKey,
        Some("claude_oauth" | "chatgpt_oauth" | "kimi_oauth" | "antigravity_oauth") => {
            AuthModeRows::OAuth
        }
        _ => AuthModeRows::None,
    }
}

fn upstream_field_keys(
    key: &ShuntEntryKey,
    u: &crate::gateway_edit::UpstreamView,
) -> Vec<ShuntFieldKey> {
    let ShuntEntryKey::Upstream { name, occurrence } = key else {
        unreachable!("an upstream field key names an upstream entry");
    };
    let at = |field| ShuntFieldKey::Upstream {
        name: name.clone(),
        occurrence: *occurrence,
        field,
    };
    let mut keys = vec![
        at(UpstreamFieldKind::Name),
        at(UpstreamFieldKind::Provider),
        at(UpstreamFieldKind::Kind),
        at(UpstreamFieldKind::BaseUrl),
        at(UpstreamFieldKind::AuthMode),
    ];
    if let Some(auth) = &u.auth {
        match auth_mode_rows(auth.mode.as_ref().map(written_display).as_deref()) {
            AuthModeRows::ApiKey => {
                keys.push(at(UpstreamFieldKind::AuthEnv));
                keys.push(at(UpstreamFieldKind::AuthHeader));
            }
            AuthModeRows::OAuth => {
                if auth.account.is_some() {
                    keys.push(at(UpstreamFieldKind::AuthAccount));
                }
                keys.push(at(UpstreamFieldKind::Accounts));
            }
            AuthModeRows::None => {}
        }
    }
    keys.push(at(UpstreamFieldKind::Effort));
    keys.push(at(UpstreamFieldKind::ServiceTier));
    keys
}

fn model_field_keys(key: &ShuntEntryKey, m: &crate::gateway_edit::ModelView) -> Vec<ShuntFieldKey> {
    let ShuntEntryKey::Model { id, occurrence } = key else {
        unreachable!("a model field key names a model entry");
    };
    let mut keys = vec![
        ShuntFieldKey::Model {
            id: id.clone(),
            occurrence: *occurrence,
            field: ModelFieldKind::Id,
        },
        ShuntFieldKey::Model {
            id: id.clone(),
            occurrence: *occurrence,
            field: ModelFieldKind::DisplayName,
        },
    ];
    if let Some(map) = &m.upstream_model {
        keys.extend(map.iter().map(|(provider, _)| ShuntFieldKey::Model {
            id: id.clone(),
            occurrence: *occurrence,
            field: ModelFieldKind::UpstreamModel(provider.clone()),
        }));
    }
    keys
}

fn route_field_keys(r: &crate::gateway_edit::RouteView) -> Vec<ShuntFieldKey> {
    [
        RouteFieldKind::Model,
        RouteFieldKind::Provider,
        RouteFieldKind::UpstreamModel,
        RouteFieldKind::Effort,
        RouteFieldKind::ServiceTier,
    ]
    .into_iter()
    .map(|field| ShuntFieldKey::Route {
        position: r.position,
        field,
    })
    .collect()
}

/// The config section's focus stops in render order — the one row model both
/// the render and the focus walk derive from: the `bind` row (only while not
/// overridden and the env file read), `default_provider`, then each entry row
/// and, when expanded, its field rows, then the `CLIENT TOKENS` group's stored
/// token rows and its `+ add token` row. The legacy `providers` row and its
/// note, the eyebrows and the unmanaged token rows are read-only and never take
/// focus.
pub(crate) fn config_field_stops(
    view: &crate::gateway_edit::ConfigView,
    expanded: &HashSet<ShuntEntryKey>,
    drafts: &ShuntDrafts,
    tokens: &ShuntTokens,
) -> Vec<ShuntFocus> {
    let mut stops = Vec::new();
    if matches!(
        view.server.bind_override,
        crate::gateway_edit::BindOverride::None
    ) {
        stops.push(ShuntFocus::Field {
            key: ShuntFieldKey::Bind,
        });
    }
    stops.push(ShuntFocus::Field {
        key: ShuntFieldKey::DefaultProvider,
    });

    let keys = config_entry_keys(view);
    for kind in [
        ShuntAddKind::Upstream,
        ShuntAddKind::Model,
        ShuntAddKind::Route,
    ] {
        for key in keys.iter().filter(|key| entry_kind(key) == kind) {
            stops.push(ShuntFocus::Entry { key: key.clone() });
            if expanded.contains(key) {
                stops.extend(
                    entry_field_keys(view, key)
                        .into_iter()
                        .map(|field| ShuntFocus::Field { key: field }),
                );
                if removable(view, key) {
                    stops.push(ShuntFocus::Remove { key: key.clone() });
                }
            }
        }
        if add_row_offered(view, kind) {
            stops.push(ShuntFocus::Add { kind });
            if drafts.is_open(kind) {
                stops.extend(
                    draft_field_keys(view, kind, drafts)
                        .into_iter()
                        .map(|field| ShuntFocus::Field { key: field }),
                );
                stops.push(ShuntFocus::SaveAdd { kind });
            }
        }
    }
    // The stored token rows in name order, then `+ add token`. The unmanaged
    // rows never join: they are faint and skipped by the walk.
    stops.extend(
        tokens
            .stored
            .iter()
            .cloned()
            .map(|name| ShuntFocus::Token { key: name }),
    );
    stops.push(ShuntFocus::AddToken);
    stops
}

/// The group kind an entry key belongs to.
pub(crate) fn entry_kind(key: &ShuntEntryKey) -> ShuntAddKind {
    match key {
        ShuntEntryKey::Upstream { .. } => ShuntAddKind::Upstream,
        ShuntEntryKey::Model { .. } => ShuntAddKind::Model,
        ShuntEntryKey::Route(_) => ShuntAddKind::Route,
    }
}

/// Whether the `+ add <kind>` row is offered: upstreams only while the config
/// declares `[[upstreams]]` (a legacy `[providers.*]` config's providers are
/// read-only), models and routes always (the groups stay editable on legacy).
pub(crate) fn add_row_offered(view: &crate::gateway_edit::ConfigView, kind: ShuntAddKind) -> bool {
    match kind {
        ShuntAddKind::Upstream => {
            matches!(view.form, crate::gateway_edit::ProviderForm::Upstreams)
        }
        ShuntAddKind::Model | ShuntAddKind::Route => true,
    }
}

/// Whether an expanded entry offers a `remove <kind>` row. The engine's
/// `RemoveUpstream`/`RemoveModel` take the name/id alone and refuse a
/// duplicated name/id (`Ctx::find` → `Ambiguous`), so a duplicated name or id
/// has no remove row (`under-scoped`); routes are keyed by position and
/// always removable.
pub(crate) fn removable(view: &crate::gateway_edit::ConfigView, key: &ShuntEntryKey) -> bool {
    match key {
        ShuntEntryKey::Upstream { name, .. } => {
            view.upstreams
                .iter()
                .filter(|u| entry_name(u.name.as_ref()) == *name)
                .count()
                == 1
        }
        ShuntEntryKey::Model { id, .. } => {
            view.models
                .iter()
                .filter(|m| entry_name(m.id.as_ref()) == *id)
                .count()
                == 1
        }
        ShuntEntryKey::Route(_) => true,
    }
}

/// The field rows one open draft renders, in render order — the same row model
/// the focus walk derives from. The upstream draft's mode-dependent rows
/// follow the draft's own `auth mode` (never the file's), mirroring C2's
/// existing-entry row set; the model draft's `upstream_model` rows follow the
/// config's current provider list.
pub(crate) fn draft_field_keys(
    view: &crate::gateway_edit::ConfigView,
    kind: ShuntAddKind,
    drafts: &ShuntDrafts,
) -> Vec<ShuntFieldKey> {
    let at = |field: ShuntDraftField| ShuntFieldKey::Draft { kind, field };
    let mut keys = Vec::new();
    match kind {
        ShuntAddKind::Upstream => {
            let Some(draft) = &drafts.upstream else {
                return Vec::new();
            };
            let mut fields = vec![
                ShuntDraftField::Upstream(UpstreamFieldKind::Name),
                ShuntDraftField::Upstream(UpstreamFieldKind::Provider),
                ShuntDraftField::Upstream(UpstreamFieldKind::Kind),
                ShuntDraftField::Upstream(UpstreamFieldKind::BaseUrl),
                ShuntDraftField::Upstream(UpstreamFieldKind::AuthMode),
            ];
            match auth_mode_rows(draft.auth_mode.as_deref()) {
                AuthModeRows::ApiKey => {
                    fields.push(ShuntDraftField::Upstream(UpstreamFieldKind::AuthEnv));
                    fields.push(ShuntDraftField::Upstream(UpstreamFieldKind::AuthHeader));
                }
                AuthModeRows::OAuth => {
                    // A fresh draft offers `accounts` only: `account` appears
                    // only when set, and a draft never sets it (no `account`
                    // row), so a one-name list serves a single account.
                    fields.push(ShuntDraftField::Upstream(UpstreamFieldKind::Accounts));
                }
                AuthModeRows::None => {}
            }
            fields.push(ShuntDraftField::Upstream(UpstreamFieldKind::Effort));
            fields.push(ShuntDraftField::Upstream(UpstreamFieldKind::ServiceTier));
            keys.extend(fields.into_iter().map(at));
        }
        ShuntAddKind::Model => {
            if drafts.model.is_none() {
                return Vec::new();
            }
            keys.push(at(ShuntDraftField::Model(ModelFieldKind::Id)));
            keys.push(at(ShuntDraftField::Model(ModelFieldKind::DisplayName)));
            keys.extend(provider_names(view).into_iter().map(|provider| {
                at(ShuntDraftField::Model(ModelFieldKind::UpstreamModel(
                    provider,
                )))
            }));
        }
        ShuntAddKind::Route => {
            if drafts.route.is_none() {
                return Vec::new();
            }
            keys.extend(
                [
                    RouteFieldKind::Model,
                    RouteFieldKind::Provider,
                    RouteFieldKind::UpstreamModel,
                    RouteFieldKind::Effort,
                    RouteFieldKind::ServiceTier,
                ]
                .into_iter()
                .map(|field| at(ShuntDraftField::Route(field))),
            );
        }
    }
    keys
}

/// The provider names a model draft's `upstream_model` rows follow, in config
/// order and one row per DISTINCT name (first occurrence's order): the
/// `[[upstreams]]` names, or a legacy `[providers.*]` config's provider keys.
/// Duplicate upstream names are valid for shunt, but a model maps provider →
/// slug by name, so two rows would collide on one `ShuntFieldKey` and build a
/// non-parse TOML map.
fn provider_names(view: &crate::gateway_edit::ConfigView) -> Vec<String> {
    let names: Vec<String> = match &view.form {
        crate::gateway_edit::ProviderForm::Upstreams => view
            .upstreams
            .iter()
            .map(|u| entry_name(u.name.as_ref()))
            .collect(),
        crate::gateway_edit::ProviderForm::Legacy { providers } => providers.clone(),
    };
    let mut seen = std::collections::HashSet::new();
    names
        .into_iter()
        .filter(|name| seen.insert(name.clone()))
        .collect()
}

/// A draft field's committed value, `None` = unset (or the draft of that kind
/// is closed, which a rendered draft field never is).
fn draft_field_value(
    drafts: &ShuntDrafts,
    kind: ShuntAddKind,
    field: &ShuntDraftField,
) -> Option<String> {
    match (kind, field) {
        (ShuntAddKind::Upstream, ShuntDraftField::Upstream(field)) => {
            let draft = drafts.upstream.as_ref()?;
            match field {
                UpstreamFieldKind::Name => draft.name.clone(),
                UpstreamFieldKind::Provider => draft.provider.clone(),
                UpstreamFieldKind::Kind => draft.kind.clone(),
                UpstreamFieldKind::BaseUrl => draft.base_url.clone(),
                UpstreamFieldKind::AuthMode => draft.auth_mode.clone(),
                UpstreamFieldKind::AuthAccount => draft.account.clone(),
                UpstreamFieldKind::Accounts => draft.accounts.clone(),
                UpstreamFieldKind::AuthEnv => draft.env.clone(),
                UpstreamFieldKind::AuthHeader => draft.header.clone(),
                UpstreamFieldKind::Effort => draft.effort.clone(),
                UpstreamFieldKind::ServiceTier => draft.service_tier.clone(),
            }
        }
        (ShuntAddKind::Model, ShuntDraftField::Model(field)) => {
            let draft = drafts.model.as_ref()?;
            match field {
                ModelFieldKind::Id => draft.id.clone(),
                ModelFieldKind::DisplayName => draft.display_name.clone(),
                ModelFieldKind::UpstreamModel(provider) => draft
                    .upstream_model
                    .iter()
                    .find(|(name, _)| name == provider)
                    .and_then(|(_, slug)| slug.clone()),
            }
        }
        (ShuntAddKind::Route, ShuntDraftField::Route(field)) => {
            let draft = drafts.route.as_ref()?;
            match field {
                RouteFieldKind::Model => draft.model.clone(),
                RouteFieldKind::Provider => draft.provider.clone(),
                RouteFieldKind::UpstreamModel => draft.upstream_model.clone(),
                RouteFieldKind::Effort => draft.effort.clone(),
                RouteFieldKind::ServiceTier => draft.service_tier.clone(),
            }
        }
        _ => None,
    }
}

/// Commit `value` into a draft field, in memory: empty text unsets it. The
/// caller rebuilds the config stops afterwards, since a committed auth mode
/// changes the mode-dependent row set.
fn set_draft_field_value(
    drafts: &mut ShuntDrafts,
    kind: ShuntAddKind,
    field: &ShuntDraftField,
    value: &str,
) {
    let value = (!value.is_empty()).then(|| value.to_string());
    match (kind, field) {
        (ShuntAddKind::Upstream, ShuntDraftField::Upstream(field)) => {
            let draft = drafts.upstream.get_or_insert_with(UpstreamDraft::default);
            match field {
                UpstreamFieldKind::Name => draft.name = value,
                UpstreamFieldKind::Provider => draft.provider = value,
                UpstreamFieldKind::Kind => draft.kind = value,
                UpstreamFieldKind::BaseUrl => draft.base_url = value,
                UpstreamFieldKind::AuthMode => draft.auth_mode = value,
                UpstreamFieldKind::AuthAccount => draft.account = value,
                UpstreamFieldKind::Accounts => draft.accounts = value,
                UpstreamFieldKind::AuthEnv => draft.env = value,
                UpstreamFieldKind::AuthHeader => draft.header = value,
                UpstreamFieldKind::Effort => draft.effort = value,
                UpstreamFieldKind::ServiceTier => draft.service_tier = value,
            }
        }
        (ShuntAddKind::Model, ShuntDraftField::Model(field)) => {
            let draft = drafts.model.get_or_insert_with(ModelDraft::default);
            match field {
                ModelFieldKind::Id => draft.id = value,
                ModelFieldKind::DisplayName => draft.display_name = value,
                ModelFieldKind::UpstreamModel(provider) => {
                    if let Some((_, slug)) = draft
                        .upstream_model
                        .iter_mut()
                        .find(|(name, _)| name == provider)
                    {
                        *slug = value;
                    } else {
                        draft.upstream_model.push((provider.clone(), value));
                    }
                }
            }
        }
        (ShuntAddKind::Route, ShuntDraftField::Route(field)) => {
            let draft = drafts.route.get_or_insert_with(RouteDraft::default);
            match field {
                RouteFieldKind::Model => draft.model = value,
                RouteFieldKind::Provider => draft.provider = value,
                RouteFieldKind::UpstreamModel => draft.upstream_model = value,
                RouteFieldKind::Effort => draft.effort = value,
                RouteFieldKind::ServiceTier => draft.service_tier = value,
            }
        }
        _ => {}
    }
}

/// The value a field row renders and its editor opens on: the decoded string,
/// else the file's own TOML text; the inner `None` = unset (renders `—`). The
/// outer `None` = the key names no editable field. A draft field's value reads
/// the open draft (the inner `None` = the draft's field is unset).
pub(crate) fn shunt_field_value(
    view: &crate::gateway_edit::ConfigView,
    drafts: &ShuntDrafts,
    key: &ShuntFieldKey,
) -> Option<Option<String>> {
    match key {
        ShuntFieldKey::Bind => Some(written_value(view.server.bind.as_ref())),
        ShuntFieldKey::DefaultProvider => {
            Some(written_value(view.server.default_provider.as_ref()))
        }
        ShuntFieldKey::Draft { kind, field } => Some(draft_field_value(drafts, *kind, field)),
        ShuntFieldKey::Upstream {
            name,
            occurrence,
            field,
        } => {
            let key = ShuntEntryKey::Upstream {
                name: name.clone(),
                occurrence: *occurrence,
            };
            let ConfigEntryRef::Upstream(u) = entry_for_key(view, &key)? else {
                unreachable!("an upstream field key resolves to an upstream");
            };
            Some(match field {
                UpstreamFieldKind::Name => written_value(u.name.as_ref()),
                UpstreamFieldKind::Provider => written_value(u.provider.as_ref()),
                UpstreamFieldKind::Kind => written_value(u.kind.as_ref()),
                UpstreamFieldKind::BaseUrl => written_value(u.base_url.as_ref()),
                UpstreamFieldKind::AuthMode => {
                    written_value(u.auth.as_ref().and_then(|a| a.mode.as_ref()))
                }
                UpstreamFieldKind::AuthAccount => {
                    written_value(u.auth.as_ref().and_then(|a| a.account.as_ref()))
                }
                UpstreamFieldKind::Accounts => u
                    .auth
                    .as_ref()
                    .and_then(|a| a.accounts.as_ref())
                    .map(|accounts| accounts_joined(accounts))
                    .filter(|names| !names.is_empty()),
                UpstreamFieldKind::AuthEnv => {
                    written_value(u.auth.as_ref().and_then(|a| a.env.as_ref()))
                }
                UpstreamFieldKind::AuthHeader => {
                    written_value(u.auth.as_ref().and_then(|a| a.header.as_ref()))
                }
                UpstreamFieldKind::Effort => written_value(u.effort.as_ref()),
                UpstreamFieldKind::ServiceTier => written_value(u.service_tier.as_ref()),
            })
        }
        ShuntFieldKey::Model {
            id,
            occurrence,
            field,
        } => {
            let key = ShuntEntryKey::Model {
                id: id.clone(),
                occurrence: *occurrence,
            };
            let ConfigEntryRef::Model(m) = entry_for_key(view, &key)? else {
                unreachable!("a model field key resolves to a model");
            };
            Some(match field {
                ModelFieldKind::Id => written_value(m.id.as_ref()),
                ModelFieldKind::DisplayName => written_value(m.display_name.as_ref()),
                ModelFieldKind::UpstreamModel(provider) => m
                    .upstream_model
                    .as_ref()
                    .and_then(|map| map.iter().find(|(k, _)| k == provider))
                    .map(|(_, slug)| written_value(Some(slug)))
                    .unwrap_or(None),
            })
        }
        ShuntFieldKey::Route { position, field } => {
            let ConfigEntryRef::Route(r) = entry_for_key(view, &ShuntEntryKey::Route(*position))?
            else {
                unreachable!("a route field key resolves to a route");
            };
            Some(match field {
                RouteFieldKind::Model => written_value(r.model.as_ref()),
                RouteFieldKind::Provider => written_value(r.provider.as_ref()),
                RouteFieldKind::UpstreamModel => written_value(r.upstream_model.as_ref()),
                RouteFieldKind::Effort => written_value(r.effort.as_ref()),
                RouteFieldKind::ServiceTier => written_value(r.service_tier.as_ref()),
            })
        }
    }
}

/// A `Written` as its render value: the decoded string, else the raw TOML text.
fn written_value(w: Option<&crate::gateway_edit::Written>) -> Option<String> {
    w.map(written_display)
}

/// The `Edit` one field key's save produces, from the typed text (the picked
/// value for a cycle row's pending pick). A typed field's empty text unsets the
/// key; a rename's empty text renames to the empty string (the check refuses);
/// an emptied accounts list removes the key.
pub(crate) fn edit_for_field(
    view: &crate::gateway_edit::ConfigView,
    key: &ShuntFieldKey,
    typed: &str,
) -> Option<crate::gateway_edit::Edit> {
    use crate::gateway_edit::{Edit, ModelField, RouteField, ServerField, UpstreamField};
    let unset = |text: &str| (!text.is_empty()).then(|| text.to_string());
    match key {
        ShuntFieldKey::Bind => Some(Edit::Server {
            field: ServerField::Bind,
            value: unset(typed),
        }),
        ShuntFieldKey::DefaultProvider => Some(Edit::Server {
            field: ServerField::DefaultProvider,
            value: unset(typed),
        }),
        ShuntFieldKey::Upstream { name, field, .. } => match field {
            UpstreamFieldKind::Name => Some(Edit::RenameUpstream {
                name: name.clone(),
                to: typed.to_string(),
            }),
            UpstreamFieldKind::Accounts => Some(Edit::SetAccounts {
                name: name.clone(),
                accounts: split_accounts(typed),
            }),
            UpstreamFieldKind::Provider => Some(Edit::Upstream {
                name: name.clone(),
                field: UpstreamField::Provider,
                value: unset(typed),
            }),
            UpstreamFieldKind::Kind => Some(Edit::Upstream {
                name: name.clone(),
                field: UpstreamField::Kind,
                value: unset(typed),
            }),
            UpstreamFieldKind::BaseUrl => Some(Edit::Upstream {
                name: name.clone(),
                field: UpstreamField::BaseUrl,
                value: unset(typed),
            }),
            UpstreamFieldKind::AuthMode => Some(Edit::Upstream {
                name: name.clone(),
                field: UpstreamField::AuthMode,
                value: unset(typed),
            }),
            UpstreamFieldKind::AuthAccount => Some(Edit::Upstream {
                name: name.clone(),
                field: UpstreamField::AuthAccount,
                value: unset(typed),
            }),
            UpstreamFieldKind::AuthEnv => Some(Edit::Upstream {
                name: name.clone(),
                field: UpstreamField::AuthEnv,
                value: unset(typed),
            }),
            UpstreamFieldKind::AuthHeader => Some(Edit::Upstream {
                name: name.clone(),
                field: UpstreamField::AuthHeader,
                value: unset(typed),
            }),
            UpstreamFieldKind::Effort => Some(Edit::Upstream {
                name: name.clone(),
                field: UpstreamField::Effort,
                value: unset(typed),
            }),
            UpstreamFieldKind::ServiceTier => Some(Edit::Upstream {
                name: name.clone(),
                field: UpstreamField::ServiceTier,
                value: unset(typed),
            }),
        },
        ShuntFieldKey::Model { id, field, .. } => match field {
            ModelFieldKind::Id => Some(Edit::RenameModel {
                id: id.clone(),
                to: typed.to_string(),
            }),
            ModelFieldKind::DisplayName => Some(Edit::Model {
                id: id.clone(),
                field: ModelField::DisplayName,
                value: unset(typed),
            }),
            ModelFieldKind::UpstreamModel(provider) => Some(Edit::Model {
                id: id.clone(),
                field: ModelField::UpstreamModel(provider.clone()),
                value: unset(typed),
            }),
        },
        ShuntFieldKey::Route { position, field } => {
            let route = match entry_for_key(view, &ShuntEntryKey::Route(*position))? {
                ConfigEntryRef::Route(r) => r.clone(),
                _ => unreachable!("a route field key resolves to a route"),
            };
            let field = match field {
                RouteFieldKind::Model => RouteField::Model,
                RouteFieldKind::Provider => RouteField::Provider,
                RouteFieldKind::UpstreamModel => RouteField::UpstreamModel,
                RouteFieldKind::Effort => RouteField::Effort,
                RouteFieldKind::ServiceTier => RouteField::ServiceTier,
            };
            Some(Edit::Route {
                route,
                field,
                value: unset(typed),
            })
        }
        // A draft field never produces a file edit: its commit lands in the
        // draft (handled before `edit_for_field` is reached).
        ShuntFieldKey::Draft { .. } => None,
    }
}

/// Split a typed accounts list: comma-separated, each name trimmed, empties
/// dropped, order kept.
fn split_accounts(text: &str) -> Vec<String> {
    text.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// The account names a `accounts` field renders, comma-joined.
fn accounts_joined(accounts: &[crate::gateway_edit::AccountView]) -> String {
    accounts
        .iter()
        .filter_map(account_display)
        .collect::<Vec<_>>()
        .join(", ")
}

/// One account's name as it renders; `None` for a selection with no name.
fn account_display(a: &crate::gateway_edit::AccountView) -> Option<String> {
    match a {
        crate::gateway_edit::AccountView::Name(w) => Some(escape_control(&written_display(w))),
        crate::gateway_edit::AccountView::Selection { name: Some(w), .. } => {
            Some(escape_control(&written_display(w)))
        }
        crate::gateway_edit::AccountView::Selection { name: None, .. } => None,
    }
}

/// The cycle options of a cycle field, in shunt's own order (off shunt
/// 532f5c7's config types): the upstream `provider` presets, `kind` minus the
/// deprecated `antigravity_cli`, the `auth mode` enum, the `auth header` enum,
/// and `service_tier`'s three normalized values. Empty = not a cycle row.
pub(crate) fn cycle_options(key: &ShuntFieldKey) -> &'static [&'static str] {
    match key {
        ShuntFieldKey::Upstream {
            field: UpstreamFieldKind::Provider,
            ..
        } => &UPSTREAM_PROVIDERS,
        ShuntFieldKey::Upstream {
            field: UpstreamFieldKind::Kind,
            ..
        } => &KINDS,
        ShuntFieldKey::Upstream {
            field: UpstreamFieldKind::AuthMode,
            ..
        } => &AUTH_MODES,
        ShuntFieldKey::Upstream {
            field: UpstreamFieldKind::AuthHeader,
            ..
        } => &AUTH_HEADERS,
        ShuntFieldKey::Upstream {
            field: UpstreamFieldKind::ServiceTier,
            ..
        }
        | ShuntFieldKey::Route {
            field: RouteFieldKind::ServiceTier,
            ..
        } => &SERVICE_TIERS,
        ShuntFieldKey::Draft { field, .. } => match field {
            ShuntDraftField::Upstream(UpstreamFieldKind::Provider) => &UPSTREAM_PROVIDERS,
            ShuntDraftField::Upstream(UpstreamFieldKind::Kind) => &KINDS,
            ShuntDraftField::Upstream(UpstreamFieldKind::AuthMode) => &AUTH_MODES,
            ShuntDraftField::Upstream(UpstreamFieldKind::AuthHeader) => &AUTH_HEADERS,
            ShuntDraftField::Upstream(UpstreamFieldKind::ServiceTier)
            | ShuntDraftField::Route(RouteFieldKind::ServiceTier) => &SERVICE_TIERS,
            _ => &[],
        },
        _ => &[],
    }
}

/// The upstream `provider` presets, in the order shunt's own preset table
/// declares them.
pub(crate) const UPSTREAM_PROVIDERS: [&str; 11] = [
    "anthropic",
    "codex",
    "openai",
    "xai",
    "grok",
    "kimi",
    "cursor",
    "kimi-code",
    "zhipu",
    "minimax-cn",
    "opencode",
];

/// The `kind` values, `antigravity_cli` omitted (deprecated).
pub(crate) const KINDS: [&str; 5] = ["anthropic", "responses", "cursor", "gemini", "antigravity"];

/// The `auth` mode names, in the order shunt's `AuthMode` enum declares them.
pub(crate) const AUTH_MODES: [&str; 10] = [
    "passthrough",
    "api_key",
    "chatgpt_oauth",
    "claude_oauth",
    "xai_oauth",
    "cursor_oauth",
    "kimi_oauth",
    "google_oauth",
    "antigravity_oauth",
    "none",
];

/// The `auth.header` names, in the order shunt's `ApiKeyHeader` enum declares
/// them.
pub(crate) const AUTH_HEADERS: [&str; 2] = ["bearer", "x_api_key"];

/// The `service_tier` values, in the order `normalize_service_tier_value`
/// reads them (the `fast` alias normalizes to `priority` and is not a pick).
pub(crate) const SERVICE_TIERS: [&str; 3] = ["priority", "flex", "default"];

/// Whether `key` is a cycle row (`space` moves the pick; `↵` at rest opens the
/// typed custom-value editor).
pub(crate) fn is_cycle_field(key: &ShuntFieldKey) -> bool {
    !cycle_options(key).is_empty()
}

/// The field key a successful rename moves focus to: the renamed entry's
/// name/id field under the new name, with the occurrence the rename gives it
/// (rename never reorders, so the new occurrence is the count of same-named
/// entries before it). Non-rename keys map to themselves.
pub(crate) fn renamed_field_key(
    view: &crate::gateway_edit::ConfigView,
    key: &ShuntFieldKey,
    to: &str,
) -> Option<ShuntFieldKey> {
    match key {
        ShuntFieldKey::Upstream {
            name,
            occurrence,
            field: UpstreamFieldKind::Name,
        } => {
            let idx = view
                .upstreams
                .iter()
                .enumerate()
                .filter(|(_, u)| entry_name(u.name.as_ref()) == *name)
                .nth(*occurrence)
                .map(|(i, _)| i)?;
            let new_occurrence = view
                .upstreams
                .iter()
                .take(idx)
                .filter(|u| entry_name(u.name.as_ref()) == *to)
                .count();
            Some(ShuntFieldKey::Upstream {
                name: to.to_string(),
                occurrence: new_occurrence,
                field: UpstreamFieldKind::Name,
            })
        }
        ShuntFieldKey::Model {
            id,
            occurrence,
            field: ModelFieldKind::Id,
        } => {
            let idx = view
                .models
                .iter()
                .enumerate()
                .filter(|(_, m)| entry_name(m.id.as_ref()) == *id)
                .nth(*occurrence)
                .map(|(i, _)| i)?;
            let new_occurrence = view
                .models
                .iter()
                .take(idx)
                .filter(|m| entry_name(m.id.as_ref()) == *to)
                .count();
            Some(ShuntFieldKey::Model {
                id: to.to_string(),
                occurrence: new_occurrence,
                field: ModelFieldKind::Id,
            })
        }
        _ => Some(key.clone()),
    }
}

/// Drop every expanded key whose entry is not in `view` (an entry that left the
/// config). Only a read view prunes — a failed read keeps the set.
pub(crate) fn prune_shunt_expanded(
    view: &crate::gateway_edit::ConfigView,
    expanded: &mut HashSet<ShuntEntryKey>,
) {
    let live: HashSet<ShuntEntryKey> = config_entry_keys(view).into_iter().collect();
    expanded.retain(|key| live.contains(key));
}

/// The name/id an entry is keyed by: the value as it renders, the raw TOML
/// text when it is not a plain string, empty when the key is absent (a shape
/// shunt itself would refuse).
pub(crate) fn entry_name(w: Option<&crate::gateway_edit::Written>) -> String {
    written_text(w).unwrap_or_default()
}

/// A `Written` as it renders: the decoded string when it is one, else the
/// file's own text (a `${…}` reference stays unresolved, a number keeps its
/// spelling).
pub(crate) fn written_display(w: &crate::gateway_edit::Written) -> String {
    w.value.clone().unwrap_or_else(|| w.raw.clone())
}

/// A `Written` as it renders: the decoded string when it is one, else the
/// file's own text (a `${…}` reference stays unresolved, a number keeps its
/// spelling).
fn written_text(w: Option<&crate::gateway_edit::Written>) -> Option<String> {
    w.map(written_display)
}

/// A path rendered inside a verbatim card line (a MOVE PLAN row): control and
/// bidi characters spelled as their visible escape, everything else verbatim —
/// the verbatim renderer never splits on `: `, so a Windows `C:` or a `: ` in
/// the path passes through untouched.
fn prose_path(path: &std::path::Path) -> String {
    escape_control(&path.to_string_lossy())
}

/// One MOVE PLAN `stays`/`NoHome` row, in its rendered two-space form.
fn kept_line(kept: &crate::gateway::KeptFile) -> String {
    match kept {
        crate::gateway::KeptFile::At { path, reason } => {
            format!("stays  {}  {}", prose_path(path), kept_reason_word(*reason))
        }
        crate::gateway::KeptFile::NoHome { store } => {
            format!("stays  {store}  no home to find it: set HOME in the env file")
        }
    }
}

/// The approved reason word for each [`crate::gateway::KeptReason`].
fn kept_reason_word(reason: crate::gateway::KeptReason) -> &'static str {
    match reason {
        crate::gateway::KeptReason::LeftBehind => "not an account file",
        crate::gateway::KeptReason::CodexLogin => "the codex CLI's own login",
        crate::gateway::KeptReason::ClauthOwned => "already clauth's",
        crate::gateway::KeptReason::HardLink => "linked under another name",
        crate::gateway::KeptReason::LinkCountUnreadable => "link count unreadable",
        crate::gateway::KeptReason::DuplicateSource => "named twice",
    }
}

/// The move-plan worker's prober: reload the record and plan its store move.
fn move_plan_probe() -> MovePlanOutcome {
    let Some(record) = crate::gateway::GatewayRecord::load().ok().flatten() else {
        return MovePlanOutcome::Unavailable;
    };
    match crate::gateway::plan_standalone_stores(&record) {
        Ok(plan) if !plan.moved.is_empty() => MovePlanOutcome::Plan(plan),
        Ok(_) => MovePlanOutcome::Nothing,
        Err(e) => MovePlanOutcome::Refused(format!("{e:#}")),
    }
}

/// The pool worker's prober: reload the record and ask the gateway which
/// `chatgpt_oauth` providers have no pool login.
fn pool_probe() -> PoolOutcome {
    let Some(record) = crate::gateway::GatewayRecord::load().ok().flatten() else {
        return PoolOutcome::Providers(Vec::new());
    };
    match crate::gateway::chatgpt_oauth_providers_needing_login(&record) {
        Ok(providers) => PoolOutcome::Providers(providers),
        Err(e) => PoolOutcome::Error(format!("{e:#}")),
    }
}

/// The production action prober: run one confirmed shunt-card action to
/// completion. The slow calls — the `/health` probe, the store plan, `shunt
/// check` — never hold a frame here. Every error keeps its cause chain as
/// [`error_toast_detail`] (root cause first): a failed write names its path in
/// the context and why it failed only in the io error under it.
fn run_shunt_action(job: ShuntActionJob) -> ShuntActionOutcome {
    match job {
        ShuntActionJob::Adopt { found } => run_adopt(&found),
        ShuntActionJob::Move { record, confirmed } => run_move(&record, &confirmed),
        ShuntActionJob::AddAdminKey { record } => {
            run_admin_edit(&record, crate::gateway::AdminNeed::WriteKey)
        }
        ShuntActionJob::AddAdminTable { record } => {
            run_admin_edit(&record, crate::gateway::AdminNeed::AdminTable)
        }
        ShuntActionJob::Hold => match crate::gateway::write_hold() {
            Ok(()) => ShuntActionOutcome::Held,
            Err(e) => ShuntActionOutcome::HoldFailed {
                error: error_toast_detail(&e),
            },
        },
        ShuntActionJob::RemoveHold => match crate::gateway::remove_hold() {
            Ok(()) => ShuntActionOutcome::Released,
            Err(e) => ShuntActionOutcome::ReleaseFailed {
                error: error_toast_detail(&e),
            },
        },
        ShuntActionJob::ConfigEdit {
            record,
            edit,
            key,
            confirmed,
        } => run_config_edit(&record, &edit, key, confirmed),
        ShuntActionJob::RemoveToken { record, profile } => {
            run_remove_client_token(&record, &profile)
        }
        ShuntActionJob::AddToken {
            record,
            profile,
            url,
            config,
        } => run_add_client_token(&record, &profile, &url, &config),
    }
}

/// A token op's error as its message: a [`ClientTokenRefusal`]'s own sentence,
/// any other error its chain (`{e:#}`).
fn token_error_message(e: &anyhow::Error) -> String {
    match e.downcast_ref::<crate::gateway_tokens::ClientTokenRefusal>() {
        Some(refusal) => refusal.to_string(),
        None => format!("{e:#}"),
    }
}

/// Revoke one profile's client token through the engine, classifying the
/// failure the card's rules name: a refusal (the last-token guard or a read
/// failure) keeps its own sentence for the `└` line; anything else is an
/// untyped failure.
fn run_remove_client_token(
    record: &crate::gateway::GatewayRecord,
    profile: &str,
) -> ShuntActionOutcome {
    match crate::gateway_tokens::remove_client_token(record, profile) {
        Ok(removed) => ShuntActionOutcome::TokenRemoved {
            profile: profile.to_string(),
            removed,
        },
        Err(e) => {
            if let Some(refusal) = e.downcast_ref::<crate::gateway_tokens::ClientTokenRefusal>() {
                ShuntActionOutcome::TokenRemoveRefused {
                    profile: profile.to_string(),
                    sentence: refusal.to_string(),
                }
            } else {
                ShuntActionOutcome::TokenRemoveFailed {
                    error: format!("{e:#}"),
                }
            }
        }
    }
}

/// Mint a client token for `profile`, then point it at the gateway through the
/// one endpoint write path ([`crate::actions::edit_profile_endpoint`], its own
/// locking). The token never reaches an outcome, a log line or a `Debug`.
fn run_add_client_token(
    record: &crate::gateway::GatewayRecord,
    profile: &str,
    url: &str,
    config: &JobConfig,
) -> ShuntActionOutcome {
    let profile_name = ProfileName::from(profile);
    // A profile that vanished between the picker and the job refuses before
    // any mint: the config lock is taken and dropped ahead of the state flock
    // `add_client_token` takes (config → flock is the ranked order), so nothing
    // is minted and the gateway never restarts for a deleted profile.
    {
        let cfg = App::lock_config(&config.0);
        if cfg.find(&profile_name).is_none() {
            return ShuntActionOutcome::TokenAddRefused {
                error: format!("{profile} no longer exists"),
            };
        }
    }
    let token = match crate::gateway_tokens::add_client_token(record, profile) {
        Ok(token) => token,
        Err(e) => {
            return ShuntActionOutcome::TokenAddRefused {
                error: token_error_message(&e),
            };
        }
    };
    let mut cfg = App::lock_config(&config.0);
    match crate::actions::edit_profile_endpoint(
        &mut cfg,
        &profile_name,
        Some(url.to_string()),
        Some(token.expose().to_string()),
    ) {
        Ok(()) => ShuntActionOutcome::TokenAdded {
            profile: profile.to_string(),
        },
        Err(e) => {
            // Decide the copy from the profile as saved on disk after the
            // error (the reload's source), never the in-memory copy the write
            // may already have mutated: the save landed and only the
            // active-profile settings write failed -> the profile points at
            // the gateway; the save itself failed -> it was not changed. Both
            // legs must match: a re-mint of a profile already on the gateway
            // whose save then failed keeps its OLD key on disk, so it was not
            // changed even though its base url still matches the gateway. The
            // token only takes part in this comparison, never a message.
            let points_at_gateway = crate::profile::load_profile(&profile_name)
                .ok()
                .is_some_and(|saved| {
                    saved.base_url.as_deref() == Some(url)
                        && saved.api_key.as_deref() == Some(token.expose())
                });
            ShuntActionOutcome::TokenAddedProfileFailed {
                error: if points_at_gateway {
                    format!("{profile} points at the gateway, but {e:#}")
                } else {
                    format!("the token is stored but {profile} was not changed: {e:#}")
                },
            }
        }
    }
}

/// Save one config field through the E1 engine, classifying the failure the
/// card's rules name: an [`EditRefusal`] keeps its own sentence, a
/// [`ConfigEditRefusal::CheckFailed`] its compact line + held stderr, anything
/// else the error chain (root cause first).
fn run_config_edit(
    record: &crate::gateway::GatewayRecord,
    edit: &crate::gateway_edit::Edit,
    key: ShuntSaveKey,
    confirmed: bool,
) -> ShuntActionOutcome {
    use crate::gateway::ConfigEditRefusal;
    match crate::gateway_edit::apply_edit(record, edit) {
        Ok(applied) => ShuntActionOutcome::ConfigSaved {
            key,
            written: applied.written,
            restart_only: applied.restart_only,
            value: edit_save_value(edit),
            confirmed,
        },
        Err(e) => {
            if let Some(refusal) = e.downcast_ref::<crate::gateway_edit::EditRefusal>() {
                return ShuntActionOutcome::ConfigRefused {
                    key,
                    sentence: refusal.to_string(),
                };
            }
            if let Some(ConfigEditRefusal::CheckFailed {
                binary,
                config,
                code,
                stderr,
            }) = e.downcast_ref::<ConfigEditRefusal>()
            {
                return ShuntActionOutcome::ConfigCheckFailed {
                    error: check_refused_line(binary, config, code, "save"),
                    check_output: Some(stderr.clone()),
                    config: record.config().to_path_buf(),
                };
            }
            ShuntActionOutcome::ConfigFailed {
                error: error_toast_detail(&e),
            }
        }
    }
}

/// The value a field save writes, for the saving row's shimmer and the bind
/// restart toast: the new value, the rename target, the accounts list, or
/// `None` for an unset.
fn edit_save_value(edit: &crate::gateway_edit::Edit) -> Option<String> {
    use crate::gateway_edit::Edit;
    match edit {
        Edit::Server { value, .. }
        | Edit::Upstream { value, .. }
        | Edit::Model { value, .. }
        | Edit::Route { value, .. } => value.clone(),
        Edit::RenameUpstream { to, .. } | Edit::RenameModel { to, .. } => Some(to.clone()),
        Edit::SetAccounts { accounts, .. } => Some(accounts.join(", ")),
        Edit::AddUpstream(_)
        | Edit::AddModel(_)
        | Edit::AddRoute(_)
        | Edit::RemoveUpstream { .. }
        | Edit::RemoveModel { .. }
        | Edit::RemoveRoute { .. }
        | Edit::RequireClientTokens => None,
    }
}

/// Adopt `found`: probe the candidate, plan its stores, and write the record
/// (`disabled` while a move is due, enabled when nothing moves).
fn run_adopt(found: &std::path::Path) -> ShuntActionOutcome {
    use crate::gateway::Health;
    let candidate = match crate::gateway::adoptable_record(found.to_path_buf()) {
        Ok(candidate) => candidate,
        Err(e) => {
            return ShuntActionOutcome::AdoptFailed {
                error: error_toast_detail(&e),
            };
        }
    };
    let addr = match crate::gateway::gateway_probe(&candidate) {
        Ok(addr) => addr,
        Err(e) => {
            return ShuntActionOutcome::AdoptFailed {
                error: error_toast_detail(&e),
            };
        }
    };
    match crate::gateway::probe_health(addr) {
        // A standalone shunt answers: refuse the adopt, write nothing.
        Ok(Health::Shunt { version }) => {
            return ShuntActionOutcome::AdoptRefused {
                version: escape_control(&version),
                addr,
            };
        }
        Ok(Health::Silent(_) | Health::NotShunt { .. }) | Err(_) => {}
    }
    // The record stays `disabled` until the move lands: a plan with a file to
    // move (or a refusal) leaves the gateway off so the daemon never starts it
    // on empty stores; a plan with nothing to move adopts enabled.
    let disabled = match crate::gateway::plan_standalone_stores(&candidate) {
        Ok(plan) => !plan.moved.is_empty(),
        Err(_) => true,
    };
    let mut record = candidate;
    record.disabled = disabled;
    let result = crate::gateway::GatewayRecord::update(|slot| {
        if slot.is_some() {
            // Another surface adopted a config meanwhile.
            anyhow::bail!("a gateway is already adopted");
        }
        *slot = Some(record.clone());
        Ok(())
    });
    match result {
        Ok(()) => ShuntActionOutcome::Adopted {
            path: escape_control(&found.to_string_lossy()),
        },
        Err(e) => ShuntActionOutcome::AdoptFailed {
            error: error_toast_detail(&e),
        },
    }
}

/// Move a stopped standalone's stores in behind a fresh silent proof, then the
/// record lands `disabled = false` so the daemon starts the gateway.
fn run_move(
    record: &crate::gateway::GatewayRecord,
    confirmed: &crate::gateway::StoreMovePlan,
) -> ShuntActionOutcome {
    use crate::gateway::Health;
    let addr = match crate::gateway::gateway_probe(record) {
        Ok(addr) => addr,
        Err(e) => {
            return ShuntActionOutcome::MoveFailed {
                error: error_toast_detail(&e),
            };
        }
    };
    let proof = match crate::gateway::probe_health(addr) {
        Ok(Health::Silent(proof)) => proof,
        // A shunt answering is the orchestrator's composition of the adopt
        // warning copy under the `move failed` head (flagged in the report).
        Ok(Health::Shunt { version }) => {
            return ShuntActionOutcome::MoveFailed {
                error: format!("shunt {} answers on {addr}", escape_control(&version)),
            };
        }
        Ok(Health::NotShunt { status }) => {
            return ShuntActionOutcome::MoveFailed {
                error: format!("something other than shunt answers on {addr} (status {status})"),
            };
        }
        Err(e) => {
            return ShuntActionOutcome::MoveFailed {
                error: error_toast_detail(&e),
            };
        }
    };
    match crate::gateway::move_standalone_stores(record, proof, confirmed) {
        Ok(moved) => {
            let disabled_off = crate::gateway::GatewayRecord::update(|slot| {
                if let Some(held) = slot.as_mut() {
                    held.disabled = false;
                }
                Ok(())
            });
            match disabled_off {
                Ok(()) => ShuntActionOutcome::Moved {
                    n: moved.moved.len(),
                },
                // The stores moved, so `moved <N> …` still shows; the record
                // write failed, so the `save failed` toast follows it.
                Err(e) => ShuntActionOutcome::MovedEnableFailed {
                    n: moved.moved.len(),
                    error: error_toast_detail(&e),
                },
            }
        }
        Err(e) => ShuntActionOutcome::MoveFailed {
            error: error_toast_detail(&e),
        },
    }
}

/// One compact toast line for a typed admin refusal: outcome first, files by
/// basename. The `CheckFailed` arm appends the `↵ check output` pointer, whose
/// tail may clip on a 38-cell terminal while the outcome stays in view.
fn admin_refusal_line(refusal: &crate::gateway::ConfigEditRefusal) -> String {
    use crate::gateway::{AdminNeed, ConfigEditRefusal as R};
    match refusal {
        R::Needs(AdminNeed::AdminTable) => {
            "the config has no [server.admin] table; adding one enables shunt's admin API and is its own step".to_string()
        }
        R::Needs(AdminNeed::WriteKey) => {
            "the config already has a [server.admin] table; clauth's write key goes into it instead".to_string()
        }
        R::Needs(AdminNeed::Neither) => "the config already carries clauth's write key".to_string(),
        R::ForeignClauthKey => format!(
            "[server.admin] already has a write key with id {:?} holding another key; clauth adds none beside it; remove that entry, then run the edit again",
            crate::gateway::WRITE_KEY_ID
        ),
        R::UnexpectedShape { what } => {
            format!("{what}; clauth edits only a [server.admin] table and its write_keys array")
        }
        R::TokenPathUnusable { path } => {
            let why = if path.to_str().is_some() {
                "holds a `}`"
            } else {
                "is not UTF-8"
            };
            format!(
                "the clauth dir path {why} · the admin key cannot be a ${{file:}} reference"
            )
        }
        R::Symlink { path } => {
            format!("{} is a symlink · clauth would replace the link by renaming over it", file_basename(path))
        }
        R::ShuntMissing { binary } => {
            format!("cannot run {}: no such file · install shunt or point the gateway at its binary", file_basename(binary))
        }
        R::CheckFailed {
            binary,
            config,
            code,
            ..
        } => check_refused_line(binary, config, code, "edit"),
        R::ChangedDuringEdit { path } => {
            format!("{} changed during the check · nothing was written · run the edit again", file_basename(path))
        }
        R::CheckTimedOut {
            binary,
            config,
            after,
        } => format!(
            "`{} check` ran past {after:?} and was stopped · {} unchanged · run `{} check --config {}` to see why it does not finish, then try again",
            file_basename(binary),
            file_basename(config),
            file_basename(binary),
            config.display(),
        ),
    }
}

/// A path's basename for a compact toast line, the path itself when it has no
/// file name.
fn file_basename(path: &std::path::Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

/// The compact `shunt check` refusal line shared by the admin edit and a field
/// save: outcome first, files by basename, the `↵ check output` pointer. `op`
/// names the refused operation (`edit` / `save`).
fn check_refused_line(
    binary: &std::path::Path,
    config: &std::path::Path,
    code: &Option<i32>,
    op: &str,
) -> String {
    let status = match code {
        Some(code) => format!("exit {code}"),
        None => "killed by a signal".to_string(),
    };
    format!(
        "`{} check` refused the {op} ({status}) · {} unchanged · {} check output on the shunt card",
        file_basename(binary),
        file_basename(config),
        prose::key("↵"),
    )
}

/// Add clauth's admin write key (or a whole `[server.admin]` table) behind the
/// engine's own `shunt check` gate.
fn run_admin_edit(
    record: &crate::gateway::GatewayRecord,
    step: crate::gateway::AdminNeed,
) -> ShuntActionOutcome {
    let result = match step {
        crate::gateway::AdminNeed::WriteKey => crate::gateway::add_admin_write_key(record),
        crate::gateway::AdminNeed::AdminTable => crate::gateway::add_admin_table(record),
        // Only offered when the config needs this exact step.
        crate::gateway::AdminNeed::Neither => Ok(crate::gateway::AdminEdit::AlreadyPresent),
    };
    match result {
        // Another writer added the key between the recompute and this worker:
        // nothing was added, so no success toast (the recompute re-reads it).
        Ok(crate::gateway::AdminEdit::AlreadyPresent) => ShuntActionOutcome::NoOp,
        Ok(_) => ShuntActionOutcome::AdminAdded,
        Err(e) => match e.downcast_ref::<crate::gateway::ConfigEditRefusal>() {
            Some(refusal) => {
                use crate::gateway::ConfigEditRefusal as R;
                let check_output = match refusal {
                    R::CheckFailed { stderr, .. } => Some(stderr.clone()),
                    R::Needs(_)
                    | R::ForeignClauthKey
                    | R::UnexpectedShape { .. }
                    | R::TokenPathUnusable { .. }
                    | R::Symlink { .. }
                    | R::ShuntMissing { .. }
                    | R::ChangedDuringEdit { .. }
                    | R::CheckTimedOut { .. } => None,
                };
                ShuntActionOutcome::AdminFailed {
                    error: admin_refusal_line(refusal),
                    check_output,
                    config: record.config().to_path_buf(),
                }
            }
            None => ShuntActionOutcome::AdminFailed {
                error: error_toast_detail(&e),
                check_output: None,
                config: record.config().to_path_buf(),
            },
        },
    }
}

/// Every slot string the detail renders goes through here. A control character
/// or a bidi formatting character is spelled as its visible `\u{…}` form so an
/// untrusted string off the feed or the record can neither inject into the
/// terminal nor reorder a line; every other character passes through unchanged.
pub(crate) fn escape_control(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_control() || crate::jobs_cli::reorders_display(c) {
                c.escape_default().to_string()
            } else {
                c.to_string()
            }
        })
        .collect()
}

/// A compact toast detail from an `anyhow::Error`: the root cause first (it
/// must stay on screen), then the rest of the chain outermost-first, joined by
/// ` · `. Paths stay in full there — they may clip, the cause may not. Logs
/// and the CLI keep the full `{e:#}` Display.
fn error_toast_detail(err: &anyhow::Error) -> String {
    let chain: Vec<String> = err.chain().map(ToString::to_string).collect();
    match chain.as_slice() {
        [] => String::new(),
        [only] => only.clone(),
        [.., root] => {
            let mut parts = vec![root.clone()];
            parts.extend(chain[..chain.len() - 1].iter().cloned());
            parts.join(" · ")
        }
    }
}

/// The clause an outdated plugin install's version line carries; the services
/// renderer tones that line by it, so the copy and its color share one spelling.
pub(crate) const OUTDATED_PLUGIN_MARK: &str = "this clauth is";

/// The `plugin` row: one detail holding the plugin install, the mcpServers
/// entry (+ the `r`-gated start probe), then the Claude Code version, clauth
/// on PATH and the data dir, a blank line between the three groups. The
/// install leads because installing is the fix a fresh box needs, and it also
/// registers the server. Health is the worst of the four; the fixable problems
/// walk in detail order, so list-level `f` lands on install first. `clauth_path` is
/// passed in (the recompute also gates the start probe on it) so the verdict
/// logic unit-tests without a live `PATH`.
fn plugin_check(
    clauth_path: Option<&std::path::Path>,
    cc_version: Option<Option<String>>,
    mcp_boot: Option<crate::plugin_probe::McpProbe>,
) -> Check {
    use crate::plugin_probe as probe;

    let mut detail: Vec<String> = Vec::new();
    let mut health = Health::Ok;
    let mut problems: Vec<Problem> = Vec::new();

    // "global" == active in every project: a CC `user`-scope plugin install. A
    // `local`/`project` install (or a `./.mcp.json`) binds clauth to one repo.
    let records = probe::installed_records();
    let installed = !records.is_empty();
    let plugin_global = records.iter().any(|r| r.scope.as_deref() == Some("user"));

    // plugin install record — installed-only verdict (CC exposes no clean
    // per-scope "enabled" boolean, so the row reports presence, scope and
    // version, not enabled/disabled).
    let marketplace = probe::marketplace_known();
    // The operative record is the `user`-scope install when one exists; a stale
    // project/local row that sorts first must not name the install beside a
    // verdict computed off the user row (`plugin_global` above).
    let record_for_check = records
        .iter()
        .find(|r| r.scope.as_deref() == Some("user"))
        .or_else(|| records.first());
    if let Some(record) = record_for_check {
        let scope = record.scope.as_deref();
        // Claude Code caches the plugin by its manifest version, so a user
        // install older than this binary keeps serving the old tree until it
        // is reinstalled, and the install fix updates it. A newer install is a
        // newer clauth's, which the install never downgrades.
        let this_version = crate::update::CURRENT_VERSION;
        let outdated = plugin_global
            && record
                .version
                .as_deref()
                .is_some_and(|installed| crate::update::is_newer(this_version, installed));
        detail.push(format!("installed: yes ({})", scope.unwrap_or("?")));
        if let Some(version) = &record.version {
            detail.push(if outdated {
                format!("version: {version} ({OUTDATED_PLUGIN_MARK} {this_version})")
            } else {
                format!("version: {version}")
            });
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
        if outdated {
            let line = detail.len();
            detail.push(fix_line(&ServiceFix::InstallPlugin));
            problems.push(Problem {
                line,
                fix: ServiceFix::InstallPlugin,
            });
            health = worst(health, Health::Warn);
        } else if plugin_global {
            health = worst(health, Health::Ok);
        } else {
            detail.push("installed for this project only, not global".to_string());
            detail.push("installs at user scope".to_string());
            let line = detail.len();
            detail.push(fix_line(&ServiceFix::InstallPlugin));
            problems.push(Problem {
                line,
                fix: ServiceFix::InstallPlugin,
            });
            health = worst(health, Health::Warn);
        }
    } else {
        let known = marketplace.is_some();
        detail.push(format!(
            "installed: no ({})",
            if known {
                "marketplace known"
            } else {
                "marketplace unknown"
            }
        ));
        if let Some(repo) = marketplace.as_ref().and_then(|m| m.repo.as_ref()) {
            detail.push(format!("marketplace: {repo}"));
        }
        detail.push("installs at user scope".to_string());
        let line = detail.len();
        detail.push(fix_line(&ServiceFix::InstallPlugin));
        problems.push(Problem {
            line,
            fix: ServiceFix::InstallPlugin,
        });
        health = worst(health, Health::Warn);
    }

    // mcpServers entry + start probe.
    detail.push(String::new());
    let wiring = probe::manual_mcp_wiring();
    let wired = installed || wiring != probe::McpWiring::None;
    let manual_global = wiring == probe::McpWiring::GlobalConfig;
    // A manual `~/.claude.json` entry whose command/args no longer match the
    // canonical launch line reads as registered but won't start the current
    // server. Only the operative manual entry matters — a `user`-scope plugin
    // install supersedes it, so drift under one is moot.
    let drifted = manual_global && !plugin_global && probe::global_entry_drifted() == Some(true);
    let globally_wired = plugin_global || (manual_global && !drifted);
    let project_only = wired && !globally_wired && !drifted;
    detail.push(format!(
        "mcp entry: {}",
        if wired {
            "registered"
        } else {
            "not registered"
        }
    ));
    match &mcp_boot {
        Some(probe::McpProbe::Ok) => detail.push("mcp server: ok".to_string()),
        Some(probe::McpProbe::Failed(reason)) => {
            detail.push(format!("mcp server: won't start ({reason})"));
        }
        None => {}
    }
    detail.push(
        if plugin_global {
            "mcp source: plugin install (user)"
        } else if manual_global {
            "mcp source: ~/.claude.json (manual)"
        } else if installed {
            "mcp source: plugin install (project)"
        } else if wiring == probe::McpWiring::ProjectFile {
            "mcp source: ./.mcp.json (manual)"
        } else {
            "mcp source: none"
        }
        .to_string(),
    );
    let needs_wire = !globally_wired || drifted;
    let boot_failed = matches!(mcp_boot, Some(probe::McpProbe::Failed(_)));
    health = worst(
        health,
        if boot_failed {
            Health::Danger
        } else if needs_wire {
            Health::Warn
        } else {
            Health::Ok
        },
    );
    if needs_wire {
        if drifted {
            detail.push("entry doesn't match the current launch line".to_string());
        } else if project_only {
            detail.push("registered for this project only, not global".to_string());
        }
        detail.push("writes the clauth entry into ~/.claude.json".to_string());
        let line = detail.len();
        detail.push(fix_line(&ServiceFix::WireMcpServers));
        problems.push(Problem {
            line,
            fix: ServiceFix::WireMcpServers,
        });
    }

    // Claude Code version, then clauth on PATH + the data dir.
    detail.push(String::new());
    match &cc_version {
        Some(Some(version)) => detail.push(format!("claude: {version}")),
        Some(None) => {
            detail.push("claude: not found".to_string());
            detail.push("claude --version failed or claude is not on PATH".to_string());
            detail.push("install claude code so the claude binary resolves".to_string());
            health = worst(health, Health::Warn);
        }
        // Unprobed is not missing: the probe is `r`-gated, so before the first
        // `r` the row names the key that fills the line in instead of claiming
        // a binary is absent.
        None => detail.push(format!("claude: press {} to probe", prose::key("r"))),
    }
    match &clauth_path {
        Some(path) => detail.push(format!("path: {}", path.display())),
        None => {
            detail.push("path: not on PATH".to_string());
            detail.push(
                "claude code spawns clauth mcp by name, so the server won't start".to_string(),
            );
            detail.push("install clauth so its bin directory is on PATH".to_string());
            health = worst(health, Health::Danger);
        }
    }
    detail.push(format!(
        "data: {}",
        crate::profile::clauth_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "\u{2014}".to_string())
    ));

    // The first fix line shown, in the order the detail walk presents them
    // (install, then wire).
    let fix = problems.first().map(|p| p.fix.clone());

    Check {
        label: "plugin",
        health,
        detail,
        fix,
        problems,
        shunt_focus: Vec::new(),
        shunt_action_start: None,
    }
}

/// The more severe of two health buckets (Ok < Warn < Danger), for folding the
/// plugin row's four sub-checks into one dot. `Idle` is neutral and never
/// outranks a verdict.
fn worst(a: Health, b: Health) -> Health {
    use Health::{Danger, Warn};
    if a == Danger || b == Danger {
        Danger
    } else if a == Warn || b == Warn {
        Warn
    } else {
        Health::Ok
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
    app.open_modal(Modal::Confirm(ConfirmState {
        message: format!("switch to '{name}'?"),
        detail: None,
        choice: true,
        scroll: ConfirmScroll::default(),
        on_confirm: ConfirmAction::Switch(name.to_string()),
    }));
}

fn activate_main_item(app: &mut App) {
    let Some(item) = app.current_main_item() else {
        return;
    };
    match item {
        MainItemKind::Profile(idx) => request_switch_to(app, idx),
        MainItemKind::Codex(_) => {}
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
    app.profile_cursor = new_idx;
    app.overview_cursor = new_idx;
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
    app.open_modal(Modal::Divergence(DivergenceForm {
        active: active.to_string(),
        sibling,
        cursor: 0,
        scroll: std::cell::Cell::new(0),
        viewport: std::cell::Cell::new(0),
        starts: std::cell::RefCell::new(Vec::new()),
        block_ends: std::cell::RefCell::new(Vec::new()),
        total: std::cell::Cell::new(0),
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
    let result = switch_profile(&app.config, name);
    clear_activity(&app.activity, name);
    match result {
        Ok(()) => {
            app.refresh_tokens();
            app.last_reload_fp = reload_fingerprint();
            app.refresh_unsaved_live_login();
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
    let result = switch_off(&app.config);
    match result {
        Ok(()) => {
            app.refresh_tokens();
            app.last_reload_fp = reload_fingerprint();
            app.refresh_unsaved_live_login();
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
    if snapshot_is_empty(&snapshot) {
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
        app.open_modal(Modal::Confirm(ConfirmState {
            message: format!("these credentials already belong to '{existing}'."),
            detail: Some("capture anyway?".to_string()),
            choice: false,
            scroll: ConfirmScroll::default(),
            on_confirm: ConfirmAction::CaptureConflict(Box::new(snapshot), from_divergence),
        }));
        return;
    }
    app.open_modal(Modal::CaptureName(CaptureNameForm {
        snapshot: Box::new(snapshot),
        input: InputState::new(""),
        from_divergence,
    }));
}

/// Land a login stash in the live `+ new` draft (`+ login`'s finished mint, or
/// `+ capture current login`'s snapshot) and park the cursor on `create
/// account`, the one step left. Returns `false` when the form was closed in
/// between, so the caller can warn about the dropped round-trip.
fn stash_new_form_login(app: &mut App, stash: DraftLogin) -> bool {
    let stashed = match app
        .config_draft
        .as_mut()
        .filter(|d| d.editing_name.is_none())
    {
        Some(draft) => {
            draft.captured_login = Some(stash);
            true
        }
        None => false,
    };
    if stashed
        && let Some(idx) = config_rows(app)
            .iter()
            .position(|r| *r == ConfigRow::Create)
    {
        app.config_action_cursor = idx;
    }
    stashed
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
/// toggles, preferred-days presets + day picker, remove.
pub(crate) const FALLBACK_ROWS: [FallbackRow; 9] = [
    FallbackRow::Threshold,
    FallbackRow::WeeklyAt,
    FallbackRow::CheckWeekly,
    FallbackRow::CheckScoped,
    FallbackRow::LastResort,
    FallbackRow::Preferred,
    FallbackRow::PreferredDays,
    FallbackRow::MaxSpend,
    FallbackRow::Remove,
];

/// Rows on the program-wide Config tab, in display order. Related knobs sit
/// together instead of interleaving halt above detection; [`GlobalConfigRow::band`]
/// names each run, and the renderer turns a band change into an eyebrow header.
pub(crate) const GLOBAL_CONFIG_ROWS: [GlobalConfigRow; 20] = [
    GlobalConfigRow::Theme,
    GlobalConfigRow::ResetShape,
    GlobalConfigRow::ClockNotation,
    GlobalConfigRow::HomeTab,
    GlobalConfigRow::DivergenceDefault,
    GlobalConfigRow::PreserveNonLoginKeys,
    GlobalConfigRow::RefreshInterval,
    GlobalConfigRow::RefreshSpentAccounts,
    GlobalConfigRow::ContextNudge,
    GlobalConfigRow::AutoStartQueue,
    GlobalConfigRow::PreemptiveRotation,
    GlobalConfigRow::AutoUpdate,
    GlobalConfigRow::WeeklyThreshold,
    GlobalConfigRow::BurnAware,
    GlobalConfigRow::WalkOrder,
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
            | GlobalConfigRow::ClockNotation
            | GlobalConfigRow::HomeTab => "appearance",
            GlobalConfigRow::DivergenceDefault
            | GlobalConfigRow::PreserveNonLoginKeys
            | GlobalConfigRow::RefreshInterval
            | GlobalConfigRow::RefreshSpentAccounts
            | GlobalConfigRow::ContextNudge
            | GlobalConfigRow::AutoStartQueue
            | GlobalConfigRow::PreemptiveRotation
            | GlobalConfigRow::AutoUpdate => "scheduler",
            GlobalConfigRow::WeeklyThreshold
            | GlobalConfigRow::BurnAware
            | GlobalConfigRow::WalkOrder
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
/// value back to the first; ⏎ opens the refresh-interval, context-nudge and
/// weekly-threshold custom-value editors and otherwise mirrors space. No row
/// here binds `+`/`-`
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
        KeyCode::PageUp => {
            let starts = app.global_config_row_starts.borrow();
            let block_ends = app.global_config_block_ends.borrow();
            let tips = app.global_config_tips.borrow();
            app.global_config_cursor = page_up_cursor_by_rows(
                app.global_config_cursor,
                &starts,
                &block_ends,
                &tips,
                app.global_config_viewport.get(),
                app.global_config_offset.get(),
                app.global_config_total.get(),
            );
        }
        KeyCode::PageDown => {
            let starts = app.global_config_row_starts.borrow();
            let block_ends = app.global_config_block_ends.borrow();
            app.global_config_cursor = page_down_cursor_by_rows(
                app.global_config_cursor,
                &starts,
                &block_ends,
                app.global_config_viewport.get(),
                app.global_config_offset.get(),
                app.global_config_total.get(),
            );
        }
        KeyCode::Char(' ') => {
            run_global_config_row(app, GLOBAL_CONFIG_ROWS[app.global_config_cursor]);
        }
        KeyCode::Enter => {
            let row = GLOBAL_CONFIG_ROWS[app.global_config_cursor];
            if row == GlobalConfigRow::RefreshInterval {
                begin_refresh_interval_edit(app);
            } else if row == GlobalConfigRow::ContextNudge {
                begin_context_nudge_edit(app);
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
        GlobalConfigRow::HomeTab => cycle_home_tab(app),
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
        GlobalConfigRow::ContextNudge => step_context_nudge(app),
        GlobalConfigRow::BurnAware => toggle_burn_aware_switching(app),
        GlobalConfigRow::WalkOrder => cycle_walk_order(app),
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
        GlobalConfigRow::AutoUpdate => toggle_auto_update(app),
        GlobalConfigRow::PreserveNonLoginKeys => toggle_preserve_non_login_keys(app),
        GlobalConfigRow::RefreshSpentAccounts => toggle_refresh_spent_accounts(app),
        // Inert while no account has `auto_start` on (rendered dimmed): a
        // queue with no possible member spaces nothing, so it stays a true
        // disabled row — the key is a no-op.
        GlobalConfigRow::AutoStartQueue => {
            if app.config().profiles.iter().any(|p| p.auto_start) {
                toggle_auto_start_queue(app);
            }
        }
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

/// An open edit on the Fallback member card, pinned to the member it opened on
/// by NAME. A reload can reorder or shrink the chain under an open editor, so a
/// write resolved through `chain_cursor` lands on whichever member took the
/// slot; every write goes to `member` instead, and [`repin_member_edit`] moves
/// `chain_cursor` after it, or closes the edit and returns focus to the chain
/// list once it left the chain or its account left the roster.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MemberEdit<T> {
    pub(crate) member: ProfileName,
    pub(crate) state: T,
}

/// What is open on the member card. One at a time: every typed field and the
/// picker own the keyboard, and the armed remove disarms on any row move.
#[derive(Debug, Clone)]
pub(crate) enum CardEdit {
    /// The `rotate at` field. `+`/`-` still step the value while closed.
    Threshold(InputState),
    /// The `weekly at` override field; an EMPTY commit clears the override.
    Weekly(InputState),
    /// The `max spend` ceiling field.
    MaxSpend(InputState),
    /// The `preferred days` chip picker. No draft: each space saves at once.
    Days(DayPicker),
    /// The first ⏎ on `remove`; the second removes this member.
    ArmedRemove,
}

impl CardEdit {
    /// The row this edit is open on.
    pub(crate) fn row(&self) -> FallbackRow {
        match self {
            Self::Threshold(_) => FallbackRow::Threshold,
            Self::Weekly(_) => FallbackRow::WeeklyAt,
            Self::MaxSpend(_) => FallbackRow::MaxSpend,
            Self::Days(_) => FallbackRow::PreferredDays,
            Self::ArmedRemove => FallbackRow::Remove,
        }
    }

    /// The typed buffer, when this edit is a typed field.
    pub(crate) fn input(&self) -> Option<&InputState> {
        match self {
            Self::Threshold(input) | Self::Weekly(input) | Self::MaxSpend(input) => Some(input),
            Self::Days(_) | Self::ArmedRemove => None,
        }
    }

    fn input_mut(&mut self) -> Option<&mut InputState> {
        match self {
            Self::Threshold(input) | Self::Weekly(input) | Self::MaxSpend(input) => Some(input),
            Self::Days(_) | Self::ArmedRemove => None,
        }
    }
}

/// The `preferred days` chip picker: the chip under the caret (a
/// `WEEKDAYS_ALL` index), and whether this descend already raised the
/// claims-nothing warning, which it raises once rather than once per toggle.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct DayPicker {
    pub(crate) cursor: usize,
    pub(crate) warned: bool,
}

/// Fallback footer hint derived from current focus + selection + edit state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FallbackHint {
    Empty,
    ChainMember,
    ChainAdd,
    DetailThreshold,
    DetailWeeklyAt,
    DetailCheckWeekly,
    DetailCheckScoped,
    DetailLastResort,
    DetailPreferred,
    DetailPreferredDays,
    DetailMaxSpend,
    DetailRemove,
    DetailRemoveArmed,
    DetailAdd,
}

/// Resolve the Fallback tab's footer hint while no editor owns the keyboard
/// ([`keyboard_owner`]); an open editor's hints are its own.
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
            let cursor = app.fallback_detail_cursor.min(FALLBACK_ROWS.len() - 1);
            match FALLBACK_ROWS[cursor] {
                FallbackRow::Threshold => FallbackHint::DetailThreshold,
                FallbackRow::WeeklyAt => FallbackHint::DetailWeeklyAt,
                FallbackRow::CheckWeekly => FallbackHint::DetailCheckWeekly,
                FallbackRow::CheckScoped => FallbackHint::DetailCheckScoped,
                FallbackRow::LastResort => FallbackHint::DetailLastResort,
                FallbackRow::Preferred => FallbackHint::DetailPreferred,
                FallbackRow::PreferredDays => FallbackHint::DetailPreferredDays,
                FallbackRow::MaxSpend => FallbackHint::DetailMaxSpend,
                FallbackRow::Remove
                    if app
                        .fallback_edit
                        .as_ref()
                        .is_some_and(|e| matches!(e.state, CardEdit::ArmedRemove)) =>
                {
                    FallbackHint::DetailRemoveArmed
                }
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
        KeyCode::PageUp => {
            app.chain_cursor = selector_page_up(app.chain_cursor, &app.chain_selector);
            sync_profile_from_chain(app);
        }
        KeyCode::PageDown => {
            app.chain_cursor = selector_page_down(app.chain_cursor, &app.chain_selector);
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

fn cycle_home_tab(app: &mut App) {
    let next: Tab = app.config().state.home_tab().into();
    let next = next.next();
    {
        let mut cfg = app.config();
        cfg.state.home_tab = Some(next.into());
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
        let next = !cfg.state.switch_off_when_spent;
        let _ = set_wrap_off(&mut cfg, next);
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

/// Cycle the walk-order mode (issue #86): `chain` → `soonest weekly reset` →
/// `most weekly headroom` → `chain`.
/// `cycle_reset_display`'s persistence shape exactly: mutate the shared
/// `AppConfig`, `save_app_state`, bump `last_reload_fp` — no separate
/// propagation to the scheduler, since every walk reads the mode off the same
/// shared `config` (`snapshot_chain` copies it into `ChainSnapshot` like
/// `burn_aware`).
fn cycle_walk_order(app: &mut App) {
    let next = match app.config().state.walk_order() {
        WalkOrder::Chain => WalkOrder::SoonestWeeklyReset,
        WalkOrder::SoonestWeeklyReset => WalkOrder::MostWeeklyHeadroom,
        WalkOrder::MostWeeklyHeadroom => WalkOrder::Chain,
    };
    {
        let mut cfg = app.config();
        cfg.state.walk_order = Some(next);
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

/// Flip the background self-update (`[update].auto_update`, default on). Same
/// persistence shape as `toggle_preemptive_rotation`; `check on launch` timing
/// — the binary updater reads the flag at next launch, the daemon's herdr leg
/// on its next reload, a new MCP server at startup. The row stays live under
/// `CLAUTH_NO_UPDATE=1`: the persisted value is what the row edits, and the
/// env var keeps overriding it until it goes.
fn toggle_auto_update(app: &mut App) {
    {
        let mut cfg = app.config();
        cfg.state.update.auto_update = !cfg.state.update.auto_update;
        let _ = save_app_state(&cfg.state);
    }
    app.last_reload_fp = reload_fingerprint();
}

/// Flip whether non-login keys stay with their account
/// (`preserve_non_login_keys`, default on). Same persistence shape as
/// `toggle_preemptive_rotation`; every reader loads the flag off disk at the
/// moment it relinks or rewrites a store (`claude::preserve_non_login_keys`),
/// so the next relink in any process acts on the flip.
fn toggle_preserve_non_login_keys(app: &mut App) {
    {
        let mut cfg = app.config();
        cfg.state.preserve_non_login_keys = !cfg.state.preserve_non_login_keys;
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

/// Interleave the auto-start kick across accounts, or let every window reopen the
/// instant it lapses (`usage::auto_start_queue`).
fn toggle_auto_start_queue(app: &mut App) {
    {
        let mut cfg = app.config();
        cfg.state.auto_start_queue = !cfg.state.auto_start_queue;
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

/// Advance the context-nudge threshold to the next-greater preset, wrapping
/// past the top back to off — space always cycles forward, never clamps. A
/// custom off-ladder value lands on the next preset above it, not one past it;
/// past the top preset it wraps to off, the cycle's first step.
fn step_context_nudge(app: &mut App) {
    const PRESETS: [u64; 4] = [300_000, 400_000, 600_000, 900_000];
    let current = app.config().state.context_nudge_threshold_tokens();
    let next = match current {
        None => Some(PRESETS[0]),
        Some(v) => PRESETS.iter().copied().find(|&p| p > v),
    };
    {
        let mut cfg = app.config();
        cfg.state.context_nudge_threshold_tokens = next;
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

/// Open the inline custom-value editor for the context-nudge threshold, seeded
/// with the current value in the row's own vocabulary (`600k`); from off it
/// seeds the first preset, the value one space-press would pick. ⏎ commits, ⎋
/// discards.
fn begin_context_nudge_edit(app: &mut App) {
    let current = app.config().state.context_nudge_threshold_tokens();
    let seed = match current {
        None => String::from("300k"),
        // Exact millions seed in k form (`2000k`), never `2M`: the parser's
        // grammar takes digits or a single trailing k, so an M-form seed
        // would open the editor in DANGER.
        Some(v) if v.is_multiple_of(1_000_000) => format!("{}k", v / 1000),
        Some(v) => format_threshold_tokens(v),
    };
    app.context_nudge_draft = Some(InputState::new(&seed));
}

/// Keystrokes while the context-nudge field is open: ⏎ saves, ⎋ discards.
fn handle_context_nudge_edit_key(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Esc => app.context_nudge_draft = None,
        KeyCode::Enter => commit_context_nudge_edit(app),
        _ => {
            if let Some(input) = app.context_nudge_draft.as_mut() {
                apply_input_edit(input, key);
            }
        }
    }
}

/// Parse and persist the typed threshold. Invalid input keeps the draft open
/// so the Config card's inline Invalid-input treatment (DANGER value +
/// `└ 50k-2M tokens` tooltip) stays on screen until corrected — no toast.
fn commit_context_nudge_edit(app: &mut App) {
    let Some(raw) = app.context_nudge_draft.as_ref().map(|i| i.trimmed()) else {
        return;
    };
    let Some(tokens) = parse_context_nudge_tokens(raw) else {
        return;
    };
    {
        let mut cfg = app.config();
        cfg.state.context_nudge_threshold_tokens = Some(tokens);
        let _ = save_app_state(&cfg.state);
    }
    app.last_reload_fp = reload_fingerprint();
    app.context_nudge_draft = None;
}

/// A typed context-nudge threshold is valid only as whole tokens — a plain
/// number or one with a single trailing `k` (case-insensitive, `600k` → 600
/// 000) — that lands in `MIN_CONTEXT_NUDGE_TOKENS..=MAX_CONTEXT_NUDGE_TOKENS`.
/// Shared by the commit path and the Config card's inline check.
pub(crate) fn parse_context_nudge_tokens(raw: &str) -> Option<u64> {
    let (digits, scale) = match raw.strip_suffix(['k', 'K']) {
        Some(prefix) => (prefix, 1_000u64),
        None => (raw, 1u64),
    };
    let tokens = digits.parse::<u64>().ok()?.checked_mul(scale)?;
    (MIN_CONTEXT_NUDGE_TOKENS..=MAX_CONTEXT_NUDGE_TOKENS)
        .contains(&tokens)
        .then_some(tokens)
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
            app.fallback_edit = None;
            app.fallback_detail_cursor = if app.fallback_detail_cursor == 0 {
                last
            } else {
                app.fallback_detail_cursor - 1
            };
        }
        KeyCode::Down => {
            app.fallback_edit = None;
            app.fallback_detail_cursor = if app.fallback_detail_cursor >= last {
                0
            } else {
                app.fallback_detail_cursor + 1
            };
        }
        KeyCode::PageUp => {
            app.fallback_edit = None;
            let starts = app.fallback_detail_row_starts.borrow();
            let block_ends = app.fallback_detail_block_ends.borrow();
            let tips = app.fallback_detail_tips.borrow();
            app.fallback_detail_cursor = page_up_cursor_by_rows(
                app.fallback_detail_cursor,
                &starts,
                &block_ends,
                &tips,
                app.fallback_detail_viewport.get(),
                app.fallback_detail_offset.get(),
                app.fallback_detail_total.get(),
            );
        }
        KeyCode::PageDown => {
            app.fallback_edit = None;
            let starts = app.fallback_detail_row_starts.borrow();
            let block_ends = app.fallback_detail_block_ends.borrow();
            app.fallback_detail_cursor = page_down_cursor_by_rows(
                app.fallback_detail_cursor,
                &starts,
                &block_ends,
                app.fallback_detail_viewport.get(),
                app.fallback_detail_offset.get(),
                app.fallback_detail_total.get(),
            );
        }
        // Continuous fields only — `max spend` (a dollar ceiling) has no
        // natural step unit and stays typed-only.
        KeyCode::Char('+' | '=') if row == FallbackRow::Threshold => adjust_threshold(app, 5.0),
        KeyCode::Char('-' | '_') if row == FallbackRow::Threshold => adjust_threshold(app, -5.0),
        KeyCode::Char('+' | '=') if row == FallbackRow::WeeklyAt => adjust_weekly(app, 5.0),
        KeyCode::Char('-' | '_') if row == FallbackRow::WeeklyAt => adjust_weekly(app, -5.0),
        // The day row is a cycle row at rest and a chip row once descended:
        // space walks the preset ladder (the `weekly limit` row's grammar), ⏎
        // opens the chip picker. Split here so `run_fallback_row` keeps its
        // single meaning for every other row.
        KeyCode::Char(' ') if row == FallbackRow::PreferredDays => step_preferred_days(app),
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
        KeyCode::PageUp => {
            let starts = app.fallback_detail_row_starts.borrow();
            let block_ends = app.fallback_detail_block_ends.borrow();
            let tips = app.fallback_detail_tips.borrow();
            app.fallback_detail_cursor = page_up_cursor_by_rows(
                app.fallback_detail_cursor,
                &starts,
                &block_ends,
                &tips,
                app.fallback_detail_viewport.get(),
                app.fallback_detail_offset.get(),
                app.fallback_detail_total.get(),
            );
        }
        KeyCode::PageDown => {
            let starts = app.fallback_detail_row_starts.borrow();
            let block_ends = app.fallback_detail_block_ends.borrow();
            app.fallback_detail_cursor = page_down_cursor_by_rows(
                app.fallback_detail_cursor,
                &starts,
                &block_ends,
                app.fallback_detail_viewport.get(),
                app.fallback_detail_offset.get(),
                app.fallback_detail_total.get(),
            );
        }
        KeyCode::Enter | KeyCode::Char(' ') => {
            let name = ProfileName::from(candidates[app.fallback_detail_cursor].clone());
            let would_mix = {
                let cfg = app.config();
                chain_would_mix(&cfg, &name)
            };
            if would_mix {
                app.open_modal(Modal::Confirm(ConfirmState {
                    message: "mixing api-key and oauth accounts can leave sessions stuck on the \
                              api account."
                        .into(),
                    detail: Some("api → oauth switches may not work until cc restarts.".into()),
                    choice: false,
                    scroll: ConfirmScroll::default(),
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
    app.fallback_edit = None;
    app.fallback_focus = FallbackFocus::Detail;
}

/// Return focus to the chain list, clearing any armed remove or live edit and
/// the card's remembered day-list stop.
fn leave_fallback_detail(app: &mut App) {
    app.fallback_focus = FallbackFocus::Chain;
    app.fallback_detail_cursor = 0;
    app.fallback_edit = None;
    app.fallback_day_stop = None;
}

/// ⇧↑↓: move the selected member up/down, cursor follows. No-op on `+ add`
/// or at boundary. Chain index == cursor for members (they precede `+ add`).
fn reorder_chain_member(app: &mut App, delta: i32) {
    let Some(pos) = selected_chain_member(app) else {
        return;
    };
    let target = pos as i32 + delta;
    let landed = {
        let mut cfg = app.config();
        if target < 0 || target as usize >= cfg.state.fallback_chain.len() {
            return;
        }
        let mut order = cfg.state.fallback_chain.clone();
        order.swap(pos, target as usize);
        let moved = order[target as usize].clone();
        set_chain_order(&mut cfg, &order)
            .ok()
            .and_then(|saved| saved.iter().position(|n| n == &moved))
    };
    // The cursor follows the member only when the reorder landed: a failed save
    // (a held state flock, a disk error) leaves the chain and the selection
    // where they were, so the next keypress still acts on the same member. The
    // saved order may have dropped an unresolvable entry, so the member's new
    // slot is looked up, never assumed.
    if let Some(slot) = landed {
        app.chain_cursor = slot;
    }
}

/// ⏎/space on a member detail row: a typed row opens its field and `preferred
/// days` its picker; remove arms, then removes on the second press. Each one
/// pins the member under the cursor.
fn run_fallback_row(app: &mut App, row: FallbackRow) {
    match row {
        FallbackRow::Threshold => {
            if let Some(current) = selected_threshold(app) {
                let field = InputState::new(&format!("{current:.0}"));
                open_card_edit(app, CardEdit::Threshold(field));
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
                open_card_edit(app, CardEdit::Weekly(InputState::new(&seed)));
            }
        }
        FallbackRow::CheckWeekly => toggle_member_flag(app, MemberFlag::CheckWeekly),
        FallbackRow::CheckScoped => toggle_member_flag(app, MemberFlag::CheckScoped),
        FallbackRow::LastResort => toggle_last_resort(app),
        FallbackRow::Preferred => toggle_preferred(app),
        // ⏎ only: space is the preset stepper and never reaches here for this row.
        FallbackRow::PreferredDays => open_day_picker(app),
        FallbackRow::MaxSpend => {
            // Inert while spend budget is off (rendered dimmed): opening the editor
            // would let a ceiling be typed that does nothing, so no-op.
            let armed = app.config().state.spend_budget_switching;
            if armed && let Some(current) = selected_max_spend(app) {
                let field = InputState::new(&format!("{current:.2}"));
                open_card_edit(app, CardEdit::MaxSpend(field));
            }
        }
        FallbackRow::Remove => match app.fallback_edit.take() {
            Some(MemberEdit {
                member,
                state: CardEdit::ArmedRemove,
            }) => remove_chain_member(app, &member),
            _ => open_card_edit(app, CardEdit::ArmedRemove),
        },
    }
}

/// Open `edit` on the card, pinned to the member under the cursor.
fn open_card_edit(app: &mut App, edit: CardEdit) {
    app.fallback_edit = selected_member_name(app).map(|member| MemberEdit {
        member,
        state: edit,
    });
}

/// Keystrokes while a typed card field is open: ⏎ saves, ⎋ discards.
fn handle_member_field_key(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Esc => app.fallback_edit = None,
        KeyCode::Enter => commit_member_field(app),
        _ => {
            if let Some(input) = app.fallback_edit.as_mut().and_then(|e| e.state.input_mut()) {
                apply_input_edit(input, key);
            }
        }
    }
}

/// Parse the open field and persist it on the member it is pinned to, whatever
/// sits under the cursor now. Invalid input keeps the field open so the card's
/// inline Invalid-input treatment (DANGER value + range tooltip) stays on
/// screen until corrected — no toast.
fn commit_member_field(app: &mut App) {
    let Some(MemberEdit { member, state }) = app.fallback_edit.clone() else {
        return;
    };
    match state {
        CardEdit::Threshold(input) => {
            let Some(value) = parse_threshold(input.trimmed()) else {
                return;
            };
            write_threshold(app, &member, value);
        }
        // An EMPTY commit clears the override back to the chain-wide default.
        CardEdit::Weekly(input) => {
            let Some(value) = parse_weekly_override(input.trimmed()) else {
                return;
            };
            write_weekly_override(app, &member, value);
        }
        CardEdit::MaxSpend(input) => {
            let Some(value) = parse_max_spend(input.trimmed()) else {
                return;
            };
            write_max_spend(app, &member, value);
        }
        CardEdit::Days(_) | CardEdit::ArmedRemove => return,
    }
    app.fallback_edit = None;
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

/// Write (or clear) `name`'s weekly-line override and persist.
fn write_weekly_override(app: &mut App, name: &ProfileName, value: Option<f64>) {
    let save_err = {
        let mut cfg = app.config();
        match cfg.find_mut(name) {
            Some(profile) => {
                profile.weekly_threshold = value;
                save_profile_config(profile).err()
            }
            None => None,
        }
    };
    if let Some(e) = save_err {
        app.toast(ToastKind::Danger, format!("save failed\n{e}"));
    }
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

/// Write `name`'s ceiling and persist.
fn write_max_spend(app: &mut App, name: &ProfileName, value: f64) {
    let save_err = {
        let mut cfg = app.config();
        match cfg.find_mut(name) {
            Some(profile) => {
                profile.max_auto_spend = Some(value);
                save_profile_config(profile).err()
            }
            None => None,
        }
    };
    if let Some(e) = save_err {
        app.toast(ToastKind::Danger, format!("save failed\n{e}"));
    }
}

/// The chain member under the cursor, by name, or `None` on `+ add`.
fn selected_member_name(app: &App) -> Option<ProfileName> {
    let pos = selected_chain_member(app)?;
    app.config().state.fallback_chain.get(pos).cloned()
}

/// `name`'s home days as the in-memory config holds them.
pub(crate) fn member_days(app: &App, name: &ProfileName) -> Option<Vec<Weekday>> {
    app.config().find(name).map(|p| p.preferred_days.clone())
}

/// After a config reload: point `chain_cursor` back at the member the open
/// [`MemberEdit`] is pinned to. When the reload took that member off the chain
/// or its account off the roster, close the edit and hand focus back to the
/// chain list, so no key meant for the edit lands on the member that moved into
/// its slot, and none writes to an account that is gone. A remembered day-list
/// stop goes once the card no longer shows its member.
fn repin_member_edit(app: &mut App) {
    if let Some(member) = app.fallback_edit.as_ref().map(|e| e.member.clone()) {
        let slot = {
            let cfg = app.config();
            cfg.find(&member)
                .and_then(|_| cfg.state.fallback_chain.iter().position(|n| *n == member))
        };
        match slot {
            Some(i) => app.chain_cursor = i,
            None => leave_fallback_detail(app),
        }
    }
    if app
        .fallback_day_stop
        .as_ref()
        .is_some_and(|stop| selected_member_name(app).as_ref() != Some(&stop.member))
    {
        app.fallback_day_stop = None;
    }
}

/// Space on the `preferred days` row at rest: step to the next stop and
/// persist. A set matching no rung (a custom list) is one more stop past the
/// last rung for as long as the card stays open: stepping off it remembers it
/// and lands on `never`, the ladder's first rung (a set has no order, so no
/// rung sits above it the way `step_weekly_threshold` finds the next preset
/// above a custom percent), and the last rung steps back onto it.
fn step_preferred_days(app: &mut App) {
    let Some(name) = selected_member_name(app) else {
        return;
    };
    // Stepped from the rung the card shows, since that is what the press
    // answered; a chain entry with no account behind it has no rung to step.
    let Some(shown) = member_days(app, &name) else {
        return;
    };
    let next = match preferred_days_preset(&shown) {
        Some(i) if i + 1 < PREFERRED_DAY_PRESETS.len() => preferred_days_at(i + 1),
        Some(_) => app
            .fallback_day_stop
            .as_ref()
            .filter(|stop| stop.member == name)
            .map(|stop| stop.state.clone())
            .unwrap_or_default(),
        None => {
            app.fallback_day_stop = Some(MemberEdit {
                member: name.clone(),
                state: shown,
            });
            preferred_days_at(0)
        }
    };
    if let Some(reason) = write_preferred_days(app, &name, move |_| next) {
        warn_list_claims_nothing(app, reason);
    }
}

/// ⏎ on the day row: descend into the chip picker, pinned to the member under
/// the cursor, caret on Monday. A chain entry with no account behind it has no
/// row to draw the picker in, so it opens nothing.
fn open_day_picker(app: &mut App) {
    if let Some(member) = selected_member_name(app)
        && member_days(app, &member).is_some()
    {
        app.fallback_edit = Some(MemberEdit {
            member,
            state: CardEdit::Days(DayPicker::default()),
        });
    }
}

/// Keystrokes while the chip picker is open (a multi-select chip row):
/// ←/→ walk the caret with wrap, space toggles the day under it and saves at
/// once, ⏎/esc/q leave the mode, ↑/↓ leave the mode and the row together.
/// `?` and `x` never reach here ([`KeyOwner::claims`]); every other key is
/// claimed and does nothing, so no other global binding fires mid-pick.
fn handle_day_picker_key(app: &mut App, key: KeyEvent) {
    let days = WEEKDAYS_ALL.len();
    match key.code {
        KeyCode::Left | KeyCode::Right => {
            if let Some(MemberEdit {
                state: CardEdit::Days(picker),
                ..
            }) = app.fallback_edit.as_mut()
            {
                let step = if key.code == KeyCode::Left {
                    days - 1
                } else {
                    1
                };
                picker.cursor = (picker.cursor + step) % days;
            }
        }
        KeyCode::Char(' ') => toggle_picked_day(app),
        KeyCode::Enter | KeyCode::Esc | KeyCode::Char('q') => app.fallback_edit = None,
        KeyCode::Up | KeyCode::Down => {
            app.fallback_edit = None;
            handle_fallback_detail_key(app, key);
        }
        _ => {}
    }
}

/// Space in the picker: flip the day under the caret on the pinned member's
/// list and save the result in `WEEKDAYS_ALL` order. A dead account's list
/// still saves; its claims-nothing warning is raised once per descend.
fn toggle_picked_day(app: &mut App) {
    let Some(MemberEdit {
        member,
        state: CardEdit::Days(state),
    }) = app.fallback_edit.clone()
    else {
        return;
    };
    let (Some(&day), Some(shown)) = (WEEKDAYS_ALL.get(state.cursor), member_days(app, &member))
    else {
        return;
    };
    // The caret's day lands where the shown list says the press sends it; the
    // other days come from the list read under the lock, so another writer's
    // days survive and the operator's own press is never inverted.
    let want = !shown.contains(&day);
    let toggle = move |days: &[Weekday]| -> Vec<Weekday> {
        WEEKDAYS_ALL
            .into_iter()
            .filter(|d| if *d == day { want } else { days.contains(d) })
            .collect()
    };
    if let Some(reason) = write_preferred_days(app, &member, toggle)
        && !state.warned
    {
        warn_list_claims_nothing(app, reason);
        if let Some(MemberEdit {
            state: CardEdit::Days(picker),
            ..
        }) = app.fallback_edit.as_mut()
        {
            picker.warned = true;
        }
    }
}

/// Rewrite `name`'s home days through `edit` and persist. A failed save toasts
/// the error and leaves the in-memory list on what the disk still holds.
/// `Some(reason)` when the saved list claims nothing, for the caller to say so.
///
/// Saved rather than refused because the state is reachable without this row — a
/// list goes inert when the account later leaves the chain or its login breaks —
/// and a row that quietly does nothing is worse than one that says why.
fn write_preferred_days(
    app: &mut App,
    name: &ProfileName,
    edit: impl FnOnce(&[Weekday]) -> Vec<Weekday>,
) -> Option<&'static str> {
    let result = {
        let mut cfg = app.config();
        crate::actions::edit_profile_preferred_days(&mut cfg, name, edit)
    };
    match result {
        Ok(saved) => (!saved.is_empty())
            .then(|| {
                let key_rejected = app.key_rejected_names();
                crate::fallback::day_claim_blocker(&app.config(), name, &key_rejected)
            })
            .flatten(),
        Err(e) => {
            app.toast(
                ToastKind::Danger,
                format!("preferred days update failed\n{e}"),
            );
            None
        }
    }
}

/// The saved-anyway warning for a day list the chain walk will never read.
fn warn_list_claims_nothing(app: &mut App, reason: &str) {
    app.toast(
        ToastKind::Warning,
        format!(
            "saved, but this list claims nothing: {reason}\nthe chain decides those days \
             without this account"
        ),
    );
}

/// Effective threshold for the selected member, or `None` on `+ add`.
fn selected_threshold(app: &App) -> Option<f64> {
    let pos = selected_chain_member(app)?;
    let cfg = app.config();
    let name = cfg.state.fallback_chain.get(pos)?;
    cfg.find(name).map(threshold_for)
}

/// Write `name`'s threshold and persist.
fn write_threshold(app: &mut App, name: &ProfileName, value: f64) {
    let save_err = {
        let mut cfg = app.config();
        set_member_threshold(&mut cfg, name, value).err()
    };
    let Some(e) = save_err else {
        return;
    };
    // A member whose roster row vanished inside the reload window is the
    // baseline's silent no-op: the account has already left, so there is
    // nothing to save and no wire code belongs on an operator toast.
    if let Some(ChainEditRefusal {
        code: ChainRefusal::ProfileNotFound,
        ..
    }) = e.downcast_ref::<ChainEditRefusal>()
    {
        return;
    }
    app.toast(ToastKind::Danger, format!("save failed\n{e}"));
}

/// Step the threshold by `delta`, clamped to 0..=100, and persist.
fn adjust_threshold(app: &mut App, delta: f64) {
    if let Some(name) = selected_member_name(app)
        && let Some(current) = selected_threshold(app)
    {
        write_threshold(
            app,
            &name,
            (current + delta).clamp(MIN_THRESHOLD, MAX_THRESHOLD),
        );
    }
}

/// Step the selected member's `weekly at` override by `delta`, clamped to
/// `MIN_WEEKLY_SWITCH_PCT..=MAX_WEEKLY_SWITCH_PCT`, and persist. Mirrors
/// `run_fallback_row`'s `check_weekly` guard: inert while the member's weekly
/// gate is off (rendered dimmed), same no-op contract as a dimmed row's ⏎.
/// An unset override bases the nudge on the chain-wide resolved default so
/// the value visibly moves off what the dimmed-default row already shows.
fn adjust_weekly(app: &mut App, delta: f64) {
    let (Some(name), Some((override_pct, check_weekly))) =
        (selected_member_name(app), selected_weekly_override(app))
    else {
        return;
    };
    if !check_weekly {
        return;
    }
    let base = override_pct.unwrap_or_else(|| app.config().state.weekly_switch_threshold_pct());
    let next = (base + delta).clamp(MIN_WEEKLY_SWITCH_PCT, MAX_WEEKLY_SWITCH_PCT);
    write_weekly_override(app, &name, Some(next));
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
        match save_profile_config(profile) {
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
                match save_profile_config(profile) {
                    Ok(()) => {
                        let mut moved_from = None;
                        if now_on {
                            for p in cfg
                                .profiles
                                .iter_mut()
                                .filter(|p| p.last_resort && p.name != name)
                            {
                                p.last_resort = false;
                                match save_profile_config(p) {
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
                match save_profile_config(profile) {
                    Ok(()) => {
                        let mut moved_from = None;
                        if now_on {
                            for p in cfg
                                .profiles
                                .iter_mut()
                                .filter(|p| p.preferred && p.name != name)
                            {
                                p.preferred = false;
                                match save_profile_config(p) {
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
        let _ = save_profile_config(profile);
    }
    cfg.state.fallback_chain.push(name.clone());
    let _ = save_app_state(&cfg.state);
}

/// Remove `name` from the chain, persist, and return focus to the list.
fn remove_chain_member(app: &mut App, name: &ProfileName) {
    {
        let mut cfg = app.config();
        if !cfg.state.fallback_chain.contains(name) {
            return;
        }
        cfg.state.fallback_chain.retain(|n| n != name);
        let _ = save_app_state(&cfg.state);
    }
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
            KeyCode::PageUp => {
                let max = app.help_max_scroll.get();
                app.help_scroll = page_up_scroll(
                    app.help_scroll as usize,
                    max as usize,
                    app.help_viewport.get() as usize,
                ) as u16;
            }
            KeyCode::PageDown => {
                let max = app.help_max_scroll.get();
                app.help_scroll = page_down_scroll(
                    app.help_scroll as usize,
                    max as usize,
                    app.help_viewport.get() as usize,
                ) as u16;
            }
            KeyCode::Esc | KeyCode::Enter | KeyCode::Char('?' | 'q') => {
                app.modals.pop();
            }
            _ => {}
        },
        Modal::Confirm(_) => handle_confirm_key(app, key),
        // The read-only `check output` modal scrolls, pages and closes like the
        // help modal, bar `?`: the body may hold one, so it is no close key here.
        Modal::CheckOutput(_) => match key.code {
            KeyCode::Up => {
                let max = app.check_output_max_scroll.get();
                app.check_output_scroll = app.check_output_scroll.min(max).saturating_sub(1);
            }
            KeyCode::Down => {
                let max = app.check_output_max_scroll.get();
                app.check_output_scroll = app.check_output_scroll.saturating_add(1).min(max);
            }
            KeyCode::PageUp => {
                let max = app.check_output_max_scroll.get();
                app.check_output_scroll = page_up_scroll(
                    app.check_output_scroll as usize,
                    max as usize,
                    app.check_output_viewport.get() as usize,
                ) as u16;
            }
            KeyCode::PageDown => {
                let max = app.check_output_max_scroll.get();
                app.check_output_scroll = page_down_scroll(
                    app.check_output_scroll as usize,
                    max as usize,
                    app.check_output_viewport.get() as usize,
                ) as u16;
            }
            KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q') => {
                app.modals.pop();
            }
            _ => {}
        },
        Modal::Divergence(_) => handle_divergence_key(app, key),
        Modal::CaptureName(_) => handle_capture_name_key(app, key),
        Modal::NamePrompt(_) => handle_name_prompt_key(app, key),
        Modal::PresetPicker(_) => handle_preset_picker_key(app, key),
        Modal::DivergenceTarget(_) => handle_divergence_target_key(app, key),
        Modal::TokenPicker(_) => handle_token_picker_key(app, key),
        Modal::ActionMenu(_) => handle_action_menu_key(app, key),
        Modal::EnvCollision(_) => handle_env_collision_key(app, key),
        Modal::Login => handle_login_modal_key(app, key),
    }
}

/// Keys on the login progress modal. While the inline code field is open it
/// owns every key ([`handle_paste_field_key`]). Otherwise `r` re-opens the URL
/// the session holds; `c` copies the hosted link and `p` opens the code field
/// on the session, both only while the paste door is open
/// ([`LoginSession::open_door`]), so the console login and a login already
/// exchanging a code leave them inert. esc/q/⏎ collapse to the footer
/// indicator; the login keeps running (the generation is untouched). A real
/// cancel is the top-level esc once collapsed; ⏎ on the login row re-expands.
fn handle_login_modal_key(app: &mut App, key: KeyEvent) {
    if app.login.as_ref().is_some_and(|s| s.paste_field.is_some()) {
        handle_paste_field_key(app, key);
        return;
    }
    match key.code {
        // The console login's URL exists once its worker announced it; before
        // that there is nothing to open, so `r` is a no-op.
        KeyCode::Char('r' | 'R') => {
            if let Some(url) = app.login.as_ref().and_then(|s| s.url.clone()) {
                match crate::platform::open_url(&url) {
                    Ok(()) => app.toast(ToastKind::Info, "opening your browser…"),
                    Err(_) => app.toast(ToastKind::Danger, "couldn't open the browser"),
                }
            }
        }
        // OSC 52 lands the link on the LOCAL terminal's clipboard, even over ssh.
        KeyCode::Char('c' | 'C') => {
            let link = app
                .login
                .as_ref()
                .and_then(LoginSession::open_door)
                .map(|door| door.links.hosted_url.clone());
            if let Some(link) = link {
                match (app.clipboard)(&link) {
                    Ok(()) => app.toast(ToastKind::Info, "link copied to clipboard"),
                    Err(e) => app.toast(
                        ToastKind::Danger,
                        format!("couldn't copy the link to clipboard\n{e}"),
                    ),
                }
            }
        }
        KeyCode::Char('p' | 'P') => {
            if let Some(session) = app.login.as_mut()
                && session.open_door().is_some()
            {
                session.paste_field = Some(InputState::new(""));
            }
        }
        KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q') => {
            app.modals.pop();
        }
        _ => {}
    }
}

/// Keys while the login modal's code field is open. It is a text field, so
/// every printable character is data (`c`, `q` and `p` included — a paste
/// arrives as one key event per character), capped at `MANUAL_CODE_MAX` at the
/// door so a runaway paste is never held first and refused after. esc clears
/// the field and restores the `p  paste code` row (the modal stays); ⏎ submits.
fn handle_paste_field_key(app: &mut App, key: KeyEvent) {
    let Some(session) = app.login.as_mut() else {
        return;
    };
    let Some(field) = session.paste_field.as_mut() else {
        return;
    };
    match key.code {
        KeyCode::Esc => session.paste_field = None,
        KeyCode::Enter => submit_paste_code(app),
        KeyCode::Char(_) if field.value.len() >= crate::oauth_login::MANUAL_CODE_MAX => {}
        _ => apply_input_edit(field, key),
    }
}

/// ⏎ on the code field: parse against the running login's own `state`, send a
/// good code down its paste door and close the field. The session's method is
/// NOT flipped here: the browser callback may already have won, so the
/// worker's own `ExchangingCode` report is what names the door. A bad paste is
/// cleared, not kept: a corrected paste appended to leftover bytes would pass
/// the shape check and burn the exchange. A field whose door closed meanwhile
/// (the drain closes it a tick later) has nothing to feed and closes too.
fn submit_paste_code(app: &mut App) {
    let Some(session) = app.login.as_mut() else {
        return;
    };
    let Some(raw) = session
        .paste_field
        .as_ref()
        .map(|field| field.trimmed().to_string())
    else {
        return;
    };
    if raw.is_empty() {
        return;
    }
    let Some(door) = session.open_door() else {
        session.paste_field = None;
        return;
    };
    match door.links.parse(&raw) {
        Ok(code) => {
            // A late paste is never read: `run` already picked its door, so
            // the send's result is discarded; the result drain handles the rest.
            let _ = door.tx.send(code);
            session.paste_field = None;
        }
        Err(e) => {
            session.paste_field = Some(InputState::new(""));
            app.toast(
                ToastKind::Danger,
                format!("{}\ncleared, paste the code again", e.message()),
            );
        }
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
            if matches!(app.current_main_item(), Some(MainItemKind::Profile(_))) {
                context = push_account_scope(app, &mut scoped);
            }
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
                // `+ new` only: its draft is what a preset stamps.
                context = app
                    .config_draft
                    .as_ref()
                    .and_then(|d| d.name.trimmed_some());
                scoped.push(ApplyPreset);
            }
        }
        // Fallback: every action the chain and its detail rows carry is bound to
        // a key of its own (⏎, ⇧↑↓, space, +/-).
        Tab::Fallback => {}
        Tab::Config => {}
        Tab::Status => {
            actions.push(RefreshStatus);
            if app.status.selected().is_some() {
                actions.push(OpenIncidentLink);
            }
        }
        // Services: `r` re-runs checks, `f` walks the fix lines, ⏎/esc navigate.
        Tab::Services => {}
    }
    // Every tab: the daemon verb that applies, none while one is in flight.
    if !app.daemon_control_busy {
        actions.push(match app.daemon_health {
            crate::daemon::DaemonHealth::Absent => StartDaemon,
            crate::daemon::DaemonHealth::Stale | crate::daemon::DaemonHealth::Fresh => StopDaemon,
        });
    }
    // Directly after it, the shunt verb that applies — never while a shunt
    // action is in flight, and the daemon-start row also yields none while a
    // daemon control runs (the daemon verb's own rule).
    if let Some(verb) = shunt_verb(app) {
        actions.push(verb.action());
    }

    ActionMenuState::new(scoped, actions, context)
}

/// What the action menu's shunt verb does, decided once by [`shunt_verb`] for
/// both the label the menu shows and the effect a pick runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ShuntVerb {
    /// `stop shunt`: the `stop shunt?` confirm, then the transient hold.
    Stop,
    /// `start shunt` with nothing adopted: the card's `f  adopt config` line.
    JumpAdopt,
    /// `start shunt` on a `disabled` gateway: the card's `enabled` row.
    JumpEnabled,
    /// `start shunt` with no daemon: the shared daemon-start confirm.
    StartDaemon,
    /// `start shunt` on a held gateway: the hold's release.
    Release,
}

impl ShuntVerb {
    pub(crate) fn action(self) -> ActionMenuAction {
        match self {
            Self::Stop => ActionMenuAction::StopShunt,
            Self::JumpAdopt | Self::JumpEnabled | Self::StartDaemon | Self::Release => {
                ActionMenuAction::StartShunt
            }
        }
    }
}

/// The one shunt verb that applies: an absent or `disabled` gateway jumps to
/// its card whatever the daemon; a running gateway under a fresh daemon stops;
/// a held one releases while a daemon runs; any other state with no daemon
/// starts the daemon. None while a shunt action is in flight, and the no-daemon
/// start none while a daemon control runs (the daemon verb's own rule). Reads
/// the cached [`App::gateway_state`], so the per-frame footer never touches the
/// disk.
pub(crate) fn shunt_verb(app: &App) -> Option<ShuntVerb> {
    use crate::daemon::DaemonHealth;
    use GatewayState as S;
    if app.services.shunt_action.running {
        return None;
    }
    let health = app.daemon_health;
    match app.gateway_state {
        S::Absent => Some(ShuntVerb::JumpAdopt),
        S::Disabled => Some(ShuntVerb::JumpEnabled),
        S::Starting | S::Healthy | S::Unhealthy | S::Restarting
            if health == DaemonHealth::Fresh =>
        {
            Some(ShuntVerb::Stop)
        }
        S::Held if health != DaemonHealth::Absent => Some(ShuntVerb::Release),
        S::Held
        | S::Starting
        | S::Healthy
        | S::Unhealthy
        | S::Restarting
        | S::NoConfig
        | S::YamlRefused
        | S::Misconfigured
        | S::BinaryMissing
        | S::Foreign
        | S::BelowFloor
        | S::Stopping
        | S::Unobserved => (health == DaemonHealth::Absent && !app.daemon_control_busy)
            .then_some(ShuntVerb::StartDaemon),
    }
}

/// Re-read the gateway slot for [`App::gateway_state`] under the daemon health
/// the app holds now.
pub(crate) fn sync_gateway_state(app: &mut App) {
    app.set_gateway_state(crate::daemon::gateway_slot(app.daemon_health).0.state);
}

/// The left-behind probe's landing: the no-daemon toast, its copy by the
/// probe's answer, raised only while the app is still in the state that
/// started it.
fn drain_left_behind_probe(app: &mut App) {
    let Some(left_running) = app.left_behind_probe.drain() else {
        return;
    };
    if app.gateway_unrun {
        let body = if left_running {
            GATEWAY_LEFT_RUNNING
        } else {
            GATEWAY_UNRUN
        };
        app.toast(ToastKind::Warning, body);
    }
}

/// The toast for a gateway that would run with no daemon to run it.
const GATEWAY_UNRUN: &str = concat!(
    "the shunt gateway will not run\nno daemon runs · ",
    prose::key_lit!("a"),
    " → start daemon"
);

/// The no-daemon toast's copy while a gateway the last daemon left still serves.
const GATEWAY_LEFT_RUNNING: &str = concat!(
    "a shunt gateway the last daemon left still runs\nno daemon supervises it · ",
    prose::key_lit!("a"),
    " → start daemon"
);

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
        KeyCode::PageUp => {
            let Some(Modal::ActionMenu(state)) = app.modals.last_mut() else {
                return;
            };
            let len = state.items.len();
            if len > 0 {
                let next = {
                    let starts = state.starts.borrow();
                    let block_ends = state.block_ends.borrow();
                    page_up_cursor_by_rows(
                        state.cursor,
                        &starts,
                        &block_ends,
                        &[],
                        state.viewport.get(),
                        state.scroll.get(),
                        state.total.get(),
                    )
                };
                state.cursor = next;
            }
        }
        KeyCode::PageDown => {
            let Some(Modal::ActionMenu(state)) = app.modals.last_mut() else {
                return;
            };
            let len = state.items.len();
            if len > 0 {
                let next = {
                    let starts = state.starts.borrow();
                    let block_ends = state.block_ends.borrow();
                    page_down_cursor_by_rows(
                        state.cursor,
                        &starts,
                        &block_ends,
                        state.viewport.get(),
                        state.scroll.get(),
                        state.total.get(),
                    )
                };
                state.cursor = next;
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
                app.open_modal(Modal::Confirm(ConfirmState {
                    message: format!("'{name}' {ROTATE_LIVE_SESSION_MSG}"),
                    detail: Some(ROTATE_LIVE_SESSION_DETAIL.to_string()),
                    choice: true,
                    scroll: ConfirmScroll::default(),
                    on_confirm: ConfirmAction::Acknowledge,
                }));
            }
            Some((name, true, _)) => {
                app.open_modal(Modal::Confirm(ConfirmState {
                    message: format!("rotate access token for '{name}'?"),
                    detail: None,
                    choice: false,
                    scroll: ConfirmScroll::default(),
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
        ActionMenuAction::StartDaemon => open_daemon_start_confirm(app),
        ActionMenuAction::StopDaemon => stop_daemon(app),
        ActionMenuAction::StartShunt | ActionMenuAction::StopShunt => {
            dispatch_shunt_verb(app, action);
        }
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

/// The toast for a shunt pick whose verb no longer applies.
const SHUNT_CHANGED: &str = "the shunt gateway changed; open the menu again";

/// The shunt verb a pick of `picked` runs now: [`shunt_verb`] over a slot
/// re-read at pick time, or the [`SHUNT_CHANGED`] toast and `None` when the
/// state moved under the open menu to a verb with another label, or to none.
fn shunt_verb_at_pick(app: &mut App, picked: ActionMenuAction) -> Option<ShuntVerb> {
    sync_gateway_state(app);
    let verb = shunt_verb(app).filter(|verb| verb.action() == picked);
    if verb.is_none() {
        app.toast(ToastKind::Info, SHUNT_CHANGED);
    }
    verb
}

/// A picked `start shunt` / `stop shunt`, acting on the verb that applies at
/// pick time.
fn dispatch_shunt_verb(app: &mut App, picked: ActionMenuAction) {
    let Some(verb) = shunt_verb_at_pick(app, picked) else {
        return;
    };
    match verb {
        ShuntVerb::Stop => open_shunt_stop_confirm(app),
        ShuntVerb::JumpAdopt => jump_to_shunt_card(app, ShuntFocusLine::Adopt),
        ShuntVerb::JumpEnabled => jump_to_shunt_card(app, ShuntFocusLine::Enabled),
        ShuntVerb::StartDaemon => open_daemon_start_confirm(app),
        ShuntVerb::Release => app.services.shunt_action.start(ShuntActionJob::RemoveHold),
    }
}

/// `stop shunt?`, its detail leading with how many live sessions run on the
/// gateway when that is at least one and clauth could count them.
fn open_shunt_stop_confirm(app: &mut App) {
    const DRAIN: &str =
        "in-flight requests drain, then the gateway stays off until the daemon restarts.";
    let detail = format!("{}{DRAIN}", live_sessions_clause(app));
    app.disarm_quit();
    app.open_modal(Modal::Confirm(ConfirmState {
        message: "stop shunt?".to_string(),
        detail: Some(detail),
        choice: false,
        scroll: ConfirmScroll::default(),
        on_confirm: ConfirmAction::StopShunt,
    }));
}

/// Live sessions whose current account's `base_url` names the gateway's
/// address, off the tally the header already polls. The port is the running
/// gateway's, off the daemon's slot (a bind edited since its spawn moves
/// nothing until a restart); the host is the one
/// [`crate::gateway::displayed_gateway_bind`] resolves. `None` when the tally,
/// the gateway record or its bind could not be read.
fn gateway_session_count(app: &App) -> Option<usize> {
    app.live_sessions.total()?;
    let record = crate::gateway::GatewayRecord::load().ok().flatten()?;
    let mut bind = crate::gateway::displayed_gateway_bind(&record).ok()?;
    if let Some(port) = crate::daemon::gateway_slot(app.daemon_health).0.port {
        bind.configured.set_port(port);
        bind.probe.set_port(port);
    }
    Some(
        app.config()
            .profiles
            .iter()
            .filter(|profile| {
                profile
                    .base_url
                    .as_deref()
                    .is_some_and(|url| crate::gateway::url_names_bind(url, &bind))
            })
            .map(|profile| app.live_sessions.member(&profile.name).sessions)
            .sum(),
    )
}

/// Switch to the Services tab, select the `shunt` row, descend, and focus the
/// named actionable line when the card offers it; a kind the card does not
/// offer leaves the focus to the descend rule (the first line in view, else
/// none).
fn jump_to_shunt_card(app: &mut App, focus: ShuntFocusLine) {
    switch_tab(app, Tab::Services);
    if let Some(idx) = app.services.checks.iter().position(|c| c.label == "shunt") {
        app.services.cursor = idx;
    }
    app.services.focus = ServicesFocus::Detail;
    app.services.detail_scroll = 0;
    app.services.set_shunt_focus(
        app.services
            .selected_check()
            .is_some_and(|c| c.shunt_focus.iter().any(|s| s.kind() == focus))
            .then_some(focus),
    );
}

/// Where the daemon's start and stop toasts send the user when either fails.
const DAEMON_LOG_HINT: &str = "see ~/.clauth/daemon.log";

/// The shared daemon-start confirm, opened by the action menu's `start daemon`
/// and by a no-daemon `start shunt`; its two detail lines are drawn off
/// [`ConfirmAction::StartDaemon`]. `cancel`/`esc` spawn nothing; `start` runs
/// [`start_daemon`].
fn open_daemon_start_confirm(app: &mut App) {
    app.disarm_quit();
    app.open_modal(Modal::Confirm(ConfirmState {
        message: "start daemon?".to_string(),
        detail: None,
        choice: false,
        scroll: ConfirmScroll::default(),
        on_confirm: ConfirmAction::StartDaemon,
    }));
}

/// The daemon-start confirm's `start`: spawn `<this binary> daemon` detached,
/// then report, off the UI thread, whether a daemon came up. The worker stays
/// to reap the child, so a daemon this TUI started and stops never lingers as
/// a zombie while the TUI runs. The singleton is re-read here, not at the
/// confirm's opening: a daemon started elsewhere while it sat open would make
/// the spawn exit at start.
fn start_daemon(app: &mut App) {
    match crate::daemon::singleton_held() {
        Ok(false) => {}
        Ok(true) => {
            app.toast(ToastKind::Info, "daemon already running");
            reprobe_daemon(app);
            return;
        }
        Err(e) => {
            app.toast(
                ToastKind::Danger,
                format!(
                    "daemon start failed\n{}",
                    escape_control(&error_toast_detail(&e))
                ),
            );
            return;
        }
    }
    let exe = match daemon_exe() {
        Ok(exe) => exe,
        Err(e) => {
            app.toast(
                ToastKind::Danger,
                format!("daemon start failed\n{}", escape_control(&e.to_string())),
            );
            return;
        }
    };
    let tx = app.daemon_control_tx.clone();
    app.daemon_control_busy = true;
    app.toast(ToastKind::Info, "starting daemon");
    spawn_worker(move || match crate::daemon::spawn_detached(&exe) {
        Ok(mut child) => {
            let outcome = crate::daemon::await_start(
                &mut child,
                crate::daemon::START_WAIT,
                DAEMON_CONTROL_POLL,
            );
            let _ = tx.send(DaemonControlResult::Start(Ok(outcome)));
            let _ = child.wait();
        }
        Err(e) => {
            let _ = tx.send(DaemonControlResult::Start(Err(error_toast_detail(&e))));
        }
    });
}

/// The binary `start daemon` runs: this one, as installed. A test build's
/// binary is the test harness, which would take `daemon` as a filter and run
/// the suite's daemon tests as its "daemon", so a test build refuses.
fn daemon_exe() -> std::io::Result<std::path::PathBuf> {
    if cfg!(test) {
        return Err(std::io::Error::other("a test build starts no daemon"));
    }
    std::env::current_exe().map(|exe| crate::platform::installed_exe_path(&exe))
}

/// The action menu's `stop daemon`: `--replace`'s termination, which can wait
/// out two bounded passes, so it runs off the UI thread.
fn stop_daemon(app: &mut App) {
    let tx = app.daemon_control_tx.clone();
    app.daemon_control_busy = true;
    app.toast(ToastKind::Info, "stopping daemon");
    spawn_worker(move || {
        let outcome = crate::daemon::stop_running().map_err(|e| error_toast_detail(&e));
        let _ = tx.send(DaemonControlResult::Stop(outcome));
    });
}

/// How often the start worker re-checks its child.
const DAEMON_CONTROL_POLL: Duration = Duration::from_millis(50);

/// Toast each daemon start/stop outcome, re-arm the menu's verb, and
/// [`reprobe_daemon`].
fn drain_daemon_control(app: &mut App) {
    use crate::daemon::{DaemonStop, StartOutcome};
    while let Ok(result) = app.daemon_control_rx.try_recv() {
        app.daemon_control_busy = false;
        match result {
            DaemonControlResult::Start(Ok(StartOutcome::Holding)) => {
                app.toast(ToastKind::Success, "daemon started");
            }
            DaemonControlResult::Start(Ok(StartOutcome::Exited)) => app.toast(
                ToastKind::Danger,
                format!("daemon exited at start\n{DAEMON_LOG_HINT}"),
            ),
            DaemonControlResult::Start(Ok(StartOutcome::NotYet)) => app.toast(
                ToastKind::Warning,
                format!(
                    "daemon not up after {}s\n{DAEMON_LOG_HINT}",
                    crate::daemon::START_WAIT.as_secs()
                ),
            ),
            DaemonControlResult::Start(Err(e)) => {
                app.toast(
                    ToastKind::Danger,
                    format!("daemon start failed\n{}", escape_control(&e)),
                );
            }
            DaemonControlResult::Stop(Ok(DaemonStop::Stopped)) => {
                app.toast(ToastKind::Success, "daemon stopped");
            }
            DaemonControlResult::Stop(Ok(DaemonStop::NotRunning)) => {
                app.toast(ToastKind::Info, "no daemon was running");
            }
            DaemonControlResult::Stop(Ok(DaemonStop::Replaced)) => {
                app.toast(
                    ToastKind::Warning,
                    "daemon stopped\nanother daemon took over",
                );
            }
            DaemonControlResult::Stop(Err(e)) => {
                app.toast(
                    ToastKind::Danger,
                    format!("daemon stop failed\n{}", escape_control(&e)),
                );
            }
        }
        reprobe_daemon(app);
    }
}

/// Re-probe the header chip and the menu's verb at once rather than at the
/// next throttled probe.
fn reprobe_daemon(app: &mut App) {
    app.daemon_health = crate::daemon::daemon_health();
    sync_gateway_state(app);
    app.last_daemon_probe = Instant::now();
}

/// Swap the Tokens model filter and re-clamp the Models cursor — the filtered
/// list can be shorter than where the cursor sat.
fn set_token_filter(app: &mut App, filter: TokenFilter) {
    app.token_filter = filter;
    app.model_detail_scroll = 0;
    clamp_token_model_cursor(app);
}

/// Swap the Tokens period lens and re-clamp the Models cursor — the scoped
/// list can be shorter than where the cursor sat.
fn set_token_period(app: &mut App, period: TokenPeriod) {
    app.token_period = period;
    app.model_detail_scroll = 0;
    clamp_token_model_cursor(app);
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
            KeyCode::PageUp => {
                app.profile_cursor = selector_page_up(app.profile_cursor, &app.setup_selector);
                app.reload_selected_note();
            }
            KeyCode::PageDown => {
                app.profile_cursor = selector_page_down(app.profile_cursor, &app.setup_selector);
                app.reload_selected_note();
            }
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
                KeyCode::PageUp => {
                    disarm_armed_action(app);
                    let starts = app.setup_detail_row_starts.borrow();
                    let block_ends = app.setup_detail_block_ends.borrow();
                    let tips = app.setup_detail_tips.borrow();
                    app.config_action_cursor = page_up_cursor_by_rows(
                        app.config_action_cursor,
                        &starts,
                        &block_ends,
                        &tips,
                        app.setup_detail_viewport.get(),
                        app.setup_detail_offset.get(),
                        app.setup_detail_total.get(),
                    );
                }
                KeyCode::PageDown => {
                    disarm_armed_action(app);
                    let starts = app.setup_detail_row_starts.borrow();
                    let block_ends = app.setup_detail_block_ends.borrow();
                    app.config_action_cursor = page_down_cursor_by_rows(
                        app.config_action_cursor,
                        &starts,
                        &block_ends,
                        app.setup_detail_viewport.get(),
                        app.setup_detail_offset.get(),
                        app.setup_detail_total.get(),
                    );
                }
                KeyCode::Enter => run_config_row(app, rows[app.config_action_cursor]),
                KeyCode::Char('+' | '=')
                    if rows[app.config_action_cursor] == ConfigRow::BellThreshold =>
                {
                    step_bell_threshold(app, 5.0);
                }
                KeyCode::Char('-' | '_')
                    if rows[app.config_action_cursor] == ConfigRow::BellThreshold =>
                {
                    step_bell_threshold(app, -5.0);
                }
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
        // The live-login capture row sits right below `+ login` and survives
        // into api mode (`+ login` doesn't): a live setup can be an endpoint
        // too. Hidden while the live login is saved or absent — nothing to
        // capture.
        if app.unsaved_live_login {
            rows.push(ConfigRow::CaptureLogin);
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
    rows.push(ConfigRow::BellThreshold);
    rows.push(ConfigRow::BaseUrl);
    if is_api {
        rows.push(ConfigRow::ApiKey);
    }
    rows.push(ConfigRow::Model);

    // Alias overrides collapse: render the ones set when the draft opened, tuck
    // the rest behind a single `+ model override` reveal until ⏎ expands them.
    let expanded = draft.is_some_and(|d| d.overrides_expanded);
    let override_set = |row: ConfigRow| match draft {
        Some(d) => d.overrides_shown.contains(&row),
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
    for row in OVERRIDE_ROWS {
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

pub(crate) fn build_draft_new() -> ConfigDraft {
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
        bell_threshold: InputState::new(""),
        env_value: InputState::new(""),
        env_new_key: InputState::new(""),
        env_just_added: None,
        active: None,
        armed_action: None,
        relogin_chain: false,
        overrides_expanded: false,
        overrides_shown: Vec::new(),
        captured_login: None,
    }
}

fn build_draft_existing(app: &App, name: &ProfileName) -> ConfigDraft {
    let cfg = app.config();
    let profile = cfg.find(name);
    let m = profile.map(|p| p.models.clone()).unwrap_or_default();
    let mut draft = ConfigDraft {
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
        bell_threshold: InputState::new(&row_committed_value(
            profile,
            name,
            ConfigRow::BellThreshold,
        )),
        env_value: InputState::new(""),
        env_new_key: InputState::new(""),
        env_just_added: None,
        active: None,
        armed_action: None,
        relogin_chain: false,
        overrides_expanded: false,
        overrides_shown: Vec::new(),
        captured_login: None,
    };
    draft.overrides_shown = OVERRIDE_ROWS
        .into_iter()
        .filter(|&row| draft.field(row).is_some_and(|i| !i.value.trim().is_empty()))
        .collect();
    draft
}

/// Back out of the Setup detail pane, dropping the draft. A `+ new` draft
/// holding a minted login loses it with the form — say so instead of
/// discarding a real browser round-trip silently. A stashed live login needs
/// no warning: re-pressing the row re-reads the same file.
fn leave_config_detail(app: &mut App) {
    let mint_dropped = app.config_draft.as_ref().is_some_and(|d| {
        d.editing_name.is_none() && matches!(d.captured_login, Some(DraftLogin::Mint(_)))
    });
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
        // The alert buffer reseeds from the profile as it opens: a reload may
        // have moved the value since the draft was built.
        if row == ConfigRow::BellThreshold {
            let saved = app
                .config_draft
                .as_ref()
                .and_then(|d| d.editing_name.clone())
                .map(ProfileName::from)
                .map(|name| {
                    let cfg = app.config();
                    row_committed_value(cfg.find(&name), &name, row)
                });
            if let (Some(saved), Some(d)) = (saved, app.config_draft.as_mut()) {
                d.bell_threshold = InputState::new(&saved);
            }
        }
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
            if let Some((name, is_new)) = oauth_login_target(app, editing) {
                begin_oauth_login(app, name, is_new);
            }
        }
        ConfigRow::CaptureLogin => {
            // The row only renders on the `+ new` form, where a draft is always
            // open; the empty and owned states are what HIDE it, so hitting one
            // here means the live file moved since the flag was computed.
            let Some(snapshot) = capture_live_or_toast(app) else {
                // The file emptied since the flag was computed; drop the row.
                app.refresh_unsaved_live_login();
                return;
            };
            let owner = {
                let cfg = app.config();
                find_matching_oauth_profile(&cfg, snapshot.credentials.as_ref())
            };
            if let Some(owner) = owner {
                // Refuse, not confirm: a NEW account over a login another
                // profile owns is the duplicate `overwrite_captured_profile`
                // exists to prevent, and this row's whole purpose is the
                // unowned case.
                app.toast(
                    ToastKind::Danger,
                    format!(
                        "these credentials already belong to '{owner}'\nswitch to it with:  {}",
                        prose::cmd(&format!("clauth {owner}"))
                    ),
                );
                app.refresh_unsaved_live_login();
                return;
            }
            let mint_stashed = app.config_draft.as_ref().is_some_and(|d| {
                d.editing_name.is_none() && matches!(d.captured_login, Some(DraftLogin::Mint(_)))
            });
            if mint_stashed {
                // The browser round-trip a mint cost can't be redone for free;
                // gate replacing it, mirroring the re-login gate above.
                app.open_modal(Modal::Confirm(ConfirmState {
                    message: "replace the logged-in mint?".to_string(),
                    detail: Some(
                        "the browser login you already captured will be dropped".to_string(),
                    ),
                    choice: false,
                    scroll: ConfirmScroll::default(),
                    on_confirm: ConfirmAction::CaptureOverMintStash(Box::new(snapshot)),
                }));
                return;
            }
            if stash_new_form_login(app, DraftLogin::LiveLogin(Box::new(snapshot))) {
                app.toast(
                    ToastKind::Success,
                    "current login captured\ncreate account saves it",
                );
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
                app.open_modal(Modal::Confirm(ConfirmState {
                    message: format!("log out of '{name}'?"),
                    detail: Some(detail.to_string()),
                    choice: false,
                    scroll: ConfirmScroll::default(),
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
    let guard = match crate::runtime::RotationGuard::try_acquire(name) {
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
    // The relink below puts the store in front of Claude Code: a staged pair
    // lands first.
    if let Err(refused) = crate::oauth::land_stage_for_install(name, &guard) {
        app.toast(
            ToastKind::Danger,
            format!("clear failed\n{}", refused.text()),
        );
        return;
    }
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
        // the in-memory snapshot — another process's config edit may have
        // landed since the last reload, and `save_profile_config` writes the
        // whole config back. The renderer's in-memory flag flips only AFTER that
        // write lands: flipped first, a failed save leaves the config lying
        // (`reload_fingerprint` never corrects it — config mtime did not move)
        // until an unrelated save makes the lie durable, and the end state is
        // a live rolling sidecar nothing re-stamps.
        let mut disarmed = on_disk.clone();
        disarmed.rolling_token = false;
        if let Err(e) = save_profile_config(&disarmed) {
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
    if let Some(pos) = config_row_index(app, ConfigRow::BaseUrl) {
        app.config_action_cursor = pos;
    }
}

/// Kick an OAuth login: mint it on this thread (no network), open the browser
/// on its loopback link, and hand the wait to a worker that takes the first
/// door — the loopback callback, or a code pasted through the login modal.
/// `is_new` → the mint lands in the `+ new` draft when it arrives; else an
/// existing profile is overwritten (divergence-gated in `apply_login`). A
/// second ⏎ while one is in flight re-expands the progress modal instead of
/// starting another login.
fn start_login(app: &mut App, name: String, is_new: bool) {
    if login_in_flight(app, &name, is_new) {
        return;
    }
    let pending = match crate::oauth_login::begin_login() {
        Ok(pending) => pending,
        Err(e) => {
            app.toast(
                ToastKind::Danger,
                format!("login failed\n{}", e.user_message()),
            );
            return;
        }
    };
    let links = pending.links().clone();
    let (paste_tx, paste_rx) = std::sync::mpsc::channel();
    app.login_generation += 1;
    let generation = app.login_generation;
    app.login = Some(LoginSession {
        name,
        is_new,
        generation,
        url: Some(links.browser_url.clone()),
        stage: LoginStage::WaitingBrowser,
        method: LoginMethod::Browser,
        paste: Some(PasteDoor {
            links,
            tx: paste_tx,
        }),
        paste_field: None,
    });
    // Best effort: the modal's `r` retries it, and the hosted link is the
    // way in where no browser can open.
    let _ = crate::platform::open_url(&pending.links().browser_url);
    let event_tx = app.login_event_tx.clone();
    let result_tx = app.login_result_tx.clone();
    spawn_worker(move || {
        let res = pending.run(paste_rx, |progress| {
            let _ = event_tx.send((generation, login_event(progress)));
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
/// `+ new` form, which can only mint; so does an unknown name, which has no
/// key to re-enter). The console verdict is [`Profile::console_login_target`],
/// shared with the row's hint and label so the copy cannot describe a
/// different flow than the one ⏎ runs.
fn login_row_flow(app: &App, editing: Option<&str>) -> LoginRowFlow {
    let Some(name) = editing else {
        return LoginRowFlow::OauthMint;
    };
    let cfg = app.config();
    let Some(p) = cfg.find(&ProfileName::from(name)) else {
        return LoginRowFlow::OauthMint;
    };
    if let Some((site, region)) = p.console_login_target() {
        return LoginRowFlow::Console { site, region };
    }
    if p.login_is_oauth() {
        LoginRowFlow::OauthMint
    } else {
        LoginRowFlow::ApiKey
    }
}

/// The account an OAuth-mint row acts on: an existing draft re-logs in place;
/// the `+ new` form validates its typed name now and creates on mint. `None`
/// (with a toast) when the typed name is unusable.
fn oauth_login_target(app: &mut App, editing: Option<String>) -> Option<(String, bool)> {
    match editing {
        Some(name) => Some((name, false)),
        None => {
            let typed = app
                .config_draft
                .as_ref()
                .map(|d| d.name.trimmed().to_string())
                .unwrap_or_default();
            let validation = validate_profile_name(&typed, Harness::Claude, None);
            match validation {
                Ok(()) => Some((typed, true)),
                Err(e) => {
                    app.toast(ToastKind::Danger, format!("{e}"));
                    None
                }
            }
        }
    }
}

/// The OAuth-mint head of the login row: nothing starts while a login is in
/// flight, and a captured stash is confirmed before it is dropped.
fn begin_oauth_login(app: &mut App, name: String, is_new: bool) {
    if login_in_flight(app, &name, is_new) {
        return;
    }
    // A stash (the `✓ logged in` / `✓ captured current login` done-states)
    // makes ⏎ a stash-replacing re-login; gate it so it can't drop the capture
    // silently. Only the `+ new` draft ever holds a stash.
    let has_stash = app
        .config_draft
        .as_ref()
        .is_some_and(|d| d.captured_login.is_some());
    if has_stash {
        app.open_modal(Modal::Confirm(ConfirmState {
            message: "replace the captured login?".to_string(),
            detail: Some("the login you already captured will be dropped".to_string()),
            choice: false,
            scroll: ConfirmScroll::default(),
            on_confirm: ConfirmAction::RestartLogin(name, is_new),
        }));
    } else {
        start_login(app, name, is_new);
    }
}

/// A login already running owns the progress modal: a ⏎ aimed at the same
/// target re-expands it, one aimed elsewhere says so. Either way nothing new
/// starts — a second worker would race the first for `app.login`.
fn login_in_flight(app: &mut App, name: &str, is_new: bool) -> bool {
    let Some(session) = app.login.as_ref() else {
        return false;
    };
    if session.name != name || session.is_new != is_new {
        app.toast(
            ToastKind::Warning,
            format!("a login for '{}' is already in progress", session.name),
        );
    }
    open_login_modal(app);
    true
}

/// The worker→UI event for one `oauth_login` milestone.
fn login_event(progress: crate::oauth_login::LoginProgress) -> LoginEvent {
    use crate::oauth_login::LoginProgress;
    match progress {
        LoginProgress::ExchangingCode(door) => LoginEvent::Stage(LoginStage::ExchangingCode(door)),
        LoginProgress::Verifying => LoginEvent::Stage(LoginStage::Verifying),
    }
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
        method: LoginMethod::Browser,
        paste: None,
        paste_field: None,
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
                // The toast leads with the root cause, then the rest of the
                // chain outermost-first: the outermost context here is a bare
                // "loopback accept failed" whose io cause is the only part that
                // says what to do about it. No constructor in `alibaba_login`
                // embeds the token or an upstream body.
                .map_err(|e| error_toast_detail(&e)),
        ));
    });
    open_login_modal(app);
}

/// Show the login progress modal (no-op when already open).
fn open_login_modal(app: &mut App) {
    if !app.modals.iter().any(|m| matches!(m, Modal::Login)) {
        app.open_modal(Modal::Login);
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
    // A keystroke, commit or revert can add or drop rows above the field (a
    // typed base url flips the account's rows), and the cursor is an index.
    let edited = app
        .config_draft
        .as_ref()
        .and_then(|d| d.active)
        .unwrap_or(active);
    if let Some(pos) = config_row_index(app, edited) {
        app.config_action_cursor = pos;
    }
}

/// ⎋ inside a field: existing accounts revert from the live profile;
/// new drafts keep the typed value. Either way, editing ends.
///
/// Escape on a just-added env entry removes the row instead: the add only
/// persisted an empty placeholder, so backing out restores the pre-add state.
fn cancel_config_edit(app: &mut App, field: ConfigRow) {
    let editing_name = app
        .config_draft
        .as_ref()
        .and_then(|d| d.editing_name.clone())
        .map(ProfileName::from);
    if let Some(name) = editing_name.clone() {
        let just_added = app
            .config_draft
            .as_ref()
            .and_then(|d| d.env_just_added.clone());
        if let (Some(added), ConfigRow::EnvEntry(i)) = (just_added, field) {
            let key = {
                let cfg = app.config();
                cfg.find(&name).and_then(|p| p.env.keys().nth(i).cloned())
            };
            // Keyed on the just-added state, never on an empty buffer: an
            // existing entry the operator is merely editing empty must keep
            // its saved value.
            if key.as_deref() == Some(added.as_str()) {
                cancel_just_added_env(app, &name, &added);
                return;
            }
        }
    }
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

/// Escape on a just-added env entry: drop the placeholder and persist through
/// [`edit_profile_env`] — the same writer the value-commit unset uses — so the
/// active profile's live settings lose the key too. On a failed persist the
/// editor and the flag stay, so a retry still removes the row.
fn cancel_just_added_env(app: &mut App, name: &ProfileName, key: &str) {
    let new_env = {
        let cfg = app.config();
        cfg.find(name).map(|p| {
            let mut env = p.env.clone();
            env.remove(key);
            env
        })
    };
    let Some(new_env) = new_env else {
        // The profile vanished from the in-memory config (an out-of-band
        // delete/rename); disarm the cancel and end the edit instead of
        // leaving the editor stuck open with a zombie flag.
        if let Some(d) = app.config_draft.as_mut() {
            d.env_just_added = None;
            d.active = None;
        }
        return;
    };
    let result = {
        let mut cfg = app.config();
        edit_profile_env(&mut cfg, name, new_env)
    };
    match result {
        Ok(()) => {
            if let Some(d) = app.config_draft.as_mut() {
                d.env_just_added = None;
                d.env_value = InputState::new("");
                d.active = None;
            }
        }
        Err(e) => {
            // `edit_profile_env` mutates the in-memory profile BEFORE its
            // disk writes, so a failed persist leaves memory without the key
            // while the placeholder stays on disk — put it back so memory
            // matches disk and a retry's index lookup finds the same row it
            // cancelled.
            {
                let mut cfg = app.config();
                if let Some(p) = cfg.find_mut(name) {
                    p.env.insert(key.to_string(), String::new());
                }
            }
            app.toast(ToastKind::Danger, format!("env update failed\n{e}"))
        }
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
        ConfigRow::BellThreshold => profile
            .and_then(|p| p.bell_threshold)
            .map(format_weekly_pct)
            .unwrap_or_default(),
        // The saved value of the i-th sorted env entry; reverts a value edit on ⎋.
        ConfigRow::EnvEntry(i) => profile
            .and_then(|p| p.env.values().nth(i).cloned())
            .unwrap_or_default(),
        ConfigRow::AutoStart
        | ConfigRow::ModelOverrideAdd
        | ConfigRow::EnvAdd
        | ConfigRow::Login
        | ConfigRow::CaptureLogin
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
        ConfigRow::BellThreshold => commit_bell_threshold(app),
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

/// ⏎ on the `alert at` field: a value in `0..=100` sets the bell, an empty one
/// turns it off; anything else keeps the editor open, as the Fallback card's
/// threshold editor does.
fn commit_bell_threshold(app: &mut App) {
    let raw = app
        .config_draft
        .as_ref()
        .map(|d| d.bell_threshold.trimmed().to_string())
        .unwrap_or_default();
    let Some(value) = parse_bell_threshold(&raw) else {
        return;
    };
    write_bell_threshold(app, value);
}

/// A typed `alert at` value: a number in `0..=100`, or EMPTY, which turns the
/// bell off. `Some(None)` = off, `Some(Some(v))` = set, `None` = refused.
pub(crate) fn parse_bell_threshold(raw: &str) -> Option<Option<f64>> {
    if raw.is_empty() {
        return Some(None);
    }
    crate::fallback::parse_threshold(raw).map(Some)
}

/// `+`/`-` on the `alert at` row: step by `delta` inside the typed range,
/// starting from 95% when the alert is off.
fn step_bell_threshold(app: &mut App, delta: f64) {
    let Some(name) = app
        .config_draft
        .as_ref()
        .and_then(|d| d.editing_name.clone())
        .map(ProfileName::from)
    else {
        return;
    };
    let current = app.config().find(&name).and_then(|p| p.bell_threshold);
    let next = match current {
        // Rounded to hundredths, so a step off `5.01` lands on `0.01`, not on
        // a float tail.
        Some(v) => (((v + delta) * 100.0).round() / 100.0).clamp(
            crate::fallback::MIN_THRESHOLD,
            crate::fallback::MAX_THRESHOLD,
        ),
        None => DEFAULT_BELL_THRESHOLD,
    };
    write_bell_threshold(app, Some(next));
}

/// Where `+`/`-` start on an account with no alert: the commented-out default
/// in a fresh `config.toml`.
const DEFAULT_BELL_THRESHOLD: f64 = 95.0;

/// Persist the drafted account's alert threshold, rolling memory back on a
/// failed save, then reseed the row's buffer from what was saved.
fn write_bell_threshold(app: &mut App, value: Option<f64>) {
    let Some(name) = app
        .config_draft
        .as_ref()
        .and_then(|d| d.editing_name.clone())
        .map(ProfileName::from)
    else {
        return;
    };
    // `-0` saves as `0`, the value a reload reads back.
    let value = value.map(|v| v + 0.0);
    let result = {
        let mut cfg = app.config();
        match cfg.find_mut(&name) {
            None => None,
            Some(profile) => {
                let before = profile.bell_threshold;
                profile.bell_threshold = value;
                let saved = save_profile_config(profile);
                if saved.is_err()
                    && let Some(p) = cfg.find_mut(&name)
                {
                    p.bell_threshold = before;
                }
                Some(saved)
            }
        }
    };
    match result {
        // The account left under the open editor (an outside delete or
        // rename): end the edit rather than leave a dead ⏎.
        None => {
            if let Some(d) = app.config_draft.as_mut() {
                d.active = None;
            }
        }
        Some(Ok(())) => {
            let shown = {
                let cfg = app.config();
                row_committed_value(cfg.find(&name), &name, ConfigRow::BellThreshold)
            };
            if let Some(d) = app.config_draft.as_mut() {
                d.bell_threshold = InputState::new(&shown);
                d.active = None;
            }
        }
        Some(Err(e)) => app.toast(ToastKind::Danger, format!("alert update failed\n{e}")),
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
        | ConfigRow::BellThreshold
        | ConfigRow::BaseUrl
        | ConfigRow::ApiKey
        | ConfigRow::ModelOverrideAdd
        | ConfigRow::EnvEntry(_)
        | ConfigRow::EnvAdd
        | ConfigRow::Login
        | ConfigRow::CaptureLogin
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

/// Advance the `model` alias cycle: default → fable → opus → sonnet → haiku →
/// opusplan → default. A value outside [`MODEL_PRESETS`] (a custom id)
/// collapses to default.
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

/// ⏎ in an env value editor: fold the trimmed buffer into the entry,
/// persist via [`edit_profile_env`], then reseed from the saved value.
/// An emptied value is an unset — the entry is dropped, so neither the
/// profile's config.toml nor the live settings.json keeps `key = ""`.
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
    let key = {
        let cfg = app.config();
        cfg.find(&name).and_then(|p| p.env.keys().nth(i).cloned())
    };
    let new_env = {
        let cfg = app.config();
        cfg.find(&name).map(|p| {
            let mut env = p.env.clone();
            if let Some(key) = key.clone() {
                if value.is_empty() {
                    // Unset, not blank: `edit_profile_env` strips the dropped
                    // key from the live settings on its own (prev-key removal).
                    env.remove(&key);
                } else {
                    env.insert(key, value.clone());
                }
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
            let saved = if value.is_empty() {
                // The entry is gone; `values().nth(i)` would now read the
                // SIBLING that slid into its slot.
                String::new()
            } else {
                let cfg = app.config();
                cfg.find(&name)
                    .and_then(|p| p.env.values().nth(i).cloned())
                    .unwrap_or_default()
            };
            if let Some(d) = app.config_draft.as_mut() {
                if d.env_just_added.as_deref() == key.as_deref() {
                    // A value commit resolves the just-added state: the entry
                    // is either kept with its value or dropped by the unset
                    // gate above — either way the cancel arm disarms.
                    d.env_just_added = None;
                }
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
        Some(c) => app.open_modal(Modal::EnvCollision(env_collision_form(
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
        // The add persisted an empty placeholder; arm the cancel so an escape
        // in the value editor removes the row instead of keeping `key = ""`.
        if let Some(d) = app.config_draft.as_mut() {
            d.env_just_added = Some(key.to_string());
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
    if let Some(row_pos) = config_row_index(app, ConfigRow::EnvEntry(idx)) {
        app.config_action_cursor = row_pos;
    }
    enter_env_value_edit(app, idx);
}

/// Position of `row` in the current detail-row list, if present.
fn config_row_index(app: &App, row: ConfigRow) -> Option<usize> {
    config_rows(app).iter().position(|r| *r == row)
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
        scroll: std::cell::Cell::new(0),
        reviewing_overwrite: false,
        confirm_overwrite: false,
        max_scroll: std::cell::Cell::new(0),
        viewport: std::cell::Cell::new(0),
        overwrite_needs_review: std::cell::Cell::new(false),
    }
}

fn handle_env_collision_key(app: &mut App, key: KeyEvent) {
    let Some(Modal::EnvCollision(state)) = app.modals.last_mut() else {
        return;
    };
    if state.reviewing_overwrite {
        match key.code {
            KeyCode::Up => {
                state.scroll.set(
                    state
                        .scroll
                        .get()
                        .min(state.max_scroll.get() as usize)
                        .saturating_sub(1),
                );
            }
            KeyCode::Down => {
                state.scroll.set(
                    state
                        .scroll
                        .get()
                        .saturating_add(1)
                        .min(state.max_scroll.get() as usize),
                );
            }
            KeyCode::PageUp => {
                state.scroll.set(page_up_scroll(
                    state.scroll.get(),
                    state.max_scroll.get() as usize,
                    state.viewport.get(),
                ));
            }
            KeyCode::PageDown => {
                state.scroll.set(page_down_scroll(
                    state.scroll.get(),
                    state.max_scroll.get() as usize,
                    state.viewport.get(),
                ));
            }
            KeyCode::Left | KeyCode::Right | KeyCode::Tab => {
                state.confirm_overwrite = !state.confirm_overwrite;
            }
            KeyCode::Esc | KeyCode::Char('q') => {
                state.reviewing_overwrite = false;
                state.confirm_overwrite = false;
                state.scroll.set(0);
            }
            KeyCode::Enter | KeyCode::Char(' ') => {
                if state.confirm_overwrite {
                    let name = ProfileName::from(state.profile.clone());
                    let pending = state.key.clone();
                    app.modals.pop();
                    run_env_collision_choice(
                        app,
                        &name,
                        &pending,
                        None,
                        EnvCollisionChoice::Overwrite,
                    );
                } else {
                    state.reviewing_overwrite = false;
                    state.scroll.set(0);
                }
            }
            _ => {}
        }
        return;
    }
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
            if choice == EnvCollisionChoice::Overwrite && state.overwrite_needs_review.get() {
                state.reviewing_overwrite = true;
                state.confirm_overwrite = false;
                state.scroll.set(0);
                return;
            }
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
                && let Some(row_pos) = config_row_index(app, ConfigRow::EnvEntry(idx))
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
    let validation = validate_profile_name(&new, Harness::Claude, Some(old.as_str()));
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

/// Drop one profile's name-keyed fetch-status + streak chrome — the shared
/// clear every credential-replacing path runs: a repaired or repointed
/// credential retires the old provider's verdicts, so the next frame
/// must read no stale `[ key rejected ]` pill and the throttler attribution
/// cannot accuse the new endpoint with the old provider's streak. The OAuth
/// fetch status joins them because the render merges it over the third-party
/// one — either left standing would still read as the stale verdict. The
/// config handle's `Profile.fetch_status` copy retires with them: that copy
/// is the render's read, and only the next tick's `apply_usage` refreshes it
/// from the cleared stores, so the pre-tick frame would flash the retired
/// pill without it. The handle write is unconditional: a poison-skipped
/// clear keeps its store entry, but that entry is dead weight — `apply_usage`
/// reads these stores with `.lock().ok()` and skips a poisoned one, so the
/// next tick can never re-copy it. `None` here is the stable outcome in
/// every poison placement; a gated write would strand the stale pill on the
/// handle indefinitely where the skipped store is the merge's third-party
/// fallback (the OAuth arm's cleared entry misses `contains_key`, the
/// poisoned fallback reads as `None`, and nothing rewrites the field).
fn clear_profile_status_chrome(app: &App, name: &ProfileName) {
    if let Ok(mut m) = app.third_party_status.lock() {
        m.remove(name.as_str());
    }
    if let Ok(mut m) = app.third_party_streaks.lock() {
        m.remove(name.as_str());
    }
    if let Ok(mut m) = app.usage_status.lock() {
        m.remove(name.as_str());
    }
    // All three store guards are dropped above, so this keeps the documented
    // order (stores 280/290/350 before Config 400). The write is the outcome
    // the store→handle sync in `apply_usage` would derive from the cleared
    // stores (OAuth entry, else third-party entry, else None) — written in
    // place, since re-reading the stores under the config guard would invert
    // that order. And unconditional: a poison-skipped clear leaves its entry
    // in a store `apply_usage` skips (`.lock().ok()`), so the entry is dead
    // weight and `None` is stable in every placement — a gated write would
    // strand the stale pill where the skipped store is the merge's
    // third-party fallback (the OAuth arm misses, the poisoned arm reads as
    // `None`, nothing rewrites the handle).
    let mut cfg = app.config();
    if let Some(p) = cfg.find_mut(name) {
        p.fetch_status = None;
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
            // Re-collect the token and third-party entries so the fetch leg
            // schedules the re-keyed credential's first fetch same-frame. The
            // rejection verdict reads the AppConfig profile fingerprint, never
            // this list, so a stale entry cannot keep the name key-rejected.
            app.refresh_tokens();
            clear_profile_status_chrome(app, &name);
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
    // The live-login stash commits in BOTH modes — a live setup can be an
    // endpoint — and the snapshot's own endpoint wins over anything typed.
    let captured = match d.captured_login.clone() {
        live @ Some(DraftLogin::LiveLogin(_)) => live,
        mint @ Some(DraftLogin::Mint(_)) if base_url.is_none() => mint,
        _ => None,
    };
    let mint_discarded =
        base_url.is_some() && matches!(d.captured_login, Some(DraftLogin::Mint(_)));
    let endpoint_overridden =
        base_url.is_some() && matches!(d.captured_login, Some(DraftLogin::LiveLogin(_)));
    let validation = validate_profile_name(&name, Harness::Claude, None);
    if let Err(e) = validation {
        app.toast(ToastKind::Danger, format!("{e}"));
        return;
    }
    let api_key = if base_url.is_some() { api_key } else { None };
    let result = {
        let mut cfg = app.config();
        match captured {
            // Both stashes parked the login's uuid until this moment fixed the
            // name; the commit functions anchor it on the create.
            Some(DraftLogin::LiveLogin(snapshot)) => {
                capture_into_profile(&mut cfg, name.clone(), model, *snapshot)
            }
            // The draft parked the login's uuid until this moment fixed the
            // name; `create_profile_from_login` anchors it on the commit.
            Some(DraftLogin::Mint(login)) => create_profile_from_login(
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
            if endpoint_overridden {
                app.toast(
                    ToastKind::Info,
                    "created from the captured current login\nthe typed endpoint fields were not used",
                );
            }
            app.refresh_tokens();
            app.last_reload_fp = reload_fingerprint();
            app.refresh_unsaved_live_login();
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
        app.open_modal(Modal::Confirm(ConfirmState {
            message: format!("delete '{name}' anyway?"),
            detail: Some(
                "this account has a live clauth start session; deleting it may log that \
                 session out."
                    .to_string(),
            ),
            choice: false,
            scroll: ConfirmScroll::default(),
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
            app.refresh_unsaved_live_login();
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
    app.open_modal(Modal::Confirm(ConfirmState {
        message: format!("disable '{name}'?"),
        detail: Some(DISABLE_DETAIL.to_string()),
        choice: false,
        scroll: ConfirmScroll::default(),
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
    app.open_modal(Modal::NamePrompt(NamePromptForm {
        input: InputState::new(""),
        action: NamePromptAction::DuplicateProfile(name.to_string()),
    }));
}

/// `save as preset`: ask for the preset name.
fn prompt_save_preset(app: &mut App) {
    let Some((name, _, _)) = focused_account(app) else {
        return;
    };
    app.open_modal(Modal::NamePrompt(NamePromptForm {
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
    app.open_modal(Modal::PresetPicker(PresetPickerForm {
        target,
        presets: crate::presets::list_presets(),
        cursor: 0,
        scroll: std::cell::Cell::new(0),
        viewport: std::cell::Cell::new(0),
        starts: std::cell::RefCell::new(Vec::new()),
        block_ends: std::cell::RefCell::new(Vec::new()),
        total: std::cell::Cell::new(0),
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
                    let validation = validate_profile_name(&name, Harness::Claude, None);
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
                    // The preset store is its own namespace — neither harness's
                    // roster has a say; only the charset + built-in rules apply.
                    if let Err(e) = validate_name_chars(&name) {
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
                        app.open_modal(Modal::Confirm(ConfirmState {
                            message: format!("preset '{name}' already exists."),
                            detail: Some(
                                "overwrite it with this account's base url and model settings?"
                                    .to_string(),
                            ),
                            choice: false,
                            scroll: ConfirmScroll::default(),
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
        KeyCode::PageUp => {
            let starts = state.starts.borrow();
            let block_ends = state.block_ends.borrow();
            let next = page_up_cursor_by_rows(
                state.cursor,
                &starts,
                &block_ends,
                &[],
                state.viewport.get(),
                state.scroll.get(),
                state.total.get(),
            );
            drop(starts);
            drop(block_ends);
            state.cursor = next;
        }
        KeyCode::PageDown => {
            let starts = state.starts.borrow();
            let block_ends = state.block_ends.borrow();
            let next = page_down_cursor_by_rows(
                state.cursor,
                &starts,
                &block_ends,
                state.viewport.get(),
                state.scroll.get(),
                state.total.get(),
            );
            drop(starts);
            drop(block_ends);
            state.cursor = next;
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
            app.open_modal(Modal::Confirm(ConfirmState {
                message: format!("delete preset '{}'?", preset.name),
                detail: Some("accounts already stamped from it are untouched.".to_string()),
                choice: false,
                scroll: ConfirmScroll::default(),
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
            app.open_modal(Modal::Confirm(ConfirmState {
                message: format!("apply '{}' over '{target}'?", preset.name),
                detail: Some(format!("replaces {}.", clobbered.join(", "))),
                choice: false,
                scroll: ConfirmScroll::default(),
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
        ("fable", m.fable.is_some()),
        ("opus", m.opus.is_some()),
        ("sonnet", m.sonnet.is_some()),
        ("haiku", m.haiku.is_some()),
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
            // The repoint retires the old provider's verdicts — the shared
            // clear keeps its streak from accusing the new endpoint.
            clear_profile_status_chrome(app, &target);
            app.last_reload_fp = reload_fingerprint();
            if app.config_draft.is_some() {
                let selected = config_rows(app).get(app.config_action_cursor).copied();
                app.config_draft = Some(build_draft_existing(app, &target));
                if let Some(pos) = selected.and_then(|row| config_row_index(app, row)) {
                    app.config_action_cursor = pos;
                }
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
/// the row stays truly inert (silent no-op, matching the dimmed-row rule):
/// `disable_profile`'s own refusal is a real `bail!`, and without this early
/// return every press would surface a red danger toast instead of doing
/// nothing, which is what a dimmed/disabled row is supposed to mean.
/// `disable_profile`/`enable_profile` persist into the live shared
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
                match save_profile_config(profile) {
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
        KeyCode::Up => {
            let max = state.scroll.max_scroll.get();
            state.scroll.offset = state.scroll.offset.min(max).saturating_sub(1);
        }
        KeyCode::Down => {
            let max = state.scroll.max_scroll.get();
            state.scroll.offset = state.scroll.offset.saturating_add(1).min(max);
        }
        KeyCode::PageUp => {
            let max = state.scroll.max_scroll.get();
            state.scroll.offset = page_up_scroll(
                state.scroll.offset as usize,
                max as usize,
                state.scroll.viewport.get() as usize,
            ) as u16;
        }
        KeyCode::PageDown => {
            let max = state.scroll.max_scroll.get();
            state.scroll.offset = page_down_scroll(
                state.scroll.offset as usize,
                max as usize,
                state.scroll.viewport.get() as usize,
            ) as u16;
        }
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
/// Shared by the heal fix and the `delegate row text` options row — the knob
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
            recompute_services_checks(app, false, ShuntRefresh::Keep);
        }
        Ok(notes) => {
            // Non-empty notes = pieces clauth refused to touch (a table it
            // cannot extend by appending). Reporting success over those is a
            // lie, so the notes surface verbatim.
            app.toast(
                ToastKind::Warning,
                format!("herdr's config needs attention\n{}", notes.join("\n")),
            );
            recompute_services_checks(app, false, ShuntRefresh::Keep);
        }
        Err(e) => app.toast(ToastKind::Danger, format!("herdr config fix failed\n{e}")),
    }
}

/// The detail line of a shunt-card confirm whose adopted record vanished
/// before the yes.
const NO_ADOPTED_CONFIG: &str = "no shunt config is adopted any more";

fn run_confirm_action(app: &mut App, action: ConfirmAction) {
    match action {
        ConfirmAction::CaptureConflict(snapshot, from_divergence) => {
            app.open_modal(Modal::CaptureName(CaptureNameForm {
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
                    clear_profile_status_chrome(app, &name);
                    app.last_reload_fp = reload_fingerprint();
                    app.refresh_unsaved_live_login();
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
                    clear_profile_status_chrome(app, &name);
                    app.last_reload_fp = reload_fingerprint();
                    app.refresh_unsaved_live_login();
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
                    recompute_services_checks(app, false, ShuntRefresh::Keep);
                }
                Err(e) => app.toast(ToastKind::Danger, format!("wire failed\n{e}")),
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
                recompute_services_checks(app, false, ShuntRefresh::Keep);
            }
            // Installed / Repaired / Adopted / Updated, or anything agentgear
            // adds later: a real change happened, so the success toast is
            // earned. (`Outcome` is #[non_exhaustive], so the wildcard is the
            // contract, not a shortcut.)
            Ok(outcome) => {
                app.toast(ToastKind::Success, format!("clauth plugin {outcome}"));
                // Reflect the fresh install in the rows without a version probe.
                recompute_services_checks(app, false, ShuntRefresh::Keep);
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
                    // No leg ever fetches a keyless profile, so a stale
                    // `[ key rejected ]` verdict would render forever — the
                    // shared clear retires it with the credential.
                    clear_profile_status_chrome(app, &name);
                    app.last_reload_fp = reload_fingerprint();
                    app.refresh_unsaved_live_login();
                    app.toast(ToastKind::Success, format!("logged out of '{name}'"));
                }
                Err(e) => app.toast(ToastKind::Danger, format!("log out failed\n{e}")),
            }
        }
        ConfirmAction::RestartLogin(name, is_new) => start_login(app, name, is_new),
        ConfirmAction::CaptureOverMintStash(snapshot) => {
            if stash_new_form_login(app, DraftLogin::LiveLogin(snapshot)) {
                app.toast(
                    ToastKind::Success,
                    "current login captured\ncreate account saves it",
                );
            }
        }
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
        ConfirmAction::AdoptConfig(path) => {
            app.services
                .shunt_action
                .start(ShuntActionJob::Adopt { found: path });
        }
        ConfirmAction::MoveStoresIn(confirmed) => {
            let Some(record) = app.services.shunt_record.clone() else {
                app.toast(
                    ToastKind::Danger,
                    format!("move failed\n{NO_ADOPTED_CONFIG}"),
                );
                return;
            };
            app.services
                .shunt_action
                .start(ShuntActionJob::Move { record, confirmed });
        }
        ConfirmAction::AddAdminKey => {
            let Some(record) = app.services.shunt_record.clone() else {
                app.toast(
                    ToastKind::Danger,
                    format!("admin key failed\n{NO_ADOPTED_CONFIG}"),
                );
                return;
            };
            app.services
                .shunt_action
                .start(ShuntActionJob::AddAdminKey { record });
        }
        ConfirmAction::AddAdminTable => {
            let Some(record) = app.services.shunt_record.clone() else {
                app.toast(
                    ToastKind::Danger,
                    format!("admin key failed\n{NO_ADOPTED_CONFIG}"),
                );
                return;
            };
            app.services
                .shunt_action
                .start(ShuntActionJob::AddAdminTable { record });
        }
        ConfirmAction::StartDaemon => start_daemon(app),
        ConfirmAction::StopShunt => {
            if shunt_verb_at_pick(app, ActionMenuAction::StopShunt).is_some() {
                app.services.shunt_action.start(ShuntActionJob::Hold);
            }
        }
        ConfirmAction::RestartBind { key, edit } => {
            start_shunt_save(app, ShuntSaveKey::Field(key), *edit, true);
        }
        ConfirmAction::RenameField { key, edit } => {
            start_shunt_save(app, ShuntSaveKey::Field(key), *edit, false);
        }
        ConfirmAction::RemoveEntry { key, edit } => {
            // Re-arming a remove clears the stale refusal line under the row.
            app.services.shunt_remove_refusal.take();
            start_shunt_save(app, ShuntSaveKey::Remove(key), *edit, false);
        }
        ConfirmAction::RemoveToken { record, profile } => {
            app.services.shunt_token_refusal.take();
            start_shunt_token_save(app, ShuntSaveKey::Token(profile.clone()), record, profile);
        }
        ConfirmAction::AddToken {
            record,
            profile,
            url,
            replace: _,
        } => {
            start_shunt_token_add(app, record, profile, url);
        }
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
        KeyCode::PageUp => {
            let starts = state.starts.borrow();
            let block_ends = state.block_ends.borrow();
            let next = page_up_cursor_by_rows(
                state.cursor,
                &starts,
                &block_ends,
                &[],
                state.viewport.get(),
                state.scroll.get(),
                state.total.get(),
            );
            drop(starts);
            drop(block_ends);
            state.cursor = next;
        }
        KeyCode::PageDown => {
            let starts = state.starts.borrow();
            let block_ends = state.block_ends.borrow();
            let next = page_down_cursor_by_rows(
                state.cursor,
                &starts,
                &block_ends,
                state.viewport.get(),
                state.scroll.get(),
                state.total.get(),
            );
            drop(starts);
            drop(block_ends);
            state.cursor = next;
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
                    app.open_modal(Modal::Confirm(ConfirmState {
                        message: format!("switch to '{owner}'? the live login is its account."),
                        detail: Some(format!(
                            "the login is saved into '{owner}' and '{owner}' becomes the active \
                             account. the running claude is untouched."
                        )),
                        choice: false,
                        scroll: ConfirmScroll::default(),
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
            // The overwrite captured a live login that may be a DIFFERENT
            // account: expire the clock so the next `/profile` read re-seeds the
            // anchor, the way `overwrite_captured_profile` does.
            crate::usage::expire_profile_ttl(&ProfileName::from(active));
            if let Err(e) = force_link_profile_credentials(&ProfileName::from(active)) {
                app.toast(ToastKind::Danger, format!("relink failed\n{e}"));
                return;
            }
            app.refresh_tokens();
            // The live login replaced the stored credential, so the old
            // credential's verdicts must not survive it — the shared clear,
            // not a wait for the next OAuth fetch.
            clear_profile_status_chrome(app, &ProfileName::from(active));
            // AUTH-1: the fresh login is the documented recovery for a revoked
            // chain, so a standing auth_broken quarantine is stale — same lift
            // every sibling fresh-login path runs.
            oauth::mark_auth_broken(&app.config, &ProfileName::from(active), false);
            app.toast(
                ToastKind::Success,
                format!("saved live credentials into '{active}'"),
            );
        }
        DivergenceChoice::NewProfile => open_divergence_target_picker(app),
        DivergenceChoice::Discard => {
            app.open_modal(Modal::Confirm(ConfirmState {
                message: format!("discard the new login and restore '{active}'?"),
                detail: Some(
                    "claude code's freshly written credentials will be overwritten with the account's stored tokens.".to_string(),
                ),
                choice: false,
                scroll: ConfirmScroll::default(),
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
            scroll: std::cell::Cell::new(0),
            viewport: std::cell::Cell::new(0),
            starts: std::cell::RefCell::new(Vec::new()),
            block_ends: std::cell::RefCell::new(Vec::new()),
            total: std::cell::Cell::new(0),
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
        KeyCode::PageUp => {
            let starts = form.starts.borrow();
            let block_ends = form.block_ends.borrow();
            let next = page_up_cursor_by_rows(
                form.cursor,
                &starts,
                &block_ends,
                &[],
                form.viewport.get(),
                form.scroll.get(),
                form.total.get(),
            );
            drop(starts);
            drop(block_ends);
            form.cursor = next;
        }
        KeyCode::PageDown => {
            let starts = form.starts.borrow();
            let block_ends = form.block_ends.borrow();
            let next = page_down_cursor_by_rows(
                form.cursor,
                &starts,
                &block_ends,
                form.viewport.get(),
                form.scroll.get(),
                form.total.get(),
            );
            drop(starts);
            drop(block_ends);
            form.cursor = next;
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
            app.open_modal(Modal::Confirm(ConfirmState {
                message: format!("save the live login into '{target}'?"),
                detail: Some(format!(
                    "'{target}' becomes the active account; its old credentials are replaced. usage history, env, and model settings are kept."
                )),
                choice: false,
                scroll: ConfirmScroll::default(),
                on_confirm: ConfirmAction::AdoptDivergence(Box::new(snapshot), target),
            }));
        }
        _ => {}
    }
}

/// Keys on the `+ add token` profile picker: Up/Down wrap, PageUp/PageDown
/// page, `esc`/`q` close, `↵`/`space` pick (a pick opens the overwrite
/// confirm). No synthetic `+ new profile` row in C1, so the cursor is the
/// profile's own index.
fn handle_token_picker_key(app: &mut App, key: KeyEvent) {
    let Some(Modal::TokenPicker(form)) = app.modals.last_mut() else {
        return;
    };
    let last = form.targets.len() - 1;
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
        KeyCode::PageUp => {
            let starts = form.starts.borrow();
            let block_ends = form.block_ends.borrow();
            let next = page_up_cursor_by_rows(
                form.cursor,
                &starts,
                &block_ends,
                &[],
                form.viewport.get(),
                form.scroll.get(),
                form.total.get(),
            );
            drop(starts);
            drop(block_ends);
            form.cursor = next;
        }
        KeyCode::PageDown => {
            let starts = form.starts.borrow();
            let block_ends = form.block_ends.borrow();
            let next = page_down_cursor_by_rows(
                form.cursor,
                &starts,
                &block_ends,
                form.viewport.get(),
                form.scroll.get(),
                form.total.get(),
            );
            drop(starts);
            drop(block_ends);
            form.cursor = next;
        }
        KeyCode::Esc | KeyCode::Char('q') => {
            app.modals.pop();
        }
        KeyCode::Enter | KeyCode::Char(' ') => {
            let cursor = form.cursor;
            let Some(Modal::TokenPicker(form)) = app.modals.pop() else {
                return;
            };
            let Some((profile, _)) = form.targets.get(cursor).cloned() else {
                return;
            };
            open_token_overwrite_confirm(app, profile);
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
            // Chars + cross-harness only — the own-roster duplicate branch of
            // `validate_profile_name` is deliberately skipped so a claude
            // collision falls through to the canonical_name lookup below
            // (capture-into-existing) instead of dead-ending with an "already
            // exists" error. A codex-held name has no such flow — a claude
            // login can't be captured into a codex profile — so that half
            // still refuses here.
            let validation = validate_name_chars(&name)
                .map(|_| ())
                .and_then(|()| validate_foreign_harness_free(&name, Harness::Claude));
            if let Err(e) = validation {
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
                app.open_modal(Modal::Confirm(ConfirmState {
                    message: format!("account '{existing}' already exists."),
                    detail: Some(
                        "overwrite its credentials with the captured login? usage history, env, and model settings are kept.".to_string(),
                    ),
                    choice: false,
                    scroll: ConfirmScroll::default(),
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
                capture_into_profile(&mut cfg, name.clone(), None, *snapshot)
            };
            match result {
                Ok(()) => {
                    app.refresh_tokens();
                    app.last_reload_fp = reload_fingerprint();
                    app.refresh_unsaved_live_login();
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
                // Re-clamp after the loader replaces the grouped list.
                clamp_token_model_cursor(app);
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

/// Drain the login worker: track URL/stage events (the door rides the first
/// stage bump, and only the worker's report may set it — a paste sent a moment
/// after the browser callback landed lost), and on a result apply it (stash or
/// overwrite) — discarding a stale result from a superseded login. A stage
/// bump that closes the paste door closes the code field with it: a code typed
/// after the browser callback won has no door to reach.
fn drain_login_events(app: &mut App) {
    while let Ok((generation, event)) = app.login_event_rx.try_recv() {
        if let Some(session) = app.login.as_mut()
            && session.generation == generation
        {
            match event {
                LoginEvent::Url(url) => session.url = Some(url),
                LoginEvent::Stage(stage) => {
                    if let LoginStage::ExchangingCode(door) = stage {
                        session.method = door;
                    }
                    session.stage = stage;
                    if session.open_door().is_none() {
                        session.paste_field = None;
                    }
                }
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
        let stashed = stash_new_form_login(app, DraftLogin::Mint(Box::new(outcome)));
        if stashed {
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
        anchor: outcome
            .account_uuid
            .map(crate::actions::AnchorAction::Proven)
            .unwrap_or(crate::actions::AnchorAction::Unproven),
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
        app.open_modal(Modal::Confirm(ConfirmState {
            message: format!("replace the stored credentials for '{}'?", session.name),
            detail: Some(
                if keeps_endpoint {
                    "a fresh login finished for this account. the old tokens are dropped; chain slot, env, model settings, and its endpoint and api key stay."
                } else {
                    "a fresh login finished for this account. the old tokens are dropped; chain slot, env, and model settings stay."
                }
                .to_string(),
            ),
            choice: false,
            scroll: ConfirmScroll::default(),
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
            clear_profile_status_chrome(app, &ProfileName::from(session.name.clone()));
            app.last_reload_fp = reload_fingerprint();
            app.refresh_unsaved_live_login();
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
            // for the figures the new one can actually read. The stale
            // `console login expired` verdict goes with the old session via
            // the shared clear — it must not survive until the queued fetch.
            app.refresh_tokens();
            clear_profile_status_chrome(app, &name);
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
                    format!(
                        "update available: v{v}\nreinstall with {}",
                        prose::cmd("cargo install clauth")
                    ),
                );
            }
        }
    }

    drain_op_results(app);
    drain_status_events(app);
    drain_tokens_events(app);
    if app.tab == Tab::Tokens
        && app.token_view == TokenView::Models
        && app.token_period.bucket().is_some()
    {
        clamp_token_model_cursor(app);
    }
    drain_pricing_events(app);
    drain_login_events(app);

    if app.reload_if_state_changed() {
        app.clamp_profile_cursor();
        if app.profile_count() == 0 {
            app.usage_detail_focus = StatusFocus::List;
            app.usage_detail_scroll = 0;
        }
    }
    app.apply_usage();

    drain_switch_gates(app);

    let auto_switch_targets: Vec<PendingSwitchTarget> = app
        .pending_switch
        .lock()
        .map(|mut g| g.drain().collect())
        .unwrap_or_default();
    for decision in auto_switch_targets {
        let name = ProfileName::from(decision.target.clone());
        if switch_gate_in_flight(&app.activity) || !is_idle(&app.activity, &name) {
            continue;
        }
        // The label keys on the DESTINATION, not on which pass fired: any move
        // landing on the preferred (home) account reads as a homecoming. The
        // healthy-active return pass is the usual source, but the exhaustion walk
        // can also land here when the preferred is the only clear member left —
        // both are genuinely "now on home", so the destination-based label holds
        // without threading the cause through `SwitchAction`.
        let key_rejected = app.key_rejected_names();
        // Revalidate under the fresh config (reloaded above). Only a record that
        // CARRIES a key-rejection cause can be stale: a repair landing between
        // the scan's queue and this dispatch changes the active's fingerprint, so
        // the switch-away it motivated is dropped instead of moving off the
        // just-repaired account. A cause-absent (ordinary exhaustion/home) record
        // executes even beside an ambient stale mark.
        {
            let cfg = app.config();
            if crate::usage::queued_switch_away_is_stale(
                decision.key_rejected_cause.as_ref(),
                &cfg.profiles,
            ) {
                continue;
            }
        }
        let returning = app.config().is_home_today(&name, &key_rejected);
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
    poll_live_sessions(app);
    sync_broken_verdicts(app);
    poll_codex_rows(app);
    drain_service_probes(app);
    poll_services_refresh(app);
    drain_daemon_control(app);
    poll_daemon_health(app);
    drain_left_behind_probe(app);

    warn_day_claim_notices(app);
    update_banner(app);
    app.prune_toasts();
}

/// Say once, per day and per config change, what today's day lists are doing
/// that the operator did not write them to do. Toast for the operator at the
/// keyboard, `logline!` for the record a headless run leaves behind.
///
/// Gated on the notice text rather than on a bool because the chain pass that
/// resolves the claim re-runs every tick: an ungated warning would repaint the
/// same line until midnight, and a bool one would stay silent when the
/// operator edits a second list in while the first notice is still up.
/// Dropping a notice out of the set is what lets the same state, removed and
/// re-introduced, warn again.
pub(crate) fn warn_day_claim_notices(app: &mut App) {
    let key_rejected = app.key_rejected_names();
    let notices = app.config().day_claim_notices_today(&key_rejected);
    if notices == app.day_claim_notices {
        return;
    }
    let fresh: Vec<String> = notices
        .iter()
        .filter(|m| !app.day_claim_notices.contains(m))
        .cloned()
        .collect();
    for msg in fresh {
        crate::logline::logline!("clauth: {msg}");
        app.toast(ToastKind::Warning, msg);
    }
    app.day_claim_notices = notices;
}

/// Re-read the codex roster for the Overview's codex section and the header's
/// account count, at most once a second: a roster TOML plus a few small files
/// per account is cheap but not per-frame cheap, and both a `clauth login --codex`
/// in another terminal and a codex usage fetch land on a human timescale.
/// Ungated by tab and by filter, so a `c` onto the codex view shows the current
/// roster rather than the one from whenever the view last showed it.
fn poll_codex_rows(app: &mut App) {
    const CODEX_ROWS_INTERVAL: Duration = Duration::from_secs(1);
    if app
        .last_codex_rows_refresh
        .is_some_and(|t| t.elapsed() < CODEX_ROWS_INTERVAL)
    {
        return;
    }
    app.last_codex_rows_refresh = Some(Instant::now());
    let selected = app.current_main_item();
    app.codex_rows = codex_rows();
    app.clamp_overview_cursor();
    if app.tab == Tab::Overview && selected != app.current_main_item() {
        app.sync_overview_profile_selection();
    }
}

/// Re-probe the daemon presence + `status.json` health for the `[ daemon ]`
/// header chip, and the gateway slot the action menu's shunt verb reads, at
/// most once a second (a flock try-lock + `status.json` read is cheap but not
/// free, and both change on a human timescale).
fn poll_daemon_health(app: &mut App) {
    const DAEMON_PROBE_INTERVAL: Duration = Duration::from_secs(1);
    if app.last_daemon_probe.elapsed() < DAEMON_PROBE_INTERVAL {
        return;
    }
    app.last_daemon_probe = Instant::now();
    app.daemon_health = crate::daemon::daemon_health();
    sync_gateway_state(app);
}

/// Re-tally the live-session registry for the Overview `live` column, the
/// Fallback member card and the header's fleet count, at most once a second —
/// a readdir plus an `open` + `try_lock` per row is cheap but not per-frame
/// cheap, and a session starting or exiting is a human-timescale event. Ungated
/// by tab: the header reads it on every tab, and a snapshot a second stale on
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
    app.set_live_sessions(tally);
}

/// The header chip's number: the fleet total, or `—` for a tally that could
/// not read the registry or the bare-session markers.
pub(crate) fn live_count_text(total: Option<usize>) -> String {
    total.map_or_else(|| "—".to_owned(), |n| n.to_string())
}

/// Re-sync the durable key-rejection verdicts into the live `ThirdPartyBroken`
/// mirror, at most once a second, gated per profile on the record's stat (an
/// mtime move, or the file appearing/disappearing). This is the stood-down
/// TUI's heal path: the fetcher (another process) clears or rewrites
/// `third_party_auth.json` under a credential the local mirror still marks,
/// and the config never changes, so the reload seed cannot see it. Both
/// directions ride the same stat walk — a record appearing seeds the name, a
/// record gone drops it. The seed's own shape gate
/// (`fallback::third_party_key_rejected`) is the ONE gate the insert branch
/// runs: a `ConsoleExpired` outcome also writes the record under a
/// still-matching fingerprint, and a lapsed Alibaba console session is
/// usage-only — it must never render as key-rejected, so a profile the
/// predicate refuses never inserts and any entry it already has drops. Reads
/// the record only when the stat moved; the per-second cost is one stat per
/// credentialed profile, the same discipline `poll_live_sessions` applies.
fn sync_broken_verdicts(app: &mut App) {
    const SYNC_INTERVAL: Duration = Duration::from_secs(1);
    if app
        .last_broken_verdict_sync
        .is_some_and(|t| t.elapsed() < SYNC_INTERVAL)
    {
        return;
    }
    app.last_broken_verdict_sync = Some(Instant::now());

    // The credentialed profiles plus their fingerprints, collected under the
    // config guard so the stat walk below is lock-free; the mirror lock (295)
    // ranks outside Config (400), so the update happens after the guard drops.
    // The clone carries what the shared shape predicate reads — the predicate
    // itself is the gate, never a reimplementation of its fields.
    let credentialed: Vec<(Profile, u64)> = {
        let cfg = app.config();
        cfg.profiles
            .iter()
            .filter_map(|p| {
                crate::usage::profile_credential_fingerprint(p).map(|fp| (p.clone(), fp))
            })
            .collect()
    };

    let changed: Vec<(Profile, Option<std::time::SystemTime>, u64)> = credentialed
        .into_iter()
        .filter_map(|(profile, fp)| {
            let name = profile.name.to_string();
            let stat = crate::profile_cache::profile_cache_path(
                &ProfileName::from(name.clone()),
                crate::profile_cache::THIRD_PARTY_AUTH_FILE,
            )
            .as_deref()
            .and_then(|p| std::fs::metadata(p).ok())
            .and_then(|m| m.modified().ok());
            // `Some(&stat)` doubles as the changed check: an unseen name has no
            // entry (always "changed"), and a seen one skips while the stat
            // matches — including two `None`s for a record that never existed.
            if app.broken_verdict_mtimes.get(&name) == Some(&stat) {
                return None;
            }
            Some((profile, stat, fp))
        })
        .collect();
    if changed.is_empty() {
        return;
    }

    if let Ok(mut broken) = app.third_party_broken.lock() {
        for (profile, stat, fp) in changed {
            // The insert branch runs the seed's own predicate: the record must
            // match the credential the profile holds RIGHT NOW, on a profile
            // shape that can be key-rejected at all. Everything the predicate
            // refuses — a cleared record, a re-keyed one, or a lapsed Alibaba
            // console session — drops the entry.
            if crate::fallback::third_party_key_rejected(&profile, &profile.name) {
                broken.insert(profile.name.to_string(), fp);
            } else {
                broken.remove(profile.name.as_str());
            }
            app.broken_verdict_mtimes
                .insert(profile.name.to_string(), stat);
        }
    }
}

/// How often a shown standalone-shunt claim is re-probed, so a shunt that
/// stopped answering loses its claim with no `r`.
const STANDALONE_RECHECK_INTERVAL: Duration = Duration::from_secs(5);

/// Services tab live refresh: re-run the cheap local reads (job store, wiring,
/// herdr config) at most once per interval while the tab is focused and no
/// modal is open, so a delegate started elsewhere shows up without a manual
/// `r`, and re-probe a shown standalone-shunt claim every
/// [`STANDALONE_RECHECK_INTERVAL`]. Never re-probes `claude --version` or
/// `clauth mcp` — both stay `r`-gated.
fn poll_services_refresh(app: &mut App) {
    const SERVICES_REFRESH_INTERVAL: Duration = Duration::from_secs(1);

    if app.tab != Tab::Services || !app.modals.is_empty() {
        return;
    }
    if app.last_services_refresh.elapsed() < SERVICES_REFRESH_INTERVAL {
        return;
    }
    app.last_services_refresh = Instant::now();
    recompute_services_checks(app, false, ShuntRefresh::Keep);
    // The claim renders only in the `shunt` card, the readout only while no
    // gateway is adopted.
    let claim_shown = app.gateway_state == GatewayState::Absent
        && app
            .services
            .selected_check()
            .is_some_and(|check| check.label == "shunt")
        && app
            .services
            .standalone
            .as_ref()
            .is_some_and(|readout| readout.answer.is_some());
    if claim_shown && app.services.last_standalone_recheck.elapsed() >= STANDALONE_RECHECK_INTERVAL
    {
        app.services.last_standalone_recheck = Instant::now();
        app.services.standalone_probe.start();
    }
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
        // The rotation marker alone: this result arrives a tick or more after
        // its worker returned, so the OAuth leg may already belong to a refetch.
        crate::usage::end_rotation(&app.activity, &ProfileName::from(name.clone()));
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
/// sync with whether the live login matches what a switch to the active
/// profile installs (the install source). Never pushes the modal — a divergence must not lock the whole TUI out
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
    // The reload fingerprint never stats the live credentials file, so a login
    // CC itself minted mid-session is invisible to every other refresh site.
    // This poll already reads at 1Hz; while the Setup tab is open it also
    // refreshes the `+ capture current login` row's flag — zero accounts is
    // that row's own onboarding window.
    if app.tab == Tab::Setup {
        app.refresh_unsaved_live_login();
    }
    let Some(active) = app.config().state.active_profile.as_ref().cloned() else {
        app.divergence_pending = None;
        return;
    };
    if !matches!(
        reconcile_decision(app, &active),
        DivergenceVerdict::NeedsPrompt
    ) {
        return;
    }
    resolve_or_note_divergence(app, active.as_ref());
}

/// What the shared divergence cascade ([`reconcile_decision`]) makes of the
/// live slot.
enum DivergenceVerdict {
    /// Linked, adopted, or already stored — no divergence to resolve.
    Clean,
    /// A logged-out shell. The poll exempts it outright (no nag, no banner);
    /// the startup reconcile treats it as a divergence so a configured
    /// `default_divergence` Overwrite still performs the boot heal (relink
    /// the stored login over the shell) — the shipped pin's behavior.
    Shell,
    /// A genuine divergence — the user's verdict is needed.
    NeedsPrompt,
}

/// The ONE divergence decision, shared by the startup reconcile and the 1Hz
/// poll (#104): the live login is judged against what a switch INSTALLS
/// (the install source — the sidecar for split profiles), never the
/// clauth-private usage pair, so a refreshless live login (a rolling bearer,
/// a static mint) is no divergence. Side effects: a first-login adoption runs
/// here; exempt states clear the banner. A logged-out shell is reported as
/// its own verdict so the two callers can disagree on it (see
/// [`DivergenceVerdict::Shell`]).
fn reconcile_decision(app: &mut App, active: &ProfileName) -> DivergenceVerdict {
    if !matches!(
        classify_credentials_link(active).ok(),
        Some(LinkState::Diverged)
    ) {
        app.divergence_pending = None; // resolved; a later divergence re-flags
        return DivergenceVerdict::Clean;
    }
    // A logged-out shell is nothing the poll must resolve: don't nag, and
    // never let a `default_divergence` Overwrite capture its blank tokens
    // over the stored chain.
    if live_credentials_are_shell() {
        app.divergence_pending = None;
        return DivergenceVerdict::Shell;
    }
    // First login on a credential-less profile: adopt silently, don't prompt.
    if is_first_login(active).unwrap_or(false) {
        let result = {
            let mut cfg = app.config();
            adopt_first_login(&mut cfg, active)
        };
        match result {
            Ok(()) => {
                app.refresh_tokens();
                // AUTH-1: an adopted first login is a fresh login — lift a
                // standing auth_broken quarantine like the sibling fresh-login
                // paths do.
                oauth::mark_auth_broken(&app.config, active, false);
                app.last_reload_fp = reload_fingerprint();
                app.refresh_unsaved_live_login();
                app.toast(ToastKind::Success, format!("saved login into '{active}'"));
            }
            Err(e) => app.toast(ToastKind::Danger, format!("adopt failed\n{e}")),
        }
        return DivergenceVerdict::Clean;
    }
    // A login already saved in the active profile's store holds nothing unsaved —
    // the next switch re-installs it, losing no login — so it must not raise the
    // banner. This clears clauth's own symlink AND the macOS regular-file mirror CC
    // writes over it. The switch/defer gates apply the same exemption through
    // `live_diverged_and_unsaved`; the poll must adopt a first login before this
    // point, so it checks the predicate directly here instead.
    if crate::claude::live_login_is_stored(active) {
        app.divergence_pending = None;
        return DivergenceVerdict::Clean;
    }
    DivergenceVerdict::NeedsPrompt
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
