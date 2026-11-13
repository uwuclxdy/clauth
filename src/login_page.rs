//! The browser pages every login flow shows, one self-contained document each.
//!
//! Both callback doors (the claude OAuth loopback and codex's) render an
//! [`Id`]; the stylesheet and the font live in `assets/login/`, are embedded
//! in the binary and are inlined into the page, so the browser needs no
//! follow-up request after the CLI exits.

use std::io::Write;
use std::net::TcpStream;
use std::sync::LazyLock;

use base64::Engine as _;

/// The callback page's stylesheet, inlined into every page it renders.
const LOGIN_CSS: &str = include_str!("../assets/login/login.css");
/// The Onest latin subset the stylesheet's `@font-face` names, inlined as a
/// base64 data URI so the page needs no font request of its own. Its name
/// ID 13 carries the OFL text; `assets/login/fonts/set-license-name.py`
/// writes it after a subset swap.
const LOGIN_FONT: &[u8] = include_bytes!("../assets/login/fonts/onest-latin.woff2");

/// The page one callback outcome shows. Every byte of the copy is fixed per
/// variant: query values are untrusted browser input and never reach the html.
#[derive(Clone, Copy)]
pub(crate) enum Id {
    ClaudeSuccess,
    ClaudeDeclined,
    ClaudeFailed,
    ClaudeNoCode,
    ClaudeBlocked,
    ClaudeBadRequest,
    ClaudeNotFound,
    CodexWaiting,
    CodexStateMismatch,
    CodexSuccess,
    CodexNoCode,
    CodexDeclined,
    CodexUpstream,
    CodexRefused,
}

impl Id {
    #[cfg(test)]
    pub(crate) const ALL: [Id; 14] = [
        Self::ClaudeSuccess,
        Self::ClaudeDeclined,
        Self::ClaudeFailed,
        Self::ClaudeNoCode,
        Self::ClaudeBlocked,
        Self::ClaudeBadRequest,
        Self::ClaudeNotFound,
        Self::CodexWaiting,
        Self::CodexStateMismatch,
        Self::CodexSuccess,
        Self::CodexNoCode,
        Self::CodexDeclined,
        Self::CodexUpstream,
        Self::CodexRefused,
    ];

    fn status(self) -> &'static str {
        match self {
            Self::ClaudeSuccess | Self::CodexSuccess => "200 OK",
            Self::ClaudeNotFound | Self::CodexWaiting => "404 Not Found",
            Self::ClaudeDeclined
            | Self::ClaudeFailed
            | Self::ClaudeNoCode
            | Self::ClaudeBlocked
            | Self::ClaudeBadRequest
            | Self::CodexStateMismatch
            | Self::CodexNoCode
            | Self::CodexDeclined
            | Self::CodexUpstream
            | Self::CodexRefused => "400 Bad Request",
        }
    }

    fn title(self) -> &'static str {
        match self {
            Self::ClaudeSuccess => "You're logged in",
            Self::ClaudeDeclined => "Login canceled",
            Self::ClaudeFailed => "Login failed",
            Self::ClaudeNoCode => "No code in the callback",
            Self::ClaudeBlocked => "Login blocked",
            Self::ClaudeBadRequest => "That request didn't parse",
            Self::ClaudeNotFound => "Nothing at this address",
            Self::CodexWaiting => "Waiting for the codex login",
            Self::CodexStateMismatch => "Login state mismatch",
            Self::CodexSuccess => "Codex login complete",
            Self::CodexNoCode | Self::CodexUpstream | Self::CodexRefused => "Codex login failed",
            Self::CodexDeclined => "Codex login canceled",
        }
    }

    fn text(self) -> &'static str {
        match self {
            Self::ClaudeSuccess => "clauth captured the login. You can close this tab now.",
            Self::ClaudeDeclined => {
                "Authorization request declined, no login was captured. You can close this tab now."
            }
            Self::ClaudeFailed => {
                "An error occurred during authorization. Close this tab and retry from clauth."
            }
            Self::ClaudeNoCode => {
                "The redirect arrived without an authorization code and clauth is still waiting for the real callback. You can close this tab now."
            }
            Self::ClaudeBlocked => {
                "This callback didn't match the login clauth started, so it was rejected for safety. Close this tab and retry from clauth."
            }
            Self::ClaudeBadRequest => {
                "clauth expected an OAuth callback here and is still waiting for it. You can close this tab now."
            }
            Self::ClaudeNotFound => {
                "The login callback arrives at /callback on its own. You can close this tab now."
            }
            Self::CodexWaiting => "The callback has not arrived yet. You can close this tab now.",
            Self::CodexStateMismatch => "Close this tab and retry from clauth.",
            Self::CodexSuccess => "You can close this tab now.",
            Self::CodexNoCode => {
                "The callback arrived without a code. Close this tab and retry from clauth."
            }
            Self::CodexDeclined => "Authorization request declined. You can close this tab now.",
            Self::CodexUpstream => {
                "OpenAI is having trouble. Close this tab and retry from clauth."
            }
            Self::CodexRefused => "OpenAI refused the login. Close this tab and retry from clauth.",
        }
    }

    fn auto_close(self) -> bool {
        matches!(self, Self::ClaudeSuccess)
    }

    #[cfg(test)]
    pub(crate) fn slug(self) -> &'static str {
        match self {
            Self::ClaudeSuccess => "claude-success",
            Self::ClaudeDeclined => "claude-declined",
            Self::ClaudeFailed => "claude-failed",
            Self::ClaudeNoCode => "claude-no-code",
            Self::ClaudeBlocked => "claude-blocked",
            Self::ClaudeBadRequest => "claude-bad-request",
            Self::ClaudeNotFound => "claude-not-found",
            Self::CodexWaiting => "codex-waiting",
            Self::CodexStateMismatch => "codex-state-mismatch",
            Self::CodexSuccess => "codex-success",
            Self::CodexNoCode => "codex-no-code",
            Self::CodexDeclined => "codex-declined",
            Self::CodexUpstream => "codex-upstream",
            Self::CodexRefused => "codex-refused",
        }
    }
}

