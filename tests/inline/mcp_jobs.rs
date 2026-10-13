#![allow(clippy::unwrap_used, clippy::expect_used)]

//! Disk job-store coverage: atomic write/read roundtrip, id safety, and GC of
//! expired / orphaned state. Home-sandboxed so files land in a tempdir, never
//! the real `~/.clauth/jobs`.

use super::*;
use crate::testutil::HomeSandbox;

/// The running spec every test writes through, so one signature change lands in
/// one place.
///
/// It is the STREAMING shape, which is what `reserve_background_job` writes for
/// a default `delegate({background: true})`: no wall clock, an idle guard at the
/// default. The pair `(3600, Some(300))` this used to carry is one the producer
/// can no longer emit, so every test in the file inherited a run that cannot
/// exist.
/// `recorded_at` equals `started_at` here because that is what a job which
/// STARTED background carries — the reserve mints the record at the run's own
/// birth. A run handed off mid-flight is the shape where the two differ, and
/// the tests about that difference set it apart deliberately.
fn spec(job_id: &str, profile: &str, started_at: u64) -> RunningSpec {
    RunningSpec {
        job_id: job_id.to_string(),
        profile: profile.to_string(),
        started_at,
        recorded_at: started_at,
        timeout_secs: 0,
        endpoint: None,
        provider: None,
        isolated: false,
        cwd: None,
        spawned_by: None,
        host_session: None,
        host_pid: 0,
        idle_secs: Some(300),
        kind: RecordKind::Collectable,
        // The legacy shape: a record an older server wrote carries no owner, so
        // the silence window is its only corpse rule.
        owner_pid: 0,
        owner_started_at: 0,
    }
}

/// The same shape a BLOCKING run's spawn mints: identical bytes, different
/// spelling on disk.
fn live_spec(job_id: &str, profile: &str, started_at: u64) -> RunningSpec {
    RunningSpec {
        kind: RecordKind::Liveness,
        ..spec(job_id, profile, started_at)
    }
}

/// The other shape the producer emits: a caller-pinned `--output-format`, where
/// the idle leg is off and the wall clock is the only deadline.
fn pinned_format_spec(job_id: &str, profile: &str, started_at: u64) -> RunningSpec {
    RunningSpec {
        timeout_secs: 900,
        idle_secs: None,
        ..spec(job_id, profile, started_at)
    }
}

/// The isolation flag rides the mint like `endpoint` does: resolved once at
/// the reserve, carried through every heartbeat, and kept on the finalized
/// record, so the orphan arm can split shared from isolated off the corpse
/// itself — a heartbeat must not drop it back to the default.
#[test]
fn the_isolation_flag_rides_the_mint_through_heartbeats_and_the_finish() {
    let _home = HomeSandbox::new();
    let id = new_job_id(1000);
    let isolated = RunningSpec {
        isolated: true,
        ..spec(&id, "work", 1000)
    };
    write_running(&isolated).unwrap();
    assert!(read(&id).unwrap().isolated, "the mint stamps it");
    write_heartbeat_with_session(&isolated, 41_000, "mid-run", Some("sess-1")).unwrap();
    assert!(
        read(&id).unwrap().isolated,
        "a heartbeat rewrites the record and keeps it"
    );
    write_done_parts(
        &id,
        "work",
        1000,
        None,
        None,
        true,
        serde_json::json!({"result": "ok"}),
    )
    .unwrap();
    assert!(
        read(&id).unwrap().isolated,
        "the finalized record keeps it too"
    );

    let shared = new_job_id(2000);
    write_running(&spec(&shared, "work", 2000)).unwrap();
    assert!(
        !read(&shared).unwrap().isolated,
        "the default shape writes shared explicitly"
    );
}

/// The run's cwd, spawning account and host (session id + process) ride the
/// mint like the isolation flag: kept by every heartbeat and by the finish. A
/// spec without them writes none of the keys, the shape an older server's
/// record has on disk.
#[test]
fn the_cwd_spawner_and_host_ride_the_mint_through_heartbeats_and_the_finish() {
    let _home = HomeSandbox::new();
    let id = new_job_id(1000);
    let origin = RunningSpec {
        cwd: Some("/home/u/repos/app".to_string()),
        spawned_by: Some("kerry".to_string()),
        host_session: Some("host-sess-7".to_string()),
        host_pid: 4242,
        ..spec(&id, "work", 1000)
    };
    let carried = |id: &str| {
        let r = read(id).unwrap();
        (r.cwd, r.spawned_by, r.host_session, r.host_pid)
    };
    let want = (
        Some("/home/u/repos/app".to_string()),
        Some("kerry".to_string()),
        Some("host-sess-7".to_string()),
        4242,
    );
    write_running(&origin).unwrap();
    assert_eq!(carried(&id), want, "the mint stamps all four");
    write_heartbeat_with_session(&origin, 41_000, "mid-run", Some("sess-1")).unwrap();
    assert_eq!(
        carried(&id),
        want,
        "a heartbeat rewrites the record and keeps all four"
    );
    write_done(&origin, serde_json::json!({"result": "ok"})).unwrap();
    assert_eq!(carried(&id), want, "the finalized record keeps all four");

    let bare = new_job_id(2000);
    write_running(&spec(&bare, "work", 2000)).unwrap();
    let bytes = std::fs::read_to_string(jobs_dir().unwrap().join(format!("{bare}.json"))).unwrap();
    assert!(
        !bytes.contains("\"cwd\"")
            && !bytes.contains("\"spawned_by\"")
            && !bytes.contains("\"host_session\"")
            && !bytes.contains("\"host_pid\""),
        "an unknown origin writes no key: {bytes}"
    );
    assert_eq!(
        carried(&bare),
        (None, None, None, 0),
        "and reads back as unknown"
    );
}

#[test]
fn write_read_roundtrip_running_then_done() {
    let _home = HomeSandbox::new();
    let id = new_job_id(1000);
    write_running(&spec(&id, "work", 1000)).unwrap();

    let r = read(&id).expect("running record");
    assert_eq!(r.state, JobState::Running);
    assert_eq!(r.profile, "work");
    assert!(r.envelope.is_none());

    let env = serde_json::json!({ "is_error": false, "result": "ok" });
    write_done_parts(&id, "work", 1000, None, None, false, env.clone()).unwrap();
    let r = read(&id).expect("done record");
    assert_eq!(r.state, JobState::Done);
    assert_eq!(r.envelope, Some(env));

    // an atomic write leaves no .tmp behind.
    let tmp_left = std::fs::read_dir(jobs_dir().unwrap())
        .unwrap()
        .flatten()
        .any(|e| e.path().extension().and_then(|x| x.to_str()) == Some("tmp"));
    assert!(!tmp_left, "atomic write leaves no .tmp");

    remove(&id);
    assert!(read(&id).is_none(), "removed job is gone");
}

