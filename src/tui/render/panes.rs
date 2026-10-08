//! Shared widgets: the bordered section box every pane uses, and the account
//! picker shared by the Usage and Setup tabs.

use std::cell::Cell;

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::symbols::border;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, List, ListItem, ListState, Padding, Paragraph, Wrap};

use super::super::app::{App, InputState};
use super::super::theme;
use super::prose;
use crate::profile::AppConfig;

/// Account-picker column width for a master-detail tab: ~30% of the body,
/// clamped 20-40 cells.
pub(super) fn selector_width(body_w: u16) -> u16 {
    (body_w.saturating_mul(3) / 10).clamp(20, 40)
}

/// Phone-width threshold: below this, layouts that place panes side-by-side
/// stack them vertically instead — at Moshi's ~45 columns the horizontal
/// master-detail split leaves the detail pane ~13 usable cells. At or above
/// it every layout is byte-identical to the desktop rendering.
pub(super) const NARROW_BODY_W: u16 = 60;

/// Key column width of the two master-detail settings cards, the Setup
/// account card and the Fallback member card: the longest key on either, the
/// Fallback card's `preferred days` (14). One width and one gutter
/// ([`DETAIL_KEY_GUTTER`]) for both, so their value columns open at the same
/// place across a tab switch.
pub(super) const DETAIL_KEY_W: usize = 14;
/// Fixed gap between the padded key and the value column on those two cards
/// (house standard).
pub(super) const DETAIL_KEY_GUTTER: usize = 2;

/// True when `w` is under the phone-width threshold.
pub(super) fn narrow(w: u16) -> bool {
    w < NARROW_BODY_W
}

/// The master-detail pane split shared by the Usage/Setup/Fallback/Status/
/// Services tabs. Desktop: the house horizontal selector|detail. Narrow: stacked
/// selector-above-detail — the selector takes its `items` rows (+ box chrome)
/// up to 40% of the body, the detail the rest, so both panes keep full-width
/// lines on a phone. Rows, not columns, are the abundant resource there.
pub(super) fn master_detail(area: Rect, items: usize) -> (Rect, Rect) {
    use ratatui::layout::{Constraint, Direction, Layout};
    if narrow(area.width) {
        let max_sel = (area.height.saturating_mul(2) / 5).max(5);
        let want = u16::try_from(items.saturating_add(3)).unwrap_or(u16::MAX);
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(want.clamp(5, max_sel)),
                Constraint::Min(8),
            ])
            .split(area);
        (rows[0], rows[1])
    } else {
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Length(selector_width(area.width)),
                Constraint::Min(20),
            ])
            .split(area);
        (cols[0], cols[1])
    }
}

/// Display columns occupied by the text before the caret in `input`.
/// `InputState::cursor` is a byte offset; every edited field is ASCII-only in
/// practice, so the char count of the pre-caret slice equals display columns.
pub(super) fn head_cols(input: &InputState) -> usize {
    input.value[..input.cursor.min(input.value.len())]
        .chars()
        .count()
}

/// Bolds `style` when `cond` is true.
pub(super) fn bold_when(style: Style, cond: bool) -> Style {
    if cond {
        style.add_modifier(Modifier::BOLD)
    } else {
        style
    }
}

/// Key-column cell: `key` left-justified to `max(width, key.len())` then a
/// fixed `gutter`-space gap, so every row's value opens at the same column
/// even when one key fills `width`. A plain `width.saturating_sub(len).max(1)`
/// pad pushes an exactly-fitting key one cell past its siblings; this shape
/// keeps the gap separate from the width so it never collides. Each pane passes
/// its own `width` (its longest key) and the house `gutter` of 2.
pub(super) fn key_cell(key: &str, width: usize, gutter: usize) -> String {
    let w = width.max(key.chars().count());
    format!("{key:<w$}{}", " ".repeat(gutter))
}

