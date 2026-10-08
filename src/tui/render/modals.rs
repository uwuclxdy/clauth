//! Modal dialogs — stacking layer above the screen.

use ratatui::Frame;
use ratatui::layout::{Alignment, Position, Rect};
use ratatui::style::Style;
use ratatui::symbols::border;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Padding, Paragraph};

use crate::profile::DivergenceChoice;

use super::super::app::{
    ActionMenuState, App, ConfirmAction, ConfirmState, DivergenceAction, DivergenceForm,
    DivergenceTargetForm, EnvCollisionChoice, EnvCollisionForm, InputState, LoginMethod,
    LoginStage, LossWarningState, Modal, NamePromptForm, PaneState, PresetApplyButton,
    PresetApplyForm, PresetPickerForm, RebindForm, ScrollStamp, StatusFocus, Tab, TokenPickerForm,
    TokenView, pane_state, token_model_count,
};
use super::super::theme;
use super::chain::reason_marker;
use super::format::{spinner_frame, truncate};
use super::overview::switch_mark;
use super::panes::{
    DIAG_AUTH_BROKEN, DIAG_BUDGET_SPENT, DIAG_CANCELED, DIAG_DISABLED, DIAG_KEY_REJECTED,
    DIAG_KICK, DIAG_NO_USAGE, DIAG_STALE, DIAG_WEEKLY_SOFT, DIAG_WEEKLY_SPENT, META_RULE_MIN,
    bold_when, draw_scrollbar, draw_scrolled_lines, field_window, follow_scroll_offset,
    follow_scroll_offset_tail, head_cols, key_cell, meta_line, place_caret, right_meta_fits,
    title_label,
};
use super::prose::{self, cmd_lit, key_lit};
use crate::fallback::BlockedReason;

/// The stamp a published scroll bound carries: the frame area it was computed
/// for plus the modal-open generation, so the footer can tell this frame's
/// bound from a resize's or a reopen's leftover.
fn stamp(area: Rect, app: &App) -> ScrollStamp {
    ScrollStamp {
        width: area.width,
        height: area.height,
        generation: app.modal_generation.get(),
    }
}

pub(super) fn draw(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &App,
    modal: &Modal,
    toast_rects: &[Rect],
) {
    match modal {
        Modal::Confirm(state) => {
            draw_confirm(frame, area, state);
            state.scroll.stamp.set(stamp(area, app));
        }
        Modal::Divergence(form) => draw_divergence(frame, area, form),
        Modal::CaptureName(form) => draw_capture_name(frame, area, &form.input, toast_rects),
        Modal::NamePrompt(form) => draw_name_prompt(frame, area, form, toast_rects),
        Modal::PresetPicker(form) => draw_preset_picker(frame, area, form),
        Modal::PresetApply(form) => {
            draw_preset_apply(frame, area, form);
            form.scroll.stamp.set(stamp(area, app));
        }
        Modal::DivergenceTarget(form) => draw_divergence_target(frame, area, form),
        Modal::TokenPicker(form) => draw_token_picker(frame, area, form),
        Modal::Help => {
            draw_help(frame, area, app);
            app.help_max_scroll_stamp.set(stamp(area, app));
        }
        Modal::ActionMenu(state) => draw_action_menu(frame, area, state),
        Modal::EnvCollision(form) => {
            draw_env_collision(frame, area, form);
            // Only a review-stage frame published a bound: the list stage must
            // not re-stamp the review stage's leftover, or a resize while the
            // list is open would read the old bound as this frame's.
            if form.reviewing_overwrite {
                form.max_scroll_stamp.set(stamp(area, app));
            }
        }
        Modal::Login => draw_login_progress(frame, area, app, toast_rects),
        Modal::CodexLogin => draw_codex_login_progress(frame, area, app),
        Modal::CheckOutput(body) => {
            draw_check_output(frame, area, body, app);
            app.check_output_max_scroll_stamp.set(stamp(area, app));
        }
        Modal::Rebind(form) => draw_rebind(frame, area, form),
        Modal::LossWarning(state) => draw_loss_warning(frame, area, state),
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
fn draw_login_progress(frame: &mut Frame<'_>, area: Rect, app: &App, toast_rects: &[Rect]) {
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
    let mut field = None;
    if let Some(shunt) = app.shunt_paste.as_ref() {
        // The shunt add: `r`/`c` only once the authorize URL is in (the codex
        // fold's shape), `p` only once the URL is in AND no paste has been sent
        // down the door — no code can exist before the link, so before the
        // announce the modal reads `opening your browser…` alone.
        match &session.url {
            Some(_) => {
                lines.push(key_row("r", "  open the browser again"));
                lines.push(key_row("c", "  copy link"));
            }
            None => lines.push(Line::from(Span::styled(
                "opening your browser…",
                theme::dim(),
            ))),
        }
        match session.paste_field.as_ref() {
            Some(input) => {
                field = Some(ModalExtra::Field {
                    at: lines.len(),
                    label: CODE_LABEL,
                    input,
                });
                lines.push(code_field_row(input));
            }
            None if !shunt.sent && session.url.is_some() => {
                lines.push(key_row("p", "  paste code"))
            }
            None => {}
        }
    } else if session.open_door().is_some() {
        lines.push(key_row("r", "  open the browser again"));
        lines.push(key_row("c", "  copy link"));
        match session.paste_field.as_ref() {
            Some(input) => {
                field = Some(ModalExtra::Field {
                    at: lines.len(),
                    label: CODE_LABEL,
                    input,
                });
                lines.push(code_field_row(input));
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
        ModalTail {
            extra: field,
            toast_rects,
        },
    );
}

/// The login modal's code field label.
const CODE_LABEL: &str = "code";

/// The login modal's code field, in place of the `p  paste code` row: an edit
/// gutter, the `code` label, then the typed text verbatim, or the placeholder
/// while there is none.
fn code_field_row(field: &InputState) -> Line<'static> {
    if field.value.is_empty() {
        labelled_line(
            CODE_LABEL,
            Span::styled("(paste it here)", theme::faint()),
            true,
        )
    } else {
        labelled_input(CODE_LABEL, field, true)
    }
}

/// The codex re-login modal: the claude login shell minus the paste door. It
/// waits on the browser callback alone (`r` opens the browser again, `c` copies
/// the authorize link), so there is no code field and no stage ladder. `r`/`c`
/// render only once the worker announced the URL — `opening your browser…`
/// before (the claude modal's console-login shape).
fn draw_codex_login_progress(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let Some(session) = app.codex_login.as_ref() else {
        return; // the login ended this frame; the modal pops on the next drain
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
            Span::styled("continue in your browser", theme::dim()),
        ]),
        Line::from(""),
    ];
    match session.url {
        Some(_) => {
            lines.push(Line::from(vec![
                Span::styled("r", theme::accent().bold()),
                Span::styled("  open the browser again", theme::dim()),
            ]));
            lines.push(Line::from(vec![
                Span::styled("c", theme::accent().bold()),
                Span::styled("  copy link", theme::dim()),
            ]));
        }
        None => lines.push(Line::from(Span::styled(
            "opening your browser…",
            theme::dim(),
        ))),
    }
    draw_modal_scrolled(
        frame,
        area,
        "LOGIN",
        lines,
        0,
        None,
        ModalTail {
            extra: None,
            toast_rects: &[],
        },
    );
}

/// The modal width cap: 60% of the terminal width (integer floor) from 80
/// columns up, `area.width - 4` below — a short modal still fits its content.
fn modal_width_cap(area_w: u16) -> u16 {
    if area_w >= 80 {
        (u32::from(area_w) * 60 / 100) as u16
    } else {
        area_w.saturating_sub(4)
    }
}

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let w = width.min(modal_width_cap(area.width));
    let h = height.min(area.height.saturating_sub(4));
    Rect {
        x: area.x + (area.width.saturating_sub(w)) / 2,
        y: area.y + (area.height.saturating_sub(h)) / 2,
        width: w,
        height: h,
    }
}