/// Row 2's demanded shape: a done record carries the run's session id off its
/// own envelope, so a collected completion is resumable — the listing and the
/// collect both name the handle the resume takes. An envelope without the key
/// keeps the legacy `None`.
#[test]
fn a_done_record_carries_the_envelopes_session_id() {
    let _home = HomeSandbox::new();
    let with = new_job_id(1_000);
    write_done_parts(
        &with,
        "work",
        1_000,
        None,
        None,
        false,
        serde_json::json!({"is_error": false, "result": "ok", "session_id": "sess-done-1"}),
    )
    .unwrap();
    assert_eq!(
        read(&with).unwrap().session_id.as_deref(),
        Some("sess-done-1"),
        "the done record carries the envelope's session id"
    );

    let without = new_job_id(2_000);
    write_done_parts(
        &without,
        "work",
        2_000,
        None,
        None,
        false,
        serde_json::json!({"is_error": false, "result": "ok"}),
    )
    .unwrap();
    assert!(
        read(&without).unwrap().session_id.is_none(),
        "an envelope without the key keeps the legacy None"
    );
}

#[test]
fn a_done_record_is_claimable_once_and_the_claim_evicts_it() {
    let _home = HomeSandbox::new();
    let id = new_job_id(1000);
    let env = serde_json::json!({ "is_error": false, "result": "ok" });
    write_done_parts(&id, "work", 1000, None, None, false, env.clone()).unwrap();

    let Claim::Owned(claimed) = claim(&id, Claimant::Monitor) else {
        panic!("the first claimant owns the record");
    };
    assert_eq!(claimed.state, JobState::Done);
    assert_eq!(claimed.envelope, Some(env));
    assert!(
        read(&id).is_none(),
        "the claim consumed the file: nothing is left to collect twice"
    );
    assert!(
        !job_path(&id, RecordKind::Collectable)
            .unwrap()
            .with_extension("json.claim")
            .exists(),
        "the claimed spelling is consumed too, not parked beside the record"
    );
    assert!(
        matches!(claim(&id, Claimant::Monitor), Claim::Lost),
        "a second claimant loses: the delivery is exactly once"
    );
}

/// The eviction rule `monitor`'s batch arm used to carry as a literal:
/// eviction follows the STORED id, so a caller-supplied id must never collect
/// a file another id's record owns. `claim` refuses the record and puts it
/// back, so the old guard survives the move into the shared primitive.
#[test]
fn claim_refuses_a_record_whose_self_report_disagrees_and_leaves_it_readable() {
    let _home = HomeSandbox::new();
    let path = job_path("d-claimed-0", RecordKind::Collectable).unwrap();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(
        path,
        serde_json::json!({
            "job_id": "d-other-0",
            "profile": "work",
            "state": "done",
            "started_at": 1,
            "done_at": 1,
        })
        .to_string(),
    )
    .unwrap();

    assert!(
        matches!(claim("d-claimed-0", Claimant::Monitor), Claim::Refused(_)),
        "a mismatched self-report is never claimed"
    );
    let restored = read("d-claimed-0").expect("the record is renamed back, not eaten");
    assert_eq!(
        restored.job_id, "d-other-0",
        "the stored id decides ownership, and the file still says so"
    );
    assert!(
        read("d-other-0").is_none(),
        "nothing was moved under the stored id's own path"
    );
}

/// The claimed spelling is a transient: invisible to every reader (its
/// extension is not `json`), and only the startup sweep's foreign-file arm
/// reaps a leftover from a claimant that died mid-claim.
#[test]
fn a_leftover_claimed_file_is_invisible_to_list_and_reaped_only_by_the_full_sweep() {
    let _home = HomeSandbox::new();
    let id = new_job_id(1000);
    write_done_parts(
        &id,
        "work",
        1000,
        None,
        None,
        false,
        serde_json::json!({"result": "ok"}),
    )
    .unwrap();
    let path = job_path(&id, RecordKind::Collectable)
        .unwrap()
        .with_extension("json.claim");
    std::fs::rename(job_path(&id, RecordKind::Collectable).unwrap(), &path).unwrap();

    assert!(
        list(crate::usage::now_ms())
            .iter()
            .all(|j| j.record.job_id != id),
        "a claimed file is no record to a reader"
    );
    gc_running_corpses(crate::usage::now_ms());
    assert!(
        path.exists(),
        "the narrow collect sweep never touches a claimed file"
    );
    gc(crate::usage::now_ms());
    assert!(
        !path.exists(),
        "the startup sweep's foreign-file arm reaps the leftover"
    );
}

/// A stale claimed spelling from a claimant that died mid-claim must not
/// block a later claim. The retry past it is Windows-only by construction
/// (a unix rename replaces the target, so the first attempt already
/// succeeds), which makes this pin inert on the linux leg and live on the
/// Windows one; it documents the contract either way.
#[test]
fn a_stale_claimed_spelling_does_not_block_the_claim() {
    let _home = HomeSandbox::new();
    let id = new_job_id(1000);
    write_done_parts(
        &id,
        "work",
        1000,
        None,
        None,
        false,
        serde_json::json!({"result": "ok"}),
    )
    .unwrap();
    let claimed = job_path(&id, RecordKind::Collectable)
        .unwrap()
        .with_extension("json.claim");
    std::fs::write(&claimed, "stale").unwrap();

    let Claim::Owned(record) = claim(&id, Claimant::Monitor) else {
        panic!("a stale claimed spelling must not block the claim");
    };
    assert_eq!(
        record.job_id, id,
        "the record behind the stale spelling is claimed"
    );
}

/// A running job file is written by the server that spawned it and read by a
/// possibly newer one, so every field added after the first release has to
/// default. Pinned against the real bytes an older server wrote, not a
/// hand-built `JobRecord`: a struct literal would compile against whatever the
/// fields are today and prove nothing about the wire. `read` swallows a parse
/// failure as `None`, which reaches the caller as `unknown job_id` on a job that
/// is running fine.
#[test]
fn a_job_file_from_an_older_server_still_parses() {
    let _home = HomeSandbox::new();
    let dir = jobs_dir().unwrap();
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("d-legacy-0.json"),
        br#"{"job_id":"d-legacy-0","profile":"work","state":"running","started_at":1000}"#,
    )
    .unwrap();

    let r = read("d-legacy-0").expect("a pre-slice-2 running file still parses");
    assert_eq!(r.state, JobState::Running);
    assert_eq!(r.timeout_secs, 0, "no deadline recorded by that server");
    assert_eq!(r.idle_secs, None);
    assert_eq!(r.last_output_at, 0);
    assert_eq!(r.tail, "");
    assert_eq!(r.done_at, 0, "no finish stamp either");
    assert_eq!(r.session_id, None, "and no session id");
    assert!(
        !r.isolated,
        "a record written before the isolation field existed reads as shared, \
         which is the delegate default"
    );
}

