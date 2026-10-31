#![allow(unsafe_code)]
#![allow(clippy::disallowed_methods)]
use super::*;
use std::fs;
use std::time::{Duration, SystemTime};

use crate::live_sessions::Launcher;
use crate::testutil::{
    HomeSandbox, hold_rotation_lock, respawn_flags_naming, set_mtime, write_claude_job,
    write_claude_job_flags, write_claude_job_in,
};

// V1 expires_at < V2 so tie-break tests can assert which side wins unambiguously.
const CREDS_V1: &[u8] = br#"{"claudeAiOauth":{"accessToken":"tok1","expiresAt":1000}}"#;
const CREDS_V2: &[u8] = br#"{"claudeAiOauth":{"accessToken":"tok2","expiresAt":2000}}"#;

/// `starting` sets the delegate mark from the launcher at registration, so a
/// `clauth start` row reads own and a delegate-spawn row reads a delegate —
/// the carry that keeps a delegate from being tallied as own before its mark
/// lands. The launcher is a typed value, never a bare bool that could swap
/// places with `follows_chain` and mark every start row a delegate.
#[test]
fn starting_marks_only_a_delegate_row() {
    let own = crate::live_sessions::LiveSession::starting(
        &SessionId::for_test("4242-0"),
        "work",
        crate::harness::Harness::Claude,
        false,
        false,
        Launcher::Own,
        None,
        None,
    );
    assert!(!own.is_delegate, "a clauth start row is own");
    let delegate = crate::live_sessions::LiveSession::starting(
        &SessionId::for_test("4242-1"),
        "work",
        crate::harness::Harness::Claude,
        false,
        false,
        Launcher::Delegate,
        None,
        None,
    );
    assert!(
        delegate.is_delegate,
        "a delegate row is marked at registration"
    );
}

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
        // cmd reads a `/` inside an argument as a switch, so mklink gets each
        // path rebuilt in native separators whatever spelling the caller joined.
        let native = |p: &Path| p.components().collect::<std::path::PathBuf>();
        let out = std::process::Command::new("cmd")
            .args(["/D", "/C", "mklink", "/J"])
            .arg(native(link))
            .arg(native(target))
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

/// A linked entry is written through to its target, and that target path
/// reaches `copy_file`'s error text: it is the plain spelling, never std's
/// `\\?\` verbatim form on Windows.
#[test]
fn a_linked_write_target_is_its_target_in_plain_spelling() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let target = tmp.path().join("target");
    fs::create_dir_all(&target).expect("target dir");
    let link = tmp.path().join("link");
    pose_dir_link(&link, &target);
    assert_eq!(write_target(&link), crate::testutil::plain_path(&target));
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

    let _ = mirror_credentials(&runtime, &canonical).expect("mirror");
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

    let _ = mirror_credentials(&runtime, &canonical).expect("mirror");
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

    let _ = mirror_credentials(&runtime, &canonical).expect("mirror");
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

    let _ = mirror_credentials(&runtime, &canonical).expect("mirror");
    assert_eq!(fs::read(&canonical).expect("read"), CREDS_V1);
}

#[test]
fn mirror_credentials_seeds_missing_side() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let canonical = tmp.path().join("nested").join("canonical.json");
    let runtime = tmp.path().join(".credentials.json");
    fs::write(&runtime, CREDS_V1).expect("write runtime");

    let _ = mirror_credentials(&runtime, &canonical).expect("mirror");
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

/// T33b1 r4 F4 (cloudy 2026-10-22): on a copy host the mirror never carries
/// `jobs/` either way — a tree's copy of a `claude rm`ed job would otherwise be
/// published straight back into the store, and a store job copied into a tree
/// would come back from it. Control: an ordinary file still crosses both ways.
#[test]
fn mirror_tree_never_carries_the_job_store_either_way() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let claude = tmp.path().join("claude");
    let runtime = tmp.path().join("runtime");
    fs::create_dir_all(claude.join("jobs").join("aaaa0001")).expect("mkdir store job");
    fs::write(
        claude.join("jobs").join("aaaa0001").join("state.json"),
        b"{}",
    )
    .expect("store job");
    fs::create_dir_all(runtime.join("jobs").join("bbbb0002")).expect("mkdir tree job");
    fs::write(
        runtime.join("jobs").join("bbbb0002").join("state.json"),
        b"{}",
    )
    .expect("tree job");
    fs::write(claude.join("todos.json"), b"[]").expect("canonical control");
    fs::write(runtime.join("notes.json"), b"[]").expect("runtime control");

    mirror_tree(&claude, &runtime).expect("mirror");

    assert!(
        !claude.join("jobs").join("bbbb0002").exists(),
        "a tree's job was published into the store"
    );
    assert!(
        !runtime.join("jobs").join("aaaa0001").exists(),
        "a store job was copied into the tree"
    );
    assert!(
        runtime.join("todos.json").is_file() && claude.join("notes.json").is_file(),
        "control: an ordinary file crosses both ways"
    );
}

/// The build half of F4: a copy-host shared build never copies `jobs/` into the
/// tree. Control: a sibling entry still arrives.
#[test]
fn a_copy_host_build_brings_no_job_store_into_the_tree() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let claude_home = fake_claude_home(tmp.path());
        fs::create_dir_all(claude_home.join("jobs").join("aaaa0001")).expect("mkdir job");
        fs::write(
            claude_home.join("jobs").join("aaaa0001").join("state.json"),
            b"{}",
        )
        .expect("job");
        fs::create_dir_all(claude_home.join("plugins")).expect("mkdir plugins");
        fs::write(claude_home.join("plugins").join("p.json"), b"{}").expect("plugin");
        let runtime = tmp.path().join("runtime");
        fs::create_dir_all(&runtime).expect("mkdir runtime");
        let profile = make_profile("copyhostjobs");
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

        assert!(
            runtime.join("jobs").symlink_metadata().is_err(),
            "the copy-host tree carries no job store"
        );
        assert!(
            runtime.join("plugins").join("p.json").is_file(),
            "control: the walk still copied a sibling entry"
        );
    });
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
/// reads `USERPROFILE`, not `HOME`. The home override is cleared on drop so a
/// panicking test can't leak it into the next test; the process-wide lock
/// timeout override is cleared on entry and on drop the same way `HomeSandbox`
/// does, so a test that panicked mid-override cannot leak a shortened lock
/// deadline into every later `with_fake_home` test.
fn with_fake_home<T>(root: &Path, f: impl FnOnce() -> T) -> T {
    let _lock = crate::profile::HOME_TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    // A prior test that panicked mid-override leaks it into this one: clear it
    // here (and on drop) so a later test never inherits a shortened lock
    // deadline it did not set.
    crate::lock::set_state_lock_timeout_override(None);
    struct ClearOnDrop;
    impl Drop for ClearOnDrop {
        fn drop(&mut self) {
            crate::profile::clear_home_override();
            crate::lock::set_state_lock_timeout_override(None);
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
/// [`LinkMode::Real`] to exist at all. Two shapes need it: a runtime tree PER
/// session, and the real symlink [`lone_session`] hardcodes. Under the shared tree
/// every session of a profile shares one bare-stem tree, and `create_symlink`
/// degrades to a copy `read_link` cannot follow — so such a test
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

/// A shared tree always reaches Claude Code's ONE job store. CC writes a
/// background job's record under `$CLAUDE_CONFIG_DIR/jobs/`, so a box that never
/// ran one has no `~/.claude/jobs` for the build's walk to link — the record
/// would land inside the tree, where the store never names it and the next
/// teardown collects the tree under its own job.
#[test]
fn a_shared_tree_links_jobs_into_the_one_store() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        if !host_poses(tmp.path(), "a shared tree's jobs link") {
            return;
        }
        let claude_home = fake_claude_home(tmp.path());
        assert!(
            !claude_home.join("jobs").exists(),
            "the fixture starts on a box that never wrote a job"
        );
        let profile = configured_profile("jobslink");

        let rt = ProfileRuntime::acquire(&profile, Isolation::Shared, &[], false, Launcher::Own)
            .expect("acquire");

        let store = claude_home.join("jobs");
        assert!(store.is_dir(), "the store is created before the walk");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&store)
                    .expect("store meta")
                    .permissions()
                    .mode()
                    & 0o777,
                0o700,
                "the store lands owner-only like every other clauth write"
            );
        }
        assert_eq!(
            fs::canonicalize(rt.config_dir().join("jobs")).expect("resolve the tree's link"),
            fs::canonicalize(&store).expect("resolve the store"),
            "the per-session tree reaches the one store"
        );
        drop(rt);
    });
}

/// A shared tree links `projects` even on a box with no `~/.claude/projects`
/// yet — the store is created before the walk, so the tree reaches the one
/// transcript store the session index reads.
#[test]
fn a_shared_tree_links_projects_on_a_box_without_the_store() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        if !host_poses(tmp.path(), "a shared tree's projects link") {
            return;
        }
        let claude_home = fake_claude_home(tmp.path());
        assert!(
            !claude_home.join("projects").exists(),
            "the fixture starts on a box that never wrote a transcript"
        );
        let runtime = tmp.path().join("runtime");
        fs::create_dir_all(&runtime).expect("mkdir runtime");
        let profile = make_profile("projectslink");
        let canonical = tmp.path().join("creds.json");

        let notes = build_runtime_dir(
            &runtime,
            &claude_home,
            &profile,
            &canonical,
            LinkMode::Real,
            Isolation::Shared,
        )
        .expect("build");

        assert!(
            notes.is_empty(),
            "a store created successfully raises no note, got: {notes:?}"
        );

        let store = claude_home.join("projects");
        assert!(store.is_dir(), "the store is created before the walk");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&store)
                    .expect("store meta")
                    .permissions()
                    .mode()
                    & 0o777,
                0o700,
                "the store lands owner-only like every other clauth write"
            );
        }
        assert_eq!(
            fs::canonicalize(runtime.join("projects")).expect("resolve the tree's link"),
            fs::canonicalize(&store).expect("resolve the store"),
            "the per-session tree reaches the one store"
        );
    });
}

/// An isolated Real tree links neither `jobs` nor `projects`, and creates
/// neither store. The shared-only gate is what keeps an isolated session's CC
/// (empty settings → default 30-day cleanupPeriodDays) from cleaning the user's
/// transcripts through a linked `projects`, and its `--bg` records from naming
/// a store that never sees them.
#[test]
fn an_isolated_tree_links_no_stores_on_a_box_without_them() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        if !host_poses(tmp.path(), "an isolated tree's store boundary") {
            return;
        }
        let claude_home = fake_claude_home(tmp.path());
        assert!(
            !claude_home.join("jobs").exists(),
            "the fixture starts without a jobs store"
        );
        assert!(
            !claude_home.join("projects").exists(),
            "the fixture starts without a projects store"
        );
        let runtime = tmp.path().join("runtime-isolated");
        fs::create_dir_all(&runtime).expect("mkdir runtime");
        let profile = make_profile("isolatedstores");
        let canonical = tmp.path().join("creds.json");

        build_runtime_dir(
            &runtime,
            &claude_home,
            &profile,
            &canonical,
            LinkMode::Real,
            Isolation::Isolated,
        )
        .expect("build");

        assert!(
            !runtime.join("jobs").exists(),
            "an isolated tree links no jobs store"
        );
        assert!(
            !runtime.join("projects").exists(),
            "an isolated tree links no projects store"
        );
        assert!(
            !claude_home.join("jobs").exists(),
            "an isolated build creates no jobs store"
        );
        assert!(
            !claude_home.join("projects").exists(),
            "an isolated build creates no projects store"
        );
    });
}

/// A store entry that is not a usable dir — a dangling `projects` symlink (a
/// target moved aside or an unmounted disk), a regular `jobs` file — is left
/// to the walk, never a refused start: one moved-aside `~/.claude` target must
/// not take down every acquire.
#[cfg(unix)]
#[test]
fn a_shared_build_skips_a_store_that_is_not_a_usable_dir() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        if !host_poses(tmp.path(), "a shared build's unusable store skip") {
            return;
        }
        let claude_home = fake_claude_home(tmp.path());
        std::os::unix::fs::symlink("gone-projects", claude_home.join("projects"))
            .expect("dangling projects symlink");
        fs::write(claude_home.join("jobs"), b"not a dir").expect("regular jobs file");
        let runtime = tmp.path().join("runtime");
        fs::create_dir_all(&runtime).expect("mkdir runtime");
        let profile = make_profile("badstores");
        let canonical = tmp.path().join("creds.json");

        let notes = build_runtime_dir(
            &runtime,
            &claude_home,
            &profile,
            &canonical,
            LinkMode::Real,
            Isolation::Shared,
        )
        .expect("a store that is not a usable dir skips, never fails the build");

        assert!(
            notes.is_empty(),
            "an unusable store skip raises no note, got: {notes:?}"
        );

        assert!(
            claude_home
                .join("projects")
                .symlink_metadata()
                .expect("projects meta")
                .file_type()
                .is_symlink(),
            "the dangling projects symlink stays a symlink, never a created dir"
        );
        assert!(
            claude_home.join("jobs").is_file(),
            "the regular jobs file stays a file, never a created dir"
        );
        assert!(
            !runtime.join("projects").is_dir(),
            "the tree gets no usable projects store"
        );
        assert!(
            !runtime.join("jobs").is_dir(),
            "the tree gets no usable jobs store"
        );
        // On a linking host the walk links an unusable entry as it stands,
        // `jobs` included: only a copy-host tree skips `jobs` outright.
        assert_eq!(
            fs::read_link(runtime.join("jobs")).expect("the tree links the jobs entry"),
            claude_home.join("jobs"),
            "the tree's jobs links the unusable entry as it stands"
        );
    });
}

/// Claude Code keys a background supervisor's socket on
/// its config dir but keeps its roster in whatever `daemon/` resolves to, so a
/// tree linking `daemon -> ~/.claude/daemon` makes every tree's supervisor share
/// one roster and adopt the others' sessions (measured: the second of two
/// concurrent launches lost its session). A shared build, on either transport,
/// therefore never links or copies `daemon` — nor the lock/status/log the
/// supervisor keys on its config dir; the tree's own supervisor makes a
/// private one. Control: a sibling entry (`plugins`) still reaches the tree.
#[test]
fn a_shared_build_never_brings_the_daemon_dir_into_the_tree() {
    for mode in [LinkMode::Real, LinkMode::Fake] {
        let tmp = tempfile::tempdir().expect("tempdir");
        with_fake_home(tmp.path(), || {
            if mode == LinkMode::Real && !host_poses(tmp.path(), "a linked shared build") {
                return;
            }
            let claude_home = fake_claude_home(tmp.path());
            fs::create_dir_all(claude_home.join("daemon")).expect("mkdir daemon");
            fs::write(claude_home.join("daemon").join("roster.json"), b"{}").expect("roster");
            fs::write(claude_home.join("daemon.json"), b"{}").expect("daemon.json");
            fs::write(claude_home.join("daemon.lock"), b"{}").expect("daemon.lock");
            fs::write(claude_home.join("daemon.status.json"), b"{}").expect("daemon.status.json");
            fs::write(claude_home.join("daemon.log"), b"{}").expect("daemon.log");
            fs::write(claude_home.join("daemon.scheduled.status.json"), b"{}")
                .expect("daemon.scheduled.status.json");
            fs::create_dir_all(claude_home.join("plugins")).expect("mkdir plugins");
            fs::write(claude_home.join("plugins").join("p.json"), b"{}").expect("plugin");
            let runtime = tmp.path().join("runtime");
            fs::create_dir_all(&runtime).expect("mkdir runtime");
            let profile = make_profile("nodaemonlink");
            let canonical = tmp.path().join("creds.json");

            build_runtime_dir(
                &runtime,
                &claude_home,
                &profile,
                &canonical,
                mode,
                Isolation::Shared,
            )
            .expect("build");

            for name in [
                "daemon",
                "daemon.json",
                "daemon.lock",
                "daemon.status.json",
                "daemon.log",
                "daemon.scheduled.status.json",
            ] {
                assert!(
                    runtime.join(name).symlink_metadata().is_err(),
                    "{mode:?}: the tree carries no {name} entry"
                );
            }
            assert!(
                runtime.join("plugins").join("p.json").is_file(),
                "{mode:?}: the walk still brought a sibling entry in"
            );
        });
    }
}

/// the watchdog's bidirectional mirror must skip
/// the supervisor's own state too — a tick must never copy the home
/// `daemon`/lock/status/log into the tree nor the tree's back out, or the
/// shared-roster bug survives on the copy transport.
#[test]
fn the_mirror_never_carries_the_daemon_state_either_way() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let claude_home = fake_claude_home(tmp.path());
        let runtime = tmp.path().join("runtime");
        fs::create_dir_all(&runtime).expect("mkdir runtime");
        for (home, tree) in [
            (claude_home.join("daemon.json"), runtime.join("daemon.json")),
            (claude_home.join("daemon.lock"), runtime.join("daemon.lock")),
            (
                claude_home.join("daemon.status.json"),
                runtime.join("daemon.status.json"),
            ),
            (claude_home.join("daemon.log"), runtime.join("daemon.log")),
            (
                claude_home.join("daemon.scheduled.status.json"),
                runtime.join("daemon.scheduled.status.json"),
            ),
        ] {
            fs::write(&home, b"home").expect("seed home daemon state");
            fs::write(&tree, b"tree").expect("seed tree daemon state");
        }
        fs::create_dir_all(claude_home.join("daemon")).expect("mkdir home daemon");
        fs::write(claude_home.join("daemon").join("roster.json"), b"home").expect("home roster");
        fs::create_dir_all(runtime.join("daemon")).expect("mkdir tree daemon");
        fs::write(runtime.join("daemon").join("roster.json"), b"tree").expect("tree roster");

        mirror_tree(&claude_home, &runtime).expect("mirror");

        for name in [
            "daemon.json",
            "daemon.lock",
            "daemon.status.json",
            "daemon.log",
            "daemon.scheduled.status.json",
        ] {
            assert_eq!(
                fs::read_to_string(claude_home.join(name)).expect("home side"),
                "home",
                "the mirror never publishes the tree's {name} into ~/.claude"
            );
            assert_eq!(
                fs::read_to_string(runtime.join(name)).expect("tree side"),
                "tree",
                "the mirror never copies ~/.claude's {name} into the tree"
            );
        }
        assert_eq!(
            fs::read_to_string(claude_home.join("daemon").join("roster.json"))
                .expect("home roster"),
            "home",
            "the mirror never publishes the tree's roster into ~/.claude/daemon"
        );
        assert_eq!(
            fs::read_to_string(runtime.join("daemon").join("roster.json")).expect("tree roster"),
            "tree",
            "the mirror never copies ~/.claude's roster into the tree's daemon/"
        );
    });
}

/// the GC re-check re-reads the job store — a job landing between
/// the decide and removal holds spares the tree.
#[test]
fn gc_recheck_rereads_the_job_store() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let _claude_home = fake_claude_home(tmp.path());
        let profiles = tmp.path().join(".clauth").join("profiles");
        let runtime = profiles.join("crashed").join("runtime-777-0");
        let sessions = profiles.join("crashed").join("sessions-777-0");
        fs::create_dir_all(&runtime).expect("mkdir runtime");
        fs::create_dir_all(&sessions).expect("mkdir sessions");
        fs::write(sessions.join("777-0"), b"").expect("dead marker");

        set_gc_recheck_hook(Some(Box::new(|runtime, _sessions| {
            let home = crate::profile::home_dir().expect("home");
            crate::testutil::write_claude_job(&home, "interposed", runtime);
        })));
        gc_one_pair_synced(&runtime, &sessions, || {}).expect("gc");
        set_gc_recheck_hook(None);

        assert!(runtime.exists(), "a job landing mid-sweep spares the tree");
    });
}

/// the GC re-check re-reads the marker — a flock-held marker
/// landing between the decide and removal holds spares the tree.
#[test]
fn gc_recheck_rereads_the_marker() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let _claude_home = fake_claude_home(tmp.path());
        let profiles = tmp.path().join(".clauth").join("profiles");
        let runtime = profiles.join("crashed").join("runtime-777-0");
        let sessions = profiles.join("crashed").join("sessions-777-0");
        fs::create_dir_all(&runtime).expect("mkdir runtime");
        fs::create_dir_all(&sessions).expect("mkdir sessions");
        fs::write(sessions.join("777-0"), b"").expect("dead marker");

        set_gc_recheck_hook(Some(Box::new(|_runtime, sessions| {
            let file = open_pid_file(&sessions.join("777-0")).expect("open marker");
            file.lock().expect("flock the marker");
            gc_held_marker_set(file);
        })));
        gc_one_pair_synced(&runtime, &sessions, || {}).expect("gc");
        set_gc_recheck_hook(None);
        let _held = gc_held_marker_take();

        assert!(
            runtime.exists(),
            "a marker landing mid-sweep spares the tree"
        );
    });
}

/// the GC re-check re-reads the lock file — a supervisor that appeared
/// between the decide and removal holds (a fresh `daemon.lock` naming a live
/// pid) spares the tree.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn gc_recheck_rereads_the_lock_file() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let _claude_home = fake_claude_home(tmp.path());
        let profiles = tmp.path().join(".clauth").join("profiles");
        let runtime = profiles.join("crashed").join("runtime-777-0");
        let sessions = profiles.join("crashed").join("sessions-777-0");
        fs::create_dir_all(&runtime).expect("mkdir runtime");
        fs::create_dir_all(&sessions).expect("mkdir sessions");
        fs::write(sessions.join("777-0"), b"").expect("dead marker");

        let sup = crate::testutil::FakeSupervisor::spawn(&runtime, false);
        set_gc_recheck_hook(Some(Box::new(move |runtime, _sessions| {
            crate::testutil::write_daemon_lock(runtime, sup.pid, &sup.proc_start);
        })));
        gc_one_pair_synced(&runtime, &sessions, || {}).expect("gc");
        set_gc_recheck_hook(None);

        assert!(
            runtime.exists(),
            "a supervisor landing mid-sweep spares the tree"
        );
    });
}

/// the teardown re-check re-reads the job store — a job landing
/// between the stop and the removal hold spares the tree.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn drop_recheck_rereads_the_job_store() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let _claude_home = fake_claude_home(tmp.path());
        let profile = configured_profile("drop-recheck");
        let rt = ProfileRuntime::acquire(&profile, Isolation::Shared, &[], false, Launcher::Own)
            .expect("acquire");
        let tree = rt.config_dir().to_path_buf();
        let sup = crate::testutil::FakeSupervisor::spawn(&tree, false);
        crate::testutil::write_daemon_lock(&tree, sup.pid, &sup.proc_start);

        set_drop_recheck_hook(Some(Box::new(move |runtime| {
            let home = crate::profile::home_dir().expect("home");
            crate::testutil::write_claude_job(&home, "interposed", runtime);
        })));
        drop(rt);
        set_drop_recheck_hook(None);

        assert!(tree.exists(), "a job landing mid-teardown spares the tree");
    });
}

/// the teardown re-check re-reads the MARKER — a flock-held
/// marker landing between the stop and the removal hold spares the tree.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn drop_recheck_rereads_the_marker() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let _claude_home = fake_claude_home(tmp.path());
        let profile = configured_profile("drop-marker");
        let rt = ProfileRuntime::acquire(&profile, Isolation::Shared, &[], false, Launcher::Own)
            .expect("acquire");
        let tree = rt.config_dir().to_path_buf();
        let sessions = rt.sessions_dir().to_path_buf();
        let sup = crate::testutil::FakeSupervisor::spawn(&tree, false);
        crate::testutil::write_daemon_lock(&tree, sup.pid, &sup.proc_start);
        let marker = sessions.join("777-0");

        set_drop_recheck_hook(Some(Box::new(move |_runtime| {
            let file = open_pid_file(&marker).expect("open marker");
            file.lock().expect("flock the marker");
            drop_held_marker_set(file);
        })));
        drop(rt);
        set_drop_recheck_hook(None);
        let _held = drop_held_marker_take();

        assert!(
            tree.exists(),
            "a marker landing mid-teardown spares the tree"
        );
    });
}

/// the teardown re-check re-reads the LOCK FILE — a supervisor
/// that appeared between the stop and the removal hold (a fresh `daemon.lock`
/// naming a live pid) spares the tree.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn drop_recheck_rereads_the_lock_file() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let _claude_home = fake_claude_home(tmp.path());
        let profile = configured_profile("drop-lock");
        let rt = ProfileRuntime::acquire(&profile, Isolation::Shared, &[], false, Launcher::Own)
            .expect("acquire");
        let tree = rt.config_dir().to_path_buf();
        let sup = crate::testutil::FakeSupervisor::spawn(&tree, false);
        crate::testutil::write_daemon_lock(&tree, sup.pid, &sup.proc_start);
        // A second supervisor lands only in the hook, after the first is
        // SIGTERMed; its lock is what the re-check must re-read.
        let landing = crate::testutil::FakeSupervisor::spawn(&tree, false);

        set_drop_recheck_hook(Some(Box::new(move |runtime| {
            crate::testutil::write_daemon_lock(runtime, landing.pid, &landing.proc_start);
        })));
        drop(rt);
        set_drop_recheck_hook(None);

        assert!(
            tree.exists(),
            "a supervisor landing mid-teardown spares the tree"
        );
    });
}

/// a teardown that spares the tree on the re-check (a supervisor
/// landed after the stop) still clears its in-flight row — without the clear,
/// every Fake-mode acquire on the profile bails "a supervisor stop is in
/// flight" until the row ages out.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn teardown_spare_clears_the_in_flight_record() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let _claude_home = fake_claude_home(tmp.path());
        let profile = configured_profile("drop-clear");
        let rt = ProfileRuntime::acquire(&profile, Isolation::Shared, &[], false, Launcher::Own)
            .expect("acquire");
        let tree = rt.config_dir().to_path_buf();
        let sup = crate::testutil::FakeSupervisor::spawn(&tree, false);
        crate::testutil::write_daemon_lock(&tree, sup.pid, &sup.proc_start);
        let landing = crate::testutil::FakeSupervisor::spawn(&tree, false);

        set_drop_recheck_hook(Some(Box::new(move |runtime| {
            crate::testutil::write_daemon_lock(runtime, landing.pid, &landing.proc_start);
        })));
        drop(rt);
        set_drop_recheck_hook(None);

        assert!(tree.exists(), "the re-check spare kept the tree");
        let key = stop_in_flight_key(&tree);
        let journal = load_stops_in_flight().expect("journal");
        assert!(
            !journal.stops.iter().any(|row| row.tree == key),
            "the teardown spare cleared the in-flight row: {:?}",
            journal.stops
        );
    });
}

/// the mirror skip's deterministic arm — a supervisor state file
/// present on ONE side only never crosses to the missing side. The
/// both-sides-seeded test ties on mtime inside a clock tick; a missing side
/// has no mtime to tie, so it reds on every run the skip is removed.
#[test]
fn the_mirror_never_copies_the_daemon_state_into_a_missing_side() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let claude_home = fake_claude_home(tmp.path());
        let runtime = tmp.path().join("runtime");
        fs::create_dir_all(&runtime).expect("mkdir runtime");
        fs::write(claude_home.join("daemon.json"), b"home").expect("seed home daemon.json");
        fs::write(claude_home.join("daemon.lock"), b"home").expect("seed home lock");
        fs::write(claude_home.join("daemon.scheduled.status.json"), b"home")
            .expect("seed home scheduled status");
        fs::create_dir_all(claude_home.join("daemon")).expect("mkdir home daemon");
        fs::write(claude_home.join("daemon").join("roster.json"), b"home").expect("home roster");

        mirror_tree(&claude_home, &runtime).expect("mirror");

        assert!(
            runtime.join("daemon.json").symlink_metadata().is_err(),
            "the mirror never copies ~/.claude's daemon.json into a missing tree side"
        );
        assert!(
            runtime.join("daemon.lock").symlink_metadata().is_err(),
            "the mirror never copies ~/.claude's daemon.lock into a missing tree side"
        );
        assert!(
            runtime
                .join("daemon.scheduled.status.json")
                .symlink_metadata()
                .is_err(),
            "the mirror never copies ~/.claude's daemon.scheduled.status.json into a missing tree side"
        );
        assert!(
            runtime.join("daemon").symlink_metadata().is_err(),
            "the mirror never copies ~/.claude's daemon/ into a missing tree side"
        );
    });
}

/// acquire consults the in-flight record — a Fake-mode shared
/// bare tree with a stop in flight refuses the start, instead of launching a
/// session whose shared supervisor the stop is about to end.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn acquire_refuses_a_shared_tree_with_a_stop_in_flight() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let _claude_home = fake_claude_home(tmp.path());
        with_link_mode(LinkMode::Fake, || {
            let profile = configured_profile("in-flight");
            let runtime = crate::profile::profile_dir(&profile.name)
                .expect("profile dir")
                .join("runtime");
            crate::profile::mkdir_700(&runtime).expect("mkdir runtime");
            with_state_lock(|_held| record_stop_in_flight_locked(&runtime)).expect("stamp");

            let err =
                ProfileRuntime::acquire(&profile, Isolation::Shared, &[], false, Launcher::Own)
                    .err()
                    .expect("an in-flight stop must refuse the acquire");
            assert_eq!(
                err.to_string(),
                format!(
                    "clauth is stopping Claude Code's background service in {}; retry in a moment",
                    runtime.display()
                ),
                "the refusal names the in-flight stop"
            );
        });
    });
}

/// The background pair keeps one runtime path whatever the sid, so a `--bg`
/// launch meeting a stop in flight there refuses instead of handing its job to
/// the supervisor the stop is about to end.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn acquire_background_refuses_a_stop_in_flight() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let _claude_home = fake_claude_home(tmp.path());
        with_link_mode(LinkMode::Real, || {
            let profile = configured_profile("bg-in-flight");
            let runtime = crate::profile::profile_dir(&profile.name)
                .expect("profile dir")
                .join("runtime-bg");
            crate::profile::mkdir_700(&runtime).expect("mkdir runtime-bg");
            with_state_lock(|_held| record_stop_in_flight_locked(&runtime)).expect("stamp");

            let err = ProfileRuntime::acquire_background(
                &profile.name,
                BackgroundHold::Launcher,
                &[],
                Some("--bg"),
            )
            .err()
            .expect("an in-flight stop must refuse the background acquire");
            assert_eq!(
                err.to_string(),
                format!(
                    "clauth is stopping Claude Code's background service in {}; retry in a moment",
                    runtime.display()
                ),
                "the refusal names the in-flight stop"
            );
            let markers = crate::profile::profile_dir(&profile.name)
                .expect("profile dir")
                .join("sessions-bg");
            assert!(
                fs::read_dir(&markers).map_or(true, |mut dir| dir.next().is_none()),
                "a refused launch leaves no marker in {}",
                markers.display()
            );
        });
    });
}

/// acquire declines the wipe of a tree whose own supervisor may
/// be alive — the additive build keeps the tree and its sentinel, never
/// `remove_dir_all`s under a live supervisor.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn acquire_declines_the_wipe_of_a_tree_whose_supervisor_may_be_alive() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let _claude_home = fake_claude_home(tmp.path());
        with_link_mode(LinkMode::Fake, || {
            let profile = configured_profile("live-sup");
            let runtime = crate::profile::profile_dir(&profile.name)
                .expect("profile dir")
                .join("runtime");
            crate::profile::mkdir_700(&runtime).expect("mkdir runtime");
            fs::write(runtime.join("sentinel"), b"keep").expect("sentinel");
            let sup = crate::testutil::FakeSupervisor::spawn(&runtime, false);
            crate::testutil::write_daemon_lock(&runtime, sup.pid, &sup.proc_start);

            let rt =
                ProfileRuntime::acquire(&profile, Isolation::Shared, &[], false, Launcher::Own)
                    .expect("acquire");
            assert!(
                runtime.join("sentinel").exists(),
                "a live supervisor spares the wipe; the sentinel survives"
            );
            drop(rt);
        });
    });
}

/// an unparseable stop journal is logged once and treated as empty —
/// every acquire still proceeds instead of bailing on the parse error.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn an_unparseable_stop_journal_reads_empty_and_logs_once() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let _claude_home = fake_claude_home(tmp.path());
        with_link_mode(LinkMode::Fake, || {
            let profile = configured_profile("corrupt");
            let runtime = crate::profile::profile_dir(&profile.name)
                .expect("profile dir")
                .join("runtime");
            crate::profile::mkdir_700(&runtime).expect("mkdir runtime");
            let journal = stop_in_flight_path().expect("journal path");
            crate::profile::mkdir_700(journal.parent().expect("journal parent"))
                .expect("mkdir .clauth");
            fs::write(&journal, b"{ not json").expect("corrupt journal");

            let lines = crate::logline::LogLines::new();
            let _capture = lines.capture_here();
            let rt =
                ProfileRuntime::acquire(&profile, Isolation::Shared, &[], false, Launcher::Own)
                    .expect("the corrupt journal does not brick the acquire");
            drop(rt);
            let rt =
                ProfileRuntime::acquire(&profile, Isolation::Shared, &[], false, Launcher::Own)
                    .expect("the corrupt journal does not brick the second acquire");
            drop(rt);
            assert_eq!(
                lines.snapshot(),
                vec![format!(
                    "clauth: treating an unparseable stop journal {} as empty",
                    journal.display()
                )],
                "the corrupt journal logged exactly one line"
            );
        });
    });
}

/// The `procStart` formatter renders `pbi_start_tvsec` as
/// `asctime(gmtime(...))`. The first two pairs are the mac probe's measured
/// values; the third (a single-digit day, space-padded `%e`) is hand-computed
/// via `date -u -d @1791158400` = `Mon Oct  5 00:00:00 2026`, independent of
/// the code under test.
#[test]
fn proc_start_formatter_matches_the_macos_probe() {
    assert_eq!(format_proc_start(1792720759), "Fri Oct 23 01:59:19 2026");
    assert_eq!(format_proc_start(1792721082), "Fri Oct 23 02:04:42 2026");
    assert_eq!(format_proc_start(1791158400), "Mon Oct  5 00:00:00 2026");
    for tvsec in [1792720759u64, 1792721082, 1791158400] {
        assert_eq!(
            parse_proc_start(&format_proc_start(tvsec)),
            Some(tvsec),
            "the parser inverts the formatter"
        );
    }
    // A string in another format is unparseable (unverifiable), never a reused pid.
    assert_eq!(parse_proc_start("2026-10-23T01:59:19Z"), None);
    assert_eq!(parse_proc_start(""), None);
    // A well-formed string with the WRONG weekday does not parse: the weekday
    // guard is the only thing that keeps a corrupted date out of the parsed
    // instant.
    assert_eq!(parse_proc_start("Thu Oct 23 01:59:19 2026"), None);
}

/// a recorded `procStart` that parses but differs from the live start
/// reads `Gone` only when the live start is strictly LATER (a reused pid);
/// anything else — a future format, or a parsed instant not earlier than the
/// live one — reads `Unverifiable` and spares.
#[test]
fn start_mismatch_verdict_only_reads_gone_for_a_later_live_start() {
    let recorded = "Fri Oct 23 01:59:19 2026"; // epoch 1792720759
    assert_eq!(
        start_mismatch_verdict(1792720760, recorded, false),
        SupervisorState::Gone,
        "a strictly-later live start is a reused pid"
    );
    assert_eq!(
        start_mismatch_verdict(1792720759, recorded, false),
        SupervisorState::Unverifiable { stale_start: None },
        "an equal instant is not a mismatch verdict (the caller handles equality)"
    );
    assert_eq!(
        start_mismatch_verdict(1792720758, recorded, false),
        SupervisorState::Unverifiable { stale_start: None },
        "an earlier-than-recorded live start spares, never collects"
    );
    assert_eq!(
        start_mismatch_verdict(1792720760, "2026-10-23T01:59:19Z", false),
        SupervisorState::Unverifiable { stale_start: None },
        "an unparseable recorded string spares"
    );
}

/// a live pid whose argv holds `daemon run` and whose config dir is this tree
/// IS this tree's supervisor even when the recorded instant differs (a build
/// that rendered it in a timezone west of UTC) — `Unverifiable` naming the
/// stale recorded start, never `Gone`.
#[test]
fn start_mismatch_verdict_reads_a_matching_supervisor_as_unverifiable() {
    assert_eq!(
        start_mismatch_verdict(1792720759, "Thu Oct 22 20:59:19 2026", true),
        SupervisorState::Unverifiable {
            stale_start: Some("Thu Oct 22 20:59:19 2026".to_string())
        },
        "a matching supervisor spares whatever the start string says"
    );
}

/// the zombie-fallback decision: a `kinfo_proc` read proves the pid gone only
/// when the sysctl reports it absent or a `SZOMB` zombie — any live non-zombie
/// state and every other read failure stays unreadable (spare). The zombie
/// value is pinned as the literal `5` from `<sys/proc.h>` (`SZOMB`), never the
/// code's own constant: a wrong constant would read a STOPPED live supervisor
/// (`SSTOP` = 4) as gone and this test would stay green.
#[test]
fn kinfo_proc_read_decides_gone_only_for_zombie_or_absent() {
    assert!(
        kinfo_proc_reads_gone(KinfoProcRead::Absent),
        "absent reads gone"
    );
    assert!(
        kinfo_proc_reads_gone(KinfoProcRead::Present(5)),
        "a zombie (SZOMB = 5) reads gone"
    );
    assert!(
        !kinfo_proc_reads_gone(KinfoProcRead::Present(4)),
        "a stopped live supervisor (SSTOP = 4) must never read gone"
    );
    assert!(
        !kinfo_proc_reads_gone(KinfoProcRead::Present(2)),
        "a live non-zombie reads unreadable"
    );
    assert!(
        !kinfo_proc_reads_gone(KinfoProcRead::Unreadable),
        "a failed read reads unreadable"
    );
    #[cfg(target_os = "macos")]
    assert_eq!(
        u32::from(P_STAT_SZOMB),
        libc::SZOMB,
        "the local SZOMB constant matches libc's Apple binding"
    );
}

/// the `kinfo_proc` second-read decision: `size` bounds the read, so the
/// first pass's buffer tail never counts. An empty read (the pid vanished
/// between the sizing call and the read, so the kernel wrote no entry) is
/// `Absent`; a read shorter than the `KinfoProcStat` prefix (40 bytes) is
/// `Unreadable`; a full-enough read reports the `p_stat` byte.
#[test]
fn kinfo_proc_second_read_decides_empty_short_and_present() {
    assert_eq!(
        kinfo_proc_second_read(&[], 0),
        KinfoProcRead::Absent,
        "an empty second read is the pid vanishing, not unreadable"
    );
    // A full-sized, zeroed buffer whose second read reported zero bytes is
    // still the pid vanishing: the first pass's sizing must not turn the
    // buffer's tail into an entry.
    let mut full = vec![0u8; std::mem::size_of::<KinfoProcStat>()];
    assert_eq!(
        kinfo_proc_second_read(&full, 0),
        KinfoProcRead::Absent,
        "a zero-byte second read over a sized buffer is the pid vanishing"
    );
    // A read too short to hold the `p_stat` byte (offset 36) is neither empty
    // nor a whole entry — unreadable, never a guess at `p_stat`.
    let short = vec![0u8; P_STAT_OFFSET];
    assert_eq!(
        kinfo_proc_second_read(&short, short.len()),
        KinfoProcRead::Unreadable,
        "a short read holds no p_stat byte"
    );
    // The threshold is the whole prefix, not the `p_stat` offset: a read one
    // byte short of the entry holds `p_stat` and still is not an entry.
    let mut one_short = vec![0u8; std::mem::size_of::<KinfoProcStat>() - 1];
    one_short[P_STAT_OFFSET] = 5;
    assert_eq!(
        kinfo_proc_second_read(&one_short, one_short.len()),
        KinfoProcRead::Unreadable,
        "a read one byte short of the entry is unreadable"
    );
    full[P_STAT_OFFSET] = 5;
    assert_eq!(
        kinfo_proc_second_read(&full, full.len()),
        KinfoProcRead::Present(5),
        "a full read reports the p_stat byte"
    );
    assert_eq!(
        kinfo_proc_second_read(&full, full.len() + 1),
        KinfoProcRead::Unreadable,
        "a reported size past the buffer is no read of it"
    );
}

/// a live fake supervisor whose recorded `procStart` differs from its live
/// start but whose argv + env still match this tree spares — `Unverifiable`
/// naming the stale recorded start, never `Gone` — and the spare line names
/// that stale start. The recorded string parses and is strictly EARLIER than
/// the live start on macOS, so dropping the argv/env read there reads `Gone`
/// and reds this test.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn a_live_matching_supervisor_with_a_stale_recorded_start_spares() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let _claude_home = fake_claude_home(tmp.path());
        let tree = tmp.path().join("tree");
        fs::create_dir_all(&tree).expect("mkdir tree");
        let sup = crate::testutil::FakeSupervisor::spawn(&tree, false);
        // Linux never parses the recorded string in the mismatch arm; any
        // differing value works. macOS parses it, so it must be strictly
        // earlier than the live start for the drop-the-read plant to red.
        #[cfg(target_os = "linux")]
        let stale = "Thu Oct 22 20:59:19 2026".to_string();
        #[cfg(target_os = "macos")]
        let stale = {
            let live = crate::runtime::proc_bsdinfo(sup.pid)
                .expect("live start")
                .pbi_start_tvsec;
            crate::runtime::format_proc_start(live - 18000)
        };
        crate::testutil::write_daemon_lock(&tree, sup.pid, &stale);

        assert_eq!(
            supervisor_state(&tree),
            SupervisorState::Unverifiable {
                stale_start: Some(stale.clone())
            },
            "a matching supervisor with a stale recorded start spares, never Gone"
        );

        let lines = crate::logline::LogLines::new();
        let _capture = lines.capture_here();
        assert_eq!(
            stop_tree_supervisor(&tree),
            SupervisorStop::Spare,
            "the stale-start verdict spares the tree"
        );
        assert_eq!(
            lines.snapshot(),
            vec![format!(
                "clauth: {}: sparing: its supervisor's recorded start {stale:?} is stale (the pid still reads as this tree's supervisor)",
                tree.display()
            )],
            "the spare line names the stale recorded start"
        );
    });
}

/// A `daemon.lock` recording a pid of `0` or above `i32::MAX` is a malformed
/// record: `read_supervisor_record` returns `Err` (the corrupt-lock path
/// spares as unverifiable), so the pid never reaches `pid_alive`'s
/// `kill(pid, 0)` probe. `kill(0, 0)` probes the caller's own process group,
/// and a pid above `i32::MAX` casts to a negative pid (`4294967295` ->
/// `kill(-1, 0)`, every process the user owns). The stop then spares the tree
/// with no signal. The linux verify would also spare a bogus pid as
/// unreadable, so the boundary read is the red the plant targets.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn a_bogus_lock_pid_is_a_malformed_record_that_never_signals() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let _claude_home = fake_claude_home(tmp.path());
        let tree = tmp.path().join("tree");
        fs::create_dir_all(&tree).expect("mkdir tree");
        let start = "Thu Oct 22 20:59:19 2026";

        for pid in [0u32, i32::MAX as u32 + 1, u32::MAX] {
            crate::testutil::write_daemon_lock(&tree, pid, start);
            assert_eq!(
                read_supervisor_record(&tree),
                Err(()),
                "a lock pid of {pid} is a malformed record, never a signal target"
            );
        }

        // The malformed record spares with no signal: the corrupt-lock path
        // returns `Unverifiable`, which the stop path spares without reaching
        // the kill.
        crate::testutil::write_daemon_lock(&tree, u32::MAX, start);
        let fired = std::rc::Rc::new(std::cell::Cell::new(0));
        {
            let fired = fired.clone();
            crate::runtime::set_signal_send_hook(Some(Box::new(move |_| {
                fired.set(fired.get() + 1);
            })));
        }
        assert_eq!(
            stop_tree_supervisor(&tree),
            SupervisorStop::Spare,
            "a bogus lock pid spares the tree"
        );
        crate::runtime::set_signal_send_hook(None);
        assert_eq!(fired.get(), 0, "no signal reached the bogus pid");
    });
}

/// The `daemon.status.json` fallback is the same trust boundary as
/// `daemon.lock`: a status pid of `0` (the caller's own process group) or
/// above `i32::MAX` (a negative pid: `4294967295` -> `kill(-1, 0)`) is a
/// malformed record and reads `Err`, so the stop spares the tree with no
/// signal. The control row proves the reader really parses the status file
/// rather than ignoring it.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn a_bogus_status_pid_is_a_malformed_record_too() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let _claude_home = fake_claude_home(tmp.path());
        let tree = tmp.path().join("tree");
        fs::create_dir_all(&tree).expect("mkdir tree");
        let start = "Thu Oct 22 20:59:19 2026";

        for pid in [0u32, i32::MAX as u32 + 1, u32::MAX] {
            crate::testutil::write_daemon_status(&tree, pid, start);
            assert_eq!(
                read_supervisor_record(&tree),
                Err(()),
                "a status pid of {pid} is a malformed record, never a signal target"
            );
        }

        crate::testutil::write_daemon_status(&tree, u32::MAX, start);
        let fired = std::rc::Rc::new(std::cell::Cell::new(0));
        {
            let fired = fired.clone();
            crate::runtime::set_signal_send_hook(Some(Box::new(move |_| {
                fired.set(fired.get() + 1);
            })));
        }
        assert_eq!(
            stop_tree_supervisor(&tree),
            SupervisorStop::Spare,
            "a bogus status pid spares the tree"
        );
        crate::runtime::set_signal_send_hook(None);
        assert_eq!(fired.get(), 0, "no signal reached the bogus pid");

        crate::testutil::write_daemon_status(&tree, 4242, start);
        assert_eq!(
            read_supervisor_record(&tree),
            Ok(Some((4242, start.to_string()))),
            "the control row parses the status file"
        );
    });
}

/// Both pid edges of the boundary are ACCEPTED, from the lock and from the
/// status fallback: `1` and `i32::MAX` are positive pids that fit `pid_t`, so
/// a record naming one must read as this tree's supervisor instead of
/// sparing it as malformed.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn the_accepted_pid_edges_read_from_both_files() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let tree = tmp.path().join("tree");
    fs::create_dir_all(&tree).expect("mkdir tree");
    let start = "Thu Oct 22 20:59:19 2026";

    for pid in [1u32, i32::MAX as u32] {
        crate::testutil::write_daemon_lock(&tree, pid, start);
        assert_eq!(
            read_supervisor_record(&tree),
            Ok(Some((pid, start.to_string()))),
            "a lock pid of {pid} is a valid kill target"
        );
        fs::remove_file(tree.join("daemon.lock")).expect("remove daemon.lock");

        crate::testutil::write_daemon_status(&tree, pid, start);
        assert_eq!(
            read_supervisor_record(&tree),
            Ok(Some((pid, start.to_string()))),
            "a status pid of {pid} is a valid kill target"
        );
        fs::remove_file(tree.join("daemon.status.json")).expect("remove daemon.status.json");
    }
}

/// the macOS `KERN_PROCARGS2` parser — `[int argc][exec path NUL][NUL
/// padding][argc argv NUL][env NUL]` — parses argv and env out of hand-built
/// buffers on every OS, and the argv/env checks see what the verify needs.
#[test]
fn parse_proc_args2_parses_the_macos_layout() {
    let tree = tempfile::tempdir().expect("tempdir");
    let tree_path = tree.path().to_path_buf();
    let args2_buffer = |argc: i32, argv: &[&[u8]], environ: &[&[u8]]| -> Vec<u8> {
        let mut buf = Vec::new();
        buf.extend_from_slice(&argc.to_ne_bytes());
        buf.extend_from_slice(b"/bin/claude");
        buf.push(0);
        buf.extend_from_slice(&[0, 0, 0]); // NUL padding to the argv boundary
        for arg in argv {
            buf.extend_from_slice(arg);
            buf.push(0);
        }
        for entry in environ {
            buf.extend_from_slice(entry);
            buf.push(0);
        }
        buf
    };

    // argv hit (argv holds `daemon run`) + env hit (CLAUDE_CONFIG_DIR resolves
    // to the tree).
    let ccd = format!("CLAUDE_CONFIG_DIR={}", tree_path.display());
    let buf = args2_buffer(
        4,
        &[b"/bin/claude", b"daemon", b"run", b"--origin"],
        &[ccd.as_bytes()],
    );
    let args = parse_proc_args2(&buf).expect("a well-formed buffer parses");
    let argv: Vec<&[u8]> = args.argv.iter().map(Vec::as_slice).collect();
    assert!(argv_holds_daemon_run(&argv), "argv hit: daemon run found");
    assert!(
        environ_bytes_config_dir_is(&args.environ, &tree_path),
        "env hit: config dir matches the tree"
    );

    // env miss: no CLAUDE_CONFIG_DIR entry -> false.
    let miss = args2_buffer(2, &[b"/bin/claude", b"daemon"], &[b"PATH=/usr/bin"]);
    let args = parse_proc_args2(&miss).expect("the miss buffer parses");
    assert!(
        !environ_bytes_config_dir_is(&args.environ, &tree_path),
        "env miss reads false"
    );

    // Truncated buffer: argc names two argv strings but the second never
    // terminates.
    let mut truncated = args2_buffer(2, &[b"/bin/claude", b"daemon"], &[]);
    truncated.pop(); // drop the final NUL
    assert_eq!(
        parse_proc_args2(&truncated),
        None,
        "a truncated argv string does not parse"
    );

    // argc past the end: argc names more strings than the buffer holds.
    let past_end = args2_buffer(6, &[b"/bin/claude", b"daemon"], &[]);
    assert_eq!(
        parse_proc_args2(&past_end),
        None,
        "an argc past the end does not parse"
    );
}

/// the cross-process spare dedupe's decision, factored off the
/// platform call — the same journal logic every OS runs, unit-tested on the
/// host.
#[test]
fn the_spared_announce_decision_dedupes_per_tree() {
    let (announce, rows) = spared_announce_decision(&[], "tree-a", 1000);
    assert!(announce, "an unseen tree announces");
    assert_eq!(
        rows,
        vec![SparedAnnounceRow {
            tree: "tree-a".into(),
            stamp_secs: 1000
        }]
    );

    let (again, rows2) = spared_announce_decision(&rows, "tree-a", 1005);
    assert!(!again, "the same tree is deduped");
    assert_eq!(rows2, rows, "a deduped decision adds no row");

    let (other, rows3) = spared_announce_decision(&rows, "tree-b", 1010);
    assert!(other, "a different tree still announces");
    assert_eq!(rows3.len(), 2, "the second tree joins the rows");
}

/// a SIGTERM that fails with `ESRCH` — the verified supervisor exited between
/// the verify and the kill — proceeds (the delete's re-check re-reads the lock
/// file before removing), with no wedged row and no "could not SIGTERM" line.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn delete_proceeds_when_the_supervisor_exits_before_the_kill() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let _claude_home = fake_claude_home(tmp.path());
        let mut config = crate::profile::AppConfig {
            state: crate::profile::AppState::default(),
            profiles: Vec::new(),
        };
        crate::actions::create_blank_profile(
            &mut config,
            "victim".to_string(),
            None,
            None,
            Default::default(),
        )
        .expect("create profile");
        let tree = tmp
            .path()
            .join(".clauth")
            .join("profiles")
            .join("victim")
            .join("runtime-4242-0");
        fs::create_dir_all(&tree).expect("mkdir tree");
        let mut sup = crate::testutil::FakeSupervisor::spawn(&tree, false);
        crate::testutil::write_daemon_lock(&tree, sup.pid, &sup.proc_start);
        let pid = sup.pid;

        let fired = std::rc::Rc::new(std::cell::Cell::new(0));
        {
            let fired = fired.clone();
            crate::runtime::set_signal_send_hook(Some(Box::new(move |hook_pid| {
                assert_eq!(hook_pid, pid, "the seam fires for the verified pid");
                fired.set(fired.get() + 1);
                let _ = sup.child.kill();
                let _ = sup.child.wait();
            })));
        }
        let lines = crate::logline::LogLines::new();
        let _capture = lines.capture_here();
        let name = crate::profile::ProfileName::from("victim");
        let guard = crate::actions::rotation_guard_for_mutation(&name).expect("rotation guard");
        crate::actions::delete_profile(&mut config, &name, false, &guard)
            .expect("the supervisor exiting before the kill lets the delete proceed");
        drop(guard);
        crate::runtime::set_signal_send_hook(None);

        assert_eq!(
            fired.get(),
            1,
            "the signal seam fired exactly once (the kill, not a misread Gone)"
        );
        assert!(!tree.exists(), "the delete removed the tree");
        assert!(
            !lines
                .snapshot()
                .iter()
                .any(|line| line.contains("could not SIGTERM")),
            "an ESRCH kill is the stopped outcome, never a could-not-SIGTERM spare"
        );
        let journal = load_stops_in_flight().expect("journal");
        assert!(
            !journal.wedged.iter().any(|row| row.pid == pid),
            "an ESRCH kill records no wedged row"
        );
    });
}

/// a supervisor recorded as wedged is spared without a second signal
/// — the stop skip reads the journal and never reaches `signal_and_wait`, so a
/// `clauth start` on a wedged tree pays no 15 s wait.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn a_wedged_supervisor_is_not_resignalled() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let _claude_home = fake_claude_home(tmp.path());
        let tree = tmp.path().join("tree");
        fs::create_dir_all(&tree).expect("mkdir tree");
        let mut sup = crate::testutil::FakeSupervisor::spawn(&tree, false);
        crate::testutil::write_daemon_lock(&tree, sup.pid, &sup.proc_start);
        record_wedged_stop(&tree, sup.pid, &sup.proc_start);

        assert_eq!(
            stop_tree_supervisor(&tree),
            SupervisorStop::Spare,
            "a wedged supervisor spares without another stop"
        );
        assert!(
            sup.child.try_wait().is_ok_and(|status| status.is_none()),
            "the wedged supervisor was not re-signalled"
        );
    });
}

/// the cross-process spare announcement's journal body — load -> sweep ->
/// push -> save in one flock hold — dedupes per tree across calls, the
/// off-linux announce latch, driven on linux through the same factored fn.
#[test]
fn the_journal_spare_announce_dedupes_per_tree() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let _claude_home = fake_claude_home(tmp.path());
        let tree_a = tmp.path().join("tree-a");
        fs::create_dir_all(&tree_a).expect("mkdir tree-a");
        let tree_b = tmp.path().join("tree-b");
        fs::create_dir_all(&tree_b).expect("mkdir tree-b");

        assert!(
            journal_spare_should_announce(&tree_a),
            "an unseen tree announces"
        );
        assert!(
            !journal_spare_should_announce(&tree_a),
            "the same tree is deduped across calls"
        );
        assert!(
            journal_spare_should_announce(&tree_b),
            "a different tree still announces"
        );
    });
}

/// A `~/.claude` that refuses new entries (read-only, `EACCES`, `ENOSPC`) must
/// not refuse a shared start: the store is skipped, as an unusable one is, and
/// the tree gets no link for it. The base started there — the walk only links
/// what `~/.claude` already holds — and this fix must not be stricter than the
/// base it corrects. Each store it cannot create is returned as a note the
/// start path prints, one line naming only that store's own consequence.
///
/// Unix only: [`crate::testutil::UnwritableDir`]'s Windows deny ACE still lets
/// a new SUBDIRECTORY through, so it cannot pose a refused `mkdir` here.
#[cfg(unix)]
#[test]
fn a_shared_build_skips_a_store_it_cannot_create() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        if !host_poses(tmp.path(), "a shared build's store-create failure") {
            return;
        }
        let claude_home = fake_claude_home(tmp.path());
        assert!(
            claude_home.join("jobs").symlink_metadata().is_err(),
            "the fixture starts without a jobs store"
        );
        assert!(
            claude_home.join("projects").symlink_metadata().is_err(),
            "the fixture starts without a projects store"
        );
        let runtime = tmp.path().join("runtime");
        fs::create_dir_all(&runtime).expect("mkdir runtime");
        let profile = make_profile("uncreatablestores");
        let canonical = tmp.path().join("creds.json");
        let unwritable = crate::testutil::UnwritableDir::new(&claude_home);
        // The refusal's text as this platform's std renders it, read off an
        // independent create under the same unwritable dir.
        let refused = fs::create_dir(claude_home.join("probe"))
            .expect_err("the unwritable dir refuses a new dir")
            .to_string();
        let lines = crate::logline::LogLines::new();
        let capture = lines.capture_here();

        let notes = build_runtime_dir(
            &runtime,
            &claude_home,
            &profile,
            &canonical,
            LinkMode::Real,
            Isolation::Shared,
        )
        .expect("a store that cannot be created skips, never fails the build");
        drop(capture);
        drop(unwritable);

        // The build raises no log line for a failed create: the notes travel
        // back to the acquire seam, and the start path prints them.
        assert!(
            lines.snapshot().is_empty(),
            "the build must raise no logline line, got: {:?}",
            lines.snapshot()
        );
        // cloudy 2026-10-22: one note per store the build could not create,
        // each naming only its own consequence.
        assert_eq!(
            notes,
            [
                (
                    "jobs",
                    "background jobs stay in its own tree, where `claude agents` outside it cannot see them",
                ),
                (
                    "projects",
                    "transcripts stay in its own tree and are lost when it ends",
                ),
            ]
            .map(|(store, consequence)| {
                format!(
                    "clauth: could not create {}: {refused}; this session's {consequence}",
                    claude_home.join(store).display()
                )
            }),
            "one note per store the build could not create"
        );

        assert!(
            claude_home.join("jobs").symlink_metadata().is_err(),
            "no jobs store was created"
        );
        assert!(
            claude_home.join("projects").symlink_metadata().is_err(),
            "no projects store was created"
        );
        assert!(
            runtime.join("jobs").symlink_metadata().is_err(),
            "the tree links no jobs store"
        );
        assert!(
            runtime.join("projects").symlink_metadata().is_err(),
            "the tree links no projects store"
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

        let rt = ProfileRuntime::acquire(&profile, Isolation::Shared, &[], false, Launcher::Own)
            .expect("acquire");

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

/// The wipe-HAPPENS half of the acquire-side stale-tree rule: a tree already
/// sitting at the path a session resolves, with NO live marker in its paired
/// sessions dir, is wiped before the build, so a dead session's leftovers do
/// not carry into this session's tree. The stale entry is a regular file with
/// no counterpart in `~/.claude` (empty here), so neither the additive build
/// walk nor `prune_dangling_links` can remove it — only the wipe can, which
/// is what makes this a wipe pin rather than a build pin. Forced Fake
/// transport because the shared bare-stem tree gives the fixture a FIXED
/// runtime path to pre-populate; the wipe itself is mode-independent.
#[test]
fn acquire_wipes_a_stale_tree_at_its_own_path_when_no_marker_holds_it() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        with_link_mode(LinkMode::Fake, || {
            fake_claude_home(tmp.path());
            let profile = configured_profile("stale");

            let runtime = tmp
                .path()
                .join(".clauth")
                .join("profiles")
                .join("stale")
                .join("runtime");
            fs::create_dir_all(&runtime).expect("mkdir stale runtime");
            fs::write(runtime.join("leftover-of-a-dead-session"), b"stale bytes")
                .expect("seed the stale entry");

            let rt =
                ProfileRuntime::acquire(&profile, Isolation::Shared, &[], false, Launcher::Own)
                    .expect("acquire");

            assert_eq!(
                rt.config_dir(),
                runtime,
                "under the forced shared fake transport the tree is the bare stem"
            );
            assert!(
                !runtime.join("leftover-of-a-dead-session").exists(),
                "a stale tree with no live marker holding it is wiped, not adopted"
            );
        });
    });
}

/// The window row 2 of the lock-race backlog names: a caller loads config, the
/// acquire's rotation-lock wait parks it, a delete lands, and the acquire then
/// rebuilds a whole session for an account nothing configures. The wait's own
/// deadline is beside the point here — the window is open for however long the
/// caller waits, bounded or not.
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
        let err = ProfileRuntime::acquire(&profile, Isolation::Shared, &[], false, Launcher::Own)
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
        // only by CONVENTION — `effective_base_url(None, false, None, &BTreeMap::new())` returning
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

        let err = ProfileRuntime::acquire_synced(
            &profile.name,
            Isolation::Shared,
            &[],
            false,
            Launcher::Own,
            || {
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
            },
            |_, _| unreachable!("the gate refuses before the stamp window ever closes"),
            || unreachable!("the gate refuses before the hold is ever released"),
        )
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

/// The stale-borrow fix, in its red-today shape: the caller's `&Profile` is
/// borrowed from a config loaded before `acquire`, and the rotation-guard wait
/// is exactly where a `base_url`/`api_key`/`env`/`models` edit can land. The
/// re-read must happen INSIDE the state-flock section — this seam fires before
/// the flock opens, so anything read before it still sees the pre-edit bytes —
/// and settings.json is one of the session-tree writes fed from it.
///
/// Same actor pose as the mixed-version removal test above, but a real one: an
/// edit persists through `save_profile` under the state flock alone and never
/// touches the rotation guard, so a second thread could pose this window — the
/// seam only makes it deterministic. A re-read hoisted above the seam would
/// see the pre-edit bytes and red; its placement below the flock section is
/// pinned by the `debug_assert!` beside it, which this test does not reach.
#[test]
fn acquire_builds_session_settings_from_the_profile_re_read_under_the_flock() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        fake_claude_home(tmp.path());
        // The caller's borrow, taken off the record before the wait.
        let profile = configured_profile("stale-start");

        let rt = ProfileRuntime::acquire_synced(
            &profile.name,
            Isolation::Shared,
            &[],
            false,
            Launcher::Own,
            || {
                // The edit lands on disk only, after the caller's borrow and
                // after the rotation guard: the borrowed copy stays stale.
                let mut edited = profile.clone();
                edited.base_url = Some("https://stale.example/anthropic".into());
                crate::profile::save_profile(&edited).expect("save edited profile");
            },
            |_, _| {},
            || {},
        )
        .expect("acquire");

        let settings: serde_json::Value =
            serde_json::from_slice(&fs::read(rt.config_dir().join("settings.json")).expect("read"))
                .expect("parse");
        assert_eq!(
            settings["env"]["ANTHROPIC_BASE_URL"],
            serde_json::json!("https://stale.example/anthropic"),
            "the session's settings.json must carry the base_url as it stood on \
             disk when the flock section ran, not the caller's pre-wait copy"
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

        let err = ProfileRuntime::acquire(&profile, Isolation::Shared, &[], false, Launcher::Own)
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

        let rt = ProfileRuntime::acquire(&profile, Isolation::Shared, &[], false, Launcher::Own)
            .expect("acquire");
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

        let rt = ProfileRuntime::acquire(&profile, Isolation::Shared, &[], false, Launcher::Own)
            .expect("acquire");
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

        let rt1 = ProfileRuntime::acquire(&profile, Isolation::Shared, &[], false, Launcher::Own)
            .expect("first acquire");
        // Pre-fix this second acquire blocks forever on the shared PID flock.
        let rt2 = ProfileRuntime::acquire(&profile, Isolation::Shared, &[], false, Launcher::Own)
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

        let a = ProfileRuntime::acquire(&profile, Isolation::Shared, &[], false, Launcher::Own)
            .expect("first acquire");
        let b = ProfileRuntime::acquire(&profile, Isolation::Shared, &[], false, Launcher::Own)
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

/// A session holds ONE liveness marker, its own `sessions[-isolated]-<sid>/<sid>`.
/// Nothing may land at the bare-stem `<profile>/sessions[-isolated]/<sid>` for a
/// session keyed per session: [`session_marker_dirs`] walks both dirs, so a
/// second marker there counts one session twice. Every per-session layout the
/// host can pose: the isolated flavor under [`LinkMode::Fake`] everywhere, and
/// both flavors under [`LinkMode::Real`] where the host has real symlinks. A
/// SHARED session under `Fake` keys on the bare stem itself and is pinned by
/// `a_shared_fake_session_keys_its_marker_on_the_bare_sessions_stem`.
#[test]
fn a_per_session_acquire_stamps_no_marker_at_the_bare_stem_path() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        fake_claude_home(tmp.path());
        let mut layouts = vec![(
            "one-marker-fake-iso",
            LinkMode::Fake,
            Isolation::Isolated,
            "sessions-isolated",
        )];
        if host_poses(tmp.path(), "a per-session layout under LinkMode::Real") {
            layouts.push((
                "one-marker-real-shared",
                LinkMode::Real,
                Isolation::Shared,
                "sessions",
            ));
            layouts.push((
                "one-marker-real-iso",
                LinkMode::Real,
                Isolation::Isolated,
                "sessions-isolated",
            ));
        }
        for (name, mode, isolation, bare_stem) in layouts {
            with_link_mode(mode, || {
                let profile = configured_profile(name);
                let rt = ProfileRuntime::acquire(&profile, isolation, &[], false, Launcher::Own)
                    .expect("acquire");
                let sid = live_sid(&rt);
                let profile_dir = tmp.path().join(".clauth").join("profiles").join(name);
                assert_eq!(
                    rt.sessions_dir(),
                    profile_dir.join(format!("{bare_stem}-{sid}")),
                    "{name}: the fixture must pose a per-session marker dir"
                );
                assert!(
                    is_session_alive(&rt.sessions_dir().join(&sid)),
                    "{name}: the session's own marker must be flock-held"
                );
                let bare = profile_dir.join(bare_stem);
                assert!(
                    !bare.join(&sid).exists(),
                    "{name}: a per-session session stamped a second marker at the \
                     pre-per-session path {}",
                    bare.join(&sid).display()
                );
                assert!(
                    !bare.exists(),
                    "{name}: a per-session session created the bare marker dir {}",
                    bare.display()
                );
                assert_eq!(
                    live_session_count(&crate::profile::ProfileName::from(name)),
                    1,
                    "{name}: one session, one marker"
                );
                drop(rt);
                assert_eq!(
                    live_session_count(&crate::profile::ProfileName::from(name)),
                    0,
                    "{name}: teardown must release the session's only marker"
                );
            });
        }
    });
}

/// A live foreign holder of this session's OWN marker must never park `acquire`.
/// Under [`LinkMode::Fake`] a SHARED session's marker sits in the bare-stem
/// `sessions/` every session of the profile shares, so a colliding sid lands on
/// a file another live process holds. The claim runs inside the state flock, so
/// a blocking wait there wedges every other clauth process on the home, not just
/// this one.
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

            // `acquire` mints its first id next, so seq+1 is the one it is about
            // to try.
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
                    ProfileRuntime::acquire(&profile, Isolation::Shared, &[], false, Launcher::Own)
                        .map(|rt| {
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

        let rt = ProfileRuntime::acquire(&profile, Isolation::Shared, &[], false, Launcher::Own)
            .expect("acquire");
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

        let rt = ProfileRuntime::acquire(&profile, Isolation::Shared, &[], false, Launcher::Own)
            .expect("acquire");
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

/// A wedge on the rotation lock ends the start with a NAMED failure instead of an
/// unbounded park. The deadline is shortened via
/// `set_rotation_lock_timeout_override` so the wedge poses without waiting out the
/// real one.
///
/// Driven on a worker with a deadline of its own, because what this defends
/// against is a wait that never ENDS: an acquire with its bound removed parks on
/// the wedge and HANGS the suite rather than failing it, and a hang is the one red
/// that never arrives. The main thread unwedges before rendering any verdict, so
/// the worker always terminates and can always be joined — which the
/// process-global home override requires anyway. The deadline override is
/// thread-local, so the worker sets its own rather than inheriting the main
/// thread's.
///
/// The typed error is the assertion, not the sentence: `run_delegate` renders it
/// through `{e}` and the TUI through the chain, so a caller that must tell
/// contention from an `~/.clauth` fault does it by `downcast_ref` and would keep
/// passing on a reworded string.
#[test]
fn a_start_behind_a_wedged_rotation_fails_with_the_bounded_wait() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        fake_claude_home(tmp.path());
        let profile = configured_profile("wedged-rot");
        let holder = hold_rotation_lock("wedged-rot");

        let (tx, rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            crate::runtime::set_rotation_lock_timeout_override(Some(Duration::from_millis(150)));
            let started = std::time::Instant::now();
            let outcome =
                ProfileRuntime::acquire(&profile, Isolation::Shared, &[], false, Launcher::Own)
                    .map(drop);
            let _ = tx.send((outcome, started.elapsed()));
        });
        // 20x the deadline: wide enough that a loaded box never reads as a park,
        // narrow enough that an unbounded acquire reports as one instead of hanging.
        let verdict = rx.recv_timeout(Duration::from_secs(3));
        drop(holder);
        let joined = worker.join();
        let (outcome, waited) = verdict
            .expect("the acquire was still parked 3s into a 150ms deadline: the wait has no bound");
        joined.expect("join the acquiring worker");

        let err = outcome.expect_err("a start behind a held rotation lock must give up, not park");
        assert!(
            err.chain().any(|c| c
                .downcast_ref::<crate::runtime::RotationLockTimeout>()
                .is_some()),
            "the wait must end in the typed timeout a caller can retry on, got: {err:#}"
        );
        let msg = format!("{err:#}");
        assert!(
            msg.contains("wedged-rot") && msg.contains("Retry"),
            "the refusal must name the account and the way out, got: {msg}"
        );
        assert!(
            waited >= Duration::from_millis(150),
            "the wait must last the whole deadline, not fail early: {waited:?}"
        );
        assert!(
            crate::live_sessions::list().is_empty(),
            "a start that never took the lock must register no live row"
        );
    });
}

/// The other half of the same verdict: a holder that releases INSIDE the deadline
/// is waited out, not refused. Without it the test above passes on an acquire that
/// gave up instantly, which is the failure mode a bound invites.
#[test]
fn a_start_behind_a_rotation_that_releases_in_time_proceeds() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        fake_claude_home(tmp.path());
        let profile = configured_profile("slow-rot");
        let holder = hold_rotation_lock("slow-rot");
        // Wide enough that the release lands well inside it on a loaded box.
        crate::runtime::set_rotation_lock_timeout_override(Some(Duration::from_secs(20)));

        let releaser = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(150));
            drop(holder);
        });
        let rt = ProfileRuntime::acquire(&profile, Isolation::Shared, &[], false, Launcher::Own)
            .expect("a rotation that releases inside the deadline must be waited out");
        releaser.join().expect("join releaser");

        assert_eq!(
            live_session_count(&crate::profile::ProfileName::from("slow-rot")),
            1,
            "the waited-out start must be a live session like any other"
        );
        drop(rt);
        crate::runtime::set_rotation_lock_timeout_override(None);
    });
}

/// The hold spans the register-and-stamp window and ends with it — held at the
/// window's head, free by the time the watcher arms.
///
/// Driven through both seams because neither end is observable from outside: the
/// tail gap is the watchdog arming, 18-34 ms on macOS, which an outside waiter
/// could only catch by racing. Inside the seams the questions are exact.
///
/// Three legs, each catching what the others cannot.
///
/// The STAMPED leg runs as the last statement inside the flock closure and is the
/// only one that can answer the question the hold exists for: are the marker's
/// flock and the registry row on disk while the lock is still held. Asked after
/// the drop instead — as an earlier version of this test asked it — it cannot tell
/// "stamped before the lock went" from "stamped one statement after", so hoisting
/// either artifact out of the closure satisfied it. Measured, twice.
///
/// The RELEASED leg runs after the drop and answers the only question the stamped
/// leg cannot: that the lock is free by then. It reds on a hold restored to the end
/// of `acquire_synced` wherever in the tail it sits, so its position is not
/// load-bearing.
///
/// The HELD leg fires BEFORE the flock closure and its reach is narrower than it
/// looks: it catches only a hold that ends before the acquire enters that closure.
/// A drop moved between it and the closure passes here and is caught by
/// `refuse_if_unconfigured`'s rank `debug_assert` — which lives on the DEBUG leg
/// alone, since the rank stack is `cfg(debug_assertions)`-only. That leg is gated
/// (CI runs it in its `debug clippy + test` step), so the floor is covered; it
/// is not covered by anything a release run can see, and this test is not what
/// covers it.
///
/// All three count themselves, because a probe that lives inside an injected
/// closure asserts nothing at all if the closure stops being called — and a
/// dropped call site is exactly the shape an edit here produces. Only the count
/// separates "every end checked out" from "no end was looked at".
#[test]
fn the_rotation_hold_ends_at_the_register_and_stamp_window() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        fake_claude_home(tmp.path());
        let profile = configured_profile("shrunk");
        let name = crate::profile::ProfileName::from("shrunk");
        let head = crate::profile::ProfileName::from("shrunk");
        let fired = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let head_fired = std::sync::Arc::clone(&fired);
        let stamped_fired = std::sync::Arc::clone(&fired);
        let released_fired = std::sync::Arc::clone(&fired);

        let rt = ProfileRuntime::acquire_synced(
            &profile.name,
            Isolation::Shared,
            &[],
            false,
            Launcher::Own,
            || {
                head_fired.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                // Same process, second fd: `flock(2)` binds to the open file
                // description, so a `None` here is this acquire's own hold.
                assert!(
                    crate::runtime::RotationGuard::try_acquire(&head)
                        .expect("probe the rotation lock")
                        .is_none(),
                    "the rotation lock must be held before the flock closure opens, \
                     or the stamp window serializes against nothing"
                );
            },
            |paths, session| {
                stamped_fired.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                // The fourth fact, and the one no other leg can observe: the lock
                // is STILL HELD here. Without this, moving this whole seam past the
                // drop — the natural shape of an extract-function edit, which moves
                // the call with the block it is named for — passes every assertion
                // below while the artifacts land after the hold. Measured, it did.
                assert!(
                    crate::runtime::RotationGuard::try_acquire(&head)
                        .expect("probe the rotation lock")
                        .is_none(),
                    "the rotation lock must still be held while the stamp window closes"
                );
                // THIS session's own marker, never the profile-wide predicate:
                // a sibling session's marker satisfies `has_live_session` alone,
                // so a probe written against that passes with the session's own
                // marker unclaimed.
                assert!(
                    is_session_alive(&paths.pid_file),
                    "the session's liveness marker must be flock-held before the hold ends"
                );
                // ...and `live_session_holds_rotatable` reads the row's launch
                // store. Keyed on the SESSION ID for the same reason the marker
                // above is this session's own: a profile-wide scan is satisfied by
                // a sibling session's row.
                assert!(
                    crate::live_sessions::get(session.as_str())
                        .is_some_and(|r| r.launch_store.is_some()),
                    "the registry row must carry its launch store before the hold ends"
                );
            },
            || {
                released_fired.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                assert!(
                    crate::runtime::RotationGuard::try_acquire(&name)
                        .expect("probe the rotation lock")
                        .is_some(),
                    "the rotation lock must be free once the stamp window closes, \
                     so a queued peer waits out that window and nothing after it"
                );
            },
        )
        .expect("acquire");
        assert_eq!(
            fired.load(std::sync::atomic::Ordering::SeqCst),
            3,
            "every seam must have run, or the assertions above pinned nothing"
        );
        drop(rt);
    });
}

/// The FLOOR under the shortened hold has exactly one guard —
/// `refuse_if_unconfigured`'s rotation-rank `debug_assert` — and deleting it is
/// silent: the suite stays green, and a later hold-shortening then passes both
/// legs. So the guard's PRESENCE is pinned here, the way
/// `run_delegate_reads_the_cancel_flag_between_the_acquire_and_the_spawn` pins
/// its own.
///
/// It observes source text and nothing else, which is why it pins the assertion
/// WHOLE: the `cfg!(test) ||` escape its two neighbours in that file carry is the
/// edit its own doc discusses, and that escape would leave every literal a
/// `contains` of the rank or the message could key on standing.
#[test]
fn the_record_re_read_still_asserts_the_rotation_lock_is_held() {
    let src = include_str!("../../src/runtime.rs");
    let body = src
        .split_once("fn refuse_if_unconfigured(")
        .expect("refuse_if_unconfigured is defined")
        .1;
    let gate = body
        .find("is_configured")
        .expect("the gate reads the record");
    // Bound to a `let` so `cargo fmt` cannot reflow the literal's continuation
    // whitespace into it, which is how the first spelling of this pin failed.
    let whole = "\n    debug_assert!(\n        crate::lockorder::holds::<crate::lockorder::rank::Rotation>(),\n";
    assert!(
        body[..gate].contains(whole),
        "the rotation-rank assertion must stand ahead of the record read, \
         unqualified: {}",
        &body[..gate]
    );
}

/// A timed-out wait leaves no ROTATION rank on the thread, and no flock either. A
/// rank entered before the lock was actually taken would survive the failure and
/// make the NEXT acquisition on this thread panic as a lock-order violation — the
/// ordering assertion firing on an inversion that never happened.
///
/// The re-acquire is BOUNDED, not blocking, and that is the whole point of its
/// shape: it also pins that the timed-out wait released the flock the helper
/// thread went on to win. A helper that kept it would hang a blocking re-acquire
/// forever, and a hang is the one red that never arrives.
#[test]
fn a_timed_out_rotation_wait_leaves_no_rank_behind() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let name = crate::profile::ProfileName::from("rank-leak");
        let holder = hold_rotation_lock("rank-leak");
        crate::runtime::set_rotation_lock_timeout_override(Some(Duration::from_millis(50)));

        crate::runtime::RotationGuard::acquire_with_timeout(
            &name,
            crate::runtime::rotation_lock_timeout(),
        )
        .map(|_| ())
        .expect_err("the wedge must time the wait out");
        drop(holder);

        // Panics on a leaked rank rather than returning an error, so the
        // acquisition itself is the rank assertion; its deadline is what makes a
        // retained flock red instead of hanging.
        let guard =
            crate::runtime::RotationGuard::acquire_with_timeout(&name, Duration::from_secs(5))
                .expect("the released lock must be takeable after a timed-out wait");
        drop(guard);
        crate::runtime::set_rotation_lock_timeout_override(None);
    });
}

/// The deadline, pinned as a LITERAL rather than re-derived from the constants it
/// adds: an assertion keyed on those tracks any re-tune silently, and this number
/// is a claim about how long a holder spends. The floor's four terms come to
/// 69 s — 19 s of the two deadlines a token call carries, plus 20 s for a macOS
/// Keychain mirror's two `security` invocations, plus 20 s for the
/// session-start Keychain seed's shared budget, plus 10 s for the `.claude.json`
/// seed's healthy sync-lock wait. The scheduler's fetch-leg rotation outlasts
/// that: two 12 s identity probes, a 20 s pre-spend stage landing, the 19 s token
/// call and the 20 s persist landing come to 83 s, and the deadline is the longer
/// of the two. Moving any term must red this and force the claim to be re-made
/// against what that term now bounds.
#[test]
fn the_rotation_deadline_outlasts_a_healthy_holders_two_slow_legs() {
    assert_eq!(
        crate::runtime::ROTATION_LOCK_TIMEOUT,
        Duration::from_secs(83),
        "the session-start wait must outlast the longest rotation-guard holder: the \
         fetch leg's two identity probes, its pre-spend stage landing, its token \
         call and its persist's landing"
    );
}

/// The CEILING, as a relation between the wait and the host's own silence
/// tolerance rather than a second literal: the pre-spawn wait is silent on the
/// wire, so it has to end before Claude Code's 30-minute stdio idle abort
/// gives up on the call. A deadline past that turns clauth's named refusal
/// into the client's opaque abort, which is the outcome the bound exists to
/// remove.
#[test]
fn the_rotation_deadline_ends_before_the_host_idles_out_a_silent_call() {
    assert!(
        crate::runtime::ROTATION_LOCK_TIMEOUT < Duration::from_secs(1800),
        "the rotation wait must end inside the silence budget of the host's \
         30-minute stdio idle abort: {:?} against 1800s",
        crate::runtime::ROTATION_LOCK_TIMEOUT,
    );
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

        let a = ProfileRuntime::acquire(&profile, Isolation::Shared, &[], false, Launcher::Own)
            .expect("first acquire");
        let b = ProfileRuntime::acquire(&profile, Isolation::Shared, &[], false, Launcher::Own)
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
/// own `<sid>`; `LinkMode::Fake` keys an isolated session the same way and keeps
/// the bare stem only for a SHARED session. In all four cases the two names must
/// satisfy the module's one layout rule (`runtime<rest>` ↔ `sessions<rest>`) and
/// both strict predicates, so no enumeration can miss a dir the naming produced.
#[test]
fn paired_dir_names_key_on_link_mode() {
    let sid = "4242-7";
    let cases = [
        (Isolation::Shared, LinkMode::Fake, "runtime", "sessions"),
        (
            Isolation::Isolated,
            LinkMode::Fake,
            "runtime-isolated-4242-7",
            "sessions-isolated-4242-7",
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

            let a = ProfileRuntime::acquire(&profile, Isolation::Shared, &[], false, Launcher::Own)
                .expect("first acquire");
            let b = ProfileRuntime::acquire(&profile, Isolation::Shared, &[], false, Launcher::Own)
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

/// Under `LinkMode::Fake` two isolated sessions of one profile still get their
/// own per-session trees, keyed by sid. The shared bare stem is a SHARED-only
/// fallback.
#[test]
fn fake_mode_isolated_sessions_get_independent_trees() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        with_link_mode(LinkMode::Fake, || {
            fake_claude_home(tmp.path());
            let profile = configured_profile("iso-twin");

            let a =
                ProfileRuntime::acquire(&profile, Isolation::Isolated, &[], false, Launcher::Own)
                    .expect("first acquire");
            let b =
                ProfileRuntime::acquire(&profile, Isolation::Isolated, &[], false, Launcher::Own)
                    .expect("second acquire");

            assert_ne!(
                a.config_dir(),
                b.config_dir(),
                "two isolated sessions of one profile must not share a runtime tree"
            );
            assert_ne!(
                a.sessions_dir(),
                b.sessions_dir(),
                "two isolated sessions of one profile must not share a marker dir"
            );

            drop(b);
            drop(a);
        });
    });
}

/// The separation the per-session keying buys: a teardown of one isolated
/// session lifts only that session's state. The sibling's transcript and
/// sidecar files stay where the sibling reads them.
#[test]
fn fake_mode_isolated_teardown_rescues_only_its_own_tree() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        with_link_mode(LinkMode::Fake, || {
            let claude_home = fake_claude_home(tmp.path());
            let profile = configured_profile("iso-twin");

            let a =
                ProfileRuntime::acquire(&profile, Isolation::Isolated, &[], false, Launcher::Own)
                    .expect("first acquire");
            let b =
                ProfileRuntime::acquire(&profile, Isolation::Isolated, &[], false, Launcher::Own)
                    .expect("second acquire");

            let a_projects = a.config_dir().join("projects");
            let b_projects = b.config_dir().join("projects");
            fs::create_dir_all(a_projects.join("-w-iso")).unwrap();
            fs::create_dir_all(b_projects.join("-w-iso")).unwrap();
            fs::write(a_projects.join("-w-iso/a1.jsonl"), "a transcript").unwrap();
            fs::write(b_projects.join("-w-iso/b1.jsonl"), "b transcript").unwrap();
            let a_snap = a.config_dir().join("shell-snapshots");
            let b_snap = b.config_dir().join("shell-snapshots");
            fs::create_dir_all(&a_snap).unwrap();
            fs::create_dir_all(&b_snap).unwrap();
            fs::write(a_snap.join("a.sh"), "a shell").unwrap();
            fs::write(b_snap.join("b.sh"), "b shell").unwrap();

            let (moved, sidecars) =
                crate::start::rescue_teardown(a.config_dir(), a.sessions_dir(), &claude_home);

            assert_eq!((moved, sidecars), (1, 1), "a's own rescue moves a's state");
            assert!(
                b_projects.join("-w-iso/b1.jsonl").is_file(),
                "the sibling's transcript stays put"
            );
            assert_eq!(
                fs::read_to_string(b_projects.join("-w-iso/b1.jsonl")).unwrap(),
                "b transcript",
                "the sibling's transcript content is untouched"
            );
            assert_eq!(
                fs::read_to_string(b_snap.join("b.sh")).unwrap(),
                "b shell",
                "the sibling's sidecar stays put"
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

            let a = ProfileRuntime::acquire(&profile, Isolation::Shared, &[], false, Launcher::Own)
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

            let b = ProfileRuntime::acquire(&profile, Isolation::Shared, &[], false, Launcher::Own)
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

            let a = ProfileRuntime::acquire(&profile, Isolation::Shared, &[], false, Launcher::Own)
                .expect("first acquire");
            let b = ProfileRuntime::acquire(&profile, Isolation::Shared, &[], false, Launcher::Own)
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
                ProfileRuntime::acquire(&profile, Isolation::Shared, &[], false, Launcher::Own)
                    .expect("acquire");
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

/// Under `LinkMode::Fake` a SHARED session keys its one marker on the bare
/// `sessions` stem, the dir every shared session of the profile uses.
#[test]
fn a_shared_fake_session_keys_its_marker_on_the_bare_sessions_stem() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        with_link_mode(LinkMode::Fake, || {
            fake_claude_home(tmp.path());

            let shared = configured_profile("fakebare-shared");
            let rt = ProfileRuntime::acquire(&shared, Isolation::Shared, &[], false, Launcher::Own)
                .expect("acquire");
            let bare = tmp
                .path()
                .join(".clauth")
                .join("profiles")
                .join("fakebare-shared")
                .join("sessions");
            assert_eq!(
                rt.sessions_dir(),
                bare,
                "the shared fake marker dir must be the bare stem"
            );
            assert_eq!(live_sessions_at(&bare), Some(1));
            assert_eq!(
                live_session_count(&crate::profile::ProfileName::from("fakebare-shared")),
                1
            );
            drop(rt);
            assert!(!bare.exists(), "the last shared session out removes it");
            assert_eq!(
                live_session_count(&crate::profile::ProfileName::from("fakebare-shared")),
                0
            );
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
            // The BARE codex store stem: outside the runtime*/sessions* stems
            // BY DESIGN, and not a per-session home either, so no GC touches
            // it. (A per-session codex-home-<sid> IS collected by
            // gc_codex_homes — see gc_collects_a_dead_codex_home_*.)
            "codex-home",
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

/// A dead per-session pair (marker present, unlocked), seeded like the pairs
/// `gc_collects_a_dead_per_session_pair_and_spares_a_held_one` builds.
fn dead_pair(profiles: &Path, profile: &str, runtime_name: &str, sid: &str) -> (PathBuf, PathBuf) {
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
    (runtime, sessions)
}

/// A `claude --bg` session keeps reading its `CLAUDE_CONFIG_DIR` long after the
/// `clauth start` that built the tree has exited and released its marker, so
/// the marker alone cannot say the tree is unused: GC spares a dead pair while
/// a Claude Code job record names its tree, and collects it once the job dir
/// is gone (`claude rm`). A job naming some other tree spares nothing.
#[test]
fn gc_spares_a_dead_pair_a_claude_job_names_until_the_job_is_removed() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let profiles = tmp.path().join(".clauth").join("profiles");
        let (named, named_sessions) = dead_pair(&profiles, "bgjob", "runtime-4242-0", "4242-0");
        let (other, other_sessions) = dead_pair(&profiles, "bgjob", "runtime-4242-1", "4242-1");
        let job = write_claude_job(tmp.path(), "37560ded", &named);
        write_claude_job(tmp.path(), "9f8e7d6c", &tmp.path().join("elsewhere"));

        gc_stale_runtimes();

        assert!(
            named.join(".claude.json").is_file(),
            "a tree a live background job names must survive the sweep"
        );
        assert!(
            named_sessions.is_dir(),
            "its sessions dir goes nowhere either"
        );
        assert!(
            !other.exists() && !other_sessions.exists(),
            "a dead pair no job names is still collected"
        );

        fs::remove_dir_all(&job).expect("claude rm");
        gc_stale_runtimes();

        assert!(
            !named.exists() && !named_sessions.exists(),
            "once the job dir is gone the dead pair is collected"
        );
    });
}

/// A job record caught mid-rewrite (or otherwise unreadable) names nothing the
/// sweep can trust, and the sweep's failure mode is the one `is_session_alive`
/// already takes: spare. Collecting on a torn read would delete the tree a live
/// background session reads.
#[test]
fn gc_spares_every_dead_pair_while_a_job_record_cannot_be_read() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let profiles = tmp.path().join(".clauth").join("profiles");
        let (runtime, sessions) = dead_pair(&profiles, "bgtorn", "runtime-4242-2", "4242-2");
        let job = tmp.path().join(".claude").join("jobs").join("0add99ec");
        fs::create_dir_all(&job).expect("mkdir job dir");
        fs::write(
            job.join("state.json"),
            br#"{"state":"working","providerEnv":{"CLAUDE_CON"#,
        )
        .expect("torn job record");

        gc_stale_runtimes();

        assert!(
            runtime.join(".claude.json").is_file() && sessions.is_dir(),
            "an unreadable job record must spare the dead pair this sweep"
        );

        fs::remove_dir_all(&job).expect("claude rm");
        gc_stale_runtimes();

        assert!(
            !runtime.exists() && !sessions.exists(),
            "with the torn record gone the dead pair is collected"
        );
    });
}

/// `clauth start <profile> --bg` returns as soon as `claude --bg` hands the
/// session to Claude Code's background service, so the start's own teardown
/// runs while the session still needs its tree: teardown leaves a tree a job
/// names in place, and still removes one no job names.
#[test]
fn teardown_leaves_a_tree_a_claude_job_names() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        fs::create_dir_all(tmp.path().join(".claude")).expect("mkdir ~/.claude");
        let mut trees = Vec::new();
        for name in ["bgkeep", "bgdrop"] {
            let profile = configured_profile(name);
            let canonical = tmp
                .path()
                .join(".clauth")
                .join("profiles")
                .join(name)
                .join("credentials.json");
            fs::create_dir_all(canonical.parent().expect("canonical parent"))
                .expect("mkdir profile dir");
            fs::write(&canonical, CREDS_V2).expect("write canonical");
            let rt =
                ProfileRuntime::acquire(&profile, Isolation::Shared, &[], false, Launcher::Own)
                    .expect("acquire");
            let tree = rt.config_dir().to_path_buf();
            trees.push((tree, rt));
        }
        let keep = trees[0].0.clone();
        let gone = trees[1].0.clone();
        write_claude_job(tmp.path(), "37560ded", &keep);

        drop(trees);

        assert!(
            keep.is_dir(),
            "teardown must leave {} in place: a background job still reads it",
            keep.display()
        );
        assert!(
            !gone.exists(),
            "teardown still removes {}: no job names it",
            gone.display()
        );
    });
}

/// Writes the canonical credentials `acquire` links for `name`.
fn seed_profile_store(home: &Path, name: &str) {
    let canonical = home
        .join(".clauth")
        .join("profiles")
        .join(name)
        .join("credentials.json");
    fs::create_dir_all(canonical.parent().expect("canonical parent")).expect("mkdir profile dir");
    fs::write(&canonical, CREDS_V2).expect("write canonical");
}

/// Writes a `{}` per-spawn `--settings` file for a live session and returns it.
fn write_spawn_settings(rt: &ProfileRuntime) -> PathBuf {
    let settings = spawn_settings_file(rt.config_dir(), rt.session_id());
    fs::write(&settings, b"{}").expect("write spawn settings");
    settings
}

/// A job's `respawnFlags` carry `--settings <spawn-settings-<sid>.json>`
/// (measured 2026-10-20, 2.1.304): that list, not the tree the job runs in, is
/// what says which settings file a respawn reads. Teardown keeps the file a job
/// names and removes a sibling session's file even while a job names that
/// sibling's tree, both sessions in one profile.
#[test]
fn teardown_keeps_only_the_spawn_settings_file_a_jobs_respawn_flags_name() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        fs::create_dir_all(tmp.path().join(".claude")).expect("mkdir ~/.claude");
        seed_profile_store(tmp.path(), "bgss");
        let profile = configured_profile("bgss");
        let named = ProfileRuntime::acquire(&profile, Isolation::Shared, &[], false, Launcher::Own)
            .expect("acquire");
        let sibling =
            ProfileRuntime::acquire(&profile, Isolation::Shared, &[], false, Launcher::Own)
                .expect("acquire");
        let named_settings = write_spawn_settings(&named);
        let sibling_settings = write_spawn_settings(&sibling);
        let sibling_tree = sibling.config_dir().to_path_buf();
        write_claude_job_flags(
            tmp.path(),
            "4b1d2c3e",
            named.config_dir(),
            respawn_flags_naming(&named_settings),
        );
        write_claude_job_flags(tmp.path(), "4b1d2c3f", &sibling_tree, serde_json::json!([]));

        drop((named, sibling));

        assert!(
            named_settings.is_file(),
            "teardown must keep {}: a job's respawnFlags name it",
            named_settings.display()
        );
        assert!(
            !sibling_settings.exists(),
            "teardown still removes {}: no job's respawnFlags name it",
            sibling_settings.display()
        );
        assert!(
            sibling_tree.is_dir(),
            "the sibling's tree itself stays: a job names it"
        );
    });
}

/// The sweep's half: a dead session's file survives exactly while a job's
/// `respawnFlags` name it, shared or isolated, and is collected once the job dir
/// is gone; a dead file no job names goes at once, even while some job names a
/// tree it could have shared (the profile's bare fake-mode `runtime`). The file
/// can hold a folded user `env` token, so nothing keeps it longer.
#[test]
fn gc_keeps_exactly_the_spawn_settings_files_a_jobs_respawn_flags_name() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let profiles = tmp.path().join(".clauth").join("profiles");
        let (shared, _) = dead_pair(&profiles, "ssbg", "runtime-4545-0", "4545-0");
        let shared_file = spawn_settings_file(&shared, "4545-0");
        fs::write(&shared_file, b"{}").expect("shared settings file");
        let (isolated, _) = dead_pair(&profiles, "ssbg", "runtime-isolated-4545-1", "4545-1");
        let isolated_file = spawn_settings_file(&isolated, "4545-1");
        fs::write(&isolated_file, b"{}").expect("isolated settings file");
        let (unnamed, _) = dead_pair(&profiles, "ssbg", "runtime-4545-2", "4545-2");
        let unnamed_file = spawn_settings_file(&unnamed, "4545-2");
        fs::write(&unnamed_file, b"{}").expect("unnamed settings file");
        let (beside_bare, _) = dead_pair(&profiles, "ssbg", "runtime-4545-3", "4545-3");
        let beside_bare_file = spawn_settings_file(&beside_bare, "4545-3");
        fs::write(&beside_bare_file, b"{}").expect("settings file beside a bare-runtime job");
        let bare = profiles.join("ssbg").join("runtime");
        fs::create_dir_all(&bare).expect("mkdir bare runtime");
        let shared_job = write_claude_job_flags(
            tmp.path(),
            "5c2e3d4f",
            &shared,
            respawn_flags_naming(&shared_file),
        );
        let isolated_job = write_claude_job_flags(
            tmp.path(),
            "6d3f4e5a",
            &isolated,
            respawn_flags_naming(&isolated_file),
        );
        write_claude_job(tmp.path(), "7e4a5f6b", &bare);

        gc_stale_runtimes();

        for (file, flavor) in [(&shared_file, "shared"), (&isolated_file, "isolated")] {
            assert!(
                file.is_file(),
                "a dead {flavor} session's {} must survive while a job's respawnFlags name it",
                file.display()
            );
        }
        for (file, why) in [
            (&unnamed_file, "no job names it"),
            (
                &beside_bare_file,
                "a job naming the bare `runtime` tree names no settings file",
            ),
        ] {
            assert!(
                !file.exists(),
                "{} must be collected: {why}",
                file.display()
            );
        }

        fs::remove_dir_all(&shared_job).expect("claude rm");
        fs::remove_dir_all(&isolated_job).expect("claude rm");
        gc_stale_runtimes();

        for (file, flavor) in [(&shared_file, "shared"), (&isolated_file, "isolated")] {
            assert!(
                !file.exists(),
                "once the job dir is gone the dead {flavor} session's {} is collected",
                file.display()
            );
        }
    });
}

/// Fake-mode shared sessions of one profile all run in the bare `runtime`
/// tree, so a job naming that tree says nothing about which session's file it
/// respawns with: only the file its `respawnFlags` name survives.
#[test]
fn gc_keeps_only_the_named_fake_mode_sessions_spawn_settings_file() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let profile_dir = tmp.path().join(".clauth").join("profiles").join("ssfk");
        let runtime = profile_dir.join("runtime");
        let sessions = profile_dir.join("sessions");
        fs::create_dir_all(&runtime).expect("mkdir runtime");
        fs::create_dir_all(&sessions).expect("mkdir sessions");
        fs::write(sessions.join("4646-0"), b"").expect("dead marker 0");
        fs::write(sessions.join("4646-1"), b"").expect("dead marker 1");
        let named = spawn_settings_file(&runtime, "4646-0");
        let other = spawn_settings_file(&runtime, "4646-1");
        fs::write(&named, b"{}").expect("named settings file");
        fs::write(&other, b"{}").expect("other settings file");
        write_claude_job_flags(
            tmp.path(),
            "8f5b6a7c",
            &runtime,
            respawn_flags_naming(&named),
        );

        gc_stale_runtimes();

        assert!(
            named.is_file(),
            "{} must survive: the job's respawnFlags name it",
            named.display()
        );
        assert!(
            !other.exists(),
            "{} must be collected: the job shares its tree but names another file",
            other.display()
        );
    });
}

/// A job record caught mid-rewrite names nothing either side can trust, so both
/// removers take the store's fail-closed answer: keep every settings file.
#[test]
fn teardown_and_gc_keep_spawn_settings_files_while_the_job_store_cannot_be_read() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let torn = tmp.path().join(".claude").join("jobs").join("0add99ed");
        fs::create_dir_all(&torn).expect("mkdir job dir");
        fs::write(
            torn.join("state.json"),
            br#"{"state":"working","respawnFlags":["--settings","#,
        )
        .expect("torn job record");
        seed_profile_store(tmp.path(), "bgtorn");
        let rt = ProfileRuntime::acquire(
            &configured_profile("bgtorn"),
            Isolation::Shared,
            &[],
            false,
            Launcher::Own,
        )
        .expect("acquire");
        let live_settings = write_spawn_settings(&rt);
        let profiles = tmp.path().join(".clauth").join("profiles");
        let (dead, _) = dead_pair(&profiles, "bgtorn", "runtime-4747-0", "4747-0");
        let dead_settings = spawn_settings_file(&dead, "4747-0");
        fs::write(&dead_settings, b"{}").expect("dead settings file");

        drop(rt);
        gc_stale_runtimes();

        for (file, remover) in [(&live_settings, "teardown"), (&dead_settings, "GC")] {
            assert!(
                file.is_file(),
                "{remover} must keep {} while the job store cannot be read",
                file.display()
            );
        }
    });
}

/// A record whose `respawnFlags` is present but not a list of strings (a
/// retyped field after a Claude Code update: a string, `null`, a non-string
/// element anywhere, a `--settings` with a non-string value) cannot say which
/// file it names, so the files beside its tree fall back to T33a's tree rule; a
/// file beside a tree no job names still goes.
#[test]
fn gc_keeps_the_settings_files_beside_a_tree_whose_jobs_respawn_flags_cannot_be_read() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let profiles = tmp.path().join(".clauth").join("profiles");
        let shapes = [
            ("a string", serde_json::json!("--settings somewhere")),
            ("null", serde_json::Value::Null),
            ("a number", serde_json::json!(7)),
            ("a negative number", serde_json::json!(-7)),
            ("a float", serde_json::json!(7.5)),
            ("a bool", serde_json::json!(true)),
            (
                "a non-string --settings value",
                serde_json::json!(["--settings", 7]),
            ),
            (
                "a non-string element before a --settings",
                serde_json::json!([1, "--settings", "/nowhere/spawn-settings-1-0.json"]),
            ),
            (
                "a map",
                serde_json::json!({"--settings": "/nowhere/spawn-settings-1-1.json"}),
            ),
            (
                "a map element before a --settings",
                serde_json::json!([{"k": [1, {"j": null}]}, "--settings", "/nowhere/spawn-settings-1-2.json"]),
            ),
        ];
        let mut kept = Vec::new();
        for (n, (shape, flags)) in shapes.into_iter().enumerate() {
            let sid = format!("4848-{n}");
            let (tree, _) = dead_pair(&profiles, "ssrt", &format!("runtime-{sid}"), &sid);
            let beside = spawn_settings_file(&tree, &sid);
            fs::write(&beside, b"{}").expect("settings file beside the job's tree");
            write_claude_job_flags(tmp.path(), &format!("9a6c7b8{n}"), &tree, flags);
            kept.push((beside, shape));
        }
        let (other, _) = dead_pair(&profiles, "ssrt", "runtime-4849-0", "4849-0");
        let elsewhere = spawn_settings_file(&other, "4849-0");
        fs::write(&elsewhere, b"{}").expect("settings file beside an unnamed tree");

        gc_stale_runtimes();

        for (beside, shape) in &kept {
            assert!(
                beside.is_file(),
                "{} must survive: its tree's job has respawnFlags clauth cannot read ({shape})",
                beside.display()
            );
        }
        assert!(
            !elsewhere.exists(),
            "{} must be collected: no job names its tree",
            elsewhere.display()
        );
    });
}

/// Teardown takes the same fallback: a job naming this session's own tree with
/// unreadable `respawnFlags` keeps the session's file.
#[test]
fn teardown_keeps_the_spawn_settings_file_beside_a_tree_whose_jobs_respawn_flags_cannot_be_read() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        fs::create_dir_all(tmp.path().join(".claude")).expect("mkdir ~/.claude");
        seed_profile_store(tmp.path(), "bgunk");
        let rt = ProfileRuntime::acquire(
            &configured_profile("bgunk"),
            Isolation::Shared,
            &[],
            false,
            Launcher::Own,
        )
        .expect("acquire");
        let settings = write_spawn_settings(&rt);
        write_claude_job_flags(
            tmp.path(),
            "9a6c7b90",
            rt.config_dir(),
            serde_json::json!("--settings somewhere"),
        );

        drop(rt);

        assert!(
            settings.is_file(),
            "teardown must keep {}: its tree's job has respawnFlags clauth cannot read",
            settings.display()
        );
    });
}

/// `--settings` also takes inline JSON, and clauth hands a user's own value
/// through unfolded, so a job's flags can carry a value that is no path. Inline
/// JSON names no file; a value that cannot resolve as a path (a NUL) falls back
/// to its own job's tree. Neither makes the whole job store unreadable: a dead
/// tree in an unrelated profile is still collected.
#[test]
fn gc_scopes_a_settings_value_that_is_no_path_to_its_own_job() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let profiles = tmp.path().join(".clauth").join("profiles");
        let (inline_tree, _) = dead_pair(&profiles, "ssval", "runtime-4949-0", "4949-0");
        let beside_inline = spawn_settings_file(&inline_tree, "4949-0");
        fs::write(&beside_inline, b"{}").expect("settings file beside the inline job's tree");
        let long_inline = format!(
            r#"{{"permissions":{{"allow":["Bash({})"]}}}}"#,
            "a".repeat(512)
        );
        write_claude_job_flags(
            tmp.path(),
            "aa1b2c3d",
            &inline_tree,
            serde_json::json!(["--settings", long_inline]),
        );
        let (nul_tree, _) = dead_pair(&profiles, "ssval", "runtime-4949-1", "4949-1");
        let beside_nul = spawn_settings_file(&nul_tree, "4949-1");
        fs::write(&beside_nul, b"{}").expect("settings file beside the NUL job's tree");
        write_claude_job_flags(
            tmp.path(),
            "aa1b2c3e",
            &nul_tree,
            serde_json::json!(["--settings", "bad\u{0}path"]),
        );
        let (unrelated, unrelated_sessions) =
            dead_pair(&profiles, "ssother", "runtime-5050-0", "5050-0");

        gc_stale_runtimes();

        assert!(
            !unrelated.exists() && !unrelated_sessions.exists(),
            "a dead tree in an unrelated profile must still be collected: no job value makes the store unreadable"
        );
        assert!(
            !beside_inline.exists(),
            "{} must be collected: an inline JSON value names no file",
            beside_inline.display()
        );
        assert!(
            inline_tree.join(".claude.json").is_file(),
            "the inline job's own tree stays: the job names it"
        );
        assert!(
            beside_nul.is_file(),
            "{} must survive: its job's value cannot resolve, so its tree falls back",
            beside_nul.display()
        );
    });
}

/// Claude Code's `--settings` also parses the `--settings=<value>` spelling, so a
/// record carrying it names that file too.
#[test]
fn gc_keeps_a_spawn_settings_file_named_in_the_equals_spelling() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let profiles = tmp.path().join(".clauth").join("profiles");
        let (tree, _) = dead_pair(&profiles, "sseq", "runtime-5151-0", "5151-0");
        let file = spawn_settings_file(&tree, "5151-0");
        fs::write(&file, b"{}").expect("settings file");
        write_claude_job_flags(
            tmp.path(),
            "bb2c3d4e",
            &tree,
            serde_json::json!([format!("--settings={}", file.to_string_lossy())]),
        );

        gc_stale_runtimes();

        assert!(
            file.is_file(),
            "{} must survive: the job's respawnFlags name it as --settings=<path>",
            file.display()
        );
    });
}

/// A `respawnFlags` nested past serde_json's 128-level recursion limit is still
/// one job's unreadable flags, never an unreadable store: the file beside its
/// tree falls back to the tree rule, and a dead tree in an unrelated profile is
/// still collected.
#[test]
fn gc_scopes_a_deeply_nested_respawn_flags_value_to_its_own_job() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let profiles = tmp.path().join(".clauth").join("profiles");
        let (tree, _) = dead_pair(&profiles, "ssdeep", "runtime-5353-0", "5353-0");
        let beside = spawn_settings_file(&tree, "5353-0");
        fs::write(&beside, b"{}").expect("settings file beside the deep job's tree");
        let mut deep = serde_json::json!([]);
        for _ in 0..199 {
            deep = serde_json::json!([deep]);
        }
        write_claude_job_flags(tmp.path(), "cc3d4e5a", &tree, deep);
        let (unrelated, unrelated_sessions) =
            dead_pair(&profiles, "ssdeepother", "runtime-5454-0", "5454-0");

        gc_stale_runtimes();

        assert!(
            !unrelated.exists() && !unrelated_sessions.exists(),
            "a dead tree in an unrelated profile must still be collected: a 200-deep respawnFlags must not make the store unreadable"
        );
        assert!(
            beside.is_file(),
            "{} must survive: its tree's job has respawnFlags clauth cannot read",
            beside.display()
        );
    });
}

/// The settings sweep reads the job store inside its state-flock hold, never
/// before it: a job written after the candidate scan, before the dead decision,
/// still spares the file it names.
#[test]
fn gc_spares_a_settings_file_whose_job_is_written_inside_the_hold() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let profiles = tmp.path().join(".clauth").join("profiles");
        let (tree, _) = dead_pair(&profiles, "sslate", "runtime-5555-0", "5555-0");
        let file = spawn_settings_file(&tree, "5555-0");
        fs::write(&file, b"{}").expect("settings file");
        let home = tmp.path().to_path_buf();
        let (job_tree, named) = (tree.clone(), file.clone());

        gc_spawn_settings_files_synced(move || {
            write_claude_job_flags(&home, "cc3d4e5b", &job_tree, respawn_flags_naming(&named));
        });

        assert!(
            file.is_file(),
            "{} must survive: its job was written inside the hold, before the dead decision",
            file.display()
        );
    });
}

/// Replace the one occurrence of `from` in a job dir's `state.json` with `to`,
/// for record bytes a `serde_json::Value` cannot hold (an out-of-range number,
/// a lone surrogate escape, a repeated key).
fn rewrite_job_record(job_dir: &Path, from: &str, to: &str) {
    let path = job_dir.join("state.json");
    let record = fs::read_to_string(&path).expect("read job");
    assert_eq!(
        record.matches(from).count(),
        1,
        "{from} must occur once in {record}"
    );
    fs::write(&path, record.replace(from, to)).expect("rewrite job");
}

/// A `respawnFlags` token serde_json refuses to convert (an out-of-range
/// number, a lone surrogate escape) or a repeated `respawnFlags` key is one
/// job's unreadable flags, never an unreadable store: the file beside that
/// job's tree falls back to the tree rule, and a dead tree in an unrelated
/// profile is still collected.
#[test]
fn gc_scopes_a_respawn_flags_token_serde_json_refuses_to_its_own_job() {
    let shapes = [
        (
            "an out-of-range number element",
            r#""respawnFlags":[1e400]"#,
        ),
        (
            "an out-of-range --settings value",
            r#""respawnFlags":["--settings",1e400]"#,
        ),
        ("an out-of-range number", r#""respawnFlags":1e400"#),
        ("a lone surrogate element", r#""respawnFlags":["\ud800"]"#),
        (
            "a lone surrogate map-element key",
            r#""respawnFlags":[{"\ud800":1}]"#,
        ),
        ("a repeated key", r#""respawnFlags":[],"respawnFlags":[]"#),
    ];
    for (shape, raw) in shapes {
        let tmp = tempfile::tempdir().expect("tempdir");
        with_fake_home(tmp.path(), || {
            let profiles = tmp.path().join(".clauth").join("profiles");
            let (tree, _) = dead_pair(&profiles, "sstoken", "runtime-5656-0", "5656-0");
            let beside = spawn_settings_file(&tree, "5656-0");
            fs::write(&beside, b"{}").expect("settings file beside the job's tree");
            let job = write_claude_job_flags(
                tmp.path(),
                "dd4e5f6a",
                &tree,
                serde_json::json!("__flags__"),
            );
            rewrite_job_record(&job, r#""respawnFlags":"__flags__""#, raw);
            let (unrelated, unrelated_sessions) =
                dead_pair(&profiles, "sstokenother", "runtime-5757-0", "5757-0");

            gc_stale_runtimes();

            assert!(
                !unrelated.exists() && !unrelated_sessions.exists(),
                "a dead tree in an unrelated profile must still be collected ({shape}): the job store stays readable"
            );
            assert!(
                beside.is_file(),
                "{} must survive ({shape}): its tree's job has respawnFlags clauth cannot read",
                beside.display()
            );
        });
    }
}

/// A record whose `providerEnv` serde_json refuses still makes the store
/// unreadable: clauth cannot tell which tree that job reads, so every tree is
/// spared, a dead tree in an unrelated profile included.
#[test]
fn gc_keeps_every_tree_while_a_jobs_provider_env_cannot_be_read() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let profiles = tmp.path().join(".clauth").join("profiles");
        let (tree, _) = dead_pair(&profiles, "ssenv", "runtime-5858-0", "5858-0");
        let job = write_claude_job(tmp.path(), "dd4e5f6b", &tree);
        rewrite_job_record(
            &job,
            r#"{"CLAUDE_CONFIG_DIR":"#,
            r#"{"CLAUDE_CONFIG_DIR":1e400,"unread":"#,
        );
        let (unrelated, unrelated_sessions) =
            dead_pair(&profiles, "ssenvother", "runtime-5959-0", "5959-0");

        gc_stale_runtimes();

        assert!(
            unrelated.is_dir() && unrelated_sessions.is_dir(),
            "a dead tree in an unrelated profile must be spared: a job whose providerEnv cannot be read may read any tree"
        );
    });
}

/// A `state` nested past serde_json's 128-level recursion limit reads as an
/// unknown state (unfinished), never as an unreadable store: the job's own
/// tree is spared, and a dead tree in an unrelated profile is still collected.
#[test]
fn gc_reads_a_deeply_nested_job_state_as_unknown_not_unreadable() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let profiles = tmp.path().join(".clauth").join("profiles");
        let (tree, _) = dead_pair(&profiles, "ssstate", "runtime-6060-0", "6060-0");
        let job =
            crate::testutil::write_claude_job_in(tmp.path(), "dd4e5f6c", &tree, "__state__", None);
        let deep = format!("{}{}", "[".repeat(200), "]".repeat(200));
        rewrite_job_record(
            &job,
            r#""state":"__state__""#,
            &format!(r#""state":{deep}"#),
        );
        let (unrelated, unrelated_sessions) =
            dead_pair(&profiles, "ssstateother", "runtime-6161-0", "6161-0");

        gc_stale_runtimes();

        assert!(
            !unrelated.exists() && !unrelated_sessions.exists(),
            "a dead tree in an unrelated profile must still be collected: a 200-deep state must not make the store unreadable"
        );
        assert!(
            tree.join(".claude.json").is_file(),
            "the job's own tree stays: the job names it"
        );
    });
}

/// Teardown removes a session's settings file before it takes the state flock,
/// so a wedged peer holding the flock past every retry still cannot strand it.
#[test]
fn teardown_removes_an_unnamed_spawn_settings_file_while_the_state_flock_is_wedged() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        fake_claude_home(tmp.path());
        let profile = configured_profile("sswedge");
        crate::lock::set_state_lock_timeout_override(Some(Duration::from_millis(50)));
        let rt = ProfileRuntime::acquire(&profile, Isolation::Shared, &[], false, Launcher::Own)
            .expect("acquire");
        let settings = write_spawn_settings(&rt);
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

        drop(rt);

        assert!(
            timeouts.load(std::sync::atomic::Ordering::SeqCst) > 0,
            "the flock must have been wedged through teardown"
        );
        assert!(
            !settings.exists(),
            "teardown must remove {} even though the flock never came free",
            settings.display()
        );

        drop(holder);
        set_teardown_timeout_hook(None);
        crate::lock::set_state_lock_timeout_override(None);
    });
}

/// #1 (TOCTOU): the sweep snapshots the job store before the per-pair marker
/// check, so a `clauth start <p> -- --bg` that writes its job and releases its
/// marker between the snapshot and this pair's turn would be collected from a
/// stale "unnamed". The dead decision re-reads the store under the state lock,
/// so a job written after the snapshot still spares the tree.
#[test]
fn gc_spares_a_pair_when_the_job_is_written_after_the_sweep_snapshot() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let profiles = tmp.path().join(".clauth").join("profiles");
        let runtime = profiles.join("bgrace").join("runtime-777-4");
        let sessions = profiles.join("bgrace").join("sessions-777-4");
        fs::create_dir_all(&runtime).expect("mkdir runtime");
        fs::create_dir_all(&sessions).expect("mkdir sessions");
        fs::write(runtime.join(".claude.json"), b"{}").expect("seed");
        let marker = open_pid_file(&sessions.join("777-4")).expect("marker");
        marker.lock().expect("lock");
        let snapshot = crate::claude_jobs::job_trees();
        write_claude_job(tmp.path(), "37560ded", &runtime);
        drop(marker);
        let _ = gc_one_pair(&runtime, &sessions, &snapshot);
        assert!(
            runtime.join(".claude.json").is_file(),
            "a job written after the sweep snapshot must still spare the tree"
        );
        assert!(sessions.is_dir(), "its sessions dir goes nowhere either");
    });
}

/// #15: the in-lock re-read sits BEFORE the `if isolated` branch, so a job
/// written after the sweep snapshot spares an ISOLATED-flavor pair too — the
/// isolated rescue path (tombstone rename, then delete outside the lock) must
/// not run on a tree a live job reads. Pins the check's position ahead of the
/// rename: moving it after (`if !isolated && …`) would let the isolated tree
/// be tombstoned, rescued and deleted.
#[test]
fn gc_spares_a_dead_isolated_pair_a_job_written_after_the_sweep_snapshot() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let profiles = tmp.path().join(".clauth").join("profiles");
        let runtime = profiles.join("bgrace").join("runtime-isolated-777-4");
        let sessions = profiles.join("bgrace").join("sessions-isolated-777-4");
        fs::create_dir_all(&runtime).expect("mkdir runtime");
        fs::create_dir_all(&sessions).expect("mkdir sessions");
        fs::write(runtime.join(".claude.json"), b"{}").expect("seed");
        let marker = open_pid_file(&sessions.join("777-4")).expect("marker");
        marker.lock().expect("lock");
        let snapshot = crate::claude_jobs::job_trees();
        write_claude_job(tmp.path(), "37560ded", &runtime);
        drop(marker);
        let _ = gc_one_pair(&runtime, &sessions, &snapshot);
        assert!(
            runtime.join(".claude.json").is_file(),
            "a job written after the sweep snapshot must spare an isolated tree"
        );
        assert!(
            !profiles
                .join("bgrace")
                .join("runtime-isolated-777-4.rescuing")
                .exists(),
            "the isolated tree must not be renamed to its rescue tombstone"
        );
    });
}
/// bare-stem tree, so the tree a `clauth start <p> -- --bg` spared at teardown
/// is the one the next `clauth start <p>` would wipe. The acquire must skip a
/// tree the job store names. The sentinel is written AFTER the teardown's final
/// sync, so the sync never carried it into `~/.claude`: a wiped tree loses it
/// (the rebuild copies `~/.claude` back), a spared tree keeps it. A sentinel
/// written BEFORE the sync round-trips and cannot show the wipe (the reviewer
/// measured that), and a dir inode does not bind either: this box's test-tmp
/// filesystem reuses a freed dir inode, so `remove_dir_all` + `mkdir` reads the
/// same `ino`.
#[test]
fn acquire_keeps_a_tree_a_job_names_under_fake_link_mode() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        with_link_mode(LinkMode::Fake, || {
            fake_claude_home(tmp.path());
            let profile = configured_profile("bgfake");
            let canonical = tmp
                .path()
                .join(".clauth")
                .join("profiles")
                .join("bgfake")
                .join("credentials.json");
            fs::create_dir_all(canonical.parent().expect("parent")).expect("mkdir");
            fs::write(&canonical, CREDS_V2).expect("write canonical");
            let rt =
                ProfileRuntime::acquire(&profile, Isolation::Shared, &[], false, Launcher::Own)
                    .expect("acquire");
            let tree = rt.config_dir().to_path_buf();
            write_claude_job(tmp.path(), "37560ded", &tree);
            drop(rt);
            assert!(tree.is_dir(), "teardown spared the tree");
            fs::write(tree.join("bg-session-sentinel"), b"x").expect("sentinel");
            let rt2 =
                ProfileRuntime::acquire(&profile, Isolation::Shared, &[], false, Launcher::Own)
                    .expect("acquire 2");
            assert_eq!(rt2.config_dir(), tree.as_path(), "same bare-stem tree");
            assert!(
                tree.join("bg-session-sentinel").is_file(),
                "the next acquire must not wipe a tree a live job names"
            );
            drop(rt2);
        });
    });
}

/// #6: a plain file (`pins.json`) beside the job dirs is no job — the reader
/// skips it, so a dead pair is still collected. The skip guard is pinned
/// because a real `~/.claude/jobs` holds `pins.json`, and losing it would read
/// the store as permanently unreadable and never collect a tree again.
#[test]
fn gc_collects_a_dead_pair_when_a_plain_file_sits_in_the_job_store() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let profiles = tmp.path().join(".clauth").join("profiles");
        let (runtime, sessions) = dead_pair(&profiles, "bgpins", "runtime-4242-7", "4242-7");
        let jobs = tmp.path().join(".claude").join("jobs");
        fs::create_dir_all(&jobs).expect("mkdir jobs");
        fs::write(jobs.join("pins.json"), b"[]").expect("pins");
        gc_stale_runtimes();
        assert!(
            !runtime.exists() && !sessions.exists(),
            "a plain file in the job store is no job, the dead pair is collected"
        );
    });
}

/// #7: a job dir caught before its first `state.json` lands spares the dead
/// pair — the reader is fail-closed on a missing record, not permissive.
/// Claude Code creates the dir, writes `state.json.tmp.<rand>`, renames it to
/// `state.json`, and only then creates `tmp/` (measured 5/5, 2.1.308, the T35
/// probe), so the mid-create shapes are an empty dir and a dir holding only
/// the staged record. A permissive arm (`Err(_) if kind == NotFound =>
/// continue`) would collect during job creation.
#[test]
fn gc_spares_a_dead_pair_when_a_job_dir_has_no_state_yet() {
    for (shape, staged) in [("empty", None), ("staged", Some("state.json.tmp.8d511663"))] {
        let tmp = tempfile::tempdir().expect("tempdir");
        with_fake_home(tmp.path(), || {
            let profiles = tmp.path().join(".clauth").join("profiles");
            let (runtime, sessions) = dead_pair(&profiles, "bgmid", "runtime-4242-8", "4242-8");
            let job = tmp.path().join(".claude").join("jobs").join("0add99ec");
            fs::create_dir_all(&job).expect("mkdir job dir");
            if let Some(staged) = staged {
                fs::write(job.join(staged), b"{\"state\":").expect("staged record");
            }
            gc_stale_runtimes();
            assert!(
                runtime.join(".claude.json").is_file() && sessions.is_dir(),
                "{shape}: a job dir with no state.json yet must spare the dead pair"
            );
        });
    }
}

/// F9 (cloudy 2026-10-22): on a copy host the tree's `jobs/` is its own real
/// dir (the mirror never carries it), so a `claude --bg` started inside that
/// session records itself ONLY there. Every job reader must still see it: GC
/// spares the dead pair and the account counts as live. Control: the same tree
/// with that record removed is collected.
#[test]
fn gc_spares_a_copy_host_tree_whose_own_job_store_names_it() {
    // T33b1 r5 F9: on a copy host the tree's `jobs/` is its own real dir (the
    // mirror never carries it, cloudy 2026-10-22), so a `claude --bg` started
    // inside that session records itself ONLY there. Every job reader must
    // still see it: GC spares the dead pair and the account counts as live.
    // Control: the same tree with that record removed is collected.
    for named in [true, false] {
        let tmp = tempfile::tempdir().expect("tempdir");
        with_fake_home(tmp.path(), || {
            let profiles = tmp.path().join(".clauth").join("profiles");
            let (runtime, sessions) = dead_pair(&profiles, "copyjob", "runtime-4242-11", "4242-11");
            if named {
                let job = runtime.join("jobs").join("0add99ee");
                fs::create_dir_all(&job).expect("mkdir tree job");
                let record = serde_json::json!({
                    "state": "working",
                    "respawnFlags": [],
                    "providerEnv": { "CLAUDE_CONFIG_DIR": runtime.to_string_lossy() },
                });
                fs::write(
                    job.join("state.json"),
                    serde_json::to_vec(&record).expect("encode job"),
                )
                .expect("tree job");
                assert!(
                    has_live_session(&crate::profile::ProfileName::from("copyjob")),
                    "a job recorded only inside the tree keeps the account live"
                );
            }
            gc_stale_runtimes();
            assert_eq!(
                runtime.exists() && sessions.exists(),
                named,
                "named: {named}: spared iff the tree's own job store names it"
            );
        });
    }
}

/// F9 guard: a tree's `jobs/` is read only as a REAL directory. A symlinked
/// `jobs` — the linked host's shape, where it IS the global store already read
/// — is never followed, so a poisoned dir it points at cannot make the read
/// unreadable; the dead pair is still collected.
#[cfg(unix)]
#[test]
fn gc_skips_a_symlinked_tree_jobs_dir() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let profiles = tmp.path().join(".clauth").join("profiles");
        let (runtime, sessions) = dead_pair(&profiles, "bgjobs", "runtime-4242-12", "4242-12");
        let target = tmp.path().join("poison");
        fs::create_dir_all(target.join("0add99ef")).expect("mkdir poison job");
        fs::write(target.join("0add99ef").join("state.json"), b"{\"state\":").expect("bad record");
        std::os::unix::fs::symlink(&target, runtime.join("jobs")).expect("symlink jobs");
        gc_stale_runtimes();
        assert!(
            !runtime.exists() && !sessions.exists(),
            "a symlinked jobs dir is never followed, so its poisoned target spares nothing"
        );
    });
}

/// F9 guard: a non-dir `jobs` entry in a tree is an unknown shape and fails
/// closed — the whole read is unreadable, so the dead pair is spared.
#[test]
fn gc_spares_a_dead_pair_when_a_tree_jobs_entry_is_not_a_directory() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let profiles = tmp.path().join(".clauth").join("profiles");
        let (runtime, sessions) = dead_pair(&profiles, "bgjobs", "runtime-4242-13", "4242-13");
        fs::write(runtime.join("jobs"), b"not a dir").expect("jobs file");
        gc_stale_runtimes();
        assert!(
            runtime.exists() && sessions.exists(),
            "a non-dir jobs entry reads as unreadable and spares the dead pair (fail closed)"
        );
    });
}

/// Write a tree-local job record: `<tree>/jobs/<id>/state.json` naming `names`.
fn tree_job(tree: &Path, id: &str, state: &str, names: &Path) {
    let job = tree.join("jobs").join(id);
    fs::create_dir_all(&job).expect("mkdir tree job");
    let record = serde_json::json!({
        "state": state,
        "respawnFlags": [],
        "providerEnv": { "CLAUDE_CONFIG_DIR": names.to_string_lossy() },
    });
    fs::write(
        job.join("state.json"),
        serde_json::to_vec(&record).expect("encode job"),
    )
    .expect("write tree job");
}

/// Chmod `dir` to 0o000 for the fail-closed arms. Returns false (and restores)
/// when running as root, where the chmod has no effect on readability.
#[cfg(unix)]
fn pose_unreadable(dir: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o000)).expect("chmod 000");
    if fs::read_dir(dir).is_ok() {
        let _ = fs::set_permissions(dir, fs::Permissions::from_mode(0o700));
        return false;
    }
    true
}

#[cfg(unix)]
fn restore_readable(dir: &Path) {
    use std::os::unix::fs::PermissionsExt as _;
    let _ = fs::set_permissions(dir, fs::Permissions::from_mode(0o700));
}

/// F19: a symlinked profile dir's trees must still be read by the job walker
/// (it enumerates the link the way the census does), so GC spares a tree a
/// tree-local job names and the account counts live.
#[cfg(unix)]
#[test]
fn a_symlinked_profile_dir_still_spares_a_tree_local_job() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let profiles = tmp.path().join(".clauth").join("profiles");
        let elsewhere = tmp.path().join("elsewhere");
        let (real_rt, real_sess) = dead_pair(&elsewhere, "symjob", "runtime-4242-30", "4242-30");
        fs::create_dir_all(&profiles).expect("mkdir profiles");
        std::os::unix::fs::symlink(elsewhere.join("symjob"), profiles.join("symjob"))
            .expect("symlink profile dir");
        let named = profiles.join("symjob").join("runtime-4242-30");
        tree_job(&real_rt, "aa000030", "working", &named);
        assert!(
            has_live_session(&crate::profile::ProfileName::from("symjob")),
            "a job recorded under a symlinked profile dir keeps the account live"
        );
        gc_stale_runtimes();
        assert!(
            real_rt.exists() && real_sess.exists(),
            "a symlinked profile dir's tree-local job spares the tree"
        );
    });
}

/// F23: an isolated tree's own job store spares the tree and blocks
/// rename/disable only while the job is UNFINISHED; a finished job holds
/// nothing, since no session can re-enter an isolated tree.
#[test]
fn an_isolated_trees_job_store_counts_only_unfinished_jobs() {
    for (state, spared) in [("working", true), ("stopped", false)] {
        let tmp = tempfile::tempdir().expect("tempdir");
        with_fake_home(tmp.path(), || {
            let profiles = tmp.path().join(".clauth").join("profiles");
            let (runtime, sessions) =
                dead_pair(&profiles, "isojob", "runtime-isolated-4242-31", "4242-31");
            tree_job(&runtime, "aa000031", state, &runtime);
            assert_eq!(
                has_live_session(&crate::profile::ProfileName::from("isojob")),
                spared,
                "{state}: a finished isolated job is not live"
            );
            gc_stale_runtimes();
            assert_eq!(
                runtime.exists() && sessions.exists(),
                spared,
                "{state}: spared iff the isolated job is unfinished"
            );
        });
    }
}

/// A tree-local record naming a DIFFERENT tree is a stale copy (a copy-host
/// mirror's leftover) and is skipped, so it cannot pin another account's tree.
#[test]
fn a_tree_local_job_naming_another_tree_is_skipped() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let profiles = tmp.path().join(".clauth").join("profiles");
        let (b_rt, b_sess) = dead_pair(&profiles, "f24b", "runtime-4242-32", "4242-32");
        let c_rt = profiles.join("f24c").join("runtime");
        let c_sess = profiles.join("f24c").join("sessions");
        fs::create_dir_all(&c_rt).expect("mkdir c");
        fs::create_dir_all(&c_sess).expect("mkdir c sessions");
        tree_job(&c_rt, "aa000032", "stopped", &b_rt);
        assert!(
            !has_live_session(&crate::profile::ProfileName::from("f24b")),
            "a tree-local record naming another tree does not make that account live"
        );
        gc_stale_runtimes();
        assert!(
            !b_rt.exists() && !b_sess.exists(),
            "a tree-local record naming another tree spares nothing"
        );
        assert!(
            !c_rt.exists() && !c_sess.exists(),
            "a tree-local store holding only foreign records spares its own tree nothing"
        );
    });
}

/// A linked-shape shared tree (its `CLAUDE.md` reads as a link) whose OWN real
/// `jobs/` holds a live record written by Claude Code itself must be spared and
/// count the account live: only a record naming a DIFFERENT tree is a stale
/// copy. The linked-host shared `jobs/` is a symlink by construction, so a real
/// dir there was written by the session, never by a mirror.
#[cfg(unix)]
#[test]
fn a_linked_shape_shared_tree_holding_its_own_live_jobs_dir_is_spared() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let profiles = tmp.path().join(".clauth").join("profiles");
        let tree = profiles.join("g1link").join("runtime-bg");
        let sessions = profiles.join("g1link").join("sessions-bg");
        fs::create_dir_all(&tree).expect("mkdir tree");
        fs::create_dir_all(&sessions).expect("mkdir sessions");
        std::os::unix::fs::symlink(tmp.path().join("real-claude-md"), tree.join("CLAUDE.md"))
            .expect("symlink CLAUDE.md");
        tree_job(&tree, "aa000036", "working", &tree);
        assert_eq!(
            crate::runtime::link_mode_of(Some(&tree)),
            crate::runtime::LinkProbe::Real,
            "the fixture reads as a linked-host tree"
        );
        assert!(
            has_live_session(&crate::profile::ProfileName::from("g1link")),
            "a linked-shape shared tree's own live jobs record counts the account live"
        );
        gc_stale_runtimes();
        assert!(
            tree.exists() && sessions.exists(),
            "the linked-shape shared tree is spared"
        );
    });
}

/// F20 (teardown): a tree-local job keeps the tree past the session teardown.
#[test]
fn teardown_spares_a_tree_local_job() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        with_link_mode(LinkMode::Fake, || {
            fake_claude_home(tmp.path());
            let profile = configured_profile("f20tear");
            seed_profile_store(tmp.path(), "f20tear");
            let rt =
                ProfileRuntime::acquire(&profile, Isolation::Shared, &[], false, Launcher::Own)
                    .expect("acquire");
            let tree = rt.config_dir().to_path_buf();
            tree_job(&tree, "aa000033", "working", &tree);
            drop(rt);
            assert!(
                tree.is_dir(),
                "a tree-local job keeps the tree past the session teardown"
            );
        });
    });
}

/// F20 (acquire wipe): the acquire wipe skips a tree its own job store names.
#[test]
fn the_acquire_wipe_spares_a_tree_local_job() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        with_link_mode(LinkMode::Fake, || {
            fake_claude_home(tmp.path());
            let profile = configured_profile("f20wipe");
            seed_profile_store(tmp.path(), "f20wipe");
            let rt =
                ProfileRuntime::acquire(&profile, Isolation::Shared, &[], false, Launcher::Own)
                    .expect("acquire");
            let tree = rt.config_dir().to_path_buf();
            tree_job(&tree, "aa000034", "working", &tree);
            drop(rt);
            assert!(tree.is_dir(), "teardown spared the tree first");
            fs::write(tree.join("f20-sentinel"), b"x").expect("sentinel");
            let rt2 =
                ProfileRuntime::acquire(&profile, Isolation::Shared, &[], false, Launcher::Own)
                    .expect("acquire 2");
            assert!(
                tree.join("f20-sentinel").is_file(),
                "the acquire must not wipe a tree its own job store names"
            );
            drop(rt2);
        });
    });
}

/// F20 (rotation term): a tree-local unfinished job blocks the macOS rotation.
#[test]
fn the_rotation_job_term_reads_a_tree_local_job() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let name = crate::profile::ProfileName::from("f20rot");
        let tree = tmp.path().join(".clauth/profiles/f20rot/runtime");
        fs::create_dir_all(&tree).expect("mkdir tree");
        write_creds(&tree.join(".credentials.json"), Some("rt"));
        tree_job(&tree, "aa000035", "working", &tree);
        assert_eq!(
            rotation_block_on(&name, true),
            Some(RotationBlock::BackgroundJob),
            "a tree-local unfinished job blocks the rotation"
        );
    });
}

/// F20 (settings spare): a tree-local job's `respawnFlags` spare a settings
/// file they name.
#[test]
fn a_tree_local_job_names_a_settings_file() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let profiles = tmp.path().join(".clauth").join("profiles");
        let (runtime, _sessions) = dead_pair(&profiles, "f20set", "runtime-4242-37", "4242-37");
        let settings = tmp.path().join("spawn-settings-4242-37.json");
        fs::write(&settings, b"{}").expect("settings");
        let job = runtime.join("jobs").join("aa000037");
        fs::create_dir_all(&job).expect("mkdir tree job");
        let record = serde_json::json!({
            "state": "working",
            "respawnFlags": ["--settings", settings.to_string_lossy()],
            "providerEnv": { "CLAUDE_CONFIG_DIR": runtime.to_string_lossy() },
        });
        fs::write(
            job.join("state.json"),
            serde_json::to_vec(&record).expect("encode job"),
        )
        .expect("write tree job");
        let jobs = crate::claude_jobs::job_trees();
        assert!(
            jobs.spares_settings(&settings, &[runtime]),
            "a tree-local job's respawnFlags spare the settings file they name"
        );
    });
}

/// F22 + F21 arm 1: an unreadable tree-local store blocks only ITS account
/// (live and rotation), never an unrelated account.
#[cfg(unix)]
#[test]
fn a_tree_local_store_failure_blocks_only_its_account() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let profiles = tmp.path().join(".clauth").join("profiles");
        let (runtime, sessions) = dead_pair(&profiles, "f22a", "runtime-4242-38", "4242-38");
        let jobs = runtime.join("jobs");
        fs::create_dir_all(&jobs).expect("mkdir tree jobs");
        if !pose_unreadable(&jobs) {
            return;
        }
        let name = crate::profile::ProfileName::from("f22a");
        assert!(
            has_live_session(&name),
            "an unreadable tree-local store is live"
        );
        assert!(
            matches!(
                rotation_block_on(&name, true),
                Some(RotationBlock::UnreadableJobStore(store))
                    if store == jobs
            ),
            "the rotation names the tree's own store"
        );
        gc_stale_runtimes();
        assert!(
            runtime.exists() && sessions.exists(),
            "the tree is spared (fail closed)"
        );
        // A second account is untouched by the first's unreadable store.
        assert!(
            !has_live_session(&crate::profile::ProfileName::from("f22b")),
            "an unrelated account is not live"
        );
        assert_eq!(
            rotation_block_on(&crate::profile::ProfileName::from("f22b"), true),
            None,
            "an unrelated account's rotation is not blocked"
        );
        restore_readable(&jobs);
    });
}

/// F21 arm 2: an lstat error on `<tree>/jobs` (an unreadable tree) still spares
/// the tree — fail closed.
#[cfg(unix)]
#[test]
fn an_unreadable_tree_still_spares_it() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let profiles = tmp.path().join(".clauth").join("profiles");
        let (runtime, sessions) = dead_pair(&profiles, "f21tree", "runtime-4242-39", "4242-39");
        if !pose_unreadable(&runtime) {
            return;
        }
        assert!(
            has_live_session(&crate::profile::ProfileName::from("f21tree")),
            "an unreadable tree reads live"
        );
        gc_stale_runtimes();
        assert!(runtime.exists() && sessions.exists(), "the tree is spared");
        restore_readable(&runtime);
    });
}

/// F21 arm 3 + F22 scoping: an unreadable profile dir blocks only that account.
#[cfg(unix)]
#[test]
fn an_unreadable_profile_dir_blocks_only_that_account() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let profiles = tmp.path().join(".clauth").join("profiles");
        let locked = profiles.join("f22locked");
        fs::create_dir_all(&locked).expect("mkdir locked");
        if !pose_unreadable(&locked) {
            return;
        }
        assert!(
            crate::claude_jobs::job_trees().spares_under(&locked),
            "an unreadable profile dir spares its own account's trees (fail closed)"
        );
        assert!(
            has_live_session(&crate::profile::ProfileName::from("f22locked")),
            "an unreadable profile dir reads live"
        );
        assert!(
            rotation_blocked_on(&crate::profile::ProfileName::from("f22locked"), true),
            "an unreadable profile dir blocks its account's rotation"
        );
        assert!(
            matches!(
                rotation_block_on(&crate::profile::ProfileName::from("f22locked"), true),
                Some(RotationBlock::UnreadableJobStore(store)) if store == locked
            ),
            "an unreadable profile dir names itself as the unreadable store, not a live session"
        );
        assert!(
            !has_live_session(&crate::profile::ProfileName::from("f22other")),
            "an unrelated account is not live"
        );
        assert!(
            !rotation_blocked_on(&crate::profile::ProfileName::from("f22other"), true),
            "an unrelated account's rotation is not blocked"
        );
        restore_readable(&locked);
    });
}

/// F21 arm 4: an unreadable `profiles/` root blocks every account (global).
#[cfg(unix)]
#[test]
fn an_unreadable_profiles_root_blocks_every_account() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let profiles = tmp.path().join(".clauth").join("profiles");
        fs::create_dir_all(&profiles).expect("mkdir profiles");
        if !pose_unreadable(&profiles) {
            return;
        }
        assert!(
            matches!(
                crate::claude_jobs::job_trees(),
                crate::claude_jobs::JobTrees::Unreadable { .. }
            ),
            "an unreadable profiles root reads the store as unreadable (fail closed)"
        );
        assert!(
            has_live_session(&crate::profile::ProfileName::from("f22any")),
            "an unreadable profiles root reads every account live"
        );
        assert!(
            rotation_blocked_on(&crate::profile::ProfileName::from("f22any"), true),
            "an unreadable profiles root blocks every account's rotation"
        );
        restore_readable(&profiles);
    });
}

/// F22 global: an unreadable global `~/.claude/jobs` still blocks every
/// account, and the refusal names the conventional store path.
#[cfg(unix)]
#[test]
fn an_unreadable_global_store_blocks_every_account() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let jobs = tmp.path().join(".claude").join("jobs");
        fs::create_dir_all(&jobs).expect("mkdir global jobs");
        if !pose_unreadable(&jobs) {
            return;
        }
        assert!(
            has_live_session(&crate::profile::ProfileName::from("f22g")),
            "an unreadable global store reads every account live"
        );
        assert_eq!(
            rotation_block_on(&crate::profile::ProfileName::from("f22g"), true),
            Some(RotationBlock::UnreadableJobStore("~/.claude/jobs".into())),
            "the global refusal names the conventional store path"
        );
        restore_readable(&jobs);
    });
}

/// cloudy 2026-10-22: `claude rm` can leave the job's supervisor running —
/// measured with a second tree's supervisor sharing the roster — and that
/// supervisor recreates the job dir with no record — an empty `tmp/`, then
/// `exit-cause` once it is signalled (a lone bare job's supervisor exits
/// instead). A creation never makes those shapes (`tmp/` follows
/// `state.json`, #7), so a dir holding nothing but an empty `tmp/` and/or an
/// `exit-cause` file is a removed job and the dead pair is collected. Any other
/// record-less content stays unreadable and spares, as before.
#[test]
fn gc_reads_a_removed_jobs_leftover_dir_as_no_job() {
    for (shape, with_tmp, with_cause, collected) in [
        ("tmp only", true, false, true),
        ("exit-cause only", false, true, true),
        ("tmp and exit-cause", true, true, true),
        ("tmp holding a file", true, false, false),
        ("exit-cause a directory", false, false, false),
    ] {
        let tmp = tempfile::tempdir().expect("tempdir");
        with_fake_home(tmp.path(), || {
            let profiles = tmp.path().join(".clauth").join("profiles");
            let (runtime, sessions) = dead_pair(&profiles, "bgrm", "runtime-4242-9", "4242-9");
            let job = tmp.path().join(".claude").join("jobs").join("0add99ed");
            fs::create_dir_all(&job).expect("mkdir job dir");
            if with_tmp {
                fs::create_dir_all(job.join("tmp")).expect("mkdir tmp");
            }
            if with_cause {
                fs::write(job.join("exit-cause"), b"SIGTERM").expect("exit-cause");
            }
            if shape == "tmp holding a file" {
                fs::write(job.join("tmp").join("x"), b"x").expect("tmp file");
            }
            if shape == "exit-cause a directory" {
                fs::create_dir_all(job.join("exit-cause")).expect("exit-cause dir");
            }
            gc_stale_runtimes();
            assert_eq!(
                !runtime.exists() && !sessions.exists(),
                collected,
                "{shape}: collected iff the dir is a removed job's leftover"
            );
        });
    }
}

/// The leftover reader's unix-only guard shapes: a `tmp` that is a SYMLINK to
/// an empty dir (not a real dir), and a job dir whose listing fails (mode
/// 0o300, so the `state.json` lookup gives NotFound and the listing gives
/// EACCES). Each stays unreadable, so the dead pair is spared — fail closed.
#[cfg(unix)]
#[test]
fn gc_spares_a_dead_pair_for_unreadable_leftover_guard_shapes() {
    use std::os::unix::fs::PermissionsExt as _;
    for (shape, symlink_tmp, unlistable) in [
        ("tmp a symlink to an empty dir", true, false),
        ("an unlistable job dir", false, true),
    ] {
        let tmp = tempfile::tempdir().expect("tempdir");
        with_fake_home(tmp.path(), || {
            let profiles = tmp.path().join(".clauth").join("profiles");
            let (runtime, sessions) = dead_pair(&profiles, "bgrm", "runtime-4242-9", "4242-9");
            let job = tmp.path().join(".claude").join("jobs").join("0add99ed");
            fs::create_dir_all(&job).expect("mkdir job dir");
            if symlink_tmp {
                let target = tmp.path().join("empty");
                fs::create_dir_all(&target).expect("empty dir");
                std::os::unix::fs::symlink(&target, job.join("tmp")).expect("symlink tmp");
            }
            if unlistable {
                fs::create_dir_all(job.join("tmp")).expect("mkdir tmp");
                fs::set_permissions(&job, fs::Permissions::from_mode(0o300)).expect("chmod");
                // Running with rights that ignore the mode (root): the listing
                // succeeds, so the unreadable shape cannot be posed; restore
                // and skip the row rather than fail vacuously.
                if fs::read_dir(&job).is_ok() {
                    let _ = fs::set_permissions(&job, fs::Permissions::from_mode(0o700));
                    return;
                }
            }
            gc_stale_runtimes();
            let spared = runtime.exists() && sessions.exists();
            // Restore the mode before asserting so the tempdir teardown can
            // remove the unlistable dir.
            if unlistable {
                let _ = fs::set_permissions(&job, fs::Permissions::from_mode(0o700));
            }
            assert!(
                spared,
                "{shape}: the dead pair is spared (unreadable, fail closed)"
            );
        });
    }
}

/// #10: an isolated-flavor (`runtime-isolated-<sid>`) job-named dead pair is
/// neither tombstoned nor deleted — the rescue path (rename to a tombstone,
/// then delete outside the lock) must not run on a tree a live job reads.
#[test]
fn gc_spares_a_dead_isolated_pair_a_job_names_without_tombstoning() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let profiles = tmp.path().join(".clauth").join("profiles");
        let (runtime, sessions) =
            dead_pair(&profiles, "bgiso", "runtime-isolated-4242-0", "4242-0");
        write_claude_job(tmp.path(), "37560ded", &runtime);
        gc_stale_runtimes();
        assert!(
            runtime.join(".claude.json").is_file(),
            "a named isolated tree must survive the sweep"
        );
        assert!(sessions.is_dir(), "its sessions dir goes nowhere either");
        assert!(
            !profiles
                .join("bgiso")
                .join("runtime-isolated-4242-0.rescuing")
                .exists(),
            "a named isolated tree must not be renamed to its rescue tombstone"
        );
    });
}

/// #11: the spare rule compares the job's config dir with the tree path in one
/// canonical spelling, so a job naming the tree through a symlinked parent
/// still spares it. Pins [`crate::canonical::identity`] over a raw path `==`.
#[cfg(unix)]
#[test]
fn gc_spares_a_dead_pair_named_through_a_symlinked_parent() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let profiles = tmp.path().join(".clauth").join("profiles");
        let (runtime, sessions) = dead_pair(&profiles, "bglink", "runtime-4242-9", "4242-9");
        let link = tmp.path().join("link-to-bglink");
        std::os::unix::fs::symlink(profiles.join("bglink"), &link).expect("symlink");
        write_claude_job(tmp.path(), "37560ded", &link.join("runtime-4242-9"));
        gc_stale_runtimes();
        assert!(
            runtime.join(".claude.json").is_file() && sessions.is_dir(),
            "a tree named through a symlinked parent must survive the sweep"
        );
    });
}

/// #4: the per-pair spare line fires only on a NAMED hit, by equality.
#[test]
fn gc_logs_the_per_pair_spare_line_once_for_a_named_tree() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let profiles = tmp.path().join(".clauth").join("profiles");
        let (runtime, sessions) = dead_pair(&profiles, "bgnamed", "runtime-4242-5", "4242-5");
        write_claude_job(tmp.path(), "37560ded", &runtime);

        let lines = crate::logline::LogLines::new();
        {
            let _cap = lines.capture_here();
            gc_stale_runtimes();
        }
        let snap = lines.snapshot();
        let per_pair = format!(
            "clauth: spared runtime tree {}: a live Claude Code background job names it",
            runtime.display()
        );
        assert_eq!(
            snap.iter().filter(|l| **l == per_pair).count(),
            1,
            "the per-pair spare line appears exactly once, by equality"
        );
        assert!(
            !snap.iter().any(|l| l.contains("could not be read")),
            "no sweep-level unreadable line under a named hit"
        );
        assert!(sessions.is_dir(), "its sessions dir goes nowhere either");
    });
}

/// #4: under an unreadable store `spares` is true for every pair — live ones
/// included — so a per-pair line there would claim a live job names each tree.
/// The sweep-level line covers Unreadable, once per sweep; no per-pair line is
/// logged.
#[test]
fn gc_logs_no_per_pair_spare_line_under_an_unreadable_store() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let profiles = tmp.path().join(".clauth").join("profiles");
        let (_live_runtime, live_sessions) =
            dead_pair(&profiles, "bglive", "runtime-4242-3", "4242-3");
        let marker = open_pid_file(&live_sessions.join("4242-3")).expect("marker");
        marker.lock().expect("lock");
        let (_dead_runtime, _dead_sessions) =
            dead_pair(&profiles, "bgdead", "runtime-4242-4", "4242-4");
        let job = tmp.path().join(".claude").join("jobs").join("0add99ec");
        fs::create_dir_all(&job).expect("mkdir job dir");
        fs::write(
            job.join("state.json"),
            br#"{"state":"working","providerEnv":{"CLAUDE_CON"#,
        )
        .expect("torn job record");

        let lines = crate::logline::LogLines::new();
        {
            let _cap = lines.capture_here();
            gc_stale_runtimes();
        }
        let snap = lines.snapshot();
        let sweep_line = format!(
            "clauth: spared every dead runtime tree this sweep: a Claude Code \
             background-job record under {} could not be read",
            job.display()
        );
        assert_eq!(
            snap.iter().filter(|l| **l == sweep_line).count(),
            1,
            "the sweep-level spare line appears exactly once, by equality"
        );
        assert!(
            !snap.iter().any(|l| l.contains("spared runtime tree")),
            "no per-pair spare line under an unreadable store, live or dead pair"
        );
        drop(marker);
    });
}

/// A scoped failure (a tree's own unreadable `jobs/`) spares the tree and logs
/// ONE sweep-level line naming the store, once per sweep; the per-pair line
/// stays silent because no job was shown to name the tree.
#[cfg(unix)]
#[test]
fn a_scoped_store_failure_logs_one_sweep_line_naming_the_path() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let profiles = tmp.path().join(".clauth").join("profiles");
        let (runtime, sessions) = dead_pair(&profiles, "g6tree", "runtime-4242-41", "4242-41");
        let jobs = runtime.join("jobs");
        fs::create_dir_all(&jobs).expect("mkdir tree jobs");
        if !pose_unreadable(&jobs) {
            return;
        }
        let lines = crate::logline::LogLines::new();
        {
            let _cap = lines.capture_here();
            gc_stale_runtimes();
        }
        let snap = lines.snapshot();
        let sweep_line = format!(
            "clauth: spared runtime trees this sweep: the Claude Code \
             background-job store {} could not be read",
            jobs.display()
        );
        assert_eq!(
            snap.iter().filter(|l| **l == sweep_line).count(),
            1,
            "the scoped spare logs one sweep-level line naming the store, by equality"
        );
        assert!(
            !snap
                .iter()
                .any(|l| l.contains("a live Claude Code background job names it")),
            "no per-pair line claims a job names a tree whose store merely could not be read"
        );
        assert!(runtime.exists() && sessions.exists(), "the tree is spared");
        restore_readable(&jobs);
    });
}

/// A crash (or a teardown that could not take the state lock) strands a
/// session's per-spawn `--settings` file, which can hold a folded user `env`
/// token. GC collects it with the dead session, a stray one with no session at
/// all, and spares a held session's.
#[test]
fn gc_collects_a_dead_sessions_spawn_settings_file_and_spares_a_held_ones() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let profiles = tmp.path().join(".clauth").join("profiles");

        let dead_runtime = profiles.join("ssdead").join("runtime-4343-0");
        let dead_sessions = profiles.join("ssdead").join("sessions-4343-0");
        fs::create_dir_all(&dead_runtime).expect("mkdir runtime");
        fs::create_dir_all(&dead_sessions).expect("mkdir sessions");
        fs::write(dead_sessions.join("4343-0"), b"").expect("dead marker");
        let dead_file = spawn_settings_file(&dead_runtime, "4343-0");
        fs::write(&dead_file, b"{}").expect("dead settings file");

        let stray_file =
            spawn_settings_file(&profiles.join("ssstray").join("runtime-5151-0"), "5151-0");
        fs::create_dir_all(stray_file.parent().expect("profile dir")).expect("mkdir profile");
        fs::write(&stray_file, b"{}").expect("stray settings file");

        let held_runtime = profiles.join("sslive").join("runtime-778-3");
        let held_sessions = profiles.join("sslive").join("sessions-778-3");
        fs::create_dir_all(&held_runtime).expect("mkdir held runtime");
        fs::create_dir_all(&held_sessions).expect("mkdir held sessions");
        let marker = open_pid_file(&held_sessions.join("778-3")).expect("open held marker");
        marker.lock().expect("lock held marker");
        let held_file = spawn_settings_file(&held_runtime, "778-3");
        fs::write(&held_file, b"{}").expect("held settings file");

        // The other two marker layouts: an isolated session keys its marker
        // `sessions-isolated-<sid>/<sid>`, a fake-mode shared one the bare
        // `sessions/<sid>` beside the one shared `runtime` dir.
        let iso_sessions = profiles.join("ssiso").join("sessions-isolated-779-1");
        fs::create_dir_all(&iso_sessions).expect("mkdir isolated sessions");
        let iso_marker = open_pid_file(&iso_sessions.join("779-1")).expect("open isolated marker");
        iso_marker.lock().expect("lock isolated marker");
        let iso_file = spawn_settings_file(
            &profiles.join("ssiso").join("runtime-isolated-779-1"),
            "779-1",
        );
        fs::write(&iso_file, b"{}").expect("isolated settings file");

        let fake_sessions = profiles.join("ssfake").join("sessions");
        fs::create_dir_all(&fake_sessions).expect("mkdir fake sessions");
        let fake_marker = open_pid_file(&fake_sessions.join("780-2")).expect("open fake marker");
        fake_marker.lock().expect("lock fake marker");
        let fake_file = spawn_settings_file(&profiles.join("ssfake").join("runtime"), "780-2");
        fs::write(&fake_file, b"{}").expect("fake-mode settings file");

        // The same two layouts, crashed: markers present but unlocked.
        let dead_iso_sessions = profiles.join("ssiso").join("sessions-isolated-881-1");
        fs::create_dir_all(&dead_iso_sessions).expect("mkdir dead isolated sessions");
        fs::write(dead_iso_sessions.join("881-1"), b"").expect("dead isolated marker");
        let dead_iso_file = spawn_settings_file(
            &profiles.join("ssiso").join("runtime-isolated-881-1"),
            "881-1",
        );
        fs::write(&dead_iso_file, b"{}").expect("dead isolated settings file");
        fs::write(fake_sessions.join("882-2"), b"").expect("dead fake-mode marker");
        let dead_fake_file = spawn_settings_file(&profiles.join("ssfake").join("runtime"), "882-2");
        fs::write(&dead_fake_file, b"{}").expect("dead fake-mode settings file");

        gc_stale_runtimes();

        for (file, flavor) in [
            (&dead_iso_file, "isolated"),
            (&dead_fake_file, "fake-mode shared"),
        ] {
            assert!(
                !file.exists(),
                "a dead {flavor} session's {} must be collected",
                file.display()
            );
        }

        assert!(
            !dead_file.exists(),
            "{} must go with its dead session",
            dead_file.display()
        );
        assert!(
            !stray_file.exists(),
            "{} has no session at all",
            stray_file.display()
        );
        for (file, flavor) in [
            (&held_file, "real-mode shared"),
            (&iso_file, "isolated"),
            (&fake_file, "fake-mode shared"),
        ] {
            assert!(
                file.is_file(),
                "a held {flavor} marker must spare {}",
                file.display()
            );
        }
        drop((marker, iso_marker, fake_marker));
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

/// The Keychain-item half of the stale-runtime GC, in its pure decision: a
/// collected tree's item goes with the tree, a live session's item never
/// does, and a dir that was never built has no item to collect. The macOS
/// executor that this decision feeds (derive the service while the dir
/// exists, re-check liveness under a state-lock hold taken immediately
/// before the delete) is unreachable under `cfg(test)` (`keychain::enabled()`
/// is false there), the same split the seed and swap arms record; what every
/// platform CAN pin is the decision itself and that the sweep's own
/// filesystem outcome feeds it the right inputs — the crashed tree below is
/// collected, so the inputs the delete keys on read dead, and the live one
/// is spared, so they read live.
#[test]
fn gc_collects_a_crashed_sessions_keychain_item_and_spares_a_live_one() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let profiles = tmp.path().join(".clauth").join("profiles");

        // Crashed: a per-session pair whose marker is dead. The sweep collects
        // the tree, so the item's dir goes with it.
        let crashed_runtime = profiles.join("crashed").join("runtime-4242-0");
        let crashed_sessions = profiles.join("crashed").join("sessions-4242-0");
        fs::create_dir_all(&crashed_runtime).expect("mkdir crashed runtime");
        fs::create_dir_all(&crashed_sessions).expect("mkdir crashed sessions");
        fs::write(crashed_runtime.join(".claude.json"), b"{}").expect("seed runtime");
        fs::write(crashed_sessions.join("4242-0"), b"").expect("dead marker");

        // Live: the same shape with a flock-held marker.
        let live_runtime = profiles.join("live").join("runtime-777-3");
        let live_sessions = profiles.join("live").join("sessions-777-3");
        fs::create_dir_all(&live_runtime).expect("mkdir live runtime");
        fs::create_dir_all(&live_sessions).expect("mkdir live sessions");
        fs::write(live_runtime.join(".claude.json"), b"{}").expect("seed live runtime");
        let held = open_pid_file(&live_sessions.join("777-3")).expect("open live marker");
        held.lock().expect("lock live marker");

        // Never built: an orphaned marker dir with no runtime sibling — the
        // crash window between minting the marker dir and building the tree.
        // No tree ever hosted a session seed, so no item exists for it.
        let unbuilt_sessions = profiles.join("unbuilt").join("sessions-9999-0");
        fs::create_dir_all(&unbuilt_sessions).expect("mkdir unbuilt sessions");
        fs::write(unbuilt_sessions.join("9999-0"), b"").expect("dead unbuilt marker");

        // Derive each tree's service the way the macOS executor does, while
        // the dirs still exist (the derivation canonicalizes them).
        let crashed_service = crate::claude::namespaced_keychain_service(
            &crashed_runtime
                .canonicalize()
                .expect("canonicalize crashed"),
        );
        let live_service = crate::claude::namespaced_keychain_service(
            &live_runtime.canonicalize().expect("canonicalize live"),
        );

        gc_stale_runtimes();

        // The decision the macOS executor takes on the post-sweep state,
        // with the marker count sampled the way its re-check samples it.
        assert_eq!(
            orphaned_keychain_item(
                Some(crashed_service.as_str()),
                prune_stale_sessions(&crashed_sessions),
                crashed_runtime.symlink_metadata().is_ok()
            ),
            Some(crashed_service.as_str()),
            "a crashed session's tree was collected, so its item is collected with it"
        );
        assert_eq!(
            orphaned_keychain_item(
                Some(live_service.as_str()),
                prune_stale_sessions(&live_sessions),
                live_runtime.symlink_metadata().is_ok()
            ),
            None,
            "a live session's tree was spared, so its item never is collected"
        );
        assert!(
            !unbuilt_sessions.exists(),
            "the never-built marker dir is collected alongside"
        );
        assert_eq!(
            orphaned_keychain_item(None, Some(0), false),
            None,
            "no tree was ever built, so no service exists to collect"
        );
        drop(held);
    });
}

/// The truth table for the item-collection decision on its own: the derived
/// service survives only when nothing live explains the item — no flock-held
/// marker in the paired sessions dir, and no dir at its path. Every other row
/// keeps the item: a live or uncollectable tree keeps its dir, a live or
/// unreadable marker count reads as live, and a dir that never existed has no
/// item to collect.
#[test]
fn orphaned_keychain_item_follows_the_dir() {
    let service = Some("Claude Code-credentials-c56fc9bd");
    let dead = Some(0);
    assert_eq!(orphaned_keychain_item(service, dead, false), service);
    assert_eq!(orphaned_keychain_item(service, dead, true), None);
    assert_eq!(orphaned_keychain_item(None, dead, false), None);
    assert_eq!(orphaned_keychain_item(None, dead, true), None);
    // The #82 rows: a marker a re-minted acquire holds spares the item even
    // before the dir is rebuilt, and an unreadable sessions dir reads as
    // live, the same fail-closed fold every destructive level makes.
    assert_eq!(orphaned_keychain_item(service, Some(1), false), None);
    assert_eq!(orphaned_keychain_item(service, Some(1), true), None);
    assert_eq!(orphaned_keychain_item(service, None, false), None);
}

/// Issue #82's interleaving, posed through the seam between the sweep's
/// collection closure and its item delete: a concurrently starting session
/// re-mints the collected path while the sweep sits between the two, so its
/// lock section has completed — marker claimed and flock-held — with the
/// runtime dir not yet rebuilt, the corner where only the marker can spare
/// the item (a rebuilt dir would spare it on the stat alone). The decision is
/// evaluated on the inputs the macOS executor samples immediately before the
/// delete: a live marker must outrank dir absence, or the delete lands after
/// the re-minted acquire's seed and destroys the item it just wrote.
#[test]
fn gc_spares_the_keychain_item_a_reminted_acquire_claims_mid_sweep() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let profiles = tmp.path().join(".clauth").join("profiles");

        // A crashed pair: dead marker, tree present — what the sweep collects.
        let runtime = profiles.join("crashed").join("runtime-4242-0");
        let sessions = profiles.join("crashed").join("sessions-4242-0");
        fs::create_dir_all(&runtime).expect("mkdir runtime");
        fs::create_dir_all(&sessions).expect("mkdir sessions");
        fs::write(runtime.join(".claude.json"), b"{}").expect("seed runtime");
        fs::write(sessions.join("4242-0"), b"").expect("dead marker");

        let service = crate::claude::namespaced_keychain_service(
            &runtime.canonicalize().expect("canonicalize"),
        );

        // The re-mint, landing between the collection and the delete: the
        // acquire's lock section leaves a flock-held marker (the claim) and,
        // at this corner, no runtime dir. The fd is held past the sweep, the
        // way a live session's acquire holds it.
        let mut reminted: Option<std::fs::File> = None;
        gc_one_pair_synced(&runtime, &sessions, || {
            fs::create_dir_all(&sessions).expect("re-mint the sessions dir");
            let held = open_pid_file(&sessions.join("4242-0")).expect("open re-minted marker");
            held.lock().expect("flock-hold the re-minted marker");
            reminted = Some(held);
        })
        .expect("gc one pair");

        // The collection half ran before the interleave posed the re-mint.
        assert!(
            !runtime.exists(),
            "the tree was collected ahead of the re-mint the seam poses"
        );
        // The decision the macOS executor takes on the re-checked world,
        // sampled the way it samples under its re-take of the state lock:
        // spared — a live marker outranks dir absence.
        assert_eq!(
            orphaned_keychain_item(
                Some(service.as_str()),
                prune_stale_sessions(&sessions),
                runtime.symlink_metadata().is_ok()
            ),
            None,
            "a marker a re-minted acquire flock-holds outranks dir absence: \
             the item it is about to seed stays"
        );
        drop(reminted);
    });
}

/// The Services tab's boot probe must not collect trees: its 3 s kill budget
/// buys neither a per-pair state-flock wait nor — on macOS — the `security`
/// delete that collects a removed tree's Keychain item, and a probe that
/// removed the tree while skipping the item would strand that item
/// permanently (the service is a one-way hash of the dir; no later walk
/// explains it). The gate is cross-platform, so the pin runs everywhere:
/// under `MCP_PROBE_ENV` the tree survives the sweep, and the next real sweep
/// collects it.
#[test]
fn gc_skips_the_tree_sweep_under_the_plugin_tab_probe() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let profiles = tmp.path().join(".clauth").join("profiles");
        let runtime = profiles.join("crashed").join("runtime-4242-0");
        let sessions = profiles.join("crashed").join("sessions-4242-0");
        fs::create_dir_all(&runtime).expect("mkdir runtime");
        fs::create_dir_all(&sessions).expect("mkdir sessions");
        fs::write(runtime.join(".claude.json"), b"{}").expect("seed runtime");
        fs::write(sessions.join("4242-0"), b"").expect("dead marker");

        // The probe child's env. Set under `with_fake_home`'s HOME_TEST_LOCK
        // so the process-global mutation is serialized against every other
        // env-touching test, and restored on drop so a panicking assertion
        // cannot leak it into a sibling.
        struct ClearProbeEnv;
        impl Drop for ClearProbeEnv {
            fn drop(&mut self) {
                // SAFETY: test-only, serialized by HOME_TEST_LOCK, restored here.
                unsafe { std::env::remove_var(crate::mcp::MCP_PROBE_ENV) };
            }
        }
        // SAFETY: test-only, serialized by HOME_TEST_LOCK, restored on drop.
        unsafe { std::env::set_var(crate::mcp::MCP_PROBE_ENV, "1") };
        let _clear = ClearProbeEnv;

        gc_stale_runtimes();
        assert!(
            runtime.exists(),
            "the probe child must not collect the tree: its 3 s budget cannot pay the \
             item delete that pairs with it"
        );

        drop(_clear);
        gc_stale_runtimes();
        assert!(
            !runtime.exists(),
            "the next real sweep collects both halves"
        );
    });
}

/// The census input: every EXISTING runtime dir under `profiles/` contributes
/// its derived service, so the census spares its item; a sessions dir and an
/// unrelated name contribute nothing, since no CC config dir exists there to
/// derive a service from.
#[test]
fn live_namespaced_keychain_services_derives_every_runtime_dir() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let profiles = tmp.path().join(".clauth").join("profiles");
        let shared = profiles.join("p1").join("runtime-4242-0");
        let isolated = profiles.join("p2").join("runtime-isolated-777-3");
        let sessions = profiles.join("p1").join("sessions-4242-0");
        fs::create_dir_all(&shared).expect("mkdir shared runtime");
        fs::create_dir_all(&isolated).expect("mkdir isolated runtime");
        fs::create_dir_all(&sessions).expect("mkdir sessions");
        fs::write(profiles.join("p1").join("runtime_state.json"), b"{}").expect("unrelated file");

        let live = live_namespaced_keychain_services().expect("derive the live set");
        assert_eq!(live.len(), 2, "exactly the two runtime dirs: {live:?}");
        for dir in [&shared, &isolated] {
            let expected = crate::claude::namespaced_keychain_service(
                &dir.canonicalize().expect("canonicalize"),
            );
            assert!(
                live.contains(&expected),
                "every runtime dir explains its item: {expected}"
            );
        }
        let sessions_service = crate::claude::namespaced_keychain_service(
            &sessions.canonicalize().expect("canonicalize"),
        );
        assert!(
            !live.contains(&sessions_service),
            "a sessions dir hosts no CC config dir, so it explains no item"
        );
    });
}

/// The fail-closed half of F1: an unreadable `profiles` root (here, a FILE
/// where the dir belongs) must be an error the census reads as "cannot rule
/// out a live session", never an empty live set that deletes every item.
#[test]
fn live_namespaced_keychain_services_fails_closed_when_the_profiles_root_is_unreadable() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let clauth = tmp.path().join(".clauth");
        fs::create_dir_all(&clauth).expect("mkdir .clauth");
        fs::write(clauth.join("profiles"), b"not a dir").expect("profiles as a file");
        assert!(
            live_namespaced_keychain_services().is_err(),
            "an unreadable profiles root must fail the derivation, not read as empty"
        );
    });
}

/// The fail-closed half of F1, per profile: an unreadable profile dir (a FILE
/// under `profiles/`) must fail the derivation too — skipping it would shrink
/// the live set and let the census delete that profile's live items.
#[test]
fn live_namespaced_keychain_services_fails_closed_when_a_profile_dir_is_unreadable() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let profiles = tmp.path().join(".clauth").join("profiles");
        fs::create_dir_all(&profiles).expect("mkdir profiles");
        fs::write(profiles.join("p1"), b"not a dir").expect("profile as a file");
        assert!(
            live_namespaced_keychain_services().is_err(),
            "an unreadable profile dir must fail the derivation, not read as empty"
        );
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
            claude_pid: None,
            launch_families: None,
            started_at: 1,
            cwd: None,
            isolated: false,
            follows_chain: false,
            intended_member: None,
            chain_cursor: None,
            credit_leave: false,
            credit_leave_aware: false,
            current_member: None,
            last_swap_at: None,
            launch_store: None,
            boot_transport: None,
            is_delegate: false,
            background: false,
            background_hold: false,
            visited_members: vec![],
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

        let rt = ProfileRuntime::acquire(&profile, Isolation::Shared, &[], false, Launcher::Own)
            .expect("acquire");
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

        let rt = ProfileRuntime::acquire(&profile, Isolation::Shared, &[], false, Launcher::Own)
            .expect("acquire");

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
            ..crate::profile::OAuthToken::default_extra()
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

/// [`member`] with no refresh token: what a swap onto a refreshless store looks
/// like to `live_session_holds_rotatable`.
fn refreshless_member(name: &str) -> Profile {
    let mut profile = make_profile(name);
    profile.credentials = Some(ClaudeCredentials {
        claude_ai_oauth: Some(crate::profile::OAuthToken {
            access_token: format!("at-{name}"),
            refresh_token: None,
            expires_at: Some(1_000),
            scopes: None,
            subscription_type: None,
            ..crate::profile::OAuthToken::default_extra()
        }),
    });
    profile
}

/// A live session with NO watchdog thread behind it, so every credential leg is
/// driven explicitly: a test asserting which leg moved which bytes can never be
/// won by a background tick landing first. `acquire` is used only where the
/// launch session's teardown is part of the assertion.
///
/// It stamps and HOLDS the launch member's marker exactly as `acquire` does. A
/// fixture that skipped it would let a swap back onto the launch member claim a
/// marker no production session could, so the returned lock is part of the
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
        .expect("stamp the launch marker")
        .expect("the launch member's marker must be free in a fresh sandbox");
    let row = crate::live_sessions::LiveSession::starting(
        &session,
        name,
        crate::harness::Harness::Claude,
        isolation == Isolation::Isolated,
        false,
        Launcher::Own,
        Some(store.to_path_buf()),
        None,
    );
    crate::live_sessions::register(&row).expect("register row");
    let swap = std::sync::Arc::new(SessionSwap::new(
        session,
        isolation,
        LinkMode::Real,
        SessionTree::PerSession,
        launch,
        store,
        &paths,
    ));
    (swap, markers)
}

/// [`lone_session`] under [`LinkMode::Fake`] (a regular-file credential copy
/// instead of a symlink), so a test can drive the fake-mode watchdog tick arms.
fn fake_session(
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
        LinkMode::Fake,
    )
    .expect("session paths");
    crate::profile::mkdir_700(&paths.runtime).expect("mkdir runtime");
    fs::copy(&store, paths.runtime.join(".credentials.json")).expect("copy creds");
    let markers = stamp_swapped_markers(&paths)
        .expect("stamp the launch marker")
        .expect("the launch member's marker must be free in a fresh sandbox");
    let row = crate::live_sessions::LiveSession::starting(
        &session,
        name,
        crate::harness::Harness::Claude,
        isolation == Isolation::Isolated,
        false,
        Launcher::Own,
        Some(store.to_path_buf()),
        None,
    );
    crate::live_sessions::register(&row).expect("register row");
    let swap = std::sync::Arc::new(SessionSwap::new(
        session,
        isolation,
        LinkMode::Fake,
        SessionTree::PerSession,
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

        let opted = ProfileRuntime::acquire(&profile, Isolation::Shared, &[], true, Launcher::Own)
            .expect("acquire");
        let opted_row = crate::live_sessions::get(&live_sid(&opted)).expect("the opted-in row");
        drop(opted);

        let plain = ProfileRuntime::acquire(&profile, Isolation::Shared, &[], false, Launcher::Own)
            .expect("acquire");
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
            == LinkMode::Real;
        assert_eq!(
            opted_row.follows_chain, host_can_swap,
            "--with-fallback must reach the registry row, and must be clamped out of \
             it wherever the executor refuses every swap — a shared runtime tree on \
             a host without the symlink privilege"
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
            let rt = ProfileRuntime::acquire(&profile, Isolation::Shared, &[], true, Launcher::Own)
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
/// `Isolated` is unreachable through `acquire` from a bare test run, and both
/// arms are refusals the executor also makes at its own chokepoint.
#[test]
fn a_chain_opt_in_survives_only_where_the_executor_can_swap() {
    assert!(
        chain_opt_in_survives(true, Isolation::Shared, LinkMode::Real),
        "a shared session on a real-symlink host is the supported case"
    );
    assert!(
        !chain_opt_in_survives(false, Isolation::Shared, LinkMode::Real),
        "nothing opts a session in but the flag"
    );
    assert!(
        !chain_opt_in_survives(true, Isolation::Isolated, LinkMode::Real),
        "an isolated session follows no chain"
    );
    assert!(
        !chain_opt_in_survives(true, Isolation::Shared, LinkMode::Fake),
        "a shared runtime tree cannot hold a per-session credential"
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
                // pid 0 is never minted: `mint` stamps `<pid>-<seq>` with the live pid.
                "0-0"
            ),
            "an unstamped session id must read dead"
        );
    });
}

/// Every member in one config, each carrying a refresh token, so only the
/// live-session gate can keep it out of `rotation_candidates`.
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
                ..crate::profile::OAuthToken::default_extra()
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
fn swap_support_refuses_a_shared_tree() {
    assert_eq!(
        swap_support(LinkMode::Fake),
        Err(SwapUnsupported::SharedRuntimeTree)
    );
    assert_eq!(swap_support(LinkMode::Real), Ok(()));
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

/// A `clauth start <p> -- --bg` session releases its marker at once and keeps
/// reading its runtime tree's namespaced Keychain item, so on macOS a rotation
/// strands it exactly like a marker-held session. Only an UNFINISHED job counts
/// for the rotation gate: a stopped job nobody `claude rm`s must not freeze the
/// chain until its access token dies. "Unfinished" is read off `state`, never
/// `firstTerminalAt`: `claude attach` revives a stopped job as `running` with
/// the stamp kept (measured 2.1.304, 2026-10-20). A state this reader does not
/// know counts as unfinished, and so does a record without one.
#[test]
fn rotation_on_macos_waits_on_an_unfinished_background_job_only() {
    let cases: [(&str, Option<&str>, bool); 6] = [
        ("working", None, true),
        ("blocked", None, true),
        ("running", Some("2026-10-20T10:06:16.283Z"), true),
        ("stopped", Some("2026-10-20T10:06:16.283Z"), false),
        ("done", Some("2026-10-20T10:06:16.283Z"), false),
        ("failed", Some("2026-10-20T10:06:16.283Z"), false),
    ];
    for (state, stamp, blocked) in cases {
        let tmp = tempfile::tempdir().expect("tempdir");
        with_fake_home(tmp.path(), || {
            let name = crate::profile::ProfileName::from("rotjob");
            let tree = tmp.path().join(".clauth/profiles/rotjob/runtime-4242-0");
            fs::create_dir_all(&tree).expect("mkdir runtime");
            write_claude_job_in(tmp.path(), "3fde53e3", &tree, state, stamp);
            assert_eq!(
                rotation_blocked_on(&name, true),
                blocked,
                "a job in state {state:?} decides the macOS rotation gate"
            );
            assert!(
                !rotation_blocked_on(&name, false),
                "off macOS the session shares the rotated file, whatever its job's state"
            );
        });
    }
}

/// The fail-closed arms of the job term: a state this reader has never seen, a
/// record with no `state` key, and a job dir whose record is not there yet (the
/// mid-create shape) all block, since a false "finished" signs a session out.
#[test]
fn rotation_on_macos_reads_an_unknown_or_missing_job_state_as_unfinished() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let name = crate::profile::ProfileName::from("rotjob");
        let tree = tmp.path().join(".clauth/profiles/rotjob/runtime-4242-0");
        fs::create_dir_all(&tree).expect("mkdir runtime");

        let job = write_claude_job_in(tmp.path(), "3fde53e3", &tree, "hibernating", None);
        assert!(
            rotation_blocked_on(&name, true),
            "an unknown state is unfinished"
        );

        let record = job.join("state.json");
        let mut value: serde_json::Value =
            serde_json::from_slice(&fs::read(&record).expect("read job")).expect("parse job");
        value.as_object_mut().expect("object").remove("state");
        fs::write(&record, serde_json::to_vec(&value).expect("encode")).expect("rewrite job");
        assert!(
            rotation_blocked_on(&name, true),
            "a record without a state is unfinished"
        );

        fs::remove_file(&record).expect("drop record");
        fs::remove_dir(job.join("tmp")).expect("drop tmp");
        assert!(
            rotation_blocked_on(&name, true),
            "a job dir without its record yet is unreadable, and unreadable blocks"
        );

        // A removed job's leftover (cloudy 2026-10-22): its supervisor's empty
        // `tmp/` plus `exit-cause` is no job, so it blocks nothing.
        fs::create_dir(job.join("tmp")).expect("leftover tmp");
        fs::write(job.join("exit-cause"), b"SIGTERM").expect("leftover exit-cause");
        assert!(
            !rotation_blocked_on(&name, true),
            "a removed job's leftover dir is no unfinished job"
        );

        fs::remove_dir_all(&job).expect("claude rm");
        assert!(
            !rotation_blocked_on(&name, true),
            "with the job removed and no marker, nothing blocks"
        );
    });
}

/// The rotation narrowing never loosens the destructive gates: a stopped job
/// still makes the profile live for delete, rename and disable, and its tree
/// still survives GC, because a stopped job can be revived and read the tree
/// again (`claude attach`).
#[test]
fn a_stopped_job_frees_rotation_but_keeps_the_profile_live() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let name = crate::profile::ProfileName::from("rotjob");
        let profiles = tmp.path().join(".clauth").join("profiles");
        let (tree, sessions) = dead_pair(&profiles, "rotjob", "runtime-4242-0", "4242-0");
        write_claude_job_in(
            tmp.path(),
            "3fde53e3",
            &tree,
            "stopped",
            Some("2026-10-20T10:06:16.283Z"),
        );

        assert!(
            !rotation_blocked_on(&name, true),
            "a stopped job frees rotation"
        );
        assert!(
            has_live_session(&name),
            "a stopped job still makes the profile live"
        );
        gc_stale_runtimes();
        assert!(
            tree.join(".claude.json").is_file() && sessions.is_dir(),
            "a stopped job's tree survives the sweep"
        );
    });
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
    assert_eq!(swap_eligible(&twin, &transport, ModelsRule::Equal), Ok(()));

    let mut endpoint = twin.clone();
    endpoint.base_url = Some("https://api.example/anthropic".into());
    assert_eq!(
        swap_eligible(&endpoint, &transport, ModelsRule::Equal),
        Err(SwapRefused::NotOauth)
    );

    let mut disabled = twin.clone();
    disabled.disabled = true;
    assert_eq!(
        swap_eligible(&disabled, &transport, ModelsRule::Equal),
        Err(SwapRefused::Disabled)
    );

    let mut env = twin.clone();
    env.env.insert("EXTRA".into(), "2".into());
    assert_eq!(
        swap_eligible(&env, &transport, ModelsRule::Equal),
        Err(SwapRefused::EnvDiffers)
    );

    let mut models = twin.clone();
    models.models.default = Some("opus".into());
    assert_eq!(
        swap_eligible(&models, &transport, ModelsRule::Equal),
        Err(SwapRefused::ModelsDiffers)
    );

    let mut keyed = twin.clone();
    keyed.api_key = Some("k".into());
    assert_eq!(
        swap_eligible(&keyed, &transport, ModelsRule::Equal),
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
        swap_eligible(&endpoint_and_disabled, &transport, ModelsRule::Equal),
        Err(SwapRefused::NotOauth),
        "the endpoint must be reported ahead of the disabled bit"
    );

    // A session launched on an api-key member: the same-state candidate clears
    // and the keyless one is the one refused, so the compare cannot degrade into
    // "the candidate has no key".
    let mut keyed_launch = launch.clone();
    keyed_launch.api_key = Some("k".into());
    let keyed_transport = LaunchTransport::of(&keyed_launch);
    assert_eq!(
        swap_eligible(&keyed, &keyed_transport, ModelsRule::Equal),
        Ok(())
    );
    assert_eq!(
        swap_eligible(&twin, &keyed_transport, ModelsRule::Equal),
        Err(SwapRefused::ApiKeyDiffers)
    );
}

/// `settings.json` env reaches Claude Code's `process.env` only at STARTUP,
/// while `ANTHROPIC_AUTH_TOKEN` is read live per client construction, so a
/// member carrying different env or model routing is a genuinely different
/// transport rather than the same account elsewhere.
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

/// The live-session row records the transport the session booted with: the
/// one built from the on-disk profile the swap snapshots, so the daemon's leg
/// and the executor judge members against one value. An `[env]` entry can
/// carry an inference credential, so the row holds no `[env]` value at all.
#[test]
fn acquire_records_the_boot_transport_without_any_env_value() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        fake_claude_home(tmp.path());
        let mut profile = make_profile("boot-env");
        profile
            .env
            .insert("ANTHROPIC_AUTH_TOKEN".into(), "sk-boot-secret-value".into());
        profile
            .env
            .insert("OTHER_SETTING".into(), "plain-boot-value".into());
        register_profile(&profile);

        let rt = ProfileRuntime::acquire(&profile, Isolation::Shared, &[], false, Launcher::Own)
            .expect("acquire");

        let row = crate::live_sessions::get(rt.session_id()).expect("the session's row");
        let on_disk = crate::profile::load_profile(&profile.name).expect("load the profile");
        assert_eq!(row.boot_transport, Some(LaunchTransport::of(&on_disk)));

        let raw = fs::read_to_string(
            tmp.path()
                .join(".clauth")
                .join("live_sessions")
                .join(format!("{}.json", rt.session_id())),
        )
        .expect("read the row file");
        for value in ["sk-boot-secret-value", "plain-boot-value"] {
            assert!(
                !raw.contains(value),
                "the row must not hold `{value}`:\n{raw}"
            );
        }
        drop(rt);
    });
}

/// Two transports are equal exactly when their `[env]` maps are: a pair split
/// at a different `=` is a different env, and a reordered insert is the same.
/// The executor refuses `EnvDiffers` on this equality, so a collision here is
/// a swap into a session whose child never received the target's env.
#[test]
fn launch_transports_differ_exactly_when_their_env_maps_do() {
    let with_env = |pairs: &[(&str, &str)]| {
        let mut p = make_profile("env-eq");
        for (k, v) in pairs {
            p.env.insert((*k).into(), (*v).into());
        }
        LaunchTransport::of(&p)
    };
    assert_eq!(
        with_env(&[("A", "1"), ("B", "2")]),
        with_env(&[("B", "2"), ("A", "1")])
    );
    assert_ne!(with_env(&[("A", "b=c")]), with_env(&[("A=b", "c")]));
    assert_ne!(with_env(&[("AB", "")]), with_env(&[("A", "B")]));
    assert_ne!(with_env(&[("A", "1"), ("B", "")]), with_env(&[("A", "1")]));
    assert_ne!(with_env(&[("A", "1")]), with_env(&[]));
    // A changed VALUE under the same key is a different env: a rotated
    // `ANTHROPIC_AUTH_TOKEN` or a moved `ANTHROPIC_BASE_URL` never reaches a
    // running child.
    assert_ne!(with_env(&[("A", "1")]), with_env(&[("A", "2")]));
    assert_ne!(with_env(&[("A", "12")]), with_env(&[("A", "21")]));
}

/// The transport is stored on the live-session row, and a row outlives a
/// binary upgrade: a daemon on a newer build judges a session an older build
/// started. So its serialized form is a format. The digests below were
/// computed outside the crate (Python `hashlib.sha256` over the pair count,
/// then each key and value, each as a little-endian `u64` byte length followed
/// by its UTF-8 bytes, in sorted key order). A digest change makes every
/// running session's row mismatch every member until it restarts; a shape
/// change makes those rows unreadable, so they drop out of every row reader.
#[test]
fn a_launch_transport_serializes_to_a_stable_row_format() {
    let empty = make_profile("fmt-empty");
    assert_eq!(
        serde_json::to_value(LaunchTransport::of(&empty)).expect("serialize"),
        serde_json::json!({
            "env_digest": "af5570f5a1810b7af78caf4bc70a660f0df51e42baf91d4de5b2328de0e83dfc",
            "models": {},
            "has_api_key": false,
        })
    );

    let mut routed = make_profile("fmt-routed");
    routed.env.insert(
        "ANTHROPIC_BASE_URL".into(),
        "https://api.example/anthropic".into(),
    );
    routed.env.insert("Z".into(), "ü".into());
    routed.api_key = Some("k".into());
    let value = serde_json::to_value(LaunchTransport::of(&routed)).expect("serialize");
    assert_eq!(
        value["env_digest"],
        "0a8287ebfd17c95b416cf2bd16f3bbcdc43fd5ccd6e6d4e8b0321d6014373929"
    );
    assert_eq!(value["has_api_key"], true);
    let back: LaunchTransport = serde_json::from_value(value).expect("round-trip");
    assert_eq!(back, LaunchTransport::of(&routed));

    // Every `ModelSettings` field is a `settings.json` key a session boots with,
    // so a renamed serde key would read `None` on every pre-upgrade row and flip
    // the leg to `ModelsDiffers`. Pin every key literal and the round-trip.
    let mut modeled = make_profile("fmt-models");
    modeled.models = crate::profile::ModelSettings {
        default: Some("model-default".into()),
        opus: Some("model-opus".into()),
        sonnet: Some("model-sonnet".into()),
        haiku: Some("model-haiku".into()),
        fable: Some("model-fable".into()),
        subagent: Some("model-subagent".into()),
    };
    let value = serde_json::to_value(LaunchTransport::of(&modeled)).expect("serialize");
    assert_eq!(
        value["models"],
        serde_json::json!({
            "default": "model-default",
            "opus": "model-opus",
            "sonnet": "model-sonnet",
            "haiku": "model-haiku",
            "fable": "model-fable",
            "subagent": "model-subagent",
        })
    );
    let back: LaunchTransport = serde_json::from_value(value).expect("round-trip");
    assert_eq!(back, LaunchTransport::of(&modeled));
}

/// A `credentials.json` that parses but holds no login installs an empty login
/// into the session, signing it out. The swap's precondition re-reads the
/// intended member's stored login lock-free and refuses the proven-none state.
#[test]
fn the_precondition_refuses_a_credentials_store_holding_no_login() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let launch = member("pc-launch");
        member_store(&launch);
        let (swap, _launch_markers) = lone_session(&launch, Isolation::Shared);

        for body in ["{}", "{\"mcpOauth\": {\"accessToken\": \"x\"}}"] {
            let gamma = member("pc-empty");
            let store = member_store(&gamma);
            std::fs::write(&store, body).expect("write a login-less credentials store");

            assert_eq!(
                swap.precondition("pc-empty").map(|plan| plan.member),
                Err(SwapRefused::NoCredentialStore),
                "a store that parses with no login is refused, never installed"
            );
        }
    });
}

/// A torn `credentials.json` reads [`crate::profile::StoredLogin::Unknown`],
/// so the re-read — which runs BEFORE the profile load — refuses it as
/// [`SwapRefused::LoginUnreadable`] and no parse-error text reaches the line.
/// A readable login beside a torn `config.toml` still fails the load: see
/// `the_precondition_refuses_a_pair_member_whose_config_is_torn_as_unreadable_profile`.
#[test]
fn the_precondition_refuses_a_torn_store() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let launch = member("pt-launch");
        member_store(&launch);
        let (swap, _launch_markers) = lone_session(&launch, Isolation::Shared);

        let gamma = member("pt-torn");
        let store = member_store(&gamma);
        std::fs::write(&store, "{\"claudeAiOauth\": {\"accessToken\": ")
            .expect("tear the intended member's store");

        assert_eq!(
            swap.precondition("pt-torn").map(|plan| plan.member),
            Err(SwapRefused::LoginUnreadable),
            "a torn credentials store is refused as an unreadable login"
        );
    });
}

/// A key member (api key in `config.toml`, no OAuth pair, no sidecar) whose
/// `config.toml` is torn reads [`crate::profile::StoredLogin::Unknown`], so the
/// re-read refuses it as [`SwapRefused::LoginUnreadable`] before `load_profile`
/// can put the parse error's bytes on the line.
#[test]
fn the_precondition_refuses_a_key_member_whose_config_is_torn() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let launch = member("pk-launch");
        member_store(&launch);
        let (swap, _launch_markers) = lone_session(&launch, Isolation::Shared);

        let mut keyed = make_profile("pk-key");
        keyed.api_key = Some("k".into());
        member_store(&keyed);
        let config = crate::profile::profile_dir(&crate::profile::ProfileName::from("pk-key"))
            .expect("profile dir")
            .join("config.toml");
        std::fs::write(&config, "api_key = \"sk-").expect("tear the key member's config.toml");

        assert_eq!(
            swap.precondition("pk-key").map(|plan| plan.member),
            Err(SwapRefused::LoginUnreadable),
            "a key member whose config.toml is torn is refused as an unreadable login"
        );
    });
}

/// A pair member whose `credentials.json` reads fine but whose `config.toml` is
/// torn reads [`crate::profile::StoredLogin::HoldsLogin`], so the re-read passes
/// it through to `load_profile`, which refuses it as
/// [`SwapRefused::ProfileUnreadable`] — the re-read must not swallow a genuine
/// load failure past a readable login.
#[test]
fn the_precondition_refuses_a_pair_member_whose_config_is_torn_as_unreadable_profile() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let launch = member("pp-launch");
        member_store(&launch);
        let (swap, _launch_markers) = lone_session(&launch, Isolation::Shared);

        let gamma = member("pp-torn-config");
        member_store(&gamma);
        let config =
            crate::profile::profile_dir(&crate::profile::ProfileName::from("pp-torn-config"))
                .expect("profile dir")
                .join("config.toml");
        std::fs::write(&config, "base_url = \"").expect("tear the pair member's config.toml");

        assert!(
            matches!(
                swap.precondition("pp-torn-config"),
                Err(SwapRefused::ProfileUnreadable(_))
            ),
            "a readable login beside a torn config.toml is refused as an unreadable profile"
        );
    });
}

/// The shared target check is a read-only question: a member holding a legacy
/// `credentials.json.pending` sidecar is checked, and neither `credentials.json`
/// nor the sidecar changes — the check must not adopt the sidecar the way
/// `load_profile`'s recovery does. Modelled on
/// `pending_sidecar_newer_than_the_commit_is_adopted_and_written_through`.
#[test]
fn the_shared_target_check_does_not_adopt_a_legacy_pending_sidecar() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let launch = member("scp-launch");
        member_store(&launch);
        let launch = crate::runtime::LaunchTransport::of(&launch);

        let gamma = member("scp-pending");
        let store = member_store(&gamma);
        let pending =
            crate::profile::profile_dir(&crate::profile::ProfileName::from("scp-pending"))
                .expect("profile dir")
                .join("credentials.json.pending");
        std::fs::write(
            &pending,
            "{\"claudeAiOauth\":{\"accessToken\":\"at-pending\",\"refreshToken\":\"rt-pending\"}}",
        )
        .expect("write a legacy pending sidecar");
        // Newer than the committed store, so a full load would adopt it.
        let now = std::time::SystemTime::now();
        crate::testutil::set_mtime(&store, now - std::time::Duration::from_secs(60));
        crate::testutil::set_mtime(&pending, now);
        let before_store = std::fs::read(&store).expect("read store before");
        let before_pending = std::fs::read(&pending).expect("read pending before");

        assert!(
            crate::runtime::swap_target_precondition(
                &crate::profile::ProfileName::from("scp-pending"),
                &launch,
                crate::runtime::ModelsRule::Equal,
            )
            .is_ok(),
            "a pair member with a legacy sidecar passes the shared check"
        );
        assert_eq!(
            std::fs::read(&store).expect("read store after"),
            before_store,
            "the shared check must not write credentials.json"
        );
        assert_eq!(
            std::fs::read(&pending).expect("read pending after"),
            before_pending,
            "the shared check must not consume or rewrite the pending sidecar"
        );
    });
}

/// The shared target check resolves an adoptable legacy `.pending` exactly as a
/// full load would: a member with NO committed `credentials.json`, a long-lived
/// sidecar login, a leftover `base_url` and an adoptable `.pending` passes the
/// check — `read_profile` adopts the sidecar in memory, so the pair wins over
/// the leftover endpoint — while nothing on disk changes. Modelled on
/// `pending_sidecar_is_adopted_when_no_commit_exists_at_all`.
#[test]
fn the_shared_target_check_resolves_an_adoptable_pending_sidecar_like_a_full_load() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let launch = member("r10p-launch");
        member_store(&launch);
        let launch = crate::runtime::LaunchTransport::of(&launch);

        let name = crate::profile::ProfileName::from("r10p-pend-baseurl");
        // No committed credentials: `save_profile` with `credentials: None`
        // writes config.toml only, so the sidecar below is the only pair.
        let mut gamma = make_profile("r10p-pend-baseurl");
        gamma.base_url = Some("https://api.example/anthropic".into());
        crate::profile::save_profile(&gamma).expect("save profile");
        crate::testutil::write_long_lived_sidecar(&name);
        let pending = crate::profile::profile_dir(&name)
            .expect("profile dir")
            .join("credentials.json.pending");
        std::fs::write(
            &pending,
            "{\"claudeAiOauth\":{\"accessToken\":\"at-pending\",\"refreshToken\":\"rt-pending\"}}",
        )
        .expect("write a legacy pending sidecar");
        let before_pending = std::fs::read(&pending).expect("read pending before");
        let store = crate::claude::install_source_path(&name).expect("install source");
        let before_store = std::fs::read(&store).expect("read store before");
        let cred_path = crate::profile::profile_dir(&name)
            .expect("profile dir")
            .join("credentials.json");

        assert!(
            crate::runtime::swap_target_precondition(
                &name,
                &launch,
                crate::runtime::ModelsRule::Equal
            )
            .is_ok(),
            "the shared check resolves the adoptable sidecar exactly as a full load would"
        );
        assert_eq!(
            std::fs::read(&pending).expect("read pending after"),
            before_pending,
            "the shared check must not consume or rewrite the pending sidecar"
        );
        assert_eq!(
            std::fs::read(&store).expect("read store after"),
            before_store,
            "the shared check must not write the install source"
        );
        assert!(
            !cred_path.exists(),
            "the resolve is in memory only: no credentials.json is written through"
        );
    });
}

/// The shared target check must not rewrite `config.toml`, and must not land a
/// legacy `.pending` sidecar either: it reads through `read_profile`, which
/// leaves both for the next ordinary load. The `[env]` key `A.B` keeps the
/// check on its `Ok` path (a launch carrying the same env), so the read reaches
/// past the transport gate. The fixture is asserted LIVE: a full load on a
/// sibling copy still writes — it lands the sidecar as `credentials.json` — so
/// the pin stays armed. Residual: no valid `config.toml` makes a full load
/// rewrite it anymore (the render round-trips), so the control observes the
/// sidecar write-through, never a config rewrite.
#[test]
fn the_shared_target_check_does_not_rewrite_config_toml() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let mut launch = member("scc-launch");
        launch.env.insert("A.B".into(), "x".into());
        member_store(&launch);
        let launch = crate::runtime::LaunchTransport::of(&launch);

        let seed = |name: &str| {
            crate::profile::save_profile(&member(name)).expect("save member");
            let dir = crate::profile::profile_dir(&crate::profile::ProfileName::from(name))
                .expect("profile dir");
            let config = dir.join("config.toml");
            std::fs::write(&config, "[env]\n\"A.B\" = \"x\"\n")
                .expect("write the launch-matching env config");
            let creds = dir.join("credentials.json");
            // An adoptable legacy `.pending` sidecar, written after
            // `credentials.json` so its mtime is not older: an ordinary load
            // lands it under the flock.
            let pending = dir.join("credentials.json.pending");
            std::fs::write(
                &pending,
                "{\"claudeAiOauth\":{\"accessToken\":\"new-access\",\"refreshToken\":\"new-refresh\"}}",
            )
            .expect("write the legacy sidecar");
            (config, creds, pending)
        };

        // The fixture is live: a full load lands the sidecar, rewriting
        // `credentials.json`.
        let (_, live_creds, live_pending) = seed("scc-rewrite-live");
        let live_creds_before = std::fs::read(&live_creds).expect("read live credentials before");
        crate::profile::load_profile(&crate::profile::ProfileName::from("scc-rewrite-live"))
            .expect("load live profile");
        assert_ne!(
            std::fs::read(&live_creds).expect("read live credentials after"),
            live_creds_before,
            "fixture liveness: a full load lands the sidecar"
        );
        assert!(
            !live_pending.exists(),
            "fixture liveness: a full load consumes the sidecar"
        );

        let (config, creds, pending) = seed("scc-rewrite");
        let before = std::fs::read(&config).expect("read config before");
        let creds_before = std::fs::read(&creds).expect("read credentials before");
        assert!(
            crate::runtime::swap_target_precondition(
                &crate::profile::ProfileName::from("scc-rewrite"),
                &launch,
                crate::runtime::ModelsRule::Equal,
            )
            .is_ok(),
            "the check runs on its Ok path: the launch carries the same env"
        );
        assert_eq!(
            std::fs::read(&config).expect("read config after"),
            before,
            "the shared check must not rewrite config.toml"
        );
        assert_eq!(
            std::fs::read(&creds).expect("read credentials after"),
            creds_before,
            "the shared check must not land the sidecar"
        );
        assert!(
            pending.exists(),
            "the shared check must not consume the sidecar"
        );
    });
}

/// The `LoginUnreadable` refusal renders the approved copy and composes into the
/// session's stay line exactly: the intended member is blamed, and the reason is
/// the store's unreadability rather than a profile load failure.
#[test]
fn the_login_unreadable_refusal_composes_the_approved_line() {
    assert_eq!(
        refusal_line("3f2a", "alpha", "gamma", &SwapRefused::LoginUnreadable),
        "clauth: session 3f2a stays on alpha: gamma is not swappable (its stored login could not be read)",
        "the exact approved copy: an unreadable stored login, not a profile load failure"
    );
}

/// A sidecar-only member whose `session-token.json` is torn is an UNREADABLE
/// login, never a proven absence: `install_source_path` falls back to the absent
/// `credentials.json`, so the existence check alone would read `NoCredentialStore`.
/// The re-read runs BEFORE that check and refuses `LoginUnreadable`.
#[test]
fn the_precondition_refuses_a_sidecar_only_member_whose_sidecar_is_torn() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let launch = member("pst-launch");
        member_store(&launch);
        let (swap, _launch_markers) = lone_session(&launch, Isolation::Shared);

        let gamma = make_profile("pst-side");
        register_profile(&gamma);
        crate::testutil::write_long_lived_sidecar(&crate::profile::ProfileName::from("pst-side"));
        let sidecar = crate::profile::profile_dir(&crate::profile::ProfileName::from("pst-side"))
            .expect("profile dir")
            .join("session-token.json");
        std::fs::write(&sidecar, "{\"claudeAiOauth\": {\"accessToken\": ")
            .expect("tear the sidecar-only member's sidecar");

        assert_eq!(
            swap.precondition("pst-side").map(|plan| plan.member),
            Err(SwapRefused::LoginUnreadable),
            "a torn sidecar is an unreadable login, not a proven absence"
        );
    });
}

/// The pin the round-6 review asked for (Q1): `credentials.json` = `{}` beside a
/// torn `session-token.json` loads fine, so `load_profile` cannot refuse it, and
/// only the login re-read stands between it and installing an empty login.
#[test]
fn the_precondition_refuses_an_empty_store_beside_a_torn_sidecar() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let launch = member("pes-launch");
        member_store(&launch);
        let (swap, _launch_markers) = lone_session(&launch, Isolation::Shared);

        let gamma = member("pes-empty-side");
        let store = member_store(&gamma);
        std::fs::write(&store, "{}").expect("write an empty credentials store");
        crate::testutil::write_long_lived_sidecar(&crate::profile::ProfileName::from(
            "pes-empty-side",
        ));
        let sidecar =
            crate::profile::profile_dir(&crate::profile::ProfileName::from("pes-empty-side"))
                .expect("profile dir")
                .join("session-token.json");
        std::fs::write(&sidecar, "{\"claudeAiOauth\": {\"accessToken\": ")
            .expect("tear the sidecar");

        assert_eq!(
            swap.precondition("pes-empty-side").map(|plan| plan.member),
            Err(SwapRefused::LoginUnreadable),
            "an empty store beside a torn sidecar is refused as an unreadable login"
        );
    });
}

/// A swap stamps and holds ONE marker on the swapped-onto member, its per-session
/// `sessions-<sid>/<sid>`, so `has_live_session` sees that member live for the
/// session's life and nothing lands at the bare-stem `sessions/<sid>`.
#[test]
fn a_swap_holds_the_intended_members_per_session_marker() {
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
        let marker = profile_dir.join(format!("sessions-{sid}")).join(&sid);
        assert!(marker.is_file(), "no marker at {}", marker.display());
        assert!(
            is_session_alive(&marker),
            "{} must be flock-held for the session's life",
            marker.display()
        );
        let bare = profile_dir.join("sessions");
        assert!(
            !bare.exists(),
            "a swap stamped a second marker dir at the pre-per-session path {}",
            bare.display()
        );
        assert!(
            has_live_session(&crate::profile::ProfileName::from("marker-b")),
            "the rotation gate must see the swapped-onto member as live"
        );
    });
}

/// A member whose marker this session cannot hold is a member the rotation gate
/// cannot see it on, so the swap refuses INSIDE the hold rather than repointing
/// the link at a chain nothing is protecting.
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
/// guards read, and on macOS what `rotation_blocked_for` reads; it never
/// removes a member from `rotation_candidates`, so both stay candidates
/// throughout. A swapped session follows whichever pair clauth writes.
#[test]
fn a_swap_keeps_both_members_marked_live_and_both_rotation_candidates() {
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

        let rt = ProfileRuntime::acquire(&launch, Isolation::Shared, &[], false, Launcher::Own)
            .expect("acquire");
        let sid = live_sid(&rt);
        assert_eq!(
            rt.swap().swap_to("rot-b").expect("swap"),
            SwapOutcome::Swapped
        );

        let launch_marker =
            crate::profile::profile_dir(&crate::profile::ProfileName::from("rot-a"))
                .expect("profile dir")
                .join(format!("sessions-{sid}"))
                .join(&sid);
        assert!(
            is_session_alive(&launch_marker),
            "{} must survive the swap — the live child still holds that chain",
            launch_marker.display()
        );

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
            "a live marker never removes a rotation candidate — both members stay candidates"
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

fn armed_durable_stamp(name: &str) -> crate::profile::ProfileName {
    let pname = crate::profile::ProfileName::from(name);
    crate::usage::seed_login_anchor(
        &pname,
        Some(&crate::profile::AccountId::from("uuid-a".to_string())),
    );
    assert!(
        crate::usage::take_profile_fetch(&pname, false, 1_000_000_000_000u64),
        "precondition: the durable /profile stamp is armed"
    );
    assert!(
        crate::profile_cache::load_profile_cache::<u64>(
            &pname,
            crate::profile_cache::PROFILE_FETCHED_CACHE_FILE
        )
        .is_some(),
        "precondition: the durable stamp exists"
    );
    pname
}

fn durable_stamp(name: &crate::profile::ProfileName) -> Option<u64> {
    crate::profile_cache::load_profile_cache::<u64>(
        name,
        crate::profile_cache::PROFILE_FETCHED_CACHE_FILE,
    )
}

/// A watchdog tick drain that copied a session-side `/login` into the store
/// marks that account's `/profile` reading stale (removes the durable stamp) —
/// no network call.
#[test]
fn a_tick_drain_marks_the_profile_reading_stale() {
    let home = HomeSandbox::new();
    let claude_home = fake_claude_home(home.home());
    let launch = member("tick-stale");
    let store = member_store(&launch);
    let pname = armed_durable_stamp("tick-stale");
    let (swap, _markers) = lone_session(&launch, Isolation::Shared);
    set_mtime(&store, SystemTime::now() - Duration::from_secs(60));
    cc_relogin(&swap.runtime, CREDS_V2, SystemTime::now());

    tick(&claude_home, &swap).expect("tick");

    assert_eq!(
        fs::read(&store).expect("read store"),
        CREDS_V2,
        "precondition: the tick drained the session's login"
    );
    assert_eq!(
        durable_stamp(&pname),
        None,
        "a tick drain that wrote the store marks the profile's /profile reading stale"
    );
}

/// The swap executor's drain (swap_to) marks the OUTGOING member's `/profile`
/// reading stale after it wrote the drained login.
#[test]
fn a_swap_drain_marks_the_outgoing_profile_reading_stale() {
    let _home = HomeSandbox::new();
    let launch = member("sw-stale-a");
    let intended = member("sw-stale-b");
    let launch_store = member_store(&launch);
    let _intended_store = member_store(&intended);
    let pname = armed_durable_stamp("sw-stale-a");
    let (swap, _markers) = lone_session(&launch, Isolation::Shared);
    set_mtime(&launch_store, SystemTime::now() - Duration::from_secs(60));
    cc_relogin(&swap.runtime, CREDS_V2, SystemTime::now());

    assert_eq!(
        swap.swap_to("sw-stale-b").expect("swap"),
        SwapOutcome::Swapped
    );

    assert_eq!(
        fs::read(&launch_store).expect("read launch store"),
        CREDS_V2,
        "precondition: the swap drained into the outgoing member"
    );
    assert_eq!(
        durable_stamp(&pname),
        None,
        "the swap's drain marks the outgoing profile's /profile reading stale"
    );
}

/// A converge (the poll onto an armed rolling sidecar, same member) that drained
/// a session-side login marks the member's `/profile` reading stale.
#[test]
fn a_converge_drain_marks_the_profile_reading_stale() {
    let _home = HomeSandbox::new();
    let launch = member("conv-stale");
    let store = member_store(&launch);
    let pname = armed_durable_stamp("conv-stale");
    let (swap, _markers) = lone_session(&launch, Isolation::Shared);
    set_mtime(&store, SystemTime::now() - Duration::from_secs(60));
    cc_relogin(&swap.runtime, CREDS_V2, SystemTime::now());
    // Arm the sidecar so the converge transition holds (current refreshable,
    // selected refreshless).
    let sidecar = crate::profile::profile_dir(&pname)
        .expect("profile_dir")
        .join("session-token.json");
    write_creds(&sidecar, None);

    swap.poll();

    assert_eq!(
        fs::read(&store).expect("read store"),
        CREDS_V2,
        "precondition: the converge drained the session's login"
    );
    assert_eq!(
        durable_stamp(&pname),
        None,
        "the converge's drain marks the profile's /profile reading stale"
    );
}

/// The build-time drain (`reconcile_credentials`) marks the profile's `/profile`
/// reading stale after a session→store copy, and leaves it untouched when the
/// copy wrote nothing.
#[test]
fn a_build_drain_marks_the_profile_reading_stale() {
    let _home = HomeSandbox::new();
    let launch = member("build-stale");
    let store = member_store(&launch);
    let pname = armed_durable_stamp("build-stale");
    let runtime = crate::profile::profile_subpath(&pname, "runtime").expect("runtime path");
    fs::create_dir_all(&runtime).expect("mkdir runtime");
    let runtime_creds = runtime.join(".credentials.json");
    fs::write(&runtime_creds, CREDS_V2).expect("session relogin");
    set_mtime(&store, SystemTime::now() - Duration::from_secs(60));
    set_mtime(&runtime_creds, SystemTime::now());

    reconcile_credentials(&runtime_creds, &store, LinkMode::Fake).expect("reconcile");

    assert_eq!(
        fs::read(&store).expect("read store"),
        CREDS_V2,
        "precondition: the build drained the newer runtime copy"
    );
    assert_eq!(
        durable_stamp(&pname),
        None,
        "the build drain marks the profile's /profile reading stale"
    );

    // A second reconcile over equal bytes writes nothing and must not re-mark.
    crate::usage::take_profile_fetch(&pname, false, 2_000_000_000_000u64);
    assert!(
        durable_stamp(&pname).is_some(),
        "precondition: the stamp is re-armed"
    );
    reconcile_credentials(&runtime_creds, &store, LinkMode::Fake).expect("reconcile");
    assert!(
        durable_stamp(&pname).is_some(),
        "a drain that wrote nothing leaves the durable stamp untouched"
    );
}

/// A copy-back whose bytes differ only in a non-login key (Claude Code rewriting
/// the file for an MCP-server token refresh) must NOT mark the `/profile`
/// reading stale — the mark fires only when the copied login changed.
#[test]
fn a_non_login_rewrite_does_not_mark_the_reading_stale() {
    let home = HomeSandbox::new();
    let claude_home = fake_claude_home(home.home());
    let launch = member("mcpprobe");
    let store = member_store(&launch);
    let pname = armed_durable_stamp("mcpprobe");
    let (swap, _markers) = lone_session(&launch, Isolation::Shared);
    let mut v: serde_json::Value =
        serde_json::from_slice(&fs::read(&store).expect("read store")).expect("store json");
    v["mcpOAuth"] = serde_json::json!({});
    v["mcpOAuth"]["srv|https://mcp.example"]["accessToken"] = "mcp-2".into();
    set_mtime(&store, SystemTime::now() - Duration::from_secs(60));
    cc_relogin(
        &swap.runtime,
        &serde_json::to_vec(&v).expect("ser"),
        SystemTime::now(),
    );
    tick(&claude_home, &swap).expect("tick");
    let after: ClaudeCredentials =
        serde_json::from_slice(&fs::read(&store).expect("read store")).expect("parse store");
    assert_eq!(
        after.claude_ai_oauth.map(|o| o.access_token).as_deref(),
        Some("at-mcpprobe"),
        "precondition: the copy-back kept the same login"
    );
    assert_eq!(
        durable_stamp(&pname),
        Some(1_000_000_000_000),
        "a copy-back that refreshed no token must not spend a /profile read"
    );
}

/// The fake-transport login-change gate (`copy_if_valid_creds`): P4's shape
/// through a fake-mode session (Windows' default), so an MCP-only rewrite marks
/// nothing.
#[test]
fn a_fake_mode_non_login_rewrite_marks_nothing() {
    let home = HomeSandbox::new();
    let claude_home = fake_claude_home(home.home());
    let launch = member("mcpprobef");
    let store = member_store(&launch);
    let pname = armed_durable_stamp("mcpprobef");
    let (swap, _markers) = fake_session(&launch, Isolation::Shared);
    let mut v: serde_json::Value =
        serde_json::from_slice(&fs::read(&store).expect("read store")).expect("store json");
    v["mcpOAuth"] = serde_json::json!({});
    v["mcpOAuth"]["srv|https://mcp.example"]["accessToken"] = "mcp-2".into();
    set_mtime(&store, SystemTime::now() - Duration::from_secs(60));
    cc_relogin(
        &swap.runtime,
        &serde_json::to_vec(&v).expect("ser"),
        SystemTime::now(),
    );
    tick(&claude_home, &swap).expect("tick");
    let after: serde_json::Value =
        serde_json::from_slice(&fs::read(&store).expect("read store")).expect("parse store");
    assert_eq!(
        after["mcpOAuth"]["srv|https://mcp.example"]["accessToken"], "mcp-2",
        "precondition: the fake copy-back landed the MCP refresh"
    );
    assert_eq!(
        durable_stamp(&pname),
        Some(1_000_000_000_000),
        "a fake-mode copy-back that moved no login must not mark"
    );
}

/// The fake-mode (shared) watchdog tick arm marks the `/profile` reading stale
/// after a session→store copy of a different login, and keeps it after a
/// store→session copy.
#[test]
fn a_fake_mode_tick_marks_the_reading_stale_only_on_a_store_copy() {
    let home = HomeSandbox::new();
    let claude_home = fake_claude_home(home.home());
    let launch = member("fake-stale");
    let store = member_store(&launch);
    let pname = armed_durable_stamp("fake-stale");
    let (swap, _markers) = fake_session(&launch, Isolation::Shared);
    set_mtime(&store, SystemTime::now() - Duration::from_secs(60));
    cc_relogin(&swap.runtime, CREDS_V2, SystemTime::now());

    tick(&claude_home, &swap).expect("tick");

    assert_eq!(
        fs::read(&store).expect("read store"),
        CREDS_V2,
        "precondition: the session login was copied into the store"
    );
    assert_eq!(
        durable_stamp(&pname),
        None,
        "a session→store copy marks the reading stale"
    );

    // A store→session copy (clauth rotated the store to a different pair; the
    // session's fake-mode copy is older) writes no store, so it must not mark.
    crate::usage::take_profile_fetch(&pname, false, 2_000_000_000_000u64);
    assert!(durable_stamp(&pname).is_some(), "precondition: re-armed");
    let runtime_creds = swap.runtime.join(".credentials.json");
    set_mtime(&runtime_creds, SystemTime::now() - Duration::from_secs(60));
    let mut rotated = serde_json::json!({"claudeAiOauth": {"expiresAt": 9_999_999_999_999u64}});
    rotated["claudeAiOauth"]["accessToken"] = "at-rotated".into();
    rotated["claudeAiOauth"]["refreshToken"] = "rt-rotated".into();
    fs::write(&store, serde_json::to_vec(&rotated).expect("rotated json"))
        .expect("rotate the store");
    set_mtime(&store, SystemTime::now());
    tick(&claude_home, &swap).expect("tick");
    let runtime_now: ClaudeCredentials =
        serde_json::from_slice(&fs::read(&runtime_creds).expect("read runtime"))
            .expect("parse runtime");
    assert_eq!(
        runtime_now
            .claude_ai_oauth
            .map(|o| o.access_token)
            .as_deref(),
        Some("at-rotated"),
        "precondition: the store→session copy landed a different login in the session"
    );
    assert!(
        durable_stamp(&pname).is_some(),
        "a store→session copy writes no store and leaves the stamp untouched"
    );
}

/// The fake-mode ISOLATED watchdog tick arm marks the reading stale after a
/// session→store copy, the same wiring as the shared arm.
#[test]
fn a_fake_mode_isolated_tick_marks_the_reading_stale() {
    let home = HomeSandbox::new();
    let claude_home = fake_claude_home(home.home());
    let launch = member("fake-iso-stale");
    let store = member_store(&launch);
    let pname = armed_durable_stamp("fake-iso-stale");
    let (swap, _markers) = fake_session(&launch, Isolation::Isolated);
    set_mtime(&store, SystemTime::now() - Duration::from_secs(60));
    cc_relogin(&swap.runtime, CREDS_V2, SystemTime::now());

    tick(&claude_home, &swap).expect("tick");

    assert_eq!(
        fs::read(&store).expect("read store"),
        CREDS_V2,
        "precondition: the isolated tick drained the session's login"
    );
    assert_eq!(
        durable_stamp(&pname),
        None,
        "the isolated tick marks the reading stale after a store copy"
    );
}

/// The Real-mode build drain (`reconcile_credentials`'s Real arm) marks the
/// profile's `/profile` reading stale after a session→store copy.
#[test]
fn a_real_mode_build_drain_marks_the_profile_reading_stale() {
    let _home = HomeSandbox::new();
    let launch = member("build-real");
    let store = member_store(&launch);
    let pname = armed_durable_stamp("build-real");
    let runtime = crate::profile::profile_subpath(&pname, "runtime").expect("runtime path");
    fs::create_dir_all(&runtime).expect("mkdir runtime");
    let runtime_creds = runtime.join(".credentials.json");
    fs::write(&runtime_creds, CREDS_V2).expect("session relogin");
    set_mtime(&store, SystemTime::now() - Duration::from_secs(60));
    set_mtime(&runtime_creds, SystemTime::now());

    reconcile_credentials(&runtime_creds, &store, LinkMode::Real).expect("reconcile");

    assert_eq!(
        fs::read(&store).expect("read store"),
        CREDS_V2,
        "precondition: the Real build drained the newer runtime copy"
    );
    assert_eq!(
        durable_stamp(&pname),
        None,
        "the Real-mode build drain marks the profile's /profile reading stale"
    );
}

/// B5. The watchdog thread and `Drop`'s final tick both used to read a MOVED
/// CLONE of `canonical`, so a swap that only mutated a field would have the next
/// tick relink the session back to the OLD member AND write the new member's
/// tokens into the old member's store.
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

        gc_stale_runtimes();
        assert!(
            is_session_alive(&own.join(&sid)),
            "GC collected a live session's per-session marker on the swapped-onto member"
        );

        drop(swap);
        gc_stale_runtimes();
        assert!(
            !own.exists(),
            "the marker dir must be collected once its session is gone"
        );
    });
}

/// GC's half of the force-delete divergence, mirroring the tally's
/// `a_swapped_session_counts_on_current_member_after_its_launch_marker_is_removed`:
/// `clauth delete <launch> --force` removes the launch profile's whole dir, marker
/// dirs included, while the session keeps running on the member it swapped onto.
/// Probing `start_profile` there finds nothing, reads the row as dead, and reaps a
/// session the tally is still counting — the exact split the shared
/// `current_member`-first probe exists to prevent.
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
        // holds the swapped-onto member's marker either — so the assertion above
        // cannot be passing on a leg that never ran.
        drop(swap);
        gc_stale_runtimes();
        assert!(
            crate::live_sessions::get(&sid).is_none(),
            "a row must go once nothing it stamped is still flock-held at a path that exists"
        );
    });
}

/// Teardown owns every marker the session stamped — on the launch member and on
/// each member it swapped onto — or a dead session keeps blocking rotation on
/// accounts nothing is using.
#[test]
fn teardown_removes_every_marker_a_swap_stamped() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        if !host_poses(tmp.path(), "a real-symlink session for the swap to repoint") {
            return;
        }
        fake_claude_home(tmp.path());
        let launch = member("down-a");
        let intended = member("down-b");
        member_store(&launch);
        member_store(&intended);

        let rt = ProfileRuntime::acquire(&launch, Isolation::Shared, &[], false, Launcher::Own)
            .expect("acquire");
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

/// A swap onto the member the link already resolves to must touch nothing: no
/// marker on a second path, no mtime move that would make Claude Code re-read
/// for no reason, no registry write.
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

/// The row's `launch_store` moves with the link. `live_session_holds_rotatable`
/// reads it for the macOS refreshless verdict, so a row left naming the LAUNCH
/// member's store keeps refusing that member's rotations after the session has
/// moved onto a refreshless one — the verdict must answer for the member the
/// session holds, not the one it launched on. Ungated like its fixture twin
/// above: the row update is transport-independent.
#[test]
fn a_swap_onto_a_refreshless_member_lets_the_launch_member_rotate_again() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let launch = member("verdict-a");
        let intended = refreshless_member("verdict-b");
        member_store(&launch);
        let intended_store = member_store(&intended);
        let (swap, _launch_markers) = lone_session(&launch, Isolation::Shared);
        let launch_name = crate::profile::ProfileName::from("verdict-a");

        assert!(
            live_session_holds_rotatable(&launch_name),
            "fixture: the launch member's store is refreshable, so the refusal stands before the swap"
        );

        assert_eq!(
            swap.swap_to("verdict-b").expect("swap"),
            SwapOutcome::Swapped
        );

        assert_eq!(
            crate::live_sessions::get(swap.session.as_str())
                .expect("row")
                .launch_store
                .as_deref(),
            Some(intended_store.as_path()),
            "the row must name the store the session reads now"
        );
        assert!(
            has_live_session(&launch_name),
            "fixture: the launch member's marker survives the swap, so only the row's store can flip the verdict"
        );
        assert!(
            !live_session_holds_rotatable(&launch_name),
            "a session holding nothing rotatable strands nothing, so the launch member may rotate again"
        );
    });
}

/// The converse half of the same defect: after a swap back onto the refreshable
/// launch member the row must drop the stale refreshless exemption, or the
/// verdict allows a rotation that strands the chain the session is holding
/// again. Gated like every swap-back fixture: the recovery hop's claim is only
/// exercised where the transport can repoint.
#[test]
fn a_swap_back_onto_a_refreshable_member_refuses_rotation_again() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        if !host_poses(tmp.path(), "a real-symlink session for the swap to repoint") {
            return;
        }
        let launch = member("back-verdict-a");
        let intended = refreshless_member("back-verdict-b");
        let launch_store = member_store(&launch);
        member_store(&intended);
        let (swap, _launch_markers) = lone_session(&launch, Isolation::Shared);
        let launch_name = crate::profile::ProfileName::from("back-verdict-a");

        assert_eq!(
            swap.swap_to("back-verdict-b").expect("out"),
            SwapOutcome::Swapped
        );
        assert!(
            !live_session_holds_rotatable(&launch_name),
            "fixture: the refreshless leg of the arc, as the ungated twin pins"
        );

        assert_eq!(
            swap.swap_to("back-verdict-a").expect("back"),
            SwapOutcome::Swapped
        );

        assert_eq!(
            crate::live_sessions::get(swap.session.as_str())
                .expect("row")
                .launch_store
                .as_deref(),
            Some(launch_store.as_path()),
            "the row must name the launch member's store again once the link resolves to it"
        );
        assert!(
            live_session_holds_rotatable(&launch_name),
            "back on a refreshable chain there is a refresh token to strand again"
        );
    });
}

/// §11 #12's residue, bounded where it is cheap: `Drop` joins the watchdog, so a
/// swap STARTED after teardown began would hold session exit for the state-lock
/// timeout plus an unbounded rotation-flock wait.
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
#[cfg(unix)]
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

/// A credit-cap leave is the ONE move that may land on a member whose
/// `[models]` differ: the daemon's `credit_leave` flag on the row relaxes the
/// models-equal rule, and its absence keeps it. Driven through `poll`, which
/// reads the row the executor reads.
#[test]
fn poll_relaxes_models_for_a_credit_leave_and_only_then() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        if !host_poses(tmp.path(), "a real-symlink session for the swap to repoint") {
            return;
        }
        let launch = member("credit-a");
        let mut intended = member("credit-b");
        intended.models.default = Some("opus".to_owned());
        member_store(&launch);
        let intended_store = member_store(&intended);
        let (swap, _launch_markers) = lone_session(&launch, Isolation::Shared);
        let sid = swap.session.as_str().to_string();
        crate::live_sessions::update_as_session(&sid, |s| {
            s.set_launch_families(vec!["fable".to_owned()]);
        })
        .expect("session write");

        // No credit reason: the models-equal rule refuses.
        crate::live_sessions::update_as_daemon(&sid, |d| {
            d.set_intended_member("credit-b");
            d.set_credit_leave(false);
        })
        .expect("daemon write");
        swap.poll();
        assert_eq!(swap.member(), "credit-a", "models differ, no credit leave");
        assert_eq!(
            swap.cell().last_refusal,
            Some(("credit-b".to_string(), SwapRefused::ModelsDiffers))
        );

        // A credit-cap leave: the relaxed rule admits.
        crate::live_sessions::update_as_daemon(&sid, |d| {
            d.set_intended_member("credit-b");
            d.set_credit_leave(true);
        })
        .expect("daemon write");
        swap.poll();
        assert_eq!(
            swap.member(),
            "credit-b",
            "a credit leave may land on differing models"
        );
        assert_eq!(
            fs::read_link(swap.runtime.join(".credentials.json")).expect("read link"),
            intended_store
        );
        let row = crate::live_sessions::get(&sid).expect("row");
        assert_eq!(
            row.launch_families,
            Some(vec!["fable".to_owned()]),
            "a swap moves credentials only, never the launch demand"
        );
    });
}

/// A standing intent the executor refuses re-fires every tick, so announcing
/// unconditionally writes one line per second for as long as it stands — but a
/// refusal nothing ever says leaves the session on its launch account invisibly.
///
/// The dedupe itself is in-memory cell state with no platform dependency, so only
/// the swap that clears it is gated. The refusals that actually stand in
/// production are `swap_eligible`'s and `NoCredentialStore`.
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
        // information, not a repeat.
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

// ── the in-place convergence (rolling-token arming under live sessions) ──────

/// The issue-#84 shape: a session that launched before its profile was armed
/// for rolling tokens holds the rotating pair, and arming changes only future
/// starts. That session's own poll must detect the transition — its canonical
/// still refreshable while the install source now selects a refreshless
/// sidecar — and converge onto the sidecar in place, one poll, same member.
#[test]
fn a_pre_arming_session_converges_onto_the_armed_sidecar_on_one_poll() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        if !host_poses(tmp.path(), "a convergence relink") {
            return;
        }
        let launch = member("conv-a");
        member_store(&launch);
        let (swap, _launch_markers) = lone_session(&launch, Isolation::Shared);
        let sid = swap.session.as_str().to_string();

        // Arm the profile under the live session.
        let sidecar = crate::profile::profile_dir(&crate::profile::ProfileName::from("conv-a"))
            .expect("profile_dir")
            .join("session-token.json");
        write_creds(&sidecar, None);

        swap.poll();

        assert_eq!(
            fs::read_link(swap.runtime.join(".credentials.json")).expect("read link"),
            sidecar,
            "one poll repoints the session onto the armed sidecar"
        );
        assert_eq!(swap.canonical(), sidecar, "the cell follows the link");
        assert_eq!(swap.member(), "conv-a", "the member does not change");
        let row = crate::live_sessions::get(&sid).expect("row");
        assert_eq!(row.current_member.as_deref(), Some("conv-a"));
        assert_eq!(
            row.launch_store.as_deref(),
            Some(sidecar.as_path()),
            "the row's launch_store names the sidecar, so the rotation refusal lifts"
        );
        assert!(
            row.last_swap_at.is_some(),
            "the convergence advances last_swap_at exactly like a swap"
        );
        // The item arm the convergence executes on macOS, pinned here through
        // the pure seam: the bearer is signed out, never installed.
        let creds = crate::profile::read_json_file::<crate::profile::ClaudeCredentials>(&sidecar)
            .expect("read sidecar");
        assert_eq!(converge_item_arm(Some(&creds)), SwapItemArm::SignOut);
    });
}

/// The convergence's twin of the swap's unjudged-drain refusal: Claude Code's
/// own refresh in the runtime file, the record beside the store unreadable,
/// and the convergence stops before it repoints the link onto the sidecar.
#[test]
fn a_convergence_refuses_while_the_drain_cannot_judge_the_session_file() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let launch = member("conv-unjudged");
        let store = member_store(&launch);
        let (swap, _launch_markers) = lone_session(&launch, Isolation::Shared);
        let sid = swap.session.as_str().to_string();
        let dir = crate::profile::profile_dir(&crate::profile::ProfileName::from("conv-unjudged"))
            .expect("profile_dir");
        write_creds(&dir.join("session-token.json"), None);
        set_mtime(&store, SystemTime::now() - Duration::from_secs(60));
        cc_relogin(&swap.runtime, CHAIN_P1, SystemTime::now());
        fs::create_dir(dir.join("auth.spent.json")).expect("block the record");
        let link = swap.runtime.join(".credentials.json");

        let outcome = swap.swap_to("conv-unjudged").expect("converge");

        assert!(
            !link
                .symlink_metadata()
                .expect("runtime file")
                .file_type()
                .is_symlink(),
            "the session's own file is not relinked away"
        );
        assert_eq!(fs::read(&link).expect("runtime file"), CHAIN_P1);
        assert_eq!(swap.canonical(), store, "the cell still names the store");
        let row = crate::live_sessions::get(&sid).expect("row");
        assert_eq!(row.current_member, None, "the row still names no swap");
        assert_eq!(
            outcome,
            SwapOutcome::Refused(SwapRefused::OutgoingRecordUnreadable(
                "conv-unjudged".to_string()
            ))
        );
    });
}

/// Same member, same source: without an armed transition the poll is a no-op
/// — no mtime move, no registry write, and no refusal recorded (the steady
/// state is not news).
#[test]
fn a_convergence_does_not_move_a_session_without_an_armed_transition() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let launch = member("conv-noop-a");
        let launch_store = member_store(&launch);
        let (swap, _launch_markers) = lone_session(&launch, Isolation::Shared);
        let sid = swap.session.as_str().to_string();

        let before = SystemTime::now() - Duration::from_secs(60);
        set_mtime(&launch_store, before);

        swap.poll();

        assert_eq!(swap.member(), "conv-noop-a");
        assert_eq!(
            fs::read_link(swap.runtime.join(".credentials.json")).expect("read link"),
            launch_store,
            "no sidecar armed — there is nothing to converge onto"
        );
        assert_eq!(
            fs::metadata(&launch_store)
                .expect("meta")
                .modified()
                .expect("mtime"),
            before,
            "a no-op convergence must not move the store's mtime"
        );
        let row = crate::live_sessions::get(&sid).expect("row");
        assert_eq!(row.current_member, None, "a no-op must not write the row");
        assert_eq!(row.last_swap_at, None);
        assert!(
            swap.cell().last_refusal.is_none(),
            "the steady state is not a refusal"
        );
    });
}

/// The admitted transition is rotatable-current → refreshless-selected only.
/// A session already on the sidecar must not converge BACK onto the rotating
/// store when the profile is disarmed — reverse convergence would re-strand
/// the session on a login the re-stamp leg no longer owns.
#[test]
fn a_refreshless_session_does_not_converge_back_onto_the_rotating_store() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let launch = member("conv-rev-a");
        register_profile(&launch);
        // Armed BEFORE the fixture, so the session launches on the sidecar.
        let sidecar = crate::profile::profile_dir(&crate::profile::ProfileName::from("conv-rev-a"))
            .expect("profile_dir")
            .join("session-token.json");
        write_creds(&sidecar, None);
        let (swap, _launch_markers) = lone_session(&launch, Isolation::Shared);
        let sid = swap.session.as_str().to_string();

        // Disarm: the install source falls back to the rotating store.
        fs::remove_file(&sidecar).expect("disarm");

        swap.poll();

        assert_eq!(swap.member(), "conv-rev-a");
        assert_eq!(
            fs::read_link(swap.runtime.join(".credentials.json")).expect("read link"),
            sidecar,
            "a refreshless current source never converges onto the rotating store"
        );
        let row = crate::live_sessions::get(&sid).expect("row");
        assert_eq!(row.current_member, None);
        assert_eq!(row.last_swap_at, None);
    });
}

/// An unreadable current source admits nothing: the session stays put and the
/// fail-closed rotation refusal stands (an unknown must never read as the
/// armed transition).
#[test]
fn an_unreadable_current_source_does_not_converge() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let launch = member("conv-torn-a");
        let launch_store = member_store(&launch);
        let (swap, _launch_markers) = lone_session(&launch, Isolation::Shared);

        fs::write(&launch_store, b"not json").expect("corrupt the current source");
        let sidecar =
            crate::profile::profile_dir(&crate::profile::ProfileName::from("conv-torn-a"))
                .expect("profile_dir")
                .join("session-token.json");
        write_creds(&sidecar, None);

        swap.poll();

        assert_eq!(
            fs::read_link(swap.runtime.join(".credentials.json")).expect("read link"),
            launch_store,
            "an unreadable current source never moves"
        );
        let row = crate::live_sessions::get(swap.session.as_str()).expect("row");
        assert_eq!(row.current_member, None);
    });
}

/// The transition discriminator, pinned pure: only a refreshable current
/// source with a DISTINCT refreshless selected source converges. Everything
/// else — same source, reverse, refreshable-to-refreshable, and every
/// unreadable shape — stays a no-op.
#[test]
fn a_convergence_admits_only_rotatable_current_to_refreshless_selected() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let rot = tmp.path().join("rot.json");
    let rot2 = tmp.path().join("rot2.json");
    let refreshless = tmp.path().join("sidecar.json");
    let refreshless2 = tmp.path().join("sidecar2.json");
    let torn = tmp.path().join("torn.json");
    write_creds(&rot, Some("rt-1"));
    write_creds(&rot2, Some("rt-2"));
    write_creds(&refreshless, None);
    write_creds(&refreshless2, None);
    fs::write(&torn, b"not json").expect("write torn");

    assert!(
        converge_transition_holds(&rot, &refreshless),
        "the armed transition is exactly what converges"
    );
    assert!(
        !converge_transition_holds(&rot, &rot),
        "same member, same source stays a no-op"
    );
    assert!(
        !converge_transition_holds(&refreshless, &rot),
        "refreshless → rotatable (disarm) does not auto-converge"
    );
    assert!(
        !converge_transition_holds(&refreshless, &refreshless2),
        "a refreshless current source is not rotatable-current, however the \
         selected source reads"
    );
    assert!(
        !converge_transition_holds(&rot, &rot2),
        "rotatable → rotatable is not an armed transition"
    );
    assert!(
        !converge_transition_holds(&torn, &refreshless),
        "an unreadable current source does not move"
    );
    assert!(
        !converge_transition_holds(&rot, &torn),
        "an unreadable selected source does not move"
    );
    assert!(
        !converge_transition_holds(&tmp.path().join("missing.json"), &refreshless),
        "a missing current source does not move"
    );
}

/// The item arm the convergence executes on macOS, pinned pure: the bearer is
/// SIGNED OUT, never installed — installing a refreshless login would strand
/// the session on a snapshot the re-stamp leg can no longer reach. The
/// Install arms are unreachable by admission (the transition selects only a
/// refreshless store) and stop the move fail-closed if ever reached.
#[test]
fn a_convergence_signs_the_item_out_and_never_installs() {
    use crate::profile::{ClaudeCredentials, OAuthToken};
    let store = |refresh: Option<&str>| ClaudeCredentials {
        claude_ai_oauth: Some(OAuthToken {
            access_token: "a".to_string(),
            refresh_token: refresh.map(str::to_string),
            expires_at: None,
            scopes: None,
            subscription_type: None,
            ..OAuthToken::default_extra()
        }),
    };

    assert_eq!(
        converge_item_arm(Some(&store(None))),
        SwapItemArm::SignOut,
        "the bearer is signed out of the item, never installed"
    );
    assert_eq!(
        converge_item_arm(Some(&store(Some("r")))),
        SwapItemArm::Install
    );
    assert_eq!(converge_item_arm(None), SwapItemArm::Install);
}

/// The convergence Keychain legs' failure routing, pinned pure: the
/// classified locked-keychain transient raises nothing — it is the steady
/// state the next poll clears once the keychain unlocks, and a line per tick
/// for its whole duration is the noise the seed's disposition pattern exists
/// to avoid — while every other class goes through the announcement memo.
#[test]
fn a_convergence_keychain_failure_routes_the_locked_keychain_as_silent() {
    assert_eq!(
        converge_leg_disposition(crate::claude::SecurityExitClass::InteractionNotAllowed),
        ConvergeLegDisposition::Silent,
        "the classified locked-keychain transient must not log per tick"
    );
    assert_eq!(
        converge_leg_disposition(crate::claude::SecurityExitClass::ItemNotFound),
        ConvergeLegDisposition::Announce
    );
    assert_eq!(
        converge_leg_disposition(crate::claude::SecurityExitClass::Unclassified),
        ConvergeLegDisposition::Announce
    );
}

/// The standing-failure memo the legs log behind, pinned through the same
/// once-per-(member, reason) gate: a persistent non-transient failure
/// announces once and stays silent across ticks until the class, the leg, or
/// the member changes — a landed swap or convergence clears the memo, which
/// `a_standing_refusal_is_announced_once_per_reason` already pins.
#[test]
fn a_convergence_keychain_failure_memo_silences_a_standing_fault() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let launch = member("carry-a");
        member_store(&launch);
        let (swap, _launch_markers) = lone_session(&launch, Isolation::Shared);

        let carry =
            SwapRefused::ConvergeCarryFailed(crate::claude::SecurityExitClass::ItemNotFound);
        assert!(
            swap.should_announce("carry-a", &carry),
            "the first failure is news"
        );
        assert!(
            !swap.should_announce("carry-a", &carry),
            "the standing failure must not repeat every tick"
        );
        let changed_class =
            SwapRefused::ConvergeCarryFailed(crate::claude::SecurityExitClass::Unclassified);
        assert!(
            swap.should_announce("carry-a", &changed_class),
            "a changed class is news"
        );
        let sign_out =
            SwapRefused::ConvergeSignOutFailed(crate::claude::SecurityExitClass::ItemNotFound);
        assert!(
            swap.should_announce("carry-a", &sign_out),
            "a changed leg is news"
        );
        assert!(
            !swap.should_announce("carry-a", &sign_out),
            "the new reason is then the standing one"
        );
    });
}

// ── bare `claude` session markers ────────────────────────────────────────────

/// The whole safety argument for counting bare sessions: their markers live
/// OUTSIDE `profiles/`, so `has_live_session` (delete, disable, rename) and the
/// marker term of the macOS rotation gate read exactly the profile's own
/// markers, never a bare session's. Both directions, because a marker
/// namespace that suppressed a real session's marker would be the same defect
/// pointing the other way.
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

/// A live marker short-circuits the fresh [`has_live_session`] before the job
/// store is read: the union checks the marker first and only walks
/// `~/.claude/jobs` when no marker is live, so the common live-`clauth start`
/// case pays no store walk.
#[test]
fn a_fresh_has_live_session_reads_the_store_zero_times_while_a_marker_is_live() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let name = crate::profile::ProfileName::from("work");
        let _live = hold_session_row_marker(&name, false, "4242-0").expect("hold a session");
        assert!(
            has_live_session(&name),
            "fixture control: the held marker alone reads live"
        );

        crate::claude_jobs::reset_job_trees_reads();
        let live = has_live_session(&name);
        assert!(live, "the live marker keeps the session live");
        assert_eq!(
            crate::claude_jobs::job_trees_reads(),
            0,
            "a live marker must short-circuit the job-store read"
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

/// The bare-marker sweep runs at every `clauth mcp` boot, the Services tab's
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
/// This is the WIRING pin — specs → watcher → reconcile → the sibling's view —
/// plus the separation from the polling fallback: each session's watchdog counts
/// its tick-driven reconciles, and both must read 0. The 1 Hz credential leg of
/// the polling fallback reaches the store and the sibling too, so only the
/// counters tell an event apart from a poll that happened to land.
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

        let a = ProfileRuntime::acquire(&profile, Isolation::Shared, &[], false, Launcher::Own)
            .expect("acquire a");
        let b = ProfileRuntime::acquire(&profile, Isolation::Shared, &[], false, Launcher::Own)
            .expect("acquire b");

        // A hang guard, not a timing pin: the event path converges in
        // milliseconds, and the tick counters below are what separate it from
        // the polling fallback. The deadline only bounds a regression to a
        // failure instead of a hang. It stays under the 30 s fallback, so a
        // fallback-driven converge cannot satisfy the file assertions either.
        const DEADLINE: std::time::Duration = std::time::Duration::from_secs(10);
        assert!(
            crate::watchdog::PRODUCTION.fallback > DEADLINE,
            "fixture: the fallback ticker must not be able to meet the deadline, \
             or the file assertions could be satisfied by a fallback-driven \
             reconcile instead of acting as a pure hang guard"
        );

        // Claude Code's re-login shape: unlink the link, write a regular file.
        let live = a.config_dir().join(".credentials.json");
        fs::remove_file(&live).expect("unlink runtime creds");
        fs::write(&live, CREDS_V2).expect("write re-login");

        let sibling = b.config_dir().join(".credentials.json");
        let deadline = std::time::Instant::now() + DEADLINE;
        while std::time::Instant::now() < deadline {
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
            "the re-login never reached the store within {DEADLINE:?}"
        );
        assert_eq!(
            fs::read(&sibling).expect("read sibling"),
            CREDS_V2,
            "the sibling session still resolves the pre-re-login chain after {DEADLINE:?}"
        );
        assert_eq!(
            a.tick_reconciles(),
            0,
            "the relogin reached the store on the filesystem event, not on a ticker: \
             the session's watchdog never reconciled on a poll"
        );
        assert_eq!(
            b.tick_reconciles(),
            0,
            "the sibling's watchdog stayed on its events too"
        );

        drop(b);
        drop(a);
    });
}

// One payload is `REPEATS` copies of an 8-byte token, so a partially
// published file is detectable by shape alone rather than by guessing which
// writer's round should have won.
const TOKEN: usize = 8;
const REPEATS: usize = 512;
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

/// The convergence fixture's one publish step: stage the round's payload, then
/// publish it atomically into `near` — the one primitive fake mode uses,
/// whichever side of the mirror the round targets. `far` is first held at an
/// explicit past mtime: a 1 s-granularity filesystem stamps a same-second
/// publish with the value the mirror and the snapshots already read, so the
/// write would stay invisible to both (`mtime_newer` is strict, and the
/// counter's before/after shapes compare equal). An explicitly old `far` makes
/// the publish strictly newer at any granularity, so the pass that propagates
/// it moves the snapshot and the write is counted.
fn publish_round(src: &Path, near: &Path, far: &Path, writer: usize, round: usize) {
    let staged = src.join(format!("w{writer}"));
    fs::write(&staged, payload(writer, round)).expect("stage");
    let name = format!("shared-{writer}.json");
    let far_file = far.join(&name);
    // The first round has no far side yet; its seeding pass is counted by the
    // entry appearing in the snapshot, not by an mtime move.
    if far_file.exists() {
        set_mtime(&far_file, SystemTime::now() - Duration::from_secs(600));
    }
    copy_file(&staged, &near.join(name)).expect("publish");
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
                        // Into whichever side of the mirror this round targets.
                        let (near, far) = if round % 2 == 0 {
                            (home, runtime)
                        } else {
                            (runtime, home)
                        };
                        publish_round(src, near, far, writer, round);
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

/// A same-second write under 1 s mtime granularity: the coarse stamp leaves the
/// mirror's `(name, len, mtime)` snapshots equal and the write uncounted, so a
/// publish the fixture just made is read as no write — `mtime_newer` is strict,
/// so the mirror cannot see the publish either. The fixture's answer is its own
/// clock: `publish_round` holds the far side at an explicit past before the
/// publish, so the publish reads strictly newer at any granularity and the pass
/// that propagates it counts. The coarse stamp is simulated by pinning both
/// sides to the same second before and after the publish.
#[test]
fn the_mirror_write_counter_counts_a_same_second_publish() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let home = tmp.path().join(".claude");
    let runtime = tmp.path().join("runtime");
    let src = tmp.path().join("src");
    for dir in [&home, &runtime, &src] {
        fs::create_dir_all(dir).expect("mkdir");
    }
    let name = "shared-0.json";
    fs::write(home.join(name), payload(0, 0)).expect("seed home");
    fs::write(runtime.join(name), payload(0, 0)).expect("seed runtime");
    // Both sides in the SAME second — the coarse stamp the defect is about. The
    // publish is re-pinned to it below, standing in for a 1 s-granularity
    // filesystem stamping a real write into the second the snapshot already read.
    let second = SystemTime::now();
    set_mtime(&home.join(name), second);
    set_mtime(&runtime.join(name), second);

    let mirror = Mirror {
        home: home.clone(),
        runtime: runtime.clone(),
        writes: std::sync::atomic::AtomicUsize::new(0),
        torn: std::sync::Mutex::new(Vec::new()),
    };

    publish_round(&src, &home, &runtime, 0, 1);
    set_mtime(&home.join(name), second);

    crate::watchdog::Reconcile::credentials(&mirror);

    assert_eq!(
        mirror.writes(),
        1,
        "the same-second publish must be counted as a write"
    );
    assert_eq!(
        fs::read(runtime.join(name)).expect("read runtime"),
        payload(0, 1),
        "and the pass that was counted must have propagated the publish"
    );
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
        claude_pid: None,
        launch_families: None,
        started_at: 1_700_000_000_000,
        cwd: None,
        isolated: false,
        follows_chain: false,
        intended_member: None,
        chain_cursor: None,
        credit_leave: false,
        credit_leave_aware: false,
        current_member: None,
        last_swap_at: None,
        launch_store,
        boot_transport: None,
        is_delegate: false,
        background: false,
        background_hold: false,
        visited_members: vec![],
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
/// `rotation_blocked_for` would leave them all green. Driven through
/// `rotation_blocked_on` with the macOS host posed, so the Linux gate pins it
/// too.
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
            rotation_blocked_on(&crate::profile::ProfileName::from("wired-rot"), true),
            "a live session on a rotating pair must still refuse"
        );

        let refreshless =
            crate::profile::profile_dir(&crate::profile::ProfileName::from("wired-roll"))
                .expect("profile_dir")
                .join("session-token.json");
        write_creds(&refreshless, None);
        let _b = live_session_launched_on("wired-roll", "99999-0", &refreshless);
        assert!(
            !rotation_blocked_on(&crate::profile::ProfileName::from("wired-roll"), true),
            "the narrowing is not wired into rotation_blocked_for"
        );
    });
}

/// An unfinished background job's tree under `profile`, its
/// `.credentials.json` holding `refresh` (`None` = refresh-less).
fn unfinished_job_tree(
    home: &Path,
    profile: &str,
    runtime: &str,
    id: &str,
    refresh: Option<&str>,
) -> PathBuf {
    let tree = home.join(".clauth/profiles").join(profile).join(runtime);
    fs::create_dir_all(&tree).expect("mkdir tree");
    write_creds(&tree.join(".credentials.json"), refresh);
    write_claude_job_in(home, id, &tree, "working", None)
}

/// The job term is scoped to the profile's own dir: a rotating job under
/// `rotjob2` blocks `rotjob2` alone, never the profile whose name it extends,
/// and a profile with no dir is blocked by nothing.
#[test]
fn an_unfinished_job_blocks_only_the_profile_whose_tree_it_names() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        fs::create_dir_all(tmp.path().join(".clauth/profiles/rotjob")).expect("mkdir");
        unfinished_job_tree(tmp.path(), "rotjob2", "runtime-1-0", "cc000001", Some("rt"));
        let blocked =
            |name: &str| rotation_blocked_on(&crate::profile::ProfileName::from(name), true);
        assert_eq!(
            (blocked("rotjob"), blocked("rotjob2"), blocked("nodir")),
            (false, true, false)
        );
    });
}

/// One rotating tree among several unfinished ones is enough to block: a
/// refresh-less tree alone frees the chain, and adding a rotating one beside it
/// blocks again (the fail-open side of the quantifier).
#[test]
fn any_rotating_unfinished_tree_blocks_beside_refreshless_ones() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let name = crate::profile::ProfileName::from("mix");
        unfinished_job_tree(tmp.path(), "mix", "runtime-1-0", "dd000001", None);
        assert!(
            !rotation_blocked_on(&name, true),
            "a refresh-less tree alone frees the chain"
        );
        unfinished_job_tree(tmp.path(), "mix", "runtime-2-0", "dd000002", Some("rt"));
        assert!(
            rotation_blocked_on(&name, true),
            "a rotating tree beside it blocks"
        );
    });
}

/// A job naming its tree through a symlinked spelling of the profile dir
/// still counts: both sides compare canonical identities.
#[cfg(unix)]
#[test]
fn a_job_naming_the_tree_through_a_symlink_still_blocks() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let real = tmp.path().join(".clauth/profiles/sym/runtime-1-0");
        fs::create_dir_all(&real).expect("mkdir");
        write_creds(&real.join(".credentials.json"), Some("rt"));
        let alias = tmp.path().join("alias");
        std::os::unix::fs::symlink(tmp.path().join(".clauth/profiles/sym"), &alias).expect("link");
        write_claude_job_in(
            tmp.path(),
            "ee000001",
            &alias.join("runtime-1-0"),
            "working",
            None,
        );
        assert!(rotation_blocked_on(
            &crate::profile::ProfileName::from("sym"),
            true
        ));
    });
}

/// A `state` of any shape keeps the store readable, so a retyped field never
/// freezes every gate: missing, `null`, a number and an object all read as an
/// unfinished job (blocking its own profile only), and only `"stopped"` frees.
#[test]
fn any_job_state_shape_keeps_the_store_readable() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let job = unfinished_job_tree(tmp.path(), "st", "runtime-1-0", "ff000001", Some("rt"));
        let record = job.join("state.json");
        let set_state = |state: Option<serde_json::Value>| {
            let mut value: serde_json::Value =
                serde_json::from_slice(&fs::read(&record).expect("read")).expect("parse");
            let object = value.as_object_mut().expect("object");
            match state {
                Some(state) => {
                    object.insert("state".into(), state);
                }
                None => {
                    object.remove("state");
                }
            }
            fs::write(&record, serde_json::to_vec(&value).expect("encode")).expect("write");
        };
        let own = crate::profile::ProfileName::from("st");
        let other = crate::profile::ProfileName::from("unrelated");
        for (label, state, own_blocked) in [
            ("missing", None, true),
            ("null", Some(serde_json::Value::Null), true),
            ("number", Some(serde_json::json!(42)), true),
            ("object", Some(serde_json::json!({"kind": "stopped"})), true),
            ("stopped", Some(serde_json::json!("stopped")), false),
        ] {
            set_state(state);
            assert_eq!(
                (
                    matches!(
                        crate::claude_jobs::job_trees(),
                        crate::claude_jobs::JobTrees::Named(_)
                    ),
                    rotation_blocked_on(&own, true),
                    has_live_session(&other),
                    rotation_blocked_on(&other, true),
                ),
                (true, own_blocked, false, false),
                "a {label} state"
            );
        }
    });
}

/// The job term narrows the way the marker term does: it asks what the
/// background session's tree HOLDS. A tree whose `.credentials.json` parses
/// with no refresh token (a `session-token.json` sidecar, linked or copied)
/// has nothing a rotation can strand, so an unfinished job on it frees the
/// chain; a rotating pair, or anything this read cannot judge (no file, a
/// dangling link, unparseable bytes), still blocks.
#[test]
fn an_unfinished_job_on_a_refreshless_tree_does_not_block_rotation() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let name = crate::profile::ProfileName::from("rotjob");
        let dir = crate::profile::profile_dir(&name).expect("profile_dir");
        let tree = dir.join("runtime-4242-0");
        fs::create_dir_all(&tree).expect("mkdir runtime");
        write_claude_job(tmp.path(), "3fde53e3", &tree);
        let creds = tree.join(".credentials.json");

        assert!(
            rotation_blocked_on(&name, true),
            "a tree with no credentials file cannot be judged, so it blocks"
        );

        write_creds(&creds, Some("rt-live"));
        assert!(
            rotation_blocked_on(&name, true),
            "a tree holding a rotating pair blocks"
        );

        write_creds(&creds, None);
        assert!(
            !rotation_blocked_on(&name, true),
            "a tree holding no refresh token frees the chain"
        );

        fs::write(&creds, b"{\"claudeAiOauth\":").expect("torn creds");
        assert!(
            rotation_blocked_on(&name, true),
            "unparseable credentials block"
        );

        #[cfg(unix)]
        {
            fs::remove_file(&creds).expect("drop creds");
            let sidecar = dir.join("session-token.json");
            write_creds(&sidecar, None);
            std::os::unix::fs::symlink(&sidecar, &creds).expect("link creds");
            assert!(
                !rotation_blocked_on(&name, true),
                "a tree linking a refresh-less sidecar frees the chain"
            );
            fs::remove_file(&sidecar).expect("drop sidecar");
            assert!(rotation_blocked_on(&name, true), "a dangling link blocks");
        }
    });
}

// ── the fan-out warning (P1) ─────────────────────────────────────────────────

/// The exact warning fires only from two live sessions up: `<n>` is the
/// observed live count including the session whose item write just landed,
/// and the threshold is `n >= 2` — one session on its own rotating pair is
/// the pre-arming norm, not a fan-out.
#[test]
fn a_fanout_warning_fires_at_two_and_three_live_sessions() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let name = crate::profile::ProfileName::from("fanout-a");
        let store = crate::profile::profile_dir(&name)
            .expect("profile_dir")
            .join("credentials.json");
        write_creds(&store, Some("rt-live"));
        let session = SessionId::mint();

        assert_eq!(
            fanout_warning(true, &name, &store, &session),
            None,
            "one session on its own rotating pair is the pre-arming norm"
        );

        let _second = live_session_launched_on("fanout-a", "22222-1", &store);
        assert_eq!(
            fanout_warning(true, &name, &store, &session),
            Some(
                "clauth: warning: 'fanout-a' has 2 live sessions sharing one rotating login; run `clauth rolling-token fanout-a` before one refresh signs the others out"
                    .to_string()
            ),
            "the second live session holding the rotating login is exactly the warning"
        );

        let _third = live_session_launched_on("fanout-a", "22222-2", &store);
        assert_eq!(
            fanout_warning(true, &name, &store, &session),
            Some(
                "clauth: warning: 'fanout-a' has 3 live sessions sharing one rotating login; run `clauth rolling-token fanout-a` before one refresh signs the others out"
                    .to_string()
            ),
            "the observed count includes every live holder"
        );
    });
}

/// Registry rows are the real thing, and a row whose session is gone drops by
/// the same liveness predicate the tally/decision leg uses — never counted
/// into a warning that names it as live.
#[test]
fn a_fanout_warning_ignores_dead_rows() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let name = crate::profile::ProfileName::from("fanout-dead");
        let store = crate::profile::profile_dir(&name)
            .expect("profile_dir")
            .join("credentials.json");
        write_creds(&store, Some("rt-live"));
        let session = SessionId::mint();

        // A row with no held marker: the session it names is gone.
        register_row("fanout-dead", "33333-3", Some(store.clone()));

        assert_eq!(
            fanout_warning(true, &name, &store, &session),
            None,
            "a dead row must not count a session the warning names as live"
        );
    });
}

/// A row whose launch_store names a refreshless sidecar holds no rotating
/// login, so it never counts toward the fan-out.
#[test]
fn a_fanout_warning_ignores_rows_holding_a_refreshless_store() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let name = crate::profile::ProfileName::from("fanout-side");
        let dir = crate::profile::profile_dir(&name).expect("profile_dir");
        let store = dir.join("credentials.json");
        write_creds(&store, Some("rt-live"));
        let sidecar = dir.join("session-token.json");
        write_creds(&sidecar, None);
        let session = SessionId::mint();

        let _side = live_session_launched_on("fanout-side", "44444-4", &sidecar);

        assert_eq!(
            fanout_warning(true, &name, &store, &session),
            None,
            "a refreshless row holds no rotating login to share"
        );
    });
}

/// Attribution follows the tally: a row whose current/start profile is not
/// this one is another account's session, however its store reads. The
/// foreign row names THIS profile's store on purpose — the store-path check
/// alone must not drop it, or a plant deleting the attribution check stays
/// green.
#[test]
fn a_fanout_warning_ignores_rows_attributed_to_another_profile() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let name = crate::profile::ProfileName::from("fanout-a");
        let store = crate::profile::profile_dir(&name)
            .expect("profile_dir")
            .join("credentials.json");
        write_creds(&store, Some("rt-live"));
        let session = SessionId::mint();

        // Attributed to fanout-other but launch_store = fanout-a's store:
        // only the attribution check can drop it from fanout-a's count.
        let _foreign = live_session_launched_on("fanout-other", "55555-5", &store);

        assert_eq!(
            fanout_warning(true, &name, &store, &session),
            None,
            "another profile's session is not this profile's fan-out"
        );
    });
}

/// A failed item write created no new copy, so it must never raise the
/// success-shaped warning — the decision is fed the write's own outcome.
#[test]
fn a_failed_item_write_never_raises_the_fanout_warning() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let name = crate::profile::ProfileName::from("fanout-fail");
        let store = crate::profile::profile_dir(&name)
            .expect("profile_dir")
            .join("credentials.json");
        write_creds(&store, Some("rt-live"));
        let session = SessionId::mint();

        let _other = live_session_launched_on("fanout-fail", "66666-6", &store);

        assert_eq!(
            fanout_warning(false, &name, &store, &session),
            None,
            "a failed write created no copy and must not warn like a landed one"
        );
    });
}

/// The swap Install arm's shape: the writing session's row still names its
/// PREVIOUS store until the row repoint lands, so the count must include the
/// writing session by construction rather than off its row.
#[test]
fn a_fanout_warning_counts_the_session_whose_write_landed_even_while_its_row_names_its_previous_store()
 {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let name = crate::profile::ProfileName::from("fanout-swap");
        let dir = crate::profile::profile_dir(&name).expect("profile_dir");
        let store = dir.join("credentials.json");
        write_creds(&store, Some("rt-live"));
        let session = SessionId::mint();

        // The writing session's own row names the outgoing member's store.
        let previous = dir.join("previous.json");
        write_creds(&previous, Some("rt-old"));
        register_row("fanout-swap", session.as_str(), Some(previous));
        let _other = live_session_launched_on("fanout-swap", "77777-7", &store);

        assert_eq!(
            fanout_warning(true, &name, &store, &session),
            Some(
                "clauth: warning: 'fanout-swap' has 2 live sessions sharing one rotating login; run `clauth rolling-token fanout-swap` before one refresh signs the others out"
                    .to_string()
            ),
            "the observed count includes the session whose write just landed"
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
    crate::testutil::write_codex_roster(&["cx"]);

    let stores = live_isolated_stores();

    assert_eq!(
        stores.iter().map(|(p, _)| p.as_str()).collect::<Vec<_>>(),
        ["cl"],
        "the codex twin of an identically live-shaped store is not listed"
    );
}

// ── codex session homes ──────────────────────────────────────────────────────

/// The copied config.toml loses exactly the keys that would let the session read
/// or write outside the home clauth just built — and nothing else. `sqlite_home`
/// and `cli_auth_credentials_store` are also pinned by a forced `-c` at spawn;
/// `debug.config_lockfile` is why that pin alone is not enough, since a lockfile
/// replay rebuilds the config from ONE layer and erases the `-c` layer entirely.
#[cfg(unix)]
#[test]
fn a_session_config_loses_the_keys_that_escape_the_home() {
    let home = crate::testutil::HomeSandbox::new();
    let operator = home.home().join(".codex");
    fs::create_dir_all(&operator).expect("mkdir operator");
    fs::write(
        operator.join("config.toml"),
        b"model = \"o3\"\n\
          sqlite_home = \"/tmp/one-shared-dir\"\n\
          cli_auth_credentials_store = \"keyring\"\n\
          \n[debug.config_lockfile]\n\
          load_path = \"/tmp/replay.lock\"\n\
          \n[tui]\n\
          notifications = true\n",
    )
    .expect("write config");

    let profile = home.home().join(".clauth/profiles/cx");
    fs::create_dir_all(&profile).expect("mkdir profile");
    let session_home = profile.join("codex-home-4242-0");
    crate::profile::mkdir_700(&session_home).expect("mkdir home");
    build_codex_home(&session_home, "cx", Isolation::Shared, LinkMode::Real).expect("build");

    let copied = fs::read_to_string(session_home.join("config.toml")).expect("read copy");
    let parsed: toml::Value = toml::from_str(&copied).expect("the rewrite is still valid TOML");
    let table = parsed.as_table().expect("table");
    assert!(
        table.get("sqlite_home").is_none(),
        "the state-DB escape is gone: {copied}"
    );
    assert!(
        table.get("cli_auth_credentials_store").is_none(),
        "the credential-store escape is gone: {copied}"
    );
    assert!(
        table
            .get("debug")
            .and_then(toml::Value::as_table)
            .map(|d| d.get("config_lockfile").is_none())
            .unwrap_or(true),
        "the lockfile replay that erases the -c layer is gone: {copied}"
    );
    assert_eq!(
        table.get("model").and_then(toml::Value::as_str),
        Some("o3"),
        "every key that is not an escape survives"
    );
    assert_eq!(
        table
            .get("tui")
            .and_then(toml::Value::as_table)
            .and_then(|t| t.get("notifications"))
            .and_then(toml::Value::as_bool),
        Some(true),
        "including whole tables the strip does not name"
    );
}

/// A config clauth cannot parse copies through untouched. codex will reject it
/// the same way, and a session that refuses to start beats one silently reshaped
/// by a parse that got it wrong.
#[cfg(unix)]
#[test]
fn an_unparseable_operator_config_copies_through_verbatim() {
    let home = crate::testutil::HomeSandbox::new();
    let operator = home.home().join(".codex");
    fs::create_dir_all(&operator).expect("mkdir operator");
    let broken = "model = \"o3\n[unclosed\n";
    fs::write(operator.join("config.toml"), broken).expect("write config");

    let profile = home.home().join(".clauth/profiles/cx");
    fs::create_dir_all(&profile).expect("mkdir profile");
    let session_home = profile.join("codex-home-4242-0");
    crate::profile::mkdir_700(&session_home).expect("mkdir home");
    build_codex_home(&session_home, "cx", Isolation::Shared, LinkMode::Real).expect("build");

    assert_eq!(
        fs::read_to_string(session_home.join("config.toml")).expect("read copy"),
        broken,
        "byte-for-byte, not a best-effort reshape"
    );
}

/// The copy lands owner-only whatever the operator's mode, strip or no strip:
/// it sits in a tree the perms sweep stops short of, so nothing downstream
/// retightens a 0644 it inherited. The bytes are pinned on every platform;
/// only the mode reads are unix.
#[test]
fn a_copied_config_is_owner_only_whatever_the_operators_mode() {
    let home = crate::testutil::HomeSandbox::new();
    let operator = home.home().join(".codex");
    fs::create_dir_all(&operator).expect("mkdir operator");
    let plain = "model = \"o3\"\n[tui]\nnotifications = true\n";
    fs::write(operator.join("config.toml"), plain).expect("write config");
    let escaping = "model = \"o3\"\nsqlite_home = \"/tmp/shared\"\n";
    fs::write(operator.join("work.config.toml"), escaping).expect("write layer");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for name in ["config.toml", "work.config.toml"] {
            fs::set_permissions(operator.join(name), fs::Permissions::from_mode(0o644))
                .expect("loosen the operator's copy");
        }
    }

    let session_home = home.home().join(".clauth/profiles/cx/codex-home-4242-0");
    let plain_dst = session_home.join("config.toml");
    let stripped_dst = session_home.join("work.config.toml");
    copy_codex_config(&operator.join("config.toml"), &plain_dst).expect("copy plain");
    copy_codex_config(&operator.join("work.config.toml"), &stripped_dst).expect("copy stripped");

    assert_eq!(
        fs::read_to_string(&plain_dst).expect("read copy"),
        plain,
        "no escape key, so the bytes are the operator's"
    );
    let stripped: toml::Value =
        toml::from_str(&fs::read_to_string(&stripped_dst).expect("read copy")).expect("valid TOML");
    assert!(
        stripped.get("sqlite_home").is_none(),
        "the escape key is gone: {stripped}"
    );
    assert_eq!(
        stripped.get("model").and_then(toml::Value::as_str),
        Some("o3"),
        "and the rest of the layer survives"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for dst in [&plain_dst, &stripped_dst] {
            assert_eq!(
                fs::metadata(dst).expect("stat copy").permissions().mode() & 0o777,
                0o600,
                "the mode is not the operator's: {}",
                dst.display()
            );
        }
    }
}

/// The shared-flavor home per the codex plan's table: auth.json links the
/// profile's ONE physical file (dangling until a login exists — that is the
/// point), the operator surfaces link in, hooks.json only by opt-in, and the
/// durable stores and rollout roots link into the profile-global home.
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
    for entry in [
        "memories_1.sqlite",
        "memories_1.sqlite-wal",
        "thread_history_1.sqlite",
        "history.jsonl",
    ] {
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
    for root in ["sessions", "archived_sessions"] {
        let link = session_home.join(root);
        assert!(
            link.symlink_metadata()
                .expect("rollout root link")
                .file_type()
                .is_symlink(),
            "{root} links into the profile-global home"
        );
        let target = profile.join("codex-home").join(root);
        assert_eq!(fs::read_link(&link).expect("read link"), target);
        assert!(
            target.is_dir(),
            "{root}'s target exists first: codex creates through the link"
        );
    }

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
    let sessions = session_home.join("sessions");
    assert!(
        sessions
            .symlink_metadata()
            .expect("sessions")
            .file_type()
            .is_dir(),
        "a real per-session sessions dir, discarded by design"
    );
    assert!(
        session_home
            .join("archived_sessions")
            .symlink_metadata()
            .is_err(),
        "nothing links into the store from an isolated home"
    );
}

/// A rollout is in the store the moment codex writes it, through the link:
/// codex creates `sessions/<yyyy>/<mm>/<dd>/` under the root, which a link
/// whose target exists takes, and every later session lists what earlier ones
/// wrote instead of waiting on a teardown copy that a crash skips.
#[cfg(unix)]
#[test]
fn a_rollout_written_through_the_linked_root_is_in_the_store_before_teardown() {
    let home = crate::testutil::HomeSandbox::new();
    let profile = home.home().join(".clauth/profiles/cx");
    fs::create_dir_all(&profile).expect("mkdir profile");
    let session_home = profile.join("codex-home-4242-0");
    crate::profile::mkdir_700(&session_home).expect("mkdir home");
    build_codex_home(&session_home, "cx", Isolation::Shared, LinkMode::Real).expect("build");

    let day = session_home.join("sessions/2026/09/16");
    fs::create_dir_all(&day).expect("codex's dated tree through the link");
    fs::write(day.join("x.jsonl"), b"{}").expect("write rollout");
    assert_eq!(
        fs::read(profile.join("codex-home/sessions/2026/09/16/x.jsonl")).expect("read in store"),
        b"{}",
        "visible under the store path with no teardown"
    );

    // Archiving is codex's rename between the two roots: both link into the
    // same store dir, so it stays a rename and the thread stays in the store.
    let archived = session_home.join("archived_sessions/x.jsonl");
    fs::rename(day.join("x.jsonl"), &archived).expect("archive");
    assert!(
        profile
            .join("codex-home/archived_sessions/x.jsonl")
            .is_file()
    );
}

/// The acquire/teardown lifecycle: a live marker the registry row never
/// outlives, the codex tag and launch_store on the row, and a teardown that
/// removes the per-session home while the durable store — the rollout roots
/// link into it — survives with every rollout the session wrote or archived.
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
    assert!(!rows[0].is_delegate, "a codex row is own");
    assert_eq!(
        rows[0].launch_store.as_deref(),
        Some(session_home.join("auth.json").as_path()),
        "launch_store names the auth.json THIS SESSION READS — under real \
         symlinks that file IS profiles/<name>/auth.json (the #59 one-liner), \
         and under fake mode it is the copy the session actually holds"
    );

    // A rollout the session wrote must survive the session — and so must one
    // the session ARCHIVED, which codex moves to the sibling rollout root.
    // Both roots link into the store, so the writes land there directly.
    let store = profile.join("codex-home");
    fs::write(session_home.join("sessions").join("rollout-1.jsonl"), b"{}").expect("write rollout");
    let archived = session_home.join("archived_sessions");
    fs::create_dir_all(&archived).expect("mkdir archived");
    fs::write(archived.join("rollout-0.jsonl"), b"{}").expect("write archived rollout");
    assert!(
        store.join("sessions").join("rollout-1.jsonl").is_file(),
        "in the store before any teardown"
    );

    drop(runtime);

    assert!(!session_home.exists(), "the per-session home is torn down");
    assert!(
        store.join("sessions").join("rollout-1.jsonl").is_file(),
        "the teardown unlinks the root's link, never what it points at"
    );
    assert!(
        store
            .join("archived_sessions")
            .join("rollout-0.jsonl")
            .is_file(),
        "archiving a thread must not mean deleting it at teardown"
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

    with_link_mode(LinkMode::Fake, || {
        let runtime = CodexRuntime::acquire("cx", Isolation::Shared).expect("acquire");
        let session_home = runtime.home().to_path_buf();
        assert_eq!(
            session_home,
            profile.join("codex-home"),
            "fake mode lives in the bare stem"
        );
        fs::write(session_home.join("memories_1.sqlite"), b"m").expect("write durable");

        drop(runtime);

        assert!(
            session_home.join("memories_1.sqlite").exists(),
            "teardown must never remove the profile's memory because the last session left"
        );
    });
}

/// An isolated codex session on a host that cannot symlink holds its marker
/// under the bare `sessions-isolated` stem, where claude's isolated layout
/// keeps the session suffix. The row probe must find it there, or GC reaps
/// the live row and the tally and listing lose the session.
#[test]
fn a_fake_mode_isolated_codex_row_stays_live_and_survives_row_gc() {
    let home = crate::testutil::HomeSandbox::new();
    let profile = home.home().join(".clauth/profiles/cx");
    fs::create_dir_all(&profile).expect("mkdir profile");

    with_link_mode(LinkMode::Fake, || {
        let runtime = CodexRuntime::acquire("cx", Isolation::Isolated).expect("acquire");
        let sid = runtime.session.as_str().to_owned();
        assert_eq!(
            runtime.pid_file,
            profile.join("sessions-isolated").join(&sid),
            "fixture: fake mode stamps the bare isolated stem"
        );

        assert!(
            session_row_is_live(&ProfileName::from("cx"), true, &sid),
            "the held marker must read live"
        );
        gc_live_session_rows();
        let rows: Vec<String> = crate::live_sessions::list()
            .into_iter()
            .map(|row| row.session_id)
            .collect();
        assert_eq!(rows, vec![sid], "row GC must keep the live session's row");

        drop(runtime);
    });
}

/// The isolated codex marker under real symlinks is keyed per session, so the
/// probe must keep finding `sessions-isolated-<sid>` — the arm that must not be
/// displaced by the fake-mode collapse.
#[test]
fn a_real_mode_isolated_codex_row_stays_live() {
    let home = crate::testutil::HomeSandbox::new();
    let profile = home.home().join(".clauth/profiles/cx");
    fs::create_dir_all(&profile).expect("mkdir profile");
    if !host_poses(&profile, "a real-symlink isolated codex home") {
        return;
    }

    let runtime = CodexRuntime::acquire("cx", Isolation::Isolated).expect("acquire");
    let sid = runtime.session.as_str().to_owned();
    assert_eq!(
        runtime.pid_file,
        profile.join(format!("sessions-isolated-{sid}")).join(&sid),
        "fixture: real mode keeps the per-session stem"
    );
    assert!(
        session_row_is_live(&ProfileName::from("cx"), true, &sid),
        "the held marker must read live"
    );

    drop(runtime);
}

/// A shared codex session under the fake transport holds its marker in the bare
/// `sessions` dir, the one path the claude shared-fake layout also uses; the
/// probe must read it there for a codex row.
#[test]
fn a_fake_mode_shared_codex_row_stays_live() {
    let home = crate::testutil::HomeSandbox::new();
    let profile = home.home().join(".clauth/profiles/cx");
    fs::create_dir_all(&profile).expect("mkdir profile");

    with_link_mode(LinkMode::Fake, || {
        let runtime = CodexRuntime::acquire("cx", Isolation::Shared).expect("acquire");
        let sid = runtime.session.as_str().to_owned();
        assert_eq!(
            runtime.pid_file,
            profile.join("sessions").join(&sid),
            "fixture: fake mode stamps the bare shared stem"
        );
        assert!(
            session_row_is_live(&ProfileName::from("cx"), false, &sid),
            "the held marker must read live"
        );

        drop(runtime);
    });
}

/// [`hold_session_row_marker`] stamps the first candidate, which must stay the
/// claude per-session (`LinkMode::Real`) layout: the codex candidates sit after
/// it, and under the fake transport the codex shared path is a bare dir.
#[test]
fn holding_a_row_marker_stamps_the_claude_real_layout() {
    let home = crate::testutil::HomeSandbox::new();
    let name = ProfileName::from("cx");

    let _marker = hold_session_row_marker(&name, false, "4242-0").expect("hold a session");

    assert!(
        home.home()
            .join(".clauth/profiles/cx/sessions-4242-0/4242-0")
            .is_file(),
        "the held marker must sit in the per-session real layout"
    );
    assert!(
        session_row_is_live(&name, false, "4242-0"),
        "and the probe reads the marker it just held"
    );
}

/// An isolated row's claude and codex arms collapse onto one path: both
/// transports key that layout per session, and the codex real arm repeats the
/// claude one. The probe must hold each path once and keep the claude real
/// layout first, which is the candidate [`hold_session_row_marker`] stamps.
#[test]
fn an_isolated_row_probes_each_marker_path_once() {
    let home = crate::testutil::HomeSandbox::new();
    let name = ProfileName::from("cx");

    let markers =
        session_marker_paths(&name, true, "4242-0").expect("derive the isolated candidates");

    let unique: std::collections::HashSet<&PathBuf> = markers.iter().collect();
    assert_eq!(
        unique.len(),
        markers.len(),
        "no candidate path is probed twice: {markers:?}"
    );
    assert_eq!(
        markers[0],
        home.home()
            .join(".clauth/profiles/cx/sessions-isolated-4242-0/4242-0"),
        "the claude real layout stays first"
    );
}

/// `codex --profile <name>` layers `$CODEX_HOME/<name>.config.toml`, and
/// CODEX_HOME is the session home — so the operator's profile-v2 files have to
/// land there too, sanitized like the base config, or the flag silently
/// resolves to an empty layer. The operator's installed plugins ride the same
/// link set as skills and rules, so an install outlives the session.
#[cfg(unix)]
#[test]
fn a_session_home_carries_the_profile_layers_and_the_plugin_store() {
    let home = crate::testutil::HomeSandbox::new();
    let operator = home.home().join(".codex");
    fs::create_dir_all(operator.join("plugins/cache")).expect("mkdir plugins");
    fs::write(operator.join("config.toml"), b"model = \"o3\"\n").expect("write config");
    fs::write(
        operator.join("work.config.toml"),
        b"model = \"gpt-5\"\nsqlite_home = \"/tmp/shared\"\n",
    )
    .expect("write profile layer");

    let profile = home.home().join(".clauth/profiles/cx");
    fs::create_dir_all(&profile).expect("mkdir profile");
    let session_home = profile.join("codex-home-4242-0");
    crate::profile::mkdir_700(&session_home).expect("mkdir home");
    build_codex_home(&session_home, "cx", Isolation::Shared, LinkMode::Real).expect("build");

    let layer = fs::read_to_string(session_home.join("work.config.toml")).expect("read layer");
    let parsed: toml::Value = toml::from_str(&layer).expect("valid TOML");
    assert_eq!(
        parsed.get("model").and_then(toml::Value::as_str),
        Some("gpt-5"),
        "the profile layer reached the home `--profile` looks in"
    );
    assert!(
        parsed.get("sqlite_home").is_none(),
        "and a layer cannot spell the escape the base config just lost: {layer}"
    );
    assert!(
        session_home.join("plugins").symlink_metadata().is_ok(),
        "installed plugins are linked like skills and rules"
    );
}

/// codex heals a corrupt state DB by RENAMING the path it was handed into
/// `db-backups/` and writing a fresh one in its place — and the path it was
/// handed is our symlink, so the rename moves the LINK and the corrupt bytes
/// stay in the store. Without a sync-back the healed DB dies with the session
/// and every later one relinks to the same corruption, forever.
#[cfg(unix)]
#[test]
fn a_db_codex_healed_in_place_reaches_the_durable_store() {
    let home = crate::testutil::HomeSandbox::new();
    let profile = home.home().join(".clauth/profiles/cx");
    fs::create_dir_all(&profile).expect("mkdir profile");
    let store = profile.join("codex-home");
    fs::create_dir_all(&store).expect("mkdir store");
    fs::write(store.join("state_5.sqlite"), b"corrupt").expect("seed corrupt db");

    let runtime = CodexRuntime::acquire("cx", Isolation::Shared).expect("acquire");
    let session_home = runtime.home().to_path_buf();
    let healed = session_home.join("state_5.sqlite");
    assert!(
        healed
            .symlink_metadata()
            .expect("placed")
            .file_type()
            .is_symlink(),
        "the build places a link"
    );
    // codex's recovery: the LINK is renamed away, a fresh REAL file takes its
    // name. The store still holds the corrupt bytes at this point.
    let backups = session_home.join("db-backups");
    fs::create_dir_all(&backups).expect("mkdir backups");
    fs::rename(&healed, backups.join("state_5.sqlite")).expect("rename the link away");
    fs::write(&healed, b"healed").expect("codex writes a fresh db");
    assert_eq!(
        fs::read(store.join("state_5.sqlite")).expect("read store"),
        b"corrupt",
        "the store is untouched by codex's rename — that is the whole problem"
    );

    drop(runtime);

    assert_eq!(
        fs::read(store.join("state_5.sqlite")).expect("read store"),
        b"healed",
        "teardown carries the healed DB back, so the next session stops relinking to corruption"
    );
}

/// The recovery sync-back must carry back ONLY what codex replaced. An
/// untouched durable entry is still a link into the store, and copying it back
/// would rewrite the store file through its own link — same bytes, NEW inode,
/// swapped out from under any concurrent session that has that sqlite open
/// through its own link. Teardown of one session must not do that to another.
#[cfg(unix)]
#[test]
fn teardown_leaves_an_untouched_durable_entry_on_its_own_inode() {
    use std::os::unix::fs::MetadataExt;

    let home = crate::testutil::HomeSandbox::new();
    let profile = home.home().join(".clauth/profiles/cx");
    fs::create_dir_all(&profile).expect("mkdir profile");
    let store = profile.join("codex-home");
    fs::create_dir_all(&store).expect("mkdir store");
    fs::write(store.join("state_5.sqlite"), b"live").expect("seed db");
    let before = fs::metadata(store.join("state_5.sqlite"))
        .expect("stat")
        .ino();

    let first = CodexRuntime::acquire("cx", Isolation::Shared).expect("first");
    let second = CodexRuntime::acquire("cx", Isolation::Shared).expect("second");
    drop(first);

    assert_eq!(
        fs::metadata(store.join("state_5.sqlite"))
            .expect("stat")
            .ino(),
        before,
        "the second session still holds this inode open — teardown may not replace it"
    );
    assert_eq!(
        fs::read(store.join("state_5.sqlite")).expect("read"),
        b"live"
    );
    drop(second);
}

/// Decision 8 lets codex sessions run concurrently BECAUSE they share one
/// physical auth.json. Under the fake transport they do not: the two flavors
/// collapse to separate bare homes, each holding its own copy converged from
/// the same store, so two live flavors are two carriers of one single-use
/// chain. The acquire refuses instead of minting the permanent-death setup.
#[test]
fn fake_mode_refuses_the_second_flavor_of_one_profile() {
    let home = crate::testutil::HomeSandbox::new();
    let profile = home.home().join(".clauth/profiles/cx");
    fs::create_dir_all(&profile).expect("mkdir profile");
    fs::write(profile.join("auth.json"), b"{\"v\":1}").expect("seed store");

    with_link_mode(LinkMode::Fake, || {
        let shared = CodexRuntime::acquire("cx", Isolation::Shared).expect("shared acquire");
        let msg = match CodexRuntime::acquire("cx", Isolation::Isolated) {
            Ok(_) => panic!("the second flavor would be a second carrier"),
            Err(e) => e.to_string(),
        };
        assert_eq!(
            msg,
            "'cx' already has a live shared codex session, and this host cannot symlink. The two \
             would hold SEPARATE copies of one single-use chain, and the first refresh on either \
             strands the other. Close the shared session before starting this one"
        );
        // Same flavor is still fine — that IS one home.
        let second =
            CodexRuntime::acquire("cx", Isolation::Shared).expect("same flavor is one home");
        drop(second);
        drop(shared);
        // With nothing live, the other flavor is free again.
        drop(CodexRuntime::acquire("cx", Isolation::Isolated).expect("free once the first ends"));
    });
}

/// A crash leaves the fake-mode ISOLATED home holding the only copy of a
/// rotated chain — no Drop ran, so nothing converged it back to the store. The
/// next acquire must not wipe that home: its name is sid-free like every fake
/// name, so a guard testing only the SHARED bare stem read it as a recycled
/// per-session tree and deleted the live refresh token, after which the build
/// converged the store's SPENT token outward for codex to replay.
#[test]
fn a_crashed_fake_isolated_home_keeps_the_only_rotated_chain() {
    let home = crate::testutil::HomeSandbox::new();
    let profile = home.home().join(".clauth/profiles/cx");
    fs::create_dir_all(&profile).expect("mkdir profile");
    fs::write(profile.join("auth.json"), b"{\"spent\":true}").expect("seed store");

    with_link_mode(LinkMode::Fake, || {
        let session_home = {
            let runtime = CodexRuntime::acquire("cx", Isolation::Isolated).expect("acquire");
            let home = runtime.home().to_path_buf();
            assert_eq!(
                home,
                profile.join("codex-home-isolated"),
                "fake mode collapses the isolated home to its own bare stem"
            );
            // The session rotates the copy, then the process dies: forget the
            // runtime instead of dropping it, so no teardown converge runs.
            fs::write(home.join("auth.json"), b"{\"rotated\":true}").expect("rotate the copy");
            std::mem::forget(runtime);
            home
        };
        // The marker the dead session left behind reads as zero active.
        let _ = fs::remove_dir_all(profile.join("sessions-isolated"));

        let runtime = CodexRuntime::acquire("cx", Isolation::Isolated).expect("re-acquire");
        assert_eq!(
            fs::read(session_home.join("auth.json")).expect("read copy"),
            b"{\"rotated\":true}",
            "the only carrier of the rotated chain survived the re-acquire"
        );
        drop(runtime);
        assert_eq!(
            fs::read(profile.join("auth.json")).expect("read store"),
            b"{\"rotated\":true}",
            "and reached the store, instead of the spent token being replayed"
        );
    });
}

/// Under Fake the auth copy is a PROJECTION of the store: a re-capture
/// reaches the NEXT session because the build re-converges the copy at every
/// acquire, instead of copying once and never again.
#[test]
fn a_fake_mode_codex_home_reconverges_the_auth_projection() {
    let home = crate::testutil::HomeSandbox::new();
    let profile = home.home().join(".clauth/profiles/cx");
    fs::create_dir_all(&profile).expect("mkdir profile");
    fs::write(profile.join("auth.json"), b"{\"v\":1}").expect("seed store");

    with_link_mode(LinkMode::Fake, || {
        drop(CodexRuntime::acquire("cx", Isolation::Shared).expect("first acquire"));
        // A re-capture replaced the store between sessions (newer mtime).
        fs::write(profile.join("auth.json"), b"{\"v\":2}").expect("replace store");
        let runtime = CodexRuntime::acquire("cx", Isolation::Shared).expect("second acquire");
        let copy = runtime.home().join("auth.json");
        assert_eq!(
            fs::read(&copy).expect("read copy"),
            b"{\"v\":2}",
            "a newer store wins the converge at session start"
        );

        // The session ROTATES the copy — the fresh chain must reach the
        // store at teardown, not sit in a copy the next converge overwrites
        // with the store's spent token (the permanent-death direction).
        fs::write(&copy, b"{\"v\":3}").expect("rotate the copy");
        let later = std::time::SystemTime::now() + std::time::Duration::from_secs(10);
        crate::testutil::set_mtime(&copy, later);
        drop(runtime);
        assert_eq!(
            fs::read(profile.join("auth.json")).expect("read store"),
            b"{\"v\":3}",
            "a chain rotated in the copy reaches the store at teardown"
        );
    });
}

/// Each boundary hands a full tie (stampless bodies, equal mtimes) to its OWN
/// prior, pinned through `acquire` and `Drop` rather than the fn: the build
/// sends it the store's way, the teardown the copy's. Either site naming the
/// other's prior would ship the permanent-death direction at that boundary.
#[test]
fn the_fake_mode_boundaries_break_a_full_tie_by_their_own_prior() {
    let home = crate::testutil::HomeSandbox::new();
    let profile = home.home().join(".clauth/profiles/cx");
    fs::create_dir_all(&profile).expect("mkdir profile");
    let store = profile.join("auth.json");
    fs::write(&store, b"{\"v\":1}").expect("seed store");
    let same = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);

    with_link_mode(LinkMode::Fake, || {
        let copy = CodexRuntime::acquire("cx", Isolation::Shared)
            .expect("first acquire")
            .home()
            .join("auth.json");

        fs::write(&store, b"{\"side\":\"store\"}").expect("write store");
        fs::write(&copy, b"{\"side\":\"copy\"}").expect("write copy");
        crate::testutil::set_mtime(&store, same);
        crate::testutil::set_mtime(&copy, same);
        let runtime = CodexRuntime::acquire("cx", Isolation::Shared).expect("second acquire");
        assert_eq!(
            fs::read(&copy).expect("read copy"),
            b"{\"side\":\"store\"}",
            "build: the store wins a full tie through acquire"
        );

        fs::write(&store, b"{\"side\":\"store2\"}").expect("write store");
        fs::write(&copy, b"{\"side\":\"copy2\"}").expect("write copy");
        crate::testutil::set_mtime(&store, same);
        crate::testutil::set_mtime(&copy, same);
        drop(runtime);
        assert_eq!(
            fs::read(&store).expect("read store"),
            b"{\"side\":\"copy2\"}",
            "teardown: the copy wins a full tie through Drop"
        );
    });
}

/// An `auth.json` body carrying a `last_refresh` stamp and a marker that tells
/// the two sides apart.
fn stamped_auth(last_refresh: &str, marker: &str) -> String {
    format!(
        "{{\"tokens\":{{\"access_token\":\"at\",\"refresh_token\":\"{marker}\"}},\
         \"last_refresh\":\"{last_refresh}\"}}"
    )
}

/// The fake-mode convergence decides by the chain's own event first: the
/// later `last_refresh` wins in EITHER direction, whatever the mtimes say,
/// since a filesystem's mtime is not what stamped the rotation.
#[test]
fn fake_convergence_takes_the_later_last_refresh_over_the_newer_mtime() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let store = tmp.path().join("store.json");
    let copy = tmp.path().join("copy.json");
    let older = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
    let newer = older + std::time::Duration::from_secs(600);

    // The copy rotated later but carries the OLDER mtime.
    fs::write(&store, stamped_auth("2026-09-16T10:00:00Z", "rt.store")).expect("write store");
    fs::write(&copy, stamped_auth("2026-09-16T11:00:00Z", "rt.copy")).expect("write copy");
    crate::testutil::set_mtime(&store, newer);
    crate::testutil::set_mtime(&copy, older);
    converge_fake_codex_auth(&store, &copy, ConvergePrior::Build).expect("converge");
    assert_eq!(
        fs::read(&store).expect("read store"),
        stamped_auth("2026-09-16T11:00:00Z", "rt.copy").as_bytes(),
        "the copy's later rotation reaches the store past its older mtime"
    );

    // And the other way round: the store rotated later, the copy is the
    // newer file on disk.
    fs::write(&store, stamped_auth("2026-09-16T12:00:00Z", "rt.store2")).expect("write store");
    fs::write(&copy, stamped_auth("2026-09-16T11:30:00Z", "rt.copy2")).expect("write copy");
    crate::testutil::set_mtime(&store, older);
    crate::testutil::set_mtime(&copy, newer);
    converge_fake_codex_auth(&store, &copy, ConvergePrior::Teardown).expect("converge");
    assert_eq!(
        fs::read(&copy).expect("read copy"),
        stamped_auth("2026-09-16T12:00:00Z", "rt.store2").as_bytes(),
        "the store's later rotation reaches the copy past the teardown prior"
    );
}

/// With no stamp to read and equal mtimes (a coarse-mtime filesystem), the
/// boundary's own prior decides: at teardown the session was the only live
/// writer, so the copy wins; at build only the store is written between
/// sessions, so the store wins. A tie sent the wrong way hands a spent token
/// back to the side that rotated.
#[test]
fn fake_convergence_breaks_a_full_tie_by_the_boundarys_prior() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let store = tmp.path().join("store.json");
    let copy = tmp.path().join("copy.json");
    let same = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);

    fs::write(&store, b"{\"side\":\"store\"}").expect("write store");
    fs::write(&copy, b"{\"side\":\"copy\"}").expect("write copy");
    crate::testutil::set_mtime(&store, same);
    crate::testutil::set_mtime(&copy, same);
    converge_fake_codex_auth(&store, &copy, ConvergePrior::Teardown).expect("converge");
    assert_eq!(
        fs::read(&store).expect("read store"),
        b"{\"side\":\"copy\"}",
        "teardown: the session alone could have written, so the copy wins"
    );

    fs::write(&store, b"{\"side\":\"store\"}").expect("write store");
    fs::write(&copy, b"{\"side\":\"copy\"}").expect("write copy");
    crate::testutil::set_mtime(&store, same);
    crate::testutil::set_mtime(&copy, same);
    converge_fake_codex_auth(&store, &copy, ConvergePrior::Build).expect("converge");
    assert_eq!(
        fs::read(&copy).expect("read copy"),
        b"{\"side\":\"store\"}",
        "build: between sessions only the store is written, so the store wins"
    );

    // Content-equal is a no-op before any of it: neither file is rewritten.
    fs::write(&store, b"{\"same\":true}").expect("write store");
    fs::write(&copy, b"{\"same\":true}").expect("write copy");
    crate::testutil::set_mtime(&store, same);
    crate::testutil::set_mtime(&copy, same + std::time::Duration::from_secs(1));
    converge_fake_codex_auth(&store, &copy, ConvergePrior::Teardown).expect("converge");
    assert_eq!(
        fs::metadata(&store)
            .expect("stat")
            .modified()
            .expect("mtime"),
        same
    );
    assert_eq!(
        fs::metadata(&copy)
            .expect("stat")
            .modified()
            .expect("mtime"),
        same + std::time::Duration::from_secs(1),
        "matching bytes short-circuit before any copy"
    );
}

/// A crashed session's per-session home is collected once its marker reads
/// dead — and the bare durable store is never touched, whatever GC runs.
#[test]
fn gc_collects_a_dead_codex_home_and_spares_the_live_and_the_bare() {
    let home = crate::testutil::HomeSandbox::new();
    let profile = home.home().join(".clauth/profiles/cx");

    // A crash's leftover: home with a dead (empty) marker dir.
    let dead_home = profile.join("codex-home-4242-0");
    fs::create_dir_all(&dead_home).expect("mkdir dead home");
    fs::create_dir_all(profile.join("sessions-4242-0")).expect("mkdir dead marker");
    // One with NO marker dir at all (teardown died between removals).
    let orphan_home = profile.join("codex-home-isolated-4243-0");
    fs::create_dir_all(&orphan_home).expect("mkdir orphan home");
    // A LIVE one: locked pid in its marker dir.
    let live_home = profile.join("codex-home-4244-0");
    fs::create_dir_all(&live_home).expect("mkdir live home");
    let live_marker = profile.join("sessions-4244-0");
    fs::create_dir_all(&live_marker).expect("mkdir live marker");
    let pid = open_pid_file(&live_marker.join("4244-0")).expect("open pid");
    pid.lock().expect("lock pid");
    // The durable store, and a name that only LOOKS per-session.
    let bare = profile.join("codex-home");
    fs::create_dir_all(&bare).expect("mkdir bare store");
    let lookalike = profile.join("codex-home-notasid");
    fs::create_dir_all(&lookalike).expect("mkdir lookalike");

    gc_stale_runtimes();

    assert!(!dead_home.exists(), "a dead session's home is collected");
    assert!(
        !orphan_home.exists(),
        "a markerless home is a crash leftover, collected"
    );
    assert!(live_home.exists(), "a live session's home is spared");
    assert!(bare.exists(), "the durable store is never GC territory");
    assert!(
        lookalike.exists(),
        "a non-sid suffix is not a per-session home"
    );
}

/// A live session's liveness marker: an open file holding the same exclusive
/// flock `ProfileRuntime::acquire` takes, so `prune_stale_sessions` counts it
/// alive. The returned handle must stay in scope.
fn live_marker(path: &std::path::Path) -> fs::File {
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .unwrap();
    file.lock().unwrap();
    file
}

/// The tombstone name GC renames a dead runtime tree to must fall through both
/// strict pairing predicates, or a later pass could re-pair it as a runtime and
/// `remove_dir_all` it unrescued, while the stranded-tombstone sweep must still
/// match every flavor of it, shared per-session trees included, or a crash
/// mid-rescue strands the tree. The `.rescuing` suffix does the first because a
/// `.` is not in [`is_session_id`]'s alphabet.
#[test]
fn the_rescue_tombstone_name_is_rejected_by_the_pairing_predicate() {
    for base in [
        "runtime",
        "runtime-isolated",
        "runtime-4242-7",
        "runtime-isolated-4242-7",
    ] {
        let tombstone = format!("{base}{RESCUE_TOMBSTONE_SUFFIX}");
        assert!(
            !is_paired_dir_name(&tombstone, RUNTIME_STEM),
            "{tombstone} must not read as a runtime dir"
        );
    }
    assert!(is_rescuing_runtime_dir_name("runtime-isolated.rescuing"));
    assert!(is_rescuing_runtime_dir_name(
        "runtime-isolated-4242-7.rescuing"
    ));
    assert!(is_rescuing_runtime_dir_name("runtime.rescuing"));
    assert!(is_rescuing_runtime_dir_name("runtime-4242-7.rescuing"));
    assert!(!is_rescuing_runtime_dir_name("runtime-4242-7"));
    assert!(!is_rescuing_runtime_dir_name("sessions-isolated.rescuing"));
    assert!(!is_rescuing_runtime_dir_name("sessions-4242-7.rescuing"));
}

/// The defect: a stale isolated tree with no live marker is deleted unrescued,
/// losing its transcript. GC must lift it into the global store before removing
/// it, and leave no tombstone behind.
#[test]
fn gc_rescues_an_isolated_tree_with_no_live_marker() {
    let sb = HomeSandbox::new();
    let claude_home = sb.home().join(".claude");
    let runtime = sb.home().join(".clauth/profiles/iso/runtime-isolated");
    let sessions = sb.home().join(".clauth/profiles/iso/sessions-isolated");
    fs::create_dir_all(runtime.join("projects/-w-iso")).unwrap();
    fs::write(runtime.join("projects/-w-iso/s1.jsonl"), "transcript").unwrap();
    fs::create_dir_all(runtime.join("shell-snapshots")).unwrap();
    fs::write(runtime.join("shell-snapshots/snap.sh"), "iso shell").unwrap();
    fs::create_dir_all(&sessions).unwrap();
    fs::write(sessions.join("dead"), b"").unwrap();

    gc_stale_runtimes();

    assert_eq!(
        fs::read_to_string(claude_home.join("projects/-w-iso/s1.jsonl")).unwrap(),
        "transcript",
        "the transcript must land in the resumable global store"
    );
    assert_eq!(
        fs::read_to_string(claude_home.join("shell-snapshots/snap.sh")).unwrap(),
        "iso shell",
        "the sidecar must land in the global store"
    );
    assert!(
        !runtime.exists(),
        "the isolated tree must be gone after rescue"
    );
    assert!(!sessions.exists(), "the marker dir must be gone");
    assert!(
        !sb.home()
            .join(".clauth/profiles/iso/runtime-isolated.rescuing")
            .exists(),
        "no tombstone may be left behind"
    );
}

/// The per-session shape production meets: `runtime-isolated-<sid>` beside
/// `sessions-isolated-<sid>`. The rename-and-rescue must reach it, not only the
/// legacy bare upgrade-window pair.
#[test]
fn gc_rescues_a_per_session_isolated_tree_with_no_live_marker() {
    let sb = HomeSandbox::new();
    let claude_home = sb.home().join(".claude");
    let runtime = sb
        .home()
        .join(".clauth/profiles/iso/runtime-isolated-4242-7");
    let sessions = sb
        .home()
        .join(".clauth/profiles/iso/sessions-isolated-4242-7");
    fs::create_dir_all(runtime.join("projects/-w-iso")).unwrap();
    fs::write(runtime.join("projects/-w-iso/s1.jsonl"), "transcript").unwrap();
    fs::create_dir_all(runtime.join("shell-snapshots")).unwrap();
    fs::write(runtime.join("shell-snapshots/snap.sh"), "iso shell").unwrap();
    fs::create_dir_all(&sessions).unwrap();

    gc_stale_runtimes();

    assert_eq!(
        fs::read_to_string(claude_home.join("projects/-w-iso/s1.jsonl")).unwrap(),
        "transcript",
        "the per-session transcript must land in the resumable global store"
    );
    assert_eq!(
        fs::read_to_string(claude_home.join("shell-snapshots/snap.sh")).unwrap(),
        "iso shell",
        "the per-session sidecar must land in the global store"
    );
    assert!(
        !runtime.exists(),
        "the per-session tree must be gone after rescue"
    );
    assert!(
        !sessions.exists(),
        "the per-session marker dir must be gone"
    );
    assert!(
        !sb.home()
            .join(".clauth/profiles/iso/runtime-isolated-4242-7.rescuing")
            .exists(),
        "no per-session tombstone may be left behind"
    );
}

/// A dead shared copy-host tree: the bare `runtime` + `sessions` pair a
/// `clauth start` killed before its final sync leaves behind (#136), holding a
/// transcript only the tree has and a `history.jsonl` newer than the global
/// one (not a transcript or a sidecar, so no rescue carries it). Its copied
/// `CLAUDE.md` is what marks the tree as a copy ([`link_mode_of`] reads
/// `Fake`). Returns `(claude_home, runtime, sessions)`.
fn dead_shared_fake_tree(root: &Path, profile: &str) -> (PathBuf, PathBuf, PathBuf) {
    let claude_home = fake_claude_home(root);
    let profile_dir = root.join(".clauth/profiles").join(profile);
    let runtime = profile_dir.join("runtime");
    let sessions = profile_dir.join("sessions");
    fs::write(claude_home.join("CLAUDE.md"), "memory").unwrap();
    fs::create_dir_all(&runtime).unwrap();
    fs::write(runtime.join("CLAUDE.md"), "memory").unwrap();
    let base = SystemTime::now() - Duration::from_secs(3600);
    fs::write(claude_home.join("history.jsonl"), "old\n").unwrap();
    set_mtime(&claude_home.join("history.jsonl"), base);
    fs::create_dir_all(runtime.join("projects/-w-kill")).unwrap();
    fs::write(runtime.join("projects/-w-kill/s1.jsonl"), "transcript").unwrap();
    fs::write(runtime.join("history.jsonl"), "old\nnew\n").unwrap();
    set_mtime(
        &runtime.join("history.jsonl"),
        base + Duration::from_secs(60),
    );
    fs::create_dir_all(&sessions).unwrap();
    fs::write(sessions.join("dead"), b"").unwrap();
    (claude_home, runtime, sessions)
}

/// #136: the GC sweep of a dead shared copy-host tree publishes what only the
/// tree holds before it removes the tree, so a killed start loses nothing.
#[test]
fn gc_publishes_a_dead_shared_fake_tree_before_removing_it() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        with_link_mode(LinkMode::Fake, || {
            let (claude_home, runtime, sessions) = dead_shared_fake_tree(tmp.path(), "killed");

            gc_stale_runtimes();

            assert_eq!(
                fs::read_to_string(claude_home.join("projects/-w-kill/s1.jsonl")).unwrap(),
                "transcript",
                "the tree-only transcript must land in the global store"
            );
            assert!(!runtime.exists(), "the dead tree must be gone");
            assert!(!sessions.exists(), "the marker dir must be gone");
        });
    });
}

/// #136, the acquire leg: a start that finds the dead shared copy-host tree at
/// its path rescues it out of the state flock's hold, before `acquire`
/// returns, so the transcript reaches the global store and no tombstone stays.
#[test]
fn acquire_publishes_a_dead_shared_fake_tree_before_its_rebuild() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        with_link_mode(LinkMode::Fake, || {
            let (claude_home, runtime, _sessions) = dead_shared_fake_tree(tmp.path(), "relaunch");
            let profile = configured_profile("relaunch");

            let rt =
                ProfileRuntime::acquire(&profile, Isolation::Shared, &[], false, Launcher::Own)
                    .expect("acquire");

            assert_eq!(
                fs::read_to_string(claude_home.join("projects/-w-kill/s1.jsonl")).unwrap(),
                "transcript",
                "the tree-only transcript must land in the global store"
            );
            assert_eq!(
                rt.config_dir(),
                runtime.as_path(),
                "the shared copy-host session runs out of the bare tree"
            );
            assert!(
                !runtime.with_file_name("runtime.rescuing").exists(),
                "a rescued tombstone is collected"
            );

            drop(rt);
        });
    });
}

/// The linking transport needs no publish: its shared tree links `projects`
/// into `~/.claude`, so a file only the tree holds is not one the GC copies out.
/// Pins that the #136 publish stays gated to copy hosts.
#[test]
fn gc_publishes_nothing_from_a_dead_linked_tree() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        with_link_mode(LinkMode::Real, || {
            let claude_home = fake_claude_home(tmp.path());
            let profile_dir = tmp.path().join(".clauth/profiles/linked");
            let runtime = profile_dir.join("runtime-4242-7");
            let sessions = profile_dir.join("sessions-4242-7");
            fs::create_dir_all(&runtime).unwrap();
            fs::write(runtime.join("stray.txt"), "tree only").unwrap();
            fs::create_dir_all(&sessions).unwrap();

            gc_stale_runtimes();

            assert!(!runtime.exists(), "the dead tree must be gone");
            assert!(
                !claude_home.join("stray.txt").exists(),
                "a linked tree's GC must publish nothing into ~/.claude"
            );
        });
    });
}

/// A publish that stops part-way must not cost the tree: the copy host's
/// tombstone keeps the transcript the walk could not land, and a later sweep
/// finishes it once the fault clears. The fault is a regular file where
/// `~/.claude/projects` belongs, so the walk cannot create the project dir.
#[test]
fn gc_keeps_a_copy_host_tombstone_whose_publish_failed_and_finishes_it_later() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        with_link_mode(LinkMode::Fake, || {
            let (claude_home, runtime, sessions) = dead_shared_fake_tree(tmp.path(), "blocked");
            let tombstone = runtime.with_file_name("runtime.rescuing");
            fs::write(claude_home.join("projects"), "not a dir").unwrap();

            gc_stale_runtimes();

            assert_eq!(
                fs::read_to_string(tombstone.join("projects/-w-kill/s1.jsonl"))
                    .ok()
                    .as_deref(),
                Some("transcript"),
                "a failed publish must keep the transcript it could not land"
            );
            assert!(!runtime.exists(), "the tree leaves its live name first");
            assert!(!sessions.exists(), "the marker dir must be gone");

            fs::remove_file(claude_home.join("projects")).unwrap();
            gc_stale_runtimes();

            assert_eq!(
                fs::read_to_string(claude_home.join("projects/-w-kill/s1.jsonl"))
                    .ok()
                    .as_deref(),
                Some("transcript"),
                "the next sweep must publish the stranded tombstone"
            );
            assert!(!tombstone.exists(), "a published tombstone is collected");
        });
    });
}

/// The acquire leg's twin: a rescue that fails keeps the dead tree's tombstone,
/// so the transcript survives beside the rebuilt tree and a later sweep lands
/// it once the fault clears.
#[test]
fn acquire_keeps_a_dead_shared_fake_tree_whose_publish_fails() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        with_link_mode(LinkMode::Fake, || {
            let (claude_home, runtime, _sessions) =
                dead_shared_fake_tree(tmp.path(), "blocked-relaunch");
            let tombstone = runtime.with_file_name("runtime.rescuing");
            let profile = configured_profile("blocked-relaunch");
            fs::write(claude_home.join("projects"), "not a dir").unwrap();

            let rt =
                ProfileRuntime::acquire(&profile, Isolation::Shared, &[], false, Launcher::Own)
                    .expect("acquire");

            assert_eq!(
                fs::read_to_string(tombstone.join("projects/-w-kill/s1.jsonl"))
                    .ok()
                    .as_deref(),
                Some("transcript"),
                "a tree whose rescue failed must keep its transcript in the tombstone"
            );

            drop(rt);
            fs::remove_file(claude_home.join("projects")).unwrap();
            gc_stale_runtimes();

            assert_eq!(
                fs::read_to_string(claude_home.join("projects/-w-kill/s1.jsonl"))
                    .ok()
                    .as_deref(),
                Some("transcript"),
                "the next sweep must land the kept tombstone's transcript"
            );
            assert!(!tombstone.exists(), "a rescued tombstone is collected");
        });
    });
}

/// A recycled `<pid>-<seq>` lands an isolated acquire on a dead session's tree
/// no sweep has rescued yet. The wipe must lift its transcript into the global
/// store first. The next sids this process mints are pre-seeded so the acquire
/// meets one of them, whichever it draws.
#[test]
fn acquire_rescues_a_dead_isolated_tree_at_its_own_path() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        with_link_mode(LinkMode::Fake, || {
            let claude_home = fake_claude_home(tmp.path());
            let profile = configured_profile("recycled");
            let profile_dir = tmp.path().join(".clauth/profiles/recycled");
            let pid = std::process::id();
            let next = SESSION_SEQ.load(std::sync::atomic::Ordering::Relaxed);
            let seeded = next..next + 8;
            for seq in seeded.clone() {
                let projects = profile_dir
                    .join(format!("runtime-isolated-{pid}-{seq}"))
                    .join("projects")
                    .join(format!("-w-{seq}"));
                fs::create_dir_all(&projects).unwrap();
                fs::write(projects.join("s1.jsonl"), "dead transcript").unwrap();
                fs::create_dir_all(profile_dir.join(format!("sessions-isolated-{pid}-{seq}")))
                    .unwrap();
            }

            let rt =
                ProfileRuntime::acquire(&profile, Isolation::Isolated, &[], false, Launcher::Own)
                    .expect("acquire");

            let seq = seeded
                .clone()
                .find(|seq| {
                    rt.config_dir() == profile_dir.join(format!("runtime-isolated-{pid}-{seq}"))
                })
                .unwrap_or_else(|| {
                    panic!(
                        "{} is none of the pre-seeded trees",
                        rt.config_dir().display()
                    )
                });
            assert_eq!(
                fs::read_to_string(
                    claude_home
                        .join("projects")
                        .join(format!("-w-{seq}"))
                        .join("s1.jsonl")
                )
                .ok()
                .as_deref(),
                Some("dead transcript"),
                "the dead isolated tree's transcript must reach the global store"
            );
            assert!(
                !rt.config_dir()
                    .join("projects")
                    .join(format!("-w-{seq}"))
                    .exists(),
                "the dead tree is still wiped before the rebuild"
            );

            drop(rt);
        });
    });
}

/// The teardown leg: a copy host's last session whose final tick fails must not
/// take the tree with it, and a later sweep publishes what the tick could not.
/// The watchdog is stopped before the transcript lands so only the final tick
/// can meet it; the fault is a regular file where `~/.claude/projects` belongs.
#[test]
fn drop_spares_a_copy_host_tree_whose_final_tick_failed() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        with_link_mode(LinkMode::Fake, || {
            let claude_home = fake_claude_home(tmp.path());
            let profile = configured_profile("torn-down");
            let mut rt =
                ProfileRuntime::acquire(&profile, Isolation::Shared, &[], false, Launcher::Own)
                    .expect("acquire");
            drop(rt.watchdog_signal.take());
            if let Some(handle) = rt.watchdog_handle.take() {
                handle.join().expect("join the watchdog");
            }
            let runtime = rt.config_dir().to_path_buf();
            fs::create_dir_all(runtime.join("projects/-w-exit")).unwrap();
            fs::write(runtime.join("projects/-w-exit/s1.jsonl"), "transcript").unwrap();
            fs::write(claude_home.join("projects"), "not a dir").unwrap();

            drop(rt);

            assert_eq!(
                fs::read_to_string(runtime.join("projects/-w-exit/s1.jsonl"))
                    .ok()
                    .as_deref(),
                Some("transcript"),
                "a tree whose final tick failed must survive its teardown"
            );

            fs::remove_file(claude_home.join("projects")).unwrap();
            gc_stale_runtimes();

            assert_eq!(
                fs::read_to_string(claude_home.join("projects/-w-exit/s1.jsonl"))
                    .ok()
                    .as_deref(),
                Some("transcript"),
                "the next sweep must publish the spared tree"
            );
            assert!(!runtime.exists(), "the published tree is collected");
        });
    });
}

/// The copy-host test reads the tree, never the sweeping process: a copy
/// host's dead tree swept by a process that can link (Developer Mode turned on
/// after the kill, or an elevated clauth) still holds the only copy of its
/// transcript, so it is rescued all the same.
#[test]
fn gc_rescues_a_copy_host_tree_that_a_linking_sweep_finds() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let (claude_home, runtime, sessions) = dead_shared_fake_tree(tmp.path(), "flipped");

        with_link_mode(LinkMode::Real, gc_stale_runtimes);

        assert_eq!(
            fs::read_to_string(claude_home.join("projects/-w-kill/s1.jsonl"))
                .ok()
                .as_deref(),
            Some("transcript"),
            "a copy host's transcript must survive a sweep that can link"
        );
        assert!(!runtime.exists(), "the dead tree must be gone");
        assert!(!sessions.exists(), "the marker dir must be gone");
    });
}

/// Sweep a dead copy-host tree whose `projects/-w-kill/s2.jsonl` holds `tree`
/// (stamped `tree_age` before now) beside a global copy holding `global`
/// (stamped `global_age` before now). The two stamps are set against the
/// outcome each test pins, so an mtime rule reds where the byte rule passes.
/// Returns `~/.claude`.
fn sweep_transcript_pair(
    root: &Path,
    tree: &str,
    tree_age: u64,
    global: &str,
    global_age: u64,
) -> PathBuf {
    let (claude_home, runtime, _sessions) = dead_shared_fake_tree(root, "pair");
    let now = SystemTime::now();
    let global_file = claude_home.join("projects/-w-kill/s2.jsonl");
    fs::create_dir_all(global_file.parent().unwrap()).unwrap();
    fs::write(&global_file, global).unwrap();
    set_mtime(&global_file, now - Duration::from_secs(global_age));
    let tree_file = runtime.join("projects/-w-kill/s2.jsonl");
    fs::write(&tree_file, tree).unwrap();
    set_mtime(&tree_file, now - Duration::from_secs(tree_age));

    with_link_mode(LinkMode::Fake, gc_stale_runtimes);

    assert!(!runtime.exists(), "the dead tree must be gone");
    claude_home
}

/// The killed session's own transcript: the global copy is what its last tick
/// published, the tree holds that plus the lines after it. The longer file
/// replaces the global one under its own name, so `--resume <id>` opens it,
/// even when the global copy carries the newer stamp.
#[test]
fn gc_replaces_a_global_transcript_the_dead_tree_extends() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let claude_home = sweep_transcript_pair(tmp.path(), "l1\nl2\n", 600, "l1\n", 60);

        assert_eq!(
            fs::read_to_string(claude_home.join("projects/-w-kill/s2.jsonl")).unwrap(),
            "l1\nl2\n",
            "the tree's unsynced tail must land under the transcript's own name"
        );
        assert!(
            !claude_home
                .join("projects/-w-kill/s2.rescued-0.jsonl")
                .exists(),
            "an extension is no collision: no sibling"
        );
    });
}

/// Another session's transcript the dead tree holds a stale copy of: the global
/// file grew after the tree copied it. The stale copy is dropped, even when it
/// carries the newer stamp.
#[test]
fn gc_drops_a_dead_trees_stale_copy_of_a_longer_global_transcript() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let claude_home = sweep_transcript_pair(tmp.path(), "l1\n", 60, "l1\nl2\n", 600);

        assert_eq!(
            fs::read_to_string(claude_home.join("projects/-w-kill/s2.jsonl")).unwrap(),
            "l1\nl2\n",
            "the longer global transcript must stay as it is"
        );
        assert!(
            !claude_home
                .join("projects/-w-kill/s2.rescued-0.jsonl")
                .exists(),
            "a stale prefix is no collision: no sibling"
        );
    });
}

/// Two copies that each hold lines the other lacks: neither is lost. The global
/// one stays, the tree's lands beside it, whichever carries the newer stamp.
#[test]
fn gc_keeps_a_diverged_dead_tree_transcript_beside_the_global_one() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let claude_home = sweep_transcript_pair(tmp.path(), "l1\nB\n", 600, "l1\nA\n", 60);

        assert_eq!(
            fs::read_to_string(claude_home.join("projects/-w-kill/s2.jsonl")).unwrap(),
            "l1\nA\n",
            "the global transcript must stay as it is"
        );
        assert_eq!(
            fs::read_to_string(claude_home.join("projects/-w-kill/s2.rescued-0.jsonl"))
                .ok()
                .as_deref(),
            Some("l1\nB\n"),
            "the tree's diverged copy must land beside it"
        );
    });
}

/// A copy host's tree holds a copy of every global sidecar: one the global store
/// already has is a stale copy and is dropped, whichever stamp is newer; one
/// only the tree has is the killed session's and moves.
#[test]
fn gc_moves_only_new_sidecars_out_of_a_dead_copy_host_tree() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let (claude_home, runtime, _sessions) = dead_shared_fake_tree(tmp.path(), "sidecars");
        let now = SystemTime::now();
        fs::create_dir_all(claude_home.join("plans")).unwrap();
        fs::write(claude_home.join("plans/p.md"), "global").unwrap();
        set_mtime(
            &claude_home.join("plans/p.md"),
            now - Duration::from_secs(600),
        );
        fs::create_dir_all(runtime.join("plans")).unwrap();
        fs::write(runtime.join("plans/p.md"), "stale copy").unwrap();
        set_mtime(&runtime.join("plans/p.md"), now - Duration::from_secs(60));
        fs::write(runtime.join("plans/q.md"), "tree only").unwrap();

        with_link_mode(LinkMode::Fake, gc_stale_runtimes);

        assert_eq!(
            fs::read_to_string(claude_home.join("plans/p.md")).unwrap(),
            "global",
            "a sidecar the global store has must stay as it is"
        );
        assert!(
            !claude_home.join("plans/p.rescued-0.md").exists(),
            "a stale sidecar copy must not land as a sibling"
        );
        assert_eq!(
            fs::read_to_string(claude_home.join("plans/q.md"))
                .ok()
                .as_deref(),
            Some("tree only"),
            "a sidecar only the tree has must move"
        );
        assert!(!runtime.exists(), "the dead tree must be gone");
    });
}

/// Only transcripts and sidecars leave a dead copy-host tree: a skill the
/// operator deleted from `~/.claude` after the kill stays deleted, and the
/// tree's newer `history.jsonl` is not published.
#[test]
fn gc_carries_nothing_else_out_of_a_dead_copy_host_tree() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let (claude_home, runtime, _sessions) = dead_shared_fake_tree(tmp.path(), "deleted");
        fs::create_dir_all(runtime.join("skills/gone")).unwrap();
        fs::write(runtime.join("skills/gone/SKILL.md"), "removed since").unwrap();

        with_link_mode(LinkMode::Fake, gc_stale_runtimes);

        assert!(
            !claude_home.join("skills/gone").exists(),
            "a deleted skill must not come back"
        );
        assert_eq!(
            fs::read_to_string(claude_home.join("history.jsonl")).unwrap(),
            "old\n",
            "the tree's history.jsonl must not be published"
        );
        assert!(!runtime.exists(), "the dead tree must be gone");
    });
}

/// The rescue is one-way: a tombstone kept after a failed rescue never takes in
/// what `~/.claude` holds, so a sparse tree killed mid-build costs no reverse
/// copy of the global store.
#[test]
fn gc_copies_nothing_into_a_copy_host_tombstone_it_keeps() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let (claude_home, runtime, _sessions) = dead_shared_fake_tree(tmp.path(), "sparse");
        let tombstone = runtime.with_file_name("runtime.rescuing");
        fs::create_dir_all(claude_home.join("agents")).unwrap();
        fs::write(claude_home.join("agents/a.md"), "global only").unwrap();
        fs::write(claude_home.join("projects"), "not a dir").unwrap();

        with_link_mode(LinkMode::Fake, gc_stale_runtimes);

        assert_eq!(
            fs::read_to_string(tombstone.join("projects/-w-kill/s1.jsonl"))
                .ok()
                .as_deref(),
            Some("transcript"),
            "a failed rescue must keep the tombstone"
        );
        assert!(
            !tombstone.join("agents").exists(),
            "a kept tombstone must not receive the global store's entries"
        );
    });
}

/// An isolated tombstone whose transcript rescue failed is kept like a shared
/// one, and the next sweep lands it: removing it would delete the only copy.
#[test]
fn gc_keeps_an_isolated_tombstone_whose_transcript_rescue_failed() {
    let sb = HomeSandbox::new();
    let claude_home = sb.home().join(".claude");
    let profile_dir = sb.home().join(".clauth/profiles/iso");
    let runtime = profile_dir.join("runtime-isolated-4242-7");
    let tombstone = profile_dir.join("runtime-isolated-4242-7.rescuing");
    fs::create_dir_all(runtime.join("projects/-w-iso")).unwrap();
    fs::write(runtime.join("projects/-w-iso/s1.jsonl"), "transcript").unwrap();
    fs::create_dir_all(profile_dir.join("sessions-isolated-4242-7")).unwrap();
    fs::create_dir_all(&claude_home).unwrap();
    fs::write(claude_home.join("projects"), "not a dir").unwrap();

    gc_stale_runtimes();

    assert_eq!(
        fs::read_to_string(tombstone.join("projects/-w-iso/s1.jsonl"))
            .ok()
            .as_deref(),
        Some("transcript"),
        "a failed transcript rescue must keep the tombstone"
    );

    fs::remove_file(claude_home.join("projects")).unwrap();
    gc_stale_runtimes();

    assert_eq!(
        fs::read_to_string(claude_home.join("projects/-w-iso/s1.jsonl"))
            .ok()
            .as_deref(),
        Some("transcript"),
        "the next sweep must land the kept tombstone's transcript"
    );
    assert!(!tombstone.exists(), "a rescued tombstone is collected");
}

/// A clean isolated exit, in the shape `start::run` and the MCP delegate use
/// (the teardown rescue, then the drop): a rescue that could not land the
/// transcript must not let the drop delete its only copy; the next sweep lands
/// it once the fault clears.
#[test]
fn an_isolated_exit_keeps_a_transcript_its_rescue_could_not_land() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        with_link_mode(LinkMode::Fake, || {
            let claude_home = fake_claude_home(tmp.path());
            let profile = configured_profile("isoexit");
            let rt =
                ProfileRuntime::acquire(&profile, Isolation::Isolated, &[], false, Launcher::Own)
                    .expect("acquire");
            let runtime = rt.config_dir().to_path_buf();
            let tombstone = runtime.with_file_name(format!(
                "{}.rescuing",
                runtime.file_name().unwrap().to_str().unwrap()
            ));
            fs::create_dir_all(runtime.join("projects/-w-x")).unwrap();
            fs::write(runtime.join("projects/-w-x/s.jsonl"), "only copy").unwrap();
            fs::write(claude_home.join("projects"), "not a dir").unwrap();

            let _ = crate::start::rescue_teardown(rt.config_dir(), rt.sessions_dir(), &claude_home);
            drop(rt);

            assert!(
                runtime.join("projects/-w-x/s.jsonl").exists()
                    || tombstone.join("projects/-w-x/s.jsonl").exists(),
                "the only copy of the transcript must survive the teardown"
            );

            fs::remove_file(claude_home.join("projects")).unwrap();
            gc_stale_runtimes();

            assert_eq!(
                fs::read_to_string(claude_home.join("projects/-w-x/s.jsonl"))
                    .ok()
                    .as_deref(),
                Some("only copy"),
                "the next sweep must land the kept transcript"
            );
        });
    });
}

/// A `profiles/<p>/runtime` that is itself a link (a Windows junction needs no
/// privilege) points at a store the rescue would walk as its own source: the
/// sweep removes the link alone and never walks through it, so the global
/// store it points at keeps every file.
#[test]
fn gc_unlinks_a_linked_runtime_root_without_walking_it() {
    let sb = HomeSandbox::new();
    let claude_home = sb.home().join(".claude");
    fs::create_dir_all(claude_home.join("projects/-w-g")).unwrap();
    fs::write(claude_home.join("projects/-w-g/s.jsonl"), "global").unwrap();
    fs::create_dir_all(claude_home.join("plans")).unwrap();
    fs::write(claude_home.join("plans/p.md"), "global plan").unwrap();
    let profile_dir = sb.home().join(".clauth/profiles/lnk");
    fs::create_dir_all(&profile_dir).unwrap();
    pose_dir_link(&profile_dir.join("runtime"), &claude_home);
    fs::create_dir_all(profile_dir.join("sessions")).unwrap();

    gc_stale_runtimes();

    assert_eq!(
        fs::read_to_string(claude_home.join("projects/-w-g/s.jsonl"))
            .ok()
            .as_deref(),
        Some("global"),
        "a global transcript must survive a sweep of a linked runtime root"
    );
    assert_eq!(
        fs::read_to_string(claude_home.join("plans/p.md"))
            .ok()
            .as_deref(),
        Some("global plan"),
        "a global sidecar must survive a sweep of a linked runtime root"
    );
    assert!(
        profile_dir.join("runtime").symlink_metadata().is_err(),
        "the dead link itself is collected"
    );
    assert!(
        !profile_dir.join("runtime.rescuing").exists(),
        "a linked root is never renamed to a tombstone"
    );
}

/// A linking host's legacy bare tree built before `~/.claude/projects` existed
/// holds a real `projects`: its transcript lives only in the tree, so it is
/// rescued like a copy host's, whatever the sweeping process can create.
#[test]
fn gc_rescues_a_legacy_linked_bare_tree_with_a_real_projects() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let claude_home = fake_claude_home(tmp.path());
        fs::create_dir_all(claude_home.join("skills")).unwrap();
        let profile_dir = tmp.path().join(".clauth/profiles/legacy");
        let runtime = profile_dir.join("runtime");
        fs::create_dir_all(runtime.join("projects/-w-l")).unwrap();
        fs::write(runtime.join("projects/-w-l/t.jsonl"), "tree only").unwrap();
        pose_dir_link(&runtime.join("skills"), &claude_home.join("skills"));
        fs::create_dir_all(profile_dir.join("sessions")).unwrap();

        with_link_mode(LinkMode::Real, gc_stale_runtimes);

        assert_eq!(
            fs::read_to_string(claude_home.join("projects/-w-l/t.jsonl"))
                .ok()
                .as_deref(),
            Some("tree only"),
            "a legacy linked tree's tree-only transcript must be rescued"
        );
        assert!(!runtime.exists(), "the dead tree must be gone");
    });
}

/// A copy host's session that wrote sidecars but no transcript yet has no
/// `projects` in its tree: a failed final tick still spares the tree, and the
/// next sweep lands the sidecar.
#[test]
fn drop_spares_a_copy_host_tree_without_projects_whose_final_tick_failed() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        with_link_mode(LinkMode::Fake, || {
            let claude_home = fake_claude_home(tmp.path());
            let profile = configured_profile("noproj");
            let mut rt =
                ProfileRuntime::acquire(&profile, Isolation::Shared, &[], false, Launcher::Own)
                    .expect("acquire");
            drop(rt.watchdog_signal.take());
            if let Some(handle) = rt.watchdog_handle.take() {
                handle.join().expect("join the watchdog");
            }
            let runtime = rt.config_dir().to_path_buf();
            assert!(
                runtime.join("projects").symlink_metadata().is_err(),
                "fixture: the tree holds no projects"
            );
            fs::create_dir_all(runtime.join("plans")).unwrap();
            fs::write(runtime.join("plans/new.md"), "tree only").unwrap();
            fs::write(claude_home.join("plans"), "not a dir").unwrap();

            drop(rt);

            assert_eq!(
                fs::read_to_string(runtime.join("plans/new.md"))
                    .ok()
                    .as_deref(),
                Some("tree only"),
                "a tree whose final tick failed must survive its teardown"
            );

            fs::remove_file(claude_home.join("plans")).unwrap();
            gc_stale_runtimes();

            assert_eq!(
                fs::read_to_string(claude_home.join("plans/new.md"))
                    .ok()
                    .as_deref(),
                Some("tree only"),
                "the next sweep must land the spared sidecar"
            );
        });
    });
}

/// A shared tree's rescue writes through a sidecar dir the operator linked out
/// of `~/.claude`, the way the live tick does, so a killed copy-host session's
/// new plan lands where the operator keeps them.
#[test]
fn gc_rescues_a_copy_host_sidecar_through_an_operator_linked_dir() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let (claude_home, runtime, _sessions) = dead_shared_fake_tree(tmp.path(), "linkplans");
        let elsewhere = tmp.path().join("dotfiles/plans");
        fs::create_dir_all(&elsewhere).unwrap();
        pose_dir_link(&claude_home.join("plans"), &elsewhere);
        fs::create_dir_all(runtime.join("plans")).unwrap();
        fs::write(runtime.join("plans/new.md"), "tree only plan").unwrap();

        with_link_mode(LinkMode::Fake, gc_stale_runtimes);

        assert_eq!(
            fs::read_to_string(elsewhere.join("new.md")).ok().as_deref(),
            Some("tree only plan"),
            "the new plan must land through the operator's link"
        );
        assert!(
            claude_home
                .join("plans")
                .symlink_metadata()
                .is_ok_and(|m| !m.file_type().is_dir()),
            "the operator's link stays a link"
        );
        assert!(!runtime.exists(), "the dead tree must be gone");
    });
}

/// A dead per-session tree on a linking host links what `~/.claude` held at
/// build; a sidecar dir it lacked then (a first-ever `plans/`) is a real dir
/// only that tree holds, rescued before the tree goes.
#[test]
fn gc_rescues_a_new_sidecar_dir_from_a_dead_linked_session_tree() {
    let sb = HomeSandbox::new();
    let claude_home = sb.home().join(".claude");
    fs::create_dir_all(claude_home.join("projects")).unwrap();
    let profile_dir = sb.home().join(".clauth/profiles/lnk");
    let runtime = profile_dir.join("runtime-4242-7");
    fs::create_dir_all(&runtime).unwrap();
    pose_dir_link(&runtime.join("projects"), &claude_home.join("projects"));
    fs::create_dir_all(runtime.join("plans")).unwrap();
    fs::write(runtime.join("plans/new.md"), "tree only").unwrap();
    fs::create_dir_all(profile_dir.join("sessions-4242-7")).unwrap();

    gc_stale_runtimes();

    assert_eq!(
        fs::read_to_string(claude_home.join("plans/new.md"))
            .ok()
            .as_deref(),
        Some("tree only"),
        "the dead session's new sidecar must land in the global store"
    );
    assert!(!runtime.exists(), "the dead tree must be gone");
}

/// The teardown leg of the same rule: a linking host's session ending with a
/// sidecar dir `~/.claude` lacked at build hands it to the global store before
/// its tree is removed.
#[test]
fn a_linked_session_teardown_rescues_a_new_sidecar_dir() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let claude_home = fake_claude_home(tmp.path());
        if !host_poses(&claude_home, "a linking host's per-session tree") {
            return;
        }
        with_link_mode(LinkMode::Real, || {
            let profile = configured_profile("linked-exit");
            let rt =
                ProfileRuntime::acquire(&profile, Isolation::Shared, &[], false, Launcher::Own)
                    .expect("acquire");
            let runtime = rt.config_dir().to_path_buf();
            fs::create_dir_all(runtime.join("plans")).unwrap();
            fs::write(runtime.join("plans/new.md"), "tree only").unwrap();

            drop(rt);

            assert_eq!(
                fs::read_to_string(claude_home.join("plans/new.md"))
                    .ok()
                    .as_deref(),
                Some("tree only"),
                "the session's new sidecar must land in the global store"
            );
            assert!(!runtime.exists(), "the ended session's tree is removed");
        });
    });
}

/// A sidecar a linked session wrote under a name the global store gained
/// meanwhile is that session's own, never a stale copy: it lands as a sibling
/// and the global one stays.
#[test]
fn gc_keeps_a_linked_trees_colliding_sidecar_as_a_sibling() {
    let sb = HomeSandbox::new();
    let claude_home = sb.home().join(".claude");
    fs::create_dir_all(claude_home.join("projects")).unwrap();
    fs::create_dir_all(claude_home.join("plans")).unwrap();
    fs::write(claude_home.join("plans/x.md"), "global").unwrap();
    let profile_dir = sb.home().join(".clauth/profiles/lnk");
    let runtime = profile_dir.join("runtime-4242-8");
    fs::create_dir_all(&runtime).unwrap();
    pose_dir_link(&runtime.join("projects"), &claude_home.join("projects"));
    fs::create_dir_all(runtime.join("plans")).unwrap();
    fs::write(runtime.join("plans/x.md"), "mine").unwrap();
    fs::create_dir_all(profile_dir.join("sessions-4242-8")).unwrap();

    gc_stale_runtimes();

    assert_eq!(
        fs::read_to_string(claude_home.join("plans/x.md")).unwrap(),
        "global",
        "the global sidecar must stay as it is"
    );
    assert_eq!(
        fs::read_to_string(claude_home.join("plans/x.rescued-0.md"))
            .ok()
            .as_deref(),
        Some("mine"),
        "the session's own colliding sidecar must land beside it"
    );
}

/// The append arm writes the missing tail into the global transcript itself,
/// never a replacement file: a process holding the global file open keeps
/// writing into the file the store names.
#[cfg(unix)]
#[test]
fn gc_appends_a_dead_trees_tail_into_the_same_global_file() {
    use std::os::unix::fs::MetadataExt;
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let (claude_home, _runtime, _sessions) = dead_shared_fake_tree(tmp.path(), "inode");
        let global = claude_home.join("projects/-w-kill/s1.jsonl");
        fs::create_dir_all(global.parent().unwrap()).unwrap();
        fs::write(&global, "transc").unwrap();
        let before = fs::metadata(&global).unwrap().ino();

        with_link_mode(LinkMode::Fake, gc_stale_runtimes);

        assert_eq!(
            fs::read_to_string(&global).unwrap(),
            "transcript",
            "the tree's tail must land in the global transcript"
        );
        assert_eq!(
            fs::metadata(&global).unwrap().ino(),
            before,
            "the global transcript must stay the same file"
        );
    });
}

/// A live marker in the pair's marker dir blocks every leg: nothing is renamed,
/// nothing is rescued, the tree stays put.
#[test]
fn gc_spares_an_isolated_tree_with_a_live_marker() {
    let sb = HomeSandbox::new();
    let claude_home = sb.home().join(".claude");
    let runtime = sb.home().join(".clauth/profiles/iso/runtime-isolated");
    let sessions = sb.home().join(".clauth/profiles/iso/sessions-isolated");
    fs::create_dir_all(runtime.join("projects/-w-iso")).unwrap();
    fs::write(runtime.join("projects/-w-iso/s1.jsonl"), "transcript").unwrap();
    fs::create_dir_all(runtime.join("shell-snapshots")).unwrap();
    fs::write(runtime.join("shell-snapshots/snap.sh"), "iso shell").unwrap();
    fs::create_dir_all(&sessions).unwrap();
    let _live = live_marker(&sessions.join("1234-0"));

    gc_stale_runtimes();

    assert!(
        runtime.join("projects/-w-iso/s1.jsonl").is_file(),
        "a live session's transcript must stay put"
    );
    assert!(
        runtime.join("shell-snapshots/snap.sh").is_file(),
        "a live session's sidecar must stay put"
    );
    assert!(
        !claude_home.join("projects").exists(),
        "nothing may be rescued"
    );
    assert!(!claude_home.join("shell-snapshots").exists());
    assert!(
        !sb.home()
            .join(".clauth/profiles/iso/runtime-isolated.rescuing")
            .exists(),
        "nothing may be renamed to a tombstone"
    );
}

/// A dead bare shared tree whose stores link into `~/.claude` (a linking
/// host's legacy tree): the rescue never walks through its links, so the
/// global transcript behind `projects` stays as it is, while a real sidecar
/// only the tree holds still lands in the global store.
#[test]
fn gc_rescues_only_the_real_entries_of_a_linked_bare_tree() {
    let sb = HomeSandbox::new();
    let claude_home = sb.home().join(".claude");
    let runtime = sb.home().join(".clauth/profiles/sh/runtime");
    let sessions = sb.home().join(".clauth/profiles/sh/sessions");
    fs::create_dir_all(claude_home.join("projects/-w-sh")).unwrap();
    fs::write(claude_home.join("projects/-w-sh/s1.jsonl"), "global").unwrap();
    fs::create_dir_all(claude_home.join("skills")).unwrap();
    fs::create_dir_all(&runtime).unwrap();
    pose_dir_link(&runtime.join("projects"), &claude_home.join("projects"));
    pose_dir_link(&runtime.join("skills"), &claude_home.join("skills"));
    fs::create_dir_all(runtime.join("plans")).unwrap();
    fs::write(runtime.join("plans/q.md"), "tree only").unwrap();
    fs::create_dir_all(&sessions).unwrap();
    fs::write(sessions.join("dead"), b"").unwrap();

    with_link_mode(LinkMode::Fake, gc_stale_runtimes);

    assert!(!runtime.exists(), "the shared tree must be removed");
    assert!(!sessions.exists(), "the shared marker dir must be removed");
    assert_eq!(
        fs::read_to_string(claude_home.join("projects/-w-sh/s1.jsonl")).unwrap(),
        "global",
        "removing the tree must not reach through its links"
    );
    assert_eq!(
        fs::read_to_string(claude_home.join("plans/q.md"))
            .ok()
            .as_deref(),
        Some("tree only"),
        "a real sidecar only the tree holds must be rescued"
    );
}

/// A `.rescuing` directory left by a crash between the rename and the delete is
/// finished by the sweep: rescue from it, then remove it.
#[test]
fn gc_finishes_a_stranded_rescue_tombstone() {
    let sb = HomeSandbox::new();
    let claude_home = sb.home().join(".claude");
    let tombstone = sb
        .home()
        .join(".clauth/profiles/iso/runtime-isolated.rescuing");
    fs::create_dir_all(tombstone.join("projects/-w-iso")).unwrap();
    fs::write(tombstone.join("projects/-w-iso/s1.jsonl"), "transcript").unwrap();
    fs::create_dir_all(tombstone.join("shell-snapshots")).unwrap();
    fs::write(tombstone.join("shell-snapshots/snap.sh"), "iso shell").unwrap();

    gc_stale_runtimes();

    assert_eq!(
        fs::read_to_string(claude_home.join("projects/-w-iso/s1.jsonl")).unwrap(),
        "transcript",
        "the transcript must be rescued from the stranded tombstone"
    );
    assert_eq!(
        fs::read_to_string(claude_home.join("shell-snapshots/snap.sh")).unwrap(),
        "iso shell",
        "the sidecar must be rescued from the stranded tombstone"
    );
    assert!(!tombstone.exists(), "the tombstone must be collected");
}

/// The append rule holds for an isolated tree too: a global transcript its
/// session's copy extends is replaced under its own name, with no sibling.
#[test]
fn gc_replaces_a_global_transcript_an_isolated_tree_extends() {
    let sb = HomeSandbox::new();
    let claude_home = sb.home().join(".claude");
    let profile_dir = sb.home().join(".clauth/profiles/iso");
    let runtime = profile_dir.join("runtime-isolated-4242-7");
    fs::create_dir_all(runtime.join("projects/-w-iso")).unwrap();
    fs::write(runtime.join("projects/-w-iso/s1.jsonl"), "l1\nl2\n").unwrap();
    fs::create_dir_all(profile_dir.join("sessions-isolated-4242-7")).unwrap();
    fs::create_dir_all(claude_home.join("projects/-w-iso")).unwrap();
    fs::write(claude_home.join("projects/-w-iso/s1.jsonl"), "l1\n").unwrap();

    gc_stale_runtimes();

    assert_eq!(
        fs::read_to_string(claude_home.join("projects/-w-iso/s1.jsonl")).unwrap(),
        "l1\nl2\n",
        "the isolated tree's longer transcript must replace the global prefix"
    );
    assert!(
        !claude_home
            .join("projects/-w-iso/s1.rescued-0.jsonl")
            .exists(),
        "an extension is no collision: no sibling"
    );
    assert!(!runtime.exists(), "the rescued tree must be gone");
    assert!(
        !profile_dir
            .join("runtime-isolated-4242-7.rescuing")
            .exists(),
        "the rescued tombstone must be collected"
    );
}

/// A tombstone whose `projects` is a link resolves into the global store
/// itself: the rescue never walks it, so no global transcript is deleted as an
/// already-rescued copy of itself.
#[test]
fn gc_never_rescues_through_a_linked_projects_store() {
    let sb = HomeSandbox::new();
    let claude_home = sb.home().join(".claude");
    let tombstone = sb
        .home()
        .join(".clauth/profiles/iso/runtime-isolated.rescuing");
    fs::create_dir_all(claude_home.join("projects/-w-iso")).unwrap();
    fs::write(claude_home.join("projects/-w-iso/s1.jsonl"), "global").unwrap();
    fs::create_dir_all(&tombstone).unwrap();
    pose_dir_link(&tombstone.join("projects"), &claude_home.join("projects"));

    gc_stale_runtimes();

    assert_eq!(
        fs::read_to_string(claude_home.join("projects/-w-iso/s1.jsonl"))
            .ok()
            .as_deref(),
        Some("global"),
        "a rescue must not reach the global store through a linked projects"
    );
    assert!(!tombstone.exists(), "the tombstone must be collected");
}

/// A transcript gone from under the rescue (a concurrent sweep moved it first)
/// leaves nothing to keep: the tombstone is collected, never kept for a retry
/// that cannot land. A link resolving to nothing poses the vanished source: its
/// read fails `NotFound` at the same step a moved-away file's does.
#[cfg(unix)]
#[test]
fn gc_collects_a_tombstone_whose_unmoved_transcript_is_gone() {
    let sb = HomeSandbox::new();
    let claude_home = sb.home().join(".claude");
    let tombstone = sb
        .home()
        .join(".clauth/profiles/iso/runtime-isolated.rescuing");
    let slug = tombstone.join("projects/-w-iso");
    fs::create_dir_all(&slug).unwrap();
    fs::write(slug.join("s1.jsonl"), "transcript").unwrap();
    std::os::unix::fs::symlink(slug.join("moved-away.jsonl"), slug.join("gone.jsonl")).unwrap();

    gc_stale_runtimes();

    assert_eq!(
        fs::read_to_string(claude_home.join("projects/-w-iso/s1.jsonl"))
            .ok()
            .as_deref(),
        Some("transcript"),
        "the transcript still in the tombstone must be rescued"
    );
    assert!(
        !tombstone.exists(),
        "a vanished source must not keep the tombstone"
    );
}

/// A dead copy-host tree whose tombstone name a kept tombstone already holds
/// cannot be renamed aside: the start builds over it, never wipes it, and the
/// kept tombstone stays for its own sweep. `daemon.json` is a name the build and
/// the mirror both skip, so only a wipe can remove it.
#[test]
fn acquire_builds_over_a_dead_copy_host_tree_it_cannot_rename() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        with_link_mode(LinkMode::Fake, || {
            let (_claude_home, runtime, _sessions) = dead_shared_fake_tree(tmp.path(), "occupied");
            let kept = runtime.with_file_name("runtime.rescuing");
            fs::create_dir_all(kept.join("projects/-w-old")).unwrap();
            fs::write(kept.join("projects/-w-old/s0.jsonl"), "kept").unwrap();
            fs::write(runtime.join("daemon.json"), "tree only").unwrap();
            let profile = configured_profile("occupied");

            let rt =
                ProfileRuntime::acquire(&profile, Isolation::Shared, &[], false, Launcher::Own)
                    .expect("acquire");

            assert_eq!(
                rt.config_dir(),
                runtime.as_path(),
                "the shared copy-host session runs out of the bare tree"
            );
            assert_eq!(
                fs::read_to_string(runtime.join("daemon.json"))
                    .ok()
                    .as_deref(),
                Some("tree only"),
                "a tree the start cannot rename aside is built over, never wiped"
            );
            assert_eq!(
                fs::read_to_string(kept.join("projects/-w-old/s0.jsonl"))
                    .ok()
                    .as_deref(),
                Some("kept"),
                "the kept tombstone stays for its own sweep"
            );

            drop(rt);
        });
    });
}

/// The acquire leg of a linked runtime root: a start that finds its path is a
/// link into `~/.claude` unlinks it and builds a real tree there, never
/// renaming the link to a tombstone or walking the store behind it.
#[test]
fn acquire_unlinks_a_linked_runtime_root_without_walking_it() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        with_link_mode(LinkMode::Fake, || {
            let claude_home = fake_claude_home(tmp.path());
            fs::write(claude_home.join("CLAUDE.md"), "memory").unwrap();
            fs::create_dir_all(claude_home.join("projects/-w-g")).unwrap();
            fs::write(claude_home.join("projects/-w-g/s.jsonl"), "global").unwrap();
            fs::create_dir_all(claude_home.join("plans")).unwrap();
            fs::write(claude_home.join("plans/p.md"), "global plan").unwrap();
            let profile = configured_profile("lnkacq");
            let runtime = tmp.path().join(".clauth/profiles/lnkacq/runtime");
            fs::create_dir_all(runtime.parent().unwrap()).unwrap();
            pose_dir_link(&runtime, &claude_home);

            let rt =
                ProfileRuntime::acquire(&profile, Isolation::Shared, &[], false, Launcher::Own)
                    .expect("acquire");

            assert_eq!(
                fs::read_to_string(claude_home.join("projects/-w-g/s.jsonl"))
                    .ok()
                    .as_deref(),
                Some("global"),
                "a global transcript must survive a start over a linked runtime root"
            );
            assert_eq!(
                fs::read_to_string(claude_home.join("plans/p.md"))
                    .ok()
                    .as_deref(),
                Some("global plan"),
                "a global sidecar must survive a start over a linked runtime root"
            );
            assert!(
                runtime.symlink_metadata().is_ok_and(|m| m.is_dir()),
                "the session runs out of a real tree at the runtime path"
            );
            assert!(
                runtime
                    .with_file_name("runtime.rescuing")
                    .symlink_metadata()
                    .is_err(),
                "a linked root is never renamed to a tombstone"
            );

            drop(rt);
        });
    });
}

/// An isolated tree's rescue never writes through a sidecar dir the operator
/// linked out of `~/.claude`: its session never wrote through it. The
/// transcript beside it is the control that the rescue ran.
#[test]
fn gc_never_writes_an_isolated_sidecar_through_an_operator_linked_dir() {
    let sb = HomeSandbox::new();
    let claude_home = sb.home().join(".claude");
    let elsewhere = sb.home().join("dotfiles/plans");
    fs::create_dir_all(&elsewhere).unwrap();
    fs::create_dir_all(&claude_home).unwrap();
    pose_dir_link(&claude_home.join("plans"), &elsewhere);
    let profile_dir = sb.home().join(".clauth/profiles/iso");
    let runtime = profile_dir.join("runtime-isolated-4242-7");
    fs::create_dir_all(runtime.join("plans")).unwrap();
    fs::write(runtime.join("plans/new.md"), "iso plan").unwrap();
    fs::create_dir_all(runtime.join("projects/-w-iso")).unwrap();
    fs::write(runtime.join("projects/-w-iso/s1.jsonl"), "transcript").unwrap();
    fs::create_dir_all(profile_dir.join("sessions-isolated-4242-7")).unwrap();

    gc_stale_runtimes();

    assert_eq!(
        fs::read_to_string(claude_home.join("projects/-w-iso/s1.jsonl"))
            .ok()
            .as_deref(),
        Some("transcript"),
        "the isolated tree's rescue ran"
    );
    assert!(
        elsewhere.join("new.md").symlink_metadata().is_err(),
        "an isolated rescue must not write through the operator's link"
    );
}

/// Two sweeps can reach one tombstone at once (the renaming sweep's own tail and
/// another process's stranded-tombstone arm). The global transcript must end up
/// holding the tree's bytes exactly once: the tail is never appended twice. The
/// 8 MiB prefix widens the window between the compare and the append.
#[test]
fn two_rescues_of_one_tombstone_append_the_tail_once() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let claude_home = fake_claude_home(tmp.path());
        let prefix = "p".repeat(8 << 20) + "\n";
        let tail: String = (0..20_000)
            .map(|i| format!("{{\"line\":{i:08}}}\n"))
            .collect();
        let whole = format!("{prefix}{tail}");
        for trial in 0..10 {
            let tombstone = tmp
                .path()
                .join(format!(".clauth/profiles/race{trial}/runtime.rescuing"));
            let rel = format!("projects/-w-race{trial}/s.jsonl");
            fs::create_dir_all(claude_home.join(&rel).parent().unwrap()).unwrap();
            fs::write(claude_home.join(&rel), &prefix).unwrap();
            fs::create_dir_all(tombstone.join(&rel).parent().unwrap()).unwrap();
            fs::write(tombstone.join(&rel), &whole).unwrap();
            let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
            let rescuers: Vec<_> = (0..2)
                .map(|_| {
                    let barrier = barrier.clone();
                    let tombstone = tombstone.clone();
                    std::thread::spawn(move || {
                        barrier.wait();
                        rescue_tombstone(&tombstone);
                    })
                })
                .collect();
            for rescuer in rescuers {
                rescuer.join().expect("join a rescuer");
            }

            assert!(
                fs::read_to_string(claude_home.join(&rel)).unwrap() == whole,
                "trial {trial}: the global transcript must hold the tree's bytes once"
            );
        }
    });
}

/// A linked runtime root that acquire's liveness checks spare (its target
/// carries a supervisor record they cannot verify) is never built into: the
/// start refuses by name, and the store the link points at is untouched.
#[test]
fn acquire_refuses_a_spared_linked_runtime_root() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        with_link_mode(LinkMode::Fake, || {
            let claude_home = fake_claude_home(tmp.path());
            fs::write(claude_home.join("settings.json"), "{\"operator\":true}").unwrap();
            fs::write(claude_home.join("daemon.lock"), b"{}").unwrap();
            let profile = configured_profile("lnkspared");
            let runtime = tmp
                .path()
                .join(".clauth")
                .join("profiles")
                .join("lnkspared")
                .join("runtime");
            fs::create_dir_all(runtime.parent().unwrap()).unwrap();
            pose_dir_link(&runtime, &claude_home);

            let Err(err) =
                ProfileRuntime::acquire(&profile, Isolation::Shared, &[], false, Launcher::Own)
            else {
                panic!("a start over a linked runtime root must refuse");
            };

            assert_eq!(
                err.root_cause().to_string(),
                format!(
                    "{} is a link, not a runtime tree clauth made; remove it and start again",
                    runtime.display()
                ),
                "the refusal names the link and the fix"
            );
            assert_eq!(
                fs::read_to_string(claude_home.join("settings.json")).unwrap(),
                "{\"operator\":true}",
                "a start must never write through a linked runtime root"
            );
        });
    });
}

/// A stranded tombstone that is a link (a crash between renaming a linked root
/// and unlinking it) is unlinked by the next sweep and never walked: the store
/// it points at keeps every file.
#[test]
fn gc_unlinks_a_stranded_linked_tombstone_without_walking_it() {
    let sb = HomeSandbox::new();
    let claude_home = sb.home().join(".claude");
    fs::create_dir_all(claude_home.join("projects/-w-g")).unwrap();
    fs::write(claude_home.join("projects/-w-g/s.jsonl"), "global").unwrap();
    fs::create_dir_all(claude_home.join("plans")).unwrap();
    fs::write(claude_home.join("plans/p.md"), "global plan").unwrap();
    let profile_dir = sb.home().join(".clauth/profiles/lnk");
    fs::create_dir_all(&profile_dir).unwrap();
    let tombstone = profile_dir.join("runtime.rescuing");
    pose_dir_link(&tombstone, &claude_home);

    gc_stale_runtimes();

    assert_eq!(
        fs::read_to_string(claude_home.join("projects/-w-g/s.jsonl"))
            .ok()
            .as_deref(),
        Some("global"),
        "a global transcript must survive a linked tombstone"
    );
    assert_eq!(
        fs::read_to_string(claude_home.join("plans/p.md"))
            .ok()
            .as_deref(),
        Some("global plan"),
        "a global sidecar must survive a linked tombstone"
    );
    assert!(
        tombstone.symlink_metadata().is_err(),
        "the linked tombstone itself is collected"
    );
}

/// A copy host whose `~/.claude` holds neither `CLAUDE.md` nor `skills` builds
/// a tree with no link in it at all: it is still a copy, so its stale copy of a
/// global sidecar is dropped, never landed beside the global one.
#[test]
fn gc_drops_a_plain_copy_trees_stale_sidecar_copy() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let claude_home = fake_claude_home(tmp.path());
        let profile_dir = tmp.path().join(".clauth/profiles/plain");
        let runtime = profile_dir.join("runtime");
        let sessions = profile_dir.join("sessions");
        fs::create_dir_all(runtime.join("projects/-w")).unwrap();
        fs::create_dir_all(claude_home.join("sessions")).unwrap();
        fs::write(
            claude_home.join("sessions/777.json"),
            "{\"status\":\"busy\"}",
        )
        .unwrap();
        fs::create_dir_all(runtime.join("sessions")).unwrap();
        fs::write(runtime.join("sessions/777.json"), "{\"status\":\"idle\"}").unwrap();
        fs::create_dir_all(&sessions).unwrap();
        fs::write(sessions.join("dead"), b"").unwrap();

        with_link_mode(LinkMode::Fake, gc_stale_runtimes);

        assert_eq!(
            fs::read_to_string(claude_home.join("sessions/777.json")).unwrap(),
            "{\"status\":\"busy\"}",
            "the global sidecar must stay as it is"
        );
        assert!(
            !claude_home.join("sessions/777.rescued-0.json").exists(),
            "a copy's stale sidecar must be dropped, not landed beside the global one"
        );
        assert!(!runtime.exists(), "the dead tree must be gone");
    });
}

/// A shared tree's real sidecar whose global dir is a dangling operator link
/// can be neither moved nor written through: it is left, so the tree is kept
/// for a later sweep instead of being deleted with it.
#[cfg(unix)]
#[test]
fn gc_keeps_a_tree_whose_sidecar_meets_a_dangling_global_link() {
    let sb = HomeSandbox::new();
    let claude_home = sb.home().join(".claude");
    fs::create_dir_all(claude_home.join("projects")).unwrap();
    std::os::unix::fs::symlink(
        sb.home().join("dotfiles-not-cloned/plans"),
        claude_home.join("plans"),
    )
    .unwrap();
    let profile_dir = sb.home().join(".clauth/profiles/lnk");
    let runtime = profile_dir.join("runtime-4242-9");
    fs::create_dir_all(&runtime).unwrap();
    pose_dir_link(&runtime.join("projects"), &claude_home.join("projects"));
    fs::create_dir_all(runtime.join("plans")).unwrap();
    fs::write(runtime.join("plans/new.md"), "tree only").unwrap();
    fs::create_dir_all(profile_dir.join("sessions-4242-9")).unwrap();

    gc_stale_runtimes();

    let tombstone = profile_dir.join("runtime-4242-9.rescuing");
    assert!(
        runtime.join("plans/new.md").exists() || tombstone.join("plans/new.md").exists(),
        "a sidecar the rescue could not land must not be deleted with the tree"
    );
}

/// A dead linked runtime root is unlinked where it stands, never renamed to its
/// tombstone: beside a tombstone a failed rescue kept, that rename fails and
/// leaves the link, so the start would refuse instead of building.
#[test]
fn acquire_unlinks_a_linked_root_beside_a_kept_tombstone() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        with_link_mode(LinkMode::Fake, || {
            let claude_home = fake_claude_home(tmp.path());
            fs::create_dir_all(claude_home.join("projects/-w-g")).unwrap();
            fs::write(claude_home.join("projects/-w-g/s.jsonl"), "global").unwrap();
            let profile = configured_profile("lnkkept");
            let runtime = tmp.path().join(".clauth/profiles/lnkkept/runtime");
            let kept = runtime.with_file_name("runtime.rescuing");
            fs::create_dir_all(kept.join("projects/-w-old")).unwrap();
            fs::write(kept.join("projects/-w-old/s0.jsonl"), "kept").unwrap();
            pose_dir_link(&runtime, &claude_home);

            let rt =
                ProfileRuntime::acquire(&profile, Isolation::Shared, &[], false, Launcher::Own)
                    .expect("a dead linked root beside a kept tombstone must not refuse the start");

            assert!(
                runtime.symlink_metadata().is_ok_and(|m| m.is_dir()),
                "the session runs out of a real tree at the runtime path"
            );
            assert_eq!(
                fs::read_to_string(claude_home.join("projects/-w-g/s.jsonl"))
                    .ok()
                    .as_deref(),
                Some("global"),
                "the store behind the link is untouched"
            );
            assert_eq!(
                fs::read_to_string(kept.join("projects/-w-old/s0.jsonl"))
                    .ok()
                    .as_deref(),
                Some("kept"),
                "the kept tombstone stays for its own sweep"
            );

            drop(rt);
        });
    });
}

/// A claim file a crashed rescue left is held by nobody: beside its tombstone
/// it does not stop the next sweep finishing the rescue, and it goes with the
/// tombstone; alone, it is collected.
#[test]
fn gc_finishes_a_tombstone_past_a_crashed_rescues_claim() {
    let sb = HomeSandbox::new();
    let claude_home = sb.home().join(".claude");
    fs::create_dir_all(&claude_home).unwrap();
    let tombstone = sb
        .home()
        .join(".clauth/profiles/crash/runtime-isolated.rescuing");
    fs::create_dir_all(tombstone.join("projects/-w")).unwrap();
    fs::write(tombstone.join("projects/-w/s.jsonl"), "transcript").unwrap();
    let claim = tombstone.with_file_name("runtime-isolated.rescuing.claim");
    fs::write(&claim, b"").unwrap();
    let orphan = sb
        .home()
        .join(".clauth/profiles/gone/runtime.rescuing.claim");
    fs::create_dir_all(orphan.parent().unwrap()).unwrap();
    fs::write(&orphan, b"").unwrap();

    gc_stale_runtimes();

    assert_eq!(
        fs::read_to_string(claude_home.join("projects/-w/s.jsonl"))
            .ok()
            .as_deref(),
        Some("transcript"),
        "a stale claim must not stop the rescue"
    );
    assert!(
        tombstone.symlink_metadata().is_err(),
        "the rescued tombstone is removed"
    );
    assert!(
        claim.symlink_metadata().is_err(),
        "the claim goes with its tombstone"
    );
    assert!(
        orphan.symlink_metadata().is_err(),
        "a claim whose tombstone is gone is collected"
    );
}

/// A sweep that opened a rescue claim just before its holder unlinked it locks
/// an orphan once the holder lets go, while a later sweep may create and lock a
/// new claim at the same name: the orphan must not count as the claim, or both
/// rescue one tombstone.
#[cfg(unix)]
#[test]
fn a_rescue_claim_unlinked_under_its_opener_is_not_the_claim() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = tmp.path().join("runtime.rescuing.claim");
    let opened = open_rescue_claim(&path).unwrap();
    assert!(
        claim_is_named(&opened, &path),
        "control: the file the name points at is the claim"
    );

    fs::remove_file(&path).unwrap();
    let _recreated = open_rescue_claim(&path).unwrap();

    assert!(
        !claim_is_named(&opened, &path),
        "a claim file unlinked under its opener must not be the claim"
    );
}

/// Two dead trees holding different tails of one global transcript, rescued at
/// the same moment by two sweeps: one tail extends the global file, the other
/// lands beside it as a sibling, and neither lands twice or torn into the
/// other. The 8 MiB prefix widens the window between compare and append.
#[test]
fn two_trees_extending_one_transcript_never_both_append() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let claude_home = fake_claude_home(tmp.path());
        let prefix = "p".repeat(8 << 20) + "\n";
        let tail_a: String = (0..10_000).map(|i| format!("{{\"a\":{i:08}}}\n")).collect();
        let tail_b: String = (0..10_000).map(|i| format!("{{\"b\":{i:08}}}\n")).collect();
        let with_a = format!("{prefix}{tail_a}");
        let with_b = format!("{prefix}{tail_b}");
        for trial in 0..10 {
            let rel = format!("projects/-w-cross{trial}/s.jsonl");
            let global = claude_home.join(&rel);
            fs::create_dir_all(global.parent().unwrap()).unwrap();
            fs::write(&global, &prefix).unwrap();
            let tombstones: Vec<PathBuf> = [("a", &with_a), ("b", &with_b)]
                .into_iter()
                .map(|(side, body)| {
                    let tombstone = tmp.path().join(format!(
                        ".clauth/profiles/cross{trial}{side}/runtime.rescuing"
                    ));
                    fs::create_dir_all(tombstone.join(&rel).parent().unwrap()).unwrap();
                    fs::write(tombstone.join(&rel), body).unwrap();
                    tombstone
                })
                .collect();
            let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
            let rescuers: Vec<_> = tombstones
                .iter()
                .cloned()
                .map(|tombstone| {
                    let barrier = barrier.clone();
                    std::thread::spawn(move || {
                        barrier.wait();
                        rescue_tombstone(&tombstone);
                    })
                })
                .collect();
            for rescuer in rescuers {
                rescuer.join().expect("join a rescuer");
            }

            let landed = fs::read_to_string(&global).unwrap();
            let sibling =
                fs::read_to_string(global.with_file_name("s.rescued-0.jsonl")).unwrap_or_default();
            assert!(
                (landed == with_a && sibling == with_b) || (landed == with_b && sibling == with_a),
                "trial {trial}: one tail extends the global transcript, the other lands as its \
                 sibling (global {} bytes, sibling {} bytes)",
                landed.len(),
                sibling.len()
            );
        }
    });
}

/// An empty sidecar dir under a dangling global link holds nothing to lose: it
/// never keeps its tree, which would otherwise sit as a tombstone forever.
#[cfg(unix)]
#[test]
fn gc_collects_a_tree_whose_only_refused_sidecar_is_empty() {
    let sb = HomeSandbox::new();
    let claude_home = sb.home().join(".claude");
    fs::create_dir_all(claude_home.join("projects")).unwrap();
    std::os::unix::fs::symlink(
        sb.home().join("dotfiles-not-cloned/plans"),
        claude_home.join("plans"),
    )
    .unwrap();
    let profile_dir = sb.home().join(".clauth/profiles/lnk");
    let runtime = profile_dir.join("runtime-4242-11");
    fs::create_dir_all(&runtime).unwrap();
    pose_dir_link(&runtime.join("projects"), &claude_home.join("projects"));
    fs::create_dir_all(runtime.join("plans")).unwrap();
    fs::create_dir_all(profile_dir.join("sessions-4242-11")).unwrap();

    gc_stale_runtimes();

    assert!(!runtime.exists(), "the dead tree must be gone");
    assert!(
        !profile_dir.join("runtime-4242-11.rescuing").exists(),
        "an empty refused sidecar must not keep a tombstone"
    );
}

/// A copy tree's copy of a global sidecar file the operator linked elsewhere is
/// a stale copy like any other: dropped, never a reason to keep the tree (a kept
/// `runtime.rescuing` would block every later retire of the bare `runtime`).
#[cfg(unix)]
#[test]
fn gc_drops_a_copy_trees_copy_of_a_linked_global_sidecar_file() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let (claude_home, runtime, _sessions) = dead_shared_fake_tree(tmp.path(), "filelink");
        let elsewhere = tmp.path().join("dotfiles/x.md");
        fs::create_dir_all(elsewhere.parent().unwrap()).unwrap();
        fs::write(&elsewhere, "operator's").unwrap();
        fs::create_dir_all(claude_home.join("plans")).unwrap();
        std::os::unix::fs::symlink(&elsewhere, claude_home.join("plans/x.md")).unwrap();
        fs::create_dir_all(runtime.join("plans")).unwrap();
        fs::write(runtime.join("plans/x.md"), "stale copy").unwrap();

        with_link_mode(LinkMode::Fake, gc_stale_runtimes);

        assert!(!runtime.exists(), "the dead tree must be gone");
        assert!(
            !runtime.with_file_name("runtime.rescuing").exists(),
            "a stale copy must not keep a tombstone"
        );
        assert_eq!(
            fs::read_to_string(&elsewhere).unwrap(),
            "operator's",
            "the operator's file stays as it is"
        );
    });
}

/// A finished rescue's claim file goes while its lock is still held: a sweep
/// that opened the claim during the rescue must not lock it in between, or it
/// holds a claim at a name the next claimant recreates and locks too.
#[cfg(unix)]
#[test]
fn a_released_claims_name_goes_while_it_is_still_held() {
    let sb = HomeSandbox::new();
    fs::create_dir_all(sb.home().join(".claude")).unwrap();
    let tombstone = sb.home().join(".clauth/profiles/rel/runtime.rescuing");
    fs::create_dir_all(tombstone.join("projects/-w")).unwrap();
    fs::write(tombstone.join("projects/-w/s.jsonl"), "transcript").unwrap();
    let taken = std::rc::Rc::new(std::cell::Cell::new(None));
    let seen = taken.clone();
    let probe = tombstone.clone();
    set_claim_hook(
        &CLAIM_UNLINK_HOOK,
        Box::new(move |_path| seen.set(Some(RescueClaim::take(&probe).is_some()))),
    );

    rescue_tombstone(&tombstone);

    assert!(
        tombstone.symlink_metadata().is_err(),
        "control: the rescue finished and released its claim"
    );
    assert_eq!(
        taken.get(),
        Some(false),
        "a claim being released must still be held when its name goes"
    );
}

/// A sweep that opened a rescue claim just before its holder unlinked it locks
/// that orphan once the holder lets go, while a later sweep holds a new claim at
/// the same name over a tombstone retired since: the orphan's holder must leave
/// that tombstone to the new claim's holder.
#[cfg(unix)]
#[test]
fn a_sweep_locking_an_unlinked_claim_leaves_the_tombstone_alone() {
    let sb = HomeSandbox::new();
    let claude_home = sb.home().join(".claude");
    fs::create_dir_all(&claude_home).unwrap();
    let tombstone = sb.home().join(".clauth/profiles/orph/runtime.rescuing");
    fs::create_dir_all(tombstone.join("projects/-w")).unwrap();
    fs::write(tombstone.join("projects/-w/s.jsonl"), "transcript").unwrap();
    let holder: std::rc::Rc<std::cell::RefCell<Option<File>>> = Default::default();
    let keep = holder.clone();
    set_claim_hook(
        &CLAIM_OPENED_HOOK,
        Box::new(move |path| {
            fs::remove_file(path).unwrap();
            let claim = open_rescue_claim(path).unwrap();
            claim.try_lock().unwrap();
            *keep.borrow_mut() = Some(claim);
        }),
    );

    rescue_tombstone(&tombstone);

    assert!(
        holder.borrow().is_some(),
        "control: a new claim was taken between the open and the lock"
    );
    assert_eq!(
        fs::read_to_string(tombstone.join("projects/-w/s.jsonl"))
            .ok()
            .as_deref(),
        Some("transcript"),
        "a sweep holding an unlinked claim must leave the tombstone to the claim's holder"
    );
    assert!(
        !claude_home.join("projects/-w/s.jsonl").exists(),
        "nothing lands from a tombstone another sweep holds"
    );
}

/// A rescue that cannot take the box-wide rescue lock in time leaves the file
/// in its tombstone, which stays; once the lock is free the next sweep finishes
/// it.
#[test]
fn a_rescue_that_cannot_take_the_file_lock_keeps_its_tombstone() {
    struct ClearWait;
    impl Drop for ClearWait {
        fn drop(&mut self) {
            crate::sessions::set_rescue_lock_wait_override(None);
        }
    }
    let sb = HomeSandbox::new();
    let claude_home = sb.home().join(".claude");
    fs::create_dir_all(&claude_home).unwrap();
    let tombstone = sb
        .home()
        .join(".clauth/profiles/wait/runtime-isolated.rescuing");
    fs::create_dir_all(tombstone.join("projects/-w")).unwrap();
    fs::write(tombstone.join("projects/-w/s.jsonl"), "transcript").unwrap();
    let held = crate::profile::open_state_file(&sb.home().join(".clauth/rescue.lock")).unwrap();
    held.lock().unwrap();
    let _clear = ClearWait;
    crate::sessions::set_rescue_lock_wait_override(Some(Duration::from_millis(50)));

    gc_stale_runtimes();

    assert_eq!(
        fs::read_to_string(tombstone.join("projects/-w/s.jsonl"))
            .ok()
            .as_deref(),
        Some("transcript"),
        "a file whose lock wait ran out stays in its tombstone"
    );
    assert!(
        !claude_home.join("projects/-w/s.jsonl").exists(),
        "nothing lands without the lock"
    );

    drop(held);
    // The positive control runs with the whole wait: a parallel test thread's
    // fork that copied `held`'s fd keeps the flock until its exec.
    crate::sessions::set_rescue_lock_wait_override(None);
    gc_stale_runtimes();

    assert_eq!(
        fs::read_to_string(claude_home.join("projects/-w/s.jsonl"))
            .ok()
            .as_deref(),
        Some("transcript"),
        "the next sweep finishes it once the lock is free"
    );
    assert!(
        tombstone.symlink_metadata().is_err(),
        "and removes the tombstone"
    );
}

/// A wedged holder of the rescue lock costs one sweep its wait once, never once
/// per file or per tree: three tombstones whose files all wait on a held lock
/// leave the sweep short of the three waits a per-file or per-tree budget
/// spends.
#[test]
fn a_sweep_against_a_held_rescue_lock_spends_its_wait_once() {
    struct ClearWait;
    impl Drop for ClearWait {
        fn drop(&mut self) {
            crate::sessions::set_rescue_lock_wait_override(None);
        }
    }
    let sb = HomeSandbox::new();
    fs::create_dir_all(sb.home().join(".claude")).unwrap();
    let transcripts: Vec<PathBuf> = ["1-0", "2-0", "3-0"]
        .into_iter()
        .map(|sid| {
            let slug = sb.home().join(format!(
                ".clauth/profiles/wedge/runtime-isolated-{sid}.rescuing/projects/-w-{sid}"
            ));
            fs::create_dir_all(&slug).unwrap();
            fs::write(slug.join("s.jsonl"), "transcript").unwrap();
            slug.join("s.jsonl")
        })
        .collect();
    let held = crate::profile::open_state_file(&sb.home().join(".clauth/rescue.lock")).unwrap();
    held.lock().unwrap();
    let _clear = ClearWait;
    let wait = Duration::from_millis(800);
    crate::sessions::set_rescue_lock_wait_override(Some(wait));

    let started = std::time::Instant::now();
    gc_stale_runtimes();
    let spent = started.elapsed();

    for transcript in &transcripts {
        assert!(
            transcript.exists(),
            "control: {} landed without the lock",
            transcript.display()
        );
    }
    assert!(
        spent >= wait,
        "control: the first file waited out the bound ({spent:?})"
    );
    assert!(
        spent < wait * 3,
        "three trees waiting on one held lock spent {spent:?}, past the one {wait:?} the sweep \
         shares"
    );
}

/// A second sweep reaching a tombstone whose rescue is about to remove it must
/// leave it to that rescue: removing it would free the name, and a dead tree
/// retired onto it then would go unwalked with the first rescue's removal.
#[test]
fn a_tombstone_mid_rescue_keeps_its_name_from_the_next_retired_tree() {
    let sb = HomeSandbox::new();
    let claude_home = sb.home().join(".claude");
    fs::create_dir_all(&claude_home).unwrap();
    let profile_dir = sb.home().join(".clauth/profiles/reuse");
    let tombstone = profile_dir.join("runtime-isolated.rescuing");
    fs::create_dir_all(tombstone.join("projects/-w-old")).unwrap();
    fs::write(tombstone.join("projects/-w-old/old.jsonl"), "old").unwrap();
    let next = profile_dir.join("runtime-isolated");
    fs::create_dir_all(next.join("projects/-w-next")).unwrap();
    fs::write(next.join("projects/-w-next/next.jsonl"), "next").unwrap();
    let other_sweep = tombstone.clone();
    let retired = next.clone();
    set_claim_hook(
        &TOMBSTONE_REMOVAL_HOOK,
        Box::new(move |_path| {
            rescue_tombstone(&other_sweep);
            let _ = retire_tree(&retired);
        }),
    );

    rescue_tombstone(&tombstone);

    assert_eq!(
        fs::read_to_string(claude_home.join("projects/-w-old/old.jsonl"))
            .ok()
            .as_deref(),
        Some("old"),
        "control: the first rescue landed its transcript"
    );
    let survivors: Vec<PathBuf> = [
        next.join("projects/-w-next/next.jsonl"),
        tombstone.join("projects/-w-next/next.jsonl"),
        claude_home.join("projects/-w-next/next.jsonl"),
    ]
    .into_iter()
    .filter(|path| fs::read_to_string(path).is_ok_and(|body| body == "next"))
    .collect();
    assert_eq!(
        survivors.len(),
        1,
        "the next dead tree's transcript must survive the first rescue's removal"
    );
}

/// A refused sidecar dir holding only links carries nothing the walk would
/// move: it never keeps its tree.
#[cfg(unix)]
#[test]
fn gc_collects_a_tree_whose_refused_sidecar_holds_only_links() {
    let sb = HomeSandbox::new();
    let claude_home = sb.home().join(".claude");
    fs::create_dir_all(claude_home.join("projects")).unwrap();
    std::os::unix::fs::symlink(
        sb.home().join("dotfiles-not-cloned/plans"),
        claude_home.join("plans"),
    )
    .unwrap();
    let profile_dir = sb.home().join(".clauth/profiles/lnk");
    let runtime = profile_dir.join("runtime-4242-12");
    fs::create_dir_all(&runtime).unwrap();
    pose_dir_link(&runtime.join("projects"), &claude_home.join("projects"));
    fs::create_dir_all(runtime.join("plans")).unwrap();
    std::os::unix::fs::symlink(sb.home().join("elsewhere.md"), runtime.join("plans/l.md")).unwrap();
    fs::create_dir_all(profile_dir.join("sessions-4242-12")).unwrap();

    gc_stale_runtimes();

    assert!(!runtime.exists(), "the dead tree must be gone");
    assert!(
        !profile_dir.join("runtime-4242-12.rescuing").exists(),
        "a refused sidecar holding only links must not keep a tombstone"
    );
}

/// A refused sidecar dir the rescue cannot read may hold anything: it counts
/// one file left, so its tree is kept for a later sweep.
#[cfg(unix)]
#[test]
fn gc_keeps_a_tree_whose_refused_sidecar_cannot_be_read() {
    use std::os::unix::fs::PermissionsExt;
    struct Readable([PathBuf; 2]);
    impl Drop for Readable {
        fn drop(&mut self) {
            for dir in &self.0 {
                let _ = fs::set_permissions(dir, fs::Permissions::from_mode(0o700));
            }
        }
    }
    let sb = HomeSandbox::new();
    let claude_home = sb.home().join(".claude");
    fs::create_dir_all(claude_home.join("projects")).unwrap();
    std::os::unix::fs::symlink(
        sb.home().join("dotfiles-not-cloned/plans"),
        claude_home.join("plans"),
    )
    .unwrap();
    let profile_dir = sb.home().join(".clauth/profiles/lnk");
    let runtime = profile_dir.join("runtime-4242-13");
    fs::create_dir_all(&runtime).unwrap();
    pose_dir_link(&runtime.join("projects"), &claude_home.join("projects"));
    fs::create_dir_all(runtime.join("plans")).unwrap();
    fs::write(runtime.join("plans/new.md"), "tree only").unwrap();
    fs::create_dir_all(profile_dir.join("sessions-4242-13")).unwrap();
    let tombstone = profile_dir.join("runtime-4242-13.rescuing");
    let _readable = Readable([runtime.join("plans"), tombstone.join("plans")]);
    fs::set_permissions(runtime.join("plans"), fs::Permissions::from_mode(0o000)).unwrap();
    if fs::read_dir(runtime.join("plans")).is_ok() {
        println!("skipped: this host reads through a 0o000 dir");
        return;
    }
    let lines = crate::logline::LogLines::new();
    let _capture = lines.capture_here();

    gc_stale_runtimes();

    assert_eq!(
        lines
            .snapshot()
            .iter()
            .filter(|line| line.contains("could not be rescued"))
            .collect::<Vec<_>>(),
        vec![&format!(
            "clauth: kept {}: 1 file(s) could not be rescued; a later sweep retries",
            tombstone.display()
        )],
        "an unreadable refused sidecar counts one file left"
    );
}

#[test]
fn namespaced_keychain_owner_records_service_profile_session_owner_only() {
    let home = HomeSandbox::new();
    let runtime = home.home().join(".clauth/profiles/ledgered/runtime-700-1");
    fs::create_dir_all(&runtime).expect("runtime dir");
    let profile = crate::profile::ProfileName::from("ledgered");
    let session = SessionId::for_test("700-1");
    let derived = crate::claude::namespaced_keychain_service(
        &runtime.canonicalize().expect("canonical runtime"),
    );
    let persisted = std::cell::Cell::new(false);
    let owned =
        namespaced_keychain_ledger::authorize_write_with(&runtime, &profile, &session, |owners| {
            persisted.set(true);
            namespaced_keychain_ledger::save(owners)
        })
        .expect("persist owner before write");

    assert!(
        persisted.get(),
        "the persist leg ran before the witness minted"
    );
    assert_eq!(
        owned.service(),
        derived,
        "the witness names the service the durable row authorizes"
    );
    let owners = namespaced_keychain_ledger::load().expect("read ledger");
    assert_eq!(
        owners.owners,
        vec![namespaced_keychain_ledger::Owner {
            service: owned.service().to_string(),
            profile,
            session: "700-1".to_string(),
        }],
        "the durable row carries the semantic service/profile/session owner"
    );
    assert_eq!(
        namespaced_keychain_ledger::owned_services().expect("owned services"),
        BTreeSet::from([owned.service().to_string()]),
        "the census view is derived from the durable owner rows"
    );

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let path = namespaced_keychain_ledger::path().expect("ledger path");
        let mode = fs::metadata(&path)
            .expect("ledger metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "the ledger is born owner-only");
        let dir_mode = fs::metadata(path.parent().expect("ledger parent"))
            .expect("parent metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(dir_mode, 0o700, "the ledger parent is owner-only");
    }
}

#[test]
fn failed_owner_persist_prevents_the_namespaced_item_write_decision() {
    let home = HomeSandbox::new();
    let runtime = home.home().join("runtime-701-1");
    fs::create_dir_all(&runtime).expect("runtime dir");
    let profile = crate::profile::ProfileName::from("blocked");
    let session = SessionId::for_test("701-1");

    let result =
        namespaced_keychain_ledger::authorize_write_with(&runtime, &profile, &session, |_| {
            anyhow::bail!("posed ledger persist failure")
        });

    assert!(
        result.is_err(),
        "the ledger failure is returned — no write witness exists to reach a Keychain sink"
    );
    assert!(
        namespaced_keychain_ledger::load()
            .expect("read ledger")
            .owners
            .is_empty(),
        "the failed persist strands no ownership row"
    );
}

/// A per-session item leg whose ownership row waits out a peer's state-flock
/// hold still hands its Keychain call the whole budget: the budget is armed
/// only once the row persisted, so the wait spends none of it.
#[test]
fn an_item_legs_ownership_wait_leaves_its_keychain_budget_whole() {
    let home = HomeSandbox::new();
    let runtime = home.home().join(".clauth/profiles/budgeted/runtime-702-1");
    fs::create_dir_all(&runtime).expect("runtime dir");
    let profile = crate::profile::ProfileName::from("budgeted");
    let session = SessionId::for_test("702-1");
    let budget = Duration::from_secs(20);
    let held = Duration::from_millis(1500);
    let holder = crate::testutil::hold_state_flock();
    let releaser = std::thread::spawn(move || {
        std::thread::sleep(held);
        drop(holder);
    });
    let _ = crate::lock::take_keychain_leg_windows();

    let start = std::time::Instant::now();
    let inside = owned_keychain_leg(
        budget,
        || namespaced_keychain_ledger::authorize_write(&runtime, &profile, &session),
        |owned| {
            Ok((
                owned.service().to_string(),
                crate::lock::armed_budget_remaining(),
            ))
        },
    );
    let waited = start.elapsed();
    releaser.join().expect("releaser");

    let (service, inside) = inside.expect("the leg runs once the row persisted");
    assert!(
        waited >= held,
        "the ownership row waited out the peer: {waited:?}"
    );
    assert_eq!(
        namespaced_keychain_ledger::owned_services().expect("owned services"),
        BTreeSet::from([service]),
        "the leg ran under the witness the persisted row minted"
    );
    let windows = crate::lock::take_keychain_leg_windows();
    assert_eq!(windows.len(), 1, "one item leg, one Keychain leg scope");
    let noted = windows[0].expect("the leg begins under a budget");
    assert!(
        noted > budget - Duration::from_millis(500),
        "the ownership row's flock wait spent the Keychain budget: {noted:?} left"
    );
    let inside = inside.expect("the leg runs under the budget");
    assert!(
        inside > budget - Duration::from_millis(500) && inside <= budget,
        "the leg's window is the whole budget: {inside:?}"
    );
    assert_eq!(
        crate::lock::armed_budget_remaining(),
        None,
        "the budget ends with the leg"
    );
}

/// An item leg reached under a budget armed above it refuses in debug builds:
/// the leg would adopt that window, which the flock waits between the outer
/// arm and the leg (the carry's store write, the ownership row) spend.
#[cfg(debug_assertions)]
#[test]
#[should_panic(
    expected = "a per-session Keychain item leg was entered under an armed budget (a budget armed above it, or a state-flock hold, which arms one): the waits in between may have spent its window; arm nothing and hold no state flock above `owned_keychain_leg`"
)]
fn an_item_leg_under_an_outer_budget_refuses() {
    let _outer = crate::lock::SharedSubprocessBudget::arm(Duration::from_secs(20));
    let _ = owned_keychain_leg(
        Duration::from_secs(20),
        || anyhow::bail!("the entry check refuses before the ownership row"),
        |_| Ok(()),
    );
}

/// `src` with every comment, string literal and char literal blanked to
/// spaces (newlines kept), so a scan over it sees code tokens only. Raw
/// strings read as plain strings.
fn code_tokens_only(src: &str) -> String {
    let chars: Vec<char> = src.chars().collect();
    let mut out = String::with_capacity(src.len());
    let blank = |c: char| if c == '\n' { '\n' } else { ' ' };
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let next = chars.get(i + 1).copied();
        if c == '/' && next == Some('/') {
            while i < chars.len() && chars[i] != '\n' {
                out.push(' ');
                i += 1;
            }
        } else if c == '/' && next == Some('*') {
            out.push_str("  ");
            i += 2;
            while i < chars.len() && !(chars[i] == '*' && chars.get(i + 1) == Some(&'/')) {
                out.push(blank(chars[i]));
                i += 1;
            }
            out.push_str("  ");
            i += 2;
        } else if c == '"' {
            out.push(' ');
            i += 1;
            while i < chars.len() && chars[i] != '"' {
                if chars[i] == '\\' {
                    out.push(' ');
                    i += 1;
                }
                if let Some(&e) = chars.get(i) {
                    out.push(blank(e));
                }
                i += 1;
            }
            out.push(' ');
            i += 1;
        } else if c == '\'' && next == Some('\\') {
            out.push_str("  ");
            i += 2;
            while i < chars.len() && chars[i] != '\'' {
                out.push(' ');
                i += 1;
            }
            out.push(' ');
            i += 1;
        } else if c == '\'' && chars.get(i + 2) == Some(&'\'') {
            out.push_str("   ");
            i += 3;
        } else {
            out.push(c);
            i += 1;
        }
    }
    out
}

/// No per-session item leg's call names the carry, a state-flock hold or the
/// row retouch that takes one: they run before or after the call, so the leg's
/// budget spends none of their waits (`authorize` takes its own hold through
/// `authorize_write` before the budget arms). A source scan, because the
/// callers are macOS-only and no linux test runs them. It reads code tokens
/// only (comments and literals blanked), takes each call (`owned_keychain_leg(`
/// or its turbofish form) up to its balancing `)`, which a `;`, `?`, `.`, `,`
/// or `}` must follow, and sees those names only when written in the call
/// itself, not reached through another helper; the call count proves it found
/// every site.
#[test]
fn no_item_leg_folds_in_a_carry_or_a_flock() {
    let code = code_tokens_only(include_str!("../../src/runtime.rs"));
    let mut calls = 0usize;
    let mut offenders = Vec::new();
    for (at, _) in code.match_indices("owned_keychain_leg") {
        if code[..at].trim_end().ends_with("fn") {
            continue;
        }
        let rest = code[at + "owned_keychain_leg".len()..].trim_start();
        assert!(
            rest.starts_with('(') || rest.starts_with("::<"),
            "`owned_keychain_leg` named outside a call: {:?}",
            rest.chars().take(40).collect::<String>()
        );
        let args = if let Some(generics) = rest.strip_prefix("::<") {
            let mut angle = 1usize;
            let close = generics
                .char_indices()
                .find(|&(_, c)| {
                    match c {
                        '<' => angle += 1,
                        '>' => angle -= 1,
                        _ => {}
                    }
                    angle == 0
                })
                .map_or(generics.len(), |(k, _)| k + 1);
            generics[close..].trim_start()
        } else {
            rest
        };
        let start = code.len() - args.len();
        let line = code[..start].lines().count();
        assert!(
            args.starts_with('('),
            "the item leg call near line {line} takes no argument list"
        );
        let mut depth = 0usize;
        let mut end = None;
        for (j, c) in code[start..].char_indices() {
            match c {
                '(' => depth += 1,
                ')' if depth > 0 => {
                    depth -= 1;
                    if depth == 0 {
                        end = Some(start + j);
                        break;
                    }
                }
                _ => {}
            }
        }
        let end = end.unwrap_or_else(|| panic!("the item leg call near line {line} never closes"));
        let next = code[end + 1..].trim_start().chars().next();
        assert!(
            matches!(next, Some(';' | '?' | '.' | ',' | '}')),
            "the item leg call near line {line} ends where no call ends: followed by {next:?}"
        );
        calls += 1;
        for banned in [
            "carry_session_item_into(",
            "with_state_lock(",
            "retouch_after_item_leg(",
        ] {
            if code[start..end].contains(banned) {
                offenders.push((line, banned));
            }
        }
    }
    assert_eq!(
        calls, 7,
        "the swap's item write and sign-out, the convergence's sign-out, the seed's write and sign-out, the seed's retry and the copied-bearer re-point each call `owned_keychain_leg` once"
    );
    assert!(
        offenders.is_empty(),
        "an item leg's call names the carry, a state-flock hold or the row retouch, which must run outside it: {offenders:?}"
    );
}

/// The ownership-first wiring, structurally: every namespaced producer routes
/// through `authorize_write`, whose [`OwnedKeychainWrite`] witness is the only
/// argument the macOS Keychain sinks accept, so a `/usr/bin/security` write
/// cannot run without a durable row behind it (the type enforces it on the
/// macOS build). The producer blocks and sink signatures are macOS-only code no
/// Linux run compiles, so the pin is a source scan, the same mechanism the
/// `run_delegate` wiring pins use.
#[test]
fn the_namespaced_keychain_sinks_require_a_durable_ownership_witness() {
    let runtime_src = include_str!("../../src/runtime.rs");
    assert_eq!(
        runtime_src
            .matches("namespaced_keychain_ledger::authorize_write(")
            .count(),
        6,
        "every namespaced producer (swap install + sign-out, start sign-out + install, \
         watchdog retry, convergence sign-out) takes the ownership-first path"
    );

    let keychain_src = include_str!("../../src/keychain.rs");
    for sink in [
        "pub(crate) fn keychain_install_for_config_dir(",
        "pub(crate) fn keychain_sign_out_for_config_dir(",
    ] {
        let signature = keychain_src
            .split_once(sink)
            .expect("the sink is defined")
            .1
            .split_once('{')
            .expect("the sink body opens")
            .0;
        assert!(
            signature.contains("OwnedKeychainWrite"),
            "the {sink} sink accepts only the durable-ownership witness, never a raw service \
             string: {signature}"
        );
    }

    let claude_src = include_str!("../../src/claude.rs");
    let signature = claude_src
        .split_once("pub(crate) fn keychain_mirror_source_for_config_dir(")
        .expect("the mirror source is defined")
        .1
        .split_once('{')
        .expect("the mirror body opens")
        .0;
    assert!(
        signature.contains("OwnedKeychainWrite"),
        "the mirror source accepts only the durable-ownership witness: {signature}"
    );
}

/// The walk-derived collector takes the same salvage path the census does:
/// readable bytes are quarantined before its delete, and no direct delete
/// survives beside it. The collector is macOS-only code no Linux run compiles,
/// so the pin is a source scan.
#[test]
fn the_stale_runtime_collector_takes_the_salvage_path() {
    let runtime_src = include_str!("../../src/runtime.rs");
    let collector = runtime_src
        .split_once("fn collect_orphaned_keychain_item(")
        .expect("the collector is defined")
        .1
        .split_once("pub(crate) fn shared_runtime_dirs")
        .expect("the collector body ends where the shared-dirs walk begins")
        .0;
    assert!(
        collector.contains("crate::keychain::salvage_delete_namespaced_item(&in_flight)"),
        "the walk-derived collector salvages readable bytes before its delete, through the \
         in-flight witness: {collector}"
    );
    assert_eq!(
        collector.matches("in_flight.clear()").count(),
        2,
        "the record clears on both outcomes — the delete landed, or it failed and nothing \
         is in flight anymore"
    );
    assert!(
        !collector.contains("delete_at("),
        "no direct delete survives beside the salvage path: {collector}"
    );
}

#[test]
fn retiring_a_namespaced_keychain_owner_removes_later_delete_authority() {
    let _home = HomeSandbox::new();
    let service = "Claude Code-credentials-deadbeef";
    let profile = crate::profile::ProfileName::from("retired");
    let session = SessionId::for_test("702-1");
    namespaced_keychain_ledger::record_with(
        service,
        &profile,
        &session,
        namespaced_keychain_ledger::save,
    )
    .expect("record owner");

    namespaced_keychain_ledger::retire(service).expect("retire owner");

    assert!(
        namespaced_keychain_ledger::owned_services()
            .expect("owned services")
            .is_empty(),
        "a reused service has no delete authority after its row retires"
    );
}

#[test]
fn the_namespaced_keychain_ledger_rejects_malformed_owner_rows() {
    let _home = HomeSandbox::new();
    let path = namespaced_keychain_ledger::path().expect("ledger path");
    fs::create_dir_all(path.parent().expect("ledger parent")).expect("clauth dir");
    let write = |body: &str| fs::write(&path, body).expect("fixture ledger");

    write(r#"{"owners":[{"service":"not-a-namespaced-service","profile":"p","session":"1-1"}]}"#);
    assert!(
        namespaced_keychain_ledger::owned_services().is_err(),
        "a service the naming rule could not produce must reject the whole ledger"
    );

    write(
        r#"{"owners":[{"service":"Claude Code-credentials-deadbeef","profile":"bad/name","session":"1-1"}]}"#,
    );
    assert!(
        namespaced_keychain_ledger::owned_services().is_err(),
        "a profile name the charset gate refuses must reject the whole ledger"
    );

    write(
        r#"{"owners":[{"service":"Claude Code-credentials-deadbeef","profile":"p","session":"not-a-sid"}]}"#,
    );
    assert!(
        namespaced_keychain_ledger::owned_services().is_err(),
        "a session id outside the minted shape must reject the whole ledger"
    );

    write(
        r#"{"owners":[{"service":"Claude Code-credentials-deadbeef","profile":"p","session":"1-1"},{"service":"Claude Code-credentials-cafebabe","profile":"q","session":"broken"}]}"#,
    );
    assert!(
        namespaced_keychain_ledger::owned_services().is_err(),
        "one malformed row among good ones fails the whole ledger closed"
    );
}

#[test]
fn a_ledger_row_outlives_its_profile_and_stays_authoritative() {
    let _home = HomeSandbox::new();
    namespaced_keychain_ledger::record_with(
        "Claude Code-credentials-deadbeef",
        &crate::profile::ProfileName::from("gone"),
        &SessionId::for_test("703-1"),
        namespaced_keychain_ledger::save,
    )
    .expect("record owner");

    assert_eq!(
        namespaced_keychain_ledger::owned_services().expect("owned services"),
        BTreeSet::from(["Claude Code-credentials-deadbeef".to_string()]),
        "a row whose profile was deleted stays authoritative — profile-deletion orphans \
         are exactly the census's stranding-input class"
    );
}

/// The residual tail #82 closed, write side: a fresh in-flight-delete record
/// — a sweep's re-check passed and its delete sits between its stamp and its
/// outcome — refuses a same-service namespaced write at the ownership-first
/// seam every producer routes through, so the delete cannot catch a queued
/// acquire's freshly seeded item. The record is posed as the raw file the fix's
/// loader must honor, so the pin doubles as the on-disk schema contract.
#[test]
fn a_namespaced_write_is_refused_while_a_sweeps_delete_is_in_flight() {
    let home = HomeSandbox::new();
    let runtime = home.home().join(".clauth/profiles/inflight/runtime-800-1");
    fs::create_dir_all(&runtime).expect("runtime dir");
    let profile = crate::profile::ProfileName::from("inflight");
    let session = SessionId::for_test("800-1");
    let service = crate::claude::namespaced_keychain_service(
        &runtime.canonicalize().expect("canonical runtime"),
    );

    let stamp = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .expect("epoch")
        .as_secs();
    let record_path = crate::profile::clauth_dir()
        .expect("clauth dir")
        .join("keychain-deletes-in-flight.json");
    fs::create_dir_all(record_path.parent().expect("record parent")).expect("clauth dir");
    fs::write(
        &record_path,
        serde_json::json!({"deletes": [{"service": service.clone(), "stamp_secs": stamp}]})
            .to_string(),
    )
    .expect("record in flight");

    let err = namespaced_keychain_ledger::authorize_write_with(
        &runtime,
        &profile,
        &session,
        namespaced_keychain_ledger::save,
    )
    .expect_err("a write racing a sweep's in-flight delete must be refused, never proceed");
    assert!(
        err.downcast_ref::<namespaced_keychain_ledger::DeleteInFlight>()
            .is_some(),
        "the refusal is the consult's typed error, which the seed's retry disposition keys on: \
         {err:#}"
    );
    assert!(
        format!("{err:#}").contains(&service),
        "the refusal names the service the delete targets: {err:#}"
    );
    assert!(
        namespaced_keychain_ledger::owned_services()
            .expect("owned services")
            .is_empty(),
        "a refused write strands no ownership row"
    );
}

/// A crashed delete's record cannot refuse writes forever: the seed's consult
/// sweeps a row older than the delete's own worst-case duration — the salvage
/// read and the delete, two `security` subprocesses under one shared 20 s
/// budget — so a record stamped at the bound admits the write, and the sweep
/// removes it durably.
#[test]
fn a_stale_in_flight_record_is_swept_and_admits_the_write() {
    let home = HomeSandbox::new();
    let runtime = home.home().join(".clauth/profiles/stale/runtime-801-1");
    fs::create_dir_all(&runtime).expect("runtime dir");
    let profile = crate::profile::ProfileName::from("stale");
    let session = SessionId::for_test("801-1");
    let service = crate::claude::namespaced_keychain_service(
        &runtime.canonicalize().expect("canonical runtime"),
    );

    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .expect("epoch")
        .as_secs();
    let stale_stamp = now - namespaced_keychain_ledger::IN_FLIGHT_STALE_AFTER.as_secs();
    let other = "Claude Code-credentials-c56fc9bd";
    let record_path = crate::profile::clauth_dir()
        .expect("clauth dir")
        .join("keychain-deletes-in-flight.json");
    fs::create_dir_all(record_path.parent().expect("record parent")).expect("clauth dir");
    fs::write(
        &record_path,
        serde_json::json!({"deletes": [
            {"service": service.clone(), "stamp_secs": stale_stamp},
            {"service": other, "stamp_secs": now}
        ]})
        .to_string(),
    )
    .expect("record in flight");

    namespaced_keychain_ledger::authorize_write_with(
        &runtime,
        &profile,
        &session,
        namespaced_keychain_ledger::save,
    )
    .expect("a record past the delete's worst case admits the write");

    let deletes = namespaced_keychain_ledger::load_in_flight()
        .expect("read record")
        .deletes;
    assert_eq!(
        deletes,
        vec![namespaced_keychain_ledger::InFlightDeleteRow {
            service: other.to_string(),
            stamp_secs: now,
            pids: Vec::new(),
        }],
        "the consult swept exactly the crashed delete's row — a fresh sibling row survives"
    );
}

/// A crashed sweeper's orphaned child outlives the age bound — the deadline
/// that would kill it is parent-local, and with the parent gone a
/// prompt-stuck `security` child can answer minutes later — so a row whose
/// stamped child is ALIVE must refuse the write however old the row is, or
/// the late delete destroys a freshly seeded item: #82 in the crash case.
/// The alive pid is the test binary's own.
// pid liveness is unix-only (`pid_alive` is false on windows by design).
#[cfg(unix)]
#[test]
fn an_alive_delete_child_keeps_refusing_past_the_age_bound() {
    let home = HomeSandbox::new();
    let runtime = home.home().join(".clauth/profiles/pidalive/runtime-815-1");
    fs::create_dir_all(&runtime).expect("runtime dir");
    let profile = crate::profile::ProfileName::from("pidalive");
    let session = SessionId::for_test("815-1");
    let service = crate::claude::namespaced_keychain_service(
        &runtime.canonicalize().expect("canonical runtime"),
    );

    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .expect("epoch")
        .as_secs();
    let record_path = crate::profile::clauth_dir()
        .expect("clauth dir")
        .join("keychain-deletes-in-flight.json");
    fs::create_dir_all(record_path.parent().expect("record parent")).expect("clauth dir");
    fs::write(
        &record_path,
        serde_json::json!({"deletes": [{
            "service": service.clone(),
            "stamp_secs": now - namespaced_keychain_ledger::IN_FLIGHT_STALE_AFTER.as_secs() - 60,
            "pids": [std::process::id()]
        }]})
        .to_string(),
    )
    .expect("record in flight");

    let err = namespaced_keychain_ledger::authorize_write_with(
        &runtime,
        &profile,
        &session,
        namespaced_keychain_ledger::save,
    )
    .expect_err(
        "a row whose stamped delete child is still alive refuses the write however old \
                 it is — the child outlives the age bound",
    );
    assert!(
        err.downcast_ref::<namespaced_keychain_ledger::DeleteInFlight>()
            .is_some(),
        "the refusal is the consult's typed error: {err:#}"
    );
    assert!(
        format!("{err:#}").contains(&service),
        "the refusal names the service the delete targets: {err:#}"
    );
}

/// A child alive until its stdin closes, from a program every OS ships: a
/// stock Windows PATH has no `true` or `sleep` (those come with Git Bash), and
/// the dropped stdin ends it even when the test panics before its kill.
/// `/D` keeps a registry AutoRun command out of the child.
fn spawn_stdin_parked_child() -> std::process::Child {
    #[cfg(not(windows))]
    let (program, args): (&str, &[&str]) = ("cat", &[]);
    #[cfg(windows)]
    let (program, args): (&str, &[&str]) = ("cmd", &["/D"]);
    std::process::Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap_or_else(|e| panic!("spawn `{program}`: {e}"))
}

/// A pid-dead row falls back to the age bound: the child is gone, so the
/// guarded delete cannot still be running past its worst-case duration, and
/// the bound sweeps the row. The dead pid is a just-reaped child's (pid
/// allocation is monotonic until it wraps at `pid_max`, so the freed pid is
/// not reissued inside this test's lifetime).
#[test]
fn a_pid_dead_row_falls_back_to_the_age_bound() {
    let home = HomeSandbox::new();
    let runtime = home.home().join(".clauth/profiles/pidded/runtime-816-1");
    fs::create_dir_all(&runtime).expect("runtime dir");
    let profile = crate::profile::ProfileName::from("pidded");
    let session = SessionId::for_test("816-1");
    let service = crate::claude::namespaced_keychain_service(
        &runtime.canonicalize().expect("canonical runtime"),
    );

    let mut child = spawn_stdin_parked_child();
    let dead_pid = child.id();
    drop(child.stdin.take());
    child.wait().expect("reap");

    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .expect("epoch")
        .as_secs();
    let record_path = crate::profile::clauth_dir()
        .expect("clauth dir")
        .join("keychain-deletes-in-flight.json");
    fs::create_dir_all(record_path.parent().expect("record parent")).expect("clauth dir");
    fs::write(
        &record_path,
        serde_json::json!({"deletes": [{
            "service": service.clone(),
            "stamp_secs": now - namespaced_keychain_ledger::IN_FLIGHT_STALE_AFTER.as_secs() - 60,
            "pids": [dead_pid]
        }]})
        .to_string(),
    )
    .expect("record in flight");

    namespaced_keychain_ledger::authorize_write_with(
        &runtime,
        &profile,
        &session,
        namespaced_keychain_ledger::save,
    )
    .expect("a pid-dead record past the bound admits the write");
    assert!(
        namespaced_keychain_ledger::load_in_flight()
            .expect("read record")
            .deletes
            .is_empty(),
        "the consult swept the pid-dead row"
    );
}

/// The probe the pid rule rests on: `kill(pid, 0)` semantics read the
/// test's own process alive and a just-reaped child dead (pid allocation is
/// monotonic until it wraps at `pid_max`, so the freed pid is not reissued
/// inside this test's lifetime).
// pid liveness is unix-only (`pid_alive` is false on windows by design).
#[cfg(unix)]
#[test]
fn pid_liveness_reads_self_alive_and_a_reaped_child_dead() {
    assert!(
        namespaced_keychain_ledger::pid_alive(std::process::id()),
        "the probe sees the test's own process alive"
    );
    let mut child = std::process::Command::new("true").spawn().expect("spawn");
    let pid = child.id();
    child.wait().expect("reap");
    assert!(
        !namespaced_keychain_ledger::pid_alive(pid),
        "the probe reads the reaped child dead"
    );
}

/// A spawned delete child must never run unguarded: the pid stamp's
/// not-found arm fails closed — the witnessing runner kills the child on the
/// hook's error, so a row the consult already swept (the hook's own flock
/// wait delayed past the age bound) cannot leave the child to finish its
/// delete against an admitted seed.
#[test]
fn an_unstampable_child_is_killed_not_run_unguarded() {
    let _home = HomeSandbox::new();
    let err = namespaced_keychain_ledger::record_in_flight_pid(
        "Claude Code-credentials-c56fc9bd",
        std::process::id(),
    )
    .expect_err("a child whose row is gone must fail the stamp closed, never run unguarded");
    assert!(
        format!("{err:#}").contains("unguarded"),
        "the refusal names the guard: {err:#}"
    );

    // The macOS half: the witnessing runner kills the child when the hook
    // fails, through the same kill path its deadline uses.
    let keychain_src = include_str!("../../src/keychain.rs");
    let runner = keychain_src
        .split_once("fn run_with_deadline_witnessing(")
        .expect("the witnessing runner is defined")
        .1
        .split_once("let deadline = Instant::now() + timeout;")
        .expect("the runner body ends where the deadline loop begins")
        .0;
    assert!(
        runner.contains("if let Err(e) = on_spawn(child.id())"),
        "the hook's failure is checked immediately after the spawn: {runner}"
    );
    assert!(
        runner.contains("let _ = child.kill();"),
        "the hook's failure kills the child: {runner}"
    );
}

/// Two concurrent collectors on the same orphaned service share one row: one
/// collector's clear must remove only ITS legs' pids, so the other's live
/// child keeps refusing writes until its own clear lands. Posed through the
/// raw file — the schema contract — plus the two witnesses the collectors
/// hold.
#[test]
fn one_collectors_clear_cannot_erase_anothers_live_tracking() {
    let _home = HomeSandbox::new();
    let service = "Claude Code-credentials-c56fc9bd";
    let mut child = spawn_stdin_parked_child();
    let live_pid = child.id();

    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .expect("epoch")
        .as_secs();
    let record_path = crate::profile::clauth_dir()
        .expect("clauth dir")
        .join("keychain-deletes-in-flight.json");
    fs::create_dir_all(record_path.parent().expect("record parent")).expect("clauth dir");
    fs::write(
        &record_path,
        serde_json::json!({"deletes": [{
            "service": service,
            "stamp_secs": now,
            "pids": [std::process::id(), live_pid]
        }]})
        .to_string(),
    )
    .expect("record in flight");

    let first = namespaced_keychain_ledger::record_in_flight_locked(service).expect("first stamp");
    let second =
        namespaced_keychain_ledger::record_in_flight_locked(service).expect("second stamp");
    first
        .record_child(std::process::id())
        .expect("stamp the first collector's child");
    second
        .record_child(live_pid)
        .expect("stamp the second collector's child");
    first.clear().expect("the first collector's delete landed");

    let err = namespaced_keychain_ledger::record_with(
        service,
        &crate::profile::ProfileName::from("twin"),
        &SessionId::for_test("817-1"),
        namespaced_keychain_ledger::save,
    )
    .expect_err(
        "one collector's clear must not erase another's live tracking — the surviving \
                 row keeps refusing",
    );
    assert!(
        err.downcast_ref::<namespaced_keychain_ledger::DeleteInFlight>()
            .is_some(),
        "the refusal is the consult's typed error: {err:#}"
    );

    second
        .clear()
        .expect("the second collector's delete landed");
    namespaced_keychain_ledger::record_with(
        service,
        &crate::profile::ProfileName::from("twin"),
        &SessionId::for_test("817-1"),
        namespaced_keychain_ledger::save,
    )
    .expect("the last clear admits the write");

    let _ = child.kill();
    let _ = child.wait();
}

/// The guarded delete stamps EACH spawned child's pid into the in-flight
/// record at spawn time — the pid rule is what refuses a crashed sweeper's
/// orphaned child past the age bound, so both subprocess legs must stamp,
/// and the stamped value must be the child's own pid, never a constant. The
/// wiring is macOS-only code no Linux run compiles; the pin is a source
/// scan, the same mechanism the ownership-witness pins use.
#[test]
fn the_guarded_delete_stamps_each_childs_pid_at_spawn() {
    let keychain_src = include_str!("../../src/keychain.rs");
    let sink = keychain_src
        .split_once("pub(crate) fn salvage_delete_namespaced_item(")
        .expect("the guarded delete sink is defined")
        .1
        .split_once("/// The census half")
        .expect("the sink ends where the census doc begins")
        .0;
    assert_eq!(
        sink.matches("|pid| in_flight.record_child(pid)").count(),
        2,
        "both subprocess legs stamp their child's pid through the witness: {sink}"
    );
    assert!(
        !sink.contains("std::process::id()"),
        "the stamp never substitutes a constant for the child's pid: {sink}"
    );
}

/// The flock wrapper around the production re-check: the decision and the
/// in-flight stamp share ONE `with_state_lock` hold, so a seed serialized
/// after the hold refuses (the record) and one serialized before it was
/// spared by the decision's own inputs. Removing the wrapper reopens #82's
/// residual tail — a seed writing between the decision and the delete
/// outruns the record. The re-check is macOS-wired, so the pin is a source
/// scan, the same mechanism the ownership-witness pin uses.
#[test]
fn the_gc_keychain_recheck_stamps_under_the_state_flock() {
    let runtime_src = include_str!("../../src/runtime.rs");
    let body = runtime_src
        .split_once("fn gc_keychain_recheck(")
        .expect("the flock-wrapped re-check is defined")
        .1
        .split_once("fn tree_keychain_service")
        .expect("the re-check ends where the service derivation begins")
        .0;
    assert!(
        body.contains("with_state_lock(|_held|"),
        "the decision and the in-flight stamp share one state-flock hold: {body}"
    );
    assert!(
        body.contains("record_in_flight_locked("),
        "the stamp persists before the delete can run: {body}"
    );
}

/// The census's per-delete gate: ONE state-flock hold re-derives the live set
/// and stamps the in-flight record, the census's analogue of the GC's
/// re-check hold — a re-derivation outside any lock leaves the same residual
/// window the record exists to close. The loop is macOS-wired; the pin is a
/// source scan.
#[test]
fn the_census_delete_runs_through_the_in_flight_gate() {
    let keychain_src = include_str!("../../src/keychain.rs");
    let census = keychain_src
        .split_once("pub(crate) fn census_namespaced_items()")
        .expect("the census is defined")
        .1
        .split_once("fn dump_keychain()")
        .expect("the census ends where the dump helper begins")
        .0;
    assert!(
        census.contains("crate::runtime::census_delete_gate("),
        "every census delete runs through the flock-held gate: {census}"
    );
    assert_eq!(
        census.matches("in_flight.clear()").count(),
        2,
        "the record clears on both outcomes — the delete landed, or it failed and \
         nothing is in flight anymore"
    );
    assert!(
        !census.contains("salvage_delete_namespaced_item(&service)"),
        "the delete sink takes the witness, never a bare service: {census}"
    );
}

/// The delete sink's signature requires the in-flight witness, so no
/// `/usr/bin/security` delete for a per-session item can run without a
/// durable record behind it — the record-first order is a type, not a
/// call-site convention, exactly like `OwnedKeychainWrite`. macOS-wired; the
/// pin is a source scan.
#[test]
fn the_keychain_delete_sink_requires_the_in_flight_witness() {
    let keychain_src = include_str!("../../src/keychain.rs");
    let signature = keychain_src
        .split_once("pub(crate) fn salvage_delete_namespaced_item(")
        .expect("the salvage delete sink is defined")
        .1
        .split_once('{')
        .expect("the sink body opens")
        .0;
    assert!(
        signature.contains("InFlightDelete"),
        "the salvage delete accepts only the in-flight witness, never a raw service string: \
         {signature}"
    );
}

/// The re-check's hold, behaviorally: while a peer holds the state flock, the
/// production re-check seam cannot complete, and the in-flight stamp lands
/// inside that hold — no record exists while the seam is blocked, so no
/// delete subprocess can start before the record is durable. Removing the
/// `with_state_lock` wrapper reds this test, which the pure-decision seam
/// test cannot. The competing thread spawns INSIDE the hold — the
/// `tests/inline/lock.rs` wedge shape — so a slow spawn cannot let the
/// re-check complete first and false-red a green tree.
#[test]
fn the_gc_recheck_blocks_while_a_peer_holds_the_state_flock() {
    let home = HomeSandbox::new();
    let sessions = home.home().join(".clauth/profiles/blocked/sessions-810-1");
    let runtime = home.home().join(".clauth/profiles/blocked/runtime-810-1");
    let service = "Claude Code-credentials-c56fc9bd";
    let record_path = crate::profile::clauth_dir()
        .expect("clauth dir")
        .join("keychain-deletes-in-flight.json");
    let (tx, rx) = std::sync::mpsc::channel();
    let handle = crate::lock::with_state_lock(|_held| {
        let handle = std::thread::spawn(move || {
            tx.send(gc_keychain_recheck(Some(service), &sessions, &runtime))
                .expect("send decision");
        });
        assert!(
            rx.recv_timeout(Duration::from_millis(500)).is_err(),
            "the re-check must wait on the state flock while a peer holds it"
        );
        assert!(
            !record_path.exists(),
            "the stamp lands inside the flock hold — no record exists while the re-check is \
             blocked"
        );
        Ok::<_, anyhow::Error>(handle)
    })
    .expect("hold the flock");
    let in_flight = rx
        .recv_timeout(Duration::from_secs(10))
        .expect("the re-check completes once the flock frees")
        .expect("re-check outcome")
        .expect("the posed world collects");
    assert_eq!(
        in_flight.service(),
        service,
        "the witness names the service the record stamps"
    );
    assert!(
        record_path.exists(),
        "the stamp is durable before any delete subprocess could run"
    );
    handle.join().expect("join the re-check thread");
}

/// The GC re-check's fail-closed arm: a stamp that cannot persist skips the
/// delete rather than running it unguarded — the skip returns no witness, so
/// no delete sink can run.
#[test]
fn the_gc_recheck_skips_the_delete_when_the_stamp_cannot_persist() {
    let home = HomeSandbox::new();
    let sessions = home.home().join(".clauth/profiles/skip/sessions-814-1");
    let runtime = home.home().join(".clauth/profiles/skip/runtime-814-1");
    let path = crate::profile::clauth_dir()
        .expect("clauth dir")
        .join("keychain-deletes-in-flight.json");
    fs::create_dir_all(path.parent().expect("record parent")).expect("clauth dir");
    fs::write(&path, r#"{"deletes": [}"#).expect("corrupt record");
    let decision = gc_keychain_recheck(
        Some("Claude Code-credentials-c56fc9bd"),
        &sessions,
        &runtime,
    )
    .expect("the re-check does not error, it skips");
    assert!(
        decision.is_none(),
        "an unrecordable in-flight delete must not run"
    );
}

/// The census gate's spare arm: a service a live runtime dir derives is
/// spared WITHOUT a record — the walk outranks, and no spurious refusal is
/// minted for a delete that will not run.
#[test]
fn the_census_gate_spares_a_live_service_without_stamping() {
    let home = HomeSandbox::new();
    let runtime = home.home().join(".clauth/profiles/gated/runtime-811-1");
    fs::create_dir_all(&runtime).expect("runtime dir");
    let service = crate::claude::namespaced_keychain_service(
        &runtime.canonicalize().expect("canonical runtime"),
    );
    assert!(
        census_delete_gate(&service).expect("gate").is_none(),
        "a dir-derived service is live: the gate spares it"
    );
    let path = crate::profile::clauth_dir()
        .expect("clauth dir")
        .join("keychain-deletes-in-flight.json");
    assert!(!path.exists(), "the spare never stamps a record");
}

/// The census gate's collect arm, and the record's lifecycle in one drive:
/// the orphaned service is stamped in the same hold as the walk, the record
/// refuses a same-service write, and the witness's clear restores it.
#[test]
fn the_census_gate_stamps_an_orphaned_service_before_its_delete() {
    let home = HomeSandbox::new();
    fs::create_dir_all(home.home().join(".clauth/profiles/gated")).expect("profiles dir");
    let service = "Claude Code-credentials-c56fc9bd";
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .expect("epoch")
        .as_secs();

    let in_flight = census_delete_gate(service)
        .expect("gate")
        .expect("no dir explains the service: still orphaned");
    assert_eq!(
        in_flight.service(),
        service,
        "the witness names the gated service"
    );

    let rows = namespaced_keychain_ledger::load_in_flight()
        .expect("read record")
        .deletes;
    assert_eq!(rows.len(), 1, "the stamp is durable: {rows:?}");
    assert_eq!(rows[0].service, service, "the row names the gated service");
    assert!(
        rows[0].stamp_secs <= now + 5 && rows[0].stamp_secs + 5 >= now,
        "the row stamps the moment of the gate, not some past or future wall time: {:?}",
        rows[0].stamp_secs
    );

    let err = namespaced_keychain_ledger::record_with(
        service,
        &crate::profile::ProfileName::from("gated"),
        &SessionId::for_test("812-1"),
        namespaced_keychain_ledger::save,
    )
    .expect_err("the gate's record refuses a same-service write");
    assert!(
        err.downcast_ref::<namespaced_keychain_ledger::DeleteInFlight>()
            .is_some(),
        "the refusal is the consult's typed error: {err:#}"
    );

    in_flight.clear().expect("clear after the delete's outcome");
    assert!(
        namespaced_keychain_ledger::load_in_flight()
            .expect("read record")
            .deletes
            .is_empty(),
        "the clear removed the row durably"
    );
    namespaced_keychain_ledger::record_with(
        service,
        &crate::profile::ProfileName::from("gated"),
        &SessionId::for_test("812-1"),
        namespaced_keychain_ledger::save,
    )
    .expect("a cleared record admits the write");
}

/// An unreadable in-flight record refuses the write rather than reading as
/// "no delete in flight" — the same fail-closed fold the ownership ledger
/// loads under.
#[test]
fn a_corrupt_in_flight_record_refuses_the_write_fail_closed() {
    let home = HomeSandbox::new();
    let runtime = home.home().join(".clauth/profiles/corrupt/runtime-813-1");
    fs::create_dir_all(&runtime).expect("runtime dir");
    let path = crate::profile::clauth_dir()
        .expect("clauth dir")
        .join("keychain-deletes-in-flight.json");
    fs::create_dir_all(path.parent().expect("record parent")).expect("clauth dir");
    fs::write(
        &path,
        r#"{"deletes":[{"service":"not-a-namespaced-service","stamp_secs":1}]}"#,
    )
    .expect("corrupt record");
    let err = namespaced_keychain_ledger::authorize_write_with(
        &runtime,
        &crate::profile::ProfileName::from("corrupt"),
        &SessionId::for_test("813-1"),
        namespaced_keychain_ledger::save,
    )
    .expect_err(
        "a record the naming rule could not produce refuses the write, never reads as absent",
    );
    assert!(format!("{err:#}").contains("not-a-namespaced-service"));
}

/// The live-pid arm of the in-flight refusal, pinned by equality so the
/// `whose security child` phrasing and the live pid cannot drift.
// pid liveness is unix-only (`pid_alive` is false on windows by design).
#[cfg(unix)]
#[test]
fn an_in_flight_delete_with_a_live_child_pins_the_refusal() {
    let pid = std::process::id();
    let err = namespaced_keychain_ledger::DeleteInFlight {
        service: "x".to_string(),
        stamp_secs: 0,
        pids: vec![pid],
    };
    assert_eq!(
        err.to_string(),
        format!(
            "a sweep's delete for the same per-session Keychain item is still in flight (`x`, \
             whose `security` child, pid {pid}, is still alive). The write would race it. It \
             lands once the delete records its outcome or its record is swept as stale"
        )
    );
}

/// The seed's retry mapping: a refusal over an in-flight delete arms the
/// watchdog-tick retry, the same arm the classified locked-keychain transient
/// takes — the record clears when the delete lands or fails, and a crashed
/// delete's is swept after the delete's worst-case duration, so the retry
/// converges instead of wedging.
#[test]
fn the_seed_retries_a_write_refused_over_an_in_flight_delete() {
    let err: anyhow::Error = namespaced_keychain_ledger::DeleteInFlight {
        service: "Claude Code-credentials-c56fc9bd".to_string(),
        stamp_secs: 0,
        pids: Vec::new(),
    }
    .into();
    assert_eq!(
        delete_in_flight_disposition(&err),
        Some(SeedDegradeDisposition::RetryOnTick),
        "the in-flight refusal maps to the retry arm the seed and its watchdog-tick retry share"
    );
    assert_eq!(
        delete_in_flight_disposition(&anyhow::anyhow!("a plain failure")),
        None,
        "every other failure keeps the classified disposition"
    );
}

/// The arm selection for the macOS session-start Keychain seed — pure, so
/// the absent→sign-out / refreshless→skip / else→carry decision is pinned on
/// every platform while the seeding itself only a Mac exercises. The
/// unparseable arm is Carry (not Skip): a torn read is not evidence of
/// refreshlessness, and the legs that follow fail loudly on the bytes.
#[test]
fn session_seed_arm_selection() {
    use crate::profile::{ClaudeCredentials, OAuthToken};
    let rotatable = |refresh: Option<&str>| ClaudeCredentials {
        claude_ai_oauth: Some(OAuthToken {
            access_token: "a".to_string(),
            refresh_token: refresh.map(str::to_string),
            expires_at: None,
            scopes: None,
            subscription_type: None,
            ..OAuthToken::default_extra()
        }),
    };
    let refreshless = rotatable(None);
    let refreshable = rotatable(Some("r"));

    // Absent install source signs the item out.
    assert_eq!(session_seed_arm(false, None), SessionSeedArm::SignOut);
    assert_eq!(
        session_seed_arm(false, Some(&refreshable)),
        SessionSeedArm::SignOut
    );
    // Refreshless sources (rolling sidecar, static mint) keep the file layer alone.
    assert_eq!(
        session_seed_arm(true, Some(&refreshless)),
        SessionSeedArm::Skip
    );
    // Refreshable stores carry-then-write.
    assert_eq!(
        session_seed_arm(true, Some(&refreshable)),
        SessionSeedArm::Carry
    );
    // Unparseable reads proceed, not skip.
    assert_eq!(session_seed_arm(true, None), SessionSeedArm::Carry);
}

/// The arm selection for the swap executor's per-session Keychain item-write —
/// pure, so the refreshless→sign-out / else→install decision is pinned on
/// every platform while the write itself only a Mac exercises. The unparseable
/// arm is Install (not SignOut): a torn read is not evidence of
/// refreshlessness, and the install leg that follows fails loudly on the
/// bytes.
#[test]
fn swap_item_arm_selection() {
    use crate::profile::{ClaudeCredentials, OAuthToken};
    let store = |refresh: Option<&str>| ClaudeCredentials {
        claude_ai_oauth: Some(OAuthToken {
            access_token: "a".to_string(),
            refresh_token: refresh.map(str::to_string),
            expires_at: None,
            scopes: None,
            subscription_type: None,
            ..OAuthToken::default_extra()
        }),
    };

    // Refreshless members (rolling sidecar, static mint) are signed out of the
    // item, never installed into it: the file layer must stay authoritative.
    assert_eq!(swap_item_arm(Some(&store(None))), SwapItemArm::SignOut);
    // Refreshable stores install.
    assert_eq!(swap_item_arm(Some(&store(Some("r")))), SwapItemArm::Install);
    // An unparseable read proceeds, not signs out.
    assert_eq!(swap_item_arm(None), SwapItemArm::Install);
}

/// The seed's failure disposition — pure, so the retry decision is pinned on
/// every platform while the seed and its watchdog-tick retry only a Mac
/// exercises. A locked keychain (the classified exit 36) is the one failure
/// that arms the retry: it clears the moment the keychain unlocks, and the
/// next credential tick re-runs the seed's carry-then-write. Everything else
/// keeps the pre-fix loud degrade — exit 51 and the unclassified codes
/// included, since nothing measures them transient.
#[test]
fn seed_degrade_disposition_retries_only_the_classified_transient() {
    use crate::claude::SecurityExitClass;
    assert_eq!(
        seed_degrade_disposition(SecurityExitClass::InteractionNotAllowed),
        SeedDegradeDisposition::RetryOnTick
    );
    assert_eq!(
        seed_degrade_disposition(SecurityExitClass::ItemNotFound),
        SeedDegradeDisposition::LogAndDegrade
    );
    assert_eq!(
        seed_degrade_disposition(SecurityExitClass::Unclassified),
        SeedDegradeDisposition::LogAndDegrade
    );
}

/// A session swapped from `item-retry-a` onto `item-retry-b` whose row still
/// names the outgoing store: the state a macOS swap leaves when its Keychain
/// item leg failed (the row follows only a landed leg). Returns the swap and
/// both stores.
fn swapped_with_unlanded_item_leg(
    prefix: &str,
) -> (
    std::sync::Arc<SessionSwap>,
    SwappedMarkers,
    PathBuf,
    PathBuf,
) {
    let launch = member(&format!("{prefix}-a"));
    let intended = member(&format!("{prefix}-b"));
    let launch_store = member_store(&launch);
    let intended_store = member_store(&intended);
    let (swap, markers) = lone_session(&launch, Isolation::Shared);
    assert_eq!(
        swap.swap_to(&format!("{prefix}-b")).expect("swap"),
        SwapOutcome::Swapped
    );
    crate::live_sessions::update_as_session(swap.session.as_str(), |fields| {
        fields.set_launch_store(launch_store.clone());
    })
    .expect("pose the unlanded item leg's row");
    (swap, markers, launch_store, intended_store)
}

fn item_retry_for(prefix: &str, store: &Path, previous_store: &Path) -> SwapItemRetry {
    SwapItemRetry {
        member: crate::profile::ProfileName::from(format!("{prefix}-b").as_str()),
        store: store.to_path_buf(),
        previous_store: previous_store.to_path_buf(),
    }
}

fn row_store(swap: &SessionSwap) -> Option<PathBuf> {
    crate::live_sessions::get(swap.session.as_str())
        .expect("row")
        .launch_store
}

/// A swap's item leg that failed on the locked-keychain transient stays armed:
/// the next poll runs the leg again, and while it keeps failing the
/// same way the target survives and the row keeps naming the outgoing store.
#[test]
fn a_transient_swap_item_leg_stays_armed_for_the_next_tick() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let (swap, _markers, launch_store, intended_store) =
            swapped_with_unlanded_item_leg("item-retry-t");
        let retry = item_retry_for("item-retry-t", &intended_store, &launch_store);
        swap.arm_item_retry(retry.clone());

        let calls = std::cell::Cell::new(0_u32);
        swap.retry_item_leg_with(|target| {
            calls.set(calls.get() + 1);
            assert_eq!(target, &retry, "the leg runs against the armed target");
            SwapItemLeg::Transient
        });

        assert_eq!(calls.get(), 1, "one attempt per tick");
        assert_eq!(
            swap.item_retry(),
            Some(retry),
            "a transient keeps the retry armed"
        );
        assert_eq!(
            row_store(&swap),
            Some(launch_store),
            "the row follows only a landed leg"
        );
    });
}

/// A retried leg that lands drops the target and moves the row's store onto
/// the incoming member, so the rotation verdict follows what the session reads.
#[test]
fn a_landed_swap_item_retry_repoints_the_row_and_disarms() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let (swap, _markers, launch_store, intended_store) =
            swapped_with_unlanded_item_leg("item-retry-l");
        swap.arm_item_retry(item_retry_for(
            "item-retry-l",
            &intended_store,
            &launch_store,
        ));

        swap.retry_item_leg_with(|_| SwapItemLeg::Landed);

        assert_eq!(swap.item_retry(), None, "a landed leg disarms the retry");
        assert_eq!(
            row_store(&swap),
            Some(intended_store),
            "the row names the store the session reads now"
        );

        let calls = std::cell::Cell::new(0_u32);
        swap.retry_item_leg_with(|_| {
            calls.set(calls.get() + 1);
            SwapItemLeg::Landed
        });
        assert_eq!(calls.get(), 0, "a disarmed retry runs no leg");
    });
}

/// A retry failing on anything but the transient is dropped: the pre-fix loud
/// degrade, and no leg on the ticks after it.
#[test]
fn a_failed_swap_item_retry_disarms_and_leaves_the_row() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let (swap, _markers, launch_store, intended_store) =
            swapped_with_unlanded_item_leg("item-retry-f");
        swap.arm_item_retry(item_retry_for(
            "item-retry-f",
            &intended_store,
            &launch_store,
        ));

        swap.retry_item_leg_with(|_| SwapItemLeg::Failed);

        assert_eq!(
            swap.item_retry(),
            None,
            "a non-transient failure disarms the retry"
        );
        assert_eq!(
            row_store(&swap),
            Some(launch_store),
            "a failed leg never repoints the row"
        );
    });
}

/// A target for a member the session no longer runs on is stale: the cell's
/// own swap legs own the item from there, so the retry runs no leg and drops.
#[test]
fn a_stale_swap_item_retry_runs_no_leg() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let (swap, _markers, launch_store, _intended_store) =
            swapped_with_unlanded_item_leg("item-retry-s");
        let elsewhere = tmp.path().join("not-the-cells-store.json");
        swap.arm_item_retry(item_retry_for("item-retry-s", &elsewhere, &launch_store));

        let calls = std::cell::Cell::new(0_u32);
        swap.retry_item_leg_with(|_| {
            calls.set(calls.get() + 1);
            SwapItemLeg::Landed
        });

        assert_eq!(
            calls.get(),
            0,
            "a target the cell's store no longer matches runs no leg"
        );
        assert_eq!(swap.item_retry(), None, "a stale target is dropped");
        assert_eq!(
            row_store(&swap),
            Some(launch_store),
            "a stale target never repoints the row"
        );
    });
}

/// A later swap owns the item from its own legs, so publishing it clears a
/// target an earlier swap armed.
#[test]
fn a_later_swap_clears_an_armed_item_retry() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let (swap, _markers, launch_store, intended_store) =
            swapped_with_unlanded_item_leg("item-retry-c");
        swap.arm_item_retry(item_retry_for(
            "item-retry-c",
            &intended_store,
            &launch_store,
        ));
        let third = member("item-retry-c-c");
        member_store(&third);

        assert_eq!(
            swap.swap_to("item-retry-c-c").expect("swap"),
            SwapOutcome::Swapped
        );

        assert_eq!(
            swap.item_retry(),
            None,
            "the later swap's publish clears the retry"
        );
    });
}

/// The retry writes the incoming member's store into the item, so it holds
/// that member's rotation lock for the leg; a lock another process holds skips
/// this tick without blocking the watchdog, and the target stays armed.
#[test]
fn a_swap_item_retry_skips_a_tick_while_the_rotation_lock_is_held() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let (swap, _markers, launch_store, intended_store) =
            swapped_with_unlanded_item_leg("item-retry-r");
        let retry = item_retry_for("item-retry-r", &intended_store, &launch_store);
        swap.arm_item_retry(retry.clone());
        let _holder = crate::testutil::hold_rotation_lock("item-retry-r-b");

        let calls = std::cell::Cell::new(0_u32);
        swap.retry_item_leg_with(|_| {
            calls.set(calls.get() + 1);
            SwapItemLeg::Landed
        });

        assert_eq!(calls.get(), 0, "a held rotation lock runs no leg this tick");
        assert_eq!(
            swap.item_retry(),
            Some(retry),
            "the target waits for the next tick"
        );
        assert_eq!(row_store(&swap), Some(launch_store), "nothing moved");
    });
}

/// The retry hands the session the incoming member's store, so a live stage a
/// failed persist left beside it lands before the leg reads that store.
#[test]
fn a_swap_item_retry_lands_the_incoming_members_stage_before_its_leg() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let (swap, _markers, launch_store, intended_store) =
            swapped_with_unlanded_item_leg("item-retry-g");
        staged_profile("item-retry-g-b");
        swap.arm_item_retry(item_retry_for(
            "item-retry-g",
            &intended_store,
            &launch_store,
        ));

        let seen = std::cell::RefCell::new(None);
        swap.retry_item_leg_with(|target| {
            let store: ClaudeCredentials =
                crate::profile::read_json_file(&target.store).expect("the incoming store");
            *seen.borrow_mut() = store.refresh_token().map(str::to_string);
            SwapItemLeg::Landed
        });

        assert_eq!(
            seen.into_inner().as_deref(),
            Some("rt-mid"),
            "the leg reads the landed stage, never the lagging store"
        );
    });
}

/// A target naming another member than the cell runs on is stale even when
/// its store matches the cell's.
#[test]
fn a_swap_item_retry_for_another_member_runs_no_leg() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let (swap, _markers, launch_store, intended_store) =
            swapped_with_unlanded_item_leg("item-retry-m");
        swap.arm_item_retry(SwapItemRetry {
            member: crate::profile::ProfileName::from("item-retry-m-a"),
            store: intended_store,
            previous_store: launch_store.clone(),
        });

        let calls = std::cell::Cell::new(0_u32);
        swap.retry_item_leg_with(|_| {
            calls.set(calls.get() + 1);
            SwapItemLeg::Landed
        });

        assert_eq!(calls.get(), 0, "a target for another member runs no leg");
        assert_eq!(swap.item_retry(), None, "a stale target is dropped");
        assert_eq!(row_store(&swap), Some(launch_store), "nothing moved");
    });
}

/// A retry that cannot start for a reason other than contention (here a stage
/// the unreadable spent record leaves unjudgeable) stays armed and says why
/// once, not once per poll.
#[test]
fn a_swap_item_retry_that_cannot_land_its_stage_says_why_once() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let (swap, _markers, launch_store, intended_store) =
            swapped_with_unlanded_item_leg("item-retry-w");
        staged_profile("item-retry-w-b");
        let dir = crate::profile::profile_dir(&crate::profile::ProfileName::from("item-retry-w-b"))
            .expect("dir");
        fs::remove_file(dir.join("auth.spent.json")).expect("drop the record");
        fs::create_dir(dir.join("auth.spent.json")).expect("block the record");
        let retry = item_retry_for("item-retry-w", &intended_store, &launch_store);
        swap.arm_item_retry(retry.clone());

        let lines = crate::logline::LogLines::new();
        let _capture = lines.capture_here();
        let calls = std::cell::Cell::new(0_u32);
        let start = std::time::Instant::now();
        // The second attempt runs past any first backoff gap, so both reach
        // the stage landing.
        for at in [start, start + ITEM_RETRY_MAX_GAP] {
            swap.retry_item_leg_at(at, |_| {
                calls.set(calls.get() + 1);
                SwapItemLeg::Landed
            });
        }

        assert_eq!(calls.get(), 0, "an unlanded stage runs no leg");
        assert_eq!(swap.item_retry(), Some(retry), "the target stays armed");
        assert_eq!(
            lines.snapshot(),
            vec![stall_line(&swap, "item-retry-w-b")],
            "two attempts on one cause announce it once"
        );
    });
}

/// The stall line for a retry onto `member` whose stage the unreadable spent
/// record leaves unjudgeable.
fn stall_line(swap: &SessionSwap, member: &str) -> String {
    format!(
        "clauth: session {} could not retry its swap onto {member}'s per-session Keychain leg: \
         could not lock '{member}' for a token refresh; check permissions on ~/.clauth. The \
         watchdog keeps retrying on its swap polls, backing off to at most every 30 s",
        swap.session.as_str()
    )
}

/// A retry onto `<prefix>-b` whose stage cannot land: the stage posed and the
/// spent record beside it blocked.
fn block_stage_landing(prefix: &str) {
    staged_profile(&format!("{prefix}-b"));
    let dir = crate::profile::profile_dir(&crate::profile::ProfileName::from(
        format!("{prefix}-b").as_str(),
    ))
    .expect("dir");
    fs::remove_file(dir.join("auth.spent.json")).expect("drop the record");
    fs::create_dir(dir.join("auth.spent.json")).expect("block the record");
}

/// The stall latch belongs to one swap: a swap away and back onto the same
/// member whose retry stalls on the same cause announces it again.
#[test]
fn a_later_swap_clears_the_item_retry_stall_latch() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let (swap, _markers, launch_store, intended_store) =
            swapped_with_unlanded_item_leg("item-retry-x");
        block_stage_landing("item-retry-x");
        swap.arm_item_retry(item_retry_for(
            "item-retry-x",
            &intended_store,
            &launch_store,
        ));
        let first = crate::logline::LogLines::new();
        {
            let _capture = first.capture_here();
            swap.retry_item_leg_with(|_| SwapItemLeg::Landed);
        }
        assert_eq!(first.snapshot(), vec![stall_line(&swap, "item-retry-x-b")]);

        // Clear the blocked landing so the swap back onto the member can land.
        let dir = crate::profile::profile_dir(&crate::profile::ProfileName::from("item-retry-x-b"))
            .expect("dir");
        fs::remove_dir(dir.join("auth.spent.json")).expect("unblock the record");
        fs::remove_file(dir.join("credentials.json.staged")).expect("drop the stage");
        let third_store = member_store(&member("item-retry-x-c"));
        assert_eq!(
            swap.swap_to("item-retry-x-c").expect("swap away"),
            SwapOutcome::Swapped
        );
        assert_eq!(
            swap.swap_to("item-retry-x-b").expect("swap back"),
            SwapOutcome::Swapped
        );

        block_stage_landing("item-retry-x");
        swap.arm_item_retry(item_retry_for(
            "item-retry-x",
            &intended_store,
            &third_store,
        ));
        let second = crate::logline::LogLines::new();
        {
            let _capture = second.capture_here();
            swap.retry_item_leg_with(|_| SwapItemLeg::Landed);
        }
        assert_eq!(
            second.snapshot(),
            vec![stall_line(&swap, "item-retry-x-b")],
            "the swap back published a fresh latch, so the same cause is announced again"
        );
    });
}

/// A transient pushes the next attempt out: a poll right after it runs no leg
/// and keeps the target armed.
#[test]
fn a_swap_item_retry_waits_out_its_backoff_after_a_transient() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let (swap, _markers, launch_store, intended_store) =
            swapped_with_unlanded_item_leg("item-retry-b");
        let retry = item_retry_for("item-retry-b", &intended_store, &launch_store);
        swap.arm_item_retry(retry.clone());

        let calls = std::cell::Cell::new(0_u32);
        for _ in 0..2 {
            swap.retry_item_leg_with(|_| {
                calls.set(calls.get() + 1);
                SwapItemLeg::Transient
            });
        }

        assert_eq!(
            calls.get(),
            1,
            "the poll right after a transient runs no leg"
        );
        assert_eq!(swap.item_retry(), Some(retry), "the target stays armed");
    });
}

/// The waits between attempts double from one second and cap at thirty, and
/// arming again starts over with an immediate attempt.
#[test]
fn a_swap_item_retry_backs_off_doubling_to_a_cap() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let (swap, _markers, launch_store, intended_store) =
            swapped_with_unlanded_item_leg("item-retry-k");
        let retry = item_retry_for("item-retry-k", &intended_store, &launch_store);
        swap.arm_item_retry(retry.clone());

        let calls = std::cell::Cell::new(0_u32);
        let attempt = |at: std::time::Instant| {
            swap.retry_item_leg_at(at, |_| {
                calls.set(calls.get() + 1);
                SwapItemLeg::Transient
            });
        };
        let mut at = std::time::Instant::now();
        attempt(at);
        assert_eq!(
            calls.get(),
            1,
            "the first attempt after arming is immediate"
        );
        for (n, secs) in [1_u64, 2, 4, 8, 16, 30, 30].into_iter().enumerate() {
            let gap = Duration::from_secs(secs);
            attempt(at + gap - Duration::from_millis(1));
            assert_eq!(
                calls.get(),
                1 + n as u32,
                "an attempt before the {secs} s gap ends runs no leg"
            );
            at += gap;
            attempt(at);
            assert_eq!(
                calls.get(),
                2 + n as u32,
                "the attempt at the {secs} s gap runs"
            );
        }

        swap.arm_item_retry(retry);
        swap.retry_item_leg_with(|_| {
            calls.set(calls.get() + 1);
            SwapItemLeg::Transient
        });
        assert_eq!(calls.get(), 9, "arming again resets the backoff");
    });
}

/// A held rotation lock skips the attempt by contention, not by the backoff:
/// once the holder lets go, the attempt one second later (past the first gap)
/// runs the leg.
#[test]
fn a_swap_item_retry_runs_once_a_held_rotation_lock_is_released() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let (swap, _markers, launch_store, intended_store) =
            swapped_with_unlanded_item_leg("item-retry-h");
        swap.arm_item_retry(item_retry_for(
            "item-retry-h",
            &intended_store,
            &launch_store,
        ));
        let calls = std::cell::Cell::new(0_u32);
        let start = std::time::Instant::now();
        {
            let _holder = crate::testutil::hold_rotation_lock("item-retry-h-b");
            swap.retry_item_leg_at(start, |_| {
                calls.set(calls.get() + 1);
                SwapItemLeg::Landed
            });
            assert_eq!(calls.get(), 0, "a held rotation lock runs no leg");
        }

        swap.retry_item_leg_at(start + Duration::from_secs(1), |_| {
            calls.set(calls.get() + 1);
            SwapItemLeg::Landed
        });

        assert_eq!(
            calls.get(),
            1,
            "the released lock lets the next attempt run"
        );
        assert_eq!(swap.item_retry(), None, "the landed leg disarms the retry");
        assert_eq!(row_store(&swap), Some(intended_store));
    });
}

/// While a retry is armed the item still holds the login of the retry's
/// outgoing store, so a swap's carry-back goes there; with none armed it goes
/// into the store the cell names.
#[test]
fn a_swap_carries_the_item_into_an_armed_retrys_outgoing_store() {
    let canonical = Path::new("/profiles/carry-b/credentials.json");
    let outgoing = Path::new("/profiles/carry-a/credentials.json");
    assert_eq!(
        item_carry_target(Some(outgoing), canonical),
        Path::new("/profiles/carry-a/credentials.json")
    );
    assert_eq!(
        item_carry_target(None, canonical),
        Path::new("/profiles/carry-b/credentials.json")
    );
}

/// The convergence's carry would write the item's login into this member's
/// store, and while a retry is armed that login is the outgoing member's: the
/// poll converges nothing until the retry lands or drops.
#[test]
fn a_convergence_waits_while_an_item_retry_is_armed() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        if !host_poses(tmp.path(), "a convergence relink") {
            return;
        }
        let launch = member("conv-r");
        let store = member_store(&launch);
        let (swap, _launch_markers) = lone_session(&launch, Isolation::Shared);
        let sidecar = crate::profile::profile_dir(&crate::profile::ProfileName::from("conv-r"))
            .expect("profile_dir")
            .join("session-token.json");
        write_creds(&sidecar, None);
        let retry = SwapItemRetry {
            member: crate::profile::ProfileName::from("conv-r"),
            store: store.clone(),
            previous_store: tmp.path().join("outgoing-store.json"),
        };
        swap.arm_item_retry(retry.clone());

        swap.poll();

        assert_eq!(
            swap.canonical(),
            store,
            "no convergence while the retry is armed"
        );
        assert_eq!(
            fs::read_link(swap.runtime.join(".credentials.json")).expect("read link"),
            store,
            "the link stays on the store the retry is for"
        );
        assert_eq!(
            swap.item_retry(),
            Some(retry),
            "the poll leaves the retry armed"
        );

        swap.retry_item_leg_with(|_| SwapItemLeg::Failed);
        swap.poll();

        assert_eq!(
            swap.canonical(),
            sidecar,
            "a dropped retry lets the next poll converge"
        );
    });
}

/// A Keychain leg's failure retries on the watchdog only where it clears on its
/// own: a locked keychain (skipped or classified) and a sweep's in-flight
/// delete. Every other class drops to a later start or swap.
#[test]
fn keychain_leg_retry_clears_retries_only_what_clears_on_its_own() {
    use crate::claude::SecurityExitClass;
    let unlock = Some("so it lands once the keychain unlocks");
    assert_eq!(
        keychain_leg_retry_clears(KeychainLegFault::SkippedLocked),
        unlock
    );
    assert_eq!(
        keychain_leg_retry_clears(KeychainLegFault::DeleteInFlight),
        Some("so it lands once the sweep's delete completes and its in-flight record clears")
    );
    assert_eq!(
        keychain_leg_retry_clears(KeychainLegFault::Exit(
            SecurityExitClass::InteractionNotAllowed
        )),
        unlock
    );
    assert_eq!(
        keychain_leg_retry_clears(KeychainLegFault::Exit(SecurityExitClass::ItemNotFound)),
        None
    );
    assert_eq!(
        keychain_leg_retry_clears(KeychainLegFault::Exit(SecurityExitClass::Unclassified)),
        None
    );
}

/// An adopted row whose store still names another store than the link
/// resolves to records item legs the launcher left unlanded: the adoption
/// re-arms them. A landed row, or one never swapped, arms nothing.
#[test]
fn an_adoption_rearms_only_an_item_leg_its_row_left_unlanded() {
    let member = crate::profile::ProfileName::from("adopt-retry-b");
    let canonical = Path::new("/profiles/adopt-retry-b/credentials.json");
    let outgoing = Path::new("/profiles/adopt-retry-a/credentials.json");
    assert_eq!(
        adopted_item_retry(Some(outgoing), canonical, &member),
        Some(SwapItemRetry {
            member: crate::profile::ProfileName::from("adopt-retry-b"),
            store: PathBuf::from("/profiles/adopt-retry-b/credentials.json"),
            previous_store: PathBuf::from("/profiles/adopt-retry-a/credentials.json"),
        })
    );
    assert_eq!(
        adopted_item_retry(Some(canonical), canonical, &member),
        None,
        "a landed leg moved the row onto the link's store"
    );
    assert_eq!(
        adopted_item_retry(None, canonical, &member),
        None,
        "a row that never swapped"
    );
}

/// The watchdog keeps a static token over a differing runtime file, and what
/// the session's Claude Code saved beside that same token (a `/design-login`)
/// went with the file. It is synced into the sidecar first. A runtime file
/// holding another login (a session-side re-login) syncs nothing.
#[test]
fn the_watchdog_keeps_what_a_session_saved_beside_a_static_token() {
    let _home = HomeSandbox::new();
    let tmp = tempfile::tempdir().expect("tempdir");
    let canonical = tmp.path().join("session-token.json");
    let link_path = tmp.path().join(".credentials.json");
    let token =
        br#"{"claudeAiOauth":{"accessToken":"sk-ant-oat01-static","expiresAt":4102444800000}}"#;
    fs::write(&canonical, token).expect("write sidecar");
    fs::write(
        &link_path,
        br#"{"claudeAiOauth":{"accessToken":"sk-ant-oat01-static","expiresAt":4102444800000},"designOauth":{"accessToken":"design-1"}}"#,
    )
    .expect("write runtime file");

    assert!(!sync_credentials_unlocked(&link_path, &canonical).expect("sync"));

    let sidecar: serde_json::Value =
        serde_json::from_slice(&fs::read(&canonical).expect("read")).expect("parse");
    assert_eq!(
        sidecar["claudeAiOauth"]["accessToken"],
        "sk-ant-oat01-static"
    );
    assert_eq!(sidecar["designOauth"]["accessToken"], "design-1");

    fs::write(&canonical, token).expect("reset sidecar");
    // The watchdog relinked the slot onto the sidecar; a write through that link
    // would land in the sidecar itself, so the re-login replaces the link.
    fs::remove_file(&link_path).expect("drop the relinked slot");
    fs::write(
        &link_path,
        br#"{"claudeAiOauth":{"accessToken":"relogin","refreshToken":"r"},"designOauth":{"accessToken":"design-2"}}"#,
    )
    .expect("write re-login");
    assert!(!sync_credentials_unlocked(&link_path, &canonical).expect("sync"));
    assert_eq!(
        fs::read(&canonical).expect("read"),
        token.to_vec(),
        "a re-login syncs nothing"
    );
}

// ── the refresh chain: the copy-back never rewinds onto a spent token ────────

const CHAIN_P1: &[u8] =
    br#"{"claudeAiOauth":{"accessToken":"at-old","refreshToken":"rt-old","expiresAt":1000}}"#;
const CHAIN_P2: &[u8] =
    br#"{"claudeAiOauth":{"accessToken":"at-new","refreshToken":"rt-new","expiresAt":2000}}"#;

/// A store dir holding P2 as canonical, a runtime copy holding P1 written
/// later, and a spent record beside the store naming P1 (fp("rt-old"),
/// hand-computed: the first 8 bytes of its SHA-256, hex).
fn chain_rewind_fixture(tmp: &Path) -> (PathBuf, PathBuf) {
    let canonical = tmp.join("credentials.json");
    let runtime = tmp.join(".credentials.json");
    fs::write(&canonical, CHAIN_P2).expect("write canonical");
    fs::write(&runtime, CHAIN_P1).expect("write runtime");
    fs::write(tmp.join("auth.spent.json"), r#"["068bfce5d78d07ac"]"#).expect("record P1");
    set_mtime(&canonical, SystemTime::now() - Duration::from_secs(60));
    set_mtime(&runtime, SystemTime::now());
    (canonical, runtime)
}

/// C8: P2 persisted; a fake-mode runtime copy still holding the spent P1 is
/// newer. The mirror leaves canonical on P2 and hands the session P2 back.
#[test]
fn the_fake_mode_mirror_never_copies_a_spent_runtime_login_over_canonical() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (canonical, runtime) = chain_rewind_fixture(tmp.path());

    let wrote = mirror_credentials(&runtime, &canonical).expect("mirror");

    assert!(!wrote, "nothing was copied into the store");
    assert_eq!(fs::read(&canonical).expect("read"), CHAIN_P2);
    assert_eq!(fs::read(&runtime).expect("read"), CHAIN_P2);
}

/// §G's real-mode twin: the watchdog drain keeps canonical over a newer runtime
/// file whose refresh token is spent, and relinks the session onto it.
#[test]
fn the_real_mode_drain_never_copies_a_spent_runtime_login_over_canonical() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (canonical, runtime) = chain_rewind_fixture(tmp.path());

    let wrote = sync_credentials_unlocked(&runtime, &canonical).expect("drain");

    assert!(!wrote);
    assert_eq!(fs::read(&canonical).expect("read"), CHAIN_P2);
    assert!(
        runtime
            .symlink_metadata()
            .expect("link")
            .file_type()
            .is_symlink(),
        "the session reads canonical again"
    );
}

/// The chain-rewind fixture with an UNREADABLE record (a directory at its path):
/// whether the runtime login is spent is unknown.
fn chain_unknown_fixture(tmp: &Path) -> (PathBuf, PathBuf) {
    let paths = chain_rewind_fixture(tmp);
    fs::remove_file(tmp.join("auth.spent.json")).expect("drop the record");
    fs::create_dir(tmp.join("auth.spent.json")).expect("block the record");
    paths
}

/// An unreadable record beside the store can neither vouch for the runtime
/// login nor condemn it, so the drain writes neither side: canonical stays, and
/// the session keeps its own file rather than being relinked away from what may
/// be the chain head.
#[test]
fn the_real_mode_drain_keeps_both_sides_when_the_record_is_unreadable() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (canonical, runtime) = chain_unknown_fixture(tmp.path());

    let wrote = sync_credentials_unlocked(&runtime, &canonical).expect("drain");

    assert!(!wrote);
    assert_eq!(fs::read(&canonical).expect("read"), CHAIN_P2);
    assert!(
        !runtime
            .symlink_metadata()
            .expect("runtime file")
            .file_type()
            .is_symlink(),
        "the session's own file is not relinked away"
    );
    assert_eq!(fs::read(&runtime).expect("read"), CHAIN_P1);
}

/// The fake-mode twin: neither side is copied over the other.
#[test]
fn the_fake_mode_mirror_keeps_both_sides_when_the_record_is_unreadable() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (canonical, runtime) = chain_unknown_fixture(tmp.path());

    let wrote = mirror_credentials(&runtime, &canonical).expect("mirror");

    assert!(!wrote);
    assert_eq!(fs::read(&canonical).expect("read"), CHAIN_P2);
    assert_eq!(fs::read(&runtime).expect("read"), CHAIN_P1);
}

/// The macOS Keychain carry's spent gate, on every platform: an unspent item
/// login is written, a spent one keeps the store, and an unreadable record
/// refuses the carry so the swap stays inert rather than overwriting the item.
#[test]
fn the_session_item_carry_refuses_while_the_record_is_unreadable() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (canonical, _) = chain_rewind_fixture(tmp.path());
    let item: ClaudeCredentials = serde_json::from_slice(CHAIN_P1).expect("parse P1");
    let record = tmp.path().join("auth.spent.json");

    assert!(!crate::claude::session_item_carry_gate(&canonical, &item).expect("spent"));
    fs::write(&record, "[]").expect("an empty record");
    assert!(crate::claude::session_item_carry_gate(&canonical, &item).expect("unspent"));
    fs::remove_file(&record).expect("drop the record");
    fs::create_dir(&record).expect("block the record");
    let err = crate::claude::session_item_carry_gate(&canonical, &item)
        .expect_err("an unknown record refuses the carry");
    assert_eq!(
        err.to_string(),
        format!(
            "cannot read the spent-token record beside {}, so whether the per-session Keychain \
             item's refresh token was already consumed is unknown: the item is left as it is; \
             check permissions on ~/.clauth",
            canonical.display()
        )
    );
}

/// A profile whose store holds the spent `rt-old`, a live stage over it holding
/// `rt-mid`, and the record naming `rt-old`.
fn staged_profile(name: &str) -> crate::profile::Profile {
    let mut profile = make_profile(name);
    profile.credentials = Some(serde_json::from_slice(CHAIN_P1).expect("parse the fixture login"));
    register_profile(&profile);
    let dir = crate::profile::profile_dir(&profile.name).expect("dir");
    fs::write(
        dir.join("credentials.json.staged"),
        r#"{"base":"068bfce5d78d07ac","base_access":"at-old",
            "creds":{"claudeAiOauth":{"accessToken":"at-mid","refreshToken":"rt-mid"}}}"#,
    )
    .expect("write the stage");
    fs::write(dir.join("auth.spent.json"), r#"["068bfce5d78d07ac"]"#).expect("record P1");
    profile
}

fn stored_refresh_of(name: &str) -> Option<String> {
    let dir = crate::profile::profile_dir(&crate::profile::ProfileName::from(name)).expect("dir");
    let creds: crate::profile::ClaudeCredentials =
        crate::profile::read_json_file(&dir.join("credentials.json")).expect("store");
    creds.refresh_token().map(str::to_string)
}

/// C22: a session start lands a live stage before it builds its tree, so the
/// session's first store read is the chain head.
#[test]
fn a_session_start_lands_a_live_stage_before_its_tree_reads_the_store() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        if !host_poses(tmp.path(), "a linked session store") {
            return;
        }
        fake_claude_home(tmp.path());
        let profile = staged_profile("chain-start");

        let rt = ProfileRuntime::acquire(&profile, Isolation::Shared, &[], false, Launcher::Own)
            .expect("acquire");

        assert_eq!(stored_refresh_of("chain-start").as_deref(), Some("rt-mid"));
        let seen: crate::profile::ClaudeCredentials =
            crate::profile::read_json_file(&rt.config_dir().join(".credentials.json"))
                .expect("the session's credentials");
        assert_eq!(seen.refresh_token(), Some("rt-mid"));
        let dir = crate::profile::profile_dir(&profile.name).expect("dir");
        assert!(!dir.join("credentials.json.staged").exists());
    });
}

/// C27 (start): a live stage whose landing cannot complete refuses the start as
/// Transient before any tree is built from the lagging store. The flock is
/// FREE here, so the start's own state-flock hold would succeed: only the
/// refusal stands between the start and a tree seeded from `rt-old`. The land
/// fails because the record beside a store still bound to the stage cannot be
/// read (`Unjudgeable`).
#[test]
fn a_session_start_refuses_while_a_live_stage_cannot_land() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        fake_claude_home(tmp.path());
        let profile = staged_profile("chain-start-wedge");
        let profile_dir = crate::profile::profile_dir(&profile.name).expect("dir");
        fs::remove_file(profile_dir.join("auth.spent.json")).expect("drop the record");
        fs::create_dir(profile_dir.join("auth.spent.json")).expect("block the record");

        let err = ProfileRuntime::acquire(&profile, Isolation::Shared, &[], false, Launcher::Own)
            .map(|_| ())
            .expect_err("a failed land refuses the start");

        assert_eq!(
            err.to_string(),
            "could not lock 'chain-start-wedge' for a token refresh; check permissions on ~/.clauth"
        );
        assert_eq!(
            stored_refresh_of("chain-start-wedge").as_deref(),
            Some("rt-old")
        );
        assert!(profile_dir.join("credentials.json.staged").exists());
        assert!(
            !profile_dir
                .read_dir()
                .expect("profile dir")
                .flatten()
                .any(|e| e.file_name().to_string_lossy().starts_with("runtime")),
            "nothing was seeded from the lagging store"
        );
    });
}

/// A swap onto a member holding a live stage lands it before the repoint, so
/// the session is handed the chain head, and the stage is gone.
#[test]
fn a_swap_lands_the_incoming_members_live_stage_before_it_repoints() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let launch = member("chain-swap-a");
        member_store(&launch);
        let intended = staged_profile("chain-swap-b");
        let (swap, _launch_markers) = lone_session(&launch, Isolation::Shared);

        assert_eq!(
            swap.swap_to("chain-swap-b").expect("swap"),
            SwapOutcome::Swapped
        );

        assert_eq!(stored_refresh_of("chain-swap-b").as_deref(), Some("rt-mid"));
        let dir = crate::profile::profile_dir(&intended.name).expect("dir");
        assert!(!dir.join("credentials.json.staged").exists());
    });
}

/// The outgoing member's spent record cannot be read while Claude Code's own
/// refresh sits in the session's runtime file: the drain cannot judge that
/// login, so the swap refuses before any link, marker or row moves, and the
/// file that may hold the member's only live refresh token stays the session's.
#[test]
fn a_swap_refuses_while_the_drain_cannot_judge_the_session_file() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let launch = member("unjudged-a");
        let launch_store = member_store(&launch);
        member_store(&member("unjudged-b"));
        let (swap, _launch_markers) = lone_session(&launch, Isolation::Shared);
        let sid = swap.session.as_str().to_string();
        let link = swap.runtime.join(".credentials.json");
        fs::remove_file(&link).expect("drop the link");
        fs::write(&link, CHAIN_P1).expect("Claude Code's refreshed login");
        set_mtime(&launch_store, SystemTime::now() - Duration::from_secs(60));
        let store_before = fs::read(&launch_store).expect("store");
        fs::create_dir(
            launch_store
                .parent()
                .expect("store dir")
                .join("auth.spent.json"),
        )
        .expect("block the record");

        let outcome = swap.swap_to("unjudged-b").expect("swap");

        assert!(
            !link
                .symlink_metadata()
                .expect("runtime file")
                .file_type()
                .is_symlink(),
            "the session's own file is not relinked away"
        );
        assert_eq!(fs::read(&link).expect("runtime file"), CHAIN_P1);
        assert_eq!(fs::read(&launch_store).expect("store"), store_before);
        let row = crate::live_sessions::get(&sid).expect("row");
        assert_eq!(row.current_member, None, "the row still names no swap");
        assert!(swap.cell().held.is_empty(), "no marker was claimed");
        assert_eq!(swap.member(), "unjudged-a");
        assert_eq!(
            outcome,
            SwapOutcome::Refused(SwapRefused::OutgoingRecordUnreadable(
                "unjudged-a".to_string()
            ))
        );
    });
}

/// The announced refusal line, read off the log: an unreadable outgoing
/// record names the session's own member, every other refusal the intended
/// one.
#[test]
fn an_unreadable_outgoing_record_is_announced_against_the_session_member() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let launch = member("announce-work");
        member_store(&launch);
        let (swap, _launch_markers) = lone_session(&launch, Isolation::Shared);
        let sid = swap.session.as_str().to_string();
        let lines = crate::logline::LogLines::new();
        let capture = lines.capture_here();

        swap.announce_refusal(
            "announce-home",
            SwapRefused::OutgoingRecordUnreadable("announce-work".to_string()),
        );
        swap.announce_refusal("announce-home", SwapRefused::Disabled);
        drop(capture);

        assert_eq!(
            lines.snapshot(),
            [
                format!(
                    "clauth: session {sid} stays on announce-work: cannot read announce-work's \
                     spent-token record, so the session keeps its own credential copy"
                ),
                format!(
                    "clauth: session {sid} stays on announce-work: announce-home is not \
                     swappable (it is disabled)"
                ),
            ]
        );
    });
}

/// T33b1 check 7: the account's background pair `runtime-bg` + `sessions-bg`
/// is a shape every runtime-dir parser knows (GC pairs and collects it, both
/// config reconcilers walk it as a shared tree, the codex scrub sees a clauth
/// tree), and nothing reads a sid off it, so `tps` and the hook note skip it.
/// Its neighbours stay outside: an isolated background tree is refused at
/// launch, and a suffix past `bg` is no clauth name.
#[test]
fn the_background_pair_is_a_runtime_dir_shape_with_no_sid() {
    assert!(is_runtime_dir_name("runtime-bg"));
    assert!(is_sessions_dir_name("sessions-bg"));
    assert!(is_shared_runtime_dir_name("runtime-bg"));
    assert_eq!(
        paired_sessions_name("runtime-bg").as_deref(),
        Some("sessions-bg")
    );
    assert_eq!(
        paired_runtime_name("sessions-bg").as_deref(),
        Some("runtime-bg")
    );
    assert_eq!(sid_of_runtime_dir_name("runtime-bg"), None);
    assert!(is_clauth_runtime_path(Path::new(
        "/home/dev/.clauth/profiles/wired/runtime-bg"
    )));
    for name in [
        "runtime-bgx",
        "runtime-xbg",
        "runtime-isolated-bg",
        "runtime-bg-4242-0",
        "runtime-bg-",
    ] {
        assert!(!is_runtime_dir_name(name), "{name} is no clauth tree");
    }
    for name in ["sessions-bgx", "sessions-isolated-bg"] {
        assert!(
            !is_sessions_dir_name(name),
            "{name} is no clauth marker dir"
        );
    }
}

/// A background row is live through its `sessions-bg/<sid>` marker: neither
/// per-session marker dir holds anything for it, so the row sweep and the tally
/// would reap the daemon's row without the background candidate.
#[test]
fn a_background_row_is_live_through_its_sessions_bg_marker() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let name = crate::profile::ProfileName::from("bglive");
        let profile = crate::profile::profile_dir(&name).expect("profile dir");
        let sessions = profile.join("sessions-bg");
        fs::create_dir_all(&sessions).expect("mkdir sessions-bg");
        assert!(!session_row_is_live(&name, false, "7100-0"));
        let marker = open_pid_file(&sessions.join("7100-0")).expect("marker");
        marker.lock().expect("hold marker");
        assert!(
            session_row_is_live(&name, false, "7100-0"),
            "the background marker is what makes the row live"
        );
        drop(marker);
        assert!(!session_row_is_live(&name, false, "7100-0"));
    });
}

/// T33b1 check 8, the marker half: the daemon's own background hold is exempt
/// from the macOS rotation gate's marker term, while a launcher's marker in the
/// same `sessions-bg/` is not. The two cases differ only in the row's
/// `background_hold` bit, and no job store exists, so the job term is empty and
/// the marker term alone decides.
#[test]
fn only_a_launchers_background_marker_blocks_a_macos_rotation() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let name = crate::profile::ProfileName::from("bgrot");
        let profile = crate::profile::profile_dir(&name).expect("profile dir");
        let sessions = profile.join("sessions-bg");
        fs::create_dir_all(&sessions).expect("mkdir sessions-bg");
        let store = profile.join("credentials.json");
        write_creds(&store, Some("rt-bg"));

        for (sid, hold, want) in [
            (
                "7001-0",
                false,
                Some(crate::runtime::RotationBlock::LiveSession),
            ),
            ("7002-0", true, None),
        ] {
            let marker = open_pid_file(&sessions.join(sid)).expect("marker");
            marker.lock().expect("hold marker");
            let mut row = crate::testutil::live_row(sid, name.as_str());
            row.background = true;
            row.background_hold = hold;
            row.launch_store = Some(store.clone());
            crate::live_sessions::register(&row).expect("register");
            assert_eq!(rotation_block_on(&name, true), want, "{sid}");
            drop(marker);
            crate::live_sessions::unregister(sid).expect("unregister");
        }
    });
}

/// T33b1 check 6, the un-Dropped half: a daemon killed by SIGTERM never runs
/// its runtime's teardown, so its marker file, row and tree stay behind with
/// no flock. The sweep spares the background pair while a job names the tree
/// or any marker in `sessions-bg/` is held, and collects it once neither does.
#[test]
fn gc_collects_the_background_pair_once_no_marker_is_held_and_no_job_names_it() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let profile = tmp.path().join(".clauth").join("profiles").join("bgp");
        let runtime = profile.join("runtime-bg");
        let sessions = profile.join("sessions-bg");
        fs::create_dir_all(&runtime).expect("mkdir runtime");
        fs::create_dir_all(&sessions).expect("mkdir sessions");
        fs::write(runtime.join(".claude.json"), b"{}").expect("seed");
        drop(open_pid_file(&sessions.join("4242-0")).expect("dead marker"));

        let job = write_claude_job(tmp.path(), "b6a1c0de", &runtime);
        gc_stale_runtimes();
        assert!(
            runtime.join(".claude.json").is_file(),
            "a job naming the tree spares it"
        );

        fs::remove_dir_all(&job).expect("claude rm");
        let held = open_pid_file(&sessions.join("4243-0")).expect("live marker");
        held.lock().expect("hold");
        gc_stale_runtimes();
        assert!(
            runtime.join(".claude.json").is_file(),
            "a held marker spares the tree"
        );

        drop(held);
        gc_stale_runtimes();
        assert!(!runtime.exists(), "the idle background tree is collected");
        assert!(!sessions.exists(), "its marker dir goes with it");
    });
}

/// F3: a background launcher's spawn-settings file is spared by a job on
/// `runtime-bg` whose flags clauth cannot read. `spares_settings`' fallback is
/// the only rule that can spare it, and it must be handed the tree the launcher
/// actually occupied.
#[test]
fn gc_spares_a_background_launchers_settings_file_behind_an_unreadable_flags_job() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        let name = crate::profile::ProfileName::from("bgsettings");
        let profile = crate::profile::profile_dir(&name).expect("profile dir");
        let tree = profile.join("runtime-bg");
        fs::create_dir_all(profile.join("sessions-bg")).expect("mkdir sessions-bg");
        fs::create_dir_all(&tree).expect("mkdir runtime-bg");
        let settings = spawn_settings_file(&tree, "5858-0");
        fs::write(&settings, b"{}").expect("launcher settings file");
        // A list element that is not a string reads as unreadable flags, so the
        // fail-closed tree fallback is the only rule that can spare the file.
        crate::testutil::write_claude_job_flags(
            tmp.path(),
            "b6a1c0de",
            &tree,
            serde_json::json!([1]),
        );

        gc_stale_runtimes();

        assert!(
            settings.is_file(),
            "a background launcher's settings file must survive a job whose flags \
             clauth cannot read"
        );
    });
}

/// N2: a background holder is fixed on its ACCOUNT, not frozen on its credential
/// pair. Its own account's convergence runs like any session's — a hold adopted
/// before the account was armed for rolling tokens must still move onto the
/// sidecar, even while a cross-account intent stands on its row — while a move
/// onto another account is refused, as `precondition` keeps enforcing for every
/// caller.
#[test]
fn a_background_holder_converges_and_refuses_a_cross_account_swap() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        if !host_poses(tmp.path(), "a convergence relink") {
            return;
        }
        let launch = member("fixed-a");
        let store = member_store(&launch);
        let session = SessionId::mint();
        let sid = session.as_str().to_string();
        let paths = SessionPaths::resolve_background(&launch.name, &session).expect("paths");
        crate::profile::mkdir_700(&paths.runtime).expect("mkdir");
        create_symlink(&store, &paths.runtime.join(".credentials.json")).expect("link creds");
        // The launch marker this holder already owns, as `acquire` leaves it:
        // publishing a convergence asserts it is still held.
        let _launch_markers = stamp_swapped_markers(&paths)
            .expect("stamp the launch marker")
            .expect("the launch member's marker must be free in a fresh sandbox");
        let mut row = crate::live_sessions::LiveSession::starting(
            &session,
            "fixed-a",
            crate::harness::Harness::Claude,
            false,
            false,
            Launcher::Own,
            Some(store.to_path_buf()),
            None,
        );
        // A standing cross-account intent (an older clauth's `switch`, a hand
        // edit): `poll` must refuse it and still converge the own account.
        row.intended_member = Some("fixed-b".to_string());
        crate::live_sessions::register(&row).expect("register row");
        let swap = SessionSwap::new(
            session,
            Isolation::Shared,
            LinkMode::Real,
            SessionTree::Background(BackgroundHold::Daemon),
            &launch,
            store,
            &paths,
        );

        // Arm the account for rolling tokens under the hold.
        let sidecar = crate::profile::profile_dir(&crate::profile::ProfileName::from("fixed-a"))
            .expect("profile_dir")
            .join("session-token.json");
        write_creds(&sidecar, None);

        swap.poll();

        assert_eq!(
            fs::read_link(paths.runtime.join(".credentials.json")).expect("read link"),
            sidecar,
            "a background holder's converge runs onto its own account's sidecar"
        );
        assert_eq!(swap.canonical(), sidecar, "the cell follows the link");
        assert_eq!(swap.member(), "fixed-a", "the account does not change");
        let row = crate::live_sessions::get(&sid).expect("row");
        assert_eq!(
            row.launch_store.as_deref(),
            Some(sidecar.as_path()),
            "the row's launch_store names the sidecar, so the rotation refusal lifts"
        );

        assert_eq!(
            swap.precondition("fixed-b").map(|plan| plan.member),
            Err(SwapRefused::FixedAccount),
            "a background holder never moves onto another account"
        );
        assert_eq!(swap.member(), "fixed-a", "the refusal moved nothing");
    });
}

/// A fresh session's seed copies home's bytes into a new runtime tree while a
/// sibling runtime copy holds a newer Claude Code write no pass has propagated
/// yet: the seed's copy is stamped at the floor, so it cannot outrank that
/// write, and the next pass keeps it and the new copy converges on it.
#[test]
fn a_fresh_seed_never_outranks_a_siblings_unpropagated_write() {
    let home = HomeSandbox::new();
    let h = home.home().to_path_buf();
    let global = h.join(".claude.json");
    let r1 = seed_test_runtime(&h, "p1", "runtime-4242-0").join(".claude.json");
    let r2dir = seed_test_runtime(&h, "p2", "runtime-4242-1");
    fs::create_dir_all(r1.parent().expect("has parent")).expect("mkdir");
    fs::create_dir_all(&r2dir).expect("mkdir");
    fs::create_dir_all(h.join(".claude")).expect("mkdir");
    let write = |p: &std::path::Path, v: serde_json::Value| {
        fs::write(p, serde_json::to_vec_pretty(&v).expect("serialize")).expect("write");
    };
    write(
        &global,
        serde_json::json!({"numStartups": 2, "oauthAccount": {"emailAddress": "g@x"}}),
    );
    write(
        &r1,
        serde_json::json!({"numStartups": 3, "ccR1": true, "oauthAccount": {"emailAddress": "r1@x"}}),
    );
    let t = |s: u64| SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000 + s);
    set_mtime(&global, t(10));
    set_mtime(&r1, t(20));

    seed_claude_json(&r2dir, &h.join(".claude")).expect("seed");
    crate::claude_json::reset_last_synced();
    crate::claude_json::sync_once().expect("next pass");

    let read = |p: &std::path::Path| {
        serde_json::from_slice::<serde_json::Value>(&fs::read(p).expect("read")).expect("parse")
    };
    assert_eq!(
        read(&r1),
        serde_json::json!({"numStartups": 3, "ccR1": true, "oauthAccount": {"emailAddress": "r1@x"}}),
        "the sibling keeps its Claude Code write"
    );
    let r2 = read(&r2dir.join(".claude.json"));
    assert_eq!(
        r2["numStartups"],
        serde_json::json!(3),
        "the seed converges on it"
    );
    assert_eq!(
        r2["ccR1"],
        serde_json::json!(true),
        "the seed converges on it"
    );
    assert_eq!(
        read(&global)["ccR1"],
        serde_json::json!(true),
        "home converges on it"
    );
}

fn seed_test_runtime(home: &std::path::Path, profile: &str, runtime: &str) -> std::path::PathBuf {
    home.join(".clauth")
        .join("profiles")
        .join(profile)
        .join(runtime)
}

fn seed_test_doc(path: &std::path::Path) -> serde_json::Value {
    serde_json::from_slice(&fs::read(path).expect("read")).expect("parse")
}

/// Home made into clauth's echo of a runtime write at t10 (the first pass), and
/// a sibling holding a newer unpropagated Claude Code write at t20. Returns
/// `(global, r1, r2, r3dir)`; `r3dir` is the tree the seed under test fills.
fn seed_from_echo_home(
    h: &std::path::Path,
) -> (
    std::path::PathBuf,
    std::path::PathBuf,
    std::path::PathBuf,
    std::path::PathBuf,
) {
    let global = h.join(".claude.json");
    let r1 = seed_test_runtime(h, "p1", "runtime-4242-0").join(".claude.json");
    let r2 = seed_test_runtime(h, "p2", "runtime-4242-1").join(".claude.json");
    let r3dir = seed_test_runtime(h, "p3", "runtime-4242-2");
    fs::create_dir_all(r1.parent().expect("has parent")).expect("mkdir");
    fs::create_dir_all(r2.parent().expect("has parent")).expect("mkdir");
    fs::create_dir_all(&r3dir).expect("mkdir");
    fs::create_dir_all(h.join(".claude")).expect("mkdir");
    let write = |p: &std::path::Path, v: serde_json::Value| {
        fs::write(p, serde_json::to_vec_pretty(&v).expect("serialize")).expect("write");
    };
    let t = |s: u64| SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000 + s);
    write(&global, serde_json::json!({"numStartups": 1}));
    write(&r1, serde_json::json!({"numStartups": 2, "ccR1": true}));
    set_mtime(&global, t(5));
    set_mtime(&r1, t(10));
    crate::claude_json::reset_last_synced();
    crate::claude_json::sync_once().expect("first pass makes home an echo");
    write(&r2, serde_json::json!({"numStartups": 3, "ccR2": true}));
    set_mtime(&r2, t(20));
    (global, r1, r2, r3dir)
}

fn assert_every_copy_keeps_the_siblings_write(copies: &[(&str, &std::path::Path)]) {
    for (name, path) in copies {
        let doc = seed_test_doc(path);
        assert_eq!(
            doc["ccR2"],
            serde_json::json!(true),
            "{name} keeps the sibling's Claude Code write"
        );
        assert_eq!(
            doc["numStartups"],
            serde_json::json!(3),
            "{name} keeps the sibling's Claude Code write"
        );
    }
}

/// Home is clauth's own echo (a fresh mtime carrying a t10 write) when a new
/// tree is seeded: the seed carries home's rank of t10, never home's fresh
/// mtime, so a sibling's unpropagated t20 write still wins the next pass.
#[test]
fn a_seed_from_an_echo_home_never_outranks_a_siblings_newer_write() {
    let home = HomeSandbox::new();
    let h = home.home().to_path_buf();
    let (global, r1, r2, r3dir) = seed_from_echo_home(&h);

    seed_claude_json(&r3dir, &h.join(".claude")).expect("seed");
    crate::claude_json::reset_last_synced();
    crate::claude_json::sync_once().expect("next pass");

    let r3 = r3dir.join(".claude.json");
    assert_every_copy_keeps_the_siblings_write(&[
        ("home", &global),
        ("r1", &r1),
        ("r2", &r2),
        ("the seeded copy", &r3),
    ]);
}

struct SeedPreLockHookGuard;

impl Drop for SeedPreLockHookGuard {
    fn drop(&mut self) {
        crate::runtime::set_seed_pre_lock_hook(None);
    }
}

/// A merge that rewrites home just before the seed takes the sync lock is the
/// home the seed copies: the seed reads home's bytes and rank under the lock,
/// so the seeded copy never carries a rank newer than the write its bytes hold.
#[test]
fn a_seed_waiting_on_a_merge_reads_home_after_it() {
    let home = HomeSandbox::new();
    let h = home.home().to_path_buf();
    let (global, r1, r2, r3dir) = seed_from_echo_home(&h);

    crate::runtime::set_seed_pre_lock_hook(Some(Box::new(|| {
        let paths = crate::claude_json::known_paths().expect("paths");
        crate::claude_json::merge_paths(&paths).expect("a merge right before the seed's lock");
    })));
    let guard = SeedPreLockHookGuard;
    seed_claude_json(&r3dir, &h.join(".claude")).expect("seed");
    drop(guard);
    crate::claude_json::reset_last_synced();
    crate::claude_json::sync_once().expect("next pass");

    let r3 = r3dir.join(".claude.json");
    assert_every_copy_keeps_the_siblings_write(&[
        ("home", &global),
        ("r1", &r1),
        ("r2", &r2),
        ("the seeded copy", &r3),
    ]);
}

/// A tree path spelled with a doubled separator names the same copy the sync
/// lists: the seed's record entry still matches it, so the next pass reads the
/// copy as the echo of home's carried rank, never as a file of its own.
#[test]
fn a_seed_into_a_differently_spelled_path_still_matches_its_record() {
    let home = HomeSandbox::new();
    let h = home.home().to_path_buf();
    let (global, r1, r2, r3dir) = seed_from_echo_home(&h);
    let sep = std::path::MAIN_SEPARATOR;
    let doubled = std::path::PathBuf::from(format!(
        "{}{sep}{sep}{}",
        r3dir.parent().expect("has parent").display(),
        r3dir.file_name().expect("has name").to_string_lossy()
    ));
    assert_eq!(doubled, r3dir, "the two spellings name one path");

    seed_claude_json(&doubled, &h.join(".claude")).expect("seed");
    crate::claude_json::reset_last_synced();
    crate::claude_json::sync_once().expect("next pass");

    let r3 = r3dir.join(".claude.json");
    assert_every_copy_keeps_the_siblings_write(&[
        ("home", &global),
        ("r1", &r1),
        ("r2", &r2),
        ("the seeded copy", &r3),
    ]);
}

/// A seed holding the sync lock records the copy it wrote: one entry naming the
/// seeded copy (a doubled-separator spelling of its tree included) at the copy's
/// own mtime and size, carrying the rank of the t10 Claude Code write home
/// echoes, so the next merge reads the copy as an echo of that write.
#[test]
fn a_seed_records_its_copy_at_homes_carried_rank() {
    let home = HomeSandbox::new();
    let h = home.home().to_path_buf();
    let (_global, _r1, _r2, r3dir) = seed_from_echo_home(&h);
    let sep = std::path::MAIN_SEPARATOR;
    let doubled = std::path::PathBuf::from(format!(
        "{}{sep}{sep}{}",
        r3dir.parent().expect("has parent").display(),
        r3dir.file_name().expect("has name").to_string_lossy()
    ));

    seed_claude_json(&doubled, &h.join(".claude")).expect("seed");

    let r3 = r3dir.join(".claude.json");
    let meta = fs::metadata(&r3).expect("the seeded copy");
    let since = meta
        .modified()
        .expect("mtime")
        .duration_since(SystemTime::UNIX_EPOCH)
        .expect("after the epoch");
    let record = crate::claude_json::written_record_path().expect("record path");
    let entries: Vec<serde_json::Value> =
        serde_json::from_slice(&fs::read(&record).expect("read the record"))
            .expect("parse the record");
    let named: Vec<&serde_json::Value> = entries
        .iter()
        .filter(|e| {
            e["path"]
                .as_str()
                .is_some_and(|p| std::path::Path::new(p) == r3)
        })
        .collect();
    assert_eq!(named.len(), 1, "one record entry names the seeded copy");
    let entry = named[0];
    assert_eq!(entry["size"], serde_json::json!(meta.len()), "its size");
    assert_eq!(
        entry["mtime_secs"],
        serde_json::json!(since.as_secs()),
        "its mtime"
    );
    assert_eq!(
        entry["mtime_nanos"],
        serde_json::json!(since.subsec_nanos()),
        "its mtime"
    );
    assert_eq!(
        entry["rank_secs"],
        serde_json::json!(1_700_000_010_i64),
        "the rank of the write home echoes"
    );
    assert_eq!(entry["rank_nanos"], serde_json::json!(0), "that rank");
}

/// Every seed stamps its copy at 1980-01-02T00:00:00Z, recorded or not: below
/// every Claude Code write, and inside the date range FAT and exFAT store.
#[test]
fn every_seed_stamps_its_copy_at_the_fat_floor() {
    // 3653 days from 1970-01-01 to 1980-01-02 (two leap days, 1972 and 1976).
    let floor = SystemTime::UNIX_EPOCH + Duration::from_secs(3_653 * 86_400);
    let home = HomeSandbox::new();
    let h = home.home().to_path_buf();
    let (_global, _r1, _r2, r3dir) = seed_from_echo_home(&h);

    seed_claude_json(&r3dir, &h.join(".claude")).expect("recorded seed");
    let recorded = fs::metadata(r3dir.join(".claude.json"))
        .expect("the recorded seed's copy")
        .modified()
        .expect("mtime");
    assert_eq!(recorded, floor, "the recorded seed's stamp");

    let r4dir = seed_test_runtime(&h, "p4", "runtime-4242-3");
    fs::create_dir_all(&r4dir).expect("mkdir");
    let lock = h.join(".clauth").join(".claude-json-sync.lock");
    let _ = fs::remove_file(&lock);
    fs::create_dir_all(&lock).expect("a directory where the lock file goes");
    seed_claude_json(&r4dir, &h.join(".claude")).expect("unrecorded seed");
    fs::remove_dir(&lock).expect("clear the lock stand-in");
    let unrecorded = fs::metadata(r4dir.join(".claude.json"))
        .expect("the unrecorded seed's copy")
        .modified()
        .expect("mtime");
    assert_eq!(unrecorded, floor, "the unrecorded seed's stamp");
}

/// Holds the [`crate::testutil::UnwritableDir`] the seed's pre-lock seam arms,
/// so the seam (a `Fn`, not `FnOnce`) can arm it and the test can restore it
/// after the acquire fails.
static SEED_FAIL_GUARD: std::sync::Mutex<Option<crate::testutil::UnwritableDir>> =
    std::sync::Mutex::new(None);

/// An unrecorded seed (the sync lock could not be opened) writes only home's
/// bytes and stamps its copy at the floor, so a sibling's newer unpropagated
/// write wins the next pass instead of being overwritten by the seed.
#[test]
fn an_unrecorded_seed_never_outranks_a_siblings_newer_write() {
    let home = HomeSandbox::new();
    let h = home.home().to_path_buf();
    let global = h.join(".claude.json");
    let r1 = seed_test_runtime(&h, "p1", "runtime-4242-0").join(".claude.json");
    let r2dir = seed_test_runtime(&h, "p2", "runtime-4242-1");
    fs::create_dir_all(r1.parent().expect("has parent")).expect("mkdir");
    fs::create_dir_all(&r2dir).expect("mkdir");
    fs::create_dir_all(h.join(".claude")).expect("mkdir");
    let write = |p: &std::path::Path, v: serde_json::Value| {
        fs::write(p, serde_json::to_vec_pretty(&v).expect("serialize")).expect("write");
    };
    write(
        &global,
        serde_json::json!({"numStartups": 2, "oauthAccount": {"emailAddress": "g@x"}}),
    );
    write(
        &r1,
        serde_json::json!({"numStartups": 3, "ccR1": true, "oauthAccount": {"emailAddress": "r1@x"}}),
    );
    let t = |s: u64| SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000 + s);
    set_mtime(&global, t(10));
    set_mtime(&r1, t(20));
    // A directory where the sync-lock file goes makes the seed's lock open fail,
    // so the seed runs unrecorded — the path the unrecorded-seed mtime covers.
    let lock = h.join(".clauth").join(".claude-json-sync.lock");
    fs::create_dir_all(&lock).expect("a directory where the lock file goes");

    seed_claude_json(&r2dir, &h.join(".claude")).expect("seed");
    fs::remove_dir(&lock).expect("clear the lock stand-in");

    crate::claude_json::reset_last_synced();
    crate::claude_json::sync_once().expect("next pass");

    let read = |p: &std::path::Path| {
        serde_json::from_slice::<serde_json::Value>(&fs::read(p).expect("read")).expect("parse")
    };
    assert_eq!(
        read(&r1),
        serde_json::json!({"numStartups": 3, "ccR1": true, "oauthAccount": {"emailAddress": "r1@x"}}),
        "the sibling keeps its Claude Code write"
    );
    for (name, path) in [
        ("home", &global),
        ("the seeded copy", &r2dir.join(".claude.json")),
    ] {
        let doc = read(path);
        assert_eq!(
            doc["numStartups"],
            serde_json::json!(3),
            "{name} converges on the sibling's write"
        );
        assert_eq!(
            doc["ccR1"],
            serde_json::json!(true),
            "{name} converges on the sibling's write"
        );
    }
}

/// A failed seed (the runtime dir refuses the seed's write) fails the start and
/// leaves no live row and no marker: the start's own registration is released,
/// not left half-done. Forced Fake transport so the runtime path is the fixed
/// bare stem the seed's pre-lock seam can make unwritable.
#[test]
fn a_failed_seed_fails_the_start_without_a_live_row_or_marker() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        with_link_mode(LinkMode::Fake, || {
            fake_claude_home(tmp.path());
            fs::write(tmp.path().join(".claude.json"), b"{\"numStartups\": 2}")
                .expect("write global .claude.json");
            let profile = configured_profile("seedfail");
            let runtime = tmp
                .path()
                .join(".clauth")
                .join("profiles")
                .join("seedfail")
                .join("runtime");

            *SEED_FAIL_GUARD
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
            crate::runtime::set_seed_pre_lock_hook(Some(Box::new(move || {
                *SEED_FAIL_GUARD
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) =
                    Some(crate::testutil::UnwritableDir::new(&runtime));
            })));
            let _hook_guard = SeedPreLockHookGuard;

            let err =
                ProfileRuntime::acquire(&profile, Isolation::Shared, &[], false, Launcher::Own)
                    .err()
                    .expect("a failed seed must fail the start");
            *SEED_FAIL_GUARD
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
            drop(_hook_guard);

            assert!(
                format!("{err:#}").contains("failed to seed"),
                "the start names the seed failure, got: {err:#}"
            );
            assert!(
                crate::live_sessions::list().is_empty(),
                "a failed seed leaves no live row"
            );
            let profile_root = tmp.path().join(".clauth").join("profiles").join("seedfail");
            let markers: usize = std::fs::read_dir(&profile_root)
                .expect("read profile root")
                .filter_map(|e| e.ok())
                .filter(|e| {
                    e.file_name()
                        .to_str()
                        .is_some_and(|n| n == "sessions" || n.starts_with("sessions-"))
                })
                .map(|e| std::fs::read_dir(e.path()).map(|d| d.count()).unwrap_or(0))
                .sum();
            assert_eq!(markers, 0, "a failed seed leaves no marker");
        });
    });
}

/// A seed whose back-date stamp the filesystem refuses (the seam forces it)
/// fails the start exactly as any failed seed: no live row, no marker, and no
/// staged temp beside the copy. A refused stamp is a failed write, never a
/// clamp to a fresh mtime that would outrank a sibling's newer Claude Code
/// write.
#[test]
fn a_seed_whose_stamp_fails_fails_the_start_without_a_live_row_or_marker() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_fake_home(tmp.path(), || {
        with_link_mode(LinkMode::Fake, || {
            fake_claude_home(tmp.path());
            fs::write(tmp.path().join(".claude.json"), b"{\"numStartups\": 2}")
                .expect("write global .claude.json");
            let profile = configured_profile("seedstampfail");

            crate::jsonsync::set_stamp_error_hook(true);
            let _hook_guard = crate::testutil::StampErrorHookGuard;

            let err =
                ProfileRuntime::acquire(&profile, Isolation::Shared, &[], false, Launcher::Own)
                    .err()
                    .expect("a seed whose stamp fails must fail the start");
            drop(_hook_guard);

            let profile_root = tmp
                .path()
                .join(".clauth")
                .join("profiles")
                .join("seedstampfail");
            assert_eq!(
                format!("{err:#}"),
                format!(
                    "failed to seed {}: test-forced stamp failure",
                    profile_root.join("runtime").join(".claude.json").display()
                ),
                "the start names only the failed seed"
            );
            assert!(
                crate::live_sessions::list().is_empty(),
                "a failed seed leaves no live row"
            );
            let markers: usize = std::fs::read_dir(&profile_root)
                .expect("read profile root")
                .filter_map(|e| e.ok())
                .filter(|e| {
                    e.file_name()
                        .to_str()
                        .is_some_and(|n| n == "sessions" || n.starts_with("sessions-"))
                })
                .map(|e| std::fs::read_dir(e.path()).map(|d| d.count()).unwrap_or(0))
                .sum();
            assert_eq!(markers, 0, "a failed seed leaves no marker");
            let runtime_dir = profile_root.join("runtime");
            let temps = std::fs::read_dir(&runtime_dir)
                .map(|d| {
                    d.filter_map(|e| e.ok())
                        .filter(|e| {
                            e.file_name()
                                .to_str()
                                .is_some_and(|n| n.contains(".claude.json.tmp."))
                        })
                        .count()
                })
                .unwrap_or(0);
            assert_eq!(temps, 0, "a failed seed stamp leaves no staged temp");
        });
    });
}

/// An unrecorded seed that lands while a merge has rewritten home but not yet
/// persisted its record never outranks a sibling's newer Claude Code write:
/// the seeded copy only holds home's bytes, so it never needs to win a pass.
#[test]
fn an_unrecorded_seed_during_a_merge_never_outranks_a_siblings_write() {
    let home = HomeSandbox::new();
    let h = home.home().to_path_buf();
    let global = h.join(".claude.json");
    let r1 = seed_test_runtime(&h, "p1", "runtime-4242-0").join(".claude.json");
    let r2 = seed_test_runtime(&h, "p2", "runtime-4242-1").join(".claude.json");
    let r4 = seed_test_runtime(&h, "p4", "runtime-4242-3").join(".claude.json");
    let r3dir = seed_test_runtime(&h, "p3", "runtime-4242-2");
    for p in [&r1, &r2, &r4] {
        fs::create_dir_all(p.parent().expect("has parent")).expect("mkdir");
    }
    fs::create_dir_all(&r3dir).expect("mkdir");
    fs::create_dir_all(h.join(".claude")).expect("mkdir");
    let write = |p: &std::path::Path, v: serde_json::Value| {
        fs::write(p, serde_json::to_vec_pretty(&v).expect("serialize")).expect("write");
    };
    let t = |s: u64| SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000 + s);
    write(&global, serde_json::json!({"numStartups": 1}));
    write(&r1, serde_json::json!({"numStartups": 2}));
    write(&r2, serde_json::json!({"numStartups": 3, "ccR2": true}));
    write(&r4, serde_json::json!({"numStartups": 1}));
    set_mtime(&global, t(5));
    set_mtime(&r1, t(10));
    set_mtime(&r2, t(20));
    set_mtime(&r4, t(3));
    crate::claude_json::reset_last_synced();
    crate::claude_json::sync_once().expect("a merge makes home an echo of r2");

    write(
        &r4,
        serde_json::json!({"numStartups": 9, "ccR2": true, "ccR4": true}),
    );
    set_mtime(&r4, t(30));

    // Mid-merge: home already rewritten, the record not yet persisted, and the
    // seed's lock unavailable, so it seeds unrecorded.
    let record = crate::claude_json::written_record_path().expect("record path");
    let parked = h.join("record-parked");
    fs::rename(&record, &parked).expect("park the record");
    let lock = h.join(".clauth").join(".claude-json-sync.lock");
    let _ = fs::remove_file(&lock);
    fs::create_dir_all(&lock).expect("a directory where the lock file goes");
    seed_claude_json(&r3dir, &h.join(".claude")).expect("seed");
    fs::remove_dir(&lock).expect("clear the lock stand-in");
    fs::rename(&parked, &record).expect("the merge persists its record");

    crate::claude_json::reset_last_synced();
    crate::claude_json::sync_once().expect("next pass");

    let r3 = r3dir.join(".claude.json");
    for (name, path) in [
        ("home", &global),
        ("r1", &r1),
        ("r2", &r2),
        ("r4", &r4),
        ("the seeded copy", &r3),
    ] {
        let doc = seed_test_doc(path);
        assert_eq!(
            doc["ccR4"],
            serde_json::json!(true),
            "{name} keeps the sibling's Claude Code write"
        );
        assert_eq!(
            doc["numStartups"],
            serde_json::json!(9),
            "{name} keeps the sibling's Claude Code write"
        );
    }
}

/// A seed that takes the lock but cannot persist its record never outranks a
/// sibling's newer Claude Code write: the seeded copy holds only home's bytes,
/// so it never needs to win a pass, recorded or not.
#[test]
fn a_seed_whose_record_write_fails_never_outranks_a_siblings_write() {
    let home = HomeSandbox::new();
    let h = home.home().to_path_buf();
    let global = h.join(".claude.json");
    let r1 = seed_test_runtime(&h, "p1", "runtime-4242-0").join(".claude.json");
    let r2 = seed_test_runtime(&h, "p2", "runtime-4242-1").join(".claude.json");
    let r3dir = seed_test_runtime(&h, "p3", "runtime-4242-2");
    for p in [&r1, &r2] {
        fs::create_dir_all(p.parent().expect("has parent")).expect("mkdir");
    }
    fs::create_dir_all(&r3dir).expect("mkdir");
    fs::create_dir_all(h.join(".claude")).expect("mkdir");
    let write = |p: &std::path::Path, v: serde_json::Value| {
        fs::write(p, serde_json::to_vec_pretty(&v).expect("serialize")).expect("write");
    };
    let t = |s: u64| SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000 + s);
    write(&global, serde_json::json!({"numStartups": 1}));
    write(&r1, serde_json::json!({"numStartups": 2}));
    set_mtime(&global, t(5));
    set_mtime(&r1, t(10));
    crate::claude_json::reset_last_synced();
    crate::claude_json::sync_once().expect("a pass creates the lock and the record");
    write(&r2, serde_json::json!({"numStartups": 3, "ccR2": true}));
    set_mtime(&r2, t(20));

    {
        // The lock file exists, so the seed still takes the lock; the record's
        // temp file cannot be created, so its record write fails.
        let _clauth = crate::testutil::UnwritableDir::new(&h.join(".clauth"));
        seed_claude_json(&r3dir, &h.join(".claude")).expect("seed");
    }

    crate::claude_json::reset_last_synced();
    crate::claude_json::sync_once().expect("next pass");

    let r3 = r3dir.join(".claude.json");
    assert_every_copy_keeps_the_siblings_write(&[
        ("home", &global),
        ("r1", &r1),
        ("r2", &r2),
        ("the seeded copy", &r3),
    ]);
}

/// An adoption re-reads the member inside the state flock and refuses when it
/// moved off the account the rotation guard was taken for, rather than holding
/// the session under the wrong member's lock.
#[test]
fn an_adoption_bails_when_the_member_moves_while_the_rotation_lock_waits() {
    let sb = HomeSandbox::new();
    std::fs::create_dir_all(sb.home().join(".claude")).expect("~/.claude");
    let sid = "4242-0";
    let mut row = crate::testutil::live_row(sid, "wfl");
    row.current_member = Some("wfl".to_string());
    crate::live_sessions::register(&row).expect("register the handed-off row");
    crate::runtime::set_adopt_reread_hook(Some(Box::new(move || {
        crate::live_sessions::update_as_session(sid, |fields| fields.set_current_member("wfs"))
            .expect("flip the member");
    })));

    let refused = crate::runtime::ProfileRuntime::acquire_adopted(sid, &[]);
    crate::runtime::set_adopt_reread_hook(None);

    let err = refused
        .err()
        .expect("the member moved during the wait, so the adoption must refuse");
    assert_eq!(
        err.to_string(),
        "the session moved accounts while the adoption waited; retrying"
    );
}

/// A GC pass re-reads the job store before reaping a dead chain row, so a job
/// record landing after the pass-start snapshot keeps the hand-off record.
#[test]
fn a_gc_pass_spares_a_chain_row_whose_job_lands_after_its_snapshot() {
    let sb = HomeSandbox::new();
    let home = sb.home().to_path_buf();
    let wfl = crate::profile::ProfileName::from("wfl");
    let profile = crate::profile::profile_dir(&wfl).expect("profile dir");
    let sid = "4242-0";
    let tree = profile.join(format!("runtime-{sid}"));
    std::fs::create_dir_all(&tree).expect("tree");
    std::fs::create_dir_all(profile.join(format!("sessions-{sid}"))).expect("sessions dir");
    crate::live_sessions::register(&crate::testutil::live_row(sid, "wfl")).expect("register");
    let tree_for_job = tree.clone();
    crate::runtime::set_gc_rows_reread_hook(Some(Box::new(move || {
        crate::testutil::write_claude_job_in(&home, "b6a1c0d6", &tree_for_job, "working", None);
    })));

    crate::runtime::gc_live_session_rows();
    crate::runtime::set_gc_rows_reread_hook(None);

    assert!(
        crate::live_sessions::get(sid).is_some(),
        "the job re-read spares the handed-off row"
    );
}

// ── copied rolling bearers: who gets re-pointed ─────────────────────────────

/// Adoption reaches a session's own per-session tree and, for a background
/// row, the account's shared `runtime-bg` tree; never another session's tree,
/// a malformed id or a directory clauth did not build.
#[test]
fn session_item_adoption_is_limited_to_a_live_sessions_own_tree() {
    let root = Path::new("/h/.clauth/profiles/acct");
    assert!(session_item_may_adopt(
        &root.join("runtime-34181-0"),
        "34181-0",
        false
    ));
    assert!(session_item_may_adopt(
        &root.join("runtime-isolated-34181-0"),
        "34181-0",
        false
    ));
    assert!(!session_item_may_adopt(
        &root.join("runtime-34181-1"),
        "34181-0",
        false
    ));
    assert!(session_item_may_adopt(
        &root.join("runtime-bg"),
        "34181-0",
        true
    ));
    assert!(!session_item_may_adopt(
        &root.join("runtime-bg"),
        "34181-0",
        false
    ));
    assert!(!session_item_may_adopt(
        &root.join("runtime-34181-0"),
        "34181-0",
        true
    ));
    assert!(!session_item_may_adopt(
        Path::new("/work/my-claude"),
        "34181-0",
        false
    ));
    assert!(!session_item_may_adopt(
        &root.join("runtime"),
        "34181-0",
        false
    ));
    assert!(!session_item_may_adopt(&root.join("runtime-bg"), "x", true));
}

/// Only sessions on the stamped member are candidates.
#[test]
fn a_stamp_concerns_only_its_members_sessions() {
    let live = |p: &str, sid: &str| LiveRuntime {
        runtime: PathBuf::from(format!("/h/{p}/runtime-{sid}")),
        profile: crate::profile::ProfileName::from(p),
        session: sid.to_string(),
        background: false,
    };
    let all = vec![live("a", "1-0"), live("b", "2-0"), live("a", "3-0")];
    let on_a = sessions_on_member(all, &crate::profile::ProfileName::from("a"));
    assert_eq!(
        on_a.iter().map(|l| l.session.as_str()).collect::<Vec<_>>(),
        vec!["1-0", "3-0"]
    );
}

/// An item no row names is adopted with the background flag the row carries:
/// a background row's `runtime-bg` is accepted, a per-session id on it is not.
#[test]
fn a_background_sessions_item_is_adopted_on_its_own_tree() {
    let home = HomeSandbox::new();
    let tree = home.home().join(".clauth/profiles/acct/runtime-bg");
    fs::create_dir_all(&tree).expect("tree");
    let profile = crate::profile::ProfileName::from("acct");
    assert!(
        namespaced_keychain_ledger::authorize_live_write(&tree, &profile, "704-1", false).is_err()
    );
    assert!(
        namespaced_keychain_ledger::load()
            .expect("ledger")
            .owners
            .is_empty()
    );
    let owned = namespaced_keychain_ledger::authorize_live_write(&tree, &profile, "704-1", true)
        .expect("adopted");
    assert_eq!(
        namespaced_keychain_ledger::load().expect("ledger").owners,
        vec![namespaced_keychain_ledger::Owner {
            service: owned.service().to_string(),
            profile,
            session: "704-1".to_string(),
        }]
    );
    namespaced_keychain_ledger::authorize_owned_write(&tree).expect("owned now");
}

/// An item the ledger does not name refuses the owned witness.
#[test]
fn the_owned_witness_needs_a_row() {
    let home = HomeSandbox::new();
    let runtime = home.home().join(".clauth/profiles/acct/runtime-705-1");
    fs::create_dir_all(&runtime).expect("runtime");
    assert!(namespaced_keychain_ledger::authorize_owned_write(&runtime).is_err());
}

/// The live-session walk keeps live Claude rows whose tree exists, records
/// the member each is on, reaches a background row's `runtime-bg`, and skips
/// codex rows, dead rows, rows without a tree and duplicates of one tree.
#[test]
fn live_session_runtime_dirs_lists_live_claude_trees_only() {
    let _home = HomeSandbox::new();
    let acct = crate::profile::ProfileName::from("acct");
    let tree = |sid: &str| {
        let [real, _] = session_runtime_trees(&acct, false, sid).expect("trees");
        real
    };

    // Live, swapped onto another member, with a tree.
    let mut swapped = crate::testutil::live_row("801-0", "acct");
    swapped.current_member = Some("other".to_string());
    crate::live_sessions::register(&swapped).expect("register");
    let _swapped_marker =
        hold_session_row_marker(&crate::profile::ProfileName::from("other"), false, "801-0")
            .expect("marker");
    fs::create_dir_all(tree("801-0")).expect("tree");

    // Live but its tree was never built.
    crate::live_sessions::register(&crate::testutil::live_row("802-0", "acct")).expect("register");
    let _treeless = hold_session_row_marker(&acct, false, "802-0").expect("marker");

    // A dead row with a tree.
    crate::live_sessions::register(&crate::testutil::live_row("803-0", "acct")).expect("register");
    fs::create_dir_all(tree("803-0")).expect("tree");

    // A codex row.
    let mut codex = crate::testutil::live_row("804-0", "acct");
    codex.harness = crate::harness::Harness::Codex;
    crate::live_sessions::register(&codex).expect("register");
    let _codex_marker = hold_session_row_marker(&acct, false, "804-0").expect("marker");
    fs::create_dir_all(tree("804-0")).expect("tree");

    // A background row on the account's shared tree.
    let mut bg = crate::testutil::live_row("805-0", "acct");
    bg.background = true;
    crate::live_sessions::register(&bg).expect("register");
    let bg_tree = background_runtime_tree(&acct).expect("bg tree");
    fs::create_dir_all(&bg_tree).expect("bg tree");
    let _bg_marker = hold_background_row_marker(&acct, "805-0").expect("bg marker");

    let mut live = live_session_runtime_dirs();
    live.sort_by(|a, b| a.session.cmp(&b.session));
    let sessions: Vec<&str> = live.iter().map(|l| l.session.as_str()).collect();
    assert!(sessions.contains(&"801-0"), "{live:?}");
    assert!(!sessions.contains(&"802-0"), "no tree: {live:?}");
    assert!(!sessions.contains(&"803-0"), "dead: {live:?}");
    assert!(!sessions.contains(&"804-0"), "codex: {live:?}");
    let swapped = live.iter().find(|l| l.session == "801-0").expect("swapped");
    assert_eq!(
        swapped.profile.as_str(),
        "other",
        "the member, not the start profile"
    );
    assert!(!swapped.background);
    let bg = live
        .iter()
        .find(|l| l.session == "805-0")
        .expect("background row");
    assert!(bg.background);
    assert_eq!(bg.runtime, bg_tree);
}

/// The queue keeps one entry per revoked bearer and member, and a drain that
/// cannot run (no Keychain under test, or off macOS) leaves it as it was; the
/// daemon's detached drain is a no-op under test.
#[test]
fn the_revoked_bearer_queue_dedupes_and_survives_a_drain_that_cannot_run() {
    let _ = take_revoked_bearers();
    let a = crate::profile::ProfileName::from("a");
    let b = crate::profile::ProfileName::from("b");
    note_revoked_rolling_bearer(&a, "old".to_string());
    note_revoked_rolling_bearer(&a, "old".to_string());
    note_revoked_rolling_bearer(&b, "old".to_string());
    note_revoked_rolling_bearer(&a, "older".to_string());
    repoint_copied_bearers();
    repoint_copied_bearers_detached();
    assert_eq!(
        take_revoked_bearers(),
        vec![
            (a.clone(), "old".to_string()),
            (b, "old".to_string()),
            (a, "older".to_string()),
        ]
    );
    assert!(take_revoked_bearers().is_empty(), "taking empties it");
}

fn drain_live(profile: &str, session: &str) -> LiveRuntime {
    LiveRuntime {
        runtime: PathBuf::from(format!("/nonexistent/runtime-{session}")),
        profile: crate::profile::ProfileName::from(profile),
        session: session.to_string(),
        background: false,
    }
}

fn queued(pairs: &[(&str, &str)]) -> Vec<(crate::profile::ProfileName, String)> {
    pairs
        .iter()
        .map(|(m, r)| (crate::profile::ProfileName::from(*m), (*r).to_string()))
        .collect()
}

/// A drain settles a pair only once every one of its member's sessions is
/// settled: an item written or one no longer holding the bearer settles its
/// session, while a leg that failed (a locked Keychain or any other error)
/// or found the sidecar not ready yet keeps the pair queued. A pair's other
/// sessions still run after one fails, and each leg sees the pair's `logged`
/// flag unset.
#[test]
fn a_drain_requeues_a_pair_until_every_session_settles() {
    let _ = take_revoked_bearers();
    let sessions = vec![
        drain_live("written", "1-0"),
        drain_live("moved", "2-0"),
        drain_live("notyet", "3-0"),
        drain_live("locked", "4-0"),
        drain_live("multi", "5-0"),
        drain_live("multi", "6-0"),
    ];
    for member in ["written", "moved", "notyet", "locked", "multi"] {
        note_revoked_rolling_bearer(
            &crate::profile::ProfileName::from(member),
            format!("old-{member}"),
        );
    }
    let mut legs = Vec::new();
    let now = Instant::now();
    drain_revoked_bearers_with(
        &sessions,
        now,
        now + Duration::from_secs(60),
        |member, revoked, session, logged: &mut bool| {
            legs.push(format!("{member}/{revoked}/{}/{logged}", session.session));
            match (member.as_str(), session.session.as_str()) {
                ("written", _) | ("multi", "6-0") => Ok(ItemRepoint::Written),
                ("moved", _) => Ok(ItemRepoint::NotHeld),
                ("notyet", _) => Ok(ItemRepoint::NotYet),
                _ => Err(anyhow::anyhow!("keychain locked")),
            }
        },
    );
    assert_eq!(
        legs,
        vec![
            "written/old-written/1-0/false",
            "moved/old-moved/2-0/false",
            "notyet/old-notyet/3-0/false",
            "locked/old-locked/4-0/false",
            "multi/old-multi/5-0/false",
            "multi/old-multi/6-0/false",
        ],
        "every pair reaches each of its member's sessions, the failed pair's second one included"
    );
    assert_eq!(
        take_revoked_bearers(),
        queued(&[
            ("notyet", "old-notyet"),
            ("locked", "old-locked"),
            ("multi", "old-multi"),
        ]),
        "a failed or not-yet leg keeps its pair queued; a written or no-longer-held one settles it"
    );
}

/// The macOS item leg writes the blob the mutating rule changed: it must call
/// `repoint_copied_bearer` (which swaps the bearer in), never only the
/// predicate, before its `put_blob_at`. Pinned by source because the leg runs
/// against a real Keychain alone.
#[test]
fn the_item_leg_writes_the_blob_the_rule_rewrote() {
    let code = code_tokens_only(include_str!("../../src/keychain.rs"));
    let start = code
        .find("fn repoint_config_dir_item_bearer(")
        .expect("the item leg exists");
    let body = &code[start..];
    let body = &body[..body.find("\n}\n").expect("the item leg's closing brace")];
    let rewrite = body
        .find("crate::claude::repoint_copied_bearer(&mut blob,")
        .expect("the leg rewrites the blob through the mutating rule");
    let write = body
        .find("put_blob_at(&service, &account, &blob)")
        .expect("the leg writes that same blob");
    assert!(
        rewrite < write,
        "the blob is rewritten before it is written"
    );
}

/// What a test leg answers on one drain attempt.
#[derive(Clone, Copy)]
enum Answer {
    Locked,
    Failed,
    NotYet,
    Written,
}

/// A re-queued pair waits before its next attempt: 1 s after the first
/// failure or not-yet, then doubling (the swap item retry's backoff). A
/// failure is logged once per pair: the flag the leg sets when it logs
/// survives the re-queue, and a quiet locked-Keychain attempt leaves it unset.
#[test]
fn a_requeued_pair_backs_off_and_logs_a_failure_once() {
    let _ = take_revoked_bearers();
    let sessions = vec![drain_live("acct", "1-0")];
    note_revoked_rolling_bearer(
        &crate::profile::ProfileName::from("acct"),
        "old".to_string(),
    );
    let t0 = Instant::now();
    let far = t0 + Duration::from_secs(600);
    let mut attempts = Vec::new();
    let drain_at = |at: Duration, attempts: &mut Vec<(Duration, bool)>, answer: Answer| {
        drain_revoked_bearers_with(&sessions, t0 + at, far, |_, _, _, logged: &mut bool| {
            attempts.push((at, *logged));
            match answer {
                Answer::Locked => Err(anyhow::anyhow!("keychain locked")),
                Answer::Failed => {
                    *logged = true;
                    Err(anyhow::anyhow!("write failed"))
                }
                Answer::NotYet => Ok(ItemRepoint::NotYet),
                Answer::Written => Ok(ItemRepoint::Written),
            }
        });
    };
    drain_at(Duration::ZERO, &mut attempts, Answer::Locked);
    drain_at(Duration::from_millis(500), &mut attempts, Answer::Written);
    drain_at(Duration::from_secs(1), &mut attempts, Answer::Failed);
    drain_at(Duration::from_millis(2500), &mut attempts, Answer::Written);
    drain_at(Duration::from_secs(3), &mut attempts, Answer::NotYet);
    drain_at(Duration::from_secs(6), &mut attempts, Answer::Written);
    drain_at(Duration::from_secs(7), &mut attempts, Answer::Written);
    assert_eq!(
        attempts,
        vec![
            (Duration::ZERO, false),
            (Duration::from_secs(1), false),
            (Duration::from_secs(3), true),
            (Duration::from_secs(7), true),
        ],
        "attempts run at 0 s, +1 s, +2 s, then +4 s (a not-yet backs off like a failure); \
         a quiet locked attempt leaves the next failure to log, and a logged one stays logged"
    );
    assert!(
        take_revoked_bearers().is_empty(),
        "the written item settles the pair"
    );
}

/// A pair the drain's deadline cut off stays queued and stays due: it did not
/// fail, so it takes no backoff.
#[test]
fn a_pair_the_deadline_cut_off_stays_due() {
    let _ = take_revoked_bearers();
    let sessions = vec![drain_live("acct", "1-0")];
    note_revoked_rolling_bearer(
        &crate::profile::ProfileName::from("acct"),
        "old".to_string(),
    );
    let now = Instant::now();
    let mut ran = 0;
    drain_revoked_bearers_with(&sessions, now, Instant::now(), |_, _, _, _| {
        ran += 1;
        Ok(ItemRepoint::Written)
    });
    assert_eq!(ran, 0, "a spent deadline runs no leg");
    drain_revoked_bearers_with(
        &sessions,
        now,
        now + Duration::from_secs(60),
        |_, _, _, _| {
            ran += 1;
            Ok(ItemRepoint::Written)
        },
    );
    assert_eq!(ran, 1, "the cut-off pair is due at once on the next drain");
    assert!(take_revoked_bearers().is_empty());
}

/// A pair whose first session failed and whose second the deadline then cut
/// off is a failed pair: it backs off rather than staying due.
#[test]
fn a_failed_pair_the_deadline_also_cut_off_backs_off() {
    let _ = take_revoked_bearers();
    let sessions = vec![drain_live("acct", "1-0"), drain_live("acct", "2-0")];
    note_revoked_rolling_bearer(
        &crate::profile::ProfileName::from("acct"),
        "old".to_string(),
    );
    let now = Instant::now();
    let mut legs = Vec::new();
    drain_revoked_bearers_with(
        &sessions,
        now,
        Instant::now() + Duration::from_millis(500),
        |_, _, session, _| {
            legs.push(session.session.clone());
            std::thread::sleep(Duration::from_millis(600));
            Err(anyhow::anyhow!("write failed"))
        },
    );
    assert_eq!(
        legs,
        vec!["1-0"],
        "the deadline passed during the first leg"
    );
    let mut ran = 0;
    drain_revoked_bearers_with(
        &sessions,
        now,
        now + Duration::from_secs(60),
        |_, _, _, _| {
            ran += 1;
            Ok(ItemRepoint::Written)
        },
    );
    assert_eq!(ran, 0, "the failed pair is not due again at once");
    drain_revoked_bearers_with(
        &sessions,
        now + Duration::from_secs(1),
        now + Duration::from_secs(60),
        |_, _, _, _| {
            ran += 1;
            Ok(ItemRepoint::Written)
        },
    );
    assert_eq!(ran, 2, "it is due after its 1 s backoff, for both sessions");
    assert!(take_revoked_bearers().is_empty());
}

/// The adopting witness refuses a tree the session does not own, records
/// nothing then, and refuses while a sweep's delete of the item is in flight.
#[test]
fn the_adopting_witness_refuses_foreign_trees_and_in_flight_deletes() {
    let home = HomeSandbox::new();
    let profile = crate::profile::ProfileName::from("acct");
    let foreign = home.home().join("my-claude");
    fs::create_dir_all(&foreign).expect("foreign");
    assert!(
        namespaced_keychain_ledger::authorize_live_write(&foreign, &profile, "806-0", false)
            .is_err()
    );
    let runtime = home.home().join(".clauth/profiles/acct/runtime-806-0");
    fs::create_dir_all(&runtime).expect("runtime");
    assert!(
        namespaced_keychain_ledger::authorize_live_write(&runtime, &profile, "806-1", false)
            .is_err(),
        "another session's id"
    );
    assert!(
        namespaced_keychain_ledger::load()
            .expect("ledger")
            .owners
            .is_empty()
    );
    let service =
        crate::claude::namespaced_keychain_service(&runtime.canonicalize().expect("canonical"));
    let _in_flight =
        with_state_lock(|_held| namespaced_keychain_ledger::record_in_flight_locked(&service))
            .expect("stamp");
    assert!(
        namespaced_keychain_ledger::authorize_live_write(&runtime, &profile, "806-0", false)
            .is_err()
    );
    assert!(namespaced_keychain_ledger::authorize_owned_write(&runtime).is_err());
}
