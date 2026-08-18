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

/// `copy_tree` must skip a `copy_file` staging sibling, exactly as
/// `union_children` does for the watchdog mirror.
///
/// A shared fake-mode tree has publishes in flight whenever the watchdog's
/// lockless `mirror_tree` runs, so an `acquire` walking `~/.claude` can meet a
/// `.tmp.<pid>.<seq>` about to be renamed away. Copying one lands an orphan
/// nothing ever removes; on Windows it does worse, because the publishing
/// thread still has the file OPEN and `copy_file` fails with "used by another
/// process" — which this walk propagates, failing the whole acquire rather
/// than a tick that would have re-converged. Seen for real on a Windows CI leg
/// in `fake_mode_second_session_does_not_rebuild_the_tree`, where the sentinel
/// the test writes into the runtime was mid-mirror back into `~/.claude`.
#[test]
fn copy_tree_skips_a_publish_in_flight() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let src = tmp.path().join("src");
    fs::create_dir_all(src.join("nested")).expect("mkdir");
    fs::write(src.join("real.txt"), b"keep me").expect("write real");
    // What `copy_file` leaves visible mid-publish, at both levels.
    fs::write(src.join(".real.txt.tmp.4242.7"), b"in flight").expect("write staging");
    fs::write(src.join("nested").join(".b.txt.tmp.4242.8"), b"in flight").expect("write nested");

    let dst = tmp.path().join("dst");
    copy_tree(&src, &dst).expect("copy_tree");

    assert_eq!(
        fs::read(dst.join("real.txt")).expect("read real"),
        b"keep me"
    );
    assert!(
        !dst.join(".real.txt.tmp.4242.7").exists(),
        "a staging sibling must never be copied — nothing here ever deletes it again"
    );
    assert!(
        !dst.join("nested").join(".b.txt.tmp.4242.8").exists(),
        "the skip has to hold at every level of the recursion, not just the top"
    );
}

/// Pose a directory link at `link` pointing at `target`: a symlink on unix, a
/// JUNCTION on Windows.
///
/// A junction because it is the directory link a fake-mode host can still be
/// carrying — it needs no `SeCreateSymbolicLinkPrivilege`, and the absence of
/// that privilege is what puts the host on the copy transport in the first place.
/// A directory SYMLINK reaches the same branch,
/// measured identical on Windows 11, and is reachable too: Developer Mode or an
/// elevated process can lay one down before an unprivileged run probes `Fake`.
///
/// `mklink /J` because a junction has no std constructor — `symlink_dir` wants
/// the privilege the host lacks, and `FSCTL_SET_REPARSE_POINT` wants a winapi
/// dep plus `unsafe`. Its ceiling: `mklink` is a cmd
/// builtin, so no shell-free route exists, and a `%` in either path would still
/// reach cmd's variable expansion. Both paths here are tempdir-derived. Upgrade
/// path is a junction crate, or the FSCTL behind a test-only dep.
///
/// Asserts the fixture poses the misclassification under test: on a host where a
/// directory link reads as a plain directory under `symlink_metadata`, every
/// caller below would assert nothing.
fn pose_dir_link(link: &Path, target: &Path) {
    #[cfg(unix)]
    std::os::unix::fs::symlink(target, link).expect("symlink dir");
    #[cfg(windows)]
    {
        let out = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(link)
            .arg(target)
            .output()
            .expect("spawn `cmd /C mklink /J`");
        assert!(
            out.status.success(),
            "mklink /J {} {} failed ({}): {}{}",
            link.display(),
            target.display(),
            out.status,
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }
    assert!(
        !link
            .symlink_metadata()
            .expect("posed link is present")
            .file_type()
            .is_dir(),
        "fixture poses nothing: {} reads as a plain directory under \
         symlink_metadata, which is the misclassification under test",
        link.display()
    );
}

/// A directory LINK in `~/.claude` (a skill linked at a plugin dir) must recurse
/// in fake mode, not fall into `copy_file`: `std::fs::copy` follows the link and
/// refuses a directory, failing the whole acquire. Measured error per platform,
/// both against a link and against a plain dir: Windows 11 gives
/// `PermissionDenied` / "Access is denied. (os error 5)", and Linux (rustc
/// 1.97.1) gives `InvalidInput` / "the source path is neither a regular file nor
/// a symlink to a regular file" with no errno — `File::open` on a directory
/// succeeds there, so std refuses in `open_from` and EISDIR is never reached.
/// Followed, the target's files land as a real dir in the runtime.
#[test]
fn copy_tree_follows_a_directory_link() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let real = tmp.path().join("plugin");
    fs::create_dir_all(real.join("nested")).expect("mkdir target");
    fs::write(real.join("nested").join("skill.md"), b"skill body").expect("write target");

    let src = tmp.path().join("src");
    fs::create_dir_all(&src).expect("mkdir src");
    pose_dir_link(&src.join("agent-browser"), &real);

    let dst = tmp.path().join("dst");
    copy_tree(&src, &dst).expect("copy_tree");

    // Both levels, because they pin different halves and only one of them can
    // red today. The LEAF is live: swapping `copy_tree`'s `copy_file` for
    // `link_entry` reds it. The ENTRY is a forward guard and is unkillable as
    // the code stands — a directory always takes the recursing branch, so
    // nothing there can leave a link behind. Keep it anyway: it is the level a
    // leaf assert cannot see, since a leaf read THROUGH a directory link still
    // reports `is_symlink=false` (measured).
    let linked = dst.join("agent-browser");
    assert!(
        !linked
            .symlink_metadata()
            .expect("entry present")
            .file_type()
            .is_symlink(),
        "the entry itself must be a real dir, not a link back at the source"
    );
    let leaf = linked.join("nested").join("skill.md");
    assert_eq!(fs::read(&leaf).expect("read"), b"skill body");
    assert!(
        !leaf
            .symlink_metadata()
            .expect("leaf present")
            .file_type()
            .is_symlink(),
        "fake mode materializes the target's bytes; a re-created link is not the contract"
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

/// The mirror must treat a directory link on the CANONICAL (`~/.claude`) side as
/// a dir, not a file: otherwise `merge_path` hands `copy_file` a directory and
/// `std::fs::copy` fails the whole tick. Per-platform error strings are on
/// [`copy_tree_follows_a_directory_link`].
#[test]
fn mirror_tree_follows_a_directory_link() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let real = tmp.path().join("plugin");
    fs::create_dir_all(real.join("nested")).expect("mkdir target");
    fs::write(real.join("nested").join("skill.md"), b"skill body").expect("write target");

    let claude = tmp.path().join("claude");
    let runtime = tmp.path().join("runtime");
    fs::create_dir_all(&claude).expect("mkdir claude");
    fs::create_dir_all(&runtime).expect("mkdir runtime");
    pose_dir_link(&claude.join("skills"), &real);

    mirror_tree(&claude, &runtime).expect("mirror");

    // Forward guard, unkillable as the code stands: `merge_path` reaches only
    // `copy_file`, which cannot produce a link. It pins the level a leaf assert
    // cannot see (a leaf read THROUGH a directory link reports
    // `is_symlink=false`), so it earns its place without earning a kill.
    let mirrored = runtime.join("skills");
    assert!(
        !mirrored
            .symlink_metadata()
            .expect("entry present")
            .file_type()
            .is_symlink(),
        "the mirrored entry must be a real dir, not a link back at the source"
    );
    assert_eq!(
        fs::read(mirrored.join("nested").join("skill.md")).expect("read"),
        b"skill body"
    );
}

/// The RUNTIME side of the same predicate pair, which the test above cannot
/// reach: it poses the link on the canonical side only, so `b_is_dir` is false
/// under the fixed AND the pre-fix spelling and reverting that one line alone
/// leaves the whole suite green.
///
/// Reachable in production, which is why it is worth its own fixture: under fake
/// mode Claude Code runs OUT of the shared runtime tree, so a plugin skill it
/// links lands at `<runtime>/skills/<name>` with nothing on the `~/.claude` side
/// yet. `merge_path` then takes its `(None, Some(_))` arm and hands `copy_file`
/// the link — the same failure as the canonical side, once per tick, forever.
#[test]
fn mirror_tree_follows_a_directory_link_on_the_runtime_side() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let real = tmp.path().join("plugin");
    fs::create_dir_all(real.join("nested")).expect("mkdir target");
    fs::write(real.join("nested").join("skill.md"), b"cc wrote this").expect("write target");

    let claude = tmp.path().join("claude");
    let runtime = tmp.path().join("runtime");
    fs::create_dir_all(&claude).expect("mkdir claude");
    fs::create_dir_all(&runtime).expect("mkdir runtime");
    pose_dir_link(&runtime.join("skills"), &real);

    mirror_tree(&claude, &runtime).expect("mirror");

    assert_eq!(
        fs::read(claude.join("skills").join("nested").join("skill.md")).expect("read"),
        b"cc wrote this",
        "a runtime-side directory link must seed the canonical side, not fail the tick"
    );
}

/// Pose a FILE link at `link` pointing at `target`. Unlike a directory link
/// there is no unprivileged Windows shape — a junction points at directories
/// only — so this reports refusal instead of failing on its own fixture. CI's
/// Windows runner holds `SeCreateSymbolicLinkPrivilege`, so the
/// coverage is real there; a Developer-Mode-off box skips, out loud, because a
/// silent skip reads as a pass.
fn pose_file_link(link: &Path, target: &Path) -> bool {
    #[cfg(unix)]
    let posed = std::os::unix::fs::symlink(target, link);
    #[cfg(windows)]
    let posed = std::os::windows::fs::symlink_file(target, link);
    #[cfg(not(any(unix, windows)))]
    let posed: std::io::Result<()> = Err(std::io::Error::other("no symlink support"));
    match posed {
        Ok(()) => true,
        Err(e) => {
            eprintln!("SKIP: this host cannot pose a file symlink ({e})");
            false
        }
    }
}

/// The mirror must read a canonical-side symlink's TARGET, not the link. A
/// symlink's own mtime never moves when its target is written, so once the
/// runtime copy has been written even once it wins every later comparison and
/// the mirror copies its STALE bytes back — discarding the operator's edit, and
/// with `copy_file`'s rename the link along with it. `mirror_tree`'s own
/// contract is that it never destroys data.
#[test]
fn mirror_tree_reads_a_canonical_symlinks_target_not_the_link() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let claude = tmp.path().join("claude");
    let runtime = tmp.path().join("runtime");
    let real = tmp.path().join("dotfiles");
    for dir in [&claude, &runtime, &real] {
        fs::create_dir_all(dir).expect("mkdir");
    }
    let target = real.join("notes.md");
    fs::write(&target, b"OPERATOR-V2").expect("write target");
    if !pose_file_link(&claude.join("notes.md"), &target) {
        return;
    }
    fs::write(runtime.join("notes.md"), b"OPERATOR-V1").expect("write runtime copy");

    // The link's own mtime is its creation time, which nothing here can set —
    // and that is exactly the point. Both real files are stamped past it, the
    // operator's target newest, so the canonical side can only win on the
    // TARGET's clock.
    let now = SystemTime::now();
    set_mtime(&runtime.join("notes.md"), now + Duration::from_secs(300));
    set_mtime(&target, now + Duration::from_secs(600));

    mirror_tree(&claude, &runtime).expect("mirror");

    assert!(
        claude
            .join("notes.md")
            .symlink_metadata()
            .expect("canonical entry present")
            .file_type()
            .is_symlink(),
        "the operator's link must survive the tick"
    );
    assert_eq!(
        fs::read(runtime.join("notes.md")).expect("read runtime"),
        b"OPERATOR-V2",
        "the newer TARGET reaches the runtime; the link's stale clock must not win"
    );
    assert_eq!(
        fs::read(&target).expect("read target"),
        b"OPERATOR-V2",
        "and the operator's own file is left alone"
    );
}

/// The other direction of the same rule: a genuinely newer RUNTIME side must
/// write THROUGH the link onto its target, not rename over the link and strand
/// the operator's real file where nothing reads it.
#[test]
fn mirror_tree_writes_through_a_canonical_symlink() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let claude = tmp.path().join("claude");
    let runtime = tmp.path().join("runtime");
    let real = tmp.path().join("dotfiles");
    for dir in [&claude, &runtime, &real] {
        fs::create_dir_all(dir).expect("mkdir");
    }
    let target = real.join("notes.md");
    fs::write(&target, b"OPERATOR-V1").expect("write target");
    if !pose_file_link(&claude.join("notes.md"), &target) {
        return;
    }
    fs::write(runtime.join("notes.md"), b"CC-WROTE-V2").expect("write runtime copy");

    let now = SystemTime::now();
    set_mtime(&target, now + Duration::from_secs(300));
    set_mtime(&runtime.join("notes.md"), now + Duration::from_secs(600));

    mirror_tree(&claude, &runtime).expect("mirror");

    assert!(
        claude
            .join("notes.md")
            .symlink_metadata()
            .expect("canonical entry present")
            .file_type()
            .is_symlink(),
        "the write must land on the target, leaving the link itself in place"
    );
    assert_eq!(
        fs::read(&target).expect("read target"),
        b"CC-WROTE-V2",
        "the runtime edit reaches the operator's real file, not a regular file shadowing it"
    );
}

/// The asymmetry, pinned: a link on the RUNTIME side is not followed, because
/// that side is clauth's own copy rather than anything the operator declared.
/// Following one would aim a mirror write at an absolute path outside BOTH
/// trees, past everything the 0600/0700 tree invariant reaches.
///
/// Reachability is narrow — `detect_link_mode` picks `Fake` precisely because
/// `try_real_symlink` failed in the profile root, so a file link rarely exists
/// inside a fake-mode runtime tree at all — but the blast radius is a write to
/// an arbitrary path, so it is pinned rather than argued away.
#[test]
fn mirror_tree_does_not_write_through_a_runtime_side_symlink() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let claude = tmp.path().join("claude");
    let runtime = tmp.path().join("runtime");
    let outside = tmp.path().join("outside");
    for dir in [&claude, &runtime, &outside] {
        fs::create_dir_all(dir).expect("mkdir");
    }
    let stray = outside.join("target.md");
    fs::write(&stray, b"OUTSIDE").expect("write stray");
    if !pose_file_link(&runtime.join("notes.md"), &stray) {
        return;
    }
    fs::write(claude.join("notes.md"), b"CANON").expect("write canonical");

    let now = SystemTime::now();
    set_mtime(&stray, now + Duration::from_secs(300));
    set_mtime(&claude.join("notes.md"), now + Duration::from_secs(600));

    mirror_tree(&claude, &runtime).expect("mirror");

    assert_eq!(
        fs::read(&stray).expect("read stray"),
        b"OUTSIDE",
        "a runtime-side link must never redirect a mirror write outside both trees"
    );
    assert_eq!(
        fs::read(runtime.join("notes.md")).expect("read runtime"),
        b"CANON",
        "the write lands in the runtime tree, restoring its copy-of-canonical shape"
    );
}

/// A dangling link on the CANONICAL side must cost that ONE name, not the tick.
/// `symlink_metadata` stats the link and says the entry is there, so the
/// `(Some, Some)` arm runs and `files_match` reads THROUGH the broken link for an
/// ENOENT that propagates out of `mirror_tree` — every reconcile pass on a
/// copy-transport host dies from then on, and nothing self-heals it because the
/// operator's tree is where the moved-aside target lives. Ordinary trigger: the
/// operator moves a `~/.claude` file aside while the runtime holds its copy.
///
/// The link itself must survive: `~/.claude` is the operator's tree, so a link
/// there is their intent and the mirror never deletes.
#[test]
fn mirror_tree_survives_a_dangling_canonical_link() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let claude = tmp.path().join("claude");
    let runtime = tmp.path().join("runtime");
    for dir in [&claude, &runtime] {
        fs::create_dir_all(dir).expect("mkdir");
    }
    // `notes.md` sorts before `todos.json`, so the broken name is walked first
    // and a failing tick can never reach the ordinary file behind it.
    if !pose_file_link(&claude.join("notes.md"), &tmp.path().join("moved-aside.md")) {
        return;
    }
    fs::write(runtime.join("notes.md"), b"STALE COPY").expect("write runtime copy");
    fs::write(claude.join("todos.json"), b"[]").expect("write canonical");

    mirror_tree(&claude, &runtime).expect("one broken name must not fail the tick");

    assert_eq!(
        fs::read(runtime.join("todos.json")).expect("read runtime"),
        b"[]",
        "the rest of the tree still reconciles past a dangling name"
    );
    assert!(
        claude
            .join("notes.md")
            .symlink_metadata()
            .expect("canonical entry present")
            .file_type()
            .is_symlink(),
        "the operator's link is their intent; the mirror never unlinks it"
    );
    assert_eq!(
        fs::read(runtime.join("notes.md")).expect("read runtime copy"),
        b"STALE COPY",
        "and the runtime copy is left where it is, not published over the broken link"
    );
}

/// The DIRECTORY shape of the same defect, which the file fixture cannot reach:
/// a dangling canonical link whose runtime counterpart is a real directory takes
/// the `b_is_dir` branch, where `a.exists()` is false through the broken link and
/// `mkdir_700(a)` runs. A recursive create swallows EEXIST only when a follow-up
/// stat says the path is a directory, and a dangling link answers neither, so it
/// returns `File exists (os error 17)` and fails the tick.
#[test]
fn mirror_tree_survives_a_dangling_canonical_link_over_a_runtime_dir() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let claude = tmp.path().join("claude");
    let runtime = tmp.path().join("runtime");
    fs::create_dir_all(&claude).expect("mkdir claude");
    fs::create_dir_all(runtime.join("skills")).expect("mkdir runtime skills");
    fs::write(runtime.join("skills").join("skill.md"), b"skill body").expect("write skill");
    // `skills` sorts before `todos.json`, same reason as the fixture above.
    if !pose_file_link(&claude.join("skills"), &tmp.path().join("moved-aside")) {
        return;
    }
    fs::write(claude.join("todos.json"), b"[]").expect("write canonical");

    mirror_tree(&claude, &runtime).expect("one broken name must not fail the tick");

    assert_eq!(
        fs::read(runtime.join("todos.json")).expect("read runtime"),
        b"[]",
        "the rest of the tree still reconciles past a dangling name"
    );
    assert!(
        claude
            .join("skills")
            .symlink_metadata()
            .expect("canonical entry present")
            .file_type()
            .is_symlink(),
        "the mirror must not have replaced the operator's link with a directory"
    );
}

/// The RUNTIME side of the same predicate: a broken link there reads as an
/// existing entry too, so `files_match` fails on the `b` read instead of the `a`
/// one and takes the same tick down. Self-healing that side is
/// [`prune_dangling_links`]'s job at build time; the mirror only has to keep
/// walking.
#[test]
fn mirror_tree_survives_a_dangling_runtime_link() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let claude = tmp.path().join("claude");
    let runtime = tmp.path().join("runtime");
    for dir in [&claude, &runtime] {
        fs::create_dir_all(dir).expect("mkdir");
    }
    if !pose_file_link(
        &runtime.join("notes.md"),
        &tmp.path().join("moved-aside.md"),
    ) {
        return;
    }
    fs::write(claude.join("notes.md"), b"CANON").expect("write canonical");
    fs::write(claude.join("todos.json"), b"[]").expect("write canonical todos");

    mirror_tree(&claude, &runtime).expect("one broken name must not fail the tick");

    assert_eq!(
        fs::read(runtime.join("todos.json")).expect("read runtime"),
        b"[]",
        "the rest of the tree still reconciles past a dangling name"
    );
    assert!(
        runtime
            .join("notes.md")
            .symlink_metadata()
            .expect("runtime entry present")
            .file_type()
            .is_symlink(),
        "the mirror never unlinks either side; prune_dangling_links owns that repair"
    );
}

/// The skip predicate is written to the OUTCOME, not to symlinks. A regular file
/// unlinked between `merge_path`'s `symlink_metadata` and its follow-through
/// answers the same "present but unfollowable" shape, and `mirror_tree` walks
/// lockless by its own doc, so a file vanishing mid-walk is ordinary rather than
/// exotic. Narrow the predicate back to `is_symlink` and this pair reaches
/// `files_match`, which fails the whole tick on an ENOENT — the exact class the
/// guard exists to close.
///
/// Driven as a unit because the race cannot be posed as a static fixture: the
/// two stats have to straddle the unlink, which is what this reproduces exactly.
#[test]
fn an_entry_that_vanished_mid_walk_reads_as_unresolvable() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let victim = tmp.path().join("vanishes.json");
    fs::write(&victim, b"{}").expect("write victim");

    // The stat the walk already took, before the unlink it cannot see.
    let meta = victim.symlink_metadata().expect("stat before the unlink");
    assert!(
        !meta.file_type().is_symlink(),
        "fixture poses nothing unless the entry is a REGULAR file"
    );
    fs::remove_file(&victim).expect("unlink under the walk");

    assert!(
        is_unresolvable_entry(&victim, Some(&meta)),
        "a file unlinked between the two stats must be skipped, not merged"
    );
}

/// A symlink LOOP is the third shape the predicate's doc names: `symlink_metadata`
/// succeeds on the link, and `exists()` is false because `Path::exists` swallows
/// ELOOP the same way it swallows ENOENT. Pins the doc's claim rather than a
/// branch — the wide predicate and the old narrow one both catch this one.
#[cfg(unix)]
#[test]
fn a_symlink_loop_reads_as_unresolvable() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let a = tmp.path().join("a");
    let b = tmp.path().join("b");
    std::os::unix::fs::symlink("b", &a).expect("symlink a -> b");
    std::os::unix::fs::symlink("a", &b).expect("symlink b -> a");

    let meta = a.symlink_metadata().expect("the link itself stats fine");
    assert!(
        !a.exists(),
        "fixture poses nothing unless the loop is unfollowable"
    );
    assert!(is_unresolvable_entry(&a, Some(&meta)));
}

/// Two `~/.claude` names resolving to ONE file must converge on the CLOCK, not on
/// filename sort order. `union_children` sorts, so the first name's runtime copy
/// is published onto the shared target and stamps it with mtime-now; the second
/// name then reads that fresh stamp as the newer side and the divergent bytes it
/// was carrying are overwritten and gone. Which name survives is alphabetical,
/// which is not a decision anyone made.
///
/// Real symlink mode has no such split — each runtime entry IS the one file, so
/// last write wins — and fake mode's two independent copies are the emulation
/// gap. Newest-mtime-wins is its closest analogue, so ONE tick must leave the
/// target and both copies holding the newest bytes.
#[test]
fn mirror_tree_converges_two_names_on_one_target_by_clock() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let claude = tmp.path().join("claude");
    let runtime = tmp.path().join("runtime");
    let real = tmp.path().join("dotfiles");
    for dir in [&claude, &runtime, &real] {
        fs::create_dir_all(dir).expect("mkdir");
    }
    let target = real.join("shared.md");
    fs::write(&target, b"ORIGINAL").expect("write target");
    // `alias-a.md` sorts first, so on the pre-fix walk it is the one that wins.
    if !pose_file_link(&claude.join("alias-a.md"), &target) {
        return;
    }
    if !pose_file_link(&claude.join("alias-b.md"), &target) {
        return;
    }
    fs::write(runtime.join("alias-a.md"), b"SORTS-FIRST").expect("write runtime a");
    fs::write(runtime.join("alias-b.md"), b"NEWEST").expect("write runtime b");

    // All three in the PAST, which is what makes the loss reproducible: the
    // publish onto the target stamps it with mtime-now, so a stale sibling's
    // fresh stamp outranks every real reading still to come.
    let now = SystemTime::now();
    set_mtime(&target, now - Duration::from_secs(600));
    set_mtime(&runtime.join("alias-a.md"), now - Duration::from_secs(400));
    set_mtime(&runtime.join("alias-b.md"), now - Duration::from_secs(200));

    mirror_tree(&claude, &runtime).expect("mirror");

    assert_eq!(
        fs::read(&target).expect("read target"),
        b"NEWEST",
        "the newest copy in the class owns the shared target, not the first-sorted one"
    );
    assert_eq!(
        fs::read(runtime.join("alias-b.md")).expect("read runtime b"),
        b"NEWEST",
        "the winner's own copy is left alone"
    );
    assert_eq!(
        fs::read(runtime.join("alias-a.md")).expect("read runtime a"),
        b"NEWEST",
        "and a copy visited BEFORE the winner still converges within the same tick"
    );
}

/// The other sort order, which the fixture above cannot reach: the newest copy
/// is the one walked FIRST. Pre-fix this happened to come out right — the
/// first-sorted copy publishes onto the target, and the mtime-now stamp it leaves
/// then beats the older sibling — so it is a guard rather than a repro. It pins
/// that the class ADOPTS the winning copy's clock: keep re-reading the seed
/// instead and the second, strictly older copy outranks the target it was just
/// given and publishes its stale bytes back over the winner's.
#[test]
fn mirror_tree_converges_two_names_when_the_first_sorted_is_newest() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let claude = tmp.path().join("claude");
    let runtime = tmp.path().join("runtime");
    let real = tmp.path().join("dotfiles");
    for dir in [&claude, &runtime, &real] {
        fs::create_dir_all(dir).expect("mkdir");
    }
    let target = real.join("shared.md");
    fs::write(&target, b"ORIGINAL").expect("write target");
    if !pose_file_link(&claude.join("alias-a.md"), &target) {
        return;
    }
    if !pose_file_link(&claude.join("alias-b.md"), &target) {
        return;
    }
    fs::write(runtime.join("alias-a.md"), b"NEWEST").expect("write runtime a");
    fs::write(runtime.join("alias-b.md"), b"SORTS-SECOND").expect("write runtime b");

    let now = SystemTime::now();
    set_mtime(&target, now - Duration::from_secs(600));
    set_mtime(&runtime.join("alias-a.md"), now - Duration::from_secs(200));
    set_mtime(&runtime.join("alias-b.md"), now - Duration::from_secs(400));

    mirror_tree(&claude, &runtime).expect("mirror");

    assert_eq!(
        fs::read(&target).expect("read target"),
        b"NEWEST",
        "an older sibling must not publish back over the copy that already won"
    );
    assert_eq!(
        fs::read(runtime.join("alias-b.md")).expect("read runtime b"),
        b"NEWEST",
        "the loser converges onto the winner's bytes"
    );
}