/// Write the full html page for `id`. Write errors are the peer's to see: a
/// browser that left mid-answer costs the login nothing.
pub(crate) fn write_page(stream: &mut TcpStream, id: Id) {
    let page = html(id);
    let head = format!(
        "HTTP/1.1 {}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\n\
         Connection: close\r\n\r\n",
        id.status(),
        page.len()
    );
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(page.as_bytes());
    let _ = stream.flush();
}

/// The page stylesheet with its font embedded: the `@font-face` url is a
/// base64 data URI of the committed woff2 subset, so the page itself needs no
/// asset request. Encoded once, since every page shares the bytes.
fn page_css() -> &'static str {
    static CSS: LazyLock<String> = LazyLock::new(|| {
        LOGIN_CSS.replacen(
            "fonts/onest-latin.woff2",
            &format!(
                "data:font/woff2;base64,{}",
                base64::engine::general_purpose::STANDARD.encode(LOGIN_FONT)
            ),
            1,
        )
    });
    CSS.as_str()
}

/// The page markup. The stylesheet and its font are embedded in the binary and
/// inlined into the page, so the browser has nothing to fetch after the CLI
/// process exits; the theme follows the OS preference in a CSS media query.
fn html(id: Id) -> String {
    let close = if id.auto_close() {
        "<script>window.setTimeout(function(){window.close()},900)</script>\n"
    } else {
        ""
    };
    format!(
        concat!(
            "<!DOCTYPE html>\n",
            "<html lang=\"en\">\n",
            "<head>\n",
            "<meta charset=\"UTF-8\">\n",
            "<meta name=\"viewport\" content=\"width=device-width, initial-scale=1.0\">\n",
            "<link rel=\"icon\" href=\"data:image/svg+xml,%3Csvg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 16 16'%3E%3Ccircle cx='8' cy='8' r='7' fill='%2368BBEF'/%3E%3C/svg%3E\">\n",
            "<title>clauth</title>\n",
            "<style>\n{css}\n</style>\n",
            "</head>\n",
            "<body>\n",
            "<main class=\"page-state\">\n",
            "<div class=\"page-state-gap\">\n",
            "<div class=\"page-state-body\">\n",
            "<h1 class=\"page-state-title\">{title}</h1>\n",
            "<p class=\"page-state-text\">{text}</p>\n",
            "</div>\n",
            "</div>\n",
            "</main>\n",
            "{close}",
            "</body>\n",
            "</html>\n",
        ),
        title = id.title(),
        text = id.text(),
        css = page_css(),
        close = close,
    )
}

#[cfg(test)]
#[path = "../tests/inline/login_page.rs"]
mod tests;
