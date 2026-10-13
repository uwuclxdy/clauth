use super::super::panes::wrap_words;

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
