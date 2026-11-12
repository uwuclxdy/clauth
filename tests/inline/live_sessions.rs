use super::*;
use std::sync::mpsc::channel;

use crate::testutil::{HomeSandbox, oauth_profile, write_linked_credentials};

/// A row for a session that never ran, with every field pinned so the tests
/// assert exact values rather than "something was written".
fn row(session_id: &str, profile: &str) -> LiveSession {
    LiveSession {
        session_id: session_id.to_string(),
        start_profile: profile.to_string(),
        harness: crate::harness::Harness::Claude,
        pid: 4242,
        claude_pid: None,
        launch_families: None,
        started_at: 1_700_000_000_000,
        cwd: Some(PathBuf::from("/w/proj")),
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
    }
}

/// A row written by a clauth that predates the opt-in field must not read as
/// opted IN on upgrade — the decision leg would then move EVERY live session off
/// the account it launched on.
#[test]
fn a_row_predating_the_opt_in_key_deserializes_as_not_following_the_chain() {
    let pre_upgrade = br#"{"session_id":"4242-0","start_profile":"work","pid":4242,
        "started_at":1700000000000,"cwd":"/w/proj","isolated":false}"#;

    let row: LiveSession = serde_json::from_slice(pre_upgrade).expect("parse a pre-upgrade row");

    assert!(
        !row.follows_chain,
        "a row with no `follows_chain` key must default to opted OUT"
    );
    assert_eq!(
        row.harness,
        crate::harness::Harness::Claude,
        "a row with no `harness` key predates the axis, so it is a claude row"
    );
}

/// A row predating the child-pid and launch-families fields parses as unjoined
/// and unknown-demand, and a `None` of either serializes ABSENT (not `null`),
/// so an older clauth's row round-trips byte-identical. The serialized-absence
/// half is what binds the serde pair: a parse-only pin would pass with
/// `#[serde(default)]` deleted, since serde implicitly defaults a missing
/// `Option`.
#[test]
fn a_row_predating_the_child_pid_key_reads_unjoined_and_omits_the_key() {
    let pre_upgrade = br#"{"session_id":"4242-0","start_profile":"work","pid":4242,
        "started_at":1700000000000,"cwd":"/w/proj","isolated":false}"#;

    let row: LiveSession = serde_json::from_slice(pre_upgrade).expect("parse a pre-upgrade row");
    assert_eq!(row.claude_pid, None, "no key must read as unjoined");
    assert_eq!(
        row.launch_families, None,
        "no key must read as unknown demand"
    );

    let written = serde_json::to_value(&row).expect("serialize");
    let obj = written.as_object().expect("object");
    assert!(
        !obj.contains_key("claude_pid"),
        "a None child pid must serialize absent, not as null"
    );
    assert!(
        !obj.contains_key("launch_families"),
        "a None launch demand must serialize absent, not as null"
    );
}

/// A row predating the credit-leave keys reads both as false, and a false of
/// either serializes ABSENT (not `false`), so an older clauth's row
/// round-trips byte-identical — the serde pair's serialized-absence half.
#[test]
fn a_row_predating_the_credit_leave_keys_omits_them() {
    let pre_upgrade = br#"{"session_id":"4242-0","start_profile":"work","pid":4242,
        "started_at":1700000000000,"cwd":"/w/proj","isolated":false}"#;

    let row: LiveSession = serde_json::from_slice(pre_upgrade).expect("parse a pre-upgrade row");
    assert!(!row.credit_leave, "no key must read as not a credit leave");
    assert!(
        !row.credit_leave_aware,
        "no key must read as not credit-leave-aware"
    );

    let written = serde_json::to_value(&row).expect("serialize");
    let obj = written.as_object().expect("object");
    assert!(
        !obj.contains_key("credit_leave"),
        "a false credit_leave must serialize absent, not as false"
    );
    assert!(
        !obj.contains_key("credit_leave_aware"),
        "a false credit_leave_aware must serialize absent, not as false"
    );
}

/// The tag survives the registry round-trip and spells itself lowercase, the
/// stable on-disk form a mixed-version window reads back.
#[test]
fn a_codex_row_round_trips_its_harness_tag() {
    let _home = HomeSandbox::new();
    let mut written = row("4242-0", "cx");
    written.harness = crate::harness::Harness::Codex;

    register(&written).expect("register");

    let listed = list().pop().expect("one row");
    assert_eq!(listed, written, "the tag must survive the round-trip");
    let raw = std::fs::read_to_string(
        crate::profile::clauth_dir()
            .expect("clauth dir")
            .join("live_sessions")
            .join("4242-0.json"),
    )
    .expect("read row file");
    assert!(
        raw.contains("\"harness\":\"codex\""),
        "the on-disk spelling is lowercase: {raw}"
    );
}

/// A row written by a clauth that predates the delegate mark must not read as
/// a delegate on upgrade — the header's split would then file every
/// already-running session under the delegates side.
#[test]
fn a_row_predating_the_delegate_key_deserializes_as_own() {
    let pre_upgrade = br#"{"session_id":"4242-0","start_profile":"work","pid":4242,
        "started_at":1700000000000,"cwd":"/w/proj","isolated":false}"#;

    let row: LiveSession = serde_json::from_slice(pre_upgrade).expect("parse a pre-upgrade row");

    assert!(
        !row.is_delegate,
        "a row with no `is_delegate` key must default to own"
    );
}

/// The mark survives the registry round-trip and spells itself a bare bool, the
/// stable on-disk form a mixed-version window reads back.
#[test]
fn a_delegate_row_round_trips_its_mark() {
    let _home = HomeSandbox::new();
    let mut written = row("4242-0", "work");
    written.is_delegate = true;

    register(&written).expect("register");

    let listed = list().pop().expect("one row");
    assert_eq!(listed, written, "the mark must survive the round-trip");
    let raw = std::fs::read_to_string(
        crate::profile::clauth_dir()
            .expect("clauth dir")
            .join("live_sessions")
            .join("4242-0.json"),
    )
    .expect("read row file");
    assert!(
        raw.contains("\"is_delegate\":true"),
        "the on-disk spelling carries the mark: {raw}"
    );
}

#[test]
fn register_then_list_returns_the_row() {
    let _home = HomeSandbox::new();
    let written = row("4242-0", "work");

    register(&written).expect("register");

    assert_eq!(list(), vec![written], "list must round-trip the exact row");
}

#[test]
fn each_writers_update_preserves_the_others_fields() {
    let _home = HomeSandbox::new();
    register(&row("4242-0", "work")).expect("register");

    update_as_daemon("4242-0", |d| {
        d.set_intended_member("kerry");
        d.set_chain_cursor(2);
    })
    .expect("daemon update");
    update_as_session("4242-0", |s| {
        s.set_current_member("work");
        s.set_last_swap_at(1_700_000_009_000);
    })
    .expect("session update");

    let after = list().pop().expect("one row");
    assert_eq!(after.intended_member.as_deref(), Some("kerry"));
    assert_eq!(after.chain_cursor, Some(2));
    assert_eq!(after.current_member.as_deref(), Some("work"));
    assert_eq!(after.last_swap_at, Some(1_700_000_009_000));

    // ...and the other direction: a daemon write after a session write must not
    // drop what the session put there.
    update_as_daemon("4242-0", |d| d.set_intended_member("filip")).expect("second daemon update");

    let after = list().pop().expect("one row");
    assert_eq!(after.intended_member.as_deref(), Some("filip"));
    assert_eq!(
        after.current_member.as_deref(),
        Some("work"),
        "the daemon's write clobbered the session's field"
    );
    assert_eq!(after.last_swap_at, Some(1_700_000_009_000));
    assert_eq!(after.chain_cursor, Some(2));
}

/// A string where the `pid` number belongs would otherwise quote it in the
/// parser's message; the update names the position alone.
#[test]
fn an_unreadable_row_names_the_position() {
    let _home = HomeSandbox::new();
    let path = row_path("4242-0").expect("row path");
    crate::profile::mkdir_700(path.parent().expect("parent")).expect("mkdir");
    std::fs::write(&path, "{\"pid\": \"sk-c2probe-NOTAKEY\"}").expect("mistyped row");

    let err = update_as_daemon("4242-0", |d| d.set_intended_member("kerry"))
        .expect_err("a mistyped row must fail the update");
    let rendered = format!("{err:#}");
    assert_eq!(
        rendered,
        "unreadable live-session row for 4242-0 (line 1, column 28)"
    );
    assert!(
        err.source().is_none(),
        "the parse error must not ride as a source: {err:?}"
    );
}

