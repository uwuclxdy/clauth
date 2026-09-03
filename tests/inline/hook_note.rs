#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;

use crate::profile::ProfileName;
use crate::profile_cache::{USAGE_CACHE_FILE, write_profile_cache};
use crate::testutil::HomeSandbox;

/// A payload carrying only what these tests vary.
fn payload(event: &str, session: &str) -> Payload {
    Payload {
        event: event.to_string(),
        session_id: session.to_string(),
        agent_id: None,
        tool_name: None,
        source: None,
        transcript: None,
    }
}

/// A parent-scope `Task` (agent-spawn) fire — the one call the nudge gates on.
fn task_fire(session: &str) -> Payload {
    let mut fire = payload("PostToolUse", session);
    fire.tool_name = Some("Task".to_string());
    fire
}

/// A stamp set whose two halves move INDEPENDENTLY, so a test can pin which
/// input the gate actually watches. The old helper hardcoded the second half,
/// which meant no fixture could tell the two apart and deleting one of them
/// survived the whole suite.
fn watch(creds: u64, config: u64) -> Watch {
    Watch {
        creds: Some(Stamp {
            mtime: SystemTime::UNIX_EPOCH,
            len: creds,
        }),
        config,
    }
}

/// A stamp set serde cannot write: `SystemTime` before the epoch fails to
/// serialize, which is the cheapest way to force `store_record` to error.
fn unwritable_watch() -> Watch {
    Watch {
        creds: Some(Stamp {
            mtime: SystemTime::UNIX_EPOCH - std::time::Duration::from_secs(1),
            len: 1,
        }),
        config: 9,
    }
}

/// An attributed-or-not account reading taken now — the shape every resolve
/// returns, the stamp travelling with the answer.
fn reading(account: Option<&str>) -> Option<Reading> {
    Some(Reading {
        account: account.map(str::to_string),
        taken_at: SystemTime::now(),
    })
}

fn kerry() -> Option<Reading> {
    reading(Some("kerry"))
}

fn cld() -> Option<Reading> {
    reading(Some("cld"))
}

/// clauth cannot attribute the loaded credentials.
fn unknown() -> Option<Reading> {
    reading(None)
}

const SWITCHED: &str =
    "clauth note: the active profile for this session switched from `kerry` to `cld`.";

/// The shipped copy, byte for byte. All three spellings counted against
/// opus-4-8 via cloudify's `token-count.mjs` on their placeholder spellings —
/// `old`/`new`/`100` standing in for the names and figure, the `%` literal:
/// ``clauth note: session resumed under `new`; earlier turns ran under `old`.``
/// counts 25, ``clauth note: the active profile for this session switched from
/// `old` to `new`.`` counts 22, and ``clauth note: the active profile for this
/// session switched from `old` to `new`; its 5h window is 100% used.`` counts
/// 33. A reworded one is a re-count.
#[test]
fn both_note_spellings_render_the_shipped_copy() {
    assert_eq!(
        Note::Resumed {
            now: "DS4",
            before: "z.ai",
        }
        .render(),
        "clauth note: session resumed under `DS4`; earlier turns ran under `z.ai`.",
    );
    assert_eq!(
        Note::Switched {
            from: "kerry",
            to: "cld",
            used: None,
        }
        .render(),
        SWITCHED,
    );
    // The E4 clause (2026-08-28), byte for byte: the `%` is literal in the
    // copy, the figure a bare number.
    assert_eq!(
        Note::Switched {
            from: "kerry",
            to: "cld",
            used: Some(62.0),
        }
        .render(),
        "clauth note: the active profile for this session switched from `kerry` \
         to `cld`; its 5h window is 62% used.",
    );
}

/// Two events, so a hard-coded name cannot pass: the host routes the context by
/// this field, and all three registrations run one binary.
#[test]
fn the_envelope_echoes_the_event_that_produced_it() {
    for event in ["PostToolUse", "UserPromptSubmit", "SessionStart"] {
        assert_eq!(
            envelope(event, "note"),
            serde_json::json!({
                "hookSpecificOutput": {
                    "hookEventName": event,
                    "additionalContext": "note",
                }
            }),
        );
    }
}

/// The join produces one envelope: whatever earned the turn — one note or two
/// — joins into ONE `additionalContext`, so the rendered payload is one
/// parseable JSON document. Pinned here because the join is assertable
/// without stdout; `run()`'s single-`outln!` property is unpinned.
#[test]
fn two_earned_notes_join_into_one_envelope() {
    let joined = joined_envelope(
        "PostToolUse",
        &["first note".to_string(), "second note".to_string()],
    );
    assert_eq!(
        joined,
        serde_json::json!({
            "hookSpecificOutput": {
                "hookEventName": "PostToolUse",
                "additionalContext": "first note\n\nsecond note",
            }
        }),
    );
    serde_json::from_str::<serde_json::Value>(&joined.to_string())
        .expect("one JSON document, never two");
}

#[test]
fn the_first_fire_is_a_baseline_and_a_move_is_announced_once() {
    let _home = HomeSandbox::new();
    let fire = payload("PostToolUse", "conv-1");

    assert_eq!(
        note_for(&fire, &watch(1, 0), &kerry),
        None,
        "there are no earlier turns to correct on a first fire",
    );
    assert_eq!(
        note_for(&fire, &watch(2, 0), &cld).as_deref(),
        Some(SWITCHED),
    );
    assert_eq!(
        note_for(&fire, &watch(3, 0), &cld),
        None,
        "a fire on the account already told repeats nothing",
    );
}

#[test]
fn a_resume_under_another_account_names_the_earlier_turns() {
    let _home = HomeSandbox::new();
    note_for(
        &payload("UserPromptSubmit", "conv-2"),
        &watch(1, 0),
        &|| reading(Some("z.ai")),
    );

    let mut resumed = payload("SessionStart", "conv-2");
    resumed.source = Some("resume".to_string());

    assert_eq!(
        note_for(&resumed, &watch(2, 0), &|| reading(Some("DS4"))).as_deref(),
        Some("clauth note: session resumed under `DS4`; earlier turns ran under `z.ai`."),
    );
}

/// The record has to outlive the process that wrote it: a resume is exactly a
/// fresh process on the same conversation id.
#[test]
fn the_record_is_left_on_disk_for_the_next_process() {
    let _home = HomeSandbox::new();
    note_for(
        &payload("UserPromptSubmit", "conv-3"),
        &watch(1, 0),
        &|| reading(Some("z.ai")),
    );

    let stored =
        load_record(&record_path("conv-3", None).expect("record path")).expect("a record on disk");

    assert_eq!(stored.told.as_deref(), Some("z.ai"));
}

#[test]
fn the_stat_gate_skips_the_resolution_until_a_watched_input_moves() {
    let _home = HomeSandbox::new();
    let fire = payload("PostToolUse", "conv-4");
    let calls = std::cell::Cell::new(0_u32);
    let resolve = || {
        calls.set(calls.get() + 1);
        kerry()
    };

    note_for(&fire, &watch(1, 0), &resolve);
    assert_eq!(calls.get(), 1, "a first fire has nothing cached to gate on");

    note_for(&fire, &watch(1, 0), &resolve);
    assert_eq!(
        calls.get(),
        1,
        "an unmoved stamp must not reach the resolution",
    );

    note_for(&fire, &watch(2, 0), &resolve);
    assert_eq!(calls.get(), 2, "a moved stamp must reach it");
}

/// A single per-conversation flag would let whichever scope fires first consume
/// the note, leaving the other believing the old account.
#[test]
fn a_subagent_and_the_main_thread_each_hear_the_same_move() {
    let _home = HomeSandbox::new();
    let main = payload("UserPromptSubmit", "conv-5");
    note_for(&main, &watch(1, 0), &kerry);

    let mut sub = payload("PostToolUse", "conv-5");
    sub.agent_id = Some("a4a894a1be41b92bf".to_string());

    assert_eq!(
        note_for(&sub, &watch(2, 0), &cld).as_deref(),
        Some(SWITCHED),
        "the subagent inherits the conversation's baseline, so it hears the move",
    );
    assert_eq!(
        note_for(&main, &watch(2, 0), &cld).as_deref(),
        Some(SWITCHED),
        "and the main thread still hears it",
    );
}

/// Compaction drops injected context while the record would suppress a second
/// note, which would leave the conversation believing the old account.
#[test]
fn a_compaction_re_announces_the_note_it_dropped() {
    let _home = HomeSandbox::new();
    let fire = payload("PostToolUse", "conv-6");
    note_for(&fire, &watch(1, 0), &kerry);
    note_for(&fire, &watch(2, 0), &cld).expect("the move is announced");

    let mut compacted = payload("SessionStart", "conv-6");
    compacted.source = Some("compact".to_string());

    assert_eq!(
        note_for(&compacted, &watch(2, 0), &cld).as_deref(),
        Some(SWITCHED),
    );
}

#[test]
fn a_compaction_with_nothing_ever_announced_stays_silent() {
    let _home = HomeSandbox::new();
    let fire = payload("PostToolUse", "conv-7");
    note_for(&fire, &watch(1, 0), &kerry);

    let mut compacted = payload("SessionStart", "conv-7");
    compacted.source = Some("compact".to_string());

    assert_eq!(note_for(&compacted, &watch(1, 0), &kerry), None);
}

