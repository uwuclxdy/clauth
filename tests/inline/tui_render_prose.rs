#![allow(clippy::unwrap_used, clippy::expect_used)]

use ratatui::style::{Modifier, Style};
use ratatui::text::Span;

use super::super::super::theme;
use super::{cmd, fit, key, plain, spans, wrap};

fn dim() -> Style {
    theme::dim()
}

fn key_style(base: Style) -> Style {
    base.fg(theme::accent_color()).add_modifier(Modifier::BOLD)
}

fn cmd_style(base: Style) -> Style {
    base.fg(theme::accent_color())
        .remove_modifier(Modifier::BOLD)
}

#[test]
fn a_key_renders_accent_bold_between_prose_at_its_own_tier() {
    let text = format!("press {} to resolve", key("d"));
    assert_eq!(
        spans(&text, dim()),
        vec![
            Span::styled("press ", dim()),
            Span::styled("d", key_style(dim())),
            Span::styled(" to resolve", dim()),
        ]
    );
}

#[test]
fn a_command_renders_accent_and_drops_a_bold_base() {
    let title = Style::default().fg(theme::text_color()).bold();
    let text = format!("run {} first", cmd("clauth daemon"));
    let out = spans(&text, title);
    assert_eq!(
        out,
        vec![
            Span::styled("run ", title),
            Span::styled("clauth daemon", cmd_style(title)),
            Span::styled(" first", title),
        ]
    );
    assert!(!out[1].style.add_modifier.contains(Modifier::BOLD));
    assert!(out[1].style.sub_modifier.contains(Modifier::BOLD));
}

#[test]
fn a_marked_span_keeps_the_base_background() {
    let base = Style::default()
        .fg(theme::warning_color())
        .bg(theme::bg_warning_color());
    let out = spans(&key("d"), base);
    assert_eq!(out, vec![Span::styled("d", key_style(base))]);
    assert_eq!(out[0].style.bg, Some(theme::bg_warning_color()));
}

#[test]
fn plain_drops_every_marker() {
    let text = format!(
        "{} or {} then {}",
        key("↵"),
        key("esc"),
        cmd("herdr config check")
    );
    assert_eq!(plain(&text), "↵ or esc then herdr config check");
    assert_eq!(plain("no markers here"), "no markers here");
}

#[test]
fn unmarked_copy_is_one_prose_span() {
    assert_eq!(
        spans("[server.admin] stays literal", dim()),
        vec![Span::styled("[server.admin] stays literal", dim())]
    );
    assert!(spans("", dim()).is_empty());
}

#[test]
fn an_opener_with_no_closer_marks_nothing() {
    let text = "\u{E000}d to resolve";
    assert_eq!(
        spans(text, dim()),
        vec![Span::styled("d to resolve", dim())]
    );
    assert_eq!(plain(text), "d to resolve");
    let cmd_open = "run \u{E001}clauth login";
    assert_eq!(
        spans(cmd_open, dim()),
        vec![Span::styled("run clauth login", dim())]
    );
}

#[test]
fn wrap_counts_visible_width_only_and_keeps_styles_across_lines() {
    // Visible: "re-login with clauth login work" (31 chars). At width 20 the
    // command splits "clauth" onto line 1 and "login work" onto line 2; the
    // markers' six code points never count toward the budget.
    let text = format!("re-login with {}", cmd("clauth login work"));
    let lines = wrap(&text, 20, dim());
    assert_eq!(
        lines,
        vec![
            vec![
                Span::styled("re-login with ", dim()),
                Span::styled("clauth", cmd_style(dim())),
            ],
            vec![Span::styled("login work", cmd_style(dim()))],
        ]
    );
}

#[test]
fn wrap_fits_a_line_its_markers_would_push_over_the_budget() {
    // Visible "press d to resolve" is 18 chars; marked it is 20 code points.
    let text = format!("press {} to resolve", key("d"));
    let lines = wrap(&text, 18, dim());
    assert_eq!(lines.len(), 1);
    assert_eq!(
        lines[0],
        vec![
            Span::styled("press ", dim()),
            Span::styled("d", key_style(dim())),
            Span::styled(" to resolve", dim()),
        ]
    );
}

