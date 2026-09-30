//! The headers codex's own CLI sends on its WHAM endpoints, applied to every
//! clauth call against `chatgpt.com/backend-api`: the usage poll
//! ([`super::codex`]) and the reset spend ([`super::codex_reset`]).
//!
//! Wire parity is with a logged-in codex CLI's backend client, verified
//! against openai/codex `4b664e0e`: the auth block from
//! `backend-client/src/client.rs` (`headers`) plus
//! `model-provider/src/auth.rs` (`add_auth_headers`), the User-Agent from
//! `login/src/auth/default_client.rs` (`get_codex_user_agent`) and
//! `terminal-detection/src/lib.rs` (`user_agent_token`, ported below). That
//! client sends no `Accept` and no `originator` header on this path — the
//! originator rides inside the UA string alone — so clauth sends neither.
//! `X-OpenAI-Fedramp` rides only when the stored tokens carry
//! `chatgpt_account_is_fedramp`, the same field codex reads it from.

use std::process::Stdio;
use std::sync::LazyLock;
use std::time::Duration;

/// codex's UA originator: the process identity its client was built as
/// (`DEFAULT_ORIGINATOR`, `login/src/auth/default_client.rs`).
const CODEX_ORIGINATOR: &str = "codex_cli_rs";

/// The codex UA is process-constant (OS, terminal and codex's version do not
/// change under a running clauth), so it is resolved once.
static CODEX_USER_AGENT: LazyLock<String> = LazyLock::new(codex_user_agent_now);

/// The agent every WHAM call rides. Same shape as the shared usage agent; the
/// one delta is `accept("")` — ureq adds `accept: */*` to any request that
/// carries no accept of its own, and codex's own client (reqwest with no
/// content negotiation) sends no `accept` at all. `accept-encoding` keeps
/// ureq's gzip, which these surfaces have always requested.
static CODEX_AGENT: LazyLock<ureq::Agent> = LazyLock::new(|| {
    ureq::Agent::config_builder()
        .timeout_connect(Some(Duration::from_secs(4)))
        .timeout_recv_response(Some(Duration::from_secs(8)))
        .http_status_as_error(false)
        .accept("")
        .build()
        .into()
});

/// The codex WHAM agent. Status codes arrive on the `Ok` response — see the
/// builder comment.
pub(crate) fn codex_agent() -> &'static ureq::Agent {
    &CODEX_AGENT
}

/// The codex UA this process sends, resolved once.
pub(crate) fn codex_user_agent() -> &'static str {
    &CODEX_USER_AGENT
}

/// `codex_cli_rs/<version> (<os> <os version>; <arch>) <terminal>` — the shape
/// codex's own client mints. Without a detected codex the version is
/// unknowable, and the UA falls back to the bare originator — codex's own
/// final fallback (`sanitize_user_agent`) — rather than inventing a version
/// no codex shipped.
fn codex_user_agent_from(
    version: Option<&str>,
    os_type: &str,
    os_version: &str,
    arch: &str,
    terminal: &str,
) -> String {
    let Some(version) = version else {
        return CODEX_ORIGINATOR.to_string();
    };
    format!("{CODEX_ORIGINATOR}/{version} ({os_type} {os_version}; {arch}) {terminal}")
}

fn codex_user_agent_now() -> String {
    let os = os_info::get();
    codex_user_agent_from(
        codex_version().as_deref(),
        &os.os_type().to_string(),
        &os.version().to_string(),
        os.architecture().unwrap_or("unknown"),
        &terminal_token(&|name| std::env::var(name).ok()),
    )
}

/// The installed codex's version, parsed off `codex --version`'s first line:
/// its last whitespace token that reads as a version (`codex-cli 0.145.0`).
fn codex_version() -> Option<String> {
    let output = crate::runtime::codex_command()
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let line = String::from_utf8_lossy(&output.stdout);
    codex_version_from(line.lines().next()?)
}