/// A startup or a clear rebaselines rather than announcing: neither context
/// holds an earlier turn to correct.
#[test]
fn a_startup_or_cleared_context_rebaselines_silently() {
    let _home = HomeSandbox::new();
    for source in ["startup", "clear"] {
        let session = format!("conv-8-{source}");
        note_for(&payload("UserPromptSubmit", &session), &watch(1, 0), &kerry);

        let mut started = payload("SessionStart", &session);
        started.source = Some(source.to_string());

        assert_eq!(
            note_for(&started, &watch(2, 0), &cld),
            None,
            "{source} must not announce",
        );
        assert_eq!(
            note_for(&payload("PostToolUse", &session), &watch(2, 0), &cld),
            None,
            "{source} must have moved the baseline it stayed silent about",
        );
    }
}

/// An unattributable credential is not evidence that anything moved, and the
/// name told last has to survive it or the recovery renders a shrug.
#[test]
fn an_unattributable_account_says_nothing_and_keeps_the_name_it_told() {
    let _home = HomeSandbox::new();
    let fire = payload("PostToolUse", "conv-9");
    note_for(&fire, &watch(1, 0), &kerry);

    assert_eq!(note_for(&fire, &watch(2, 0), &unknown), None);
    assert_eq!(
        note_for(&fire, &watch(3, 0), &cld).as_deref(),
        Some(SWITCHED),
        "the recovery still renders both real names",
    );
}

#[test]
fn an_id_that_cannot_spell_a_bare_filename_is_refused() {
    let ok = r#"{"hook_event_name":"PostToolUse","session_id":"0ee5e2ad-04b3"}"#;
    assert!(parse_payload(ok).is_some(), "a real session id parses");

    for bad in [
        r#"{"hook_event_name":"PostToolUse","session_id":"../../escape"}"#,
        r#"{"hook_event_name":"PostToolUse","session_id":"a/b"}"#,
        // A dot is the separator between the two record shapes, so admitting one
        // would let a conversation id spell a subagent's file.
        r#"{"hook_event_name":"PostToolUse","session_id":"a.b"}"#,
        r#"{"hook_event_name":"PostToolUse","session_id":"ok-1","agent_id":"../x"}"#,
        r#"{"hook_event_name":"PostToolUse","session_id":"ok-1","agent_id":"a.b"}"#,
    ] {
        assert!(parse_payload(bad).is_none(), "must refuse {bad}");
    }
}

#[cfg(unix)]
#[test]
fn a_conversation_record_and_its_dir_are_owner_only() {
    use std::os::unix::fs::PermissionsExt as _;

    let _home = HomeSandbox::new();
    note_for(
        &payload("UserPromptSubmit", "conv-10"),
        &watch(1, 0),
        &kerry,
    );

    let path = record_path("conv-10", None).expect("record path");
    let file = std::fs::metadata(&path).expect("the record");
    let dir = std::fs::metadata(path.parent().expect("a parent")).expect("the dir");

    assert_eq!(file.permissions().mode() & 0o777, 0o600);
    assert_eq!(dir.permissions().mode() & 0o777, 0o700);
}

#[test]
fn the_sweep_reaps_a_record_whose_transcript_is_gone() {
    let home = HomeSandbox::new();
    let live = home.home().join("live.jsonl");
    std::fs::write(&live, b"{}").expect("write a transcript");

    let mut kept = payload("UserPromptSubmit", "conv-live");
    kept.transcript = Some(live);
    note_for(&kept, &watch(1, 0), &kerry);

    let mut gone = payload("UserPromptSubmit", "conv-gone");
    gone.transcript = Some(home.home().join("gone.jsonl"));
    note_for(&gone, &watch(1, 0), &kerry);
    // Aged past the grace deliberately. The grace covers a transcript that has
    // not appeared YET (pinned separately); this is the case it must not
    // protect, a transcript that is genuinely gone.
    crate::testutil::set_mtime(
        &record_path("conv-gone", None).expect("path"),
        SystemTime::now() - MISSING_TRANSCRIPT_GRACE - std::time::Duration::from_secs(60),
    );

    gc_conversation_records();

    assert!(
        record_path("conv-live", None).expect("path").exists(),
        "a conversation whose transcript is still there keeps its record",
    );
    assert!(
        !record_path("conv-gone", None).expect("path").exists(),
        "a record whose transcript is gone is reaped",
    );
}

// ── the gate's input set ────────────────────────────────────────────────────

/// The credential stamp and the config fingerprint are two SEPARATE inputs, and
/// a gate watching only one of them lets a real account change through. Deleting
/// either half used to survive the whole suite, because the only fixture pinned
/// one axis and hardcoded the other.
#[test]
fn the_gate_watches_the_config_fingerprint_and_the_credential_store_apart() {
    let _home = HomeSandbox::new();
    let fire = payload("PostToolUse", "conv-gate");
    let calls = std::cell::Cell::new(0_u32);
    let resolve = || {
        calls.set(calls.get() + 1);
        kerry()
    };

    note_for(&fire, &watch(1, 1), &resolve);
    assert_eq!(calls.get(), 1, "a first fire has nothing cached");

    note_for(&fire, &watch(1, 1), &resolve);
    assert_eq!(calls.get(), 1, "neither input moved");

    note_for(&fire, &watch(2, 1), &resolve);
    assert_eq!(
        calls.get(),
        2,
        "the credential stamp moving must reach the resolution"
    );

    note_for(&fire, &watch(2, 2), &resolve);
    assert_eq!(
        calls.get(),
        3,
        "the config fingerprint moving must reach it too — a per-profile \
         config.toml changes the answer and touches no other watched file",
    );
}

/// An unattributable read must not bank the stamp move that produced it. The
/// move is what opened the gate, so caching it means nothing ever reopens the
/// gate and the note is lost rather than deferred.
#[test]
fn an_unattributable_read_is_never_cached() {
    let _home = HomeSandbox::new();
    let fire = payload("PostToolUse", "conv-poison");
    note_for(&fire, &watch(1, 0), &kerry);

    let calls = std::cell::Cell::new(0_u32);
    let unresolvable = || {
        calls.set(calls.get() + 1);
        unknown()
    };
    note_for(&fire, &watch(2, 0), &unresolvable);
    note_for(&fire, &watch(2, 0), &unresolvable);
    assert_eq!(
        calls.get(),
        2,
        "the second fire at the same stamp must resolve again, not read a cached None",
    );

    // The record field, not just the call count. `cache_holds` refuses a cached
    // `None` on the READ side too, so the write-side guard is invisible from the
    // count alone — and `resolved` is what the planned owner-store consumer
    // reads, where a clobbered value is the whole defect rather than a slow path.
    let stored = load_record(&record_path("conv-poison", None).expect("path")).expect("a record");
    assert_eq!(
        stored.resolved.as_deref(),
        Some("kerry"),
        "an unattributed read must not overwrite the last account actually resolved",
    );

    assert_eq!(
        note_for(&fire, &watch(2, 0), &cld).as_deref(),
        Some(SWITCHED),
        "and the account it could not read is still announced once it can",
    );
}

/// The stamp is an optimisation; the TTL is the correctness bound. Anything the
/// fingerprint does not cover has to expire rather than stick forever.
#[test]
fn a_resolution_is_retaken_once_the_ttl_expires() {
    let _home = HomeSandbox::new();
    let fire = payload("PostToolUse", "conv-ttl");
    let calls = std::cell::Cell::new(0_u32);
    let resolve = || {
        calls.set(calls.get() + 1);
        kerry()
    };

    note_for(&fire, &watch(1, 0), &resolve);
    assert_eq!(calls.get(), 1);

    let path = record_path("conv-ttl", None).expect("record path");
    let mut record = load_record(&path).expect("a record");
    record.resolved_at = Some(SystemTime::UNIX_EPOCH);
    store_record(&path, &record).expect("backdate the resolution");

    note_for(&fire, &watch(1, 0), &resolve);
    assert_eq!(
        calls.get(),
        2,
        "an expired resolution is retaken even though no watched input moved",
    );
}

/// The record IS the suppression mechanism, so a note it cannot remember would
/// be re-emitted on every tool call for the life of the conversation.
#[test]
fn a_note_that_cannot_be_recorded_is_not_emitted() {
    let _home = HomeSandbox::new();
    let fire = payload("PostToolUse", "conv-nostore");
    note_for(&fire, &watch(1, 0), &kerry);

    assert_eq!(
        note_for(&fire, &unwritable_watch(), &cld),
        None,
        "the account moved, but the record cannot be written, so nothing is said",
    );
    assert_eq!(
        load_record(&record_path("conv-nostore", None).expect("path")).and_then(|r| r.told),
        Some("kerry".to_string()),
        "and the record still holds the account it last managed to remember",
    );
}

/// The read the session→profile attribution takes as the exact per-conversation
/// observation: the last account the hook actually attributed, and nothing else.
#[test]
fn resolved_account_reads_the_main_scope_record() {
    let _home = HomeSandbox::new();
    assert_eq!(
        resolved_account("never-filed"),
        None,
        "no record, no account"
    );

    let path = record_path("conv-res", None).expect("record path");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    store_record(
        &path,
        &NoteRecord {
            resolved: Some("cld".to_string()),
            ..NoteRecord::default()
        },
    )
    .expect("write record");
    assert_eq!(
        resolved_account("conv-res").as_deref(),
        Some("cld"),
        "the attributed account comes back",
    );

    let blank = record_path("conv-blank", None).expect("record path");
    store_record(
        &blank,
        &NoteRecord {
            told: Some("kerry".to_string()),
            ..NoteRecord::default()
        },
    )
    .expect("write record");
    assert_eq!(
        resolved_account("conv-blank"),
        None,
        "a record that never attributed an account answers None, not its told baseline",
    );

    assert_eq!(
        resolved_account("../evil"),
        None,
        "a non-bare id never reaches a filename",
    );
}

