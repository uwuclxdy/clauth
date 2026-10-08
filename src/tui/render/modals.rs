//! Modal dialogs — stacking layer above the screen.

use ratatui::Frame;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::Style;
use ratatui::symbols::border;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Padding, Paragraph};

use crate::profile::DivergenceChoice;

use super::super::app::{
    ActionMenuState, App, ConfirmAction, ConfirmState, DivergenceAction, DivergenceForm,
    DivergenceTargetForm, EnvCollisionChoice, EnvCollisionForm, InputState, LoginMethod,
    LoginStage, Modal, NamePromptForm, PresetPickerForm, StatusFocus, Tab, TokenView,
    token_model_count,
};
use super::super::theme;
use super::chain::reason_marker;
use super::format::spinner_frame;
use super::overview::switch_mark;
use super::panes::{
    DIAG_AUTH_BROKEN, DIAG_BUDGET_SPENT, DIAG_CANCELED, DIAG_DISABLED, DIAG_KEY_REJECTED,
    DIAG_KICK, DIAG_NO_USAGE, DIAG_STALE, DIAG_WEEKLY_SOFT, DIAG_WEEKLY_SPENT, bold_when,
    draw_scrollbar, draw_scrolled_lines, follow_scroll_offset, head_cols, key_cell, meta_line,
};
use super::prose::{self, cmd_lit, key_lit};
use crate::fallback::BlockedReason;

pub(super) fn draw(frame: &mut Frame<'_>, area: Rect, app: &App, modal: &Modal) {
    match modal {
        Modal::Confirm(state) => draw_confirm(frame, area, state),
        Modal::Divergence(form) => draw_divergence(frame, area, form),
        Modal::CaptureName(form) => draw_capture_name(frame, area, &form.input),
        Modal::NamePrompt(form) => draw_name_prompt(frame, area, form),
        Modal::PresetPicker(form) => draw_preset_picker(frame, area, form),
        Modal::DivergenceTarget(form) => draw_divergence_target(frame, area, form),
        Modal::Help => draw_help(frame, area, app),
        Modal::ActionMenu(state) => draw_action_menu(frame, area, state),
        Modal::EnvCollision(form) => draw_env_collision(frame, area, form),
        Modal::Login => draw_login_progress(frame, area, app),
    }
}

/// In-flight login progress. Renders live from `App::login` (the stage, the
/// console login's URL and the code field land async), so the modal variant
/// carries no state of its own. An OAuth login waits on two doors at once —
/// the browser this machine opened, or the hosted link on any other device —
/// so while it waits the modal offers `r`, `c` and `p`, and `p`'s own row
/// becomes the code field once opened. The link itself is never rendered: a
/// wrapped ~440-char authorize URL isn't clickable and clips in compact mode,
/// and `c` hands it over whole. Once a door delivered a code the stage line is
/// the whole story, whichever door it was.
fn draw_login_progress(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let Some(session) = app.login.as_ref() else {
        return; // login ended this frame; the modal pops on the next drain
    };
    // One waiting text for both flows. The paste door's later stages read as
    // one act; the browser door's keep their two.
    let stage = match (session.stage, session.method) {
        (LoginStage::WaitingBrowser, _) if session.paste_field.is_some() => "pasting the code",
        (LoginStage::WaitingBrowser, _) => "continue in your browser",
        (LoginStage::ExchangingCode(_) | LoginStage::Verifying, LoginMethod::Manual) => {
            "logging in with the code"
        }
        (LoginStage::ExchangingCode(_), LoginMethod::Browser) => "exchanging the code for tokens",
        (LoginStage::Verifying, LoginMethod::Browser) => "verifying the minted token",
    };
    let mut lines: Vec<Line<'_>> = vec![
        Line::from(Span::styled(
            format!("logging in '{}'", session.name),
            theme::body(),
        )),
        Line::from(""),
        Line::from(vec![
            Span::styled(
                format!("{} ", spinner_frame(app.tick_count)),
                theme::accent(),
            ),
            Span::styled(stage, theme::dim()),
        ]),
        Line::from(""),
    ];
    let key_row = |key: &'static str, what: &'static str| {
        Line::from(vec![
            Span::styled(key, theme::accent().bold()),
            Span::styled(what, theme::dim()),
        ])
    };
    let mut caret = None;
    if session.open_door().is_some() {
        lines.push(key_row("r", "  open the browser again"));
        lines.push(key_row("c", "  copy link"));
        match session.paste_field.as_ref() {
            Some(field) => {
                // The caret sits after the edit gutter (2), the label (4) and
                // its space (1), then the cells before `InputState::cursor`.
                caret = Some((lines.len(), 2 + 4 + 1 + head_cols(field)));
                lines.push(code_field_row(field));
            }
            None => lines.push(key_row("p", "  paste code")),
        }
    } else if session.stage == LoginStage::WaitingBrowser {
        // The console login: one browser, whose URL its worker announces —
        // the retry is live once it has.
        match session.url {
            Some(_) => lines.push(key_row("r", "  open the browser again")),
            None => lines.push(Line::from(Span::styled(
                "opening your browser…",
                theme::dim(),
            ))),
        }
    } else {
        lines.pop();
    }
    draw_modal_scrolled(
        frame,
        area,
        "LOGIN",
        lines,
        0,
        None,
        caret.map(|(row, col)| ModalExtra::Caret(row, col)),
    );
}

/// The login modal's code field, in place of the `p  paste code` row: an edit
/// gutter, the `code` label, then the typed text verbatim, or the placeholder
/// while there is none.
fn code_field_row(field: &InputState) -> Line<'static> {
    if field.value.is_empty() {
        labelled_line(
            "code",
            Span::styled("(paste it here)", theme::faint()),
            true,
        )
    } else {
        labelled_input("code", field, true)
    }
}

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let w = width.min(area.width.saturating_sub(4));
    let h = height.min(area.height.saturating_sub(4));
    Rect {
        x: area.x + (area.width.saturating_sub(w)) / 2,
        y: area.y + (area.height.saturating_sub(h)) / 2,
        width: w,
        height: h,
    }
}

