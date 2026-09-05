//! Setup tab — account picker (plus a trailing `+ new` row) on the left, the
//! selected account's settings on the right. Editing happens inline in the
//! right pane: ⏎ on the left drops focus into the detail rows, ⏎ on a text row
//! opens an inline caret, ⏎ on a toggle flips it, and `+ new` turns the right
//! pane into a create form. No popups.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};

use super::super::app::{
    App, ConfigDraft, ConfigFocus, ConfigRow, InputState, MODEL_PRESETS, config_rows,
};
use super::super::theme;
use super::panes::{
    DIAG_DISABLED, bold_when, cycle_option, draw_scrolled_lines, draw_selector_list, head_cols,
    help_tooltip_lines, highlight_row, key_cell, label_style, master_detail, name_color,
    picker_row, pill, section_box, section_box_verbatim,
};

const KEY_W: usize = 11;
/// Fixed gap between the padded key and the value column (house standard).
const KEY_GUTTER: usize = 2;

pub(super) fn draw(frame: &mut Frame<'_>, area: Rect, app: &App) {
    // +1 for the trailing `+ new` picker row.
    let items = app.config().profiles.len() + 1;
    let (selector, settings) = master_detail(area, items);

    let profiles_focused = app.config_focus == ConfigFocus::Profiles;
    draw_selector(frame, selector, app, profiles_focused);
    draw_settings(frame, settings, app);
}

fn draw_selector(frame: &mut Frame<'_>, area: Rect, app: &App, focused: bool) {
    let cfg = app.config();
    let count = cfg.profiles.len();
    let sel = app.profile_cursor.min(count);
    draw_selector_list(frame, area, "accounts", focused, sel, |w| {
        let mut rows: Vec<_> = cfg
            .profiles
            .iter()
            .enumerate()
            .map(|(i, p)| {
                // A disabled account can never be active, so dim wins outright.
                let ns = if p.is_disabled() {
                    theme::dim()
                } else {
                    name_color(cfg.is_active(&p.name))
                };
                picker_row(i == sel, focused, p.name.to_string(), ns, w)
            })
            .collect();
        rows.push(picker_row(
            count == sel,
            focused,
            "+ new".to_string(),
            theme::accent(),
            w,
        ));
        rows
    });
}

/// Snapshot taken under one short `config` guard, decoupled from render so
/// `config_rows` can re-lock without nesting the non-reentrant mutex.
/// Text fields are skipped when a draft is active — the draft buffers own them.
struct Snap {
    title: String,
    name: String,
    base_url: String,
    api_key: String,
    model: String,
    opus: String,
    sonnet: String,
    haiku: String,
    fable: String,
    subagent: String,
    /// Sorted `(key, value)` custom env entries — one `EnvEntry` row each.
    env: Vec<(String, String)>,
    auto_start: bool,
    /// `Profile::is_disabled` — drives the `disabled` row's toggle value.
    disabled: bool,
    /// The global active profile — one of the two gates (mirroring
    /// `actions::disable_profile`'s own gate) that dims the `disabled` row inert.
    is_active: bool,
    /// A live `clauth start` session — the other gate.
    has_live_session: bool,
    /// Whether the profile holds a stored credential — the OAuth token or, for an
    /// API account, the api key. Drives the `Login` row's re-login vs first-login
    /// label and the `DeleteCreds` row's presence.
    logged_in: bool,
    /// Credential typing for the login / log-out rows (`Profile::login_is_oauth`,
    /// so a hybrid's stored pair wins over its base url). Endpoint-shaped rows
    /// keep tracking the base-url buffer instead.
    login_is_oauth: bool,
    /// Whether the account stores a login BESIDES its long-lived sidecar —
    /// either credential, regardless of typing, mirroring the CLI's own
    /// refusal. Deliberately not `logged_in`, which picks one credential by
    /// `login_is_oauth` and so reads false for a hybrid whose OAuth pair is
    /// exactly what the clear would fall back to. Gates `ClearSessionToken`.
    has_other_login: bool,
    /// Whether the clear would fall back to an OAuth LOGIN specifically, rather
    /// than merely to some credential. Separate from `has_other_login` because
    /// that one is satisfied by an api key alone, and such an account clears onto
    /// an ABSENT install source: the relink removes the live slot and, on macOS,
    /// signs the Keychain out. The `ClearSessionToken` hint promised a relink in
    /// both states until 2026-08-12. Mirrors `claude::has_stored_oauth_login`,
    /// which the CLI and the action itself read.
    clear_falls_back_to_oauth: bool,
    /// `+ new` form only: the draft holds a minted login awaiting `create
    /// account` — flips the `Login` row to its `✓ logged in` state.
    captured: bool,
    /// Recognised third-party provider display name, if any.
    provider: Option<&'static str>,
    /// This account's login IS the Alibaba console login
    /// ([`crate::profile::Profile::console_login_target`]). Read here so the
    /// `Login` row's hint and label describe the flow ⏎ actually runs: the
    /// account is api-typed by `login_is_oauth`, so both would otherwise
    /// announce the api-key re-entry.
    console_login: bool,
    /// CLA-SPLIT sidecar state (`claude::session_token_status`): `None` = no
    /// sidecar; long-lived with its stamped horizon, or the mis-filled
    /// not-long-lived shape the split disengages for. Read per frame for the
    /// selected profile only (one small file).
    session_token: Option<crate::claude::SessionTokenStatus>,
    /// What the sidecar HOLDS, not what the profile is flagged for. The two
    /// disagree in a state that is both reachable and permanent: a terminally
    /// dead chain degrades a rolling profile onto its static mint and leaves
    /// the flag set, and nothing clears it. Keyed on the flag, the row then
    /// promises a re-stamp in ~8760h for a year-scale mint no one is going to
    /// re-stamp — which is the same class of comfortable-looking lie the
    /// honest hours-scale countdown exists to prevent.
    rolling_token: bool,
    /// The CONFIG flag, for the clear row's disclosure alone: "re-stamping
    /// stops" must fire whenever the daemon would re-stamp — flag truth — even
    /// while the sidecar holds a mint (degraded) or nothing (not yet stamped).
    /// Every RENDERING surface keys off `rolling_token` above; this one names
    /// what the clear is about to turn off.
    rolling_armed: bool,
    /// Whether a preserved mint backup sits beside the sidecar — the clear
    /// row's disclosure that a second, restorable credential goes with it.
    has_static_backup: bool,
}

