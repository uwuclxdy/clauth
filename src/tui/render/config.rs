//! Setup tab — account picker (plus a trailing `+ new` row) on the left, the
//! selected account's settings on the right. Editing happens inline in the
//! right pane: ⏎ on the left drops focus into the detail rows, ⏎ on a text row
//! opens an inline caret, ⏎ on a toggle flips it, and `+ new` turns the right
//! pane into a create form. No popups.

use ratatui::Frame;
use ratatui::layout::{Position, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};

use crate::profile_cache::{ACCOUNT_ID_CACHE_FILE, PROFILE_FETCHED_CACHE_FILE, load_profile_cache};

use super::super::app::{
    App, BandEntry, CodexDetailRow, ConfigDraft, ConfigFocus, ConfigRow, DraftLogin, HarnessFilter,
    InputState, MODEL_PRESETS, MainItemKind, PoolDetailRow, PoolMutationKind, ReadingsSnapshot,
    SetupSelection, config_rows, escape_control, new_form_draft, pool_auth_label,
};
use super::super::theme;
use super::format::{NO_DATA, middle_truncate, spinner_frame, truncate};
use super::panes::{
    DETAIL_KEY_GUTTER, DETAIL_KEY_W, DIAG_DISABLED, ListView, MIN_VALUE_W, bold_when,
    cycle_row_lines, draw_following_list, draw_scrolled_lines, head_cols, help_tooltip_lines,
    highlight_row, invalid_tooltip_lines, key_cell, label_style, master_detail, name_color,
    picker_row, pill, place_caret, section_box, section_box_verbatim, toggle_value, value_caret,
};
use super::prose::{cmd, key_lit, spans as prose_spans};

pub(super) fn draw(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &App,
    readings: &ReadingsSnapshot,
    toast_rects: &[Rect],
) {
    // +1 for the trailing `+ new` picker row.
    let items = app.setup_items().len() + 1;
    let (selector, settings) = master_detail(area, items);

    let profiles_focused = app.config_focus == ConfigFocus::Profiles;
    draw_selector(frame, selector, app, profiles_focused);
    draw_settings(frame, settings, app, readings, toast_rects);
}

fn draw_selector(frame: &mut Frame<'_>, area: Rect, app: &App, focused: bool) {
    draw_band_selector(
        frame,
        area,
        app,
        focused,
        app.setup_position(),
        true,
        &app.setup_selector,
    );
}

/// The shared band-list selector, Setup's shape: the [`App::band_entries`]
/// walk under the all-filter, each harness row rendered via
/// [`setup_selector_row`], then — when `with_new` — the trailing `+ new` row.
/// Setup passes `+ new`; Usage passes none. `selected_flat` is the flat
/// [`App::setup_items`] index of the selection, its `len()` the `+ new` row
/// when `with_new`.
pub(super) fn draw_band_selector(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &App,
    focused: bool,
    selected_flat: usize,
    with_new: bool,
    list: &ListView,
) {
    let block = section_box("accounts", focused, true, area.width);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    // Compute the band walk, the flat item list and the cursor position BEFORE
    // the config guard: all three read the profile count under their own short
    // guard, and `RankedMutex` refuses a nested acquisition of the same rank.
    let entries = app.band_entries(HarnessFilter::All);
    let items = app.setup_items();
    let pos = selected_flat;
    let cfg = app.config();
    let ranges = App::heading_ranges(&entries);
    let width = inner.width;

    // One pass over the shared band walk builds both the rendered rows and the
    // cursor-indexed starts, so a heading or spacer never shifts a selection
    // off its row. `pos` is the flat item index, `items.len()` the `+ new`
    // row — the clamped form would double-select the last item beside `+ new`.
    let mut rows: Vec<ratatui::widgets::ListItem<'static>> =
        Vec::with_capacity(items.len() + usize::from(with_new) + 8);
    let mut starts: Vec<usize> = Vec::with_capacity(items.len() + usize::from(with_new));
    let mut flat: usize = 0;
    for (i, entry) in entries.iter().enumerate() {
        match entry {
            BandEntry::Spacer => {
                rows.push(ratatui::widgets::ListItem::new(Line::from("")));
            }
            BandEntry::Heading(text) => {
                let heading = if ranges
                    .iter()
                    .any(|&(hi, start, end)| hi == i && (start..end).contains(&pos))
                {
                    theme::label_active()
                } else {
                    theme::label()
                };
                rows.push(ratatui::widgets::ListItem::new(Line::from(prose_spans(
                    &format!("  {text}"),
                    heading,
                ))));
            }
            BandEntry::Item(item) => {
                starts.push(rows.len());
                rows.push(ratatui::widgets::ListItem::new(setup_selector_row(
                    app,
                    &cfg,
                    *item,
                    flat == pos,
                    focused,
                    width,
                )));
                flat += 1;
            }
        }
    }
    let new_row = rows.len();
    if with_new {
        rows.push(ratatui::widgets::ListItem::new(picker_row(
            pos == items.len(),
            focused,
            "+ new".to_string(),
            theme::accent(),
            width,
        )));
    }

    let render_sel = if with_new && pos >= items.len() {
        new_row
    } else {
        starts.get(pos).copied().unwrap_or(0)
    };
    // The `+ new` row joins the starts list so the page step treats it as the
    // last selectable row (a PageDown at `+ new` stays, not clamps to the last
    // item).
    if with_new {
        starts.push(new_row);
    }
    draw_following_list(frame, inner, rows, render_sel, list);
    let block_ends: Vec<usize> = starts.iter().map(|s| s + 1).collect();
    list.starts.replace(starts);
    list.block_ends.replace(block_ends);
}

