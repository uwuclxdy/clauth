#![allow(clippy::unwrap_used, clippy::expect_used)]

//! `clauth jobs` coverage: the four phases a row can be in, what each time
//! column means, and the `--json` field set. Home-sandboxed so every seeded
//! record lands in a tempdir, never the operator's real `~/.clauth/jobs`.

use super::*;
use crate::mcp::jobs::{
    RecordKind, RunningSpec, jobs_dir, write_heartbeat, write_heartbeat_with_session, write_running,
};
use crate::testutil::HomeSandbox;

/// Epoch ms every fixture is dated against. A real 2026 clock rather than a
/// round synthetic one: the corpse verdict is decided by comparing a record's
/// anchor against `now`, and a far-future `now` routes every fixture into one
/// branch.
const NOW: u64 = 1_786_000_000_000;

/// The streaming shape a default `delegate({background: true})` reserves.
fn spec(job_id: &str, started_at: u64) -> RunningSpec {
    RunningSpec {
        job_id: job_id.to_string(),
        profile: "work".to_string(),
        started_at,
        recorded_at: started_at,
        timeout_secs: 0,
        endpoint: None,
        provider: None,
        isolated: false,
        idle_secs: Some(300),
        kind: RecordKind::Collectable,
    }
}

/// A `done` file with an explicit `done_at`: `write_done` stamps the real clock,
/// and every age assertion here is about a stamp the test has to choose.
fn seed_done(job_id: &str, started_at: u64, done_at: u64) {
    let dir = jobs_dir().unwrap();
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join(format!("{job_id}.json")),
        serde_json::to_vec(&serde_json::json!({
            "job_id": job_id,
            "profile": "work",
            "state": "done",
            "started_at": started_at,
            "done_at": done_at,
            // NON-EMPTY on purpose: `jobs_cli::row` blanks a done record's tail
            // deliberately (its envelope carries the whole result), and with an
            // absent `tail` field the fixture satisfied that assertion whatever
            // the code did.
            "tail": "the finished run said this",
            "envelope": { "result": "ok" },
        }))
        .unwrap(),
    )
    .unwrap();
}

/// The same bytes under the spelling a BLOCKING run's spawn mints.
fn live_spec(job_id: &str, started_at: u64) -> RunningSpec {
    RunningSpec {
        kind: RecordKind::Liveness,
        ..spec(job_id, started_at)
    }
}

/// The row for one id out of a whole listing, found by its id rather than by
/// position: an ordering change must not silently move which row an assertion
/// is reading.
fn row_for<'a>(rows: &'a [JobRow], job_id: &str) -> &'a JobRow {
    rows.iter()
        .find(|r| r.job_id == job_id)
        .unwrap_or_else(|| panic!("no row for {job_id} in {:?}", ids(rows)))
}

fn ids(rows: &[JobRow]) -> Vec<String> {
    rows.iter().map(|r| r.job_id.clone()).collect()
}

/// The one line of the rendered table that names `job_id`, found by that id
/// rather than by line number: reading line N would pass whatever the code put
/// there.
fn table_line<'a>(table: &'a str, job_id: &str) -> &'a str {
    table
        .lines()
        .find(|l| l.contains(job_id))
        .unwrap_or_else(|| panic!("no table line names {job_id}:\n{table}"))
}

