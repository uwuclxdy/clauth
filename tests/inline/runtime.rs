#![allow(unsafe_code)]
use super::*;
use std::fs;
use std::time::{Duration, SystemTime};

use crate::testutil::{HomeSandbox, set_mtime};

// V1 expires_at < V2 so tie-break tests can assert which side wins unambiguously.
const CREDS_V1: &[u8] = br#"{"claudeAiOauth":{"accessToken":"tok1","expiresAt":1000}}"#;
const CREDS_V2: &[u8] = br#"{"claudeAiOauth":{"accessToken":"tok2","expiresAt":2000}}"#;

#[test]
fn sync_no_op_when_link_missing() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let canonical = tmp.path().join("canonical.json");
    let link_path = tmp.path().join(".credentials.json");
    assert!(!sync_credentials_unlocked(&link_path, &canonical).expect("sync"));
    assert!(!canonical.exists());
}

#[cfg(unix)]
#[test]
fn sync_no_op_when_link_is_symlink() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let canonical = tmp.path().join("canonical.json");
    let link_path = tmp.path().join(".credentials.json");
    fs::write(&canonical, CREDS_V1).expect("write canonical");
    std::os::unix::fs::symlink(&canonical, &link_path).expect("symlink");
    assert!(!sync_credentials_unlocked(&link_path, &canonical).expect("sync"));
    assert_eq!(fs::read(&canonical).expect("read"), CREDS_V1);
    assert!(
        link_path
            .symlink_metadata()
            .expect("meta")
            .file_type()
            .is_symlink()
    );
}

#[test]
fn sync_skips_invalid_json() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let canonical = tmp.path().join("canonical.json");
    let link_path = tmp.path().join(".credentials.json");
    fs::write(&link_path, b"not json").expect("write link");
    fs::write(&canonical, CREDS_V1).expect("write canonical");
    assert!(!sync_credentials_unlocked(&link_path, &canonical).expect("sync"));
    assert_eq!(fs::read(&canonical).expect("read"), CREDS_V1);
    // link stayed a regular file — waiting for CC's write to complete
    let meta = link_path.symlink_metadata().expect("meta");
    assert!(!meta.file_type().is_symlink());
}

#[test]
fn sync_skips_empty_credentials() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let canonical = tmp.path().join("canonical.json");
    let link_path = tmp.path().join(".credentials.json");
    // {} parses as ClaudeCredentials but carries no OAuth token — treat as partial
    fs::write(&link_path, b"{}").expect("write link");
    fs::write(&canonical, CREDS_V1).expect("write canonical");
    assert!(!sync_credentials_unlocked(&link_path, &canonical).expect("sync"));
    assert_eq!(fs::read(&canonical).expect("read"), CREDS_V1);
}

#[test]
fn sync_relinks_when_content_matches_canonical() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let canonical = tmp.path().join("canonical.json");
    let link_path = tmp.path().join(".credentials.json");
    fs::write(&link_path, CREDS_V1).expect("write link");
    fs::write(&canonical, CREDS_V1).expect("write canonical");
    assert!(!sync_credentials_unlocked(&link_path, &canonical).expect("sync"));
    assert_eq!(fs::read(&canonical).expect("read"), CREDS_V1);
    #[cfg(unix)]
    assert!(
        link_path
            .symlink_metadata()
            .expect("meta")
            .file_type()
            .is_symlink()
    );
}

#[test]
fn sync_writes_canonical_when_differs() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let canonical = tmp.path().join("canonical.json");
    let link_path = tmp.path().join(".credentials.json");
    fs::write(&link_path, CREDS_V2).expect("write link");
    fs::write(&canonical, CREDS_V1).expect("write canonical");
    let base = SystemTime::now(); // runtime is newer → wins mtime tie-break
    set_mtime(&canonical, base);
    set_mtime(&link_path, base + Duration::from_secs(5));
    assert!(sync_credentials_unlocked(&link_path, &canonical).expect("sync"));
    assert_eq!(fs::read(&canonical).expect("read"), CREDS_V2);
    #[cfg(unix)]
    assert!(
        link_path
            .symlink_metadata()
            .expect("meta")
            .file_type()
            .is_symlink()
    );
}

#[test]
fn sync_creates_canonical_when_missing() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let canonical = tmp.path().join("nested").join("canonical.json");
    let link_path = tmp.path().join(".credentials.json");
    fs::write(&link_path, CREDS_V1).expect("write link");
    assert!(sync_credentials_unlocked(&link_path, &canonical).expect("sync"));
    assert_eq!(fs::read(&canonical).expect("read"), CREDS_V1);
    #[cfg(unix)]
    assert!(
        link_path
            .symlink_metadata()
            .expect("meta")
            .file_type()
            .is_symlink()
    );
}

// ── expires_at tie-breaking in sync_credentials_unlocked ─────────────────────

#[test]
fn sync_no_write_when_bytes_identical() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let canonical = tmp.path().join("canonical.json");
    let link_path = tmp.path().join(".credentials.json");
    fs::write(&link_path, CREDS_V1).expect("write link");
    fs::write(&canonical, CREDS_V1).expect("write canonical");

    let written = sync_credentials_unlocked(&link_path, &canonical).expect("sync");
    assert!(!written, "no write when bytes identical");
    assert_eq!(fs::read(&canonical).expect("read"), CREDS_V1);
}

// Canonical newer → canonical wins; mtime is primary (expires_at agrees: V2 > V1).
#[test]
fn sync_canonical_wins_when_written_more_recently() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let canonical = tmp.path().join("canonical.json");
    let link_path = tmp.path().join(".credentials.json");
    fs::write(&link_path, CREDS_V1).expect("write runtime (stale)");
    fs::write(&canonical, CREDS_V2).expect("write canonical (rotated)");
    let base = SystemTime::now(); // canonical strictly newer
    set_mtime(&link_path, base);
    set_mtime(&canonical, base + Duration::from_secs(5));

    let written = sync_credentials_unlocked(&link_path, &canonical).expect("sync");
    assert!(
        !written,
        "canonical must not be overwritten when it is the more recent write"
    );
    assert_eq!(fs::read(&canonical).expect("read canonical"), CREDS_V2);
    #[cfg(unix)]
    assert!(
        link_path
            .symlink_metadata()
            .expect("meta")
            .file_type()
            .is_symlink(),
        "runtime re-linked to canonical even when canonical wins"
    );
}

// Runtime newer → runtime wins; mtime is primary (expires_at agrees: V2 > V1).
#[test]
fn sync_runtime_wins_when_written_more_recently() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let canonical = tmp.path().join("canonical.json");
    let link_path = tmp.path().join(".credentials.json");
    fs::write(&link_path, CREDS_V2).expect("write runtime (newer)");
    fs::write(&canonical, CREDS_V1).expect("write canonical (older)");
    let base = SystemTime::now();
    set_mtime(&canonical, base);
    set_mtime(&link_path, base + Duration::from_secs(5));

    let written = sync_credentials_unlocked(&link_path, &canonical).expect("sync");
    assert!(
        written,
        "canonical must be overwritten when runtime is the more recent write"
    );
    assert_eq!(fs::read(&canonical).expect("read canonical"), CREDS_V2);
}

// Bug fix: rotate-all can stamp a canonical token with later expires_at than a
// fresh CC re-login written after. mtime must decide — not expires_at — or the
// watchdog silently discards the user's just-completed login and burns its chain.
#[test]
fn sync_runtime_wins_when_newer_mtime_despite_lower_expires_at() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let canonical = tmp.path().join("canonical.json");
    let link_path = tmp.path().join(".credentials.json");
    // canonical (rotated) has later expires_at (V2=2000); runtime (CC re-login) has V1=1000 but written last
    fs::write(&canonical, CREDS_V2).expect("write canonical (rotated, later exp)");
    fs::write(&link_path, CREDS_V1).expect("write runtime (fresh re-login)");
    let base = SystemTime::now();
    set_mtime(&canonical, base);
    set_mtime(&link_path, base + Duration::from_secs(5));

    let written = sync_credentials_unlocked(&link_path, &canonical).expect("sync");
    assert!(
        written,
        "runtime re-login must win on newer mtime even with lower expires_at"
    );
    assert_eq!(
        fs::read(&canonical).expect("read canonical"),
        CREDS_V1,
        "CC's fresh login bytes must be preserved into canonical, not discarded"
    );
}

// mtime tie → fall back to expires_at; canonical V2 > V1 wins, runtime re-linked.
#[test]
fn sync_falls_back_to_expires_at_on_equal_mtime() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let canonical = tmp.path().join("canonical.json");
    let link_path = tmp.path().join(".credentials.json");
    fs::write(&link_path, CREDS_V1).expect("write runtime");
    fs::write(&canonical, CREDS_V2).expect("write canonical");
    let when = SystemTime::now();
    set_mtime(&link_path, when);
    set_mtime(&canonical, when);

    let written = sync_credentials_unlocked(&link_path, &canonical).expect("sync");
    assert!(
        !written,
        "on equal mtime, higher expires_at (canonical) wins the fallback"
    );
    assert_eq!(fs::read(&canonical).expect("read canonical"), CREDS_V2);
}