/// One segment of a cycle row: the active option renders `[label]` while the
/// row holds the cursor (the bracket pair is the focus cue — the row widens by
/// 2 on focus), bare `label` otherwise. Active → `ACCENT`, rest `TEXT_FAINT`.
fn cycle_option(label: &str, active: bool, row_selected: bool) -> Span<'static> {
    let style = if active {
        theme::accent()
    } else {
        theme::faint()
    };
    let text = if active && row_selected {
        format!("[{label}]")
    } else {
        label.to_string()
    };
    Span::styled(text, style)
}

/// Where a stacked cycle run's lines open: past the caret gutter, under the key.
const STACK_INDENT: usize = 2;

/// A cycle row as one or more lines: `lead` (the gutter glyph and the key
/// cell), then each option as a [`cycle_option`] chip, 2 cells apart. A
/// `custom` value matching no option trails the run the same 2 cells out,
/// never bracketed (the refresh row's appended value): `ACCENT` while it is the
/// row's value, `TEXT_FAINT` while it is only a stop the cycle can return to.
/// A `reminder` (the off-default `default: X` note) trails last, 3 cells out,
/// `TEXT_FAINT`.
///
/// A run too wide for `width` breaks between chips onto continuation lines
/// indented to the value column, never inside a chip. The custom value and the
/// reminder each share the line before them only when they fit there whole,
/// else open a line of their own, breaking between their words where even that
/// cannot hold them, so neither reads as one more word of the run. When the
/// value column cannot hold the widest option at all, the whole run drops under
/// the key instead: stacked rather than clipped. A custom value or reminder
/// with a word the value column cannot hold drops alone the same way, and the
/// options keep the value column. Every fit counts the active option's
/// brackets whether or not the row holds the cursor, so focus never moves a
/// break or flips a row between the layouts.
pub(super) fn cycle_row_lines(
    lead: Vec<Span<'static>>,
    options: &[(&str, bool)],
    custom: Option<(&str, bool)>,
    reminder: Option<&str>,
    row_selected: bool,
    width: usize,
) -> Vec<Line<'static>> {
    let value_col: usize = lead.iter().map(Span::width).sum();
    let widest_option = options
        .iter()
        .map(|(label, _)| Span::raw(*label).width() + 2)
        .max()
        .unwrap_or(0);
    let stacked = value_col + widest_option > width;
    let run_indent = if stacked { STACK_INDENT } else { value_col };
    // Each chip with the gap that opens it, the cells it holds whether or not
    // the row is focused, the room it asks of its line, and the indent its
    // lines open at: the options 2 cells apart, a trailing group's words 1
    // apart after its own lead, its first word asking room for the whole group.
    let mut chips: Vec<(usize, Span<'static>, usize, usize, usize)> = options
        .iter()
        .map(|(label, active)| {
            let chip = cycle_option(label, *active, row_selected);
            let fit = chip.width() + if *active && !row_selected { 2 } else { 0 };
            (2, chip, fit, fit, run_indent)
        })
        .collect();
    let mut trail = |text: &str, lead_gap: usize, style: Style| {
        let whole = Span::raw(text).width();
        let widest_word = text
            .split_whitespace()
            .map(|w| Span::raw(w).width())
            .max()
            .unwrap_or(0);
        let indent = if run_indent + widest_word > width {
            STACK_INDENT
        } else {
            run_indent
        };
        chips.extend(text.split_whitespace().enumerate().map(|(i, word)| {
            let chip = Span::styled(word.to_string(), style);
            let fit = chip.width();
            let (gap, ask) = if i == 0 { (lead_gap, whole) } else { (1, fit) };
            (gap, chip, fit, ask, indent)
        }));
    };
    if let Some((value, active)) = custom {
        trail(
            value,
            2,
            if active {
                theme::accent()
            } else {
                theme::faint()
            },
        );
    }
    if let Some(text) = reminder {
        trail(text, 3, theme::faint());
    }
    let blank = |indent: usize| Line::from(Span::raw(" ".repeat(indent)));

    let mut out = vec![Line::from(lead)];
    let mut used = value_col;
    if stacked {
        out.push(blank(run_indent));
        used = run_indent;
    }
    let mut fresh = true;
    for (gap, chip, fit, ask, indent) in chips {
        if !fresh && used + gap + ask > width {
            out.push(blank(indent));
            used = indent;
            fresh = true;
        }
        if let Some(line) = out.last_mut() {
            if !fresh {
                line.spans.push(Span::raw(" ".repeat(gap)));
                used += gap;
            }
            line.spans.push(chip);
        }
        used += fit;
        fresh = false;
    }
    out
}

