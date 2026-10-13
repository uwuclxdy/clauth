#![allow(clippy::unwrap_used, clippy::expect_used)]

//! The uncollected-results leg: which records a resumed conversation is told
//! about, and the exact copy. Home-sandboxed so every seeded record lands in a
//! tempdir, never the real `~/.clauth/jobs`.

use super::*;
use crate::mcp::jobs::{Claim, Claimant, DONE_TTL_MS, claim, jobs_dir};
use crate::testutil::HomeSandbox;

/// Epoch ms every fixture is dated against: a real 2026 clock, so the Done TTL
/// and the corpse verdict both read a record the way they would in production.
const NOW: u64 = 1_786_000_000_000;

const SESSION: &str = "5e55-a";
const OTHER: &str = "5e55-b";

const TWO: &str = "clauth note: background delegates this session started have results waiting uncollected: `d-b` on `glm1`, `d-a` on `DS1`. `monitor` with their job_ids collects them.";

fn fire(event: &str, source: Option<&str>, agent_id: Option<&str>) -> Payload {
    Payload {
        event: event.to_string(),
        session_id: SESSION.to_string(),
        agent_id: agent_id.map(str::to_string),
        tool_name: None,
        source: source.map(str::to_string),
        transcript: None,
    }
}

fn resume() -> Payload {
    fire("SessionStart", Some("resume"), None)
}

fn one(id: &str, profile: &str) -> String {
    format!(
        "clauth note: a background delegate this session started has a result waiting uncollected: `{id}` on `{profile}`. `monitor` with its job_id collects it."
    )
}

fn write_record(record: &serde_json::Value) {
    let dir = jobs_dir().unwrap();
    std::fs::create_dir_all(&dir).unwrap();
    let id = record["job_id"].as_str().unwrap();
    std::fs::write(
        dir.join(format!("{id}.json")),
        serde_json::to_vec(record).unwrap(),
    )
    .unwrap();
}

/// A finished background job whose result still sits in the store, finished
/// `ago` ms before [`NOW`]. The finish is the store's sort key, so it fixes the
/// order the note names jobs in: the smaller `ago`, the earlier.
fn seed_done(job_id: &str, profile: &str, host_session: Option<&str>, ago: u64) {
    let mut record = serde_json::json!({
        "job_id": job_id,
        "profile": profile,
        "state": "done",
        "started_at": NOW - ago - 500,
        "done_at": NOW - ago,
        "envelope": { "result": "ok" },
    });
    if let Some(host) = host_session {
        record["host_session"] = host.into();
    }
    write_record(&record);
}

fn seed_many(n: u64) {
    for i in 0..n {
        seed_done(&format!("d-{i:02}"), "DS1", Some(SESSION), 1_000 - i);
    }
}

#[test]
fn a_resume_names_this_sessions_uncollected_results_newest_first() {
    let _home = HomeSandbox::new();
    seed_done("d-a", "DS1", Some(SESSION), 2_000);
    seed_done("d-b", "glm1", Some(SESSION), 1_000);

    assert_eq!(note_at(&resume(), NOW).as_deref(), Some(TWO));
}

#[test]
fn one_result_reads_in_the_singular() {
    let _home = HomeSandbox::new();
    seed_done("d-a", "DS1", Some(SESSION), 1_000);

    assert_eq!(note_at(&resume(), NOW), Some(one("d-a", "DS1")));
}

#[test]
fn a_collected_result_is_never_named() {
    let _home = HomeSandbox::new();
    seed_done("d-a", "DS1", Some(SESSION), 2_000);
    seed_done("d-b", "glm1", Some(SESSION), 1_000);

    assert!(matches!(claim("d-a", Claimant::Monitor), Claim::Owned(_)));
    assert_eq!(note_at(&resume(), NOW), Some(one("d-b", "glm1")));

    assert!(matches!(claim("d-b", Claimant::Hook), Claim::Owned(_)));
    assert_eq!(note_at(&resume(), NOW), None);
}