/// One Setup selector row for a harness item: a claude row (dim disabled,
/// active-orange), a codex row (its name, `×` when quarantined), or a pool row
/// (its name, dim when paused/disabled, `×` when it needs a re-login). Headings
/// and the `+ new` row are never items.
fn setup_selector_row(
    app: &App,
    cfg: &crate::profile::AppConfig,
    item: MainItemKind,
    selected: bool,
    focused: bool,
    width: u16,
) -> Line<'static> {
    match item {
        MainItemKind::Profile(i) => {
            let Some(p) = cfg.profiles.get(i) else {
                return Line::from("");
            };
            // A disabled account can never be active, so dim wins outright.
            let ns = if p.is_disabled() {
                theme::dim()
            } else {
                name_color(cfg.is_active(&p.name))
            };
            picker_row(selected, focused, p.name.to_string(), ns, width)
        }
        MainItemKind::Codex(i) => {
            let Some(row) = app.codex_rows.get(i) else {
                return Line::from("");
            };
            let ns = theme::base();
            let name = if row.broken {
                format!("{} {}", theme::dead_credential_glyph(), row.name)
            } else {
                row.name.to_string()
            };
            picker_row(selected, focused, name, ns, width)
        }
        MainItemKind::Shunt(pi, ai) => {
            let Some(provider) = app.shunt_pool_row(pi, ai) else {
                return Line::from("");
            };
            let ns = if provider.0.paused || provider.0.disabled {
                theme::dim()
            } else {
                theme::base()
            };
            // Names off the wire are escaped before render, the Overview's rule.
            let account = escape_control(&provider.0.name);
            let name = if provider.0.needs_relogin {
                format!("{} {account}", theme::dead_credential_glyph())
            } else {
                account
            };
            picker_row(selected, focused, name, ns, width)
        }
    }
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
    advisor: String,
    /// Sorted `(key, value)` custom env entries — one `EnvEntry` row each.
    env: Vec<(String, String)>,
    auto_start: bool,
    usage_probe: bool,
    /// `Profile::usage_probe_interval_ms` in seconds, as the `probe every`
    /// buffer spells it.
    usage_probe_interval: String,
    /// `Profile::bell_threshold` as the `alert at` buffer spells it; empty = off.
    bell_threshold: String,
    /// `Profile::is_disabled` — drives the `disabled` row's toggle value.
    disabled: bool,
    /// The global active profile — one of the two gates (mirroring
    /// `actions::disable_profile`'s own gate) that dims the `disabled` row inert.
    is_active: bool,
    /// A live `clauth start` session — the other gate.
    has_live_session: bool,
    /// Whether the profile holds a stored credential — the OAuth token or, for an
    /// API account, the api key. Drives the `Login` row's re-login vs first-login
    /// label.
    logged_in: bool,
    /// Whether the profile's `credentials.json` is unreadable: flips the api-key
    /// log-out hint to name the file the log out also deletes.
    has_unreadable_store: bool,
    /// Whether the stored api key is non-blank: read off the profile, not the
    /// `api_key` text field, which a draft-open snap leaves empty.
    has_api_key: bool,
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
    /// `+ new` form only: the draft holds a login stash awaiting `create
    /// account`, flipping its row to the ✓ done state. `captured` is `+
    /// login`'s mint (oauth mode only, mirroring the consume rule);
    /// `captured_live` is `+ capture current login`'s snapshot.
    captured: bool,
    captured_live: bool,
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
    /// The server refused the account's setup token: a dead login whatever
    /// the token's expiry says, so the `token` row names the refusal and its
    /// re-mint fix.
    token_rejected: bool,
    /// Whether a preserved mint backup sits beside the sidecar — the clear
    /// row's disclosure that a second, restorable credential goes with it.
    has_static_backup: bool,
    /// The account's email off its cached `/profile` plan, control characters
    /// escaped; `None` until `/profile` has reported one for the login the
    /// account holds now (`login_email`).
    email: Option<String>,
    /// `+ new` form only: the draft mints a client token on `create account`
    /// (the picker's `+ new profile` seeded it and the base url still names
    /// the gateway). Turns the api key row read-only `minted on save`.
    mint_armed: bool,
    /// `+ new` form only: the `type` cycle row's current pick.
    account_type: crate::tui::app::NewAccountType,
    /// `+ new` form only: the `family` cycle row's current pick.
    family: crate::gateway_admin::PoolFamily,
    /// `+ new` form only: the codex `+ login` mint is staged.
    captured_codex_mint: bool,
    /// `+ new` form only: the codex `+ capture current login` marker is staged.
    captured_codex_capture: bool,
    /// `+ new` form only: a refusal on the name row's `└` line (a codex name
    /// held by either roster), stored on the draft.
    name_refusal: Option<String>,
    /// `+ new` form only: a refusal on the create row's `└` line (a codex
    /// create with nothing staged), stored on the draft.
    create_refusal: Option<String>,
    /// `+ new` form only: the shunt `+ login`'s disabled reason, `Some` while
    /// the add is unavailable. A read-error rung carries the error text, so it
    /// is a `String`, not a `&'static str`.
    login_disabled_reason: Option<String>,
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
            advisor: String::new(),
            env: Vec::new(),
            auto_start: false,
            usage_probe: false,
            usage_probe_interval: String::new(),
            bell_threshold: String::new(),
            disabled: false,
            is_active: false,
            has_live_session: false,
            logged_in: false,
            has_unreadable_store: false,
            has_api_key: false,
            login_is_oauth: true,
            has_other_login: false,
            clear_falls_back_to_oauth: false,
            captured: false,
            captured_live: false,
            provider: None,
            console_login: false,
            session_token: None,
            rolling_token: false,
            rolling_armed: false,
            token_rejected: false,
            has_static_backup: false,
            email: None,
            mint_armed: false,
            account_type: crate::tui::app::NewAccountType::Claude,
            family: crate::gateway_admin::PoolFamily::Claude,
            captured_codex_mint: false,
            captured_codex_capture: false,
            name_refusal: None,
            create_refusal: None,
            login_disabled_reason: None,
        }
    }
}

fn build_snap(app: &App, readings: &ReadingsSnapshot, with_text: bool) -> Snap {
    let text = |s: &Option<String>| {
        if with_text {
            s.clone().unwrap_or_default()
        } else {
            String::new()
        }
    };
    // Resolve the selection BEFORE taking the config guard: `setup_selection`
    // reads the profile count under its own short guard, and `RankedMutex`
    // refuses a nested acquisition of the same rank. The disabled reason and
    // the `+ new` draft too: `shunt_login_disabled` re-runs `setup_selection`
    // for its type check, and `new_form_draft`'s fallback branch re-runs it.
    let selection = app.setup_selection();
    let login_disabled_reason = app.shunt_login_disabled();
    let new_draft = new_form_draft(app);
    // The refused logins too: the set takes the config guard itself.
    let key_rejected = app.key_rejected_names();
    let cfg = app.config();
    let profile_idx = match selection {
        SetupSelection::Item(MainItemKind::Profile(i)) => i,
        _ => {
            let mut snap = Snap::blank("+ new account");
            let stash = new_draft.and_then(|d| d.captured_login.as_ref());
            // Mirror commit_new_account's consume rule: a typed base url flips the
            // form to API mode and the mint will be discarded, so no stale ✓. The
            // live-login stash survives into api mode, so its ✓ tracks the stash
            // alone.
            let oauth_mode = new_draft.is_some_and(|d| d.base_url.value.trim().is_empty());
            snap.captured = oauth_mode && stash.is_some_and(|s| matches!(s, DraftLogin::Mint(_)));
            snap.captured_live = stash.is_some_and(|s| matches!(s, DraftLogin::LiveLogin(_)));
            snap.captured_codex_mint = stash.is_some_and(|s| matches!(s, DraftLogin::CodexMint(_)));
            snap.captured_codex_capture =
                stash.is_some_and(|s| matches!(s, DraftLogin::CodexCapture));
            snap.mint_armed = new_draft.is_some_and(ConfigDraft::mint_armed);
            if let Some(d) = new_draft {
                // The at-rest preview draws the suspended draft as it is (a
                // switch suspends what it leaves); descended, the live buffer
                // renders and the snap carries no text.
                snap.name = if with_text {
                    d.name.value.clone()
                } else {
                    String::new()
                };
                snap.account_type = d.account_type;
                snap.family = d.family;
                snap.name_refusal = d.name_refusal.clone();
                snap.create_refusal = d.create_refusal.clone();
            }
            // The disabled reason reads off the tick's cache (and this form's
            // own add, which keeps the row live), never a disk read on the
            // render path; resolved before the guard above.
            snap.login_disabled_reason = login_disabled_reason;
            return snap;
        }
    };
    match cfg.profiles.get(profile_idx) {
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
                advisor: text(&p.models.advisor),
                // Env rows render from the snapshot (no per-entry draft buffer), so
                // they're always populated — even while a draft owns the text fields.
                env: p.env.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
                auto_start: p.auto_start,
                usage_probe: p.usage_probe,
                usage_probe_interval: (p.usage_probe_interval_ms / 1000).to_string(),
                bell_threshold: p
                    .bell_threshold
                    .map(crate::tui::app::format_weekly_pct)
                    .unwrap_or_default(),
                disabled: p.is_disabled(),
                is_active: cfg.is_active(&p.name),
                // The marker probe reads fresh for the selected profile only
                // (same as `session_token` below), but the job-store walk and
                // the handed-off set it implies come from the per-tick
                // snapshot (`app.job_trees` / `app.handed_off`) — a frame pays
                // a marker probe and set lookups, never a store or registry
                // walk.
                has_live_session: crate::runtime::has_live_session_in(
                    &p.name,
                    &app.job_trees,
                    &app.handed_off,
                ),
                // OAuth accounts carry a token; API accounts carry an api key. Either
                // one flips the Login row to "re-login".
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
                has_unreadable_store: p.unreadable_credentials_store.is_some(),
                has_api_key: p.api_key.as_deref().is_some_and(|k| !k.trim().is_empty()),
                login_is_oauth: p.login_is_oauth(),
                has_other_login: p.credentials.is_some() || p.api_key.is_some(),
                clear_falls_back_to_oauth: p.credentials.is_some(),
                captured: false,
                captured_live: false,
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
                token_rejected: key_rejected.contains(&p.name)
                    && crate::usage::dead_login_is_setup_token(p),
                has_static_backup: crate::claude::has_static_backup(&p.name),
                email: login_email(p, readings.usage_of(&p.name).and_then(|u| u.plan.as_ref())),
                mint_armed: false,
                account_type: crate::tui::app::NewAccountType::Claude,
                family: crate::gateway_admin::PoolFamily::Claude,
                captured_codex_mint: false,
                captured_codex_capture: false,
                name_refusal: None,
                create_refusal: None,
                login_disabled_reason: None,
            }
        }
        None => Snap::blank("settings"),
    }
}