/// Full-width selection bar: bg tint and stretch. Callers handle per-row bold.
pub(super) fn highlight_row(line: Line<'static>, width: usize) -> Line<'static> {
    let pad = width.saturating_sub(line.width());
    let mut line = line.style(theme::selected_row());
    if pad > 0 {
        line.push_span(Span::raw(" ".repeat(pad)));
    }
    line
}

/// Selected-row treatment: bold+bar+caret when focused, hover-tint-only when blurred.
pub(super) fn select_line(
    line: Line<'static>,
    selected: bool,
    focused: bool,
    width: u16,
) -> Line<'static> {
    if !selected {
        line
    } else if focused {
        highlight_row(line, width as usize)
    } else {
        // Keep BG_HOVER tint so the user sees where they were; drop caret + bold.
        // Filler must carry the tint too — bare Span::raw paints Color::Reset holes.
        let pad = (width as usize).saturating_sub(line.width());
        let mut line = line.style(theme::selected_row());
        if pad > 0 {
            line.push_span(Span::styled(" ".repeat(pad), theme::selected_row()));
        }
        line
    }
}

/// Orange for the active profile, plain text otherwise. This is the app's only
/// active-account marker: the `ACCENT_2` name and the `[ active ]` pill are
/// two spellings of one signal, so the detail panes carry neither, and the
/// selector's orange name speaks for the whole screen.
pub(super) fn name_color(active: bool) -> Style {
    if active {
        Style::default().fg(theme::accent_2_color())
    } else {
        Style::default().fg(theme::text_color())
    }
}

/// The interleaved auto-start queue (`usage::auto_start_queue`) as one render pass sees it:
/// who is in the queue and how long until it may open its next 5h window.
///
/// Built ONCE per pass rather than per row, because membership is a sort over
/// every account and the Fallback card's detail asks it for the selected member
/// on every frame.
pub(super) struct QueueView {
    members: Vec<crate::profile::ProfileName>,
    anchor: Option<i64>,
    interval_ms: u64,
    now_secs: i64,
}

impl QueueView {
    /// `anchor` is passed IN rather than read here: it lives behind
    /// AutoStartQueue(240) and every caller already holds Config(400), so reading it
    /// inside would invert the global lock order (`lockorder`, which
    /// debug-asserts). Callers take it with [`crate::usage::queue_anchor_cached`]
    /// before locking the config, next to the `switch_grade_kick_lifts` read
    /// they already make there.
    ///
    /// `kick_lifts` doubles as the blocked set: its keys are exactly the
    /// switch-grade kick blocks (`switch_grade_kick_lifts` and the scheduler's
    /// own `kick_rejected_names` share one predicate), which is what the
    /// election excludes from the queue.
    pub(super) fn new(
        cfg: &AppConfig,
        kick_lifts: &std::collections::HashMap<String, i64>,
        anchor: Option<i64>,
    ) -> Self {
        let blocked: Vec<crate::profile::ProfileName> =
            kick_lifts.keys().map(|k| k.as_str().into()).collect();
        Self {
            members: crate::usage::auto_start_queue_members(cfg, &blocked),
            anchor,
            interval_ms: cfg.state.refresh_interval_ms,
            now_secs: crate::usage::now_epoch_secs(),
        }
    }

    /// `name`'s queue slot, or `None` when it holds none (not opted in, cannot
    /// open a window, or the queue toggle is off).
    pub(super) fn slot(&self, name: &str) -> Option<crate::usage::QueueSlot> {
        crate::usage::queue_slot(
            &self.members,
            name,
            self.anchor,
            self.interval_ms,
            self.now_secs,
        )
    }
}