/// A copy byte-equal to the canonical target must NOT lend the class its clock.
/// Such a copy is this mirror's own echo from an earlier tick and an mtime move
/// is not a write, so a freshly-touched echo would outrank the sibling carrying a
/// real edit and the merge would publish the old shared bytes back over it.
///
/// The fixture is the `CLAUDE.local.md` -> `CLAUDE.md` shape, on the exact clocks
/// that make the echo look newest: the echo sorts first at now-100 while the edit
/// sits at now-600 behind a canonical file from now-900. One tick must land the
/// edit on the canonical file AND on both runtime copies —
/// [`mirror_tree`]'s contract is that it never destroys data.
#[test]
fn mirror_tree_alias_echo_does_not_outrank_a_siblings_real_edit() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let claude = tmp.path().join("claude");
    let runtime = tmp.path().join("runtime");
    for dir in [&claude, &runtime] {
        fs::create_dir_all(dir).expect("mkdir");
    }
    let memory = claude.join("CLAUDE.md");
    fs::write(&memory, b"OLD SHARED").expect("write memory");
    if !pose_file_link(&claude.join("CLAUDE.local.md"), &memory) {
        return;
    }
    fs::write(runtime.join("CLAUDE.md"), b"OPERATOR EDIT").expect("write runtime memory");
    fs::write(runtime.join("CLAUDE.local.md"), b"OLD SHARED").expect("write runtime echo");

    let now = SystemTime::now();
    set_mtime(&memory, now - Duration::from_secs(900));
    set_mtime(
        runtime.join("CLAUDE.md").as_path(),
        now - Duration::from_secs(600),
    );
    set_mtime(
        runtime.join("CLAUDE.local.md").as_path(),
        now - Duration::from_secs(100),
    );

    mirror_tree(&claude, &runtime).expect("mirror");

    assert_eq!(
        fs::read(&memory).expect("read memory"),
        b"OPERATOR EDIT",
        "a touched echo must not beat the sibling holding the only real edit"
    );
    assert_eq!(
        fs::read(runtime.join("CLAUDE.md")).expect("read runtime memory"),
        b"OPERATOR EDIT",
        "and the edit is certainly not overwritten in place"
    );
    assert_eq!(
        fs::read(runtime.join("CLAUDE.local.md")).expect("read runtime echo"),
        b"OPERATOR EDIT",
        "the echo converges onto the edit within the same tick"
    );
}

/// The DIRECTORY spelling of the same aliasing: two `~/.claude` names linked at
/// one directory, reaching its files under leaf names that are not themselves
/// links. `canonicalize` cannot identify the shared file while it is still
/// runtime-only, which is exactly when the loss happens — the first name creates
/// it and stamps it with mtime-now, and the second reads that stamp as newer.
/// Resolving the PARENT and re-attaching the lexical tail is what gives the class
/// its identity a tick early.
#[test]
fn mirror_tree_converges_two_directory_aliases_on_one_target() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let claude = tmp.path().join("claude");
    let runtime = tmp.path().join("runtime");
    let shared = tmp.path().join("dotfiles").join("shared");
    for dir in [&claude, &runtime, &shared] {
        fs::create_dir_all(dir).expect("mkdir");
    }
    pose_dir_link(&claude.join("link-a"), &shared);
    pose_dir_link(&claude.join("link-b"), &shared);
    fs::create_dir_all(runtime.join("link-a")).expect("mkdir runtime a");
    fs::create_dir_all(runtime.join("link-b")).expect("mkdir runtime b");
    fs::write(runtime.join("link-a").join("note.md"), b"SORTS-FIRST").expect("write runtime a");
    fs::write(runtime.join("link-b").join("note.md"), b"NEWEST").expect("write runtime b");

    let now = SystemTime::now();
    set_mtime(
        runtime.join("link-a").join("note.md").as_path(),
        now - Duration::from_secs(400),
    );
    set_mtime(
        runtime.join("link-b").join("note.md").as_path(),
        now - Duration::from_secs(200),
    );

    mirror_tree(&claude, &runtime).expect("mirror");

    assert_eq!(
        fs::read(shared.join("note.md")).expect("read shared"),
        b"NEWEST",
        "a file two linked directories share is one class, seeded before it exists"
    );
    assert_eq!(
        fs::read(runtime.join("link-a").join("note.md")).expect("read runtime a"),
        b"NEWEST",
        "and the copy walked first converges within the same tick"
    );
}

/// An exact mtime tie with divergent bytes: `mtime_newer` is strict, so the
/// per-name merge writes nothing and both sides keep what they hold. Inside an
/// alias class that leaves two spellings of ONE file disagreeing with no resting
/// state, so `converge` breaks the tie toward the shared target. Outside a class
/// nothing breaks it, because two independent files are allowed to differ — both
/// halves are pinned here, since the second is what bounds the first.
#[test]
fn mirror_tree_breaks_an_mtime_tie_only_inside_an_alias_class() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let claude = tmp.path().join("claude");
    let runtime = tmp.path().join("runtime");
    let real = tmp.path().join("dotfiles");
    for dir in [&claude, &runtime, &real] {
        fs::create_dir_all(dir).expect("mkdir");
    }
    let target = real.join("shared.md");
    fs::write(&target, b"SHARED").expect("write target");
    if !pose_file_link(&claude.join("alias-a.md"), &target) {
        return;
    }
    if !pose_file_link(&claude.join("alias-b.md"), &target) {
        return;
    }
    fs::write(runtime.join("alias-a.md"), b"DIVERGED").expect("write runtime a");
    fs::write(runtime.join("alias-b.md"), b"SHARED").expect("write runtime b");
    fs::write(claude.join("solo.md"), b"CANON SOLO").expect("write canon solo");
    fs::write(runtime.join("solo.md"), b"RUNTIME SOLO").expect("write runtime solo");

    let tie = SystemTime::now() - Duration::from_secs(300);
    for p in [
        target.as_path(),
        &runtime.join("alias-a.md"),
        &runtime.join("alias-b.md"),
        &claude.join("solo.md"),
        &runtime.join("solo.md"),
    ] {
        set_mtime(p, tie);
    }
    // The fixture poses nothing unless the stamps land EXACTLY equal: one
    // filesystem tick of drift turns this into an ordinary newer-side merge.
    let stamp = |p: &Path| p.metadata().expect("meta").modified().expect("mtime");
    assert_eq!(
        stamp(&target),
        stamp(&runtime.join("alias-a.md")),
        "aliased pair must be an exact tie"
    );
    assert_eq!(
        stamp(&claude.join("solo.md")),
        stamp(&runtime.join("solo.md")),
        "solo pair must be an exact tie"
    );

    mirror_tree(&claude, &runtime).expect("mirror");

    assert_eq!(
        fs::read(runtime.join("alias-a.md")).expect("read runtime a"),
        b"SHARED",
        "an aliased tie has no resting state, so the shared target breaks it"
    );
    assert_eq!(
        fs::read(&target).expect("read target"),
        b"SHARED",
        "and the tie never moves the target itself"
    );
    assert_eq!(
        fs::read(claude.join("solo.md")).expect("read canon solo"),
        b"CANON SOLO",
        "a tie between two independent files is still left exactly alone"
    );
    assert_eq!(
        fs::read(runtime.join("solo.md")).expect("read runtime solo"),
        b"RUNTIME SOLO",
        "both sides of it, in both directions"
    );
}

/// The regression guard for the rule we did NOT pick. Skipping an aliased name
/// would also stop sort order deciding, and it would strand the common shape:
/// `CLAUDE.local.md` symlinked at `CLAUDE.md`, whose two runtime copies are
/// byte-identical, so nothing is ever at risk of being lost. An operator edit to
/// the target has to reach BOTH copies in one tick, exactly as it did before the
/// alias class existed.
///
/// This one passes before the fix as well as after — it exists to fail a fix that
/// buys convergence by refusing to merge.
#[test]
fn mirror_tree_still_seeds_both_copies_of_a_benign_alias() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let claude = tmp.path().join("claude");
    let runtime = tmp.path().join("runtime");
    for dir in [&claude, &runtime] {
        fs::create_dir_all(dir).expect("mkdir");
    }
    let memory = claude.join("CLAUDE.md");
    fs::write(&memory, b"OPERATOR EDIT").expect("write memory");
    // `CLAUDE.local.md` sorts before `CLAUDE.md`, so the alias seeds the class.
    if !pose_file_link(&claude.join("CLAUDE.local.md"), &memory) {
        return;
    }
    fs::write(runtime.join("CLAUDE.md"), b"OLD").expect("write runtime memory");
    fs::write(runtime.join("CLAUDE.local.md"), b"OLD").expect("write runtime alias");

    let now = SystemTime::now();
    set_mtime(
        runtime.join("CLAUDE.md").as_path(),
        now - Duration::from_secs(600),
    );
    set_mtime(
        runtime.join("CLAUDE.local.md").as_path(),
        now - Duration::from_secs(600),
    );
    set_mtime(&memory, now - Duration::from_secs(200));

    mirror_tree(&claude, &runtime).expect("mirror");

    assert_eq!(
        fs::read(runtime.join("CLAUDE.md")).expect("read runtime memory"),
        b"OPERATOR EDIT",
        "the operator's edit must still reach the runtime copy"
    );
    assert_eq!(
        fs::read(runtime.join("CLAUDE.local.md")).expect("read runtime alias"),
        b"OPERATOR EDIT",
        "and the aliased copy too — an alias class converges, it does not opt out of merging"
    );
    assert_eq!(
        fs::read(&memory).expect("read memory"),
        b"OPERATOR EDIT",
        "the operator's own file is left alone"
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

    // Any `.dst.json.tmp.*` sidecar must be renamed away after the atomic write.
    // Matched by PREFIX, not by the exact `<pid>` name: pinning the full name
    // would pass vacuously the moment the tmp scheme gains a component.
    let stray: Vec<String> = dir_entry_names(tmp.path())
        .into_iter()
        .filter(|n| n.starts_with(".dst.json.tmp."))
        .collect();
    assert!(
        stray.is_empty(),
        "atomic copy must not leave a tmp file, found {stray:?}"
    );
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

/// Same invariant for the BULK materialize path. Under `LinkMode::Fake` a second
/// session's `acquire` copies new `~/.claude` entries into a tree a live sibling
/// is already using, while that sibling's lockless `mirror_tree` walks it. A
/// truncate-then-stream copy is byte-different and mtime-now, so `merge_path`
/// reads it as the newer side and copies the PARTIAL bytes back over
/// `~/.claude/<entry>` — operator data loss outside the runtime tree.
#[test]
fn copy_tree_visible_state_is_never_torn() {
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
        copy_tree(&src, &dst).expect("copy_tree");
    }
    stop.store(true, Ordering::Relaxed);
    reader.join().expect("reader panicked");
    assert_eq!(fs::read(dst.as_ref()).expect("final read"), new);
}

/// Both fake-mode publish paths must carry the source's mode over. `~/.claude`
/// holds `statusline.sh`, hooks, and plugin executables; a copy at 0644 runs a
/// Claude Code whose statusline and hooks fail. A read-then-`atomic_write`
/// creates at the umask, which is why both paths stream through `std::fs::copy`.
///
/// The mirror leg is the one that used to lose it, and in BOTH directions: the
/// bit only dies on the first edit after the tree is built, because
/// `files_match` short-circuits identical files until then.
#[cfg(unix)]
#[test]
fn both_fake_mode_publish_paths_preserve_the_executable_bit() {
    use std::os::unix::fs::PermissionsExt;

    let mode_of = |p: &Path| {
        fs::metadata(p)
            .unwrap_or_else(|e| panic!("stat {}: {e}", p.display()))
            .permissions()
            .mode()
            & 0o777
    };

    let tmp = tempfile::tempdir().expect("tempdir");
    let claude = tmp.path().join("claude");
    let runtime = tmp.path().join("runtime");
    fs::create_dir_all(&claude).expect("mkdir claude");
    fs::create_dir_all(&runtime).expect("mkdir runtime");

    // 1. The bulk materialize walk.
    let hook = claude.join("hook.sh");
    fs::write(&hook, b"#!/bin/sh\necho v1\n").expect("write hook");
    fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).expect("chmod hook");
    copy_tree(&hook, &runtime.join("hook.sh")).expect("copy_tree");
    assert_eq!(
        mode_of(&runtime.join("hook.sh")),
        0o755,
        "a hook materialized into the runtime tree must stay executable"
    );

    // 2. The mirror leg, ~/.claude → runtime: the operator edits the hook, so
    //    `files_match` stops short-circuiting and the copy actually runs.
    fs::write(&hook, b"#!/bin/sh\necho v2\n").expect("edit hook");
    set_mtime(&hook, SystemTime::now() + Duration::from_secs(60));
    mirror_tree(&claude, &runtime).expect("mirror to runtime");
    assert_eq!(
        fs::read(runtime.join("hook.sh")).expect("read runtime hook"),
        b"#!/bin/sh\necho v2\n",
        "the edit must actually reach the runtime — otherwise the mode assert is vacuous"
    );
    assert_eq!(
        mode_of(&runtime.join("hook.sh")),
        0o755,
        "the mirror must not strip +x off the runtime copy"
    );

    // 3. The mirror leg, runtime → ~/.claude: a write-back at 0644 would strip
    //    +x off the operator's own file, outside the runtime tree.
    let back = runtime.join("cc-made-this.sh");
    fs::write(&back, b"#!/bin/sh\necho cc\n").expect("write runtime-only hook");
    fs::set_permissions(&back, fs::Permissions::from_mode(0o755)).expect("chmod runtime hook");
    mirror_tree(&claude, &runtime).expect("mirror to claude");
    assert_eq!(
        mode_of(&claude.join("cc-made-this.sh")),
        0o755,
        "the write-back must not strip +x off the operator's side"
    );
}

#[test]
fn detect_link_mode_returns_real_on_unix() {
    // Same lock every `with_link_mode` test holds, so a parallel override can
    // never leak into the probe this test exists to check.
    let _lock = crate::profile::HOME_TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let tmp = tempfile::tempdir().expect("tempdir");
    let mode = detect_link_mode(tmp.path()).expect("detect");
    #[cfg(unix)] // Unix CI always grants symlinks; Windows depends on dev mode
    assert_eq!(mode, LinkMode::Real);
    #[cfg(not(unix))]
    let _ = mode;
}

/// The read-only twin of [`detect_link_mode`]: `link_mode_of` observes the tree
/// an acquire already built rather than testing what this process may create.
/// One verdict per probe shape, each driven through a real on-disk layout: an
/// empty dir reads `NothingShared`, two plain entries `Fake`, a link beside a
/// copy `Mixed`, two links `Real`. `Mixed` is the rename-replace hazard the
/// probe exists to catch: an atomic-save edit of `CLAUDE.md` leaves a plain
/// file beside a `skills` link on a symlink host, and trusting either entry
/// would state the wrong transport.
#[test]
fn link_mode_of_reads_the_transport_off_the_existing_tree() {
    let tmp = tempfile::tempdir().expect("tempdir");
    assert_eq!(
        link_mode_of(Some(tmp.path())),
        LinkProbe::NothingShared,
        "an empty dir shares nothing",
    );

    // Copy mode: both shared slots plain.
    fs::write(tmp.path().join("CLAUDE.md"), b"x").expect("write shared entry");
    fs::create_dir(tmp.path().join("skills")).expect("create skills copy");
    assert_eq!(
        link_mode_of(Some(tmp.path())),
        LinkProbe::Fake,
        "two plain entries mean the copy mirror",
    );

    // Symlink mode: the same slots as links. Unix CI always grants symlinks.
    #[cfg(unix)]
    {
        let target = tmp.path().join("target");
        fs::write(&target, b"x").expect("write target");
        fs::remove_file(tmp.path().join("CLAUDE.md")).expect("remove copy");
        fs::remove_dir(tmp.path().join("skills")).expect("remove skills copy");
        std::os::unix::fs::symlink(&target, tmp.path().join("CLAUDE.md"))
            .expect("link shared entry");
        std::os::unix::fs::symlink(&target, tmp.path().join("skills")).expect("link skills entry");
        assert_eq!(
            link_mode_of(Some(tmp.path())),
            LinkProbe::Real,
            "two links mean the symlink transport",
        );
    }

    // Disagreement is its own verdict, never whichever entry the probe checked
    // first. The hedge prose is true under either.
    let mixed = tempfile::tempdir().expect("tempdir");
    fs::write(mixed.path().join("CLAUDE.md"), b"x").expect("write copy entry");
    #[cfg(unix)]
    {
        let target = mixed.path().join("skills-target");
        fs::create_dir(&target).expect("create target dir");
        std::os::unix::fs::symlink(&target, mixed.path().join("skills")).expect("link skills");
        assert_eq!(
            link_mode_of(Some(mixed.path())),
            LinkProbe::Mixed,
            "one link plus one copy reads Mixed",
        );
    }

    // One entry present answers with that entry's verdict.
    let one = tempfile::tempdir().expect("tempdir");
    fs::create_dir(one.path().join("skills")).expect("create skills");
    assert_eq!(
        link_mode_of(Some(one.path())),
        LinkProbe::Fake,
        "the skills slot answers when CLAUDE.md is absent",
    );

    assert_eq!(
        link_mode_of(None),
        LinkProbe::NothingShared,
        "no config dir shares nothing",
    );
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

/// Force [`detect_link_mode`] to report `mode` for the duration of `f`.
/// `try_real_symlink` always succeeds on unix, so the fake-symlink transport —
/// and the shared bare-stem tree it selects — is otherwise unreachable from a
/// Linux/macOS run. Call INSIDE [`with_fake_home`]: its `HOME_TEST_LOCK` hold is
/// what serializes this process-global override.
fn with_link_mode<T>(mode: LinkMode, f: impl FnOnce() -> T) -> T {
    struct ClearOnDrop;
    impl Drop for ClearOnDrop {
        fn drop(&mut self) {
            clear_link_mode_override();
        }
    }
    set_link_mode_override(mode);
    let _clear = ClearOnDrop;
    f()
}

/// Whether this host can pose `subject`, for a fixture that needs
/// [`LinkMode::Real`] to exist at all. Three shapes need it: a compat marker
/// SEPARATE from the session's own, a runtime tree PER session, and the real
/// symlink [`lone_session`] hardcodes. Under the shared tree the two marker paths
/// collapse into one, every session of a profile shares one bare-stem tree, and
/// `create_symlink` degrades to a copy `read_link` cannot follow — so such a test
/// fails on its own fixture rather than on the behavior it guards. A host without
/// `SeCreateSymbolicLinkPrivilege` (Windows outside Developer Mode) probes into
/// `Fake` for every test here, which is where that bites.
///
/// A capability skip, not a mode force: `with_link_mode` overrides only
/// [`detect_link_mode`], so forcing `Real` on such a host would still attempt
/// real symlinks in the build and fail with os error 1314. Call INSIDE
/// [`with_fake_home`], and name what is skipped out loud, since a silent skip
/// reads as a pass.
///
/// Reach for it only when the fixture itself is impossible. A test whose SUBJECT
/// survives the shared tree gets a host-aware expectation instead, so the box
/// keeps covering it — `the_with_fallback_flag_reaches_the_row_only_where_a_swap_can_land`
/// is the pattern.
fn host_poses(probe_dir: &Path, subject: &str) -> bool {
    let mode = detect_link_mode(probe_dir).expect("probe link mode");
    if mode != LinkMode::Real {
        eprintln!("SKIP: this host is {mode:?} and cannot pose {subject}");
    }
    mode == LinkMode::Real
}

/// Truncate `touch_store`'s READ-BACK to whole seconds for the duration of `f` —
/// the one thing the receipt guard consults, not a model of a coarse filesystem
/// end to end. `file_mtime` is untouched, so `memoized` stays full-precision and
/// the stamp-ahead fallback cannot fire here the way it could on a genuine 1s
/// mount; that branch is out of scope for what this poses. Every filesystem a
/// Linux/macOS run can reach keeps the exact value, so the guard is otherwise
/// unreachable. Call INSIDE [`with_fake_home`], whose `HOME_TEST_LOCK` hold
/// serializes this process-global override.
///
/// Gated with its caller, which drives a `swap_to` macOS refuses at platform
/// level.
#[cfg(not(target_os = "macos"))]
fn with_coarse_mtime<T>(f: impl FnOnce() -> T) -> T {
    struct ClearOnDrop;
    impl Drop for ClearOnDrop {
        fn drop(&mut self) {
            set_coarse_mtime_override(false);
        }
    }
    set_coarse_mtime_override(true);
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

/// [`make_profile`] plus the on-disk record `ProfileRuntime::acquire` re-reads
/// under the state flock. Every acquire fixture needs it: an unregistered name
/// IS the deleted-account state `refuse_if_unconfigured` refuses, so a fixture
/// without it would pin that gate instead of whatever the test is about.
/// Read-modify-write, so one fixture can register several. Call INSIDE
/// [`with_fake_home`] — it writes into `~/.clauth`.
fn configured_profile(name: &str) -> crate::profile::Profile {
    let profile = make_profile(name);
    register_profile(&profile);
    profile
}

/// Put an already-built profile on disk and into the profile list. Split from
/// [`configured_profile`] for the fixtures that mint their own [`Profile`]
/// (credentials, endpoint, disabled flag) before registering it.
fn register_profile(profile: &Profile) {
    crate::profile::save_profile(profile).expect("save profile");
    let mut state = crate::profile::load_config().expect("load config").state;
    if !state.profiles.iter().any(|n| n == &profile.name) {
        state.profiles.push(profile.name.as_str().into());
    }
    crate::profile::save_app_state(&state).expect("save app state");
}

/// The `<sid>` keying a live session's dirs, read back off its runtime path.
fn sid_of(runtime: &Path) -> String {
    runtime
        .file_name()
        .and_then(|n| n.to_str())
        .and_then(|n| n.strip_prefix("runtime-"))
        .map(str::to_owned)
        .unwrap_or_else(|| panic!("{} is not a per-session runtime dir", runtime.display()))
}

/// A live session's id, read back off its own marker dir — which holds exactly
/// one marker, named for the session. Flavor-agnostic, unlike [`sid_of`].
fn live_sid(rt: &ProfileRuntime) -> String {
    let mut names = dir_entry_names(rt.sessions_dir());
    assert_eq!(names.len(), 1, "a marker dir holds exactly one marker");
    names.remove(0)
}

/// Sorted file names directly under `dir`.
fn dir_entry_names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("read {}: {e}", dir.display()))
        .flatten()
        .filter_map(|e| e.file_name().to_str().map(str::to_owned))
        .collect();
    names.sort();
    names
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

/// A file another writer is part-way through publishing is not tree content.
/// `union_children` skips those on the mirror side; the acquire-time walk did
/// not, so a second session's tree build sampled a `.<name>.tmp.<pid>.<seq>`
/// sibling the first session's mirror still held. Both halves of the loss the
/// skip rule names are reachable from here: the copy fails when the rename lands
/// mid-walk, and it lands an orphan the mirror never deletes when it does not.
/// Measured on Windows 11 under a stripped symlink token, 22 of 30 narrow-filter
/// runs, as `os error 32` — share modes there are per-handle, so one process
/// holding the source open for writing is no exemption.
#[test]
fn the_tree_build_skips_a_publish_in_flight() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let claude_home = fake_claude_home(tmp.path());
        fs::create_dir_all(claude_home.join("projects")).expect("mkdir projects");
        let staged = crate::profile::tmp_sibling(&claude_home.join("memory.md"));
        let nested = crate::profile::tmp_sibling(&claude_home.join("projects").join("a.jsonl"));
        fs::write(&staged, b"half a publish").expect("stage");
        fs::write(&nested, b"half a publish").expect("stage nested");
        // Positive control on the same dimension the subject varies: same walk,
        // same two directories, only the NAME differs.
        fs::write(claude_home.join("memory.md"), b"real").expect("write real");
        fs::write(claude_home.join("projects").join("a.jsonl"), b"real").expect("write nested");
        let staged_name = staged.file_name().expect("staged name").to_owned();
        let nested_name = nested.file_name().expect("nested name").to_owned();
        let profile = make_profile("staging");
        let canonical = tmp.path().join("profile-creds.json");

        let copied = tmp.path().join("runtime-fake");
        fs::create_dir_all(&copied).expect("mkdir runtime");
        build_runtime_dir(
            &copied,
            &claude_home,
            &profile,
            &canonical,
            LinkMode::Fake,
            Isolation::Shared,
        )
        .expect("build");
        assert!(
            !copied.join(&staged_name).exists(),
            "a publish in flight must not be copied into the runtime tree"
        );
        assert!(
            !copied.join("projects").join(&nested_name).exists(),
            "the recursion into a subtree must skip one too"
        );
        assert_eq!(
            fs::read(copied.join("memory.md")).expect("read the real file"),
            b"real",
            "the walk still materializes real tree content"
        );
        assert_eq!(
            fs::read(copied.join("projects").join("a.jsonl")).expect("read the nested real file"),
            b"real"
        );

        // The same walk feeds real mode, where the entry becomes a symlink that
        // dangles the moment the rename lands. Pinned so the skip cannot be
        // moved down into the copy and read as covered.
        //
        // Gated on the host actually posing a symlink, because forcing
        // `LinkMode::Real` where the OS denies one fails the build outright
        // (os error 1314) instead of exercising the walk. A runtime probe, not
        // `cfg(unix)`: an elevated Windows box poses this leg fine, and the
        // neighbours' compile-time gate would sweep it out there too. Not
        // `host_poses` either — the fake half above runs on every host, so that
        // helper's SKIP line would report a running test as skipped.
        let linked = tmp.path().join("runtime-real");
        fs::create_dir_all(&linked).expect("mkdir runtime");
        if detect_link_mode(&linked).expect("probe link mode") != LinkMode::Real {
            return;
        }
        build_runtime_dir(
            &linked,
            &claude_home,
            &profile,
            &canonical,
            LinkMode::Real,
            Isolation::Shared,
        )
        .expect("build");
        assert!(
            linked.join(&staged_name).symlink_metadata().is_err(),
            "a publish in flight must not be linked into the runtime tree either"
        );
        assert!(
            linked.join("memory.md").symlink_metadata().is_ok(),
            "the walk still links real tree content"
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

/// The top-level materialize walk must skip a `copy_file` publish in flight,
/// the same way `union_children` does for the watchdog mirror.
///
/// This is the walk that actually bit: a shared fake-mode tree has the
/// watchdog's lockless `mirror_tree` publishing runtime-side files back into
/// `~/.claude` while a sibling session acquires, so `read_dir(claude_home)`
/// hands `pending` a `.tmp.<pid>.<seq>` that is about to be renamed away. On
/// Windows the publishing thread still holds it OPEN, so the copy fails with
/// "used by another process" — and this walk PROPAGATES, failing the whole
/// `acquire` rather than a tick that would have re-converged. Caught on a
/// Windows CI leg in `fake_mode_second_session_does_not_rebuild_the_tree`.
///
/// Both link modes, because real mode is no better: it would land a symlink
/// pointing at a path that is about to vanish.
#[test]
fn build_runtime_dir_skips_a_publish_in_flight() {
    for mode in [LinkMode::Fake, LinkMode::Real] {
        let tmp = tempfile::tempdir().expect("tempdir");
        with_fake_home(tmp.path(), || {
            let claude_home = fake_claude_home(tmp.path());
            fs::write(claude_home.join("history.jsonl"), b"{}").expect("write history");
            // What the watchdog's publish looks like from this walk's side.
            fs::write(claude_home.join(".history.jsonl.tmp.4242.9"), b"in flight")
                .expect("write staging");
            let runtime = tmp.path().join("runtime");
            fs::create_dir_all(&runtime).expect("mkdir runtime");
            let profile = make_profile("staging");
            let canonical = tmp.path().join("creds.json");

            build_runtime_dir(
                &runtime,
                &claude_home,
                &profile,
                &canonical,
                mode,
                Isolation::Shared,
            )
            .expect("build must not fail on a publish in flight");

            assert!(
                runtime.join("history.jsonl").exists(),
                "real content still materializes"
            );
            assert!(
                runtime
                    .join(".history.jsonl.tmp.4242.9")
                    .symlink_metadata()
                    .is_err(),
                "a staging sibling must never be materialized ({mode:?})"
            );
        });
    }
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
        assert!(!has_live_session(&crate::profile::ProfileName::from(
            "ghost"
        ))); // no sessions dir → false, not error
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
        assert!(!has_live_session(&crate::profile::ProfileName::from(
            "empty"
        )));
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
        assert!(!has_live_session(&crate::profile::ProfileName::from(
            "dead"
        )));
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
        assert!(has_live_session(&crate::profile::ProfileName::from(
            "alive"
        )));
        drop(file);
        // The probe is deliberately fail-alive (any try_lock I/O error reads
        // as "alive" — see `is_session_alive`), so one transient error under a
        // parallel suite run can inflate a single reading. Poll briefly: only
        // a PERSISTENTLY-alive reading is a regression. Same hardening as
        // `live_session_count_counts_only_alive`.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let settled_dead = loop {
            let alive = has_live_session(&crate::profile::ProfileName::from("alive"));
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
        assert!(has_live_session(&crate::profile::ProfileName::from(
            "mixed"
        )));
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
                let n = live_session_count(&crate::profile::ProfileName::from("counted"));
                if n == expect || std::time::Instant::now() >= deadline {
                    return n;
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        };
        assert_eq!(settled(2), 2);
        drop(a);
        assert_eq!(settled(1), 1);
        assert_eq!(
            live_session_count(&crate::profile::ProfileName::from("ghost")),
            0
        ); // no sessions dir → zero
    });
}