// The tie-break in isolation, no filesystem: mtime is primary, expires_at only
// breaks an equal/missing-mtime tie, and an absent canonical always yields.
#[test]
fn resolve_credential_winner_prefers_recency_then_expiry() {
    let early = SystemTime::UNIX_EPOCH + Duration::from_secs(100);
    let late = SystemTime::UNIX_EPOCH + Duration::from_secs(200);

    // Newer runtime mtime wins even with a later canonical expiry.
    assert!(!resolve_credential_winner(
        Some(999),
        Some(1),
        Some(early),
        Some(late)
    ));
    // Newer canonical mtime keeps canonical despite a later runtime expiry.
    assert!(resolve_credential_winner(
        Some(1),
        Some(999),
        Some(late),
        Some(early)
    ));
    // Equal mtime → expiry tie-break; canonical wins the `>=` tie.
    assert!(resolve_credential_winner(
        Some(5),
        Some(5),
        Some(late),
        Some(late)
    ));
    // Runtime carries no token → keep canonical.
    assert!(resolve_credential_winner(Some(1), None, None, None));
    // Canonical missing/unparseable → runtime wins.
    assert!(!resolve_credential_winner(None, Some(1), None, None));
}

// Canonical absent → runtime always wins.
#[test]
fn sync_runtime_wins_when_canonical_missing_expires_at() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let canonical = tmp.path().join("nested").join("canonical.json");
    let link_path = tmp.path().join(".credentials.json");
    fs::write(&link_path, CREDS_V1).expect("write runtime");

    let written = sync_credentials_unlocked(&link_path, &canonical).expect("sync");
    assert!(
        written,
        "runtime must become canonical when canonical is absent"
    );
    assert_eq!(fs::read(&canonical).expect("read"), CREDS_V1);
}

// Canonical unparseable → runtime wins (safer than discarding it).
#[test]
fn sync_runtime_wins_when_canonical_unparseable() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let canonical = tmp.path().join("canonical.json");
    let link_path = tmp.path().join(".credentials.json");
    fs::write(&link_path, CREDS_V1).expect("write runtime");
    fs::write(&canonical, b"corrupt json {{{").expect("write corrupt canonical");

    let written = sync_credentials_unlocked(&link_path, &canonical).expect("sync");
    assert!(written, "runtime must win when canonical is unparseable");
    assert_eq!(fs::read(&canonical).expect("read"), CREDS_V1);
}

#[test]
fn live_session_blocks_liveness_probe() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let pid_file = tmp.path().join("pid");
    let file = open_pid_file(&pid_file).expect("open");
    file.lock().expect("lock");
    assert!(is_session_alive(&pid_file));
    drop(file);
    assert!(!is_session_alive(&pid_file));
}

#[test]
fn prune_removes_dead_keeps_alive() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let alive_path = tmp.path().join("alive");
    let dead_path = tmp.path().join("dead");
    let alive = open_pid_file(&alive_path).expect("open alive");
    alive.lock().expect("lock alive");
    fs::write(&dead_path, b"").expect("write dead");

    let count = prune_stale_sessions(tmp.path()).expect("prune");
    assert_eq!(count, 1);
    assert!(alive_path.exists());
    assert!(!dead_path.exists());
    drop(alive);
}

#[test]
fn copy_tree_replicates_files_and_subdirs() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let src = tmp.path().join("src");
    fs::create_dir_all(src.join("nested")).expect("mkdir");
    fs::write(src.join("a.txt"), b"hello").expect("write a");
    fs::write(src.join("nested").join("b.txt"), b"world").expect("write b");

    let dst = tmp.path().join("dst");
    copy_tree(&src, &dst).expect("copy_tree");

    assert_eq!(fs::read(dst.join("a.txt")).expect("read a"), b"hello");
    assert_eq!(
        fs::read(dst.join("nested").join("b.txt")).expect("read b"),
        b"world"
    );
}

#[test]
fn mirror_credentials_newer_runtime_wins() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let canonical = tmp.path().join("canonical.json");
    let runtime = tmp.path().join(".credentials.json");
    fs::write(&canonical, CREDS_V1).expect("write canonical");
    fs::write(&runtime, CREDS_V2).expect("write runtime");
    let past = SystemTime::now() - Duration::from_secs(60);
    let now = SystemTime::now();
    set_mtime(&canonical, past);
    set_mtime(&runtime, now);

    mirror_credentials(&runtime, &canonical).expect("mirror");
    assert_eq!(fs::read(&canonical).expect("read"), CREDS_V2);
}

#[test]
fn mirror_credentials_newer_canonical_wins() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let canonical = tmp.path().join("canonical.json");
    let runtime = tmp.path().join(".credentials.json");
    fs::write(&canonical, CREDS_V2).expect("write canonical");
    fs::write(&runtime, CREDS_V1).expect("write runtime");
    let past = SystemTime::now() - Duration::from_secs(60);
    let now = SystemTime::now();
    set_mtime(&runtime, past);
    set_mtime(&canonical, now);

    mirror_credentials(&runtime, &canonical).expect("mirror");
    assert_eq!(fs::read(&runtime).expect("read"), CREDS_V2);
}

#[test]
fn mirror_credentials_skips_invalid_json() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let canonical = tmp.path().join("canonical.json");
    let runtime = tmp.path().join(".credentials.json");
    fs::write(&canonical, CREDS_V1).expect("write canonical");
    fs::write(&runtime, b"partial write").expect("write runtime");
    let past = SystemTime::now() - Duration::from_secs(60);
    let now = SystemTime::now();
    set_mtime(&canonical, past);
    set_mtime(&runtime, now);

    mirror_credentials(&runtime, &canonical).expect("mirror");
    assert_eq!(fs::read(&canonical).expect("read"), CREDS_V1); // canonical untouched; partial JSON ignored
}

#[test]
fn mirror_credentials_skips_empty_credentials() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let canonical = tmp.path().join("canonical.json");
    let runtime = tmp.path().join(".credentials.json");
    fs::write(&canonical, CREDS_V1).expect("write canonical");
    // {} parses as ClaudeCredentials but has no OAuth token
    fs::write(&runtime, b"{}").expect("write runtime");
    let past = SystemTime::now() - Duration::from_secs(60);
    let now = SystemTime::now();
    set_mtime(&canonical, past);
    set_mtime(&runtime, now);

    mirror_credentials(&runtime, &canonical).expect("mirror");
    assert_eq!(fs::read(&canonical).expect("read"), CREDS_V1);
}

#[test]
fn mirror_credentials_seeds_missing_side() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let canonical = tmp.path().join("nested").join("canonical.json");
    let runtime = tmp.path().join(".credentials.json");
    fs::write(&runtime, CREDS_V1).expect("write runtime");

    mirror_credentials(&runtime, &canonical).expect("mirror");
    assert_eq!(fs::read(&canonical).expect("read"), CREDS_V1);
}

#[test]
fn mirror_tree_propagates_runtime_edit_to_canonical() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let claude = tmp.path().join("claude");
    let runtime = tmp.path().join("runtime");
    fs::create_dir_all(&claude).expect("mkdir claude");
    fs::create_dir_all(&runtime).expect("mkdir runtime");
    fs::write(claude.join("todos.json"), b"[]").expect("write canonical");

    copy_tree(&claude, &runtime).expect("copy");

    // simulate CC rewriting the runtime copy
    fs::write(runtime.join("todos.json"), br#"[{"id":1}]"#).expect("write runtime");
    let past = SystemTime::now() - Duration::from_secs(60);
    let now = SystemTime::now();
    set_mtime(&claude.join("todos.json"), past);
    set_mtime(&runtime.join("todos.json"), now);

    mirror_tree(&claude, &runtime).expect("mirror");
    assert_eq!(
        fs::read(claude.join("todos.json")).expect("read canonical"),
        br#"[{"id":1}]"#
    );
}

