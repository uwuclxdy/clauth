use super::super::panes::wrap_words;
use std::time::Instant;

#[test]
fn short_line_unchanged() {
    assert_eq!(wrap_words("hello world", 36), vec!["hello world"]);
}

#[test]
fn wraps_at_word_boundary() {
    let out = wrap_words("terminal too small · enlarge for full layout", 36);
    assert_eq!(out.len(), 2, "expected 2 wrapped lines, got {out:?}");
    for l in &out {
        assert!(
            l.chars().count() <= 36,
            "line exceeds cap: {l:?} ({} chars)",
            l.chars().count()
        );
    }
}

#[test]
fn empty_input_yields_one_empty_line() {
    assert_eq!(wrap_words("", 36), vec![""]);
}

#[test]
fn single_word_exceeding_cap_hard_breaks() {
    let long_word = "a".repeat(80);
    let out = wrap_words(&long_word, 36);
    assert_eq!(out.len(), 3);
    for l in &out {
        assert!(l.chars().count() <= 36);
    }
}

#[test]
fn wide_glyphs_wrap_by_cells() {
    // `漢` takes two cells: three fill a 6-cell line, and a 5-cell line holds
    // two before the word is split.
    assert_eq!(wrap_words("漢漢漢 漢漢", 6), vec!["漢漢漢", "漢漢"]);
    assert_eq!(wrap_words("漢漢漢 漢漢", 5), vec!["漢漢", "漢", "漢漢"]);
}

#[test]
fn an_emoji_presentation_sequence_splits_as_one_two_cell_glyph() {
    // `⚠️` is U+26A0 + U+FE0F: two chars, one grapheme, two cells.
    let w = "\u{26A0}\u{FE0F}";
    assert_eq!(
        wrap_words(&w.repeat(10), 6),
        vec![w.repeat(3), w.repeat(3), w.repeat(3), w.to_string()]
    );
}

/// `لالالا` (three lam-alef pairs): a whole-string `Span::width` folds each
/// pair to one cell and measures 3, but the buffer draws per grapheme and
/// lays down 6 cells, so a word-width taken from the whole string lets the
/// line overrun its budget. The wrap measures each grapheme's own width, the
/// way [`super::super::format::grapheme_cells`] does, and splits the word.
#[test]
fn a_lam_alef_word_wraps_by_its_drawn_cells() {
    let out = wrap_words("لالالا", 3);
    // Each letter is its own one-cell grapheme, so a 3-cell fill cuts after
    // three letters: ["لال", "الا"].
    assert_eq!(out, vec!["لال", "الا"]);
    for line in &out {
        assert!(
            super::super::format::grapheme_cells(line) <= 3,
            "a wrapped line overruns the 3-cell budget: {out:?}"
        );
    }
    assert_eq!(out.join(""), "لالالا", "no grapheme is lost in the split");
}

/// The stack's wrap budget and column width measure drawn cells: a toast body
/// interpolates profile names, and `لالالا` draws 6 cells where a whole-string
/// width reads 3 — a whole-string budget force-wraps the line, and a
/// whole-string column leaves the row short of its drawn width plus the
/// 3-cell chrome.
#[test]
fn a_lam_alef_name_in_a_toast_budgets_by_drawn_cells() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = crate::tui::app::App::new(crate::profile::AppConfig {
        state: crate::profile::AppState::default(),
        profiles: Vec::new(),
    });
    app.toast(crate::tui::app::ToastKind::Info, "switched to 'لالالا'");
    let laid = super::layout(ratatui::layout::Rect::new(0, 0, 80, 24), &app);
    assert_eq!(laid.len(), 1, "one toast lays out: {}", laid.len());
    assert_eq!(
        laid[0].rows.len(),
        1,
        "the body fits one row at its drawn width: {:?}",
        laid[0].rows
    );
    // `switched to 'لالالا'`: 13 ASCII + the name's 6 drawn cells (three
    // lam-alef pairs, one cell per letter) + the closing quote = 20.
    assert_eq!(
        laid[0].rect.width, 23,
        "the column is the row's 20 drawn cells plus the 3-cell chrome: rect {:?}",
        laid[0].rect
    );
}

/// A cut toast's last kept row frees a cell for `…` by drawn width: a lam-alef
/// name draws 6 where a whole-string sum reads 3, so that sum pops
/// nothing and lands the `…` a drawn cell past the budget. The body makes
/// `لالالا m` the last kept row (4 lines, capped at 3): its drawn width 8
/// equals the budget, so the ellipsis must replace one trailing grapheme.
#[test]
fn a_truncated_toasts_last_row_frees_ellipsis_cells_by_drawn_width() {
    use crate::tui::app::{Toast, ToastKind};

    let toast = Toast {
        kind: ToastKind::Info,
        body: "h\nzz\nلالالا m\nzzz".into(),
        born: Instant::now(),
    };
    let rows = super::toast_rows(&toast, 8);
    assert_eq!(rows.len(), 3, "a 4-line body keeps 3 rows: {rows:?}");
    let last = &rows[2];
    // Hand-derived: the row `لالالا m` draws 8, the budget's worth, so the
    // ellipsis frees its cell by popping the trailing `m`: `لالالا ` + `…`.
    let last_text: String = last.iter().map(|span| span.content.as_ref()).collect();
    assert_eq!(last_text, "لالالا …", "the ellipsis replaces the popped `m`");
    assert_eq!(last.last().map(|span| span.content.as_ref()), Some("…"));
    let drawn: usize = last
        .iter()
        .map(|span| super::super::format::grapheme_cells(span.content.as_ref()))
        .sum();
    assert!(
        drawn <= 8,
        "the last kept row overruns its budget with the ellipsis: {last:?}"
    );
}