/// The cached `/profile` address, only while it still belongs to the account's
/// login: the account holds an OAuth login, the address was read for the
/// account its identity anchor names (a login install rewrites or removes the
/// anchor), and no copy-back has replaced the login since the last `/profile`
/// attempt (a copy-back keeps the anchor and removes the durable stamp until
/// the next read re-anchors). A plan carried over from an earlier login, here
/// or in another process, names no one. Read per frame for the selected
/// profile only (two small files).
fn login_email(
    p: &crate::profile::Profile,
    plan: Option<&crate::usage::PlanInfo>,
) -> Option<String> {
    p.credentials.as_ref()?.claude_ai_oauth.as_ref()?;
    let email = plan?.email.as_ref()?;
    load_profile_cache::<u64>(&p.name, PROFILE_FETCHED_CACHE_FILE)?;
    let anchor = load_profile_cache::<crate::profile::AccountId>(&p.name, ACCOUNT_ID_CACHE_FILE)?;
    (email.account == anchor).then(|| escape_control(&email.address))
}

fn draw_settings(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &App,
    readings: &ReadingsSnapshot,
    toast_rects: &[Rect],
) {
    // The codex and pool detail panes render their own rows and actions.
    match app.setup_selection() {
        SetupSelection::Item(MainItemKind::Codex(_)) => {
            draw_codex_detail(frame, area, app);
            return;
        }
        SetupSelection::Item(MainItemKind::Shunt(..)) => {
            draw_pool_detail(frame, area, app);
            return;
        }
        _ => {}
    }
    let actions_focused = app.config_focus == ConfigFocus::Actions;
    let draft = app.config_draft.as_ref();
    let snap = build_snap(app, readings, draft.is_none());

    // Profile names render verbatim; structural titles ("+ new account", "settings") stay uppercased.
    let is_profile_name = matches!(
        app.setup_selection(),
        SetupSelection::Item(MainItemKind::Profile(_))
    );
    let block = if is_profile_name {
        section_box_verbatim(&snap.title, actions_focused, false, area.width)
    } else {
        section_box(&snap.title, actions_focused, false, area.width)
    };
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let rows = config_rows(app);
    let cursor = app.config_action_cursor.min(rows.len().saturating_sub(1));

    draw_settings_rows(
        frame,
        inner,
        app,
        &rows,
        cursor,
        &snap,
        SettingsRowsTail {
            actions_focused,
            toast_rects,
        },
    );
}

/// The codex detail pane: `type`/`plan` (and `login  [ needs re-login ]` only
/// while quarantined) as static key rows, then the `re-login` and `delete
/// account` action rows. A codex account is deliberately NOT a `Profile` (see
/// [`CodexRow`]), so this is its own renderer, never the claude detail.
fn draw_codex_detail(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let actions_focused = app.config_focus == ConfigFocus::Actions;
    let Some(row) = app.setup_codex_index().and_then(|i| app.codex_rows.get(i)) else {
        return;
    };
    let block = section_box_verbatim(row.name.as_ref(), actions_focused, false, area.width);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let mut lines: Vec<Line<'static>> = Vec::new();
    lines.push(Line::from(vec![
        Span::styled(
            key_cell("type", DETAIL_KEY_W, DETAIL_KEY_GUTTER),
            theme::label(),
        ),
        Span::styled("codex", theme::accent()),
    ]));
    lines.push(Line::from(vec![
        Span::styled(
            key_cell("plan", DETAIL_KEY_W, DETAIL_KEY_GUTTER),
            theme::label(),
        ),
        match row.plan.as_deref() {
            Some(plan) => Span::styled(escape_control(plan), theme::accent()),
            None => Span::styled(NO_DATA.to_string(), theme::faint()),
        },
    ]));
    // The quarantine is read live for the selected account only (one small
    // file), the claude detail's live `has_live_session`/`session_token` read
    // precedent: the `login [ needs re-login ]` row must not lag a verdict
    // planted since the last 1 s codex snapshot refresh.
    if crate::codex_auth::read_quarantine(row.name.as_str()).is_some() {
        lines.push(Line::from(
            [
                vec![Span::styled(
                    key_cell("login", DETAIL_KEY_W, DETAIL_KEY_GUTTER),
                    theme::label(),
                )],
                pill("needs re-login".to_string(), theme::danger().bold()),
            ]
            .concat(),
        ));
    }
    lines.push(Line::from(""));

    let rows = CodexDetailRow::ALL;
    let cursor = app.config_action_cursor.min(rows.len().saturating_sub(1));
    let mut starts = Vec::with_capacity(rows.len());
    let mut focus = (lines.len(), lines.len() + 1);
    for (i, row_kind) in rows.iter().enumerate() {
        let selected = actions_focused && i == cursor;
        starts.push(lines.len());
        let arrow = if selected {
            Span::styled("❯ ", theme::accent().bold())
        } else {
            Span::raw("  ")
        };
        let (label, style) = match row_kind {
            CodexDetailRow::ReLogin => {
                ("re-login".to_string(), bold_when(theme::accent(), selected))
            }
            CodexDetailRow::Delete => {
                let label = if app.codex_detail_armed {
                    "press again to delete".to_string()
                } else {
                    "delete account".to_string()
                };
                (label, theme::danger().bold())
            }
        };
        if selected {
            focus.0 = lines.len();
        }
        let line = Line::from(vec![arrow, Span::styled(label, style)]);
        // The focused action row takes the claude detail's full-width tint.
        lines.push(if selected {
            highlight_row(line, inner.width as usize)
        } else {
            line
        });
        if selected {
            focus.1 = lines.len();
        }
    }

    app.setup_detail_viewport.set(inner.height as usize);
    app.setup_detail_total.set(lines.len());
    app.setup_detail_row_starts.replace(starts);
    app.setup_detail_block_ends
        .replace(rows.iter().map(|_| 1).collect::<Vec<_>>());
    app.setup_detail_tips.replace(vec![0; rows.len()]);
    draw_scrolled_lines(frame, inner, lines, focus, Some(&app.setup_detail_offset));
}