// ── the exact owner-store fold ───────────────────────────────────────────────

/// The hook's attribution is exact, so it must overwrite a `Contested` entry a
/// sweep folded — the whole reason it lands in the durable store rather than
/// staying in the reaped record.
#[test]
fn the_hook_fold_overwrites_a_contested_store_entry() {
    let home = HomeSandbox::new();
    let projects = home.home().join(".claude/projects");
    let s = projects.join("-w-exact/conv-exact.jsonl");
    std::fs::create_dir_all(s.parent().unwrap()).unwrap();
    std::fs::write(&s, b"{}\n").unwrap();
    let t0 = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(10_000);
    crate::testutil::set_mtime(&s, t0);

    // Two sweeps contest the id, the shape a store holds when the exact
    // observation is missing.
    crate::sessions::stamp_run_sessions("A", &projects, false, t0);
    crate::sessions::stamp_run_sessions("B", &projects, false, t0);

    // Pre-state: the two differing sweeps must have left a CONTESTED entry, the
    // shape this test's name claims. `owner_of` collapses Contested to None, so
    // read the store file directly rather than through the reader.
    let store_file = crate::profile::clauth_dir()
        .expect("clauth dir")
        .join("session_profiles.json");
    let raw: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&store_file).expect("store")).expect("json");
    assert_eq!(
        raw["sessions"]["conv-exact"],
        serde_json::json!("contested"),
        "two differing sweeps leave a Contested entry for the exact fold to overwrite"
    );

    // The hook then attributes the account for the first time.
    note_for(&payload("PostToolUse", "conv-exact"), &watch(1, 0), &kerry);

    // Simulate the record reap: with the record gone, the owner store is the
    // only observer left, and it must now answer Known — not Contested.
    std::fs::remove_file(record_path("conv-exact", None).expect("path")).expect("reap");
    assert_eq!(
        crate::sessions::owner_of("conv-exact").as_deref(),
        Some("kerry"),
        "the exact fold must overwrite the sweep's Contested stamp"
    );
}

/// The fold rides the resolution gate: the state flock is taken only when the
/// resolved account is first set or changes, never on a repeat resolution that
/// answered the same account.
#[test]
fn the_hook_fold_skips_a_repeat_resolution_of_the_same_account() {
    let _home = HomeSandbox::new();
    let fire = payload("PostToolUse", "conv-repeat");
    crate::lock::OUTERMOST_ACQUISITIONS.with(|c| c.set(0));

    note_for(&fire, &watch(1, 0), &kerry);
    assert_eq!(
        crate::lock::OUTERMOST_ACQUISITIONS.with(|c| c.get()),
        1,
        "a first attribution lands in the store"
    );

    crate::lock::OUTERMOST_ACQUISITIONS.with(|c| c.set(0));
    note_for(&fire, &watch(2, 0), &kerry);
    assert_eq!(
        crate::lock::OUTERMOST_ACQUISITIONS.with(|c| c.get()),
        0,
        "a repeat resolution of the same account takes no state flock"
    );
}

/// The store write keys on the bare conversation id, the MAIN scope — an
/// agent_id-bearing fire is a SUBAGENT's reading of its own scope, so it must
/// write no store entry at all. Dropping the gate would let the agent's stale
/// reading overwrite the parent's correct owner.
#[test]
fn a_subagent_fire_stamps_no_owner_store_entry() {
    let _home = HomeSandbox::new();
    note_for(&payload("PostToolUse", "conv-scope"), &watch(1, 0), &kerry);

    // Reap the main record so `owner_of` reads the store, not the record.
    std::fs::remove_file(record_path("conv-scope", None).expect("path")).expect("reap");
    assert_eq!(
        crate::sessions::owner_of("conv-scope").as_deref(),
        Some("kerry"),
        "the main scope attributed kerry into the store"
    );

    let mut sub = payload("PostToolUse", "conv-scope");
    sub.agent_id = Some("agent-1".to_string());
    note_for(&sub, &watch(2, 0), &cld);

    assert_eq!(
        crate::sessions::owner_of("conv-scope").as_deref(),
        Some("kerry"),
        "a subagent fire must not stamp the parent conversation's owner"
    );
}

/// The store write takes the state flock, which sits OUTER to the scope lock in
/// the lock order. `note_for` must drop the scope lock before that write: a
/// thread holding the state flock and reaching for the scope flock deadlocks
/// against a stamp that never released it. Posed as a second thread holding the
/// state flock while the main thread fires a first attribution, then probing the
/// scope flock — it must find it free while the stamp waits on the state flock.
#[test]
fn the_store_stamp_releases_the_scope_lock_before_the_state_flock() {
    let _home = HomeSandbox::new();
    let records_lock = records_dir().expect("records dir").join(".lock");

    let (held_tx, held_rx) = std::sync::mpsc::channel::<()>();
    let (probe_tx, probe_rx) = std::sync::mpsc::channel::<bool>();

    let records_lock_thread = records_lock.clone();
    let probe = std::thread::spawn(move || {
        let held = crate::lock::StateLock::acquire().expect("state lock free");
        held_tx.send(()).expect("signal held");
        // Let the main thread finish its note work and reach the stamp. It
        // blocks on `lock::THREAD_LOCK` — held by the probe's own state-lock
        // acquire — before it ever polls the flock. Correct code dropped the
        // scope lock first; the mutated code still holds it here.
        std::thread::sleep(std::time::Duration::from_millis(500));
        let file = crate::profile::open_state_file(&records_lock_thread).expect("open scope lock");
        let scope_free =
            crate::lock::lock_file_with_timeout(&file, std::time::Duration::from_millis(1_000))
                .is_ok();
        probe_tx.send(scope_free).expect("signal probe");
        drop(held);
    });

    held_rx.recv().expect("state lock held");
    note_for(&payload("PostToolUse", "conv-order"), &watch(1, 0), &kerry);

    let scope_free = probe_rx
        .recv_timeout(std::time::Duration::from_secs(5))
        .expect("probe reports");
    probe.join().expect("probe thread");
    assert!(
        scope_free,
        "the stamp must not hold the scope lock while waiting on the state flock"
    );
}

/// `ScopeLock` must enter its rank in the global lock order, so a future edit
/// that reaches for the state flock while the scope lock is held trips the
/// order assertion instead of deadlocking.
#[test]
fn the_scope_lock_enters_its_rank() {
    let _home = HomeSandbox::new();
    let _hold = ScopeLock::acquire();
    debug_assert!(
        crate::lockorder::holds::<crate::lockorder::rank::Scope>(),
        "the scope lock must enter its SCOPE rank"
    );
}

// ── the account-changed note's headroom figure (r9) ─────────────────────────

/// A blank profile whose disk usage cache holds a live 5h window at `pct` —
/// the bytes `switched_headroom_pct` reads. The profile itself is required,
/// not optional: the cache writer skips names the on-disk profile record does
/// not carry, so seeding the cache alone writes nothing.
fn seed_headroom(name: &str, pct: f64) {
    let mut config = crate::profile::AppConfig {
        state: crate::profile::AppState::default(),
        profiles: Vec::new(),
    };
    crate::actions::create_blank_profile(&mut config, name.to_string(), None, None, None)
        .expect("create profile");

    let now_secs = crate::usage::now_epoch_secs();
    write_profile_cache(
        &ProfileName::from(name),
        USAGE_CACHE_FILE,
        &crate::usage::UsageInfo {
            five_hour: Some(crate::usage::UsageWindow {
                utilization: pct,
                resets_at: Some(crate::usage::epoch_secs_to_iso(now_secs + 3600)),
            }),
            ..Default::default()
        },
    );
}

/// The E4-approved copy, byte for byte, with the placeholders filled: the
/// switched sentence plus the new account's cached 5h window percent, gathered
/// off the disk cache through the real readers. Both accounts carry figures
/// and they differ, so the pin also shows the figure named is the NEW
/// account's — reading the old account's cache would print 33.
#[test]
fn an_account_change_names_the_new_accounts_headroom() {
    let _home = HomeSandbox::new();
    seed_headroom("kerry", 33.0);
    seed_headroom("cld", 62.0);
    let fire = payload("PostToolUse", "conv-headroom");

    note_for(&fire, &watch(1, 0), &kerry);
    assert_eq!(
        note_for(&fire, &watch(2, 0), &cld).as_deref(),
        Some(
            "clauth note: the active profile for this session switched from \
             `kerry` to `cld`; its 5h window is 62% used.",
        ),
    );

    // The compact-with-change arm is the switched spelling's second render
    // site; the clause must land there too.
    let mut compacted = payload("SessionStart", "conv-headroom");
    compacted.source = Some("compact".to_string());
    assert_eq!(
        note_for(&compacted, &watch(3, 0), &kerry).as_deref(),
        Some(
            "clauth note: the active profile for this session switched from \
             `cld` to `kerry`; its 5h window is 33% used.",
        ),
    );
}