/// A delegate's row is registered by the `clauth mcp` that spawns it (that
/// process's `std::process::id()` is what register reads) and re-keyed onto the
/// delegate child right after spawn. `set_pid` is the mutator behind the
/// re-key, and it must move nothing else — the daemon's decision fields least
/// of all.
#[test]
fn set_pid_rekeys_the_row_and_touches_nothing_else() {
    let _home = HomeSandbox::new();
    register(&row("4242-0", "work")).expect("register");
    update_as_daemon("4242-0", |d| d.set_intended_member("kerry")).expect("daemon update");

    update_as_session("4242-0", |s| s.set_pid(7777)).expect("session update");

    let after = list().pop().expect("one row");
    assert_eq!(after.pid, 7777, "the row names the re-keyed process");
    assert_eq!(
        after.session_id, "4242-0",
        "the id is not part of the re-key"
    );
    assert_eq!(
        after.start_profile, "work",
        "the launch member is untouched"
    );
    assert_eq!(
        after.intended_member.as_deref(),
        Some("kerry"),
        "the daemon's field survives the re-key"
    );
    assert_eq!(after.current_member, None);
    assert_eq!(after.cwd.as_deref(), Some(Path::new("/w/proj")));
}

/// THE LOST-UPDATE TEST. The load has to happen INSIDE the state lock, not just
/// the store: a row read before a swap and written after silently reverts
/// whatever the other writer put there in between. Thread A parks inside its
/// closure while holding the lock; B contends. `with_state_lock` serializes them,
/// so B reloads A's stored row and the file must end up carrying BOTH writes.
#[test]
fn a_concurrent_daemon_write_is_not_lost_under_a_parked_session_write() {
    let _home = HomeSandbox::new();
    register(&row("4242-0", "work")).expect("register");

    let (inside_tx, inside_rx) = channel::<()>();
    let (release_tx, release_rx) = channel::<()>();

    let session_writer = std::thread::spawn(move || {
        update_as_session("4242-0", |s| {
            inside_tx.send(()).expect("signal inside");
            release_rx.recv().expect("await release");
            s.set_current_member("work");
        })
    });
    inside_rx.recv().expect("A reached its closure");

    let daemon_writer = std::thread::spawn(|| {
        update_as_daemon("4242-0", |d| {
            d.set_intended_member("kerry");
            d.set_chain_cursor(7);
        })
    });
    // B is contending for the lock (or about to be); let it get there before A
    // stores, so a load taken outside the lock would read the pre-A row.
    std::thread::sleep(std::time::Duration::from_millis(300));
    release_tx.send(()).expect("release A");

    session_writer
        .join()
        .expect("session thread panicked")
        .expect("session update");
    daemon_writer
        .join()
        .expect("daemon thread panicked")
        .expect("daemon update");

    let after = list().pop().expect("one row");
    assert_eq!(
        after.current_member.as_deref(),
        Some("work"),
        "the session's write was lost — the daemon stored a row it loaded before it"
    );
    assert_eq!(
        after.intended_member.as_deref(),
        Some("kerry"),
        "the daemon's write was lost — the session stored a row it loaded before it"
    );
    assert_eq!(after.chain_cursor, Some(7));
}

#[test]
fn unregister_removes_the_row_and_is_idempotent() {
    let _home = HomeSandbox::new();
    register(&row("4242-0", "work")).expect("register");
    register(&row("4242-1", "kerry")).expect("register sibling");

    unregister("4242-0").expect("unregister");

    let left: Vec<String> = list().into_iter().map(|r| r.session_id).collect();
    assert_eq!(left, vec!["4242-1".to_string()]);

    unregister("4242-0").expect("a row already gone is not an error");
}

#[test]
fn an_update_of_a_missing_row_names_the_id() {
    let _home = HomeSandbox::new();

    let err = update_as_daemon("4242-9", |d| d.set_chain_cursor(1))
        .expect_err("a missing row must not silently no-op");

    assert!(
        format!("{err:#}").contains("4242-9"),
        "the error must name the id, got: {err:#}"
    );
}

/// A path join must never take a separator or a `..` from an id read back off
/// disk or handed in by a later phase's decision leg.
#[test]
fn a_malformed_session_id_is_refused() {
    let _home = HomeSandbox::new();

    for bad in ["../escape", "4242-0/x", "", "isolated", "4242"] {
        assert!(
            unregister(bad).is_err(),
            "{bad:?} must be refused as a session id"
        );
    }
}

// ── live tally ───────────────────────────────────────────────────────────────

/// `current_member` is written only by a session's FIRST swap, so a session that
/// never moved — every pinned one, and every opted-in one before it swaps — is
/// running as the account it launched on. Reading `current_member` alone leaves
/// the overwhelmingly common case attributed to nobody.
#[test]
fn a_session_that_never_swapped_counts_on_the_account_it_launched_on() {
    let tally = LiveTally::from_live_rows([row("4242-0", "work")]);

    assert_eq!(
        tally
            .member(&crate::profile::ProfileName::from("work"))
            .sessions,
        1
    );
}

/// The other direction: once a session has swapped, the launch account is a
/// place nothing authenticates as, and counting it there is the exact defect
/// that made the Services tab report one child as two.
#[test]
fn a_swapped_session_counts_on_its_current_member_and_not_its_launch_one() {
    let mut swapped = row("4242-0", "work");
    swapped.current_member = Some("spare".to_string());

    let tally = LiveTally::from_live_rows([swapped]);

    assert_eq!(
        tally
            .member(&crate::profile::ProfileName::from("spare"))
            .sessions,
        1
    );
    assert_eq!(
        tally
            .member(&crate::profile::ProfileName::from("work"))
            .sessions,
        0
    );
}

/// `follows_chain` is what separates a session the chain can move from a pinned
/// one — both hold the account's marker and burn its window, so both are
/// counted, and only the follower earns the `⇄` the render layers put on it.
#[test]
fn only_opted_in_sessions_count_as_following_the_chain() {
    let pinned = row("4242-0", "work");
    let mut follower = row("4242-1", "work");
    follower.follows_chain = true;

    let tally = LiveTally::from_live_rows([pinned, follower]);

    assert_eq!(
        tally
            .member(&crate::profile::ProfileName::from("work"))
            .sessions,
        2
    );
    assert_eq!(
        tally
            .member(&crate::profile::ProfileName::from("work"))
            .following,
        1
    );
}

/// The card names when a session last landed here, so the newest swap onto an
/// account wins over an older one; a session that never swapped contributes no
/// stamp at all, which is also what tells the render layer no pickup lag applies.
#[test]
fn the_newest_swap_onto_an_account_is_the_one_reported() {
    let mut old = row("4242-0", "work");
    old.current_member = Some("spare".to_string());
    old.last_swap_at = Some(1_700_000_010_000);
    let mut new = row("4242-1", "work");
    new.current_member = Some("spare".to_string());
    new.last_swap_at = Some(1_700_000_020_000);
    let never = row("4242-2", "work");

    // Newest FIRST: `list()` reads rows back in readdir order, so a tally that
    // simply kept the last row it saw would agree with this fixture in ascending
    // order and disagree with production at random.
    let tally = LiveTally::from_live_rows([new, old, never]);

    assert_eq!(
        tally
            .member(&crate::profile::ProfileName::from("spare"))
            .last_swap_at,
        Some(1_700_000_020_000)
    );
    assert_eq!(
        tally
            .member(&crate::profile::ProfileName::from("work"))
            .last_swap_at,
        None
    );
}

/// An account hosting nothing reports zeroes, never a missing-key panic — the
/// render layers ask about every configured account, most of which host none.
#[test]
fn an_account_with_no_sessions_tallies_empty() {
    let tally = LiveTally::from_live_rows([row("4242-0", "work")]);

    assert_eq!(
        tally.member(&crate::profile::ProfileName::from("idle")),
        MemberSessions::default()
    );
}