/// Modal sized to content: snaps to widest line/title, exact line count.
/// Chrome = rounded border (1) + `Padding::new(2,2,1,1)` = 6 cols, 4 rows.
///
/// A line wider than the modal's inner width (only possible on a terminal
/// too narrow for the content) is pre-split into inner-width rows by
/// [`chunk_line`], so the height is exact by construction — ratatui's own
/// word-wrap may use MORE rows than any cheap estimate and silently clip the
/// tail. On any terminal wide enough for the content nothing splits and the
/// modal renders exactly as before.
fn draw_modal(frame: &mut Frame<'_>, area: Rect, title: &str, lines: Vec<Line<'_>>) {
    draw_modal_scrolled(frame, area, title, lines, 0, None, None);
}

fn draw_modal_focused(
    frame: &mut Frame<'_>,
    area: Rect,
    title: &str,
    lines: Vec<Line<'_>>,
    focus: std::ops::Range<usize>,
    saved: &std::cell::Cell<usize>,
) {
    draw_modal_scrolled(frame, area, title, lines, 0, Some((focus, saved)), None);
}

/// [`draw_modal`] with the content scrolled to start at row `scroll`, returning
/// the largest offset the content allows so a caller holding the offset in state
/// can clamp its key handler against it (the render pass owns the viewport).
/// `caret` names a text field's caret as `(line index, cell offset)` over the
/// lines as handed in: the native terminal cursor lands on that cell after the
/// chunking below folded the line into rows, or nowhere when the row scrolled
/// out of the viewport.
///
/// A terminal too short for the whole modal used to drop the tail with nothing
/// on screen saying so. The rows now go through the shared scrolled-lines
/// helper, which draws the overflow scrollbar, the one legal overflow signal on
/// a surface that scrolls. The focus block handed to it is the viewport window
/// itself — "keep rows `scroll..scroll + viewport` on screen" — which resolves
/// to exactly `scroll` once clamped. A modal that fits scrolls by 0 and draws
/// no bar, so it renders as before.
enum ModalExtra<'a> {
    Caret(usize, usize),
    Buttons(Line<'a>),
}

fn draw_modal_scrolled(
    frame: &mut Frame<'_>,
    area: Rect,
    title: &str,
    lines: Vec<Line<'_>>,
    scroll: u16,
    focus: Option<(std::ops::Range<usize>, &std::cell::Cell<usize>)>,
    extra: Option<ModalExtra<'_>>,
) -> u16 {
    let content_w = lines.iter().map(Line::width).max().unwrap_or(0) as u16;
    let button_w = match &extra {
        Some(ModalExtra::Buttons(buttons)) => buttons.width() as u16,
        _ => 0,
    };
    let w = (content_w.max(button_w) + 6)
        .max(title.chars().count() as u16 + 4)
        .min(area.width.saturating_sub(4));
    let inner_w = (w.saturating_sub(6) as usize).max(1);
    let line_count = lines.len();
    let mut caret_row = None;
    let mut focus_rows = (0, 0);
    let mut rows: Vec<Line<'static>> = Vec::new();
    for (i, line) in lines.into_iter().enumerate() {
        if matches!(extra, Some(ModalExtra::Caret(at, _)) if at == i) {
            caret_row = Some(rows.len());
        }
        if focus.as_ref().is_some_and(|(range, _)| range.start == i) {
            focus_rows.0 = rows.len();
        }
        if focus.as_ref().is_some_and(|(range, _)| range.end == i) {
            focus_rows.1 = rows.len();
        }
        rows.extend(chunk_line(line, inner_w));
    }
    if focus
        .as_ref()
        .is_some_and(|(range, _)| range.end >= line_count)
    {
        focus_rows.1 = rows.len();
    }
    let lines = rows;
    let button_rows = match &extra {
        Some(ModalExtra::Buttons(buttons)) if buttons.width() > inner_w => 3,
        Some(ModalExtra::Buttons(_)) => 2,
        _ => 0,
    };
    let h = (lines.len() as u16 + 4 + button_rows).min(area.height.saturating_sub(4));

    let rect = centered(area, w, h);
    frame.render_widget(Clear, rect);
    let block = modal_block(title);
    let inner = block.inner(rect);
    frame.render_widget(block, rect);

    let viewport = inner.height.saturating_sub(button_rows) as usize;
    // A terminal short enough to leave no inner rows at all draws nothing, and
    // `total - 0` would publish the whole content as a reachable offset.
    let max_scroll = if viewport == 0 {
        0
    } else {
        lines.len().saturating_sub(viewport).min(u16::MAX as usize) as u16
    };
    let scroll = if let Some((_, saved)) = focus {
        let offset = follow_scroll_offset(lines.len(), viewport, focus_rows, saved.get());
        saved.set(offset);
        offset
    } else {
        scroll.min(max_scroll) as usize
    };
    let body = Rect {
        height: viewport as u16,
        ..inner
    };
    draw_scrolled_lines(frame, body, lines, (scroll, scroll + viewport), None);
    if let Some(ModalExtra::Buttons(buttons)) = &extra {
        let button_area = Rect {
            y: inner.y + inner.height.saturating_sub(button_rows.saturating_sub(1)),
            height: button_rows.saturating_sub(1),
            ..inner
        };
        if buttons.width() > inner_w {
            // No button widget can stack the choice while keeping each complete on a narrow screen.
            let [cancel, _, add] = &buttons.spans[..] else {
                unreachable!("a choice always has cancel, gap, and affirmative spans");
            };
            frame.render_widget(
                Paragraph::new(Line::from(cancel.clone()).alignment(Alignment::Right)),
                Rect {
                    height: 1,
                    ..button_area
                },
            );
            frame.render_widget(
                Paragraph::new(Line::from(add.clone()).alignment(Alignment::Right)),
                Rect {
                    y: button_area.y + 1,
                    height: 1,
                    ..button_area
                },
            );
        } else {
            frame.render_widget(Paragraph::new(buttons.clone()), button_area);
        }
    }
    if let (Some(ModalExtra::Caret(_, cell)), Some(row)) = (extra, caret_row) {
        // A caret right after a row's last cell stays on that row, in the
        // right padding, rather than folding onto a row the chunking never
        // made: a content-sized modal is exactly as wide as its widest line,
        // so the end of a full field is the common case, not an edge.
        let fold = cell.saturating_sub(1) / inner_w;
        let row = row + fold;
        if (scroll..scroll + viewport).contains(&row) {
            frame.set_cursor_position((
                inner.x.saturating_add((cell - fold * inner_w) as u16),
                inner.y.saturating_add((row - scroll) as u16),
            ));
        }
    }
    max_scroll
}

