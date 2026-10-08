//! Shared widgets: the bordered section box every pane uses, and the account
//! picker shared by the Usage and Setup tabs.

use std::cell::Cell;

use ratatui::Frame;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::symbols::border;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, List, ListItem, ListState, Padding, Paragraph, Wrap};

use super::super::app::InputState;
use super::super::theme;
use super::format::{fixed_split, grapheme_cells, truncate};
use super::prose;
use crate::profile::AppConfig;

/// Place the native terminal cursor at `caret` unless a blocker rect covers
/// the cell: a drawn toast, or the whole frame while a modal owns input (body
/// editors only). Every editor — modal fields and body text editors — routes
/// its caret through here, so a body caret under a modal, and any caret under
/// a toast, stays hidden this frame.
pub(super) fn place_caret(frame: &mut Frame<'_>, toast_rects: &[Rect], caret: Position) {
    if !super::toasts::covers(toast_rects, caret) {
        frame.set_cursor_position(caret);
    }
}

/// Account-picker column width for a master-detail tab: ~30% of the body,
/// clamped 20-40 cells.
pub(super) fn selector_width(body_w: u16) -> u16 {
    (body_w.saturating_mul(3) / 10).clamp(20, 40)
}

/// Phone-width threshold: below this, layouts that place panes side-by-side
/// stack them vertically instead — at Moshi's ~45 columns the horizontal
/// master-detail split leaves the detail pane ~13 usable cells. At or above
/// it every layout renders as on desktop, except the Overview chain rows: they
/// test their panel's inner width (body − 4), so bodies 60–63 still narrow them.
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

/// The fewest cells a row being edited keeps for its typed value: a key cell
/// too wide for that clamps (Setup) or narrows (Config tab) instead. A Setup
/// value at rest whose key is too wide drops below the key instead of
/// clamping.
pub(super) const MIN_VALUE_W: usize = 4;

/// The cells before the caret in the window the field's last draw drew
/// ([`field_window`]): display cells of the whole graphemes of the
/// control-escaped body before the caret, less the window's start. Never a
/// char count: a wide grapheme takes its two cells.
pub(super) fn head_cols(input: &InputState) -> usize {
    let (_, cells, caret) = field_cells(input);
    cells
        .iter()
        .take(caret)
        .sum::<usize>()
        .saturating_sub(input.offset.get())
}

/// The control-escaped body an editor draws (the form [`value_caret`] renders),
/// its graphemes' display cells, and how many of them sit before the caret.
fn field_cells(input: &InputState) -> (String, Vec<usize>, usize) {
    let escape = crate::tui::app::escape_control;
    let body = escape(&input.value);
    let head = escape(&input.value[..input.cursor.min(input.value.len())]).len();
    let (cells, caret) = grapheme_cells_before(&body, head);
    (body, cells, caret)
}

/// The display cells of each grapheme of `text` as the buffer draws it (a
/// control grapheme, which the buffer drops, takes none), and how many of
/// them end at or before byte `head`.
pub(super) fn grapheme_cells_before(text: &str, head: usize) -> (Vec<usize>, usize) {
    let bounds = crate::tui::app::grapheme_bounds(text);
    let cells = bounds
        .windows(2)
        .map(|pair| grapheme_cells(&text[pair[0]..pair[1]]))
        .collect();
    let before = bounds.iter().skip(1).take_while(|b| **b <= head).count();
    (cells, before)
}

/// The edited field's visible run: whole graphemes of its control-escaped body
/// in a `width`-cell window that holds the caret's grapheme. The window start
/// persists in `input.offset` across frames, read and written here at draw
/// like the panes' scroll offsets, and moves only per [`follow_window`] (whose
/// `end_slack` this passes on). A value that fits draws whole.
pub(super) fn field_window(input: &InputState, width: usize, end_slack: usize) -> String {
    let (body, cells, caret) = field_cells(input);
    let start = follow_window(&cells, caret, input.offset.get(), width, end_slack);
    input.offset.set(start);
    cells_window(&body, start, width)
}