impl Snap {
    /// The `ClearSessionToken` gate — ONE spelling for both render readers
    /// (`detail_row`'s dim and `row_hint`'s gate line; `run_config_row` makes
    /// the same judgment from a fresh disk read): refused only when clearing
    /// would strip the account's LAST credential, i.e. a stored PIECE (sidecar
    /// or preserved mint) with no other login behind it. A flag-only account
    /// disarms without touching a credential, so it is never gated — a row
    /// that dims while its press acts would be the renderer's own lie.
    fn clear_gated(&self) -> bool {
        if self.has_other_login {
            return false;
        }
        let flag_only =
            self.rolling_armed && self.session_token.is_none() && !self.has_static_backup;
        !flag_only
    }

    /// Blank snapshot for the `+ new` form and the empty fallback.
    fn blank(title: &str) -> Snap {
        Snap {
            title: title.to_string(),
            name: String::new(),
            base_url: String::new(),
            api_key: String::new(),
            model: String::new(),
            opus: String::new(),
            sonnet: String::new(),
            haiku: String::new(),
            fable: String::new(),
            subagent: String::new(),
            env: Vec::new(),
            auto_start: false,
            disabled: false,
            is_active: false,
            has_live_session: false,
            logged_in: false,
            login_is_oauth: true,
            has_other_login: false,
            clear_falls_back_to_oauth: false,
            captured: false,
            provider: None,
            console_login: false,
            session_token: None,
            rolling_token: false,
            rolling_armed: false,
            has_static_backup: false,
        }
    }
}