/// A subagent spawned BEFORE the move has no record of its own, so the gather
/// gate finds no `told` on the peek — the clause must still land on the note
/// the subagent hears. This is the load-bearing arm of the `told != candidate`
/// gate: it may not read as "no baseline, nothing can fire".
#[test]
fn a_subagent_that_predates_the_move_hears_the_clause() {
    let _home = HomeSandbox::new();
    seed_headroom("kerry", 33.0);
    seed_headroom("cld", 62.0);
    let main = payload("UserPromptSubmit", "conv-sub-clause");
    note_for(&main, &watch(1, 0), &kerry);

    let mut sub = payload("PostToolUse", "conv-sub-clause");
    sub.agent_id = Some("a4a894a1be41b92bf".to_string());
    assert_eq!(
        note_for(&sub, &watch(2, 0), &cld).as_deref(),
        Some(
            "clauth note: the active profile for this session switched from \
             `kerry` to `cld`; its 5h window is 62% used.",
        ),
    );
}

/// The E4 omit rule: a new account with no cached usage figure keeps the
/// sentence byte-identical to the pre-r9 spelling. The clause is omitted,
/// never rendered with a made-up figure.
#[test]
fn a_usage_less_account_keeps_the_sentence_unchanged() {
    let _home = HomeSandbox::new();
    let fire = payload("PostToolUse", "conv-nocache");
    note_for(&fire, &watch(1, 0), &kerry);
    assert_eq!(
        note_for(&fire, &watch(2, 0), &cld).as_deref(),
        Some(SWITCHED),
    );
}

/// A cached window that has already lapsed has no figure to name: its percent
/// belongs to a pool that is open again, and printing it would be the false
/// claim about "its 5h window" the note exists to prevent.
#[test]
fn a_lapsed_window_omits_the_clause() {
    let _home = HomeSandbox::new();
    seed_headroom("cld", 62.0);
    let now_secs = crate::usage::now_epoch_secs();
    write_profile_cache(
        &ProfileName::from("cld"),
        USAGE_CACHE_FILE,
        &crate::usage::UsageInfo {
            five_hour: Some(crate::usage::UsageWindow {
                utilization: 62.0,
                resets_at: Some(crate::usage::epoch_secs_to_iso(now_secs - 3600)),
            }),
            ..Default::default()
        },
    );
    let fire = payload("PostToolUse", "conv-lapsed");
    note_for(&fire, &watch(1, 0), &kerry);
    assert_eq!(
        note_for(&fire, &watch(2, 0), &cld).as_deref(),
        Some(SWITCHED)
    );
}

/// The resume spelling stays unchanged, clause or not: a resume onto an
/// account WITH a cached figure still renders the pre-r9 sentence — the
/// headroom clause is the switched spelling's alone.
#[test]
fn a_resume_never_carries_the_headroom_clause() {
    let _home = HomeSandbox::new();
    seed_headroom("DS4", 62.0);
    note_for(
        &payload("UserPromptSubmit", "conv-resume-headroom"),
        &watch(1, 0),
        &|| reading(Some("z.ai")),
    );

    let mut resumed = payload("SessionStart", "conv-resume-headroom");
    resumed.source = Some("resume".to_string());
    assert_eq!(
        note_for(&resumed, &watch(2, 0), &|| reading(Some("DS4"))).as_deref(),
        Some("clauth note: session resumed under `DS4`; earlier turns ran under `z.ai`."),
    );
}

// ── the sweep ───────────────────────────────────────────────────────────────

/// A baseline written at `SessionStart` can land before Claude Code creates the
/// transcript. A bare `!exists()` reaps it, and the conversation's next real
/// move is then absorbed as a first fire and never announced.
#[test]
fn a_transcript_that_has_not_appeared_yet_keeps_its_record() {
    let home = HomeSandbox::new();
    let mut fire = payload("UserPromptSubmit", "conv-young");
    fire.transcript = Some(home.home().join("not-yet.jsonl"));
    note_for(&fire, &watch(1, 0), &kerry);

    gc_conversation_records();

    let path = record_path("conv-young", None).expect("path");
    assert!(path.exists(), "a record inside the grace window survives");

    crate::testutil::set_mtime(
        &path,
        SystemTime::now() - MISSING_TRANSCRIPT_GRACE - std::time::Duration::from_secs(60),
    );
    gc_conversation_records();
    assert!(
        !path.exists(),
        "and is reaped once it has aged past the grace"
    );
}

/// A scope still firing keeps its record across the sweep whatever its
/// transcript's state: every fire moves the record's mtime (the touch), so
/// the grace measures silence, not the transcript. The sequential shape that
/// used to reap a live scope's baseline — age, sweep, fire — must now survive
/// the sweep because the fire landed first, and the same record must still be
/// reaped once the fires stop.
#[test]
fn a_still_firing_scope_survives_the_sweep_with_its_transcript_gone() {
    let home = HomeSandbox::new();
    let mut fire = payload("UserPromptSubmit", "conv-firing");
    fire.transcript = Some(home.home().join("absent.jsonl"));
    note_for(&fire, &watch(1, 0), &kerry);

    // A fire with nothing to say — same watch, same account — changes no
    // record bytes, so only the touch moves the mtime.
    crate::testutil::set_mtime(
        &record_path("conv-firing", None).expect("path"),
        SystemTime::now() - MISSING_TRANSCRIPT_GRACE - std::time::Duration::from_secs(60),
    );
    note_for(&fire, &watch(1, 0), &kerry);

    gc_conversation_records();
    assert!(
        record_path("conv-firing", None).expect("path").exists(),
        "a scope still firing keeps its record across the sweep, transcript absent or not",
    );

    // The same record, once the fires stop: age it again with no fire in
    // between, and it is reaped.
    crate::testutil::set_mtime(
        &record_path("conv-firing", None).expect("path"),
        SystemTime::now() - MISSING_TRANSCRIPT_GRACE - std::time::Duration::from_secs(60),
    );
    gc_conversation_records();
    assert!(
        !record_path("conv-firing", None).expect("path").exists(),
        "a scope that has genuinely gone quiet is still reaped",
    );
}

/// A backward clock step future-dates a record's mtime. The sweep keeps it (its
/// age reads as none), and the prune's grace helper must match: a future mtime
/// counts as within the grace, never as silent-past-it.
#[test]
fn a_future_record_mtime_counts_as_within_the_grace() {
    let _home = HomeSandbox::new();
    let mut fire = payload("UserPromptSubmit", "conv-future");
    fire.transcript = Some(PathBuf::from("/absent.jsonl"));
    note_for(&fire, &watch(1, 0), &kerry);

    let path = record_path("conv-future", None).expect("path");
    crate::testutil::set_mtime(&path, SystemTime::now() + Duration::from_secs(3600));

    assert!(
        last_fire_within_missing_transcript_grace("conv-future"),
        "a future mtime keeps, matching the sweep's age predicate"
    );

    gc_conversation_records();
    assert!(
        path.exists(),
        "the sweep also keeps the future-dated record"
    );
}

/// The dir also holds the lock file. A sweep that reaps it is a sweep deleting
/// the machinery serialising its own writers.
#[test]
fn the_sweep_leaves_everything_that_is_not_a_record_alone() {
    let _home = HomeSandbox::new();
    note_for(&payload("PostToolUse", "conv-keep"), &watch(1, 0), &kerry);
    let lock = records_dir().expect("dir").join(".lock");
    assert!(
        lock.exists(),
        "the fire took the lock, so the file is there"
    );

    crate::testutil::set_mtime(
        &lock,
        SystemTime::now() - ORPHAN_RECORD_MAX_AGE - std::time::Duration::from_secs(60),
    );
    gc_conversation_records();

    assert!(
        lock.exists(),
        "the lock file is not a record and is never reaped"
    );
}

// ── payload edges ───────────────────────────────────────────────────────────

/// Claude Code documents five `SessionStart` sources and may add more. An
/// unrecognised one must rebaseline, never announce a switch about turns a fresh
/// context never held.
#[test]
fn an_unrecognised_session_start_source_rebaselines_silently() {
    let _home = HomeSandbox::new();
    for source in ["fork", "startup", "clear", "something-claude-adds-later"] {
        let session = format!("conv-src-{source}");
        note_for(&payload("UserPromptSubmit", &session), &watch(1, 0), &kerry);

        let mut started = payload("SessionStart", &session);
        started.source = Some(source.to_string());

        assert_eq!(
            note_for(&started, &watch(2, 0), &cld),
            None,
            "{source} must not announce",
        );
        assert_eq!(
            note_for(&payload("PostToolUse", &session), &watch(2, 0), &cld),
            None,
            "{source} must have moved the baseline it stayed silent about",
        );
    }
}

/// A compaction arriving before anything was ever told has nothing to
/// re-announce, and must still leave the scope with a baseline.
#[test]
fn a_compaction_before_any_baseline_establishes_one() {
    let _home = HomeSandbox::new();
    let mut compacted = payload("SessionStart", "conv-early-compact");
    compacted.source = Some("compact".to_string());

    assert_eq!(note_for(&compacted, &watch(1, 0), &kerry), None);
    assert_eq!(
        note_for(
            &payload("PostToolUse", "conv-early-compact"),
            &watch(2, 0),
            &cld
        )
        .as_deref(),
        Some(SWITCHED),
        "the compaction left a baseline, so the next real move is announced",
    );
}

/// A resume on the SAME account must drop the previous process's note, or a
/// later compaction re-announces a switch this context never saw — every time.
#[test]
fn a_resume_on_the_same_account_drops_the_previous_processes_note() {
    let _home = HomeSandbox::new();
    let fire = payload("PostToolUse", "conv-carry");
    note_for(&fire, &watch(1, 0), &kerry);
    note_for(&fire, &watch(2, 0), &cld).expect("the move is announced");

    let mut resumed = payload("SessionStart", "conv-carry");
    resumed.source = Some("resume".to_string());
    assert_eq!(
        note_for(&resumed, &watch(2, 0), &cld),
        None,
        "same account, silent"
    );

    let mut compacted = payload("SessionStart", "conv-carry");
    compacted.source = Some("compact".to_string());
    assert_eq!(
        note_for(&compacted, &watch(2, 0), &cld),
        None,
        "the note belonged to a process this context never saw",
    );
}