#[test]
fn acquire_creates_runtime_and_pid_file() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        if !host_poses(tmp.path(), "a runtime tree per session") {
            return;
        }
        fake_claude_home(tmp.path());
        let profile = configured_profile("lifecycle");

        let rt = ProfileRuntime::acquire(&profile, Isolation::Shared, &[], false).expect("acquire");

        assert!(
            rt.config_dir().is_dir(),
            "runtime dir must exist after acquire"
        );

        let sessions = rt.sessions_dir().to_path_buf();
        let sid = sid_of(rt.config_dir());
        assert_eq!(
            dir_entry_names(&sessions),
            vec![sid.clone()],
            "exactly one marker, named for this session"
        );
        let pid_file = sessions.join(&sid);
        assert!(
            sid.starts_with(&format!("{}-", std::process::id())),
            "the session id must carry the `<pid>-` prefix, got {sid}"
        );
        assert!(
            is_session_alive(&pid_file),
            "PID file must be flock-held while runtime is alive"
        );

        let profile_dir = tmp
            .path()
            .join(".clauth")
            .join("profiles")
            .join("lifecycle");
        let expected_runtime = profile_dir.join(format!("runtime-{sid}"));
        assert_eq!(rt.config_dir(), expected_runtime);
        assert_eq!(sessions, profile_dir.join(format!("sessions-{sid}")));

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

/// The window row 2 of the lock-race backlog names: a caller loads config,
/// `RotationGuard::acquire` BLOCKS, a delete lands, and the acquire then rebuilds
/// a whole session for an account nothing configures.
///
/// Driven single-threaded and through the REAL `actions::delete_profile`,
/// because the seam is between two statements of the CALLER rather than inside
/// `acquire`: `start::run` reads `config.find(&crate::profile::ProfileName::from(name))` and hands the borrow down,
/// so a test that deletes between those two statements occupies exactly the
/// window. A second thread would add a scheduler to the fixture without moving
/// the seam, and could not land the delete inside `acquire`'s own hold anyway —
/// the delete takes the same flock. Driving the real action rather than
/// hand-unlinking keeps the fixture honest if the delete's order changes.
///
/// What this does NOT separate: the gate sitting inside the state-flock hold
/// from the gate sitting just before it. The delete here has already finished,
/// so both placements refuse. That half is
/// `acquire_refuses_a_record_removed_without_a_rotation_lock`.
#[test]
fn acquire_refuses_a_profile_deleted_after_the_config_load() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        fake_claude_home(tmp.path());
        // The borrow `start::run` hands down, taken while the account still exists.
        let profile = configured_profile("vanishes");

        let mut config = crate::profile::load_config().expect("load config");
        assert!(
            config
                .find(&crate::profile::ProfileName::from("vanishes"))
                .is_some(),
            "fixture must start from a configured account, or the gate below \
             proves nothing"
        );
        let guard = RotationGuard::acquire(&crate::profile::ProfileName::from("vanishes"))
            .expect("rotation guard");
        crate::actions::delete_profile(
            &mut config,
            &crate::profile::ProfileName::from("vanishes"),
            false,
            &guard,
        )
        .expect("delete");
        drop(guard);
        assert!(
            crate::profile::load_config()
                .expect("reload config")
                .find(&crate::profile::ProfileName::from("vanishes"))
                .is_none(),
            "fixture must have removed the record it is about to start against"
        );

        // Mapped away because `ProfileRuntime` is not `Debug`; an Ok here also
        // tears the session down before the panic reports it.
        let err = ProfileRuntime::acquire(&profile, Isolation::Shared, &[], false)
            .map(|_| ())
            .expect_err("a start for a deleted account must fail loudly");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("vanishes") && msg.contains("clauth list"),
            "the refusal must name the account and the way out, got: {msg}"
        );

        // The gate precedes every write this hold does, so none of the acquire's
        // own side effects ran. It reaches wider than M2's placement claim: the
        // legs that run BEFORE the gate (`arm_rolling_from_disk` -> `load_profile`
        // -> `maybe_rewrite_config_toml`) are inert for a cleanly deleted name
        // only by CONVENTION — `effective_base_url(None, false, None)` returning
        // `None` and the default render round-tripping — and `atomic_write_600`
        // creates the missing parent. Should either drift, `load_profile` becomes
        // a resurrector sitting ahead of the gate and this assertion is what
        // reds. Do not weaken it to match the narrower claim it reads as.
        assert!(
            !crate::profile::profile_dir(&crate::profile::ProfileName::from("vanishes"))
                .expect("profile dir")
                .exists(),
            "a refused start must not put the deleted profile directory back"
        );
        assert!(
            crate::live_sessions::list().is_empty(),
            "a refused start must register no live row"
        );
    });
}

/// The mixed-version actor: a record removal that holds NO rotation lock,
/// landing between `acquire`'s rotation guard and its state flock.
///
/// This does NOT pin where the gate sits relative to the flock. The seam fires
/// before the flock acquisition, so a gate moved just below the seam reads the
/// same post-removal record and refuses identically. `refuse_if_unconfigured`'s
/// `debug_assert!` on `rank::State` is what pins the placement. What this pins
/// is that the actor exists and is refused at all, which no other test poses.
///
/// No SAME-VERSION mutation can pose this window, and that is why a gate placed
/// outside the hold survives the whole suite. `acquire` holds its `RotationGuard`
/// to the end of the function, and every mutation call site takes its own
/// through `actions::rotation_guard_for_mutation`, which is a `try_acquire` and
/// REFUSES rather than queues. The rotation guard is doing that work, not the
/// flock.
///
/// What the flock placement buys is the mutation holding NO rotation lock: a
/// clauth predating the guard witness on `actions::delete_profile`, where the
/// state flock is the only serialization point the two versions share. That is a
/// live mixed-version state, not a hypothetical, and the seam is what makes it
/// deterministic — no threads, no sleeps, nothing to schedule.
#[test]
fn acquire_refuses_a_record_removed_without_a_rotation_lock() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        fake_claude_home(tmp.path());
        let profile = configured_profile("mixedver");

        let err = ProfileRuntime::acquire_synced(&profile, Isolation::Shared, &[], false, || {
            // A record removal taking no rotation lock — the shape a clauth
            // predating the witness ships. Its own body still runs under
            // `with_state_lock`, which is the serialization this gate's
            // placement rests on.
            let mut config = crate::profile::load_config().expect("load config");
            assert!(
                config
                    .find(&crate::profile::ProfileName::from("mixedver"))
                    .is_some(),
                "the seam must fire while the account is still configured, \
                     or it poses nothing"
            );
            crate::lock::with_state_lock(|held| {
                config.remove(&crate::profile::ProfileName::from("mixedver"), held);
                Ok(())
            })
            .expect("remove record");
            crate::profile::save_app_state(&config.state).expect("save app state");
            std::fs::remove_dir_all(
                crate::profile::profile_dir(&crate::profile::ProfileName::from("mixedver"))
                    .expect("profile dir"),
            )
            .expect("remove the profile dir");
        })
        .map(|_| ())
        .expect_err("a record removed inside the window must still refuse");

        let msg = format!("{err:#}");
        assert!(
            msg.contains("mixedver") && msg.contains("was deleted or renamed"),
            "the refusal must be the gate's own, got: {msg}"
        );
        assert!(
            crate::live_sessions::list().is_empty(),
            "a refused start must register no live row"
        );
    });
}

/// The gate reads the RECORD, and the profile DIRECTORY is the neighbouring
/// question that fails here: `unsupported_swap_transport` — `start::run`'s last
/// `--with-fallback` gate — sits inside this same window and `mkdir_700`s the
/// profile root, so a delete's leftovers are back on disk before the acquire
/// runs. A directory-existence gate would pass this and rebuild the ghost.
#[test]
fn acquire_refuses_a_deleted_account_whose_directory_came_back() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        fake_claude_home(tmp.path());
        let profile = configured_profile("resurrected");

        let mut config = crate::profile::load_config().expect("load config");
        let guard = RotationGuard::acquire(&crate::profile::ProfileName::from("resurrected"))
            .expect("rotation guard");
        crate::actions::delete_profile(
            &mut config,
            &crate::profile::ProfileName::from("resurrected"),
            false,
            &guard,
        )
        .expect("delete");
        drop(guard);

        // The transport probe every `--with-fallback` start runs, verbatim.
        unsupported_swap_transport(&crate::profile::ProfileName::from("resurrected"))
            .expect("probe the transport");
        assert!(
            crate::profile::profile_dir(&crate::profile::ProfileName::from("resurrected"))
                .expect("profile dir")
                .exists(),
            "fixture must have put the directory back, or it poses nothing"
        );

        let err = ProfileRuntime::acquire(&profile, Isolation::Shared, &[], false)
            .map(|_| ())
            .expect_err("a start must follow the record, not the leftover directory");
        // The gate's own sentence, not just the name: every failure path in
        // `acquire` interpolates a path carrying the profile name, so a future
        // change that fails at `mkdir_700` instead would keep a name-only
        // assertion green with the gate gone.
        let msg = format!("{err:#}");
        assert!(
            msg.contains("resurrected") && msg.contains("was deleted or renamed"),
            "the refusal must be the gate's own, got: {msg}"
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
        let profile = configured_profile("isolated");
        let canonical = tmp
            .path()
            .join(".clauth")
            .join("profiles")
            .join("isolated")
            .join("credentials.json");
        fs::create_dir_all(canonical.parent().expect("canonical parent"))
            .expect("mkdir profile dir");
        fs::write(&canonical, CREDS_V2).expect("write canonical");

        let rt = ProfileRuntime::acquire(&profile, Isolation::Shared, &[], false).expect("acquire");
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

/// Pins the runtime-tree partition the MCP init note hands every model:
/// `runtime_paths_note` in `src/mcp/render.rs`. In a shared session every
/// top-level entry under `$CLAUDE_CONFIG_DIR` is a symlink onto
/// `~/.claude/<same-name>`, except three per-profile files. Those three are
/// `.claude.json`, `settings.json` and `.credentials.json`. A fourth
/// profile-local file or a changed link target leaves that note lying to every
/// session it reaches. Drives the real `acquire` rather than a hand-built
/// fixture, since a fixture agrees with whatever its author guessed the layout
/// to be.
#[cfg(unix)]
#[test]
fn acquire_builds_the_runtime_partition_the_mcp_note_describes() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let claude_home = fake_claude_home(tmp.path());
        // A realistic spread: one dir, one nested dir, one plain file, one
        // dotfile, plus the two exclusions the walk must not link.
        fs::create_dir_all(claude_home.join("projects")).expect("mkdir projects");
        fs::create_dir_all(claude_home.join("plugins").join("repos")).expect("mkdir plugins");
        fs::write(claude_home.join("history.jsonl"), b"{}").expect("write history");
        fs::write(claude_home.join(".foo.json"), b"{}").expect("write dotfile");
        fs::write(claude_home.join("settings.json"), br#"{"home":true}"#).expect("write settings");
        fs::write(claude_home.join(".credentials.json"), CREDS_V1).expect("write live creds");
        fs::write(tmp.path().join(".claude.json"), br#"{"numStartups":1}"#)
            .expect("write global .claude.json");

        // Pre-stage the profile's canonical credentials: without them the
        // runtime's `.credentials.json` has no per-profile target to link to.
        let profile = configured_profile("partition");
        let canonical = tmp
            .path()
            .join(".clauth")
            .join("profiles")
            .join("partition")
            .join("credentials.json");
        fs::create_dir_all(canonical.parent().expect("canonical parent"))
            .expect("mkdir profile dir");
        fs::write(&canonical, CREDS_V2).expect("write canonical");

        let rt = ProfileRuntime::acquire(&profile, Isolation::Shared, &[], false).expect("acquire");
        let runtime = rt.config_dir();

        // 1. The partition holds exhaustively: every top-level entry is one of
        // the three per-profile names or a symlink onto ~/.claude/<same-name>.
        for name in dir_entry_names(runtime) {
            if matches!(
                name.as_str(),
                "settings.json" | ".claude.json" | ".credentials.json"
            ) {
                continue;
            }
            let path = runtime.join(&name);
            assert!(
                path.symlink_metadata()
                    .expect("runtime entry meta")
                    .file_type()
                    .is_symlink(),
                "`{name}` is not a per-profile file and is not a symlink"
            );
            assert_eq!(
                fs::read_link(&path).expect("read link"),
                claude_home.join(&name),
                "`{name}` must link onto ~/.claude/{name}"
            );
        }

        // 2. The three per-profile names are exactly right: settings.json and
        // .claude.json are regular files, .credentials.json links into the
        // profile's canonical store rather than into ~/.claude/.
        for file in [runtime.join("settings.json"), runtime.join(".claude.json")] {
            let meta = file.symlink_metadata().expect("meta");
            assert!(
                meta.file_type().is_file(),
                "{} must be a per-profile regular file",
                file.display()
            );
        }
        let creds = runtime.join(".credentials.json");
        assert!(
            creds
                .symlink_metadata()
                .expect("creds meta")
                .file_type()
                .is_symlink(),
            ".credentials.json must be a symlink"
        );
        assert_eq!(
            fs::read_link(&creds).expect("read creds link"),
            canonical,
            ".credentials.json must link into the profile's canonical store ({}), not ~/.claude/",
            canonical.display()
        );

        // 3. Nothing dropped: every ~/.claude/ top-level entry except the two
        // exclusions has a counterpart in the runtime tree.
        for name in dir_entry_names(&claude_home) {
            if name == "settings.json" || name == ".credentials.json" {
                continue;
            }
            assert!(
                runtime.join(&name).symlink_metadata().is_ok(),
                "~/.claude/{name} was dropped from the runtime tree"
            );
        }

        drop(rt);
    });
}

/// Regression: one process holding two concurrent sessions of the same
/// profile+flavor must not collide on the session file. Before the per-acquire
/// `-<n>` suffix both keyed `sessions/<pid>`, so the second `acquire` blocked
/// forever on the first's `flock(2)` — the background-`delegate` hang where a
/// second same-profile job never spawned a session. Both must register live.
#[test]
fn acquire_twice_same_process_counts_two_sessions() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        fake_claude_home(tmp.path());
        let profile = configured_profile("concurrent");

        let rt1 = ProfileRuntime::acquire(&profile, Isolation::Shared, &[], false)
            .expect("first acquire");
        // Pre-fix this second acquire blocks forever on the shared PID flock.
        let rt2 = ProfileRuntime::acquire(&profile, Isolation::Shared, &[], false)
            .expect("second acquire");

        assert_eq!(
            live_session_count(&crate::profile::ProfileName::from("concurrent")),
            2,
            "two concurrent same-process sessions must both register live"
        );

        let rt1_runtime = rt1.config_dir().to_path_buf();

        drop(rt2);
        assert!(
            rt1_runtime.is_dir(),
            "the surviving session's runtime is untouched by a sibling's teardown"
        );
        assert_eq!(
            live_session_count(&crate::profile::ProfileName::from("concurrent")),
            1
        );

        drop(rt1);
        assert!(
            !rt1_runtime.exists(),
            "runtime torn down once its own session drops"
        );
    });
}

/// Every `clauth start` session gets its OWN tree — the shared flavor included,
/// which two same-profile sessions used to share. Pins the exact per-session
/// names and the `runtime<rest>` ↔ `sessions<rest>` pairing they rest on.
#[test]
fn two_shared_sessions_get_independent_trees() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        if !host_poses(tmp.path(), "a runtime tree per session") {
            return;
        }
        fake_claude_home(tmp.path());
        let profile = configured_profile("twin");

        let a = ProfileRuntime::acquire(&profile, Isolation::Shared, &[], false)
            .expect("first acquire");
        let b = ProfileRuntime::acquire(&profile, Isolation::Shared, &[], false)
            .expect("second acquire");

        assert_ne!(
            a.config_dir(),
            b.config_dir(),
            "two shared sessions of one profile must not share a runtime tree"
        );
        assert_ne!(
            a.sessions_dir(),
            b.sessions_dir(),
            "two shared sessions of one profile must not share a marker dir"
        );

        let profile_dir = tmp.path().join(".clauth").join("profiles").join("twin");
        for rt in [&a, &b] {
            let sid = sid_of(rt.config_dir());
            assert_eq!(rt.config_dir(), profile_dir.join(format!("runtime-{sid}")));
            assert_eq!(
                rt.sessions_dir(),
                profile_dir.join(format!("sessions-{sid}"))
            );
            assert_eq!(
                dir_entry_names(rt.sessions_dir()),
                vec![sid],
                "a marker dir holds this session's marker and no other's"
            );
            assert!(
                rt.config_dir().join("settings.json").is_file(),
                "each tree is built independently, not shared"
            );
        }
        assert_eq!(
            live_session_count(&crate::profile::ProfileName::from("twin")),
            2
        );

        drop(b);
        drop(a);
    });
}

/// THE UPGRADE GATE. A clauth process built before the per-session layout probes
/// exactly `<profile>/sessions[-isolated]`. Without a marker there its
/// `has_live_session` reads a live new-layout session as idle. That old binary
/// still gates rotation on liveness, so it would spend the single-use refresh
/// token the session holds — costing that session one failed refresh, not the
/// account. Post-upgrade the old binary is the DEFAULT supervisor until the next
/// restart (`clauth daemon --replace` exists for exactly that).
///
/// The `live_sessions_at` assertion below IS the old binary's predicate, applied
/// to the old binary's path.
#[test]
fn acquire_stamps_the_pre_upgrade_liveness_marker_for_both_flavors() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        fake_claude_home(tmp.path());

        for (name, isolation, legacy_dir) in [
            ("upgrade-shared", Isolation::Shared, "sessions"),
            ("upgrade-iso", Isolation::Isolated, "sessions-isolated"),
        ] {
            let profile = configured_profile(name);
            let rt = ProfileRuntime::acquire(&profile, isolation, &[], false).expect("acquire");
            let sid = live_sid(&rt);

            let legacy = tmp
                .path()
                .join(".clauth")
                .join("profiles")
                .join(name)
                .join(legacy_dir);
            let legacy_marker = legacy.join(&sid);
            assert!(
                legacy_marker.is_file(),
                "no upgrade-compat marker at {}",
                legacy_marker.display()
            );
            assert!(
                is_session_alive(&legacy_marker),
                "the upgrade-compat marker must be flock-held for the session's life"
            );
            assert_eq!(
                live_sessions_at(&legacy),
                Some(1),
                "a pre-upgrade clauth probes exactly {legacy_dir} and must see this session"
            );
            assert_eq!(
                live_session_count(&crate::profile::ProfileName::from(name)),
                1,
                "the compat marker and the per-session marker are ONE session, not two"
            );

            drop(rt);

            assert!(
                !legacy_marker.exists(),
                "teardown must drop the upgrade-compat marker"
            );
            assert!(
                !legacy.exists(),
                "the last session out removes the shared compat dir"
            );
            assert_eq!(
                live_session_count(&crate::profile::ProfileName::from(name)),
                0
            );
        }
    });
}

/// A live foreign holder of this session's OWN marker must never park `acquire`.
/// Under [`LinkMode::Fake`] the session's marker sits at the bare-stem
/// `sessions/<sid>`, the same path `stamp_legacy_marker` guards with `try_lock`
/// under `Real`, so a colliding sid reaches the claim at a branch that has no
/// such guard. That claim runs inside the state flock, so a blocking wait there
/// wedges every other clauth process on the home, not just this one.
///
/// Runs `acquire` on a worker thread and fails on a timeout rather than hanging:
/// a regression here parks a thread inside `with_state_lock` and would otherwise
/// take the rest of the suite down with it, reporting nothing.
#[test]
fn a_foreign_holder_of_our_own_marker_never_blocks_acquire() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        with_link_mode(LinkMode::Fake, || {
            fake_claude_home(tmp.path());

            // Same sid arithmetic as the compat-marker tests: `acquire` mints
            // exactly one id, so seq+1 is the one it is about to take.
            let probe = SessionId::mint();
            let (pid, seq) = probe.as_str().split_once('-').expect("<pid>-<seq>");
            let colliding_sid = format!("{pid}-{}", seq.parse::<u64>().expect("seq") + 1);

            let sessions = tmp
                .path()
                .join(".clauth")
                .join("profiles")
                .join("collide")
                .join("sessions");
            fs::create_dir_all(&sessions).expect("mkdir sessions");
            let foreign_marker = sessions.join(&colliding_sid);
            let held = open_pid_file(&foreign_marker).expect("open foreign marker");
            held.lock().expect("lock foreign marker");

            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let profile = configured_profile("collide");
                let claimed =
                    ProfileRuntime::acquire(&profile, Isolation::Shared, &[], false).map(|rt| {
                        // Read the dir while the session still holds its marker:
                        // teardown unlinks it on drop.
                        let names = dir_entry_names(rt.sessions_dir());
                        drop(rt);
                        names
                    });
                let _ = tx.send(claimed);
            });

            let names = rx
                .recv_timeout(std::time::Duration::from_secs(20))
                .expect("acquire parked on a marker a live holder owns")
                .expect("acquire");

            assert!(
                names.contains(&colliding_sid),
                "the foreign holder's marker was taken or renamed: {names:?}"
            );
            assert_eq!(
                names.len(),
                2,
                "the session must stamp a marker of its own beside the foreign one: {names:?}"
            );
            assert!(
                is_session_alive(&foreign_marker),
                "the foreign holder's lock must be untouched"
            );
            drop(held);
        });
    });
}

/// `stamp_legacy_marker` must decline rather than block when the marker is
/// already held, and leave the file exactly as it found it.
#[test]
fn stamp_legacy_marker_declines_a_marker_another_holder_owns() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let marker = tmp.path().join("sessions").join("4242-0");
    fs::create_dir_all(marker.parent().expect("parent")).expect("mkdir sessions");
    let held = open_pid_file(&marker).expect("open marker");
    held.lock().expect("lock marker");

    assert!(
        stamp_legacy_marker(&marker).is_none(),
        "a marker another holder owns must not be adopted"
    );
    assert!(marker.is_file(), "declining must not disturb the file");
    assert!(
        is_session_alive(&marker),
        "the holder's flock must survive the decline"
    );

    drop(held);
    assert!(
        stamp_legacy_marker(&marker).is_some(),
        "an unlocked marker is free to take"
    );
}

/// Teardown must not unlink a marker this session never owned. `stamp_legacy_marker`
/// yields `None` when `try_lock` loses to a live process that minted the same sid,
/// and unlinking on that path deletes a FOREIGN session's liveness signal — the
/// same rotation burn the compat marker exists to prevent.
#[test]
fn teardown_leaves_a_pre_upgrade_marker_it_never_owned() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        if !host_poses(
            tmp.path(),
            "a compat marker separate from the session's own",
        ) {
            return;
        }
        fake_claude_home(tmp.path());

        // `acquire` mints exactly one `SessionId`, and `with_fake_home` holds the
        // lock that is the only way into `acquire`, so the counter cannot move
        // between this probe and the acquire below. The assert after the acquire
        // is what catches that arithmetic going stale.
        let probe = SessionId::mint();
        let (pid, seq) = probe.as_str().split_once('-').expect("<pid>-<seq>");
        let foreign_sid = format!("{pid}-{}", seq.parse::<u64>().expect("seq") + 1);

        let legacy = tmp
            .path()
            .join(".clauth")
            .join("profiles")
            .join("foreign")
            .join("sessions");
        fs::create_dir_all(&legacy).expect("mkdir legacy sessions");
        let foreign_marker = legacy.join(&foreign_sid);
        let held = open_pid_file(&foreign_marker).expect("open foreign marker");
        held.lock().expect("lock foreign marker");

        let profile = configured_profile("foreign");
        let rt = ProfileRuntime::acquire(&profile, Isolation::Shared, &[], false).expect("acquire");
        assert_eq!(
            live_sid(&rt),
            foreign_sid,
            "sid arithmetic drifted — `acquire` no longer mints exactly one id, \
             so this test is no longer posing the collision it claims to"
        );

        drop(rt);

        assert!(
            foreign_marker.is_file(),
            "teardown unlinked a liveness marker owned by another live process"
        );
        assert!(
            is_session_alive(&foreign_marker),
            "the foreign holder's flock must be untouched"
        );
        drop(held);
    });
}

