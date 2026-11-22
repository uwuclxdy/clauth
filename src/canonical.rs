//! Path canonicalization, one function per purpose. No other module calls
//! `canonicalize` or `dunce`: `clippy.toml`'s `disallowed-methods` refuses any
//! raw canonicalizer outside this file.

use std::io;
use std::path::{Path, PathBuf};

/// `path` resolved for comparing or keying inside this process. Every side of
/// a comparison takes this same form; never shown, never handed to another
/// program.
#[expect(
    clippy::disallowed_methods,
    reason = "this module is the one place that calls it"
)]
pub(crate) fn identity(path: impl AsRef<Path>) -> io::Result<PathBuf> {
    std::fs::canonicalize(path)
}

/// `path` resolved the way codex resolves its `CODEX_HOME` before keying hook trust: std's resolution, so on Windows the `\\?\` verbatim prefix stays, since codex keys trust with that spelling and a plain one never matches it.
#[expect(
    clippy::disallowed_methods,
    reason = "this module is the one place that calls it"
)]
pub(crate) fn codex_home(path: impl AsRef<Path>) -> io::Result<PathBuf> {
    std::fs::canonicalize(path)
}

/// `dunce::canonicalize`: `path` resolved for showing or handing to another
/// program. Windows' `\\?\` verbatim prefix goes wherever a plain spelling
/// exists; past 260 characters and on UNC paths it stays, as Windows needs.
#[expect(
    clippy::disallowed_methods,
    reason = "this module is the one place that calls it"
)]
pub(crate) fn plain(path: impl AsRef<Path>) -> io::Result<PathBuf> {
    dunce::canonicalize(path)
}

/// `dunce::simplified`: an already-resolved path in its plain spelling: an
/// [`identity`] result at the moment it is shown, or a stored path an older
/// clauth wrote verbatim.
#[expect(
    clippy::disallowed_methods,
    reason = "this module is the one place that calls it"
)]
pub(crate) fn display(path: &Path) -> &Path {
    dunce::simplified(path)
}

/// `path` spelled as Claude Code keys its project dirs on Windows: `\\?\`
/// dropped at any length, `\\?\UNC\server\…` as `\\server\…`.
pub(crate) fn claude_code_spelling(path: &str) -> String {
    if let Some(rest) = path.strip_prefix("\\\\?\\UNC\\") {
        format!("\\\\{rest}")
    } else if let Some(rest) = path.strip_prefix("\\\\?\\") {
        rest.to_string()
    } else {
        path.to_string()
    }
}

#[cfg(test)]
#[path = "../tests/inline/canonical.rs"]
mod tests;