/// Canonical `[ label ]` wording for the diagnostic states whose text pill
/// surfaces on more than one tab (Usage / Fallback / Config; the Overview marker
/// shares only the `reason_marker` glyph, no text). One source so the same
/// account state never wears two words on two tabs — `canceled` used to read
/// `subscription canceled` on the Fallback card. Each pill's `└` hint carries
/// the explanation, so the label itself stays short. The hint layer stays
/// per-surface on purpose: `chain::reason_fix` and `usage::diag_fix` map
/// different enums with different config context.
///
/// The words themselves live in [`crate::format`] beside every other
/// cross-surface spelling; re-exported here so the tab-local `use super::panes::`
/// imports stay put.
pub(super) use crate::format::{
    DIAG_AUTH_BROKEN, DIAG_BUDGET_SPENT, DIAG_CANCELED, DIAG_DISABLED, DIAG_KEY_REJECTED,
    DIAG_KICK, DIAG_NO_USAGE, DIAG_STALE, DIAG_WEEKLY_SOFT, DIAG_WEEKLY_SPENT,
};

/// Status pill `[ label ]`: brackets in `TEXT_DIM`, the label in the
/// caller's semantic style (bold for a charged state). Returns the three spans
/// so a caller can compose them after a key cell; wrap in a `Line` for a
/// standalone pill.
pub(super) fn pill(label: String, label_style: Style) -> Vec<Span<'static>> {
    vec![
        Span::styled("[ ", theme::dim()),
        Span::styled(label, label_style),
        Span::styled(" ]", theme::dim()),
    ]
}

pub(super) fn picker_row(
    selected: bool,
    focused: bool,
    name: String,
    name_style: Style,
    width: u16,
) -> Line<'static> {
    // Caret only in the focused pane; blurred rows keep BG_HOVER via select_line.
    let arrow = if selected && focused {
        Span::styled("❯ ", theme::accent().add_modifier(Modifier::BOLD))
    } else {
        Span::raw("  ")
    };
    let line = Line::from(vec![
        arrow,
        Span::styled(name, bold_when(name_style, selected && focused)),
    ]);
    select_line(line, selected, focused, width)
}

/// Empty-state widget: rounded frame in `LINE`, hint on first line `TEXT_DIM`,
/// hotkey `ACCENT` + action on second line.
pub(super) fn empty_state(hint: &str, hotkey: &str, action: &str) -> Paragraph<'static> {
    Paragraph::new(vec![
        Line::from(Span::styled(hint.to_string(), theme::dim())),
        Line::from(prose::spans(
            &format!("{} {action}", prose::key(hotkey)),
            theme::dim(),
        )),
    ])
    .block(
        Block::bordered()
            .border_set(border::ROUNDED)
            .border_style(Style::default().fg(theme::line_color())),
    )
    .style(theme::base())
    .wrap(Wrap { trim: false })
}

/// Renders a scrollbar into the 1-cell right-padding column of a panel.
///
/// Track: `┊` in `LINE`. Thumb: `┃` in `TEXT_DIM`.
/// Only renders when `total > viewport` (content overflows). The column sits
/// flush against the content area's right edge — it reuses the padding cell
/// `section_box` already reserves, so content width is unchanged.
pub(super) fn draw_scrollbar(
    frame: &mut Frame<'_>,
    inner: Rect,
    total: usize,
    offset: usize,
    viewport: usize,
) {
    if total <= viewport || viewport == 0 || inner.height == 0 || inner.width == 0 {
        return;
    }
    // Right-padding column: one cell to the right of the content rect.
    let col_x = inner.x + inner.width;
    let col_y = inner.y;
    let col_h = inner.height as usize;

    let thumb_len = ((col_h * viewport) / total).max(1).min(col_h);
    let max_offset = total.saturating_sub(viewport);
    let thumb_top = ((col_h - thumb_len) * offset)
        .checked_div(max_offset)
        .unwrap_or(0);
    let thumb_end = thumb_top + thumb_len;

    let buf = frame.buffer_mut();
    for row in 0..col_h {
        let cell = buf.cell_mut((col_x, col_y + row as u16));
        if let Some(cell) = cell {
            if row >= thumb_top && row < thumb_end {
                cell.set_symbol("┃");
                cell.set_style(Style::default().fg(theme::text_dim_color()));
            } else {
                cell.set_symbol("┊");
                cell.set_style(Style::default().fg(theme::line_color()));
            }
        }
    }
}