/// A wedged peer holds the state flock past the deadline, then releases it:
/// teardown's first acquisition times out and its bounded retry — the SAME hold,
/// never a split — re-acquires and removes the session's own files. The deadline
/// is shortened via `set_state_lock_timeout_override` so the wedge poses without
/// a real 25 s wait, and the release is wired to `on_teardown_acquire_timeout`
/// so the first timeout deterministically precedes the retry rather than racing
/// it. Both overrides are thread-local, so they die with this test's thread.
#[test]
fn teardown_racing_a_wedged_peer_removes_its_own_files_within_one_retry() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        fake_claude_home(tmp.path());
        let profile = configured_profile("wedged");
        crate::lock::set_state_lock_timeout_override(Some(Duration::from_millis(150)));

        let rt = ProfileRuntime::acquire(&profile, Isolation::Shared, &[], false).expect("acquire");
        let runtime = rt.config_dir().to_path_buf();
        let sessions = rt.sessions_dir().to_path_buf();

        // A wedged peer: a second open file description holding the flock, which
        // conflicts with the state flock exactly as a second process would.
        let lock_path = crate::profile::clauth_dir()
            .expect("clauth dir")
            .join(crate::lock::LOCK_FILENAME);
        let holder = std::sync::Arc::new(std::sync::Mutex::new(Some(
            crate::profile::open_state_file(&lock_path).expect("open holder"),
        )));
        holder
            .lock()
            .expect("holder mutex")
            .as_ref()
            .expect("holder file")
            .lock()
            .expect("hold the flock");

        // Release the wedge on the first teardown-acquire timeout, so the retry
        // is the acquisition that succeeds.
        let holder_for_hook = std::sync::Arc::clone(&holder);
        set_teardown_timeout_hook(Some(Box::new(move || {
            *holder_for_hook.lock().expect("holder mutex") = None;
        })));

        // Full drop: the pre-teardown sync legs fail fast under the shortened
        // deadline, then teardown's first acquire times out, the hook releases
        // the wedge, and the retry acquires the same hold and cleans up.
        drop(rt);

        assert!(
            !runtime.exists(),
            "teardown must remove its own runtime tree within one retry"
        );
        assert!(
            !sessions.exists(),
            "teardown must remove its own marker dir within one retry"
        );
        assert_eq!(
            live_session_count(&crate::profile::ProfileName::from("wedged")),
            0,
            "teardown must release every liveness marker within one retry"
        );
        assert!(
            holder.lock().expect("holder mutex").is_none(),
            "the first teardown acquire must time out and release the wedge via the retry hook"
        );

        set_teardown_timeout_hook(None);
        crate::lock::set_state_lock_timeout_override(None);
    });
}

/// The retry is bounded, not infinite: a wedge held past the whole retry budget
/// makes teardown give up after retrying, logging rather than hanging. Pins the
/// bound by COUNTING the timed-out acquires. The expected count is a LITERAL,
/// not `TEARDOWN_ACQUIRE_RETRIES`: re-tuning the bound (down to zero included)
/// must red this test and force a conscious re-derivation, where an assertion
/// keyed on the constant would silently track the change.
#[test]
fn teardown_retries_a_persistent_wedge_then_gives_up() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        fake_claude_home(tmp.path());
        let profile = configured_profile("stuck");
        crate::lock::set_state_lock_timeout_override(Some(Duration::from_millis(50)));

        let rt = ProfileRuntime::acquire(&profile, Isolation::Shared, &[], false).expect("acquire");
        let runtime = rt.config_dir().to_path_buf();

        let lock_path = crate::profile::clauth_dir()
            .expect("clauth dir")
            .join(crate::lock::LOCK_FILENAME);
        let holder = crate::profile::open_state_file(&lock_path).expect("open holder");
        holder.lock().expect("hold the flock");

        let timeouts = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let count = std::sync::Arc::clone(&timeouts);
        set_teardown_timeout_hook(Some(Box::new(move || {
            count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        })));

        // The wedge never releases: every acquisition (1 + RETRIES) times out and
        // teardown gives up, leaving the tree for the next run's GC.
        drop(rt);

        assert_eq!(
            timeouts.load(std::sync::atomic::Ordering::SeqCst) as u32,
            2,
            "teardown must retry exactly the bounded number of times before giving up"
        );
        assert!(
            runtime.exists(),
            "a wedge that never releases must leave the tree for the next run's GC"
        );

        drop(holder);
        set_teardown_timeout_hook(None);
        crate::lock::set_state_lock_timeout_override(None);
    });
}

/// Two same-profile sessions share the one compat dir, so it may only go when
/// the last of them releases.
#[test]
fn the_pre_upgrade_marker_dir_survives_until_the_last_session_leaves() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        if !host_poses(
            tmp.path(),
            "a compat marker separate from the session's own",
        ) {
            return;
        }
        fake_claude_home(tmp.path());
        let profile = configured_profile("upgrade-twin");
        let legacy = tmp
            .path()
            .join(".clauth")
            .join("profiles")
            .join("upgrade-twin")
            .join("sessions");

        let a = ProfileRuntime::acquire(&profile, Isolation::Shared, &[], false)
            .expect("first acquire");
        let b = ProfileRuntime::acquire(&profile, Isolation::Shared, &[], false)
            .expect("second acquire");

        assert_eq!(
            live_sessions_at(&legacy),
            Some(2),
            "both sessions must be visible to a pre-upgrade probe"
        );
        assert_eq!(
            live_session_count(&crate::profile::ProfileName::from("upgrade-twin")),
            2,
            "two sessions, four markers, still two sessions"
        );

        drop(b);
        assert_eq!(live_sessions_at(&legacy), Some(1));
        assert!(
            legacy.is_dir(),
            "the compat dir is shared — it must survive"
        );
        assert_eq!(
            live_session_count(&crate::profile::ProfileName::from("upgrade-twin")),
            1
        );

        drop(a);
        assert!(!legacy.exists());
    });
}

/// Teardown is per session: dropping one of two same-profile shared sessions
/// discards only its own tree and marker.
#[test]
fn dropping_one_shared_session_leaves_the_sibling_intact() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        if !host_poses(tmp.path(), "a runtime tree per session") {
            return;
        }
        fake_claude_home(tmp.path());
        let profile = configured_profile("survivor");

        let a = ProfileRuntime::acquire(&profile, Isolation::Shared, &[], false)
            .expect("first acquire");
        let b = ProfileRuntime::acquire(&profile, Isolation::Shared, &[], false)
            .expect("second acquire");

        let a_runtime = a.config_dir().to_path_buf();
        let a_sessions = a.sessions_dir().to_path_buf();
        let a_marker = a_sessions.join(sid_of(&a_runtime));
        let b_runtime = b.config_dir().to_path_buf();
        let b_sessions = b.sessions_dir().to_path_buf();
        fs::write(a_runtime.join("survivor.txt"), b"keep me").expect("seed a's tree");

        drop(b);

        assert!(
            !b_runtime.exists(),
            "the dropped session's tree is discarded"
        );
        assert!(
            !b_sessions.exists(),
            "the dropped session's marker dir goes with it"
        );
        assert!(a_runtime.is_dir(), "the sibling's tree must survive");
        assert_eq!(
            fs::read(a_runtime.join("survivor.txt")).expect("read sibling file"),
            b"keep me",
            "the sibling's tree contents must be untouched"
        );
        assert_eq!(dir_entry_names(&a_sessions), vec![sid_of(&a_runtime)]);
        assert!(
            is_session_alive(&a_marker),
            "the sibling's marker must still be flock-held"
        );
        assert_eq!(
            live_session_count(&crate::profile::ProfileName::from("survivor")),
            1
        );

        drop(a);
        assert!(!a_runtime.exists());
        assert!(!a_sessions.exists());
    });
}

// ── LinkMode::Fake keeps the shared (profile, flavor) tree ────────────────────

/// The naming rule as a unit. `LinkMode::Real` keys each session's pair by its
/// own `<sid>`; `LinkMode::Fake` returns the bare stem every session of that
/// profile+flavor shares. In all four cases the two names must satisfy the
/// module's one layout rule (`runtime<rest>` ↔ `sessions<rest>`) and both strict
/// predicates, so no enumeration can miss a dir the naming produced.
#[test]
fn paired_dir_names_key_on_link_mode() {
    let sid = "4242-7";
    let cases = [
        (Isolation::Shared, LinkMode::Fake, "runtime", "sessions"),
        (
            Isolation::Isolated,
            LinkMode::Fake,
            "runtime-isolated",
            "sessions-isolated",
        ),
        (
            Isolation::Shared,
            LinkMode::Real,
            "runtime-4242-7",
            "sessions-4242-7",
        ),
        (
            Isolation::Isolated,
            LinkMode::Real,
            "runtime-isolated-4242-7",
            "sessions-isolated-4242-7",
        ),
    ];
    for (isolation, mode, want_runtime, want_sessions) in cases {
        let (runtime, sessions) = paired_dir_names(isolation, sid, mode);
        assert_eq!(runtime, want_runtime, "{isolation:?}/{mode:?} runtime name");
        assert_eq!(
            sessions, want_sessions,
            "{isolation:?}/{mode:?} sessions name"
        );
        assert_eq!(
            paired_sessions_name(&runtime).as_deref(),
            Some(sessions.as_str()),
            "{runtime} must pair with {sessions}"
        );
        assert_eq!(
            paired_runtime_name(&sessions).as_deref(),
            Some(runtime.as_str()),
            "{sessions} must pair back to {runtime}"
        );
        assert!(is_runtime_dir_name(&runtime), "GC must reach {runtime}");
        assert!(is_sessions_dir_name(&sessions), "GC must reach {sessions}");
        assert_eq!(
            is_shared_runtime_dir_name(&runtime),
            isolation == Isolation::Shared,
            "{runtime} flavor must be readable off the name alone"
        );
    }
}

/// Under `LinkMode::Fake` the tree is a recursive COPY of `~/.claude/`, so two
/// shared sessions of one profile land on ONE bare-stem tree. The real-symlink
/// counterpart — where they must NOT — is
/// `two_shared_sessions_get_independent_trees`.
#[test]
fn fake_mode_shares_one_tree_across_two_sessions() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        with_link_mode(LinkMode::Fake, || {
            fake_claude_home(tmp.path());
            let profile = configured_profile("faketwin");

            let a = ProfileRuntime::acquire(&profile, Isolation::Shared, &[], false)
                .expect("first acquire");
            let b = ProfileRuntime::acquire(&profile, Isolation::Shared, &[], false)
                .expect("second acquire");

            let profile_dir = tmp.path().join(".clauth").join("profiles").join("faketwin");
            assert_eq!(a.config_dir(), profile_dir.join("runtime"));
            assert_eq!(b.config_dir(), profile_dir.join("runtime"));
            assert_eq!(a.sessions_dir(), profile_dir.join("sessions"));
            assert_eq!(b.sessions_dir(), profile_dir.join("sessions"));

            let mut want = vec![
                a.swap.session.as_str().to_string(),
                b.swap.session.as_str().to_string(),
            ];
            want.sort();
            assert_ne!(want[0], want[1], "the two sessions must still be distinct");
            assert_eq!(
                dir_entry_names(a.sessions_dir()),
                want,
                "one shared marker dir carrying both sessions' markers"
            );

            drop(b);
            drop(a);
        });
    });
}

/// Session 2 must neither wipe nor rebuild the tree session 1 is using — that is
/// the whole point of sharing it. The witness has to be a file a rebuild cannot
/// put back, and an ordinary sentinel is not one: the fake-mode tree mirror is
/// BIDIRECTIONAL, session 1's own watchdog publishes tree files out to
/// `~/.claude`, and a wiping rebuild then copies them straight back in. Measured
/// at 1.5 s of watchdog: the sentinel reached `~/.claude` and the assertion below
/// passed over an unconditional wipe.
///
/// A staging-shaped name is the one class every walk over this tree skips by
/// contract — the mirror's (`union_children`), the acquire-time build's, and
/// `copy_tree`'s recursion under it — so it lives in the tree and nowhere else.
/// `published` is the positive control: same tree, ordinary name, and it DOES
/// cross over, which is what makes the sentinel's absence the exemption rather
/// than an idle mirror.
#[test]
fn fake_mode_second_session_does_not_rebuild_the_tree() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        with_link_mode(LinkMode::Fake, || {
            let claude_home = fake_claude_home(tmp.path());
            let profile = configured_profile("fakecopy");

            let a = ProfileRuntime::acquire(&profile, Isolation::Shared, &[], false)
                .expect("first acquire");

            let sentinel = ".session-one-was-here.tmp.pin";
            assert!(
                crate::watchdog::is_staging(std::ffi::OsStr::new(sentinel)),
                "the sentinel is unrestorable only while the walks skip its name"
            );
            let published = "session-one-published.txt";
            fs::write(a.config_dir().join(sentinel), b"do not re-copy me").expect("seed sentinel");
            fs::write(a.config_dir().join(published), b"ordinary tree file").expect("seed control");

            mirror_tree(&claude_home, a.config_dir()).expect("mirror");
            assert!(
                claude_home.join(published).exists(),
                "the mirror must publish an ordinary tree file outward, or the check \
                 below proves nothing about the sentinel"
            );
            assert!(
                !claude_home.join(sentinel).exists(),
                "the mirror published the sentinel, so a rebuild could restore it and \
                 this test would prove nothing"
            );

            let b = ProfileRuntime::acquire(&profile, Isolation::Shared, &[], false)
                .expect("second acquire");

            assert_eq!(
                b.config_dir(),
                a.config_dir(),
                "the second session must reuse the first's tree, not pay a second copy"
            );
            assert_eq!(
                fs::read(a.config_dir().join(sentinel)).expect("read sentinel"),
                b"do not re-copy me",
                "the second acquire wiped or rebuilt a tree a live sibling is using"
            );

            drop(b);
            drop(a);
        });
    });
}

/// Liveness over a shared marker dir. A `has_live_session` false negative lets
/// a delete or disable through against a running session, so the count must
/// stay per SESSION even though two sessions share one dir — and the tree may
/// only be discarded by the last one out.
#[test]
fn fake_mode_liveness_counts_both_shared_sessions() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        with_link_mode(LinkMode::Fake, || {
            fake_claude_home(tmp.path());
            let profile = configured_profile("fakegate");

            let a = ProfileRuntime::acquire(&profile, Isolation::Shared, &[], false)
                .expect("first acquire");
            let b = ProfileRuntime::acquire(&profile, Isolation::Shared, &[], false)
                .expect("second acquire");
            let tree = a.config_dir().to_path_buf();
            let markers = a.sessions_dir().to_path_buf();

            assert!(has_live_session(&crate::profile::ProfileName::from(
                "fakegate"
            )));
            assert_eq!(
                live_session_count(&crate::profile::ProfileName::from("fakegate")),
                2
            );
            assert_eq!(
                dir_entry_names(&markers).len(),
                2,
                "one shared marker dir must carry a marker per session"
            );

            drop(b);
            assert!(has_live_session(&crate::profile::ProfileName::from(
                "fakegate"
            )));
            assert_eq!(
                live_session_count(&crate::profile::ProfileName::from("fakegate")),
                1
            );
            assert!(
                tree.is_dir(),
                "the shared tree must survive while a sibling still holds it"
            );

            drop(a);
            assert!(!has_live_session(&crate::profile::ProfileName::from(
                "fakegate"
            )));
            assert_eq!(
                live_session_count(&crate::profile::ProfileName::from("fakegate")),
                0
            );
            assert!(!tree.exists(), "the last session out discards the tree");
            assert!(!markers.exists());
        });
    });
}

/// A registry row carries the profile, the flavor, and the session id — but NOT
/// the transport. Probing only the per-session marker path drops every fake-mode
/// row the first time any sweep runs, while its session is live.
#[test]
fn fake_mode_registry_row_survives_gc() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        with_link_mode(LinkMode::Fake, || {
            fake_claude_home(tmp.path());
            let profile = configured_profile("fakerow");

            let rt =
                ProfileRuntime::acquire(&profile, Isolation::Shared, &[], false).expect("acquire");
            let sid = rt.swap.session.as_str().to_string();

            gc_stale_runtimes();

            let left: Vec<String> = crate::live_sessions::list()
                .into_iter()
                .map(|r| r.session_id)
                .collect();
            assert_eq!(
                left,
                vec![sid],
                "GC reaped a LIVE fake-mode session's registry row"
            );
            assert!(
                rt.config_dir().is_dir(),
                "GC must spare the live shared tree too"
            );

            drop(rt);
        });
    });
}

/// Under `LinkMode::Fake` the session's own marker ALREADY sits at the
/// pre-per-session path a pre-layout clauth probes, so there is no second marker
/// to stamp. Stamping one anyway would `try_lock` that same path against this
/// process's own fd, fail, and log "not lockable" on every fake-mode start. The
/// absence is structural: `legacy_marker` is `None`, so the stamp is never
/// reached.
#[test]
fn fake_mode_stamps_no_second_compat_marker() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        with_link_mode(LinkMode::Fake, || {
            fake_claude_home(tmp.path());

            for (name, isolation, legacy_dir) in [
                ("fakecompat-shared", Isolation::Shared, "sessions"),
                ("fakecompat-iso", Isolation::Isolated, "sessions-isolated"),
            ] {
                let profile = configured_profile(name);
                let rt = ProfileRuntime::acquire(&profile, isolation, &[], false).expect("acquire");

                assert_eq!(
                    rt.legacy_marker, None,
                    "{name}: a shared-tree session's own marker IS the compat marker"
                );
                assert!(
                    rt.legacy_lock.is_none(),
                    "{name}: nothing to lock when nothing is stamped"
                );

                let legacy = tmp
                    .path()
                    .join(".clauth")
                    .join("profiles")
                    .join(name)
                    .join(legacy_dir);
                assert_eq!(
                    rt.sessions_dir(),
                    legacy,
                    "{name}: the session's marker dir must BE the pre-upgrade path"
                );
                assert_eq!(
                    live_sessions_at(&legacy),
                    Some(1),
                    "{name}: a pre-upgrade clauth probes exactly {legacy_dir} and must see this session"
                );
                assert_eq!(
                    live_session_count(&crate::profile::ProfileName::from(name)),
                    1,
                    "{name}: one marker, one session"
                );

                drop(rt);

                assert!(!legacy.exists(), "{name}: the last session out removes it");
                assert_eq!(
                    live_session_count(&crate::profile::ProfileName::from(name)),
                    0
                );
            }
        });
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

/// The DIRECTORY-link half of the prune, which the file-symlink case above
/// cannot reach. On Windows `remove_file` clears a dangling file symlink but
/// answers os error 5 on a dangling junction or directory symlink and leaves it
/// standing (measured on Windows 11, elevated and with the symlink privilege
/// stripped alike); a survivor is permanent, because `build_runtime_dir`'s
/// re-walk skips any entry whose `symlink_metadata` succeeds.
///
/// Calls `prune_dangling_links` directly rather than through `build_runtime_dir`
/// so the fixture needs no `LinkMode::Real`, which a Windows box outside
/// Developer Mode cannot pose. On unix `remove_file` unlinks any symlink, so
/// this leg can only ever red on Windows — which is the whole reason it must not
/// be a unix-only pin.
#[test]
fn prune_removes_a_dangling_directory_link() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let runtime = tmp.path().join("runtime");
    fs::create_dir_all(&runtime).expect("mkdir runtime");
    let target = tmp.path().join("skills-target");
    fs::create_dir_all(&target).expect("mkdir target");

    let dangling = runtime.join("skills");
    pose_dir_link(&dangling, &target);
    fs::remove_dir(&target).expect("drop the target");
    assert!(
        dangling.symlink_metadata().is_ok(),
        "the link itself is still there"
    );
    assert!(!dangling.exists(), "its target is gone");

    // Positive controls on the dimension the guard reads. The EMPTY one is the
    // load-bearing half: a non-empty directory survives `remove_dir` on its own
    // merits, so only an empty one can tell the guard from the call's own
    // refusal, and it is the single entry an unguarded `remove_dir` would take.
    let keep_full = runtime.join("projects");
    fs::create_dir_all(keep_full.join("nested")).expect("mkdir full keeper");
    let keep_empty = runtime.join("paste-cache");
    fs::create_dir_all(&keep_empty).expect("mkdir empty keeper");

    prune_dangling_links(&runtime).expect("prune");

    assert!(
        dangling.symlink_metadata().is_err(),
        "a dangling directory link must be pruned, not left for the re-walk to skip forever"
    );
    assert!(
        keep_full.join("nested").is_dir(),
        "a real directory is never touched"
    );
    assert!(
        keep_empty.is_dir(),
        "an EMPTY real directory is never touched either — the guard decides that, not remove_dir"
    );
}

// ── isolation liveness + GC ──────────────────────────────────────────────────

/// THE LIVENESS GATE behind delete and disable. Every session now keys its
/// marker dir by session id, so the gate has to enumerate the profile dir
/// rather than probe two fixed names. A false negative here pulls an account
/// out from under a running session, so both flavors are pinned.
#[test]
fn has_live_session_sees_a_per_session_dir_of_either_flavor() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let profiles = tmp.path().join(".clauth").join("profiles");
        for (profile, sessions_name, sid) in [
            ("gate-shared", "sessions-31337-0", "31337-0"),
            ("gate-iso", "sessions-isolated-31337-1", "31337-1"),
        ] {
            let sessions = profiles.join(profile).join(sessions_name);
            fs::create_dir_all(&sessions).expect("mkdir sessions");
            let marker = open_pid_file(&sessions.join(sid)).expect("open marker");
            marker.lock().expect("lock marker");

            assert!(
                has_live_session(&crate::profile::ProfileName::from(profile)),
                "a live marker in {sessions_name} must gate rotation"
            );
            assert_eq!(
                live_session_count(&crate::profile::ProfileName::from(profile)),
                1
            );

            drop(marker);
            // The probe is deliberately fail-alive (any try_lock I/O error reads
            // as "alive" — see `is_session_alive`), so only a PERSISTENTLY-alive
            // reading after the holder dropped is a regression.
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            while has_live_session(&crate::profile::ProfileName::from(profile))
                && std::time::Instant::now() < deadline
            {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            assert!(!has_live_session(&crate::profile::ProfileName::from(
                profile
            )));
            assert_eq!(
                live_session_count(&crate::profile::ProfileName::from(profile)),
                0
            );
        }
    });
}

/// The gate's fail-open must not cover the ENUMERATION step. `<profile>/` exists
/// for every configured profile (it holds `config.toml`, `credentials.json` and
/// the session dirs), so its unreadability is not the idle case — a transient
/// EMFILE/EACCES reading as "no sessions" would unblock a rotation against a live
/// session. Only a genuinely absent dir is idle.
#[cfg(unix)]
#[test]
fn an_unreadable_profile_dir_reads_as_live_not_idle() {
    use std::os::unix::fs::PermissionsExt;

    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let profile = tmp
            .path()
            .join(".clauth")
            .join("profiles")
            .join("unreadable");
        let sessions = profile.join("sessions-9001-0");
        fs::create_dir_all(&sessions).expect("mkdir sessions");
        fs::write(sessions.join("9001-0"), b"").expect("dead marker");

        // Control: readable and genuinely idle.
        assert!(!has_live_session(&crate::profile::ProfileName::from(
            "unreadable"
        )));
        // Control: never configured at all is still idle, not unknown.
        assert!(!has_live_session(&crate::profile::ProfileName::from(
            "never-started"
        )));

        fs::set_permissions(&profile, fs::Permissions::from_mode(0o000)).expect("chmod");
        if fs::read_dir(&profile).is_ok() {
            // Running with rights that ignore the mode (root); the probe cannot
            // be posed, so assert nothing rather than pass vacuously.
            fs::set_permissions(&profile, fs::Permissions::from_mode(0o700)).expect("restore");
            return;
        }

        assert!(
            has_live_session(&crate::profile::ProfileName::from("unreadable")),
            "an unreadable profile dir must read as live — a spurious false burns the chain"
        );
        assert_eq!(
            live_session_count(&crate::profile::ProfileName::from("unreadable")),
            1,
            "the count must not contradict the gate within a tick"
        );

        fs::set_permissions(&profile, fs::Permissions::from_mode(0o700)).expect("restore");
    });
}

/// Same rule one level down: `live_sessions_at` distinguishes "absent" from
/// "could not tell", so each caller picks which way an unknown falls.
#[cfg(unix)]
#[test]
fn live_sessions_at_reports_unknown_separately_from_zero() {
    use std::os::unix::fs::PermissionsExt;

    let tmp = tempfile::tempdir().expect("tempdir");
    let sessions = tmp.path().join("sessions-9002-0");
    fs::create_dir_all(&sessions).expect("mkdir sessions");

    assert_eq!(live_sessions_at(&tmp.path().join("absent")), Some(0));
    assert_eq!(live_sessions_at(&sessions), Some(0));

    fs::set_permissions(&sessions, fs::Permissions::from_mode(0o000)).expect("chmod");
    if fs::read_dir(&sessions).is_ok() {
        fs::set_permissions(&sessions, fs::Permissions::from_mode(0o700)).expect("restore");
        return;
    }

    assert_eq!(
        live_sessions_at(&sessions),
        None,
        "an unreadable marker dir is unknown, never zero"
    );

    fs::set_permissions(&sessions, fs::Permissions::from_mode(0o700)).expect("restore");
}

/// The same rule at the DESTRUCTIVE level. `prune_stale_sessions` unlinks what
/// `is_session_alive` reads as dead, and its zero is what three callers turn into
/// `remove_dir_all` of a runtime tree — shared across the profile's sessions
/// under `LinkMode::Fake`. So an unopenable marker (EMFILE, ESTALE, EACCES) must
/// read LIVE: folding one into a false deletes a live session's only marker and
/// unblocks a rotation against the single-use token it still holds.
#[cfg(unix)]
#[test]
fn an_unopenable_marker_reads_as_live_and_is_never_unlinked() {
    use std::os::unix::fs::PermissionsExt;

    let tmp = tempfile::tempdir().expect("tempdir");
    let sessions = tmp.path().join("sessions-9003-0");
    fs::create_dir_all(&sessions).expect("mkdir sessions");
    let marker = sessions.join("9003-0");
    fs::write(&marker, b"").expect("marker");

    // Control: a readable, unlocked marker IS dead, and pruning removes it.
    assert!(!is_session_alive(&marker));
    assert_eq!(prune_stale_sessions(&sessions), Some(0));
    assert!(
        !marker.exists(),
        "a genuinely dead marker must be collected"
    );

    fs::write(&marker, b"").expect("re-create marker");
    fs::set_permissions(&marker, fs::Permissions::from_mode(0o000)).expect("chmod");
    if fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&marker)
        .is_ok()
    {
        // Running with rights that ignore the mode (root); the probe cannot be
        // posed, so assert nothing rather than pass vacuously.
        fs::set_permissions(&marker, fs::Permissions::from_mode(0o600)).expect("restore");
        return;
    }

    assert!(
        is_session_alive(&marker),
        "an unopenable marker is unknown, and unknown must read live"
    );
    assert_eq!(
        prune_stale_sessions(&sessions),
        Some(1),
        "an unopenable marker must not be counted as dead"
    );
    assert!(
        marker.exists(),
        "pruning unlinked a marker it could not prove dead"
    );

    fs::set_permissions(&marker, fs::Permissions::from_mode(0o600)).expect("restore");
}

/// And the same rule for the marker DIR one level up. `prune_stale_sessions`'s
/// zero authorizes `remove_dir_all`, so an unreadable dir has to be an unknown —
/// `Some(0)` is reserved for a dir that is genuinely absent.
#[cfg(unix)]
#[test]
fn prune_reports_an_unreadable_marker_dir_as_unknown_not_zero() {
    use std::os::unix::fs::PermissionsExt;

    let tmp = tempfile::tempdir().expect("tempdir");
    let sessions = tmp.path().join("sessions-9004-0");
    fs::create_dir_all(&sessions).expect("mkdir sessions");

    assert_eq!(
        prune_stale_sessions(&tmp.path().join("absent")),
        Some(0),
        "a genuinely absent dir is idle, not unknown"
    );
    assert_eq!(prune_stale_sessions(&sessions), Some(0));

    fs::set_permissions(&sessions, fs::Permissions::from_mode(0o000)).expect("chmod");
    if fs::read_dir(&sessions).is_ok() {
        fs::set_permissions(&sessions, fs::Permissions::from_mode(0o700)).expect("restore");
        return;
    }

    assert_eq!(
        prune_stale_sessions(&sessions),
        None,
        "an unreadable marker dir must never authorize a teardown"
    );

    fs::set_permissions(&sessions, fs::Permissions::from_mode(0o700)).expect("restore");
}