/// The pool detail pane: static `type`/`family`/`provider`/`plan` rows, one
/// `status` pill (first that holds: needs re-login, paused, disabled, cooldown;
/// none while available), then the action rows — `refresh` (claude and
/// antigravity) and `remove` (every routed family), none for kimi or an
/// unknown auth mode.
fn draw_pool_detail(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let actions_focused = app.config_focus == ConfigFocus::Actions;
    let Some((pi, ai)) = app.setup_pool_pair() else {
        return;
    };
    let Some((account, auth)) = app.shunt_pool_row(pi, ai) else {
        return;
    };
    let title = escape_control(&account.name);
    let block = section_box_verbatim(&title, actions_focused, false, area.width);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let mut lines: Vec<Line<'static>> = Vec::new();
    lines.push(Line::from(vec![
        Span::styled(
            key_cell("type", DETAIL_KEY_W, DETAIL_KEY_GUTTER),
            theme::label(),
        ),
        Span::styled("shunt", theme::accent()),
    ]));
    lines.push(Line::from(vec![
        Span::styled(
            key_cell("family", DETAIL_KEY_W, DETAIL_KEY_GUTTER),
            theme::label(),
        ),
        Span::styled(pool_auth_label(auth), theme::accent()),
    ]));
    let providers = app.pool_providers_for(auth, &account.name);
    if !providers.is_empty() {
        // The value column opens after the key cell; a join wider than it ends
        // in `…` (grapheme-safe), never a mid-word cut.
        let value_width = inner
            .width
            .saturating_sub((DETAIL_KEY_W + DETAIL_KEY_GUTTER) as u16)
            as usize;
        lines.push(Line::from(vec![
            Span::styled(
                key_cell("provider", DETAIL_KEY_W, DETAIL_KEY_GUTTER),
                theme::label(),
            ),
            Span::styled(
                truncate(&providers.join(" · "), value_width),
                theme::accent(),
            ),
        ]));
    }
    lines.push(Line::from(vec![
        Span::styled(
            key_cell("plan", DETAIL_KEY_W, DETAIL_KEY_GUTTER),
            theme::label(),
        ),
        match account.plan.as_deref() {
            Some(plan) => Span::styled(escape_control(plan), theme::accent()),
            None => Span::styled(NO_DATA.to_string(), theme::faint()),
        },
    ]));
    // The status pill is the first that holds; none while the account is
    // available, so a clean row carries no status word. The Usage pool pane
    // shares this one judgment via [`pool_status_pill`], which also hands back
    // the cooldown countdown the Setup detail deliberately omits.
    if let Some((label, style, _)) = pool_status_pill(account) {
        lines.push(Line::from(
            [
                vec![Span::styled(
                    key_cell("status", DETAIL_KEY_W, DETAIL_KEY_GUTTER),
                    theme::label(),
                )],
                pill(label.to_string(), style),
            ]
            .concat(),
        ));
    }
    lines.push(Line::from(""));

    let rows = app.pool_detail_rows();
    let cursor = app.config_action_cursor.min(rows.len().saturating_sub(1));
    // The action row whose job runs takes the braille spinner in its caret slot
    // (a running remove never lights the refresh row, and a codex-family
    // remove — rows `[remove]` only — still shows its cue).
    let running_kind = app
        .pool_mutation_row
        .as_ref()
        .zip(app.pool_mutation_kind)
        .and_then(|((family, name), kind)| {
            (auth.clone().family() == Some(*family) && account.name == name.as_str())
                .then_some(kind)
        });
    let mut starts = Vec::with_capacity(rows.len());
    let mut focus = (lines.len(), lines.len() + 1);
    for (i, row_kind) in rows.iter().enumerate() {
        let selected = actions_focused && i == cursor;
        starts.push(lines.len());
        let arrow = match (row_kind, running_kind) {
            (PoolDetailRow::Refresh, Some(PoolMutationKind::Refresh))
            | (PoolDetailRow::Remove, Some(PoolMutationKind::Remove)) => Span::styled(
                format!("{} ", spinner_frame(app.tick_count)),
                theme::accent(),
            ),
            _ if selected => Span::styled("❯ ", theme::accent().bold()),
            _ => Span::raw("  "),
        };
        let (label, style) = match row_kind {
            PoolDetailRow::Refresh => ("refresh".to_string(), bold_when(theme::accent(), selected)),
            PoolDetailRow::Remove => {
                let label = if app.pool_detail_armed {
                    "press again to remove".to_string()
                } else {
                    "remove".to_string()
                };
                (label, theme::danger().bold())
            }
        };
        if selected {
            focus.0 = lines.len();
        }
        let line = Line::from(vec![arrow, Span::styled(label, style)]);
        lines.push(if selected {
            highlight_row(line, inner.width as usize)
        } else {
            line
        });
        if selected {
            focus.1 = lines.len();
        }
    }

    app.setup_detail_viewport.set(inner.height as usize);
    app.setup_detail_total.set(lines.len());
    app.setup_detail_row_starts.replace(starts);
    app.setup_detail_block_ends
        .replace(rows.iter().map(|_| 1).collect::<Vec<_>>());
    app.setup_detail_tips.replace(vec![0; rows.len()]);
    draw_scrolled_lines(frame, inner, lines, focus, Some(&app.setup_detail_offset));
}

/// The pool detail's `status` pill, the first that holds: needs re-login
/// (DANGER), paused (dim), disabled (dim), cooldown (WARNING) — `None` while
/// the account is available, so a clean row carries no status word. The third
/// tuple element is the cooldown countdown (epoch-seconds remaining) for the
/// one caller that renders it; the Setup detail omits it. Shared by Setup's
/// [`draw_pool_detail`] and the Usage pool pane, so the two cannot disagree.
pub(super) fn pool_status_pill(
    account: &crate::gateway_admin::PoolAccount,
) -> Option<(&'static str, Style, Option<u64>)> {
    if account.needs_relogin {
        Some(("needs re-login", theme::danger().bold(), None))
    } else if account.paused {
        Some(("paused", theme::dim().bold(), None))
    } else if account.disabled {
        Some(("disabled", theme::dim().bold(), None))
    } else if let Some(secs) = account.cooldown_secs_remaining {
        Some(("cooldown", theme::warning().bold(), Some(secs)))
    } else {
        None
    }
}