fn build_snap(app: &App, with_text: bool) -> Snap {
    let text = |s: &Option<String>| {
        if with_text {
            s.clone().unwrap_or_default()
        } else {
            String::new()
        }
    };
    let cfg = app.config();
    if app.profile_cursor >= cfg.profiles.len() {
        let mut snap = Snap::blank("+ new account");
        // Mirror commit_new_account's consume rule: a typed base url flips the
        // form to API mode and the mint will be discarded, so no stale ✓.
        snap.captured = app
            .config_draft
            .as_ref()
            .is_some_and(|d| d.captured_login.is_some() && d.base_url.value.trim().is_empty());
        return snap;
    }
    match cfg.profiles.get(app.profile_cursor) {
        Some(p) => {
            let sidecar = crate::claude::sidecar_summary(&p.name);
            Snap {
                title: p.name.to_string(),
                name: if with_text {
                    p.name.to_string()
                } else {
                    String::new()
                },
                base_url: text(&p.base_url),
                api_key: text(&p.api_key),
                model: text(&p.models.default),
                opus: text(&p.models.opus),
                sonnet: text(&p.models.sonnet),
                haiku: text(&p.models.haiku),
                fable: text(&p.models.fable),
                subagent: text(&p.models.subagent),
                // Env rows render from the snapshot (no per-entry draft buffer), so
                // they're always populated — even while a draft owns the text fields.
                env: p.env.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
                auto_start: p.auto_start,
                disabled: p.is_disabled(),
                is_active: cfg.is_active(&p.name),
                // Read per frame for the selected profile only, same as
                // `session_token` below — a single small directory stat, not a
                // per-profile loop.
                has_live_session: crate::runtime::has_live_session(&p.name),
                // OAuth accounts carry a token; API accounts carry an api key. Either
                // one flips the Login row to "re-login" and shows the log-out row.
                // A console account's `log in` row captures the SESSION, so that
                // is the credential its done-state names. Its api key is a
                // different credential on a different row and cannot stand in:
                // a keyless account with a live session is logged in for this
                // row's purposes, and a keyed one with no session is not.
                logged_in: if p.console_login_target().is_some() {
                    p.console.is_some()
                } else if p.login_is_oauth() {
                    p.credentials.is_some()
                } else {
                    p.api_key.as_deref().is_some_and(|k| !k.trim().is_empty())
                },
                login_is_oauth: p.login_is_oauth(),
                has_other_login: p.credentials.is_some() || p.api_key.is_some(),
                clear_falls_back_to_oauth: p.credentials.is_some(),
                captured: false,
                provider: p.provider.map(|p| p.display_name()),
                console_login: p.console_login_target().is_some(),
                // ONE sidecar read per frame feeds both facts. The status is
                // derived from the same classification rather than a second
                // parse: Misfilled ⇔ `NotLongLived` (both mean "refresh token
                // present"), everything readable else is `LongLived` with its
                // recorded expiry, and an absent/corrupt sidecar is `None` on
                // both readers.
                session_token: sidecar.as_ref().map(|(kind, oauth)| match kind {
                    crate::claude::SidecarKind::Misfilled => {
                        crate::claude::SessionTokenStatus::NotLongLived
                    }
                    _ => crate::claude::SessionTokenStatus::LongLived(oauth.expires_at),
                }),
                rolling_token: matches!(&sidecar, Some((crate::claude::SidecarKind::Rolling, _))),
                rolling_armed: p.rolling_token,
                has_static_backup: crate::claude::has_static_backup(&p.name),
            }
        }
        None => Snap::blank("settings"),
    }
}

fn draw_settings(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let actions_focused = app.config_focus == ConfigFocus::Actions;
    let draft = app.config_draft.as_ref();
    let snap = build_snap(app, draft.is_none());

    // Profile names render verbatim; structural titles ("+ new account", "settings") stay uppercased.
    let is_profile_name = app.profile_cursor < app.config().profiles.len();
    let block = if is_profile_name {
        section_box_verbatim(&snap.title, actions_focused, false)
    } else {
        section_box(&snap.title, actions_focused, false)
    };
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let rows = config_rows(app);
    let cursor = app.config_action_cursor.min(rows.len().saturating_sub(1));

    draw_settings_rows(frame, inner, app, &rows, cursor, &snap, actions_focused);
}