/// The four situations a record can be in each get their own word, and the two
/// LIVE ones are told apart by the spelling on disk alone.
///
/// This is the vocabulary both text surfaces share, so the operator's table and
/// the model's listing cannot name one record two ways.
#[test]
fn each_stored_shape_reads_as_its_own_phase() {
    let _home = HomeSandbox::new();
    write_running(&spec("d-bg-0", NOW - 60_000)).unwrap();
    write_running(&live_spec("d-blocking-0", NOW - 30_000)).unwrap();
    seed_done("d-fin-0", NOW - 600_000, NOW - 120_000);
    // Silent past the corpse window, which is what makes a record an orphan.
    write_running(&spec(
        "d-dead-0",
        NOW - crate::mcp::jobs::RUNNING_TTL_MS - 60_000,
    ))
    .unwrap();

    let rows = rows(NOW);

    assert_eq!(row_for(&rows, "d-bg-0").phase.label(), "running");
    assert_eq!(row_for(&rows, "d-blocking-0").phase.label(), "blocking");
    assert_eq!(row_for(&rows, "d-fin-0").phase.label(), "done");
    assert_eq!(row_for(&rows, "d-dead-0").phase.label(), "orphaned");

    // The word is not the whole answer: only two of the four name a record a
    // `monitor` call could take a result from.
    assert!(row_for(&rows, "d-bg-0").phase.is_collectable());
    assert!(row_for(&rows, "d-fin-0").phase.is_collectable());
    assert!(!row_for(&rows, "d-blocking-0").phase.is_collectable());
    assert!(!row_for(&rows, "d-dead-0").phase.is_collectable());
}

/// Each time column answers ONE question, and renders `-` on a record that has
/// no answer to it rather than borrowing the other column's meaning.
///
/// The done row is the discriminating one: it still carries `started_at`, so an
/// elapsed figure IS derivable there — and it would count the time the envelope
/// then sat waiting to be collected, which is not what the word means.
#[test]
fn the_time_columns_never_share_a_meaning() {
    let _home = HomeSandbox::new();
    write_heartbeat(&spec("d-live-0", NOW - 125_000), NOW - 5_000, "building").unwrap();
    seed_done("d-fin-0", NOW - 3_600_000, NOW - 90_000);

    let rows = rows(NOW);
    let live = row_for(&rows, "d-live-0");
    let done = row_for(&rows, "d-fin-0");

    // All four figures differ, so no two of these cells could be swapped and
    // still pass.
    assert_eq!(
        elapsed_cell(live),
        "2m 5s",
        "a live run's elapsed is its own"
    );
    assert_eq!(
        elapsed_cell(done),
        "-",
        "a finished run has no elapsed left to report"
    );
    // AGE is the store's own retention stamp on every row: a running record's
    // freshest sign of life, a done one's finish. The live row has been going
    // two minutes and spoke five seconds ago; the done row started an hour
    // before `now` and finished ninety seconds ago. Neither column can borrow
    // the other's number.
    assert_eq!(age_cell(live.age_secs), "5s");
    assert_eq!(age_cell(done.age_secs), "1m 30s");

    // The zero boundary, which had no assertion behind it: `humanize_duration`
    // spells zero `now`, and a column of lengths cannot hold an instant.
    assert_eq!(age_cell(0), "0s");
    assert_eq!(duration_cell(0), "0s");
}

/// A run that has produced nothing reads `never`, not `-`.
///
/// Three answers, not two: `-` means no run is going, `never` means one is and
/// has said nothing. Collapsing them reports a stuck delegate exactly like a
/// finished one, which is the whole signal an operator opened this table for.
#[test]
fn a_silent_run_reads_never_and_a_finished_one_reads_nothing() {
    let _home = HomeSandbox::new();
    write_running(&spec("d-quiet-0", NOW - 45_000)).unwrap();
    write_heartbeat(&spec("d-talky-0", NOW - 45_000), NOW - 5_000, "building").unwrap();
    seed_done("d-fin-0", NOW - 60_000, NOW - 10_000);

    let rows = rows(NOW);

    assert_eq!(last_output_cell(row_for(&rows, "d-quiet-0")), "never");
    assert_eq!(last_output_cell(row_for(&rows, "d-talky-0")), "5s");
    assert_eq!(last_output_cell(row_for(&rows, "d-fin-0")), "-");
}