/// Split one styled line into `w`-column rows by character, preserving span
/// styles and the line's alignment/style. A line that already fits returns
/// itself (owned) untouched. Modal copy is ASCII, so chars ≈ display cells.
fn chunk_line(line: Line<'_>, w: usize) -> Vec<Line<'static>> {
    let own = |l: &Line<'_>| -> Line<'static> {
        let mut out = Line::from(
            l.spans
                .iter()
                .map(|s| Span::styled(s.content.to_string(), s.style))
                .collect::<Vec<_>>(),
        )
        .style(l.style);
        out.alignment = l.alignment;
        out
    };
    if w == 0 || line.width() <= w {
        return vec![own(&line)];
    }

    let mut rows: Vec<Line<'static>> = Vec::new();
    let mut cur: Vec<Span<'static>> = Vec::new();
    let mut used = 0usize;
    let mut flush = |cur: &mut Vec<Span<'static>>| {
        let mut row = Line::from(std::mem::take(cur)).style(line.style);
        row.alignment = line.alignment;
        rows.push(row);
    };
    for span in &line.spans {
        let mut buf = String::new();
        for ch in span.content.chars() {
            if used == w {
                if !buf.is_empty() {
                    cur.push(Span::styled(std::mem::take(&mut buf), span.style));
                }
                flush(&mut cur);
                used = 0;
            }
            buf.push(ch);
            used += 1;
        }
        if !buf.is_empty() {
            cur.push(Span::styled(buf, span.style));
        }
    }
    if !cur.is_empty() {
        flush(&mut cur);
    }
    rows
}

/// Rounded `ACCENT_2` border, uppercase italic dim title, base `BG` fill.
/// The title opens with the corner-adjacent border dash (`╭─ TITLE`), the
/// same chrome rule the panel family follows.
fn modal_block(title: impl Into<String>) -> Block<'static> {
    let title_line = Line::from(vec![
        Span::styled("─", Style::default().fg(theme::accent_2_color())),
        Span::raw(" "),
        Span::styled(
            title.into().to_uppercase(),
            Style::default().fg(theme::text_dim_color()).italic(),
        ),
        Span::raw(" "),
    ]);
    Block::bordered()
        .border_set(border::ROUNDED)
        .border_style(Style::default().fg(theme::accent_2_color()))
        .title(title_line)
        .style(theme::base())
        .padding(Padding::new(2, 2, 1, 1))
}

/// [`modal_block`] plus the contract's title-right meta slot, for a modal whose
/// contents are scoped to one named thing. The name keeps its own case (it is a
/// proper noun, not a structural label) and stays `TEXT_DIM` — it is data about
/// the modal, so it takes neither the title's italic nor any bold.
fn modal_block_with_meta(title: &str, meta: Option<&str>) -> Block<'static> {
    let block = modal_block(title);
    match meta {
        // The name closes with a border cell of its own (`… name ─╮`), the
        // border token matching the modal's orange border.
        Some(meta) => block.title(meta_line(
            meta,
            Style::default().fg(theme::accent_2_color()),
        )),
        None => block,
    }
}

fn draw_confirm(frame: &mut Frame<'_>, area: Rect, state: &ConfirmState) {
    let title = match state.on_confirm {
        // The one non-confirm modal: an in-use account can't be acted on.
        ConfirmAction::Acknowledge => "IN USE",
        _ => "CONFIRM",
    };

    // Destructive/global ops carry a DANGER cue on their confirm button.
    // `CaptureOverwrite` replaces an existing profile's credentials in
    // place — irreversible like the other destructive actions here.
    let destructive = matches!(
        state.on_confirm,
        ConfirmAction::Switch(_)
            | ConfirmAction::RotateAll
            | ConfirmAction::RotateOne(_)
            | ConfirmAction::DisableOne(_)
            | ConfirmAction::CaptureOverwrite(..)
            | ConfirmAction::AdoptDivergence(..)
            | ConfirmAction::BlankCredentials(_)
            | ConfirmAction::DeleteLiveSession(_)
            | ConfirmAction::AddAdminTable
            | ConfirmAction::StopShunt
    );

    // `AddChainCandidate` names the candidate in its confirm button so the
    // operator sees the exact add they are agreeing to, not a generic label;
    // the shunt card's actions name their own verb too (adopt / move / add).
    let confirm_label = match &state.on_confirm {
        ConfirmAction::AddChainCandidate(name) => format!("add '{name}'"),
        ConfirmAction::AdoptConfig(_) => "adopt".to_string(),
        ConfirmAction::MoveStoresIn(_) => "move".to_string(),
        ConfirmAction::AddAdminKey | ConfirmAction::AddAdminTable => "add".to_string(),
        ConfirmAction::StartDaemon => "start".to_string(),
        ConfirmAction::StopShunt => "stop".to_string(),
        _ => "confirm".to_string(),
    };

    let mut lines: Vec<Line<'_>> = vec![Line::from(Span::styled(
        state.message.clone(),
        theme::body(),
    ))];
    if let Some(detail) = &state.detail {
        lines.push(Line::from(prose::spans(detail, theme::dim())));
    }
    // The daemon-start confirm's detail is two lines, which the one-line
    // `detail` cannot hold.
    if matches!(state.on_confirm, ConfirmAction::StartDaemon) {
        lines.push(Line::from(prose::spans(
            concat!(
                "spawns the clauth daemon detached: ",
                cmd_lit!("clauth daemon")
            ),
            theme::dim(),
        )));
        lines.push(Line::from(Span::styled(
            "logfile at ~/.clauth/daemon.log.",
            theme::dim(),
        )));
    }
    lines.push(Line::from(""));
    // An acknowledge-only notice has nothing to cancel — a single focused `ok`.
    let buttons = if matches!(state.on_confirm, ConfirmAction::Acknowledge) {
        Line::from(modal_button(" ok ", true))
    } else {
        choice_buttons(state.choice, destructive, &confirm_label, "   ")
    };
    lines.push(buttons.alignment(Alignment::Right));

    draw_modal(frame, area, title, lines);
}

fn choice_buttons(
    choice: bool,
    destructive_confirm: bool,
    confirm_label: &str,
    gap: &'static str,
) -> Line<'static> {
    let label = format!(" {confirm_label} ");
    Line::from(vec![
        modal_button(" cancel ", !choice),
        Span::raw(gap),
        if destructive_confirm {
            danger_button(&label, choice)
        } else {
            modal_button(&label, choice)
        },
    ])
}