/// Row GC runs from `gc_stale_runtimes` at daemon STARTUP, not per tick, so a
/// SIGKILLed session's row sits on disk for the whole daemon run. A tally that
/// trusted the file would keep showing a session that is gone.
#[test]
fn collect_drops_a_row_whose_session_is_no_longer_running() {
    let _home = HomeSandbox::new();

    let live = row("4242-0", "work");
    let dead = row("4242-1", "work");
    register(&live).expect("register the live row");
    register(&dead).expect("register the dead row");
    let _marker = crate::runtime::hold_session_row_marker(
        &crate::profile::ProfileName::from(live.start_profile.clone()),
        false,
        "4242-0",
    )
    .expect("hold the live session's marker");

    assert_eq!(
        LiveTally::collect(&config_with(vec![], "work"))
            .member(&crate::profile::ProfileName::from("work"))
            .sessions,
        1,
        "only the row whose marker is still held is a live session"
    );
}

/// F8: the daemon's background hold is not a clauth start session — one row per
/// fixed tree however many jobs, and it exists while a job names the tree, not
/// while a launch runs. A LAUNCHER's own background row (no hold flag) is still
/// a session, for its launch.
#[test]
fn collect_excludes_the_daemons_background_hold() {
    let _home = HomeSandbox::new();
    let name = crate::profile::ProfileName::from("work");

    let hold = LiveSession {
        background: true,
        background_hold: true,
        ..row("4242-0", "work")
    };
    register(&hold).expect("register the hold row");
    let _hold_marker = crate::runtime::hold_background_row_marker(&name, "4242-0")
        .expect("hold the daemon's marker");
    assert_eq!(
        LiveTally::collect(&config_with(vec![], "work"))
            .member(&name)
            .sessions,
        0,
        "the daemon's hold is no session"
    );

    let launcher = LiveSession {
        background: true,
        ..row("4243-0", "work")
    };
    register(&launcher).expect("register the launcher row");
    let _launcher_marker = crate::runtime::hold_background_row_marker(&name, "4243-0")
        .expect("hold the launcher's marker");
    assert_eq!(
        LiveTally::collect(&config_with(vec![], "work"))
            .member(&name)
            .sessions,
        1,
        "a launcher's own background row still counts"
    );
}

// ── background jobs ─────────────────────────────────────────────────────────

/// A `clauth start <p> -- --bg` session outlives its launcher: the launcher
/// exits, its marker releases, and Claude Code's job store alone still names
/// the account's fixed background tree. That job holds one of the account's
/// slots, so it counts on the account it runs on.
#[test]
fn a_background_job_with_a_released_marker_counts_on_its_account() {
    let home = HomeSandbox::new();
    let name = crate::profile::ProfileName::from("work");
    let config = config_with(vec![crate::testutil::blank_profile(&name)], "work");
    let tree = crate::runtime::background_runtime_tree(&name).expect("background tree");
    std::fs::create_dir_all(&tree).expect("build the fixed background tree");
    crate::testutil::write_live_supervisor(&tree);
    // The launcher's row outlives its marker: nothing holds it, so the job
    // store is the only evidence the session is still running.
    register(&LiveSession {
        background: true,
        ..row("4243-0", "work")
    })
    .expect("register the launcher row");
    crate::testutil::write_claude_job(home.home(), "job-1", &tree);

    assert_eq!(
        LiveTally::collect(&config).member(&name).sessions,
        1,
        "a running background job is a live session on the account it runs on"
    );
}

/// `claude stop` ends a background session but leaves its job dir in the store
/// until `claude rm` (the captured 2.1.304 shape: `state: "stopped"` with
/// `firstTerminalAt` stamped). A stopped job holds none of the account's
/// slots, so it is no live session — netting it in keeps every stopped job on
/// the shared fixed tree reading as a phantom session forever.
#[test]
fn a_stopped_job_counts_no_session() {
    let home = HomeSandbox::new();
    let name = crate::profile::ProfileName::from("work");
    let config = config_with(vec![crate::testutil::blank_profile(&name)], "work");
    let tree = crate::runtime::background_runtime_tree(&name).expect("background tree");
    std::fs::create_dir_all(&tree).expect("build the fixed background tree");
    crate::testutil::write_live_supervisor(&tree);
    register(&LiveSession {
        background: true,
        ..row("4243-0", "work")
    })
    .expect("register the launcher row");
    crate::testutil::write_claude_job_in(
        home.home(),
        "job-1",
        &tree,
        "stopped",
        Some("2026-10-30T00:00:00Z"),
    );

    assert_eq!(
        LiveTally::collect(&config).member(&name).sessions,
        0,
        "a stopped background job holds no slot, so it is no live session"
    );
}

/// A `claude attach` revives a stopped job as `running` with the
/// `firstTerminalAt` stamp kept, and a revived job runs again: it counts on the
/// account it runs on like any other working job.
#[test]
fn a_revived_job_counts_on_its_account() {
    let home = HomeSandbox::new();
    let name = crate::profile::ProfileName::from("work");
    let config = config_with(vec![crate::testutil::blank_profile(&name)], "work");
    let tree = crate::runtime::background_runtime_tree(&name).expect("background tree");
    std::fs::create_dir_all(&tree).expect("build the fixed background tree");
    crate::testutil::write_live_supervisor(&tree);
    register(&LiveSession {
        background: true,
        ..row("4243-0", "work")
    })
    .expect("register the launcher row");
    crate::testutil::write_claude_job_in(
        home.home(),
        "job-1",
        &tree,
        "running",
        Some("2026-10-30T00:00:00Z"),
    );

    assert_eq!(
        LiveTally::collect(&config).member(&name).sessions,
        1,
        "a revived job runs again, so it counts on its account"
    );
}

/// A handed-off (`--with-fallback`) session's tree sits under its START
/// profile's dir while the session runs on `current_member`, so the job that
/// names it counts on the member the row names and on no earlier account.
#[test]
fn a_handed_off_job_counts_only_on_its_current_account() {
    let home = HomeSandbox::new();
    let work = crate::profile::ProfileName::from("work");
    let spare = crate::profile::ProfileName::from("spare");
    let config = config_with(
        vec![
            crate::testutil::blank_profile(&work),
            crate::testutil::blank_profile(&spare),
        ],
        "work",
    );
    register(&LiveSession {
        current_member: Some("spare".to_string()),
        follows_chain: true,
        ..row("7777-0", "work")
    })
    .expect("register the moved row");
    let trees = crate::runtime::session_runtime_trees(&work, false, "7777-0").expect("tree layout");
    let tree = trees[0].clone();
    std::fs::create_dir_all(&tree).expect("build the per-session tree");
    crate::testutil::write_live_supervisor(&tree);
    crate::testutil::write_claude_job(home.home(), "job-1", &tree);

    let tally = LiveTally::collect(&config);
    assert_eq!(
        tally.member(&spare).sessions,
        1,
        "the job counts on the account the session runs on now"
    );
    assert_eq!(
        tally.member(&work).sessions,
        0,
        "the account the session left counts nothing"
    );
    // The whole slice, so the job's `following` cannot drop off the mark the
    // chain card and the Overview `⇄` glyph read.
    assert_eq!(
        tally.member(&spare),
        MemberSessions {
            sessions: 1,
            delegates: 0,
            following: 1,
            last_swap_at: None,
        },
        "a job on a follower's tree is a session the chain may move"
    );
}

/// A launcher's held marker and the job naming its tree are ONE session: the
/// count takes the greater of the two readings per tree, never their sum, and
/// a second job beside a held marker is the second session it is.
#[test]
fn a_job_beside_a_held_marker_counts_the_session_once() {
    let home = HomeSandbox::new();
    let name = crate::profile::ProfileName::from("work");
    let config = config_with(vec![crate::testutil::blank_profile(&name)], "work");
    let tree = crate::runtime::background_runtime_tree(&name).expect("background tree");
    std::fs::create_dir_all(&tree).expect("build the fixed background tree");
    crate::testutil::write_live_supervisor(&tree);
    register(&LiveSession {
        background: true,
        ..row("4243-0", "work")
    })
    .expect("register the launcher row");
    let _marker =
        crate::runtime::hold_background_row_marker(&name, "4243-0").expect("hold the marker");
    crate::testutil::write_claude_job(home.home(), "job-1", &tree);

    assert_eq!(
        LiveTally::collect(&config).member(&name).sessions,
        1,
        "the held marker and the job naming its tree are one session"
    );

    crate::testutil::write_claude_job(home.home(), "job-2", &tree);
    assert_eq!(
        LiveTally::collect(&config).member(&name).sessions,
        2,
        "a second job on the same tree is a second session"
    );
}