/// The focus-following modal's scroll/viewport cells and, for option lists,
/// the geometry to publish — bundled so [`draw_modal_focused`] stays under the
/// argument-count lint.
struct ModalFollow<'a> {
    saved: &'a std::cell::Cell<usize>,
    viewport: &'a std::cell::Cell<usize>,
    geometry: Option<ModalGeometry<'a>>,
}

fn draw_modal_focused(
    frame: &mut Frame<'_>,
    area: Rect,
    title: &str,
    lines: Vec<Line<'_>>,
    focus: std::ops::Range<usize>,
    follow: ModalFollow<'_>,
) {
    let d = draw_modal_scrolled(
        frame,
        area,
        title,
        lines,
        0,
        Some((focus, follow.saved, FocusEnd::Head)),
        ModalTail {
            extra: None,
            toast_rects: &[],
        },
    );
    follow.viewport.set(d.viewport as usize);
    if let Some(g) = follow.geometry {
        let starts: Vec<usize> = (0..g.count).map(|i| d.line_starts[g.base + i]).collect();
        let block_ends: Vec<usize> = starts
            .iter()
            .enumerate()
            .map(|(i, _)| {
                if i + 1 < g.count {
                    d.line_starts[g.base + i + 1]
                } else {
                    d.total
                }
            })
            .collect();
        g.total.set(d.total);
        g.starts.replace(starts);
        g.block_ends.replace(block_ends);
    }
}

/// Modal sized to content: snaps to widest line/title, exact line count.
/// Chrome = rounded border (1) + `Padding::new(2,2,1,1)` = 6 cols, 4 rows.
///
/// A line wider than the modal's inner width (a capped modal, or a terminal
/// too narrow for the content) is pre-split into inner-width rows by
/// [`chunk_line`] — between words, a word wider than the row still splitting
/// by grapheme — so the height is exact by construction; ratatui's own
/// word-wrap may use MORE rows than any cheap estimate and silently clip the
/// tail. On a modal wide enough for the content nothing splits.
///
/// The content scrolls to start at row `scroll`, returning
/// a [`ModalDraw`] — the largest offset the content allows (so a caller holding
/// the offset can clamp its key handler), the visible row count the render
/// actually drew, the chunked start row of each input line, and the total line
/// count (so a key handler's page step tracks a resize and each wrapped
/// option's spans).
/// A `Field` names the line holding a [`labelled_input`] text field: once the
/// modal's width is known, that line is redrawn with the value scrolled inside
/// the cells the field gets ([`field_window`]), so it never folds into rows,
/// and the native terminal cursor lands on its caret on whichever row the
/// chunking put it, or nowhere when that row scrolled out of the viewport.
///
/// A terminal too short for the whole modal used to drop the tail with nothing
/// on screen saying so. The rows now go through the shared scrolled-lines
/// helper, which draws the overflow scrollbar, the one legal overflow signal on
/// a surface that scrolls. The focus block handed to it is the viewport window
/// itself — "keep rows `scroll..scroll + viewport` on screen" — which resolves
/// to exactly `scroll` once clamped. A modal that fits scrolls by 0 and draws
/// no bar, so it renders as before.
enum ModalExtra<'a> {
    Field {
        at: usize,
        label: &'a str,
        input: &'a InputState,
    },
    Buttons(Line<'a>),
}

/// What [`draw_modal_scrolled`] reports back: the scroll bound and viewport,
/// plus the chunked geometry the page step needs — the start row of each
/// pre-chunk line and the total chunked line count.
struct ModalDraw {
    max_scroll: u16,
    viewport: u16,
    line_starts: Vec<usize>,
    total: usize,
}

/// The option list a focus-following modal publishes for the page step: the
/// pre-chunk line index its first option sits on, the option count, and the
/// cells the chunked starts/ends/total land in.
struct ModalGeometry<'a> {
    base: usize,
    count: usize,
    starts: &'a std::cell::RefCell<Vec<usize>>,
    block_ends: &'a std::cell::RefCell<Vec<usize>>,
    total: &'a std::cell::Cell<usize>,
}

/// Which end of a focus range stays visible when it is taller than the
/// viewport: every option list keeps its head (the row's start); the re-bind
/// confirm keeps its tail (the cursor's toggle line), so `space` always flips
/// a visible account.
#[derive(Clone, Copy, PartialEq, Eq)]
enum FocusEnd {
    Head,
    Tail,
}

/// The trailing inputs [`draw_modal_scrolled`] takes past the argument-count
/// lint: the optional field/buttons extra, and the toast rects a field caret
/// must not land under.
struct ModalTail<'a> {
    extra: Option<ModalExtra<'a>>,
    toast_rects: &'a [Rect],
}