/// CLA-SPLIT status row (`token`): a profile carrying a long-lived-token sidecar
/// runs its sessions on that static login, and the ~1yr horizon is the one thing
/// about it worth watching. A comfortable horizon is a plain accent value; the
/// last 30 days warn as a pill; expired and mis-filled escalate to a DANGER pill
/// plus a `└` fix line — matching the usage-tab diagnostic shape. Expired means
/// every switch would sign sessions out; a mis-filled sidecar (rotating pair,
/// split disengaged) means the operator thinks the split is armed and it isn't.
///
/// Returns the row line plus, for the two charged states, an always-on `└` fix
/// line (this row lives in the non-focusable header block, so the hint can't be
/// focus-gated — it renders like the usage-tab status hints). `width` sizes the
/// tooltip wrap.
fn session_token_lines(
    status: &crate::claude::SessionTokenStatus,
    rolling: bool,
    name: &str,
    now_ms: i64,
    width: usize,
) -> Vec<Line<'static>> {
    use crate::claude::SessionTokenStatus;
    let key = || Span::styled(key_cell("token", KEY_W, KEY_GUTTER), theme::label());
    let plain =
        |text: String, style: Style| vec![Line::from(vec![key(), Span::styled(text, style)])];
    let pill_row =
        |label: String, style: Style| Line::from([vec![key()], pill(label, style)].concat());
    // A charged state = pill row + a `└ fix` line (reuses the help-tooltip leader).
    let charged = |label: String, fix: &str| {
        let mut lines = vec![pill_row(label, theme::danger().bold())];
        lines.extend(help_tooltip_lines(fix, width));
        lines
    };

    match status {
        SessionTokenStatus::LongLived(Some(ms)) => {
            if now_ms >= *ms {
                if rolling {
                    charged(
                        "rolling token stalled".to_string(),
                        &format!(
                            "nothing re-stamped it before expiry · clauth rolling-token {name} re-arms"
                        ),
                    )
                } else {
                    charged("expired".to_string(), "re-mint with claude setup-token")
                }
            } else if rolling {
                // Hours-scale countdown, accent not warning: the daemon
                // re-stamps well inside this window. Counted to the RE-STAMP
                // (expiry minus the renewal horizon), not to the expiry — the
                // leg fires `ROLLING_RESTAMP_HORIZON_MS` ahead, so an
                // expiry-based label read 2h high everywhere except zero.
                let until_restamp = (ms - crate::oauth::ROLLING_RESTAMP_HORIZON_MS - now_ms).max(0);
                let label = if until_restamp < 3_600_000 {
                    "rolling · re-stamp due".to_string()
                } else {
                    format!("rolling · re-stamps in ~{}h", until_restamp / 3_600_000)
                };
                plain(label, theme::accent())
            } else {
                // Truncating division: an expiry inside the next 24h reads
                // "~0d" and still warns; only a past expiry (handled above) is
                // DANGER, so a sub-day-expired token no longer mislabels as
                // "~0d / warning" while the install gate already refuses it.
                let days = (ms - now_ms) / 86_400_000;
                if days <= 30 {
                    vec![pill_row(
                        format!("expires in ~{days}d"),
                        theme::warning().bold(),
                    )]
                } else {
                    plain(format!("long-lived · ~{days}d left"), theme::accent())
                }
            }
        }
        SessionTokenStatus::LongLived(None) => plain(
            if rolling {
                "rolling · no recorded expiry".to_string()
            } else {
                "long-lived · no recorded expiry".to_string()
            },
            theme::accent(),
        ),
        SessionTokenStatus::NotLongLived => charged(
            "mis-filled".to_string(),
            "sidecar has a refresh token, split is off",
        ),
    }
}

fn draw_settings_rows(
    frame: &mut Frame<'_>,
    inner: Rect,
    app: &App,
    rows: &[ConfigRow],
    cursor: usize,
    snap: &Snap,
    actions_focused: bool,
) {
    let draft = app.config_draft.as_ref();
    let editing = draft.and_then(|d| d.active);
    let armed_action = draft.and_then(|d| d.armed_action);

    // Derived from the base-url buffer so it tracks the draft live.
    let is_api = !row_input(draft, snap, ConfigRow::BaseUrl)
        .value
        .trim()
        .is_empty();
    let (type_value, type_style) = if is_api {
        ("api", theme::accent())
    } else {
        ("oauth", theme::accent())
    };

    let mut lines: Vec<Line<'static>> = Vec::new();

    // Status purity: an enabled account has no status worth a row, so the row
    // only exists while the account is disabled.
    if snap.disabled {
        let mut spans = vec![Span::styled(
            key_cell("status", KEY_W, KEY_GUTTER),
            theme::label(),
        )];
        spans.extend(pill(DIAG_DISABLED.to_string(), theme::dim().bold()));
        lines.push(Line::from(spans));
    }

    lines.push(Line::from(vec![
        Span::styled(key_cell("type", KEY_W, KEY_GUTTER), theme::label()),
        Span::styled(type_value, type_style),
    ]));

    // Provider row — only for recognised third-party providers. Hidden while a
    // draft empties the base-url buffer (`is_api` tracks the draft live).
    let provider_label = if is_api { snap.provider } else { None };
    if let Some(label) = provider_label {
        lines.push(Line::from(vec![
            Span::styled(key_cell("provider", KEY_W, KEY_GUTTER), theme::label()),
            Span::styled(label, theme::accent()),
        ]));
    }

    if let Some(status) = &snap.session_token {
        lines.extend(session_token_lines(
            status,
            snap.rolling_token,
            &snap.title,
            crate::usage::now_ms() as i64,
            inner.width as usize,
        ));
    }

    lines.push(Line::from(""));
    // Tracks the absolute line index + buffer + row of the active edit row for
    // cursor placement after rendering. The header block above is variable
    // (optional status + type + optional provider + optional session + blank),
    // so the row loop's base index is simply what has been pushed so far.
    let mut edit_caret: Option<(u16, InputState, ConfigRow)> = None;
    let mut line_idx: u16 = lines.len() as u16;

    // Start + end of the focused row's block (row plus its tooltip lines), so a
    // wrapped hint can't scroll off the bottom while its row stays visible.
    let mut focus = (0usize, 1usize);
    for (i, row) in rows.iter().enumerate() {
        let selected = actions_focused && i == cursor;
        let is_editing = editing == Some(*row);
        let input = row_input(draft, snap, *row);
        let line = detail_row(*row, selected, is_editing, armed_action, snap, &input);
        if is_editing {
            edit_caret = Some((line_idx, input, *row));
        }
        if selected || is_editing {
            focus.0 = line_idx as usize;
        }
        lines.push(if selected {
            highlight_row(line, inner.width as usize)
        } else {
            line
        });
        line_idx += 1;
        if selected
            && !is_editing
            && let Some(text) = row_hint(*row, snap)
        {
            let hint = help_tooltip_lines(&text, inner.width as usize);
            line_idx += hint.len() as u16;
            lines.extend(hint);
        }
        if selected || is_editing {
            focus.1 = line_idx as usize;
        }
    }

    // The row list outgrows a short terminal (env entries + model overrides are
    // unbounded), so it scrolls to the focused row rather than clipping its tail.
    let offset = draw_scrolled_lines(frame, inner, lines, focus);

    // Position the native terminal cursor at the caret when a text/model field is active.
    if let Some((ly, input, row)) = edit_caret
        && let Some(visible) = (ly as usize)
            .checked_sub(offset)
            .filter(|v| *v < inner.height as usize)
    {
        // x = "❯ " (2) + label block (row_label_cols: KEY_W+gutter, or key+gutter for a long env key) + caret cols
        let prefix_cols = 2 + row_label_cols(row, snap) + head_cols(&input);
        let cx = inner.x.saturating_add(prefix_cols as u16);
        let cy = inner.y.saturating_add(visible as u16);
        frame.set_cursor_position((cx, cy));
    }
}