/// A job store the tally cannot read leaves the fleet total unread — the same
/// direction the bare-marker dir takes: folding an unknown in as zero would
/// hide a running session that holds one of the account's slots.
#[test]
fn an_unreadable_job_store_leaves_the_total_unread() {
    let _home = HomeSandbox::new();
    let name = crate::profile::ProfileName::from("work");
    let config = config_with(vec![crate::testutil::blank_profile(&name)], "work");
    let claude = crate::profile::claude_dir().expect("claude dir");
    std::fs::create_dir_all(&claude).expect("mkdir .claude");
    std::fs::write(claude.join("jobs"), b"not a directory").expect("block the job store");

    assert_eq!(
        LiveTally::collect(&config).total(),
        None,
        "an unreadable job store has no count to give"
    );
}

/// A TREE-local store failing is unknown for that account alone: its own jobs
/// contribute nothing while every readable account's jobs still count. The
/// fleet chip's own/delegates are single fleet numbers, so one account's
/// unknown contribution still leaves the total unread — folding it in as zero
/// is the direction this tally already refuses.
#[test]
fn a_tree_local_unreadable_job_store_leaves_the_total_unread() {
    let _home = HomeSandbox::new();
    let name = crate::profile::ProfileName::from("work");
    let config = config_with(vec![crate::testutil::blank_profile(&name)], "work");
    let tree = crate::runtime::background_runtime_tree(&name).expect("background tree");
    std::fs::create_dir_all(&tree).expect("build the fixed background tree");
    std::fs::write(tree.join("jobs"), b"not a directory").expect("block the tree's job store");

    assert_eq!(
        LiveTally::collect(&config).total(),
        None,
        "one account's unreadable job store leaves the fleet total unread"
    );
}

/// One account's unreadable tree-local store contributes no job of its own and
/// must not discard ANOTHER account's running background job: the readable
/// account's job still counts on it, while the one unknown contribution leaves
/// the fleet total unread.
#[test]
fn an_unreadable_tree_does_not_discard_another_accounts_background_job() {
    let home = HomeSandbox::new();
    let work = crate::profile::ProfileName::from("work");
    let busy = crate::profile::ProfileName::from("busy");
    let config = config_with(
        vec![
            crate::testutil::blank_profile(&work),
            crate::testutil::blank_profile(&busy),
        ],
        "work",
    );
    let work_tree = crate::runtime::background_runtime_tree(&work).expect("work's tree");
    let busy_tree = crate::runtime::background_runtime_tree(&busy).expect("busy's tree");
    std::fs::create_dir_all(&work_tree).expect("build work's tree");
    std::fs::create_dir_all(&busy_tree).expect("build busy's tree");
    crate::testutil::write_live_supervisor(&busy_tree);
    std::fs::write(work_tree.join("jobs"), b"not a directory").expect("block work's job store");
    crate::testutil::write_claude_job(home.home(), "job-1", &busy_tree);

    let tally = LiveTally::collect(&config);
    assert_eq!(
        tally.member(&busy).sessions,
        1,
        "a readable account's running background job still counts on it"
    );
    assert_eq!(
        tally.member(&work).sessions,
        0,
        "the unreadable account contributes no job of its own"
    );
    assert_eq!(
        tally.total(),
        None,
        "the one unknown contribution still leaves the fleet total unread"
    );
}

/// A host upgraded from the pre-`runtime-<sid>` keying keeps the legacy bare
/// `runtime` dir beside the per-session trees. A per-session row occupies only
/// its OWN tree, so a job naming the bare legacy tree still counts: netting it
/// against a row that does not hold that tree would net a real session away.
#[test]
fn a_row_nets_jobs_only_against_the_tree_it_occupies() {
    let home = HomeSandbox::new();
    let name = crate::profile::ProfileName::from("work");
    let config = config_with(vec![crate::testutil::blank_profile(&name)], "work");
    let trees = crate::runtime::session_runtime_trees(&name, false, "4242-0").expect("layout");
    let per_session = trees[0].clone();
    let legacy = trees[1].clone();
    std::fs::create_dir_all(&per_session).expect("build the per-session tree");
    std::fs::create_dir_all(&legacy).expect("build the leftover legacy tree");
    crate::testutil::write_live_supervisor(&legacy);
    register(&row("4242-0", "work")).expect("register the per-session row");
    let _marker = crate::runtime::hold_session_row_marker(&name, false, "4242-0")
        .expect("hold the per-session marker");
    crate::testutil::write_claude_job(home.home(), "job-1", &legacy);

    assert_eq!(
        LiveTally::collect(&config).member(&name).sessions,
        2,
        "the row and the job naming the bare legacy tree are two sessions"
    );
}

/// A job record's `state` alone does not say a session runs. A supervisor that
/// shut down, was killed, or never started leaves its job's record at `state:
/// "working"` until `claude rm`, and nothing reads that record: the count asks
/// the tree's own supervisor, which fails closed only for the gate that must
/// not collect a tree under a job.
#[test]
fn a_job_counts_only_while_its_trees_supervisor_is_live() {
    let home = HomeSandbox::new();
    let name = crate::profile::ProfileName::from("work");
    let config = config_with(vec![crate::testutil::blank_profile(&name)], "work");
    let tree = crate::runtime::background_runtime_tree(&name).expect("background tree");
    std::fs::create_dir_all(&tree).expect("build the fixed background tree");
    crate::testutil::write_claude_job(home.home(), "job-1", &tree);

    assert_eq!(
        LiveTally::collect(&config).member(&name).sessions,
        0,
        "a working job whose tree has no supervisor record holds no slot"
    );

    // A recorded pid that is positively dead — a child that exited and was
    // reaped — is a `Gone` supervisor: nothing supervises the tree, so the
    // working job holds no slot. Linux/macOS only while the Windows verify has
    // no positively-dead reading there (every parseable record reads
    // `Unverifiable` and counts).
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        let mut dead = std::process::Command::new("true")
            .spawn()
            .expect("spawn a child to reap");
        let dead_pid = dead.id();
        let start = crate::testutil::proc_start_of(dead_pid).expect("the dead pid's live start");
        let _ = dead.wait();
        crate::testutil::write_daemon_lock(&tree, dead_pid, &start);

        assert_eq!(
            LiveTally::collect(&config).member(&name).sessions,
            0,
            "a working job whose recorded supervisor pid is positively dead holds no slot"
        );
    }

    crate::testutil::write_live_supervisor(&tree);
    assert_eq!(
        LiveTally::collect(&config).member(&name).sessions,
        1,
        "the same job under a live supervisor counts on its account"
    );
}

/// A `daemon.lock` that does not parse names no pid, so nothing shows a live
/// supervisor: the count reads the tree unread, never as a live session.
#[test]
fn an_unparseable_supervisor_record_reads_unread_not_live() {
    let home = HomeSandbox::new();
    let name = crate::profile::ProfileName::from("work");
    let config = config_with(vec![crate::testutil::blank_profile(&name)], "work");
    let tree = crate::runtime::background_runtime_tree(&name).expect("background tree");
    std::fs::create_dir_all(&tree).expect("build the fixed background tree");
    crate::testutil::write_live_supervisor(&tree);
    crate::testutil::write_claude_job(home.home(), "job-1", &tree);
    assert_eq!(
        LiveTally::collect(&config).member(&name).sessions,
        1,
        "fixture control: the same tree with a parseable live record counts its job"
    );

    std::fs::write(tree.join("daemon.lock"), br#"{"pid":12"#).expect("truncate the lock");

    let tally = LiveTally::collect(&config);
    assert_eq!(
        tally.member(&name).sessions,
        0,
        "a record naming no pid shows no live supervisor, so its job holds no slot"
    );
    assert_eq!(
        tally.total(),
        None,
        "the count has no reading to give, so it says unread rather than complete"
    );
}

/// An older build linked the home's `daemon.lock`/`daemon.status.json` into the
/// tree (see `runtime::read_supervisor_record`). For the count the link is
/// FOLLOWED and its target verified like a plain record: the home roster
/// supervisor's liveness is exactly what the tree's job hangs on. A dangling
/// link or a target whose recorded pid is positively dead holds no slot — a
/// clean shutdown deletes the target file, leaving the tree-side link to
/// dangle. The GC keeps reading the link as absent, so the tree still collects
/// with no stop.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn a_symlinked_supervisor_record_is_verified_through_its_link() {
    enum Target {
        Missing,
        DeadPid,
        LivePid,
    }
    for (shape, target, counts) in [
        ("dangling", Target::Missing, 0),
        ("dead pid", Target::DeadPid, 0),
        ("live pid", Target::LivePid, 1),
    ] {
        let home = HomeSandbox::new();
        let name = crate::profile::ProfileName::from("work");
        let config = config_with(vec![crate::testutil::blank_profile(&name)], "work");
        let tree = crate::runtime::background_runtime_tree(&name).expect("background tree");
        std::fs::create_dir_all(&tree).expect("build the fixed background tree");
        let record_dir = home.home().join("roster-supervisor");
        std::fs::create_dir_all(&record_dir).expect("the linked record's dir");
        match target {
            Target::Missing => {}
            Target::DeadPid => {
                let mut dead = std::process::Command::new("true")
                    .spawn()
                    .expect("spawn a child to reap");
                let dead_pid = dead.id();
                let start =
                    crate::testutil::proc_start_of(dead_pid).expect("the dead pid's live start");
                let _ = dead.wait();
                crate::testutil::write_daemon_lock(&record_dir, dead_pid, &start);
            }
            Target::LivePid => {
                let pid = std::process::id();
                let start =
                    crate::testutil::proc_start_of(pid).expect("the test process's start time");
                crate::testutil::write_daemon_lock(&record_dir, pid, &start);
            }
        }
        std::os::unix::fs::symlink(record_dir.join("daemon.lock"), tree.join("daemon.lock"))
            .expect("link the lock");
        crate::testutil::write_claude_job(home.home(), "job-1", &tree);

        assert_eq!(
            LiveTally::collect(&config).member(&name).sessions,
            counts,
            "{shape}: the link's target decides whether the job counts"
        );
    }
}