/// Rows of context the form scroll keeps past the focused line while content
/// remains (the cursor never rests against the viewport edge).
const SCROLL_PAD: usize = 3;

/// Render a form pane's assembled lines into `inner`, scrolled so the focused
/// block `focus.0..focus.1` stays on screen, plus the overflow scrollbar.
/// Returns the applied offset so a caller placing the native terminal cursor can
/// shift its row by it.
///
/// The scroll follows the focused block without clipping wrapped hints. A
/// stateful form passes its own offset; other callers derive one each draw.
pub(super) fn draw_scrolled_lines(
    frame: &mut Frame<'_>,
    inner: Rect,
    lines: Vec<Line<'static>>,
    focus: (usize, usize),
    saved: Option<&Cell<usize>>,
) -> usize {
    let total = lines.len();
    let viewport = inner.height as usize;
    let offset = saved.map_or_else(
        || scroll_offset(total, viewport, focus),
        |saved| {
            let offset = follow_scroll_offset(total, viewport, focus, saved.get());
            saved.set(offset);
            offset
        },
    );
    frame.render_widget(
        Paragraph::new(lines)
            .style(theme::base())
            .scroll((offset as u16, 0)),
        inner,
    );
    draw_scrollbar(frame, inner, total, offset, viewport);
    offset
}

/// Smallest offset that keeps the focused block (`focus.0` inclusive, `focus.1`
/// exclusive) plus a [`SCROLL_PAD`] band on screen, clamped to the content.
///
/// The block, not just its first line: a row's help tooltip wraps to the pane
/// width. Capping at `focus.0` keeps the row visible when the block is taller
/// than the viewport. Stateless callers keep their existing focus-derived view.
pub(super) fn scroll_offset(total: usize, viewport: usize, focus: (usize, usize)) -> usize {
    if viewport == 0 || total <= viewport {
        return 0;
    }
    let pad = SCROLL_PAD.min(viewport.saturating_sub(1) / 2);
    (focus.1 + pad)
        .saturating_sub(viewport)
        .min(focus.0)
        .min(total - viewport)
}

pub(super) fn follow_scroll_offset(
    total: usize,
    viewport: usize,
    focus: (usize, usize),
    prior: usize,
) -> usize {
    if viewport == 0 || total <= viewport {
        return 0;
    }
    let max = total - viewport;
    let offset = prior.min(max);
    let pad = SCROLL_PAD.min(viewport.saturating_sub(1) / 2);
    let minimum = focus.1.saturating_sub(viewport).min(focus.0);
    let maximum = focus.0.min(max);
    if focus.0 < offset.saturating_add(pad) {
        focus.0.saturating_sub(pad).max(minimum).min(maximum)
    } else if focus.1.saturating_add(pad) > offset.saturating_add(viewport) {
        focus
            .1
            .saturating_add(pad)
            .saturating_sub(viewport)
            .max(minimum)
            .min(maximum)
    } else {
        offset.clamp(minimum, maximum)
    }
}

pub(super) fn draw_following_list(
    frame: &mut Frame<'_>,
    inner: Rect,
    rows: Vec<ListItem<'static>>,
    sel: usize,
    offset: &Cell<usize>,
) {
    let total = rows.len();
    let viewport = inner.height as usize;
    let prior = if viewport == 0 || total <= viewport {
        0
    } else {
        offset.get().min(total - viewport)
    };
    let mut state = ListState::default().with_offset(prior);
    state.select(Some(sel));
    frame.render_stateful_widget(
        List::new(rows).style(theme::base()).scroll_padding(3),
        inner,
        &mut state,
    );
    offset.set(state.offset());
    draw_scrollbar(frame, inner, total, state.offset(), viewport);
}

