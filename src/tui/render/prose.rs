//! Keys and typed commands inside TUI prose:
//! copy marks them with [`key`] / [`cmd`], every render site
//! turns the marked string into spans through [`spans`] or [`wrap`].
//!
//! The markers are private-use code points, so no literal copy can hold one:
//! a bracket marker would collide with copy that prints `[server.admin]` or a
//! `[ 3 live ]` chip.

use std::ops::Range;

use ratatui::style::{Modifier, Style};
use ratatui::text::Span;

use super::super::theme;
use super::format::{grapheme_cells, prefix_in_cells};
use super::panes;

const KEY_OPEN: char = '\u{E000}';
const CMD_OPEN: char = '\u{E001}';
const CLOSE: char = '\u{E002}';

/// Whether `ch` is one of the marker code points ([`KEY_OPEN`], [`CMD_OPEN`],
/// [`CLOSE`]): a copy source must escape it before the string reaches any
/// tokenizer entry in this module, so an untrusted string cannot style the
/// cells after it or drop a character.
pub(crate) fn is_marker(ch: char) -> bool {
    matches!(ch, KEY_OPEN | CMD_OPEN | CLOSE)
}

/// [`key`] for `&'static` copy built with `concat!`; spells the markers out
/// because `concat!` takes literals only.
macro_rules! key_lit {
    ($name:literal) => {
        concat!("\u{E000}", $name, "\u{E002}")
    };
}

/// [`cmd`] for `&'static` copy built with `concat!`.
macro_rules! cmd_lit {
    ($command:literal) => {
        concat!("\u{E001}", $command, "\u{E002}")
    };
}

pub(crate) use {cmd_lit, key_lit};

/// How a visible character draws: plain prose, a key, or a command.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Face {
    Prose,
    Key,
    Cmd,
}

/// A visible character's face and the marked run it belongs to. The run id is
/// what lets two runs of one face pressed together part at their edge: the
/// markers between them drop, so the face alone cannot tell them apart.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Kind {
    face: Face,
    run: usize,
}

/// A plain prose character.
const PROSE: Kind = Kind {
    face: Face::Prose,
    run: 0,
};

impl Kind {
    fn marked(self) -> bool {
        self.face != Face::Prose
    }
}

/// A key named in running copy (`press {key("d")} to resolve`).
pub(crate) fn key(name: &str) -> String {
    format!("{KEY_OPEN}{name}{CLOSE}")
}

/// A command the end user types (`run {cmd("clauth login")}`).
pub(crate) fn cmd(command: &str) -> String {
    format!("{CMD_OPEN}{command}{CLOSE}")
}

/// The one fix for a setup token that is refused or expired: a new mint,
/// captured into the profile.
pub(crate) fn setup_token_remint_fix(name: &str) -> String {
    format!(
        "re-mint with {}",
        cmd(&format!("clauth login {name} --setup-token"))
    )
}

/// A binary path is never longer than Linux's `PATH_MAX`; capping the
/// candidates there keeps a huge backticked span (a tool's whole stderr) from
/// costing a rescan per space.
const BINARY_PATH_MAX: usize = 4096;

/// The CLIs whose commands clauth's copy tells the end user to run.
const COMMAND_BINARIES: &[&str] = &["clauth", "claude", "codex", "herdr", "shunt", "cargo"];

/// Copy shared with the CLI (an error, a `format::Message`) names a command in
/// backticks. A toast built from it marks each backticked span that is a
/// command (one of [`COMMAND_BINARIES`], bare or as a path's file stem, a path
/// holding spaces included, then at least one more word, on one line) and
/// leaves every other backticked span (an error code, a config key, a bare
/// tool name) as written.
pub(crate) fn mark_commands(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(open) = rest.find('`') {
        let after = &rest[open + 1..];
        let Some(close) = after.find('`') else {
            break;
        };
        let span = &after[..close];
        out.push_str(&rest[..=open]);
        if is_command(span) {
            out.pop();
            out.push_str(&cmd(span));
        } else {
            out.push_str(span);
            out.push('`');
        }
        rest = &after[close + 1..];
    }
    out.push_str(rest);
    out
}

/// Every prefix ending at a space is a candidate binary, so a path holding a
/// space (`C:\Program Files\…\shunt.exe`) still counts; a candidate with a
/// separator must start like a path, one without must be a single word, so
/// `foo /usr/bin/clauth x` is no command. A drive letter opens a path only as
/// an ASCII letter, `:`, then a separator — a bare colon at the second byte
/// (`a:b/clauth`) is no path start. Both separators are split by hand so a
/// Windows path reads the same on every host.
fn is_command(span: &str) -> bool {
    !span.contains('\n')
        && span
            .match_indices(' ')
            .take_while(|(at, _)| *at <= BINARY_PATH_MAX)
            .any(|(at, _)| {
                let candidate = &span[..at];
                let shaped = if candidate.contains(['/', '\\']) {
                    candidate.starts_with(['/', '\\', '~', '.'])
                        || matches!(candidate.as_bytes(), [letter, b':', sep, ..]
                            if letter.is_ascii_alphabetic() && (*sep == b'/' || *sep == b'\\'))
                } else {
                    !candidate.contains(' ')
                };
                let leaf = candidate.rsplit(['/', '\\']).next().unwrap_or_default();
                let stem = leaf.rsplit_once('.').map_or(leaf, |(stem, _)| stem);
                shaped && COMMAND_BINARIES.contains(&stem) && !span[at + 1..].trim().is_empty()
            })
}