/// Width of a row's label block (caret excluded) for native-cursor placement:
/// the shared key-cell width (`max(KEY_W, key.len()) + KEY_GUTTER`), mirroring
/// [`kv_field`] so the caret lands right after the gap.
fn row_label_cols(row: ConfigRow, snap: &Snap) -> usize {
    match row {
        ConfigRow::EnvEntry(i) => {
            let key_len = snap.env.get(i).map(|(k, _)| k.chars().count()).unwrap_or(0);
            KEY_W.max(key_len) + KEY_GUTTER
        }
        _ => KEY_W + KEY_GUTTER,
    }
}

/// The edit buffer for a row: the live draft buffer when present, else a
/// throwaway `InputState` seeded from the read-only [`Snap`]. Toggle/action rows
/// have no buffer and resolve to an empty one (never rendered as a field).
fn row_input(draft: Option<&ConfigDraft>, snap: &Snap, row: ConfigRow) -> InputState {
    draft
        .and_then(|d| d.field(row))
        .cloned()
        .unwrap_or_else(|| InputState::new(snap_value(snap, row)))
}

fn snap_value(snap: &Snap, row: ConfigRow) -> &str {
    match row {
        ConfigRow::Name => &snap.name,
        ConfigRow::BaseUrl => &snap.base_url,
        ConfigRow::ApiKey => &snap.api_key,
        ConfigRow::Model => &snap.model,
        ConfigRow::OpusModel => &snap.opus,
        ConfigRow::SonnetModel => &snap.sonnet,
        ConfigRow::HaikuModel => &snap.haiku,
        ConfigRow::FableModel => &snap.fable,
        ConfigRow::SubagentModel => &snap.subagent,
        ConfigRow::EnvEntry(i) => snap.env.get(i).map(|(_, v)| v.as_str()).unwrap_or(""),
        ConfigRow::AutoStart
        | ConfigRow::ModelOverrideAdd
        | ConfigRow::EnvAdd
        | ConfigRow::Login
        | ConfigRow::DeleteCreds
        | ConfigRow::ClearSessionToken
        | ConfigRow::Disabled
        | ConfigRow::Delete
        | ConfigRow::Create => "",
    }
}