#[test]
fn mirror_tree_skips_top_level_settings_and_credentials() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let claude = tmp.path().join("claude");
    let runtime = tmp.path().join("runtime");
    fs::create_dir_all(&claude).expect("mkdir claude");
    fs::create_dir_all(&runtime).expect("mkdir runtime");
    fs::write(claude.join("settings.json"), br#"{"home":true}"#).expect("write h settings");
    fs::write(runtime.join("settings.json"), br#"{"runtime":true}"#).expect("write r settings");
    fs::write(claude.join(".credentials.json"), CREDS_V1).expect("write h creds");
    fs::write(runtime.join(".credentials.json"), CREDS_V2).expect("write r creds");

    mirror_tree(&claude, &runtime).expect("mirror");

    assert_eq!(
        fs::read(claude.join("settings.json")).expect("read"),
        br#"{"home":true}"#
    );
    assert_eq!(
        fs::read(runtime.join("settings.json")).expect("read"),
        br#"{"runtime":true}"#
    );
    assert_eq!(
        fs::read(claude.join(".credentials.json")).expect("read"),
        CREDS_V1
    );
    assert_eq!(
        fs::read(runtime.join(".credentials.json")).expect("read"),
        CREDS_V2
    );
}

#[test]
fn mirror_tree_skips_identical_files_with_different_mtimes() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let claude = tmp.path().join("claude");
    let runtime = tmp.path().join("runtime");
    fs::create_dir_all(&claude).expect("mkdir claude");
    fs::create_dir_all(&runtime).expect("mkdir runtime");
    let canonical_file = claude.join("state.json");
    let runtime_file = runtime.join("state.json");
    fs::write(&canonical_file, br#"{"same":true}"#).expect("write canonical");
    fs::write(&runtime_file, br#"{"same":true}"#).expect("write runtime");
    let past = SystemTime::now() - Duration::from_secs(60);
    let now = SystemTime::now();
    set_mtime(&canonical_file, past);
    set_mtime(&runtime_file, now);

    mirror_tree(&claude, &runtime).expect("mirror");

    assert_eq!(
        canonical_file
            .metadata()
            .expect("canonical meta")
            .modified()
            .ok(),
        Some(past)
    );
    assert_eq!(
        runtime_file
            .metadata()
            .expect("runtime meta")
            .modified()
            .ok(),
        Some(now)
    );
    assert_eq!(
        fs::read(&canonical_file).expect("read canonical"),
        br#"{"same":true}"#
    );
}

#[test]
fn mirror_tree_seeds_runtime_only_file_to_canonical() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let claude = tmp.path().join("claude");
    let runtime = tmp.path().join("runtime");
    fs::create_dir_all(&claude).expect("mkdir claude");
    fs::create_dir_all(&runtime).expect("mkdir runtime");
    fs::write(runtime.join("runtime-only.json"), br#"{"who":"cc"}"#).expect("write runtime");

    mirror_tree(&claude, &runtime).expect("mirror");
    assert_eq!(
        fs::read(claude.join("runtime-only.json")).expect("read"),
        br#"{"who":"cc"}"#
    );
    assert!(runtime.join("runtime-only.json").exists()); // runtime side preserved
}

#[test]
fn mirror_tree_seeds_canonical_only_file_to_runtime() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let claude = tmp.path().join("claude");
    let runtime = tmp.path().join("runtime");
    fs::create_dir_all(&claude).expect("mkdir claude");
    fs::create_dir_all(&runtime).expect("mkdir runtime");
    fs::write(claude.join("user-edit.json"), br#"{"who":"user"}"#).expect("write canonical");

    mirror_tree(&claude, &runtime).expect("mirror");
    assert_eq!(
        fs::read(runtime.join("user-edit.json")).expect("read"),
        br#"{"who":"user"}"#
    );
}

#[test]
fn mirror_tree_seeds_runtime_only_nested_to_canonical() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let claude = tmp.path().join("claude");
    let runtime = tmp.path().join("runtime");
    fs::create_dir_all(claude.join("projects")).expect("mkdir claude/projects");
    fs::create_dir_all(runtime.join("projects").join("new")).expect("mkdir runtime nested");
    fs::write(
        runtime.join("projects").join("new").join("state.json"),
        br#"{"step":1}"#,
    )
    .expect("write runtime");

    mirror_tree(&claude, &runtime).expect("mirror");
    assert_eq!(
        fs::read(claude.join("projects").join("new").join("state.json")).expect("read"),
        br#"{"step":1}"#
    );
    assert!(
        runtime
            .join("projects")
            .join("new")
            .join("state.json")
            .exists()
    );
}

/// A dir `mirror_tree` seeds back onto the canonical `~/.claude/` side (the
/// runtime side created it first, e.g. CC writing a fresh session-state tree
/// under the runtime's `CLAUDE_CONFIG_DIR`) must land owner-only like every
/// other dir clauth creates under `~/.claude/`, not at the process umask
/// (typically 0755) — same invariant as the rescue path, different trigger
/// (the Fake-symlink-mode watchdog tick instead of isolated-runtime teardown).
#[cfg(unix)]
#[test]
fn mirror_tree_creates_canonical_side_dir_owner_only() {
    use std::os::unix::fs::PermissionsExt;

    let tmp = tempfile::tempdir().expect("tempdir");
    let claude = tmp.path().join("claude");
    let runtime = tmp.path().join("runtime");
    fs::create_dir_all(claude.join("projects")).expect("mkdir claude/projects");
    fs::create_dir_all(runtime.join("projects").join("new")).expect("mkdir runtime nested");
    fs::write(
        runtime.join("projects").join("new").join("state.json"),
        br#"{"step":1}"#,
    )
    .expect("write runtime");

    mirror_tree(&claude, &runtime).expect("mirror");

    let mode = fs::metadata(claude.join("projects").join("new"))
        .expect("meta")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(
        mode, 0o700,
        "a dir mirror_tree creates under ~/.claude must not land at the process umask"
    );
}

#[test]
fn mirror_tree_seeds_canonical_only_nested_to_runtime() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let claude = tmp.path().join("claude");
    let runtime = tmp.path().join("runtime");
    fs::create_dir_all(claude.join("projects").join("alpha")).expect("mkdir canonical nested");
    fs::create_dir_all(&runtime).expect("mkdir runtime");
    fs::write(
        claude.join("projects").join("alpha").join("notes.json"),
        br#"{"note":"hi"}"#,
    )
    .expect("write canonical");

    mirror_tree(&claude, &runtime).expect("mirror");
    assert_eq!(
        fs::read(runtime.join("projects").join("alpha").join("notes.json")).expect("read"),
        br#"{"note":"hi"}"#
    );
}

#[test]
fn copy_file_overwrites_existing_destination() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let src = tmp.path().join("src.json");
    let dst = tmp.path().join("dst.json");
    fs::write(&src, b"new bytes").expect("write src");
    fs::write(&dst, b"old bytes").expect("write dst");

    copy_file(&src, &dst).expect("copy_file");
    assert_eq!(fs::read(&dst).expect("read dst"), b"new bytes");
}

#[test]
fn copy_file_creates_missing_parent_dirs() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let src = tmp.path().join("src.json");
    let dst = tmp.path().join("nested").join("deeper").join("dst.json");
    fs::write(&src, b"payload").expect("write src");

    copy_file(&src, &dst).expect("copy_file");
    assert_eq!(fs::read(&dst).expect("read dst"), b"payload");
}

#[test]
fn copy_file_leaves_no_tmp_artifact() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let src = tmp.path().join("src.json");
    let dst = tmp.path().join("dst.json");
    fs::write(&src, b"payload").expect("write src");

    copy_file(&src, &dst).expect("copy_file");

    // `.<name>.tmp.<pid>` sidecar must be renamed away after atomic write
    let stray = tmp
        .path()
        .join(format!(".dst.json.tmp.{}", std::process::id()));
    assert!(!stray.exists(), "atomic copy must not leave a tmp file");
}

// A racing reader must never see a torn file — only old or complete-new bytes.
// This is the invariant that lets mirror_tree run lockless: rename is the
// atomicity boundary. A non-atomic copy (truncate-then-stream) would fail this.
#[test]
fn copy_file_visible_state_is_never_torn() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    let tmp = tempfile::tempdir().expect("tempdir");
    let src = tmp.path().join("src.json");
    let dst = Arc::new(tmp.path().join("dst.json"));

    let old = vec![b'a'; 64 * 1024];
    let new = vec![b'b'; 64 * 1024];
    fs::write(&src, &new).expect("write src");
    fs::write(dst.as_ref(), &old).expect("seed dst");

    let stop = Arc::new(AtomicBool::new(false));
    let reader_dst = dst.clone();
    let reader_stop = stop.clone();
    let old_clone = old.clone();
    let new_clone = new.clone();
    let reader = std::thread::spawn(move || {
        while !reader_stop.load(Ordering::Relaxed) {
            // mid-rename: path may not resolve; any successful read must be old or complete-new
            if let Ok(bytes) = fs::read(reader_dst.as_ref()) {
                assert!(
                    bytes == old_clone || bytes == new_clone,
                    "reader observed a torn file ({} bytes)",
                    bytes.len()
                );
            }
        }
    });

    for _ in 0..200 {
        copy_file(&src, &dst).expect("copy_file");
    }
    stop.store(true, Ordering::Relaxed);
    reader.join().expect("reader panicked");
    assert_eq!(fs::read(dst.as_ref()).expect("final read"), new);
}

#[test]
fn detect_link_mode_returns_real_on_unix() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let mode = detect_link_mode(tmp.path()).expect("detect");
    #[cfg(unix)] // Unix CI always grants symlinks; Windows depends on dev mode
    assert_eq!(mode, LinkMode::Real);
    #[cfg(not(unix))]
    let _ = mode;
}

// ── HOME-mutating tests ────────────────────────────────────────────────────────

/// Redirect `home_dir()` into `root` for the duration of `f`, serialized on
/// `profile::HOME_TEST_LOCK`. Uses the process-global `HOME_OVERRIDE` rather
/// than `$HOME` so resolution matches on Windows too, where `dirs::home_dir()`
/// reads `USERPROFILE`, not `HOME`. The override is cleared on drop so a
/// panicking test can't leak it into the next test.
fn with_fake_home<T>(root: &Path, f: impl FnOnce() -> T) -> T {
    let _lock = crate::profile::HOME_TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    struct ClearOnDrop;
    impl Drop for ClearOnDrop {
        fn drop(&mut self) {
            crate::profile::clear_home_override();
        }
    }
    crate::profile::set_home_override(root.to_path_buf());
    let _clear = ClearOnDrop;
    f()
}

/// Build `~/.claude/` (required by `acquire`).
fn fake_claude_home(root: &Path) -> PathBuf {
    let claude = root.join(".claude");
    fs::create_dir_all(&claude).expect("mkdir .claude");
    claude
}

fn make_profile(name: &str) -> crate::profile::Profile {
    crate::profile::Profile::new(name.to_string(), None, None)
}