/// The text as it reads on screen: every marker dropped.
pub(crate) fn plain(text: &str) -> String {
    classify(text).into_iter().map(|(ch, _)| ch).collect()
}

/// One line of marked copy as spans: prose in `base`, a key `ACCENT + bold`,
/// a command `ACCENT` without bold, each keeping `base`'s background.
pub(crate) fn spans(text: &str, base: Style) -> Vec<Span<'static>> {
    runs(&classify(text), base)
}

/// [`spans`] plus the grapheme ranges that wrap as one unit, for a wrapper
/// that breaks a line after it was styled ([`super::modals::chunk_line`]).
pub(crate) struct Marked {
    spans: Vec<Span<'static>>,
    units: Vec<Range<usize>>,
}

impl Marked {
    pub(crate) fn into_parts(self) -> (Vec<Span<'static>>, Vec<Range<usize>>) {
        (self.spans, self.units)
    }
}

/// [`spans`] keeping which graphemes are a marked unit.
pub(crate) fn marked(text: &str, base: Style) -> Marked {
    // Whitespace inside a marked run is settled by [`settle`], the routine
    // [`wrap`] also runs.
    let chars = settle(classify(text));
    let mut units = Vec::new();
    let mut open: Option<usize> = None;
    let mut grapheme = 0usize;
    // Count graphemes span by span, the way the modal body's wrapper walks the
    // styled line: it maps a tab to one space up front, and a grapheme whose
    // text holds another control character is dropped by both walks, so a
    // cluster straddling a span edge counts once per side. A span is one opener's
    // run, so the ranges part exactly where two runs meet.
    for (kind, run) in kind_runs(&chars) {
        let run = run.replace('\t', " ");
        let held = Span::raw(run.as_str())
            .styled_graphemes(Style::default())
            .count();
        if let Some(start) = open.take() {
            units.push(start..grapheme);
        }
        if kind.marked() {
            open = Some(grapheme);
        }
        grapheme += held;
    }
    if let Some(start) = open {
        units.push(start..grapheme);
    }
    Marked {
        spans: runs(&chars, base),
        units,
    }
}

/// Whether `ch` is the whitespace a marked run settles: ASCII space and tab
/// alone. Every other whitespace (a non-breaking space included) is data.
fn marked_whitespace(ch: char) -> bool {
    matches!(ch, ' ' | '\t')
}

/// A marked run holding only ASCII spaces and tabs, past any [`absent`]
/// character: it draws nothing, so the wrapper collapses it to the one
/// separator cell between the words around it.
fn blank_run(chars: &[(char, Kind)]) -> bool {
    chars.iter().any(|(ch, _)| marked_whitespace(*ch))
        && chars
            .iter()
            .all(|(ch, _)| marked_whitespace(*ch) || absent(*ch))
}

/// Collapse each run of [`marked_whitespace`], with any [`absent`] character
/// inside or after it, to one ordinary space and drop a trailing run, unless
/// `pressed` keeps the one space that separates the run's last word from prose
/// glued to it.
fn collapse_marked_whitespace(chars: &[(char, Kind)], pressed: bool) -> Vec<(char, Kind)> {
    let mut out = Vec::with_capacity(chars.len());
    let mut i = 0;
    while i < chars.len() {
        let (ch, kind) = chars[i];
        if marked_whitespace(ch) {
            let mut end = i;
            while end < chars.len() && (marked_whitespace(chars[end].0) || absent(chars[end].0)) {
                end += 1;
            }
            if end < chars.len() || pressed {
                out.push((' ', kind));
            }
            i = end;
            continue;
        }
        out.push((ch, kind));
        i += 1;
    }
    out
}

/// Drop a marked run's leading ASCII spaces and tabs, with any [`absent`]
/// character among them: they repeat a separator the run already follows, or
/// start the line where nothing precedes them.
fn drop_leading_marked_whitespace(chars: &mut Vec<(char, Kind)>) {
    let lead = chars
        .iter()
        .take_while(|(ch, kind)| kind.marked() && (marked_whitespace(*ch) || absent(*ch)))
        .count();
    chars.drain(..lead);
}

/// [`settled`] when a marked run holds whitespace it settles; any other copy
/// keeps its characters byte for byte.
fn settle(chars: Vec<(char, Kind)>) -> Vec<(char, Kind)> {
    if chars
        .iter()
        .any(|(ch, kind)| kind.marked() && marked_whitespace(*ch))
    {
        settled(&chars)
    } else {
        chars
    }
}

