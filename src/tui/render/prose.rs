//! Keys and typed commands inside TUI prose (cloudy-tui "Keys and commands
//! inside prose"): copy marks them with [`key`] / [`cmd`], every render site
//! turns the marked string into spans through [`spans`] or [`wrap`].
//!
//! The markers are private-use code points, so no literal copy can hold one:
//! a bracket marker would collide with copy that prints `[server.admin]` or a
//! `[ 3 live ]` chip.

use ratatui::style::{Modifier, Style};
use ratatui::text::Span;

use super::super::theme;
use super::panes::wrap_words;

const KEY_OPEN: char = '\u{E000}';
const CMD_OPEN: char = '\u{E001}';
const CLOSE: char = '\u{E002}';

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

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Prose,
    Key,
    Cmd,
}

/// A key named in running copy (`press {key("d")} to resolve`).
pub(crate) fn key(name: &str) -> String {
    format!("{KEY_OPEN}{name}{CLOSE}")
}

/// A command the end user types (`run {cmd("clauth login")}`).
pub(crate) fn cmd(command: &str) -> String {
    format!("{CMD_OPEN}{command}{CLOSE}")
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
/// `foo /usr/bin/clauth x` is no command. Both separators are split by hand so
/// a Windows path reads the same on every host.
fn is_command(span: &str) -> bool {
    !span.contains('\n')
        && span
            .match_indices(' ')
            .take_while(|(at, _)| *at <= BINARY_PATH_MAX)
            .any(|(at, _)| {
                let candidate = &span[..at];
                let shaped = if candidate.contains(['/', '\\']) {
                    candidate.starts_with(['/', '\\', '~', '.'])
                        || candidate.as_bytes().get(1) == Some(&b':')
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

/// [`spans`] cut to `width` visible characters the way
/// [`crate::format::truncate`] cuts plain copy: a trailing `…` in `base` when
/// anything was dropped.
pub(crate) fn fit(text: &str, width: usize, base: Style) -> Vec<Span<'static>> {
    let mut chars = classify(text);
    if chars.len() > width {
        chars.truncate(width.saturating_sub(1));
        if width > 0 {
            chars.push(('…', Kind::Prose));
        }
    }
    runs(&chars, base)
}

/// [`wrap_words`] over marked copy: the wrap budget counts only visible
/// characters, and each wrapped line keeps the styling its characters had.
pub(crate) fn wrap(text: &str, width: usize, base: Style) -> Vec<Vec<Span<'static>>> {
    let chars = classify(text);
    let visible: String = chars.iter().map(|(ch, _)| *ch).collect();
    let mut source = chars.iter().peekable();
    wrap_words(&visible, width)
        .into_iter()
        .map(|line| {
            let mut styled = Vec::with_capacity(line.len());
            for ch in line.chars() {
                if ch == ' ' {
                    // `wrap_words` joins words with one space; take the kind
                    // of the whitespace run it stands for.
                    let mut kind = Kind::Prose;
                    while let Some(&&(c, k)) = source.peek() {
                        if !c.is_whitespace() {
                            break;
                        }
                        kind = k;
                        source.next();
                    }
                    styled.push((' ', kind));
                    continue;
                }
                // A line break swallows the whitespace run before the line's
                // first word; skip to the character this one copies.
                let kind = loop {
                    match source.next() {
                        Some(&(c, k)) if c == ch => break k,
                        Some(_) => {}
                        None => break Kind::Prose,
                    }
                };
                styled.push((ch, kind));
            }
            runs(&styled, base)
        })
        .collect()
}

/// Each visible character with its kind. An opener with no closer after it
/// marks nothing and is dropped, so a stray opener at a line's tail styles
/// nothing; marker code points never reach the screen.
fn classify(text: &str) -> Vec<(char, Kind)> {
    let mut out = Vec::with_capacity(text.len());
    let mut kind = Kind::Prose;
    for (i, ch) in text.char_indices() {
        match ch {
            KEY_OPEN | CMD_OPEN => {
                let closed = text[i + ch.len_utf8()..].contains(CLOSE);
                if closed {
                    kind = if ch == KEY_OPEN { Kind::Key } else { Kind::Cmd };
                }
            }
            CLOSE => kind = Kind::Prose,
            _ => out.push((ch, kind)),
        }
    }
    out
}

fn runs(chars: &[(char, Kind)], base: Style) -> Vec<Span<'static>> {
    let mut out = Vec::new();
    let mut run = String::new();
    let mut run_kind = Kind::Prose;
    for &(ch, kind) in chars {
        if kind != run_kind && !run.is_empty() {
            out.push(Span::styled(
                std::mem::take(&mut run),
                style_for(run_kind, base),
            ));
        }
        run_kind = kind;
        run.push(ch);
    }
    if !run.is_empty() {
        out.push(Span::styled(run, style_for(run_kind, base)));
    }
    out
}

fn style_for(kind: Kind, base: Style) -> Style {
    match kind {
        Kind::Prose => base,
        Kind::Key => base.fg(theme::accent_color()).add_modifier(Modifier::BOLD),
        Kind::Cmd => base
            .fg(theme::accent_color())
            .remove_modifier(Modifier::BOLD),
    }
}

#[cfg(test)]
#[path = "../../../tests/inline/tui_render_prose.rs"]
mod tests;