fn draw_modal_scrolled(
    frame: &mut Frame<'_>,
    area: Rect,
    title: &str,
    lines: Vec<Line<'_>>,
    scroll: u16,
    focus: Option<(std::ops::Range<usize>, &std::cell::Cell<usize>, FocusEnd)>,
    tail: ModalTail<'_>,
) -> ModalDraw {
    let content_w = lines
        .iter()
        .map(|line| normalize_tabs(line).width())
        .max()
        .unwrap_or(0) as u16;
    let button_w = match &tail.extra {
        Some(ModalExtra::Buttons(buttons)) => buttons.width() as u16,
        _ => 0,
    };
    let w = (content_w.max(button_w) + 6)
        .max(modal_title_w(title))
        .min(modal_width_cap(area.width));
    let inner_w = (w.saturating_sub(6) as usize).max(1);
    let line_count = lines.len();
    let mut caret_row = None;
    let mut focus_rows = (0, 0);
    let mut line_starts: Vec<usize> = Vec::with_capacity(line_count);
    let mut rows: Vec<Line<'static>> = Vec::new();
    for (i, mut line) in lines.into_iter().enumerate() {
        if let Some(ModalExtra::Field { at, label, input }) = &tail.extra
            && *at == i
        {
            let lead = labelled_lead_w(label);
            // A content-sized modal is exactly as wide as a fitting field, so
            // the End caret parks in the right padding rather than scroll it.
            let value = field_window(input, inner_w.saturating_sub(lead), 1);
            if !input.value.is_empty() {
                line = labelled_value(label, value, true);
            }
            caret_row = Some((rows.len(), lead + head_cols(input)));
        }
        if focus.as_ref().is_some_and(|(range, _, _)| range.start == i) {
            focus_rows.0 = rows.len();
        }
        if focus.as_ref().is_some_and(|(range, _, _)| range.end == i) {
            focus_rows.1 = rows.len();
        }
        line_starts.push(rows.len());
        rows.extend(chunk_line(line, inner_w));
    }
    if focus
        .as_ref()
        .is_some_and(|(range, _, _)| range.end >= line_count)
    {
        focus_rows.1 = rows.len();
    }
    let lines: Vec<Line<'static>> = rows
        .into_iter()
        .map(|mut line| {
            // A selected option row carries the BG_HOVER tint on its spans; pad
            // it to the modal's full content width with an explicit tinted
            // filler so the tint reaches the padding boundary (a `Line::style`
            // tint stops at the last span).
            let tinted = line
                .spans
                .iter()
                .any(|s| s.style.bg == Some(theme::bg_hover()));
            if tinted {
                let fill = inner_w.saturating_sub(line.width());
                if fill > 0 {
                    line.push_span(Span::styled(
                        " ".repeat(fill),
                        Style::default().bg(theme::bg_hover()),
                    ));
                }
            }
            line
        })
        .collect();
    let total = lines.len();
    // A too-narrow choice (3 spans) stacks into two rows; a narrower one-span
    // line (an `Acknowledge` confirm's ` close `) renders as one row — reserve
    // only what the branch below actually draws.
    let button_rows = match &tail.extra {
        Some(ModalExtra::Buttons(buttons))
            if buttons.width() > inner_w && buttons.spans.len() >= 3 =>
        {
            3
        }
        Some(ModalExtra::Buttons(_)) => 2,
        _ => 0,
    };
    let h = (lines.len() as u16 + 4 + button_rows).min(area.height.saturating_sub(4));

    let rect = centered(area, w, h);
    frame.render_widget(Clear, rect);
    let block = modal_block(title, rect.width);
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
    let scroll = if let Some((_, saved, end)) = focus {
        let offset = match end {
            FocusEnd::Head => follow_scroll_offset(lines.len(), viewport, focus_rows, saved.get()),
            FocusEnd::Tail => {
                follow_scroll_offset_tail(lines.len(), viewport, focus_rows, saved.get())
            }
        };
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
    if let Some(ModalExtra::Buttons(buttons)) = &tail.extra {
        let button_area = Rect {
            y: inner.y + inner.height.saturating_sub(button_rows.saturating_sub(1)),
            height: button_rows.saturating_sub(1),
            ..inner
        };
        if buttons.width() > inner_w {
            match &buttons.spans[..] {
                // No button widget can stack the choice while keeping each
                // complete on a narrow screen: the cancel/affirmative pair
                // stacks into two rows.
                [cancel, _, add] => {
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
                }
                // A one-span line (an `Acknowledge` confirm's ` close `) has no
                // pair to stack; render it as a plain right-aligned paragraph.
                _ => frame.render_widget(Paragraph::new(buttons.clone()), button_area),
            }
        } else {
            frame.render_widget(Paragraph::new(buttons.clone()), button_area);
        }
    }
    if let Some((row, cell)) = caret_row
        && (scroll..scroll + viewport).contains(&row)
    {
        let caret = (
            inner.x.saturating_add(cell as u16),
            inner.y.saturating_add((row - scroll) as u16),
        );
        // A toast drawn over the caret hides it this frame (the shared placer
        // every editor routes through): the rects were laid out before this
        // modal drew, so the check sees the final stack.
        place_caret(frame, tail.toast_rects, Position::new(caret.0, caret.1));
    }
    ModalDraw {
        max_scroll,
        viewport: viewport as u16,
        line_starts,
        total,
    }
}

/// A tab in a modal body is one break space: ratatui drops control characters
/// from both its grapheme iterator and the buffer, so a raw tab would glue its
/// neighbours, never wrap, and render as nothing. Map `\t` to one space up
/// front so the width, the break logic and the render all agree.
fn normalize_tabs(line: &Line<'_>) -> Line<'static> {
    let mut out = Line::from(
        line.spans
            .iter()
            .map(|s| Span::styled(s.content.replace('\t', " "), s.style))
            .collect::<Vec<_>>(),
    )
    .style(line.style);
    out.alignment = line.alignment;
    out
}

/// Split one styled line into `w`-column rows between words, measured in
/// display cells, preserving span styles and the line's alignment/style: modal
/// bodies can carry a failed check's real stderr, and a CJK or emoji grapheme
/// counted as one char would overrun its row, which ratatui cuts with no `…`.
/// A word wider than `w` splits by grapheme, whole; a grapheme wider than `w`
/// takes a row of its own, whole. A tab is one break space. Only a break's
/// whitespace run is dropped: a leading indent is kept, trailing whitespace
/// that fits stays and the overflow drops, and a non-breaking space
/// (U+00A0/U+202F/U+2007) is never a break point.
fn chunk_line(line: Line<'_>, w: usize) -> Vec<Line<'static>> {
    let line = normalize_tabs(&line);
    if w == 0 || line.width() <= w {
        return vec![line];
    }

    // Flatten to (grapheme, cells, style), so a word spanning styled spans
    // still wraps as one word and each broken row re-coalesces its styles.
    let mut flat: Vec<(String, usize, Style)> = Vec::new();
    for span in &line.spans {
        for grapheme in Span::raw(span.content.as_ref()).styled_graphemes(Style::default()) {
            flat.push((
                grapheme.symbol.to_string(),
                Span::raw(grapheme.symbol).width(),
                span.style,
            ));
        }
    }
    // Break points are whitespace minus the glue U+00A0/U+202F/U+2007.
    let is_break_space = |sym: &str| {
        !sym.is_empty()
            && sym
                .chars()
                .all(|c| c.is_whitespace() && !matches!(c, '\u{a0}' | '\u{202f}' | '\u{2007}'))
    };

    // Split into alternating runs of break whitespace and words (everything
    // else, the glue non-breaking spaces included).
    #[derive(Clone)]
    enum Atom {
        Space(Vec<(String, usize, Style)>),
        Word(Vec<(String, usize, Style)>),
    }
    let mut atoms: Vec<Atom> = Vec::new();
    let mut idx = 0;
    while idx < flat.len() {
        let spacey = is_break_space(&flat[idx].0);
        let mut run: Vec<(String, usize, Style)> = Vec::new();
        while idx < flat.len() && is_break_space(&flat[idx].0) == spacey {
            run.push(flat[idx].clone());
            idx += 1;
        }
        atoms.push(if spacey {
            Atom::Space(run)
        } else {
            Atom::Word(run)
        });
    }
    // A leading indent glues to the first word: when the two don't fit one row
    // the word splits by cell and keeps the indent on its first row, unless the
    // indent leaves no room for the first grapheme of the word — then the indent
    // takes whitespace-only leading rows and the word starts the next.
    if atoms.len() >= 2
        && let Atom::Space(indent) = atoms[0].clone()
        && let Atom::Word(mut word) = atoms[1].clone()
    {
        let mut merged = indent;
        merged.append(&mut word);
        atoms[0] = Atom::Word(merged);
        atoms.remove(1);
    }

    let line_style = line.style;
    let alignment = line.alignment;
    let mut rows: Vec<Line<'static>> = Vec::new();
    let mut cur: Vec<(String, usize, Style)> = Vec::new();
    let mut used = 0usize;

    let flush = |rows: &mut Vec<Line<'static>>, cur: &mut Vec<(String, usize, Style)>| {
        let mut spans: Vec<Span<'static>> = Vec::new();
        for (sym, _, style) in cur.drain(..) {
            match spans.last_mut() {
                Some(last) if last.style == style => last.content.to_mut().push_str(&sym),
                _ => spans.push(Span::styled(sym, style)),
            }
        }
        let mut row = Line::from(spans).style(line_style);
        row.alignment = alignment;
        rows.push(row);
    };
    let push = |g: (String, usize, Style),
                used: &mut usize,
                cur: &mut Vec<(String, usize, Style)>,
                rows: &mut Vec<Line<'static>>| {
        let gw = g.1;
        if *used + gw <= w {
            cur.push(g);
            *used += gw;
        } else {
            if !cur.is_empty() {
                flush(rows, cur);
                *used = 0;
            }
            cur.push(g);
            *used += gw;
        }
    };

    let mut atoms = atoms.into_iter().peekable();
    while let Some(atom) = atoms.next() {
        match atom {
            Atom::Word(word) => {
                for g in word {
                    push(g, &mut used, &mut cur, &mut rows);
                }
            }
            Atom::Space(space) => {
                let sp_w: usize = space.iter().map(|(_, cw, _)| *cw).sum();
                match atoms.peek() {
                    Some(Atom::Word(next)) => {
                        let next_w: usize = next.iter().map(|(_, cw, _)| *cw).sum();
                        if used + sp_w + next_w <= w {
                            for g in space {
                                let gw = g.1;
                                cur.push(g);
                                used += gw;
                            }
                        } else {
                            flush(&mut rows, &mut cur);
                            used = 0;
                        }
                    }
                    // Trailing whitespace is not a break: a run that fits the
                    // row stays, and the rest straddles the row end and drops
                    // (it would draw a blank row). An all-whitespace line
                    // keeps the cell split.
                    _ => {
                        let all_ws = cur.is_empty() && rows.is_empty();
                        for g in space {
                            if all_ws {
                                push(g, &mut used, &mut cur, &mut rows);
                            } else {
                                let gw = g.1;
                                if used + gw <= w {
                                    cur.push(g);
                                    used += gw;
                                } else {
                                    break;
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    if !cur.is_empty() {
        flush(&mut rows, &mut cur);
    }
    rows
}

/// Rounded `ACCENT_2` border, uppercase italic dim title, base `BG` fill.
/// The title opens with the corner-adjacent border dash (`╭─ TITLE`), the
/// same chrome rule the panel family follows, and fits a `width`-cell modal
/// the way a panel title does.
fn modal_block(title: &str, width: u16) -> Block<'static> {
    let label = title_label(title, true, width);
    let text = label
        .strip_prefix(' ')
        .and_then(|label| label.strip_suffix(' '))
        .unwrap_or(&label)
        .to_string();
    let title_line = Line::from(vec![
        Span::styled("─", Style::default().fg(theme::accent_2_color())),
        Span::raw(" "),
        Span::styled(text, Style::default().fg(theme::text_dim_color()).italic()),
        Span::raw(" "),
    ]);
    Block::bordered()
        .border_set(border::ROUNDED)
        .border_style(Style::default().fg(theme::accent_2_color()))
        .title(title_line)
        .style(theme::base())
        .padding(Padding::new(2, 2, 1, 1))
}

/// The narrowest modal that draws `title` whole: the corners, the
/// corner-adjacent dash and the ` TITLE ` break.
fn modal_title_w(title: &str) -> u16 {
    Line::from(title_label(title, true, u16::MAX)).width() as u16 + 3
}

/// [`modal_block`] plus the contract's title-right meta slot, for a modal whose
/// contents are scoped to one named thing. The name keeps its own case (it is a
/// proper noun, not a structural label) and stays `TEXT_DIM` — it is data about
/// the modal, so it takes neither the title's italic nor any bold. The name
/// gives way, like a panel's meta slot, when it would leave no rule.
fn modal_block_with_meta(title: &str, meta: Option<&str>, width: u16) -> Block<'static> {
    let block = modal_block(title, width);
    match meta.filter(|meta| right_meta_fits(title, 0, meta, width)) {
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
    // place — irreversible like the other destructive actions here. A bind
    // restart drains in-flight requests and bounces N live sessions' gateway,
    // the same blast radius `StopShunt` is marked destructive for.
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
            | ConfirmAction::RestartBind { .. }
            | ConfirmAction::RemoveEntry { .. }
            | ConfirmAction::RemoveToken { .. }
            | ConfirmAction::AddToken { .. }
            | ConfirmAction::RequireClientTokens { .. }
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
        ConfirmAction::RestartBind { .. } => "restart".to_string(),
        ConfirmAction::RenameField { .. } => "rename".to_string(),
        ConfirmAction::RemoveEntry { .. } | ConfirmAction::RemoveToken { .. } => {
            "remove".to_string()
        }
        ConfirmAction::AddToken { .. } => "overwrite".to_string(),
        ConfirmAction::RequireClientTokens { .. } => "require".to_string(),
        _ => "confirm".to_string(),
    };

    let mut lines: Vec<Line<'_>> = vec![Line::from(Span::styled(
        state.message.clone(),
        theme::body(),
    ))];
    // A detail may be several lines, and each is PROSE: split on the newlines
    // the caller wrote, then word-wrap. Left to the modal, a long line goes
    // through `chunk_line`, which cuts at the cell boundary and keeps every
    // character, so the space at the cut became a one-column indent on the
    // continuation row (seen at 80 columns in a real terminal). The bound is
    // the modal's own geometry: it never grows past `area.width - 4`, and its
    // inner width is 6 cells narrower than that, so wrapping here can only
    // prevent the chunking, never make the modal wider.
    if let Some(detail) = &state.detail {
        let inner = (area.width.saturating_sub(10) as usize).max(8);
        for line in detail.lines() {
            for row in prose::wrap(line, inner, theme::dim()) {
                lines.push(Line::from(row));
            }
        }
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
    // The token-add confirm's third line names only what the overwrite
    // replaces, `None` when the profile had neither a base url nor a key.
    if let ConfirmAction::AddToken { replace, .. } = &state.on_confirm
        && let Some(replace) = replace
    {
        lines.push(Line::from(prose::spans(replace, theme::dim())));
    }
    // The require confirm's third line names the profiles whose `base_url`
    // names the gateway; empty = none to name, no line.
    if let ConfirmAction::RequireClientTokens { profiles } = &state.on_confirm
        && !profiles.is_empty()
    {
        lines.push(Line::from(prose::spans(
            &format!("profiles using the gateway: {profiles}."),
            theme::dim(),
        )));
    }
    // An acknowledge-only notice has nothing to cancel — a single focused `close`.
    let buttons = if matches!(state.on_confirm, ConfirmAction::Acknowledge) {
        Line::from(modal_button(" close ", true))
    } else {
        choice_buttons(state.choice, "cancel", destructive, &confirm_label)
    };

    // The body scrolls (`↑`/`↓` one line, `PageUp`/`PageDown` one page) while
    // the buttons stay pinned — the env-collision review screen's shape, so a
    // short terminal can reach a confirm's detail tail without losing the
    // decision row.
    let d = draw_modal_scrolled(
        frame,
        area,
        title,
        lines,
        state.scroll.offset,
        None,
        ModalTail {
            extra: Some(ModalExtra::Buttons(buttons.alignment(Alignment::Right))),
            toast_rects: &[],
        },
    );
    state.scroll.max_scroll.set(d.max_scroll);
    state.scroll.viewport.set(d.viewport);
}

/// The loss warning a screen switch raises over unsaved work another screen
/// also writes: `WARNING` title, lowercase body naming the screen holding
/// the work and the other writer, and a `go back` / `discard` pair reusing the
/// confirm modal's button machinery.
fn draw_loss_warning(frame: &mut Frame<'_>, area: Rect, state: &LossWarningState) {
    let mut lines: Vec<Line<'_>> = vec![Line::from(Span::styled(
        "discard the unsaved edit?",
        theme::body(),
    ))];
    // One line per sentence so a sentence never shares a row with the next.
    for sentence in state.drop.detail_lines() {
        lines.push(Line::from(prose::spans(sentence, theme::dim())));
    }

    let buttons = choice_buttons(state.choice, "go back", true, "discard");

    draw_modal_scrolled(
        frame,
        area,
        "WARNING",
        lines,
        0,
        None,
        ModalTail {
            extra: Some(ModalExtra::Buttons(buttons.alignment(Alignment::Right))),
            toast_rects: &[],
        },
    );
}

/// The startup re-bind confirm: grouped account moves, one toggle line per
/// account, and a `not now` / `apply` button pair. The view follows the cursor
/// and the render publishes each account toggle's line geometry for the page
/// keys, the same follow/paging shape the other choice menus use.
fn draw_rebind(frame: &mut Frame<'_>, area: Rect, form: &RebindForm) {
    let row_w = form
        .blocks
        .iter()
        .flat_map(|b| &b.rows)
        .map(|m| m.row.len())
        .max()
        .unwrap_or(0);
    let old_w = form
        .blocks
        .iter()
        .flat_map(|b| &b.rows)
        .map(|m| m.old_display().len())
        .max()
        .unwrap_or(0);

    let mut lines: Vec<Line<'_>> = vec![
        Line::from(Span::styled(
            "move accounts onto the new preset models?",
            theme::body(),
        )),
        Line::from(Span::styled(
            "these accounts are on a built-in preset's endpoint. rows that are empty or hold an id an earlier release shipped take the preset's current value; ids you set stay.",
            theme::dim(),
        )),
        Line::from(""),
    ];

    // The pre-chunk line index of each account toggle, in flat order, plus each
    // block's first line: the first account's focus reaches back to it so the
    // block's change set (and, for block 0, the confirm's message) stays on
    // screen when the cursor returns to it.
    let mut toggle_lines: Vec<usize> = Vec::new();
    let mut block_starts: Vec<usize> = Vec::new();
    let mut flat = 0usize;
    for (bi, block) in form.blocks.iter().enumerate() {
        block_starts.push(if bi == 0 { 0 } else { lines.len() });
        for m in &block.rows {
            lines.push(Line::from(Span::styled(
                format!(
                    "  {:<row_w$}  {:<old_w$}  → {}",
                    m.row,
                    m.old_display(),
                    m.new_display(),
                ),
                theme::dim(),
            )));
        }
        for (ai, account) in block.accounts.iter().enumerate() {
            toggle_lines.push(lines.len());
            let selected = flat == form.cursor;
            let arrow = if selected {
                Span::styled("\u{276f} ", theme::accent())
            } else {
                Span::raw("  ")
            };
            let toggle = if form.checked[bi][ai] {
                Span::styled(theme::toggle_on().to_string(), theme::accent())
            } else {
                Span::styled(theme::toggle_off().to_string(), theme::faint())
            };
            let name_style = if selected {
                theme::accent()
            } else {
                theme::dim()
            };
            lines.push(Line::from(vec![
                arrow,
                toggle,
                Span::raw(" "),
                Span::styled(account.clone(), name_style),
            ]));
            flat += 1;
        }
        lines.push(Line::from(""));
    }

    let buttons = Line::from(vec![
        modal_button(" not now ", !form.choice),
        Span::raw(" "),
        modal_button(" apply ", form.choice),
    ])
    .alignment(Alignment::Right);

    let focus = form.account_at(form.cursor).and_then(|(bi, ai)| {
        toggle_lines.get(form.cursor).map(|&line| {
            let start = if ai == 0 { block_starts[bi] } else { line };
            (start..line + 1, &form.scroll, FocusEnd::Tail)
        })
    });
    let d = draw_modal_scrolled(
        frame,
        area,
        "CONFIRM",
        lines,
        0,
        focus,
        ModalTail {
            extra: Some(ModalExtra::Buttons(buttons)),
            toast_rects: &[],
        },
    );
    form.viewport.set(d.viewport as usize);
    form.total.set(d.total);
    let starts: Vec<usize> = toggle_lines
        .iter()
        .map(|&line| d.line_starts[line])
        .collect();
    let block_ends: Vec<usize> = starts.iter().map(|start| start + 1).collect();
    form.starts.replace(starts);
    form.block_ends.replace(block_ends);
}

fn choice_buttons(
    choice: bool,
    cancel_label: &str,
    destructive_confirm: bool,
    confirm_label: &str,
) -> Line<'static> {
    let cancel = format!(" {cancel_label} ");
    let label = format!(" {confirm_label} ");
    Line::from(vec![
        modal_button(&cancel, !choice),
        Span::raw(" "),
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

/// The apply-preset confirm with a third button: the fields the preset leaves
/// unset are kept or cleared. Laid out like `draw_confirm`, the body
/// scrolling over pinned buttons.
fn draw_preset_apply(frame: &mut Frame<'_>, area: Rect, form: &PresetApplyForm) {
    let mut lines = vec![Line::from(Span::styled(
        form.message.clone(),
        theme::body(),
    ))];
    if !form.replaces.is_empty() {
        lines.push(Line::from(Span::styled(
            format!("replaces {}.", form.replaces.join(", ")),
            theme::dim(),
        )));
    }
    lines.push(Line::from(Span::styled(
        format!("the preset leaves {} unset.", form.leaves.join(", ")),
        theme::dim(),
    )));
    // The full labels overflow a phone-width modal; the body above still
    // names what keep and clear act on.
    let (keep, clear) = if super::panes::narrow(area.width) {
        (" keep ", " clear ")
    } else {
        (" keep them ", " clear them ")
    };
    let buttons = Line::from(vec![
        modal_button(" cancel ", form.focus == PresetApplyButton::Cancel),
        Span::raw(" "),
        modal_button(keep, form.focus == PresetApplyButton::Keep),
        Span::raw(" "),
        danger_button(clear, form.focus == PresetApplyButton::Clear),
    ]);
    let d = draw_modal_scrolled(
        frame,
        area,
        "CONFIRM",
        lines,
        form.scroll.offset,
        None,
        ModalTail {
            extra: Some(ModalExtra::Buttons(buttons.alignment(Alignment::Right))),
            toast_rects: &[],
        },
    );
    form.scroll.max_scroll.set(d.max_scroll);
    form.scroll.viewport.set(d.viewport);
}

/// Destructive variant of `modal_button`: DANGER fg unfocused, inverse DANGER block
/// focused. Same bar-less house style as `modal_button` (no `▐`/`▌`).
pub(super) fn danger_button(label: &str, focused: bool) -> Span<'static> {
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
        ModalFollow {
            saved: &form.scroll,
            viewport: &form.viewport,
            geometry: Some(ModalGeometry {
                base: first,
                count: actions.len(),
                starts: &form.starts,
                block_ends: &form.block_ends,
                total: &form.total,
            }),
        },
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

/// Arrow-selected menu row shared by the Divergence and target-picker modals,
/// the action-menu row look: `❯ ` in ACCENT + bold and the label in TEXT +
/// bold, both on a `BG_HOVER` tint, when selected; an unselected label is
/// TEXT, not bold, on no tint. The scrolled-modal draw pads the tint to the
/// modal's full content width.
fn option_line(selected: bool, label: String) -> Line<'static> {
    let tint = selected.then(theme::bg_hover);
    let with_bg = |style: Style| match tint {
        Some(color) => style.bg(color),
        None => style,
    };
    let arrow = if selected {
        Span::styled("\u{276f} ", with_bg(theme::accent().bold()))
    } else {
        Span::raw("  ")
    };
    let style = if selected {
        with_bg(theme::body().bold())
    } else {
        theme::body()
    };
    Line::from(vec![arrow, Span::styled(label, style)])
}

fn draw_divergence_target(frame: &mut Frame<'_>, area: Rect, form: &DivergenceTargetForm) {
    let cursor = form.cursor.min(form.targets.len());

    let mut lines: Vec<Line<'_>> = vec![
        Line::from(Span::styled("where to save the login?", theme::dim())),
        Line::from(""),
        new_row(cursor == 0, "+ new account"),
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
        ModalFollow {
            saved: &form.scroll,
            viewport: &form.viewport,
            geometry: Some(ModalGeometry {
                base: 2,
                count: form.targets.len() + 1,
                starts: &form.starts,
                block_ends: &form.block_ends,
                total: &form.total,
            }),
        },
    );
}

fn draw_token_picker(frame: &mut Frame<'_>, area: Rect, form: &TokenPickerForm) {
    // Row 0 is `+ new profile`; the profiles sit below it, one row each.
    let cursor = form.cursor.min(form.targets.len());

    let mut lines: Vec<Line<'_>> = vec![
        Line::from(Span::styled("which profile sends it?", theme::dim())),
        Line::from(""),
        new_row(cursor == 0, "+ new profile"),
    ];
    for (i, (name, has_token)) in form.targets.iter().enumerate() {
        lines.push(token_picker_row(cursor == i + 1, name, *has_token));
    }

    draw_modal_focused(
        frame,
        area,
        "ADD TOKEN",
        lines,
        cursor + 2..cursor + 3,
        ModalFollow {
            saved: &form.scroll,
            viewport: &form.viewport,
            geometry: Some(ModalGeometry {
                base: 2,
                count: form.targets.len() + 1,
                starts: &form.starts,
                block_ends: &form.block_ends,
                total: &form.total,
            }),
        },
    );
}