/// A profile dir that is a symlink to a dir outside `profiles/` is a layout
/// [`crate::runtime::runtime_trees`] deliberately follows. Its tree's canonical
/// path then sits outside `profiles/`, so attribution must come from the
/// profiles walk's own name for the dir, not from the tree path's grandparent:
/// the job counts on the configured name.
#[cfg(unix)]
#[test]
fn a_job_under_a_profile_dir_linked_outside_profiles_counts_on_its_name() {
    let home = HomeSandbox::new();
    let name = crate::profile::ProfileName::from("work");
    let config = config_with(vec![crate::testutil::blank_profile(&name)], "work");
    let profile = crate::profile::profile_dir(&name).expect("profile dir");
    std::fs::create_dir_all(profile.parent().expect("profiles root")).expect("mkdir profiles");
    let target = home.home().join("elsewhere");
    std::fs::create_dir_all(&target).expect("mkdir the link target");
    std::os::unix::fs::symlink(&target, &profile).expect("link the profile dir");
    let tree = crate::runtime::background_runtime_tree(&name).expect("background tree");
    std::fs::create_dir_all(&tree).expect("build the tree through the link");
    crate::testutil::write_live_supervisor(&tree);
    crate::testutil::write_claude_job(home.home(), "job-1", &tree);

    assert_eq!(
        LiveTally::collect(&config).member(&name).sessions,
        1,
        "a job under a profile dir linked outside profiles/ counts on its own name"
    );
}

/// A profile dir linked to a SIBLING dir inside `profiles/` canonicalizes to
/// the sibling, so a tree-path grandparent test attributes the job to the
/// sibling's entry name. Attribution reads the profiles walk instead: the
/// configured dir's own name wins over the dir it points at.
#[cfg(unix)]
#[test]
fn a_job_under_a_profile_dir_linked_to_a_sibling_counts_on_its_own_name() {
    let home = HomeSandbox::new();
    let name = crate::profile::ProfileName::from("work");
    let pointed_at = crate::profile::ProfileName::from("realwork");
    let config = config_with(vec![crate::testutil::blank_profile(&name)], "work");
    let profile = crate::profile::profile_dir(&name).expect("profile dir");
    let sibling = profile.parent().expect("profiles root").join("realwork");
    std::fs::create_dir_all(&sibling).expect("mkdir the sibling");
    std::os::unix::fs::symlink(&sibling, &profile).expect("link the profile dir");
    let tree = crate::runtime::background_runtime_tree(&name).expect("background tree");
    std::fs::create_dir_all(&tree).expect("build the tree through the link");
    crate::testutil::write_live_supervisor(&tree);
    crate::testutil::write_claude_job(home.home(), "job-1", &tree);

    let tally = LiveTally::collect(&config);
    assert_eq!(
        tally.member(&name).sessions,
        1,
        "a job under a sibling-linked profile dir counts on its configured name"
    );
    assert_eq!(
        tally.member(&pointed_at).sessions,
        0,
        "and never on the name of the sibling it points at"
    );
}

/// A `claude --bg` on a config dir clauth does not own names no clauth session:
/// a live job whose tree sits outside `profiles/` counts on no account and
/// leaves the fleet total at zero.
#[test]
fn a_job_outside_the_profiles_root_counts_on_no_account() {
    let home = HomeSandbox::new();
    let name = crate::profile::ProfileName::from("work");
    let config = config_with(vec![crate::testutil::blank_profile(&name)], "work");
    std::fs::create_dir_all(crate::profile::profile_dir(&name).expect("profile dir"))
        .expect("mkdir work, so an account exists to misattribute to");
    let foreign = home.home().join("some-config").join("runtime-bg");
    std::fs::create_dir_all(&foreign).expect("mkdir a foreign tree");
    crate::testutil::write_live_supervisor(&foreign);
    crate::testutil::write_claude_job(home.home(), "job-1", &foreign);

    let tally = LiveTally::collect(&config);
    assert_eq!(tally.member(&name).sessions, 0, "no account owns the tree");
    assert_eq!(tally.total(), Some(0), "the job counts on no account");
}

/// A link whose target is not a regular file — a writer-less FIFO, a device —
/// must neither hang nor blow up the count: the tally runs at 1 Hz in the TUI
/// and once per MCP `profiles` call, so the read opens with `O_NONBLOCK` (a
/// writer-less FIFO opens at once) and rejects the open handle's non-regular
/// type (a device read never ends). Such a target names no pid, so nothing
/// shows a live supervisor: the count reads the tree unread and the collect
/// returns within a bounded wait.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn a_non_regular_link_target_returns_bounded_and_reads_unread() {
    for (shape, link_target) in [("fifo", None), ("/dev/zero", Some("/dev/zero"))] {
        let home = HomeSandbox::new();
        let name = crate::profile::ProfileName::from("work");
        let config = config_with(vec![crate::testutil::blank_profile(&name)], "work");
        let tree = crate::runtime::background_runtime_tree(&name).expect("background tree");
        std::fs::create_dir_all(&tree).expect("build the fixed background tree");
        let target = match link_target {
            None => {
                let fifo = home.home().join("roster-fifo");
                crate::testutil::mkfifo(&fifo);
                fifo
            }
            Some(path) => std::path::PathBuf::from(path),
        };
        std::os::unix::fs::symlink(&target, tree.join("daemon.lock")).expect("link the lock");
        crate::testutil::write_claude_job(home.home(), "job-1", &tree);

        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let tally = LiveTally::collect(&config);
            let sessions = tally.member(&name).sessions;
            tx.send((sessions, tally.total()))
                .expect("report the collect");
        });
        let (sessions, total) = rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("the collect returns within the bound");
        assert_eq!(
            sessions, 0,
            "{shape}: a non-regular link target shows no live supervisor"
        );
        assert_eq!(total, None, "{shape}: so the count has no reading to give");
    }
}

/// A registry row that is not a regular file — a FIFO planted among the row
/// files — must neither hang nor blow up the count: the registry read runs
/// every tick. The row is dropped the way an unreadable one always was, and
/// the collect returns within a bounded wait.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn a_non_regular_registry_row_returns_bounded_and_drops_the_row() {
    let _home = HomeSandbox::new();
    let name = crate::profile::ProfileName::from("work");
    let config = config_with(vec![crate::testutil::blank_profile(&name)], "work");
    crate::live_sessions::register(&row("4242-0", "work")).expect("register a real row");
    let _marker = crate::runtime::hold_session_row_marker(&name, false, "4242-0")
        .expect("hold the real row's marker");
    let fifo = registry_dir().expect("registry dir").join("4242-9");
    crate::testutil::mkfifo(&fifo);

    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let sessions = LiveTally::collect(&config).member(&name).sessions;
        tx.send(sessions).expect("report the collect");
    });
    let sessions = rx
        .recv_timeout(std::time::Duration::from_secs(10))
        .expect("the collect returns within the bound");
    assert_eq!(
        sessions, 1,
        "the planted FIFO row is dropped, the real row still counts"
    );
}