/// Bordered selector list; `build_rows` receives the inner width for the selection bar.
pub(super) fn draw_selector_list(
    frame: &mut Frame<'_>,
    area: Rect,
    title: &str,
    focused: bool,
    sel: usize,
    offset: &Cell<usize>,
    build_rows: impl FnOnce(u16) -> Vec<Line<'static>>,
) {
    let block = section_box(title, focused, true);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let rows = build_rows(inner.width);
    if rows.is_empty() {
        offset.set(0);
        frame.render_widget(empty_state("no accounts yet", "n", "to create one"), inner);
        return;
    }

    draw_following_list(
        frame,
        inner,
        rows.into_iter().map(ListItem::new).collect(),
        sel,
        offset,
    );
}

/// A `├`/`└` fix-hint line: glyph + text at col 0/2, anchored to the caller's
/// own key column (no leading indent, unlike [`help_tooltip_lines`]'s one-cell
/// offset for panes whose row opens past col 0). `more_follow` picks `├`
/// (another hint follows in the same rail) vs `└` (closes it, or the lone-hint
/// case with nothing to connect); wrapped continuation lines keep the text at
/// col 2 and carry `│` at col 0 while the rail is still open, blank once it has
/// closed.
///
/// The one rail drawer: the Usage tab's `status` block and the Fallback card's
/// blocked-reason pills both render through it, so the two can't drift apart.
pub(super) fn rail_hint_lines(text: &str, width: usize, more_follow: bool) -> Vec<Line<'static>> {
    const LEAD_W: usize = 2; // glyph + 1 space, text at col 2
    let lead = if more_follow { "├ " } else { "└ " };
    let cont = if more_follow { "│ " } else { "  " };
    prose::wrap(text, width.saturating_sub(LEAD_W).max(8), theme::faint())
        .into_iter()
        .enumerate()
        .map(|(i, seg)| {
            let mut spans = vec![Span::styled(
                if i == 0 { lead } else { cont },
                theme::line(),
            )];
            spans.extend(seg);
            Line::from(spans)
        })
        .collect()
}

/// Greedy word-wrap to `width` cells, measured the way the buffer renders: by
/// grapheme, a wide one taking two; long words are hard-split between
/// graphemes.
pub(super) fn wrap_words(text: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![text.to_string()];
    }
    let mut lines = Vec::new();
    let mut line = String::new();
    let mut line_w = 0;
    for word in text.split_whitespace() {
        let word_w = Span::raw(word).width();
        if word_w > width {
            if !line.is_empty() {
                lines.push(std::mem::take(&mut line));
            }
            let mut chunk = String::new();
            let mut chunk_w = 0;
            for grapheme in Span::raw(word).styled_graphemes(Style::default()) {
                let grapheme_w = Span::raw(grapheme.symbol).width();
                if !chunk.is_empty() && chunk_w + grapheme_w > width {
                    lines.push(std::mem::take(&mut chunk));
                    chunk_w = 0;
                }
                chunk.push_str(grapheme.symbol);
                chunk_w += grapheme_w;
            }
            line = chunk;
            line_w = chunk_w;
            continue;
        }
        let extra = usize::from(!line.is_empty());
        if line_w + extra + word_w > width {
            lines.push(std::mem::take(&mut line));
            line.push_str(word);
            line_w = word_w;
        } else {
            if !line.is_empty() {
                line.push(' ');
            }
            line.push_str(word);
            line_w += extra + word_w;
        }
    }
    if !line.is_empty() {
        lines.push(line);
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

/// A ` └ text` help sub-line wrapped to `width`: the `└ ` leader stays `LINE`,
/// the reason renders `faint`; continuation lines indent under the text so the
/// hint reads as one block instead of clipping off the pane edge.
pub(super) fn help_tooltip_lines(text: &str, width: usize) -> Vec<Line<'static>> {
    tooltip_lines(text, width, theme::line(), theme::faint())
}

/// Invalid-input twin of [`help_tooltip_lines`]: both the leader and the
/// reason render in `DANGER`.
pub(super) fn invalid_tooltip_lines(text: &str, width: usize) -> Vec<Line<'static>> {
    tooltip_lines(text, width, theme::danger(), theme::danger())
}

