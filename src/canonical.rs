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

/// The file a write to `path` acts on, and whether the user keeps a link at
/// `path`.
#[cfg(not(windows))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WriteTarget {
    /// The regular file clauth writes, replaces or removes.
    pub(crate) file: PathBuf,
    /// A symlink sits at `path`: the user's, never clauth's own file.
    pub(crate) linked: bool,
}

/// Resolve the file an override write acts on: `path` itself for a regular
/// file or nothing there, the resolved target for a symlink, so a link the
/// user keeps stays a link and clauth writes, replaces or removes only the
/// file it points at. A path that is neither a regular file nor a link to one
/// (a dangling link, a link loop, a directory) is an error: clauth writes
/// nothing there.
#[cfg(not(windows))]
pub(crate) fn write_target(path: &Path) -> io::Result<WriteTarget> {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return Ok(WriteTarget {
                file: path.to_path_buf(),
                linked: false,
            });
        }
        Err(e) => return Err(e),
    };
    if !meta.file_type().is_symlink() {
        return if meta.is_file() {
            Ok(WriteTarget {
                file: path.to_path_buf(),
                linked: false,
            })
        } else {
            Err(io::Error::other(format!(
                "{} is not a regular file",
                path.display()
            )))
        };
    }
    let file = identity(path)?;
    if !std::fs::metadata(&file)?.is_file() {
        return Err(io::Error::other(format!(
            "{} does not resolve to a regular file",
            path.display()
        )));
    }
    Ok(WriteTarget { file, linked: true })
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