/// CLA-SPLIT status row (`token`): a profile carrying a long-lived-token sidecar
/// runs its sessions on that static login, and the ~1yr horizon is the one thing
/// about it worth watching. A comfortable horizon is a plain accent value; the
/// last 30 days warn as a pill; expired and mis-filled escalate to a DANGER pill
/// plus a `└` fix line — matching the usage-tab diagnostic shape. Expired means
/// every switch would sign sessions out; a mis-filled sidecar (rotating pair,
/// split disengaged) means the operator thinks the split is armed and it isn't.
///
/// A refused setup token comes before all of these: the server refused the
/// token itself, so it is a dead login whatever its expiry says, and the row
/// reads `[ token rejected ]` with the re-mint fix the cards show.
///
/// Returns the row line plus, for the charged states, an always-on `└` fix
/// line (this row lives in the non-focusable header block, so the hint can't be
/// focus-gated — it renders like the usage-tab status hints). `width` sizes the
/// tooltip wrap.
fn session_token_lines(
    status: &crate::claude::SessionTokenStatus,
    rolling: bool,
    token_rejected: bool,
    name: &str,
    now_ms: i64,
    width: usize,
) -> Vec<Line<'static>> {
    use crate::claude::SessionTokenStatus;
    let key = || {
        Span::styled(
            key_cell("token", DETAIL_KEY_W, DETAIL_KEY_GUTTER),
            theme::label(),
        )
    };
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

    if token_rejected {
        return charged(
            crate::format::DIAG_TOKEN_REJECTED.to_string(),
            &super::prose::setup_token_remint_fix(name),
        );
    }
    match status {
        SessionTokenStatus::LongLived(Some(ms)) => {
            if now_ms >= *ms {
                if rolling {
                    charged(
                        "rolling token stalled".to_string(),
                        &format!(
                            "nothing re-stamped it before expiry · {} re-arms",
                            cmd(&format!("clauth rolling-token {name}"))
                        ),
                    )
                } else {
                    charged(
                        "expired".to_string(),
                        &super::prose::setup_token_remint_fix(name),
                    )
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

/// The trailing inputs [`draw_settings_rows`] takes past the argument-count
/// lint: whether the action rows hold focus, and the toast rects a field caret
/// avoids.
struct SettingsRowsTail<'a> {
    actions_focused: bool,
    toast_rects: &'a [Rect],
}

fn draw_settings_rows(
    frame: &mut Frame<'_>,
    inner: Rect,
    app: &App,
    rows: &[ConfigRow],
    cursor: usize,
    snap: &Snap,
    tail: SettingsRowsTail<'_>,
) {
    let SettingsRowsTail {
        actions_focused,
        toast_rects,
    } = tail;
    // Every field VALUE reads the one `+ new` draft lookup too: at rest that is
    // the suspended `new_account_draft`, so a typed base url / api key / model
    // (and the mint form's gateway url) render instead of `—`; an existing
    // account's detail keeps its live `config_draft` buffer. No config guard is
    // held here — this is a render leaf on `&App`, and the lookup's fallback
    // branch takes its own short guard internally.
    let draft = match app.setup_selection() {
        SetupSelection::New => new_form_draft(app),
        _ => app.config_draft.as_ref(),
    };
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
            key_cell("status", DETAIL_KEY_W, DETAIL_KEY_GUTTER),
            theme::label(),
        )];
        spans.extend(pill(DIAG_DISABLED.to_string(), theme::dim().bold()));
        lines.push(Line::from(spans));
    }

    // The `+ new` form replaces the read-only `type  oauth|api` line with its
    // own `type` cycle row (the first row); an existing account keeps the line.
    // Keyed on the row list, not the selection: the render's test fixtures draw
    // the header directly with an empty roster, which reads as `New` but still
    // wants the line.
    let is_new_form = rows.first() == Some(&ConfigRow::Type);
    if !is_new_form {
        lines.push(Line::from(vec![
            Span::styled(
                key_cell("type", DETAIL_KEY_W, DETAIL_KEY_GUTTER),
                theme::label(),
            ),
            Span::styled(type_value, type_style),
        ]));
    }

    // The address belongs to the OAuth login, so it goes with the `oauth` type
    // the moment a draft base url turns the account api-typed.
    if !is_api && let Some(email) = &snap.email {
        let key = key_cell("email", DETAIL_KEY_W, DETAIL_KEY_GUTTER);
        let value_w = (inner.width as usize).saturating_sub(key.chars().count());
        lines.push(Line::from(vec![
            Span::styled(key, theme::label()),
            Span::styled(middle_truncate(email, value_w), theme::accent()),
        ]));
    }

    // Provider row — only for recognised third-party providers. Hidden while a
    // draft empties the base-url buffer (`is_api` tracks the draft live).
    let provider_label = if is_api { snap.provider } else { None };
    if let Some(label) = provider_label {
        lines.push(Line::from(vec![
            Span::styled(
                key_cell("provider", DETAIL_KEY_W, DETAIL_KEY_GUTTER),
                theme::label(),
            ),
            Span::styled(label, theme::accent()),
        ]));
    }

    if let Some(status) = &snap.session_token {
        lines.extend(session_token_lines(
            status,
            snap.rolling_token,
            snap.token_rejected,
            &snap.title,
            crate::usage::now_ms() as i64,
            inner.width as usize,
        ));
    }

    lines.push(Line::from(""));
    // Tracks the absolute line index + buffer + row of the active edit row for
    // cursor placement after rendering. The header block above is variable
    // (optional status + type + optional email + optional provider + optional
    // session + blank),
    // so the row loop's base index is simply what has been pushed so far.
    let mut edit_caret: Option<(u16, std::borrow::Cow<'_, InputState>, usize)> = None;
    let mut line_idx: u16 = lines.len() as u16;

    // Start + end of the focused row's block (row plus its tooltip lines), so a
    // wrapped hint can't scroll off the bottom while its row stays visible.
    let mut focus = (0usize, 1usize);
    let mut starts: Vec<usize> = Vec::with_capacity(rows.len());
    let mut block_ends: Vec<usize> = Vec::with_capacity(rows.len());
    let mut tips: Vec<usize> = Vec::with_capacity(rows.len());
    for (i, row) in rows.iter().enumerate() {
        let selected = actions_focused && i == cursor;
        let is_editing = editing == Some(*row);
        let input = row_input(draft, snap, *row);
        let row_lines = detail_row(
            *row,
            selected,
            is_editing,
            armed_action,
            snap,
            &input,
            inner.width as usize,
        );
        // The tooltip this row carries when focused: its behaviour hint, kept
        // while a text field edits; the bell threshold, a numeric row, swaps
        // in its range tooltip while typing; a name/create refusal renders the
        // DANGER invalid-input line instead.
        let refusal: Option<String> = match *row {
            ConfigRow::Name => {
                let stored = snap.name_refusal.clone();
                if stored.is_some() {
                    stored
                } else if snap.account_type == crate::tui::app::NewAccountType::Shunt {
                    let name = draft.map(|d| d.name.value.trim()).unwrap_or("");
                    (!name.is_empty()
                        && crate::gateway_admin::ShuntAccountName::new(name).is_none())
                    .then(|| "name: lowercase a-z, 0-9 and - only".to_string())
                } else {
                    None
                }
            }
            ConfigRow::Create => snap.create_refusal.clone(),
            _ => None,
        };
        let tip_lines: Vec<Line<'static>> = if is_editing && *row == ConfigRow::BellThreshold {
            alert_range_tooltip(&input, inner.width as usize)
        } else if is_editing && *row == ConfigRow::UsageProbeInterval {
            probe_range_tooltip(&input, inner.width as usize)
        } else if let Some(reason) = refusal {
            invalid_tooltip_lines(&reason, inner.width as usize)
        } else {
            row_hint(*row, snap)
                .map(|text| help_tooltip_lines(&text, inner.width as usize))
                .unwrap_or_default()
        };
        let tip_len = tip_lines.len() as u16;
        if is_editing {
            // The value opens after the row's first two spans: the gutter and
            // the key cell as rendered, which a narrow pane clamps.
            let lead = row_lines
                .first()
                .map_or(0, |line| line.spans.iter().take(2).map(Span::width).sum());
            edit_caret = Some((line_idx, input, lead));
        }
        starts.push(line_idx as usize);
        if selected || is_editing {
            focus.0 = line_idx as usize;
        }
        for line in row_lines {
            lines.push(if selected {
                highlight_row(line, inner.width as usize)
            } else {
                line
            });
            line_idx += 1;
        }
        if selected || is_editing {
            line_idx += tip_len;
            lines.extend(tip_lines);
        }
        if selected || is_editing {
            focus.1 = line_idx as usize;
        }
        block_ends.push(if selected || is_editing {
            line_idx as usize
        } else {
            (line_idx + tip_len) as usize
        });
        tips.push(tip_len as usize);
    }

    // The row list outgrows a short terminal (env entries + model overrides are
    // unbounded), so it scrolls to the focused row rather than clipping its tail.
    app.setup_detail_viewport.set(inner.height as usize);
    app.setup_detail_total.set(lines.len());
    app.setup_detail_row_starts.replace(starts);
    app.setup_detail_block_ends.replace(block_ends);
    app.setup_detail_tips.replace(tips);
    let offset = draw_scrolled_lines(frame, inner, lines, focus, Some(&app.setup_detail_offset));

    // Position the native terminal cursor at the caret when a text/model field is active.
    if let Some((ly, input, lead)) = edit_caret
        && let Some(visible) = (ly as usize)
            .checked_sub(offset)
            .filter(|v| *v < inner.height as usize)
    {
        let cx = inner.x.saturating_add((lead + head_cols(&input)) as u16);
        let cy = inner.y.saturating_add(visible as u16);
        place_caret(frame, toast_rects, Position::new(cx, cy));
    }
}