/// Inline help for rows whose labels don't self-describe, phrased for the row's
/// current value so it re-explains itself as the value changes. `login_is_oauth`
/// (not the base-url buffer) picks the login/log-out wording — the copy has to
/// name what ⏎ really does — while `auto_start` / `base_url` flip on their own
/// value.
fn row_hint(row: ConfigRow, snap: &Snap) -> Option<String> {
    let api_login = !snap.login_is_oauth;
    let hint = match row {
        ConfigRow::BaseUrl if snap.base_url.trim().is_empty() => {
            "leave empty for a claude.ai account, or set an api endpoint"
        }
        ConfigRow::BaseUrl => "the api endpoint this account calls instead of claude.ai",
        ConfigRow::ApiKey => "api key sent to the endpoint above",
        // The value grammar (`space cycle · ↵ custom`) already lives in the footer.
        ConfigRow::Model => "default model for this account",
        ConfigRow::OpusModel => "what the opus alias resolves to (full model id)",
        ConfigRow::SonnetModel => "what the sonnet alias resolves to (full model id)",
        ConfigRow::HaikuModel => "what the haiku alias resolves to (full model id)",
        ConfigRow::FableModel => "what the fable alias resolves to (full model id)",
        ConfigRow::SubagentModel => "model forced for every subagent in this account",
        // No row or key drops the key itself — an emptied value saves as an
        // empty string — so the hint has to say where the entry actually goes.
        ConfigRow::EnvEntry(_) => "set while this account is active · an empty value keeps the key",
        ConfigRow::EnvAdd => "add an env var for this account",
        // Gate reasons name the same blockers as the CLI's own refusal copy
        // (`actions::disable_profile`), then the on/off state — checked in that
        // order since a gate can only ever bite the OFF (not-yet-disabled)
        // state. `live session` is the app-wide noun for a running `clauth
        // start`; the CLI's wording is its own.
        ConfigRow::Disabled if snap.is_active => {
            "the active account can't be disabled · switch away first"
        }
        ConfigRow::Disabled if snap.has_live_session => {
            "has a live session, close it before disabling"
        }
        ConfigRow::Disabled if snap.disabled => {
            "excluded from auto-switch, usage polling, and status until re-enabled"
        }
        ConfigRow::Disabled => {
            "removes this account from auto-switch, usage polling, and status until re-enabled"
        }
        ConfigRow::AutoStart if snap.auto_start => {
            "starts a throwaway session when idle so the 5h window counts"
        }
        ConfigRow::AutoStart => "never starts a session on its own",
        ConfigRow::ModelOverrideAdd => "pin what an alias resolves to, or force the subagent model",
        ConfigRow::Login if snap.console_login => {
            "opens the alibaba console to capture this account's usage session"
        }
        ConfigRow::Login if api_login => "re-enter the base url + api key for this account",
        ConfigRow::Login => "browser OAuth login; mints fresh tokens for this account",
        ConfigRow::DeleteCreds if api_login => {
            "clears the stored api key; keeps the account and its settings"
        }
        ConfigRow::DeleteCreds => {
            "clears the stored OAuth login; keeps the account and its settings"
        }
        // Gate reason first (same order as `Disabled` above — a gate only ever
        // bites the clearable state), then what the clear does from here. The
        // active account's wording names the relink, since that is the half a
        // running session feels. Both halves then split again on what the clear
        // falls back TO: an api-key account has no login to install, so it is
        // signed out rather than relinked — and the FULL scope of the clear is
        // spelled out below, because this hint is the TUI's entire disclosure
        // that re-stamping stops and the preserved mint goes too (the CLI
        // prints two explicit lines for the same act, and a two-press arm is
        // not a disclosure).
        //
        // The gate arm is `Snap::clear_gated` — the same judgment
        // `run_config_row` makes and `detail_row` dims on: a flag-only account
        // (armed, nothing stamped, no preserved mint) disarms without
        // stripping a credential, so it skips past — a gate line over a row
        // that acts would be the one lie a hint can tell.
        ConfigRow::ClearSessionToken if snap.clear_gated() => "no other login stored, log in first",
        // The flag-only state with NO login gets its own lines rather than the
        // 4-way base below: those arms were written under the old invariant
        // that "no OAuth login" implies an api key behind it, and promising an
        // api key this account does not hold would be the same lie in a
        // different tense. Active still names the sign-out: the relink onto an
        // absent install source removes the live slot.
        ConfigRow::ClearSessionToken if !snap.has_other_login && snap.is_active => {
            "stops the daemon re-stamping this account · signs Claude Code out · nothing is \
             stored behind it"
        }
        ConfigRow::ClearSessionToken if !snap.has_other_login => {
            "stops the daemon re-stamping this account · nothing else is stored"
        }
        ConfigRow::ClearSessionToken => {
            let base = match (snap.is_active, snap.clear_falls_back_to_oauth) {
                (true, true) => "relinks this account's own login now · running sessions follow",
                (true, false) => "signs Claude Code out now · this account runs on its api key",
                (false, true) => "the next switch installs this account's own login again",
                (false, false) => "the next switch runs this account on its api key",
            };
            let mut hint = base.to_string();
            if snap.rolling_armed || snap.rolling_token {
                hint.push_str(" · re-stamping stops");
            }
            if snap.has_static_backup {
                hint.push_str(" · the preserved mint goes too");
            }
            return Some(hint);
        }
        ConfigRow::Delete => {
            "deletes the account and everything stored for it, usage history included"
        }
        ConfigRow::Name | ConfigRow::Create => return None,
    };
    Some(hint.to_string())
}