/// The start cell of a `width`-cell window over graphemes measuring `cells`,
/// the caret before grapheme `caret`. At End the caret needs the one blank
/// cell the cursor sits on, which may lie up to `end_slack` cells past the
/// window (a content-sized modal parks it in its right padding). `prior`
/// stands while it holds the caret's grapheme whole; otherwise the window makes
/// the least move that does, so scroll follows the caret and never pins it.
/// Starts land on grapheme boundaries, and never past the one that ends the
/// window on the End cell.
pub(super) fn follow_window(
    cells: &[usize],
    caret: usize,
    prior: usize,
    width: usize,
    end_slack: usize,
) -> usize {
    let bounds: Vec<usize> = std::iter::once(0)
        .chain(cells.iter().scan(0, |at, w| {
            *at += w;
            Some(*at)
        }))
        .collect();
    let lo = bounds.get(caret).copied().unwrap_or_default();
    if width == 0 {
        return lo;
    }
    let (hi, room) = match cells.get(caret) {
        Some(w) => (lo + (*w).max(1), width),
        None => (lo + 1, width + end_slack),
    };
    let end = bounds.last().copied().unwrap_or_default() + 1;
    let at_or_after = |cell: usize| bounds.iter().copied().find(|b| *b >= cell);
    let mut start = bounds
        .iter()
        .copied()
        .rfind(|b| *b <= prior)
        .unwrap_or_default();
    if lo < start {
        start = lo;
    } else if hi > start + room {
        start = at_or_after(hi - room).unwrap_or(lo).min(lo);
    }
    at_or_after(end.saturating_sub(width + end_slack)).map_or(start, |last| start.min(last))
}

/// The graphemes of `text` that lie whole inside cells `start..start + width`.
/// A wide grapheme cut by the left edge leaves its visible cells blank; one cut
/// by the right edge is left out.
pub(super) fn cells_window(text: &str, start: usize, width: usize) -> String {
    let mut out = String::new();
    let mut at = 0;
    for g in Span::raw(text).styled_graphemes(Style::default()) {
        let (from, to) = (at, at + Span::raw(g.symbol).width());
        at = to;
        if from < start {
            out.extend(std::iter::repeat_n(' ', to.saturating_sub(start)));
        } else if to > start + width {
            break;
        } else {
            out.push_str(g.symbol);
        }
    }
    out
}

/// Bolds `style` when `cond` is true.
pub(super) fn bold_when(style: Style, cond: bool) -> Style {
    if cond {
        style.add_modifier(Modifier::BOLD)
    } else {
        style
    }
}

/// Key-column cell: `key` left-justified to `max(width, key cells)` then a
/// fixed `gutter`-space gap, so every row's value opens at the same column
/// even when one key fills `width`. A plain `width.saturating_sub(cells).max(1)`
/// pad pushes an exactly-fitting key one cell past its siblings; this shape
/// keeps the gap separate from the width so it never collides. Each pane passes
/// its own `width` (its longest key) and the house `gutter` of 2.
pub(super) fn key_cell(key: &str, width: usize, gutter: usize) -> String {
    let kw = Span::raw(key).width();
    format!("{key}{}", " ".repeat(width.max(kw) - kw + gutter))
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
        canceled: impl Fn(&crate::profile::ProfileName) -> bool,
    ) -> Self {
        let blocked: Vec<crate::profile::ProfileName> =
            kick_lifts.keys().map(|k| k.as_str().into()).collect();
        Self {
            members: crate::usage::auto_start_queue_members(cfg, &blocked, canceled),
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
    DIAG_KICK, DIAG_MONTHLY_SOFT, DIAG_MONTHLY_SPENT, DIAG_NO_USAGE, DIAG_STALE, DIAG_WEEKLY_SOFT,
    DIAG_WEEKLY_SPENT,
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
    // A profile name longer than the selector (an email address passes
    // `validate_name_chars`) clamps with `…` instead of running past the pane
    // edge — the marked Fallback arm's clamp, without the marker column.
    let (text, pad) = fixed_split(&name, usize::from(width).saturating_sub(2));
    let line = Line::from(vec![
        arrow,
        Span::styled(text, bold_when(name_style, selected && focused)),
        Span::raw(pad),
    ]);
    select_line(line, selected, focused, width)
}

/// The empty state's two text lines (dim hint, hotkey-accented action), shared
/// by the bordered [`empty_state`] widget and a caller that renders them bare
/// so a pinned footnote composes with them.
pub(super) fn empty_state_lines(hint: &str, hotkey: &str, action: &str) -> Vec<Line<'static>> {
    vec![
        Line::from(Span::styled(hint.to_string(), theme::dim())),
        Line::from(prose::spans(
            &format!("{} {action}", prose::key(hotkey)),
            theme::dim(),
        )),
    ]
}

/// Empty-state widget: rounded frame in `LINE`, hint on first line `TEXT_DIM`,
/// hotkey `ACCENT` + action on second line.
pub(super) fn empty_state(hint: &str, hotkey: &str, action: &str) -> Paragraph<'static> {
    Paragraph::new(empty_state_lines(hint, hotkey, action))
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