#[test]
fn build_runtime_dir_writes_settings_not_symlink() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let claude_home = fake_claude_home(tmp.path());
        fs::write(
            claude_home.join("settings.json"),
            br#"{"env":{"EXISTING":"1"}}"#,
        )
        .expect("write settings");
        let runtime = tmp.path().join("runtime");
        fs::create_dir_all(&runtime).expect("mkdir runtime");
        let profile = make_profile("test");
        let canonical = tmp.path().join("creds.json");

        build_runtime_dir(
            &runtime,
            &claude_home,
            &profile,
            &canonical,
            LinkMode::Fake,
            Isolation::Shared,
        )
        .expect("build");

        let settings_dst = runtime.join("settings.json");
        let meta = settings_dst.symlink_metadata().expect("settings present");
        assert!(
            !meta.file_type().is_symlink(),
            "settings.json must not be a symlink"
        );

        let expected =
            build_claude_settings_json(Some(&claude_home.join("settings.json")), &profile, &[])
                .expect("build_claude_settings_json");
        let actual = fs::read_to_string(&settings_dst).expect("read settings");
        assert_eq!(actual, expected);
    });
}

#[test]
fn build_runtime_dir_strips_active_env_from_another_profile() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let claude_home = fake_claude_home(tmp.path());
        // Live settings carry the active profile's custom env (`FOO`) plus an
        // operator-owned key (`KEEP`) that must survive every switch/start.
        fs::write(
            claude_home.join("settings.json"),
            br#"{"env":{"FOO":"active","KEEP":"mine"}}"#,
        )
        .expect("write settings");
        let runtime = tmp.path().join("runtime");
        fs::create_dir_all(&runtime).expect("mkdir runtime");
        let target = make_profile("target");
        let canonical = tmp.path().join("creds.json");

        build_runtime_dir_with_active_env(
            &runtime,
            &claude_home,
            &target,
            &canonical,
            LinkMode::Fake,
            Isolation::Shared,
            &["FOO".to_string()],
        )
        .expect("build");

        let settings: serde_json::Value =
            serde_json::from_slice(&fs::read(runtime.join("settings.json")).expect("read"))
                .expect("parse");
        assert!(
            settings["env"].get("FOO").is_none(),
            "active profile's custom env must not leak into another profile's runtime"
        );
        assert_eq!(
            settings["env"]["KEEP"],
            serde_json::json!("mine"),
            "operator env inherited untouched"
        );
    });
}

#[test]
fn build_runtime_dir_active_env_strip_is_noop_when_target_is_active() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let claude_home = fake_claude_home(tmp.path());
        fs::write(
            claude_home.join("settings.json"),
            br#"{"env":{"FOO":"active"}}"#,
        )
        .expect("write settings");
        let runtime = tmp.path().join("runtime");
        fs::create_dir_all(&runtime).expect("mkdir runtime");
        let mut target = make_profile("target");
        target.env.insert("FOO".into(), "active".into());
        let canonical = tmp.path().join("creds.json");

        build_runtime_dir_with_active_env(
            &runtime,
            &claude_home,
            &target,
            &canonical,
            LinkMode::Fake,
            Isolation::Shared,
            &["FOO".to_string()],
        )
        .expect("build");

        let settings: serde_json::Value =
            serde_json::from_slice(&fs::read(runtime.join("settings.json")).expect("read"))
                .expect("parse");
        assert_eq!(
            settings["env"]["FOO"],
            serde_json::json!("active"),
            "starting the active profile itself keeps its own env (strip is a no-op)"
        );
    });
}

#[test]
fn build_runtime_dir_credentials_not_from_claude_home() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let claude_home = fake_claude_home(tmp.path());
        // ~/.claude/.credentials.json must NOT appear in runtime
        fs::write(claude_home.join(".credentials.json"), CREDS_V1).expect("write creds");
        let runtime = tmp.path().join("runtime");
        fs::create_dir_all(&runtime).expect("mkdir runtime");
        let profile = make_profile("test");
        let canonical = tmp.path().join("profile-creds.json"); // no canonical → runtime creds absent

        build_runtime_dir(
            &runtime,
            &claude_home,
            &profile,
            &canonical,
            LinkMode::Fake,
            Isolation::Shared,
        )
        .expect("build");

        let runtime_creds = runtime.join(".credentials.json");
        assert!(
            !runtime_creds.exists(),
            ".credentials.json from ~/.claude/ must not be copied into runtime"
        );
    });
}

#[test]
fn build_runtime_dir_fake_preserves_live_runtime_credentials() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let claude_home = fake_claude_home(tmp.path());
        let runtime = tmp.path().join("runtime");
        fs::create_dir_all(&runtime).expect("mkdir runtime");
        let profile = make_profile("test");
        let canonical = tmp.path().join("profile-creds.json");
        let runtime_creds = runtime.join(".credentials.json");
        fs::write(&canonical, CREDS_V1).expect("write canonical");
        fs::write(&runtime_creds, CREDS_V2).expect("write runtime credentials");
        let past = SystemTime::now() - Duration::from_secs(60);
        let now = SystemTime::now();
        set_mtime(&canonical, past);
        set_mtime(&runtime_creds, now);

        build_runtime_dir(
            &runtime,
            &claude_home,
            &profile,
            &canonical,
            LinkMode::Fake,
            Isolation::Shared,
        )
        .expect("build");

        assert_eq!(fs::read(&canonical).expect("read canonical"), CREDS_V2);
        assert_eq!(fs::read(&runtime_creds).expect("read runtime"), CREDS_V2);
    });
}

#[cfg(unix)]
#[test]
fn build_runtime_dir_real_preserves_live_runtime_credentials() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let claude_home = fake_claude_home(tmp.path());
        let runtime = tmp.path().join("runtime");
        fs::create_dir_all(&runtime).expect("mkdir runtime");
        let profile = make_profile("test");
        let canonical = tmp.path().join("profile-creds.json");
        let runtime_creds = runtime.join(".credentials.json");
        fs::write(&canonical, CREDS_V1).expect("write canonical");
        fs::write(&runtime_creds, CREDS_V2).expect("write runtime credentials");

        build_runtime_dir(
            &runtime,
            &claude_home,
            &profile,
            &canonical,
            LinkMode::Real,
            Isolation::Shared,
        )
        .expect("build");

        assert_eq!(fs::read(&canonical).expect("read canonical"), CREDS_V2);
        assert!(
            runtime_creds
                .symlink_metadata()
                .expect("runtime credentials meta")
                .file_type()
                .is_symlink()
        );
    });
}

#[cfg(unix)]
#[test]
fn build_runtime_dir_real_keeps_invalid_runtime_credentials_for_retry() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let claude_home = fake_claude_home(tmp.path());
        let runtime = tmp.path().join("runtime");
        fs::create_dir_all(&runtime).expect("mkdir runtime");
        let profile = make_profile("test");
        let canonical = tmp.path().join("profile-creds.json");
        let runtime_creds = runtime.join(".credentials.json");
        fs::write(&canonical, CREDS_V1).expect("write canonical");
        fs::write(&runtime_creds, b"partial write").expect("write runtime credentials");

        build_runtime_dir(
            &runtime,
            &claude_home,
            &profile,
            &canonical,
            LinkMode::Real,
            Isolation::Shared,
        )
        .expect("build");

        assert_eq!(fs::read(&canonical).expect("read canonical"), CREDS_V1);
        assert_eq!(
            fs::read(&runtime_creds).expect("read runtime"),
            b"partial write"
        );
    });
}

#[test]
fn build_runtime_dir_other_entries_materialized() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let claude_home = fake_claude_home(tmp.path());
        // A few ordinary entries that should be mirrored.
        fs::create_dir_all(claude_home.join("projects")).expect("mkdir projects");
        fs::write(claude_home.join("history.jsonl"), b"{}").expect("write history");
        let runtime = tmp.path().join("runtime");
        fs::create_dir_all(&runtime).expect("mkdir runtime");
        let profile = make_profile("test");
        let canonical = tmp.path().join("creds.json");

        build_runtime_dir(
            &runtime,
            &claude_home,
            &profile,
            &canonical,
            LinkMode::Fake,
            Isolation::Shared,
        )
        .expect("build");

        assert!(runtime.join("projects").is_dir(), "projects dir copied"); // Fake mode: copied, not symlinked
        assert!(
            runtime.join("history.jsonl").exists(),
            "history.jsonl copied"
        );
    });
}

#[cfg(unix)]
#[test]
fn build_runtime_dir_other_entries_symlinked_on_unix() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let claude_home = fake_claude_home(tmp.path());
        fs::write(claude_home.join("todos.json"), b"[]").expect("write todos");
        let runtime = tmp.path().join("runtime");
        fs::create_dir_all(&runtime).expect("mkdir runtime");
        let profile = make_profile("test");
        let canonical = tmp.path().join("creds.json");

        build_runtime_dir(
            &runtime,
            &claude_home,
            &profile,
            &canonical,
            LinkMode::Real,
            Isolation::Shared,
        )
        .expect("build");

        let dst = runtime.join("todos.json");
        assert!(
            dst.symlink_metadata()
                .expect("todos present")
                .file_type()
                .is_symlink(),
            "todos.json should be a symlink in Real mode"
        );
    });
}

#[test]
fn build_runtime_dir_links_claude_json_from_parent() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let claude_home = fake_claude_home(tmp.path());
        // ~/.claude.json sits next to ~/.claude/, not inside it
        fs::write(tmp.path().join(".claude.json"), br#"{"userId":"u1"}"#)
            .expect("write .claude.json");
        let runtime = tmp.path().join("runtime");
        fs::create_dir_all(&runtime).expect("mkdir runtime");
        let profile = make_profile("test");
        let canonical = tmp.path().join("creds.json");

        build_runtime_dir(
            &runtime,
            &claude_home,
            &profile,
            &canonical,
            LinkMode::Fake,
            Isolation::Shared,
        )
        .expect("build");

        let dst = runtime.join(".claude.json");
        assert!(dst.exists(), ".claude.json must appear in runtime");
        assert_eq!(
            fs::read(&dst).expect("read"),
            br#"{"userId":"u1"}"#,
            "content must match source"
        );
    });
}