fn modal_button(label: &str, focused: bool) -> Span<'static> {
    if focused {
        Span::styled(
            label.to_string(),
            Style::default().fg(theme::bg()).bg(theme::text_color()),
        )
    } else {
        Span::styled(label.to_string(), theme::dim())
    }
}

/// Destructive variant of `modal_button`: DANGER fg unfocused, inverse DANGER block
/// focused. Same bar-less house style as `modal_button` (no `▐`/`▌`).
fn danger_button(label: &str, focused: bool) -> Span<'static> {
    if focused {
        Span::styled(
            label.to_string(),
            Style::default().fg(theme::bg()).bg(theme::danger_color()),
        )
    } else {
        Span::styled(label.to_string(), theme::danger())
    }
}

fn draw_divergence(frame: &mut Frame<'_>, area: Rect, form: &DivergenceForm) {
    let actions = form.actions();
    let cursor = form.cursor.min(actions.len() - 1);

    let mut lines: Vec<Line<'_>> = vec![Line::from(vec![
        Span::styled("the live login no longer matches ", theme::dim()),
        Span::styled(
            format!("'{}'", form.active),
            Style::default().fg(theme::accent_color()),
        ),
        Span::styled(".", theme::dim()),
    ])];
    if let Some(owner) = &form.sibling {
        lines.push(Line::from(vec![
            Span::styled("it is ", theme::dim()),
            Span::styled(
                format!("'{owner}'"),
                Style::default().fg(theme::accent_color()),
            ),
            Span::styled("'s login.", theme::dim()),
        ]));
    }
    lines.push(Line::from(""));

    for (i, action) in actions.iter().enumerate() {
        let selected = i == cursor;
        lines.push(option_line(
            selected,
            divergence_action_text(action, &form.active),
        ));
    }

    let first = lines.len() - actions.len();
    draw_modal_focused(
        frame,
        area,
        "DIVERGENCE",
        lines,
        first + cursor..first + cursor + 1,
        &form.scroll,
    );
}

fn divergence_action_text(action: &DivergenceAction, active: &str) -> String {
    match action {
        DivergenceAction::SwitchToOwner(owner) => {
            format!("switch to '{owner}' (this login is its account)")
        }
        DivergenceAction::Choice(DivergenceChoice::Overwrite) => {
            format!("overwrite '{active}' with this login")
        }
        DivergenceAction::Choice(DivergenceChoice::NewProfile) => {
            "save this login to another account…".to_string()
        }
        DivergenceAction::Choice(DivergenceChoice::Discard) => {
            format!("discard this login and restore '{active}'")
        }
    }
}

/// Arrow-selected menu row shared by the Divergence and target-picker modals:
/// `❯ ` accent when selected, two-space indent + dim otherwise.
fn option_line(selected: bool, label: String) -> Line<'static> {
    let arrow = if selected {
        Span::styled("\u{276f} ", theme::accent())
    } else {
        Span::raw("  ")
    };
    let style = if selected {
        theme::accent()
    } else {
        theme::dim()
    };
    Line::from(vec![arrow, Span::styled(label, style)])
}

fn draw_divergence_target(frame: &mut Frame<'_>, area: Rect, form: &DivergenceTargetForm) {
    let cursor = form.cursor.min(form.targets.len());

    let mut lines: Vec<Line<'_>> = vec![
        Line::from(Span::styled("where to save the login?", theme::dim())),
        Line::from(""),
        option_line(cursor == 0, "+ new account".to_string()),
    ];
    for (i, name) in form.targets.iter().enumerate() {
        lines.push(option_line(cursor == i + 1, format!("overwrite '{name}'")));
    }

    draw_modal_focused(
        frame,
        area,
        "SAVE LOGIN",
        lines,
        cursor + 2..cursor + 3,
        &form.scroll,
    );
}

fn draw_env_collision(frame: &mut Frame<'_>, area: Rect, form: &EnvCollisionForm) {
    if form.reviewing_overwrite {
        let (_, consequence) = env_collision_option_text(EnvCollisionChoice::Overwrite, form);
        let lines = vec![
            Line::from(Span::styled(
                format!("add '{}' to '{}' ?", form.key, form.profile),
                theme::body(),
            )),
            Line::from(""),
            Line::from(Span::styled(consequence, theme::dim())),
        ];
        // Ratatui has no button-group widget; keep the decision outside the scrolling prose.
        form.max_scroll.set(draw_modal_scrolled(
            frame,
            area,
            "CONFIRM",
            lines,
            form.scroll.get().min(u16::MAX as usize) as u16,
            None,
            Some(ModalExtra::Buttons(
                choice_buttons(form.confirm_overwrite, true, "add", " ")
                    .alignment(Alignment::Right),
            )),
        ));
        return;
    }
    let options = EnvCollisionForm::options();
    let cursor = form.cursor.min(options.len() - 1);

    let mut lines: Vec<Line<'_>> = vec![
        Line::from(vec![
            Span::styled(
                format!("'{}'", form.key),
                Style::default().fg(theme::accent_color()),
            ),
            Span::styled(" is already used by ", theme::dim()),
            Span::styled(form.reason.clone(), theme::body()),
            Span::styled(".", theme::dim()),
        ]),
        Line::from(""),
    ];

    for (i, option) in options.iter().enumerate() {
        let selected = i == cursor;
        let arrow = if selected {
            Span::styled("\u{276f} ", theme::accent())
        } else {
            Span::raw("  ")
        };
        let (label, detail) = env_collision_option_text(*option, form);
        let label_style = if selected {
            theme::accent()
        } else {
            theme::dim()
        };
        lines.push(Line::from(vec![arrow, Span::styled(label, label_style)]));
        lines.push(Line::from(vec![
            Span::raw("    "),
            Span::styled(detail, theme::dim()),
        ]));
    }

    let content_w = lines.iter().map(Line::width).max().unwrap_or(0) as u16;
    let width = (content_w + 6)
        .max("KEY IN USE".len() as u16 + 4)
        .min(area.width.saturating_sub(4));
    let inner_w = width.saturating_sub(6).max(1) as usize;
    let viewport = area.height.saturating_sub(8) as usize;
    let selected_rows = lines[2..4]
        .iter()
        .map(|line| chunk_line(line.clone(), inner_w).len())
        .sum::<usize>();
    form.overwrite_needs_review
        .set(selected_rows > viewport && cursor == 0);
    draw_modal_focused(
        frame,
        area,
        "KEY IN USE",
        lines,
        2 + cursor * 2..4 + cursor * 2,
        &form.scroll,
    );
}