/// GC hands `remove_dir_all` whatever it pairs, so it gates on the strict name
/// predicate. A profile child that merely starts with `runtime`/`sessions` is not
/// a runtime tree and must survive.
#[test]
fn gc_leaves_profile_children_that_only_look_like_runtime_dirs() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let profile = tmp
            .path()
            .join(".clauth")
            .join("profiles")
            .join("bystander");
        fs::create_dir_all(&profile).expect("mkdir profile");

        let bystanders = [
            "runtime_state.json",
            "runtimes",
            "sessions.json",
            "runtime-isolatedish",
            "runtime-4242-x",
            // A codex session home: outside the runtime*/sessions* stems BY
            // DESIGN, so the pairing rule has no claim on it and the orphan
            // branch never sees it. Its lifecycle belongs to the codex
            // runtime, not this GC.
            "codex-home-4242-0",
        ];
        for name in bystanders {
            let path = profile.join(name);
            if name.contains('.') {
                fs::write(&path, b"{}").expect("write bystander file");
            } else {
                fs::create_dir_all(&path).expect("mkdir bystander");
                fs::write(path.join("keep"), b"x").expect("seed bystander");
            }
        }

        gc_stale_runtimes();

        for name in bystanders {
            assert!(
                profile.join(name).exists(),
                "{name} is not a runtime tree and must not be collected"
            );
        }
    });
}

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
        assert!(
            has_live_session(&crate::profile::ProfileName::from("iso")),
            "isolated live session counts"
        );
        assert_eq!(
            live_session_count(&crate::profile::ProfileName::from("iso")),
            1
        );
        drop(file);
        // The probe is deliberately fail-alive (any try_lock I/O error reads
        // as "alive" — see `is_session_alive`), so transient errors under a
        // parallel suite run (fd pressure) can flip readings for a while. Poll
        // generously; only a PERSISTENT "alive" after the lock holder dropped
        // is a regression (flaked once under the full suite, 2026-07-12).
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while has_live_session(&crate::profile::ProfileName::from("iso"))
            && std::time::Instant::now() < deadline
        {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(!has_live_session(&crate::profile::ProfileName::from("iso")));
    });
}

/// GC removes a runtime tree left by a crashed session (no live PID), and never
/// touches one with a live session. All fixtures here are the LEGACY unsuffixed
/// layout a pre-per-session release left on disk: the `runtime<rest>` ↔
/// `sessions<rest>` pairing rule must reach it, which is the whole migration
/// path.
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

        // Stale, isolated flavor: the same legacy shape one dir name over.
        let stale_iso_runtime = profiles.join("staleiso").join("runtime-isolated");
        let stale_iso_sessions = profiles.join("staleiso").join("sessions-isolated");
        fs::create_dir_all(&stale_iso_runtime).expect("mkdir stale iso runtime");
        fs::create_dir_all(&stale_iso_sessions).expect("mkdir stale iso sessions");
        fs::write(stale_iso_runtime.join(".claude.json"), b"{}").expect("seed iso runtime");
        fs::write(stale_iso_sessions.join("88888"), b"").expect("dead iso pid");

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
            !stale_iso_runtime.exists(),
            "a legacy isolated pair must be collected the same way"
        );
        assert!(!stale_iso_sessions.exists());
        assert!(
            live_runtime.exists(),
            "a live session's runtime must be spared"
        );
        drop(held);
    });
}

/// Per-session dirs are collected by the same pairing rule, both flavors, and a
/// held marker still spares its own pair.
#[test]
fn gc_collects_a_dead_per_session_pair_and_spares_a_held_one() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let profiles = tmp.path().join(".clauth").join("profiles");

        // Dead: both flavors, marker present but unlocked.
        let mut dead = Vec::new();
        for (profile, runtime_name, sid) in [
            ("psdead", "runtime-4242-0", "4242-0"),
            ("psdeadiso", "runtime-isolated-4242-1", "4242-1"),
        ] {
            let runtime = profiles.join(profile).join(runtime_name);
            let sessions = profiles.join(profile).join(
                runtime_name
                    .strip_prefix("runtime")
                    .map(|rest| format!("sessions{rest}"))
                    .expect("paired name"),
            );
            fs::create_dir_all(&runtime).expect("mkdir runtime");
            fs::create_dir_all(&sessions).expect("mkdir sessions");
            fs::write(runtime.join(".claude.json"), b"{}").expect("seed runtime");
            fs::write(sessions.join(sid), b"").expect("dead marker");
            dead.push((runtime, sessions));
        }

        // Held: a per-session pair whose marker is flock-held.
        let held_runtime = profiles.join("pslive").join("runtime-777-3");
        let held_sessions = profiles.join("pslive").join("sessions-777-3");
        fs::create_dir_all(&held_runtime).expect("mkdir held runtime");
        fs::create_dir_all(&held_sessions).expect("mkdir held sessions");
        fs::write(held_runtime.join(".claude.json"), b"{}").expect("seed held runtime");
        let marker = open_pid_file(&held_sessions.join("777-3")).expect("open held marker");
        marker.lock().expect("lock held marker");

        gc_stale_runtimes();

        for (runtime, sessions) in &dead {
            assert!(
                !runtime.exists(),
                "{} must be collected — its marker is unlocked",
                runtime.display()
            );
            assert!(!sessions.exists(), "{} must go with it", sessions.display());
        }
        assert!(
            held_runtime.join(".claude.json").is_file(),
            "a held marker must spare its own runtime tree and its contents"
        );
        assert!(held_sessions.join("777-3").is_file());
        drop(marker);
    });
}

/// `acquire` mints a marker dir before it builds the tree, so a crash in that
/// window strands one with no runtime sibling — a fresh empty dir every session
/// under per-session keying. GC must collect it, and must still leave one whose
/// marker is held.
#[test]
fn gc_collects_an_orphaned_sessions_dir_with_no_runtime_sibling() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let profiles = tmp.path().join(".clauth").join("profiles");

        let orphan = profiles.join("orphan").join("sessions-5150-0");
        fs::create_dir_all(&orphan).expect("mkdir orphan");
        fs::write(orphan.join("5150-0"), b"").expect("dead marker");

        let orphan_empty = profiles.join("orphan").join("sessions-5150-1");
        fs::create_dir_all(&orphan_empty).expect("mkdir empty orphan");

        let held = profiles.join("orphan").join("sessions-5150-2");
        fs::create_dir_all(&held).expect("mkdir held orphan");
        let marker = open_pid_file(&held.join("5150-2")).expect("open held marker");
        marker.lock().expect("lock held marker");

        gc_stale_runtimes();

        assert!(
            !orphan.exists(),
            "an orphaned marker dir with a dead marker must be collected"
        );
        assert!(
            !orphan_empty.exists(),
            "an orphaned marker dir with no marker at all must be collected"
        );
        assert!(
            held.join("5150-2").is_file(),
            "a still-held marker dir must be spared even with no runtime sibling"
        );
        drop(marker);
    });
}

/// Registry rows ride the same sweep as the dirs, keyed off the marker their own
/// fields name: a row whose marker is unlocked is dead, one whose marker is held
/// is not.
#[test]
fn gc_drops_a_registry_row_whose_marker_is_unlocked_and_keeps_a_held_one() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let profiles = tmp.path().join(".clauth").join("profiles");

        // Dead: marker file present but unlocked.
        let dead_markers = profiles.join("rowdead").join("sessions-6001-0");
        fs::create_dir_all(&dead_markers).expect("mkdir dead markers");
        fs::write(dead_markers.join("6001-0"), b"").expect("dead marker");

        // Held, isolated flavor — the marker path is derived from `isolated`.
        let held_markers = profiles.join("rowlive").join("sessions-isolated-6001-1");
        fs::create_dir_all(&held_markers).expect("mkdir held markers");
        let marker = open_pid_file(&held_markers.join("6001-1")).expect("open held marker");
        marker.lock().expect("lock held marker");

        let mut dead = crate::live_sessions::LiveSession {
            session_id: "6001-0".into(),
            start_profile: "rowdead".into(),
            harness: crate::harness::Harness::Claude,
            pid: 6001,
            started_at: 1,
            cwd: None,
            isolated: false,
            follows_chain: false,
            intended_member: None,
            chain_cursor: None,
            current_member: None,
            last_swap_at: None,
            launch_store: None,
        };
        crate::live_sessions::register(&dead).expect("register dead");
        dead.session_id = "6001-1".into();
        dead.start_profile = "rowlive".into();
        dead.isolated = true;
        crate::live_sessions::register(&dead).expect("register live");

        gc_stale_runtimes();

        let left: Vec<String> = crate::live_sessions::list()
            .into_iter()
            .map(|r| r.session_id)
            .collect();
        assert_eq!(
            left,
            vec!["6001-1".to_string()],
            "only the row whose marker is still flock-held may survive"
        );
        drop(marker);
    });
}

/// The wiring, end to end: a real `acquire` files a row carrying this session's
/// own identity, and its teardown takes the row with it.
#[test]
fn acquire_registers_a_row_and_teardown_removes_it() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        fake_claude_home(tmp.path());
        let profile = configured_profile("registered");

        let rt = ProfileRuntime::acquire(&profile, Isolation::Shared, &[], false).expect("acquire");
        // `live_sid`, not `sid_of`: the subject here is the ROW, which a host
        // sharing one runtime tree files exactly the same way. Reading the sid off
        // the tree's name instead would skip this wiring on every such host.
        let sid = live_sid(&rt);

        let rows = crate::live_sessions::list();
        assert_eq!(rows.len(), 1, "acquire must file exactly one row");
        let registered = &rows[0];
        assert_eq!(registered.session_id, sid);
        assert_eq!(registered.start_profile, "registered");
        assert_eq!(registered.pid, std::process::id());
        assert!(!registered.isolated);
        assert_eq!(registered.intended_member, None);
        assert_eq!(registered.current_member, None);
        assert_eq!(registered.chain_cursor, None);
        assert_eq!(registered.last_swap_at, None);
        // The CANONICAL store, by exact path — `live_session_holds_rotatable`
        // re-reads this very file, so `None` here degrades the refusal to
        // always-refuse (fails closed, silently costs the exemption) while a
        // runtime-side copy would fail OPEN: the copy is refresh-less by
        // construction, every session would read "holds nothing rotatable",
        // and a rotation would strand a session that launched on the pair.
        assert_eq!(
            registered.launch_store.as_deref(),
            Some(
                crate::profile::profile_dir(&crate::profile::ProfileName::from("registered"))
                    .expect("profile dir")
                    .join("credentials.json")
                    .as_path()
            ),
            "acquire must record the canonical credential store it launched on"
        );

        drop(rt);

        assert!(
            crate::live_sessions::list().is_empty(),
            "teardown must take the session's row with it"
        );
    });
}

/// The session teardown holds the state flock a FIXED number of times, and the
/// unregister call never becomes a hold of its own. `unregister` takes the lock
/// itself, so the drop must not call it outside the one teardown closure: a
/// hoisted `unregister` is one extra outermost acquisition, leaving a window
/// where the row is gone but the marker is not. Measured 2026-08-08: a real
/// drop takes the lock in `tick` (credential reconcile), in
/// `settings_sync::sync_members`, and once in the teardown closure itself
/// (`src/runtime.rs` Drop); `claude_json::sync_once`'s fast path skips it in
/// this fixture. The load-bearing number is that the closure stays ONE hold —
/// the `tick`/sync legs are named so a change to them re-derives the count
/// rather than misattributing it to the unregister invariant.
#[test]
fn session_teardown_holds_the_state_flock_once() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        fake_claude_home(tmp.path());
        let profile = configured_profile("teardown-count");

        let rt = ProfileRuntime::acquire(&profile, Isolation::Shared, &[], false).expect("acquire");

        crate::lock::OUTERMOST_ACQUISITIONS.with(|c| c.set(0));
        drop(rt);
        let held = crate::lock::OUTERMOST_ACQUISITIONS.with(|c| c.get());
        assert_eq!(
            held, 3,
            "a session teardown must take the state flock exactly three times: tick reconcile, \
             settings sync, and the teardown closure (got {held})"
        );
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

// ── per-session swap executor ────────────────────────────────────────────────

/// A chain member with a store on disk, told apart by its access token.
fn member(name: &str) -> Profile {
    let mut profile = make_profile(name);
    profile.credentials = Some(ClaudeCredentials {
        claude_ai_oauth: Some(crate::profile::OAuthToken {
            access_token: format!("at-{name}"),
            refresh_token: Some(format!("rt-{name}")),
            expires_at: Some(1_000),
            scopes: None,
            subscription_type: None,
        }),
    });
    profile
}

/// Register `profile` and return the store a swap onto it repoints the link at.
/// Through [`register_profile`] rather than `save_profile` alone: a chain member
/// is a configured account, and a launch member an `acquire` runs against has to
/// be in the record that acquire re-reads.
fn member_store(profile: &Profile) -> PathBuf {
    register_profile(profile);
    crate::claude::install_source_path(&crate::profile::ProfileName::from(profile.name.as_str()))
        .expect("install source")
}

/// A live session with NO watchdog thread behind it, so every credential leg is
/// driven explicitly: a test asserting which leg moved which bytes can never be
/// won by a background tick landing first. `acquire` is used only where the
/// launch session's teardown is part of the assertion.
///
/// It stamps and HOLDS the launch member's markers exactly as `acquire` does. A
/// fixture that skipped them would let a swap back onto the launch member claim a
/// marker no production session could, so the returned locks are part of the
/// fixture, not litter.
fn lone_session(
    launch: &Profile,
    isolation: Isolation,
) -> (std::sync::Arc<SessionSwap>, SwappedMarkers) {
    let name = launch.name.as_str();
    let session = SessionId::mint();
    let store = crate::claude::install_source_path(&crate::profile::ProfileName::from(name))
        .expect("install source");
    let paths = SessionPaths::resolve(
        &crate::profile::ProfileName::from(name),
        isolation,
        &session,
        LinkMode::Real,
    )
    .expect("session paths");
    crate::profile::mkdir_700(&paths.runtime).expect("mkdir runtime");
    create_symlink(&store, &paths.runtime.join(".credentials.json")).expect("link creds");
    let markers = stamp_swapped_markers(&paths)
        .expect("stamp launch markers")
        .expect("the launch member's markers must be free in a fresh sandbox");
    let row = crate::live_sessions::LiveSession::starting(
        &session,
        name,
        crate::harness::Harness::Claude,
        isolation == Isolation::Isolated,
        false,
        Some(store.to_path_buf()),
    );
    crate::live_sessions::register(&row).expect("register row");
    let swap = std::sync::Arc::new(SessionSwap::new(
        session,
        isolation,
        LinkMode::Real,
        launch,
        store,
        &paths,
    ));
    (swap, markers)
}

/// The decision leg gates on `follows_chain` and nothing sets it true yet, so an
/// acquire-shaped registration must leave a session opted OUT — otherwise landing
/// the leg would move EVERY live session off the account it launched on.
#[test]
fn a_registered_session_is_opted_out_of_the_chain() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let launch = member("optin-a");
        member_store(&launch);
        let (swap, _launch_markers) = lone_session(&launch, Isolation::Shared);

        let row = crate::live_sessions::get(swap.session.as_str()).expect("the registered row");

        assert!(
            !row.follows_chain,
            "registration must not opt a session into the fallback chain"
        );
    });
}

/// `--with-fallback` is the only thing that sets `follows_chain`, so the flag has
/// to survive the whole way to the on-disk row: the decision leg reads that field
/// and nothing else decides whether a session is steerable.
///
/// On a host whose executor refuses every swap that same field is the CLAMP, so
/// the expected value is forked on the HOST rather than the test being skipped
/// there: such a row would collect daemon intents nothing can execute. Both clamp
/// arms are read from the host itself, keyed to the same two probes the gate uses,
/// so a Windows box outside Developer Mode covers this rather than reddening on it.
/// `a_fake_mode_host_never_registers_a_session_as_following_the_chain` pins the
/// transport arm against a forced mode; this is the only pin on this call site
/// passing the host's real value rather than a constant.
#[test]
fn the_with_fallback_flag_reaches_the_row_only_where_a_swap_can_land() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        fake_claude_home(tmp.path());
        let profile = configured_profile("optin-flag");

        let opted =
            ProfileRuntime::acquire(&profile, Isolation::Shared, &[], true).expect("acquire");
        let opted_row = crate::live_sessions::get(&live_sid(&opted)).expect("the opted-in row");
        drop(opted);

        let plain =
            ProfileRuntime::acquire(&profile, Isolation::Shared, &[], false).expect("acquire");
        let plain_row = crate::live_sessions::get(&live_sid(&plain)).expect("the opted-out row");
        drop(plain);

        // Both clamp arms, off the host rather than off a constant: a Windows box
        // outside Developer Mode probes into `LinkMode::Fake` and clamps for the
        // transport reason, exactly as macOS clamps for the platform one.
        //
        // Derived WITHOUT `swap_support`, which is what the subject's clamp is
        // built on: reading the expectation through the same predicate makes a
        // broken predicate flip both sides together and the assertion hold on a
        // host that can no longer swap. `acquire` above already materialized the
        // profile dir, so this probes what the clamp probed.
        let host_can_swap = detect_link_mode(
            &profile_dir(&crate::profile::ProfileName::from("optin-flag")).expect("profile dir"),
        )
        .expect("probe link mode")
            == LinkMode::Real
            && !cfg!(target_os = "macos");
        assert_eq!(
            opted_row.follows_chain, host_can_swap,
            "--with-fallback must reach the registry row, and must be clamped out of \
             it wherever the executor refuses every swap — keychain-first on macOS, \
             a shared runtime tree on a host without the symlink privilege"
        );
        assert!(
            !plain_row.follows_chain,
            "a bare start must stay on its launch account"
        );
    });
}

/// The transport mode is known only INSIDE `acquire`'s state-lock hold, which is
/// also where the row is written. So the structural floor under the CLI refusal
/// lives there: a row claiming to follow the chain on a host whose executor
/// refuses every swap would collect daemon intents nothing can execute, each
/// announced exactly once into a log nobody is reading.
#[test]
fn a_fake_mode_host_never_registers_a_session_as_following_the_chain() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        fake_claude_home(tmp.path());
        let profile = configured_profile("optin-fake");
        with_link_mode(LinkMode::Fake, || {
            let rt = ProfileRuntime::acquire(&profile, Isolation::Shared, &[], true)
                .expect("acquire under the shared fake-mode tree");
            let rows = crate::live_sessions::list();
            drop(rt);

            assert_eq!(rows.len(), 1, "exactly this session is registered");
            assert!(
                !rows[0].follows_chain,
                "a fake-mode row must never claim to follow the chain"
            );
        });
    });
}

/// The predicate behind that floor, spelled once and exercised on every arm —
/// `Isolated` and macOS are each unreachable through `acquire` from a Linux run,
/// and all three arms are refusals the executor also makes at its own chokepoint.
#[test]
fn a_chain_opt_in_survives_only_where_the_executor_can_swap() {
    assert!(
        chain_opt_in_survives(true, Isolation::Shared, LinkMode::Real, false),
        "a shared session on a real-symlink non-mac host is the supported case"
    );
    assert!(
        !chain_opt_in_survives(false, Isolation::Shared, LinkMode::Real, false),
        "nothing opts a session in but the flag"
    );
    assert!(
        !chain_opt_in_survives(true, Isolation::Isolated, LinkMode::Real, false),
        "an isolated session follows no chain"
    );
    assert!(
        !chain_opt_in_survives(true, Isolation::Shared, LinkMode::Fake, false),
        "a shared runtime tree cannot hold a per-session credential"
    );
    assert!(
        !chain_opt_in_survives(true, Isolation::Shared, LinkMode::Real, true),
        "macOS resolves credentials keychain-first, so a file swap is inert"
    );
}

/// The platform arm answers with no disk at all, so `start::run` can refuse a
/// statically-known verdict without a probe that could time out on the state flock
/// or fail on IO. Pinned as a pure call because `cfg!(target_os = "macos")` makes
/// the arm unreachable from a Linux run any other way.
#[test]
fn the_swap_platform_verdict_needs_no_probe() {
    assert_eq!(
        unsupported_swap_platform(true),
        Some(SwapUnsupported::KeychainFirst),
        "macOS is refused off a compile-time constant"
    );
    assert_eq!(
        unsupported_swap_platform(false),
        None,
        "every other platform leaves the verdict to the transport probe"
    );
}

/// The pre-`acquire` transport half: `start::run` needs the verdict BEFORE a tree
/// is built or `claude` is spawned, and it can only get one by probing the profile
/// dir the way `acquire` does.
///
/// The unforced half reads THIS host, so its expectation comes off the host too: a
/// box without the symlink privilege answers `SharedRuntimeTree` there and is right
/// to, and pinning `None` would assert one flavor of host rather than the probe.
#[test]
fn the_swap_host_probe_names_each_unsupported_transport() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        // `detect_link_mode` directly, not `host_poses`: this test RUNS on a
        // shared-tree host, so printing that helper's skip line would report a
        // test as skipped while it went on to assert.
        //
        // Probe the dir the SUBJECT probes: `unsupported_swap_transport` reads
        // the profile dir under this home, so an expectation taken off the home
        // root is about a directory the subject never looks at. Both spellings
        // sit in one tempdir tree, so no fixture the suite can build separates
        // them, which is exactly why the mismatch was invisible.
        let probed =
            profile_dir(&crate::profile::ProfileName::from("probe-host")).expect("profile dir");
        crate::profile::mkdir_700(&probed).expect("materialize the probed dir");
        let host_shares_one_tree =
            detect_link_mode(&probed).expect("probe link mode") == LinkMode::Fake;
        assert_eq!(
            unsupported_swap_transport(&crate::profile::ProfileName::from("probe-host"))
                .expect("probe"),
            host_shares_one_tree.then_some(SwapUnsupported::SharedRuntimeTree),
            "the unforced probe must name the transport this host actually has"
        );
        with_link_mode(LinkMode::Fake, || {
            assert_eq!(
                unsupported_swap_transport(&crate::profile::ProfileName::from("probe-host"))
                    .expect("probe"),
                Some(SwapUnsupported::SharedRuntimeTree),
                "a fake-symlink host shares one tree across the profile's sessions"
            );
        });
    });
}

/// The liveness predicate the decision leg gates every row on, anchored against the
/// REAL stamper rather than against a fixture that shares its path derivation. The
/// shared (non-isolated) layout is the production default and had no positive
/// anchor: a GC test asserting a row is DEAD passes for any wrong path, since a
/// marker that isn't there reads `NotFound` → dead.
#[test]
fn session_row_is_live_finds_the_marker_a_real_session_stamped() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let launch = member("rowlive-a");
        member_store(&launch);
        let (swap, _launch_markers) = lone_session(&launch, Isolation::Shared);
        let sid = swap.session.as_str();

        assert!(
            session_row_is_live(&crate::profile::ProfileName::from("rowlive-a"), false, sid),
            "the probe must look where `stamp_swapped_markers` actually writes"
        );
        // The other direction, so the assert above cannot be won by a predicate that
        // reads everything as live: a session id nothing stamped is dead.
        assert!(
            !session_row_is_live(
                &crate::profile::ProfileName::from("rowlive-a"),
                false,
                "9999-0"
            ),
            "an unstamped session id must read dead"
        );
    });
}

/// Every member in one config, each carrying a refresh token, so only the
/// live-session gate can keep it out of `rotation_candidates`.
#[cfg(not(target_os = "macos"))]
fn config_of(members: &[&Profile]) -> crate::profile::AppConfig {
    let mut config = crate::profile::AppConfig {
        state: crate::profile::AppState::default(),
        profiles: members.iter().map(|p| (*p).clone()).collect(),
    };
    for profile in members {
        config.state.profiles.push(profile.name.clone());
    }
    config
}

/// A Claude Code re-login as it lands on disk: the runtime link replaced by a
/// regular file, mtime `when` so the recency compare is unambiguous.
#[cfg(not(target_os = "macos"))]
fn cc_relogin(runtime: &Path, bytes: &[u8], when: SystemTime) -> PathBuf {
    let link = runtime.join(".credentials.json");
    let _ = fs::remove_file(&link);
    fs::write(&link, bytes).expect("write relogin");
    set_mtime(&link, when);
    link
}

/// THE §12 TEST. Claude Code stats the mtime of the symlink's TARGET at the head
/// of every request and re-reads only when that value CHANGED, so an
/// mtime-preserving repoint is a SILENT no-op: the session keeps authenticating
/// as the old member and nothing anywhere reports a problem.
#[cfg(not(target_os = "macos"))]
#[test]
fn a_swap_moves_the_mtime_of_the_store_it_repoints_to() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let launch = member("mtime-a");
        let intended = member("mtime-b");
        let launch_store = member_store(&launch);
        let intended_store = member_store(&intended);
        let (swap, _launch_markers) = lone_session(&launch, Isolation::Shared);

        // ONE shared mtime — the pathological case the live probe found, where
        // repointing the link changes nothing Claude Code can observe.
        let shared = SystemTime::now() - Duration::from_secs(60);
        set_mtime(&launch_store, shared);
        set_mtime(&intended_store, shared);

        let before_swap = SystemTime::now();
        assert_eq!(swap.swap_to("mtime-b").expect("swap"), SwapOutcome::Swapped);

        let after = fs::metadata(&intended_store)
            .expect("meta")
            .modified()
            .expect("mtime");
        assert!(
            after > shared,
            "the store CC stats through the link kept its mtime, \
             so this swap is a silent no-op"
        );
        // WHICH mechanism moved it, not just that something did: the swap stamps
        // the CLOCK. A value derived from the old store's mtime clears the assert
        // above too, while carrying that store's skew onto this one.
        assert!(
            after >= before_swap,
            "the new store's mtime must come from the clock, not from the old store"
        );
    });
}