/// A record written before the liveness fields existed knows nothing about its
/// own output, and must not be reported as a run that has said nothing.
///
/// Driven through real bytes an older server wrote rather than a `RunningSpec`
/// built here: a spec compiles against whatever the fields are today and proves
/// nothing about what is on disk.
#[test]
fn a_pre_liveness_record_reports_no_output_figure_at_all() {
    let _home = HomeSandbox::new();
    let dir = crate::mcp::jobs::jobs_dir().unwrap();
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("d-legacy-0.json"),
        format!(
            r#"{{"job_id":"d-legacy-0","profile":"work","state":"running","started_at":{}}}"#,
            NOW - 30_000
        ),
    )
    .unwrap();

    let rows = rows(NOW);
    let legacy = row_for(&rows, "d-legacy-0");

    assert_eq!(legacy.phase.label(), "running");
    assert_eq!(
        last_output_cell(legacy),
        "-",
        "unrecorded is not the same fact as `has said nothing`"
    );
    assert_eq!(
        kill_cell(legacy),
        "-",
        "that server recorded no deadline to count down to"
    );
}

/// Both deadlines render where both exist, and an absent one is clauth knowing
/// there is none rather than a zero countdown.
#[test]
fn the_kill_column_names_whichever_deadlines_the_run_has() {
    let _home = HomeSandbox::new();
    // Streaming: an idle guard and no wall clock at all.
    write_running(&spec("d-stream-0", NOW - 65_000)).unwrap();
    // A caller-pinned `--output-format`: the wall is the only deadline left.
    write_running(&RunningSpec {
        timeout_secs: 900,
        idle_secs: None,
        ..spec("d-pinned-0", NOW - 65_000)
    })
    .unwrap();

    let rows = rows(NOW);

    assert_eq!(kill_cell(row_for(&rows, "d-stream-0")), "idle 3m 55s");
    assert_eq!(kill_cell(row_for(&rows, "d-pinned-0")), "wall 13m");
}

/// An empty store is a success with a line naming what would fill it, never a
/// blank or a failure: no delegate having run recently is the normal state, and
/// exiting non-zero for it breaks every script that polls this.
#[test]
fn an_empty_store_renders_a_named_empty_state_and_an_empty_json_array() {
    let _home = HomeSandbox::new();

    let table = render_table(&rows(NOW));
    assert!(
        table.contains("no delegate jobs"),
        "the empty table says so: {table}"
    );
    assert!(
        table.contains("background"),
        "and names what starts one: {table}"
    );

    let json: serde_json::Value = serde_json::from_str(&rows_json(&rows(NOW))).unwrap();
    assert_eq!(
        json,
        serde_json::json!([]),
        "an empty store is an empty array, so `jq` needs no special case"
    );
}

/// A record OUTLIVES the server that wrote it, and this is what that buys: the
/// row for a job a dead `clauth mcp` left behind still carries the session id
/// linking it to its transcript, which is the whole reason the heartbeat stamps
/// one onto a running record.
///
/// Driven through the PRODUCTION writer rather than hand-built bytes: a fixture
/// spelling the field itself would pass while the beat wrote nothing.
#[test]
fn a_dead_servers_row_still_carries_the_session_id_its_record_kept() {
    let _home = HomeSandbox::new();
    const HANDLE: &str = "8fbb04c1-2e3d-4a55-9c17-6d0e2b7a1f39";
    // Silent past the corpse window: the shape a killed server leaves behind.
    let dead = spec("d-dead-0", NOW - crate::mcp::jobs::RUNNING_TTL_MS - 60_000);
    write_heartbeat_with_session(
        &dead,
        NOW - crate::mcp::jobs::RUNNING_TTL_MS - 30_000,
        "halfway through",
        Some(HANDLE),
    )
    .unwrap();
    // A streaming run whose first event has not named a session yet.
    write_running(&spec("d-quiet-0", NOW - 3_000)).unwrap();
    seed_done("d-fin-0", NOW - 600_000, NOW - 120_000);

    let rows = rows(NOW);
    assert_eq!(
        row_for(&rows, "d-dead-0").phase.label(),
        "orphaned",
        "the fixture is the dead-server shape, not a live run"
    );

    let json: serde_json::Value = serde_json::from_str(&rows_json(&rows)).unwrap();
    let array = json.as_array().expect("a JSON array").clone();
    let by_id = |id: &str| {
        array
            .iter()
            .find(|r| r["job_id"] == id)
            .unwrap_or_else(|| panic!("no row for {id} in {json}"))
            .clone()
    };

    assert_eq!(
        by_id("d-dead-0")["session_id"],
        serde_json::json!(HANDLE),
        "the orphan's row hands back the handle `delegate({{resume}})` takes"
    );
    // These pin the VALUE; the key's presence on every row is
    // `the_json_row_carries_every_key_on_every_state`'s, one test down, since
    // serde answers a missing key with `Null` too.
    assert_eq!(
        by_id("d-quiet-0")["session_id"],
        serde_json::Value::Null,
        "a run no event has named a session for yet claims no handle"
    );
    assert_eq!(
        by_id("d-fin-0")["session_id"],
        serde_json::Value::Null,
        "a finished run's handle rides its envelope, never this key"
    );
}