fn env_collision_option_text(
    choice: EnvCollisionChoice,
    form: &EnvCollisionForm,
) -> (String, String) {
    match choice {
        EnvCollisionChoice::Overwrite => (
            "add the custom field anyway".to_string(),
            format!("this account's value overrides {}", form.reason),
        ),
        EnvCollisionChoice::KeepExisting => (
            "keep the existing value".to_string(),
            if form.existing_idx.is_some() {
                "jump to the existing custom field".to_string()
            } else {
                "leave it untouched; don't add the field".to_string()
            },
        ),
        EnvCollisionChoice::Cancel => ("cancel".to_string(), "back out, no change".to_string()),
    }
}

fn draw_capture_name(frame: &mut Frame<'_>, area: Rect, input: &InputState) {
    let lines = vec![
        Line::from(Span::styled(
            "stores the live ~/.claude/.credentials.json under this account.",
            theme::dim(),
        )),
        Line::from(""),
        labelled_input("name", input, true),
    ];

    // Replicate draw_modal's geometry to place the native terminal cursor on the
    // input line (index 2 in the vec).  Chrome = border (1) + padding (2 left, 1 top).
    let title = "CAPTURE";
    let content_w = lines.iter().map(Line::width).max().unwrap_or(0) as u16;
    let w = (content_w + 6)
        .max(title.chars().count() as u16 + 4)
        .min(area.width.saturating_sub(4));
    let h = (lines.len() as u16 + 4).min(area.height.saturating_sub(4));
    // Same geometry as draw_modal so the cursor lands inside the block it draws.
    let rect = centered(area, w, h);
    // inner = rect + border (1) + padding left/top (2, 1)
    let inner_x = rect.x.saturating_add(3);
    let inner_y = rect.y.saturating_add(2);

    draw_modal(frame, area, title, lines);

    // x = edit gutter "✎ " (2) + label "name" (4) + " " (1) + cols before caret
    let cx = inner_x.saturating_add(2 + 4 + 1 + head_cols(input) as u16);
    let cy = inner_y.saturating_add(2); // line index 2
    frame.set_cursor_position((cx, cy));
}

/// The Setup menu's shared name prompt. Same geometry trick as
/// [`draw_capture_name`] — the input is line index 2, so the native cursor can
/// be placed without re-measuring what `draw_modal` already sized.
fn draw_name_prompt(frame: &mut Frame<'_>, area: Rect, form: &NamePromptForm) {
    let lines = vec![
        Line::from(Span::styled(form.action.blurb(), theme::dim())),
        Line::from(""),
        labelled_input("name", &form.input, true),
    ];

    let title = form.action.title();
    let content_w = lines.iter().map(Line::width).max().unwrap_or(0) as u16;
    let w = (content_w + 6)
        .max(title.chars().count() as u16 + 4)
        .min(area.width.saturating_sub(4));
    let h = (lines.len() as u16 + 4).min(area.height.saturating_sub(4));
    let rect = centered(area, w, h);
    let inner_x = rect.x.saturating_add(3);
    let inner_y = rect.y.saturating_add(2);

    draw_modal(frame, area, title, lines);

    // x = edit gutter "✎ " (2) + label "name" (4) + " " (1) + cols before caret
    let cx = inner_x.saturating_add(2 + 4 + 1 + head_cols(&form.input) as u16);
    let cy = inner_y.saturating_add(2);
    frame.set_cursor_position((cx, cy));
}

/// `apply preset` picker. Built-ins lead the list and carry a dim `built-in`
/// tail so the two groups read apart without a second rule.
fn draw_preset_picker(frame: &mut Frame<'_>, area: Rect, form: &PresetPickerForm) {
    let last = form.presets.len().saturating_sub(1);
    let cursor = form.cursor.min(last);

    let mut lines: Vec<Line<'_>> = vec![
        Line::from(Span::styled(
            format!("sets the base url and models on '{}'.", form.target),
            theme::dim(),
        )),
        Line::from(""),
    ];
    for (i, preset) in form.presets.iter().enumerate() {
        let mut line = option_line(i == cursor, preset.name.clone());
        if preset.builtin {
            line.push_span(Span::styled("  built-in", theme::dim()));
        }
        lines.push(line);
    }
    // `d` reaches nothing else from here, so the picker has to teach it.
    lines.push(Line::from(""));
    lines.push(Line::from(prose::spans(
        concat!(key_lit!("d"), " deletes a saved preset"),
        theme::dim(),
    )));

    draw_modal_focused(
        frame,
        area,
        "PRESET",
        lines,
        2 + cursor..3 + cursor,
        &form.scroll,
    );
}