/// Another conversation's result, one whose record names no conversation, a
/// run still going, and a crashed run's tombstone (no result to collect) are
/// all left out; the one finished result this conversation started is named.
#[test]
fn only_this_sessions_finished_results_are_named() {
    let _home = HomeSandbox::new();
    seed_done("d-mine", "DS1", Some(SESSION), 4_000);
    seed_done("d-foreign", "DS2", Some(OTHER), 3_000);
    seed_done("d-unknown", "DS3", None, 2_000);
    write_record(&serde_json::json!({
        "job_id": "d-running",
        "profile": "DS4",
        "state": "running",
        "started_at": NOW - 1_000,
        "recorded_at": NOW - 1_000,
        "host_session": SESSION,
    }));
    write_record(&serde_json::json!({
        "job_id": "d-crashed",
        "profile": "DS5",
        "state": "done",
        "started_at": NOW - 600,
        "done_at": NOW - 500,
        "crashed": true,
        "host_session": SESSION,
    }));

    assert_eq!(note_at(&resume(), NOW), Some(one("d-mine", "DS1")));
}

/// The startup sweep reaps a `done` record once it is past the Done TTL, so
/// the note never offers one: `monitor` would answer it unknown. The boundary
/// is the sweep's own: exactly at the TTL it is kept.
#[test]
fn a_result_past_the_done_ttl_is_never_named() {
    let _home = HomeSandbox::new();
    seed_done("d-kept", "DS1", Some(SESSION), DONE_TTL_MS);
    seed_done("d-reaped", "DS2", Some(SESSION), DONE_TTL_MS + 1);

    assert_eq!(note_at(&resume(), NOW), Some(one("d-kept", "DS1")));
}

/// A resume names them; a compaction names them again, since it drops what
/// the resume injected. Every other fire, and any fire inside a subagent, is
/// silent.
#[test]
fn only_a_main_scope_resume_or_compaction_names_them() {
    let _home = HomeSandbox::new();
    seed_done("d-a", "DS1", Some(SESSION), 1_000);

    for (event, source, agent, want) in [
        ("SessionStart", Some("resume"), None, true),
        ("SessionStart", Some("compact"), None, true),
        ("SessionStart", Some("startup"), None, false),
        ("SessionStart", Some("clear"), None, false),
        ("SessionStart", Some("fork"), None, false),
        ("SessionStart", None, None, false),
        ("UserPromptSubmit", Some("resume"), None, false),
        ("SessionStart", Some("resume"), Some("agent-1"), false),
    ] {
        assert_eq!(
            note_at(&fire(event, source, agent), NOW),
            want.then(|| one("d-a", "DS1")),
            "{event} / {source:?} / {agent:?}"
        );
    }
}

#[test]
fn ten_results_are_all_named() {
    let _home = HomeSandbox::new();
    seed_many(10);

    assert_eq!(
        note_at(&resume(), NOW).as_deref(),
        Some(
            "clauth note: background delegates this session started have results waiting uncollected: `d-09` on `DS1`, `d-08` on `DS1`, `d-07` on `DS1`, `d-06` on `DS1`, `d-05` on `DS1`, `d-04` on `DS1`, `d-03` on `DS1`, `d-02` on `DS1`, `d-01` on `DS1`, `d-00` on `DS1`. `monitor` with their job_ids collects them."
        )
    );
}

#[test]
fn past_ten_results_the_rest_are_counted() {
    let _home = HomeSandbox::new();
    seed_many(11);

    assert_eq!(
        note_at(&resume(), NOW).as_deref(),
        Some(
            "clauth note: background delegates this session started have results waiting uncollected: `d-10` on `DS1`, `d-09` on `DS1`, `d-08` on `DS1`, `d-07` on `DS1`, `d-06` on `DS1`, `d-05` on `DS1`, `d-04` on `DS1`, `d-03` on `DS1`, `d-02` on `DS1`, `d-01` on `DS1`, and 1 more (`clauth jobs` lists them). `monitor` with their job_ids collects them."
        )
    );
}
