use super::*;

fn codex_record(sent_at_ms: u64) -> CodexPending {
    CodexPending {
        credit_id: "c".to_string(),
        redeem_request_id: "r".to_string(),
        sent_at_ms,
    }
}

/// A fresh record is kept; a stale one and one that does not parse are removed
/// so a fresh spend can run, and a future stamp still reads fresh.
#[test]
fn take_fresh_keeps_fresh_and_removes_stale_or_unparseable() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("record.json");

    write(&path, &codex_record(crate::usage::now_ms() - 10_000)).expect("write fresh");
    let fresh = take_fresh::<CodexPending>(&path)
        .expect("load fresh")
        .expect("a fresh record");
    assert_eq!(fresh.credit_id, "c");
    assert!(path.exists(), "a fresh record stays on disk for the re-run");

    write(&path, &codex_record(crate::usage::now_ms() + 60_000)).expect("write future");
    assert!(
        take_fresh::<CodexPending>(&path)
            .expect("load future")
            .is_some(),
        "a stamp in the future is fresh"
    );

    write(&path, &codex_record(crate::usage::now_ms() - 600_000)).expect("write stale");
    assert!(
        take_fresh::<CodexPending>(&path)
            .expect("load stale")
            .is_none()
    );
    assert!(!path.exists(), "a stale record is removed");

    std::fs::write(&path, "not json").expect("seed garbage");
    assert!(
        take_fresh::<CodexPending>(&path)
            .expect("load garbage")
            .is_none()
    );
    assert!(!path.exists(), "an unparseable record is removed");
}

/// A record that cannot be read at all refuses rather than guessing whether a
/// spend is still pending.
#[test]
fn take_fresh_refuses_a_record_it_cannot_read() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("record.json");
    std::fs::create_dir(&path).expect("a directory at the record path");
    let err = take_fresh::<CodexPending>(&path).expect_err("a directory is not a record");
    assert!(err.to_string().contains("could not read"), "{err}");
}

/// Removing a path that is not a file is an error naming the file, never
/// treated as already gone.
#[test]
fn clear_errors_on_a_non_file() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("record.json");
    std::fs::create_dir(&path).expect("a directory at the record path");
    let err = clear(&path).expect_err("a directory is not removed as a file");
    assert!(err.to_string().contains("could not remove"), "{err}");
}

/// `fresh_stamp` is read-only: a stale record is reported absent, never
/// removed, so `--list` does not mutate the store.
#[test]
fn fresh_stamp_reads_without_removing() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("record.json");
    write(&path, &codex_record(crate::usage::now_ms() - 600_000)).expect("write stale");
    assert_eq!(fresh_stamp::<CodexPending>(&path), None);
    assert!(path.exists(), "--list removes nothing");
}

/// The freshness boundary is the record's distance from now on either side:
/// exactly 600 s old or ahead is stale, one millisecond inside is fresh. A
/// fixed `now` keeps the boundary from moving between the seed and the check.
#[test]
fn fresh_reads_the_boundary_inclusively_on_either_side() {
    let now = 1_000_000u64;
    assert!(
        fresh(now, now - 599_999),
        "one ms inside the past edge is fresh"
    );
    assert!(!fresh(now, now - 600_000), "600 s old is stale");
    assert!(
        fresh(now, now - 300_000),
        "a record inside the window stays fresh when the window shrinks"
    );
    assert!(
        fresh(now, now + 599_999),
        "one ms inside the future edge is fresh"
    );
    assert!(!fresh(now, now + 600_000), "600 s ahead is stale");
    assert!(!fresh(now, now + 600_001), "past the future edge is stale");
}

/// A record whose ids are off the arm's sendable shape was not written by
/// clauth: it reads as unreadable and is removed so the fresh spend runs.
#[test]
fn take_fresh_removes_a_record_with_unsendable_ids() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("record.json");

    let bad_grant = ClaudePending {
        grant_id: "g.id".to_string(),
        request_id: "ok-id".to_string(),
        sent_at_ms: crate::usage::now_ms() - 10_000,
    };
    write(&path, &bad_grant).expect("write claude grant");
    assert!(
        take_fresh::<ClaudePending>(&path).expect("load").is_none(),
        "an unsendable grant id reads unreadable"
    );
    assert!(!path.exists(), "an unreadable record is removed");

    let bad_request = ClaudePending {
        grant_id: "g".to_string(),
        request_id: "not a key!".to_string(),
        sent_at_ms: crate::usage::now_ms() - 10_000,
    };
    write(&path, &bad_request).expect("write claude request");
    assert!(
        take_fresh::<ClaudePending>(&path).expect("load").is_none(),
        "an off-pattern request id reads unreadable"
    );
    assert!(!path.exists(), "an unreadable record is removed");

    let bad_codex = CodexPending {
        credit_id: "c".to_string(),
        redeem_request_id: "not a key!".to_string(),
        sent_at_ms: crate::usage::now_ms() - 10_000,
    };
    write(&path, &bad_codex).expect("write codex");
    assert!(
        take_fresh::<CodexPending>(&path).expect("load").is_none(),
        "an off-pattern redeem id reads unreadable"
    );
    assert!(!path.exists(), "an unreadable record is removed");
}