/// Whether `ch` is neither whitespace nor a control character: the visible
/// text an edge decision weighs (a joiner or a combining mark counts, though
/// it takes no cell of its own).
fn draws(ch: char) -> bool {
    !ch.is_whitespace() && !ch.is_control()
}

/// A control character other than the tab, which draws as a space: it draws
/// no cell and is never a break on a marked line, so an edge or break
/// decision looks past it. Two places still read it: [`tokens`], which keeps
/// a control-only word on a line that fits and keeps the whitespace runs
/// beside a dropped one apart, and [`space_glued_to_next`], where it parts a
/// breaking space from the joiner or combining mark after it.
fn absent(ch: char) -> bool {
    ch.is_control() && ch != '\t'
}

/// The characters left once whitespace inside marked runs is settled: the stream
/// [`fill`] places. Only a run's own whitespace changes, plus the one
/// separator a whitespace-only run stands in for: prose keeps its indent, its
/// doubled spaces, its trailing cells and its non-breaking glue exactly as
/// written. Every edge test looks past an [`absent`] character.
fn settled(chars: &[(char, Kind)]) -> Vec<(char, Kind)> {
    let mut out: Vec<(char, Kind)> = Vec::with_capacity(chars.len());
    let mut i = 0;
    // Set while the one separator a whitespace-only run stood in for still
    // holds: the prose whitespace around it has nothing left to separate.
    let mut sep = false;
    while i < chars.len() {
        let (ch, kind) = chars[i];
        if !kind.marked() {
            if sep && (marked_whitespace(ch) || absent(ch)) {
                i += 1;
                continue;
            }
            sep = false;
            out.push((ch, kind));
            i += 1;
            continue;
        }
        let start = i;
        // Absent prose between two marked runs parts nothing: they settle as
        // the one run they are with it gone.
        while i < chars.len() {
            let gap = chars[i..]
                .iter()
                .take_while(|(ch, kind)| !kind.marked() && absent(*ch))
                .count();
            if !chars.get(i + gap).is_some_and(|(_, kind)| kind.marked()) {
                break;
            }
            i += gap + 1;
        }
        let run = &chars[start..i];
        if blank_run(run) {
            let before = out.iter().any(|(ch, _)| draws(*ch));
            let after = chars[i..].iter().any(|(ch, _)| draws(*ch));
            sep = before && after;
            if sep {
                // The blank run around the unit collapses to one cell: a
                // non-breaking cell prose already holds stays, any other is
                // one plain space.
                let mut held = None;
                while let Some(&(ch, _)) = out.last() {
                    if draws(ch) {
                        break;
                    }
                    let cell = out.pop();
                    if held.is_none()
                        && cell.is_some_and(|(ch, _)| !marked_whitespace(ch) && !absent(ch))
                    {
                        held = cell;
                    }
                }
                out.push(held.unwrap_or((' ', PROSE)));
            }
            continue;
        }
        let pressed = chars[i..]
            .iter()
            .find(|(ch, _)| !absent(*ch))
            .is_some_and(|(ch, kind)| !kind.marked() && draws(*ch));
        let mut kept = collapse_marked_whitespace(run, pressed);
        if chars[..start]
            .iter()
            .rfind(|(ch, _)| !absent(*ch))
            .is_none_or(|(ch, _)| !draws(*ch))
        {
            drop_leading_marked_whitespace(&mut kept);
        }
        out.extend_from_slice(&kept);
        sep = false;
    }
    out
}

/// [`spans`] cut to `width` display cells, grapheme-safe: a trailing `…` in
/// `base` when anything was dropped. The budget counts only visible characters
/// (markers never reach the screen), and a wide glyph (`⚠️`) counts its two
/// cells so the `…` stays inside the pane.
pub(crate) fn fit(text: &str, width: usize, base: Style) -> Vec<Span<'static>> {
    // Control graphemes never reach the screen, so they drop out of both the
    // measurement and the kept list — otherwise `chars[..consumed]` indexes a
    // list that still holds them and the cut lands inside a grapheme.
    let chars: Vec<(char, Kind)> = classify(text)
        .into_iter()
        .filter(|(c, _)| !c.is_control())
        .collect();
    let visible: String = chars.iter().map(|(ch, _)| *ch).collect();
    if grapheme_cells(&visible) <= width {
        return runs(&chars, base);
    }
    if width == 0 {
        return Vec::new();
    }
    // One cell is reserved for `…`; the rest fills with whole graphemes.
    let (kept_text, _) = prefix_in_cells(&visible, width - 1);
    let consumed = kept_text.chars().count();
    let mut kept: Vec<(char, Kind)> = chars[..consumed].to_vec();
    kept.push(('…', PROSE));
    runs(&kept, base)
}

