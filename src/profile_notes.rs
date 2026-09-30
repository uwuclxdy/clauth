//! Per-account free-form notes: one plain-text `note.txt` per profile dir.
//!
//! The file rides the profile directory, so a rename carries the note and a
//! delete removes it with the account. claude-roster only: the TUI's usage tab
//! picks claude profiles, so no codex-roster gate sits beside `is_configured`.

/// The per-profile file name under the profile dir (`profiles/<name>/note.txt`).
pub(crate) const NOTE_FILE: &str = "note.txt";

use crate::profile::ProfileName;

/// The account's stored note, `None` when it has none. An empty file reads as
/// no note, matching what saving an empty note does (removes the file).
pub(crate) fn load_note(name: &ProfileName) -> Option<String> {
    // Bytes + lossy, never `read_to_string`: a hand-edited note.txt holding
    // invalid UTF-8 must surface in the editor, not read as "no note" — an
    // empty draft saved over it would silently destroy it.
    let bytes = crate::profile_cache::profile_cache_path(name, NOTE_FILE)
        .and_then(|p| std::fs::read(p).ok())?;
    let text = String::from_utf8_lossy(&bytes).into_owned();
    Some(text).filter(|t| !t.is_empty())
}

/// Store the account's note, 0600 via the tree discipline. An empty note
/// removes the file instead — clearing the note returns the tab to its hint.
/// Refused for a name the roster no longer carries, so a save can never
/// re-create a deleted account's directory.
pub(crate) fn save_note(name: &ProfileName, text: &str) -> std::io::Result<()> {
    if text.is_empty() {
        // Clearing an absent note is fine (NotFound); any other failure is the
        // caller's to surface — a lying `note saved` toast is worse than the
        // error.
        return match crate::profile_cache::profile_cache_path(name, NOTE_FILE) {
            Some(path) => match std::fs::remove_file(path) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(e),
            },
            None => Ok(()),
        };
    }
    if !crate::profile::is_configured(name).unwrap_or(false) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "account is no longer in the roster",
        ));
    }
    let Some(path) = crate::profile_cache::profile_cache_path(name, NOTE_FILE) else {
        return Err(std::io::Error::other("cannot resolve the profile dir"));
    };
    crate::profile::atomic_write_600(&path, text.as_bytes())
}

#[cfg(test)]
#[path = "../tests/inline/profile_notes.rs"]
mod tests;