fn detail_row(
    row: ConfigRow,
    selected: bool,
    editing: bool,
    armed_action: Option<ConfigRow>,
    snap: &Snap,
    input: &InputState,
) -> Line<'static> {
    let arrow = if editing {
        Span::styled(format!("{} ", theme::edit_glyph()), theme::accent().bold())
    } else if selected {
        Span::styled("❯ ", theme::accent().bold())
    } else {
        Span::raw("  ")
    };
    match row {
        ConfigRow::Name => kv_field(arrow, "name", input, editing, selected, false),
        ConfigRow::BaseUrl => kv_field(arrow, "base url", input, editing, selected, false),
        ConfigRow::ApiKey => kv_field(arrow, "api key", input, editing, selected, true),
        // Hybrid: the alias cycle at rest, a plain text field while typing a custom id.
        ConfigRow::Model if !editing => model_cycle_line(arrow, &input.value, selected),
        ConfigRow::Model => kv_field(arrow, "model", input, editing, selected, false),
        ConfigRow::OpusModel => kv_field(arrow, "opus", input, editing, selected, false),
        ConfigRow::SonnetModel => kv_field(arrow, "sonnet", input, editing, selected, false),
        ConfigRow::HaikuModel => kv_field(arrow, "haiku", input, editing, selected, false),
        ConfigRow::FableModel => kv_field(arrow, "fable", input, editing, selected, false),
        ConfigRow::SubagentModel => kv_field(arrow, "subagent", input, editing, selected, false),
        // A custom env entry: its key is the label; mask the value when the key
        // looks like a credential (mirrors the api-key row).
        ConfigRow::EnvEntry(i) => {
            let key = snap.env.get(i).map(|(k, _)| k.clone()).unwrap_or_default();
            let mask = env_key_is_secret(&key);
            kv_field(arrow, &key, input, editing, selected, mask)
        }
        // While editing, the typed text is the new key; at rest, the add chip.
        ConfigRow::EnvAdd if editing => kv_field(arrow, "key", input, editing, selected, false),
        ConfigRow::EnvAdd => Line::from(vec![
            arrow,
            Span::styled("+ add env", bold_when(theme::accent(), selected)),
        ]),
        ConfigRow::ModelOverrideAdd => Line::from(vec![
            arrow,
            Span::styled("+ model override", bold_when(theme::accent(), selected)),
        ]),
        // Same button CLASS as `Delete`: a state-flipped label, not a
        // key/value toggle. Disabling has real operational impact (drops the
        // account from auto-switch/polling/status mid-flight), so it renders
        // DANGER and always-bold, and arms on the first ⏎ like `Delete`
        // ("press again to disable"). Enabling is harmless and immediate, so
        // it takes the accent, bold-on-select treatment shared with
        // `Login`/`Create` instead. Dimmed/inert while active or a live session
        // is open — cloudy-tui disabled row (mirrors the Fallback tab's `max
        // spend`): the whole row renders faint and the key handler no-ops
        // (`run_config_row`'s gate in `app.rs`).
        ConfigRow::Disabled => {
            let gated = snap.is_active || snap.has_live_session;
            let row_arrow = if gated && selected {
                Span::styled("❯ ", theme::faint())
            } else {
                arrow
            };
            let (label, style) = if gated {
                let label = if snap.disabled {
                    "enable account"
                } else {
                    "disable account"
                };
                (label.to_string(), theme::faint())
            } else if snap.disabled {
                (
                    "enable account".to_string(),
                    bold_when(theme::accent(), selected),
                )
            } else if armed_action == Some(ConfigRow::Disabled) {
                ("press again to disable".to_string(), theme::danger().bold())
            } else {
                ("disable account".to_string(), theme::danger().bold())
            };
            Line::from(vec![row_arrow, Span::styled(label, style)])
        }
        ConfigRow::AutoStart => {
            let (value, style) = if snap.auto_start {
                (theme::toggle_on().to_string(), theme::accent())
            } else {
                (theme::toggle_off().to_string(), theme::faint())
            };
            kv_static(arrow, "auto-start", value, style, selected)
        }
        ConfigRow::Delete => {
            let label = if armed_action == Some(ConfigRow::Delete) {
                "press again to delete".to_string()
            } else {
                "delete account".to_string()
            };
            Line::from(vec![arrow, Span::styled(label, theme::danger().bold())])
        }
        ConfigRow::Create => Line::from(vec![
            arrow,
            Span::styled("create account", bold_when(theme::accent(), selected)),
        ]),
        ConfigRow::Login => {
            // A draft-held mint renders the done state; ⏎ re-runs the login but
            // confirms first before replacing the stash.
            if snap.captured {
                Line::from(vec![
                    arrow,
                    Span::styled("✓ logged in", bold_when(theme::success(), selected)),
                ])
            } else {
                let label = if snap.logged_in {
                    "re-login"
                } else {
                    "+ login"
                };
                Line::from(vec![
                    arrow,
                    Span::styled(label, bold_when(theme::accent(), selected)),
                ])
            }
        }
        ConfigRow::DeleteCreds => {
            Line::from(vec![arrow, Span::styled("log out", theme::danger().bold())])
        }
        // `Delete`'s button class — always-bold DANGER, `press again to <verb>`
        // once armed — because a clear retargets every future switch and moves
        // the active account's live credentials. Dimmed/inert (arrow included,
        // matching the gated `disabled` row) when clearing would strip the
        // account's last credential: `run_config_row`'s own gate no-ops there,
        // and `Snap::clear_gated` is the one spelling all three surfaces share.
        ConfigRow::ClearSessionToken => {
            let gated = snap.clear_gated();
            let row_arrow = if gated && selected {
                Span::styled("❯ ", theme::faint())
            } else {
                arrow
            };
            let (label, style) = if gated {
                ("clear long-lived token", theme::faint())
            } else if armed_action == Some(ConfigRow::ClearSessionToken) {
                ("press again to clear", theme::danger().bold())
            } else {
                ("clear long-lived token", theme::danger().bold())
            };
            Line::from(vec![row_arrow, Span::styled(label, style)])
        }
    }
}