/// Rows of context every cursor-following scroll keeps past the focused line
/// while content remains (the cursor never rests against the viewport edge):
/// forms, modals, Status, the page-key landing and the selector lists.
const SCROLL_PAD: usize = 2;

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

/// The footnote's wrapped lines: raw text cut to the content width, each line
/// `TEXT_FAINT`.
pub(super) fn footnote_lines(text: &str, width: usize) -> Vec<Line<'static>> {
    wrap_words(text, width)
        .into_iter()
        .map(|seg| Line::from(Span::styled(seg, theme::faint())))
        .collect()
}

/// How a pinned footnote splits the pane interior.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum FootnoteLayout {
    /// No footnote: the scroll area is the whole interior.
    Absent,
    /// Footnote pinned below the scroll area with `margin` blank rows above it.
    Pinned { scroll: usize, margin: usize },
    /// Interior too short to pin: the footnote renders as the scroll content's
    /// last line instead.
    Inline,
}

/// The split a pinned footnote produces for an `inner_h`-row interior and a
/// `footnote_h`-line footnote: pinned with a margin while the scroll area keeps
/// 3 rows, margin dropped next, inlined past that. `footnote_h == 0` is absent.
pub(super) fn footnote_layout(inner_h: usize, footnote_h: usize) -> FootnoteLayout {
    if footnote_h == 0 {
        FootnoteLayout::Absent
    } else if inner_h >= footnote_h + 1 + 3 {
        FootnoteLayout::Pinned {
            scroll: inner_h - footnote_h - 1,
            margin: 1,
        }
    } else if inner_h >= footnote_h + 3 {
        FootnoteLayout::Pinned {
            scroll: inner_h - footnote_h,
            margin: 0,
        }
    } else {
        FootnoteLayout::Inline
    }
}

/// Split `inner` for a `foot_h`-line footnote: the scroll area and the footnote
/// area (absent when the footnote is inlined into the content or missing). The
/// one place the pinned geometry lives — the scrolled-footnote helper and a
/// bare empty state both read it.
pub(super) fn footnote_split(inner: Rect, foot_h: usize) -> (Rect, Option<Rect>) {
    match footnote_layout(inner.height as usize, foot_h) {
        FootnoteLayout::Pinned { scroll, margin } => {
            let scroll_area = Rect {
                x: inner.x,
                y: inner.y,
                width: inner.width,
                height: scroll as u16,
            };
            let foot_area = Rect {
                x: inner.x,
                y: inner.y + scroll as u16 + margin as u16,
                width: inner.width,
                height: foot_h as u16,
            };
            (scroll_area, Some(foot_area))
        }
        FootnoteLayout::Absent | FootnoteLayout::Inline => (inner, None),
    }
}

/// What a [`draw_scrolled_lines_with_footnote`] frame drew: the applied offset,
/// the scroll viewport (a pinned footnote shrinks it) and the total line count
/// (an inline footnote is part of the scroll content, so it counts).
pub(super) struct ScrolledDraw {
    pub(crate) offset: usize,
    pub(crate) viewport: usize,
    pub(crate) total: usize,
}