/// A FIFO at the global credentials path — the well-known file every bare-marker
/// tally tick and every session-scope `profiles` call reads — must neither hang
/// nor blow up the count: the read opens with `O_NONBLOCK` and rejects the open
/// handle's non-regular type. A non-regular file reads `None`, the arm an
/// unreadable credentials file already takes: the resolution proceeds
/// credential-less and the bare session lands in the unattributed bucket, the
/// count still a count.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn a_non_regular_credentials_file_returns_bounded_and_reads_unknown() {
    let home = HomeSandbox::new();
    let name = crate::profile::ProfileName::from("work");
    let config = config_with(vec![crate::testutil::blank_profile(&name)], "work");
    crate::testutil::write_linked_credentials("rt-work");
    let _bare = crate::runtime::register_bare_session().expect("hold a bare marker");
    let fifo = home.home().join(".claude").join(".credentials.json");
    std::fs::remove_file(&fifo).expect("clear the credentials file");
    crate::testutil::mkfifo(&fifo);

    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let tally = LiveTally::collect(&config);
        tx.send(tally).expect("report the collect");
    });
    let tally = rx
        .recv_timeout(std::time::Duration::from_secs(10))
        .expect("the collect returns within the bound");
    // The resolution proceeds credential-less — the same arm any unreadable
    // credentials file takes — and tier 4 attributes the bare session to the
    // credential-less active profile, so the tally stays a count.
    assert_eq!(
        tally.member(&name).sessions,
        1,
        "the non-regular credentials file reads unknown: the bare session attributes \
         through the credential-less active profile"
    );
    assert_eq!(
        tally.total(),
        Some(1),
        "the collect is a count, not an unread"
    );
}

/// A FIFO at a profile's `session-token.json` sidecar must neither hang nor
/// blow up the count: a refresh-less login's resolution walks every profile's
/// sidecar on every bare-marker tally tick (tier 1b), so the read opens with
/// `O_NONBLOCK` and rejects the open handle's non-regular type. A non-regular
/// sidecar reads `None`, the arm a corrupt sidecar already takes: the tier
/// misses and the count stays a count.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn a_non_regular_session_token_returns_bounded_and_misses_the_tier() {
    let _home = HomeSandbox::new();
    let name = crate::profile::ProfileName::from("work");
    let config = config_with(vec![crate::testutil::blank_profile(&name)], "work");
    // Tier 1b fires only on a refresh-less login: a real credentials file
    // carrying an access token and no refresh token.
    let claude = crate::profile::claude_dir().expect("claude dir");
    std::fs::create_dir_all(&claude).expect("mkdir .claude");
    std::fs::write(
        claude.join(".credentials.json"),
        r#"{"claudeAiOauth":{"accessToken":"at-fifo"}}"#,
    )
    .expect("write the refresh-less credentials");
    let profile_dir = crate::profile::profile_dir(&name).expect("profile dir");
    std::fs::create_dir_all(&profile_dir).expect("mkdir the profile dir");
    crate::testutil::mkfifo(&profile_dir.join("session-token.json"));
    let _bare = crate::runtime::register_bare_session().expect("hold a bare marker");

    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let tally = LiveTally::collect(&config);
        tx.send(tally).expect("report the collect");
    });
    let tally = rx
        .recv_timeout(std::time::Duration::from_secs(10))
        .expect("the collect returns within the bound");
    assert_eq!(
        tally.member(&name).sessions,
        1,
        "the non-regular sidecar misses the tier: the bare session attributes through \
         the credential-less active profile"
    );
    assert_eq!(
        tally.total(),
        Some(1),
        "the collect is a count, not an unread"
    );
}

/// A job record whose first 64 KiB parses and whose tail is junk pins the read
/// cap's bounded-work half: the capped read stops at the cap, the record
/// parses and the job counts; an unbounded read would swallow the tail, fail
/// the parse and read the store unreadable (0). No under-cap fixture can tell
/// the two apart.
#[test]
fn a_job_record_at_the_cap_reads_only_the_cap() {
    let home = HomeSandbox::new();
    let name = crate::profile::ProfileName::from("work");
    let config = config_with(vec![crate::testutil::blank_profile(&name)], "work");
    let tree = crate::runtime::background_runtime_tree(&name).expect("background tree");
    std::fs::create_dir_all(&tree).expect("build the fixed background tree");
    crate::testutil::write_live_supervisor(&tree);
    let job_dir = home.home().join(".claude").join("jobs").join("0add99f4");
    std::fs::create_dir_all(&job_dir).expect("mkdir job dir");
    let mut record = serde_json::json!({
        "state": "working",
        "respawnFlags": [],
        "providerEnv": { "CLAUDE_CONFIG_DIR": tree.to_string_lossy() },
    })
    .to_string();
    let cap = crate::runtime::RECORD_READ_CAP as usize;
    while record.len() < cap {
        record.push(' ');
    }
    let mut bytes = record.into_bytes();
    assert_eq!(
        bytes.len(),
        crate::runtime::RECORD_READ_CAP as usize,
        "fixture: the padded record fills exactly the cap"
    );
    bytes.extend_from_slice(b"junk");
    std::fs::write(job_dir.join("state.json"), &bytes).expect("write the oversized record");

    assert_eq!(
        LiveTally::collect(&config).member(&name).sessions,
        1,
        "the capped read stops at the cap: the record parses and the job counts"
    );
}

/// `claude --bg` stages its record as `state.json.tmp.<rand>` before renaming
/// it, and creates the job dir empty first, so a store caught mid-create holds
/// a dir no record can be read from. That is a job that names no tree yet, not
/// an unreadable store: the store's real records must still count, and the
/// fleet total must stay a count rather than going unknown.
#[test]
fn a_mid_create_job_dir_does_not_hide_the_stores_running_jobs() {
    for staged in [None, Some("state.json.tmp.8d511663")] {
        let home = HomeSandbox::new();
        let name = crate::profile::ProfileName::from("work");
        let config = config_with(vec![crate::testutil::blank_profile(&name)], "work");
        let tree = crate::runtime::background_runtime_tree(&name).expect("background tree");
        std::fs::create_dir_all(&tree).expect("build the fixed background tree");
        crate::testutil::write_live_supervisor(&tree);
        crate::testutil::write_claude_job(home.home(), "job-1", &tree);
        let mid_create = home
            .home()
            .join(".claude")
            .join("jobs")
            .join("job-creating");
        std::fs::create_dir_all(&mid_create).expect("mkdir the job dir");
        if let Some(staged) = staged {
            std::fs::write(mid_create.join(staged), b"{\"state\":").expect("stage the record");
        }

        let tally = LiveTally::collect(&config);
        assert_eq!(
            tally.member(&name).sessions,
            1,
            "{staged:?}: the store's running job still counts"
        );
        assert_eq!(
            tally.total(),
            Some(1),
            "{staged:?}: a job dir with no record yet is no unreadable store"
        );
    }
}

/// A bg job on an account deleted with `delete --force` still spends that
/// account's window and still runs in a tree under `profiles/`: the count walks
/// the job trees, so the job reaches the fleet total and the account's slot the
/// way a registry row of a deleted account already does.
#[test]
fn a_job_on_a_deleted_account_reaches_the_fleet_total() {
    let home = HomeSandbox::new();
    let deleted = crate::profile::ProfileName::from("gone");
    let config = config_with(vec![], "work");
    let tree = crate::runtime::background_runtime_tree(&deleted).expect("background tree");
    std::fs::create_dir_all(&tree).expect("build the deleted account's tree");
    crate::testutil::write_live_supervisor(&tree);
    crate::testutil::write_claude_job(home.home(), "job-1", &tree);

    let tally = LiveTally::collect(&config);
    assert_eq!(
        tally.member(&deleted).sessions,
        1,
        "the job counts on the account whose dir its tree sits under"
    );
    assert_eq!(
        tally.total(),
        Some(1),
        "a session on a deleted account still spends, so the fleet total counts it"
    );
}