fn kv_field(
    arrow: Span<'static>,
    key: &str,
    input: &InputState,
    editing: bool,
    focused: bool,
    mask_value: bool,
) -> Line<'static> {
    let mut spans = vec![
        arrow,
        Span::styled(key_cell(key, KEY_W, KEY_GUTTER), label_style(focused)),
    ];
    spans.extend(value_spans(input, editing, mask_value));
    Line::from(spans)
}

fn kv_static(
    arrow: Span<'static>,
    key: &str,
    value: String,
    value_style: Style,
    focused: bool,
) -> Line<'static> {
    Line::from(vec![
        arrow,
        Span::styled(key_cell(key, KEY_W, KEY_GUTTER), label_style(focused)),
        Span::styled(value, value_style),
    ])
}

/// Mask a custom env value when its key names a credential (mirrors the api-key row).
fn env_key_is_secret(key: &str) -> bool {
    let upper = key.to_ascii_uppercase();
    ["KEY", "TOKEN", "SECRET", "AUTH"]
        .iter()
        .any(|needle| upper.contains(needle))
}

fn value_spans(input: &InputState, editing: bool, mask_value: bool) -> Vec<Span<'static>> {
    if !editing {
        if input.value.is_empty() {
            return vec![Span::styled("—", theme::faint())];
        }
        let display = if mask_value {
            "••••••••".to_string()
        } else {
            input.value.clone()
        };
        return vec![Span::styled(display, theme::accent())];
    }
    // In edit mode the terminal cursor (set via frame.set_cursor_position) owns
    // the caret glyph — no simulated block highlight needed.
    let body = Style::default()
        .fg(theme::text_color())
        .bg(theme::bg_sunken());
    vec![Span::styled(input.value.clone(), body)]
}

/// The `model` row at rest: a segmented alias control (`default` + presets).
/// The active option is `ACCENT` and wraps in `[]` only while the row is the
/// cursor (the row widens by 2 on focus — the Config-tab focus cue); the rest
/// stay bare `TEXT_FAINT`. A custom id (set via ⏎) matches no preset, so the
/// real value is appended in `ACCENT` rather than mis-bracketing the nearest
/// alias.
fn model_cycle_line(arrow: Span<'static>, current: &str, selected: bool) -> Line<'static> {
    let mut spans = vec![
        arrow,
        Span::styled(key_cell("model", KEY_W, KEY_GUTTER), label_style(selected)),
    ];
    let mut options: Vec<(&str, bool)> = vec![("default", current.is_empty())];
    options.extend(MODEL_PRESETS.iter().map(|p| (*p, *p == current)));
    for (i, (label, active)) in options.iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw("  "));
        }
        spans.push(cycle_option(label, *active, selected));
    }
    if !current.is_empty() && !MODEL_PRESETS.contains(&current) {
        spans.push(Span::styled(format!("   {current}"), theme::accent()));
    }
    Line::from(spans)
}

#[cfg(test)]
#[path = "../../../tests/inline/tui_render_config.rs"]
mod tests;