/// Keyed on the field being absent, never on `as_str()` succeeding: a
/// present-but-unusable value used to read as absent and consume the main
/// thread's record.
#[test]
fn a_present_but_unusable_agent_id_is_refused() {
    for bad in [
        r#"{"hook_event_name":"PostToolUse","session_id":"ok-1","agent_id":12345}"#,
        r#"{"hook_event_name":"PostToolUse","session_id":"ok-1","agent_id":true}"#,
        r#"{"hook_event_name":"PostToolUse","session_id":"ok-1","agent_id":{}}"#,
        r#"{"hook_event_name":"PostToolUse","session_id":"ok-1","agent_id":[]}"#,
    ] {
        assert!(parse_payload(bad).is_none(), "must refuse {bad}");
    }
    let absent = r#"{"hook_event_name":"PostToolUse","session_id":"ok-1","agent_id":null}"#;
    assert!(
        parse_payload(absent).is_some_and(|p| p.agent_id.is_none()),
        "an explicit null is absent, which is the main thread",
    );
}

/// The event name is echoed into the envelope, so it is bounded like both ids.
/// Bounded because it is echoed back, but NOT held to the id charset: that
/// value never reaches a filename, and sharing the charset would take the hook
/// silently offline for any event Claude Code ever namespaces.
#[test]
fn an_event_name_is_bounded_without_being_held_to_the_id_charset() {
    let huge = "A".repeat(65);
    for bad in [
        format!(r#"{{"hook_event_name":"{huge}","session_id":"ok-1"}}"#),
        r#"{"hook_event_name":"","session_id":"ok-1"}"#.to_string(),
        r#"{"hook_event_name":"Post\nToolUse","session_id":"ok-1"}"#.to_string(),
    ] {
        assert!(parse_payload(&bad).is_none(), "must refuse {bad}");
    }
    for ok in [
        r#"{"hook_event_name":"PostToolUse","session_id":"ok-1"}"#,
        r#"{"hook_event_name":"a.b","session_id":"ok-1"}"#,
        r#"{"hook_event_name":"a:b","session_id":"ok-1"}"#,
    ] {
        assert!(parse_payload(ok).is_some(), "must accept {ok}");
    }
}

#[test]
fn the_bare_id_bounds_are_exactly_sixty_four_bytes_and_non_empty() {
    assert!(is_bare_id(&"a".repeat(64)), "64 bytes is the last accepted");
    assert!(!is_bare_id(&"a".repeat(65)), "65 is one too many");
    assert!(!is_bare_id(""), "empty spells no filename");
}

/// `watch_now` is the PRODUCTION side of the gate, and it had no test caller at
/// all: every fixture handed `note_for` a `Watch` it built itself, so deleting
/// half of what `watch_now` stamps survived the whole suite. These two drive it
/// against a real tree instead.
#[test]
fn watch_now_sees_a_per_profile_config_edit() {
    let home = HomeSandbox::new();
    let profile = home.home().join(".clauth/profiles/acme");
    std::fs::create_dir_all(&profile).expect("profile dir");
    let config = profile.join("config.toml");
    std::fs::write(&config, b"# one\n").expect("write config");

    let before = watch_now();
    std::fs::write(&config, b"# two\n").expect("rewrite config");
    // An explicit future mtime rather than a second write: two writes inside one
    // coarse filesystem tick leave the fingerprint equal and the test would pass
    // for the wrong reason.
    crate::testutil::set_mtime(
        &config,
        SystemTime::now() + std::time::Duration::from_secs(5),
    );
    let after = watch_now();

    assert_ne!(
        before.config, after.config,
        "a per-profile config.toml edit changes the attributed account and \
         touches no other watched file, so the fingerprint must move",
    );
    assert_eq!(
        before.creds, after.creds,
        "and it must not be smuggled in through the credential stamp",
    );
}

#[test]
fn watch_now_sees_the_credential_store_move() {
    let home = HomeSandbox::new();
    let claude = home.home().join(".claude");
    std::fs::create_dir_all(&claude).expect("claude dir");
    let creds = claude.join(".credentials.json");
    std::fs::write(&creds, b"{}").expect("write creds");

    let before = watch_now();
    std::fs::write(&creds, b"{\"a\":1}").expect("rewrite creds");
    crate::testutil::set_mtime(
        &creds,
        SystemTime::now() + std::time::Duration::from_secs(5),
    );
    let after = watch_now();

    assert_ne!(
        before.creds, after.creds,
        "the store this session authenticates from must be stamped",
    );
    assert_eq!(
        before.config, after.config,
        "and it must not be smuggled in through the config fingerprint",
    );
}

/// The no-transcript arm of the sweep. The fire-mtime is the only liveness
/// signal such a record has, so it ages out once the fires stop — and no
/// fixture reached this branch at all, which let both a disabled reap and an
/// inverted comparison survive the whole suite.
#[test]
fn a_record_that_never_carried_a_transcript_ages_out() {
    let _home = HomeSandbox::new();
    // No `transcript` on the payload, which is the only way to reach the arm.
    note_for(&payload("PostToolUse", "conv-orphan"), &watch(1, 0), &kerry);
    let path = record_path("conv-orphan", None).expect("path");
    assert_eq!(
        load_record(&path).and_then(|r| r.transcript),
        None,
        "the fixture has to actually reach the no-transcript branch",
    );

    gc_conversation_records();
    assert!(path.exists(), "a young orphan is kept");

    crate::testutil::set_mtime(
        &path,
        SystemTime::now() - ORPHAN_RECORD_MAX_AGE - std::time::Duration::from_secs(60),
    );
    gc_conversation_records();
    assert!(!path.exists(), "and an aged one is reaped");
}

/// An empty or relative `transcript_path` is dropped at the boundary rather
/// than stored: `Path::new("").exists()` is false, so storing it verbatim aimed
/// the sweep at a live conversation's record.
#[test]
fn an_unusable_transcript_path_is_not_stored() {
    for payload_json in [
        r#"{"hook_event_name":"PostToolUse","session_id":"ok-1","transcript_path":""}"#,
        r#"{"hook_event_name":"PostToolUse","session_id":"ok-1","transcript_path":"rel/x.jsonl"}"#,
    ] {
        let parsed = parse_payload(payload_json).expect("the payload itself is fine");
        assert_eq!(parsed.transcript, None, "must drop {payload_json}");
    }
    // One "good" fixture per platform: std documents `Path::is_absolute` as
    // prefix-plus-root on Windows ("c:\windows is absolute, c:temp and \temp
    // are not"), so `/a/b.jsonl` has no drive/UNC prefix and drops like the two
    // bad legs there. Both drive the same assertion, so the absolute-positive
    // pin stays covered where the semantics differ.
    let (good, expected) = if cfg!(target_os = "windows") {
        (
            r#"{"hook_event_name":"PostToolUse","session_id":"ok-1","transcript_path":"C:\\a\\b.jsonl"}"#,
            PathBuf::from(r"C:\a\b.jsonl"),
        )
    } else {
        (
            r#"{"hook_event_name":"PostToolUse","session_id":"ok-1","transcript_path":"/a/b.jsonl"}"#,
            PathBuf::from("/a/b.jsonl"),
        )
    };
    assert_eq!(
        parse_payload(good).expect("parses").transcript,
        Some(expected),
    );
}

/// Both sides of the TTL boundary. Backdating to the epoch alone occupies only
/// the far tail, so the constant could move to an hour or a day and nothing
/// would fail.
#[test]
fn the_resolution_ttl_holds_just_inside_and_expires_just_outside() {
    let _home = HomeSandbox::new();
    let fire = payload("PostToolUse", "conv-ttl-edge");
    let calls = std::cell::Cell::new(0_u32);
    let resolve = || {
        calls.set(calls.get() + 1);
        kerry()
    };
    let backdate = |by: Duration| {
        let path = record_path("conv-ttl-edge", None).expect("path");
        let mut record = load_record(&path).expect("a record");
        record.resolved_at = Some(SystemTime::now() - by);
        store_record(&path, &record).expect("backdate");
    };

    note_for(&fire, &watch(1, 0), &resolve);
    assert_eq!(calls.get(), 1);

    // Literal seconds, never `RESOLUTION_TTL ± n`: a margin derived from the
    // constant under test slides with it, so both legs stay on their own side
    // of any value the constant takes and the test can never observe it moving.
    backdate(Duration::from_secs(55));
    note_for(&fire, &watch(1, 0), &resolve);
    assert_eq!(
        calls.get(),
        1,
        "55s is inside the 60s TTL and still serves the cache"
    );

    backdate(Duration::from_secs(65));
    note_for(&fire, &watch(1, 0), &resolve);
    assert_eq!(calls.get(), 2, "65s is outside it and must resolve again");
}

/// Two concurrent fires resolve OUTSIDE the hold, so the order they reach the
/// lock in says nothing about the order they observed in. The one carrying the
/// older reading must defer, or it announces the reversal — a switch that never
/// happened — and caches its stale answer for the whole TTL.
#[test]
fn a_fire_carrying_the_older_observation_defers_to_the_record() {
    let _home = HomeSandbox::new();
    let fire = payload("PostToolUse", "conv-stale");
    note_for(&fire, &watch(1, 0), &kerry);
    note_for(&fire, &watch(2, 0), &cld).expect("the real move is announced once");

    // A peer fire that observed LATER but landed first. It writes from inside
    // this fire's own resolution, which is the only way to put its stamp
    // strictly between this fire's `taken_at` and now — a hand-written FUTURE
    // stamp would instead exercise the clock-step case the guard now refuses.
    let stale_observation_racing_a_peer = || {
        // The stamp travels with the reading, so this fire's own reading is
        // stamped BEFORE the peer's write lands — the older observation it is.
        let taken_at = SystemTime::now();
        std::thread::sleep(Duration::from_millis(5));
        let path = record_path("conv-stale", None).expect("path");
        let mut peer = load_record(&path).expect("a record");
        peer.resolved = Some("cld".to_string());
        peer.resolved_at = Some(SystemTime::now());
        store_record(&path, &peer).expect("the peer lands first");
        Some(Reading {
            account: Some("kerry".to_string()),
            taken_at,
        }) // this fire's own, older reading
    };

    assert_eq!(
        note_for(&fire, &watch(3, 0), &stale_observation_racing_a_peer),
        None,
        "the stale reading must not announce `cld` -> `kerry`, which never happened",
    );
    assert_eq!(
        load_record(&record_path("conv-stale", None).expect("path"))
            .and_then(|r| r.resolved)
            .as_deref(),
        Some("cld"),
        "and it must not overwrite the fresher answer it lost to",
    );
}

/// A backward clock step leaves `resolved_at` in the future with no peer fire
/// involved. Without the past-instant half of the guard, every later fire defers
/// to it and correct answers are discarded for the size of the step — and the
/// TTL cannot bound that, because this path runs only when `cache_holds` has
/// already rejected the cache.
#[test]
fn a_future_timestamp_from_a_clock_step_does_not_freeze_the_answer() {
    let _home = HomeSandbox::new();
    let fire = payload("PostToolUse", "conv-clockstep");
    note_for(&fire, &watch(1, 0), &kerry);

    let path = record_path("conv-clockstep", None).expect("path");
    let mut record = load_record(&path).expect("a record");
    record.resolved_at = Some(SystemTime::now() + RESOLUTION_TTL * 5);
    store_record(&path, &record).expect("stamp the future");

    assert_eq!(
        note_for(&fire, &watch(2, 0), &cld).as_deref(),
        Some(SWITCHED),
        "a stamp that cannot have been taken yet must not win the comparison",
    );
    assert_eq!(
        load_record(&path).and_then(|r| r.resolved).as_deref(),
        Some("cld"),
        "and the fresh answer must land rather than being thrown away",
    );
}

// ── the headroom nudge ──────────────────────────────────────────────────────

/// A live, exhausted, rate-bearing window read at `now`, resetting `resets_in`
/// seconds later. One instance is reused across the fires of a test so every
/// verdict shares a window identity and a second-boundary step cannot re-arm it.
fn headroom(used: f64, rate: f64, resets_in: i64) -> Headroom {
    let now = crate::usage::now_epoch_secs();
    Headroom {
        used,
        threshold: 95.0,
        resets_at: now + resets_in,
        rate: Some(rate),
        now,
    }
}

fn nudge_read(used: f64, rate: f64, resets_in: i64, chain_acts: bool) -> NudgeRead {
    NudgeRead {
        headroom: Some(headroom(used, rate, resets_in)),
        chain_acts,
    }
}

/// The approved copy, byte for byte, with the placeholders the gate computed:
/// used, the measured rate, the projected cap instant, the reset. The two
/// stamps render through the crate's one local-stamp formatter, so the pin
/// asserts the WIRING — which instants reach the copy — while `local_stamp`'s
/// own shape is pinned in its module; the copy's literal text is pinned here
/// in full. 97% at 5%/h caps in (100-97)/5 h = 2160 s, inside the 3600 s left.
#[test]
fn a_task_past_the_threshold_with_nowhere_to_go_emits_the_approved_copy() {
    let _home = HomeSandbox::new();
    let fire = task_fire("conv-nudge");
    let read = nudge_read(97.0, 5.0, 3600, false);
    let h = read.headroom.expect("a window");

    let note = nudge_note(&fire, &read).expect("the nudge fires");
    let when = crate::format::local_stamp(h.now + 2160).expect("stamp");
    let reset = crate::format::local_stamp(h.resets_at).expect("stamp");
    assert_eq!(
        note,
        format!(
            "clauth note: 5h window 97% used (5.0%/h). at this rate, it reaches \
             its cap {when}, resets {reset}. no fallback is set; further agent \
             spawns may fail with 429s.",
        ),
    );
}

/// The record is the suppression mechanism: an unchanged verdict stays silent
/// across fires and across record reloads — a fresh hook process is exactly a
/// fresh record read off the same bytes — and the state that buys the silence
/// is on disk, not in memory.
#[test]
fn an_unchanged_verdict_does_not_re_emit_across_fires_or_reloads() {
    let _home = HomeSandbox::new();
    let fire = task_fire("conv-again");
    let read = nudge_read(97.0, 5.0, 3600, false);
    let window = read.headroom.expect("a window").resets_at;

    nudge_note(&fire, &read).expect("the first fire emits");
    assert_eq!(
        nudge_note(&fire, &read),
        None,
        "the same window under the same verdict stays silent",
    );

    let path = record_path("conv-again", None).expect("path");
    let stored = load_record(&path).expect("a record");
    assert_eq!(
        stored.nudge.as_ref().map(|s| (s.resets_at, s.emitted)),
        Some((Some(window), true)),
        "the emitted state for this window is on disk",
    );

    // The reload proof: the next fire reads the record back off disk, and the
    // state it finds there suppresses it all the same.
    assert_eq!(nudge_note(&fire, &read), None);
}

/// Two re-arms, both keyed on the stored window identity: the window rolling
/// over (a different reset instant) and the verdict flipping false then true
/// again inside one window. r8's projection gate inherits both shapes
/// unchanged — the field holds the identity and the emission state, nothing
/// more is needed to re-arm.
#[test]
fn the_nudge_re_arms_on_a_window_reset_and_on_a_verdict_flip() {
    let _home = HomeSandbox::new();
    let fire = task_fire("conv-rearm");
    let base = headroom(97.0, 5.0, 3600);
    let spent = NudgeRead {
        headroom: Some(base),
        chain_acts: false,
    };
    // The same window, now under the threshold: a silent verdict.
    let quiet = NudgeRead {
        headroom: Some(Headroom { used: 60.0, ..base }),
        chain_acts: false,
    };

    nudge_note(&fire, &spent).expect("fires");
    assert_eq!(nudge_note(&fire, &spent), None);

    nudge_note(&fire, &quiet);
    assert!(
        nudge_note(&fire, &spent).is_some(),
        "the verdict flipped false and back: the same window re-announces",
    );

    // A different reset instant is a different window, told or not.
    assert!(
        nudge_note(&fire, &nudge_read(97.0, 5.0, 7200, false)).is_some(),
        "a new window re-arms by identity",
    );
}

/// Every arm of the gate that keeps the note silent: a switch the chain would
/// land, a window with no measured rate — silence either way (a no-rate
/// window was silent under r7 either way; the copy's `{rate}`/`{when}`
/// placeholders have no figures to fill) — and a rate that reaches the cap
/// only after the reset (the copy's cap claim would be false). A silent
/// verdict also writes no record — nothing was learned.
#[test]
fn a_chain_target_a_missing_rate_and_a_late_cap_each_stay_silent() {
    let _home = HomeSandbox::new();
    let fire = task_fire("conv-quiet");
    let path = record_path("conv-quiet", None).expect("path");

    assert_eq!(
        nudge_note(&fire, &nudge_read(97.0, 5.0, 3600, true)),
        None,
        "a usable next chain member means a switch lands instead of the refusal",
    );
    assert!(!path.exists(), "a silent verdict writes no record");

    let no_rate = NudgeRead {
        headroom: Some(Headroom {
            rate: None,
            ..headroom(97.0, 2.0, 3600)
        }),
        chain_acts: false,
    };
    assert_eq!(
        nudge_note(&fire, &no_rate),
        None,
        "at the threshold with no measured rate: silence — r7's deleted static \
         check answered true here, and the rate filter after it silenced the emit",
    );

    let no_rate_below = NudgeRead {
        headroom: Some(Headroom {
            used: 80.0,
            rate: None,
            ..headroom(97.0, 2.0, 3600)
        }),
        chain_acts: false,
    };
    assert_eq!(
        nudge_note(&fire, &no_rate_below),
        None,
        "below the threshold with no measured rate: silence either way, exactly \
         as r7's static check answered",
    );

    // 0.1%/h from 97% needs 30 h; the window resets in 5 h — the reset wins.
    assert_eq!(
        nudge_note(&fire, &nudge_read(97.0, 0.1, 5 * 3600, false)),
        None,
    );
}

/// Verify 1: the r8 arm — a window BELOW the static threshold whose measured
/// rate still reaches the cap before the reset emits (r7's gate silenced it;
/// that is the whole point of the projection). 74% at 30%/h caps in
/// (100-74)/30 h = 3120 s, inside the 7200 s left, and the figures pin the
/// wiring for this new emit path the same way the 97% fixture pins the
/// threshold-side one.
#[test]
fn a_below_threshold_window_whose_rate_caps_before_the_reset_emits() {
    let _home = HomeSandbox::new();
    let fire = task_fire("conv-proj");
    let read = nudge_read(74.0, 30.0, 7200, false);
    let h = read.headroom.expect("a window");

    let note = nudge_note(&fire, &read).expect("the projection arm fires");
    let when = crate::format::local_stamp(h.now + 3120).expect("stamp");
    let reset = crate::format::local_stamp(h.resets_at).expect("stamp");
    assert_eq!(
        note,
        format!(
            "clauth note: 5h window 74% used (30.0%/h). at this rate, it reaches \
             its cap {when}, resets {reset}. no fallback is set; further agent \
             spawns may fail with 429s.",
        ),
    );

    // The r7 fixture re-pinned: 80% at 30%/h caps in 2400 s, inside the
    // 7200 s left, and r7 silenced it on the static threshold alone — the one
    // leg only `window_exhausted` owned. The projection arm now emits it.
    assert!(
        nudge_note(
            &task_fire("conv-proj-80"),
            &nudge_read(80.0, 30.0, 7200, false)
        )
        .is_some(),
        "a below-threshold window whose rate caps inside the window emits",
    );
}

/// Verify 2: the same 74% at a rate that reaches the cap only after the reset
/// stays silent — the approved copy's cap claim would be false. 2%/h needs
/// 13 h from 74%; the window resets in 2 h.
#[test]
fn a_below_threshold_window_whose_rate_misses_the_reset_stays_silent() {
    let _home = HomeSandbox::new();
    let fire = task_fire("conv-slow");
    assert_eq!(nudge_note(&fire, &nudge_read(74.0, 2.0, 7200, false)), None,);
}

/// Verify 3: the floor guard the projection shares with the fallback leg. The
/// window-relative rate reads high early, and without the floor this 40%
/// window's 300%/h would "reach the cap" inside the 7200 s left — the smaller
/// tiers' false fire the guard exists to bound. 40% sits under
/// `NUDGE_BURN_FLOOR_PCT` (50, half the cap) and under the threshold, so the
/// floor conjunct is the only thing silencing it: deleting it reds this test.
#[test]
fn the_projection_floor_keeps_a_low_window_with_a_huge_early_rate_silent() {
    let _home = HomeSandbox::new();
    let fire = task_fire("conv-floor");
    assert_eq!(
        nudge_note(&fire, &nudge_read(40.0, 300.0, 7200, false)),
        None,
    );
}

/// The record IS the suppression mechanism for the nudge exactly as it is for
/// the account note: a verdict that cannot be remembered is not emitted. A
/// directory occupying the record's path makes the atomic rename fail.
#[test]
fn a_nudge_that_cannot_be_recorded_is_not_emitted() {
    let _home = HomeSandbox::new();
    let fire = task_fire("conv-nostore-nudge");
    let path = record_path("conv-nostore-nudge", None).expect("path");
    std::fs::create_dir_all(&path).expect("occupy the record's path");

    assert_eq!(nudge_note(&fire, &nudge_read(97.0, 5.0, 3600, false)), None,);
}

/// The tool name is the field the gate keys on, and a present-but-unusable one
/// reads as absent: it cannot spell `Task`, and refusing the whole payload over
/// it would take the account note down with it.
#[test]
fn the_tool_name_reaches_the_parser_leniently() {
    let parsed = parse_payload(
        r#"{"hook_event_name":"PostToolUse","session_id":"ok-1","tool_name":"Task"}"#,
    )
    .expect("parses");
    assert_eq!(parsed.tool_name.as_deref(), Some("Task"));

    let absent =
        parse_payload(r#"{"hook_event_name":"PostToolUse","session_id":"ok-1"}"#).expect("parses");
    assert_eq!(absent.tool_name, None);

    let unusable =
        parse_payload(r#"{"hook_event_name":"PostToolUse","session_id":"ok-1","tool_name":12345}"#)
            .expect("still parses — lenient, unlike agent_id");
    assert_eq!(unusable.tool_name, None);
}

/// Records written before the field existed must keep parsing: the serde
/// default is the upgrade gate.
#[test]
fn a_record_without_the_nudge_field_still_parses() {
    let _home = HomeSandbox::new();
    let path = record_path("conv-old", None).expect("path");
    std::fs::create_dir_all(path.parent().expect("dir")).expect("mkdir");
    std::fs::write(&path, br#"{"told":"kerry","last_note":null}"#).expect("an old record");

    let record = load_record(&path).expect("old bytes parse");
    assert_eq!(record.told.as_deref(), Some("kerry"));
    assert_eq!(
        record.nudge, None,
        "the default for a field that never existed"
    );
}

/// The scope gate and the tool gate live in the reader, before any disk read:
/// a subagent fire, or any tool that is not `Task`, is never nudge-eligible.
#[test]
fn a_subagent_or_non_task_fire_is_never_nudge_eligible() {
    let _home = HomeSandbox::new();

    let mut sub = task_fire("conv-scope");
    sub.agent_id = Some("a4a894a1be41b92bf".to_string());
    assert!(
        read_nudge(&sub, None).is_none(),
        "a subagent fire answers for nobody"
    );

    let mut bash = task_fire("conv-scope");
    bash.tool_name = Some("Bash".to_string());
    assert!(
        read_nudge(&bash, None).is_none(),
        "only the agent-spawn tool earns it"
    );

    assert!(
        read_nudge(&payload("PostToolUse", "conv-scope"), None).is_none(),
        "a fire carrying no tool name cannot be a Task fire",
    );
}

/// The `shared = Some(_)` arm: `run()` hands the fire's one `load_config` to
/// the reader, and the arm must read THAT config, never re-load the disk one.
/// Here the two diverge — the disk config's active is `b`, the shared
/// config's is `a` — and only `a` carries a cached window, so the arm answers
/// `a`'s window or nothing.
#[test]
fn the_shared_config_arm_reads_the_shared_config_not_the_disk() {
    let _home = HomeSandbox::new();
    let mut config = crate::profile::AppConfig {
        state: crate::profile::AppState::default(),
        profiles: Vec::new(),
    };
    crate::actions::create_blank_profile(&mut config, "a".to_string(), None, None, None)
        .expect("create a");
    crate::actions::create_blank_profile(&mut config, "b".to_string(), None, None, None)
        .expect("create b");
    config.state.active_profile = Some(ProfileName::from("b"));
    crate::profile::save_app_state(&config.state).expect("save state");

    let now_secs = crate::usage::now_epoch_secs();
    write_profile_cache(
        &ProfileName::from("a"),
        USAGE_CACHE_FILE,
        &crate::usage::UsageInfo {
            five_hour: Some(crate::usage::UsageWindow {
                utilization: 97.0,
                resets_at: Some(crate::usage::epoch_secs_to_iso(now_secs + 3600)),
            }),
            ..Default::default()
        },
    );

    // The shared config diverges from the disk state without ever being saved.
    let mut shared = crate::profile::load_config().expect("load");
    shared.state.active_profile = Some(ProfileName::from("a"));

    let fire = task_fire("conv-shared");
    let read = read_nudge(&fire, Some(&shared)).expect("eligible");
    assert_eq!(
        read.headroom.map(|h| h.used),
        Some(97.0),
        "the shared config's active `a` carries the cached window",
    );

    let disk_read = read_nudge(&fire, None).expect("eligible");
    assert!(
        disk_read.headroom.is_none(),
        "the disk config's active `b` carries no window, so a re-load would \
         have answered no headroom",
    );
}

// ── the real reader: disk cache + registry + decision-leg replay ────────────

/// A real tree: `a` is the active profile with a live 5h window at 97%; `b` is
/// a clear chain member; the chain is `[a, b]`. Every figure the replay and
/// the gate read comes off the seeded disk bytes through the production
/// readers — nothing is asserted from a hand-computed rate.
fn seed_exhausted_chain() {
    seed_chain_with_active_at(97.0);
}

/// The same tree with the active's cached window at `active_pct` instead of
/// 97% — the below-threshold fixtures, whose gate reads the same bytes.
fn seed_chain_with_active_at(active_pct: f64) {
    let mut config = crate::profile::AppConfig {
        state: crate::profile::AppState::default(),
        profiles: Vec::new(),
    };
    crate::actions::create_blank_profile(&mut config, "a".to_string(), None, None, None)
        .expect("create a");
    crate::actions::create_blank_profile(&mut config, "b".to_string(), None, None, None)
        .expect("create b");
    config.state.active_profile = Some(ProfileName::from("a"));
    config.state.fallback_chain = vec![ProfileName::from("a"), ProfileName::from("b")];
    crate::profile::save_app_state(&config.state).expect("save state");

    let now_secs = crate::usage::now_epoch_secs();
    let live_at = |pct: f64| crate::usage::UsageInfo {
        five_hour: Some(crate::usage::UsageWindow {
            utilization: pct,
            resets_at: Some(crate::usage::epoch_secs_to_iso(now_secs + 3600)),
        }),
        ..Default::default()
    };
    write_profile_cache(
        &ProfileName::from("a"),
        USAGE_CACHE_FILE,
        &live_at(active_pct),
    );
    write_profile_cache(&ProfileName::from("b"), USAGE_CACHE_FILE, &live_at(10.0));
}

/// Three distinct samples inside the lookback, rising to the live 97% — enough
/// for a measured rate without predicting what the regression answers.
fn seed_burn_history() {
    seed_burn_history_samples(88.0, 93.0, 96.0);
}

/// The same history, with the three older samples named explicitly — the
/// below-threshold fixtures need a steeper rise than the 97% one.
fn seed_burn_history_samples(oldest: f64, middle: f64, newest: f64) {
    let now_ms = crate::usage::now_ms();
    let at = |pct: f64| crate::usage::UsageInfo {
        five_hour: Some(crate::usage::UsageWindow {
            utilization: pct,
            resets_at: None,
        }),
        ..Default::default()
    };
    crate::testutil::write_usage_history(
        &ProfileName::from("a"),
        &[
            (now_ms - 3_000_000, at(oldest)),
            (now_ms - 1_800_000, at(middle)),
            (now_ms - 60_000, at(newest)),
        ],
    );
}

/// Verify lines 1 and 2 through the REAL reader: the disk cache, the burn
/// history, and the decision-leg walk replayed over the same bytes — not a
/// hand-built `NudgeRead`.
#[test]
fn the_real_reader_replays_the_decision_leg_over_the_disk_cache() {
    let _home = HomeSandbox::new();
    seed_exhausted_chain();
    seed_burn_history();
    let fire = task_fire("conv-real");

    // b is a clear chain member: the leg would switch onto it, so the nudge
    // stays silent.
    let read = read_nudge(&fire, None).expect("eligible and readable");
    assert!(
        read.headroom.is_some(),
        "the active window reached the reader"
    );
    assert!(read.chain_acts, "the walk saw a target to switch to");
    assert_eq!(
        nudge_note(&fire, &read),
        None,
        "a covered session stays silent"
    );

    // The chain holds only the spent active: the leg has nowhere to point, and
    // the nudge lands in that turn's context.
    let mut config = crate::profile::load_config().expect("reload");
    config.state.fallback_chain = vec![ProfileName::from("a")];
    crate::profile::save_app_state(&config.state).expect("save chain");

    let read = read_nudge(&fire, None).expect("still eligible");
    assert!(!read.chain_acts, "the walk has nowhere to point");
    let note = nudge_note(&fire, &read).expect("the nudge fires");
    assert!(
        note.starts_with("clauth note: 5h window 97% used ("),
        "the used figure is the disk-cached one: {note}",
    );
    assert!(
        note.ends_with(". no fallback is set; further agent spawns may fail with 429s."),
        "the approved tail: {note}",
    );
}

/// The chain-walk pre-gate keyed on the static threshold in r7 must key on the
/// projection arm in r8: a fire BELOW the threshold whose projection would
/// emit still has to run the walk, or the copy's "no fallback is set" claim
/// fires over a chain that would switch. The leg acts below the threshold only
/// with burn-aware switching armed and the floor at its 90 band minimum, so
/// this fixture sets both plus a 1 h poll horizon: the active sits at 92% with
/// a rate that projects past 100 inside the seeded hour, the leg's own
/// projection then judges it exhausted, `b` is clear, and the walk must answer
/// true — under the r7 pre-gate (`window_exhausted`, false at 92%) the walk
/// would be skipped and `chain_acts` would read false, the pin this test reds.
#[test]
fn a_below_threshold_projection_fire_still_replays_the_chain_walk() {
    let _home = HomeSandbox::new();
    seed_chain_with_active_at(92.0);
    seed_burn_history_samples(80.0, 86.0, 90.0);
    let mut config = crate::profile::load_config().expect("reload");
    config.state.burn_aware_switching = true;
    config.state.burn_switch_floor_pct = Some(90.0);
    config.state.burn_horizon_cap_ms = Some(3_600_000);
    config.state.refresh_interval_ms = 3_600_000;
    crate::profile::save_app_state(&config.state).expect("save state");
    let fire = task_fire("conv-walk-below");

    let read = read_nudge(&fire, None).expect("eligible and readable");
    assert!(
        read.chain_acts,
        "the walk ran on a below-threshold projection-arm fire"
    );
    assert_eq!(
        nudge_note(&fire, &read),
        None,
        "a covered session stays silent"
    );
}

/// The walk anchors on the account the gate resolved, never the global
/// active. A pinned runtime session sits on the profile its own
/// `CLAUDE_CONFIG_DIR` names even after a switch moves `active_profile`
/// elsewhere, and a walk anchored on the global active answers "the chain
/// would act" about that switch — which never moves this session. Here the
/// session resolves to `a`, not a chain member at all, while the global
/// active `b` is exhausted and `c` is clear: the global walk would land
/// `b -> c`, but nothing catches the session on `a`, whose own window is
/// burning to the cap, so the nudge must land.
#[test]
fn a_switch_that_never_moves_a_pinned_session_does_not_suppress_its_nudge() {
    let home = HomeSandbox::new();
    // Three profiles; the chain covers only `b` and `c`, and `b` is the
    // global active. The session's own account, `a`, sits outside the chain.
    let mut config = crate::profile::AppConfig {
        state: crate::profile::AppState::default(),
        profiles: Vec::new(),
    };
    crate::actions::create_blank_profile(&mut config, "a".to_string(), None, None, None)
        .expect("create a");
    crate::actions::create_blank_profile(&mut config, "b".to_string(), None, None, None)
        .expect("create b");
    crate::actions::create_blank_profile(&mut config, "c".to_string(), None, None, None)
        .expect("create c");
    config.state.active_profile = Some(ProfileName::from("b"));
    config.state.fallback_chain = vec![ProfileName::from("b"), ProfileName::from("c")];
    crate::profile::save_app_state(&config.state).expect("save state");

    let now_secs = crate::usage::now_epoch_secs();
    let live_at = |pct: f64| crate::usage::UsageInfo {
        five_hour: Some(crate::usage::UsageWindow {
            utilization: pct,
            resets_at: Some(crate::usage::epoch_secs_to_iso(now_secs + 3600)),
        }),
        ..Default::default()
    };
    // The session's own window at 97% with a measured burn; `b` exhausted and
    // `c` clear, so the walk anchored on the GLOBAL active would land a switch.
    write_profile_cache(&ProfileName::from("a"), USAGE_CACHE_FILE, &live_at(97.0));
    write_profile_cache(&ProfileName::from("b"), USAGE_CACHE_FILE, &live_at(97.0));
    write_profile_cache(&ProfileName::from("c"), USAGE_CACHE_FILE, &live_at(10.0));
    seed_burn_history();

    // Pinned runtime on `a`, no registry row: nothing may move it, and the
    // reader's row check stays silent — the fire is eligible.
    let _dir = crate::testutil::ConfigDirSandbox::new(
        &home,
        &home.home().join(".clauth/profiles/a/runtime-456-7"),
    );
    let fire = task_fire("conv-pinned");

    let read = read_nudge(&fire, None).expect("eligible and readable");
    assert!(
        !read.chain_acts,
        "the walk is anchored on the resolved account `a`, outside the chain: \
         nothing would catch the session"
    );
    let note = nudge_note(&fire, &read).expect("the nudge fires");
    assert!(
        note.starts_with("clauth note: 5h window 97% used ("),
        "the session's own window earns it: {note}",
    );
}

/// (c): a session the chain may move is covered by the account note one tool
/// call after the move, so its registry row silences the whole reader before
/// any other read. Both directions off the same fixture, so the `None` is the
/// row's doing, not the tree's — and a subagent fire stays silent on the same
/// tree a parent-scope fire reads.
#[test]
fn an_armed_session_silences_the_reader_before_any_read() {
    let home = HomeSandbox::new();
    seed_exhausted_chain();
    seed_burn_history();
    let fire = task_fire("conv-armed");
    assert!(
        read_nudge(&fire, None).is_some(),
        "unarmed on the same tree: eligible"
    );

    let mut sub = task_fire("conv-armed");
    sub.agent_id = Some("a4a894a1be41b92bf".to_string());
    assert!(
        read_nudge(&sub, None).is_none(),
        "a subagent fire answers for nobody, same tree",
    );

    // The hook child finds its row through the runtime sid in CLAUDE_CONFIG_DIR
    // (the payload's session_id is Claude Code's conversation id).
    let _dir = crate::testutil::ConfigDirSandbox::new(
        &home,
        &home.home().join(".clauth/profiles/a/runtime-123-1"),
    );
    crate::live_sessions::register(&crate::live_sessions::LiveSession {
        session_id: "123-1".to_string(),
        start_profile: "a".to_string(),
        pid: std::process::id(),
        started_at: 0,
        cwd: None,
        isolated: false,
        follows_chain: true,
        intended_member: None,
        chain_cursor: None,
        current_member: None,
        last_swap_at: None,
        launch_store: None,
    })
    .expect("row");

    assert!(
        read_nudge(&fire, None).is_none(),
        "the armed row silences the reader before any other read",
    );
}

/// The sid parser is the nudge's door into the registry: only a per-session
/// runtime dir name yields a row key, and the legacy bare stems plus unrelated
/// names all read as "no row" — which is also "not armed".
#[test]
fn the_runtime_dir_sid_parser_accepts_only_per_session_dirs() {
    assert_eq!(
        crate::runtime::sid_of_runtime_dir_name("runtime-123-4").as_deref(),
        Some("123-4"),
    );
    assert_eq!(
        crate::runtime::sid_of_runtime_dir_name("runtime-isolated-123-4").as_deref(),
        Some("123-4"),
    );
    for bare in [
        "runtime",
        "runtime-isolated",
        "sessions-1-2",
        "runtime-x",
        "profiles",
    ] {
        assert_eq!(
            crate::runtime::sid_of_runtime_dir_name(bare),
            None,
            "{bare}"
        );
    }
}