/// `runtime/settings.json` carries clauth-owned credential routing for an
/// api-key profile (top-level `apiKeyHelper` naming the profile, plus the
/// base_url and model env keys), so it is a credential file and must land
/// 0o600 like every other clauth-owned write. The raw key is NOT in this file
/// (it lives in `config.toml`, minted per request by the helper) — but the
/// helper string and the surrounding env are still operator-sensitive, so the
/// perm invariant is unchanged from the pre-helper era. The seeded
/// `.claude.json` rides the same rule.
#[cfg(unix)]
#[test]
fn runtime_settings_and_seed_are_owner_only() {
    use std::os::unix::fs::PermissionsExt;

    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let claude_home = fake_claude_home(tmp.path());
        fs::write(claude_home.join("settings.json"), br#"{}"#).expect("write settings");
        fs::write(tmp.path().join(".claude.json"), br#"{"numStartups":1}"#).expect("write global");
        let runtime = tmp.path().join("runtime");
        fs::create_dir_all(&runtime).expect("mkdir runtime");
        let profile = crate::profile::Profile::new(
            "keyed".to_string(),
            Some("https://api.example.com".to_string()),
            Some("sk-secret-key".to_string()),
        );

        build_runtime_dir(
            &runtime,
            &claude_home,
            &profile,
            &tmp.path().join("creds.json"),
            LinkMode::Fake,
            Isolation::Shared,
        )
        .expect("build");

        let settings = runtime.join("settings.json");
        let settings_bytes = fs::read_to_string(&settings).expect("read settings");
        assert!(
            settings_bytes.contains("apiKeyHelper"),
            "precondition: the apiKeyHelper wiring is in this file (got: {settings_bytes})"
        );
        assert!(
            !settings_bytes.contains("sk-secret-key"),
            "the raw api key must NOT be in this file — only the helper command string"
        );
        let mode = fs::metadata(&settings).expect("meta").permissions().mode();
        assert_eq!(
            mode & 0o777,
            0o600,
            "runtime settings.json holds the api key; mode should be 0o600, got {:#o}",
            mode & 0o777,
        );
        let seed_mode = fs::metadata(runtime.join(".claude.json"))
            .expect("meta")
            .permissions()
            .mode();
        assert_eq!(
            seed_mode & 0o777,
            0o600,
            "seeded .claude.json mode should be 0o600, got {:#o}",
            seed_mode & 0o777,
        );
    });
}

/// A settings.json an older build left at 0o644 keeps its bytes forever once
/// the profile stops changing, so a byte-only write gate would never retighten
/// it. The gate has to see the mode too.
#[cfg(unix)]
#[test]
fn runtime_settings_retightens_a_loose_file_with_current_bytes() {
    use std::os::unix::fs::PermissionsExt;

    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let claude_home = fake_claude_home(tmp.path());
        fs::write(claude_home.join("settings.json"), br#"{}"#).expect("write settings");
        let runtime = tmp.path().join("runtime");
        fs::create_dir_all(&runtime).expect("mkdir runtime");
        let profile = crate::profile::Profile::new(
            "keyed".to_string(),
            Some("https://api.example.com".to_string()),
            Some("sk-secret-key".to_string()),
        );

        // Byte-identical to what the merge produces, at the old umask mode.
        let current =
            build_claude_settings_json(Some(&claude_home.join("settings.json")), &profile, &[])
                .expect("build_claude_settings_json");
        let settings = runtime.join("settings.json");
        fs::write(&settings, &current).expect("write legacy settings");
        fs::set_permissions(&settings, fs::Permissions::from_mode(0o644)).expect("chmod");

        write_merged_settings(&runtime, &claude_home, &profile, Isolation::Shared, &[])
            .expect("write_merged_settings");

        let mode = fs::metadata(&settings).expect("meta").permissions().mode();
        assert_eq!(
            mode & 0o777,
            0o600,
            "a 0o644 settings.json from an older build must be retightened, got {:#o}",
            mode & 0o777,
        );
        assert_eq!(
            fs::read_to_string(&settings).expect("read"),
            current,
            "content must be unchanged by the mode repair"
        );
    });
}

/// Issue #17 systemic finding: a raw copy is born carrying whichever account
/// was active at seed time, wrong for every non-active profile. Seeding must
/// strip it so the fresh runtime starts identity-less and Claude Code
/// re-derives it from THIS profile's own credentials.
#[test]
fn seed_claude_json_strips_oauth_account_from_fresh_member() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let claude_home = fake_claude_home(tmp.path());
        fs::write(
            tmp.path().join(".claude.json"),
            br#"{"oauthAccount":{"emailAddress":"active@x"},"numStartups":4}"#,
        )
        .expect("write global .claude.json");
        let runtime = tmp.path().join("runtime");
        fs::create_dir_all(&runtime).expect("mkdir runtime");

        seed_claude_json(&runtime, &claude_home).expect("seed");

        let dst = runtime.join(".claude.json");
        let seeded: serde_json::Value =
            serde_json::from_slice(&fs::read(&dst).expect("read seeded")).expect("parse");
        assert!(
            seeded.get("oauthAccount").is_none(),
            "a freshly seeded runtime copy must not inherit the active profile's identity"
        );
        assert_eq!(seeded["numStartups"], serde_json::json!(4));
    });
}

/// A profile whose runtime already has its own real `.claude.json` (its own
/// prior login wrote a genuine identity) must keep it — seeding only applies
/// to a missing file or a leftover shared symlink, never to an existing copy.
#[test]
fn seed_claude_json_leaves_existing_real_copy_untouched() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let claude_home = fake_claude_home(tmp.path());
        fs::write(
            tmp.path().join(".claude.json"),
            br#"{"oauthAccount":{"emailAddress":"active@x"},"numStartups":4}"#,
        )
        .expect("write global .claude.json");
        let runtime = tmp.path().join("runtime");
        fs::create_dir_all(&runtime).expect("mkdir runtime");
        let dst = runtime.join(".claude.json");
        let own: &[u8] = br#"{"oauthAccount":{"emailAddress":"own@x"},"numStartups":1}"#;
        fs::write(&dst, own).expect("write existing runtime copy");

        seed_claude_json(&runtime, &claude_home).expect("seed");

        assert_eq!(
            fs::read(&dst).expect("read"),
            own,
            "an existing real copy already has its own identity and must not be reseeded"
        );
    });
}

#[test]
fn has_live_session_false_when_no_sessions_dir() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        assert!(!has_live_session("ghost")); // no sessions dir → false, not error
    });
}

#[test]
fn has_live_session_false_when_sessions_dir_empty() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let sessions = tmp
            .path()
            .join(".clauth")
            .join("profiles")
            .join("empty")
            .join("sessions");
        fs::create_dir_all(&sessions).expect("mkdir sessions");
        assert!(!has_live_session("empty"));
    });
}

#[test]
fn has_live_session_false_when_all_sessions_dead() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let sessions = tmp
            .path()
            .join(".clauth")
            .join("profiles")
            .join("dead")
            .join("sessions");
        fs::create_dir_all(&sessions).expect("mkdir sessions");
        fs::write(sessions.join("99999"), b"").expect("write dead pid"); // unlocked file = dead
        assert!(!has_live_session("dead"));
    });
}

#[test]
fn has_live_session_true_when_any_session_alive() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let sessions = tmp
            .path()
            .join(".clauth")
            .join("profiles")
            .join("alive")
            .join("sessions");
        fs::create_dir_all(&sessions).expect("mkdir sessions");
        let pid_path = sessions.join("12345");
        let file = open_pid_file(&pid_path).expect("open pid");
        file.lock().expect("lock pid");
        assert!(has_live_session("alive"));
        drop(file);
        // The probe is deliberately fail-alive (any try_lock I/O error reads
        // as "alive" — see `is_session_alive`), so one transient error under a
        // parallel suite run can inflate a single reading. Poll briefly: only
        // a PERSISTENTLY-alive reading is a regression. Same hardening as
        // `live_session_count_counts_only_alive`.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let settled_dead = loop {
            let alive = has_live_session("alive");
            if !alive {
                break true;
            }
            if std::time::Instant::now() >= deadline {
                break false;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        };
        assert!(settled_dead, "a dropped session lock must read as dead");
    });
}

#[test]
fn has_live_session_true_with_mixed_alive_and_dead() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let sessions = tmp
            .path()
            .join(".clauth")
            .join("profiles")
            .join("mixed")
            .join("sessions");
        fs::create_dir_all(&sessions).expect("mkdir sessions");
        fs::write(sessions.join("11111"), b"").expect("write dead pid"); // dead
        let live_path = sessions.join("22222"); // live
        let file = open_pid_file(&live_path).expect("open live pid");
        file.lock().expect("lock live pid");
        assert!(has_live_session("mixed"));
        drop(file);
    });
}

