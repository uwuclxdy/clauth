/// Enable VT processing on Windows for ANSI escapes in raw mode. No-op on
/// macOS/Linux.
pub(crate) fn init() {
    #[cfg(windows)]
    {
        use ratatui::crossterm::execute;
        use std::io::stdout;
        // A no-op command triggers crossterm's ANSI-enable path.
        let _ = execute!(stdout(), ratatui::crossterm::style::ResetColor);
    }
}

/// Open `url` in the operator's default browser. Used by the interactive OAuth
/// login (`oauth_login`) to launch the authorize page. Detached (stdio nulled)
/// so it never blocks or leaks output into clauth's own stdout/stderr.
pub(crate) fn open_url(url: &str) -> anyhow::Result<()> {
    use anyhow::Context;
    use std::process::{Command, Stdio};

    #[cfg(target_os = "macos")]
    let mut cmd = Command::new("open");
    #[cfg(all(unix, not(target_os = "macos")))]
    let mut cmd = Command::new("xdg-open");
    #[cfg(windows)]
    let mut cmd = {
        // Not `cmd /C start`: cmd.exe re-tokenizes its command line, so every
        // bare `&` in the query splits the URL into separate commands (std
        // quotes an arg only on space/tab/quote — verified on a real Windows
        // box), and the `%xx` percent-encodes risk variable expansion even
        // inside quotes. rundll32 ShellExecutes the URL with no shell
        // tokenizer in between.
        let mut c = Command::new("rundll32");
        c.arg("url.dll,FileProtocolHandler");
        c
    };

    cmd.arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("failed to open browser for {url}"))?;
    Ok(())
}

/// Put `text` on the LOCAL terminal's clipboard through OSC 52, the escape
/// iTerm2, Terminal.app, kitty, WezTerm, Windows Terminal, and tmux (with
/// `set-clipboard on`) honor across an ssh hop. Used by the TUI's manual login
/// to hand over an authorize link too long to select cleanly once wrapped.
/// `Ok` means the bytes were written and flushed; whether the terminal acted
/// on them is not observable from here, so the caller's copy must not promise
/// more than "sent".
pub(crate) fn copy_to_clipboard_osc52(text: &str) -> std::io::Result<()> {
    use std::io::Write as _;
    let mut out = std::io::stdout().lock();
    write_osc52(&mut out, text)?;
    out.flush()
}

/// The OSC 52 sequence itself, on any writer so a test can read it back:
/// `ESC ] 52 ; c ; <base64 of text> BEL`. `c` selects the clipboard (as
/// opposed to a primary selection); standard base64 WITH padding, which is the
/// alphabet the escape specifies (not the PKCE base64url).
pub(crate) fn write_osc52(w: &mut impl std::io::Write, text: &str) -> std::io::Result<()> {
    write!(w, "\x1b]52;c;{}\x07", base64_std(text.as_bytes()))
}

/// RFC 4648 §4 base64, padded. Ten lines beat a direct dependency for one
/// call site (`base64` is in the lockfile only transitively).
fn base64_std(input: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0];
        let b1 = chunk.get(1).copied().unwrap_or(0);
        let b2 = chunk.get(2).copied().unwrap_or(0);
        out.push(ALPHABET[(b0 >> 2) as usize] as char);
        out.push(ALPHABET[(((b0 & 0b11) << 4) | (b1 >> 4)) as usize] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(((b1 & 0b1111) << 2) | (b2 >> 6)) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[(b2 & 0b11_1111) as usize] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
#[path = "../tests/inline/platform.rs"]
mod tests;