/// Per-tab rows for the KEYS help modal, beneath the shared `tabs`/`global`
/// sections. A standalone builder (not inlined into `draw_help`) so tests can
/// enumerate every tab's real content without rendering a frame — see
/// `every_sub_focus_tab_documents_esc_in_help`.
fn tab_specific_rows(app: &App) -> Vec<(&'static str, &'static [(&'static str, &'static str)])> {
    match app.tab {
        Tab::Overview => vec![(
            "accounts",
            &[
                ("\u{2191} \u{2193}", "move cursor"),
                ("\u{21b5}", "switch to selected account (confirm)"),
                ("shift \u{2191} \u{2193}", "reorder account up / down"),
            ][..],
        )],
        // `n` is the note editor only while an account exists to hold one; on
        // an empty roster the global `n new account` row must survive the
        // shadow filter below, matching the empty state's `n to create one`.
        Tab::Usage if app.usage_detail_focus == StatusFocus::Detail => vec![(
            "usage",
            &[
                ("\u{2191} \u{2193}", "scroll account detail"),
                ("page up", "scroll account detail up a viewport"),
                ("page down", "scroll account detail down a viewport"),
                ("esc", "return to account selector"),
                ("\u{2190}", "return to account selector"),
                ("r", "refresh account"),
                ("n", "edit the account's note"),
                ("e", "toggle estimates"),
                ("p", "toggle pace marker"),
            ][..],
        )],
        Tab::Usage if app.profile_count() > 0 => vec![(
            "usage",
            &[
                ("\u{2191} \u{2193}", "pick account to inspect"),
                ("\u{21b5}", "open account detail"),
                ("r", "refresh account"),
                ("n", "edit the account's note"),
                ("e", "toggle estimates"),
                ("p", "toggle pace marker"),
            ][..],
        )],
        Tab::Usage => vec![(
            "usage",
            &[
                ("\u{2191} \u{2193}", "pick account to inspect"),
                ("e", "toggle estimates"),
                ("p", "toggle pace marker"),
            ][..],
        )],
        Tab::Tokens if app.token_view == TokenView::Dashboard && token_model_count(app) == 0 => {
            vec![(
                "tokens",
                &[
                    ("c", "count cache in token figures"),
                    (
                        "t",
                        "cycle period \u{b7} lifetime / daily / weekly / monthly",
                    ),
                    ("r", "reload on-disk stats"),
                ][..],
            )]
        }
        Tab::Tokens if app.token_view == TokenView::Dashboard => vec![(
            "tokens",
            &[
                ("\u{21b5}", "open per-model breakdown"),
                ("c", "count cache in token figures"),
                (
                    "t",
                    "cycle period \u{b7} lifetime / daily / weekly / monthly",
                ),
                ("r", "reload on-disk stats"),
            ][..],
        )],
        Tab::Tokens if app.model_detail_focus == StatusFocus::Detail => vec![(
            "tokens",
            &[
                ("\u{2191} \u{2193}", "scroll model detail"),
                ("page up", "scroll model detail up a viewport"),
                ("page down", "scroll model detail down a viewport"),
                ("esc", "return to model selector"),
                ("\u{2190}", "return to model selector"),
                ("c", "count cache in token figures"),
                (
                    "t",
                    "cycle period \u{b7} lifetime / daily / weekly / monthly",
                ),
                ("r", "reload on-disk stats"),
            ][..],
        )],
        Tab::Tokens if token_model_count(app) == 0 => vec![(
            "tokens",
            &[
                ("\u{2191} \u{2193}", "pick model"),
                ("esc", "back to dashboard"),
                ("c", "count cache in token figures"),
                (
                    "t",
                    "cycle period \u{b7} lifetime / daily / weekly / monthly",
                ),
                ("r", "reload on-disk stats"),
            ][..],
        )],
        Tab::Tokens => vec![(
            "tokens",
            &[
                ("\u{2191} \u{2193}", "pick model"),
                ("\u{21b5}", "open model detail"),
                ("esc", "back to dashboard"),
                ("c", "count cache in token figures"),
                (
                    "t",
                    "cycle period \u{b7} lifetime / daily / weekly / monthly",
                ),
                ("r", "reload on-disk stats"),
            ][..],
        )],
        Tab::Setup => vec![(
            "setup",
            &[
                ("\u{2191} \u{2193}", "pick account / + new, then a row"),
                ("\u{21b5}", "open settings · edit field · flip toggle"),
                (
                    "\u{21b5} on a field",
                    concat!("edit inline; ", key_lit!("\u{21b5}"), " again saves"),
                ),
                ("space", "cycle the model preset (model row)"),
                ("+ / -", "step alert at by 5"),
                (
                    "env",
                    concat!("+ add env · ", key_lit!("\u{21b5}"), " edits a value"),
                ),
                (
                    "a",
                    "duplicate the account \u{b7} save it as a preset \u{b7} apply one",
                ),
                (
                    "disable / enable",
                    concat!(
                        key_lit!("\u{21b5}"),
                        " arms disable, again confirms \u{b7} enable is one press \u{b7} inert while active or a live session is open",
                    ),
                ),
                (
                    "delete",
                    concat!(key_lit!("\u{21b5}"), " once to arm, again to confirm"),
                ),
                ("esc", "stop editing / back to account list"),
            ][..],
        )],
        Tab::Config => vec![(
            "config",
            &[
                ("\u{2191} \u{2193}", "move between settings"),
                ("space", "cycle the focused setting"),
                (
                    "\u{21b5}",
                    concat!(
                        "same as ",
                        key_lit!("space"),
                        " · type a value on refresh or weekly limit",
                    ),
                ),
            ][..],
        )],
        Tab::Status => vec![(
            "status",
            &[
                ("\u{2191} \u{2193}", "pick incident / scroll detail"),
                ("\u{21b5}", "open incident timeline"),
                ("r", "refresh the feed"),
                ("esc", "back to the list"),
            ][..],
        )],
        Tab::Services => vec![(
            "services",
            &[
                (
                    "\u{2191} \u{2193}",
                    "pick row · scroll detail · walk plugin problems · walk herdr options",
                ),
                ("\u{21b5}", "open detail · activate an option"),
                ("space", "activate the focused herdr option"),
                ("+ / -", "step the tag refresh"),
                ("f", "apply the focused fix"),
                ("r", "re-run all checks"),
                ("esc", "back to the list · close the editor"),
            ][..],
        )],
        Tab::Fallback => vec![(
            "fallback chain",
            &[
                ("\u{2191} \u{2193}", "move cursor / detail row"),
                ("shift \u{2191} \u{2193}", "reorder to set priority"),
                (
                    "\u{21b5}",
                    "open \u{00b7} edit threshold \u{00b7} edit weekly at \u{00b7} edit max spend \u{00b7} toggle gates / last resort \u{00b7} remove \u{00b7} add",
                ),
                ("+ / -", "step rotate at / weekly at by 5"),
                (
                    "\u{21b5} on rotate at",
                    concat!("type a value, ", key_lit!("\u{21b5}"), " saves"),
                ),
                ("\u{21b5} on weekly at", "type a %, empty clears"),
                (
                    "space on preferred days",
                    "step never / weekdays / weekends / every day",
                ),
                (
                    "\u{21b5} on preferred days",
                    concat!(
                        "pick days: ",
                        key_lit!("\u{2190} \u{2192}"),
                        " walk \u{00b7} ",
                        key_lit!("space"),
                        " toggles and saves \u{00b7} ",
                        key_lit!("\u{21b5} esc q"),
                        " leave \u{00b7} ",
                        key_lit!("\u{2191} \u{2193}"),
                        " leave and move",
                    ),
                ),
                ("esc", "back / cancel edit"),
            ][..],
        )],
    }
}