/// A heartbeat rewrites the SAME running record: the identity and whichever
/// deadline that run has survive it, and only the liveness fields move.
///
/// Both producible shapes, because each proves the half the other cannot — a
/// streaming record's absent wall is a serialized default, so only the
/// pinned-format one can show a real figure carried through.
#[test]
fn a_heartbeat_rewrites_the_running_record_in_place() {
    let _home = HomeSandbox::new();
    for (id, spec) in [
        ("d-beat-0", spec("d-beat-0", "work", 1000)),
        ("d-beat-1", pinned_format_spec("d-beat-1", "work", 1000)),
    ] {
        write_running(&spec).unwrap();
        let fresh = read(id).expect("running record");
        assert_eq!(fresh.last_output_at, 0, "nothing has arrived yet");
        assert_eq!(fresh.tail, "");

        // Epoch ms, the same anchor `started_at` uses — a run-relative stamp
        // would silently disagree with it by the acquire+spawn latency.
        write_heartbeat(&spec, 41_000, "moving to the fallback tests").unwrap();
        let beaten = read(id).expect("heartbeat record");
        assert_eq!(beaten.state, JobState::Running, "still the same job");
        assert_eq!(beaten.job_id, id);
        assert_eq!(beaten.profile, "work");
        assert_eq!(beaten.started_at, 1000);
        assert_eq!(beaten.timeout_secs, spec.timeout_secs);
        assert_eq!(beaten.idle_secs, spec.idle_secs);
        assert_eq!(beaten.last_output_at, 41_000);
        assert_eq!(beaten.tail, "moving to the fallback tests");
        assert!(beaten.envelope.is_none());
    }
}

/// The structural guarantee behind the liveness spelling, asserted as BEHAVIOUR
/// rather than as a comment: no id a reader accepts can resolve a liveness
/// record, so `monitor` cannot collect a result whose blocking caller is still
/// waiting for it.
///
/// Two halves, and both are needed. The id a liveness file is written under
/// resolves only to the COLLECTABLE spelling, which is a different file; and the
/// only string that would spell the liveness path is refused by the charset
/// gate every reader filters through first. Drop either half — let
/// [`is_safe_job_id`] admit `.`, or point the `Liveness` arm of `job_path` at
/// `{id}.json` — and one of these reds.
#[test]
fn no_id_a_reader_accepts_can_resolve_a_liveness_record() {
    let _home = HomeSandbox::new();
    let id = new_job_id(1000);
    write_running(&live_spec(&id, "work", 1000)).unwrap();

    // Fixture control: the file really is on disk, under the other spelling.
    let dir = jobs_dir().unwrap();
    assert!(
        dir.join(format!("{id}.live.json")).exists(),
        "a liveness record lands under its own name: {:?}",
        std::fs::read_dir(&dir).unwrap().flatten().count(),
    );
    assert!(
        !dir.join(format!("{id}.json")).exists(),
        "and never under the collectable one",
    );

    assert!(
        read(&id).is_none(),
        "the run's own id resolves to the collectable file, which does not exist",
    );
    let naming_it = format!("{id}.live");
    assert!(
        !is_safe_job_id(&naming_it),
        "and the one id whose `{{id}}.json` join would land on it is refused: {naming_it}",
    );
}

/// The crossing: one run keeps ONE identity, and only the spelling moves.
///
/// A fresh mint here would leave two ids for one run — the one its own
/// heartbeats already carry, and the one the caller is handed — so a record
/// collected under the second would have been heartbeat into under the first.
#[test]
fn promote_moves_the_spelling_and_keeps_the_id() {
    let _home = HomeSandbox::new();
    let id = new_job_id(1000);
    write_heartbeat(&live_spec(&id, "work", 1000), 41_000, "half way").unwrap();

    let collectable = RunningSpec {
        kind: RecordKind::Collectable,
        ..live_spec(&id, "work", 1000)
    };
    promote(&collectable).unwrap();

    let record = read(&id).expect("the run's own id now resolves");
    assert_eq!(record.job_id, id, "the id did not change");
    assert_eq!(
        record.tail, "half way",
        "and the record carried its heartbeats across rather than starting over",
    );
    let dir = jobs_dir().unwrap();
    assert!(
        !dir.join(format!("{id}.live.json")).exists(),
        "the liveness spelling is gone, so nothing is listed twice",
    );

    // The other arm: nothing to rename, because the liveness write never landed
    // or a finish removed the file first. The record still has to exist for the
    // run to be collectable at all.
    let fresh = new_job_id(2000);
    let spec = RunningSpec {
        kind: RecordKind::Collectable,
        ..spec(&fresh, "work", 2000)
    };
    promote(&spec).unwrap();
    assert!(
        read(&fresh).is_some(),
        "a promote with no source still leaves a collectable record",
    );
}

/// A blocking run's record rides the same sweep as any other: the extension is
/// still `.json`, so the silence rule reaches it with no arm of its own. The
/// outcome differs — a silent one is converted to a tombstone, not reaped —
/// which the conversion test below pins.
#[test]
fn a_liveness_record_is_swept_on_the_same_rules_as_any_other_running_one() {
    let _home = HomeSandbox::new();
    let now = 10_000_000_000u64;
    let ancient = now - 10 * RUNNING_TTL_MS;

    write_heartbeat(
        &live_spec("d-live-talking", "p", ancient),
        now - 1000,
        "alive",
    )
    .unwrap();
    write_running(&live_spec("d-live-silent", "p", ancient)).unwrap();

    gc_running_corpses(now);

    let dir = jobs_dir().unwrap();
    assert!(
        dir.join("d-live-talking.live.json").exists(),
        "a blocking run that is still talking survives, whatever its age",
    );
    assert!(
        !dir.join("d-live-silent.live.json").exists(),
        "a silent one's liveness spelling is gone",
    );
    assert!(
        read("d-live-silent").is_some_and(|r| r.crashed),
        "and the collectable spelling holds the converted tombstone",
    );
}

/// The sweep's conversion, driven through the real producer: a silent blocking
/// run's liveness record, written by `write_heartbeat_with_session` the way the
/// streaming reader writes it, becomes a `Done` tombstone that keeps the handle
/// and the isolation flag and invents no envelope. Seeding post-conversion bytes
/// would leave the conversion itself untested.
#[test]
fn the_sweep_converts_a_silent_liveness_record_into_a_tombstone() {
    let _home = HomeSandbox::new();
    let now = 10_000_000_000u64;
    let ancient = now - 10 * RUNNING_TTL_MS;
    let id = new_job_id(ancient);
    let spec = RunningSpec {
        kind: RecordKind::Liveness,
        isolated: true,
        ..spec(&id, "work", ancient)
    };
    write_heartbeat_with_session(&spec, 0, "", Some("sess-tomb-1")).unwrap();

    gc_running_corpses(now);

    let converted = read(&id).expect("the collectable spelling holds the tombstone");
    assert_eq!(
        converted.state,
        JobState::Done,
        "the tombstone is a done record"
    );
    assert!(converted.crashed, "it marks the crash");
    assert_eq!(
        converted.session_id.as_deref(),
        Some("sess-tomb-1"),
        "the resume handle survives"
    );
    assert!(converted.isolated, "the isolation flag survives");
    assert!(
        converted.envelope.is_none(),
        "no envelope is invented for a crash"
    );
    assert!(
        !jobs_dir().unwrap().join(format!("{id}.live.json")).exists(),
        "the liveness spelling is gone",
    );
}

