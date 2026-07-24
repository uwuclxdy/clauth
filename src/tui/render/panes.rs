//! Shared widgets: the bordered section box every pane uses, and the account
//! picker shared by the Usage and Setup tabs.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::symbols::border;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, List, ListItem, ListState, Padding, Paragraph, Wrap};

use super::super::app::{App, InputState};
use super::super::theme;

/// Account-picker column width for a master-detail tab: ~30% of the body,
/// clamped 20-40 cells (cloudy-tui master-detail contract).
pub(super) fn selector_width(body_w: u16) -> u16 {
    (body_w.saturating_mul(3) / 10).clamp(20, 40)
}

/// Phone-width threshold: below this, layouts that place panes side-by-side
/// stack them vertically instead — at Moshi's ~45 columns the horizontal
/// master-detail split leaves the detail pane ~13 usable cells. At or above
/// it every layout is byte-identical to the desktop rendering.
pub(super) const NARROW_BODY_W: u16 = 60;

/// True when `w` is under the phone-width threshold.
pub(super) fn narrow(w: u16) -> bool {
    w < NARROW_BODY_W
}

/// The master-detail pane split shared by the Usage/Setup/Fallback/Status/
/// Plugin tabs. Desktop: the house horizontal selector|detail. Narrow: stacked
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
pub(super) fn cycle_option(label: &str, active: bool, row_selected: bool) -> Span<'static> {
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
/// active-account marker: cloudy-tui takes the `ACCENT_2` name and the
/// `[ active ]` pill as two spellings of one signal, so the detail panes carry
/// neither, and the selector's orange name speaks for the whole screen.
pub(super) fn name_color(active: bool) -> Style {
    if active {
        Style::default().fg(theme::accent_2_color())
    } else {
        Style::default().fg(theme::text_color())
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
pub(super) const DIAG_DISABLED: &str = "disabled";
pub(super) const DIAG_CANCELED: &str = "canceled";
pub(super) const DIAG_AUTH_BROKEN: &str = "auth broken";
pub(super) const DIAG_BUDGET_SPENT: &str = "extra usage spent";
pub(super) const DIAG_KICK: &str = "claude code blocked";

/// cloudy-tui status pill `[ label ]`: brackets in `TEXT_DIM`, the label in the
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

/// `[ active ]` — the active-account marker on the usage tab's plan row,
/// [`pill`] pre-bound to the accent-bold label.
pub(super) fn active_pill() -> Vec<Span<'static>> {
    pill(
        "active".to_string(),
        theme::accent().add_modifier(Modifier::BOLD),
    )
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
        Line::from(vec![
            Span::styled(hotkey.to_string(), theme::accent()),
            Span::styled(format!(" {action}"), theme::dim()),
        ]),
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
/// remains (cloudy-tui: the cursor never rests against the viewport edge).
const SCROLL_PAD: usize = 3;

/// Render a form pane's assembled lines into `inner`, scrolled so the focused
/// block `focus.0..focus.1` stays on screen, plus the overflow scrollbar.
/// Returns the applied offset so a caller placing the native terminal cursor can
/// shift its row by it.
///
/// These panes rebuild one `Vec<Line>` per frame and hold no offset in `App`, so
/// the scroll is derived from the focused block each draw. Without it a pane
/// taller than its viewport silently drops its bottom rows — no scrollbar, no
/// clue anything is missing, which is how a hint tooltip and a whole settings
/// row went missing on a 24-row terminal.
pub(super) fn draw_scrolled_lines(
    frame: &mut Frame<'_>,
    inner: Rect,
    lines: Vec<Line<'static>>,
    focus: (usize, usize),
) -> usize {
    let total = lines.len();
    let viewport = inner.height as usize;
    let offset = scroll_offset(total, viewport, focus);
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
/// width, so a narrow pane can push a 4-line hint past the viewport while the
/// row it explains sits comfortably on screen. Capping at `focus.0` keeps the
/// row itself visible when its block is taller than the whole viewport. The
/// focus alone determines the offset, so no cross-frame state is needed and the
/// view can never drift out of sync with the cursor.
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

/// Bordered selector list; `build_rows` receives the inner width for the selection bar.
pub(super) fn draw_selector_list(
    frame: &mut Frame<'_>,
    area: Rect,
    title: &str,
    focused: bool,
    sel: usize,
    build_rows: impl FnOnce(u16) -> Vec<Line<'static>>,
) {
    let block = section_box(title, focused, true);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let rows = build_rows(inner.width);
    if rows.is_empty() {
        frame.render_widget(empty_state("no accounts yet", "n", "to create one"), inner);
        return;
    }

    let total = rows.len();
    let list =
        List::new(rows.into_iter().map(ListItem::new).collect::<Vec<_>>()).style(theme::base());
    let mut state = ListState::default();
    state.select(Some(sel));
    frame.render_stateful_widget(list, inner, &mut state);

    let viewport = inner.height as usize;
    draw_scrollbar(frame, inner, total, state.offset(), viewport);
}

/// A `├`/`└` fix-hint line: glyph + text at col 0/2, anchored to the caller's
/// own key column (no leading indent, unlike [`help_tooltip_lines`]'s one-cell
/// offset for panes whose row opens past col 0). `more_follow` picks `├`
/// (another hint follows in the same rail) vs `└` (closes it, or the lone-hint
/// case with nothing to connect); wrapped continuation lines keep the text at
/// col 2 and carry `│` at col 0 while the rail is still open, blank once it has
/// closed (cloudy-tui Stacked hints).
///
/// The one rail drawer: the Usage tab's `status` block and the Fallback card's
/// blocked-reason pills both render through it, so the two can't drift apart.
pub(super) fn rail_hint_lines(text: &str, width: usize, more_follow: bool) -> Vec<Line<'static>> {
    const LEAD_W: usize = 2; // glyph + 1 space, text at col 2
    let lead = if more_follow { "├ " } else { "└ " };
    let cont = if more_follow { "│ " } else { "  " };
    wrap_words(text, width.saturating_sub(LEAD_W).max(8))
        .into_iter()
        .enumerate()
        .map(|(i, seg)| {
            Line::from(vec![
                Span::styled(if i == 0 { lead } else { cont }, theme::line()),
                Span::styled(seg, theme::faint()),
            ])
        })
        .collect()
}

/// Greedy word-wrap to `width` chars; long words are hard-split. Shared by the
/// status-tab timeline, the tooltip sub-lines, and the chain add-pane prose.
pub(super) fn wrap_words(text: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![text.to_string()];
    }
    let mut lines = Vec::new();
    let mut line = String::new();
    for word in text.split_whitespace() {
        if word.chars().count() > width {
            if !line.is_empty() {
                lines.push(std::mem::take(&mut line));
            }
            let mut chunk = String::new();
            for ch in word.chars() {
                if chunk.chars().count() == width {
                    lines.push(std::mem::take(&mut chunk));
                }
                chunk.push(ch);
            }
            if !chunk.is_empty() {
                line = chunk;
            }
            continue;
        }
        let extra = if line.is_empty() { 0 } else { 1 };
        if line.chars().count() + extra + word.chars().count() > width {
            lines.push(std::mem::take(&mut line));
            line.push_str(word);
        } else {
            if !line.is_empty() {
                line.push(' ');
            }
            line.push_str(word);
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
/// reason render in `DANGER` (cloudy-tui Invalid-input tooltip).
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
    wrap_words(text, width.saturating_sub(LEAD_W).max(8))
        .into_iter()
        .enumerate()
        .map(|(i, seg)| {
            let lead = if i == 0 { " └ " } else { "   " };
            Line::from(vec![
                Span::styled(lead, leader_style),
                Span::styled(seg, text_style),
            ])
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

/// Rounded box with contract-compliant chrome.
///
/// Border: `LINE_STRONG` when focused, `LINE` when blurred.
/// Title: always italic, always UPPERCASE; bold added only when focused.
/// Color: `ACCENT_2` for the first bordered panel on the screen body, `TEXT_DIM` for the rest.
pub(super) fn section_box(title: &str, focused: bool, first: bool) -> Block<'static> {
    section_box_impl(title, focused, first, true, Vec::new())
}

/// Like [`section_box`] but preserves the title's original case — use only when
/// the title is a profile/account name, not a structural label.
pub(super) fn section_box_verbatim(title: &str, focused: bool, first: bool) -> Block<'static> {
    section_box_impl(title, focused, first, false, Vec::new())
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
    section_box_impl(title, focused, first, true, suffix)
}

fn section_box_impl(
    title: &str,
    focused: bool,
    first: bool,
    uppercase: bool,
    suffix: Vec<Span<'static>>,
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
    let label = if uppercase {
        format!(" {} ", title.to_uppercase())
    } else {
        format!(" {} ", title)
    };
    let mut title_spans = vec![Span::styled(label, title_style)];
    title_spans.extend(suffix);
    Block::bordered()
        .border_set(border::ROUNDED)
        .border_style(border_style)
        .title(Line::from(title_spans))
        .padding(Padding::horizontal(1))
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
    draw_selector_list(frame, area, "accounts", focused, sel, |w| {
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
    });
}