fn draw_help(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let title = "KEYS";

    let tab_specific = tab_specific_rows(app);

    // In a descended read-only pane, arrows do not switch tabs; Tab/BackTab still do.
    let nav: &[(&str, &str)] = if (app.tab == Tab::Usage
        && app.usage_detail_focus == StatusFocus::Detail)
        || (app.tab == Tab::Tokens
            && app.token_view == TokenView::Models
            && app.model_detail_focus == StatusFocus::Detail)
    {
        &[
            ("\u{2190}", "return to selector"),
            ("\u{2192}", "stays on detail"),
            ("tab", "next tab"),
            ("shift tab", "previous tab"),
        ]
    } else {
        &[(
            "\u{2190} \u{2192} \u{00b7} tab",
            concat!(
                "previous / next tab (",
                key_lit!("shift tab"),
                ": previous)"
            ),
        )]
    };

    // The modal's own keys, in their own section. Not folded into `global`
    // below: every tab binds ↑↓ for its own list, so the shadow filter there
    // would drop this row on all eight — and hanging it off another section
    // would put two ↑↓ rows with different senses a few lines apart, the exact
    // contradiction that filter exists to prevent.
    // The close keys were documented only at app level, where `q` reads
    // "back / quit" and `esc` reads "back within a sub-view" — neither tells a
    // reader how to dismiss what they are looking at.
    let modal_keys: &[(&str, &str)] = &[
        ("\u{2191} \u{2193}", "scroll"),
        ("esc \u{00b7} q \u{00b7} ?", "close"),
    ];

    let global_all: &[(&str, &str)] = &[
        ("n", "new account"),
        ("r", "refresh usage now"),
        ("t", "rotate all tokens"),
        ("d", "resolve credential divergence (when flagged)"),
        ("?", "toggle this help"),
        ("a", "actions"),
        ("x", "dismiss toast / alert"),
        ("q", "back / quit"),
        ("esc", "back within a sub-view (no-op at the top level)"),
        ("\u{2303}c", "quit from anywhere"),
    ];
    // A key the current tab redefines is documented in its own section above;
    // keeping the global sense too would contradict it (`t` cycles the period
    // and `r` reloads stats on Tokens, `r` refreshes one account on Usage).
    let shadowed: Vec<&str> = tab_specific
        .iter()
        .flat_map(|(_, rows)| rows.iter().map(|(k, _)| *k))
        .collect();
    let global: Vec<(&str, &str)> = global_all
        .iter()
        .copied()
        .filter(|(k, _)| !shadowed.contains(k))
        .collect();

    let mut lines: Vec<Line<'_>> = Vec::new();
    lines.extend(key_section("tabs", nav));
    lines.extend(key_section("this modal", modal_keys));
    for (section, entries) in &tab_specific {
        lines.extend(key_section(section, entries));
    }
    lines.extend(key_section("global", &global));
    // Last: the keys are what a reader came for, so they stay in view on a
    // terminal too short for the whole modal. ↑↓ reaches the legend from there.
    lines.extend(glyph_section("glyphs", glyph_rows()));
    lines.pop(); // trim trailing blank from last section
    app.help_max_scroll.set(draw_modal_scrolled(
        frame,
        area,
        title,
        lines,
        app.help_scroll,
        None,
        None,
    ));
}

/// Legend for the 1-cell marks the account surfaces carry, with no key of their
/// own to document them: the Overview row's leading `●` and `⇄`, the Overview
/// chain's projected-switch `↲`, and every blocked-reason marker on the
/// Fallback chain.
///
/// Each blocked-reason row takes its glyph AND its hue from [`reason_marker`]
/// itself, so the legend cannot drift from what the chain renders. `⊖` and `⊘`
/// each appear twice because they split their two senses on hue alone (see
/// `chain::reason_marker` for why), and a legend naming one sense per glyph
/// would be worse than none. `×` and `⋯` appear twice at one hue: the card's
/// pill tells their two senses apart.
fn glyph_rows() -> Vec<(Span<'static>, &'static str)> {
    let reason = |r: BlockedReason, desc| (reason_marker(&r), desc);
    vec![
        (
            Span::styled("\u{25cf}", Style::default().fg(theme::accent_2_color())),
            "the active account",
        ),
        (
            Span::styled("\u{21c4}", theme::dim()),
            "a live session here follows the fallback chain",
        ),
        (switch_mark(), "the chain switches to this account next"),
        reason(BlockedReason::Disabled, DIAG_DISABLED),
        reason(BlockedReason::Canceled, DIAG_CANCELED),
        reason(BlockedReason::AuthBroken, DIAG_AUTH_BROKEN),
        reason(BlockedReason::KeyRejected, DIAG_KEY_REJECTED),
        reason(
            BlockedReason::WeeklySpent { resets_in: None },
            DIAG_WEEKLY_SPENT,
        ),
        reason(BlockedReason::KickRejected { lifts_in: 0 }, DIAG_KICK),
        reason(BlockedReason::BudgetSpent, DIAG_BUDGET_SPENT),
        reason(
            BlockedReason::FiveHour {
                pct: 0.0,
                resets_in: None,
            },
            "5h window spent",
        ),
        reason(
            BlockedReason::ScopedSpent {
                label: String::new(),
                pct: 0.0,
            },
            "one model's week spent, other models ok",
        ),
        reason(BlockedReason::WeeklySoft { pct: 0.0 }, DIAG_WEEKLY_SOFT),
        reason(BlockedReason::Stale, DIAG_STALE),
        reason(BlockedReason::NoUsage, DIAG_NO_USAGE),
    ]
}

fn key_section(title: &str, pairs: &[(&str, &str)]) -> Vec<Line<'static>> {
    let mut lines = section_head(title);
    for (key, desc) in pairs {
        lines.push(help_row(key, desc));
    }
    lines.push(Line::from(""));
    lines
}

fn glyph_section(title: &str, rows: Vec<(Span<'static>, &'static str)>) -> Vec<Line<'static>> {
    let mut lines = section_head(title);
    lines.extend(rows.into_iter().map(|(mark, desc)| glyph_row(mark, desc)));
    lines.push(Line::from(""));
    lines
}

fn section_head(title: &str) -> Vec<Line<'static>> {
    vec![
        Line::from(Span::styled(
            title.to_uppercase(),
            Style::default().fg(theme::text_dim_color()),
        )),
        Line::from(""),
    ]
}

const HELP_KEY_W: usize = 18;
const HELP_KEY_GUTTER: usize = 2;

fn help_row(key: &str, desc: &str) -> Line<'static> {
    let mut spans = vec![Span::styled(
        format!("  {}", key_cell(key, HELP_KEY_W, HELP_KEY_GUTTER)),
        Style::default().fg(theme::accent_color()).bold(),
    )];
    spans.extend(prose::spans(desc, Style::default().fg(theme::text_color())));
    Line::from(spans)
}