/// A pane note pinned under a scrolled form:
/// one blank margin row above the wrapped note, the scroll area shrunk by the
/// footnote's height plus that margin, the overflow scrollbar spanning the
/// scroll area only. `footnote` is the raw one-fragment text; it wraps at the
/// content width, renders `TEXT_FAINT`, never scrolls, never takes focus and
/// never carries a key. `None` leaves the scroll area at the full height.
///
/// Short pane: when the scroll area would fall under 3 rows the margin drops
/// first, and past that the footnote renders as the scroll content's last line
/// instead of pinning — there the focused block's follow carries it, so walking
/// to the last row still reaches it.
pub(super) fn draw_scrolled_lines_with_footnote(
    frame: &mut Frame<'_>,
    inner: Rect,
    lines: Vec<Line<'static>>,
    focus: (usize, usize),
    saved: Option<&Cell<usize>>,
    footnote: Option<&str>,
) -> ScrolledDraw {
    let foot: Vec<Line<'static>> = footnote
        .map(|text| footnote_lines(text, inner.width as usize))
        .unwrap_or_default();
    match footnote_layout(inner.height as usize, foot.len()) {
        FootnoteLayout::Absent => {
            let total = lines.len();
            let offset = draw_scrolled_lines(frame, inner, lines, focus, saved);
            ScrolledDraw {
                offset,
                viewport: inner.height as usize,
                total,
            }
        }
        FootnoteLayout::Inline => {
            let pre = lines.len();
            let mut lines = lines;
            lines.extend(foot);
            let viewport = inner.height as usize;
            // When the focused block ends at the old last line AND the block
            // plus the footnote fit in the viewport together, extend the block
            // over the footnote so the follow reaches it; otherwise the block
            // wins — the cursor stays visible and the footnote stays below the
            // fold at that pane height.
            let focus = if focus.1 == pre && lines.len() - focus.0 <= viewport {
                (focus.0, lines.len())
            } else {
                focus
            };
            let total = lines.len();
            let offset = draw_scrolled_lines(frame, inner, lines, focus, saved);
            ScrolledDraw {
                offset,
                viewport,
                total,
            }
        }
        FootnoteLayout::Pinned { .. } => {
            let total = lines.len();
            let (scroll_area, foot_area) = footnote_split(inner, foot.len());
            let offset = draw_scrolled_lines(frame, scroll_area, lines, focus, saved);
            if let Some(area) = foot_area {
                frame.render_widget(Paragraph::new(foot).style(theme::base()), area);
            }
            ScrolledDraw {
                offset,
                viewport: scroll_area.height as usize,
                total,
            }
        }
    }
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

pub(crate) fn follow_scroll_offset(
    total: usize,
    viewport: usize,
    focus: (usize, usize),
    prior: usize,
) -> usize {
    follow_scroll_offset_with(total, viewport, focus, prior, false)
}

/// Tail-priority variant for the re-bind confirm's first account: when the
/// focus range (the block head..the cursor's toggle) is taller than the
/// viewport, the toggle — the end — stays visible instead of the head, so
/// `space` always flips a visible account.
pub(crate) fn follow_scroll_offset_tail(
    total: usize,
    viewport: usize,
    focus: (usize, usize),
    prior: usize,
) -> usize {
    follow_scroll_offset_with(total, viewport, focus, prior, true)
}

fn follow_scroll_offset_with(
    total: usize,
    viewport: usize,
    focus: (usize, usize),
    prior: usize,
    tail_wins: bool,
) -> usize {
    if viewport == 0 || total <= viewport {
        return 0;
    }
    let max = total - viewport;
    let offset = prior.min(max);
    let pad = SCROLL_PAD.min(viewport.saturating_sub(1) / 2);
    let minimum = if tail_wins {
        focus.1.saturating_sub(viewport)
    } else {
        focus.1.saturating_sub(viewport).min(focus.0)
    };
    let maximum = focus.0.min(max);
    let clamp = |x: usize| {
        if tail_wins {
            x.min(maximum).max(minimum)
        } else {
            x.max(minimum).min(maximum)
        }
    };
    if focus.0 < offset.saturating_add(pad) {
        clamp(focus.0.saturating_sub(pad))
    } else if focus.1.saturating_add(pad) > offset.saturating_add(viewport) {
        clamp(focus.1.saturating_add(pad).saturating_sub(viewport))
    } else {
        clamp(offset)
    }
}

/// A selector list's scroll state: the offset the render wrote back, the
/// visible-row count the last render drew, each cursor row's start line, the
/// end line its block reaches when focused (own lines plus any tooltip), and
/// the total line count. The key handler reads these at key time, so its page
/// step tracks a resize, a multi-line row's spans, and the follow the cursor
/// landing will produce instead of guessing.
#[derive(Debug, Default)]
pub(crate) struct ListView {
    pub(crate) offset: Cell<usize>,
    pub(crate) viewport: Cell<usize>,
    pub(crate) total: Cell<usize>,
    pub(crate) starts: std::cell::RefCell<Vec<usize>>,
    pub(crate) block_ends: std::cell::RefCell<Vec<usize>>,
    pub(crate) tips: std::cell::RefCell<Vec<usize>>,
}

pub(super) fn draw_following_list(
    frame: &mut Frame<'_>,
    inner: Rect,
    rows: Vec<ListItem<'static>>,
    sel: usize,
    list: &ListView,
) {
    let total = rows.len();
    let viewport = inner.height as usize;
    list.viewport.set(viewport);
    list.total.set(total);
    // A flat `List` has one rendered line per row; a caller whose rows carry a
    // band header or spacer (Overview) overrides these starts after this call.
    list.starts.replace((0..total).collect());
    list.block_ends.replace((0..total).map(|i| i + 1).collect());
    list.tips.replace(vec![0; total]);
    let prior = if viewport == 0 || total <= viewport {
        0
    } else {
        list.offset.get().min(total - viewport)
    };
    let mut state = ListState::default().with_offset(prior);
    state.select(Some(sel));
    frame.render_stateful_widget(
        List::new(rows)
            .style(theme::base())
            .scroll_padding(SCROLL_PAD),
        inner,
        &mut state,
    );
    list.offset.set(state.offset());
    draw_scrollbar(frame, inner, total, state.offset(), viewport);
}

/// Bordered selector list; `build_rows` receives the inner width for the selection bar.
pub(super) fn draw_selector_list(
    frame: &mut Frame<'_>,
    area: Rect,
    title: &str,
    focused: bool,
    sel: usize,
    list: &ListView,
    build_rows: impl FnOnce(u16) -> Vec<Line<'static>>,
) {
    let block = section_box(title, focused, true, area.width);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let rows = build_rows(inner.width);
    if rows.is_empty() {
        list.offset.set(0);
        list.viewport.set(0);
        list.total.set(0);
        list.starts.replace(Vec::new());
        list.block_ends.replace(Vec::new());
        list.tips.replace(Vec::new());
        frame.render_widget(empty_state("no accounts yet", "n", "to create one"), inner);
        return;
    }

    draw_following_list(
        frame,
        inner,
        rows.into_iter().map(ListItem::new).collect(),
        sel,
        list,
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

/// Chunk `text` into `width`-cell runs, splitting between graphemes by their
/// display width and never on whitespace: every byte lands in exactly one
/// chunk. A path whose name holds spaces keeps them.
pub(super) fn chunk_graphemes(text: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![text.to_string()];
    }
    let mut lines = Vec::new();
    let mut chunk = String::new();
    let mut chunk_w = 0;
    for grapheme in Span::raw(text).styled_graphemes(Style::default()) {
        let grapheme_w = Span::raw(grapheme.symbol).width();
        if !chunk.is_empty() && chunk_w + grapheme_w > width {
            lines.push(std::mem::take(&mut chunk));
            chunk_w = 0;
        }
        chunk.push_str(grapheme.symbol);
        chunk_w += grapheme_w;
    }
    if !chunk.is_empty() {
        lines.push(chunk);
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
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
            let mut chunks = chunk_graphemes(word, width);
            let last = chunks.pop().unwrap_or_default();
            lines.extend(chunks);
            line = last;
            line_w = Span::raw(&line).width();
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
    tooltip_lines(text, width, 1, theme::line(), theme::faint())
}

/// [`help_tooltip_lines`] with the `└` leader at `lead_indent` (the column the
/// `└` opens at): a caller whose line explains a label at col 2 passes 2, so
/// the `└` sits under the text it explains.
pub(super) fn help_tooltip_lines_at(
    text: &str,
    width: usize,
    lead_indent: usize,
) -> Vec<Line<'static>> {
    tooltip_lines(text, width, lead_indent, theme::line(), theme::faint())
}

/// Invalid-input twin of [`help_tooltip_lines`]: both the leader and the
/// reason render in `DANGER`.
pub(super) fn invalid_tooltip_lines(text: &str, width: usize) -> Vec<Line<'static>> {
    tooltip_lines(text, width, 1, theme::danger(), theme::danger())
}

fn tooltip_lines(
    text: &str,
    width: usize,
    lead_indent: usize,
    leader_style: Style,
    text_style: Style,
) -> Vec<Line<'static>> {
    let lead_w = lead_indent + 2; // the `└ ` and its indent
    let lead = format!("{}└ ", " ".repeat(lead_indent));
    let cont = " ".repeat(lead_w);
    prose::wrap(text, width.saturating_sub(lead_w).max(8), text_style)
        .into_iter()
        .enumerate()
        .map(move |(i, seg)| {
            let leader = if i == 0 { lead.clone() } else { cont.clone() };
            let mut spans = vec![Span::styled(leader, leader_style)];
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
/// when invalid), the value control-escaped so a control/bidi character the
/// saved value carried cannot inject into the terminal or reorder the line.
/// The native terminal cursor — set via `frame.set_cursor_position` — owns the
/// caret glyph, so no simulated block cursor. A value past the `width` cells
/// its row gives it scrolls inside them ([`field_window`]). Shared by the
/// Setup fields, the chain threshold/max-spend editors, the Config-tab
/// refresh/weekly editors, the Plugin-tab herdr tag-refresh editor, and the
/// shunt card's field editor.
pub(super) fn value_caret(input: &InputState, invalid: bool, width: usize) -> Vec<Span<'static>> {
    let body = if invalid {
        theme::danger()
    } else {
        theme::body()
    }
    .bg(theme::bg_sunken());
    vec![Span::styled(field_window(input, width, 0), body)]
}

/// Rounded box with contract-compliant chrome.
///
/// Border: `LINE_STRONG` when focused, `LINE` when blurred.
/// Title: always italic, always UPPERCASE; bold added only when focused.
/// Color: `ACCENT_2` for the first bordered panel on the screen body, `TEXT_DIM` for the rest.
pub(super) fn section_box(title: &str, focused: bool, first: bool, width: u16) -> Block<'static> {
    section_box_impl(title, focused, first, true, Vec::new(), None, width)
}

/// Like [`section_box`] but preserves the title's original case — use only when
/// the title is the selected item's own identity (a profile/account name, an
/// incident detail named after its incident), not a structural label.
pub(super) fn section_box_verbatim(
    title: &str,
    focused: bool,
    first: bool,
    width: u16,
) -> Block<'static> {
    section_box_impl(title, focused, first, false, Vec::new(), None, width)
}