#[test]
fn live_session_count_counts_only_alive() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let sessions = tmp
            .path()
            .join(".clauth")
            .join("profiles")
            .join("counted")
            .join("sessions");
        fs::create_dir_all(&sessions).expect("mkdir sessions");
        fs::write(sessions.join("11111"), b"").expect("write dead pid"); // dead
        let a = open_pid_file(&sessions.join("22222")).expect("open a");
        a.lock().expect("lock a");
        let b = open_pid_file(&sessions.join("33333")).expect("open b");
        b.lock().expect("lock b");
        // The probe is deliberately fail-alive (any try_lock I/O error reads
        // as "alive" — see `is_session_alive`), so one transient error under a
        // parallel suite run can inflate a single reading. Poll briefly: only
        // a PERSISTENT wrong count is a regression.
        let settled = |expect: usize| {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            loop {
                let n = live_session_count("counted");
                if n == expect || std::time::Instant::now() >= deadline {
                    return n;
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        };
        assert_eq!(settled(2), 2);
        drop(a);
        assert_eq!(settled(1), 1);
        assert_eq!(live_session_count("ghost"), 0); // no sessions dir → zero
    });
}

#[test]
fn acquire_creates_runtime_and_pid_file() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        fake_claude_home(tmp.path());
        let profile = make_profile("lifecycle");

        let rt = ProfileRuntime::acquire(&profile, Isolation::Shared, &[]).expect("acquire");

        assert!(
            rt.config_dir().is_dir(),
            "runtime dir must exist after acquire"
        );

        let sessions = tmp
            .path()
            .join(".clauth")
            .join("profiles")
            .join("lifecycle")
            .join("sessions");
        let session_files: Vec<PathBuf> = fs::read_dir(&sessions)
            .expect("read sessions")
            .flatten()
            .map(|e| e.path())
            .collect();
        assert_eq!(session_files.len(), 1, "exactly one PID file");
        let pid_file = &session_files[0];
        assert!(
            pid_file
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with(&format!("{}-", std::process::id()))),
            "session file must carry the `<pid>-` prefix"
        );
        assert!(
            is_session_alive(pid_file),
            "PID file must be flock-held while runtime is alive"
        );

        let expected_runtime = tmp
            .path()
            .join(".clauth")
            .join("profiles")
            .join("lifecycle")
            .join("runtime");
        assert_eq!(rt.config_dir(), expected_runtime);

        assert!(
            rt.config_dir().join("settings.json").exists(),
            "settings.json must be written"
        );

        drop(rt);

        assert!(
            !expected_runtime.exists(),
            "runtime dir torn down on last-session drop"
        );
        assert!(
            !sessions.exists(),
            "sessions dir removed when no live siblings remain"
        );
    });
}

/// Black-box `clauth start` isolation: a full `acquire` must build the runtime
/// tree from the profile's OWN canonical credentials and never leak the live
/// `~/.claude/.credentials.json` (a different account's tokens) into it. Also
/// pins that `acquire` leaves the real home's credential file untouched.
#[test]
fn acquire_isolates_credentials_from_real_home() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let claude_home = fake_claude_home(tmp.path());
        // The real `~/.claude/.credentials.json` belongs to a DIFFERENT account
        // (a "wrong" chain). Isolation means it must never reach the runtime.
        let live_creds = claude_home.join(".credentials.json");
        fs::write(&live_creds, CREDS_V1).expect("write live creds");

        // Pre-stage the profile's own canonical credentials (what `clauth start`
        // restores for this profile) with a DISTINCT token chain.
        let profile = make_profile("isolated");
        let canonical = tmp
            .path()
            .join(".clauth")
            .join("profiles")
            .join("isolated")
            .join("credentials.json");
        fs::create_dir_all(canonical.parent().expect("canonical parent"))
            .expect("mkdir profile dir");
        fs::write(&canonical, CREDS_V2).expect("write canonical");

        let rt = ProfileRuntime::acquire(&profile, Isolation::Shared, &[]).expect("acquire");
        let runtime_creds = rt.config_dir().join(".credentials.json");

        // The runtime's credentials resolve to the profile's OWN chain (V2),
        // not the live wrong-account chain (V1). On Unix this is a symlink into
        // canonical; either way the resolved bytes must be the profile's.
        assert_eq!(
            fs::read(&runtime_creds).expect("read runtime creds"),
            CREDS_V2,
            "runtime must carry the profile's canonical chain, not the live one"
        );
        assert_ne!(
            fs::read(&runtime_creds).expect("read runtime creds"),
            CREDS_V1,
            "the live ~/.claude chain must never leak into the runtime"
        );

        // The real home's credential file is untouched by the launch.
        assert_eq!(
            fs::read(&live_creds).expect("read live creds"),
            CREDS_V1,
            "acquire must not overwrite the real ~/.claude/.credentials.json"
        );

        // settings.json is a per-profile rewrite, never a symlink into the
        // shared home — the isolation boundary for env/base-url too.
        let settings = rt.config_dir().join("settings.json");
        assert!(
            !settings
                .symlink_metadata()
                .expect("settings present")
                .file_type()
                .is_symlink(),
            "runtime settings.json must be a per-profile copy, not a shared symlink"
        );

        drop(rt);
    });
}

/// Regression: one process holding two concurrent sessions of the same
/// profile+flavor must not collide on the session file. Before the per-acquire
/// `-<n>` suffix both keyed `sessions/<pid>`, so the second `acquire` blocked
/// forever on the first's `flock(2)` — the background-`delegate` hang where a
/// second same-profile job never spawned a session. Both must register live,
/// and teardown must wait for the last drop.
#[test]
fn acquire_twice_same_process_counts_two_sessions() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        fake_claude_home(tmp.path());
        let profile = make_profile("concurrent");

        let rt1 = ProfileRuntime::acquire(&profile, Isolation::Shared, &[]).expect("first acquire");
        // Pre-fix this second acquire blocks forever on the shared PID flock.
        let rt2 =
            ProfileRuntime::acquire(&profile, Isolation::Shared, &[]).expect("second acquire");

        assert_eq!(
            live_session_count("concurrent"),
            2,
            "two concurrent same-process sessions must both register live"
        );

        let runtime = tmp
            .path()
            .join(".clauth")
            .join("profiles")
            .join("concurrent")
            .join("runtime");

        drop(rt2);
        assert!(
            runtime.exists(),
            "runtime must survive while a sibling session is still live"
        );
        assert_eq!(live_session_count("concurrent"), 1);

        drop(rt1);
        assert!(
            !runtime.exists(),
            "runtime torn down once the last session drops"
        );
    });
}

/// `build_runtime_dir` re-walk must pick up entries added between two acquires.
/// Drives `build_runtime_dir` directly to isolate the re-walk from the rest of
/// the acquire path (watchdog spawn, flock, teardown).
#[test]
fn build_runtime_dir_rewalk_picks_up_late_entries() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let claude_home = fake_claude_home(tmp.path());
        fs::write(claude_home.join("existing.txt"), b"v1").expect("write existing");
        let runtime = tmp.path().join("runtime");
        fs::create_dir_all(&runtime).expect("mkdir runtime");
        let profile = make_profile("rewalk");
        let canonical = tmp.path().join("creds.json");

        build_runtime_dir(
            &runtime,
            &claude_home,
            &profile,
            &canonical,
            LinkMode::Fake,
            Isolation::Shared,
        )
        .expect("first build");
        assert!(
            runtime.join("existing.txt").exists(),
            "first build: existing.txt present"
        );

        fs::write(claude_home.join("late_entry.txt"), b"new").expect("write late entry");

        // second build (second session's acquire) — late entry must appear
        build_runtime_dir(
            &runtime,
            &claude_home,
            &profile,
            &canonical,
            LinkMode::Fake,
            Isolation::Shared,
        )
        .expect("second build");
        assert!(
            runtime.join("late_entry.txt").exists(),
            "second build must pick up late_entry.txt"
        );
        assert!(
            // re-walk is additive, not destructive
            runtime.join("existing.txt").exists(),
            "second build must preserve existing.txt"
        );
    });
}

/// A second live session must prevent teardown. Drives `prune_stale_sessions`
/// on hand-placed flock files to test the count logic in isolation.
#[test]
fn prune_with_two_live_sessions_returns_two() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let sessions = tmp.path().join("sessions");
    fs::create_dir_all(&sessions).expect("mkdir sessions");

    let pid1 = sessions.join("100001");
    let pid2 = sessions.join("100002");
    let f1 = open_pid_file(&pid1).expect("open pid1");
    f1.lock().expect("lock pid1");
    let f2 = open_pid_file(&pid2).expect("open pid2");
    f2.lock().expect("lock pid2");

    let count = prune_stale_sessions(&sessions).expect("prune");
    assert_eq!(count, 2, "both live sessions must be counted");

    drop(f2);
    let count = prune_stale_sessions(&sessions).expect("prune after drop f2");
    assert_eq!(count, 1, "one live session after f2 dropped");
    assert!(!pid2.exists(), "dead session file removed");

    drop(f1);
    let count = prune_stale_sessions(&sessions).expect("prune after drop f1");
    assert_eq!(count, 0, "no live sessions after both dropped");
    assert!(!pid1.exists(), "dead session file removed");
}

// ── sync_credentials_unlocked concurrent contention (Unix) ───────────────────
//
// Two barrier-synchronized threads call sync on the same link_path (same
// PID-suffixed tmp). Regardless of which wins the rename race, end state must
// be consistent: link_path is a symlink, canonical holds the right bytes, no
// dangling tmp.