/// [`wrap_words`] over marked copy: the wrap budget counts only visible
/// characters, each wrapped line keeps the styling its characters had, and a
/// marked key or command wraps as ONE unit — it joins the row when it fits
/// beside what is there, else starts the next row when it fits one alone, else
/// breaks at its own spaces like prose (only a word, or a glue run, wider than
/// the row hard-splits, by grapheme). Units joined by punctuation wrap as one
/// run, and glue pressed against an over-wide run's first or last word takes a
/// row of its own when that word fits one but the two together do not. Inside
/// such a run, two joined runs part where they meet and the punctuation between
/// them rides the word before it. Prose-only copy wraps exactly as
/// [`wrap_words`].
pub(crate) fn wrap(text: &str, width: usize, base: Style) -> Vec<Vec<Span<'static>>> {
    let chars = classify(text);
    let marked_line = chars.iter().any(|(_, kind)| kind.marked());
    // Settled by [`settle`], the routine [`marked`] also runs.
    let chars = settle(chars);
    fill(&chars, width, marked_line)
        .into_iter()
        .map(|line| runs(&line, base))
        .collect()
}

/// [`wrap`]'s greedy fill as plain strings: the no-unit entry point
/// [`super::panes::wrap_words`] takes.
pub(crate) fn plain_wrap(text: &str, width: usize) -> Vec<String> {
    let chars: Vec<(char, Kind)> = text.chars().map(|ch| (ch, PROSE)).collect();
    fill(&chars, width, false)
        .into_iter()
        .map(|line| line.into_iter().map(|(ch, _)| ch).collect())
        .collect()
}

/// One wrap token: visible characters with their kinds, and the whitespace run
/// before it. A marked run is one token however many spaces it holds, which is
/// what keeps a key or command from breaking; a prose word is one token.
struct Token {
    chars: Vec<(char, Kind)>,
    width: usize,
    /// The separator cells the whitespace before the token draws, empty when
    /// the token follows the previous one with nothing between
    /// (`run{cmd("x")}`). A contiguous whitespace run collapses to one space;
    /// a dropped control-only word or marked run keeps the runs on either
    /// side apart, so each stands as its own space.
    sep: Vec<(char, Kind)>,
}

/// Whether the whitespace at `at` draws as one grapheme with the char after
/// it (a combining mark, a joiner, a variation selector): the buffer keeps
/// the two together, so the space is data, never a break. The pair glues only
/// inside one span, and never around a control char, which draws nothing.
fn space_glued_to_next(chars: &[(char, Kind)], at: usize) -> bool {
    let Some(&(next, next_kind)) = chars.get(at + 1) else {
        return false;
    };
    let (lead, lead_kind) = chars[at];
    if lead_kind != next_kind || lead.is_control() || next.is_control() {
        return false;
    }
    let pair: String = [lead, next].into_iter().collect();
    Span::raw(pair.as_str())
        .styled_graphemes(Style::default())
        .count()
        == 1
}

fn tokens(chars: &[(char, Kind)], glue_aware: bool, drop_controls: bool) -> Vec<Token> {
    // A prose break is whitespace the wrapper may cut at, except a space the
    // buffer draws as one grapheme with the char after it
    // (`space_glued_to_next`), on both paths: a cut there would part one
    // grapheme across two rows. The marked line keeps its remaining stricter
    // rule: a non-breaking space is data there, while prose-only copy may
    // still cut at it.
    let is_break = |at: usize| {
        if glue_aware {
            marked_break(chars, at)
        } else {
            chars[at].0.is_whitespace() && !space_glued_to_next(chars, at)
        }
    };
    let mut out: Vec<Token> = Vec::new();
    let mut sep: Vec<(char, Kind)> = Vec::new();
    // A dropped control-only word or marked run keeps the whitespace runs on
    // either side apart: the next run is a separate separator cell, not one
    // more space in the run before it.
    let mut control_gap = false;
    let mut i = 0;
    while i < chars.len() {
        let (ch, kind) = chars[i];
        if !kind.marked() && is_break(i) {
            let mut end = i + 1;
            while end < chars.len() && !chars[end].1.marked() && is_break(end) {
                end += 1;
            }
            finalize(&mut out);
            if sep.is_empty() || control_gap {
                sep.push((' ', kind));
            }
            control_gap = false;
            i = end;
            continue;
        }
        // A control-only prose word draws nothing: drop it without touching
        // the separator, so the whitespace runs on either side never merge.
        if drop_controls && !kind.marked() && ch.is_control() {
            let mut end = i + 1;
            while end < chars.len() && !chars[end].1.marked() && chars[end].0.is_control() {
                end += 1;
            }
            control_gap = true;
            i = end;
            continue;
        }
        let marked = kind.marked();
        let start = i;
        while i < chars.len() {
            let (_, kind) = chars[i];
            let same = if marked {
                kind.marked()
            } else {
                !kind.marked() && !is_break(i)
            };
            if !same {
                break;
            }
            i += 1;
        }
        // A marked run of control characters alone draws nothing either: it
        // drops the same way, so no row is cut for it.
        if marked && drop_controls && chars[start..i].iter().all(|(ch, _)| ch.is_control()) {
            control_gap = true;
            continue;
        }
        if sep.is_empty()
            && let Some(last) = out.last_mut()
        {
            // Pressed against the token before it: keep the visible text.
            last.chars.extend_from_slice(&chars[start..i]);
            continue;
        }
        finalize(&mut out);
        out.push(Token {
            chars: chars[start..i].to_vec(),
            width: 0,
            sep: std::mem::take(&mut sep),
        });
        control_gap = false;
    }
    finalize(&mut out);
    out
}