/// The conversion writes to the collectable spelling without reading it, so it
/// must never overwrite a record that already carries an envelope: a finish
/// whose liveness leftover is still on disk keeps its result, and only the
/// stale liveness spelling is dropped.
#[test]
fn the_conversion_never_overwrites_a_finished_result() {
    let _home = HomeSandbox::new();
    let now = 10_000_000_000u64;
    let ancient = now - 10 * RUNNING_TTL_MS;
    let id = new_job_id(ancient);
    write_heartbeat_with_session(
        &RunningSpec {
            kind: RecordKind::Liveness,
            ..spec(&id, "work", ancient)
        },
        0,
        "",
        Some("sess-stale-1"),
    )
    .unwrap();
    write_done_parts(
        &id,
        "work",
        ancient,
        None,
        None,
        false,
        serde_json::json!({"result": "kept"}),
    )
    .unwrap();

    gc_running_corpses(now);

    let kept = read(&id).expect("the finished result survives the sweep");
    assert_eq!(
        kept.envelope,
        Some(serde_json::json!({"result": "kept"})),
        "the envelope is not overwritten"
    );
    assert!(!kept.crashed, "the conversion did not clobber the finish");
    assert!(
        !jobs_dir().unwrap().join(format!("{id}.live.json")).exists(),
        "the stale liveness spelling is dropped",
    );
}

/// A tombstone is an orphan, never a done record: `phase()` reads the `crashed`
/// flag before the generic `Done` arm, or `clauth jobs`, `monitor`'s listing and
/// the TUI all read a crashed run as a collectable `done`.
#[test]
fn a_tombstone_reads_orphaned_not_done() {
    let _home = HomeSandbox::new();
    let now = 10_000_000_000u64;
    let ancient = now - 10 * RUNNING_TTL_MS;
    let id = new_job_id(ancient);
    write_heartbeat_with_session(
        &RunningSpec {
            kind: RecordKind::Liveness,
            ..spec(&id, "work", ancient)
        },
        0,
        "",
        Some("sess-orph-1"),
    )
    .unwrap();
    gc_running_corpses(now);

    let row = list(now)
        .into_iter()
        .find(|j| j.record.job_id == id)
        .expect("the tombstone is listed");
    assert_eq!(
        row.phase(),
        JobPhase::Orphaned,
        "a crashed tombstone is an orphan"
    );
    assert_eq!(row.phase().label(), "orphaned");
    assert!(
        !row.phase().is_collectable(),
        "no result waits in a tombstone"
    );
}

#[test]
fn unknown_job_reads_none() {
    let _home = HomeSandbox::new();
    assert!(read("d-1-999").is_none());
}

// ── server owner marker + widened corpse rule ────────────────────────────────

/// The owner marker is the signal a dead server leaves behind: a flock released
/// by the kernel on ANY death, SIGKILL included, so a later server reads a
/// killed owner as dead with no teardown path to run. Holding it is what makes
/// a record minted by this process read live; dropping the guard is the
/// in-process stand-in for killing the spawning session.
#[test]
fn the_server_marker_releases_when_its_holder_drops() {
    let _home = HomeSandbox::new();
    let pid = std::process::id();
    assert!(
        !owner_is_live(pid),
        "no marker is held yet: the owner reads dead"
    );
    assert!(
        !owner_is_live(42_424),
        "a pid that never held a marker reads dead"
    );
    let guard = hold_server_marker().expect("hold the marker");
    assert!(owner_is_live(pid), "a held marker reads live");
    drop(guard);
    assert!(
        !owner_is_live(pid),
        "the flock drops with the holder: killing the spawning session \
         releases its rows' owners"
    );
}

/// A `running` record whose owner marker is released is a corpse AT THE NEXT
/// READ, never only after the 24h+600 s silence window: the narrow sweep a
/// `monitor` collect runs reaps it, and the listing classifies it the same way
/// the sweep would reap it.
#[test]
fn a_record_whose_owner_is_gone_reads_as_a_corpse_and_the_narrow_sweep_reaps_it() {
    let _home = HomeSandbox::new();
    let now = 10_000_000_000u64;
    let id = new_job_id(now);
    write_running(&RunningSpec {
        owner_pid: 42_424,
        ..spec(&id, "work", now)
    })
    .unwrap();

    let row = list(now)
        .into_iter()
        .find(|j| j.record.job_id == id)
        .expect("the record is listed");
    assert_eq!(
        row.liveness,
        JobLiveness::Corpse,
        "a fresh record whose server is gone is already a corpse, \
         not a running row"
    );
    assert_eq!(row.phase(), JobPhase::Orphaned);

    gc_running_corpses(now);
    assert!(
        read(&id).is_none(),
        "the narrow sweep reaps the record a dead server left behind"
    );
}

/// The verify line's shape: a record minted by a live server (its marker held)
/// lists as running; the moment the marker drops — the spawning session killed —
/// the next listing reads it orphaned, at most one poll after the owner's death.
#[test]
fn a_record_owned_by_a_live_server_reads_live_until_its_marker_drops() {
    let _home = HomeSandbox::new();
    let now = 10_000_000_000u64;
    let id = new_job_id(now);
    let pid = std::process::id();
    let _guard = hold_server_marker().expect("hold the marker");
    write_running(&RunningSpec {
        owner_pid: pid,
        owner_started_at: server_started_at(),
        ..spec(&id, "work", now)
    })
    .unwrap();

    let live = list(now)
        .into_iter()
        .find(|j| j.record.job_id == id)
        .expect("the record is listed");
    assert_eq!(
        live.phase(),
        JobPhase::Running,
        "a record owned by a live server lists as running"
    );

    drop(_guard);
    let dead = list(now)
        .into_iter()
        .find(|j| j.record.job_id == id)
        .expect("the record is still listed");
    assert_eq!(
        dead.phase(),
        JobPhase::Orphaned,
        "one poll after the owner's death the row is a dead state"
    );
}