/// After `clauth delete <launch_profile> --force`, the launch profile's marker
/// dir is gone — but the session keeps running on `current_member` and holds
/// that member's marker. The probe must look at `current_member`, not
/// `start_profile`, to find it.
#[test]
fn a_swapped_session_counts_on_current_member_after_its_launch_marker_is_removed() {
    let _home = HomeSandbox::new();

    let swapped = LiveSession {
        start_profile: "work".to_string(),
        current_member: Some("spare".to_string()),
        ..row("4242-0", "work")
    };
    register(&swapped).expect("register the swapped row");
    // Hold the marker for `current_member` ("spare"), not `start_profile`
    // ("work"). The procedure `clauth delete work --force` removed the
    // work markers, so only the spare marker exists.
    let _marker = crate::runtime::hold_session_row_marker(
        &crate::profile::ProfileName::from("spare"),
        false,
        "4242-0",
    )
    .expect("hold the spare member's marker");

    let config = config_with(vec![], "work");
    assert_eq!(
        LiveTally::collect(&config)
            .member(&crate::profile::ProfileName::from("spare"))
            .sessions,
        1,
        "the swapped session must count on current_member when its \
         start_profile marker is absent"
    );
    assert_eq!(
        LiveTally::collect(&config)
            .member(&crate::profile::ProfileName::from("work"))
            .sessions,
        0,
        "the swapped session must not count on the launch account"
    );
}

/// Mirror: a session that never swapped has no `current_member`, so the probe
/// falls back to `start_profile` — the only marker dir that exists.
#[test]
fn a_session_that_never_swapped_probes_start_profile_in_collect() {
    let _home = HomeSandbox::new();

    let never = LiveSession {
        current_member: None,
        ..row("4242-0", "work")
    };
    register(&never).expect("register the row");
    // Only the start_profile marker exists.
    let _marker = crate::runtime::hold_session_row_marker(
        &crate::profile::ProfileName::from("work"),
        false,
        "4242-0",
    )
    .expect("hold the start_profile marker");

    assert_eq!(
        LiveTally::collect(&config_with(vec![], "work"))
            .member(&crate::profile::ProfileName::from("work"))
            .sessions,
        1,
        "a session that never swapped must count on the launch account"
    );
}

// ── bare `claude` sessions ───────────────────────────────────────────────────

fn config_with(profiles: Vec<crate::profile::Profile>, active: &str) -> AppConfig {
    AppConfig {
        state: crate::profile::AppState {
            active_profile: Some(active.into()),
            profiles: profiles.iter().map(|p| p.name.clone()).collect(),
            ..Default::default()
        },
        profiles,
    }
}

/// A bare `claude` — started without `clauth start` — burns the same account
/// window a supervised session does, and the daemon's global auto-switch really
/// does move it (it repoints the very link the session re-reads), so it counts as
/// following the chain too.
#[test]
fn a_held_bare_marker_counts_on_the_account_the_credential_link_resolves_to() {
    let _home = HomeSandbox::new();
    let config = config_with(vec![oauth_profile("work", "rt-work")], "work");
    write_linked_credentials("rt-work");

    let _bare = crate::runtime::register_bare_session().expect("hold a bare marker");

    assert_eq!(
        LiveTally::collect(&config).member(&crate::profile::ProfileName::from("work")),
        MemberSessions {
            sessions: 1,
            delegates: 0,
            following: 1,
            last_swap_at: None,
        },
    );
}

/// The link is the fact; `active_profile` is a wish. Under a divergence the bare
/// `claude` authenticates as whatever the link resolves to, so that is where its
/// window is spent and where it must be counted.
#[test]
fn bare_attribution_follows_the_credential_link_not_the_active_profile() {
    let _home = HomeSandbox::new();
    let config = config_with(
        vec![
            oauth_profile("work", "rt-work"),
            oauth_profile("spare", "rt-spare"),
        ],
        "work",
    );
    write_linked_credentials("rt-spare");

    let _bare = crate::runtime::register_bare_session().expect("hold a bare marker");

    let tally = LiveTally::collect(&config);
    assert_eq!(
        tally
            .member(&crate::profile::ProfileName::from("spare"))
            .sessions,
        1
    );
    assert_eq!(
        tally.member(&crate::profile::ProfileName::from("work")),
        MemberSessions::default(),
        "the account the link does NOT resolve to hosts nothing"
    );
}

/// A bare `claude` on a link no profile matches (rotated tokens, an account
/// clauth never captured) is attributed to no account, but it is still a live
/// session: the fleet total counts it.
#[test]
fn an_unattributed_bare_session_still_counts_toward_the_fleet_total() {
    let _home = HomeSandbox::new();
    let config = config_with(vec![oauth_profile("work", "rt-work")], "work");
    write_linked_credentials("rt-unknown");

    let _bare = crate::runtime::register_bare_session().expect("hold a bare marker");

    let tally = LiveTally::collect(&config);
    assert_eq!(
        tally.member(&crate::profile::ProfileName::from("work")),
        MemberSessions::default(),
        "no account hosts it"
    );
    assert_eq!(tally.total(), Some(1));
    assert_eq!(
        tally.own(),
        Some(1),
        "an unattributed bare session counts as own"
    );
    assert_eq!(tally.delegates(), Some(0));
}

/// A bare `claude` is the end user's own session, never a delegate: the split
/// folds it into the own side, and `total` stays the sum of both parts.
#[test]
fn a_bare_session_counts_as_own() {
    let _home = HomeSandbox::new();
    let config = config_with(vec![oauth_profile("work", "rt-work")], "work");
    write_linked_credentials("rt-work");

    let _bare = crate::runtime::register_bare_session().expect("hold a bare marker");

    let tally = LiveTally::collect(&config);
    assert_eq!(tally.total(), Some(1));
    assert_eq!(tally.own(), Some(1), "a bare session is an own session");
    assert_eq!(tally.delegates(), Some(0));
}

/// `total` counts sessions, never `following`, and a session whose account was
/// deleted stays in it: the tally is keyed by the row's member, not the config.
#[test]
fn the_fleet_total_counts_every_session_whatever_its_account_or_chain_flag() {
    let tally = LiveTally::of([
        LiveSession {
            follows_chain: true,
            ..crate::testutil::live_row("4242-0", "work")
        },
        LiveSession {
            follows_chain: false,
            ..crate::testutil::live_row("4242-1", "work")
        },
        crate::testutil::live_row("4343-0", "deleted-account"),
    ]);

    assert_eq!(tally.total(), Some(3));
}

/// The header splits the fleet count into own (non-delegate) and delegate
/// sessions. A codex row is own, and `total` keeps counting both, so every
/// existing caller of `total` keeps its meaning.
#[test]
fn the_tally_splits_own_from_delegates_and_total_is_unchanged() {
    let mut codex = row("4242-2", "work");
    codex.harness = crate::harness::Harness::Codex;
    let mut delegate = crate::testutil::live_row("9999-0", "work");
    delegate.is_delegate = true;
    let mut other_delegate = crate::testutil::live_row("9999-1", "work");
    other_delegate.is_delegate = true;

    let tally = LiveTally::of([
        crate::testutil::live_row("4242-0", "work"),
        crate::testutil::live_row("4242-1", "work"),
        codex,
        delegate,
        other_delegate,
    ]);

    assert_eq!(tally.total(), Some(5), "total keeps counting every row");
    assert_eq!(tally.own(), Some(3), "own is the non-delegate rows");
    assert_eq!(tally.delegates(), Some(2), "delegates is the marked rows");
    assert_eq!(
        tally.member(&crate::profile::ProfileName::from("work")),
        MemberSessions {
            sessions: 5,
            delegates: 2,
            following: 4,
            last_swap_at: None,
        },
        "the per-account slice splits the delegates out of its own sessions"
    );
}

/// An unread tally gives neither part a reading — the split chip's `—` form.
#[test]
fn an_unread_tally_reads_neither_part() {
    assert_eq!(LiveTally::unread().own(), None);
    assert_eq!(LiveTally::unread().delegates(), None);
}

/// The fd closing IS the release, which is what makes this survive SIGKILL: a
/// bare session runs no clauth code and has no teardown path to unregister from.
#[test]
fn releasing_a_bare_marker_stops_it_counting() {
    let _home = HomeSandbox::new();
    let config = config_with(vec![oauth_profile("work", "rt-work")], "work");
    write_linked_credentials("rt-work");

    let bare = crate::runtime::register_bare_session().expect("hold a bare marker");
    assert_eq!(
        LiveTally::collect(&config)
            .member(&crate::profile::ProfileName::from("work"))
            .sessions,
        1,
        "positive control: the marker counts while it is held"
    );

    drop(bare);

    // The liveness probe is fail-ALIVE (any `try_lock` I/O error reads as alive),
    // so one transient error under a parallel suite can inflate a single reading;
    // only a persistently-live reading is a regression. Same hardening as
    // `runtime`'s `has_live_session_true_when_any_session_alive`.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let settled = loop {
        if LiveTally::collect(&config)
            .member(&crate::profile::ProfileName::from("work"))
            .sessions
            == 0
        {
            break true;
        }
        if std::time::Instant::now() >= deadline {
            break false;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    };
    assert!(settled, "a released bare marker must stop counting");
}