#[cfg(unix)]
#[test]
fn sync_credentials_unlocked_concurrent_same_link_consistent_end_state() {
    use std::sync::{Arc, Barrier};

    let tmp = tempfile::tempdir().expect("tempdir");
    let canonical = Arc::new(tmp.path().join("canonical.json"));
    let link_path = Arc::new(tmp.path().join(".credentials.json"));

    fs::write(link_path.as_ref(), CREDS_V1).expect("write link");

    let barrier = Arc::new(Barrier::new(2));

    let b1 = barrier.clone();
    let ca1 = canonical.clone();
    let lp1 = link_path.clone();
    let t1 = std::thread::spawn(move || {
        b1.wait();
        sync_credentials_unlocked(&lp1, &ca1)
    });

    let b2 = barrier.clone();
    let ca2 = canonical.clone();
    let lp2 = link_path.clone();
    let t2 = std::thread::spawn(move || {
        b2.wait();
        sync_credentials_unlocked(&lp2, &ca2)
    });

    // one or both may error (same-PID tmp collision); end state is what matters
    let _ = t1.join().expect("thread 1 panicked");
    let _ = t2.join().expect("thread 2 panicked");

    // rename is atomic on POSIX — at least one thread wins; link_path must be a symlink
    assert!(
        link_path
            .symlink_metadata()
            .expect("link_path must exist")
            .file_type()
            .is_symlink(),
        "link_path must be a symlink after concurrent sync"
    );

    assert_eq!(
        fs::read(canonical.as_ref()).expect("read canonical"),
        CREDS_V1,
        "canonical must hold link content"
    );

    let tmp_name =
        link_path.with_file_name(format!(".credentials.json.tmp.{}", std::process::id()));
    assert!(
        !tmp_name.exists(),
        "PID-suffixed tmp must not persist after sync completes"
    );
}

// ── isolated runtime layout ──────────────────────────────────────────────────

/// Isolated mode omits operator memory/plugins/hooks but keeps account state.
#[test]
fn build_runtime_dir_isolated_omits_operator_extensions() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let claude_home = fake_claude_home(tmp.path());
        fs::write(claude_home.join("CLAUDE.md"), b"# operator memory").expect("write memory");
        fs::create_dir_all(claude_home.join("plugins")).expect("mkdir plugins");
        fs::create_dir_all(claude_home.join("hooks")).expect("mkdir hooks");
        fs::create_dir_all(claude_home.join("commands")).expect("mkdir commands");
        // Writable operator state that MUST NOT be shared: an isolated session's CC
        // (empty settings → default 30-day cleanupPeriodDays) would otherwise delete
        // the operator's transcripts through a shared `projects/` symlink.
        fs::write(claude_home.join("history.jsonl"), b"{}").expect("write history");
        fs::create_dir_all(claude_home.join("projects")).expect("mkdir projects");
        fs::write(claude_home.join("stats-cache.json"), b"{}").expect("write stats");
        let runtime = tmp.path().join("runtime-isolated");
        fs::create_dir_all(&runtime).expect("mkdir runtime");
        let profile = make_profile("iso");
        let canonical = tmp.path().join("creds.json");

        build_runtime_dir(
            &runtime,
            &claude_home,
            &profile,
            &canonical,
            LinkMode::Fake,
            Isolation::Isolated,
        )
        .expect("build");

        // Skip-all: no operator house style AND no writable store is linked.
        for omitted in [
            "CLAUDE.md",
            "plugins",
            "hooks",
            "commands",
            "history.jsonl",
            "projects",
            "stats-cache.json",
        ] {
            assert!(
                !runtime.join(omitted).exists(),
                "isolated runtime must omit `{omitted}` (no shared writable state)"
            );
        }
        assert!(
            runtime.join("settings.json").exists(),
            "settings.json still written"
        );
    });
}

/// Shared mode keeps the same entries isolated mode strips — the control case.
#[test]
fn build_runtime_dir_shared_keeps_operator_extensions() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let claude_home = fake_claude_home(tmp.path());
        fs::write(claude_home.join("CLAUDE.md"), b"# operator memory").expect("write memory");
        fs::create_dir_all(claude_home.join("plugins")).expect("mkdir plugins");
        fs::create_dir_all(claude_home.join("projects")).expect("mkdir projects");
        fs::write(claude_home.join("stats-cache.json"), b"{}").expect("write stats");
        let runtime = tmp.path().join("runtime");
        fs::create_dir_all(&runtime).expect("mkdir runtime");
        let profile = make_profile("shared");
        let canonical = tmp.path().join("creds.json");

        build_runtime_dir(
            &runtime,
            &claude_home,
            &profile,
            &canonical,
            LinkMode::Fake,
            Isolation::Shared,
        )
        .expect("build");

        assert!(runtime.join("CLAUDE.md").exists(), "shared keeps memory");
        assert!(runtime.join("plugins").exists(), "shared keeps plugins");
        // The operator's own session shares the global writable store (project
        // history, aggregate) — the intentional contrast with isolated.
        assert!(runtime.join("projects").exists(), "shared keeps projects");
        assert!(
            runtime.join("stats-cache.json").exists(),
            "shared keeps stats-cache"
        );
    });
}

/// Isolated settings start from an empty base, so operator hooks never leak.
#[test]
fn build_runtime_dir_isolated_settings_drop_operator_config() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let claude_home = fake_claude_home(tmp.path());
        fs::write(
            claude_home.join("settings.json"),
            br#"{"hooks":{"PreToolUse":[]},"statusLine":{"type":"command"},"env":{"OP":"1"}}"#,
        )
        .expect("write settings");
        let runtime = tmp.path().join("runtime-isolated");
        fs::create_dir_all(&runtime).expect("mkdir runtime");
        let profile = make_profile("iso");
        let canonical = tmp.path().join("creds.json");

        build_runtime_dir(
            &runtime,
            &claude_home,
            &profile,
            &canonical,
            LinkMode::Fake,
            Isolation::Isolated,
        )
        .expect("build");

        let raw = fs::read_to_string(runtime.join("settings.json")).expect("read settings");
        let v: serde_json::Value = serde_json::from_str(&raw).expect("parse settings");
        assert!(v.get("hooks").is_none(), "operator hooks dropped");
        assert!(v.get("statusLine").is_none(), "operator statusLine dropped");
        assert!(
            v["env"].get("OP").is_none(),
            "operator env entry dropped (empty base)"
        );
    });
}

/// A dangling top-level symlink (its `~/.claude/` source moved away) is removed
/// on the next build — the reported `runtime/CLAUDE.md.benchbak` leftover.
#[cfg(unix)]
#[test]
fn build_runtime_dir_prunes_dangling_symlink() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let claude_home = fake_claude_home(tmp.path());
        let runtime = tmp.path().join("runtime");
        fs::create_dir_all(&runtime).expect("mkdir runtime");
        // A link left from a prior build whose source no longer exists.
        let dangling = runtime.join("CLAUDE.md.benchbak");
        std::os::unix::fs::symlink(tmp.path().join("gone"), &dangling).expect("symlink");
        assert!(
            dangling.symlink_metadata().is_ok(),
            "link exists (dangling)"
        );
        assert!(!dangling.exists(), "target is gone");

        let profile = make_profile("heal");
        let canonical = tmp.path().join("creds.json");
        build_runtime_dir(
            &runtime,
            &claude_home,
            &profile,
            &canonical,
            LinkMode::Real,
            Isolation::Shared,
        )
        .expect("build");

        assert!(
            dangling.symlink_metadata().is_err(),
            "dangling symlink must be pruned on rebuild"
        );
    });
}

// ── isolation liveness + GC ──────────────────────────────────────────────────

/// An isolated session must register as live so rotation never spends a token
/// it still holds — `has_live_session` unions both flavors.
#[test]
fn has_live_session_sees_isolated_session() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let sessions = tmp
            .path()
            .join(".clauth")
            .join("profiles")
            .join("iso")
            .join("sessions-isolated");
        fs::create_dir_all(&sessions).expect("mkdir isolated sessions");
        let pid = sessions.join("4242");
        let file = open_pid_file(&pid).expect("open pid");
        file.lock().expect("lock pid");
        assert!(has_live_session("iso"), "isolated live session counts");
        assert_eq!(live_session_count("iso"), 1);
        drop(file);
        // The probe is deliberately fail-alive (any try_lock I/O error reads as
        // "alive" — see `is_session_alive`), so transient errors under a
        // parallel suite run (fd pressure) can flip readings for a while. Poll
        // generously — 2s still flaked under the full suite (2026-07-12); only
        // a PERSISTENT "alive" after the lock holder dropped is a regression.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while has_live_session("iso") && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(!has_live_session("iso"));
    });
}

/// GC removes a runtime tree left by a crashed session (no live PID), and never
/// touches one with a live session.
#[test]
fn gc_removes_stale_runtime_but_spares_live() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let profiles = tmp.path().join(".clauth").join("profiles");

        // Stale: a runtime tree with a dead (unlocked) pid file.
        let stale_runtime = profiles.join("stale").join("runtime");
        let stale_sessions = profiles.join("stale").join("sessions");
        fs::create_dir_all(&stale_runtime).expect("mkdir stale runtime");
        fs::create_dir_all(&stale_sessions).expect("mkdir stale sessions");
        fs::write(stale_runtime.join("settings.json"), b"{}").expect("seed runtime");
        fs::write(stale_sessions.join("99999"), b"").expect("dead pid");

        // Live: an isolated runtime with a flock-held pid file.
        let live_runtime = profiles.join("live").join("runtime-isolated");
        let live_sessions = profiles.join("live").join("sessions-isolated");
        fs::create_dir_all(&live_runtime).expect("mkdir live runtime");
        fs::create_dir_all(&live_sessions).expect("mkdir live sessions");
        let held = open_pid_file(&live_sessions.join("1234")).expect("open live pid");
        held.lock().expect("lock live pid");

        gc_stale_runtimes();

        assert!(
            !stale_runtime.exists(),
            "stale runtime with no live session must be collected"
        );
        assert!(
            !stale_sessions.exists(),
            "stale sessions dir cleaned alongside"
        );
        assert!(
            live_runtime.exists(),
            "a live session's runtime must be spared"
        );
        drop(held);
    });
}