/// A record an older server wrote carries no owner and keeps the silence-only
/// rule: fresh reads live, silence past the window reads a corpse. The owner
/// check must never widen onto a record it cannot judge.
#[test]
fn an_ownerless_legacy_record_keeps_the_silence_rule() {
    let _home = HomeSandbox::new();
    let now = 10_000_000_000u64;
    let fresh = new_job_id(now);
    write_running(&spec(&fresh, "work", now)).unwrap();
    let ancient = now - RUNNING_TTL_MS - 1;
    let old = new_job_id(ancient);
    write_running(&spec(&old, "work", ancient)).unwrap();

    assert_eq!(
        list(now)
            .into_iter()
            .find(|j| j.record.job_id == fresh)
            .expect("listed")
            .phase(),
        JobPhase::Running,
        "a fresh ownerless record reads live"
    );
    assert_eq!(
        list(now)
            .into_iter()
            .find(|j| j.record.job_id == old)
            .expect("listed")
            .phase(),
        JobPhase::Orphaned,
        "silence past the window still reaps an ownerless record"
    );
}

/// The pid-reuse corner: a record minted by a DEAD server whose pid this
/// process now holds must not read as this server's. The owner start stamp is
/// what tells the two epochs apart — the pid is ours, the stamp is not — so
/// the record is a corpse even though the flock under this pid is held.
#[test]
fn a_record_from_a_dead_epoch_of_a_reused_pid_is_a_corpse() {
    let _home = HomeSandbox::new();
    let now = 10_000_000_000u64;
    let id = new_job_id(now);
    let _guard = hold_server_marker().expect("hold the marker");
    write_running(&RunningSpec {
        owner_pid: std::process::id(),
        owner_started_at: 1,
        ..spec(&id, "work", now)
    })
    .unwrap();

    let row = list(now)
        .into_iter()
        .find(|j| j.record.job_id == id)
        .expect("the record is listed");
    assert_eq!(
        row.phase(),
        JobPhase::Orphaned,
        "a dead epoch of this pid is a corpse, never a self-owned live run"
    );

    gc_running_corpses(now);
    assert!(
        read(&id).is_none(),
        "the narrow sweep reaps the dead epoch's record"
    );
}

/// The partition the docs state: silence is the OWNERLESS rule. An owned
/// record whose server is alive is never a corpse however silent it sits —
/// its marker is the whole verdict — so the two legs cannot disagree about a
/// live run.
#[test]
fn an_owned_live_record_silent_past_the_window_is_not_a_corpse() {
    let _home = HomeSandbox::new();
    let now = 10_000_000_000u64;
    let id = new_job_id(now - 10 * RUNNING_TTL_MS);
    let pid = std::process::id();
    let _guard = hold_server_marker().expect("hold the marker");
    write_running(&RunningSpec {
        owner_pid: pid,
        owner_started_at: server_started_at(),
        ..spec(&id, "work", now - 10 * RUNNING_TTL_MS)
    })
    .unwrap();

    assert_eq!(
        list(now)
            .into_iter()
            .find(|j| j.record.job_id == id)
            .expect("listed")
            .phase(),
        JobPhase::Running,
        "a live owner's record survives the silence window: the marker is the verdict"
    );

    drop(_guard);
    assert_eq!(
        list(now)
            .into_iter()
            .find(|j| j.record.job_id == id)
            .expect("listed")
            .phase(),
        JobPhase::Orphaned,
        "the moment the owner dies the same record is a corpse"
    );
}

/// A blocking run's liveness record whose owner died is CONVERTED, not reaped:
/// the tombstone keeps the resume handle on the collectable spelling, the same
/// arm the silent conversion already uses — the owner marker just makes it fire
/// at the owner's death instead of a day later.
#[test]
fn the_liveness_record_of_a_dead_owner_is_tombstoned_not_reaped() {
    let _home = HomeSandbox::new();
    let now = 10_000_000_000u64;
    let id = new_job_id(now);
    write_heartbeat_with_session(
        &RunningSpec {
            kind: RecordKind::Liveness,
            owner_pid: 42_424,
            ..spec(&id, "work", now)
        },
        0,
        "",
        Some("sess-owner-1"),
    )
    .unwrap();

    gc_running_corpses(now);

    let tomb = read(&id).expect("the collectable spelling holds the tombstone");
    assert!(tomb.crashed, "it marks the crash");
    assert_eq!(
        tomb.session_id.as_deref(),
        Some("sess-owner-1"),
        "the resume handle survives the owner's death"
    );
    assert!(
        !jobs_dir().unwrap().join(format!("{id}.live.json")).exists(),
        "the liveness spelling is gone"
    );
}

// ── delivery ledger ──────────────────────────────────────────────────────────

/// A claimed record leaves a ledger behind naming WHO delivered it and WHEN,
/// so a later `monitor` naming the id can answer with the fact instead of the
/// hedged unknown copy. The ledger rides the claim — the one place every
/// delivery path already serializes on.
#[test]
fn the_delivery_ledger_names_who_delivered_and_when() {
    let _home = HomeSandbox::new();
    let id = new_job_id(1_000);
    let env = serde_json::json!({ "is_error": false, "result": "ok" });
    write_done_parts(&id, "work", 1_000, None, None, false, env).unwrap();
    let before = crate::usage::now_ms();

    let Claim::Owned(_) = claim(&id, Claimant::Hook) else {
        panic!("the hook owns the delivery");
    };
    let ledger = delivery_ledger(&id).expect("the claim left a ledger");
    assert_eq!(ledger.by, "hook", "the ledger names the claimant");
    assert_eq!(ledger.job_id, id);
    assert!(
        ledger.at >= before && ledger.at <= crate::usage::now_ms(),
        "the ledger stamps the delivery instant"
    );

    let dir = jobs_dir().unwrap();
    assert!(
        dir.join(format!("{id}.json.delivered")).exists(),
        "the ledger file survives the claim's own cleanup"
    );
    assert_eq!(
        list(crate::usage::now_ms())
            .iter()
            .filter(|j| j.record.job_id == id)
            .count(),
        0,
        "the ledger is invisible to the listing"
    );
}

/// The ledger is retained only as long as the unknown answer matters: the full
/// startup sweep reaps one past the done TTL and keeps a fresh one — the
/// foreign-file arm must not treat it as a stray.
#[test]
fn the_full_sweep_keeps_a_fresh_ledger_and_reaps_a_stale_one() {
    let _home = HomeSandbox::new();
    let now = 10_000_000_000u64;
    let fresh = new_job_id(now);
    let stale = new_job_id(now - DONE_TTL_MS - 1);
    std::fs::create_dir_all(jobs_dir().unwrap()).unwrap();
    for (id, at) in [
        (fresh.as_str(), now),
        (stale.as_str(), now - DONE_TTL_MS - 1),
    ] {
        std::fs::write(
            ledger_path(id).unwrap(),
            serde_json::json!({
                "job_id": id,
                "by": "hook",
                "at": at,
                "profile": "work",
            })
            .to_string(),
        )
        .unwrap();
    }

    gc(now);

    assert!(
        ledger_path(&fresh).unwrap().exists(),
        "a fresh ledger survives the startup sweep"
    );
    assert!(
        !ledger_path(&stale).unwrap().exists(),
        "a stale ledger is reaped with the same TTL the unknown answer keeps"
    );
}