/// Measure a finished token once nothing can press against it any more. Its
/// whitespace is already settled, so only the width is still open.
fn finalize(out: &mut [Token]) {
    let Some(last) = out.last_mut() else {
        return;
    };
    last.width = drawn_cells(&last.chars);
}

/// The cells a run draws, summed per grapheme: a control character takes none
/// and a lam-alef pair takes two, one per letter, so a cut decided on this
/// measure never leaves a row the buffer overruns.
fn drawn_cells(chars: &[(char, Kind)]) -> usize {
    let text: String = chars.iter().map(|(ch, _)| *ch).collect();
    grapheme_cells(&text)
}

/// Greedy fill to `width` cells, measured the way the buffer renders: by
/// grapheme, a wide one taking two.
fn fill(chars: &[(char, Kind)], width: usize, marked_line: bool) -> Vec<Vec<(char, Kind)>> {
    let owned;
    let chars = if marked_line {
        owned = chars
            .iter()
            .map(|&(ch, kind)| (if ch == '\t' { ' ' } else { ch }, kind))
            .collect::<Vec<_>>();
        owned.as_slice()
    } else {
        chars
    };
    // A control-only word or marked run draws nothing; the buffer drops it,
    // never the wrapper, except on a marked line long enough to wrap, where
    // the modal drops it during its flatten and the wrapper must too.
    let drop_controls = marked_line && drawn_cells(chars) > width;
    let mut lines: Vec<Vec<(char, Kind)>> = Vec::new();
    let mut cur: Vec<(char, Kind)> = Vec::new();
    let mut cur_w = 0usize;
    for token in tokens(chars, marked_line, drop_controls) {
        let sep: &[(char, Kind)] = if cur.is_empty() { &[] } else { &token.sep };
        let sep_w = drawn_cells(sep);
        if token.width <= width && cur_w + sep_w + token.width <= width {
            cur.extend_from_slice(sep);
            cur_w += sep_w;
            cur.extend_from_slice(&token.chars);
            cur_w += token.width;
            continue;
        }
        if token.width <= width {
            if !cur.is_empty() {
                lines.push(std::mem::take(&mut cur));
            }
            cur.extend_from_slice(&token.chars);
            cur_w = token.width;
            continue;
        }
        // Too wide for any row: glued punctuation is placed beside the marked
        // run, which keeps a row it fits; otherwise the token is no unit any
        // more and its words fill from where the row already stands.
        place_overflow(
            &token,
            sep,
            width,
            &mut cur,
            &mut cur_w,
            &mut lines,
            marked_line,
        );
    }
    if !cur.is_empty() {
        lines.push(cur);
    }
    if lines.is_empty() {
        lines.push(Vec::new());
    }
    lines
}

/// A marked character that is not [`absent`]: what a run's bounds and its
/// joins are read off.
fn drawn_mark(&(ch, kind): &(char, Kind)) -> bool {
    kind.marked() && !absent(ch)
}

/// The first and last [`drawn_mark`] in a token: punctuation pressed against
/// a marked run sits outside these bounds.
fn unit_bounds(chars: &[(char, Kind)]) -> Option<(usize, usize)> {
    let start = chars.iter().position(drawn_mark)?;
    let end = chars.iter().rposition(drawn_mark)? + 1;
    Some((start, end))
}

/// Place a token no row holds whole. When it carries a marked run that fits a
/// row, the punctuation glued to the run is never measured with it: the
/// leading glue stays on the row it fits (else opens the next), the run takes
/// a row it fits, and the trailing glue rides the run's row when it fits there
/// (else opens the next). A run wider than a row breaks at its own spaces, the
/// punctuation glued to its first or last word taking a row of its own when
/// that word fits one but the two together do not. Otherwise the token breaks
/// at its spaces like prose.
#[allow(clippy::too_many_arguments)]
fn place_overflow(
    token: &Token,
    sep: &[(char, Kind)],
    width: usize,
    cur: &mut Vec<(char, Kind)>,
    cur_w: &mut usize,
    lines: &mut Vec<Vec<(char, Kind)>>,
    glue_aware: bool,
) {
    let Some((start, end)) = unit_bounds(&token.chars) else {
        break_at_spaces(
            &token.chars,
            sep,
            width,
            cur,
            cur_w,
            lines,
            None,
            glue_aware,
        );
        return;
    };
    let unit = &token.chars[start..end];
    if drawn_cells(unit) > width {
        break_at_spaces(
            &token.chars,
            sep,
            width,
            cur,
            cur_w,
            lines,
            Some((start, end)),
            glue_aware,
        );
        return;
    }
    let lead = &token.chars[..start];
    if lead.iter().any(|(ch, _)| !absent(*ch)) {
        place_word(lead, sep, width, cur, cur_w, lines);
        place_unit(unit, &[], width, cur, cur_w, lines);
    } else {
        // A lead of absent characters draws nothing: it rides the run, so
        // the separator stays the run's.
        place_unit(&token.chars[..end], sep, width, cur, cur_w, lines);
    }
    if end < token.chars.len() {
        place_word(&token.chars[end..], &[], width, cur, cur_w, lines);
    }
}

