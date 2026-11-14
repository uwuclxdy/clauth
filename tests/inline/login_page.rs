#![allow(clippy::unwrap_used, clippy::expect_used)]
//! The browser pages every login flow shows: one renderer for both callback
//! doors, each page a self-contained document, and the preview tree a local
//! server hands to the eye.

use std::io::Read;
use std::net::{TcpListener, TcpStream};

use base64::Engine as _;
use sha2::{Digest as _, Sha256};

use super::{Id, html, write_page};

/// One page over a real loopback socket pair: `write_page` writes to the
/// accepted end, the "browser" reads the whole response.
fn page_response(id: Id) -> String {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind loopback");
    let addr = listener.local_addr().expect("local addr");
    let mut client = TcpStream::connect(addr).expect("connect");
    let (mut server, _) = listener.accept().expect("accept");
    write_page(&mut server, id);
    drop(server);
    let mut response = String::new();
    client.read_to_string(&mut response).expect("read response");
    response
}

fn body(response: &str) -> &str {
    response.split_once("\r\n\r\n").expect("header block").1
}

#[test]
fn every_page_states_its_status_title_text_and_close_policy() {
    for id in Id::ALL {
        let response = page_response(id);
        let (head, page) = response.split_once("\r\n\r\n").expect("header block");
        assert!(
            head.starts_with(&format!("HTTP/1.1 {}", id.status())),
            "{}: {head}",
            id.slug()
        );
        assert!(
            head.contains("Content-Type: text/html; charset=utf-8"),
            "{}: {head}",
            id.slug()
        );
        assert!(
            head.contains(&format!("Content-Length: {}\r\n", page.len())),
            "{}: {head}",
            id.slug()
        );
        assert!(page.contains(id.title()), "{}: title", id.slug());
        assert!(page.contains(id.text()), "{}: text", id.slug());
        assert_eq!(
            page.contains("window.close"),
            id.auto_close(),
            "{}: auto-close",
            id.slug()
        );
    }
}

/// The page copy and status rows, pinned exactly and independently of the
/// accessors the renderer interpolates: a wrong string or status for an id
/// reds here even when the page faithfully shows it.
#[test]
fn the_page_copy_and_statuses_are_pinned() {
    let pinned: [(Id, &str, &str, &str); 14] = [
        (
            Id::ClaudeSuccess,
            "200 OK",
            "You're logged in",
            "clauth captured the login. You can close this tab now.",
        ),
        (
            Id::ClaudeDeclined,
            "400 Bad Request",
            "Login canceled",
            "Authorization request declined, no login was captured. You can close this tab now.",
        ),
        (
            Id::ClaudeFailed,
            "400 Bad Request",
            "Login failed",
            "An error occurred during authorization. Close this tab and retry from clauth.",
        ),
        (
            Id::ClaudeNoCode,
            "400 Bad Request",
            "No code in the callback",
            "The redirect arrived without an authorization code and clauth is still waiting for the real callback. You can close this tab now.",
        ),
        (
            Id::ClaudeBlocked,
            "400 Bad Request",
            "Login blocked",
            "This callback didn't match the login clauth started, so it was rejected for safety. Close this tab and retry from clauth.",
        ),
        (
            Id::ClaudeBadRequest,
            "400 Bad Request",
            "That request didn't parse",
            "clauth expected an OAuth callback here and is still waiting for it. You can close this tab now.",
        ),
        (
            Id::ClaudeNotFound,
            "404 Not Found",
            "Nothing at this address",
            "The login callback arrives at /callback on its own. You can close this tab now.",
        ),
        (
            Id::CodexWaiting,
            "404 Not Found",
            "Waiting for the codex login",
            "The callback has not arrived yet. You can close this tab now.",
        ),
        (
            Id::CodexStateMismatch,
            "400 Bad Request",
            "Login state mismatch",
            "Close this tab and retry from clauth.",
        ),
        (
            Id::CodexSuccess,
            "200 OK",
            "Codex login complete",
            "You can close this tab now.",
        ),
        (
            Id::CodexNoCode,
            "400 Bad Request",
            "Codex login failed",
            "The callback arrived without a code. Close this tab and retry from clauth.",
        ),
        (
            Id::CodexDeclined,
            "400 Bad Request",
            "Codex login canceled",
            "Authorization request declined. You can close this tab now.",
        ),
        (
            Id::CodexUpstream,
            "400 Bad Request",
            "Codex login failed",
            "OpenAI is having trouble. Close this tab and retry from clauth.",
        ),
        (
            Id::CodexRefused,
            "400 Bad Request",
            "Codex login failed",
            "OpenAI refused the login. Close this tab and retry from clauth.",
        ),
    ];
    for (id, status, title, text) in pinned {
        assert_eq!(id.status(), status, "{}: status", id.slug());
        assert_eq!(id.title(), title, "{}: title", id.slug());
        assert_eq!(id.text(), text, "{}: text", id.slug());
    }
}