/// Border cells of rule a title must keep before the meta slot may render: a
/// single dash between the two reads as part of the title's own rule run.
pub(super) const META_RULE_MIN: usize = 3;

/// Whether a title-right `meta` fits a `width`-cell top border beside the
/// title (and a `left_w`-cell left slot) with [`META_RULE_MIN`] cells of rule
/// between them; the title names the box and the meta only describes it, so
/// the meta is what gives way.
pub(super) fn right_meta_fits(title: &str, left_w: usize, meta: &str, width: u16) -> bool {
    // `╭─` + ` TITLE ` + left + rule + ` meta ─` + `╮`: the corner-adjacent dash
    // is a border cell of its own.
    let insets = 3
        + Line::from(title_label(title, true, u16::MAX)).width()
        + left_w
        + meta_line(meta, Style::default()).width();
    !meta.is_empty() && usize::from(width).saturating_sub(insets) >= META_RULE_MIN
}

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
    // This builder keeps the title un-ellipsized: its left slot never gives way,
    // so a too-narrow row clips from the right (the left slot first) — the
    // documented shape, not a title `…`. The bare border cell before ` left `
    // counts toward the left slot.
    let left_w = left.as_ref().map_or(0, |left| 1 + left.width());
    let meta = right_meta_fits(title, left_w, meta, width).then_some(meta);
    let block = section_box_impl(title, focused, first, true, Vec::new(), meta, u16::MAX);
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
    width: u16,
) -> Block<'static> {
    let suffix = vec![Span::styled(format!("{frame} "), theme::accent())];
    section_box_impl(title, focused, first, true, suffix, None, width)
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
        u16::MAX,
    )
}