/// B2. The touch above makes the intended member's store strictly newer, and
/// `profile::recover_pending_credentials` adopts a `credentials.json.pending`
/// sidecar only while it is at least as new as the store — so a sidecar left by a
/// rotation that died mid-save would be silently discarded, losing a refresh pair
/// that may be the only live one. `load_profile` adopting it first is what makes
/// the touch safe, and the plan the touch requires is minted by that load.
#[cfg(not(target_os = "macos"))]
#[test]
fn a_swap_adopts_a_crash_staged_sidecar_before_moving_the_store_mtime() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let launch = member("stage-a");
        let intended = member("stage-b");
        member_store(&launch);
        let intended_store = member_store(&intended);
        let (swap, _launch_markers) = lone_session(&launch, Isolation::Shared);

        // A rotation that staged its new pair and died before the commit.
        let staged = ClaudeCredentials {
            claude_ai_oauth: Some(crate::profile::OAuthToken {
                access_token: "at-rotated".into(),
                refresh_token: Some("rt-rotated".into()),
                expires_at: Some(9_000),
                scopes: None,
                subscription_type: None,
            }),
        };
        crate::profile::stage_rotated_credentials(
            &crate::profile::ProfileName::from("stage-b"),
            &staged,
        )
        .expect("stage");
        let sidecar = crate::profile::profile_dir(&crate::profile::ProfileName::from("stage-b"))
            .expect("profile dir")
            .join("credentials.json.pending");
        assert!(
            sidecar.is_file(),
            "fixture: the sidecar must exist pre-swap"
        );

        assert_eq!(swap.swap_to("stage-b").expect("swap"), SwapOutcome::Swapped);

        let store: ClaudeCredentials =
            serde_json::from_slice(&fs::read(&intended_store).expect("read store"))
                .expect("parse store");
        assert_eq!(
            store.claude_ai_oauth.and_then(|o| o.refresh_token),
            Some("rt-rotated".to_string()),
            "the staged rotation must be adopted before the store's mtime moves, \
             or the touch discards it and the refresh pair is gone"
        );
        assert!(
            !sidecar.exists(),
            "an adopted sidecar must be removed, so nothing can re-adopt it later"
        );
    });
}

/// The platform/transport gate is PURE so both refusals are exercised from a
/// Linux run. A swap that silently leaves the session on its launch account is
/// the one outcome §12 exists to prevent, so refusing loudly is the requirement.
#[test]
fn swap_support_refuses_a_shared_tree_and_a_keychain_first_host() {
    assert_eq!(
        swap_support(LinkMode::Fake, false),
        Err(SwapUnsupported::SharedRuntimeTree)
    );
    assert_eq!(
        swap_support(LinkMode::Fake, true),
        Err(SwapUnsupported::SharedRuntimeTree)
    );
    assert_eq!(
        swap_support(LinkMode::Real, true),
        Err(SwapUnsupported::KeychainFirst)
    );
    assert_eq!(swap_support(LinkMode::Real, false), Ok(()));
}

/// The rotation refusal is macOS-ONLY and pure, so both arms run from a Linux
/// box. It exists because clauth cannot write the Keychain item a `clauth start`
/// session's Claude Code reads (that item is namespaced per `CLAUDE_CONFIG_DIR`;
/// `keychain::SERVICE` is the unsuffixed one), so a rotation there signs the
/// session out rather than propagating to it.
#[test]
fn rotation_is_blocked_by_a_live_session_only_on_macos() {
    // macOS: a live `clauth start` session is the whole refusal.
    assert!(rotation_blocked_by_live_session(true, true));
    assert!(!rotation_blocked_by_live_session(false, true));
    // Everywhere else the session shares the credential FILE clauth rotates,
    // so it follows the new pair on its next request.
    assert!(!rotation_blocked_by_live_session(true, false));
    assert!(!rotation_blocked_by_live_session(false, false));
}

/// The profile-comparison half of the precondition, as a pure function the
/// daemon's per-session walk shares. It exists so the two cannot drift: a
/// candidate the executor refuses on CONFIG grounds has to be walked PAST, or the
/// intent never changes and the session never reaches the next viable member.
/// One case per arm, and the api-key arm both directions — it compares STATES, so
/// a launch that has a key needs a candidate that has one too.
#[test]
fn swap_eligible_refuses_exactly_the_config_grounds_the_precondition_does() {
    let mut launch = make_profile("elig-launch");
    launch.env.insert("SHARED".into(), "1".into());
    launch.models.default = Some("sonnet".into());
    let transport = LaunchTransport::of(&launch);

    let mut twin = make_profile("elig-twin");
    twin.env = launch.env.clone();
    twin.models = launch.models.clone();
    assert_eq!(swap_eligible(&twin, &transport), Ok(()));

    let mut endpoint = twin.clone();
    endpoint.base_url = Some("https://api.example/anthropic".into());
    assert_eq!(
        swap_eligible(&endpoint, &transport),
        Err(SwapRefused::NotOauth)
    );

    let mut disabled = twin.clone();
    disabled.disabled = true;
    assert_eq!(
        swap_eligible(&disabled, &transport),
        Err(SwapRefused::Disabled)
    );

    let mut env = twin.clone();
    env.env.insert("EXTRA".into(), "2".into());
    assert_eq!(
        swap_eligible(&env, &transport),
        Err(SwapRefused::EnvDiffers)
    );

    let mut models = twin.clone();
    models.models.default = Some("opus".into());
    assert_eq!(
        swap_eligible(&models, &transport),
        Err(SwapRefused::ModelsDiffers)
    );

    let mut keyed = twin.clone();
    keyed.api_key = Some("k".into());
    assert_eq!(
        swap_eligible(&keyed, &transport),
        Err(SwapRefused::ApiKeyDiffers)
    );

    // ORDER is observable, not incidental: `announce_refusal` dedupes per
    // `(member, reason)`, so which cause an operator is told for a member hitting
    // two arms is decided here. Nothing couples `base_url` to `disabled`, so a
    // disabled third-party member hits both — the endpoint is the cause to report,
    // since it disqualifies the member even once re-enabled.
    let mut endpoint_and_disabled = endpoint.clone();
    endpoint_and_disabled.disabled = true;
    assert_eq!(
        swap_eligible(&endpoint_and_disabled, &transport),
        Err(SwapRefused::NotOauth),
        "the endpoint must be reported ahead of the disabled bit"
    );

    // A session launched on an api-key member: the same-state candidate clears
    // and the keyless one is the one refused, so the compare cannot degrade into
    // "the candidate has no key".
    let mut keyed_launch = launch.clone();
    keyed_launch.api_key = Some("k".into());
    let keyed_transport = LaunchTransport::of(&keyed_launch);
    assert_eq!(swap_eligible(&keyed, &keyed_transport), Ok(()));
    assert_eq!(
        swap_eligible(&twin, &keyed_transport),
        Err(SwapRefused::ApiKeyDiffers)
    );
}

/// `settings.json` env reaches Claude Code's `process.env` only at STARTUP,
/// while `ANTHROPIC_AUTH_TOKEN` is read live per client construction, so a
/// member carrying different env or model routing is a genuinely different
/// transport rather than the same account elsewhere.
#[cfg(not(target_os = "macos"))]
#[test]
fn the_precondition_refuses_a_member_whose_transport_differs() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let launch = member("pre-launch");
        member_store(&launch);
        let (swap, _launch_markers) = lone_session(&launch, Isolation::Shared);

        let twin = member("pre-twin");
        member_store(&twin);

        let mut env = member("pre-env");
        env.env.insert("SOME_KEY".into(), "1".into());
        member_store(&env);

        let mut models = member("pre-models");
        models.models.default = Some("opus".into());
        member_store(&models);

        // base_url + api key + NO stored pair: `load_profile` normalizes a
        // base_url away only when a pair is stored and no usable key is.
        let mut endpoint = make_profile("pre-endpoint");
        endpoint.base_url = Some("https://api.example/anthropic".into());
        endpoint.api_key = Some("k".into());
        crate::profile::save_profile(&endpoint).expect("save endpoint");

        let mut disabled = member("pre-disabled");
        disabled.disabled = true;
        member_store(&disabled);

        let mut keyed = member("pre-keyed");
        keyed.api_key = Some("k".into());
        member_store(&keyed);

        // The cleared case yields the plan the touch step needs, keyed to the
        // member it loaded.
        let cleared = |name: &str| swap.precondition(name).map(|plan| plan.member);
        assert_eq!(
            cleared("pre-twin"),
            Ok(crate::profile::ProfileName::from("pre-twin"))
        );
        assert_eq!(cleared("pre-env"), Err(SwapRefused::EnvDiffers));
        assert_eq!(cleared("pre-models"), Err(SwapRefused::ModelsDiffers));
        assert_eq!(cleared("pre-endpoint"), Err(SwapRefused::NotOauth));
        assert_eq!(cleared("pre-disabled"), Err(SwapRefused::Disabled));
        assert_eq!(cleared("pre-keyed"), Err(SwapRefused::ApiKeyDiffers));
        assert_eq!(cleared("pre-absent"), Err(SwapRefused::NoCredentialStore));
    });
}

/// A clauth predating the per-session layout probes exactly `<profile>/sessions`,
/// so without a marker there its `has_live_session` reads the swapped-onto member
/// as IDLE and its rotation leg spends the single-use refresh token the live
/// Claude Code child is authenticating with. Right after an upgrade that old
/// binary is the running daemon.
#[cfg(not(target_os = "macos"))]
#[test]
fn a_swap_holds_both_of_the_intended_members_liveness_markers() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let launch = member("marker-a");
        let intended = member("marker-b");
        member_store(&launch);
        member_store(&intended);
        let (swap, _launch_markers) = lone_session(&launch, Isolation::Shared);
        let sid = swap.session.as_str().to_string();

        assert_eq!(
            swap.swap_to("marker-b").expect("swap"),
            SwapOutcome::Swapped
        );

        let profile_dir =
            crate::profile::profile_dir(&crate::profile::ProfileName::from("marker-b"))
                .expect("profile dir");
        for marker in [
            profile_dir.join(format!("sessions-{sid}")).join(&sid),
            profile_dir.join("sessions").join(&sid),
        ] {
            assert!(marker.is_file(), "no marker at {}", marker.display());
            assert!(
                is_session_alive(&marker),
                "{} must be flock-held for the session's life",
                marker.display()
            );
        }
        assert!(
            has_live_session(&crate::profile::ProfileName::from("marker-b")),
            "the rotation gate must see the swapped-onto member as live"
        );
    });
}

/// A member whose marker this session cannot hold is a member the rotation gate
/// cannot see it on, so the swap refuses INSIDE the hold rather than repointing
/// the link at a chain nothing is protecting.
#[cfg(not(target_os = "macos"))]
#[test]
fn a_swap_refuses_a_member_whose_marker_another_process_holds() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        if !host_poses(tmp.path(), "a real-symlink session for the swap to repoint") {
            return;
        }
        let launch = member("held-a");
        let intended = member("held-b");
        let launch_store = member_store(&launch);
        member_store(&intended);
        let (swap, _launch_markers) = lone_session(&launch, Isolation::Shared);
        let sid = swap.session.as_str().to_string();

        // A live foreign process already owns the per-session marker path.
        let markers = crate::profile::profile_dir(&crate::profile::ProfileName::from("held-b"))
            .expect("profile dir")
            .join(format!("sessions-{sid}"));
        fs::create_dir_all(&markers).expect("mkdir markers");
        let held = open_pid_file(&markers.join(&sid)).expect("open marker");
        held.lock().expect("lock marker");

        let outcome = swap.swap_to("held-b").expect("swap");

        assert_eq!(
            fs::read_link(swap.runtime.join(".credentials.json")).expect("read link"),
            launch_store,
            "a refused swap must leave the link on the member it was protecting"
        );
        assert_eq!(
            outcome,
            SwapOutcome::Refused(SwapRefused::MarkerNotLockable)
        );
        drop(held);
    });
}

/// §11 step 8: the previous member's marker is NEVER dropped, because the live
/// Claude Code child still holds its refresh token in memory and nothing can
/// observe when it stops. The marker is liveness bookkeeping the destructive
/// guards read — it is NOT a rotation gate, so both members stay rotatable
/// throughout. A swapped session follows whichever pair clauth writes.
#[cfg(not(target_os = "macos"))]
#[test]
fn a_swap_keeps_both_members_marked_live_and_still_rotatable() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        if !host_poses(tmp.path(), "a session whose credentials a swap can repoint") {
            return;
        }
        fake_claude_home(tmp.path());
        let launch = member("rot-a");
        let intended = member("rot-b");
        member_store(&launch);
        member_store(&intended);

        let rt = ProfileRuntime::acquire(&launch, Isolation::Shared, &[], false).expect("acquire");
        let sid = live_sid(&rt);
        assert_eq!(
            rt.swap().swap_to("rot-b").expect("swap"),
            SwapOutcome::Swapped
        );

        let launch_dir = crate::profile::profile_dir(&crate::profile::ProfileName::from("rot-a"))
            .expect("profile dir");
        for marker in [
            launch_dir.join(format!("sessions-{sid}")).join(&sid),
            launch_dir.join("sessions").join(&sid),
        ] {
            assert!(
                is_session_alive(&marker),
                "{} must survive the swap — the live child still holds that chain",
                marker.display()
            );
        }

        let config = config_of(&[&launch, &intended]);
        let want = vec![
            (
                crate::profile::ProfileName::from("rot-a"),
                "rt-rot-a".to_string(),
            ),
            (
                crate::profile::ProfileName::from("rot-b"),
                "rt-rot-b".to_string(),
            ),
        ];
        assert_eq!(
            crate::oauth::rotation_candidates(&config, false),
            want,
            "a live marker is not a rotation gate — both members stay candidates"
        );
        assert_eq!(
            crate::oauth::rotation_candidates(&config, true),
            want,
            "force changes nothing here; liveness never excluded either member"
        );
        drop(rt);
    });
}

/// The repoint itself: `.credentials.json` resolves to the intended member's
/// store, through the tmp+rename swap rather than a remove+create.
#[cfg(not(target_os = "macos"))]
#[test]
fn a_swap_repoints_the_runtime_link_at_the_intended_store() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        if !host_poses(tmp.path(), "a real-symlink session for the swap to repoint") {
            return;
        }
        let launch = member("link-a");
        let intended = member("link-b");
        member_store(&launch);
        let intended_store = member_store(&intended);
        let (swap, _launch_markers) = lone_session(&launch, Isolation::Shared);
        let link = swap.runtime.join(".credentials.json");

        assert_eq!(swap.swap_to("link-b").expect("swap"), SwapOutcome::Swapped);

        assert_eq!(
            fs::read_link(&link).expect("read link"),
            intended_store,
            "the credential link must resolve to the intended member's store"
        );
        assert_eq!(
            fs::read(&link).expect("read through link"),
            fs::read(&intended_store).expect("read store"),
        );
    });
}

/// §11 #1. A Claude Code re-login sitting in the runtime file belongs to the
/// member the link STILL resolves to; without the drain those bytes land in the
/// new member's store on the next tick and its refresh token is gone.
#[cfg(not(target_os = "macos"))]
#[test]
fn a_swap_drains_a_pending_relogin_into_the_launch_store() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let launch = member("drain-a");
        let intended = member("drain-b");
        let launch_store = member_store(&launch);
        let intended_store = member_store(&intended);
        let (swap, _launch_markers) = lone_session(&launch, Isolation::Shared);

        let intended_before = fs::read(&intended_store).expect("read intended store");
        set_mtime(&launch_store, SystemTime::now() - Duration::from_secs(60));
        cc_relogin(&swap.runtime, CREDS_V2, SystemTime::now());

        assert_eq!(swap.swap_to("drain-b").expect("swap"), SwapOutcome::Swapped);

        assert_eq!(
            fs::read(&launch_store).expect("read launch store"),
            CREDS_V2,
            "the re-login must be captured into the member the link still resolved to"
        );
        assert_eq!(
            fs::read(&intended_store).expect("read intended store"),
            intended_before,
            "the intended member's own chain must be untouched by the drain"
        );
    });
}

/// B5. The watchdog thread and `Drop`'s final tick both used to read a MOVED
/// CLONE of `canonical`, so a swap that only mutated a field would have the next
/// tick relink the session back to the OLD member AND write the new member's
/// tokens into the old member's store.
#[cfg(not(target_os = "macos"))]
#[test]
fn the_tick_after_a_swap_drains_into_the_intended_store() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        if !host_poses(tmp.path(), "a real-symlink session for the swap to repoint") {
            return;
        }
        let claude_home = fake_claude_home(tmp.path());
        let launch = member("tick-a");
        let intended = member("tick-b");
        let launch_store = member_store(&launch);
        let intended_store = member_store(&intended);
        let (swap, _launch_markers) = lone_session(&launch, Isolation::Shared);

        assert_eq!(swap.swap_to("tick-b").expect("swap"), SwapOutcome::Swapped);

        let launch_before = fs::read(&launch_store).expect("read launch store");
        set_mtime(&intended_store, SystemTime::now() - Duration::from_secs(60));
        let link = cc_relogin(&swap.runtime, CREDS_V2, SystemTime::now());

        tick(&claude_home, &swap).expect("tick");

        assert_eq!(
            fs::read(&intended_store).expect("read intended store"),
            CREDS_V2,
            "the tick must drain into the member the swap published, not the launch one"
        );
        assert_eq!(
            fs::read(&launch_store).expect("read launch store"),
            launch_before,
            "the launch member's store must never receive the new member's bytes"
        );
        assert_eq!(
            fs::read_link(&link).expect("read link"),
            intended_store,
            "the tick must re-establish the link to the intended member"
        );
    });
}

/// B6. `<intended>/sessions-<sid>/` has no `runtime-<sid>` sibling, so it lands
/// in `gc_stale_runtimes`'s orphaned-marker-dir arm. It is spared only because
/// the flock the swap holds reads live — one edit away from deleting a live
/// session's rotation protection.
#[cfg(not(target_os = "macos"))]
#[test]
fn gc_spares_a_swapped_members_marker_dir_while_the_session_lives() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let launch = member("gc-a");
        let intended = member("gc-b");
        member_store(&launch);
        member_store(&intended);
        let (swap, _launch_markers) = lone_session(&launch, Isolation::Shared);
        let sid = swap.session.as_str().to_string();

        assert_eq!(swap.swap_to("gc-b").expect("swap"), SwapOutcome::Swapped);

        let profile_dir = crate::profile::profile_dir(&crate::profile::ProfileName::from("gc-b"))
            .expect("profile dir");
        let own = profile_dir.join(format!("sessions-{sid}"));
        let compat = profile_dir.join("sessions");

        gc_stale_runtimes();
        assert!(
            is_session_alive(&own.join(&sid)),
            "GC collected a live session's per-session marker on the swapped-onto member"
        );
        assert!(
            is_session_alive(&compat.join(&sid)),
            "GC collected a live session's upgrade-compat marker on the swapped-onto member"
        );

        drop(swap);
        gc_stale_runtimes();
        assert!(
            !own.exists(),
            "the marker dir must be collected once its session is gone"
        );
        assert!(!compat.exists(), "so must the compat dir");
    });
}

/// GC's half of the force-delete divergence, mirroring the tally's
/// `a_swapped_session_counts_on_current_member_after_its_launch_marker_is_removed`:
/// `clauth delete <launch> --force` removes the launch profile's whole dir, marker
/// dirs included, while the session keeps running on the member it swapped onto.
/// Probing `start_profile` there finds nothing, reads the row as dead, and reaps a
/// session the tally is still counting — the exact split the shared
/// `current_member`-first probe exists to prevent.
#[cfg(not(target_os = "macos"))]
#[test]
fn gc_keeps_a_swapped_row_after_its_launch_profile_is_force_deleted() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let launch = member("rowgc-a");
        let intended = member("rowgc-b");
        member_store(&launch);
        member_store(&intended);
        let (swap, _launch_markers) = lone_session(&launch, Isolation::Shared);
        let sid = swap.session.as_str().to_string();

        assert_eq!(
            swap.swap_to("rowgc-b").expect("swap"),
            SwapOutcome::Swapped,
            "fixture: only a landed swap writes the `current_member` under test"
        );

        // The real force-delete, not a hand `remove_dir_all`: this state is
        // reachable only because `--force` is the one thing that skips the
        // live-session gate, and nothing else removes a profile dir out from under
        // a running session.
        let mut config = crate::profile::AppConfig {
            state: crate::profile::AppState {
                profiles: vec![launch.name.clone(), intended.name.clone()],
                ..Default::default()
            },
            profiles: vec![launch, intended],
        };
        let rotation = crate::actions::rotation_guard_for_mutation(
            &crate::profile::ProfileName::from("rowgc-a"),
        )
        .expect("uncontended rotation lock");
        crate::actions::delete_profile(
            &mut config,
            &crate::profile::ProfileName::from("rowgc-a"),
            true,
            &rotation,
        )
        .expect("force-delete");
        assert!(
            !crate::profile::profile_dir(&crate::profile::ProfileName::from("rowgc-a"))
                .expect("profile dir")
                .exists(),
            "fixture: the force-delete must take the launch marker dirs with it"
        );

        gc_stale_runtimes();

        assert_eq!(
            crate::live_sessions::get(&sid)
                .and_then(|row| row.current_member)
                .as_deref(),
            Some("rowgc-b"),
            "GC reaped the row of a session still running on its swapped-onto member"
        );

        // Control: the sweep does reach this row and does reap it once nothing
        // holds the swapped-onto member's markers either — so the assertion above
        // cannot be passing on a leg that never ran.
        drop(swap);
        gc_stale_runtimes();
        assert!(
            crate::live_sessions::get(&sid).is_none(),
            "a row must go once nothing it stamped is still flock-held at a path that exists"
        );
    });
}

/// Teardown owns every marker the session stamped — both layouts, on the launch
/// member and on each member it swapped onto — or a dead session keeps blocking
/// rotation on accounts nothing is using.
#[cfg(not(target_os = "macos"))]
#[test]
fn teardown_removes_every_marker_a_swap_stamped() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        if !host_poses(
            tmp.path(),
            "a compat marker separate from the session's own",
        ) {
            return;
        }
        fake_claude_home(tmp.path());
        let launch = member("down-a");
        let intended = member("down-b");
        member_store(&launch);
        member_store(&intended);

        let rt = ProfileRuntime::acquire(&launch, Isolation::Shared, &[], false).expect("acquire");
        let sid = live_sid(&rt);
        assert_eq!(
            rt.swap().swap_to("down-b").expect("swap"),
            SwapOutcome::Swapped
        );

        let mut markers = Vec::new();
        for name in ["down-a", "down-b"] {
            let dir = crate::profile::profile_dir(&crate::profile::ProfileName::from(name))
                .expect("profile dir");
            markers.push(dir.join(format!("sessions-{sid}")).join(&sid));
            markers.push(dir.join("sessions").join(&sid));
        }
        for marker in &markers {
            assert!(is_session_alive(marker), "{} not held", marker.display());
        }

        drop(rt);

        for marker in &markers {
            assert!(
                !marker.exists(),
                "teardown left {} behind, blocking rotation on a dead session",
                marker.display()
            );
        }
        assert!(
            !has_live_session(&crate::profile::ProfileName::from("down-b")),
            "the swapped-onto member must be rotatable again once the session exits"
        );
    });
}

/// Phase 0b's discipline, now on the swap path: `stamp_legacy_marker` yields
/// `None` when `try_lock` loses to a live process that minted the same sid, and
/// unlinking there deletes a FOREIGN session's liveness signal — the same
/// rotation burn the compat marker exists to prevent.
#[cfg(not(target_os = "macos"))]
#[test]
fn teardown_leaves_a_swapped_compat_marker_it_never_owned() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        if !host_poses(
            tmp.path(),
            "a compat marker separate from the session's own",
        ) {
            return;
        }
        fake_claude_home(tmp.path());
        let launch = member("foreign-a");
        let intended = member("foreign-b");
        member_store(&launch);
        member_store(&intended);

        let rt = ProfileRuntime::acquire(&launch, Isolation::Shared, &[], false).expect("acquire");
        let sid = live_sid(&rt);

        // A live foreign holder already owns the compat path on the member we are
        // about to swap onto.
        let compat = crate::profile::profile_dir(&crate::profile::ProfileName::from("foreign-b"))
            .expect("profile dir")
            .join("sessions");
        fs::create_dir_all(&compat).expect("mkdir compat");
        let foreign = compat.join(&sid);
        let held = open_pid_file(&foreign).expect("open foreign marker");
        held.lock().expect("lock foreign marker");

        assert_eq!(
            rt.swap().swap_to("foreign-b").expect("swap"),
            SwapOutcome::Swapped
        );

        drop(rt);

        assert!(
            foreign.is_file(),
            "teardown unlinked a compat marker owned by another live process"
        );
        assert!(
            is_session_alive(&foreign),
            "the foreign holder's flock must be untouched"
        );
        drop(held);
    });
}

/// A swap onto the member the link already resolves to must touch nothing: no
/// marker on a second path, no mtime move that would make Claude Code re-read
/// for no reason, no registry write.
#[cfg(not(target_os = "macos"))]
#[test]
fn a_swap_onto_the_member_already_current_changes_nothing() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let launch = member("noop-a");
        let launch_store = member_store(&launch);
        let (swap, _launch_markers) = lone_session(&launch, Isolation::Shared);
        let sid = swap.session.as_str().to_string();

        let before = SystemTime::now() - Duration::from_secs(60);
        set_mtime(&launch_store, before);

        // The side effects are asserted BEFORE the outcome: an outcome assert
        // first would panic on any mutation that lets the swap run, so the
        // touches-nothing claims would never be reached.
        let outcome = swap.swap_to("noop-a").expect("swap");

        assert_eq!(
            fs::metadata(&launch_store)
                .expect("meta")
                .modified()
                .expect("mtime"),
            before,
            "a no-op swap must not move the store's mtime"
        );
        // Not evidence on its own — a same-member swap would resolve to the
        // launch marker and claim nothing anyway. It pins the narrower thing: a
        // `claim_markers` that stamped unconditionally.
        assert!(
            swap.cell().held.is_empty(),
            "a no-op swap must not claim a marker"
        );
        let row = crate::live_sessions::get(&sid).expect("row");
        assert_eq!(
            row.current_member, None,
            "a no-op swap must not write the row"
        );
        assert_eq!(row.last_swap_at, None);
        assert_eq!(outcome, SwapOutcome::Refused(SwapRefused::AlreadyCurrent));
    });
}

/// §11 #11. The daemon writes `intended_member` while the session executes; a row
/// loaded before the swap and stored after would silently revert it, and the
/// session would keep re-swapping onto a member the daemon has moved past.
#[cfg(not(target_os = "macos"))]
#[test]
fn a_swap_preserves_a_daemon_written_intended_member() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let launch = member("row-a");
        let intended = member("row-b");
        member_store(&launch);
        member_store(&intended);
        let (swap, _launch_markers) = lone_session(&launch, Isolation::Shared);
        let sid = swap.session.as_str().to_string();

        crate::live_sessions::update_as_daemon(&sid, |d| {
            d.set_intended_member("row-b");
            d.set_chain_cursor(2);
        })
        .expect("daemon write");

        assert_eq!(swap.swap_to("row-b").expect("swap"), SwapOutcome::Swapped);

        let row = crate::live_sessions::get(&sid).expect("row");
        assert_eq!(
            row.intended_member.as_deref(),
            Some("row-b"),
            "the session's own write must not revert a daemon-owned field"
        );
        assert_eq!(row.chain_cursor, Some(2));
        assert_eq!(row.current_member.as_deref(), Some("row-b"));
        assert!(row.last_swap_at.is_some());
    });
}