/// Whether a handed-back `session_id` is a handle at all is the run's isolation,
/// and the id alone cannot say: an isolated run's transcript died with its
/// throwaway tree, so `delegate({resume})` refuses the very id the row prints.
#[test]
fn an_isolated_runs_row_is_marked_isolated() {
    let _home = HomeSandbox::new();
    let mut isolated = spec("d-iso-0", NOW - 60_000);
    isolated.isolated = true;
    write_heartbeat(&isolated, NOW - 1_000, "in a throwaway tree").unwrap();
    write_heartbeat(
        &spec("d-shared-0", NOW - 60_000),
        NOW - 1_000,
        "in the global store",
    )
    .unwrap();

    let json: serde_json::Value = serde_json::from_str(&rows_json(&rows(NOW))).unwrap();
    let array = json.as_array().expect("a JSON array").clone();
    let by_id = |id: &str| {
        array
            .iter()
            .find(|r| r["job_id"] == id)
            .unwrap_or_else(|| panic!("no row for {id} in {json}"))
            .clone()
    };

    assert_eq!(
        by_id("d-iso-0")["isolated"],
        serde_json::json!(true),
        "an isolated run's row says its handle is not one"
    );
    assert_eq!(
        by_id("d-shared-0")["isolated"],
        serde_json::json!(false),
        "a shared run's row says its handle resolves"
    );
}

/// The `--json` field set is FIXED: every key is present on every row, and a
/// figure the record does not have is `null`.
///
/// The opposite of the MCP surface's absent-means-structurally-none rule, and
/// deliberately: a model reads prose and pays for every key it is handed, while
/// a `jq` filter pays for every key it has to probe for.
#[test]
fn the_json_row_carries_every_key_on_every_state() {
    let _home = HomeSandbox::new();
    write_heartbeat(&spec("d-live-0", NOW - 125_000), NOW - 5_000, "building").unwrap();
    seed_done("d-fin-0", NOW - 600_000, NOW - 120_000);

    let rows = rows(NOW);
    let json: serde_json::Value = serde_json::from_str(&rows_json(&rows)).unwrap();
    let array = json.as_array().expect("a JSON array");
    assert_eq!(array.len(), 2);

    const KEYS: [&str; 12] = [
        "job_id",
        "profile",
        "state",
        "collectable",
        "session_id",
        "isolated",
        "age_secs",
        "elapsed_secs",
        "last_output_secs_ago",
        "idle_kill_in_secs",
        "wall_kill_in_secs",
        "tail",
    ];
    for row in array {
        let obj = row.as_object().expect("each row is an object");
        for key in KEYS {
            assert!(obj.contains_key(key), "row is missing {key}: {row}");
        }
        assert_eq!(obj.len(), KEYS.len(), "row carries an unlisted key: {row}");
    }

    let live = array
        .iter()
        .find(|r| r["job_id"] == "d-live-0")
        .expect("the live row");
    assert_eq!(live["state"], "running");
    assert_eq!(live["collectable"], true);
    assert_eq!(live["elapsed_secs"], 125);
    assert_eq!(live["last_output_secs_ago"], 5);
    assert_eq!(live["idle_kill_in_secs"], 295);
    assert_eq!(
        live["wall_kill_in_secs"],
        serde_json::Value::Null,
        "a streaming run has no wall clock, and null is how that reads here"
    );
    assert_eq!(live["tail"], "building");
    assert_eq!(
        live["isolated"], false,
        "a shared run's session id IS a resume handle, and the flag says so"
    );

    let done = array
        .iter()
        .find(|r| r["job_id"] == "d-fin-0")
        .expect("the done row");
    assert_eq!(done["state"], "done");
    assert_eq!(done["elapsed_secs"], serde_json::Value::Null);
    assert_eq!(done["last_output_secs_ago"], serde_json::Value::Null);
    assert_eq!(
        done["tail"], "",
        "a done envelope carries the whole result; a tail beside it says nothing new \
         — and the fixture's own bytes DO carry one, so this discriminates"
    );
}