/// The edit buffer for a row: the live draft buffer when present (borrowed, so
/// its scroll window persists across frames), else a throwaway `InputState`
/// seeded from the read-only [`Snap`]. Toggle/action rows have no buffer and
/// resolve to an empty one (never rendered as a field).
fn row_input<'a>(
    draft: Option<&'a ConfigDraft>,
    snap: &Snap,
    row: ConfigRow,
) -> std::borrow::Cow<'a, InputState> {
    draft.and_then(|d| d.field(row)).map_or_else(
        || std::borrow::Cow::Owned(InputState::new(snap_value(snap, row))),
        std::borrow::Cow::Borrowed,
    )
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
        ConfigRow::AdvisorModel => &snap.advisor,
        ConfigRow::BellThreshold => &snap.bell_threshold,
        ConfigRow::UsageProbeInterval => &snap.usage_probe_interval,
        ConfigRow::EnvEntry(i) => snap.env.get(i).map(|(_, v)| v.as_str()).unwrap_or(""),
        ConfigRow::AutoStart
        | ConfigRow::UsageProbe
        | ConfigRow::Type
        | ConfigRow::Family
        | ConfigRow::ModelOverrideAdd
        | ConfigRow::EnvAdd
        | ConfigRow::Login
        | ConfigRow::CaptureLogin
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
/// name what ⏎ really does — while `auto_start`, `base_url` and `alert at` flip
/// on their own value.
fn row_hint(row: ConfigRow, snap: &Snap) -> Option<String> {
    let api_login = !snap.login_is_oauth;
    let hint = match row {
        ConfigRow::BaseUrl if snap.base_url.trim().is_empty() => {
            "leave empty for a Claude account, or set an API endpoint"
        }
        ConfigRow::BaseUrl => "the API endpoint this account calls instead of claude.ai",
        ConfigRow::ApiKey if snap.mint_armed => "clauth mints a client token for this profile",
        ConfigRow::ApiKey => "provided to Claude Code via \"apiKeyHelper\" field",
        ConfigRow::SubagentModel => "default subagent model in this account",
        ConfigRow::AdvisorModel => "server-side advisor model in this account",
        // Gate reasons name the same blockers as the CLI's own refusal copy
        // (`actions::disable_profile`), then the on/off state — checked in that
        // order since a gate can only ever bite the OFF (not-yet-disabled)
        // state. `live session` is the app-wide noun for a running `clauth
        // start`; the CLI's wording is its own.
        ConfigRow::Disabled if snap.is_active => "cannot disable the global active account",
        ConfigRow::Disabled if snap.has_live_session => {
            "cannot disable an account with a live session"
        }
        ConfigRow::Disabled if snap.disabled => {
            "excluded from auto-switch, usage polling, and status until re-enabled"
        }
        ConfigRow::Disabled => {
            "excludes this account from auto-switch, usage polling, and status until re-enabled"
        }
        ConfigRow::AutoStart if snap.auto_start => {
            "sends a 1-token request to Haiku at every 5h usage reset"
        }
        ConfigRow::AutoStart => "5h usage window stays closed after reset",
        // Both states name the cost, since turning it on is what pays it.
        ConfigRow::UsageProbe if snap.usage_probe => {
            "a 1-token request to Haiku reads usage and may start the 5h window"
        }
        ConfigRow::UsageProbe => {
            "usage goes unread; a probe is a 1-token request that may start the 5h window"
        }
        ConfigRow::UsageProbeInterval => "time between two probes, each one billed",
        ConfigRow::BellThreshold if snap.bell_threshold.is_empty() => concat!(
            "no alert when 5h usage climbs; ",
            key_lit!("↵"),
            " or ",
            key_lit!("+"),
            " to set one"
        ),
        ConfigRow::BellThreshold => {
            "alerts once when 5h usage reaches this, again after it drops below"
        }
        ConfigRow::ModelOverrideAdd => "set custom model names on this account",
        ConfigRow::Login if snap.console_login => {
            "log into alibaba console to see this account's usage"
        }
        ConfigRow::Login if snap.account_type == crate::tui::app::NewAccountType::Codex => {
            "mint a codex login in the browser"
        }
        ConfigRow::Login if snap.login_disabled_reason.is_some() => {
            return Some(snap.login_disabled_reason.clone().unwrap_or_default());
        }
        ConfigRow::Login if api_login => "re-enter the base URL + API key for this account",
        ConfigRow::Login => "browser OAuth login",
        ConfigRow::CaptureLogin if snap.account_type == crate::tui::app::NewAccountType::Codex => {
            "capture the current codex login into this account"
        }
        ConfigRow::CaptureLogin => "save the current global credentials into a new account",
        ConfigRow::DeleteCreds if api_login && snap.has_unreadable_store => {
            if snap.has_api_key {
                "clears the stored API key and the unreadable credentials.json"
            } else {
                "clears the unreadable credentials.json"
            }
        }
        ConfigRow::DeleteCreds if api_login => "clears the stored API key",
        ConfigRow::DeleteCreds => "clears the stored OAuth login",
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
        ConfigRow::ClearSessionToken if snap.clear_gated() => {
            "cannot clear with no other login stored"
        }
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
                (true, false) => "signs Claude Code out now · this account runs on its API key",
                (false, true) => "the next switch installs this account's own login again",
                (false, false) => "the next switch runs this account on its API key",
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
        ConfigRow::Model
        | ConfigRow::OpusModel
        | ConfigRow::SonnetModel
        | ConfigRow::HaikuModel
        | ConfigRow::FableModel
        | ConfigRow::EnvEntry(_)
        | ConfigRow::EnvAdd
        | ConfigRow::Name
        | ConfigRow::Type
        | ConfigRow::Family
        | ConfigRow::Create => return None,
    };
    Some(hint.to_string())
}