/// A cut toast's last kept row draws the whole budget and ends in a zero-width
/// grapheme (U+200B): the first pop frees no cell, so the ellipsis needs the
/// pop loop to pass again over the same row. Stopped after one pass, the row
/// keeps its budget's worth of drawn text and then adds `…` a cell past it.
#[test]
fn a_truncated_toast_row_ending_in_a_zero_width_grapheme_frees_a_whole_cell() {
    use crate::tui::app::{Toast, ToastKind};

    // `abcdefgh` draws 8, the budget's worth; U+200B draws none, so the first
    // pop takes the zero-width grapheme and leaves the row still full.
    let toast = Toast {
        kind: ToastKind::Info,
        body: "h\nzz\nabcdefgh\u{200B}\nzzz".into(),
        born: Instant::now(),
    };
    let rows = super::toast_rows(&toast, 8);
    assert_eq!(rows.len(), 3, "a 4-line body keeps 3 rows: {rows:?}");
    let last = &rows[2];
    let last_text: String = last.iter().map(|span| span.content.as_ref()).collect();
    assert_eq!(
        last_text, "abcdefg…",
        "the second pop pass frees the cell the `…` takes"
    );
    let drawn: usize = last
        .iter()
        .map(|span| super::super::format::grapheme_cells(span.content.as_ref()))
        .sum();
    assert!(
        drawn <= 8,
        "the last kept row overruns its budget with the ellipsis: {last:?}"
    );
}

/// `ｶﾞｶﾞ` is two clusters drawing 2 cells each (the buffer advances by
/// `CellWidth::cell_width`: `unicode-width` plus one per halfwidth sound
/// mark), so 4 drawn cells where a per-grapheme `Span::width` sum reads 2 —
/// that measure keeps the word whole in a 3-cell budget, drawing 4. The
/// wrap splits after one cluster: each row draws 2.
#[test]
fn a_katakana_sound_mark_word_wraps_by_its_drawn_cells() {
    let out = wrap_words("ｶﾞｶﾞ", 3);
    assert_eq!(out, vec!["ｶﾞ", "ｶﾞ"]);
    for line in &out {
        assert!(
            super::super::format::grapheme_cells(line) <= 3,
            "a wrapped line overruns the 3-cell budget: {out:?}"
        );
    }
    assert_eq!(out.join(""), "ｶﾞｶﾞ", "no grapheme is lost in the split");
}

#[test]
fn col_width_equals_content_width_plus_chrome() {
    let msg = "· enlarge for full layout";
    let content_cap: u16 = 36;
    let max_content_width = [msg]
        .iter()
        .map(|l| l.chars().count() as u16)
        .max()
        .unwrap()
        .min(content_cap);
    let col_width = max_content_width + 3;
    assert_eq!(max_content_width, 25);
    assert_eq!(col_width, 28);
}

/// Every span a toast lays out carries no background of its own, so the
/// cell-level glass blend stays visible under the text (a span bg would paint a
/// flat fill under it). The bar span (`┃ `) is built inside `draw`, not here,
/// and stays a named residual: the glass overwrites every cell's bg after the
/// spans, so a span bg is not cell-observable — this pins at span level.
#[test]
fn toast_rows_set_no_background_of_their_own() {
    use crate::tui::app::{Toast, ToastKind};

    // A head that wraps onto a second row, a detail row, then a body cut to the
    // three-row cap with its `…`: title (bold), continuation, detail and the
    // ellipsis spans must all carry `bg == None`.
    let toast = Toast {
        kind: ToastKind::Danger,
        body: format!("{}\ndetail line\n{}", "y".repeat(60), "z".repeat(60)),
        born: Instant::now(),
    };
    let rows = super::toast_rows(&toast, 57);
    assert!(!rows.is_empty(), "a toast lays out at least one row");
    for (r, row) in rows.iter().enumerate() {
        for (c, span) in row.iter().enumerate() {
            assert_eq!(
                span.style.bg, None,
                "span ({r},{c}) {:?} sets no bg",
                span.content
            );
        }
    }
}

/// A toast built from CLI copy names its command in backticks; the command
/// stays on one row at a budget that used to split it.
#[test]
fn a_backticked_command_in_a_toast_stays_on_one_row() {
    use crate::tui::app::{Toast, ToastKind};
    let toast = Toast {
        kind: ToastKind::Info,
        body: super::super::prose::mark_commands(
            "update available\nreinstall with `cargo install clauth`",
        ),
        born: Instant::now(),
    };
    let rows: Vec<String> = super::toast_rows(&toast, 20)
        .iter()
        .map(|row| row.iter().map(|span| span.content.as_ref()).collect())
        .collect();
    assert_eq!(
        rows,
        vec![
            "update available".to_string(),
            "reinstall with".to_string(),
            "cargo install clauth".to_string(),
        ]
    );
}
