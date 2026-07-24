//! Shared test-only helpers used across the inline test modules
//! (`tests/inline/*.rs`). Defined once here rather than copied per module so the
//! home-sandbox, mtime, and key-event scaffolding stays in a single place.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::SystemTime;

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// RAII home sandbox: acquires `HOME_TEST_LOCK` and redirects `home_dir()` into
/// a tempdir for its lifetime, clearing the override on drop (even on panic).
/// Required for any test that writes into the per-profile tree or creates
/// session dirs, pid files, or rotation locks — otherwise those paths land in
/// the real `~/.clauth`.
pub(crate) struct HomeSandbox {
    // Drop order: tempdir first, then the shared lock.
    _tmp: tempfile::TempDir,
    _guard: crate::lockorder::RankedGuard<'static, ()>,
    home: PathBuf,
}

impl HomeSandbox {
    pub(crate) fn new() -> Self {
        // Untracked HOME_TEST_LOCK acquired first; no RankedMutex/flock is held.
        let guard = crate::profile::HOME_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().expect("create home sandbox");
        let home = tmp.path().to_path_buf();
        crate::profile::set_home_override(home.clone());
        Self {
            _tmp: tmp,
            _guard: guard,
            home,
        }
    }

    /// Path to the sandboxed home directory.
    pub(crate) fn home(&self) -> &Path {
        &self.home
    }
}

impl Drop for HomeSandbox {
    fn drop(&mut self) {
        crate::profile::clear_home_override();
    }
}

/// RAII tier pin: acquires `TIER_TEST_LOCK` and forces the process-global color
/// tier for its lifetime, putting the previous pin back on drop (even on panic).
/// Required for any test asserting on a tier-dependent style, since the tier is
/// process-global and otherwise auto-detects from the ambient `$COLORTERM`.
pub(crate) struct TierSandbox {
    // Drop order: this type's `drop` restores under the lock, which the field
    // then releases.
    _guard: crate::lockorder::RankedGuard<'static, ()>,
    prev: Option<crate::tui::theme::Tier>,
}

impl TierSandbox {
    pub(crate) fn new(tier: crate::tui::theme::Tier) -> Self {
        let guard = crate::tui::theme::TIER_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let prev = crate::tui::theme::tier_override();
        crate::tui::theme::set_tier(tier);
        Self {
            _guard: guard,
            prev,
        }
    }
}

impl Drop for TierSandbox {
    fn drop(&mut self) {
        crate::tui::theme::restore_tier(self.prev);
    }
}

/// A minimal `Profile` with every optional field unset — tests fill in what
/// they assert on.
pub(crate) fn blank_profile(name: &str) -> crate::profile::Profile {
    crate::profile::Profile {
        harness: Default::default(),
        name: name.into(),
        base_url: None,
        api_key: None,
        auto_start: false,
        env: Default::default(),
        models: Default::default(),
        fallback_threshold: None,
        weekly_threshold: None,
        last_resort: false,
        session_feed: false,
        max_auto_spend: None,
        check_weekly: true,
        check_scoped: true,
        bell_threshold: None,
        disabled: false,
        credentials: None,
        usage: None,
        fetch_status: None,
        provider: None,
        third_party_usage: None,
    }
}

/// Overwrite a file's modification time — for cache-staleness / tie-break tests.
pub(crate) fn set_mtime(path: &Path, when: SystemTime) {
    let file = std::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .expect("open for mtime");
    file.set_modified(when).expect("set_modified");
}

/// A `Press` key event with no modifiers.
pub(crate) fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

/// Collect a `Command`'s queued env overrides: key → `Some(value)` for a set
/// var, key → `None` for a removed one. `get_envs` reflects only the explicit
/// `env`/`env_remove` ops, which is exactly what we assert. No process env or
/// spawn needed, so this is lock-free and non-flaky.
pub(crate) fn env_overrides(cmd: &Command) -> HashMap<String, Option<String>> {
    cmd.get_envs()
        .map(|(k, v)| {
            (
                k.to_string_lossy().into_owned(),
                v.map(|s| s.to_string_lossy().into_owned()),
            )
        })
        .collect()
}

/// Unpadded base64url encoder (test fixtures only) — the single home for the
/// fake-JWT building the codex tests share, so the three test modules can't
/// drift apart on the encoding.
pub(crate) fn b64url_nopad(data: &[u8]) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::new();
    for chunk in data.chunks(3) {
        let mut acc: u32 = 0;
        for (i, &b) in chunk.iter().enumerate() {
            acc |= (b as u32) << (16 - 8 * i);
        }
        for i in 0..(chunk.len() * 8).div_ceil(6) {
            out.push(ALPHABET[((acc >> (18 - 6 * i)) & 0x3f) as usize] as char);
        }
    }
    out
}

/// A fake, unsigned JWT carrying `claims` — NEVER a real token. Shape matches
/// what `codex::auth::jwt_claims` decodes (header.payload.signature).
pub(crate) fn fake_jwt(claims: &serde_json::Value) -> String {
    format!(
        "{}.{}.fake-sig",
        b64url_nopad(br#"{"alg":"RS256","typ":"JWT"}"#),
        b64url_nopad(claims.to_string().as_bytes())
    )
}

/// Every path under `root` breaking the owner-only invariant clauth holds over
/// `~/.clauth` (0o700 dirs, 0o600 files), rendered as `<mode> <path>` lines.
/// Symlinks are skipped — a link's own mode is meaningless and its target lives
/// outside the tree.
#[cfg(unix)]
pub(crate) fn owner_only_violations(root: &Path) -> Vec<String> {
    use std::os::unix::fs::PermissionsExt;

    let mut out = Vec::new();
    let Ok(meta) = root.symlink_metadata() else {
        return out;
    };
    if meta.file_type().is_symlink() {
        return out;
    }
    let is_dir = meta.file_type().is_dir();
    let mode = meta.permissions().mode() & 0o777;
    let want = if is_dir { 0o700 } else { 0o600 };
    if mode != want {
        out.push(format!("{mode:#o} {} (want {want:#o})", root.display()));
    }
    if is_dir && let Ok(entries) = std::fs::read_dir(root) {
        for entry in entries.flatten() {
            out.extend(owner_only_violations(&entry.path()));
        }
    }
    out
}

/// Flatten a rendered `TestBackend` buffer to one `String` per row (cell symbols
/// concatenated). Shared by the TUI render tests so each keeps a single copy of
/// the buffer→text step; callers `.concat()` or `.join("\n")` to taste.
pub(crate) fn buffer_rows(buf: &ratatui::buffer::Buffer) -> Vec<String> {
    let w = buf.area.width as usize;
    let h = buf.area.height as usize;
    (0..h)
        .map(|y| (0..w).map(|x| buf.content[y * w + x].symbol()).collect())
        .collect()
}