/// Sub-line under the `probe every` field while typing: the band a typed
/// value must sit in, DANGER while the buffer would be refused (no number of
/// seconds, or one outside the band). The band reads in the row's own unit
/// form, `90s`.
fn probe_range_tooltip(input: &InputState, width: usize) -> Vec<Line<'static>> {
    let range = format!(
        "{}s-{}s",
        crate::profile::MIN_USAGE_PROBE_INTERVAL_MS / 1000,
        crate::profile::MAX_USAGE_PROBE_INTERVAL_MS / 1000
    );
    if crate::tui::app::parse_usage_probe_interval(input.trimmed()).is_none() {
        invalid_tooltip_lines(&range, width)
    } else {
        help_tooltip_lines(&range, width)
    }
}

/// Sub-line under the `alert at` field while typing: the accepted input,
/// DANGER while the buffer would be refused (the Fallback `rotate at` twin).
fn alert_range_tooltip(input: &InputState, width: usize) -> Vec<Line<'static>> {
    let range = "0-100 % · empty turns it off";
    if crate::tui::app::parse_bell_threshold(input.trimmed()).is_none() {
        invalid_tooltip_lines(range, width)
    } else {
        help_tooltip_lines(range, width)
    }
}

fn detail_row(
    row: ConfigRow,
    selected: bool,
    editing: bool,
    armed_action: Option<ConfigRow>,
    snap: &Snap,
    input: &InputState,
    width: usize,
) -> Vec<Line<'static>> {
    let arrow = if editing {
        Span::styled(format!("{} ", theme::edit_glyph()), theme::accent().bold())
    } else if selected {
        Span::styled("❯ ", theme::accent().bold())
    } else {
        Span::raw("  ")
    };
    let line = match row {
        ConfigRow::Name => {
            return kv_field(arrow, "name", input, editing, selected, false, false, width);
        }
        // The `+ new` form's account-type cycle, first row of the form.
        ConfigRow::Type => {
            let lead = kv_lead(arrow, "type", selected);
            let options: Vec<(&str, bool)> = [
                crate::tui::app::NewAccountType::Claude,
                crate::tui::app::NewAccountType::Codex,
                crate::tui::app::NewAccountType::Shunt,
            ]
            .iter()
            .map(|t| (t.as_str(), *t == snap.account_type))
            .collect();
            return cycle_row_lines(lead, &options, None, None, selected, width);
        }
        // The shunt form's family cycle.
        ConfigRow::Family => {
            let lead = kv_lead(arrow, "family", selected);
            let options: Vec<(&str, bool)> = [
                crate::gateway_admin::PoolFamily::Claude,
                crate::gateway_admin::PoolFamily::Codex,
                crate::gateway_admin::PoolFamily::Antigravity,
            ]
            .iter()
            .map(|f| (f.segment(), *f == snap.family))
            .collect();
            return cycle_row_lines(lead, &options, None, None, selected, width);
        }
        ConfigRow::BaseUrl => {
            return kv_field(
                arrow, "base url", input, editing, selected, false, true, width,
            );
        }
        // The mint-seeded key row is a Disabled row: whole line faint,
        // focusable but inert, the tooltip (below) naming what happens on save.
        ConfigRow::ApiKey if snap.mint_armed => {
            let row_arrow = if selected {
                Span::styled("❯ ", theme::faint())
            } else {
                Span::raw("  ")
            };
            let key = key_cell("api key", DETAIL_KEY_W, DETAIL_KEY_GUTTER);
            let label = Span::styled("minted on save", theme::faint());
            // Inline while the label fits beside the key; past that the label
            // drops to its own line under the key (the `kv_field` drop arm's
            // shape) instead of clipping mid-word, measured in cells.
            let lead = 2 + Span::raw(&key).width();
            if lead + Span::raw("minted on save").width() <= width {
                Line::from(vec![row_arrow, Span::styled(key, theme::faint()), label])
            } else {
                return vec![
                    Line::from(vec![row_arrow, Span::styled(key, theme::faint())]),
                    Line::from(vec![Span::raw("  "), label]),
                ];
            }
        }
        ConfigRow::ApiKey => {
            return kv_field(
                arrow, "api key", input, editing, selected, true, false, width,
            );
        }
        // Hybrid: the alias cycle at rest, a plain text field while typing a custom id.
        ConfigRow::Model if !editing => {
            return model_cycle_lines(arrow, &input.value, selected, width);
        }
        ConfigRow::Model => {
            return kv_field(
                arrow, "model", input, editing, selected, false, false, width,
            );
        }
        ConfigRow::OpusModel => {
            return kv_field(arrow, "opus", input, editing, selected, false, false, width);
        }
        ConfigRow::SonnetModel => {
            return kv_field(
                arrow, "sonnet", input, editing, selected, false, false, width,
            );
        }
        ConfigRow::HaikuModel => {
            return kv_field(
                arrow, "haiku", input, editing, selected, false, false, width,
            );
        }
        ConfigRow::FableModel => {
            return kv_field(
                arrow, "fable", input, editing, selected, false, false, width,
            );
        }
        ConfigRow::SubagentModel => {
            return kv_field(
                arrow, "subagent", input, editing, selected, false, false, width,
            );
        }
        ConfigRow::AdvisorModel => {
            return kv_field(
                arrow, "advisor", input, editing, selected, false, false, width,
            );
        }
        // A custom env entry: its key is the label; mask the value when the key
        // looks like a credential (mirrors the api-key row).
        ConfigRow::EnvEntry(i) => {
            let key = snap.env.get(i).map(|(k, _)| k.clone()).unwrap_or_default();
            let mask = env_key_is_secret(&key);
            return kv_field(arrow, &key, input, editing, selected, mask, false, width);
        }
        // While editing, the typed text is the new key; at rest, the add chip.
        ConfigRow::EnvAdd if editing => {
            return kv_field(arrow, "key", input, editing, selected, false, false, width);
        }
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
        // is open — a disabled row (mirrors the Fallback tab's `max
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
        // A refused buffer renders DANGER beside its range line, and the ` %`
        // unit trails the native caret: the `rotate at` editor's shape.
        ConfigRow::BellThreshold if editing => {
            let invalid = crate::tui::app::parse_bell_threshold(input.trimmed()).is_none();
            let key = field_key_cell("alert at", width, MIN_VALUE_W + " %".len());
            let room = width.saturating_sub(2 + Span::raw(&key).width() + " %".len());
            let mut spans = vec![arrow, Span::styled(key, label_style(selected))];
            spans.extend(value_caret(input, invalid, room));
            spans.push(Span::styled(
                " %",
                if invalid {
                    theme::danger()
                } else {
                    theme::faint()
                },
            ));
            Line::from(spans)
        }
        ConfigRow::BellThreshold => {
            let (value, style) = match snap.bell_threshold.as_str() {
                "" => ("off".to_string(), theme::faint()),
                pct => (format!("{pct}%"), theme::accent()),
            };
            kv_static(arrow, "alert at", value, style, selected)
        }
        ConfigRow::AutoStart => {
            let mut spans = kv_lead(arrow, "auto-start", selected);
            spans.extend(toggle_value(snap.auto_start, false));
            Line::from(spans)
        }
        ConfigRow::UsageProbe => {
            let mut spans = kv_lead(arrow, "usage probe", selected);
            spans.extend(toggle_value(snap.usage_probe, false));
            Line::from(spans)
        }
        // The `alert at` shape: a refused buffer renders DANGER, and the `s`
        // unit trails the native caret.
        ConfigRow::UsageProbeInterval if editing => {
            let invalid = crate::tui::app::parse_usage_probe_interval(input.trimmed()).is_none();
            let key = field_key_cell("probe every", width, MIN_VALUE_W + "s".len());
            let room = width.saturating_sub(2 + Span::raw(&key).width() + "s".len());
            let mut spans = vec![arrow, Span::styled(key, label_style(selected))];
            spans.extend(value_caret(input, invalid, room));
            spans.push(Span::styled(
                "s",
                if invalid {
                    theme::danger()
                } else {
                    theme::faint()
                },
            ));
            Line::from(spans)
        }
        ConfigRow::UsageProbeInterval => kv_static(
            arrow,
            "probe every",
            format!("{}s", snap.usage_probe_interval),
            theme::accent(),
            selected,
        ),
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
        // Same done-state pattern as `Login`: a stashed snapshot renders the ✓,
        // ⏎ re-captures but confirms first before replacing a stashed mint.
        ConfigRow::CaptureLogin => {
            if snap.captured_live || snap.captured_codex_capture {
                Line::from(vec![
                    arrow,
                    Span::styled(
                        "✓ captured current login",
                        bold_when(theme::success(), selected),
                    ),
                ])
            } else {
                Line::from(vec![
                    arrow,
                    Span::styled(
                        "+ capture current login",
                        bold_when(theme::accent(), selected),
                    ),
                ])
            }
        }
        ConfigRow::Login => {
            // The shunt `+ login` is a Disabled row while the add is
            // unavailable: whole line faint, focusable but inert, the reason on
            // its `└` line (via `row_hint`).
            if snap.login_disabled_reason.is_some() {
                let row_arrow = if selected {
                    Span::styled("❯ ", theme::faint())
                } else {
                    Span::raw("  ")
                };
                return vec![Line::from(vec![
                    row_arrow,
                    Span::styled("+ login", theme::faint()),
                ])];
            }
            // A draft-held mint (claude or codex) renders the done state; ⏎
            // re-runs the login but confirms first before replacing the stash.
            if snap.captured || snap.captured_codex_mint {
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
    };
    vec![line]
}

/// The key cell a Setup field row renders. A long env-key label widens the
/// cell past DETAIL_KEY_W (`key_cell` pads to the key's own width), so the
/// value column opens after the RENDERED key — measured, never the constant. A
/// key wider than the pane clamps with `…` like any value, never a mid-word
/// cut, and past where it would leave `reserve` cells after it: an edited
/// field keeps room for its value and caret.
fn field_key_cell(key: &str, width: usize, reserve: usize) -> String {
    let full_key = key_cell(key, DETAIL_KEY_W, DETAIL_KEY_GUTTER);
    if Span::raw(&full_key).width() > width.saturating_sub(2 + reserve) {
        let budget = width.saturating_sub(2 + DETAIL_KEY_GUTTER + reserve);
        format!("{}{}", truncate(key, budget), " ".repeat(DETAIL_KEY_GUTTER))
    } else {
        full_key
    }
}

/// One form row's key + value line(s). `middle` picks middle-ellipsis for the
/// value (a URL row) instead of the trailing-ellipsis default.
#[expect(
    clippy::too_many_arguments,
    reason = "one call per row carries its whole chrome: key, value, edit/mask/ellipsis flags"
)]
fn kv_field(
    arrow: Span<'static>,
    key: &str,
    input: &InputState,
    editing: bool,
    focused: bool,
    mask_value: bool,
    middle: bool,
    width: usize,
) -> Vec<Line<'static>> {
    let key_cell_span = field_key_cell(key, width, if editing { MIN_VALUE_W } else { 0 });
    let lead = 2 + Span::raw(&key_cell_span).width();
    let value_w = width.saturating_sub(lead);
    let mut spans = vec![arrow, Span::styled(key_cell_span, label_style(focused))];
    if editing || mask_value || input.value.is_empty() {
        spans.extend(value_spans(input, editing, mask_value, value_w));
        return vec![Line::from(spans)];
    }
    if value_w >= MIN_VALUE_W {
        let value = if middle {
            middle_truncate(&input.value, value_w)
        } else {
            truncate(&input.value, value_w)
        };
        spans.push(Span::styled(value, theme::accent()));
        return vec![Line::from(spans)];
    }
    // The value column can't hold a clamped value: drop it below the key,
    // indented past the caret gutter, at the full pane width.
    vec![
        Line::from(spans),
        Line::from(vec![
            Span::raw("  "),
            Span::styled(
                truncate(&input.value, width.saturating_sub(2)),
                theme::accent(),
            ),
        ]),
    ]
}