/// §11 #12's residue, bounded where it is cheap: `Drop` joins the watchdog, so a
/// swap STARTED after teardown began would hold session exit for the state-lock
/// timeout plus an unbounded rotation-flock wait.
#[cfg(not(target_os = "macos"))]
#[test]
fn a_swap_does_not_start_once_teardown_has_begun() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let launch = member("bye-a");
        let intended = member("bye-b");
        member_store(&launch);
        let intended_store = member_store(&intended);
        let (swap, _launch_markers) = lone_session(&launch, Isolation::Shared);

        let before = SystemTime::now() - Duration::from_secs(60);
        set_mtime(&intended_store, before);
        swap.shutdown.begin();

        assert_eq!(
            swap.precondition("bye-b").map(|plan| plan.member),
            Err(SwapRefused::ShuttingDown)
        );
        assert_eq!(
            swap.swap_to("bye-b").expect("swap"),
            SwapOutcome::Refused(SwapRefused::ShuttingDown)
        );
        assert_eq!(
            fs::metadata(&intended_store)
                .expect("meta")
                .modified()
                .expect("mtime"),
            before,
            "a refused swap must touch nothing"
        );
    });
}

/// THE RECOVERY HALF OF THE CHAIN. `flock` locks the open file description, so a
/// second `open` + `try_lock` from THIS process is denied by our OWN lock — and
/// this session never releases a marker (step 8). So a swap back onto a member it
/// has already run on has to recognize the marker as already ours; reading it as a
/// foreign holder would refuse every recovery hop for the session's whole life,
/// after exactly one log line.
#[cfg(not(target_os = "macos"))]
#[test]
fn a_swap_back_onto_a_member_the_session_already_ran_on_succeeds() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        if !host_poses(tmp.path(), "a real-symlink session for the swap to repoint") {
            return;
        }
        let launch = member("back-a");
        let intended = member("back-b");
        let launch_store = member_store(&launch);
        let intended_store = member_store(&intended);
        let (swap, _launch_markers) = lone_session(&launch, Isolation::Shared);
        let link = swap.runtime.join(".credentials.json");

        assert_eq!(swap.swap_to("back-b").expect("out"), SwapOutcome::Swapped);
        let away = SystemTime::now() - Duration::from_secs(60);
        set_mtime(&launch_store, away);

        assert_eq!(
            swap.swap_to("back-a").expect("back"),
            SwapOutcome::Swapped,
            "a member this session already holds a marker on is not a foreign holder"
        );

        assert_eq!(
            fs::read_link(&link).expect("read link"),
            launch_store,
            "the link must resolve back to the recovered member's store"
        );
        assert!(
            fs::metadata(&launch_store)
                .expect("meta")
                .modified()
                .expect("mtime")
                > away,
            "the recovered store's mtime must move, or Claude Code never re-reads it"
        );
        for name in ["back-a", "back-b"] {
            assert!(
                has_live_session(&crate::profile::ProfileName::from(name)),
                "{name} must stay rotation-blocked: the child still holds its chain"
            );
        }
        assert!(
            fs::metadata(&intended_store).is_ok(),
            "fixture: the member swapped away from keeps its own store"
        );
    });
}

/// A repoint that fails leaves the session authenticating as the member its link
/// still resolves to, so the cell must not have moved: a cell pointing at the
/// intended member while the link resolves to the launch one is §12's silent
/// no-op reached through an error path, permanent (`poll` filters on
/// `member()` equality) and reported by one log line.
#[cfg(all(unix, not(target_os = "macos")))]
#[test]
fn a_failed_repoint_leaves_the_session_on_the_member_its_link_resolves_to() {
    use std::os::unix::fs::PermissionsExt;

    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let claude_home = fake_claude_home(tmp.path());
        let launch = member("fail-a");
        let intended = member("fail-b");
        let launch_store = member_store(&launch);
        let intended_store = member_store(&intended);
        let (swap, _launch_markers) = lone_session(&launch, Isolation::Shared);

        // The repoint stages a temp symlink INSIDE the runtime dir, so a
        // write-denied dir is what makes `relink_to_canonical` fail.
        fs::set_permissions(&swap.runtime, fs::Permissions::from_mode(0o500)).expect("chmod");
        if create_symlink(&launch_store, &swap.runtime.join(".probe")).is_ok() {
            // Running with rights that ignore the mode (root): the probe cannot be
            // posed, so assert nothing rather than pass vacuously.
            let _ = fs::remove_file(swap.runtime.join(".probe"));
            fs::set_permissions(&swap.runtime, fs::Permissions::from_mode(0o700)).expect("restore");
            return;
        }

        assert!(
            swap.swap_to("fail-b").is_err(),
            "fixture: the repoint must actually fail for this test to pose anything"
        );
        fs::set_permissions(&swap.runtime, fs::Permissions::from_mode(0o700)).expect("restore");

        assert_eq!(
            swap.canonical(),
            launch_store,
            "the cell moved onto a member the link never reached"
        );

        // The consequence, if it had moved: an interactive `/login` in the session
        // belongs to the LAUNCH account, and the tick would write it over the
        // intended member's store, destroying a chain nothing ever used.
        let intended_before = fs::read(&intended_store).expect("read intended");
        set_mtime(&launch_store, SystemTime::now() - Duration::from_secs(60));
        cc_relogin(&swap.runtime, CREDS_V2, SystemTime::now());
        tick(&claude_home, &swap).expect("tick");

        assert_eq!(
            fs::read(&launch_store).expect("read launch"),
            CREDS_V2,
            "the re-login belongs to the member the link resolved to"
        );
        assert_eq!(
            fs::read(&intended_store).expect("read intended"),
            intended_before,
            "a member the session never authenticated as must keep its own chain"
        );
    });
}

/// What Claude Code compares is EQUALITY against the mtime it memoized for the
/// previous target (`if(e!==Oeu)`), so the swap only has to make the new store's
/// value DIFFER — and must not reach that by importing the old store's clock
/// skew. A store left ahead of the clock makes `recover_pending_credentials`
/// discard every later crash-staged sidecar and `resolve_credential_winner`
/// discard every later re-login, on a member whose mtime was healthy until the
/// swap touched it.
#[cfg(not(target_os = "macos"))]
#[test]
fn a_swap_moves_the_mtime_without_importing_the_old_stores_skew() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let launch = member("ahead-a");
        let intended = member("ahead-b");
        let launch_store = member_store(&launch);
        let intended_store = member_store(&intended);
        let (swap, _launch_markers) = lone_session(&launch, Isolation::Shared);

        // The store the link came from is stamped an hour ahead of the clock — a
        // restored backup, a skewed network mount.
        let ahead = SystemTime::now() + Duration::from_secs(3_600);
        set_mtime(&launch_store, ahead);

        assert_eq!(swap.swap_to("ahead-b").expect("out"), SwapOutcome::Swapped);

        let after = fs::metadata(&intended_store)
            .expect("meta")
            .modified()
            .expect("mtime");
        assert_ne!(
            after, ahead,
            "the new target's mtime must differ from the one CC memoized"
        );
        assert!(
            after <= SystemTime::now(),
            "the swap stamped the new member's store ahead of the clock, which \
             discards its later sidecars and re-logins for as long as it stands"
        );
    });
}

/// The stamp is a write-recency signal with no write behind it, and the runtime
/// side is written by CLAUDE CODE — nothing can be attached there to compensate,
/// so the stamp itself has to stop reading as a write. Otherwise, for one
/// watchdog tick after a swap onto B, a SECOND live session on B whose Claude
/// Code just wrote an interactive `/login` loses it: canonical looks newer, that
/// session's tick keeps canonical and relinks over the regular file.
#[cfg(not(target_os = "macos"))]
#[test]
fn a_bare_store_stamp_does_not_beat_a_sibling_sessions_relogin() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let launch = member("sibling-a");
        let intended = member("sibling-b");
        member_store(&launch);
        let intended_store = member_store(&intended);
        let (swap, _launch_markers) = lone_session(&launch, Isolation::Shared);

        let now = SystemTime::now();
        set_mtime(&intended_store, now - Duration::from_secs(120));
        assert_eq!(
            swap.swap_to("sibling-b").expect("out"),
            SwapOutcome::Swapped
        );

        // The sibling session on B, five seconds before that stamp landed: Claude
        // Code replaced its symlink with a regular file holding a fresh login.
        let sibling = tmp.path().join("sibling-runtime");
        fs::create_dir_all(&sibling).expect("mkdir sibling runtime");
        let sibling_link = cc_relogin(&sibling, CREDS_V1, now - Duration::from_secs(5));

        let written = sync_credentials_unlocked(&sibling_link, &intended_store).expect("sync");
        assert!(
            written,
            "an interactive re-login must survive a swap that only stamped the store"
        );
        assert_eq!(
            fs::read(&intended_store).expect("read intended"),
            CREDS_V1,
            "the sibling's login bytes must land in canonical, not be relinked away"
        );
    });
}

/// A chain-following session revisits members: A→B→A→B is three switches on a
/// two-member chain. The value a swap records as "when these bytes were last
/// written" is therefore often the PREVIOUS swap's own stamp, so it has to be
/// resolved the same way the readers resolve it. Recording a raw mtime instead
/// advances the reported write time by one stamp per revisit, and after a few
/// cycles both decisions are back to reading a bump as a write.
#[cfg(not(target_os = "macos"))]
#[test]
fn a_second_swap_onto_a_member_keeps_reporting_its_real_last_write() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let launch = member("revisit-a");
        let intended = member("revisit-b");
        member_store(&launch);
        let intended_store = member_store(&intended);
        let (swap, _launch_markers) = lone_session(&launch, Isolation::Shared);

        let last_write = SystemTime::now() - Duration::from_secs(300);
        set_mtime(&intended_store, last_write);

        // Onto B, back to A, onto B again — nothing writes B's bytes throughout.
        assert_eq!(
            swap.swap_to("revisit-b").expect("out"),
            SwapOutcome::Swapped
        );
        assert_eq!(
            swap.swap_to("revisit-a").expect("out"),
            SwapOutcome::Swapped
        );
        assert_eq!(
            swap.swap_to("revisit-b").expect("out"),
            SwapOutcome::Swapped
        );

        assert_ne!(
            fs::metadata(&intended_store)
                .expect("meta")
                .modified()
                .expect("mtime"),
            last_write,
            "precondition: the revisit stamped the store again"
        );
        assert_eq!(
            crate::profile_cache::effective_write_time(&intended_store),
            Some(last_write),
            "a stamp displacing an earlier stamp must carry the real write forward"
        );
    });
}

/// On a filesystem whose mtimes truncate, a receipt is WORSE than none: a real
/// write landing in the same tick as the stamp carries the stamp's own mtime, so
/// the receipt would resolve that write back to the value it displaced and invert
/// both credential decisions — on a member whose store was just committed. The
/// swap must therefore leave no receipt there and fall back to the raw mtime,
/// which is the pre-receipt answer rather than a wrong one. This fails silently:
/// drop the guard and every other test still passes.
#[cfg(not(target_os = "macos"))]
#[test]
fn a_truncating_filesystem_gets_no_receipt_at_all() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let launch = member("coarse-a");
        let intended = member("coarse-b");
        member_store(&launch);
        let intended_store = member_store(&intended);
        let (swap, _launch_markers) = lone_session(&launch, Isolation::Shared);

        let last_write = SystemTime::now() - Duration::from_secs(300);
        set_mtime(&intended_store, last_write);

        with_coarse_mtime(|| {
            assert_eq!(swap.swap_to("coarse-b").expect("out"), SwapOutcome::Swapped);
        });

        let receipt = intended_store.with_file_name(crate::profile_cache::TOUCH_RECEIPT_FILE);
        assert!(
            !receipt.exists(),
            "a receipt whose stamp a later write can alias onto must never be written"
        );
        assert_ne!(
            fs::metadata(&intended_store)
                .expect("meta")
                .modified()
                .expect("mtime"),
            last_write,
            "precondition: the swap still stamped, so the refusal is the receipt's alone"
        );
    });
}

/// The other direction: a rotation genuinely writes B's store after the swap, so
/// the stamp's receipt is retired and canonical is the more recent login again.
/// An older re-login must NOT be adopted over it.
#[cfg(not(target_os = "macos"))]
#[test]
fn a_real_write_after_a_stamp_still_keeps_canonical() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let launch = member("rewrite-a");
        let intended = member("rewrite-b");
        member_store(&launch);
        let intended_store = member_store(&intended);
        let (swap, _launch_markers) = lone_session(&launch, Isolation::Shared);

        let now = SystemTime::now();
        set_mtime(&intended_store, now - Duration::from_secs(120));
        assert_eq!(
            swap.swap_to("rewrite-b").expect("out"),
            SwapOutcome::Swapped
        );

        // A rotation commits B's chain after the swap. Its own mtime already
        // moves off the stamp; pinned to a later instant so the assertion does
        // not rest on the filesystem's timestamp granularity.
        crate::profile::save_profile(&intended).expect("commit rotation");
        set_mtime(&intended_store, now + Duration::from_secs(30));
        let canonical_before = fs::read(&intended_store).expect("read intended");

        let sibling = tmp.path().join("sibling-runtime");
        fs::create_dir_all(&sibling).expect("mkdir sibling runtime");
        let sibling_link = cc_relogin(&sibling, CREDS_V1, now - Duration::from_secs(5));

        let written = sync_credentials_unlocked(&sibling_link, &intended_store).expect("sync");
        assert!(
            !written,
            "a store written after the stamp is a real commit and must keep canonical"
        );
        assert_eq!(
            fs::read(&intended_store).expect("read intended"),
            canonical_before,
            "the rotated chain must not be overwritten by an older re-login"
        );
    });
}

/// `--isolated` and fallback-following are mutually exclusive (settled). The
/// executor is the single chokepoint every phase goes through, so the refusal
/// lives here rather than being re-remembered by the decision leg and the flag.
#[cfg(not(target_os = "macos"))]
#[test]
fn an_isolated_session_never_swaps() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let launch = member("iso-a");
        let intended = member("iso-b");
        member_store(&launch);
        let intended_store = member_store(&intended);
        let (swap, _launch_markers) = lone_session(&launch, Isolation::Isolated);

        let before = SystemTime::now() - Duration::from_secs(60);
        set_mtime(&intended_store, before);

        let outcome = swap.swap_to("iso-b").expect("out");
        assert_eq!(
            fs::metadata(&intended_store)
                .expect("meta")
                .modified()
                .expect("mtime"),
            before,
            "a refused swap must touch nothing"
        );
        assert_eq!(outcome, SwapOutcome::Refused(SwapRefused::IsolatedSession));
    });
}

// ── the watchdog's swap leg (`poll`) ─────────────────────────────────────────

/// The shipped inertness: nothing writes `intended_member` until the decision leg
/// lands, so `poll` must be a no-op on every tick of every session today.
#[test]
fn poll_does_nothing_until_the_daemon_names_a_member() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let launch = member("poll-a");
        let intended = member("poll-b");
        member_store(&launch);
        let intended_store = member_store(&intended);
        let (swap, _launch_markers) = lone_session(&launch, Isolation::Shared);
        let sid = swap.session.as_str().to_string();

        let before = SystemTime::now() - Duration::from_secs(60);
        set_mtime(&intended_store, before);

        swap.poll();

        assert_eq!(swap.member(), "poll-a", "no intent, no move");
        assert_eq!(
            fs::metadata(&intended_store)
                .expect("meta")
                .modified()
                .expect("mtime"),
            before
        );
        assert_eq!(
            crate::live_sessions::get(&sid).expect("row").current_member,
            None
        );

        // An intent naming the member the link already resolves to is the steady
        // state, not a refusal — it must stay silent too.
        crate::live_sessions::update_as_daemon(&sid, |d| d.set_intended_member("poll-a"))
            .expect("daemon write");
        swap.poll();
        assert_eq!(swap.member(), "poll-a");
        assert_eq!(
            crate::live_sessions::get(&sid).expect("row").current_member,
            None,
            "an intent equal to the current member must not write the row"
        );
        assert!(
            swap.cell().last_refusal.is_none(),
            "the steady state is not a refusal — routing it through one would log \
             a line per tick for as long as the intent stands"
        );
    });
}

/// The production trigger: the session's own tick reads its own row and executes.
#[cfg(not(target_os = "macos"))]
#[test]
fn poll_executes_the_member_the_daemon_named() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        if !host_poses(tmp.path(), "a real-symlink session for the swap to repoint") {
            return;
        }
        let launch = member("polled-a");
        let intended = member("polled-b");
        member_store(&launch);
        let intended_store = member_store(&intended);
        let (swap, _launch_markers) = lone_session(&launch, Isolation::Shared);
        let sid = swap.session.as_str().to_string();

        crate::live_sessions::update_as_daemon(&sid, |d| d.set_intended_member("polled-b"))
            .expect("daemon write");

        swap.poll();

        assert_eq!(swap.member(), "polled-b");
        assert_eq!(
            fs::read_link(swap.runtime.join(".credentials.json")).expect("read link"),
            intended_store
        );
        let row = crate::live_sessions::get(&sid).expect("row");
        assert_eq!(row.current_member.as_deref(), Some("polled-b"));
        assert_eq!(row.intended_member.as_deref(), Some("polled-b"));
    });
}

/// A standing intent the executor refuses re-fires every tick, so announcing
/// unconditionally writes one line per second for as long as it stands — but a
/// refusal nothing ever says leaves the session on its launch account invisibly.
///
/// The dedupe itself is in-memory cell state with no platform dependency, so only
/// the swap that clears it is gated. The refusals that actually stand in
/// production are `swap_eligible`'s and `NoCredentialStore` — a keychain-first
/// host reaches none of them, since the clamp on `follows_chain` keeps the daemon
/// from ever writing an intent for that row.
#[test]
fn a_standing_refusal_is_announced_once_per_reason() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let launch = member("say-a");
        let intended = member("say-b");
        member_store(&launch);
        member_store(&intended);
        let (swap, _launch_markers) = lone_session(&launch, Isolation::Shared);

        assert!(
            swap.should_announce("say-b", &SwapRefused::NotOauth),
            "the first refusal must be said"
        );
        assert!(
            !swap.should_announce("say-b", &SwapRefused::NotOauth),
            "the same refusal, standing, must not repeat every tick"
        );
        assert!(
            swap.should_announce("say-b", &SwapRefused::Disabled),
            "a changed reason is news"
        );
        assert!(
            swap.should_announce("say-c", &SwapRefused::Disabled),
            "a changed member is news"
        );

        // A landed swap resets it: the next refusal on that member is new
        // information, not a repeat. Unreachable on a host that refuses the swap
        // itself, where the reset has nothing to reset.
        #[cfg(not(target_os = "macos"))]
        {
            assert!(!swap.should_announce("say-c", &SwapRefused::Disabled));
            assert_eq!(swap.swap_to("say-b").expect("out"), SwapOutcome::Swapped);
            assert!(
                swap.should_announce("say-c", &SwapRefused::Disabled),
                "a swap clears the announced state"
            );
        }
    });
}

/// The executor's own platform refusal, which is why `swap_to`'s tests are gated
/// off macOS: it is reached, refuses, and names the cause.
/// `swap_support_refuses_a_shared_tree_and_a_keychain_first_host` pins the pure
/// predicate; this pins that the executor still routes through it, so lifting the
/// refusal without landing the per-config-dir Keychain write cannot pass in
/// silence. Defence in depth rather than a live path: the `follows_chain` clamp
/// means production never asks for a swap here in the first place.
#[cfg(target_os = "macos")]
#[test]
fn a_swap_on_a_keychain_first_host_refuses_with_the_platform_cause() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let launch = member("kc-a");
        let intended = member("kc-b");
        let launch_store = member_store(&launch);
        member_store(&intended);
        let (swap, _launch_markers) = lone_session(&launch, Isolation::Shared);

        assert_eq!(
            swap.swap_to("kc-b")
                .expect("the refusal is an outcome, not an error"),
            SwapOutcome::Refused(SwapRefused::Unsupported(SwapUnsupported::KeychainFirst))
        );
        assert_eq!(
            swap.member(),
            "kc-a",
            "a refused swap moves the cell nowhere"
        );
        // The on-disk artifact, not the cell it was seeded from: `swap_to` moves
        // the link before it publishes, so a refusal that repointed and failed to
        // publish is invisible to `member()` alone.
        assert_eq!(
            fs::read_link(swap.runtime.join(".credentials.json")).expect("read link"),
            launch_store,
            "a refused swap leaves the link on the launch member's store"
        );
    });
}

// ── bare `claude` session markers ────────────────────────────────────────────

/// The whole safety argument for counting bare sessions: their markers live
/// OUTSIDE `profiles/`, so `has_live_session` — which gates delete, disable, and
/// every macOS rotation leg — reads exactly the `clauth start` sessions it read
/// before. Both directions, because a marker namespace that suppressed a real
/// session's marker would be the same defect pointing the other way.
#[test]
fn a_bare_session_marker_is_invisible_to_has_live_session() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let _bare = register_bare_session().expect("register a bare session");
        assert_eq!(
            live_bare_sessions(),
            Some(1),
            "fixture control: the marker must actually be held"
        );

        assert!(
            !has_live_session(&crate::profile::ProfileName::from("work")),
            "a bare `claude` must not gate this profile's delete/disable/rotation"
        );

        let _started =
            hold_session_row_marker(&crate::profile::ProfileName::from("work"), false, "4242-0")
                .expect("hold a session");
        assert!(
            has_live_session(&crate::profile::ProfileName::from("work")),
            "a real `clauth start` session still reads live with a bare marker present"
        );
    });
}

/// A bare session dies without teardown as the normal case (it never ran clauth
/// code), so its marker file outlives it and only GC removes it.
#[test]
fn gc_prunes_a_dead_bare_session_marker() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let dir = tmp.path().join(".clauth").join("live_bare");
        drop(register_bare_session().expect("register a bare session"));
        assert_eq!(
            fs::read_dir(&dir).expect("read live_bare").count(),
            1,
            "fixture control: a released marker's file stays on disk"
        );

        gc_stale_runtimes();

        assert_eq!(
            fs::read_dir(&dir).expect("read live_bare").count(),
            0,
            "a marker nothing holds must be pruned"
        );
    });
}

#[test]
fn gc_spares_a_held_bare_session_marker() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let _bare = register_bare_session().expect("register a bare session");

        gc_stale_runtimes();

        assert_eq!(
            live_bare_sessions(),
            Some(1),
            "GC must not unlink a marker whose session is still running"
        );
    });
}

/// The bare-marker sweep runs at every `clauth mcp` boot, the Plugin tab's
/// 3s-budget probe child included, and the state flock waits up to
/// `STATE_LOCK_TIMEOUT` behind a macOS switch's keychain shell-out. Every other
/// acquisition inside this sweep is conditional on there being work; this one
/// must be too.
#[test]
fn gc_takes_no_state_flock_when_no_bare_marker_exists() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        crate::lock::OUTERMOST_ACQUISITIONS.with(|c| c.set(0));
        gc_stale_runtimes();
        assert_eq!(
            crate::lock::OUTERMOST_ACQUISITIONS.with(|c| c.get()),
            0,
            "a sweep with nothing to collect must not wait on the cross-process lock"
        );

        // Fixture control: with a marker to look at, the sweep DOES lock — or the
        // assertion above would hold against a leg that never runs at all.
        let bare = register_bare_session().expect("register a bare session");
        crate::lock::OUTERMOST_ACQUISITIONS.with(|c| c.set(0));
        gc_stale_runtimes();
        assert_eq!(
            crate::lock::OUTERMOST_ACQUISITIONS.with(|c| c.get()),
            1,
            "the prune itself still runs under the lock, exactly once"
        );

        // The steady state on any box the feature has ever run on, and the arm
        // the first leg does NOT reach: `register_bare_session` mints the dir and
        // the sweep only ever unlinks FILES, so "no bare session running" means an
        // EMPTY dir here, never an absent one. Pinning only the absent case would
        // pin the sweep exactly where it was already free.
        drop(bare);
        let dir = live_bare_dir().expect("bare dir path");
        // The liveness probe is fail-ALIVE, so one transient error can skip a
        // prune; only a persistently-unpruned marker is a regression. Same
        // hardening as `has_live_session_true_when_any_session_alive`, and it
        // keeps a skipped prune from reading as a lock-count failure below.
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let emptied = loop {
            gc_stale_runtimes();
            if fs::read_dir(&dir).expect("read live_bare").next().is_none() {
                break true;
            }
            if std::time::Instant::now() >= deadline {
                break false;
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        assert!(
            emptied && dir.is_dir(),
            "fixture: the dir must survive the prune, emptied — or this leg degrades into the absent case above"
        );

        crate::lock::OUTERMOST_ACQUISITIONS.with(|c| c.set(0));
        gc_stale_runtimes();
        assert_eq!(
            crate::lock::OUTERMOST_ACQUISITIONS.with(|c| c.get()),
            0,
            "an existing-but-empty marker dir must not wait on the lock either"
        );
    });
}

// ── event-driven reconcile ───────────────────────────────────────────────────

/// A Claude Code re-login lands in ONE session's runtime file. It must reach the
/// profile's credential store — and through it every sibling session's view — on
/// the filesystem event, not on the fallback ticker: the point of the event path
/// is that a contended rotation does not sit on a 30 s timer.
///
/// This is the WIRING pin — specs → watcher → reconcile → the sibling's view. It
/// does not on its own separate an event from the 1 Hz credential leg of the
/// polling fallback, since both fit the window; that separation is
/// `watchdog::tests::a_store_publish_reconciles_with_every_ticker_disabled`,
/// which leaves no ticker able to explain a reconcile.
#[cfg(unix)]
#[test]
fn a_relogin_reaches_a_sibling_session_without_waiting_for_the_fallback() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        fake_claude_home(tmp.path());
        let profile = configured_profile("evented");
        let canonical = tmp
            .path()
            .join(".clauth")
            .join("profiles")
            .join("evented")
            .join("credentials.json");
        fs::create_dir_all(canonical.parent().expect("canonical parent")).expect("mkdir store");
        fs::write(&canonical, CREDS_V1).expect("write canonical");
        // Back-date the store so the re-login is unambiguously the later write:
        // `resolve_credential_winner` keeps canonical on an mtime tie.
        set_mtime(&canonical, SystemTime::now() - Duration::from_secs(60));

        let a =
            ProfileRuntime::acquire(&profile, Isolation::Shared, &[], false).expect("acquire a");
        let b =
            ProfileRuntime::acquire(&profile, Isolation::Shared, &[], false).expect("acquire b");

        // Sized at the credential cadence the poll fallback would run: measured
        // convergence here is ~30 ms, so this is a ~30x margin that still fails
        // the moment the event path stops being the thing driving it.
        let window = crate::watchdog::PRODUCTION.credential_poll;
        assert!(
            crate::watchdog::PRODUCTION.fallback > window,
            "fixture: the fallback ticker must not be able to meet the window, \
             or a pass says nothing about the event path"
        );

        // Claude Code's re-login shape: unlink the link, write a regular file.
        let live = a.config_dir().join(".credentials.json");
        fs::remove_file(&live).expect("unlink runtime creds");
        fs::write(&live, CREDS_V2).expect("write re-login");

        let sibling = b.config_dir().join(".credentials.json");
        let started = std::time::Instant::now();
        while started.elapsed() < window {
            if fs::read(&canonical).ok().as_deref() == Some(CREDS_V2)
                && fs::read(&sibling).ok().as_deref() == Some(CREDS_V2)
            {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }

        assert_eq!(
            fs::read(&canonical).expect("read canonical"),
            CREDS_V2,
            "the re-login never reached the store within {window:?}"
        );
        assert_eq!(
            fs::read(&sibling).expect("read sibling"),
            CREDS_V2,
            "the sibling session still resolves the pre-re-login chain after {window:?}"
        );

        drop(b);
        drop(a);
    });
}