#[test]
fn job_id_safety_rejects_traversal_and_separators() {
    assert!(is_safe_job_id("d-123-4"));
    assert!(is_safe_job_id("abc_DEF-9"));
    assert!(!is_safe_job_id(""));
    assert!(!is_safe_job_id("../escape"));
    assert!(!is_safe_job_id("a/b"));
    assert!(!is_safe_job_id("a.json"));
    assert!(!is_safe_job_id(&"x".repeat(200)));
}

#[test]
fn new_job_id_is_unique_and_safe() {
    let a = new_job_id(5);
    let b = new_job_id(5);
    assert_ne!(a, b, "same-ms ids differ via the counter");
    assert!(is_safe_job_id(&a) && is_safe_job_id(&b));
}

/// `base36`'s buffer is sized by an argument no wall-clock stamp can ever
/// exercise, so passing the widest input at all is what reds an under-sized one
/// — by panicking before any assertion here runs, on the index in release and
/// on the counter's own underflow in debug. An OVER-sized buffer is invisible to
/// this test and costs nothing but stack, so nothing below claims to catch it:
/// the two assertions pin the encoding, not the sizing.
#[test]
fn base36_spans_its_whole_domain() {
    assert_eq!(base36(0), "0", "zero is a digit, never the empty string");
    let widest = base36(u64::MAX);
    assert_eq!(widest.len(), 13, "u64::MAX spells 13 base-36 digits");
    assert_eq!(u64::from_str_radix(&widest, 36), Ok(u64::MAX));
}

/// Write a `done` file with an explicit `done_at`, as raw bytes: `write_done`
/// stamps the real clock, and the retention rule under test is about a stamp a
/// test has to choose. Omitting `done_at` writes the pre-`done_at` shape.
fn seed_done_at(job_id: &str, started_at: u64, done_at: Option<u64>) {
    let dir = jobs_dir().unwrap();
    std::fs::create_dir_all(&dir).unwrap();
    let mut record = serde_json::json!({
        "job_id": job_id,
        "profile": "p",
        "state": "done",
        "started_at": started_at,
        "envelope": { "result": "kept" },
    });
    if let Some(at) = done_at {
        record["done_at"] = serde_json::json!(at);
    }
    std::fs::write(
        dir.join(format!("{job_id}.json")),
        serde_json::to_vec(&record).unwrap(),
    )
    .unwrap();
}

#[test]
fn gc_reaps_expired_running_and_done_keeps_fresh() {
    let _home = HomeSandbox::new();
    // A synthetic clock, high enough that every TTL this file subtracts from it
    // stays positive; entries seeded at `now` read as fresh against it.
    let now = 10_000_000_000u64;

    seed_done_at("d-fresh-done", now, Some(now));
    write_running(&spec("d-fresh-run", "p", now)).unwrap();
    seed_done_at(
        "d-old-done",
        now - DONE_TTL_MS - 1,
        Some(now - DONE_TTL_MS - 1),
    );
    write_running(&spec("d-old-run", "p", now - RUNNING_TTL_MS - 1)).unwrap();

    gc(now);

    assert!(read("d-fresh-done").is_some());
    assert!(read("d-fresh-run").is_some());
    assert!(read("d-old-done").is_none(), "expired done reaped");
    assert!(read("d-old-run").is_none(), "orphaned running reaped");
}

/// The Done TTL retains a file for a day after it FINISHES, which is what its
/// own doc promises a poller returning after a reboot. Measured from the mint
/// instead, any delegate that ran for over a day is already expired the instant
/// it finalizes, and the next sweep destroys the salvage envelope.
#[test]
fn the_done_ttl_measures_from_the_finish_not_the_mint() {
    let _home = HomeSandbox::new();
    let now = 10_000_000_000u64;

    // Minted two days ago, finished a moment ago: a run killed at its wall
    // clock, or any long delegate.
    seed_done_at("d-long-run", now - 2 * DONE_TTL_MS, Some(now - 1000));
    // Minted a moment ago, finished past the TTL — impossible in practice,
    // but it pins that the FINISH is what the rule reads.
    seed_done_at("d-stale-finish", now - 1000, Some(now - DONE_TTL_MS - 1));
    // No `done_at` at all: a file an older server wrote, which must keep
    // exactly the mint-anchored behaviour rather than becoming immortal.
    seed_done_at("d-legacy-done", now - DONE_TTL_MS - 1, None);
    seed_done_at("d-legacy-fresh", now - 1000, None);

    gc(now);

    assert!(
        read("d-long-run").is_some(),
        "a long run's envelope survives its own length"
    );
    assert!(read("d-stale-finish").is_none(), "a day past the finish");
    assert!(read("d-legacy-done").is_none(), "old file, old rule");
    assert!(read("d-legacy-fresh").is_some(), "old file, still fresh");
}

/// The two windows pinned in literal days, so a TTL that slips back under them
/// reds here rather than riding the constants the tests above express
/// themselves in. A day is the line this task draws: a record written by a
/// killed server still resolves a day later, and a done record expires at a
/// day, not an hour.
#[test]
fn the_windows_hold_for_a_day_each() {
    let _home = HomeSandbox::new();
    let now = 10_000_000_000u64;
    const DAY_MS: u64 = 24 * 60 * 60 * 1000;

    // The last thing a killed server wrote, exactly a day ago.
    write_heartbeat_with_session(
        &spec("d-day-run", "p", now - DAY_MS),
        now - DAY_MS,
        "last beat before the crash",
        Some("sess-day-1"),
    )
    .unwrap();
    // A done record a day old.
    seed_done_at("d-day-done", now - DAY_MS, Some(now - DAY_MS));

    gc(now);

    let run = read("d-day-run").expect("a crashed run's record still resolves a day later");
    assert_eq!(run.state, JobState::Running);
    assert_eq!(
        run.session_id.as_deref(),
        Some("sess-day-1"),
        "and carries the resume handle it captured",
    );
    let listed = list(now);
    let row = listed
        .iter()
        .find(|j| j.record.job_id == "d-day-run")
        .expect("the day-old record is listed");
    assert_eq!(
        row.liveness,
        JobLiveness::Running,
        "a day of silence is inside the window, so no reader may call it a corpse",
    );
    assert!(
        read("d-day-done").is_some(),
        "a day-old done record still resolves",
    );

    // Past the day, the done TTL expires it; the day-old survivors above are
    // the half of the pin a 1 h TTL would have failed.
    seed_done_at("d-stale-done", now - DAY_MS - 1, Some(now - DAY_MS - 1));
    gc(now);
    assert!(
        read("d-stale-done").is_none(),
        "a done record past the day is reaped"
    );
}