/// The tally is read by a TUI that may itself be running inside a `clauth start`
/// session, where `CLAUDE_CONFIG_DIR` names its own runtime tree. That env
/// describes the READER, so letting it reach the attribution claims every bare
/// `claude` on the box for the reader's profile. Pinning the resolver in
/// isolation is not enough — this pins which one the fold calls.
#[test]
fn bare_attribution_ignores_the_readers_own_config_dir() {
    let home = HomeSandbox::new();
    let config = config_with(
        vec![
            oauth_profile("work", "rt-work"),
            oauth_profile("spare", "rt-spare"),
        ],
        "work",
    );
    write_linked_credentials("rt-work");
    let reader_runtime = home
        .home()
        .join(".clauth")
        .join("profiles")
        .join("spare")
        .join("runtime-4242-0");
    let _config_dir = crate::testutil::ConfigDirSandbox::new(&home, &reader_runtime);

    let _bare = crate::runtime::register_bare_session().expect("hold a bare marker");

    let tally = LiveTally::collect(&config);
    assert_eq!(
        tally
            .member(&crate::profile::ProfileName::from("work"))
            .sessions,
        1,
        "the bare session belongs to the account the global link resolves to"
    );
    assert_eq!(
        tally.member(&crate::profile::ProfileName::from("spare")),
        MemberSessions::default(),
        "the READER's own runtime profile hosts nothing"
    );
}

// ── `total`: a count the tally never read is no count ────────────────────────
//
// The header's counter chip renders `total`; a `0` there for a registry or a
// bare-marker dir that exists but cannot be listed would be a figure clauth did
// not read. An ABSENT dir is a real reading: nothing ever registered.

/// Put a regular file where a dir clauth lists should be: `read_dir` then fails
/// with something other than `NotFound` (ENOTDIR on unix).
#[cfg(unix)]
fn block_dir(dir: &Path) {
    std::fs::create_dir_all(dir.parent().expect("the blocked dir has a parent"))
        .expect("create the parent");
    std::fs::write(dir, b"").expect("write the blocking file");
}

#[test]
fn an_absent_registry_and_bare_dir_read_as_zero() {
    let _home = HomeSandbox::new();
    assert_eq!(
        LiveTally::collect(&config_with(vec![], "work")).total(),
        Some(0)
    );
}

#[cfg(unix)]
#[test]
fn an_unlistable_registry_reads_as_no_total() {
    let _home = HomeSandbox::new();
    block_dir(&registry_dir().expect("registry dir"));
    assert_eq!(
        LiveTally::collect(&config_with(vec![], "work")).total(),
        None
    );
}

/// A readable registry holding a live row does not rescue the total when the
/// bare-marker dir cannot be listed: `Some(1)` would still claim a reading of
/// the bare half.
#[cfg(unix)]
#[test]
fn an_unlistable_bare_dir_reads_as_no_total_even_beside_live_rows() {
    let _home = HomeSandbox::new();
    let live = row("4242-0", "work");
    register(&live).expect("register the live row");
    let _marker = crate::runtime::hold_session_row_marker(
        &crate::profile::ProfileName::from("work"),
        false,
        "4242-0",
    )
    .expect("hold the live session's marker");
    block_dir(
        &crate::profile::clauth_dir()
            .expect("clauth dir")
            .join("live_bare"),
    );

    let tally = LiveTally::collect(&config_with(vec![], "work"));
    assert_eq!(tally.total(), None);
    assert_eq!(
        tally
            .member(&crate::profile::ProfileName::from("work"))
            .sessions,
        1,
        "the per-account slice keeps the rows it did read"
    );
}

/// `list` keeps its best-effort contract for the decision leg and GC: an
/// unlistable registry is an empty sweep there, never an error.
#[cfg(unix)]
#[test]
fn list_stays_empty_on_an_unlistable_registry() {
    let _home = HomeSandbox::new();
    block_dir(&registry_dir().expect("registry dir"));
    assert!(list().is_empty());
}

/// A staging sibling a row write leaves (a crashed writer's whole copy of the
/// row) is no row: `list` reads the session once, and the registry still
/// reads whole, so every reader agrees on what a row is.
#[test]
fn a_staging_sibling_is_no_row_for_any_reader() {
    let _home = HomeSandbox::new();
    let live = row("4343-0", "work");
    register(&live).expect("register");
    let bytes = serde_json::to_vec(&live).expect("row json");
    std::fs::write(
        registry_dir()
            .expect("registry dir")
            .join(".4343-0.json.tmp.999.0"),
        bytes,
    )
    .expect("a staging sibling");

    assert_eq!(list().len(), 1, "the session is read once");
    assert!(registry_reads_whole(), "the staging sibling is skipped");
}

/// The rule one level below an unlistable dir: an ENTRY the OS fails to yield
/// leaves the tally's registry listing unknown, so the fleet total reads unread
/// rather than shorter. `list` and `live_rows` stay lenient on purpose — the
/// daemon decision leg sweeps every row it can read — so the SAME listing still
/// reads as known there.
#[test]
fn an_unreadable_registry_entry_leaves_the_tallys_listing_unknown() {
    let _home = HomeSandbox::new();
    let dir = tempfile::tempdir().expect("tempdir");
    let live = row("4242-0", "work");
    std::fs::write(
        dir.path().join("4242-0.json"),
        serde_json::to_vec(&live).expect("row json"),
    )
    .expect("write the row");

    let failing = || -> crate::runtime::DirListing {
        std::fs::read_dir(dir.path())
            .expect("list the fixture")
            .chain(std::iter::once(Err(std::io::Error::from(
                std::io::ErrorKind::InvalidData,
            ))))
            .collect()
    };

    assert_eq!(
        read_registry_tally(failing()),
        None,
        "an entry the OS fails to yield leaves the tally's listing unknown"
    );
    assert_eq!(
        read_registry(failing()),
        Some(vec![live.clone()]),
        "the decision leg's sweep keeps every row it could read"
    );
    assert_eq!(
        list_in(Some(failing())),
        vec![live],
        "list keeps every row it could read beside an entry the OS fails to yield"
    );
    // The fixture's row holds no marker flock, so `live_rows` filters it out:
    // a lenient read is an empty vec, a strict one would be `None`.
    assert_eq!(
        live_rows_in(Some(failing())),
        Some(Vec::new()),
        "live_rows reads the listing as known beside an entry the OS fails to yield"
    );
}

/// The same failure through the tally the header renders: its total reads
/// unread, because the tally's rows come from the strict registry reader and
/// never from `list`'s lenient one. Pinned here because no real dir yields an
/// entry error — the listing is handed in where [`LiveTally::collect_in`] would
/// read it.
#[test]
fn a_failing_registry_listing_leaves_the_tally_unread() {
    let _home = HomeSandbox::new();
    let config = config_with(vec![], "work");
    let jobs = crate::claude_jobs::job_trees();

    assert_eq!(
        LiveTally::collect_with_registry(&config, &jobs, Some(Vec::new())).total(),
        Some(0),
        "an empty listing is a reading, never an unknown"
    );
    assert_eq!(
        LiveTally::collect_with_registry(
            &config,
            &jobs,
            Some(vec![Err(std::io::Error::from(
                std::io::ErrorKind::InvalidData
            ))]),
        )
        .total(),
        None,
        "an entry the OS fails to yield leaves the fleet total unread"
    );
}

/// The tally keeps each registry row's id with the account the row runs on —
/// the row's `current_member`, else its start account — so a reader can tell
/// a session starting from one moving accounts; an unread tally names none,
/// so a failed read never reads as every session ending.
#[test]
fn the_tally_names_each_rows_member_and_an_unread_one_names_none() {
    let mut moved = row("4242-1", "a");
    moved.current_member = Some("b".to_string());
    let rows = [row("4242-0", "a"), moved];
    let want = std::collections::BTreeMap::from([
        ("4242-0".to_string(), "a".to_string()),
        ("4242-1".to_string(), "b".to_string()),
    ]);
    assert_eq!(LiveTally::of(rows.clone()).session_members(), Some(&want));
    assert_eq!(LiveTally::partly_read(rows).session_members(), None);
}