fn tooltip_lines(
    text: &str,
    width: usize,
    leader_style: Style,
    text_style: Style,
) -> Vec<Line<'static>> {
    const LEAD_W: usize = 3; // " └ " and the matching continuation indent
    prose::wrap(text, width.saturating_sub(LEAD_W).max(8), text_style)
        .into_iter()
        .enumerate()
        .map(|(i, seg)| {
            let lead = if i == 0 { " └ " } else { "   " };
            let mut spans = vec![Span::styled(lead, leader_style)];
            spans.extend(seg);
            Line::from(spans)
        })
        .collect()
}

/// Form-row label style: `TEXT + bold` when focused, `TEXT_DIM` when blurred.
pub(super) fn label_style(focused: bool) -> Style {
    if focused {
        Style::default()
            .fg(theme::text_color())
            .add_modifier(Modifier::BOLD)
    } else {
        theme::dim()
    }
}

/// Render a typed editor's buffer with uniform `BG_SUNKEN` styling (DANGER fg
/// when invalid). The native terminal cursor — set via
/// `frame.set_cursor_position` — owns the caret glyph, so no simulated block
/// cursor. Shared by the chain threshold/max-spend editors, the Config-tab
/// refresh/weekly editors, and the Plugin-tab herdr tag-refresh editor.
pub(super) fn value_caret(input: &InputState, invalid: bool) -> Vec<Span<'static>> {
    let body = if invalid {
        theme::danger()
    } else {
        theme::body()
    }
    .bg(theme::bg_sunken());
    vec![Span::styled(input.value.clone(), body)]
}

/// Rounded box with contract-compliant chrome.
///
/// Border: `LINE_STRONG` when focused, `LINE` when blurred.
/// Title: always italic, always UPPERCASE; bold added only when focused.
/// Color: `ACCENT_2` for the first bordered panel on the screen body, `TEXT_DIM` for the rest.
pub(super) fn section_box(title: &str, focused: bool, first: bool) -> Block<'static> {
    section_box_impl(title, focused, first, true, Vec::new(), None)
}

/// Like [`section_box`] but preserves the title's original case — use only when
/// the title is a profile/account name, not a structural label.
pub(super) fn section_box_verbatim(title: &str, focused: bool, first: bool) -> Block<'static> {
    section_box_impl(title, focused, first, false, Vec::new(), None)
}

/// Border cells of rule a title must keep before the meta slot may render: a
/// single dash between the two reads as part of the title's own rule run.
const META_RULE_MIN: usize = 3;

/// [`section_box`] with two meta slots: `left` as a title of its own one
/// border cell after the title (`╭─ TITLE ─ left ───`), `meta` in the border
/// break just before the top-right corner (`… meta ─╮`). Both are data styled
/// alike, `TEXT_DIM` and never bold or italic, and the dashes around them keep
/// the border token: the corner-adjacent dash and the dash between the title
/// and `left` are border cells — the corner dash is part of the title line,
/// the between-titles dash is the cell ratatui leaves bare between two
/// left-aligned titles.
///
/// Only the right slot gives way: it renders while `width` leaves it at least
/// [`META_RULE_MIN`] border cells of rule after the title and the left slot,
/// since the title names the panel and the meta only describes what is in it.
/// The left slot never gives way to the right one, because it qualifies what
/// the title names; only a panel too narrow for the title line itself clips
/// that line from the right, the left slot first. An empty `meta` renders no
/// right slot.
pub(super) fn section_box_meta(
    title: &str,
    left: Option<&str>,
    meta: &str,
    focused: bool,
    first: bool,
    width: u16,
) -> Block<'static> {
    let left = left.map(|name| Line::from(Span::styled(format!(" {name} "), theme::dim())));
    // `╭─` + ` TITLE ` + (the bare border cell + ` left `) + rule + ` meta ─` + `╮`:
    // the corner-adjacent dash is a border cell of its own, counted here so the
    // right slot keeps its ≥[`META_RULE_MIN`] rule cells at the compliant shape.
    let insets = 3
        + Line::from(title_label(title, true)).width()
        + left.as_ref().map_or(0, |left| 1 + left.width())
        + meta_line(meta, Style::default()).width();
    let rule = usize::from(width).saturating_sub(insets);
    let meta = (!meta.is_empty() && rule >= META_RULE_MIN).then_some(meta);
    let block = section_box_impl(title, focused, first, true, Vec::new(), meta);
    match left {
        Some(left) => block.title_top(left),
        None => block,
    }
}