#[test]
fn wrap_collapses_whitespace_runs_like_wrap_words() {
    let text = format!("a   {}   b", key("q"));
    assert_eq!(
        wrap(&text, 40, dim()),
        vec![vec![
            Span::styled("a ", dim()),
            Span::styled("q", key_style(dim())),
            Span::styled(" b", dim()),
        ]]
    );
}

#[test]
fn fit_cuts_visible_characters_and_keeps_a_line_that_fits_whole() {
    // "  clauth herdr install" is 22 visible chars, 25 marked.
    let text = format!("  {}", cmd("clauth herdr install"));
    assert_eq!(
        fit(&text, 22, dim()),
        vec![
            Span::styled("  ", dim()),
            Span::styled("clauth herdr install", cmd_style(dim())),
        ]
    );
    assert_eq!(
        fit(&text, 10, dim()),
        vec![
            Span::styled("  ", dim()),
            Span::styled("clauth ", cmd_style(dim())),
            Span::styled("…", dim()),
        ]
    );
    assert!(fit(&text, 0, dim()).is_empty());
}

#[test]
fn the_literal_macros_mark_exactly_as_the_functions_do() {
    assert_eq!(super::key_lit!("\u{21b5}"), key("\u{21b5}"));
    assert_eq!(
        super::cmd_lit!("herdr config check"),
        cmd("herdr config check")
    );
}

#[test]
fn wrap_of_empty_text_is_one_empty_line() {
    assert_eq!(wrap("", 10, dim()), vec![Vec::<Span<'static>>::new()]);
}

#[test]
fn mark_commands_marks_a_backticked_command_and_leaves_other_spans_literal() {
    use super::mark_commands;
    assert_eq!(
        mark_commands("revoked: run `clauth login work`"),
        format!("revoked: run {}", cmd("clauth login work"))
    );
    assert_eq!(
        mark_commands("add `[server.admin]` (error `internal`)"),
        "add `[server.admin]` (error `internal`)"
    );
    assert_eq!(
        mark_commands("`herdr config check` and `codex login` then `x`"),
        format!(
            "{} and {} then `x`",
            cmd("herdr config check"),
            cmd("codex login")
        )
    );
    // A command led by the binary's full path is still a command.
    assert_eq!(
        mark_commands("`/opt/bin/shunt check --config /etc/s.toml` refused"),
        format!(
            "{} refused",
            cmd("/opt/bin/shunt check --config /etc/s.toml")
        )
    );
    assert_eq!(
        mark_commands("`/usr/bin/herdr.exe config check`"),
        cmd("/usr/bin/herdr.exe config check")
    );
    // A binary path holding a space, in either separator.
    assert_eq!(
        mark_commands("`/Users/A B/bin/shunt check --config /x.toml` refused"),
        format!(
            "{} refused",
            cmd("/Users/A B/bin/shunt check --config /x.toml")
        )
    );
    assert_eq!(
        mark_commands("`C:\\Program Files\\shunt\\shunt.exe check` refused"),
        format!(
            "{} refused",
            cmd("C:\\Program Files\\shunt\\shunt.exe check")
        )
    );
    // A path after another word is an argument, not the binary.
    assert_eq!(
        mark_commands("`foo /usr/bin/clauth x` y"),
        "`foo /usr/bin/clauth x` y"
    );
    // A binary path past the 4096-byte cap is no binary.
    let long = format!("`/{}clauth login x`", "d/".repeat(2100));
    assert_eq!(mark_commands(&long), long);
    // A trailing space is no second word.
    assert_eq!(mark_commands("`clauth ` x"), "`clauth ` x");
    // Two words that do not open with a CLI are not a command.
    assert_eq!(
        mark_commands("call `GET /api/v1/panes` first"),
        "call `GET /api/v1/panes` first"
    );
    // A bare tool name is a mention, not a command to type.
    assert_eq!(
        mark_commands("install `clauth` first"),
        "install `clauth` first"
    );
    // An unclosed backtick and a span crossing a line stay as written.
    assert_eq!(mark_commands("run `clauth login"), "run `clauth login");
    assert_eq!(
        mark_commands("`clauth login\nwork` here"),
        "`clauth login\nwork` here"
    );
    assert_eq!(mark_commands(""), "");
}