/// The pickers' first-class `+ new` row: `ACCENT` at rest, `ACCENT + bold`
/// while selected (the `+ new` row's color is its identity), with the
/// action-menu tint when selected.
fn new_row(selected: bool, label: &str) -> Line<'static> {
    let tint = selected.then(theme::bg_hover);
    let with_bg = |style: Style| match tint {
        Some(color) => style.bg(color),
        None => style,
    };
    let arrow = if selected {
        Span::styled("\u{276f} ", with_bg(theme::accent().bold()))
    } else {
        Span::raw("  ")
    };
    let style = if selected {
        with_bg(theme::accent().bold())
    } else {
        theme::accent()
    };
    Line::from(vec![arrow, Span::styled(label.to_string(), style)])
}

/// One profile row of the token picker: the shared `option_line` shape (the
/// Divergence/target-picker row), plus a dim `  has a token` suffix for a
/// profile already holding one. The suffix keeps `TEXT_DIM` but rides the
/// selected row's tint.
fn token_picker_row(selected: bool, name: &str, has_token: bool) -> Line<'static> {
    let mut line = option_line(selected, name.to_string());
    if has_token {
        let style = if selected {
            theme::dim().bg(theme::bg_hover())
        } else {
            theme::dim()
        };
        line.push_span(Span::styled("  has a token", style));
    }
    line
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
        let d = draw_modal_scrolled(
            frame,
            area,
            "CONFIRM",
            lines,
            form.scroll.get().min(u16::MAX as usize) as u16,
            None,
            ModalTail {
                extra: Some(ModalExtra::Buttons(
                    choice_buttons(form.confirm_overwrite, "cancel", true, "add")
                        .alignment(Alignment::Right),
                )),
                toast_rects: &[],
            },
        );
        form.max_scroll.set(d.max_scroll);
        form.viewport.set(d.viewport as usize);
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
        let tint = selected.then(theme::bg_hover);
        let with_bg = |style: Style| match tint {
            Some(color) => style.bg(color),
            None => style,
        };
        let arrow = if selected {
            Span::styled("\u{276f} ", with_bg(theme::accent().bold()))
        } else {
            Span::raw("  ")
        };
        let (label, detail) = env_collision_option_text(*option, form);
        let label_style = if selected {
            with_bg(theme::body().bold())
        } else {
            theme::body()
        };
        lines.push(Line::from(vec![arrow, Span::styled(label, label_style)]));
        // The detail line is part of the selected option's block: it keeps
        // TEXT_DIM but rides the same tint, so a wrapped option tints every
        // line of its block.
        lines.push(Line::from(vec![
            Span::styled("    ", with_bg(Style::default())),
            Span::styled(detail, with_bg(theme::dim())),
        ]));
    }

    let content_w = lines
        .iter()
        .map(|line| normalize_tabs(line).width())
        .max()
        .unwrap_or(0) as u16;
    let width = (content_w + 6)
        .max("KEY IN USE".len() as u16 + 4)
        .min(modal_width_cap(area.width));
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
        ModalFollow {
            saved: &form.scroll,
            viewport: &form.viewport,
            geometry: None,
        },
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