/// The shared lead of a Setup key/value row: the gutter glyph and the key
/// cell, styled by the row's focus. `kv_static`, the `auto-start` toggle and
/// the `type` / `family` / `model` cycles all build their lead on it.
fn kv_lead(arrow: Span<'static>, key: &str, focused: bool) -> Vec<Span<'static>> {
    vec![
        arrow,
        Span::styled(
            key_cell(key, DETAIL_KEY_W, DETAIL_KEY_GUTTER),
            label_style(focused),
        ),
    ]
}

fn kv_static(
    arrow: Span<'static>,
    key: &str,
    value: String,
    value_style: Style,
    focused: bool,
) -> Line<'static> {
    let mut spans = kv_lead(arrow, key, focused);
    spans.push(Span::styled(value, value_style));
    Line::from(spans)
}

/// Mask a custom env value when its key names a credential (mirrors the api-key row).
fn env_key_is_secret(key: &str) -> bool {
    let upper = key.to_ascii_uppercase();
    ["KEY", "TOKEN", "SECRET", "AUTH"]
        .iter()
        .any(|needle| upper.contains(needle))
}

/// A field row's value: the shared editor body ([`value_caret`]) scrolled
/// inside `value_w` cells while editing, else the value, its mask or `—`.
fn value_spans(
    input: &InputState,
    editing: bool,
    mask_value: bool,
    value_w: usize,
) -> Vec<Span<'static>> {
    if editing {
        return value_caret(input, false, value_w);
    }
    if input.value.is_empty() {
        return vec![Span::styled("—", theme::faint())];
    }
    let display = if mask_value {
        "••••••••".to_string()
    } else {
        input.value.clone()
    };
    vec![Span::styled(display, theme::accent())]
}

/// The `model` row at rest. A custom id (set via ⏎) matches no preset, so it
/// trails the run rather than mis-bracketing the nearest alias.
fn model_cycle_lines(
    arrow: Span<'static>,
    current: &str,
    selected: bool,
    width: usize,
) -> Vec<Line<'static>> {
    let lead = kv_lead(arrow, "model", selected);
    let mut options: Vec<(&str, bool)> = vec![("default", current.is_empty())];
    options.extend(MODEL_PRESETS.iter().map(|p| (*p, *p == current)));
    let custom =
        (!current.is_empty() && !MODEL_PRESETS.contains(&current)).then_some((current, true));
    cycle_row_lines(lead, &options, custom, None, selected, width)
}

#[cfg(test)]
#[path = "../../../tests/inline/tui_render_config.rs"]
mod tests;
