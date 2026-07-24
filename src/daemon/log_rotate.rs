//! In-place trim of the daemon's launchd-redirected log (TECH-12 / finding #39).
//!
//! `~/.clauth/daemon.log` is opened by **launchd** (`StandardErrorPath`) with an
//! `O_APPEND` fd that launchd holds for the daemon's whole lifetime and never
//! reopens. That rules out rename-based rotation: renaming the file leaves
//! launchd appending to the renamed inode, so the "new" `daemon.log` stays empty
//! and the real file grows unbounded. The only rotation that survives a held
//! `O_APPEND` fd is an **in-place trim** — rewrite the SAME inode to keep just its
//! tail. launchd's next append lands at the new EOF, so logging continues
//! seamlessly; at most a few bytes written during the trim window are dropped (a
//! diagnostic log, not a ledger — an acceptable, rare loss).
//!
//! Sized for the real worst case (#39 corrected: ~5 MB/day, not "multi-GB in
//! months"), so this is a size cap, not a heavyweight log pipeline.

use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

#[cfg(unix)]
use crate::logline::logline;

/// Trim once the log passes ~5 MiB…
pub(crate) const LOG_MAX_BYTES: u64 = 5 * 1024 * 1024;
/// …keeping the last ~1 MiB of history.
pub(crate) const LOG_KEEP_BYTES: u64 = 1024 * 1024;

/// If `path` is larger than `max_bytes`, rewrite it in place to retain only its
/// last `keep_bytes` (trimmed to a line boundary). Returns `Ok(true)` when a trim
/// happened. The common case — file absent or under the cap — is a single
/// `metadata` stat returning `Ok(false)`, cheap enough to call on a timer.
///
/// Never renames: launchd holds this inode's `O_APPEND` fd, so a rename would
/// orphan every future log line. The rewrite is on the same inode, and after
/// `set_len` launchd's next append resumes at the new EOF.
pub(crate) fn rotate_log_if_large(
    path: &Path,
    max_bytes: u64,
    keep_bytes: u64,
) -> std::io::Result<bool> {
    let len = match std::fs::metadata(path) {
        Ok(m) => m.len(),
        // Absent — e.g. a manual `clauth daemon` whose stderr goes to the tty, not
        // this file. Nothing to trim.
        Err(_) => return Ok(false),
    };
    if len <= max_bytes {
        return Ok(false);
    }
    let keep = keep_bytes.min(len);
    let mut f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)?;

    // Read the tail we intend to preserve.
    f.seek(SeekFrom::Start(len - keep))?;
    let mut tail = Vec::with_capacity(keep as usize);
    f.read_to_end(&mut tail)?;

    // Drop a leading partial line so the retained region starts clean.
    let start = tail.iter().position(|&b| b == b'\n').map_or(0, |i| i + 1);
    let tail = &tail[start..];

    // Rewrite in place on the same inode (see module doc — never rename).
    f.seek(SeekFrom::Start(0))?;
    f.write_all(tail)?;
    f.set_len(tail.len() as u64)?;
    f.flush()?;
    Ok(true)
}

/// Whether the size cap can still hold: stderr is a regular file opened WITHOUT
/// `O_APPEND`. The in-place trim moves EOF backwards, and a writer that is not in
/// append mode keeps writing at its own stale offset — so the trim leaves a
/// sparse hole and the file grows without bound anyway. launchd's
/// `StandardErrorPath` opens `O_APPEND`; a hand-rolled `clauth daemon >
/// daemon.log` does not (`>>` does). A tty or pipe has no cap to defeat.
#[cfg(unix)]
fn log_cap_defeated(stderr_is_regular_file: bool, stderr_is_append: bool) -> bool {
    stderr_is_regular_file && !stderr_is_append
}

/// `(stderr is a regular file, stderr is O_APPEND)`; `None` when fd 2 cannot be
/// interrogated.
#[cfg(unix)]
#[allow(unsafe_code)]
fn stderr_file_mode() -> Option<(bool, bool)> {
    // SAFETY: both calls only read fd 2's kernel state, `fstat` into a `stat` we
    // own. fd 2 is always open in a live process.
    let (flags, st) = unsafe {
        let flags = libc::fcntl(libc::STDERR_FILENO, libc::F_GETFL);
        let mut st: libc::stat = std::mem::zeroed();
        let rc = libc::fstat(libc::STDERR_FILENO, &mut st);
        if flags < 0 || rc != 0 {
            return None;
        }
        (flags, st)
    };
    Some((
        st.st_mode & libc::S_IFMT == libc::S_IFREG,
        flags & libc::O_APPEND != 0,
    ))
}

/// Boot check for the append-mode requirement the cap rests on: the daemon is
/// long-lived and its log silently unbounded without it, so this is worth a loud
/// line at startup rather than a doc note.
#[cfg(unix)]
pub(crate) fn warn_if_log_cap_defeated() {
    if stderr_file_mode().is_some_and(|(file, append)| log_cap_defeated(file, append)) {
        logline!(
            "clauth daemon: stderr is a non-append file redirect: the daemon.log size cap \
             cannot hold and the file will grow unbounded; redirect with `>>` or let launchd's \
             StandardErrorPath open it"
        );
    }
}

#[cfg(not(unix))]
pub(crate) fn warn_if_log_cap_defeated() {}

#[cfg(test)]
#[path = "../../tests/inline/daemon_log_rotate.rs"]
mod tests;