fn codex_version_from(line: &str) -> Option<String> {
    line.split_whitespace()
        .rev()
        .find(|token| {
            token.contains('.') && token.chars().next().is_some_and(|c| c.is_ascii_digit())
        })
        .map(str::to_string)
}

/// The terminal token codex's UA ends with: a faithful port of
/// terminal-detection's `user_agent_token` (same detection order —
/// `TERM_PROGRAM` with its version unless it names tmux, then the
/// terminal-specific variables, then `TERM` itself). The raw `TERM_PROGRAM`
/// and `TERM` values are sent as-is, the way codex's token does; the
/// multiplexer codex detects is never rendered into the token, so it is not
/// detected here either. `env` answers `Some("")` for a variable that is set
/// but empty — the table reads that differently from an unset one.
fn terminal_token(env: &dyn Fn(&str) -> Option<String>) -> String {
    let non_empty = |name: &str| env(name).filter(|value| !value.is_empty());
    let has = |name: &str| env(name).is_some();
    let versioned = |name: &str, version: Option<String>| match version {
        Some(version) => format!("{name}/{version}"),
        None => name.to_string(),
    };
    let raw = if let Some(program) =
        non_empty("TERM_PROGRAM").filter(|p| !p.eq_ignore_ascii_case("tmux"))
    {
        match non_empty("TERM_PROGRAM_VERSION") {
            Some(version) => format!("{program}/{version}"),
            None => program,
        }
    } else if non_empty("GHOSTTY_RESOURCES_DIR").is_some() {
        "Ghostty".to_string()
    } else if has("WEZTERM_VERSION") {
        versioned("WezTerm", non_empty("WEZTERM_VERSION"))
    } else if has("ITERM_SESSION_ID") || has("ITERM_PROFILE") || has("ITERM_PROFILE_NAME") {
        "iTerm.app".to_string()
    } else if has("TERM_SESSION_ID") {
        "Apple_Terminal".to_string()
    } else if has("KITTY_WINDOW_ID") || non_empty("TERM").is_some_and(|t| t.contains("kitty")) {
        "kitty".to_string()
    } else if has("ALACRITTY_SOCKET") || non_empty("TERM").is_some_and(|t| t == "alacritty") {
        "Alacritty".to_string()
    } else if has("KONSOLE_VERSION") {
        versioned("Konsole", non_empty("KONSOLE_VERSION"))
    } else if has("GNOME_TERMINAL_SCREEN") {
        "gnome-terminal".to_string()
    } else if has("VTE_VERSION") {
        versioned("VTE", non_empty("VTE_VERSION"))
    } else if has("WT_SESSION") {
        "WindowsTerminal".to_string()
    } else {
        non_empty("TERM").unwrap_or_else(|| "unknown".to_string())
    };
    sanitize_header_value(raw)
}

/// Replaces every character outside codex's allowed UA set with `_`, the way
/// its `sanitize_header_value` does.
fn sanitize_header_value(value: String) -> String {
    value
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// The headers codex's backend client puts on every WHAM request, applied over
/// `req`: its UA, the bearer token, the account id (a multi-workspace login
/// answers for whichever account this names; without it the server picks, and
/// a spend could land on the wrong workspace) and the fedramp flag.
pub(crate) fn apply_codex_headers<B>(
    req: ureq::RequestBuilder<B>,
    access_token: &str,
    account_id: Option<&str>,
    fedramp: bool,
) -> ureq::RequestBuilder<B> {
    let mut req = req.header("User-Agent", codex_user_agent());
    req = req.header("Authorization", &format!("Bearer {access_token}"));
    if let Some(id) = account_id.map(str::trim).filter(|id| !id.is_empty()) {
        req = req.header("ChatGPT-Account-ID", id);
    }
    if fedramp {
        req = req.header("X-OpenAI-Fedramp", "true");
    }
    req
}

#[cfg(test)]
#[path = "../../tests/inline/codex_headers.rs"]
mod tests;