/// The page every door writes is one self-contained document: its stylesheet,
/// the font inside it, the favicon and the one script are all in the html, so
/// the browser has nothing to fetch after the CLI process exits. Any `src`,
/// `href`, `url(` or `@import` reference reds here; `srcset`, `object data=`,
/// `meta http-equiv=refresh` and CSS `image-set()` are not scanned.
#[test]
fn the_page_state_markup_is_one_self_contained_page() {
    let response = page_response(Id::ClaudeSuccess);
    let page = body(&response);
    for part in [
        "<main class=\"page-state\">",
        "class=\"page-state-gap\"",
        "class=\"page-state-body\"",
        "class=\"page-state-title\"",
        "class=\"page-state-text\"",
        "<style>",
        "@font-face",
        "@media (prefers-color-scheme: light)",
        "data:font/woff2;base64,",
        "rel=\"icon\"",
    ] {
        assert!(page.contains(part), "missing {part}");
    }
    assert_eq!(page.matches("<style>").count(), 1, "one inline stylesheet");
    assert!(page.contains("</style>"), "the inline stylesheet closes");

    // Nothing to fetch: no file-backed script, sheet, image or asset path.
    assert!(!page.contains(" src="), "the page loads no script file");
    assert!(
        !page.contains("<link rel=\"stylesheet\""),
        "no external stylesheet"
    );
    assert!(!page.contains("/assets/"), "no asset path");
    assert!(!page.contains("login.css"), "the stylesheet is inlined");
    assert!(
        !page.contains("fonts/onest-latin.woff2"),
        "the font is inlined"
    );
    let mut rest = page;
    while let Some(at) = rest.find("href=\"") {
        let after = &rest[at + "href=\"".len()..];
        assert!(
            after.starts_with("data:"),
            "an href is not a data URI: {after:.48}"
        );
        rest = after;
    }
    // The one CSS reference is the font data URI, and nothing imports another
    // sheet.
    assert_eq!(
        page.matches("url(").count(),
        1,
        "one CSS reference, the font"
    );
    assert!(!page.contains("@import"), "no CSS imports");
    assert_eq!(
        page.matches("data:font/woff2;base64,").count(),
        1,
        "one inline font"
    );

    // The theme follows the OS preference in CSS; no script or query value
    // decides it.
    assert!(!page.contains("URLSearchParams"), "no theme script");
    assert!(!page.contains("data-theme"), "the theme is a media query");

    // The inline font is the committed subset, byte for byte.
    let marker = "data:font/woff2;base64,";
    let at = page.find(marker).expect("the inline font");
    let encoded = &page[at + marker.len()..];
    let encoded = &encoded[..encoded.find(['"', '\'']).expect("closing quote")];
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .expect("the data URI is base64");
    assert_eq!(
        decoded,
        include_bytes!("../../assets/login/fonts/onest-latin.woff2"),
        "the inline font is the committed subset"
    );
    assert_eq!(
        hex::encode(Sha256::digest(&decoded)),
        "0ee33f7e7c0e8912971309d0cb0f3e73cb332731e8286a896d416eb664cb52c2",
        "a new subset needs set-license-name.py, then this hash"
    );
}

/// Writes every page to `CLAUTH_PREVIEW_DIR` so a static server can show
/// them. Ignored: it is a preview generator, not a test.
#[test]
#[ignore = "writes the preview tree; set CLAUTH_PREVIEW_DIR"]
fn write_preview_tree() {
    let root = std::path::PathBuf::from(
        std::env::var_os("CLAUTH_PREVIEW_DIR").expect("set CLAUTH_PREVIEW_DIR"),
    );
    let pages = root.join("pages");
    std::fs::create_dir_all(&pages).expect("pages dir");
    for id in Id::ALL {
        std::fs::write(pages.join(format!("{}.html", id.slug())), html(id)).expect("page");
    }
    std::fs::write(root.join("index.html"), preview_index()).expect("index");
    println!("preview tree at {}", root.display());
}

fn preview_index() -> String {
    let rows: String = Id::ALL
        .iter()
        .map(|id| {
            format!(
                "<li><a href=\"pages/{slug}.html\">{slug}</a></li>",
                slug = id.slug()
            )
        })
        .collect();
    format!(
        "<!DOCTYPE html>\n<html lang=\"en\">\n<head>\n\
         <meta charset=\"UTF-8\">\n\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1.0\">\n\
         <title>clauth login pages</title>\n\
         </head>\n<body>\n\
         <ul>{rows}</ul>\n\
         </body>\n</html>\n"
    )
}