/// A streaming delegate has no wall clock, so "older than the max delegate
/// lifetime" bounds nothing any more: anchored on the mint, a run still healthy
/// past the TTL had its `running` file deleted under it — at startup AND by the
/// sweep every `monitor` collect runs — and the job then answered `unknown
/// job_id` while its child kept spending the account. What separates a corpse
/// from a long run is SILENCE, not age: a live background run rewrites this file
/// on every heartbeat and cannot go quiet for longer than its own idle guard
/// without being killed.
#[test]
fn a_job_still_talking_survives_the_corpse_sweep_however_old_it_is() {
    let _home = HomeSandbox::new();
    let now = 10_000_000_000u64;
    let ancient = now - 10 * RUNNING_TTL_MS;

    // Minted ten corpse windows ago, said something a second ago: alive.
    write_heartbeat(&spec("d-talking", "p", ancient), now - 1000, "still going").unwrap();
    // Same age, never heard from since the mint: a corpse.
    write_running(&spec("d-silent", "p", ancient)).unwrap();

    gc_running_corpses(now);

    assert!(
        read("d-talking").is_some(),
        "a heartbeat inside the window is liveness, whatever the run's total age"
    );
    assert!(
        read("d-silent").is_none(),
        "silence past the window is still a corpse"
    );

    // The startup sweep reads the same anchor, and it is the one that runs while
    // ANOTHER server's jobs are in flight.
    write_heartbeat(
        &spec("d-talking-2", "p", ancient),
        now - 1000,
        "still going",
    )
    .unwrap();
    gc(now);
    assert!(
        read("d-talking-2").is_some(),
        "startup GC must not reap a live run a sibling server is still driving"
    );
}

/// The collect path runs a NARROWER sweep than startup: it reaps only the
/// `running` corpses a dead server orphaned, which is the whole reason finding 6
/// wanted a sweep there. Reaping `done` before a read destroys the envelope the
/// call came for, and the `.tmp` sweep buys nothing at all.
#[test]
fn the_corpse_sweep_touches_only_orphaned_running_files() {
    let _home = HomeSandbox::new();
    let now = 10_000_000_000u64;

    seed_done_at(
        "d-expired-done",
        now - 2 * DONE_TTL_MS,
        Some(now - 2 * DONE_TTL_MS),
    );
    write_running(&spec("d-live-run", "p", now)).unwrap();
    write_running(&spec("d-corpse-run", "p", now - RUNNING_TTL_MS - 1)).unwrap();
    let dir = jobs_dir().unwrap();
    std::fs::write(dir.join("d-9-9.json.tmp"), b"partial").unwrap();

    gc_running_corpses(now);

    assert!(
        read("d-corpse-run").is_none(),
        "a dead server's file is a corpse"
    );
    assert!(read("d-live-run").is_some(), "a live job is untouched");
    assert!(
        read("d-expired-done").is_some(),
        "a collect must never destroy a result, whatever its age"
    );
    assert!(
        dir.join("d-9-9.json.tmp").exists(),
        "the tmp sweep is startup's job, not a reader's"
    );
}

#[test]
fn gc_sweeps_stray_tmp_files() {
    let _home = HomeSandbox::new();
    let dir = jobs_dir().unwrap();
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("d-1-0.json.tmp"), b"partial").unwrap();
    gc(0);
    assert!(!dir.join("d-1-0.json.tmp").exists(), "stray tmp swept");
}

/// A day of jobs can exceed [`MAX_RETAINED`] on a busy box, and no count cap may
/// evict a record its TTL still protects: the cap this store used to carry,
/// sorted on the same anchor the TTL reads, dropped a crashed run's record while
/// shorter, newer ones survived. The store is bounded by the two TTLs alone, so
/// a store over the cap keeps every fresh record.
#[test]
fn the_store_keeps_more_than_the_cap_while_the_ttls_protect_them() {
    let _home = HomeSandbox::new();
    let now = 10_000_000_000u64;
    let total = MAX_RETAINED + 5;
    for i in 0..total {
        // Both stamps rise with i, so low i are the oldest either way. The rule
        // under test is about stamps a test has to choose; `write_done` would
        // stamp every finish at the real clock.
        let age = total as u64 - i as u64;
        seed_done_at(&format!("d-day-{i}"), now - age, Some(now - age));
    }
    gc(now);

    assert_eq!(
        std::fs::read_dir(jobs_dir().unwrap())
            .unwrap()
            .flatten()
            .count(),
        total,
        "nothing is evicted by count while its TTL protects it",
    );
    assert!(
        read("d-day-0").is_some(),
        "the oldest fresh record survives a store over the cap",
    );
    assert!(
        read(&format!("d-day-{}", total - 1)).is_some(),
        "and so does the newest"
    );
}

/// The listing reads and NOTHING else. Not the Done TTL, not the `.tmp` sweep,
/// not the corpse reap — every one of them is somebody asking for a sweep by
/// name, and a reader that reaps what it came for is the defect this store has
/// shipped twice.
///
/// The fixture is deliberately made of records every destructive rule wants:
/// a `done` file a day past its TTL, a `running` corpse, a stray `.tmp`,
/// and a store past [`MAX_RETAINED`] records.
#[test]
fn the_listing_destroys_nothing_it_reads() {
    let _home = HomeSandbox::new();
    let now = 10_000_000_000u64;

    seed_done_at(
        "d-expired",
        now - 2 * DONE_TTL_MS,
        Some(now - 2 * DONE_TTL_MS),
    );
    write_running(&spec("d-corpse", "p", now - RUNNING_TTL_MS - 1)).unwrap();
    write_running(&live_spec("d-live-corpse", "p", now - RUNNING_TTL_MS - 1)).unwrap();
    let dir = jobs_dir().unwrap();
    std::fs::write(dir.join("d-partial.json.tmp"), b"partial").unwrap();
    std::fs::write(dir.join("d-garbage.json"), b"{ not json").unwrap();
    for i in 0..=MAX_RETAINED {
        seed_done_at(&format!("d-cap-{i}"), now - i as u64, Some(now - i as u64));
    }
    let names = || -> std::collections::BTreeSet<String> {
        std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect()
    };
    let before = names();
    // Read off the FIXTURE rather than off the listing, so a sweep inside `list`
    // reds the comparison below instead of short-circuiting on this line.
    assert!(
        before.len() > MAX_RETAINED,
        "fixture control: a store this size would show a sweep here",
    );

    let listed = list(now);

    assert_eq!(
        before,
        names(),
        "listing the store must leave every file exactly where it was",
    );
    assert!(
        !listed.iter().any(|j| j.record.job_id == "d-garbage"),
        "an unreadable file is skipped, never reported and never deleted",
    );
}