fn draw_capture_name(frame: &mut Frame<'_>, area: Rect, input: &InputState, toast_rects: &[Rect]) {
    draw_name_field(
        frame,
        area,
        "CAPTURE",
        "stores the live ~/.claude/.credentials.json under this account.",
        input,
        toast_rects,
    );
}

/// The Setup menu's shared name prompt, the capture prompt's shape.
fn draw_name_prompt(
    frame: &mut Frame<'_>,
    area: Rect,
    form: &NamePromptForm,
    toast_rects: &[Rect],
) {
    draw_name_field(
        frame,
        area,
        form.action.title(),
        &form.action.blurb(),
        &form.input,
        toast_rects,
    );
}

/// A blurb, a blank line, then the `name` field, whose caret
/// [`draw_modal_scrolled`] places on whichever row the field lands on once the
/// blurb wraps.
fn draw_name_field(
    frame: &mut Frame<'_>,
    area: Rect,
    title: &str,
    blurb: &str,
    input: &InputState,
    toast_rects: &[Rect],
) {
    let lines = vec![
        Line::from(Span::styled(blurb.to_string(), theme::dim())),
        Line::from(""),
        labelled_input("name", input, true),
    ];
    let field = ModalExtra::Field {
        at: 2,
        label: "name",
        input,
    };
    draw_modal_scrolled(
        frame,
        area,
        title,
        lines,
        0,
        None,
        ModalTail {
            extra: Some(field),
            toast_rects,
        },
    );
}

