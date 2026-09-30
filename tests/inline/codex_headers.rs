use super::*;

/// Env lookup over a fixed table, where `""` means set-but-empty — the
/// distinction the detection table reads.
fn env_of<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
    move |name| {
        pairs
            .iter()
            .find(|(key, _)| *key == name)
            .map(|(_, value)| value.to_string())
    }
}

/// The terminal token: `TERM_PROGRAM` (+ version) raw, the terminal-specific
/// variables in codex's order, then `TERM` raw, then `unknown`.
#[test]
fn the_terminal_token_follows_codexs_detection_order() {
    let term = |pairs: &[(&str, &str)]| terminal_token(&env_of(pairs));

    assert_eq!(
        term(&[
            ("TERM_PROGRAM", "Apple_Terminal"),
            ("TERM_PROGRAM_VERSION", "400"),
        ]),
        "Apple_Terminal/400",
        "TERM_PROGRAM rides raw, with its version when there is one"
    );
    assert_eq!(term(&[("TERM_PROGRAM", "iTerm.app")]), "iTerm.app");
    assert_eq!(
        term(&[("TERM_PROGRAM", "tmux"), ("ITERM_SESSION_ID", "x")]),
        "iTerm.app",
        "TERM_PROGRAM=tmux masks the program and the probes fall through"
    );
    assert_eq!(
        term(&[("WEZTERM_VERSION", "")]),
        "WezTerm",
        "a set-but-empty version names the terminal without one"
    );
    assert_eq!(term(&[("WEZTERM_VERSION", "25.8")]), "WezTerm/25.8");
    assert_eq!(
        term(&[("GHOSTTY_RESOURCES_DIR", "/usr/share/ghostty")]),
        "Ghostty",
        "the ghostty probe wants a non-empty value"
    );
    assert_eq!(
        term(&[("ITERM_SESSION_ID", "")]),
        "iTerm.app",
        "the iterm probes take a set-but-empty variable"
    );
    assert_eq!(term(&[("TERM_SESSION_ID", "x")]), "Apple_Terminal");
    assert_eq!(term(&[("KITTY_WINDOW_ID", "")]), "kitty");
    assert_eq!(term(&[("TERM", "xterm-kitty")]), "kitty");
    assert_eq!(term(&[("ALACRITTY_SOCKET", "")]), "Alacritty");
    assert_eq!(term(&[("KONSOLE_VERSION", "6.1")]), "Konsole/6.1");
    assert_eq!(term(&[("GNOME_TERMINAL_SCREEN", "")]), "gnome-terminal");
    assert_eq!(term(&[("VTE_VERSION", "7800")]), "VTE/7800");
    assert_eq!(term(&[("WT_SESSION", "x")]), "WindowsTerminal");
    assert_eq!(
        term(&[("TERM", "xterm-256color")]),
        "xterm-256color",
        "an unknown terminal sends its TERM value raw"
    );
    assert_eq!(term(&[]), "unknown");
}

/// Control characters and header-invalid bytes in terminal- or version-derived
/// text never reach the wire.
#[test]
fn the_terminal_token_is_sanitized() {
    let token = terminal_token(&env_of(&[("TERM_PROGRAM", "bad value\nhere")]));
    assert_eq!(token, "bad_value_here");
}

/// The UA is exactly codex's shape: `codex_cli_rs/<version> (<os> <os
/// version>; <arch>) <terminal>`; without a detected codex it is the bare
/// originator, codex's own final fallback.
#[test]
fn the_codex_user_agent_is_codexs_shape() {
    assert_eq!(
        codex_user_agent_from(
            Some("0.145.0"),
            "Ubuntu",
            "26.04",
            "x86_64",
            "Apple_Terminal/400"
        ),
        "codex_cli_rs/0.145.0 (Ubuntu 26.04; x86_64) Apple_Terminal/400"
    );
    assert_eq!(
        codex_user_agent_from(None, "Ubuntu", "26.04", "x86_64", "unknown"),
        "codex_cli_rs",
    );
}

/// The version is the first line's last version-looking token.
#[test]
fn the_codex_version_is_the_version_looking_token() {
    assert_eq!(
        codex_version_from("codex-cli 0.145.0"),
        Some("0.145.0".to_string())
    );
    assert_eq!(
        codex_version_from("codex-cli 0.145.0 (2026-09-01)"),
        Some("0.145.0".to_string()),
        "a trailing date is not a version"
    );
    assert_eq!(codex_version_from("weird"), None);
}