/// The listing classifies a corpse the way [`gc_running_corpses`] reaps one, and
/// the pin is absolute rather than relative: a rule that moved would have to
/// move BOTH sides, so naming which record lands where is what catches it.
#[test]
fn the_listing_calls_a_corpse_what_the_sweep_would_reap() {
    let _home = HomeSandbox::new();
    let now = 10_000_000_000u64;
    let ancient = now - 10 * RUNNING_TTL_MS;

    write_heartbeat(&spec("d-talking", "p", ancient), now - 1000, "alive").unwrap();
    write_running(&spec("d-silent", "p", ancient)).unwrap();
    seed_done_at("d-finished", now - 60_000, Some(now - 60_000));

    let by_id = |id: &str| {
        list(now)
            .into_iter()
            .find(|j| j.record.job_id == id)
            .unwrap_or_else(|| panic!("{id} listed"))
    };
    assert_eq!(by_id("d-talking").liveness, JobLiveness::Running);
    assert_eq!(by_id("d-silent").liveness, JobLiveness::Corpse);
    assert_eq!(by_id("d-finished").liveness, JobLiveness::Done);

    // And the sweep agrees about which one is dead, so the row a reader sees as
    // a corpse is the row the next collect removes.
    gc_running_corpses(now);
    assert!(read("d-talking").is_some());
    assert!(read("d-silent").is_none());
    assert!(read("d-finished").is_some());
}

/// Newest-mattering first, on the same stamp both retention rules read — so the
/// row a sweep would drop LAST is the row a reader sees FIRST. Sorting on the
/// mint instead puts a long run that just spoke below a short one that finished
/// an hour ago.
#[test]
fn the_listing_orders_on_the_retention_anchor_across_both_spellings() {
    let _home = HomeSandbox::new();
    let now = 10_000_000_000u64;

    // Oldest mint, freshest sign of life.
    write_heartbeat(
        &live_spec("d-long", "p", now - 3_600_000),
        now - 1_000,
        "still going",
    )
    .unwrap();
    // Newest mint, nothing since.
    write_running(&spec("d-quiet", "p", now - 300_000)).unwrap();
    // Finished in between.
    seed_done_at("d-old", now - 1_000, Some(now - 600_000));

    let listed = list(now);
    let order: Vec<&str> = listed.iter().map(|j| j.record.job_id.as_str()).collect();
    assert_eq!(
        order,
        vec!["d-long", "d-quiet", "d-old"],
        "ordered by when each record last mattered, never by when it was minted",
    );
    assert_eq!(
        listed[0].kind,
        RecordKind::Liveness,
        "and each row reports which spelling held it",
    );
    assert_eq!(listed[1].kind, RecordKind::Collectable);
    assert_eq!(
        listed[0].anchor,
        now - 1_000,
        "the anchor it sorted on rides along, so a reader dates a row from the \
         same stamp the store keeps it by",
    );
}

/// Two records sharing an anchor come back in ONE order, every call.
///
/// Without the `job_id` tiebreak a tie falls through to `read_dir`, which is
/// arbitrary and not stable across calls on an unchanged store: a fan-out whose
/// members land inside the same millisecond enumerated differently each time, so
/// a model diffing two `monitor` replies saw changes that had not happened and
/// an operator watching `clauth jobs` saw rows swap under a still store.
///
/// Six records at ONE anchor, written in an order that is neither the expected
/// output nor its reverse: at six, an accidental `read_dir` agreement is 1 in
/// 720, and the mutation run against this confirmed the red rather than assuming
/// it. The ids are same-width base-36 stamps with distinct counters, which is
/// what a real same-millisecond fan-out mints.
#[test]
fn records_sharing_an_anchor_are_ordered_by_id_not_by_readdir() {
    let _home = HomeSandbox::new();
    let now = crate::usage::now_ms();
    // One stamp, six counters. Written scrambled.
    for n in [3u64, 0, 5, 1, 4, 2] {
        write_running(&spec(&format!("d-msvr98yv-{n}"), "p", now - 60_000)).unwrap();
    }

    let listed: Vec<String> = list(now).into_iter().map(|j| j.record.job_id).collect();

    assert_eq!(
        listed,
        vec![
            "d-msvr98yv-5",
            "d-msvr98yv-4",
            "d-msvr98yv-3",
            "d-msvr98yv-2",
            "d-msvr98yv-1",
            "d-msvr98yv-0",
        ],
        "one anchor, one order — newest mint first",
    );

    // Stability is the property, so assert it ACROSS calls rather than inferring
    // it from a single one: an unstable order can satisfy any one permutation.
    for _ in 0..3 {
        let again: Vec<String> = list(now).into_iter().map(|j| j.record.job_id).collect();
        assert_eq!(
            again, listed,
            "the same store answers the same way each call"
        );
    }
}

/// The tiebreak orders a tie and nothing else: a fresher anchor still wins,
/// whatever the ids say.
///
/// Its own test because the one above cannot see this — every record there
/// shares an anchor, so a mutant that sorted by `job_id` ALONE would pass it.
#[test]
fn the_id_tiebreak_never_outranks_the_anchor() {
    let _home = HomeSandbox::new();
    let now = crate::usage::now_ms();
    // The id that sorts LAST descending, on the freshest anchor.
    write_running(&spec("d-aaaa-0", "p", now - 10_000)).unwrap();
    // The id that sorts FIRST descending, on the oldest.
    write_running(&spec("d-zzzz-9", "p", now - 600_000)).unwrap();

    let listed: Vec<String> = list(now).into_iter().map(|j| j.record.job_id).collect();

    assert_eq!(
        listed,
        vec!["d-aaaa-0", "d-zzzz-9"],
        "the anchor decides; the id only breaks a tie",
    );
}

/// A job file carries the delegate's prompt and the account's full response, and
/// the dir naming every background job is as readable as the files in it. Both
/// ride clauth's owner-only rule for `~/.clauth`.
#[cfg(unix)]
#[test]
fn job_files_and_dir_are_owner_only() {
    use std::os::unix::fs::PermissionsExt;

    let _home = HomeSandbox::new();
    let id = new_job_id(1000);
    write_done_parts(
        &id,
        "work",
        1000,
        None,
        None,
        false,
        serde_json::json!({"result": "secret output"}),
    )
    .unwrap();

    let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    let dir = jobs_dir().unwrap();
    assert_eq!(
        mode(&dir),
        0o700,
        "jobs dir mode should be 0o700, got {:#o}",
        mode(&dir)
    );
    let file = dir.join(format!("{id}.json"));
    assert_eq!(
        mode(&file),
        0o600,
        "job file mode should be 0o600, got {:#o}",
        mode(&file)
    );
}