/// `apply preset` picker. Built-ins lead the list and carry a dim `built-in`
/// tail so the two groups read apart without a second rule.
fn draw_preset_picker(frame: &mut Frame<'_>, area: Rect, form: &PresetPickerForm) {
    let last = form.presets.len().saturating_sub(1);
    let cursor = form.cursor.min(last);

    let mut lines: Vec<Line<'_>> = vec![
        Line::from(Span::styled(
            format!("sets the base url and models on {}.", form.target),
            theme::dim(),
        )),
        Line::from(""),
    ];
    for (i, preset) in form.presets.iter().enumerate() {
        let selected = i == cursor;
        let mut line = option_line(selected, preset.name.clone());
        if preset.builtin {
            let style = if selected {
                theme::dim().bg(theme::bg_hover())
            } else {
                theme::dim()
            };
            line.push_span(Span::styled("  built-in", style));
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
        ModalFollow {
            saved: &form.scroll,
            viewport: &form.viewport,
            geometry: Some(ModalGeometry {
                base: 2,
                count: form.presets.len(),
                starts: &form.starts,
                block_ends: &form.block_ends,
                total: &form.total,
            }),
        },
    );
}

/// The Usage help-modal rows, derived from the same per-row predicate set the
/// footer's hints use ([`super::footer::usage_key_facts`]), so the help never
/// promises a key the shown row ignores: `n` on claude rows only, `r` where a
/// re-read exists, `↵` on any item row.
fn usage_help_rows(app: &App) -> &'static [(&'static str, &'static str)] {
    use super::footer::{UsageRefresh, usage_key_facts};
    let facts = usage_key_facts(app);
    if facts.detail {
        return match facts.r {
            Some(UsageRefresh::RefreshAccount) => &[
                ("\u{2191} \u{2193}", "scroll account detail"),
                ("esc", "return to account selector"),
                ("\u{2190}", "return to account selector"),
                ("r", "refresh account"),
                ("n", "edit the account's note"),
                ("e", "toggle estimates"),
                ("p", "toggle pace marker"),
            ],
            Some(UsageRefresh::ReReadPool) => &[
                ("\u{2191} \u{2193}", "scroll account detail"),
                ("esc", "return to account selector"),
                ("\u{2190}", "return to account selector"),
                ("r", "re-read the pool listing"),
                ("e", "toggle estimates"),
                ("p", "toggle pace marker"),
            ],
            None => &[
                ("\u{2191} \u{2193}", "scroll account detail"),
                ("esc", "return to account selector"),
                ("\u{2190}", "return to account selector"),
                ("e", "toggle estimates"),
                ("p", "toggle pace marker"),
            ],
        };
    }
    match facts.r {
        Some(UsageRefresh::RefreshAccount) => &[
            ("\u{2191} \u{2193}", "pick account to inspect"),
            ("\u{21b5}", "open account detail"),
            ("r", "refresh account"),
            ("n", "edit the account's note"),
            ("e", "toggle estimates"),
            ("p", "toggle pace marker"),
        ],
        Some(UsageRefresh::ReReadPool) => &[
            ("\u{2191} \u{2193}", "pick account to inspect"),
            ("\u{21b5}", "open account detail"),
            ("r", "re-read the pool listing"),
            ("e", "toggle estimates"),
            ("p", "toggle pace marker"),
        ],
        None if facts.enter => &[
            ("\u{2191} \u{2193}", "pick account to inspect"),
            ("\u{21b5}", "open account detail"),
            ("e", "toggle estimates"),
            ("p", "toggle pace marker"),
        ],
        None => &[
            ("\u{2191} \u{2193}", "pick account to inspect"),
            ("e", "toggle estimates"),
            ("p", "toggle pace marker"),
        ],
    }
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
        // Derived from the same per-row predicate set as the footer's hints
        // (`usage_key_facts`), so the help never promises a key the shown row
        // ignores; on an empty roster the section omits `n`, keeping the
        // global `n new account` row (matching the empty state's `n to create
        // one`).
        Tab::Usage => vec![("usage", usage_help_rows(app))],
        Tab::Tokens if app.token_view == TokenView::Dashboard && token_model_count(app) == 0 => {
            vec![(
                "tokens",
                &[
                    ("\u{2191} \u{2193}", "move between cards"),
                    ("pgup/pgdn", "move a page of cards"),
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
                ("\u{2191} \u{2193}", "move between cards"),
                ("pgup/pgdn", "move a page of cards"),
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
                    "pick row · scroll shunt detail · walk plugin problems · walk herdr fix + options · walk delegates",
                ),
                (
                    "pgup/pgdn",
                    "scroll a page · read plugin / herdr diagnostics · page delegates",
                ),
                ("\u{21b5}", "open detail · activate · open fix"),
                ("space", "activate the focused herdr option"),
                ("+ / -", "step the tag refresh"),
                ("f", "next fix"),
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

/// The shunt card's `check output` modal: the masked failed-check stderr,
/// read-only, ↑↓ scrolling like the help modal. The body is already masked,
/// ANSI-stripped and control-escaped per line at open; its `\n` joins are the
/// real line structure, split here into rows that `draw_modal_scrolled` wraps
/// and scrolls.
fn draw_check_output(frame: &mut Frame<'_>, area: Rect, body: &str, app: &App) {
    let lines: Vec<Line<'_>> = body
        .lines()
        .map(|line| Line::from(Span::styled(line.to_string(), theme::body())))
        .collect();
    let lines = if lines.is_empty() {
        vec![Line::from("")]
    } else {
        lines
    };
    let d = draw_modal_scrolled(
        frame,
        area,
        "check output",
        lines,
        app.check_output_scroll,
        None,
        ModalTail {
            extra: None,
            toast_rects: &[],
        },
    );
    app.check_output_max_scroll.set(d.max_scroll);
    app.check_output_viewport.set(d.viewport);
}

fn draw_help(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let title = "KEYS";

    let tab_specific = tab_specific_rows(app);

    // The arrow-trap in a descended pane: `←` ascends, `→` stays on detail, and
    // in an editable pane `tab`/`⇧tab` stay too. The switch keys themselves now
    // live in GLOBAL below, so this block is descended-only.
    let nav: &[(&str, &str)] = match pane_state(app) {
        PaneState::ReadOnly => &[
            ("\u{2190}", "return to selector"),
            ("\u{2192}", "stays on detail"),
        ],
        PaneState::Editable => &[
            ("\u{2190}", "return to selector"),
            (
                "\u{2192} \u{00b7} tab \u{00b7} \u{21e7}tab",
                "stays on detail",
            ),
        ],
        PaneState::Rest => &[],
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
        ("\u{2190} \u{2192}", "switch screen"),
        ("tab", "next screen"),
        ("\u{21e7}tab", "previous screen"),
        ("\u{2325}1-9", "jump to tab"),
        ("esc", "back within a sub-view (no-op at the top level)"),
        ("pgup/pgdn", "scroll a page"),
        ("\u{2303}c", "quit from anywhere"),
    ];
    // A key the current tab redefines is documented in its own section above;
    // keeping the global sense too would contradict it (`t` cycles the period
    // and `r` reloads stats on Tokens, `r` refreshes one account on Usage).
    let mut shadowed: Vec<&str> = tab_specific
        .iter()
        .flat_map(|(_, rows)| rows.iter().map(|(k, _)| *k))
        .collect();
    // On the Usage tab `r` and `n` are redefined for every row (the key
    // handler routes them per selection), so the global `r refresh usage now`
    // never applies there, and `n new account` applies only to the empty
    // roster — the one case the Usage section leaves `n` to the global row.
    if app.tab == Tab::Usage {
        shadowed.push("r");
        if !app.setup_items().is_empty() {
            shadowed.push("n");
        }
    }
    let global: Vec<(&str, &str)> = global_all
        .iter()
        .copied()
        .filter(|(k, _)| !shadowed.contains(k))
        .collect();

    let mut lines: Vec<Line<'_>> = Vec::new();
    if !nav.is_empty() {
        lines.extend(key_section("tabs", nav));
    }
    lines.extend(key_section("this modal", modal_keys));
    for (section, entries) in &tab_specific {
        lines.extend(key_section(section, entries));
    }
    lines.extend(key_section("global", &global));
    // Last: the keys are what a reader came for, so they stay in view on a
    // terminal too short for the whole modal. ↑↓ reaches the legend from there.
    lines.extend(glyph_section("glyphs", glyph_rows()));
    lines.pop(); // trim trailing blank from last section
    let d = draw_modal_scrolled(
        frame,
        area,
        title,
        lines,
        app.help_scroll,
        None,
        ModalTail {
            extra: None,
            toast_rects: &[],
        },
    );
    app.help_max_scroll.set(d.max_scroll);
    app.help_viewport.set(d.viewport);
}

/// Legend for the 1-cell marks the account surfaces carry, with no key of their
/// own to document them: the Overview row's leading `●` and `⇄`, the Overview
/// chain's projected-switch `↲`, every blocked-reason marker on the Fallback
/// chain, and the Services selector's trailing `f`.
///
/// Each blocked-reason row takes its glyph AND its hue from [`reason_marker`]
/// itself, so the legend cannot drift from what the chain renders. `⊖` and `⊘`
/// each appear twice because they split their two senses on hue alone (see
/// `chain::reason_marker` for why), and a legend naming one sense per glyph
/// would be worse than none. `×` appears twice at one hue: the card's pill
/// tells its two senses apart.
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
            BlockedReason::WeeklySpent {
                resets_in: None,
                monthly: false,
            },
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
        reason(
            BlockedReason::WeeklySoft {
                pct: 0.0,
                monthly: false,
            },
            DIAG_WEEKLY_SOFT,
        ),
        reason(BlockedReason::Stale, DIAG_STALE),
        reason(BlockedReason::NoUsage, DIAG_NO_USAGE),
        (super::services::fix_mark(), "has a fix"),
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
    labelled_value(
        label,
        crate::tui::app::escape_control(&input.value),
        focused,
    )
}

/// A [`labelled_input`] row showing `value`, the field's text or the window of
/// it a narrow modal scrolls.
fn labelled_value(label: &str, value: String, focused: bool) -> Line<'static> {
    // When focused the native terminal cursor owns the caret — no block highlight.
    // Unfocused fields still render with plain text styling (no BG_SUNKEN tint).
    let value_style = if focused {
        Style::default()
            .fg(theme::text_color())
            .bg(theme::bg_sunken())
    } else {
        Style::default().fg(theme::text_color())
    };
    labelled_line(label, Span::styled(value, value_style), focused)
}

/// The cells before a [`labelled_line`]'s value: the gutter, the label and its
/// space.
fn labelled_lead_w(label: &str) -> usize {
    2 + Span::raw(label).width() + 1
}

/// A modal form row: the gutter, the label, one space, then `value`. A focused
/// row carries the `✎` edit-mode gutter glyph (same as form rows); the 2-col
/// gutter is counted by [`labelled_lead_w`].
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

    // Render rows with right-aligned hotkeys — can't use draw_modal_scrolled
    // because that wraps all lines in one Paragraph, preventing per-row
    // background tinting.
    // Custom draw: measure → size → clear → border → per-row widgets.
    let max_label_w = state
        .items
        .iter()
        .map(|item| Span::raw(item.label).width())
        .max()
        .unwrap_or(0) as u16;
    let content_w = GUTTER + max_label_w + 3 + HOTKEY_W;
    let title = "actions";
    // Grow to fit both border breaks, ` ACTIONS ` and ` <account> ─` with the
    // meta rule between them; a terminal too narrow for that drops the name.
    let meta_w = state.context.as_ref().map_or(0, |c| {
        META_RULE_MIN + meta_line(c, Style::default()).width()
    });
    let w = (content_w + 6)
        .max(modal_title_w(title) + meta_w as u16)
        .min(modal_width_cap(area.width));
    // items rows + the rule + 4 chrome (border + padding)
    let h = (state.items.len() as u16 + u16::from(rule_at.is_some()) + 4)
        .min(area.height.saturating_sub(4));

    let rect = centered(area, w, h);
    frame.render_widget(Clear, rect);
    let block = modal_block_with_meta(title, state.context.as_deref(), rect.width);
    let inner = block.inner(rect);
    frame.render_widget(block, rect);

    let inner_w = inner.width;
    let total = state.items.len() + usize::from(rule_at.is_some());
    let viewport = inner.height as usize;
    state.viewport.set(viewport);
    state.total.set(total);
    // The rule line sits between the groups, so a cursor row below it renders
    // one line further down — the same geometry the page step needs.
    let starts: Vec<usize> = (0..state.items.len())
        .map(|i| i + usize::from(rule_at.is_some_and(|at| i >= at as usize)))
        .collect();
    let block_ends: Vec<usize> = starts.iter().map(|s| s + 1).collect();
    state.starts.replace(starts);
    state.block_ends.replace(block_ends);
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
        let label = truncate(item.label, label_width);
        let label_len = Span::raw(&label).width() as u16;
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