/// A delegate's own words reach the operator's terminal with every control
/// character stripped.
///
/// This is another account's model output arriving verbatim, and the table is
/// the first clauth surface that prints it raw — the MCP replies are JSON and
/// the TUI goes through ratatui. `tail_line` collapses whitespace runs, which
/// leaves `\x1b`, `\x07` and `\x00` untouched: none of them is
/// `char::is_whitespace`. An escape sequence would reach the terminal intact,
/// and truncation could cut one in half.
///
/// `--json` is deliberately NOT stripped: serde escapes C0 as `\uXXXX`, so the
/// raw text survives there for anyone who wants it.
#[test]
fn the_table_strips_control_characters_out_of_a_delegates_words() {
    let _home = HomeSandbox::new();
    write_heartbeat(
        &spec("d-live-0", NOW - 30_000),
        NOW - 5_000,
        "before\u{1b}[31mAFTER\u{7}\u{0}end",
    )
    .unwrap();

    let rows = rows(NOW);
    let table = render_table(&rows);

    assert!(
        table.contains("    \"before[31mAFTERend\""),
        "every control char is gone and the text around them survives:\n{table:?}"
    );
    assert!(
        !table.chars().any(|c| c.is_control() && c != '\n'),
        "no control character reaches the terminal:\n{table:?}"
    );

    // The raw bytes still reach a machine reader, escaped rather than dropped.
    let json: serde_json::Value = serde_json::from_str(&rows_json(&rows)).unwrap();
    assert_eq!(
        json[0]["tail"], "before\u{1b}[31mAFTER\u{7}\u{0}end",
        "`--json` keeps what the delegate actually wrote"
    );
}