/// Append a marked run that fits a row as one piece: beside what the row holds
/// when the run and the whitespace before it fit there, else on a row of its
/// own.
fn place_unit(
    unit: &[(char, Kind)],
    sep: &[(char, Kind)],
    width: usize,
    cur: &mut Vec<(char, Kind)>,
    cur_w: &mut usize,
    lines: &mut Vec<Vec<(char, Kind)>>,
) {
    let unit_w = drawn_cells(unit);
    let sep = if cur.is_empty() { &[] } else { sep };
    let sep_w = drawn_cells(sep);
    if !cur.is_empty() && *cur_w + sep_w + unit_w > width {
        lines.push(std::mem::take(cur));
        *cur_w = 0;
    } else {
        cur.extend_from_slice(sep);
        *cur_w += sep_w;
    }
    cur.extend_from_slice(unit);
    *cur_w += unit_w;
}

/// One piece of a token that breaks at its spaces: visible characters, the
/// separator cells the whitespace run before them draws, and whether the piece
/// is punctuation joining two marked runs. That glue rides the word before it
/// or takes a row of its own, never the word after it; two runs pressed
/// together part where they meet.
struct Piece {
    chars: Vec<(char, Kind)>,
    sep: Vec<(char, Kind)>,
    glue: bool,
}

/// Whitespace that breaks a row: a non-breaking space is data and stays inside
/// the word beside it.
fn break_space(ch: char) -> bool {
    ch.is_whitespace() && !matches!(ch, '\u{a0}' | '\u{202f}' | '\u{2007}')
}

/// Whether the grapheme at `at` is a break point on a marked line: breakable
/// whitespace the buffer does not glue to the next grapheme. A control
/// character draws nothing, so it never breaks. [`fill`] maps a tab to a space
/// before either walker that cuts a marked line runs this one predicate.
fn marked_break(chars: &[(char, Kind)], at: usize) -> bool {
    let ch = chars[at].0;
    !ch.is_control() && break_space(ch) && !space_glued_to_next(chars, at)
}