/// `LinkMode::Fake` shares ONE tree across every session of a profile, so
/// `copy_tree`, `merge_path` and `mirror_credentials` publish into it
/// concurrently — one set per live session. Under a storm of those publishes the
/// mirror must never observe a torn file, and it must not turn each of its own
/// writes into the next event: the watch now sits on the directory the mirror
/// writes into, so a non-convergent reconcile would feed itself forever.
#[test]
fn the_fake_mode_mirror_converges_under_concurrent_publishes() {
    const WRITERS: usize = 3;
    const ROUNDS: usize = 24;
    const TOKEN: usize = 8;
    const REPEATS: usize = 512;

    /// One payload is `REPEATS` copies of an 8-byte token, so a partially
    /// published file is detectable by shape alone rather than by guessing which
    /// writer's round should have won.
    fn payload(writer: usize, round: usize) -> Vec<u8> {
        format!("w{writer}r{round:05}").repeat(REPEATS).into_bytes()
    }
    fn intact(bytes: &[u8]) -> bool {
        bytes.len() == TOKEN * REPEATS && bytes.chunks(TOKEN).all(|c| c == &bytes[..TOKEN])
    }

    /// Every published entry on one side, as `(name, len, mtime)`. What a write
    /// loop moves and a converged mirror does not — including a rewrite of
    /// identical bytes, which `copy_file`'s rename stamps with a fresh mtime.
    /// Staging siblings are excluded for the same reason production excludes
    /// them: they are not published yet.
    fn shape(side: &Path) -> Vec<(std::ffi::OsString, u64, SystemTime)> {
        let mut out: Vec<_> = fs::read_dir(side)
            .expect("read side")
            .flatten()
            .filter(|e| !crate::watchdog::is_staging(&e.file_name()))
            .filter_map(|e| {
                let meta = e.metadata().ok()?;
                Some((e.file_name(), meta.len(), meta.modified().ok()?))
            })
            .collect();
        out.sort();
        out
    }

    struct Mirror {
        home: PathBuf,
        runtime: PathBuf,
        /// Passes that actually PUBLISHED something. Counting passes instead
        /// cannot tell a self-feeding loop from a notify reader draining a
        /// backlog in dribs — the latter reconciles at the cooldown cap for as
        /// long as the backlog lasts, which is correct behavior.
        writes: std::sync::atomic::AtomicUsize,
        torn: std::sync::Mutex<Vec<String>>,
    }
    impl Mirror {
        fn writes(&self) -> usize {
            self.writes.load(std::sync::atomic::Ordering::Relaxed)
        }
        fn torn(&self) -> Vec<String> {
            self.torn.lock().unwrap_or_else(|p| p.into_inner()).clone()
        }
    }
    impl crate::watchdog::Reconcile for Mirror {
        fn config(&self) {}
        fn credentials(&self) {
            let before = (shape(&self.home), shape(&self.runtime));
            mirror_tree(&self.home, &self.runtime).expect("mirror");
            let after = (shape(&self.home), shape(&self.runtime));
            if before != after {
                self.writes
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            for side in [&self.home, &self.runtime] {
                for entry in fs::read_dir(side).expect("read side").flatten() {
                    // A staging sibling a concurrent `copy_file` is mid-copy into
                    // is half-written by definition and not yet published, so it
                    // is not torn — the same filter production applies.
                    if crate::watchdog::is_staging(&entry.file_name()) {
                        continue;
                    }
                    let path = entry.path();
                    let Ok(bytes) = fs::read(&path) else { continue };
                    if !intact(&bytes) {
                        self.torn
                            .lock()
                            .unwrap_or_else(|p| p.into_inner())
                            .push(path.display().to_string());
                    }
                }
            }
        }
        fn swap_poll(&self) {}
    }

    let tmp = tempfile::tempdir().expect("tempdir");
    let home = tmp.path().join(".claude");
    let runtime = tmp.path().join("runtime");
    let store = tmp.path().join("store").join("credentials.json");
    let src = tmp.path().join("src");
    for dir in [&home, &runtime, &src] {
        fs::create_dir_all(dir).expect("mkdir");
    }
    fs::create_dir_all(store.parent().expect("store parent")).expect("mkdir store");

    let cooldown = Duration::from_millis(100);
    let timings = crate::watchdog::Timings {
        debounce: Duration::from_millis(30),
        cooldown,
        // The fallback must not be able to explain a reconcile, or the
        // quiescence check below cannot tell a write loop from a ticker.
        fallback: Duration::from_secs(600),
        config_poll: Duration::from_secs(600),
        credential_poll: Duration::from_secs(600),
        swap_poll: Duration::from_secs(600),
    };
    let specs = crate::watchdog::watch_specs(&runtime, &store, &home);
    let mirror = Mirror {
        home: home.clone(),
        runtime: runtime.clone(),
        writes: std::sync::atomic::AtomicUsize::new(0),
        torn: std::sync::Mutex::new(Vec::new()),
    };
    let (shutdown_tx, shutdown_rx) = crossbeam_channel::bounded::<()>(1);

    // Armed before the spawn, as `acquire` does it. The soak used to outlast the
    // arm by sheer length, which made the fixture's own "published 0 times"
    // guard the only thing standing between a slow arm and a vacuous pass.
    let watcher = crate::watchdog::try_start(&specs, timings.debounce);
    let requested = specs.len();
    let (shutdown, t, rec) = (&shutdown_rx, &timings, &mirror);

    std::thread::scope(|scope| {
        scope
            .spawn(move || crate::watchdog::run_with_watcher(watcher, requested, shutdown, t, rec));

        let writers: Vec<_> = (0..WRITERS)
            .map(|writer| {
                let (home, runtime, src) = (&home, &runtime, &src);
                scope.spawn(move || {
                    for round in 0..ROUNDS {
                        let staged = src.join(format!("w{writer}"));
                        fs::write(&staged, payload(writer, round)).expect("stage");
                        // The one publish primitive fake mode uses, into
                        // whichever side of the mirror this round targets.
                        let side = if round % 2 == 0 { home } else { runtime };
                        copy_file(&staged, &side.join(format!("shared-{writer}.json")))
                            .expect("publish");
                        std::thread::sleep(Duration::from_millis(5));
                    }
                })
            })
            .collect();
        // JOINED, not slept past: the quiescence measurement below asserts "with
        // no writer running", and a writer the box was too loaded to finish on
        // time turns its own legitimate wakes into a write-loop verdict.
        for writer in writers {
            writer.join().expect("writer");
        }

        // A convergent mirror stops PUBLISHING once the writers stop. It may
        // still run any number of passes — a notify reader draining the writer
        // phase's backlog keeps waking it, which is correct — so the oracle is
        // bytes moved, not passes taken. And its LAST legitimate publish can
        // land arbitrarily late on a loaded box (the debounce + cooldown
        // pacing runs behind the backlog), so quiescence is measured from an
        // OBSERVED plateau, never a fixed sleep: a fixed pre-sample wait
        // turned exactly one late-but-correct convergence pass into a
        // "feeding on its own writes" verdict on the Windows runner. A real
        // self-feed never plateaus and exits through the deadline instead.
        let deadline = std::time::Instant::now() + cooldown * 40;
        let mut settled = mirror.writes();
        loop {
            std::thread::sleep(cooldown * 4);
            let now = mirror.writes();
            if now == settled {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the mirror was still publishing {now} passes in with no writer \
                 running: it is feeding on its own writes"
            );
            settled = now;
        }
        std::thread::sleep(cooldown * 8);
        let after = mirror.writes();
        drop(shutdown_tx);

        assert!(
            settled >= 2,
            "fixture: the mirror published {settled} times during the soak, \
             so quiescence below would hold no matter what the code does"
        );
        assert_eq!(
            after,
            settled,
            "the mirror published {} more times with no writer running: it is \
             feeding on its own writes",
            after - settled
        );
        assert!(
            mirror.torn().is_empty(),
            "the mirror observed torn files: {:?}",
            mirror.torn()
        );
    });

    // One final pass stands in for the next tick: the last publish may have
    // landed after the last mirror ran, and the mirror's contract is that it
    // converges, not that it is instantaneous.
    mirror_tree(&home, &runtime).expect("final mirror");
    for writer in 0..WRITERS {
        let name = format!("shared-{writer}.json");
        let left = fs::read(home.join(&name)).expect("read home side");
        let right = fs::read(runtime.join(&name)).expect("read runtime side");
        assert!(intact(&left), "{name} is torn on the ~/.claude side");
        assert_eq!(left, right, "{name} did not converge across the mirror");
    }
    for side in [&home, &runtime] {
        let orphans: Vec<_> = dir_entry_names(side)
            .into_iter()
            .filter(|n| crate::watchdog::is_staging(n.as_ref()))
            .collect();
        assert!(
            orphans.is_empty(),
            "{} holds staging files the mirror can never delete: {orphans:?}",
            side.display()
        );
    }
}

/// A staging sibling is a publish in flight, on its way to being renamed away.
/// The mirror must walk past one: treating it as tree content fails the tick
/// when the source vanishes between the stat and the copy, and succeeding is
/// worse — the mirror never deletes, so the copy is a permanent orphan.
#[test]
fn the_mirror_walks_past_a_publish_in_flight() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let home = tmp.path().join(".claude");
    let runtime = tmp.path().join("runtime");
    fs::create_dir_all(home.join("plugins")).expect("mkdir home");
    fs::create_dir_all(&runtime).expect("mkdir runtime");
    fs::write(home.join("statusline.sh"), b"#!/bin/sh\n").expect("write real");
    // Nested, because `mirror_tree`'s top-level skip list would mask a walk that
    // recurses into staging siblings one level down.
    let staging = crate::profile::tmp_sibling(&home.join("plugins").join("config.json"));
    fs::write(&staging, b"half-written").expect("write staging");

    mirror_tree(&home, &runtime).expect("mirror");

    assert!(
        runtime.join("statusline.sh").exists(),
        "a real entry must still mirror"
    );
    let name = staging.file_name().expect("staging name");
    assert!(
        !runtime.join("plugins").join(name).exists(),
        "a publish in flight was mirrored as tree content"
    );
}

// ── the rotation refusal's content narrowing ─────────────────────────────────
//
// `rotation_blocked_for` refuses on macOS whenever a `clauth start` session is
// live, because that session's Claude Code holds the pair in a Keychain item
// clauth cannot write. `live_session_holds_rotatable` narrows it to the
// sessions the mechanism can actually reach: signing a session out takes an
// `invalid_grant`, which takes a refresh token to spend. These pin the
// narrowing AND every fail-closed direction, since each unknown here is a
// rotation that must still be refused.

/// A live marker plus its registry row, launched on `store`.
fn live_session_launched_on(profile: &str, sid: &str, store: &Path) -> std::fs::File {
    let sessions = crate::profile::profile_dir(&crate::profile::ProfileName::from(profile))
        .expect("profile_dir")
        .join("sessions");
    fs::create_dir_all(&sessions).expect("mkdir sessions");
    let file = open_pid_file(&sessions.join(sid)).expect("open pid");
    file.lock().expect("lock pid");
    register_row(profile, sid, Some(store.to_path_buf()));
    file
}

/// A registry row built as a literal, so these tests can mint an arbitrary
/// session id and an arbitrary `launch_store` (including the pre-upgrade
/// `None`) without going through `SessionId::mint`.
fn register_row(profile: &str, sid: &str, launch_store: Option<std::path::PathBuf>) {
    crate::live_sessions::register(&crate::live_sessions::LiveSession {
        session_id: sid.to_string(),
        start_profile: profile.to_string(),
        harness: crate::harness::Harness::Claude,
        pid: std::process::id(),
        started_at: 1_700_000_000_000,
        cwd: None,
        isolated: false,
        follows_chain: false,
        intended_member: None,
        chain_cursor: None,
        current_member: None,
        last_swap_at: None,
        launch_store,
    })
    .expect("register row");
}

fn write_creds(path: &Path, refresh: Option<&str>) {
    fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    let body = match refresh {
        Some(rt) => format!(
            r#"{{"claudeAiOauth":{{"accessToken":"at","expiresAt":9999999999999,"refreshToken":"{rt}"}}}}"#
        ),
        None => r#"{"claudeAiOauth":{"accessToken":"at","expiresAt":9999999999999}}"#.to_string(),
    };
    fs::write(path, body).expect("write creds");
}

#[test]
fn a_session_launched_on_a_rotating_pair_still_blocks_rotation() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let store = crate::profile::profile_dir(&crate::profile::ProfileName::from("rot"))
            .expect("profile_dir")
            .join("credentials.json");
        write_creds(&store, Some("rt-live"));
        let _held = live_session_launched_on("rot", "11111-0", &store);
        assert!(
            live_session_holds_rotatable(&crate::profile::ProfileName::from("rot")),
            "a session holding a refresh token is exactly what the refusal protects"
        );
    });
}

#[test]
fn a_session_launched_on_a_refreshless_sidecar_does_not_block_rotation() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let store = crate::profile::profile_dir(&crate::profile::ProfileName::from("roll"))
            .expect("profile_dir")
            .join("session-token.json");
        write_creds(&store, None);
        let _held = live_session_launched_on("roll", "22222-0", &store);
        assert!(
            !live_session_holds_rotatable(&crate::profile::ProfileName::from("roll")),
            "no refresh token means no invalid_grant, so there is nothing to strand"
        );
    });
}

/// The narrowing is feed-agnostic by construction: nothing here sets a flag.
/// An upstream #53 `setup-token` mint answers the same way a rolling token
/// does, and one rotatable session is enough to refuse for the whole profile.
#[test]
fn one_rotatable_session_refuses_for_the_whole_profile() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let dir = crate::profile::profile_dir(&crate::profile::ProfileName::from("mixed"))
            .expect("profile_dir");
        write_creds(&dir.join("session-token.json"), None);
        write_creds(&dir.join("credentials.json"), Some("rt-live"));
        let _a = live_session_launched_on("mixed", "33333-0", &dir.join("session-token.json"));
        let _b = live_session_launched_on("mixed", "44444-0", &dir.join("credentials.json"));
        assert!(live_session_holds_rotatable(
            &crate::profile::ProfileName::from("mixed")
        ));
    });
}

/// Every unknown is a rotation that must still be refused. `acquire` tolerates
/// a failed registration, a row predating `launch_store` deserializes to
/// `None`, and a half-written credential file must never read as "safe".
#[test]
fn every_unknown_launch_store_reads_as_rotatable() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        // (a) a live marker with no registry row at all.
        let sessions = crate::profile::profile_dir(&crate::profile::ProfileName::from("orphan"))
            .expect("profile_dir")
            .join("sessions");
        fs::create_dir_all(&sessions).expect("mkdir");
        let held = open_pid_file(&sessions.join("55555-0")).expect("open pid");
        held.lock().expect("lock");
        assert!(
            live_session_holds_rotatable(&crate::profile::ProfileName::from("orphan")),
            "a marker with no row is an unknown, and unknowns block"
        );
        drop(held);

        // (b) a row from a clauth that predates the field.
        let store = crate::profile::profile_dir(&crate::profile::ProfileName::from("legacy"))
            .expect("profile_dir")
            .join("session-token.json");
        write_creds(&store, None);
        let _l = live_session_launched_on("legacy", "66666-0", &store);
        register_row("legacy", "66666-0", None); // overwrite with a pre-field row
        assert!(
            live_session_holds_rotatable(&crate::profile::ProfileName::from("legacy")),
            "serde(default) must fail CLOSED, or an upgrade silently unblocks rotation"
        );

        // (c) a marker whose name is not a session id at all, so no row can
        // ever be found for it.
        let odd = crate::profile::profile_dir(&crate::profile::ProfileName::from("odd"))
            .expect("profile_dir")
            .join("sessions");
        fs::create_dir_all(&odd).expect("mkdir");
        let stray = open_pid_file(&odd.join("not-a-sid")).expect("open pid");
        stray.lock().expect("lock");
        assert!(
            live_session_holds_rotatable(&crate::profile::ProfileName::from("odd")),
            "an unparseable marker name is an unknown, and unknowns block"
        );
        drop(stray);

        // (d) the store is unreadable / half-written.
        let torn = crate::profile::profile_dir(&crate::profile::ProfileName::from("torn"))
            .expect("profile_dir")
            .join("session-token.json");
        fs::create_dir_all(torn.parent().expect("parent")).expect("mkdir");
        fs::write(&torn, b"{\"claudeAiOauth\":{\"acc").expect("write partial");
        let _t = live_session_launched_on("torn", "77777-0", &torn);
        assert!(
            live_session_holds_rotatable(&crate::profile::ProfileName::from("torn")),
            "a partial read is not proof of absence"
        );
    });
}

#[test]
fn no_live_session_is_not_rotatable() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        assert!(
            !live_session_holds_rotatable(&crate::profile::ProfileName::from("idle")),
            "with nothing live there is nothing to strand"
        );
    });
}

/// The narrowing has to be WIRED, not merely defined. Every test above drives
/// `live_session_holds_rotatable` directly, so deleting its conjunct from
/// `rotation_blocked_for` would leave them all green. macOS-gated because that
/// is the only host where the refusal is armed at all.
#[cfg(target_os = "macos")]
#[test]
fn rotation_blocked_for_reads_what_the_live_session_holds() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let rotating = crate::profile::profile_dir(&crate::profile::ProfileName::from("wired-rot"))
            .expect("profile_dir")
            .join("credentials.json");
        write_creds(&rotating, Some("rt-live"));
        let _a = live_session_launched_on("wired-rot", "88888-0", &rotating);
        assert!(
            rotation_blocked_for(&crate::profile::ProfileName::from("wired-rot")),
            "a live session on a rotating pair must still refuse"
        );

        let refreshless =
            crate::profile::profile_dir(&crate::profile::ProfileName::from("wired-roll"))
                .expect("profile_dir")
                .join("session-token.json");
        write_creds(&refreshless, None);
        let _b = live_session_launched_on("wired-roll", "99999-0", &refreshless);
        assert!(
            !rotation_blocked_for(&crate::profile::ProfileName::from("wired-roll")),
            "the narrowing is not wired into rotation_blocked_for"
        );
    });
}

/// The isolated stores are CLAUDE stores by roster, not by directory shape: a
/// codex profile's dir is skipped before its children are read — folded fix 4's
/// membership half, now that the false dead_code attribute is gone
/// (sessions.rs consumes this fn).
#[test]
fn live_isolated_stores_skip_codex_profiles_by_roster() {
    let home = crate::testutil::HomeSandbox::new();
    let mut locks = Vec::new();
    for name in ["cl", "cx"] {
        let projects = home
            .home()
            .join(format!(".clauth/profiles/{name}/runtime-isolated/projects"));
        fs::create_dir_all(&projects).expect("mkdir projects");
        let sessions = home
            .home()
            .join(format!(".clauth/profiles/{name}/sessions-isolated"));
        fs::create_dir_all(&sessions).expect("mkdir sessions");
        let lock = open_pid_file(&sessions.join("12345")).expect("open pid");
        lock.lock().expect("lock pid");
        locks.push(lock);
    }
    fs::write(
        home.home().join(".clauth/codex-profiles.toml"),
        "profiles = [\"cx\"]\n",
    )
    .expect("write codex roster");

    let stores = live_isolated_stores();

    assert_eq!(
        stores.iter().map(|(p, _)| p.as_str()).collect::<Vec<_>>(),
        ["cl"],
        "the codex twin of an identically live-shaped store is not listed"
    );
}

// ── codex session homes ──────────────────────────────────────────────────────

/// The shared-flavor home per the codex plan's table: auth.json links the
/// profile's ONE physical file (dangling until a login exists — that is the
/// point), the operator surfaces link in, hooks.json only by opt-in, and the
/// durable stores link into the profile-global home.
#[cfg(unix)]
#[test]
fn a_shared_codex_home_links_the_table() {
    let home = crate::testutil::HomeSandbox::new();
    let operator = home.home().join(".codex");
    fs::create_dir_all(operator.join("skills")).expect("mkdir operator skills");
    fs::write(operator.join("AGENTS.md"), b"# agents").expect("write agents");
    fs::write(operator.join("hooks.json"), b"{}").expect("write hooks");
    fs::write(operator.join("config.toml"), b"model = \"o3\"\n").expect("write config");

    let profile = home.home().join(".clauth/profiles/cx");
    fs::create_dir_all(&profile).expect("mkdir profile");
    let session_home = profile.join("codex-home-4242-0");
    crate::profile::mkdir_700(&session_home).expect("mkdir home");

    build_codex_home(&session_home, "cx", Isolation::Shared, LinkMode::Real).expect("build");

    let auth = session_home.join("auth.json");
    assert!(
        auth.symlink_metadata()
            .expect("auth link")
            .file_type()
            .is_symlink(),
        "auth.json is a link, never a copy"
    );
    assert_eq!(
        fs::read_link(&auth).expect("read link"),
        profile.join("auth.json"),
        "…to the profile's one physical file"
    );
    assert!(
        !auth.exists(),
        "dangling until a login is captured — by design"
    );

    assert!(session_home.join("skills").symlink_metadata().is_ok());
    assert!(session_home.join("AGENTS.md").symlink_metadata().is_ok());
    assert!(
        session_home.join("hooks.json").symlink_metadata().is_err(),
        "hooks execute code: never linked by default"
    );
    assert_eq!(
        fs::read_to_string(session_home.join("config.toml")).expect("read config"),
        "model = \"o3\"\n",
        "config.toml is a copy of the operator's (codex writes it in place)"
    );
    for entry in ["memories_1.sqlite", "history.jsonl"] {
        let link = session_home.join(entry);
        assert!(
            link.symlink_metadata()
                .expect("durable link")
                .file_type()
                .is_symlink(),
            "{entry} links into the profile-global home"
        );
        assert_eq!(
            fs::read_link(&link).expect("read link"),
            profile.join("codex-home").join(entry)
        );
    }
    assert!(session_home.join("sessions").is_dir());

    // The opt-in flips exactly the hooks link.
    fs::write(profile.join("config.toml"), b"hooks_json = true\n").expect("write opts");
    build_codex_home(&session_home, "cx", Isolation::Shared, LinkMode::Real).expect("rebuild");
    assert!(
        session_home.join("hooks.json").symlink_metadata().is_ok(),
        "the per-profile opt-in links hooks.json"
    );
}

/// Isolated links NOTHING from the operator and shares nothing durable — only
/// the auth.json link (one physical file in both flavors) and the config copy.
#[cfg(unix)]
#[test]
fn an_isolated_codex_home_links_only_the_auth() {
    let home = crate::testutil::HomeSandbox::new();
    let operator = home.home().join(".codex");
    fs::create_dir_all(operator.join("skills")).expect("mkdir operator skills");
    fs::write(operator.join("AGENTS.md"), b"# agents").expect("write agents");

    let profile = home.home().join(".clauth/profiles/cx");
    fs::create_dir_all(&profile).expect("mkdir profile");
    let session_home = profile.join("codex-home-isolated-4242-0");
    crate::profile::mkdir_700(&session_home).expect("mkdir home");

    build_codex_home(&session_home, "cx", Isolation::Isolated, LinkMode::Real).expect("build");

    assert!(session_home.join("auth.json").symlink_metadata().is_ok());
    assert!(session_home.join("skills").symlink_metadata().is_err());
    assert!(session_home.join("AGENTS.md").symlink_metadata().is_err());
    assert!(
        session_home
            .join("memories_1.sqlite")
            .symlink_metadata()
            .is_err()
    );
    assert!(session_home.join("sessions").is_dir());
}

/// The acquire/teardown lifecycle: a live marker the registry row never
/// outlives, the codex tag and launch_store on the row, and a teardown that
/// removes the per-session home while the durable store — sessions synced
/// back into it — survives.
#[test]
fn codex_acquire_registers_and_teardown_keeps_the_durable_store() {
    let home = crate::testutil::HomeSandbox::new();
    let profile = home.home().join(".clauth/profiles/cx");
    fs::create_dir_all(&profile).expect("mkdir profile");

    let runtime = CodexRuntime::acquire("cx", Isolation::Shared).expect("acquire");
    let session_home = runtime.home().to_path_buf();
    assert!(
        session_home
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(is_codex_home_dir_name),
        "the home carries the codex stem: {session_home:?}"
    );
    assert!(
        has_live_session(&crate::profile::ProfileName::from("cx")),
        "the marker layout makes the shared liveness gates answer for codex"
    );
    let rows = crate::live_sessions::list();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].harness, crate::harness::Harness::Codex);
    assert_eq!(rows[0].start_profile, "cx");
    assert_eq!(
        rows[0].launch_store.as_deref(),
        Some(profile.join("auth.json").as_path()),
        "launch_store names what the session actually holds — the #59 one-liner"
    );

    // A rollout the session wrote must survive the session.
    fs::write(session_home.join("sessions").join("rollout-1.jsonl"), b"{}").expect("write rollout");

    drop(runtime);

    assert!(!session_home.exists(), "the per-session home is torn down");
    assert!(
        profile
            .join("codex-home")
            .join("sessions")
            .join("rollout-1.jsonl")
            .exists(),
        "the rollout was synced into the durable store"
    );
    assert!(
        crate::live_sessions::list().is_empty(),
        "the row went with it"
    );
    assert!(!has_live_session(&crate::profile::ProfileName::from("cx")));
}

/// Under the fake transport the home collapses to the BARE stem — which is
/// the durable store itself — and teardown must never remove it.
#[test]
fn a_fake_mode_codex_home_is_the_durable_store_and_survives_teardown() {
    let home = crate::testutil::HomeSandbox::new();
    let profile = home.home().join(".clauth/profiles/cx");
    fs::create_dir_all(&profile).expect("mkdir profile");
    set_link_mode_override(LinkMode::Fake);

    let runtime = CodexRuntime::acquire("cx", Isolation::Shared).expect("acquire");
    let session_home = runtime.home().to_path_buf();
    assert_eq!(
        session_home,
        profile.join("codex-home"),
        "fake mode lives in the bare stem"
    );
    fs::write(session_home.join("memories_1.sqlite"), b"m").expect("write durable");

    drop(runtime);
    clear_link_mode_override();

    assert!(
        session_home.join("memories_1.sqlite").exists(),
        "teardown must never remove the profile's memory because the last session left"
    );
}
