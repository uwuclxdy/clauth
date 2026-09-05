use super::*;
use std::sync::mpsc::channel;

use crate::testutil::HomeSandbox;

/// A row for a session that never ran, with every field pinned so the tests
/// assert exact values rather than "something was written".
fn row(session_id: &str, profile: &str) -> LiveSession {
    LiveSession {
        session_id: session_id.to_string(),
        start_profile: profile.to_string(),
        pid: 4242,
        started_at: 1_700_000_000_000,
        cwd: Some(PathBuf::from("/w/proj")),
        isolated: false,
        follows_chain: false,
        intended_member: None,
        chain_cursor: None,
        current_member: None,
        last_swap_at: None,
        launch_store: None,
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
/// that made the Plugin tab report one child as two.
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

fn oauth_profile(name: &str, refresh: &str) -> crate::profile::Profile {
    let mut profile = crate::testutil::blank_profile(&crate::profile::ProfileName::from(name));
    profile.credentials = Some(crate::profile::ClaudeCredentials {
        claude_ai_oauth: Some(crate::profile::OAuthToken {
            access_token: format!("at-{name}"),
            refresh_token: Some(refresh.to_string()),
            expires_at: None,
            scopes: None,
            subscription_type: None,
        }),
    });
    profile
}

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

/// What the credential link the bare `claude` reads actually holds.
fn write_linked_credentials(refresh: &str) {
    let dir = crate::profile::claude_dir().expect("claude dir");
    std::fs::create_dir_all(&dir).expect("mkdir .claude");
    std::fs::write(
        dir.join(".credentials.json"),
        format!(r#"{{"claudeAiOauth":{{"accessToken":"at","refreshToken":"{refresh}"}}}}"#),
    )
    .expect("write linked credentials");
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