/// A delegate cannot make its words READ as something else either.
///
/// `char::is_control()` is false for Unicode's bidi formatting characters, so
/// the control filter alone does not reach them — and `U+202E` reverses the
/// display order of the rest of the line, which is the Trojan-Source class on
/// the one surface an operator opened to see what a delegate said.
///
/// The last assertion is the deliberate BOUNDARY, pinned so that keeping the
/// zero-width orthographic marks is a decision rather than an oversight: ZWNJ
/// and ZWJ carry meaning inside real words and cannot reorder anything.
#[test]
fn the_table_strips_bidi_overrides_but_keeps_orthographic_zero_width_marks() {
    let _home = HomeSandbox::new();
    // One of each: override, isolate, directional mark, then the marks that stay.
    write_heartbeat(
        &spec("d-live-0", NOW - 30_000),
        NOW - 5_000,
        "safe\u{202e}reversed\u{2066}iso\u{200f}mark\u{061c}alm\u{200b}zwsp\u{200c}zwnj\u{200d}zwj\u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}",
    )
    .unwrap();

    let rows = rows(NOW);
    let table = render_table(&rows);

    for (name, hazard) in [
        ("RLO override", '\u{202e}'),
        ("LRI isolate", '\u{2066}'),
        ("RLM directional mark", '\u{200f}'),
        // The twelfth `Bidi_Control` codepoint, and the one an earlier version
        // of the set missed while calling itself closed. It is RLM's
        // Arabic-script twin, so treating them differently was arbitrary.
        ("ALM directional mark", '\u{061c}'),
    ] {
        assert!(
            !table.contains(hazard),
            "{name} reached the terminal: {table:?}"
        );
    }
    // The surrounding text is untouched — the filter drops the hazard, not the
    // words around it.
    assert!(
        table.contains("safereversedisomarkalm"),
        "the delegate's actual words survive: {table:?}"
    );
    // The load-bearing half of the carve-out: ZWJ composes every multi-person
    // emoji, so stripping it would shatter them.
    assert!(
        table.contains("\u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}"),
        "a ZWJ-composed family emoji survives intact: {table:?}"
    );
    // BOUNDARY: these are letters-level marks, not display hazards, and they stay.
    for (name, kept) in [
        ("ZWSP", '\u{200b}'),
        ("ZWNJ", '\u{200c}'),
        ("ZWJ", '\u{200d}'),
    ] {
        assert!(
            table.contains(kept),
            "{name} is orthographic and must not be stripped: {table:?}"
        );
    }

    // `--json` still carries every byte the delegate wrote, hazards included,
    // and emits them RAW: `serde_json` escapes below 0x20 only, so the bidi
    // class goes out as literal UTF-8. Pinned on the emitted TEXT, never on a
    // re-parsed value — parsing hides exactly the property under test.
    let emitted = rows_json(&rows);
    assert!(
        emitted.contains('\u{202e}') && emitted.contains('\u{061c}'),
        "`--json` returns the true bytes: {emitted:?}"
    );
    assert!(
        !emitted.to_lowercase().contains("\\u202e"),
        "and does NOT escape them, whatever the C0 case does: {emitted:?}"
    );
}

/// A store that exists and cannot be opened is not a store with nothing in it.
///
/// `jobs::list` swallows a `read_dir` failure into an empty Vec, which is right
/// for a reader with other work; turning that into "no delegate jobs" would be
/// an affirmative operator-facing claim about a directory nothing opened. A
/// MISSING directory is a different fact and stays exit 0.
///
/// The unreadable case is staged by putting a FILE where the jobs dir belongs,
/// which fails `read_dir` with `NotADirectory` for every uid — a `chmod 000`
/// would read as openable under a root CI.
#[test]
fn an_unreadable_job_store_is_an_error_and_a_missing_one_is_not() {
    let _home = HomeSandbox::new();

    // Missing: the normal state of a box that has never run a delegate.
    assert!(
        run(true).is_ok(),
        "a store that was never created is empty, not broken"
    );

    let dir = jobs_dir().unwrap();
    std::fs::create_dir_all(dir.parent().unwrap()).unwrap();
    std::fs::write(&dir, b"not a directory").unwrap();

    let err = run(false).expect_err("an unopenable store must not report as empty");
    let msg = err.to_string();
    assert!(
        msg.contains("cannot read the job store"),
        "the failure names the operation: {msg}"
    );
    assert!(
        msg.contains(&dir.display().to_string()),
        "and the path it failed on: {msg}"
    );
}

/// The table names each row's id, account and state, and puts a running job's
/// own words on their own indented line rather than in a column that could wrap.
#[test]
fn the_table_names_each_row_and_quotes_a_tail_on_its_own_line() {
    let _home = HomeSandbox::new();
    write_heartbeat(
        &spec("d-live-0", NOW - 125_000),
        NOW - 5_000,
        "building the crate",
    )
    .unwrap();

    let table = render_table(&rows(NOW));
    let line = table_line(&table, "d-live-0");

    assert!(line.starts_with("running"), "state leads the row: {line:?}");
    assert!(
        table.starts_with("STATE") && table.contains("KILL IN"),
        "the header names a countdown, not an action:\n{table}"
    );
    assert!(line.contains("work"), "the account is named: {line:?}");
    assert!(
        !line.contains("building the crate"),
        "the tail is not a column: {line:?}"
    );
    assert!(
        table.contains("    \"building the crate\""),
        "the tail rides its own indented, quoted line:\n{table}"
    );
}