/// [`section_box`] with a live braille spinner `frame` appended inside the title
/// inset (` TITLE ⠋ `), for a card whose data is still loading. The spinner is
/// its own `ACCENT` span; the border dashes keep the border token (chrome owns
/// the dashes).
pub(super) fn section_box_loading(
    title: &str,
    focused: bool,
    first: bool,
    frame: &str,
) -> Block<'static> {
    let suffix = vec![Span::styled(format!("{frame} "), theme::accent())];
    section_box_impl(title, focused, first, true, suffix, None)
}

/// The note editor's docked slot: an empty-title section box whose title break
/// carries the `✎` edit mark in accent — `╭─ ✎ ───╮`, the contract's multi-line
/// input slot. The chrome dash carries the border token, never the mark color.
pub(super) fn edit_slot_block() -> Block<'static> {
    section_box_impl(
        "",
        true,
        false,
        false,
        vec![
            Span::styled("─ ", Style::default().fg(theme::line_strong_color())),
            Span::styled(format!("{} ", theme::edit_glyph()), theme::accent().bold()),
        ],
        None,
    )
}

fn section_box_impl(
    title: &str,
    focused: bool,
    first: bool,
    uppercase: bool,
    suffix: Vec<Span<'static>>,
    meta: Option<&str>,
) -> Block<'static> {
    let border_style = if focused {
        Style::default().fg(theme::line_strong_color())
    } else {
        Style::default().fg(theme::line_color())
    };
    let title_color = if first {
        theme::accent_2_color()
    } else {
        theme::text_dim_color()
    };
    let title_style = {
        let base = Style::default()
            .fg(title_color)
            .add_modifier(Modifier::ITALIC);
        if focused {
            base.add_modifier(Modifier::BOLD)
        } else {
            base
        }
    };
    let mut title_spans = Vec::with_capacity(2 + suffix.len());
    if !title.is_empty() {
        // The corner-adjacent dash `╭─ TITLE`: chrome owns every `─` cell, so it
        // carries the border token, never the title style.
        title_spans.push(Span::styled("─", border_style));
        title_spans.push(Span::styled(title_label(title, uppercase), title_style));
    }
    // An EMPTY title pushes no spans at all — not even `title_label("")`'s
    // two-space inset, which would punch a hole in the top border. The
    // width-probe callers never render, and a rendered empty title must keep
    // the full rule run (no dash, no hole).
    title_spans.extend(suffix);
    let mut block = Block::bordered()
        .border_set(border::ROUNDED)
        .border_style(border_style)
        .title(Line::from(title_spans))
        .padding(Padding::horizontal(1));
    if let Some(meta) = meta {
        block = block.title_top(meta_line(meta, border_style));
    }
    block
}

/// A panel title as it sits in the border break: ` TITLE `.
fn title_label(title: &str, uppercase: bool) -> String {
    if uppercase {
        format!(" {} ", title.to_uppercase())
    } else {
        format!(" {title} ")
    }
}

/// The title-right meta slot. A right-aligned title ends flush against the
/// top-right corner, so the slot closes with a border cell of its own:
/// `… meta ─╮`.
pub(super) fn meta_line(meta: &str, border_style: Style) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!(" {meta} "), theme::dim()),
        Span::styled("─", border_style),
    ])
    .right_aligned()
}

pub(super) fn draw_profile_selector(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &App,
    selected: usize,
    focused: bool,
) {
    let cfg = app.config();
    let sel = selected.min(cfg.profiles.len().saturating_sub(1));
    draw_selector_list(
        frame,
        area,
        "accounts",
        focused,
        sel,
        &app.usage_selector_offset,
        |w| {
            cfg.profiles
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
                .collect()
        },
    );
}