/// Place a token's words one row at a time: a marked run wider than a row
/// breaks at its own spaces, and a lone word wider than the row by grapheme.
/// `first` is the separator before the token's first word. On a marked line a
/// break run draws its own characters where it separates two words, and one
/// ending the token is trailing whitespace: the cells of it that fit the row
/// stay. `unit` names the run's bounds when the token holds one wider than a
/// row: a join between two runs inside it parts where the runs meet, and its
/// first and last word shed the punctuation pressed against them when the
/// word fits a row but the two together do not, so neither word is cut for
/// the glue.
#[allow(clippy::too_many_arguments)]
fn break_at_spaces(
    chars: &[(char, Kind)],
    first: &[(char, Kind)],
    width: usize,
    cur: &mut Vec<(char, Kind)>,
    cur_w: &mut usize,
    lines: &mut Vec<Vec<(char, Kind)>>,
    unit: Option<(usize, usize)>,
    glue_aware: bool,
) {
    let breaks = |at: usize| {
        if glue_aware {
            marked_break(chars, at)
        } else {
            chars[at].0.is_whitespace() && !space_glued_to_next(chars, at)
        }
    };
    let mut pieces: Vec<Piece> = Vec::new();
    let mut piece: Vec<(char, Kind)> = Vec::new();
    let mut sep = first.to_vec();
    for (i, &(ch, kind)) in chars.iter().enumerate() {
        if breaks(i) {
            if !piece.is_empty() {
                pieces.push(Piece {
                    chars: std::mem::take(&mut piece),
                    sep: std::mem::take(&mut sep),
                    glue: false,
                });
            }
            if glue_aware {
                sep.push((ch, kind));
            } else {
                sep = vec![(' ', kind)];
            }
            continue;
        }
        // A join is read off the characters that draw: an [`absent`] one
        // neither starts a run's join nor ends the glue before it.
        if let Some((start, end)) = unit
            && i > start
            && i < end
            && kind.marked()
            && !absent(ch)
            && !piece.is_empty()
            && let Some(prev) = chars[..i].iter().rposition(|(ch, _)| !absent(*ch))
        {
            let (_, prev_kind) = chars[prev];
            let joins = if prev_kind.marked() {
                prev_kind != kind
            } else {
                !breaks(prev)
            };
            if joins {
                let glue = if prev_kind.marked() {
                    0
                } else {
                    piece
                        .iter()
                        .rev()
                        .enumerate()
                        .take_while(|(off, (ch, kind))| {
                            absent(*ch) || (!kind.marked() && !breaks(i - 1 - off))
                        })
                        .count()
                };
                if glue == piece.len() {
                    // The glue is the whole piece: a space kept before it
                    // separates it from the word it rides, and it ends the row
                    // when it cannot ride, never carrying the word after it.
                    pieces.push(Piece {
                        chars: std::mem::take(&mut piece),
                        sep: std::mem::take(&mut sep),
                        glue: true,
                    });
                } else if glue > 0 {
                    let tail = piece.split_off(piece.len() - glue);
                    pieces.push(Piece {
                        chars: std::mem::take(&mut piece),
                        sep: std::mem::take(&mut sep),
                        glue: false,
                    });
                    pieces.push(Piece {
                        chars: tail,
                        sep: Vec::new(),
                        glue: true,
                    });
                } else {
                    pieces.push(Piece {
                        chars: std::mem::take(&mut piece),
                        sep: std::mem::take(&mut sep),
                        glue: false,
                    });
                }
            }
        }
        piece.push((ch, kind));
    }
    let mut trailing = if piece.is_empty() && glue_aware {
        std::mem::take(&mut sep)
    } else {
        Vec::new()
    };
    if !piece.is_empty() {
        pieces.push(Piece {
            chars: piece,
            sep,
            glue: false,
        });
    }
    if let Some((start, end)) = unit {
        if start > 0 && start < pieces.first().map_or(0, |piece| piece.chars.len()) {
            let rest = pieces[0].chars.split_off(start);
            let lead = std::mem::replace(&mut pieces[0].chars, rest);
            let rest_w = drawn_cells(&pieces[0].chars);
            if drawn_cells(&lead) + rest_w > width && rest_w <= width {
                let before = std::mem::take(&mut pieces[0].sep);
                pieces.insert(
                    0,
                    Piece {
                        chars: lead,
                        sep: before,
                        glue: false,
                    },
                );
            } else {
                let mut fused = lead;
                fused.extend_from_slice(&pieces[0].chars);
                pieces[0].chars = fused;
            }
        }
        let trail = &chars[end..];
        if !trail.is_empty()
            && let Some(last) = pieces.len().checked_sub(1)
            && trail.len() < pieces[last].chars.len()
        {
            let split_at = pieces[last].chars.len() - trail.len();
            let tail = pieces[last].chars.split_off(split_at);
            let rest_w = drawn_cells(&pieces[last].chars);
            if rest_w + drawn_cells(&tail) > width && rest_w <= width {
                pieces.push(Piece {
                    chars: tail,
                    sep: Vec::new(),
                    glue: false,
                });
            } else {
                pieces[last].chars.extend_from_slice(&tail);
            }
        }
    }
    // A piece of [`absent`] characters alone draws nothing on a marked line:
    // it drops, and its separator joins the one after it.
    let mut k = 0;
    while k < pieces.len() {
        if !glue_aware || !pieces[k].chars.iter().all(|(ch, _)| absent(*ch)) {
            k += 1;
            continue;
        }
        let mut joined = pieces.remove(k).sep;
        let after = pieces
            .get_mut(k)
            .map_or(&mut trailing, |next| &mut next.sep);
        joined.append(after);
        *after = joined;
    }
    // A prose-only piece between two marked runs inside the run bounds is
    // glue, whatever kept space sits on either side of it: it rides the word
    // before it or takes its own row, never carrying the next run's word. Its
    // marks are read off the characters that draw.
    for k in 1..pieces.len().saturating_sub(1) {
        if !pieces[k].glue
            && !pieces[k].chars.iter().any(drawn_mark)
            && pieces[k - 1].chars.iter().any(drawn_mark)
            && pieces[k + 1].chars.iter().any(drawn_mark)
        {
            pieces[k].glue = true;
        }
    }
    for piece in pieces {
        if !piece.glue {
            place_word(&piece.chars, &piece.sep, width, cur, cur_w, lines);
            continue;
        }
        let glue_w = drawn_cells(&piece.chars);
        let sep: &[(char, Kind)] = if cur.is_empty() { &[] } else { &piece.sep };
        let sep_w = drawn_cells(sep);
        if !cur.is_empty() && *cur_w + sep_w + glue_w <= width {
            cur.extend_from_slice(sep);
            *cur_w += sep_w;
            cur.extend_from_slice(&piece.chars);
            *cur_w += glue_w;
            continue;
        }
        if !cur.is_empty() {
            lines.push(std::mem::take(cur));
            *cur_w = 0;
        }
        place_word(&piece.chars, &[], width, cur, cur_w, lines);
        lines.push(std::mem::take(cur));
        *cur_w = 0;
    }
    // A line holding nothing but this whitespace splits it by cell; after
    // anything else the cells past the row end drop, as they would draw a
    // blank row.
    let whitespace_only = cur.is_empty() && lines.is_empty();
    for cell in trailing {
        let cell_w = drawn_cells(&[cell]);
        if *cur_w + cell_w > width {
            if !whitespace_only {
                break;
            }
            if !cur.is_empty() {
                lines.push(std::mem::take(cur));
            }
            *cur_w = 0;
        }
        cur.push(cell);
        *cur_w += cell_w;
    }
}