fn section_box_impl(
    title: &str,
    focused: bool,
    first: bool,
    uppercase: bool,
    suffix: Vec<Span<'static>>,
    meta: Option<&str>,
    width: u16,
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
    let suffix_w: usize = suffix.iter().map(Span::width).sum();
    let mut title_spans = Vec::with_capacity(2 + suffix.len());
    if !title.is_empty() {
        // The corner-adjacent dash `╭─ TITLE`: chrome owns every `─` cell, so it
        // carries the border token, never the title style.
        title_spans.push(Span::styled("─", border_style));
        // A trailing suffix (the loading spinner) shares the border break with
        // the title, so the title budget must leave it room or the spinner
        // pushes into the corner.
        let title_w = width.saturating_sub(suffix_w.min(u16::MAX as usize) as u16);
        title_spans.push(Span::styled(
            title_label(title, uppercase, title_w),
            title_style,
        ));
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

/// A panel title as it sits in the border break: ` TITLE `, ellipsized to the
/// border when the panel is narrower than it. The title line renders into the
/// `width - 2` area past the corner; `─` plus the two insets leave `width - 5`
/// cells for the title itself, so a longer title ends in `…` inside the border
/// instead of running into the corner. Byte-identical whenever the title fits.
pub(super) fn title_label(title: &str, uppercase: bool, width: u16) -> String {
    let text = if uppercase {
        title.to_uppercase()
    } else {
        title.to_string()
    };
    let budget = usize::from(width).saturating_sub(5);
    let text = if grapheme_cells(&text) > budget {
        truncate(&text, budget)
    } else {
        text
    };
    format!(" {text} ")
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