/// A legend row, aligned to the same description column the key rows open at.
/// The mark keeps the style its renderer gave it — a hue-split glyph read in
/// the accent every other key row wears would document the wrong thing.
fn glyph_row(mark: Span<'static>, desc: &str) -> Line<'static> {
    Line::from(vec![
        Span::raw("  "),
        mark,
        // The mark already spent one of the key cell's columns.
        Span::raw(key_cell("", HELP_KEY_W - 1, HELP_KEY_GUTTER)),
        Span::styled(desc.to_string(), Style::default().fg(theme::text_color())),
    ])
}

fn labelled_input(label: &str, input: &InputState, focused: bool) -> Line<'static> {
    // When focused the native terminal cursor owns the caret — no block highlight.
    // Unfocused fields still render with plain text styling (no BG_SUNKEN tint).
    let value_style = if focused {
        Style::default()
            .fg(theme::text_color())
            .bg(theme::bg_sunken())
    } else {
        Style::default().fg(theme::text_color())
    };
    labelled_line(
        label,
        Span::styled(input.value.clone(), value_style),
        focused,
    )
}

/// A modal form row: the gutter, the label, one space, then `value`. A focused
/// row carries the `✎` edit-mode gutter glyph (same as form rows); the 2-col
/// gutter is accounted for in the caller's cursor-x math.
fn labelled_line(label: &str, value: Span<'static>, focused: bool) -> Line<'static> {
    let gutter = if focused {
        Span::styled(format!("{} ", theme::edit_glyph()), theme::accent().bold())
    } else {
        Span::raw("  ")
    };
    Line::from(vec![
        gutter,
        Span::styled(label.to_string(), theme::label()),
        Span::raw(" "),
        value,
    ])
}

fn draw_action_menu(frame: &mut Frame<'_>, area: Rect, state: &ActionMenuState) {
    const HOTKEY_W: u16 = 1; // 1 char for hotkey letter, or 1 space if none
    const GUTTER: u16 = 2; // "❯ " or "  "

    // The rule between the account-scoped items and the tab-global ones. Only
    // when both groups exist — a one-group menu needs nothing separated.
    let rule_at = (state.scoped_len > 0 && state.scoped_len < state.items.len())
        .then_some(state.scoped_len as u16);

    // Render rows with right-aligned hotkeys — can't use draw_modal because that
    // wraps all lines in one Paragraph, preventing per-row background tinting.
    // Custom draw: measure → size → clear → border → per-row widgets.
    let max_label_w = state
        .items
        .iter()
        .map(|item| item.label.chars().count())
        .max()
        .unwrap_or(0) as u16;
    let content_w = GUTTER + max_label_w + 3 + HOTKEY_W;
    let title = "actions";
    // Both border breaks have to fit: ` ACTIONS ` on the left, ` <account> ` on
    // the right, 2 corners and at least one dash of chrome between them.
    let title_w = title.chars().count() as u16
        + 2
        + state
            .context
            .as_ref()
            .map_or(0, |c| c.chars().count() as u16 + 2);
    let w = (content_w + 6)
        .max(title_w + 3)
        .min(area.width.saturating_sub(4));
    // items rows + the rule + 4 chrome (border + padding)
    let h = (state.items.len() as u16 + u16::from(rule_at.is_some()) + 4)
        .min(area.height.saturating_sub(4));

    let rect = centered(area, w, h);
    frame.render_widget(Clear, rect);
    let block = modal_block_with_meta(title, state.context.as_deref());
    let inner = block.inner(rect);
    frame.render_widget(block, rect);

    let inner_w = inner.width;
    let total = state.items.len() + usize::from(rule_at.is_some());
    let viewport = inner.height as usize;
    let selected = state.cursor.min(state.items.len().saturating_sub(1));
    let focus = selected + usize::from(rule_at.is_some_and(|at| selected >= at as usize));
    let offset = follow_scroll_offset(total, viewport, (focus, focus + 1), state.scroll.get());
    state.scroll.set(offset);
    draw_scrollbar(frame, inner, total, offset, viewport);
    if let Some(at) = rule_at {
        let y = (at as usize).saturating_sub(offset);
        if (at as usize) >= offset && y < viewport {
            let y = inner.y + y as u16;
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    "─".repeat(inner.width as usize),
                    theme::line(),
                )))
                .style(theme::base()),
                Rect {
                    y,
                    height: 1,
                    ..inner
                },
            );
        }
    }

    for (i, item) in state.items.iter().enumerate() {
        let focused = i == state.cursor;
        // Rows below the rule sit one further down; the cursor never lands on
        // it, so `items` stays the only index anything else needs.
        let row = i + usize::from(rule_at.is_some_and(|at| i >= at as usize));
        if row < offset || row >= offset + viewport {
            continue;
        }
        let y = inner.y + (row - offset) as u16;
        let row_area = Rect {
            y,
            height: 1,
            ..inner
        };

        let label_style = bold_when(Style::default().fg(theme::text_color()), focused);
        let row_bg = if focused {
            Style::default().bg(theme::bg_hover())
        } else {
            theme::base()
        };
        let glyph = if focused {
            Span::styled("❯ ", Style::default().fg(theme::accent_color()).bold())
        } else {
            Span::styled("  ", Style::default())
        };
        let hotkey_width = usize::from(inner_w >= GUTTER + HOTKEY_W + 3);
        let label_width = (inner_w as usize).saturating_sub(GUTTER as usize + 3 + hotkey_width);
        let label = crate::format::truncate(item.label, label_width);
        let label_len = label.chars().count() as u16;
        let pad = inner_w
            .saturating_sub(GUTTER)
            .saturating_sub(label_len)
            .saturating_sub(hotkey_width as u16);
        let padding = Span::styled(" ".repeat(pad as usize), Style::default());
        let hotkey_span = Span::styled(
            if hotkey_width == 0 { None } else { item.hotkey }
                .map_or_else(String::new, |c| c.to_string()),
            Style::default().fg(theme::text_dim_color()),
        );
        let line = Line::from(vec![
            glyph,
            Span::styled(label, label_style),
            padding,
            hotkey_span,
        ])
        .style(row_bg);
        frame.render_widget(Paragraph::new(line).style(row_bg), row_area);
    }
}

#[cfg(test)]
#[path = "../../../tests/inline/tui_render_modals.rs"]
mod tests;