fn place_word(
    word: &[(char, Kind)],
    sep: &[(char, Kind)],
    width: usize,
    cur: &mut Vec<(char, Kind)>,
    cur_w: &mut usize,
    lines: &mut Vec<Vec<(char, Kind)>>,
) {
    let word_w = drawn_cells(word);
    if word_w > width {
        if !cur.is_empty() {
            lines.push(std::mem::take(cur));
            *cur_w = 0;
        }
        let mut chunks = chunk_word(word, width);
        let last = chunks.pop().unwrap_or_default();
        lines.extend(chunks);
        *cur_w = drawn_cells(&last);
        *cur = last;
        return;
    }
    let sep_w = drawn_cells(sep);
    if cur.is_empty() {
        cur.extend_from_slice(word);
        *cur_w = word_w;
    } else if *cur_w + sep_w + word_w > width {
        lines.push(std::mem::take(cur));
        *cur_w = word_w;
        cur.extend_from_slice(word);
    } else {
        cur.extend_from_slice(sep);
        *cur_w += sep_w;
        cur.extend_from_slice(word);
        *cur_w += word_w;
    }
}

/// One word split into `width`-cell chunks by grapheme, a wide one taking two
/// cells. The chunking is the shared grapheme splitter, so a character the
/// buffer never draws (a control character) is dropped exactly as the buffer
/// drops it and no later chunk shifts; each kept character keeps its kind.
fn chunk_word(word: &[(char, Kind)], width: usize) -> Vec<Vec<(char, Kind)>> {
    let text: String = word.iter().map(|(ch, _)| *ch).collect();
    let mut chunks = Vec::new();
    let mut consumed = 0usize;
    for chunk in panes::chunk_graphemes(&text, width) {
        let mut kept = Vec::with_capacity(chunk.len());
        for _ in chunk.chars() {
            while word.get(consumed).is_some_and(|(ch, _)| ch.is_control()) {
                consumed += 1;
            }
            if let Some(&(ch, kind)) = word.get(consumed) {
                kept.push((ch, kind));
                consumed += 1;
            }
        }
        chunks.push(kept);
    }
    if chunks.is_empty() {
        chunks.push(Vec::new());
    }
    chunks
}

/// Each visible character with its kind. An opener with no closer after it
/// marks nothing and is dropped, so a stray opener at a line's tail styles
/// nothing; marker code points never reach the screen. Every opener numbers its
/// own run, so two runs of one face pressed together stay distinguishable.
fn classify(text: &str) -> Vec<(char, Kind)> {
    let mut out = Vec::with_capacity(text.len());
    let mut kind = PROSE;
    let mut run = 0usize;
    for (i, ch) in text.char_indices() {
        match ch {
            KEY_OPEN | CMD_OPEN => {
                let closed = text[i + ch.len_utf8()..].contains(CLOSE);
                if closed {
                    run += 1;
                    kind = Kind {
                        face: if ch == KEY_OPEN { Face::Key } else { Face::Cmd },
                        run,
                    };
                }
            }
            CLOSE => kind = PROSE,
            _ => out.push((ch, kind)),
        }
    }
    out
}

fn runs(chars: &[(char, Kind)], base: Style) -> Vec<Span<'static>> {
    kind_runs(chars)
        .into_iter()
        .map(|(kind, text)| Span::styled(text, style_for(kind, base)))
        .collect()
}

/// The visible characters grouped into maximal runs of one kind — one opener's
/// run each: the grouping [`runs`] styles and [`marked`] counts graphemes over.
fn kind_runs(chars: &[(char, Kind)]) -> Vec<(Kind, String)> {
    let mut out: Vec<(Kind, String)> = Vec::new();
    for &(ch, kind) in chars {
        match out.last_mut() {
            Some((run_kind, text)) if *run_kind == kind => text.push(ch),
            _ => out.push((kind, ch.to_string())),
        }
    }
    out
}

fn style_for(kind: Kind, base: Style) -> Style {
    match kind.face {
        Face::Prose => base,
        Face::Key => base.fg(theme::accent_color()).add_modifier(Modifier::BOLD),
        Face::Cmd => base
            .fg(theme::accent_color())
            .remove_modifier(Modifier::BOLD),
    }
}

#[cfg(test)]
#[path = "../../../tests/inline/tui_render_prose.rs"]
mod tests;