#[test]
fn scrub_profile_env_drops_managed_and_active_custom_keys() {
    // `clauth start <B>` from a session running profile A must not inherit A's
    // endpoint/auth/model overrides nor A's custom `[env]`. The target's
    // runtime settings.json re-supplies whichever it defines.
    let mut cmd = std::process::Command::new("claude");
    scrub_profile_env(&mut cmd, &["FOO".to_string()]);

    let envs = crate::testutil::env_overrides(&cmd);
    for key in MANAGED_ENV_KEYS {
        assert_eq!(
            envs.get(*key),
            Some(&None),
            "{key} must be stripped from the inherited env",
        );
    }
    assert_eq!(
        envs.get("FOO"),
        Some(&None),
        "active custom env key must be stripped",
    );
}

#[test]
fn cwd_is_real_home_matches_only_the_sandboxed_home() {
    let sandbox = HomeSandbox::new();
    assert!(cwd_is_real_home(sandbox.home()));

    let elsewhere = sandbox.home().join("repos").join("some-project");
    fs::create_dir_all(&elsewhere).expect("create project dir");
    assert!(!cwd_is_real_home(&elsewhere));
}

#[test]
fn guard_home_project_settings_appends_setting_sources_only_at_home() {
    let sandbox = HomeSandbox::new();

    let mut at_home = std::process::Command::new("claude");
    guard_home_project_settings(&mut at_home, sandbox.home());
    let args: Vec<_> = at_home
        .get_args()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        args,
        vec!["--setting-sources".to_string(), "user".to_string()],
        "cwd == $HOME must force the user-only settings tier"
    );

    let elsewhere = sandbox.home().join("repos").join("some-project");
    fs::create_dir_all(&elsewhere).expect("create project dir");
    let mut in_project = std::process::Command::new("claude");
    guard_home_project_settings(&mut in_project, &elsewhere);
    assert!(
        in_project.get_args().next().is_none(),
        "a normal project cwd must keep reading its own project settings"
    );
}

// ---------------------------------------------------------------------------
// CDX-1b: CodexRuntime — isolated CODEX_HOME with lease + adopt-back
// ---------------------------------------------------------------------------

mod codex_runtime {
    use super::super::*;
    use crate::testutil::HomeSandbox;

    fn codex_auth(access: &str, account: &str) -> Vec<u8> {
        serde_json::json!({
            "auth_mode": "chatgpt",
            "tokens": {
                "access_token": access,
                "refresh_token": format!("rt-{access}"),
                "account_id": account,
            },
            "unmodeled": { "keep": 1 },
        })
        .to_string()
        .into_bytes()
    }

    fn seed(name: &str, account: &str) {
        let mut p = crate::testutil::blank_profile(name);
        p.harness = crate::profile::Harness::Codex;
        crate::profile::save_profile(&p).unwrap();
        crate::codex::write_profile_auth(name, &codex_auth("at-seed", account)).unwrap();
    }

    #[test]
    fn acquire_builds_the_isolated_home_and_holds_a_lease() {
        let sandbox = HomeSandbox::new();
        seed("cdx-iso", "acct-iso");
        // A real operator config.toml must be COPIED into the home.
        let codex_dir = sandbox.home().join(".codex");
        std::fs::create_dir_all(&codex_dir).unwrap();
        std::fs::write(codex_dir.join("config.toml"), "model = \"gpt-5.6\"\n").unwrap();

        let runtime = CodexRuntime::acquire("cdx-iso").expect("acquire");
        let home = runtime.codex_home().to_path_buf();
        assert_eq!(
            std::fs::read(home.join("auth.json")).unwrap(),
            codex_auth("at-seed", "acct-iso"),
            "auth.json seeded verbatim from the store"
        );
        assert_eq!(
            std::fs::read_to_string(home.join("config.toml")).unwrap(),
            "model = \"gpt-5.6\"\n"
        );
        assert!(
            !home.join("config.toml").is_symlink(),
            "config.toml is a COPY — codex's own config writes stay isolated"
        );
        assert!(
            has_live_codex_session("cdx-iso"),
            "lease held while running"
        );

        drop(runtime);
        assert!(!has_live_codex_session("cdx-iso"));
        assert!(
            home.symlink_metadata().is_err(),
            "last session tears the tree down"
        );
    }

    #[test]
    fn acquire_refuses_the_live_owner_and_loginless_profiles() {
        let _sandbox = HomeSandbox::new();
        seed("cdx-live", "acct-live");
        crate::codex::write_live(&codex_auth("at-live-rotated", "acct-live")).unwrap();
        let Err(err) = CodexRuntime::acquire("cdx-live") else {
            panic!("live owner must be refused")
        };
        assert!(err.to_string().contains("LIVE codex login"), "{err}");

        let mut p = crate::testutil::blank_profile("cdx-empty");
        p.harness = crate::profile::Harness::Codex;
        crate::profile::save_profile(&p).unwrap();
        let Err(err) = CodexRuntime::acquire("cdx-empty") else {
            panic!("loginless profile must be refused")
        };
        assert!(err.to_string().contains("no stored codex login"), "{err}");
    }

    #[test]
    fn acquire_refuses_a_non_file_store_mode() {
        let sandbox = HomeSandbox::new();
        seed("cdx-keyring", "acct-k");
        let codex_dir = sandbox.home().join(".codex");
        std::fs::create_dir_all(&codex_dir).unwrap();
        std::fs::write(
            codex_dir.join("config.toml"),
            "cli_auth_credentials_store = \"keyring\"\n",
        )
        .unwrap();
        let Err(err) = CodexRuntime::acquire("cdx-keyring") else {
            panic!("keyring store mode must be refused")
        };
        assert!(err.to_string().contains("keyring"), "{err}");
    }

    #[test]
    fn watchdog_sync_adopts_a_rotated_chain_back_but_skips_torn_writes() {
        let _sandbox = HomeSandbox::new();
        seed("cdx-sync", "acct-sync");
        let runtime = CodexRuntime::acquire("cdx-sync").expect("acquire");
        let home = runtime.codex_home().to_path_buf();

        // codex rotates its chain inside the isolated home…
        let rotated = codex_auth("at-rotated", "acct-sync");
        std::fs::write(home.join("auth.json"), &rotated).unwrap();
        sync_codex_home_to_store("cdx-sync", &home).expect("sync");
        assert_eq!(
            crate::codex::read_profile_auth("cdx-sync")
                .unwrap()
                .as_deref(),
            Some(&rotated[..]),
            "rotation adopted back into the store"
        );

        // …a half-written file is left alone until it parses.
        std::fs::write(home.join("auth.json"), b"{\"tokens\":{").unwrap();
        sync_codex_home_to_store("cdx-sync", &home).expect("sync tolerates torn bytes");
        assert_eq!(
            crate::codex::read_profile_auth("cdx-sync")
                .unwrap()
                .as_deref(),
            Some(&rotated[..]),
            "torn bytes never reach the store"
        );
    }

    #[test]
    fn drop_runs_a_final_adopt_back_before_teardown() {
        let _sandbox = HomeSandbox::new();
        seed("cdx-final", "acct-final");
        let runtime = CodexRuntime::acquire("cdx-final").expect("acquire");
        let rotated = codex_auth("at-last-rotation", "acct-final");
        std::fs::write(runtime.codex_home().join("auth.json"), &rotated).unwrap();
        drop(runtime);
        assert_eq!(
            crate::codex::read_profile_auth("cdx-final")
                .unwrap()
                .as_deref(),
            Some(&rotated[..]),
            "the last rotation survives the teardown"
        );
    }

    #[test]
    fn sibling_acquire_never_reseeds_a_live_home() {
        let _sandbox = HomeSandbox::new();
        seed("cdx-sib", "acct-sib");
        let first = CodexRuntime::acquire("cdx-sib").expect("first");
        let rotated = codex_auth("at-rotated-live", "acct-sib");
        std::fs::write(first.codex_home().join("auth.json"), &rotated).unwrap();

        let second = CodexRuntime::acquire("cdx-sib").expect("sibling");
        assert_eq!(
            std::fs::read(second.codex_home().join("auth.json")).unwrap(),
            rotated,
            "a sibling shares the live tree — reseeding would roll the chain back"
        );
        drop(second);
        assert!(
            first.codex_home().join("auth.json").exists(),
            "tree survives while a session remains"
        );
        drop(first);
    }

    #[test]
    fn gc_adopts_back_then_removes_a_stranded_home() {
        let _sandbox = HomeSandbox::new();
        seed("cdx-crash", "acct-crash");
        // Simulate a SIGKILLed session: tree exists, no live lease.
        let home = codex_home_dir("cdx-crash").unwrap();
        std::fs::create_dir_all(&home).unwrap();
        let stranded = codex_auth("at-stranded", "acct-crash");
        std::fs::write(home.join("auth.json"), &stranded).unwrap();

        gc_stale_runtimes();
        assert_eq!(
            crate::codex::read_profile_auth("cdx-crash")
                .unwrap()
                .as_deref(),
            Some(&stranded[..]),
            "gc adopts the stranded chain back before removing the tree"
        );
        assert!(home.symlink_metadata().is_err(), "stranded tree removed");
    }
}
