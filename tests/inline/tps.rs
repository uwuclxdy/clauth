#![allow(clippy::unwrap_used, clippy::expect_used)]
#![allow(unsafe_code)]
//! The tps producer against sandboxed transcript stores: speed, the windowed
//! rate, growing partials, and attribution (unattributed, isolated runtime
//! after a swap, the hook's resolved account). Expected values are hand
//! computed from the fixture's timestamps.

use std::path::{Path, PathBuf};

use super::*;
use crate::testutil::HomeSandbox;

/// The record's measured response: 1222 output tokens from the latest user
/// line `20:46:09.417` (an earlier prompt at `20:45:50.000` is not the start)
/// to the id's last line, the text block at `20:46:23.019` (its thinking block,
/// carrying the same 1222, landed 5 ms earlier at `END`).
const FIXTURE: &str = include_str!("../fixtures/tps-one-response.jsonl");
const FIXTURE_SESSION: &str = "0c1f6a52-7d4e-4b0a-9a51-3f2b8e6c1d01";
const START: &str = "2026-09-22T20:46:09.417Z";
/// The fixture's first block line; the synthetic one-line responses below end
/// here, so their span is 13.597 s.
const END: &str = "2026-09-22T20:46:23.014Z";
const FIXTURE_END: &str = "2026-09-22T20:46:23.019Z";
const MODEL: &str = "claude-opus-5-5";

fn ms(iso: &str) -> u64 {
    u64::try_from(
        chrono::DateTime::parse_from_rfc3339(iso)
            .unwrap()
            .timestamp_millis(),
    )
    .unwrap()
}

fn iso(ms: u64) -> String {
    chrono::DateTime::from_timestamp_millis(i64::try_from(ms).unwrap())
        .unwrap()
        .format("%Y-%m-%dT%H:%M:%S%.3fZ")
        .to_string()
}

fn user(ts: &str, session: &str) -> String {
    serde_json::json!({
        "type": "user",
        "timestamp": ts,
        "sessionId": session,
        "uuid": format!("u-{ts}"),
        "message": {"role": "user", "content": "go"},
    })
    .to_string()
}

fn assistant(ts: &str, session: &str, id: &str, output: u64) -> String {
    serde_json::json!({
        "type": "assistant",
        "timestamp": ts,
        "sessionId": session,
        "uuid": format!("a-{id}-{ts}"),
        "message": {
            "id": id,
            "model": MODEL,
            "role": "assistant",
            "content": [{"type": "text", "text": "ok"}],
            "usage": {
                "input_tokens": 12,
                "output_tokens": output,
                "cache_read_input_tokens": 0,
                "cache_creation_input_tokens": 0,
            },
        },
    })
    .to_string()
}

/// The Claude Code spend-cap 429 row, recognized by its own fields before the
/// usage filters, carrying no usable usage.
fn spend_cap_429(ts: &str) -> String {
    serde_json::json!({
        "type": "assistant",
        "timestamp": ts,
        "isApiErrorMessage": true,
        "error": "rate_limit",
        "apiError": "model_requires_usage_credits",
        "message": {"model": "<synthetic>", "role": "assistant"},
    })
    .to_string()
}

/// The same 429 row with its transcript line `uuid`, as Claude Code writes a
/// real one.
fn spend_cap_429_uuid(ts: &str) -> String {
    serde_json::json!({
        "type": "assistant",
        "timestamp": ts,
        "isApiErrorMessage": true,
        "error": "rate_limit",
        "apiError": "model_requires_usage_credits",
        "uuid": format!("429-{ts}"),
        "message": {"model": "<synthetic>", "role": "assistant"},
    })
    .to_string()
}

/// The same 429 row with its `uuid` field the raw JSON fragment `uuid`,
/// inserted verbatim so a malformed shape (a duplicate `uuid` key, an
/// out-of-range float, a lone-surrogate escape) reaches the parser
/// un-normalized.
fn spend_cap_429_raw_uuid(ts: &str, uuid: &str) -> String {
    format!(
        "{{\"type\":\"assistant\",\"timestamp\":\"{ts}\",\"isApiErrorMessage\":true,\"error\":\"rate_limit\",\"apiError\":\"model_requires_usage_credits\",{uuid}\"message\":{{\"model\":\"<synthetic>\",\"role\":\"assistant\"}}}}"
    )
}

/// A timestamp-less line `parse_line` ignores, `len` bytes without its newline,
/// so a test can place a 429 line at an exact byte offset.
fn pad_line(len: usize) -> String {
    let empty = serde_json::json!({"type": "progress", "pad": ""}).to_string();
    serde_json::json!({"type": "progress", "pad": "x".repeat(len.saturating_sub(empty.len()))})
        .to_string()
}

fn global_store(home: &Path) -> PathBuf {
    home.join(".claude/projects/-home-u-proj")
}

fn write_lines(path: &Path, lines: &[String]) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut body = lines.join("\n");
    body.push('\n');
    std::fs::write(path, body).unwrap();
}

fn append_lines(path: &Path, lines: &[String]) {
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new().append(true).open(path).unwrap();
    for l in lines {
        writeln!(f, "{l}").unwrap();
    }
}

/// One 1222-token response, 13.602 s from its user line to its end, landed as
/// a transcript in the global store with no owner anywhere.
fn seed_fixture(home: &Path) {
    let path = global_store(home).join(format!("{FIXTURE_SESSION}.jsonl"));
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, FIXTURE).unwrap();
}

fn close(actual: f64, want: f64, what: &str) {
    assert!(
        (actual - want).abs() < 1e-3,
        "{what}: got {actual}, want {want}"
    );
}

/// A day sum with its speed peak stripped, so a day-sum test pins
/// `(output, gen_ms)` without also pinning the cell's peak (a separate concern
/// the peaks tests cover).
fn sums_only(d: &DaySum) -> DaySum {
    DaySum {
        output: d.output,
        gen_ms: d.gen_ms,
        ..Default::default()
    }
}

// 1222 / 13.597 = 89.8727…, a synthetic one-line response ending at `END`.
const SPEED: f64 = 1222.0 / 13.597;
// 1222 / 13.602 = 89.8397…, the fixture ending at its last line.
const FIXTURE_SPEED: f64 = 1222.0 / 13.602;

#[test]
fn a_fixture_response_pins_speed_and_each_window_rate() {
    let home = HomeSandbox::new();
    seed_fixture(home.home());

    // 4 s after the end, the 10 s live window [end-6 s, end+4 s] holds 6 s of
    // the 13.602 s span: 1222 * 6 / 13.602 = 539.038 tokens -> 53.904 tok/s.
    let snap = Producer::new().tick(ms(FIXTURE_END) + 4_000);
    let s = &snap.sessions[FIXTURE_SESSION];
    close(s.figures.live.speed.unwrap(), FIXTURE_SPEED, "live speed");
    assert_eq!(format_figure(s.figures.live.speed.unwrap()), "90");
    close(
        s.figures.live.rate,
        1222.0 * 6.0 / 13.602 / 10.0,
        "live rate",
    );
    // The whole span sits inside 5 min and 1 h.
    close(s.figures.m5.rate, 1222.0 / 300.0, "5 min rate");
    assert_eq!(format_figure(s.figures.m5.rate), "4.1");
    close(s.figures.h1.rate, 1222.0 / 3600.0, "1 h rate");
    assert_eq!(format_figure(s.figures.h1.rate), "0.3");
    close(s.figures.m5.speed.unwrap(), FIXTURE_SPEED, "5 min speed");
    assert_eq!(snap.total, s.figures, "one session: the total is it");
    assert_eq!(snap.version, CACHE_VERSION);

    // 11 s after the end the live window no longer touches the span.
    let later = Producer::new().tick(ms(FIXTURE_END) + 11_000);
    let s = &later.sessions[FIXTURE_SESSION];
    assert_eq!(s.figures.live.speed, None, "no response touches live");
    close(s.figures.live.rate, 0.0, "live rate past the window");
    close(s.figures.m5.rate, 1222.0 / 300.0, "5 min still holds it");
}

#[test]
fn format_figure_spells_zero_bare_one_decimal_below_ten_whole_above() {
    for (v, want) in [
        (0.0, "0"),
        (0.04, "0"),
        (0.05, "0.1"),
        (0.34, "0.3"),
        (1.0, "1.0"),
        (9.94, "9.9"),
        (10.0, "10"),
        (89.87, "90"),
        (1234.4, "1234"),
    ] {
        assert_eq!(format_figure(v), want, "{v}");
    }
}

#[test]
fn growing_partials_give_the_end_landing_speed_counted_once() {
    let home = HomeSandbox::new();
    let path = global_store(home.home()).join("grow.jsonl");
    let start = ms(START);
    // Two partials of one message id land first, the final line later.
    write_lines(
        &path,
        &[
            user(START, "grow"),
            assistant(&iso(start + 2_000), "grow", "msg_grow", 40),
            assistant(&iso(start + 8_000), "grow", "msg_grow", 700),
        ],
    );
    let mut producer = Producer::new();
    producer.tick(start + 8_500);
    append_lines(&path, &[assistant(END, "grow", "msg_grow", 1222)]);

    let snap = producer.tick(ms(END) + 4_000);
    let s = &snap.sessions["grow"];
    close(s.figures.m5.speed.unwrap(), SPEED, "end-landing speed");
    // 1222 once, never 40 + 700 + 1222.
    close(
        s.figures.m5.rate,
        1222.0 / 300.0,
        "the response counts once",
    );
}

#[test]
fn an_unattributed_session_counts_in_the_total_only() {
    let home = HomeSandbox::new();
    let start = ms(START);
    // `kerry`'s isolated runtime: its store names the profile and the sid.
    let iso_store = home
        .home()
        .join(".clauth/profiles/kerry/runtime-isolated-4242-1/projects/-home-u-proj");
    crate::live_sessions::register(&crate::live_sessions::LiveSession {
        isolated: true,
        ..crate::testutil::live_row("4242-1", "kerry")
    })
    .unwrap();
    write_lines(
        &iso_store.join("owned.jsonl"),
        &[
            user(START, "owned"),
            assistant(END, "owned", "msg_owned", 1222),
        ],
    );
    // A bare session with no hook record, no owner stamp, no runtime.
    write_lines(
        &global_store(home.home()).join("stray.jsonl"),
        &[
            user(START, "stray"),
            assistant(&iso(start + 10_000), "stray", "msg_stray", 500),
        ],
    );

    let snap = Producer::new().tick(ms(END) + 4_000);
    assert_eq!(
        snap.accounts.keys().collect::<Vec<_>>(),
        vec!["kerry"],
        "only the attributed session reaches an account"
    );
    close(snap.accounts["kerry"].m5.rate, 1222.0 / 300.0, "kerry");
    assert_eq!(snap.sessions["stray"].account, None);
    assert_eq!(snap.sessions["owned"].account.as_deref(), Some("kerry"));
    close(
        snap.total.m5.rate,
        (1222.0 + 500.0) / 300.0,
        "total has both",
    );
}

#[test]
fn a_swapped_session_lands_on_its_current_member() {
    let home = HomeSandbox::new();
    // Launched on `kerry`, swapped onto `mommy`: the store path still names
    // the launch profile, the live row's `current_member` follows the swap.
    let iso_store = home
        .home()
        .join(".clauth/profiles/kerry/runtime-isolated-4242-1/projects/-home-u-proj");
    crate::live_sessions::register(&crate::live_sessions::LiveSession {
        isolated: true,
        current_member: Some("mommy".to_owned()),
        ..crate::testutil::live_row("4242-1", "kerry")
    })
    .unwrap();
    write_lines(
        &iso_store.join("swapped.jsonl"),
        &[
            user(START, "swapped"),
            assistant(END, "swapped", "msg_swapped", 1222),
        ],
    );

    let snap = Producer::new().tick(ms(END) + 4_000);
    assert_eq!(snap.accounts.keys().collect::<Vec<_>>(), vec!["mommy"]);
    assert_eq!(snap.sessions["swapped"].account.as_deref(), Some("mommy"));
    let day = &snap.days["2026-09-22"];
    assert_eq!(
        day.keys().collect::<Vec<_>>(),
        vec!["mommy"],
        "the day sums follow the member that spent them"
    );
    assert_eq!(
        sums_only(&day["mommy"][MODEL]),
        DaySum {
            output: 1222,
            gen_ms: 13_597,
            ..Default::default()
        }
    );
}

#[test]
fn a_shared_session_attributes_through_the_hooks_resolved_account() {
    let home = HomeSandbox::new();
    let sid = "5d7e2a10-1b3c-4d5e-8f90-a1b2c3d4e5f6";
    let record = crate::hook_note::record_path(sid, None).unwrap();
    std::fs::create_dir_all(record.parent().unwrap()).unwrap();
    std::fs::write(&record, r#"{"resolved":"mommy"}"#).unwrap();
    write_lines(
        &global_store(home.home()).join(format!("{sid}.jsonl")),
        &[user(START, sid), assistant(END, sid, "msg_shared", 1222)],
    );
    // A subagent of that session attributes through its parent.
    write_lines(
        &global_store(home.home()).join(format!("{sid}/subagents/agent-a1b2c3.jsonl")),
        &[user(START, sid), assistant(END, sid, "msg_sub", 600)],
    );

    let snap = Producer::new().tick(ms(END) + 4_000);
    assert_eq!(snap.accounts.keys().collect::<Vec<_>>(), vec!["mommy"]);
    close(
        snap.accounts["mommy"].m5.rate,
        (1222.0 + 600.0) / 300.0,
        "the session and its subagent",
    );
}

#[test]
fn the_cache_round_trips_and_ages_off_generated_at() {
    let home = HomeSandbox::new();
    seed_fixture(home.home());
    let now = ms(END) + 4_000;
    let snap = Producer::new().tick(now);
    assert_eq!(snap.age_ms(now + 7_000), Some(7_000));
    let path = home.home().join(".clauth").join(TPS_CACHE_FILE);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, serde_json::to_vec(&snap).unwrap()).unwrap();
    assert_eq!(load_cache(), Some(snap));
}

// ── extra: truncation restart, restart dedupe, transcript_speed ───────────────

/// A transcript truncated below the stored tail offset restarts that file at
/// byte 0 and still reads the lines appended afterwards.
#[test]
fn a_truncated_transcript_restarts_that_file_at_byte_zero() {
    let home = HomeSandbox::new();
    let path = global_store(home.home()).join("trunc.jsonl");
    write_lines(
        &path,
        &[
            user(START, "trunc"),
            assistant(END, "trunc", "msg_old", 1222),
        ],
    );
    let mut producer = Producer::new();
    producer.tick(ms(END) + 4_000);

    // Truncate the file to nothing, then append a fresh response: the stored
    // tail offset is now past the shorter length, so the file restarts at byte
    // 0 and the new line is read. `msg_old` stays counted once (it happened
    // inside the window), never twice.
    std::fs::write(&path, "").unwrap();
    append_lines(
        &path,
        &[
            user(START, "trunc"),
            assistant(&iso(ms(END) + 20_000), "trunc", "msg_new", 400),
        ],
    );
    let snap = producer.tick(ms(END) + 24_000);
    let s = &snap.sessions["trunc"];
    close(
        s.figures.m5.rate,
        (1222.0 + 400.0) / 300.0,
        "both responses, each once",
    );
}

/// A restart beside the cache the previous producer published re-derives
/// today from the transcript and never reads the cache back: the response,
/// two block lines of one message id, counts once (nor its second line's span
/// past the first).
#[test]
fn a_restart_does_not_double_count_day_sums() {
    let home = HomeSandbox::new();
    let iso_store = home
        .home()
        .join(".clauth/profiles/kerry/runtime-isolated-4242-1/projects/-home-u-proj");
    crate::live_sessions::register(&crate::live_sessions::LiveSession {
        isolated: true,
        ..crate::testutil::live_row("4242-1", "kerry")
    })
    .unwrap();
    write_lines(
        &iso_store.join("restart.jsonl"),
        &[
            user(START, "restart"),
            assistant(&iso(ms(END) - 3_000), "restart", "msg_restart", 1222),
            assistant(END, "restart", "msg_restart", 1222),
        ],
    );
    let now = ms(END) + 4_000;
    let mut producer = Producer::new();
    let snap = producer.tick(now);
    assert_eq!(
        sums_only(&snap.days["2026-09-22"]["kerry"][MODEL]),
        DaySum {
            output: 1222,
            gen_ms: 13_597,
            ..Default::default()
        }
    );
    // Persist the cache the way the scheduler leg does.
    let path = home.home().join(".clauth").join(TPS_CACHE_FILE);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, serde_json::to_vec(&snap).unwrap()).unwrap();

    // A fresh producer re-derives today's sum from the re-read lines: 1222
    // once, never 2444.
    let mut restarted = Producer::new();
    let snap2 = restarted.tick(now + 1_000);
    assert_eq!(
        sums_only(&snap2.days["2026-09-22"]["kerry"][MODEL]),
        DaySum {
            output: 1222,
            gen_ms: 13_597,
            ..Default::default()
        }
    );
}

/// One transcript's whole-life sums over every finished response: the fixture's
/// single response is 1222 output over 13 602 ms.
#[test]
fn transcript_speed_sums_the_fixture_response() {
    let home = HomeSandbox::new();
    let path = global_store(home.home()).join(format!("{FIXTURE_SESSION}.jsonl"));
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, FIXTURE).unwrap();
    assert_eq!(
        transcript_speed(&global_store(home.home()).join(format!("{FIXTURE_SESSION}.jsonl"))),
        Some(DaySum {
            output: 1222,
            gen_ms: 13_602,
            ..Default::default()
        })
    );
}

/// A tool-use turn in a main-session transcript: Claude Code writes one line
/// per content block, each carrying the message's final `output_tokens`, the
/// tool_use block landing once its input has streamed (seconds after the
/// thinking block, measured 2026-10-18), and an earlier tool's result can land
/// between them. Lines here: thinking at +10 s, a tool_result user line at
/// +12 s, tool_use at +30 s, both block lines 1000 tokens; the parser reads no
/// block kind, so `assistant` stands in for both. The span runs from the user
/// line before the id's first line to its last line: 1000 / 30 s.
fn tool_use_turn(session: &str, id: &str) -> [String; 4] {
    let start = ms(START);
    [
        user(START, session),
        assistant(&iso(start + 10_000), session, id, 1000),
        user(&iso(start + 12_000), session),
        assistant(&iso(start + 30_000), session, id, 1000),
    ]
}

#[test]
fn a_tool_use_block_streamed_after_the_thinking_ends_the_span() {
    let home = HomeSandbox::new();
    reg(None);
    write_lines(
        &iso_kerry(home.home()).join("tool.jsonl"),
        &tool_use_turn("tool", "msg_tool"),
    );
    let snap = Producer::new().tick(ms(START) + 31_000);
    close(
        snap.sessions["tool"].figures.m5.speed.unwrap(),
        1000.0 / 30.0,
        "speed spans the tool_use block",
    );
    assert_eq!(
        sums_only(&snap.days["2026-09-22"]["kerry"][MODEL]),
        DaySum {
            output: 1000,
            gen_ms: 30_000,
            ..Default::default()
        }
    );
}

/// A tick landing between two block lines of one id folds a 10 s span, and
/// the tool_use line read on the next tick extends it to 30 s. The day sum
/// takes the extension once: 30 000 ms, never 10 000 nor 40 000.
#[test]
fn a_block_line_landing_after_a_tick_extends_the_folded_span_once() {
    let home = HomeSandbox::new();
    reg(None);
    let path = iso_kerry(home.home()).join("tool.jsonl");
    let [u, thinking, result, tool_use] = tool_use_turn("tool", "msg_tool");
    write_lines(&path, &[u, thinking]);
    let mut p = Producer::new();
    let mid = p.tick(ms(START) + 11_000);
    assert_eq!(
        sums_only(&mid.days["2026-09-22"]["kerry"][MODEL]),
        DaySum {
            output: 1000,
            gen_ms: 10_000,
            ..Default::default()
        },
        "mid-stream the span ends at the thinking line"
    );
    append_lines(&path, &[result, tool_use]);
    let snap = p.tick(ms(START) + 31_000);
    assert_eq!(
        sums_only(&snap.days["2026-09-22"]["kerry"][MODEL]),
        DaySum {
            output: 1000,
            gen_ms: 30_000,
            ..Default::default()
        }
    );
    close(
        snap.sessions["tool"].figures.m5.speed.unwrap(),
        1000.0 / 30.0,
        "the live figure reads the extended span",
    );
}

/// A producer restart (a lease handoff, a TUI reopened) between the two block
/// lines: the previous producer counted the 10 s span and its cache's
/// `generated_at` sits between the lines; the restarted one re-reads the turn
/// and folds only the extension: 30 000 ms once, never 10 000 nor 40 000.
#[test]
fn a_restart_between_two_block_lines_extends_the_counted_span_once() {
    let home = HomeSandbox::new();
    reg(None);
    let path = iso_kerry(home.home()).join("tool.jsonl");
    let [u, thinking, result, tool_use] = tool_use_turn("tool", "msg_tool");
    write_lines(&path, &[u, thinking]);
    let before = Producer::new().tick(ms(START) + 11_000);
    assert_eq!(
        sums_only(&before.days["2026-09-22"]["kerry"][MODEL]),
        DaySum {
            output: 1000,
            gen_ms: 10_000,
            ..Default::default()
        }
    );
    write_cache(home.home(), &before);
    append_lines(&path, &[result, tool_use]);
    let snap = Producer::new().tick(ms(START) + 31_000);
    assert_eq!(
        sums_only(&snap.days["2026-09-22"]["kerry"][MODEL]),
        DaySum {
            output: 1000,
            gen_ms: 30_000,
            ..Default::default()
        }
    );
}

/// The same restart with the turn straddling UTC midnight: the previous
/// producer put the 5 s span on 09-22 (the thinking line's day); the restarted
/// one moves it to 09-23 with the extension, so 09-22 keeps nothing of it and
/// 09-23 holds 1000 over 30 000 ms.
#[test]
fn a_restart_between_block_lines_across_midnight_moves_the_counted_span() {
    let home = HomeSandbox::new();
    reg(None);
    let path = iso_kerry(home.home()).join("midtool.jsonl");
    write_lines(
        &path,
        &[
            user("2026-09-22T23:59:50.000Z", "midtool"),
            assistant("2026-09-22T23:59:55.000Z", "midtool", "msg_mt", 1000),
        ],
    );
    let before = Producer::new().tick(ms("2026-09-22T23:59:57.000Z"));
    assert_eq!(
        sums_only(&before.days["2026-09-22"]["kerry"][MODEL]),
        DaySum {
            output: 1000,
            gen_ms: 5_000,
            ..Default::default()
        }
    );
    write_cache(home.home(), &before);
    append_lines(
        &path,
        &[assistant(
            "2026-09-23T00:00:20.000Z",
            "midtool",
            "msg_mt",
            1000,
        )],
    );
    let snap = Producer::new().tick(ms("2026-09-23T00:00:21.000Z"));
    assert_eq!(
        sums_only(&snap.days["2026-09-23"]["kerry"][MODEL]),
        DaySum {
            output: 1000,
            gen_ms: 30_000,
            ..Default::default()
        }
    );
    let left = snap
        .days
        .get("2026-09-22")
        .and_then(|d| d.get("kerry"))
        .and_then(|m| m.get(MODEL));
    assert!(
        left.is_none_or(|s| *s == DaySum::default()),
        "09-22 keeps nothing of the moved turn: {left:?}"
    );
}

/// A swap from `kerry` to `mommy` lands between the previous producer's read
/// of the thinking line and the restart: the turn stays on `kerry` (the
/// restart records the unstamped swap at its own first tick, after the turn's
/// span start), its extension included, and `mommy` gets nothing.
#[test]
fn a_swap_before_a_restart_keeps_the_in_flight_turn_on_its_member() {
    let home = HomeSandbox::new();
    reg(None);
    let path = iso_kerry(home.home()).join("tool.jsonl");
    let [u, thinking, result, tool_use] = tool_use_turn("tool", "msg_tool");
    write_lines(&path, &[u, thinking]);
    let before = Producer::new().tick(ms(START) + 11_000);
    write_cache(home.home(), &before);
    reg(Some("mommy"));
    append_lines(&path, &[result, tool_use]);
    let snap = Producer::new().tick(ms(START) + 31_000);
    assert_eq!(
        snap.days["2026-09-22"].keys().collect::<Vec<_>>(),
        vec!["kerry"],
        "mommy gets no row: {:?}",
        snap.days["2026-09-22"]
    );
    assert_eq!(
        sums_only(&snap.days["2026-09-22"]["kerry"][MODEL]),
        DaySum {
            output: 1000,
            gen_ms: 30_000,
            ..Default::default()
        }
    );
    assert_eq!(
        snap.accounts.keys().collect::<Vec<_>>(),
        vec!["kerry"],
        "the live figures keep the turn on kerry"
    );
}

/// The same swap, with no producer running for 15 minutes before the restart
/// (both block lines landed 20 s apart, long before it): the turn still stays
/// on `kerry`, the member the saved timeline gives its span start, however
/// long the restart took.
#[test]
fn a_swap_before_a_late_restart_keeps_the_in_flight_turn_on_its_member() {
    let home = HomeSandbox::new();
    reg(None);
    let path = iso_kerry(home.home()).join("tool.jsonl");
    let [u, thinking, result, tool_use] = tool_use_turn("tool", "msg_tool");
    write_lines(&path, &[u, thinking]);
    let before = Producer::new().tick(ms(START) + 11_000);
    write_cache(home.home(), &before);
    reg(Some("mommy"));
    append_lines(&path, &[result, tool_use]);
    let snap = Producer::new().tick(ms(START) + 11_000 + 15 * 60_000);
    assert_eq!(
        snap.days["2026-09-22"].keys().collect::<Vec<_>>(),
        vec!["kerry"],
        "mommy gets no row: {:?}",
        snap.days["2026-09-22"]
    );
    assert_eq!(
        sums_only(&snap.days["2026-09-22"]["kerry"][MODEL]),
        DaySum {
            output: 1000,
            gen_ms: 30_000,
            ..Default::default()
        }
    );
}

/// The same swap, with the turn's transcript missing on the restart's first
/// tick (moved away, as a file replaced mid-walk or a failed open leaves it)
/// and back by the next discovery walk: the turn still stays on `kerry`, so
/// no tick of the restart loses the saved timeline's member for it.
/// An unattributed session in the global store keeps the restart's first
/// tick from reading an empty tree.
#[test]
fn a_transcript_missing_on_the_restarts_first_tick_keeps_its_member() {
    let home = HomeSandbox::new();
    reg(None);
    let path = iso_kerry(home.home()).join("tool.jsonl");
    let [u, thinking, result, tool_use] = tool_use_turn("tool", "msg_tool");
    write_lines(&path, &[u, thinking]);
    write_lines(
        &global_store(home.home()).join("other.jsonl"),
        &[
            user(START, "other"),
            assistant(&iso(ms(START) + 5_000), "other", "msg_other", 500),
        ],
    );
    let before = Producer::new().tick(ms(START) + 11_000);
    write_cache(home.home(), &before);
    reg(Some("mommy"));
    append_lines(&path, &[result, tool_use]);
    let away = path.with_extension("away");
    std::fs::rename(&path, &away).unwrap();
    let mut p = Producer::new();
    p.tick(ms(START) + 31_000);
    std::fs::rename(&away, &path).unwrap();
    let snap = p.tick(ms(START) + 31_000 + DISCOVER_INTERVAL_MS);
    let day = &snap.days["2026-09-22"];
    assert!(!day.contains_key("mommy"), "mommy gets no row: {day:?}");
    assert_eq!(
        sums_only(&day["kerry"][MODEL]),
        DaySum {
            output: 1000,
            gen_ms: 30_000,
            ..Default::default()
        }
    );
}

/// The same swap with the turn straddling UTC midnight: the turn moves to its
/// end day on the member that spent it and counts once; 09-22 keeps nothing
/// of it.
#[test]
fn a_swap_before_a_restart_across_midnight_moves_the_turn_on_its_member() {
    let home = HomeSandbox::new();
    reg(None);
    let path = iso_kerry(home.home()).join("midtool.jsonl");
    write_lines(
        &path,
        &[
            user("2026-09-22T23:59:50.000Z", "midtool"),
            assistant("2026-09-22T23:59:55.000Z", "midtool", "msg_mt", 1000),
        ],
    );
    let before = Producer::new().tick(ms("2026-09-22T23:59:57.000Z"));
    write_cache(home.home(), &before);
    reg(Some("mommy"));
    append_lines(
        &path,
        &[assistant(
            "2026-09-23T00:00:20.000Z",
            "midtool",
            "msg_mt",
            1000,
        )],
    );
    let snap = Producer::new().tick(ms("2026-09-23T00:00:21.000Z"));
    let kerry_old = snap
        .days
        .get("2026-09-22")
        .and_then(|d| d.get("kerry"))
        .and_then(|m| m.get(MODEL));
    assert!(
        kerry_old.is_none_or(|s| *s == DaySum::default()),
        "09-22 keeps nothing of the moved turn: {kerry_old:?}"
    );
    assert_eq!(
        snap.days["2026-09-23"].keys().collect::<Vec<_>>(),
        vec!["kerry"],
        "the turn lands on kerry alone: {:?}",
        snap.days["2026-09-23"]
    );
    assert_eq!(
        sums_only(&snap.days["2026-09-23"]["kerry"][MODEL]),
        DaySum {
            output: 1000,
            gen_ms: 30_000,
            ..Default::default()
        }
    );
}

/// A v1 cache an older clauth wrote, without any field this build added, still
/// loads for every reader: each newer field is additive (`#[serde(default)]`),
/// so a reader keeps an older producer's figures.
#[test]
fn an_old_cache_without_the_newer_fields_still_loads() {
    let home = HomeSandbox::new();
    let clauth = home.home().join(".clauth");
    std::fs::create_dir_all(&clauth).unwrap();
    std::fs::write(
        clauth.join(TPS_CACHE_FILE),
        r#"{"version":1,"generated_at":"2026-09-22T20:46:27.014+00:00","total":{"live":{"rate":0.0,"speed":null},"m5":{"rate":0.0,"speed":null},"h1":{"rate":0.0,"speed":null}},"accounts":{},"sessions":{},"days":{}}"#,
    )
    .unwrap();
    let snap = load_cache().unwrap();
    assert_eq!(snap.version, 1, "the older producer's cache loads");
    assert!(
        snap.rate_peaks.is_empty() && snap.peaks == Peaks::default(),
        "the newer fields default empty: {snap:?}"
    );
}

/// A restart between two block lines of a span-less response (no user line in
/// its file) that crosses midnight: the previous producer never put it in the
/// day sums, so the move takes nothing off 09-22's other response and adds
/// nothing to 09-23.
#[test]
fn a_restart_moves_nothing_for_a_spanless_response_crossing_midnight() {
    let home = HomeSandbox::new();
    reg(None);
    let other = iso_kerry(home.home()).join("other.jsonl");
    write_lines(
        &other,
        &[
            user("2026-09-22T23:59:00.000Z", "other"),
            assistant("2026-09-22T23:59:30.000Z", "other", "msg_sp", 500),
        ],
    );
    let bare = iso_kerry(home.home()).join("bare.jsonl");
    write_lines(
        &bare,
        &[assistant(
            "2026-09-22T23:59:55.000Z",
            "bare",
            "msg_bare",
            1000,
        )],
    );
    let before = Producer::new().tick(ms("2026-09-22T23:59:57.000Z"));
    write_cache(home.home(), &before);
    append_lines(
        &bare,
        &[assistant(
            "2026-09-23T00:00:20.000Z",
            "bare",
            "msg_bare",
            1000,
        )],
    );
    let snap = Producer::new().tick(ms("2026-09-23T00:00:21.000Z"));
    assert_eq!(
        sums_only(&snap.days["2026-09-22"]["kerry"][MODEL]),
        DaySum {
            output: 500,
            gen_ms: 30_000,
            ..Default::default()
        }
    );
    assert!(
        !snap.days.contains_key("2026-09-23"),
        "a span-less response adds no day row: {:?}",
        snap.days.get("2026-09-23")
    );
}

/// A response whose block lines already crossed midnight before a restart
/// (the previous producer moved its 15 s span to 09-23) gains a line after
/// it: the restart counts it once on 09-23 over its whole 30 s span, and
/// 09-22 keeps only the other response.
#[test]
fn a_restart_after_the_baseline_crossed_midnight_folds_only_the_extension() {
    let home = HomeSandbox::new();
    reg(None);
    let other = iso_kerry(home.home()).join("other.jsonl");
    write_lines(
        &other,
        &[
            user("2026-09-22T23:59:00.000Z", "other"),
            assistant("2026-09-22T23:59:30.000Z", "other", "msg_o", 500),
        ],
    );
    let path = iso_kerry(home.home()).join("midb.jsonl");
    write_lines(
        &path,
        &[
            user("2026-09-22T23:59:50.000Z", "midb"),
            assistant("2026-09-22T23:59:55.000Z", "midb", "msg_b", 1000),
            assistant("2026-09-23T00:00:05.000Z", "midb", "msg_b", 1000),
        ],
    );
    let before = Producer::new().tick(ms("2026-09-23T00:00:06.000Z"));
    assert_eq!(
        sums_only(&before.days["2026-09-23"]["kerry"][MODEL]),
        DaySum {
            output: 1000,
            gen_ms: 15_000,
            ..Default::default()
        }
    );
    write_cache(home.home(), &before);
    append_lines(
        &path,
        &[assistant("2026-09-23T00:00:20.000Z", "midb", "msg_b", 1000)],
    );
    let snap = Producer::new().tick(ms("2026-09-23T00:00:21.000Z"));
    assert_eq!(
        sums_only(&snap.days["2026-09-23"]["kerry"][MODEL]),
        DaySum {
            output: 1000,
            gen_ms: 30_000,
            ..Default::default()
        }
    );
    assert_eq!(
        sums_only(&snap.days["2026-09-22"]["kerry"][MODEL]),
        DaySum {
            output: 500,
            gen_ms: 30_000,
            ..Default::default()
        },
        "09-22 keeps only the other response"
    );
}

/// A block line read later but stamped earlier never moves the end back: the
/// end is the latest stamp among the id's largest-output lines.
#[test]
fn a_line_read_later_with_an_earlier_stamp_never_moves_the_end_back() {
    let home = HomeSandbox::new();
    reg(None);
    let start = ms(START);
    let lines = [
        user(START, "ooo"),
        assistant(&iso(start + 30_000), "ooo", "msg_ooo", 1000),
        assistant(&iso(start + 10_000), "ooo", "msg_ooo", 1000),
    ];
    let path = iso_kerry(home.home()).join("ooo.jsonl");
    write_lines(&path, &lines);
    let snap = Producer::new().tick(start + 31_000);
    assert_eq!(
        sums_only(&snap.days["2026-09-22"]["kerry"][MODEL]),
        DaySum {
            output: 1000,
            gen_ms: 30_000,
            ..Default::default()
        }
    );
    close(
        snap.sessions["ooo"].figures.m5.speed.unwrap(),
        1000.0 / 30.0,
        "the live figure keeps the later end",
    );
    assert_eq!(
        transcript_speed(&path),
        Some(DaySum {
            output: 1000,
            gen_ms: 30_000,
            ..Default::default()
        })
    );
}

#[test]
fn transcript_speed_ends_a_tool_use_turn_at_its_last_block() {
    let home = HomeSandbox::new();
    let path = global_store(home.home()).join("tool.jsonl");
    write_lines(&path, &tool_use_turn("tool", "msg_tool"));
    assert_eq!(
        transcript_speed(&path),
        Some(DaySum {
            output: 1000,
            gen_ms: 30_000,
            ..Default::default()
        })
    );
}

// ── round-2 fixes (reviewer probes re-derived through Producer::tick) ────────

fn iso_kerry(home: &Path) -> PathBuf {
    home.join(".clauth/profiles/kerry/runtime-isolated-4242-1/projects/-home-u-proj")
}

fn reg(member: Option<&str>) {
    crate::live_sessions::register(&crate::live_sessions::LiveSession {
        isolated: true,
        current_member: member.map(str::to_owned),
        ..crate::testutil::live_row("4242-1", "kerry")
    })
    .unwrap();
}

fn write_cache(home: &Path, snap: &Snapshot) {
    let path = home.join(".clauth").join(TPS_CACHE_FILE);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, serde_json::to_vec(snap).unwrap()).unwrap();
}

/// The owed-day list a previous producer left: `tps_days.json` at version 1,
/// naming `days`.
fn write_owed(home: &Path, days: &[&str]) {
    let path = home.join(".clauth").join(TPS_DAYS_FILE);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let wire = serde_json::json!({"version": 1, "owed": days});
    std::fs::write(&path, wire.to_string()).unwrap();
}

/// One kerry response on `day` (`output` tokens over a `gen_ms` span from
/// 12:00), alone in an isolated-store transcript last written that day, so
/// only a read back to `day` sees it.
fn day_transcript(home: &Path, day: &str, output: u64, gen_ms: u64) {
    let session = format!("owed-{day}");
    let path = iso_kerry(home).join(format!("{session}.jsonl"));
    let start = ms(&format!("{day}T12:00:00.000Z"));
    write_lines(
        &path,
        &[
            user(&iso(start), &session),
            assistant(
                &iso(start + gen_ms),
                &session,
                &format!("msg_{day}"),
                output,
            ),
        ],
    );
    crate::testutil::set_mtime(
        &path,
        std::time::UNIX_EPOCH + std::time::Duration::from_millis(start + gen_ms + 1_000),
    );
}

/// `[9.95, 10)` rounds up to a whole number, never "10.0".
#[test]
fn format_figure_rounds_the_95_band_up_to_whole() {
    assert_eq!(format_figure(9.94), "9.9");
    assert_eq!(format_figure(9.95), "10");
    assert_eq!(format_figure(9.96), "10");
}

/// A response with no user line before it has no span: out of speed and out of
/// the day sums (a day sum is a speed basis), but its tokens land as a point in
/// the rate.
#[test]
fn a_response_with_no_user_line_has_no_span() {
    let home = HomeSandbox::new();
    reg(None);
    write_lines(
        &iso_kerry(home.home()).join("nouser.jsonl"),
        &[assistant(END, "nouser", "msg_nouser", 1222)],
    );
    let snap = Producer::new().tick(ms(END) + 4_000);
    assert_eq!(
        snap.sessions["nouser"].figures.m5.speed, None,
        "no span, no speed"
    );
    assert!(!snap.days.contains_key("2026-09-22"), "no span, no day sum");
    // Its tokens count whole in the rate: end_ms is inside the 5 min window.
    close(
        snap.sessions["nouser"].figures.m5.rate,
        1222.0 / 300.0,
        "point tokens land in the rate",
    );
}

/// Parallel subagents share a parent session key, but each transcript's span
/// start is its OWN latest user line: sibling B's later user line must not
/// become A's span start. speed = 1222 / 12 s.
#[test]
fn parallel_subagents_keep_their_own_span_start() {
    let home = HomeSandbox::new();
    let s = ms(START);
    let dir = global_store(home.home()).join("par/subagents");
    let a = dir.join("agent-a.jsonl");
    let b = dir.join("agent-b.jsonl");
    write_lines(&a, &[user(&iso(s), "par")]);
    let mut p = Producer::new();
    p.tick(s + 1_000);
    write_lines(&b, &[user(&iso(s + 5_000), "par")]);
    p.tick(s + 6_500);
    append_lines(&a, &[assistant(&iso(s + 12_000), "par", "msg_a", 1222)]);
    let snap = p.tick(s + 13_000);
    let sp = snap.sessions["par"].figures.m5.speed.unwrap();
    close(
        sp,
        1222.0 / 12.0,
        "subagent A span starts at A's own user line",
    );
}

/// A live swap: each response stays on the member that spent it, and the
/// session row names the current member (deterministically). The row carries
/// no swap stamp, so the swap takes the instant it was observed at.
#[test]
fn a_live_swap_keeps_each_response_on_its_member() {
    let home = HomeSandbox::new();
    let path = iso_kerry(home.home()).join("sw.jsonl");
    let mut row_values = std::collections::BTreeSet::new();
    let mut last = None;
    for _ in 0..40 {
        reg(None);
        write_lines(
            &path,
            &[user(START, "sw"), assistant(END, "sw", "msg_a", 1222)],
        );
        let mut p = Producer::new();
        p.tick(ms(END) + 1_000);
        reg(Some("mommy"));
        p.tick(ms(END) + 1_500);
        append_lines(
            &path,
            &[
                user(&iso(ms(END) + 2_000), "sw"),
                assistant(&iso(ms(END) + 12_000), "sw", "msg_b", 500),
            ],
        );
        let snap = p.tick(ms(END) + 13_000);
        row_values.insert(snap.sessions["sw"].account.clone());
        last = Some(snap);
    }
    let snap = last.unwrap();
    assert_eq!(
        row_values.len(),
        1,
        "session row account is order-dependent"
    );
    assert_eq!(snap.sessions["sw"].account.as_deref(), Some("mommy"));
    assert_eq!(
        snap.accounts.keys().collect::<Vec<_>>(),
        vec!["kerry", "mommy"],
        "each response stays on the member that spent it"
    );
    assert_eq!(snap.days["2026-09-22"]["kerry"][MODEL].output, 1222);
    assert_eq!(snap.days["2026-09-22"]["mommy"][MODEL].output, 500);
}

/// A producer restart after an unstamped swap reads the pre-swap response
/// under the member the saved timeline gives it, never under the current one.
#[test]
fn a_restart_after_a_swap_never_reattributes_a_counted_response() {
    let home = HomeSandbox::new();
    let path = iso_kerry(home.home()).join("rst.jsonl");
    reg(None);
    write_lines(
        &path,
        &[user(START, "rst"), assistant(END, "rst", "msg_a", 1222)],
    );
    let snap = Producer::new().tick(ms(END) + 1_000);
    assert_eq!(snap.days["2026-09-22"]["kerry"][MODEL].output, 1222);
    write_cache(home.home(), &snap);

    reg(Some("mommy"));
    let restarted = Producer::new().tick(ms(END) + 2_000);
    assert_eq!(
        restarted.days["2026-09-22"]["kerry"][MODEL].output, 1222,
        "the saved timeline's member survives the restart"
    );
    assert!(
        !restarted.days["2026-09-22"].contains_key("mommy"),
        "the pre-swap response was re-attributed"
    );
    assert_eq!(
        restarted.sessions["rst"].account.as_deref(),
        Some("mommy"),
        "the session row names the current member"
    );
}

/// A response that ends just before UTC midnight reaches its day, even though
/// the tick first reads it after midnight.
#[test]
fn a_response_landing_just_before_utc_midnight_reaches_its_day() {
    let home = HomeSandbox::new();
    reg(None);
    let path = iso_kerry(home.home()).join("mid.jsonl");
    write_lines(&path, &[user("2026-09-22T23:59:50.000Z", "mid")]);
    let mut p = Producer::new();
    p.tick(ms("2026-09-22T23:59:59.000Z"));
    append_lines(
        &path,
        &[assistant(
            "2026-09-22T23:59:59.800Z",
            "mid",
            "msg_mid",
            1222,
        )],
    );
    let snap = p.tick(ms("2026-09-23T00:00:00.500Z"));
    assert_eq!(
        snap.days
            .get("2026-09-22")
            .and_then(|d| d.get("kerry"))
            .and_then(|m| m.get(MODEL))
            .map(|s| s.output),
        Some(1222),
        "the 23:59:59.8 response is missing from its day"
    );
}

/// A restart after midnight completes the previous day from its retained lines:
/// the 23:30 response joins the 21:00 response.
#[test]
fn a_restart_after_midnight_completes_the_previous_day() {
    let home = HomeSandbox::new();
    reg(None);
    let path = iso_kerry(home.home()).join("late.jsonl");
    write_lines(
        &path,
        &[
            user("2026-09-22T21:00:00.000Z", "late"),
            assistant("2026-09-22T21:00:10.000Z", "late", "msg_early", 1000),
        ],
    );
    let snap = Producer::new().tick(ms("2026-09-22T21:00:20.000Z"));
    write_cache(home.home(), &snap);
    append_lines(
        &path,
        &[
            user("2026-09-22T23:30:00.000Z", "late"),
            assistant("2026-09-22T23:30:10.000Z", "late", "msg_late", 500),
        ],
    );
    let snap2 = Producer::new().tick(ms("2026-09-23T00:10:00.000Z"));
    assert_eq!(
        snap2.days["2026-09-22"]["kerry"][MODEL].output, 1500,
        "the 23:30 response is missing from its day"
    );
}

/// A cold start reads back only the recent tail: a file whose lines are all
/// days old retains nothing.
#[test]
fn a_cold_start_reads_back_only_the_recent_tail() {
    let home = HomeSandbox::new();
    let now = ms(END) + 3 * 86_400_000;
    seed_fixture(home.home());
    let mut p = Producer::new();
    p.tick(now);
    assert_eq!(
        p.responses.len(),
        0,
        "3-day-old lines are all before the cutoff"
    );
}

/// A synthetic API-error line (placeholder model, zero output) never drags the
/// speed denominator nor pollutes the day sums.
#[test]
fn a_synthetic_error_line_does_not_drag_speed() {
    let home = HomeSandbox::new();
    reg(None);
    let synth = serde_json::json!({
        "type": "assistant", "timestamp": iso(ms(END) + 61_000), "sessionId": "syn",
        "message": {"id": "2e6d3668-da9b-40b0-aafe-1a8b4f614e94", "model": "<synthetic>", "role": "assistant",
            "content": [{"type": "text", "text": "API Error"}],
            "usage": {"input_tokens": 0, "output_tokens": 0, "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0}},
    })
    .to_string();
    write_lines(
        &iso_kerry(home.home()).join("syn.jsonl"),
        &[
            user(START, "syn"),
            assistant(END, "syn", "msg_real", 1222),
            user(&iso(ms(END) + 1_000), "syn"),
            synth,
        ],
    );
    let snap = Producer::new().tick(ms(END) + 62_000);
    close(
        snap.sessions["syn"].figures.m5.speed.unwrap(),
        SPEED,
        "the synthetic line must not enter the speed denominator",
    );
    assert!(
        !snap.days["2026-09-22"]["kerry"].contains_key("<synthetic>"),
        "no synthetic day row"
    );
}

/// A spend-cap 429 row is recognized and drained once, but folds no tokens: the
/// error row's only effect is the trigger-b signal the scheduler consumes.
#[test]
fn a_spend_cap_429_row_adds_no_tokens_and_drains_once() {
    let home = HomeSandbox::new();
    reg(None);
    let cap = spend_cap_429(&iso(ms(END) + 2_000));
    let transcript = iso_kerry(home.home()).join("cap.jsonl");
    write_lines(
        &transcript,
        &[
            user(START, "cap"),
            assistant(END, "cap", "msg_cap", 1222),
            user(&iso(ms(END) + 1_000), "cap"),
            cap,
        ],
    );
    let mut p = Producer::new();
    let snap = p.tick(ms(END) + 62_000);
    close(
        snap.sessions["cap"].figures.m5.speed.unwrap(),
        SPEED,
        "the spend-cap row must not enter the speed denominator",
    );
    assert!(
        !snap.days["2026-09-22"]["kerry"].contains_key("<synthetic>"),
        "no synthetic day row"
    );
    let drained = p.drain_spend_cap();
    assert_eq!(
        drained.pending.get("cap").map(|f| f.ts),
        Some(ms(END) + 2_000),
        "the spend-cap 429 drains once, keyed by conversation"
    );
    assert_eq!(
        p.spend_cap_recovery("cap", &transcript)
            .map(|recovery| recovery.ts),
        Some(ms(END)),
        "the successful turn before the 429 is its file's success record"
    );
    // The turn's line ends after the first user line and itself, in the file's
    // first read life.
    let turn_end =
        (user(START, "cap").len() + 1 + assistant(END, "cap", "msg_cap", 1222).len() + 1) as u64;
    assert_eq!(
        drained
            .turns_by_model
            .get("cap")
            .and_then(|by_file| by_file.get(&transcript)),
        Some(&HashMap::from([(
            MODEL.to_string(),
            SpendCapRecovery {
                life: FileLife(1),
                end: turn_end,
                ts: ms(END),
            }
        )])),
        "the same turn, keyed by file and model, at its line's position"
    );
    assert!(
        p.drain_spend_cap().pending.is_empty(),
        "a fact is delivered exactly once"
    );
}

// ── spend-cap delivery: the byte-position bound and the 429 uuid dedupe ────

/// A byte-0 re-read (a clock step back) never re-delivers spend-cap facts the
/// producer already delivered and the scheduler already drained: a re-delivered
/// 429 would re-latch a session whose recovery already dropped the latch.
#[test]
fn a_clock_step_back_re_read_delivers_no_spend_cap_fact_twice() {
    let home = HomeSandbox::new();
    reg(None);
    let cap = spend_cap_429(&iso(ms(END) + 2_000));
    let transcript = iso_kerry(home.home()).join("cap.jsonl");
    write_lines(
        &transcript,
        &[
            user(START, "cap"),
            assistant(END, "cap", "msg_cap", 1222),
            user(&iso(ms(END) + 1_000), "cap"),
            cap,
        ],
    );
    let now = ms(END) + 62_000;
    let mut p = Producer::new();
    p.tick(now);
    let drained = p.drain_spend_cap();
    assert_eq!(
        drained.pending.get("cap").map(|f| f.ts),
        Some(ms(END) + 2_000),
        "the 429 drains once"
    );
    let success = p.spend_cap_recovery("cap", &transcript);
    assert_eq!(
        success.map(|recovery| recovery.ts),
        Some(ms(END)),
        "the success is its file's record"
    );
    // A full-day clock step back lowers the cutoff below `whole_since`, so the
    // hot file re-reads from byte 0.
    p.tick(now - DAY_MS);
    let again = p.drain_spend_cap();
    assert!(
        again.pending.is_empty(),
        "the re-read re-delivered the drained 429: {:?}",
        again.pending
    );
    assert_eq!(
        p.spend_cap_recovery("cap", &transcript),
        success,
        "the byte-0 re-read re-delivered the success"
    );
}

/// The success's turn on its model rides the same bound: a byte-0 re-read
/// delivers it once, so a credit refusal placed after the drain is never
/// cleared by a turn the scheduler already read.
#[test]
fn a_clock_step_back_re_read_delivers_no_turn_by_model_twice() {
    let home = HomeSandbox::new();
    reg(None);
    let transcript = iso_kerry(home.home()).join("turns.jsonl");
    write_lines(
        &transcript,
        &[
            user(START, "turns"),
            assistant(END, "turns", "msg_turns", 1222),
        ],
    );
    let now = ms(END) + 62_000;
    let mut p = Producer::new();
    p.tick(now);
    let record = p.spend_cap_recovery("turns", &transcript);
    assert_eq!(record.map(|recovery| recovery.ts), Some(ms(END)));
    assert_eq!(
        p.drain_spend_cap()
            .turns_by_model
            .get("turns")
            .and_then(|by_file| by_file.get(&transcript))
            .cloned(),
        record.map(|recovery| HashMap::from([(MODEL.to_string(), recovery)])),
        "the success is the file's turn on its model"
    );
    p.tick(now - DAY_MS);
    let again = p.drain_spend_cap();
    assert!(
        again.turns_by_model.is_empty(),
        "the re-read re-delivered the drained turn: {:?}",
        again.turns_by_model
    );
}

/// A success and a 429 stamped to the same millisecond both deliver: the 429's
/// own row is what latches the cap, whatever a same-timestamp success did.
#[test]
fn a_same_ms_success_then_429_delivers_both_facts() {
    let home = HomeSandbox::new();
    reg(None);
    let transcript = iso_kerry(home.home()).join("same.jsonl");
    let t = ms(END) + 2_000;
    write_lines(
        &transcript,
        &[
            user(START, "same"),
            assistant(&iso(t), "same", "msg_same", 1222),
            spend_cap_429(&iso(t)),
        ],
    );
    let mut p = Producer::new();
    p.tick(ms(END) + 62_000);
    let drained = p.drain_spend_cap();
    assert_eq!(
        drained.pending.get("same").map(|f| f.ts),
        Some(t),
        "the same-ms 429 delivers"
    );
    assert_eq!(
        p.spend_cap_recovery("same", &transcript)
            .map(|recovery| recovery.ts),
        Some(t),
        "the same-ms success is its file's record"
    );
}

/// A 429 stamped below an earlier success in the same read still delivers: its
/// row is the cap evidence, and order in the file never gates it off.
#[test]
fn a_429_stamped_below_an_earlier_success_delivers() {
    let home = HomeSandbox::new();
    reg(None);
    let transcript = iso_kerry(home.home()).join("below.jsonl");
    let t = ms(END) + 2_000;
    write_lines(
        &transcript,
        &[
            user(START, "below"),
            assistant(&iso(t), "below", "msg_below", 1222),
            spend_cap_429(&iso(t - 5_000)),
        ],
    );
    let mut p = Producer::new();
    p.tick(ms(END) + 62_000);
    let drained = p.drain_spend_cap();
    assert_eq!(
        drained.pending.get("below").map(|f| f.ts),
        Some(t - 5_000),
        "the earlier-stamped 429 delivers"
    );
    assert_eq!(
        p.spend_cap_recovery("below", &transcript)
            .map(|recovery| recovery.ts),
        Some(t),
        "the later success is its file's record"
    );
}

/// A 429 appended after a small clock step back still delivers: it is a new
/// line past the already-read offset, so the position bound lets it through.
#[test]
fn a_429_appended_after_a_small_clock_step_back_delivers() {
    let home = HomeSandbox::new();
    reg(None);
    let transcript = iso_kerry(home.home()).join("stepback.jsonl");
    let t = ms(END) + 2_000;
    write_lines(
        &transcript,
        &[
            user(START, "stepback"),
            assistant(&iso(t), "stepback", "msg_step", 1222),
        ],
    );
    let now = ms(END) + 62_000;
    let mut p = Producer::new();
    p.tick(now);
    let drained = p.drain_spend_cap();
    let success = p.spend_cap_recovery("stepback", &transcript);
    assert_eq!(
        success.map(|recovery| recovery.ts),
        Some(t),
        "the first read delivers the success"
    );
    assert!(drained.pending.is_empty(), "no 429 yet");
    append_lines(&transcript, &[spend_cap_429(&iso(t - 2_400))]);
    p.tick(now - 3_000);
    let drained = p.drain_spend_cap();
    assert_eq!(
        drained.pending.get("stepback").map(|f| f.ts),
        Some(t - 2_400),
        "the appended 429 delivers despite its earlier stamp"
    );
    assert_eq!(
        p.spend_cap_recovery("stepback", &transcript),
        success,
        "no new success on the second read"
    );
}

/// A file replaced at the same path by a SHORTER file re-reads from byte 0
/// (the shrink arm) and delivers the replacement's 429: the old file's
/// delivered facts never suppress a shorter replacement's. A longer
/// replacement's early 429 is not delivered (see
/// `a_longer_replacement_delivers_no_early_429`).
#[test]
fn a_replaced_file_delivers_its_429() {
    let home = HomeSandbox::new();
    reg(None);
    let transcript = iso_kerry(home.home()).join("replaced.jsonl");
    let t = ms(END) + 2_000;
    write_lines(
        &transcript,
        &[
            user(START, "replaced"),
            assistant(&iso(t), "replaced", "msg_repl", 1222),
        ],
    );
    let now = ms(END) + 62_000;
    let mut p = Producer::new();
    p.tick(now);
    assert_eq!(
        p.spend_cap_recovery("replaced", &transcript)
            .map(|recovery| recovery.ts),
        Some(t),
        "the first read delivers the success"
    );
    let tmp = iso_kerry(home.home()).join("replaced.tmp");
    write_lines(
        &tmp,
        &[user(START, "replaced"), spend_cap_429(&iso(t - 10_000))],
    );
    std::fs::rename(&tmp, &transcript).unwrap();
    p.tick(now + 6_000);
    let drained = p.drain_spend_cap();
    assert_eq!(
        drained.pending.get("replaced").map(|f| f.ts),
        Some(t - 10_000),
        "the replacement's 429 delivers"
    );
    assert_eq!(
        p.spend_cap_recovery("replaced", &transcript),
        None,
        "the replacement's new life holds no success"
    );
}

/// A file replaced at the same path by a LONGER file whose 429 lies wholly
/// before the old offset does not deliver it: the read keeps the old-offset
/// bound, so a fact line ending at or before it stays undelivered.
#[test]
fn a_longer_replacement_delivers_no_early_429() {
    let home = HomeSandbox::new();
    reg(None);
    let transcript = iso_kerry(home.home()).join("renameover.jsonl");
    let t = ms(END) + 2_000;
    write_lines(
        &transcript,
        &[
            user(START, "renameover"),
            assistant(&iso(t), "renameover", "msg_ro", 1222),
        ],
    );
    let now = ms(END) + 62_000;
    let mut p = Producer::new();
    p.tick(now);
    assert_eq!(
        p.spend_cap_recovery("renameover", &transcript)
            .map(|recovery| recovery.ts),
        Some(t),
        "the first read delivers the success"
    );
    let old = std::fs::metadata(&transcript).unwrap().len() as usize;
    let tmp = transcript.with_extension("tmp");
    write_lines(
        &tmp,
        &[
            spend_cap_429(&iso(t - 10_000)),
            pad_line(old),
            user(&iso(t + 1_000), "renameover"),
        ],
    );
    std::fs::rename(&tmp, &transcript).unwrap();
    p.tick(now + 6_000);
    let drained = p.drain_spend_cap();
    assert!(
        drained.pending.is_empty(),
        "a longer replacement's early 429 stays undelivered: {:?}",
        drained.pending
    );
    assert_eq!(
        p.spend_cap_recovery("renameover", &transcript),
        None,
        "the replacement's new life holds no success"
    );
}

/// A 429 line that STRADDLES the old offset on a longer replacement delivers:
/// the position bound keys on the line's END offset, so a line that starts
/// before the bound and ends past it delivers whole. The byte-0 re-read that
/// follows skips it again, so it delivers once.
#[test]
fn a_straddling_429_on_a_longer_replacement_delivers_once() {
    let home = HomeSandbox::new();
    reg(None);
    let transcript = iso_kerry(home.home()).join("straddle.jsonl");
    let t = ms(END) + 2_000;
    write_lines(
        &transcript,
        &[
            user(START, "straddle"),
            assistant(&iso(t), "straddle", "msg_strad", 1222),
        ],
    );
    let now = ms(END) + 62_000;
    let mut p = Producer::new();
    p.tick(now);
    assert_eq!(
        p.spend_cap_recovery("straddle", &transcript)
            .map(|recovery| recovery.ts),
        Some(t),
        "the first read delivers the success"
    );
    let old = std::fs::metadata(&transcript).unwrap().len() as usize;
    let tmp = transcript.with_extension("tmp");
    write_lines(
        &tmp,
        &[
            pad_line(old - 60),
            spend_cap_429(&iso(t - 10_000)),
            user(&iso(t + 1_000), "straddle"),
        ],
    );
    std::fs::rename(&tmp, &transcript).unwrap();
    p.tick(now + 6_000);
    let drained = p.drain_spend_cap();
    assert_eq!(
        drained.pending.get("straddle").map(|f| f.ts),
        Some(t - 10_000),
        "the straddling 429 delivers once"
    );
    assert_eq!(
        p.spend_cap_recovery("straddle", &transcript),
        None,
        "the replacement's new life holds no success"
    );
    // A byte-0 re-read (a clock step back) folds the straddling line again:
    // its end sits at or before the consumed offset, so it must not deliver a
    // second time.
    p.tick(now - DAY_MS);
    let again = p.drain_spend_cap();
    assert!(
        again.pending.is_empty(),
        "the byte-0 re-read re-delivered the straddling 429: {:?}",
        again.pending
    );
}

/// A hot file's first read records a pre-cutoff success in its file's record:
/// a pre-cutoff success still counts as its file's recovery candidate.
#[test]
fn a_first_read_delivers_a_pre_cutoff_success_recovery() {
    let home = HomeSandbox::new();
    reg(None);
    let now = ms(END) + 62_000;
    let cutoff = now - (now % DAY_MS) - DAY_MS;
    let transcript = iso_kerry(home.home()).join("p4recov.jsonl");
    let success = cutoff - 7_100_000;
    let cap = cutoff - 3_600_000;
    write_lines(
        &transcript,
        &[
            user(&iso(cutoff - 7_200_000), "p4recov"),
            assistant(&iso(success), "p4recov", "msg_old", 50),
            user(&iso(cutoff - 3_700_000), "p4recov"),
            spend_cap_429(&iso(cap)),
            user(&iso(ms(END)), "p4recov"),
        ],
    );
    let mut p = Producer::new();
    p.tick(now);
    let drained = p.drain_spend_cap();
    assert_eq!(
        p.spend_cap_recovery("p4recov", &transcript)
            .map(|recovery| recovery.ts),
        Some(success),
        "a first read delivers a pre-cutoff success as its file's record"
    );
    assert_eq!(
        drained.pending.get("p4recov").map(|f| f.ts),
        Some(cap),
        "the same read delivers the pre-cutoff 429"
    );
}

/// The same pre-cutoff success is its file's turn on its model too: a cold
/// start that reads a refusal and a later credit-billed turn from before the
/// window still hands the clear rule that turn.
#[test]
fn a_first_read_delivers_a_pre_cutoff_success_turn_by_model() {
    let home = HomeSandbox::new();
    reg(None);
    let now = ms(END) + 62_000;
    let cutoff = now - (now % DAY_MS) - DAY_MS;
    let transcript = iso_kerry(home.home()).join("p4turns.jsonl");
    let success = cutoff - 7_100_000;
    write_lines(
        &transcript,
        &[
            user(&iso(cutoff - 7_200_000), "p4turns"),
            assistant(&iso(success), "p4turns", "msg_old", 50),
            user(&iso(ms(END)), "p4turns"),
        ],
    );
    let mut p = Producer::new();
    p.tick(now);
    let record = p.spend_cap_recovery("p4turns", &transcript);
    assert_eq!(record.map(|recovery| recovery.ts), Some(success));
    assert_eq!(
        p.drain_spend_cap()
            .turns_by_model
            .get("p4turns")
            .and_then(|by_file| by_file.get(&transcript))
            .cloned(),
        record.map(|recovery| HashMap::from([(MODEL.to_string(), recovery)])),
        "a first read delivers a pre-cutoff success under its model"
    );
}

/// A tail-hash re-read (rewritten in place, longer, same inode) delivers a 429
/// wholly after the old offset: the Rewritten re-read keeps the old-offset
/// bound, so only lines past it deliver.
#[test]
fn a_tail_hash_rewrite_delivers_a_429_after_the_old_offset() {
    let home = HomeSandbox::new();
    reg(None);
    let transcript = iso_kerry(home.home()).join("tailhash.jsonl");
    let t = ms(END) + 2_000;
    write_lines(
        &transcript,
        &[
            user(START, "tailhash"),
            assistant(&iso(t), "tailhash", "msg_th", 1222),
        ],
    );
    let now = ms(END) + 62_000;
    let mut p = Producer::new();
    p.tick(now);
    assert_eq!(
        p.spend_cap_recovery("tailhash", &transcript)
            .map(|recovery| recovery.ts),
        Some(t),
        "the first read delivers the success"
    );
    let old = std::fs::metadata(&transcript).unwrap().len() as usize;
    let cap = t - 10_000;
    let body = [pad_line(old + 10), spend_cap_429(&iso(cap))].join("\n") + "\n";
    std::fs::write(&transcript, body).unwrap();
    p.tick(now + 6_000);
    let drained = p.drain_spend_cap();
    assert_eq!(
        drained.pending.get("tailhash").map(|f| f.ts),
        Some(cap),
        "the tail-hash re-read delivers the 429 past the old offset"
    );
    assert_eq!(
        p.spend_cap_recovery("tailhash", &transcript),
        None,
        "the rewrite's new life holds no success"
    );
}

/// A file rewritten between two reads of one drain puts its later turn in a
/// new read life: the turn per model that drain hands over is the new life's,
/// as the file's success record is, so its position never compares against a
/// line of the old life.
#[test]
fn a_rewrite_between_two_reads_hands_over_the_new_lifes_turn_by_model() {
    let home = HomeSandbox::new();
    reg(None);
    let transcript = iso_kerry(home.home()).join("lives.jsonl");
    let first = ms(END) + 2_000;
    write_lines(
        &transcript,
        &[
            user(START, "lives"),
            assistant(&iso(first), "lives", "msg_first", 1222),
        ],
    );
    let now = ms(END) + 62_000;
    let mut p = Producer::new();
    p.tick(now);
    let old = std::fs::metadata(&transcript).unwrap().len() as usize;
    let later = first + 1_000;
    let body = [
        pad_line(old + 10),
        assistant(&iso(later), "lives", "msg_later", 50),
    ]
    .join("\n")
        + "\n";
    std::fs::write(&transcript, body).unwrap();
    p.tick(now + 6_000);
    let record = p.spend_cap_recovery("lives", &transcript);
    assert_eq!(
        record.map(|recovery| recovery.ts),
        Some(later),
        "fixture control: the rewrite's new life holds the later success"
    );
    assert_eq!(
        p.drain_spend_cap()
            .turns_by_model
            .get("lives")
            .and_then(|by_file| by_file.get(&transcript))
            .cloned(),
        record.map(|recovery| HashMap::from([(MODEL.to_string(), recovery)])),
        "the drain hands over the new life's turn, at its position"
    );
}

/// A tail-hash re-read (rewritten in place, same inode, longer) that keeps an
/// already-drained 429 in its prefix does not deliver it again: the Rewritten
/// re-read keeps the old-offset bound, so a 429 ending before it stays
/// undelivered.
#[test]
fn a_tail_hash_rewrite_does_not_redeliver_a_drained_429() {
    let home = HomeSandbox::new();
    reg(None);
    let transcript = iso_kerry(home.home()).join("tailhashkeep.jsonl");
    let cap = ms(END);
    write_lines(
        &transcript,
        &[
            user(START, "tailhashkeep"),
            spend_cap_429(&iso(cap)),
            user(&iso(ms(END) + 1_000), "tailhashkeep"),
        ],
    );
    let now = ms(END) + 62_000;
    let mut p = Producer::new();
    p.tick(now);
    let drained = p.drain_spend_cap();
    assert_eq!(
        drained.pending.get("tailhashkeep").map(|f| f.ts),
        Some(cap),
        "the first read delivers the 429"
    );
    write_lines(
        &transcript,
        &[
            user(START, "tailhashkeep"),
            spend_cap_429(&iso(cap)),
            user(&iso(ms(END) + 1_500), "tailhashkeep"),
            user(&iso(ms(END) + 3_000), "tailhashkeep"),
        ],
    );
    p.tick(now + 6_000);
    let again = p.drain_spend_cap();
    assert!(
        again.pending.is_empty(),
        "the tail-hash re-read re-delivered the drained 429: {:?}",
        again.pending
    );
}

/// A 429 appended after a fast clock is corrected back an hour still delivers:
/// its stamp sits a full hour below the earlier success, but it is a new line.
#[test]
fn a_429_appended_after_a_forward_clock_correction_delivers() {
    let home = HomeSandbox::new();
    reg(None);
    let transcript = iso_kerry(home.home()).join("fastclock.jsonl");
    let t = ms(END) + 2_000;
    let hour = 3_600_000;
    let success_ts = t + 2 * hour;
    let now = ms(END) + 62_000 + 2 * hour;
    write_lines(
        &transcript,
        &[
            user(START, "fastclock"),
            assistant(&iso(success_ts), "fastclock", "msg_fast", 1222),
        ],
    );
    let mut p = Producer::new();
    p.tick(now);
    let success = p.spend_cap_recovery("fastclock", &transcript);
    assert_eq!(
        success.map(|recovery| recovery.ts),
        Some(success_ts),
        "the fast-clock success is its file's record"
    );
    append_lines(&transcript, &[spend_cap_429(&iso(t + hour))]);
    p.tick(now - hour);
    let drained = p.drain_spend_cap();
    assert_eq!(
        drained.pending.get("fastclock").map(|f| f.ts),
        Some(t + hour),
        "the corrected 429 delivers"
    );
    assert_eq!(
        p.spend_cap_recovery("fastclock", &transcript),
        success,
        "no new success on the second read"
    );
}

/// An owed-day read-back of a file discovery joined at its length (its mtime
/// predates the cutoff) delivers no spend-cap fact: the producer reads it only
/// to rebuild a past day's sums, so lines discovery skipped mint nothing, even
/// though they are stamped after the cutoff.
#[test]
fn an_owed_day_read_back_of_a_cold_file_delivers_no_spend_cap_fact() {
    let home = HomeSandbox::new();
    let session = "owedcap";
    let path = iso_kerry(home.home()).join(format!("{session}.jsonl"));
    let day = "2026-09-20";
    let line_ts = ms("2026-09-21T12:00:00.000Z");
    write_lines(
        &path,
        &[
            user(&iso(line_ts), session),
            assistant(&iso(line_ts + 2_000), session, "msg_owedcap", 500),
            spend_cap_429(&iso(line_ts + 4_000)),
        ],
    );
    crate::testutil::set_mtime(
        &path,
        std::time::UNIX_EPOCH + std::time::Duration::from_millis(ms("2026-09-20T12:00:00.000Z")),
    );
    write_owed(home.home(), &[day]);
    let mut p = Producer::new();
    p.tick(ms(END) + 62_000);
    assert!(
        p.responses.contains_key("msg_owedcap"),
        "the owed-day read-back read the cold file"
    );
    let drained = p.drain_spend_cap();
    assert!(
        drained.pending.is_empty(),
        "an owed-day read-back mints no 429 for a file discovery skipped: {:?}",
        drained.pending
    );
    assert_eq!(
        p.spend_cap_recovery("owedcap", &path),
        None,
        "an owed-day read-back mints no success record for a file discovery skipped"
    );
}

/// A cold-joined file that gains a 429 and then an owed-day rebuild reads it
/// delivers that 429 exactly once: the rebuild's re-read mints the fact (the
/// appended line ends past the join length). The later hot tail starts past
/// the 429 (the rebuild advanced the offset to its end), so the line is never
/// read again and mints nothing.
#[test]
fn a_cold_joined_file_delivers_its_rebuilt_429_exactly_once() {
    let home = HomeSandbox::new();
    reg(None);
    let now = ms(END) + 62_000;
    let transcript = iso_kerry(home.home()).join("coldrebuild.jsonl");
    write_lines(
        &transcript,
        &[
            user("2026-09-20T10:00:00.000Z", "coldrebuild"),
            spend_cap_429(&iso(ms("2026-09-20T10:00:05.000Z"))),
        ],
    );
    crate::testutil::set_mtime(
        &transcript,
        std::time::UNIX_EPOCH + std::time::Duration::from_millis(ms("2026-09-20T11:00:00.000Z")),
    );
    let days = home.home().join(".clauth").join(TPS_DAYS_FILE);
    std::fs::create_dir_all(&days).unwrap();
    let mut p = Producer::new();
    p.tick(now);
    assert!(
        p.drain_spend_cap().pending.is_empty(),
        "a cold join delivers no 429 before the rebuild"
    );
    let cap = ms("2026-09-20T11:30:05.000Z");
    append_lines(
        &transcript,
        &[
            user("2026-09-20T11:30:00.000Z", "coldrebuild"),
            spend_cap_429(&iso(cap)),
        ],
    );
    crate::testutil::set_mtime(
        &transcript,
        std::time::UNIX_EPOCH + std::time::Duration::from_millis(ms("2026-09-20T12:00:00.000Z")),
    );
    std::fs::remove_dir(&days).unwrap();
    write_owed(home.home(), &["2026-09-19"]);
    p.tick(now + 10_000);
    let drained = p.drain_spend_cap();
    assert_eq!(
        drained.pending.get("coldrebuild").map(|f| f.ts),
        Some(cap),
        "the owed-day rebuild delivers the appended 429 once"
    );
    append_lines(&transcript, &[user(END, "coldrebuild")]);
    p.tick(now + 20_000);
    let again = p.drain_spend_cap();
    assert!(
        again.pending.is_empty(),
        "the later hot tail re-delivered the drained 429: {:?}",
        again.pending
    );
}

/// The cold re-entry's other side: a cold new entry whose path holds no 429
/// this producer delivered keeps joining at its length, so its history mints
/// nothing when the file turns hot and the entry's life survives; only a line
/// past the join delivers.
#[test]
fn a_cold_new_entry_without_a_delivered_429_joins_at_its_length() {
    let home = HomeSandbox::new();
    let path = global_store(home.home()).join("cold-join.jsonl");
    let history_cap_ts = ms(END) + 2_000;
    let history_uuid = format!("429-{}", iso(history_cap_ts));
    write_lines(
        &path,
        &[
            user(START, "cold-join"),
            spend_cap_429_uuid(&iso(history_cap_ts)),
            assistant(&iso(history_cap_ts - 5_000), "cold-join", "msg_cold", 400),
        ],
    );
    crate::testutil::set_mtime(
        &path,
        std::time::UNIX_EPOCH + std::time::Duration::from_millis(ms(END) - 3 * 86_400_000),
    );
    let now = ms(END) + 62_000;
    let mut p = Producer::new();
    p.tick(now);
    let joined = file_life(&p, &path).expect("the cold file joined the walk");
    // Turn the file hot with a line past the join: only that line is read.
    let later = ms(END) + 4_000;
    append_lines(&path, &[spend_cap_429_uuid(&iso(later))]);
    p.tick(now + 10_000);
    let drained = p.drain_spend_cap();
    assert_eq!(
        drained.pending.get("cold-join").map(|f| f.ts),
        Some(later),
        "the line past the join delivers"
    );
    assert_eq!(
        p.spend_cap_recovery("cold-join", &path),
        None,
        "the cold history replayed a success"
    );
    assert_eq!(
        p.spend_cap_position("cold-join", &history_uuid, &path),
        None,
        "the history 429 below the join was never delivered"
    );
    assert_eq!(
        file_life(&p, &path),
        Some(joined),
        "the join-at-length read kept the entry's life: no byte-0 re-read"
    );
}

/// A 429 already delivered this life at another path in the same conversation
/// does not make a cold new entry re-read: the predicate is scoped to the
/// entry's own path, so the entry keeps joining at its length.
#[test]
fn a_429_delivered_at_another_path_does_not_make_a_cold_entry_reread() {
    let home = HomeSandbox::new();
    // The conversation's 429 is delivered first at a rescue sibling.
    let other = global_store(home.home()).join("cold-join.rescued-1.jsonl");
    let other_cap = ms(END) + 1_000;
    write_lines(
        &other,
        &[
            user(START, "cold-join"),
            spend_cap_429_uuid(&iso(other_cap)),
        ],
    );
    let now = ms(END) + 62_000;
    let mut p = Producer::new();
    p.tick(now);
    assert_eq!(
        p.drain_spend_cap().pending.get("cold-join").map(|f| f.ts),
        Some(other_cap),
        "the other path's 429 delivered before the cold entry joined"
    );
    // The entry's own path holds no delivered 429; its history lies below the
    // join and a later line sits past it.
    let path = global_store(home.home()).join("cold-join.jsonl");
    let history_cap_ts = ms(END) + 2_000;
    let history_uuid = format!("429-{}", iso(history_cap_ts));
    write_lines(
        &path,
        &[
            user(START, "cold-join"),
            spend_cap_429_uuid(&iso(history_cap_ts)),
            assistant(&iso(history_cap_ts - 5_000), "cold-join", "msg_cold", 400),
        ],
    );
    crate::testutil::set_mtime(
        &path,
        std::time::UNIX_EPOCH + std::time::Duration::from_millis(ms(END) - 3 * 86_400_000),
    );
    p.tick(now + 10_000);
    let joined = file_life(&p, &path).expect("the cold file joined the walk");
    // Turn the file hot with a line past the join: only that line is read.
    let later = ms(END) + 4_000;
    append_lines(&path, &[spend_cap_429_uuid(&iso(later))]);
    p.tick(now + 20_000);
    let drained = p.drain_spend_cap();
    assert_eq!(
        drained.pending.get("cold-join").map(|f| f.ts),
        Some(later),
        "the line past the join delivers"
    );
    assert_eq!(
        p.spend_cap_position("cold-join", &history_uuid, &path),
        None,
        "the history 429 below the join was never delivered"
    );
    assert_eq!(
        file_life(&p, &path),
        Some(joined),
        "another path's delivered 429 made the cold entry re-read from byte 0"
    );
}

/// A hot file's first read delivers a 429 stamped before the cutoff: the 429
/// arm has no cutoff gate, only the position bound.
#[test]
fn a_first_read_delivers_a_pre_cutoff_429() {
    let home = HomeSandbox::new();
    reg(None);
    let transcript = iso_kerry(home.home()).join("precut.jsonl");
    let pre = ms("2026-09-20T12:00:00.000Z");
    write_lines(
        &transcript,
        &[user(START, "precut"), spend_cap_429(&iso(pre))],
    );
    let mut p = Producer::new();
    p.tick(ms(END) + 62_000);
    let drained = p.drain_spend_cap();
    assert_eq!(
        drained.pending.get("precut").map(|f| f.ts),
        Some(pre),
        "a first read delivers a pre-cutoff 429"
    );
}

/// A drained 429 that an in-place rewrite moves to STRADDLE the old offset
/// does not deliver again: its line uuid already folded, so the byte-0 re-read
/// skips it even though its new end sits past the old bound.
#[test]
fn an_in_place_rewrite_does_not_redeliver_a_drained_429_it_moved_to_straddle() {
    let home = HomeSandbox::new();
    reg(None);
    let t = iso_kerry(home.home()).join("s1.jsonl");
    let u2 = user(&iso(ms(END) + 1_000), "s1");
    write_lines(
        &t,
        &[
            user(START, "s1"),
            spend_cap_429_uuid(&iso(ms(END))),
            u2.clone(),
        ],
    );
    let now = ms(END) + 62_000;
    let mut p = Producer::new();
    p.tick(now);
    let drained = p.drain_spend_cap();
    assert_eq!(
        drained.pending.get("s1").map(|f| f.ts),
        Some(ms(END)),
        "the first read delivers the 429"
    );
    let pad = pad_line(u2.len() + 60);
    write_lines(
        &t,
        &[
            pad,
            user(START, "s1"),
            spend_cap_429_uuid(&iso(ms(END))),
            user(&iso(ms(END) + 1_500), "s1"),
            user(&iso(ms(END) + 3_000), "s1"),
        ],
    );
    p.tick(now + 10_000);
    let again = p.drain_spend_cap();
    assert!(
        again.pending.is_empty(),
        "the in-place rewrite re-delivered the drained 429: {:?}",
        again.pending
    );
}

/// A drained 429 that an in-place rewrite moves wholly PAST the old offset
/// does not deliver again: its line uuid already folded.
#[test]
fn an_in_place_rewrite_does_not_redeliver_a_drained_429_it_moved_past_the_offset() {
    let home = HomeSandbox::new();
    reg(None);
    let t = iso_kerry(home.home()).join("s2.jsonl");
    write_lines(
        &t,
        &[
            user(START, "s2"),
            spend_cap_429_uuid(&iso(ms(END))),
            user(&iso(ms(END) + 1_000), "s2"),
        ],
    );
    let now = ms(END) + 62_000;
    let mut p = Producer::new();
    p.tick(now);
    let drained = p.drain_spend_cap();
    assert_eq!(
        drained.pending.get("s2").map(|f| f.ts),
        Some(ms(END)),
        "the first read delivers the 429"
    );
    let old = std::fs::metadata(&t).unwrap().len() as usize;
    write_lines(
        &t,
        &[
            pad_line(old),
            user(START, "s2"),
            spend_cap_429_uuid(&iso(ms(END))),
            user(&iso(ms(END) + 1_000), "s2"),
        ],
    );
    p.tick(now + 10_000);
    let again = p.drain_spend_cap();
    assert!(
        again.pending.is_empty(),
        "the rewrite re-delivered the drained 429: {:?}",
        again.pending
    );
}

/// A shrink re-reads from byte 0 with the `All` bound, so a success already in
/// the record lands in it again, in the file's NEW life. Re-delivery is
/// allowed, not required; the latch outcome is pinned by
/// `an_older_same_file_success_does_not_drop_the_latched_429` in the scheduler
/// tests.
#[test]
fn a_shrink_redelivers_a_drained_recovery() {
    let home = HomeSandbox::new();
    reg(None);
    let t = iso_kerry(home.home()).join("s3.jsonl");
    let head = [
        user(START, "s3"),
        assistant(&iso(ms(END) + 2_000), "s3", "msg_s3", 1222),
    ];
    let mut all = head.to_vec();
    all.push(user(&iso(ms(END) + 3_000), "s3"));
    all.push(user(&iso(ms(END) + 4_000), "s3"));
    write_lines(&t, &all);
    let now = ms(END) + 62_000;
    let mut p = Producer::new();
    p.tick(now);
    let first = p
        .spend_cap_recovery("s3", &t)
        .expect("the first read records the success");
    assert_eq!(
        first.ts,
        ms(END) + 2_000,
        "the first read records the success"
    );
    write_lines(&t, &head);
    p.tick(now + 10_000);
    let again = p
        .spend_cap_recovery("s3", &t)
        .expect("the shrink re-reads the success");
    assert_eq!(
        again.ts,
        ms(END) + 2_000,
        "the shrink re-reads the success: successes are not uuid-deduped"
    );
    assert_ne!(
        again.life, first.life,
        "the re-read lands in the file's new life"
    );
}

/// A file that leaves one walk and returns re-enters fresh at offset 0, but a
/// drained 429's line uuid survives the walk, so it does not deliver again.
#[test]
fn a_walk_miss_does_not_redeliver_a_drained_429() {
    let home = HomeSandbox::new();
    reg(None);
    let t = iso_kerry(home.home()).join("q4c.jsonl");
    write_lines(&t, &[user(START, "q4c"), spend_cap_429_uuid(&iso(ms(END)))]);
    let now = ms(END) + 62_000;
    let mut p = Producer::new();
    p.tick(now);
    let drained = p.drain_spend_cap();
    assert_eq!(
        drained.pending.get("q4c").map(|f| f.ts),
        Some(ms(END)),
        "the first read delivers the 429"
    );
    let away = t.with_extension("away");
    std::fs::rename(&t, &away).unwrap();
    p.tick(now + 5_000);
    assert!(
        p.drain_spend_cap().pending.is_empty(),
        "the away tick mints nothing"
    );
    std::fs::rename(&away, &t).unwrap();
    p.tick(now + 10_000);
    let again = p.drain_spend_cap();
    assert!(
        again.pending.is_empty(),
        "the walk miss re-delivered the drained 429: {:?}",
        again.pending
    );
}

/// A transcript deleted from disk takes its success record out of the walk:
/// nothing else ever removes that record, so without the walk-miss prune it
/// would outlive the file for the producer's whole life.
#[test]
fn a_walk_miss_drops_the_deleted_paths_success_record() {
    let home = HomeSandbox::new();
    reg(None);
    let t = iso_kerry(home.home()).join("prune.jsonl");
    write_lines(
        &t,
        &[
            user(START, "prune"),
            assistant(END, "prune", "msg_prune", 1222),
        ],
    );
    let now = ms(END) + 62_000;
    let mut p = Producer::new();
    p.tick(now);
    assert!(
        p.spend_cap_recovery("prune", &t).is_some(),
        "control: the read recorded the success"
    );

    std::fs::remove_file(&t).unwrap();
    p.tick(now + DISCOVER_INTERVAL_MS);
    assert_eq!(
        p.spend_cap_recovery("prune", &t),
        None,
        "the deleted transcript's success record survived its walk miss"
    );
}

/// A transcript rescued to a new path (isolated → global; the rescued name
/// keeps its conversation key) does not re-deliver a drained 429: the line
/// uuid survives the path change.
#[test]
fn a_rescue_move_does_not_redeliver_a_drained_429() {
    let home = HomeSandbox::new();
    reg(None);
    let src = iso_kerry(home.home()).join("rescue.jsonl");
    write_lines(
        &src,
        &[user(START, "rescue"), spend_cap_429_uuid(&iso(ms(END)))],
    );
    let now = ms(END) + 62_000;
    let mut p = Producer::new();
    p.tick(now);
    let drained = p.drain_spend_cap();
    assert_eq!(
        drained.pending.get("rescue").map(|f| f.ts),
        Some(ms(END)),
        "the first read delivers the 429"
    );
    let dst = global_store(home.home()).join("rescue.rescued-1.jsonl");
    std::fs::create_dir_all(dst.parent().unwrap()).unwrap();
    std::fs::rename(&src, &dst).unwrap();
    p.tick(now + 6_000);
    let again = p.drain_spend_cap();
    assert!(
        again.pending.is_empty(),
        "the rescue re-delivered the drained 429: {:?}",
        again.pending
    );
}

/// A rewrite that replaces a drained 429 with a NEW 429 line (a new uuid)
/// delivers the new fact: the dedupe keys on the uuid, so a genuinely new line
/// is never suppressed by an old one it happened to move past.
#[test]
fn a_rewrite_delivers_a_new_429_with_a_new_uuid() {
    let home = HomeSandbox::new();
    reg(None);
    let t = iso_kerry(home.home()).join("newuuid.jsonl");
    write_lines(
        &t,
        &[user(START, "newuuid"), spend_cap_429_uuid(&iso(ms(END)))],
    );
    let now = ms(END) + 62_000;
    let mut p = Producer::new();
    p.tick(now);
    let drained = p.drain_spend_cap();
    assert_eq!(
        drained.pending.get("newuuid").map(|f| f.ts),
        Some(ms(END)),
        "the first read delivers the 429"
    );
    let old = std::fs::metadata(&t).unwrap().len() as usize;
    write_lines(
        &t,
        &[
            pad_line(old),
            user(START, "newuuid"),
            spend_cap_429_uuid(&iso(ms(END) + 5_000)),
        ],
    );
    p.tick(now + 10_000);
    let drained = p.drain_spend_cap();
    assert_eq!(
        drained.pending.get("newuuid").map(|f| f.ts),
        Some(ms(END) + 5_000),
        "a rewrite delivers a new 429 with a new uuid"
    );
}

/// A uuid-less 429 keeps the offset-bound behaviour: a rewrite that moves the
/// drained line wholly past the old offset re-delivers it, because there is no
/// line uuid to dedupe on.
#[test]
fn a_uuid_less_429_keeps_the_offset_bound() {
    let home = HomeSandbox::new();
    reg(None);
    let t = iso_kerry(home.home()).join("nouuid.jsonl");
    write_lines(
        &t,
        &[
            user(START, "nouuid"),
            spend_cap_429(&iso(ms(END))),
            user(&iso(ms(END) + 1_000), "nouuid"),
        ],
    );
    let now = ms(END) + 62_000;
    let mut p = Producer::new();
    p.tick(now);
    let drained = p.drain_spend_cap();
    assert_eq!(
        drained.pending.get("nouuid").map(|f| f.ts),
        Some(ms(END)),
        "the first read delivers the 429"
    );
    let old = std::fs::metadata(&t).unwrap().len() as usize;
    write_lines(
        &t,
        &[
            pad_line(old),
            user(START, "nouuid"),
            spend_cap_429(&iso(ms(END))),
            user(&iso(ms(END) + 1_000), "nouuid"),
        ],
    );
    p.tick(now + 10_000);
    let drained = p.drain_spend_cap();
    assert_eq!(
        drained.pending.get("nouuid").map(|f| f.ts),
        Some(ms(END)),
        "a uuid-less 429 re-delivers when the offset bound passes"
    );
}

/// A line whose `uuid` is an escaped JSON string still counts its tokens, and
/// a 429 row with an escaped uuid still delivers. The escaped uuid must read
/// as a uuid, not absent: a shrink that re-reads the drained 429 does not
/// deliver it again.
#[test]
fn an_escaped_uuid_line_still_counts_and_its_429_delivers() {
    let home = HomeSandbox::new();
    reg(None);
    let t = iso_kerry(home.home()).join("escuuid.jsonl");
    let plain_ok = assistant(END, "escuuid", "msg_escuuid", 500);
    let ok = plain_ok.replacen("\"uuid\":\"a-", "\"uuid\":\"\\u0061-", 1);
    assert_ne!(
        ok, plain_ok,
        "the escaped-uuid replacen applied to the assistant line"
    );
    let plain_cap = spend_cap_429_uuid(&iso(ms(END) + 2_000));
    let cap = plain_cap.replacen("\"uuid\":\"429-", "\"uuid\":\"\\u0034\\u0032\\u0039-", 1);
    assert_ne!(
        cap, plain_cap,
        "the escaped-uuid replacen applied to the 429 row"
    );
    write_lines(&t, &[user(START, "escuuid"), ok, cap.clone()]);
    let mut p = Producer::new();
    let snap = p.tick(ms(END) + 62_000);
    assert_eq!(
        sums_only(&snap.days["2026-09-22"]["kerry"][MODEL]),
        DaySum {
            output: 500,
            gen_ms: 13_597,
            ..Default::default()
        },
        "the escaped-uuid assistant line still counts its tokens"
    );
    let drained = p.drain_spend_cap();
    assert_eq!(
        drained.pending.get("escuuid").map(|f| f.ts),
        Some(ms(END) + 2_000),
        "the escaped-uuid 429 still delivers"
    );
    // The escaped uuid reads as a uuid, so a shrink re-read folds it.
    write_lines(&t, &[user(START, "escuuid"), cap]);
    p.tick(ms(END) + 72_000);
    let again = p.drain_spend_cap();
    assert!(
        again.pending.is_empty(),
        "the shrink re-delivered the escaped-uuid 429: {:?}",
        again.pending
    );
}

/// A line whose `uuid` is a non-string JSON value (a number) reads as absent
/// instead of failing the whole line: the assistant line still counts its
/// tokens, and the 429 reads as uuid-less and still delivers.
#[test]
fn a_non_string_uuid_line_still_counts_and_its_429_delivers() {
    let home = HomeSandbox::new();
    reg(None);
    let t = iso_kerry(home.home()).join("numuuid.jsonl");
    let mut ok: serde_json::Value =
        serde_json::from_str(&assistant(END, "numuuid", "msg_numuuid", 500)).unwrap();
    ok["uuid"] = serde_json::json!(5);
    let mut cap: serde_json::Value =
        serde_json::from_str(&spend_cap_429_uuid(&iso(ms(END) + 2_000))).unwrap();
    cap["uuid"] = serde_json::json!(7);
    write_lines(
        &t,
        &[user(START, "numuuid"), ok.to_string(), cap.to_string()],
    );
    let mut p = Producer::new();
    let snap = p.tick(ms(END) + 62_000);
    assert_eq!(
        sums_only(&snap.days["2026-09-22"]["kerry"][MODEL]),
        DaySum {
            output: 500,
            gen_ms: 13_597,
            ..Default::default()
        },
        "the non-string-uuid assistant line still counts its tokens"
    );
    let drained = p.drain_spend_cap();
    assert_eq!(
        drained.pending.get("numuuid").map(|f| f.ts),
        Some(ms(END) + 2_000),
        "the non-string-uuid 429 reads as uuid-less and still delivers"
    );
}

/// A 429 whose `uuid` re-parse fails (a duplicate `uuid` key, an out-of-range
/// float, a lone-surrogate escape) still delivers: the failed read falls back
/// to the position bound, so no odd `uuid` shape can lose the 429.
#[test]
fn a_429_whose_uuid_read_fails_still_delivers() {
    let cases: [(&str, &str); 3] = [
        ("a duplicate uuid key", "\"uuid\":\"a\",\"uuid\":\"b\","),
        ("an out-of-range float uuid", "\"uuid\":1e400,"),
        ("a lone-surrogate uuid", "\"uuid\":\"\\ud800\","),
    ];
    for (name, uuid_field) in cases {
        let home = HomeSandbox::new();
        reg(None);
        let t = iso_kerry(home.home()).join("baduuid.jsonl");
        let ts = ms(END) + 2_000;
        write_lines(
            &t,
            &[
                user(START, "baduuid"),
                spend_cap_429_raw_uuid(&iso(ts), uuid_field),
            ],
        );
        let mut p = Producer::new();
        p.tick(ms(END) + 62_000);
        let drained = p.drain_spend_cap();
        assert_eq!(
            drained.pending.get("baduuid").map(|f| f.ts),
            Some(ts),
            "{name}: a 429 whose uuid read fails still delivers by the offset bound"
        );
    }
}

/// Two distinct real 429 rows in one session, each carrying an empty `uuid`
/// string, both deliver: the empty uuid reads as absent, never as a shared
/// dedupe key, so the offset bound delivers each new line once.
#[test]
fn two_empty_uuid_429s_in_one_session_both_deliver() {
    let home = HomeSandbox::new();
    reg(None);
    let t = iso_kerry(home.home()).join("emptyuuid.jsonl");
    let mk = |at: u64| spend_cap_429_raw_uuid(&iso(at), "\"uuid\":\"\",");
    write_lines(&t, &[user(START, "emptyuuid"), mk(ms(END) + 2_000)]);
    let mut p = Producer::new();
    p.tick(ms(END) + 62_000);
    let first = p.drain_spend_cap();
    assert_eq!(
        first.pending.get("emptyuuid").map(|f| f.ts),
        Some(ms(END) + 2_000),
        "the first empty-uuid 429 delivers"
    );
    append_lines(&t, &[mk(ms(END) + 70_000)]);
    p.tick(ms(END) + 72_000);
    let second = p.drain_spend_cap();
    assert_eq!(
        second.pending.get("emptyuuid").map(|f| f.ts),
        Some(ms(END) + 70_000),
        "the second empty-uuid 429 delivers too"
    );
}

/// A conversation's only file that misses four walks and returns does not
/// re-deliver a drained 429: the 429 uuid record is never pruned. The fixture
/// spans four walks over 16 s, so a prune after more walks or a longer
/// absence would pass it — none exists to outlast.
#[test]
fn a_walk_miss_of_many_walks_does_not_redeliver_a_drained_429() {
    let home = HomeSandbox::new();
    reg(None);
    let t = iso_kerry(home.home()).join("manywalk.jsonl");
    write_lines(
        &t,
        &[user(START, "manywalk"), spend_cap_429_uuid(&iso(ms(END)))],
    );
    let now = ms(END) + 62_000;
    let mut p = Producer::new();
    p.tick(now);
    let drained = p.drain_spend_cap();
    assert_eq!(
        drained.pending.get("manywalk").map(|f| f.ts),
        Some(ms(END)),
        "the first read delivers the 429"
    );
    let away = t.with_extension("away");
    std::fs::rename(&t, &away).unwrap();
    p.tick(now + DISCOVER_INTERVAL_MS);
    p.tick(now + 2 * DISCOVER_INTERVAL_MS);
    p.tick(now + 2 * DISCOVER_INTERVAL_MS + 200);
    p.tick(now + 2 * DISCOVER_INTERVAL_MS + 400);
    std::fs::rename(&away, &t).unwrap();
    p.tick(now + 3 * DISCOVER_INTERVAL_MS + 1_000);
    let again = p.drain_spend_cap();
    assert!(
        again.pending.is_empty(),
        "the multi-walk miss re-delivered the drained 429: {:?}",
        again.pending
    );
}

/// Many success lines leave the 429 uuid set empty: a success line is never
/// uuid-deduped, so its uuid never enters the set (only 429 rows do).
#[test]
fn many_success_lines_leave_the_429_uuid_set_empty() {
    let home = HomeSandbox::new();
    reg(None);
    let t = iso_kerry(home.home()).join("succonly.jsonl");
    let mut lines = vec![user(START, "succonly")];
    for i in 0..5 {
        lines.push(assistant(
            &iso(ms(END) + i * 1_000),
            "succonly",
            &format!("msg_{i}"),
            100,
        ));
    }
    write_lines(&t, &lines);
    let mut p = Producer::new();
    p.tick(ms(END) + 62_000);
    assert_eq!(
        p.spend_cap_recovery("succonly", &t)
            .map(|recovery| recovery.ts),
        Some(ms(END) + 4_000),
        "the newest success is its file's record"
    );
    assert!(
        p.delivered_facts.is_empty(),
        "success lines leave the 429 uuid set empty: {:?}",
        p.delivered_facts
    );
}

/// A hot file whose newest success predates the cutoff records it on the first
/// read, and a shrink re-reads it: the pre-cutoff success arm
/// folds by position alone, never by uuid. Re-delivery is allowed, not
/// required; the latch outcome is pinned by
/// `an_older_same_file_success_does_not_drop_the_latched_429` in the
/// scheduler tests.
#[test]
fn a_shrink_redelivers_a_pre_cutoff_recovery() {
    let home = HomeSandbox::new();
    reg(None);
    let t = iso_kerry(home.home()).join("precuts.jsonl");
    let success = ms("2026-09-20T12:00:00.000Z");
    let head = [
        user("2026-09-20T11:00:00.000Z", "precuts"),
        assistant(&iso(success), "precuts", "msg_precuts", 500),
    ];
    let mut all = head.to_vec();
    all.push(user(&iso(success + 1_000), "precuts"));
    write_lines(&t, &all);
    let now = ms(END) + 62_000;
    let mut p = Producer::new();
    p.tick(now);
    let first = p
        .spend_cap_recovery("precuts", &t)
        .expect("the first read records the pre-cutoff success");
    assert_eq!(
        first.ts, success,
        "the first read records the pre-cutoff success"
    );
    write_lines(&t, &head);
    p.tick(now + 10_000);
    let again = p
        .spend_cap_recovery("precuts", &t)
        .expect("the shrink re-reads the pre-cutoff success");
    assert_eq!(
        again.ts, success,
        "the shrink re-reads the pre-cutoff success: the pre-cutoff arm has no uuid gate"
    );
    assert_ne!(
        again.life, first.life,
        "the re-read lands in the file's new life"
    );
}

/// A pre-cutoff 429 keeps its uuid gate: a shrink that re-reads the drained
/// line from byte 0 does not deliver it again.
#[test]
fn a_shrink_does_not_redeliver_a_pre_cutoff_429() {
    let home = HomeSandbox::new();
    reg(None);
    let t = iso_kerry(home.home()).join("precut429.jsonl");
    let pre = ms("2026-09-20T12:00:00.000Z");
    let head = [
        user("2026-09-20T11:00:00.000Z", "precut429"),
        spend_cap_429_uuid(&iso(pre)),
    ];
    let mut all = head.to_vec();
    all.push(user(&iso(pre + 1_000), "precut429"));
    write_lines(&t, &all);
    let now = ms(END) + 62_000;
    let mut p = Producer::new();
    p.tick(now);
    let drained = p.drain_spend_cap();
    assert_eq!(
        drained.pending.get("precut429").map(|f| f.ts),
        Some(pre),
        "the first read delivers the pre-cutoff 429"
    );
    write_lines(&t, &head);
    p.tick(now + 10_000);
    let again = p.drain_spend_cap();
    assert!(
        again.pending.is_empty(),
        "the shrink re-delivered the drained pre-cutoff 429: {:?}",
        again.pending
    );
}

/// The core tail property: a tick advances the file's byte offset to its length.
#[test]
fn a_tick_advances_the_tail_offset_to_the_files_length() {
    let home = HomeSandbox::new();
    let path = global_store(home.home()).join("tail.jsonl");
    write_lines(
        &path,
        &[
            user(START, "tail"),
            assistant(END, "tail", "msg_tail", 1222),
        ],
    );
    let mut p = Producer::new();
    p.tick(ms(END) + 4_000);
    let len = std::fs::metadata(&path).unwrap().len();
    assert_eq!(p.files.get(&path).map(|e| e.offset), Some(len));
}

// ── round-3 fixes ─────────────────────────────────────────────────────────────

/// A shared session's owner follows the hook record's own change event: after a
/// switch the later response lands on the new member, the earlier stays put.
/// The record carries no stamp, so the switch takes the instant it was
/// observed at.
#[test]
fn a_shared_session_switch_after_first_read_reattributes_later_responses() {
    let home = HomeSandbox::new();
    let sid = "5d7e2a10-1b3c-4d5e-8f90-a1b2c3d4e5f6";
    let record = crate::hook_note::record_path(sid, None).unwrap();
    std::fs::create_dir_all(record.parent().unwrap()).unwrap();
    std::fs::write(&record, r#"{"resolved":"kerry"}"#).unwrap();
    let path = global_store(home.home()).join(format!("{sid}.jsonl"));
    write_lines(
        &path,
        &[user(START, sid), assistant(END, sid, "msg_a", 1222)],
    );
    let mut p = Producer::new();
    p.tick(ms(END) + 1_000);
    std::fs::write(&record, r#"{"resolved":"mommy"}"#).unwrap();
    p.tick(ms(END) + 1_500);
    append_lines(
        &path,
        &[
            user(&iso(ms(END) + 2_000), sid),
            assistant(&iso(ms(END) + 12_000), sid, "msg_b", 500),
        ],
    );
    let snap = p.tick(ms(END) + 13_000);
    assert_eq!(snap.sessions[sid].account.as_deref(), Some("mommy"));
    assert_eq!(
        snap.accounts.keys().collect::<Vec<_>>(),
        vec!["kerry", "mommy"]
    );
    assert_eq!(snap.days["2026-09-22"]["kerry"][MODEL].output, 1222);
    assert_eq!(snap.days["2026-09-22"]["mommy"][MODEL].output, 500);
}

/// A hook record that appears after the first read re-attaches the session: the
/// later response lands on the new member, the earlier stays unattributed.
/// The record carries no stamp, so it takes the instant it was observed at.
#[test]
fn a_hook_record_landing_after_first_read_reattributes_later_responses() {
    let home = HomeSandbox::new();
    let sid = "5d7e2a10-1b3c-4d5e-8f90-a1b2c3d4e5f6";
    let record = crate::hook_note::record_path(sid, None).unwrap();
    std::fs::create_dir_all(record.parent().unwrap()).unwrap();
    let path = global_store(home.home()).join(format!("{sid}.jsonl"));
    write_lines(
        &path,
        &[user(START, sid), assistant(END, sid, "msg_a", 1222)],
    );
    let mut p = Producer::new();
    p.tick(ms(END) + 1_000);
    std::fs::write(&record, r#"{"resolved":"mommy"}"#).unwrap();
    p.tick(ms(END) + 1_500);
    append_lines(
        &path,
        &[
            user(&iso(ms(END) + 2_000), sid),
            assistant(&iso(ms(END) + 12_000), sid, "msg_b", 500),
        ],
    );
    let snap = p.tick(ms(END) + 13_000);
    assert_eq!(snap.sessions[sid].account.as_deref(), Some("mommy"));
    assert_eq!(snap.accounts.keys().collect::<Vec<_>>(), vec!["mommy"]);
    assert_eq!(snap.days["2026-09-22"]["*"][MODEL].output, 1222);
    assert_eq!(snap.days["2026-09-22"]["mommy"][MODEL].output, 500);
}

/// An owed past day (`tps_days.json`) is rebuilt from its transcript and
/// folds at start; a version-2 cache's day never does: the cache is output
/// only.
#[test]
fn a_restart_folds_an_owed_day_and_never_a_version_two_cache() {
    let home = HomeSandbox::new();
    let cell = |output: u64, gen_ms: u64| {
        BTreeMap::from([(
            "kerry".to_owned(),
            BTreeMap::from([(
                MODEL.to_owned(),
                DaySum {
                    output,
                    gen_ms,
                    ..Default::default()
                },
            )]),
        )])
    };
    day_transcript(home.home(), "2026-09-19", 1, 2);
    write_owed(home.home(), &["2026-09-19"]);
    let snap = crate::tps::Snapshot {
        version: CACHE_VERSION,
        generated_at: "2026-09-25T00:00:00.000+00:00".to_owned(),
        days: BTreeMap::from([("2026-09-18".to_owned(), cell(5, 6))]),
        ..crate::tps::Snapshot::default()
    };
    write_cache(home.home(), &snap);
    let mut p = Producer::new();
    p.tick(ms("2026-09-25T00:01:00.000Z"));
    let speed = crate::token_ledger::SpeedLedger::load(&home.home().join(".clauth"));
    assert!(
        !speed.days.contains_key("2026-09-18"),
        "a version-2 cache's day folded"
    );
    assert_eq!(
        speed.days["2026-09-19"]["kerry"][MODEL],
        crate::tps::DaySum {
            output: 1,
            gen_ms: 2,
            ..Default::default()
        }
    );
    assert!(!p.days.contains_key("2026-09-19"));
}

/// Id-less growing partials share the (path, preceding user ts) key, so they
/// count once with their final output.
#[test]
fn idless_growing_partials_count_once() {
    let home = HomeSandbox::new();
    reg(None);
    let path = iso_kerry(home.home()).join("idless.jsonl");
    let start = ms(START);
    write_lines(
        &path,
        &[
            user(START, "idless"),
            assistant(&iso(start + 2_000), "idless", "", 40),
            assistant(&iso(start + 8_000), "idless", "", 1222),
        ],
    );
    let snap = Producer::new().tick(start + 13_000);
    close(
        snap.sessions["idless"].figures.m5.rate,
        1222.0 / 300.0,
        "id-less partials counted per partial",
    );
}

/// A cold file joins at its length (no whole-history read); after it grows hot
/// only the appended line is read and the offset advances.
#[test]
fn a_cold_file_joins_at_len_and_reads_only_the_appended_tail() {
    let home = HomeSandbox::new();
    let path = global_store(home.home()).join("cold.jsonl");
    write_lines(
        &path,
        &[user(START, "cold"), assistant(END, "cold", "msg_old", 1222)],
    );
    let old = std::time::UNIX_EPOCH + std::time::Duration::from_millis(ms(START));
    crate::testutil::set_mtime(&path, old);
    let mut p = Producer::new();
    let now = ms(END) + 3 * 86_400_000;
    p.tick(now);
    assert_eq!(p.responses.len(), 0, "a cold file is skipped");
    let old_len = std::fs::metadata(&path).unwrap().len();
    assert_eq!(p.files.get(&path).map(|e| e.offset), Some(old_len));

    append_lines(
        &path,
        &[
            user(&iso(now - 1_000), "cold"),
            assistant(&iso(now), "cold", "msg_new", 500),
        ],
    );
    crate::testutil::set_mtime(&path, std::time::SystemTime::now());
    p.tick(now + 6_000);
    assert!(
        p.responses.contains_key("msg_new"),
        "the appended line was read"
    );
    assert!(
        !p.responses.contains_key("msg_old"),
        "the old lines stayed unread"
    );
    let new_len = std::fs::metadata(&path).unwrap().len();
    assert_eq!(p.files.get(&path).map(|e| e.offset), Some(new_len));
}

// ── round-4 fixes ─────────────────────────────────────────────────────────────

/// An out-of-order subagent reply (read after the parent's later reply) keeps
/// its member across a restart: both replies start before the restart's first
/// tick, which records the unstamped swap, so the restarted producer re-reads
/// both under `kerry` and adds neither under the new member.
#[test]
fn out_of_order_subagent_reply_keeps_its_member_after_a_restart() {
    let home = HomeSandbox::new();
    reg(None);
    let s = ms(START);
    let parent = iso_kerry(home.home()).join("par.jsonl");
    let sub = iso_kerry(home.home()).join("par/subagents/agent-a.jsonl");
    write_lines(
        &parent,
        &[
            user(START, "par"),
            assistant(&iso(s + 20_000), "par", "msg_parent", 1000),
        ],
    );
    let mut p = Producer::new();
    p.tick(s + 26_000);
    write_lines(
        &sub,
        &[
            user(START, "par"),
            assistant(&iso(s + 15_000), "par", "msg_sub", 500),
        ],
    );
    let snap = p.tick(s + 32_000);
    assert_eq!(snap.days["2026-09-22"]["kerry"][MODEL].output, 1500);
    write_cache(home.home(), &snap);

    reg(Some("mommy"));
    let restarted = Producer::new().tick(s + 33_000);
    assert_eq!(
        restarted.days["2026-09-22"]["kerry"][MODEL].output, 1500,
        "both replies stay on the member that spent them"
    );
    assert!(!restarted.days["2026-09-22"].contains_key("mommy"));
}

/// A new subagent file read after a swap takes the member the timeline gives
/// its span start (mommy, after the swap's stamp); the split survives a
/// restart.
#[test]
fn a_new_file_after_a_swap_uses_the_member_that_spent_it() {
    let home = HomeSandbox::new();
    reg(None);
    let s = ms(START);
    let parent = iso_kerry(home.home()).join("p2.jsonl");
    write_lines(
        &parent,
        &[
            user(START, "p2"),
            assistant(&iso(s + 10_000), "p2", "msg_parent", 1000),
        ],
    );
    let mut p = Producer::new();
    let snap = p.tick(s + 11_000);
    write_cache(home.home(), &snap);

    reg_swapped("mommy", s + 11_500);
    let sub = iso_kerry(home.home()).join("p2/subagents/agent-a.jsonl");
    write_lines(
        &sub,
        &[
            user(&iso(s + 12_000), "p2"),
            assistant(&iso(s + 20_000), "p2", "msg_sub", 500),
        ],
    );
    let snap2 = p.tick(s + 21_000);
    assert_eq!(snap2.days["2026-09-22"]["kerry"][MODEL].output, 1000);
    assert_eq!(snap2.days["2026-09-22"]["mommy"][MODEL].output, 500);
    write_cache(home.home(), &snap2);

    let restarted = Producer::new().tick(s + 22_000);
    assert_eq!(restarted.days["2026-09-22"]["kerry"][MODEL].output, 1000);
    assert_eq!(restarted.days["2026-09-22"]["mommy"][MODEL].output, 500);
}

/// A subagent reply read in the tick that observes a swap but spent before it
/// (its span ends before the swap's stamp) stays on the member that spent it,
/// live and across a restart: its span starts before the swap's stamp, so the
/// timeline gives it the old member.
#[test]
fn out_of_order_reads_across_a_swap_follow_the_swap_instant() {
    let home = HomeSandbox::new();
    reg(None);
    let s = ms(START);
    let parent = iso_kerry(home.home()).join("p3.jsonl");
    let sub = iso_kerry(home.home()).join("p3/subagents/agent-a.jsonl");
    write_lines(
        &parent,
        &[
            user(START, "p3"),
            assistant(&iso(s + 10_000), "p3", "msg_a", 1000),
            assistant(&iso(s + 30_000), "p3", "msg_b", 300),
        ],
    );
    let mut p = Producer::new();
    p.tick(s + 31_000);
    reg_swapped("mommy", s + 35_000);
    write_lines(
        &sub,
        &[
            user(&iso(s + 18_000), "p3"),
            assistant(&iso(s + 20_000), "p3", "msg_sub", 500),
        ],
    );
    let snap2 = p.tick(s + 40_000);
    assert_eq!(snap2.days["2026-09-22"]["kerry"][MODEL].output, 1800);
    assert!(!snap2.days["2026-09-22"].contains_key("mommy"));
    write_cache(home.home(), &snap2);

    let restarted = Producer::new().tick(s + 41_000);
    assert_eq!(restarted.days["2026-09-22"]["kerry"][MODEL].output, 1800);
    assert!(!restarted.days["2026-09-22"].contains_key("mommy"));
}

// ── round-6 ──────────────────────────────────────────────────────────────────

/// A growing partial adds only its delta to the day sums (the additive core).
#[test]
fn a_growing_partial_adds_only_its_delta_to_the_day_sums() {
    let home = HomeSandbox::new();
    reg(None);
    let path = iso_kerry(home.home()).join("grow.jsonl");
    let start = ms(START);
    write_lines(
        &path,
        &[
            user(START, "grow"),
            assistant(&iso(start + 2_000), "grow", "msg_grow", 40),
        ],
    );
    let mut p = Producer::new();
    p.tick(start + 8_500);
    append_lines(
        &path,
        &[assistant(&iso(start + 8_000), "grow", "msg_grow", 700)],
    );
    p.tick(start + 12_000);
    append_lines(&path, &[assistant(END, "grow", "msg_grow", 1222)]);
    let snap = p.tick(ms(END) + 1_000);
    assert_eq!(
        sums_only(&snap.days["2026-09-22"]["kerry"][MODEL]),
        DaySum {
            output: 1222,
            gen_ms: 13_597,
            ..Default::default()
        }
    );
}

/// An owed day that rolled out of yesterday is rebuilt at start, folds into
/// token_speed.json, and is gone from the snapshot's `days` (the producer is
/// the speed ledger's only writer).
#[test]
fn a_day_rolling_out_of_yesterday_folds_into_the_speed_ledger() {
    let home = HomeSandbox::new();
    day_transcript(home.home(), "2026-09-20", 7, 3);
    write_owed(home.home(), &["2026-09-20"]);
    let mut p = Producer::new();
    let after = p.tick(ms("2026-09-22T00:01:00.000Z"));
    let speed = crate::token_ledger::SpeedLedger::load(&home.home().join(".clauth"));
    assert_eq!(
        speed.days["2026-09-20"]["kerry"][MODEL],
        crate::tps::DaySum {
            output: 7,
            gen_ms: 3,
            ..Default::default()
        }
    );
    assert!(
        !after.days.contains_key("2026-09-20"),
        "the snapshot drops it"
    );
}

/// A fold that cannot land (a newer token_speed.json) keeps the owed day in
/// the list across ticks and a restart, and no tick rebuilds it while the
/// fold could not land.
#[test]
fn a_failed_fold_save_keeps_the_day_owed() {
    let home = HomeSandbox::new();
    let clauth = home.home().join(".clauth");
    std::fs::create_dir_all(&clauth).unwrap();
    std::fs::write(
        clauth.join("token_speed.json"),
        r#"{"version":99,"days":{}}"#,
    )
    .unwrap();
    day_transcript(home.home(), "2026-09-20", 7, 3);
    write_owed(home.home(), &["2026-09-20"]);
    let mut p = Producer::new();
    let t1 = p.tick(ms("2026-09-22T00:01:00.000Z"));
    assert!(!t1.days.contains_key("2026-09-20"), "tick 1 rebuilt it");
    let t2 = p.tick(ms("2026-09-22T00:02:00.000Z"));
    assert!(!t2.days.contains_key("2026-09-20"), "tick 2 rebuilt it");
    // Restart off the owed-day list the producer itself last wrote, so a
    // producer that dropped the day reds here.
    p.tick(ms("2026-09-22T00:03:00.000Z"));
    assert_eq!(
        owed_file(&clauth)["owed"],
        serde_json::json!(["2026-09-20", "2026-09-21", "2026-09-22"]),
        "the list keeps it"
    );
    let restarted = Producer::new().tick(ms("2026-09-22T00:04:00.000Z"));
    assert!(
        !restarted.days.contains_key("2026-09-20"),
        "a restart rebuilt it"
    );
    assert_eq!(
        owed_file(&clauth)["owed"],
        serde_json::json!(["2026-09-20", "2026-09-21", "2026-09-22"]),
        "a restart keeps it"
    );
}

/// A plain save failure (a non-writable `.clauth`) keeps the day owed and
/// unbuilt across two ticks, and it lands once the dir is writable again.
#[test]
fn a_plain_save_failure_retries_and_lands_once_writable() {
    let home = HomeSandbox::new();
    let clauth = home.home().join(".clauth");
    std::fs::create_dir_all(&clauth).unwrap();
    day_transcript(home.home(), "2026-09-20", 7, 3);
    write_owed(home.home(), &["2026-09-20"]);

    let unwritable = crate::testutil::UnwritableDir::new(&clauth);
    let mut p = Producer::new();
    let t1 = p.tick(ms("2026-09-22T00:01:00.000Z"));
    assert!(!t1.days.contains_key("2026-09-20"), "t1 rebuilt it");
    let t2 = p.tick(ms("2026-09-22T00:02:00.000Z"));
    assert!(!t2.days.contains_key("2026-09-20"), "t2 rebuilt it");
    assert!(p.unbuilt.contains("2026-09-20"), "t2 dropped it");
    drop(unwritable);
    let t3 = p.tick(ms("2026-09-22T00:03:00.000Z"));
    assert!(!t3.days.contains_key("2026-09-20"), "t3 folds it");
    let speed = crate::token_ledger::SpeedLedger::load(&clauth);
    assert_eq!(
        speed.days["2026-09-20"]["kerry"][MODEL],
        crate::tps::DaySum {
            output: 7,
            gen_ms: 3,
            ..Default::default()
        }
    );
}

/// The session row reads the hook's resolved fresh, even when the store batch
/// cached an older value.
#[test]
fn the_session_row_reads_the_hook_resolved_fresh() {
    let home = HomeSandbox::new();
    let sid = "5d7e2a10-1b3c-4d5e-8f90-a1b2c3d4e5f6";
    let record = crate::hook_note::record_path(sid, None).unwrap();
    std::fs::create_dir_all(record.parent().unwrap()).unwrap();
    std::fs::write(&record, r#"{"resolved":"kerry"}"#).unwrap();
    let sp = home.home().join(".clauth").join("session_profiles.json");
    std::fs::create_dir_all(sp.parent().unwrap()).unwrap();
    std::fs::write(
        &sp,
        r#"{"sessions":{"5d7e2a10-1b3c-4d5e-8f90-a1b2c3d4e5f6":{"known":"kerry"}}}"#,
    )
    .unwrap();
    let path = global_store(home.home()).join(format!("{sid}.jsonl"));
    write_lines(
        &path,
        &[user(START, sid), assistant(END, sid, "msg_a", 1222)],
    );
    let mut p = Producer::new();
    p.tick(ms(END) + 1_000);
    std::fs::write(&record, r#"{"resolved":"mommy"}"#).unwrap();
    let snap = p.tick(ms(END) + 2_000);
    assert_eq!(
        snap.sessions[sid].account.as_deref(),
        Some("mommy"),
        "the row reads the hook fresh"
    );
}

/// A growing partial whose end crosses UTC midnight lands wholly on the end
/// day (the ledger keys by end).
#[test]
fn a_growing_partial_crossing_midnight_moves_to_the_end_day() {
    let home = HomeSandbox::new();
    reg(None);
    let path = iso_kerry(home.home()).join("midgrow.jsonl");
    write_lines(
        &path,
        &[
            user("2026-09-22T23:59:50.000Z", "midgrow"),
            assistant("2026-09-22T23:59:58.000Z", "midgrow", "msg_mg", 40),
        ],
    );
    let mut p = Producer::new();
    p.tick(ms("2026-09-22T23:59:59.000Z"));
    append_lines(
        &path,
        &[assistant(
            "2026-09-23T00:00:05.000Z",
            "midgrow",
            "msg_mg",
            1222,
        )],
    );
    let snap = p.tick(ms("2026-09-23T00:00:06.000Z"));
    assert_eq!(
        sums_only(&snap.days["2026-09-23"]["kerry"][MODEL]),
        DaySum {
            output: 1222,
            gen_ms: 15_000,
            ..Default::default()
        }
    );
}

// ── round-9 ──────────────────────────────────────────────────────────────────

/// A permission blip on the speed ledger recovers via ctime: chmod 000 at
/// start (unreadable, so the owed day waits for its rebuild), chmod 600 one
/// tick later, and the day is rebuilt and folds.
#[cfg(unix)]
#[test]
fn a_permission_blip_on_the_speed_ledger_recovers_via_ctime() {
    use std::os::unix::fs::PermissionsExt;
    // The named guard: root ignores permission bits, so the chmod-000 leg
    // cannot fail and this test proves nothing.
    if unsafe { libc::geteuid() } == 0 {
        eprintln!("skipped: root ignores permission bits");
        return;
    }
    let home = HomeSandbox::new();
    let clauth = home.home().join(".clauth");
    std::fs::create_dir_all(&clauth).unwrap();
    std::fs::write(
        clauth.join("token_speed.json"),
        r#"{"version":1,"days":{"2026-09-10":{"kerry":{"claude-opus-5-5":{"output":1,"gen_ms":2}}}}}"#,
    )
    .unwrap();
    day_transcript(home.home(), "2026-09-20", 7, 3);
    write_owed(home.home(), &["2026-09-20"]);

    std::fs::set_permissions(
        clauth.join("token_speed.json"),
        std::fs::Permissions::from_mode(0o000),
    )
    .unwrap();
    let mut p = Producer::new();
    let held = p.tick(ms("2026-09-22T00:01:00.000Z"));
    assert!(
        !held.days.contains_key("2026-09-20"),
        "rebuilt while unreadable"
    );

    std::fs::set_permissions(
        clauth.join("token_speed.json"),
        std::fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    let landed = p.tick(ms("2026-09-22T00:02:00.000Z"));
    assert!(
        !landed.days.contains_key("2026-09-20"),
        "folded once readable"
    );
    let speed = crate::token_ledger::SpeedLedger::load(&clauth);
    assert_eq!(
        speed.days["2026-09-20"]["kerry"][MODEL],
        crate::tps::DaySum {
            output: 7,
            gen_ms: 3,
            ..Default::default()
        }
    );
}

/// A content rewrite of the speed ledger (mtime moves) rebuilds and folds the
/// owed day that waited while the ledger was unreadable.
#[cfg(unix)]
#[test]
fn a_speed_ledger_content_rewrite_folds_the_held_day() {
    use std::os::unix::fs::PermissionsExt;
    if unsafe { libc::geteuid() } == 0 {
        eprintln!("skipped: root ignores permission bits");
        return;
    }
    let home = HomeSandbox::new();
    let clauth = home.home().join(".clauth");
    std::fs::create_dir_all(&clauth).unwrap();
    std::fs::write(
        clauth.join("token_speed.json"),
        r#"{"version":1,"days":{"2026-09-10":{"kerry":{"claude-opus-5-5":{"output":1,"gen_ms":2}}}}}"#,
    )
    .unwrap();
    day_transcript(home.home(), "2026-09-20", 7, 3);
    write_owed(home.home(), &["2026-09-20"]);

    std::fs::set_permissions(
        clauth.join("token_speed.json"),
        std::fs::Permissions::from_mode(0o000),
    )
    .unwrap();
    let mut p = Producer::new();
    let held = p.tick(ms("2026-09-22T00:01:00.000Z"));
    assert!(
        !held.days.contains_key("2026-09-20"),
        "rebuilt while unreadable"
    );

    std::fs::set_permissions(
        clauth.join("token_speed.json"),
        std::fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    std::fs::write(
        clauth.join("token_speed.json"),
        r#"{"version":1,"days":{"2026-09-10":{"kerry":{"claude-opus-5-5":{"output":1,"gen_ms":2}}}}}"#,
    )
    .unwrap();
    let landed = p.tick(ms("2026-09-22T00:02:00.000Z"));
    assert!(
        !landed.days.contains_key("2026-09-20"),
        "folded after the rewrite"
    );
    let speed = crate::token_ledger::SpeedLedger::load(&clauth);
    assert_eq!(
        speed.days["2026-09-20"]["kerry"][MODEL],
        crate::tps::DaySum {
            output: 7,
            gen_ms: 3,
            ..Default::default()
        }
    );
}

// ── peaks ────────────────────────────────────────────────────────────────────

const OTHER_MODEL: &str = "deepseek-v4-pro";

fn assistant_model(ts: &str, session: &str, id: &str, model: &str, output: u64) -> String {
    let mut v: serde_json::Value =
        serde_json::from_str(&assistant(ts, session, id, output)).unwrap();
    v["message"]["model"] = serde_json::json!(model);
    v.to_string()
}

/// The cache's `peaks` object, by its wire shape.
fn peaks(snap: &Snapshot) -> serde_json::Value {
    serde_json::to_value(snap).unwrap()["peaks"].clone()
}

/// The feed's `at` spelling (`+00:00` RFC 3339 with milliseconds).
fn at(ms: u64) -> String {
    chrono::DateTime::from_timestamp_millis(i64::try_from(ms).unwrap())
        .unwrap()
        .format("%Y-%m-%dT%H:%M:%S%.3f+00:00")
        .to_string()
}

fn speed_peak(value: f64, at_ms: u64, account: &str, model: &str) -> serde_json::Value {
    serde_json::json!({"value": value, "at": at(at_ms), "account": account, "model": model})
}

/// Peak speed is the fastest response's own speed in its final shape: a
/// 2000-token response over 10 s (200 t/s) beats a tool-using turn whose first
/// block line carried its whole 3000 tokens 5 s in (600 t/s mid-stream, 100
/// t/s once its tool_use line landed 30 s in), and a 50-token reply over
/// 100 ms (500 t/s) sits under the 100-token floor. A running max of what each
/// tick saw would keep the 600.
#[test]
fn the_peak_speed_is_the_fastest_final_response_over_the_floor() {
    let home = HomeSandbox::new();
    reg(None);
    let s = ms(START);
    let store = iso_kerry(home.home());
    write_lines(
        &store.join("a.jsonl"),
        &[
            user(START, "a"),
            assistant(&iso(s + 10_000), "a", "msg_a", 2000),
        ],
    );
    write_lines(
        &store.join("b.jsonl"),
        &[
            user(&iso(s + 20_000), "b"),
            assistant(&iso(s + 20_100), "b", "msg_b", 50),
        ],
    );
    let c = store.join("c.jsonl");
    write_lines(
        &c,
        &[
            user(&iso(s + 100_000), "c"),
            assistant(&iso(s + 105_000), "c", "msg_c", 3000),
        ],
    );
    let mut p = Producer::new();
    p.tick(s + 106_000);
    append_lines(&c, &[assistant(&iso(s + 130_000), "c", "msg_c", 3000)]);
    let pk = peaks(&p.tick(s + 131_000));
    let want = speed_peak(200.0, s + 10_000, "kerry", MODEL);
    assert_eq!(pk["today"]["speed"], want, "total: {pk}");
    assert_eq!(
        pk["today"]["accounts"]["kerry"]["speed"], want,
        "kerry: {pk}"
    );
    assert_eq!(pk["today"]["models"][MODEL]["speed"], want, "model: {pk}");
    assert_eq!(pk["ever"]["speed"], want, "ever: {pk}");
}

/// Peak rate is the busiest 10 s window over the final spans: `a` (2000 over
/// 0-10 s) and `d` (1000 over 2-7 s) both fit the window ending at 10 s, 3000
/// / 10 s = 300 tok/s, and no other window holds more. `c` (6000 tokens) read
/// mid-stream at 5 s would fill one window (600 tok/s); its final 30 s span
/// spreads it to at most 2000 in a window (200 tok/s).
#[test]
fn the_peak_rate_is_the_busiest_ten_second_window_of_final_spans() {
    let home = HomeSandbox::new();
    reg(None);
    let s = ms(START);
    let store = iso_kerry(home.home());
    write_lines(
        &store.join("a.jsonl"),
        &[
            user(START, "a"),
            assistant(&iso(s + 10_000), "a", "msg_a", 2000),
        ],
    );
    write_lines(
        &store.join("d.jsonl"),
        &[
            user(&iso(s + 2_000), "d"),
            assistant(&iso(s + 7_000), "d", "msg_d", 1000),
        ],
    );
    let c = store.join("c.jsonl");
    write_lines(
        &c,
        &[
            user(&iso(s + 100_000), "c"),
            assistant(&iso(s + 105_000), "c", "msg_c", 6000),
        ],
    );
    let mut p = Producer::new();
    p.tick(s + 106_000);
    append_lines(&c, &[assistant(&iso(s + 130_000), "c", "msg_c", 6000)]);
    let pk = peaks(&p.tick(s + 131_000));
    for (what, rate) in [
        ("total", &pk["today"]["rate"]),
        ("kerry", &pk["today"]["accounts"]["kerry"]["rate"]),
        ("model", &pk["today"]["models"][MODEL]["rate"]),
        ("ever", &pk["ever"]["rate"]),
    ] {
        close(
            rate["value"].as_f64().unwrap_or(f64::NAN),
            300.0,
            &format!("{what}: {pk}"),
        );
        assert_eq!(rate["at"], at(s + 10_000), "{what}: {pk}");
    }
}

/// Rates do not aggregate by max: `kerry` (2000 over 0-10 s on the default
/// model) and a shared session the hook names `mommy` (1000 over 0-10 s on
/// another model) run at once, so the total peaks at 300 tok/s while each
/// account and each model keeps its own 200 / 100. Speed peaks follow each
/// grouping's fastest response (200 t/s kerry, 100 t/s mommy).
#[test]
fn each_grouping_keeps_its_own_peaks() {
    let home = HomeSandbox::new();
    reg(None);
    let s = ms(START);
    write_lines(
        &iso_kerry(home.home()).join("a.jsonl"),
        &[
            user(START, "a"),
            assistant(&iso(s + 10_000), "a", "msg_a", 2000),
        ],
    );
    let sid = "5d7e2a10-1b3c-4d5e-8f90-a1b2c3d4e5f6";
    let record = crate::hook_note::record_path(sid, None).unwrap();
    std::fs::create_dir_all(record.parent().unwrap()).unwrap();
    std::fs::write(&record, r#"{"resolved":"mommy"}"#).unwrap();
    write_lines(
        &global_store(home.home()).join(format!("{sid}.jsonl")),
        &[
            user(START, sid),
            assistant_model(&iso(s + 10_000), sid, "msg_e", OTHER_MODEL, 1000),
        ],
    );
    let pk = peaks(&Producer::new().tick(s + 11_000));
    let rate = |v: &serde_json::Value| v["rate"]["value"].as_f64().unwrap_or(f64::NAN);
    close(rate(&pk["today"]), 300.0, &format!("total: {pk}"));
    close(
        rate(&pk["today"]["accounts"]["kerry"]),
        200.0,
        &format!("kerry: {pk}"),
    );
    close(
        rate(&pk["today"]["accounts"]["mommy"]),
        100.0,
        &format!("mommy: {pk}"),
    );
    close(
        rate(&pk["today"]["models"][MODEL]),
        200.0,
        &format!("model: {pk}"),
    );
    close(
        rate(&pk["today"]["models"][OTHER_MODEL]),
        100.0,
        &format!("other: {pk}"),
    );
    assert_eq!(
        pk["today"]["speed"],
        speed_peak(200.0, s + 10_000, "kerry", MODEL)
    );
    assert_eq!(
        pk["today"]["accounts"]["mommy"]["speed"],
        speed_peak(100.0, s + 10_000, "mommy", OTHER_MODEL)
    );
    assert_eq!(
        pk["today"]["models"][OTHER_MODEL]["speed"],
        speed_peak(100.0, s + 10_000, "mommy", OTHER_MODEL)
    );
}

/// A day that rolls out of {today, yesterday} folds its peaks into the speed
/// ledger (now `version` 2): `today` reads none, `ever` keeps 09-22's, and a
/// fresh producer reads them back from the ledger alone.
#[test]
fn a_rolled_over_days_peaks_stay_in_the_ledger_and_in_ever() {
    let home = HomeSandbox::new();
    reg(None);
    let s = ms(START);
    write_lines(
        &iso_kerry(home.home()).join("a.jsonl"),
        &[
            user(START, "a"),
            assistant(&iso(s + 10_000), "a", "msg_a", 2000),
        ],
    );
    let mut p = Producer::new();
    p.tick(s + 11_000);
    let later = ms("2026-09-24T00:00:10.000Z");
    let pk = peaks(&p.tick(later));
    assert!(pk["today"].is_object(), "today is present: {pk}");
    assert!(pk["today"].get("speed").is_none(), "no speed today: {pk}");
    assert!(pk["today"].get("rate").is_none(), "no rate today: {pk}");
    let want = speed_peak(200.0, s + 10_000, "kerry", MODEL);
    assert_eq!(pk["ever"]["speed"], want, "ever: {pk}");
    close(
        pk["ever"]["rate"]["value"].as_f64().unwrap_or(f64::NAN),
        200.0,
        &format!("ever rate: {pk}"),
    );
    let ledger: serde_json::Value = serde_json::from_slice(
        &std::fs::read(home.home().join(".clauth/token_speed.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(ledger["version"], 2, "ledger: {ledger}");
    let fresh = peaks(&Producer::new().tick(later + 1_000));
    assert_eq!(fresh["ever"]["speed"], want, "from the ledger: {fresh}");
}

/// A peak no response set is omitted, never `0`: a 50-token reply sets no
/// speed peak (under the floor) but does set a rate peak (50 over its 100 ms
/// span all fit one window: 5 tok/s); a tree with no response sets neither.
#[test]
fn an_unset_peak_is_absent_never_zero() {
    let home = HomeSandbox::new();
    reg(None);
    let s = ms(START);
    let path = iso_kerry(home.home()).join("b.jsonl");
    write_lines(&path, &[user(START, "b")]);
    let none = peaks(&Producer::new().tick(s + 1_000));
    for half in ["today", "ever"] {
        assert!(none[half].is_object(), "{half} is present: {none}");
        assert!(none[half].get("speed").is_none(), "{half}: {none}");
        assert!(none[half].get("rate").is_none(), "{half}: {none}");
    }
    append_lines(&path, &[assistant(&iso(s + 100), "b", "msg_b", 50)]);
    let small = peaks(&Producer::new().tick(s + 1_000));
    assert!(
        small["today"].get("speed").is_none(),
        "under the floor: {small}"
    );
    close(
        small["today"]["rate"]["value"].as_f64().unwrap_or(f64::NAN),
        5.0,
        &format!("rate: {small}"),
    );
}

/// A restart recomputes the same peaks from the re-read lines. `a` (2000 over
/// 10 s) and `d` (1000 over 5 s) tie at 200 t/s; the tie names the earlier
/// response, `d`, ending 7 s in.
#[test]
fn a_restart_recomputes_the_same_peaks() {
    let home = HomeSandbox::new();
    reg(None);
    let s = ms(START);
    let store = iso_kerry(home.home());
    write_lines(
        &store.join("a.jsonl"),
        &[
            user(START, "a"),
            assistant(&iso(s + 10_000), "a", "msg_a", 2000),
        ],
    );
    write_lines(
        &store.join("d.jsonl"),
        &[
            user(&iso(s + 2_000), "d"),
            assistant(&iso(s + 7_000), "d", "msg_d", 1000),
        ],
    );
    let before = Producer::new().tick(s + 11_000);
    assert_eq!(
        peaks(&before)["today"]["speed"],
        speed_peak(200.0, s + 7_000, "kerry", MODEL)
    );
    write_cache(home.home(), &before);
    let after = Producer::new().tick(s + 12_000);
    assert_eq!(peaks(&after), peaks(&before));
}

/// An older day whose responses are gone keeps the peaks its cache held: a
/// restart two days later re-reads no lines (the files are cold), so the
/// recompute of today/yesterday runs empty and must not wipe the older day's
/// stored peaks — the fold carries them into the ledger and `ever` reads them
/// back.
#[test]
fn an_older_days_peaks_survive_the_recompute_with_its_responses_gone() {
    let home = HomeSandbox::new();
    reg(None);
    let s = ms(START);
    write_lines(
        &iso_kerry(home.home()).join("a.jsonl"),
        &[
            user(START, "a"),
            assistant(&iso(s + 10_000), "a", "msg_a", 2000),
        ],
    );
    let before = Producer::new().tick(s + 11_000);
    write_cache(home.home(), &before);

    // Two days later the 09-22 files are cold (mtime before yesterday's
    // midnight), so the response is gone; the cache still holds its peaks.
    let later = ms("2026-09-24T00:00:10.000Z");
    let pk = peaks(&Producer::new().tick(later));
    assert!(
        pk["today"].get("speed").is_none(),
        "no responses today: {pk}"
    );
    let want = speed_peak(200.0, s + 10_000, "kerry", MODEL);
    assert_eq!(
        pk["ever"]["speed"], want,
        "the older day's peak survives the recompute: {pk}"
    );
    close(
        pk["ever"]["rate"]["value"].as_f64().unwrap_or(f64::NAN),
        200.0,
        &format!("ever rate survives: {pk}"),
    );
}

/// A seeded 64-bit LCG, so the oracle sweep comparison is deterministic.
fn oracle_lcg(state: &mut u64) -> u64 {
    *state = state
        .wrapping_mul(6_364_136_223_846_793_005)
        .wrapping_add(1_442_695_040_888_963_407);
    *state
}

/// The sweep reproduces `window_figure`'s rate at every instant: over a small
/// seeded pseudo-random mix (spans, span-less, overlapping, shared edges,
/// midnight-straddling) plus point-on-edge cases, the sweep's peak equals the
/// brute force's — `window_figure` evaluated at every millisecond of the
/// range, never at the sweep's own candidates — in value and `at`.
#[test]
fn the_rate_peak_sweep_matches_the_brute_force_window_peak() {
    let midnight = 1_792_281_600_000u64; // 2026-10-18T00:00:00Z
    let mut state = 0x1234_5678_9abc_def0u64;
    let mut rs: Vec<Resp> = Vec::new();
    for i in 0..150 {
        let account = ["a", "b", "c"][(oracle_lcg(&mut state) % 3) as usize];
        let model = ["m0", "m1", "m2"][(oracle_lcg(&mut state) % 3) as usize];
        let base = midnight - 30_000 + oracle_lcg(&mut state) % 60_000;
        let span = 1 + oracle_lcg(&mut state) % 20_000;
        let end_ms = base + span;
        let has_span = !oracle_lcg(&mut state).is_multiple_of(4);
        let start_ms = if has_span { Some(base) } else { None };
        let output = 1 + oracle_lcg(&mut state) % 5_000;
        rs.push(Resp {
            session: format!("s{i}"),
            account: account.to_owned(),
            model: model.to_owned(),
            start_ms,
            end_ms,
            output,
            file: oracle_file(),
        });
    }
    // Explicit edge cases the random mix might miss: a point on the day's
    // first instant, one 10 s before it, a span straddling midnight, and a
    // span ending 2 s before midnight.
    for (start, end, output) in [
        (None, midnight, 300u64),
        (None, midnight - 10_000, 250),
        (Some(midnight - 5_000), midnight + 5_000, 800),
        (Some(midnight - 12_000), midnight - 2_000, 1_000),
    ] {
        rs.push(Resp {
            session: format!("edge{start:?}{end}"),
            account: "e".to_owned(),
            model: "m0".to_owned(),
            start_ms: start,
            end_ms: end,
            output,
            file: oracle_file(),
        });
    }
    let refs: Vec<&Resp> = rs.iter().collect();
    // A rate tie names any window at the max, and the
    // float `window_figure` sums differ by ulps on a plateau: the sweep's
    // value matches the brute max within 1e-9 × max(|max|, 1), and the window
    // it names reads that max.
    let max = (midnight..=midnight + 70_000)
        .map(|t| window_figure(&refs, t, LIVE_WINDOW_MS).rate)
        .fold(0.0, f64::max);
    let sweep = rate_peak_sweep(&refs, midnight, midnight + DAY_MS - 1);
    match sweep {
        Some(s) if max > 0.0 => {
            let tol = 1e-9 * max.abs().max(1.0);
            assert!(
                (s.value - max).abs() <= tol,
                "value: sweep {} brute {max}",
                s.value
            );
            let there = window_figure(&refs, s.at, LIVE_WINDOW_MS).rate;
            assert!(
                (there - max).abs() <= tol,
                "at: sweep names {} reading {there}, brute max {max}",
                s.at
            );
        }
        None if max == 0.0 => {}
        s => panic!("presence mismatch: sweep {s:?} brute max {max}"),
    }
}

/// The live window is closed at both edges (`window_figure` counts a span-less
/// response's tokens whole at its end when that instant sits in
/// `[t - 10 s, t]`): two span-less 1000-token responses exactly 10 s apart
/// share the window ending at the second, 2000 / 10 s = 200 tok/s, while any
/// later window holds the second alone (100 tok/s).
#[test]
fn a_rate_window_holds_points_on_both_its_edges() {
    let home = HomeSandbox::new();
    reg(None);
    let s = ms(START);
    write_lines(
        &iso_kerry(home.home()).join("points.jsonl"),
        &[
            assistant(&iso(s), "points", "msg_p1", 1000),
            assistant(&iso(s + 10_000), "points", "msg_p2", 1000),
        ],
    );
    let pk = peaks(&Producer::new().tick(s + 11_000));
    close(
        pk["today"]["rate"]["value"].as_f64().unwrap_or(f64::NAN),
        200.0,
        &format!("both edge points in one window: {pk}"),
    );
    assert_eq!(pk["today"]["rate"]["at"], at(s + 10_000), "{pk}");
    assert!(
        pk["today"].get("speed").is_none(),
        "span-less, no speed: {pk}"
    );
}

/// An unstamped swap, then a restart more than 10 min after the response
/// ended: the restart re-reads the response under `kerry` (the swap takes the
/// restart's first tick), so its peaks stay on `kerry` and `mommy` gets no row
/// anywhere.
#[test]
fn a_restart_after_a_swap_keeps_settled_peaks_on_their_member() {
    let home = HomeSandbox::new();
    reg(None);
    let s = ms(START);
    write_lines(
        &iso_kerry(home.home()).join("a.jsonl"),
        &[
            user(START, "a"),
            assistant(&iso(s + 10_000), "a", "msg_a", 2000),
        ],
    );
    let write_at = s + 10_000 + 11 * 60_000;
    let before = Producer::new().tick(write_at);
    write_cache(home.home(), &before);
    reg(Some("mommy"));
    let after = Producer::new().tick(write_at + 1_000);
    let pk = peaks(&after);
    assert_eq!(
        pk["today"]["speed"],
        speed_peak(200.0, s + 10_000, "kerry", MODEL),
        "{pk}"
    );
    assert!(pk["today"]["accounts"].get("mommy").is_none(), "{pk}");
    assert_eq!(
        pk["today"]["accounts"]["kerry"],
        peaks(&before)["today"]["accounts"]["kerry"],
        "{pk}"
    );
    assert!(
        !after.days["2026-09-22"].contains_key("mommy"),
        "no phantom cell: {:?}",
        after.days["2026-09-22"]
    );
}

/// A restart while a tool-using turn streams (its first block line carries
/// the whole 3000 tokens 5 s in: 600 t/s) never keeps that mid-stream figure:
/// once the tool_use line lands 30 s in, the peak reads 100 t/s.
#[test]
fn a_restart_mid_stream_never_keeps_the_mid_stream_peak() {
    let home = HomeSandbox::new();
    reg(None);
    let s = ms(START);
    let path = iso_kerry(home.home()).join("c.jsonl");
    write_lines(
        &path,
        &[
            user(START, "c"),
            assistant(&iso(s + 5_000), "c", "msg_c", 3000),
        ],
    );
    let before = Producer::new().tick(s + 6_000);
    write_cache(home.home(), &before);
    append_lines(&path, &[assistant(&iso(s + 30_000), "c", "msg_c", 3000)]);
    let pk = peaks(&Producer::new().tick(s + 31_000));
    assert_eq!(
        pk["today"]["speed"],
        speed_peak(100.0, s + 30_000, "kerry", MODEL),
        "{pk}"
    );
    close(
        pk["today"]["rate"]["value"].as_f64().unwrap_or(f64::NAN),
        100.0,
        &format!("rate over the final 30 s span: {pk}"),
    );
}

/// A turn spanning 23:59:48-23:59:58 (1000 tokens, 100 tok/s) read 9 s past
/// midnight: yesterday peaks at 100 tok/s at 23:59:58; today's busiest window
/// is the one ending at 00:00:00, holding the span's last 8 s (800 tokens, 80
/// tok/s), never a 0.
#[test]
fn the_rate_peak_reads_the_windows_on_the_day_edges() {
    let home = HomeSandbox::new();
    reg(None);
    write_lines(
        &iso_kerry(home.home()).join("late.jsonl"),
        &[
            user("2026-09-22T23:59:48.000Z", "late"),
            assistant("2026-09-22T23:59:58.000Z", "late", "msg_late", 1000),
        ],
    );
    let pk = peaks(&Producer::new().tick(ms("2026-09-23T00:00:09.000Z")));
    close(
        pk["today"]["rate"]["value"].as_f64().unwrap_or(f64::NAN),
        80.0,
        &format!("today: {pk}"),
    );
    assert_eq!(
        pk["today"]["rate"]["at"],
        at(ms("2026-09-23T00:00:00.000Z")),
        "{pk}"
    );
    close(
        pk["ever"]["rate"]["value"].as_f64().unwrap_or(f64::NAN),
        100.0,
        &format!("ever: {pk}"),
    );
    assert_eq!(
        pk["ever"]["rate"]["at"],
        at(ms("2026-09-22T23:59:58.000Z")),
        "{pk}"
    );
}

/// Yesterday's first windows still hold the day before's tail after the
/// rollover evicts that day: `a` (1000 over 09-21 23:59:48-58) and `b` (600
/// over 09-22 00:00:00-02) share the window ending 00:00:02, 600 + 600 = 1200
/// / 10 s = 120 tok/s, which is 09-22's peak and the all-time one (09-21 peaks
/// at 100; `b` alone would read 60).
#[test]
fn yesterdays_peak_keeps_the_day_befores_tail() {
    let home = HomeSandbox::new();
    reg(None);
    let store = iso_kerry(home.home());
    write_lines(
        &store.join("a.jsonl"),
        &[
            user("2026-09-21T23:59:48.000Z", "a"),
            assistant("2026-09-21T23:59:58.000Z", "a", "msg_a", 1000),
        ],
    );
    write_lines(
        &store.join("b.jsonl"),
        &[
            user("2026-09-22T00:00:00.000Z", "b"),
            assistant("2026-09-22T00:00:02.000Z", "b", "msg_b", 600),
        ],
    );
    let mut p = Producer::new();
    p.tick(ms("2026-09-22T00:00:03.000Z"));
    let pk = peaks(&p.tick(ms("2026-09-23T00:00:05.000Z")));
    close(
        pk["ever"]["rate"]["value"].as_f64().unwrap_or(f64::NAN),
        120.0,
        &format!("ever: {pk}"),
    );
    assert_eq!(
        pk["ever"]["rate"]["at"],
        at(ms("2026-09-22T00:00:02.000Z")),
        "{pk}"
    );
}

/// A span-less response's tokens fill every window ending in its next 10 s;
/// the peak names one of those equal windows (a rate tie names any window at
/// the max).
#[test]
fn an_equal_rate_peak_names_a_window_holding_the_point() {
    let home = HomeSandbox::new();
    reg(None);
    let s = ms(START);
    write_lines(
        &iso_kerry(home.home()).join("point.jsonl"),
        &[assistant(START, "point", "msg_point", 1000)],
    );
    let pk = peaks(&Producer::new().tick(s + 1_000));
    close(
        pk["today"]["rate"]["value"].as_f64().unwrap_or(f64::NAN),
        100.0,
        &format!("{pk}"),
    );
    let got = pk["today"]["rate"]["at"].as_str().map(ms);
    assert!(
        got.is_some_and(|t| (s..=s + LIVE_WINDOW_MS).contains(&t)),
        "{pk}"
    );
}

/// The floor is inclusive: a 100-token response sets a speed peak.
#[test]
fn a_response_of_exactly_the_floor_sets_a_speed_peak() {
    let home = HomeSandbox::new();
    reg(None);
    let s = ms(START);
    write_lines(
        &iso_kerry(home.home()).join("floor.jsonl"),
        &[
            user(START, "floor"),
            assistant(&iso(s + 1_000), "floor", "msg_floor", 100),
        ],
    );
    let pk = peaks(&Producer::new().tick(s + 2_000));
    assert_eq!(
        pk["today"]["speed"],
        speed_peak(100.0, s + 1_000, "kerry", MODEL),
        "{pk}"
    );
}

/// An unattributed session's peak counts in the total, names no account, and
/// never appears under the reserved `*`.
#[test]
fn an_unattributed_peak_names_no_account() {
    let home = HomeSandbox::new();
    let s = ms(START);
    write_lines(
        &global_store(home.home()).join("anon.jsonl"),
        &[
            user(START, "anon"),
            assistant(&iso(s + 10_000), "anon", "msg_anon", 2000),
        ],
    );
    let pk = peaks(&Producer::new().tick(s + 11_000));
    assert_eq!(
        pk["today"]["speed"],
        serde_json::json!({"value": 200.0, "at": at(s + 10_000), "model": MODEL}),
        "{pk}"
    );
    assert!(pk["today"]["accounts"].get(UNATTRIBUTED).is_none(), "{pk}");
}

/// A v1 ledger's peakless cells add no empty groups to `ever`.
#[test]
fn ever_lists_only_groups_that_hold_a_peak() {
    let home = HomeSandbox::new();
    let dir = home.home().join(".clauth");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("token_speed.json"),
        format!(
            r#"{{"version":1,"days":{{"2026-09-20":{{"kerry":{{"{MODEL}":{{"output":100,"gen_ms":1000}}}}}}}}}}"#
        ),
    )
    .unwrap();
    let pk = peaks(&Producer::new().tick(ms(START)));
    assert!(pk["ever"].is_object(), "{pk}");
    assert!(pk["ever"]["accounts"].get("kerry").is_none(), "{pk}");
    assert!(pk["ever"]["models"].get(MODEL).is_none(), "{pk}");
}

/// A day no producer ran folds no rate-peak row into the ledger (yesterday at
/// the first launch here), while a day with a response does.
#[test]
fn a_day_without_responses_folds_no_rate_peak_row() {
    let home = HomeSandbox::new();
    reg(None);
    let s = ms(START);
    write_lines(
        &iso_kerry(home.home()).join("a.jsonl"),
        &[
            user(START, "a"),
            assistant(&iso(s + 10_000), "a", "msg_a", 2000),
        ],
    );
    let mut p = Producer::new();
    p.tick(s + 11_000);
    p.tick(ms("2026-09-24T00:00:10.000Z"));
    let ledger: serde_json::Value = serde_json::from_slice(
        &std::fs::read(home.home().join(".clauth/token_speed.json")).unwrap(),
    )
    .unwrap();
    assert!(ledger["rate_peaks"].get("2026-09-21").is_none(), "{ledger}");
    assert!(ledger["rate_peaks"]["2026-09-22"].is_object(), "{ledger}");
}

// ── peaks across restarts: every schedule keeps what a producer showed ─────

/// A producer that started from a cache keeps showing a peak it read live,
/// 11 min on and after another (slower) response re-ran the recompute.
#[test]
fn a_restarted_producer_keeps_its_own_peaks_past_ten_minutes() {
    let home = HomeSandbox::new();
    reg(None);
    let s = ms(START);
    let store = iso_kerry(home.home());
    write_cache(home.home(), &Producer::new().tick(s - 60_000));
    let mut p = Producer::new();
    write_lines(
        &store.join("a.jsonl"),
        &[
            user(START, "a"),
            assistant(&iso(s + 10_000), "a", "msg_a", 2000),
        ],
    );
    p.tick(s + 11_000);
    write_lines(
        &store.join("b.jsonl"),
        &[
            user(&iso(s + 11 * 60_000), "b"),
            assistant(&iso(s + 11 * 60_000 + 10_000), "b", "msg_b", 100),
        ],
    );
    let pk = peaks(&p.tick(s + 11 * 60_000 + 11_000));
    assert_eq!(
        pk["today"]["speed"],
        speed_peak(200.0, s + 10_000, "kerry", MODEL),
        "{pk}"
    );
    close(
        pk["today"]["rate"]["value"].as_f64().unwrap_or(f64::NAN),
        200.0,
        &format!("{pk}"),
    );
    assert_eq!(pk["today"]["rate"]["at"], at(s + 10_000), "{pk}");
}

/// A producer reads a response, idles 11 min (no new line), then restarts:
/// the restart re-derives the response's peaks from the transcript, so they
/// still show.
#[test]
fn an_idle_stretch_before_a_restart_keeps_the_peaks() {
    let home = HomeSandbox::new();
    reg(None);
    let s = ms(START);
    write_lines(
        &iso_kerry(home.home()).join("a.jsonl"),
        &[
            user(START, "a"),
            assistant(&iso(s + 10_000), "a", "msg_a", 2000),
        ],
    );
    let mut p = Producer::new();
    p.tick(s + 11_000);
    write_cache(home.home(), &p.tick(s + 10_000 + 11 * 60_000));
    let pk = peaks(&Producer::new().tick(s + 10_000 + 11 * 60_000 + 1_000));
    assert_eq!(
        pk["today"]["speed"],
        speed_peak(200.0, s + 10_000, "kerry", MODEL),
        "{pk}"
    );
    close(
        pk["today"]["rate"]["value"].as_f64().unwrap_or(f64::NAN),
        200.0,
        &format!("{pk}"),
    );
}

/// Two restarts: the middle producer (itself restarted) read the response;
/// the third, more than 10 min later, still shows its peak.
#[test]
fn a_second_restart_keeps_the_middle_producers_peaks() {
    let home = HomeSandbox::new();
    reg(None);
    let s = ms(START);
    let store = iso_kerry(home.home());
    write_cache(home.home(), &Producer::new().tick(s - 60_000));
    let mut p2 = Producer::new();
    write_lines(
        &store.join("a.jsonl"),
        &[
            user(START, "a"),
            assistant(&iso(s + 10_000), "a", "msg_a", 2000),
        ],
    );
    p2.tick(s + 11_000);
    write_lines(
        &store.join("b.jsonl"),
        &[
            user(&iso(s + 11 * 60_000), "b"),
            assistant(&iso(s + 11 * 60_000 + 10_000), "b", "msg_b", 100),
        ],
    );
    write_cache(home.home(), &p2.tick(s + 11 * 60_000 + 11_000));
    let pk = peaks(&Producer::new().tick(s + 11 * 60_000 + 12_000));
    assert_eq!(
        pk["today"]["speed"],
        speed_peak(200.0, s + 10_000, "kerry", MODEL),
        "{pk}"
    );
}

/// A restart on the far side of midnight: the last write was 23:55, the
/// day's busiest window ended 23:47:00 (2000 tokens over 23:46:50-23:47:00,
/// 200 tok/s), the restart runs 00:05; yesterday keeps its 200.
#[test]
fn a_restart_past_midnight_keeps_yesterdays_late_peak() {
    let home = HomeSandbox::new();
    reg(None);
    write_lines(
        &iso_kerry(home.home()).join("a.jsonl"),
        &[
            user("2026-09-22T23:46:50.000Z", "a"),
            assistant("2026-09-22T23:47:00.000Z", "a", "msg_a", 2000),
        ],
    );
    write_cache(
        home.home(),
        &Producer::new().tick(ms("2026-09-22T23:55:00.000Z")),
    );
    let pk = peaks(&Producer::new().tick(ms("2026-09-23T00:05:00.000Z")));
    close(
        pk["ever"]["rate"]["value"].as_f64().unwrap_or(f64::NAN),
        200.0,
        &format!("{pk}"),
    );
    assert_eq!(
        pk["ever"]["rate"]["at"],
        at(ms("2026-09-22T23:47:00.000Z")),
        "{pk}"
    );
}

/// Span-less `p1` and `p2`, 9 s apart in two files, share the 10 s window
/// ending at p2 (2000 / 10 s = 200 tok/s): a restart 10 min later re-derives
/// that window from both transcripts.
#[test]
fn a_restart_rederives_a_rate_window_spanning_two_files() {
    let home = HomeSandbox::new();
    reg(None);
    let w = ms(START) + 20 * 60_000;
    let edge = w - 10 * 60_000;
    let store = iso_kerry(home.home());
    write_lines(
        &store.join("p1.jsonl"),
        &[assistant(&iso(edge - 1_000), "p1", "msg_p1", 1000)],
    );
    write_lines(
        &store.join("p2.jsonl"),
        &[assistant(&iso(edge + 8_000), "p2", "msg_p2", 1000)],
    );
    write_cache(home.home(), &Producer::new().tick(w));
    let pk = peaks(&Producer::new().tick(w + 1_000));
    close(
        pk["today"]["rate"]["value"].as_f64().unwrap_or(f64::NAN),
        200.0,
        &format!("{pk}"),
    );
    assert_eq!(pk["today"]["rate"]["at"], at(edge + 8_000), "{pk}");
}

/// A running producer never settles a mid-stream shape: a tool-using turn's
/// first block line carries its whole 3000 tokens 5 s in (600 t/s), and its
/// tool_use line lands 11 min later, after that first shape aged past the
/// settle boundary; the final shape (3000 over 665 s) is the day's peak.
#[test]
fn a_running_producer_never_settles_a_mid_stream_shape() {
    let home = HomeSandbox::new();
    reg(None);
    let s = ms(START);
    let path = iso_kerry(home.home()).join("slow.jsonl");
    write_lines(
        &path,
        &[
            user(START, "slow"),
            assistant(&iso(s + 5_000), "slow", "msg_slow", 3000),
        ],
    );
    let mut p = Producer::new();
    p.tick(s + 6_000);
    p.tick(s + 5_000 + 10 * 60_000 + 11_000);
    let end = s + 5_000 + 11 * 60_000;
    append_lines(&path, &[assistant(&iso(end), "slow", "msg_slow", 3000)]);
    let pk = peaks(&p.tick(end + 1_000));
    let want = 3000.0 / 665.0;
    close(
        pk["today"]["speed"]["value"].as_f64().unwrap_or(f64::NAN),
        want,
        &format!("speed: {pk}"),
    );
    assert_eq!(pk["today"]["speed"]["at"], at(end), "{pk}");
    close(
        pk["today"]["rate"]["value"].as_f64().unwrap_or(f64::NAN),
        want,
        &format!("rate: {pk}"),
    );
}

/// A response first read after its windows settled (a transcript that shows
/// up late, holding lines 15 min old) still sets the day's peaks: 2000 over
/// 10 s, 200 t/s and 200 tok/s.
#[test]
fn a_response_first_read_after_its_windows_settled_sets_the_peaks() {
    let home = HomeSandbox::new();
    reg(None);
    let s = ms(START);
    let store = iso_kerry(home.home());
    write_lines(
        &store.join("early.jsonl"),
        &[
            user(START, "early"),
            assistant(&iso(s + 1_000), "early", "msg_early", 100),
        ],
    );
    let mut p = Producer::new();
    let now = s + 30 * 60_000;
    p.tick(now);
    let late_end = now - 15 * 60_000;
    write_lines(
        &store.join("late.jsonl"),
        &[
            user(&iso(late_end - 10_000), "late"),
            assistant(&iso(late_end), "late", "msg_late", 2000),
        ],
    );
    let pk = peaks(&p.tick(now + DISCOVER_INTERVAL_MS));
    assert_eq!(
        pk["today"]["speed"],
        speed_peak(200.0, late_end, "kerry", MODEL),
        "{pk}"
    );
    close(
        pk["today"]["rate"]["value"].as_f64().unwrap_or(f64::NAN),
        200.0,
        &format!("rate: {pk}"),
    );
}

/// An upgrade from a cache an older clauth wrote (no peak fields at all): the
/// restart has nothing settled to start from, so it falls back to current
/// attribution and still shows today's peaks.
#[test]
fn an_upgrade_from_a_cache_without_peaks_still_shows_todays_peaks() {
    let home = HomeSandbox::new();
    reg(None);
    let s = ms(START);
    write_lines(
        &iso_kerry(home.home()).join("a.jsonl"),
        &[
            user(START, "a"),
            assistant(&iso(s + 10_000), "a", "msg_a", 2000),
        ],
    );
    let before = Producer::new().tick(s + 10_000 + 11 * 60_000);
    let mut v = serde_json::to_value(&before).unwrap();
    let keep = [
        "version",
        "generated_at",
        "total",
        "accounts",
        "sessions",
        "days",
        "frozen",
    ];
    v.as_object_mut()
        .unwrap()
        .retain(|k, _| keep.contains(&k.as_str()));
    for accounts in v["days"].as_object_mut().unwrap().values_mut() {
        for models in accounts.as_object_mut().unwrap().values_mut() {
            for cell in models.as_object_mut().unwrap().values_mut() {
                cell.as_object_mut()
                    .unwrap()
                    .retain(|k, _| k == "output" || k == "gen_ms");
            }
        }
    }
    let path = home.home().join(".clauth").join(TPS_CACHE_FILE);
    std::fs::write(&path, serde_json::to_vec(&v).unwrap()).unwrap();
    let pk = peaks(&Producer::new().tick(s + 10_000 + 11 * 60_000 + 1_000));
    assert_eq!(
        pk["today"]["speed"],
        speed_peak(200.0, s + 10_000, "kerry", MODEL),
        "{pk}"
    );
}

// ── incremental settle ───────────────────────────────────────────────────────

/// The oracle's own peak ordering: a higher value wins, a tie goes to the
/// earlier instant (the design's tie rule, written out here so the oracle does
/// not reuse the code's `peak_better`).
fn oracle_better(a: &PeakAt, b: &PeakAt) -> bool {
    a.value > b.value || (a.value == b.value && a.at < b.at)
}

/// Element-wise max of two cell speed-peak maps, written out for the oracle.
fn oracle_merge_speed(mut a: SpeedPeaks, b: &SpeedPeaks) -> SpeedPeaks {
    for (day, accounts) in b {
        for (account, models) in accounts {
            for (model, peak) in models {
                let slot = a
                    .entry(day.clone())
                    .or_default()
                    .entry(account.clone())
                    .or_default()
                    .entry(model.clone())
                    .or_default();
                if oracle_better(peak, slot) {
                    *slot = *peak;
                }
            }
        }
    }
    a
}

/// Element-wise max of two one-day rate-peak maps, written out for the oracle.
fn oracle_merge_rate(mut a: DayRatePeaks, b: &DayRatePeaks) -> DayRatePeaks {
    if let Some(p) = b.total {
        a.total = Some(match a.total {
            Some(t) if !oracle_better(&p, &t) => t,
            _ => p,
        });
    }
    for (account, p) in &b.accounts {
        a.accounts
            .entry(account.clone())
            .and_modify(|t| {
                if oracle_better(p, t) {
                    *t = *p;
                }
            })
            .or_insert(*p);
    }
    for (model, p) in &b.models {
        a.models
            .entry(model.clone())
            .and_modify(|t| {
                if oracle_better(p, t) {
                    *t = *p;
                }
            })
            .or_insert(*p);
    }
    a
}

fn oracle_empty(d: &DayRatePeaks) -> bool {
    d.total.is_none() && d.accounts.is_empty() && d.models.is_empty()
}

/// Plain max over response speeds (output ÷ span), keyed day → account → model,
/// independent of the sweep; only the two live days take part (the producer's
/// `speed_peaks_over` skips older days, whose peaks stay folded).
fn brute_speed(rs: &[&Resp], today: &str, yesterday: &str) -> SpeedPeaks {
    let mut out = SpeedPeaks::new();
    for r in rs {
        let Some((_, span)) = r.span() else {
            continue;
        };
        if r.output < SPEED_FLOOR {
            continue;
        }
        let day = day_of_ms(r.end_ms);
        if day != today && day != yesterday {
            continue;
        }
        let peak = PeakAt {
            value: r.output as f64 / (span as f64 / 1000.0),
            at: r.end_ms,
        };
        let slot = out
            .entry(day)
            .or_default()
            .entry(r.account.clone())
            .or_default()
            .entry(r.model.clone())
            .or_default();
        if oracle_better(&peak, slot) {
            *slot = peak;
        }
    }
    out
}

/// `window_figure` at every millisecond of the response support, independent of
/// the sweep's candidate generator.
fn brute_rate(rs: &[&Resp], lo: u64, hi: u64) -> Option<PeakAt> {
    if rs.is_empty() || lo > hi {
        return None;
    }
    let mut min_t = u64::MAX;
    let mut max_t = 0u64;
    for r in rs {
        let left = match r.span() {
            Some((start, _)) => start,
            None => r.end_ms,
        };
        min_t = min_t.min(left);
        max_t = max_t.max(r.end_ms + LIVE_WINDOW_MS);
    }
    let lo = lo.max(min_t.saturating_sub(LIVE_WINDOW_MS));
    let hi = hi.min(max_t);
    let mut best: Option<PeakAt> = None;
    for t in lo..=hi {
        let rate = window_figure(rs, t, LIVE_WINDOW_MS).rate;
        if rate > 0.0 {
            let cand = PeakAt { value: rate, at: t };
            if best.is_none_or(|b| oracle_better(&cand, &b)) {
                best = Some(cand);
            }
        }
    }
    best
}

fn brute_day_rate(
    total: &[&Resp],
    by_account: &BTreeMap<&str, Vec<&Resp>>,
    by_model: &BTreeMap<&str, Vec<&Resp>>,
    lo: u64,
    hi: u64,
) -> DayRatePeaks {
    let mut out = DayRatePeaks::default();
    if let Some(p) = brute_rate(total, lo, hi) {
        out.total = Some(p);
    }
    for (account, rs) in by_account {
        if let Some(p) = brute_rate(rs, lo, hi) {
            out.accounts.insert((*account).to_owned(), p);
        }
    }
    for (model, rs) in by_model {
        if let Some(p) = brute_rate(rs, lo, hi) {
            out.models.insert((*model).to_owned(), p);
        }
    }
    out
}

/// The oracle's responses and their rate groupings (total, per account, per
/// model): every response the producer holds (every value derives
/// from the responses' current state).
type OracleGroups<'a> = (
    Vec<&'a Resp>,
    Vec<&'a Resp>,
    BTreeMap<&'a str, Vec<&'a Resp>>,
    BTreeMap<&'a str, Vec<&'a Resp>>,
);

fn oracle_groups(p: &Producer) -> OracleGroups<'_> {
    let attr: Vec<&Resp> = p.responses.values().collect();
    let mut total: Vec<&Resp> = Vec::new();
    let mut by_account: BTreeMap<&str, Vec<&Resp>> = BTreeMap::new();
    let mut by_model: BTreeMap<&str, Vec<&Resp>> = BTreeMap::new();
    for r in &attr {
        total.push(*r);
        if r.account != UNATTRIBUTED {
            by_account.entry(r.account.as_str()).or_default().push(*r);
        }
        by_model.entry(r.model.as_str()).or_default().push(*r);
    }
    (attr, total, by_account, by_model)
}

/// The independent oracle: brute-force `window_figure` at every ms and plain
/// max over response speeds, with the settle boundary written out below. It
/// never calls the producer's sweep or settle functions; it reads only the
/// producer's input state (the responses). The boundary comes from the
/// caller, derived from the tick schedule, never from producer fields.
/// Returns (settled_speed, settled_rate, shown_speed, shown_rate).
fn peaks_from_scratch(
    p: &Producer,
    today: &str,
    yesterday: &str,
    settled_through: u64,
) -> (
    SpeedPeaks,
    BTreeMap<String, DayRatePeaks>,
    SpeedPeaks,
    BTreeMap<String, DayRatePeaks>,
) {
    let (attr, total, by_account, by_model) = oracle_groups(p);
    // The settled boundary, from the caller's schedule: settled speed covers
    // responses ending before `settled_through - LIVE`, settled rate the
    // windows ending before `settled_through`.
    let speed_boundary = settled_through.saturating_sub(LIVE_WINDOW_MS);
    let rate_hi = settled_through.saturating_sub(1);

    let settled_rs: Vec<&Resp> = attr
        .iter()
        .copied()
        .filter(|r| r.end_ms < speed_boundary)
        .collect();
    let settled_speed = oracle_merge_speed(
        SpeedPeaks::new(),
        &brute_speed(&settled_rs, today, yesterday),
    );
    let shown_speed = oracle_merge_speed(SpeedPeaks::new(), &brute_speed(&attr, today, yesterday));

    let mut settled_rate = BTreeMap::new();
    let mut shown_rate = BTreeMap::new();
    for (day, day_start) in [
        (today, day_start_ms(today)),
        (yesterday, day_start_ms(yesterday)),
    ] {
        // A shown day is every window ending on it that holds read tokens,
        // whatever the clock reads: a window ending after the clock holds
        // only responses stamped after it, and those stay shown (the spec).
        let full_hi = day_start + DAY_MS - 1;
        let srp = brute_day_rate(
            &total,
            &by_account,
            &by_model,
            day_start,
            rate_hi.min(full_hi),
        );
        let merged_s = oracle_merge_rate(DayRatePeaks::default(), &srp);
        if !oracle_empty(&merged_s) {
            settled_rate.insert((*day).to_owned(), merged_s);
        }
        let shown = brute_day_rate(&total, &by_account, &by_model, day_start, full_hi);
        let merged_u = oracle_merge_rate(DayRatePeaks::default(), &shown);
        if !oracle_empty(&merged_u) {
            shown_rate.insert((*day).to_owned(), merged_u);
        }
    }
    (settled_speed, settled_rate, shown_speed, shown_rate)
}

/// The settle boundary a tick's clock derives: 10 min before the tick plus
/// the live window, kept as a running max. Written out here so the oracle
/// derives the boundary from the schedule, never from the producer's
/// `settled_through`.
fn settle_at(now: u64) -> u64 {
    now.saturating_sub(10 * 60_000)
        .saturating_add(LIVE_WINDOW_MS)
}

/// The shown speed cells, compared both ways: every cell the producer wrote
/// must be in the oracle and vice versa, so an extra cell in the code reds.
fn assert_shown_speed(p: &Producer, us: &SpeedPeaks, today: &str, yesterday: &str, now: u64) {
    let mut got = SpeedPeaks::new();
    for day in [today, yesterday] {
        if let Some(accounts) = p.days.get(day) {
            for (account, models) in accounts {
                for (model, sum) in models {
                    if let Some(peak) = sum.speed {
                        got.entry(day.to_owned())
                            .or_default()
                            .entry(account.clone())
                            .or_default()
                            .insert(model.clone(), peak);
                    }
                }
            }
        }
    }
    let mut want = SpeedPeaks::new();
    for day in [today, yesterday] {
        if let Some(accounts) = us.get(day) {
            for (account, models) in accounts {
                for (model, peak) in models {
                    want.entry(day.to_owned())
                        .or_default()
                        .entry(account.clone())
                        .or_default()
                        .insert(model.clone(), *peak);
                }
            }
        }
    }
    assert_eq!(&got, &want, "shown speed at {now}");
}

/// The oracle's rate comparison. A rate value is a float sum whose last bit
/// depends on the accumulation order (the sweep sums densities, the brute
/// force sums token masses), so a peak is pinned by its structure, its
/// value within `1e-9 × max(|v|, 1)` — the same tolerance the sweep's own
/// brute test uses — never bit-for-bit, and an instant whose window reads
/// that value.
fn assert_rate_peaks_eq(
    got: &BTreeMap<String, DayRatePeaks>,
    want: &BTreeMap<String, DayRatePeaks>,
    groups: &OracleGroups,
    ctx: &str,
) {
    assert_eq!(
        got.keys().collect::<Vec<_>>(),
        want.keys().collect::<Vec<_>>(),
        "{ctx}: days"
    );
    for (day, g) in got {
        assert_rate_day_eq(g, &want[day], groups, day, &format!("{ctx} {day}"));
    }
}

fn assert_rate_day_eq(
    got: &DayRatePeaks,
    want: &DayRatePeaks,
    groups: &OracleGroups,
    day: &str,
    ctx: &str,
) {
    let (_, total, by_account, by_model) = groups;
    // A window belongs to the day it ends on, so every peak a day holds names
    // an instant of that day.
    for p in got
        .total
        .iter()
        .chain(got.accounts.values())
        .chain(got.models.values())
    {
        assert_eq!(day_of_ms(p.at), day, "{ctx}: {p:?} names another day");
    }
    assert_rate_peak_eq(got.total, want.total, total, &format!("{ctx} total"));
    assert_eq!(
        got.accounts.keys().collect::<Vec<_>>(),
        want.accounts.keys().collect::<Vec<_>>(),
        "{ctx} accounts"
    );
    for (k, g) in &got.accounts {
        assert_rate_peak_eq(
            Some(*g),
            Some(want.accounts[k]),
            by_account.get(k.as_str()).map_or(&[][..], Vec::as_slice),
            &format!("{ctx} account {k}"),
        );
    }
    assert_eq!(
        got.models.keys().collect::<Vec<_>>(),
        want.models.keys().collect::<Vec<_>>(),
        "{ctx} models"
    );
    for (k, g) in &got.models {
        assert_rate_peak_eq(
            Some(*g),
            Some(want.models[k]),
            by_model.get(k.as_str()).map_or(&[][..], Vec::as_slice),
            &format!("{ctx} model {k}"),
        );
    }
}

/// A rate peak names any window at the max: the values
/// agree, and the code's `at` is either the oracle's or a window of the same
/// grouping that `window_figure` reads at that max.
fn assert_rate_peak_eq(got: Option<PeakAt>, want: Option<PeakAt>, group: &[&Resp], ctx: &str) {
    match (got, want) {
        (None, None) => {}
        (Some(g), Some(w)) => {
            let tol = 1e-9 * g.value.abs().max(w.value.abs()).max(1.0);
            assert!(
                (g.value - w.value).abs() <= tol,
                "{ctx}: value {g:?} vs {w:?}"
            );
            if g.at != w.at {
                let there = window_figure(group, g.at, LIVE_WINDOW_MS).rate;
                assert!(
                    (there - w.value).abs() <= tol,
                    "{ctx}: at {g:?} reads {there}, the max is {w:?}"
                );
            }
        }
        _ => panic!("{ctx}: presence {got:?} vs {want:?}"),
    }
}

/// Tick once and verify the producer's settled and shown peaks against the
/// independent brute-force oracle, whose boundary the schedule supplies.
/// Returns the advanced settle boundary.
fn verify_tick(p: &mut Producer, now: u64, boundary: u64) -> u64 {
    let boundary = boundary.max(settle_at(now));
    let today = day_of_ms(now);
    let yesterday = day_of_ms(now - DAY_MS);
    p.tick(now);
    let (ss, sr, us, ur) = peaks_from_scratch(p, &today, &yesterday, boundary);
    let groups = oracle_groups(p);
    assert_eq!(p.settled_speed, ss, "settled speed at {now}");
    assert_rate_peaks_eq(
        &p.settled_rate_peaks,
        &sr,
        &groups,
        &format!("settled rate at {now}"),
    );
    assert_rate_peaks_eq(&p.rate_peaks, &ur, &groups, &format!("shown rate at {now}"));
    assert_shown_speed(p, &us, &today, &yesterday, now);
    boundary
}

/// One producer driven through many ticks — responses landing across the
/// settle boundary, a plateau, a point within 10 s of the boundary, an idle
/// stretch, a restart, a grown settled response, and a long span crossing
/// midnight read only after yesterday's windows settled — settles and shows
/// exactly the independent brute-force derivation after every tick.
#[test]
fn the_incremental_settle_matches_a_from_scratch_derivation() {
    let home = HomeSandbox::new();
    reg(None);
    let s = ms(START);
    let store = iso_kerry(home.home());
    let mut p = Producer::new();
    write_lines(
        &store.join("a.jsonl"),
        &[
            user(START, "a"),
            assistant(&iso(s + 10_000), "a", "msg_a", 200),
        ],
    );
    let mut boundary = verify_tick(&mut p, s + 11_000, 0);
    // b (25 t/s), c (1000/60 ≈ 16.67 t/s, its own model m_c), d a 60 s
    // plateau (20 t/s) and e a span-less point 5 s before the settle boundary
    // at the idle tick (its own model m_e): each owns its model grouping, so
    // a boundary move or a re-derive maxed with the producer's own settled
    // values shows up in the oracle's per-model check.
    write_lines(
        &store.join("b.jsonl"),
        &[
            user(&iso(s + 60_000), "b"),
            assistant(&iso(s + 80_000), "b", "msg_b", 500),
        ],
    );
    write_lines(
        &store.join("c.jsonl"),
        &[
            user(START, "c"),
            assistant_model(&iso(s + 60_000), "c", "msg_c", "m_c", 1000),
        ],
    );
    write_lines(
        &store.join("d.jsonl"),
        &[
            user(&iso(s + 20_000), "d"),
            assistant(&iso(s + 80_000), "d", "msg_d", 1200),
        ],
    );
    write_lines(
        &store.join("e.jsonl"),
        &[assistant_model(&iso(s + 125_000), "e", "msg_e", "m_e", 300)],
    );
    boundary = verify_tick(&mut p, s + 80_000, boundary);
    boundary = verify_tick(&mut p, s + 12 * 60_000, boundary);
    // c grows past the settle boundary — its first shape (16.67 t/s) is
    // already settled, so the re-derive must drop it and show the final shape
    // (1000/725 ≈ 1.38 t/s), never the mid-stream settle.
    append_lines(
        &store.join("c.jsonl"),
        &[
            user(&iso(s + 12 * 60_000), "c"),
            assistant(&iso(s + 12 * 60_000 + 5_000), "c", "msg_c", 1000),
        ],
    );
    verify_tick(&mut p, s + 12 * 60_000 + 6_000, boundary);
    // A restart derives everything afresh: its settle boundary starts over
    // from its own first tick.
    let cache_now = s + 22 * 60_000;
    write_cache(home.home(), &p.tick(cache_now));
    let mut p2 = Producer::new();
    let mut boundary2 = verify_tick(&mut p2, cache_now + 1_000, 0);
    // A tick past m's span start settles yesterday's windows before m lands.
    boundary2 = verify_tick(&mut p2, ms("2026-09-23T00:10:00.000Z"), boundary2);
    // m: 60 000 tokens over 23:55 → 00:11:40 (60 t/s), crossing midnight and
    // first read after yesterday's windows settled, so yesterday re-derives.
    write_lines(
        &store.join("m.jsonl"),
        &[
            user("2026-09-22T23:55:00.000Z", "m"),
            assistant("2026-09-23T00:11:40.000Z", "m", "msg_m", 60_000),
        ],
    );
    verify_tick(&mut p2, ms("2026-09-23T00:12:01.000Z"), boundary2);
}

/// The file every hand-built oracle response belongs to.
fn oracle_file() -> std::sync::Arc<FileRank> {
    std::sync::Arc::new(FileRank {
        path: PathBuf::from("oracle.jsonl"),
        created: None,
    })
}

fn lone_resp(start_ms: Option<u64>, end_ms: u64, output: u64) -> Resp {
    Resp {
        session: "p".into(),
        account: "a".into(),
        model: "m".into(),
        start_ms,
        end_ms,
        output,
        file: oracle_file(),
    }
}

/// A response first read after midnight whose span started yesterday, after
/// yesterday's windows settled, still reaches yesterday's rate peak: 10 200
/// tokens over 23:55:00 → 00:12:00 (1020 s) is 10 tok/s, first full window
/// ending 23:55:10.
#[test]
fn a_late_read_response_straddling_midnight_reaches_yesterdays_rate_peak() {
    let home = HomeSandbox::new();
    reg(None);
    let path = iso_kerry(home.home()).join("m.jsonl");
    write_lines(&path, &[user("2026-09-22T23:55:00.000Z", "m")]);
    let mut p = Producer::new();
    let mut t = ms("2026-09-22T23:55:01.000Z");
    while t < ms("2026-09-23T00:12:00.000Z") {
        p.tick(t);
        t += 60_000;
    }
    append_lines(
        &path,
        &[assistant("2026-09-23T00:12:00.000Z", "m", "msg_m", 10_200)],
    );
    let snap = p.tick(ms("2026-09-23T00:12:01.000Z"));
    let y = snap.rate_peaks.get("2026-09-22").and_then(|d| d.total);
    assert!(
        y.is_some_and(|p| (p.value - 10.0).abs() < 1e-9 && p.at == ms("2026-09-22T23:55:10.000Z")),
        "yesterday's rate peak: {y:?}"
    );
}

/// A response straddling midnight that grows after its yesterday windows
/// settled re-derives yesterday: 6600 tokens first over 23:50 → 00:01 (10
/// tok/s mid-stream), finally over 23:50 → 00:15 (1500 s, 4.4 tok/s).
#[test]
fn a_response_grown_after_midnight_rederives_yesterdays_rate_peak() {
    let home = HomeSandbox::new();
    reg(None);
    let path = iso_kerry(home.home()).join("m.jsonl");
    write_lines(&path, &[user("2026-09-22T23:50:00.000Z", "m")]);
    let mut p = Producer::new();
    let mut t = ms("2026-09-22T23:50:01.000Z");
    while t < ms("2026-09-23T00:01:00.000Z") {
        p.tick(t);
        t += 60_000;
    }
    append_lines(
        &path,
        &[assistant("2026-09-23T00:01:00.000Z", "m", "msg_m", 6_600)],
    );
    let mut t = ms("2026-09-23T00:01:01.000Z");
    while t < ms("2026-09-23T00:15:00.000Z") {
        p.tick(t);
        t += 60_000;
    }
    append_lines(
        &path,
        &[assistant("2026-09-23T00:15:00.000Z", "m", "msg_m", 6_600)],
    );
    let snap = p.tick(ms("2026-09-23T00:15:01.000Z"));
    let y = snap.rate_peaks.get("2026-09-22").and_then(|d| d.total);
    assert!(
        y.is_some_and(|p| (p.value - 4.4).abs() < 1e-9 && p.at == ms("2026-09-22T23:50:10.000Z")),
        "yesterday's rate peak kept the mid-stream shape: {y:?}"
    );
}

/// A user line between two block lines of one message id leaves the response
/// its first start (the span runs from the user line before the id's first
/// line), so the growth still re-derives the settled windows: 3000 tokens over
/// 665 s, never the 600 t/s mid-stream shape.
#[test]
fn a_user_line_between_block_lines_never_keeps_the_mid_stream_peak() {
    let home = HomeSandbox::new();
    reg(None);
    let s = ms(START);
    let path = iso_kerry(home.home()).join("u.jsonl");
    write_lines(
        &path,
        &[
            user(START, "u"),
            assistant(&iso(s + 5_000), "u", "msg_u", 3000),
        ],
    );
    let mut p = Producer::new();
    p.tick(s + 6_000);
    p.tick(s + 5_000 + 10 * 60_000 + 11_000);
    append_lines(
        &path,
        &[
            user(&iso(s + 11 * 60_000), "u"),
            assistant(&iso(s + 11 * 60_000 + 5_000), "u", "msg_u", 3000),
        ],
    );
    let pk = peaks(&p.tick(s + 11 * 60_000 + 6_000));
    let want = 3000.0 / 665.0;
    close(
        pk["today"]["speed"]["value"].as_f64().unwrap_or(f64::NAN),
        want,
        "speed",
    );
    close(
        pk["today"]["rate"]["value"].as_f64().unwrap_or(f64::NAN),
        want,
        "rate",
    );
}

/// On a plateau (every window inside one span holds the same tokens) the rate
/// peak reads the plateau's value at a window inside the span, in
/// `[start + 10 s, end]`: 1000 seeded lone responses longer than 10 s.
#[test]
fn a_plateau_rate_peak_names_a_window_inside_it() {
    let mut state = 0x1234_5678_u64;
    let base = 1_790_100_000_000u64;
    let mut late = Vec::new();
    for _ in 0..1000 {
        let start = base + oracle_lcg(&mut state) % 3_600_000;
        let span = 10_001 + oracle_lcg(&mut state) % 600_000;
        let output = 100 + oracle_lcg(&mut state) % 20_000;
        let r = [lone_resp(Some(start), start + span, output)];
        let rs: Vec<&Resp> = r.iter().collect();
        let p = rate_peak_sweep(&rs, base, base + 2 * 3_600_000).unwrap();
        let want = output as f64 / (span as f64 / 1000.0);
        let inside = (start + LIVE_WINDOW_MS..=start + span).contains(&p.at);
        if !inside || (p.value - want).abs() > 1e-9 * want.max(1.0) {
            late.push((start, span, output, p.at - start, p.value));
        }
    }
    assert!(
        late.is_empty(),
        "{} of 1000 miss the plateau, first: {:?}",
        late.len(),
        late.first()
    );
}

/// The busiest window can end exactly at a span's `start + 10 s`: A 1000
/// tokens over [0, 100 s] and B 1000 over [0, 2 s] peak at 10 s with
/// (100 + 1000) / 10 = 110 tok/s.
#[test]
fn the_rate_peak_can_end_at_a_spans_start_plus_ten_seconds() {
    let base = 1_790_100_000_000u64;
    let r = [
        lone_resp(Some(base), base + 100_000, 1000),
        lone_resp(Some(base), base + 2_000, 1000),
    ];
    let rs: Vec<&Resp> = r.iter().collect();
    let p = rate_peak_sweep(&rs, base, base + 200_000);
    assert!(
        p.is_some_and(|p| (p.value - 110.0).abs() < 1e-9 && p.at == base + 10_000),
        "{p:?}"
    );
}

/// A line stamped before midnight for a response whose stored end lies after
/// it (a span-less 00:00:05 response, then a 23:59:58 line read once the
/// boundary passed both) finishes its tick: the days owed a re-derive are a
/// bounded set, never a walk that can miss its stop.
#[test]
fn a_line_stamped_before_its_responses_stored_day_finishes_the_tick() {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let home = HomeSandbox::new();
        reg(None);
        let path = iso_kerry(home.home()).join("o.jsonl");
        write_lines(
            &path,
            &[assistant("2026-09-23T00:00:05.000Z", "o", "msg_o", 100)],
        );
        let mut p = Producer::new();
        let mut t = ms("2026-09-23T00:00:06.000Z");
        while t < ms("2026-09-23T00:12:00.000Z") {
            p.tick(t);
            t += 60_000;
        }
        append_lines(
            &path,
            &[assistant("2026-09-22T23:59:58.000Z", "o", "msg_o", 200)],
        );
        p.tick(ms("2026-09-23T00:12:01.000Z"));
        let _ = tx.send(());
        drop(home);
    });
    assert!(
        rx.recv_timeout(std::time::Duration::from_secs(20)).is_ok(),
        "the tick did not return within 20 s"
    );
}

/// Two equal-density plateaus (A and C, 1000 tokens over 100 s each: 100
/// tokens per window) with small overlapping spans between them: the peak is
/// 10 tok/s at a window inside A or C, whatever the spans between leave in
/// the running sums.
#[test]
fn equal_plateaus_after_other_spans_name_a_window_inside_either() {
    let base = 1_790_100_000_000u64;
    let r = [
        lone_resp(Some(base), base + 100_000, 1000),
        lone_resp(Some(base + 120_000), base + 120_000 + 16_934, 40),
        lone_resp(Some(base + 132_058), base + 132_058 + 1_370, 17),
        lone_resp(Some(base + 300_000), base + 400_000, 1000),
    ];
    let rs: Vec<&Resp> = r.iter().collect();
    let p = rate_peak_sweep(&rs, base, base + 500_000);
    assert!(
        p.is_some_and(|p| (p.value - 10.0).abs() < 1e-9
            && ((base + 10_000..=base + 100_000).contains(&p.at)
                || (base + 310_000..=base + 400_000).contains(&p.at))),
        "{p:?}"
    );
}

/// A range whose every window ends 10 s or more after every span has no rate
/// peak, for 20 000 seeded pairs of overlapping spans ending anywhere.
#[test]
fn an_empty_range_after_overlapping_spans_has_no_rate_peak() {
    let base = 1_790_100_000_000u64;
    let mut state = 0xabcdu64;
    let mut hits = Vec::new();
    for _ in 0..20_000 {
        let s1 = 1_000 + oracle_lcg(&mut state) % 20_000;
        let o1 = 1 + oracle_lcg(&mut state) % 5_000;
        let o2 = 1 + oracle_lcg(&mut state) % 5_000;
        let off = 1 + oracle_lcg(&mut state) % (s1 - 1);
        let s2 = s1 - off;
        let r = [
            lone_resp(Some(base), base + s1, o1),
            lone_resp(Some(base + off), base + off + s2, o2),
        ];
        let rs: Vec<&Resp> = r.iter().collect();
        let end = (base + s1).max(base + off + s2);
        if let Some(p) = rate_peak_sweep(&rs, end + LIVE_WINDOW_MS, end + LIVE_WINDOW_MS + 5_000) {
            hits.push((s1, s2, o1, o2, off, p));
        }
    }
    assert!(
        hits.is_empty(),
        "{} of 20000 empty ranges emit a peak, first: {:?}",
        hits.len(),
        hits.first()
    );
}

/// Two responses of one model ending at exactly 23:59:50.000 leave today's
/// windows empty: today has no rate peak in the feed (absent, never a 0).
#[test]
fn responses_ending_ten_seconds_before_midnight_leave_today_without_a_rate_peak() {
    let home = HomeSandbox::new();
    reg(None);
    let e = ms("2026-09-22T23:59:50.000Z");
    let store = iso_kerry(home.home());
    write_lines(
        &store.join("r1.jsonl"),
        &[
            user(&iso(e - 7_744), "r1"),
            assistant(&iso(e), "r1", "msg_r1", 2864),
        ],
    );
    write_lines(
        &store.join("r2.jsonl"),
        &[
            user(&iso(e - 2_425), "r2"),
            assistant(&iso(e), "r2", "msg_r2", 283),
        ],
    );
    let mut p = Producer::new();
    let mut t = e + 1_000;
    let mut snap = None;
    while t < ms("2026-09-23T00:15:00.000Z") {
        snap = Some(p.tick(t));
        t += 30_000;
    }
    let pk = peaks(&snap.unwrap());
    assert!(
        pk["today"]["rate"].is_null(),
        "today's rate peak: {}",
        pk["today"]["rate"]
    );
}

/// After a 15 min backward clock step, a response read at the rewound clock
/// still reaches the peak whose window ends after that read: Q (1000 over
/// [x − 5 s, x]) meets P (2000 over [x, x + 20 s]), busiest window ending at
/// x + 5 s with (1000 + 500) / 10 = 150 tok/s.
#[test]
fn a_backward_clock_step_still_finds_the_window_after_the_read() {
    let home = HomeSandbox::new();
    reg(None);
    let x = ms(START) + 110 * 60_000;
    let path = iso_kerry(home.home()).join("k.jsonl");
    write_lines(
        &path,
        &[
            user(&iso(x), "k"),
            assistant(&iso(x + 20_000), "k", "msg_p", 2000),
        ],
    );
    let mut p = Producer::new();
    let mut t = x + 21_000;
    while t < x + 15 * 60_000 {
        p.tick(t);
        t += 60_000;
    }
    append_lines(
        &path,
        &[
            user(&iso(x - 5_000), "k"),
            assistant(&iso(x), "k", "msg_q", 1000),
        ],
    );
    p.tick(x + 1_000);
    let mut last = None;
    for k in 1..=20u64 {
        last = Some(p.tick(x + 1_000 + k * 60_000));
    }
    let pk = peaks(&last.unwrap());
    close(
        pk["today"]["rate"]["value"].as_f64().unwrap_or(f64::NAN),
        150.0,
        "rate",
    );
    assert_eq!(pk["today"]["rate"]["at"], serde_json::json!(at(x + 5_000)));
}

/// A span plateau and a span-less point of equal window mass both read the
/// max, and the peak names a window of either (a rate tie
/// names any window at the max): A 1000 over [x, x + 100 s] (100 tokens in
/// every window inside it) and a 100-token point at x + 300 s are both
/// 10 tok/s, in windows ending in [x + 10 s, x + 100 s] or
/// [x + 300 s, x + 310 s].
#[test]
fn a_plateau_and_a_point_of_equal_mass_name_a_window_at_the_max() {
    let home = HomeSandbox::new();
    reg(None);
    let x = ms(START) + 60 * 60_000;
    let store = iso_kerry(home.home());
    write_lines(
        &store.join("a.jsonl"),
        &[
            user(&iso(x), "a"),
            assistant(&iso(x + 100_000), "a", "msg_a", 1000),
        ],
    );
    write_lines(
        &store.join("pt.jsonl"),
        &[assistant(&iso(x + 300_000), "pt", "msg_pt", 100)],
    );
    let pk = peaks(&Producer::new().tick(x + 311_000));
    close(
        pk["today"]["rate"]["value"].as_f64().unwrap_or(f64::NAN),
        10.0,
        "rate",
    );
    let got = pk["today"]["rate"]["at"].as_str().map(ms);
    assert!(
        got.is_some_and(
            |t| (x + 10_000..=x + 100_000).contains(&t) || (x + 300_000..=x + 310_000).contains(&t)
        ),
        "at: {}",
        pk["today"]["rate"]
    );
}

/// An exact 10 tok/s plateau reads exactly 10 in the feed, never a hair under
/// it: 1000 tokens over 100 s.
#[test]
fn an_exact_plateau_rate_reads_its_exact_value() {
    let home = HomeSandbox::new();
    reg(None);
    let x = ms(START) + 60 * 60_000;
    write_lines(
        &iso_kerry(home.home()).join("a.jsonl"),
        &[
            user(&iso(x), "a"),
            assistant(&iso(x + 100_000), "a", "msg_a", 1000),
        ],
    );
    let pk = peaks(&Producer::new().tick(x + 101_000));
    assert_eq!(pk["today"]["rate"]["value"], serde_json::json!(10.0));
}

/// A response ending in the last 10 s before midnight, first read after the
/// settle boundary passed midnight, still gives today its rate peak: 1000
/// tokens over 23:59:50 → 23:59:58; the window ending 00:00:00 holds all of
/// it, 100 tok/s, and a window belongs to the day it ends on.
#[test]
fn a_response_ending_just_before_midnight_fills_todays_first_window() {
    let home = HomeSandbox::new();
    reg(None);
    let store = iso_kerry(home.home());
    write_lines(
        &store.join("other.jsonl"),
        &[user("2026-09-22T23:00:00.000Z", "other")],
    );
    let mut p = Producer::new();
    let mut t = ms("2026-09-22T23:58:00.000Z");
    while t < ms("2026-09-23T00:12:00.000Z") {
        p.tick(t);
        t += 60_000;
    }
    write_lines(
        &store.join("tail.jsonl"),
        &[
            user("2026-09-22T23:59:50.000Z", "tail"),
            assistant("2026-09-22T23:59:58.000Z", "tail", "msg_tail", 1000),
        ],
    );
    let pk = peaks(&p.tick(ms("2026-09-23T00:12:01.000Z")));
    close(
        pk["today"]["rate"]["value"].as_f64().unwrap_or(f64::NAN),
        100.0,
        "today's rate",
    );
    assert_eq!(
        pk["today"]["rate"]["at"],
        serde_json::json!(at(ms("2026-09-23T00:00:00.000Z")))
    );
}

/// A peak shown before a backward clock step stays shown, with its real `at`,
/// through the whole rewound stretch: P 2000 over [x, x + 20 s] peaks at
/// 100 tok/s from x + 10 s; the clock then reads x − 30 min and walks forward
/// past P again.
#[test]
fn a_backward_clock_step_keeps_the_peak_already_shown() {
    let home = HomeSandbox::new();
    reg(None);
    let x = ms(START) + 110 * 60_000;
    write_lines(
        &iso_kerry(home.home()).join("k.jsonl"),
        &[
            user(&iso(x), "k"),
            assistant(&iso(x + 20_000), "k", "msg_p", 2000),
        ],
    );
    let mut p = Producer::new();
    let mut t = x + 21_000;
    let mut steps = Vec::new();
    while t < x + 15 * 60_000 {
        steps.push(t);
        t += 60_000;
    }
    steps.extend([
        x - 30 * 60_000,
        x - 29 * 60_000,
        x + 5_000,
        x + 11_000,
        x + 31_000,
        x + 15 * 60_000,
    ]);
    for now in steps {
        let pk = peaks(&p.tick(now));
        close(
            pk["today"]["rate"]["value"].as_f64().unwrap_or(f64::NAN),
            100.0,
            &format!("rate at {}", iso(now)),
        );
        assert_eq!(
            pk["today"]["rate"]["at"],
            serde_json::json!(at(x + 10_000)),
            "at, read at {}",
            iso(now)
        );
    }
}

/// A response stamped ahead of the clock is shown once it is read, with its
/// real `at`: 1000 tokens over [T + 60 s, T + 70 s] read at T peak at 100 tok/s
/// at T + 70 s, in the feed while the clock still reads before T + 60 s.
#[test]
fn a_response_stamped_ahead_of_the_clock_is_shown_at_read() {
    let home = HomeSandbox::new();
    reg(None);
    let tt = ms(START) + 60 * 60_000;
    let store = iso_kerry(home.home());
    write_lines(
        &store.join("old.jsonl"),
        &[
            user(START, "old"),
            assistant(&iso(ms(START) + 10_000), "old", "msg_old", 200),
        ],
    );
    let mut p = Producer::new();
    p.tick(tt - 30 * 60_000);
    p.tick(tt - 1_000);
    write_lines(
        &store.join("fut.jsonl"),
        &[
            user(&iso(tt + 60_000), "fut"),
            assistant(&iso(tt + 70_000), "fut", "msg_fut", 1000),
        ],
    );
    let mut last = None;
    for k in 0..30u64 {
        last = Some(p.tick(tt + k * 1_000));
    }
    let pk = peaks(&last.unwrap());
    close(
        pk["today"]["rate"]["value"].as_f64().unwrap_or(f64::NAN),
        100.0,
        "rate",
    );
    assert_eq!(
        pk["today"]["rate"]["at"],
        serde_json::json!(at(tt + 70_000))
    );
}

/// A span one token heavier per window than a point reads above it: the point
/// (10 000 tokens at x, 1000 tok/s) comes first, then A (100 001 tokens over
/// [x + 100 s, x + 200 s], 10 000.1 tokens in every window inside it,
/// 1000.01 tok/s). The peak is A's, in a window ending in
/// [x + 110 s, x + 200 s].
#[test]
fn a_span_a_tenth_of_a_token_heavier_than_a_point_wins_the_rate_peak() {
    let home = HomeSandbox::new();
    reg(None);
    let x = ms(START) + 60 * 60_000;
    let store = iso_kerry(home.home());
    write_lines(
        &store.join("pt.jsonl"),
        &[assistant(&iso(x), "pt", "msg_pt", 10_000)],
    );
    write_lines(
        &store.join("a.jsonl"),
        &[
            user(&iso(x + 100_000), "a"),
            assistant(&iso(x + 200_000), "a", "msg_a", 100_001),
        ],
    );
    let pk = peaks(&Producer::new().tick(x + 201_000));
    let value = pk["today"]["rate"]["value"].as_f64().unwrap_or(f64::NAN);
    assert!(
        (value - 1000.01).abs() < 1e-6,
        "rate: {}",
        pk["today"]["rate"]
    );
    let got = pk["today"]["rate"]["at"].as_str().map(ms);
    assert!(
        got.is_some_and(|t| (x + 110_000..=x + 200_000).contains(&t)),
        "at: {}",
        pk["today"]["rate"]
    );
}

/// A clock step back across midnight, after the rollover folded the day before
/// yesterday into the ledger, never refolds that day lower: D-1 (2026-09-21)
/// holds A (2000 over 12:00:00 → 12:00:20, 100 tok/s) and a 30-token tail over
/// 23:59:52 → 23:59:55; the ledger holds D-1 at D+1 00:05, then the clock reads
/// D 23:50 and walks back past midnight. The stored day keeps its peak and its
/// sums (older days keep the peaks they folded with).
#[test]
fn a_clock_step_back_across_midnight_keeps_a_folded_day() {
    let home = HomeSandbox::new();
    reg(None);
    let store = iso_kerry(home.home());
    write_lines(
        &store.join("a.jsonl"),
        &[
            user("2026-09-21T12:00:00.000Z", "a"),
            assistant("2026-09-21T12:00:20.000Z", "a", "msg_a", 2000),
        ],
    );
    let mut p = Producer::new();
    let mut t = ms("2026-09-21T12:01:00.000Z");
    while t < ms("2026-09-21T23:59:56.000Z") {
        p.tick(t);
        t += 600_000;
    }
    write_lines(
        &store.join("t.jsonl"),
        &[
            user("2026-09-21T23:59:52.000Z", "t"),
            assistant("2026-09-21T23:59:55.000Z", "t", "msg_t", 30),
        ],
    );
    let mut t = ms("2026-09-21T23:59:56.000Z");
    while t < ms("2026-09-23T00:05:00.000Z") {
        p.tick(t);
        t += 600_000;
    }
    p.tick(ms("2026-09-23T00:05:00.000Z"));
    let ledger = home.home().join(".clauth").join("token_speed.json");
    let before: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&ledger).unwrap()).unwrap();
    close(
        before["rate_peaks"]["2026-09-21"]["total"]["value"]
            .as_f64()
            .unwrap_or(f64::NAN),
        100.0,
        "D-1 folded before the step",
    );
    let mut t = ms("2026-09-22T23:50:00.000Z");
    let mut last = None;
    while t <= ms("2026-09-23T00:10:01.000Z") {
        last = Some(p.tick(t));
        t += 60_000;
    }
    let pk = peaks(&last.unwrap());
    close(
        pk["ever"]["rate"]["value"].as_f64().unwrap_or(f64::NAN),
        100.0,
        "ever rate",
    );
    let disk: serde_json::Value = serde_json::from_slice(&std::fs::read(&ledger).unwrap()).unwrap();
    close(
        disk["rate_peaks"]["2026-09-21"]["total"]["value"]
            .as_f64()
            .unwrap_or(f64::NAN),
        100.0,
        "stored D-1 rate peak",
    );
    assert_eq!(
        disk["days"]["2026-09-21"]["kerry"][MODEL]["output"],
        serde_json::json!(2030),
        "stored D-1 sums: {}",
        disk["days"]["2026-09-21"]
    );
}

/// A restart after a forward clock error: producer 1 ran with the clock 2 h
/// ahead and wrote its cache there; producer 2 runs on the corrected clock.
/// A response appended to the same transcript after the restart (R2, 2000
/// over [x + 60 s, x + 80 s]: 100 t/s, 100 tok/s) lies past what producer 1
/// read, so it counts, whatever its timestamp: in the day sums and the peaks.
#[test]
fn a_restart_after_a_forward_clock_error_counts_the_lines_read_after_it() {
    let home = HomeSandbox::new();
    reg(None);
    let x = ms(START) + 30 * 60_000;
    let path = iso_kerry(home.home()).join("r.jsonl");
    write_lines(
        &path,
        &[
            user(&iso(x + 7_200_000 - 30_000), "r"),
            assistant(&iso(x + 7_200_000 - 10_000), "r", "msg_r1", 200),
        ],
    );
    let mut p1 = Producer::new();
    let s1 = p1.tick(x + 7_200_000);
    write_cache(home.home(), &s1);
    let mut p2 = Producer::new();
    p2.tick(x);
    append_lines(
        &path,
        &[
            user(&iso(x + 60_000), "r"),
            assistant(&iso(x + 80_000), "r", "msg_r2", 2000),
        ],
    );
    let mut last = None;
    let mut t = x + 81_000;
    while t < x + 20 * 60_000 {
        last = Some(p2.tick(t));
        t += 60_000;
    }
    let snap = last.unwrap();
    let pk = peaks(&snap);
    assert_eq!(
        pk["today"]["speed"],
        speed_peak(100.0, x + 80_000, "kerry", MODEL),
        "{pk}"
    );
    close(
        pk["today"]["rate"]["value"].as_f64().unwrap_or(f64::NAN),
        100.0,
        "rate",
    );
    let wire = serde_json::to_value(&snap).unwrap();
    assert_eq!(
        wire["days"][day_of_ms(x)]["kerry"][MODEL]["output"],
        serde_json::json!(2200),
        "day sums: {}",
        wire["days"]
    );
}

/// A line stamped on the box's next UTC day, read before this box's midnight
/// (its clock 2 min behind), counts toward `ever` at once with its real `at`
/// and joins `today` once the box's day reaches it: 2000
/// over 2026-09-23 00:01:00 → 00:01:20 (100 t/s, 100 tok/s) beside a 5 tok/s
/// response of 09-22.
#[test]
fn a_line_stamped_on_the_next_day_counts_toward_ever_at_once() {
    let home = HomeSandbox::new();
    reg(None);
    let store = iso_kerry(home.home());
    write_lines(
        &store.join("old.jsonl"),
        &[
            user("2026-09-22T23:00:00.000Z", "old"),
            assistant("2026-09-22T23:00:10.000Z", "old", "msg_old", 50),
        ],
    );
    let mut p = Producer::new();
    p.tick(ms("2026-09-22T23:30:00.000Z"));
    write_lines(
        &store.join("fut.jsonl"),
        &[
            user("2026-09-23T00:01:00.000Z", "fut"),
            assistant("2026-09-23T00:01:20.000Z", "fut", "msg_fut", 2000),
        ],
    );
    let fut_end = ms("2026-09-23T00:01:20.000Z");
    let in_fut = |v: &serde_json::Value| {
        v["at"]
            .as_str()
            .map(ms)
            .is_some_and(|t| (fut_end - 10_000..=fut_end).contains(&t))
    };
    for now in ["2026-09-22T23:59:01.000Z", "2026-09-22T23:59:59.000Z"] {
        let pk = peaks(&p.tick(ms(now)));
        close(
            pk["ever"]["rate"]["value"].as_f64().unwrap_or(f64::NAN),
            100.0,
            &format!("ever rate at {now}"),
        );
        assert!(in_fut(&pk["ever"]["rate"]), "ever rate at {now}: {pk}");
        assert_eq!(
            pk["ever"]["speed"],
            speed_peak(100.0, fut_end, "kerry", MODEL),
            "ever speed at {now}"
        );
        close(
            pk["today"]["rate"]["value"].as_f64().unwrap_or(f64::NAN),
            5.0,
            &format!("today rate at {now}"),
        );
    }
    let pk = peaks(&p.tick(ms("2026-09-23T00:00:01.000Z")));
    close(
        pk["today"]["rate"]["value"].as_f64().unwrap_or(f64::NAN),
        100.0,
        "today rate after midnight",
    );
    assert!(in_fut(&pk["today"]["rate"]), "today rate: {pk}");
}

/// A file the previous producer never read counts every line on a restart,
/// whatever its timestamp — even a line stamped before the previous cache's
/// `generated_at`.
#[test]
fn a_restart_counts_a_file_the_previous_producer_never_read() {
    let home = HomeSandbox::new();
    reg(None);
    let x = ms(START) + 30 * 60_000;
    let store = iso_kerry(home.home());
    write_lines(
        &store.join("a.jsonl"),
        &[
            user(&iso(x), "a"),
            assistant(&iso(x + 10_000), "a", "msg_a", 200),
        ],
    );
    let mut p1 = Producer::new();
    let s1 = p1.tick(x + 60_000);
    write_cache(home.home(), &s1);
    // A new file, stamped before the previous write.
    write_lines(
        &store.join("b.jsonl"),
        &[
            user(&iso(x), "b"),
            assistant(&iso(x + 10_000), "b", "msg_b", 2000),
        ],
    );
    let snap = Producer::new().tick(x + 61_000);
    assert_eq!(
        snap.days[&day_of_ms(x + 10_000)]["kerry"][MODEL].output,
        2200,
        "the never-read file counts: {:?}",
        snap.days[&day_of_ms(x + 10_000)]
    );
}

/// A file truncated and rewritten while no producer ran: the restart counts
/// what the file holds now (no cache floor: the truncated
/// response is gone with its bytes), 400 over 20 s.
#[test]
fn a_restart_counts_what_a_truncated_file_holds_now() {
    let home = HomeSandbox::new();
    reg(None);
    let x = ms(START);
    let path = iso_kerry(home.home()).join("trunc.jsonl");
    write_lines(
        &path,
        &[
            user(START, "trunc"),
            assistant(
                &iso(x + 10_000),
                "trunc",
                "a-rather-long-message-id-for-the-old-response",
                200,
            ),
        ],
    );
    let mut p1 = Producer::new();
    let s1 = p1.tick(x + 60_000);
    write_cache(home.home(), &s1);
    // Truncate, then append a shorter new response.
    std::fs::write(&path, "").unwrap();
    append_lines(
        &path,
        &[
            user(START, "trunc"),
            assistant(&iso(x + 20_000), "trunc", "m2", 400),
        ],
    );
    let snap = Producer::new().tick(x + 61_000);
    let cell = &snap.days[&day_of_ms(x + 20_000)]["kerry"][MODEL];
    assert_eq!(cell.output, 400, "the file's response only: {cell:?}");
    assert_eq!(cell.gen_ms, 20_000, "its span: {cell:?}");
}

/// A future day's response, a later user line after it, stays out of the
/// speed ledger while its day is today and while it is yesterday, and folds
/// once the day leaves the window.
#[test]
fn a_future_day_folds_once_it_leaves_the_window() {
    let home = HomeSandbox::new();
    reg(None);
    let store = iso_kerry(home.home());
    write_lines(
        &store.join("old.jsonl"),
        &[
            user("2026-09-22T23:00:00.000Z", "old"),
            assistant("2026-09-22T23:00:10.000Z", "old", "msg_old", 50),
        ],
    );
    write_lines(
        &store.join("fut.jsonl"),
        &[
            user("2026-09-23T00:01:00.000Z", "fut"),
            assistant("2026-09-23T00:01:20.000Z", "fut", "msg_fut", 2000),
            user("2026-09-23T00:02:00.000Z", "fut"),
        ],
    );
    let mut p = Producer::new();
    let ledger = home.home().join(".clauth").join("token_speed.json");
    let disk = || -> serde_json::Value {
        match std::fs::read(&ledger) {
            Ok(b) => serde_json::from_slice(&b).unwrap(),
            Err(_) => serde_json::json!({}),
        }
    };
    p.tick(ms("2026-09-22T23:59:01.000Z"));
    p.tick(ms("2026-09-23T00:00:01.000Z")); // the future day is today
    let d = disk();
    assert!(
        d["days"].get("2026-09-23").is_none(),
        "today never folds: {d}"
    );
    p.tick(ms("2026-09-24T00:00:01.000Z")); // now yesterday
    let d = disk();
    assert!(
        d["days"].get("2026-09-23").is_none(),
        "yesterday never folds: {d}"
    );
    p.tick(ms("2026-09-25T00:00:01.000Z")); // out of the window
    let d = disk();
    assert!(
        d["days"].get("2026-09-23").is_some(),
        "folds once it left the window: {d}"
    );
    assert!(
        d["rate_peaks"].get("2026-09-23").is_some(),
        "rate peaks fold with it: {d}"
    );
}

/// The output summed over every day, account and model a snapshot holds.
fn day_total(s: &Snapshot) -> u64 {
    let wire = serde_json::to_value(s).unwrap();
    let mut total = 0;
    for accounts in wire["days"]
        .as_object()
        .into_iter()
        .flat_map(|d| d.values())
    {
        for models in accounts.as_object().into_iter().flat_map(|a| a.values()) {
            for cell in models.as_object().into_iter().flat_map(|m| m.values()) {
                total += cell["output"].as_u64().unwrap_or(0);
            }
        }
    }
    total
}

/// The output a ledger day holds on disk, summed over its accounts and models.
fn ledger_day_output(home: &Path, day: &str) -> u64 {
    let disk: serde_json::Value = serde_json::from_slice(
        &std::fs::read(home.join(".clauth").join("token_speed.json")).unwrap(),
    )
    .unwrap();
    let mut total = 0;
    for models in disk["days"][day]
        .as_object()
        .into_iter()
        .flat_map(|a| a.values())
    {
        for cell in models.as_object().into_iter().flat_map(|m| m.values()) {
            total += cell["output"].as_u64().unwrap_or(0);
        }
    }
    total
}

/// clauth's own isolated-teardown rescue moves a transcript the previous
/// producer already counted into the global store: a restart counts it once
/// (one response per message id), as the running producer does.
#[test]
fn a_restart_after_a_rescue_moved_a_counted_transcript_counts_it_once() {
    let home = HomeSandbox::new();
    reg(None);
    let x = ms(START) + 30 * 60_000;
    let iso_root = home
        .home()
        .join(".clauth/profiles/kerry/runtime-isolated-4242-1/projects");
    write_lines(
        &iso_kerry(home.home()).join("r.jsonl"),
        &[
            user(&iso(x), "r"),
            assistant(&iso(x + 20_000), "r", "msg_r", 2000),
        ],
    );
    let mut p1 = Producer::new();
    let s1 = p1.tick(x + 60_000);
    assert_eq!(day_total(&s1), 2000, "before the move");
    write_cache(home.home(), &s1);
    let global = home.home().join(".claude/projects");
    assert_eq!(
        crate::sessions::rescue_transcripts(&iso_root, &global).moved,
        1,
        "the rescue moved the transcript"
    );
    assert_eq!(day_total(&p1.tick(x + 66_000)), 2000, "running producer");
    assert_eq!(
        day_total(&Producer::new().tick(x + 67_000)),
        2000,
        "restarted producer"
    );
}

/// A file truncated below what the previous producer read, then regrown past
/// that point after the restart: the restart counts what the file holds (no
/// cache floor: 400), and every line of the regrowth counts
/// (800 more: 1200).
#[test]
fn a_file_truncated_then_regrown_past_its_offset_counts_every_new_line() {
    let home = HomeSandbox::new();
    reg(None);
    let x = ms(START);
    let path = iso_kerry(home.home()).join("trunc.jsonl");
    let long_old = format!("old-{}", "o".repeat(700));
    write_lines(
        &path,
        &[
            user(START, "trunc"),
            assistant(&iso(x + 10_000), "trunc", &long_old, 200),
        ],
    );
    let mut p1 = Producer::new();
    write_cache(home.home(), &p1.tick(x + 60_000));
    let saved = std::fs::metadata(&path).unwrap().len();
    std::fs::write(&path, "").unwrap();
    append_lines(
        &path,
        &[
            user(&iso(x + 61_000), "trunc"),
            assistant(&iso(x + 70_000), "trunc", "m2", 400),
        ],
    );
    let mut p2 = Producer::new();
    assert_eq!(day_total(&p2.tick(x + 71_000)), 400, "after the restart");
    let long_new = format!("m3-{}", "n".repeat(700));
    append_lines(
        &path,
        &[
            user(&iso(x + 72_000), "trunc"),
            assistant(&iso(x + 80_000), "trunc", &long_new, 800),
        ],
    );
    assert!(
        std::fs::metadata(&path).unwrap().len() > saved,
        "the fixture regrew past the saved offset"
    );
    assert_eq!(day_total(&p2.tick(x + 86_000)), 1200, "after the regrowth");
}

/// A file rewritten longer than before while no producer runs: the restart
/// counts what the file holds now (no cache floor), the 400
/// of the rewrite.
#[test]
fn a_file_regrown_past_its_offset_before_the_restart_counts_every_new_line() {
    let home = HomeSandbox::new();
    reg(None);
    let x = ms(START);
    let path = iso_kerry(home.home()).join("regrow.jsonl");
    let old = format!("old-{}", "o".repeat(700));
    write_lines(
        &path,
        &[
            user(START, "regrow"),
            assistant(&iso(x + 10_000), "regrow", &old, 200),
        ],
    );
    let mut p1 = Producer::new();
    let s1 = p1.tick(x + 60_000);
    assert_eq!(day_total(&s1), 200, "before the restart");
    write_cache(home.home(), &s1);
    let saved = std::fs::metadata(&path).unwrap().len();
    let new = format!("new-{}", "n".repeat(900));
    write_lines(
        &path,
        &[
            user(&iso(x + 61_000), "regrow"),
            assistant(&iso(x + 70_000), "regrow", &new, 400),
        ],
    );
    assert!(
        std::fs::metadata(&path).unwrap().len() > saved,
        "the fixture regrew past the saved offset"
    );
    assert_eq!(
        day_total(&Producer::new().tick(x + 71_000)),
        400,
        "the regrown file's response only"
    );
}

/// A forward clock error of 3 days folds the real today D into the ledger
/// early; after the correction B lands on D (4000 over 20 s: 200 t/s,
/// 200 tok/s). The correction re-reads D whole and the real fold keeps the
/// larger state: D ends with A + B = 6000 on disk and a 200 tok/s
/// rate peak, and today's peaks after the correction come from the responses'
/// current state.
#[test]
fn a_day_folded_early_by_a_fast_clock_merges_its_later_tokens() {
    let home = HomeSandbox::new();
    reg(None);
    let path = iso_kerry(home.home()).join("a.jsonl");
    write_lines(
        &path,
        &[
            user("2026-09-21T12:00:00.000Z", "a"),
            assistant("2026-09-21T12:00:20.000Z", "a", "msg_a", 2000),
        ],
    );
    let mut p = Producer::new();
    p.tick(ms("2026-09-21T12:01:00.000Z"));
    p.tick(ms("2026-09-24T00:05:00.000Z"));
    assert_eq!(
        ledger_day_output(home.home(), "2026-09-21"),
        2000,
        "D folded early"
    );
    p.tick(ms("2026-09-21T12:10:00.000Z"));
    append_lines(
        &path,
        &[
            user("2026-09-21T12:20:00.000Z", "a"),
            assistant("2026-09-21T12:20:20.000Z", "a", "msg_b", 4000),
        ],
    );
    let pk = peaks(&p.tick(ms("2026-09-21T12:21:00.000Z")));
    assert_eq!(
        pk["today"]["speed"],
        speed_peak(200.0, ms("2026-09-21T12:20:20.000Z"), "kerry", MODEL),
        "today after the correction"
    );
    let mut t = ms("2026-09-21T12:31:00.000Z");
    while t < ms("2026-09-23T00:05:00.000Z") {
        p.tick(t);
        t += 600_000;
    }
    p.tick(ms("2026-09-23T00:05:00.000Z"));
    assert_eq!(
        ledger_day_output(home.home(), "2026-09-21"),
        6000,
        "D merged"
    );
    let disk: serde_json::Value = serde_json::from_slice(
        &std::fs::read(home.home().join(".clauth").join("token_speed.json")).unwrap(),
    )
    .unwrap();
    close(
        disk["rate_peaks"]["2026-09-21"]["total"]["value"]
            .as_f64()
            .unwrap_or(f64::NAN),
        200.0,
        "stored D rate peak",
    );
}

/// A clock step back never stalls discovery: after a 15 min step back a new
/// transcript reaches the feed within one discovery interval of landing.
#[test]
fn a_new_transcript_after_a_clock_step_back_is_discovered_at_once() {
    let home = HomeSandbox::new();
    reg(None);
    let store = iso_kerry(home.home());
    let x = ms(START);
    write_lines(
        &store.join("a.jsonl"),
        &[
            user(&iso(x), "a"),
            assistant(&iso(x + 20_000), "a", "msg_a", 2000),
        ],
    );
    let mut p = Producer::new();
    p.tick(x + 15 * 60_000 + 30_000);
    write_lines(
        &store.join("n.jsonl"),
        &[
            user(&iso(x + 60_000), "n"),
            assistant(&iso(x + 80_000), "n", "msg_n", 3000),
        ],
    );
    let mut seen = None;
    let mut t = x + 81_000;
    while t < x + 81_000 + 60_000 && seen.is_none() {
        if p.tick(t).sessions.contains_key("n") {
            seen = Some(t);
        }
        t += 1_000;
    }
    assert!(
        seen.is_some_and(|s| s <= x + 80_000 + DISCOVER_INTERVAL_MS + 1_000),
        "the new session first reached the feed at {seen:?} (landed {})",
        x + 80_000
    );
}

/// A D-1-stamped line landing after D-1 was folded, under a clock step back
/// across midnight that makes D-1 live again: the re-read day grows and its
/// refold keeps the larger state, D-1 held 2030 and gains the late 7-token
/// response, 2037; its rate peak stays 100.
#[test]
fn a_late_line_on_a_folded_day_merges_into_it() {
    let home = HomeSandbox::new();
    reg(None);
    let store = iso_kerry(home.home());
    write_lines(
        &store.join("a.jsonl"),
        &[
            user("2026-09-21T12:00:00.000Z", "a"),
            assistant("2026-09-21T12:00:20.000Z", "a", "msg_a", 2000),
        ],
    );
    let mut p = Producer::new();
    let mut t = ms("2026-09-21T12:01:00.000Z");
    while t < ms("2026-09-21T23:59:56.000Z") {
        p.tick(t);
        t += 600_000;
    }
    write_lines(
        &store.join("t.jsonl"),
        &[
            user("2026-09-21T23:59:52.000Z", "t"),
            assistant("2026-09-21T23:59:55.000Z", "t", "msg_t", 30),
        ],
    );
    let mut t = ms("2026-09-21T23:59:56.000Z");
    while t < ms("2026-09-23T00:05:00.000Z") {
        p.tick(t);
        t += 600_000;
    }
    p.tick(ms("2026-09-23T00:05:00.000Z"));
    assert_eq!(ledger_day_output(home.home(), "2026-09-21"), 2030, "folded");
    p.tick(ms("2026-09-22T23:50:00.000Z"));
    append_lines(
        &store.join("t.jsonl"),
        &[
            user("2026-09-21T23:59:56.000Z", "t"),
            assistant("2026-09-21T23:59:58.000Z", "t", "msg_late", 7),
        ],
    );
    let mut t = ms("2026-09-22T23:51:00.000Z");
    while t <= ms("2026-09-23T00:10:01.000Z") {
        p.tick(t);
        t += 60_000;
    }
    assert_eq!(ledger_day_output(home.home(), "2026-09-21"), 2037, "merged");
    let disk: serde_json::Value = serde_json::from_slice(
        &std::fs::read(home.home().join(".clauth").join("token_speed.json")).unwrap(),
    )
    .unwrap();
    close(
        disk["rate_peaks"]["2026-09-21"]["total"]["value"]
            .as_f64()
            .unwrap_or(f64::NAN),
        100.0,
        "stored D-1 rate peak",
    );
}

/// The crash shape: the ledger saved with the fold applied while the owed-day
/// list still names the day (a crash between the save and the next
/// `tps_days.json` write). A restart rebuilds the day and folds it again by
/// keep-larger: the stored sums are unchanged, never doubled.
#[test]
fn a_restart_after_a_crash_between_the_two_writes_never_doubles_a_stored_day() {
    let home = HomeSandbox::new();
    let clauth = home.home().join(".clauth");
    std::fs::create_dir_all(&clauth).unwrap();
    let day = "2026-09-20";
    let cell = DaySum {
        output: 2000,
        gen_ms: 20_000,
        speed: Some(PeakAt {
            value: 100.0,
            at: ms("2026-09-20T12:00:20.000Z"),
        }),
    };
    let accounts: BTreeMap<String, BTreeMap<String, DaySum>> = BTreeMap::from([(
        "kerry".to_owned(),
        BTreeMap::from([(MODEL.to_owned(), cell)]),
    )]);
    let mut days = DaySums::new();
    days.insert(day.to_owned(), accounts.clone());
    // The owed-day list still names the day.
    day_transcript(home.home(), day, 2000, 20_000);
    write_owed(home.home(), &[day]);
    // The ledger already saved the fold (the crash happened after the save).
    std::fs::write(
        clauth.join("token_speed.json"),
        serde_json::json!({
            "version": 2,
            "days": days,
        })
        .to_string(),
    )
    .unwrap();
    let mut p = Producer::new();
    p.tick(ms("2026-09-22T00:01:00.000Z"));
    let speed = crate::token_ledger::SpeedLedger::load(&clauth);
    assert_eq!(speed.days[day]["kerry"][MODEL].output, 2000, "not doubled");
    assert_eq!(speed.days[day]["kerry"][MODEL].gen_ms, 20_000);
}

/// A v1-ledger day (sums, no rate peaks) gains its rate peaks by the fold: the
/// stored sums keep the larger state (the rebuilt day's smaller 1000 never
/// replaces or adds to the stored 2000) and the rebuilt day's rate peak (1000
/// tokens inside one 10 s window: 100 tok/s) appears.
#[test]
fn a_v1_ledger_day_gains_rate_peaks_by_keep_larger() {
    let home = HomeSandbox::new();
    let clauth = home.home().join(".clauth");
    std::fs::create_dir_all(&clauth).unwrap();
    std::fs::write(
        clauth.join("token_speed.json"),
        r#"{"version":1,"days":{"2026-09-20":{"kerry":{"claude-opus-5-5":{"output":2000,"gen_ms":20000}}}}}"#,
    )
    .unwrap();
    day_transcript(home.home(), "2026-09-20", 1000, 3);
    write_owed(home.home(), &["2026-09-20"]);
    let mut p = Producer::new();
    p.tick(ms("2026-09-22T00:01:00.000Z"));
    let speed = crate::token_ledger::SpeedLedger::load(&clauth);
    assert_eq!(
        speed.days["2026-09-20"]["kerry"][MODEL].output, 2000,
        "the larger state stays"
    );
    assert_eq!(speed.days["2026-09-20"]["kerry"][MODEL].gen_ms, 20_000);
    close(
        speed.rate_peaks["2026-09-20"].total.unwrap().value,
        100.0,
        "rate peak gained",
    );
}

/// A day whose ledger save fails stays held in the cache; a clock step back
/// makes it yesterday again and a late line (7) grows it before the save
/// lands. The ledger ends with the day once: 2000 + 7.
#[test]
fn a_held_day_reopened_by_a_step_back_lands_once() {
    let home = HomeSandbox::new();
    reg(None);
    let store = iso_kerry(home.home());
    write_lines(
        &store.join("a.jsonl"),
        &[
            user("2026-09-21T12:00:00.000Z", "a"),
            assistant("2026-09-21T12:00:20.000Z", "a", "msg_a", 2000),
        ],
    );
    let mut p = Producer::new();
    p.tick(ms("2026-09-21T12:01:00.000Z"));
    block_ledger_saves(home.home());
    let held = p.tick(ms("2026-09-23T00:05:00.000Z"));
    assert_eq!(
        snap_day_output(&held, "2026-09-21"),
        2000,
        "the refused fold keeps the day in the cache"
    );
    p.tick(ms("2026-09-22T23:50:00.000Z"));
    append_lines(
        &store.join("a.jsonl"),
        &[
            user("2026-09-21T23:59:56.000Z", "a"),
            assistant("2026-09-21T23:59:58.000Z", "a", "msg_late", 7),
        ],
    );
    p.tick(ms("2026-09-22T23:51:00.000Z"));
    std::fs::remove_dir_all(home.home().join(".clauth/token_speed.json")).unwrap();
    p.tick(ms("2026-09-23T00:05:00.000Z"));
    assert_eq!(
        ledger_day_output(home.home(), "2026-09-21"),
        2007,
        "the held day lands once"
    );
}

/// The output one day of a snapshot holds, summed over its accounts and models.
fn snap_day_output(s: &Snapshot, day: &str) -> u64 {
    let wire = serde_json::to_value(s).unwrap();
    let mut total = 0;
    for models in wire["days"][day]
        .as_object()
        .into_iter()
        .flat_map(|a| a.values())
    {
        for cell in models.as_object().into_iter().flat_map(|m| m.values()) {
            total += cell["output"].as_u64().unwrap_or(0);
        }
    }
    total
}

/// Make every `token_speed.json` save fail on every OS while reads of the
/// producer's in-memory copy go on: a non-empty directory at the ledger path
/// refuses the save's rename. The producer writes the ledger at its first
/// tick, so a file there goes first.
fn block_ledger_saves(home: &Path) {
    let ledger = home.join(".clauth/token_speed.json");
    if ledger.is_file() {
        std::fs::remove_file(&ledger).unwrap();
    }
    std::fs::create_dir_all(ledger.join("blocker")).unwrap();
}

/// The fixture of a forward clock error: A (2000 over 20 s on D = 09-21) is
/// read, the clock jumps to 09-24 and folds D early, then the clock is
/// corrected back to D 12:10.
fn forward_error_fixture(home: &Path) -> (PathBuf, Producer) {
    let path = iso_kerry(home).join("a.jsonl");
    write_lines(
        &path,
        &[
            user("2026-09-21T12:00:00.000Z", "a"),
            assistant("2026-09-21T12:00:20.000Z", "a", "msg_a", 2000),
        ],
    );
    let mut p = Producer::new();
    p.tick(ms("2026-09-21T12:01:00.000Z"));
    p.tick(ms("2026-09-24T00:05:00.000Z"));
    assert_eq!(
        ledger_day_output(home, "2026-09-21"),
        2000,
        "D folded early"
    );
    p.tick(ms("2026-09-21T12:10:00.000Z"));
    (path, p)
}

/// Tick hourly from D 12:21 to past D+1's midnight, so D folds for real.
fn tick_through_the_real_fold(p: &mut Producer) {
    let mut t = ms("2026-09-21T12:21:00.000Z");
    while t < ms("2026-09-23T00:05:00.000Z") {
        p.tick(t);
        t += 3_600_000;
    }
    p.tick(ms("2026-09-23T00:05:00.000Z"));
}

/// The cache was written after a later line (500) landed: the restart rebuilds
/// the day from the transcript and counts 2000 + 500 once.
#[test]
fn a_restart_after_a_cache_holding_a_later_line_counts_it_once() {
    let home = HomeSandbox::new();
    reg(None);
    let x = ms(START);
    let path = iso_kerry(home.home()).join("o.jsonl");
    write_lines(
        &path,
        &[
            user(START, "o"),
            assistant(&iso(x + 10_000), "o", "m1", 2000),
        ],
    );
    let mut p1 = Producer::new();
    write_cache(home.home(), &p1.tick(x + 60_000));
    append_lines(
        &path,
        &[
            user(&iso(x + 61_000), "o"),
            assistant(&iso(x + 70_000), "o", "m2", 500),
        ],
    );
    let s2 = p1.tick(x + 71_000);
    assert_eq!(day_total(&s2), 2500, "the live producer");
    write_cache(home.home(), &s2);
    assert_eq!(
        day_total(&Producer::new().tick(x + 72_000)),
        2500,
        "the restarted producer"
    );
}

/// A later line (500) landed and was read, but the cache write after it
/// failed: the restart counts the 500 the cache never held.
#[test]
fn a_restart_after_a_cache_missing_a_later_line_counts_it() {
    let home = HomeSandbox::new();
    reg(None);
    let x = ms(START);
    let path = iso_kerry(home.home()).join("o.jsonl");
    write_lines(
        &path,
        &[
            user(START, "o"),
            assistant(&iso(x + 10_000), "o", "m1", 2000),
        ],
    );
    let mut p1 = Producer::new();
    write_cache(home.home(), &p1.tick(x + 60_000));
    append_lines(
        &path,
        &[
            user(&iso(x + 61_000), "o"),
            assistant(&iso(x + 70_000), "o", "m2", 500),
        ],
    );
    p1.tick(x + 71_000);
    assert_eq!(
        day_total(&Producer::new().tick(x + 72_000)),
        2500,
        "the restarted producer"
    );
}

/// After a forward clock error folded D early, B (4000 over 20 s: 200 tok/s)
/// lands on D: today's rate peak is B's 200, never the stored 100 of A.
#[test]
fn todays_rate_peak_after_an_early_fold_takes_the_max() {
    let home = HomeSandbox::new();
    reg(None);
    let (path, mut p) = forward_error_fixture(home.home());
    append_lines(
        &path,
        &[
            user("2026-09-21T12:20:00.000Z", "a"),
            assistant("2026-09-21T12:20:20.000Z", "a", "msg_b", 4000),
        ],
    );
    let s = p.tick(ms("2026-09-21T12:21:00.000Z"));
    let rate = peaks(&s)["today"]["rate"]["value"].as_f64().unwrap();
    assert!((rate - 200.0).abs() < 1e-6, "today's rate peak {rate}");
}

/// After a forward clock error folded D early (A, 2000), today's token count
/// holds the stored part too: B (4000) lands, today reads
/// 6000, and D's real fold leaves the ledger at 6000, never more.
#[test]
fn todays_sums_after_an_early_fold_include_the_stored_part() {
    let home = HomeSandbox::new();
    reg(None);
    let (path, mut p) = forward_error_fixture(home.home());
    append_lines(
        &path,
        &[
            user("2026-09-21T12:20:00.000Z", "a"),
            assistant("2026-09-21T12:20:20.000Z", "a", "msg_b", 4000),
        ],
    );
    let s = p.tick(ms("2026-09-21T12:21:00.000Z"));
    assert_eq!(snap_day_output(&s, "2026-09-21"), 6000, "today's sums");
    tick_through_the_real_fold(&mut p);
    assert_eq!(
        ledger_day_output(home.home(), "2026-09-21"),
        6000,
        "the ledger after D's real fold"
    );
}

/// A forward clock error makes D's transcript cold under the wrong cutoff;
/// the producer restarts after the correction and B (4000) lands on D. The
/// restart must not count A (2000) again: the ledger ends at 6000.
#[test]
fn a_restart_after_a_forward_clock_error_never_recounts_the_early_fold() {
    let home = HomeSandbox::new();
    reg(None);
    let path = iso_kerry(home.home()).join("a.jsonl");
    write_lines(
        &path,
        &[
            user("2026-09-21T12:00:00.000Z", "a"),
            assistant("2026-09-21T12:00:20.000Z", "a", "msg_a", 2000),
        ],
    );
    crate::testutil::set_mtime(
        &path,
        std::time::UNIX_EPOCH + std::time::Duration::from_millis(ms("2026-09-21T12:00:20.000Z")),
    );
    let mut p1 = Producer::new();
    p1.tick(ms("2026-09-21T12:01:00.000Z"));
    let s = p1.tick(ms("2026-09-24T00:05:00.000Z"));
    assert_eq!(
        ledger_day_output(home.home(), "2026-09-21"),
        2000,
        "D folded early"
    );
    write_cache(home.home(), &s);
    let mut p2 = Producer::new();
    p2.tick(ms("2026-09-21T12:10:00.000Z"));
    append_lines(
        &path,
        &[
            user("2026-09-21T12:20:00.000Z", "a"),
            assistant("2026-09-21T12:20:20.000Z", "a", "msg_b", 4000),
        ],
    );
    tick_through_the_real_fold(&mut p2);
    assert_eq!(
        ledger_day_output(home.home(), "2026-09-21"),
        6000,
        "the ledger after D's real fold"
    );
}

/// A file read on restart, then truncated and rewritten while the producer
/// runs: the truncation means replaced, so every line written after it is new
/// (200 counted before, 400 after: 600).
#[test]
fn a_truncation_after_a_restart_counts_every_new_line() {
    let home = HomeSandbox::new();
    reg(None);
    let x = ms(START);
    let path = iso_kerry(home.home()).join("t.jsonl");
    let long_old = format!("old-{}", "o".repeat(700));
    write_lines(
        &path,
        &[
            user(START, "t"),
            assistant(&iso(x + 10_000), "t", &long_old, 200),
        ],
    );
    let mut p1 = Producer::new();
    write_cache(home.home(), &p1.tick(x + 60_000));
    let mut p2 = Producer::new();
    assert_eq!(day_total(&p2.tick(x + 61_000)), 200, "after the restart");
    std::fs::write(&path, "").unwrap();
    append_lines(
        &path,
        &[
            user(&iso(x + 62_000), "t"),
            assistant(&iso(x + 70_000), "t", "m2", 400),
        ],
    );
    assert_eq!(day_total(&p2.tick(x + 71_000)), 600, "after the truncation");
}

/// Both stores hold the same relative path with different bytes (2000 in the
/// isolated store, 300 in the global one); the rescue lands the isolated file
/// under a renamed `.rescued-N` name. The restart counts each response once:
/// 2300.
#[test]
fn a_restart_after_a_renaming_rescue_counts_the_moved_transcript_once() {
    let home = HomeSandbox::new();
    reg(None);
    let x = ms(START) + 30 * 60_000;
    let iso_root = home
        .home()
        .join(".clauth/profiles/kerry/runtime-isolated-4242-1/projects");
    write_lines(
        &iso_kerry(home.home()).join("r.jsonl"),
        &[
            user(&iso(x), "r"),
            assistant(&iso(x + 20_000), "r", "msg_r", 2000),
        ],
    );
    write_lines(
        &global_store(home.home()).join("r.jsonl"),
        &[
            user(&iso(x), "r"),
            assistant(&iso(x + 25_000), "r", "msg_g", 300),
        ],
    );
    let mut p1 = Producer::new();
    let s1 = p1.tick(x + 60_000);
    assert_eq!(day_total(&s1), 2300, "before the rescue");
    write_cache(home.home(), &s1);
    let global = home.home().join(".claude/projects");
    assert_eq!(
        crate::sessions::rescue_transcripts(&iso_root, &global).moved,
        1,
        "the rescue moved the transcript"
    );
    assert!(
        std::fs::read_dir(global_store(home.home()))
            .unwrap()
            .flatten()
            .any(|e| e.file_name().to_string_lossy().contains(".rescued-")),
        "the rescue renamed the colliding file"
    );
    assert_eq!(
        day_total(&Producer::new().tick(x + 67_000)),
        2300,
        "the restarted producer"
    );
}

/// Two genuine folds of one day with byte-equal content (same account, model,
/// output, span and end; different message ids): the first (50) folds, a
/// step back reopens the day and the second (50) folds after it. The ledger
/// holds both: 100.
#[test]
fn a_second_fold_with_equal_content_still_adds() {
    let home = HomeSandbox::new();
    reg(None);
    let store = iso_kerry(home.home());
    write_lines(
        &store.join("a.jsonl"),
        &[
            user("2026-09-21T12:00:00.000Z", "a"),
            assistant("2026-09-21T12:00:10.000Z", "a", "msg_1", 50),
        ],
    );
    let mut p = Producer::new();
    p.tick(ms("2026-09-21T12:01:00.000Z"));
    p.tick(ms("2026-09-23T00:05:00.000Z"));
    assert_eq!(
        ledger_day_output(home.home(), "2026-09-21"),
        50,
        "first fold"
    );
    p.tick(ms("2026-09-22T23:50:00.000Z"));
    write_lines(
        &store.join("b.jsonl"),
        &[
            user("2026-09-21T12:00:00.000Z", "b"),
            assistant("2026-09-21T12:00:10.000Z", "b", "msg_2", 50),
        ],
    );
    p.tick(ms("2026-09-22T23:51:00.000Z"));
    p.tick(ms("2026-09-23T00:05:00.000Z"));
    assert_eq!(
        ledger_day_output(home.home(), "2026-09-21"),
        100,
        "second fold"
    );
}

/// A later fold of a day whose speed peak is lower than the stored one never
/// lowers it: A (100 t/s) folds early under a fast clock, B (1000 over 20 s:
/// 50 t/s) folds for real; D's stored speed peak stays 100.
#[test]
fn a_later_fold_never_lowers_a_stored_speed_peak() {
    let home = HomeSandbox::new();
    reg(None);
    let (path, mut p) = forward_error_fixture(home.home());
    append_lines(
        &path,
        &[
            user("2026-09-21T12:20:00.000Z", "a"),
            assistant("2026-09-21T12:20:20.000Z", "a", "msg_b", 1000),
        ],
    );
    tick_through_the_real_fold(&mut p);
    let disk: serde_json::Value = serde_json::from_slice(
        &std::fs::read(home.home().join(".clauth/token_speed.json")).unwrap(),
    )
    .unwrap();
    let v = disk["days"]["2026-09-21"]["kerry"][MODEL]["speed"]["value"]
        .as_f64()
        .unwrap();
    assert!((v - 100.0).abs() < 1e-9, "stored D speed peak {v}");
}

/// A response growing across a restart: its next block line (CC writes a
/// tool-use turn's blocks seconds apart) lands after the restart's read and
/// counts its growth, 200 -> 500.
#[test]
fn a_growth_line_after_a_restart_counts() {
    let home = HomeSandbox::new();
    reg(None);
    let x = ms(START);
    let path = iso_kerry(home.home()).join("g.jsonl");
    write_lines(
        &path,
        &[
            user(START, "g"),
            assistant(&iso(x + 10_000), "g", "msg_g", 200),
        ],
    );
    let mut p1 = Producer::new();
    write_cache(home.home(), &p1.tick(x + 60_000));
    let mut p2 = Producer::new();
    assert_eq!(day_total(&p2.tick(x + 61_000)), 200, "after the restart");
    append_lines(&path, &[assistant(&iso(x + 70_000), "g", "msg_g", 500)]);
    assert_eq!(day_total(&p2.tick(x + 71_000)), 500, "after the growth");
}

/// R spans 23:59:55 -> 00:00:05 (2000 tokens, 200 tok/s). Yesterday's rate
/// peak names an instant of yesterday (its best window ends 23:59:59.999,
/// 100 tok/s); the window ending 00:00:05 (200) belongs to today.
#[test]
fn a_days_rate_peak_never_names_the_next_day() {
    let home = HomeSandbox::new();
    reg(None);
    let store = iso_kerry(home.home());
    write_lines(
        &store.join("m.jsonl"),
        &[
            user("2026-09-21T12:00:00.000Z", "m"),
            assistant("2026-09-21T12:00:20.000Z", "m", "msg_m", 100),
        ],
    );
    write_lines(
        &store.join("r.jsonl"),
        &[
            user("2026-09-21T23:59:55.000Z", "r"),
            assistant("2026-09-22T00:00:05.000Z", "r", "msg_r", 2000),
        ],
    );
    let s = Producer::new().tick(ms("2026-09-22T00:01:00.000Z"));
    let y = s
        .rate_peaks
        .get("2026-09-21")
        .and_then(|d| d.total)
        .unwrap();
    assert_eq!(
        day_of_ms(y.at),
        "2026-09-21",
        "yesterday's rate peak names {}",
        iso(y.at)
    );
}

/// The fold tick itself reads a late D line (7), the ledger save lands, and the
/// process dies before the scheduler's own cache write: the restart finds the
/// fold already applied and leaves D at 2007.
#[test]
fn a_crash_after_the_ledger_save_never_refolds_the_day() {
    let home = HomeSandbox::new();
    reg(None);
    let store = iso_kerry(home.home());
    write_lines(
        &store.join("a.jsonl"),
        &[
            user("2026-09-21T12:00:00.000Z", "a"),
            assistant("2026-09-21T12:00:20.000Z", "a", "msg_a", 2000),
        ],
    );
    let mut p = Producer::new();
    p.tick(ms("2026-09-21T12:01:00.000Z"));
    write_cache(home.home(), &p.tick(ms("2026-09-22T23:50:00.000Z")));
    append_lines(
        &store.join("a.jsonl"),
        &[
            user("2026-09-21T23:59:56.000Z", "a"),
            assistant("2026-09-21T23:59:58.000Z", "a", "msg_late", 7),
        ],
    );
    p.tick(ms("2026-09-23T00:05:00.000Z"));
    assert_eq!(ledger_day_output(home.home(), "2026-09-21"), 2007, "saved");
    drop(p);
    Producer::new().tick(ms("2026-09-23T00:06:00.000Z"));
    assert_eq!(
        ledger_day_output(home.home(), "2026-09-21"),
        2007,
        "after the restart"
    );
}

// ── consecutive folds; a restart never refolds today's stored part ──

/// Two consecutive days fold each into its own ledger day: D-2 folds, then D-1
/// a day later, and every refold leaves both as they landed.
#[test]
fn consecutive_days_each_fold_into_their_own_ledger_day() {
    let home = HomeSandbox::new();
    reg(None);
    let store = iso_kerry(home.home());
    write_lines(
        &store.join("a.jsonl"),
        &[
            user("2026-09-20T12:00:00.000Z", "a"),
            assistant("2026-09-20T12:00:20.000Z", "a", "msg_a", 2000),
        ],
    );
    write_lines(
        &store.join("b.jsonl"),
        &[
            user("2026-09-21T12:00:00.000Z", "b"),
            assistant("2026-09-21T12:00:20.000Z", "b", "msg_b", 1000),
        ],
    );
    let mut p = Producer::new();
    p.tick(ms("2026-09-21T12:01:00.000Z"));
    p.tick(ms("2026-09-22T00:05:00.000Z"));
    p.tick(ms("2026-09-23T00:05:00.000Z"));
    assert_eq!(ledger_day_output(home.home(), "2026-09-20"), 2000);
    assert_eq!(ledger_day_output(home.home(), "2026-09-21"), 1000);
}

/// A restart from a cache whose today holds the ledger's stored part (a fold
/// under a fast clock) subtracts that part: A (2000) is stored early, B (4000)
/// lands, the cache is written at D 12:21 with today at 6000, and after a
/// restart D's real fold leaves the ledger at 6000, never 8000.
#[test]
fn a_restart_never_folds_todays_stored_part_again() {
    let home = HomeSandbox::new();
    reg(None);
    let (path, mut p) = forward_error_fixture(home.home());
    append_lines(
        &path,
        &[
            user("2026-09-21T12:20:00.000Z", "a"),
            assistant("2026-09-21T12:20:20.000Z", "a", "msg_b", 4000),
        ],
    );
    let t = ms("2026-09-21T12:21:00.000Z");
    write_cache(home.home(), &p.tick(t));
    drop(p);
    let mut p2 = Producer::new();
    tick_through_the_real_fold(&mut p2);
    assert_eq!(
        ledger_day_output(home.home(), "2026-09-21"),
        6000,
        "the ledger after D's real fold"
    );
}

/// The clock steps back 30 s, a line (500) lands, and the clock reaches the
/// first write's ms again: a second, different cache carries the same
/// `generated_at`, and a crash loses its offsets write. The restart must not
/// pair that cache with the first write's offsets: 2000 + 500, once.
#[test]
fn a_cache_rewritten_on_the_same_ms_never_pairs_with_older_offsets() {
    let home = HomeSandbox::new();
    reg(None);
    let x = ms(START);
    let path = iso_kerry(home.home()).join("o.jsonl");
    write_lines(
        &path,
        &[
            user(START, "o"),
            assistant(&iso(x + 10_000), "o", "m1", 2000),
        ],
    );
    let t = x + 60_000;
    let mut p1 = Producer::new();
    write_cache(home.home(), &p1.tick(t));
    p1.tick(t - 30_000);
    append_lines(
        &path,
        &[
            user(&iso(x + 31_000), "o"),
            assistant(&iso(x + 40_000), "o", "m2", 500),
        ],
    );
    let s2 = p1.tick(t);
    assert_eq!(day_total(&s2), 2500, "the live producer");
    write_cache(home.home(), &s2);
    assert_eq!(
        day_total(&Producer::new().tick(t + 1_000)),
        2500,
        "the restarted producer"
    );
}

/// D folded and left the window; the restart finds the ledger unreadable, a
/// line (7) stamped on D lands, then the ledger reads again. One box's clock
/// cannot write a line on a folded, out-of-window day, so it counts nowhere
/// D stays 2000 and no live day holds the 7.
#[cfg(unix)]
#[test]
fn a_line_on_a_folded_day_outside_the_window_counts_nowhere() {
    use std::os::unix::fs::PermissionsExt;
    let home = HomeSandbox::new();
    reg(None);
    let store = iso_kerry(home.home());
    write_lines(
        &store.join("a.jsonl"),
        &[
            user("2026-09-21T12:00:00.000Z", "a"),
            assistant("2026-09-21T12:00:20.000Z", "a", "msg_a", 2000),
        ],
    );
    let mut p = Producer::new();
    p.tick(ms("2026-09-21T12:01:00.000Z"));
    p.tick(ms("2026-09-23T00:05:00.000Z"));
    assert_eq!(ledger_day_output(home.home(), "2026-09-21"), 2000, "saved");
    drop(p);
    let ledger = home.home().join(".clauth/token_speed.json");
    std::fs::set_permissions(&ledger, std::fs::Permissions::from_mode(0o000)).unwrap();
    let mut p2 = Producer::new();
    p2.tick(ms("2026-09-23T00:06:00.000Z"));
    append_lines(
        &store.join("a.jsonl"),
        &[
            user("2026-09-21T23:59:56.000Z", "a"),
            assistant("2026-09-21T23:59:58.000Z", "a", "msg_late", 7),
        ],
    );
    p2.tick(ms("2026-09-23T00:07:00.000Z"));
    std::fs::set_permissions(&ledger, std::fs::Permissions::from_mode(0o600)).unwrap();
    p2.tick(ms("2026-09-23T00:08:00.000Z"));
    let last = p2.tick(ms("2026-09-23T00:09:00.000Z"));
    assert_eq!(
        ledger_day_output(home.home(), "2026-09-21"),
        2000,
        "the folded day"
    );
    assert_eq!(day_total(&last), 0, "a live day holds the line");
}

/// A restart under a forward clock error folds D early from the cache; a
/// restart after the correction, then B (4000) lands on D. A is never counted
/// again: the ledger ends at 6000.
#[test]
fn a_restart_during_a_forward_clock_error_never_recounts_the_early_fold() {
    let home = HomeSandbox::new();
    reg(None);
    let path = iso_kerry(home.home()).join("a.jsonl");
    write_lines(
        &path,
        &[
            user("2026-09-21T12:00:00.000Z", "a"),
            assistant("2026-09-21T12:00:20.000Z", "a", "msg_a", 2000),
        ],
    );
    crate::testutil::set_mtime(
        &path,
        std::time::UNIX_EPOCH + std::time::Duration::from_millis(ms("2026-09-21T12:00:20.000Z")),
    );
    let t1 = ms("2026-09-21T12:01:00.000Z");
    let mut p1 = Producer::new();
    write_cache(home.home(), &p1.tick(t1 + 60_000));
    drop(p1);
    let t2 = ms("2026-09-24T00:05:00.000Z");
    let mut p2 = Producer::new();
    let s2 = p2.tick(t2);
    assert_eq!(
        ledger_day_output(home.home(), "2026-09-21"),
        2000,
        "D folded early"
    );
    write_cache(home.home(), &s2);
    drop(p2);
    let mut p3 = Producer::new();
    p3.tick(ms("2026-09-21T12:10:00.000Z"));
    append_lines(
        &path,
        &[
            user("2026-09-21T12:20:00.000Z", "a"),
            assistant("2026-09-21T12:20:20.000Z", "a", "msg_b", 4000),
        ],
    );
    tick_through_the_real_fold(&mut p3);
    assert_eq!(
        ledger_day_output(home.home(), "2026-09-21"),
        6000,
        "the ledger after D's real fold"
    );
}

/// A file truncated and rewritten in place, past its read offset, between two
/// ticks: it is replaced, so every rewritten line counts (200 before, then
/// 400 + 50: 650).
#[test]
fn a_file_rewritten_past_its_offset_between_two_ticks_counts_every_line() {
    let home = HomeSandbox::new();
    reg(None);
    let x = ms(START);
    let path = iso_kerry(home.home()).join("t.jsonl");
    write_lines(
        &path,
        &[
            user(START, "t"),
            assistant(&iso(x + 10_000), "t", "m1", 200),
        ],
    );
    let mut p = Producer::new();
    assert_eq!(day_total(&p.tick(x + 60_000)), 200, "before the rewrite");
    let pad = format!(
        "pad-{}",
        "p".repeat(std::fs::metadata(&path).unwrap().len() as usize)
    );
    write_lines(
        &path,
        &[
            user(&iso(x + 61_000), "t"),
            assistant(&iso(x + 62_000), "t", &pad, 400),
            user(&iso(x + 63_000), "t"),
            assistant(&iso(x + 64_000), "t", "m3", 50),
        ],
    );
    assert_eq!(day_total(&p.tick(x + 65_000)), 650, "after the rewrite");
}

/// A file replaced by a rename (a new file at the same path), longer than the
/// read offset, between two ticks: every line of the new file counts (200
/// before, then 400 + 50: 650).
#[test]
fn a_file_replaced_by_rename_between_two_ticks_counts_every_line() {
    let home = HomeSandbox::new();
    reg(None);
    let x = ms(START);
    let path = iso_kerry(home.home()).join("u.jsonl");
    write_lines(
        &path,
        &[
            user(START, "u"),
            assistant(&iso(x + 10_000), "u", "n1", 200),
        ],
    );
    let mut p = Producer::new();
    assert_eq!(day_total(&p.tick(x + 60_000)), 200, "before the rename");
    let tmp = iso_kerry(home.home()).join("u.tmp");
    let pad = format!(
        "pad-{}",
        "q".repeat(std::fs::metadata(&path).unwrap().len() as usize)
    );
    write_lines(
        &tmp,
        &[
            user(&iso(x + 61_000), "u"),
            assistant(&iso(x + 62_000), "u", &pad, 400),
            user(&iso(x + 63_000), "u"),
            assistant(&iso(x + 64_000), "u", "n3", 50),
        ],
    );
    std::fs::rename(&tmp, &path).unwrap();
    assert_eq!(day_total(&p.tick(x + 66_000)), 650, "after the rename");
}

/// The global store's `r.jsonl` (300) is a byte prefix of the isolated one's
/// (300 + 2000); the rescue lands the isolated copy as `.rescued-N`. Every
/// restart counts each response once: 2300. Which entry a first-match rule
/// takes follows hash order, so the fixture runs 12 times (a 5-in-12 double
/// count reds at least once in 12 with probability 1 - (7/12)^12 > 0.998).
#[test]
fn a_rescued_copy_extending_its_sibling_counts_once_on_every_restart() {
    for run in 0..12 {
        let home = HomeSandbox::new();
        reg(None);
        let x = ms(START) + 30 * 60_000;
        let iso_root = home
            .home()
            .join(".clauth/profiles/kerry/runtime-isolated-4242-1/projects");
        let global = [
            user(&iso(x), "r"),
            assistant(&iso(x + 20_000), "r", "msg_g", 300),
        ];
        write_lines(&global_store(home.home()).join("r.jsonl"), &global);
        let mut isolated = global.to_vec();
        isolated.push(user(&iso(x + 30_000), "r"));
        isolated.push(assistant(&iso(x + 50_000), "r", "msg_r", 2000));
        write_lines(&iso_kerry(home.home()).join("r.jsonl"), &isolated);
        let mut p1 = Producer::new();
        let s1 = p1.tick(x + 60_000);
        assert_eq!(day_total(&s1), 2300, "run {run}: before the rescue");
        write_cache(home.home(), &s1);
        drop(p1);
        crate::sessions::rescue_transcripts(&iso_root, &home.home().join(".claude/projects"));
        let s2 = Producer::new().tick(x + 67_000);
        assert_eq!(day_total(&s2), 2300, "run {run}: the restarted producer");
        assert_eq!(
            serde_json::to_value(&s2.days).unwrap(),
            serde_json::to_value(&s1.days).unwrap(),
            "run {run}: the rescue moved a response to another account"
        );
    }
}

/// D's fold lands and the process dies before the scheduler's write; the
/// restart runs under a clock stepped back so D is live again, and a late line
/// (7) grows D. The restart never folds the applied part again: D = 2007.
#[test]
fn a_restart_under_a_step_back_after_a_landed_fold_adds_only_the_late_line() {
    let home = HomeSandbox::new();
    reg(None);
    let store = iso_kerry(home.home());
    write_lines(
        &store.join("a.jsonl"),
        &[
            user("2026-09-21T12:00:00.000Z", "a"),
            assistant("2026-09-21T12:00:20.000Z", "a", "msg_a", 2000),
        ],
    );
    let mut p = Producer::new();
    p.tick(ms("2026-09-21T12:01:00.000Z"));
    p.tick(ms("2026-09-23T00:05:00.000Z"));
    assert_eq!(ledger_day_output(home.home(), "2026-09-21"), 2000, "saved");
    drop(p);
    let mut p2 = Producer::new();
    p2.tick(ms("2026-09-22T23:50:00.000Z"));
    append_lines(
        &store.join("a.jsonl"),
        &[
            user("2026-09-21T23:59:56.000Z", "a"),
            assistant("2026-09-21T23:59:58.000Z", "a", "msg_late", 7),
        ],
    );
    p2.tick(ms("2026-09-22T23:51:00.000Z"));
    p2.tick(ms("2026-09-23T00:05:00.000Z"));
    assert_eq!(
        ledger_day_output(home.home(), "2026-09-21"),
        2007,
        "the late line only"
    );
}

// ── swaps, the larger-cell fold, the attribution timeline, the owed-day list ──

/// A swap made while no producer ran, on a session the previous producer
/// already attributed: the restart splits its responses at the swap's
/// recorded instant (the live row's `last_swap_at`), kerry 1000 before it and
/// mommy 500 after it, from the restart's first tick.
#[test]
fn a_swap_while_no_producer_ran_splits_a_known_session_at_its_instant() {
    let home = HomeSandbox::new();
    reg(None);
    let path = iso_kerry(home.home()).join("dt.jsonl");
    write_lines(
        &path,
        &[
            user("2026-09-21T11:50:00.000Z", "dt"),
            assistant("2026-09-21T11:50:20.000Z", "dt", "msg_pre", 1000),
        ],
    );
    Producer::new().tick(ms("2026-09-21T11:51:00.000Z"));
    crate::live_sessions::register(&crate::live_sessions::LiveSession {
        isolated: true,
        current_member: Some("mommy".to_owned()),
        last_swap_at: Some(ms("2026-09-21T12:00:00.000Z")),
        ..crate::testutil::live_row("4242-1", "kerry")
    })
    .unwrap();
    append_lines(
        &path,
        &[
            user("2026-09-21T12:05:00.000Z", "dt"),
            assistant("2026-09-21T12:05:20.000Z", "dt", "msg_post", 500),
        ],
    );
    let wire = serde_json::to_value(Producer::new().tick(ms("2026-09-21T12:06:00.000Z"))).unwrap();
    let day = &wire["days"]["2026-09-21"];
    assert_eq!(day["kerry"][MODEL]["output"], 1000, "kerry: {day}");
    assert_eq!(day["mommy"][MODEL]["output"], 500, "mommy: {day}");
}

/// The fold keeps the larger model total, comparing `(output, gen_ms)` in
/// that order: on 09-19 an equal output over a longer span
/// replaces the stored cell; on 09-20 a smaller output never does, whatever
/// its span. Both days are owed and rebuilt from their transcripts.
#[test]
fn a_fold_keeps_the_larger_cell_by_output_then_span() {
    let home = HomeSandbox::new();
    let clauth = home.home().join(".clauth");
    std::fs::create_dir_all(&clauth).unwrap();
    std::fs::write(
        clauth.join("token_speed.json"),
        r#"{"version":2,"days":{"2026-09-19":{"kerry":{"claude-opus-5-5":{"output":1000,"gen_ms":1670}}},"2026-09-20":{"kerry":{"claude-opus-5-5":{"output":1000,"gen_ms":10000}}}}}"#,
    )
    .unwrap();
    day_transcript(home.home(), "2026-09-19", 1000, 10_000);
    day_transcript(home.home(), "2026-09-20", 900, 20_000);
    write_owed(home.home(), &["2026-09-19", "2026-09-20"]);
    Producer::new().tick(ms("2026-09-22T00:01:00.000Z"));
    let disk: serde_json::Value =
        serde_json::from_slice(&std::fs::read(clauth.join("token_speed.json")).unwrap()).unwrap();
    let state = |day: &str| {
        let cell = &disk["days"][day]["kerry"][MODEL];
        (cell["output"].as_u64(), cell["gen_ms"].as_u64())
    };
    assert_eq!(
        state("2026-09-19"),
        (Some(1000), Some(10_000)),
        "an equal output over a longer span replaces: {disk}"
    );
    assert_eq!(
        state("2026-09-20"),
        (Some(1000), Some(10_000)),
        "a smaller output never replaces: {disk}"
    );
}

/// The attribution timeline keeps at most its cap of entries and evicts the
/// oldest-added first, never by clock: the first entry added carries the
/// largest `since` and still goes first; the second stays.
#[test]
fn the_attribution_timeline_evicts_its_oldest_added_entry_past_the_cap() {
    let mut timeline = Timeline::default();
    for i in 0..=ATTRIBUTION_CAP {
        timeline.push(TimelineEntry {
            session: format!("s{i}"),
            since: (ATTRIBUTION_CAP - i) as u64,
            member: "kerry".to_owned(),
        });
    }
    assert_eq!(timeline.order.len(), ATTRIBUTION_CAP, "capped");
    assert!(
        !timeline.by_session.contains_key("s0"),
        "the oldest-added entry is evicted"
    );
    assert!(timeline.by_session.contains_key("s1"), "the next one stays");
    assert!(
        timeline
            .by_session
            .contains_key(&format!("s{ATTRIBUTION_CAP}")),
        "the newest stays"
    );
}

/// A swap whose live row lands ticks late (stamped t0+1 s, seen at t0+5 s)
/// still moves the turn spent after its stamp (read at t0+4 s, under kerry
/// then) onto mommy, live and after a restart: the swap sits at the first held
/// response spent after its stamp, however many ticks ago it was read.
#[test]
fn a_swap_observed_ticks_late_moves_the_turns_spent_after_its_stamp() {
    let home = HomeSandbox::new();
    reg(None);
    let t0 = ms("2026-09-21T12:00:00.000Z");
    let path = iso_kerry(home.home()).join("late.jsonl");
    write_lines(
        &path,
        &[
            user(&iso(t0 - 60_000), "late"),
            assistant(&iso(t0 - 50_000), "late", "msg_a", 1000),
        ],
    );
    let mut p = Producer::new();
    p.tick(t0);
    append_lines(
        &path,
        &[
            user(&iso(t0 + 2_000), "late"),
            assistant(&iso(t0 + 3_000), "late", "msg_b", 400),
        ],
    );
    p.tick(t0 + 4_000);
    reg_swapped("mommy", t0 + 1_000);
    let live = p.tick(t0 + 5_000);
    drop(p);
    let restarted = Producer::new().tick(t0 + 6_000);
    for (what, snap) in [("live", &live), ("restarted", &restarted)] {
        assert_eq!(
            snap_account_day(snap, "2026-09-21", "kerry"),
            1000,
            "{what}: kerry"
        );
        assert_eq!(
            snap_account_day(snap, "2026-09-21", "mommy"),
            400,
            "{what}: mommy"
        );
    }
}

/// D's turn (2000) under an attribution file this life cannot read: the file
/// is never saved over and D's fold waits after D left the window; once the
/// file reads again the producer takes it back, has nothing new to save, and
/// D folds.
#[cfg(unix)]
#[test]
fn an_unreadable_attribution_file_holds_the_fold_until_it_reads_again() {
    use std::os::unix::fs::PermissionsExt;
    let home = HomeSandbox::new();
    reg(None);
    write_lines(
        &iso_kerry(home.home()).join("u.jsonl"),
        &[
            user("2026-09-21T12:00:00.000Z", "u"),
            assistant("2026-09-21T12:00:20.000Z", "u", "msg_u", 2000),
            user("2026-09-21T12:05:00.000Z", "u"),
        ],
    );
    Producer::new().tick(ms("2026-09-21T12:06:00.000Z"));
    let tl = home.home().join(".clauth/tps_attribution.json");
    let saved = std::fs::read(&tl).unwrap();
    std::fs::set_permissions(&tl, std::fs::Permissions::from_mode(0o000)).unwrap();
    let mut p = Producer::new();
    p.tick(ms("2026-09-22T00:00:10.000Z"));
    p.tick(ms("2026-09-23T00:00:10.000Z"));
    let ledger = home.home().join(".clauth/token_speed.json");
    assert!(
        !ledger.exists() || ledger_day_output(home.home(), "2026-09-21") == 0,
        "D folded while the attribution file was unreadable"
    );
    std::fs::set_permissions(&tl, std::fs::Permissions::from_mode(0o600)).unwrap();
    p.tick(ms("2026-09-23T00:00:20.000Z"));
    assert_eq!(
        ledger_day_output(home.home(), "2026-09-21"),
        2000,
        "D folded once the file read"
    );
    assert_eq!(
        std::fs::read(&tl).unwrap(),
        saved,
        "the file kept its bytes"
    );
}

/// An attribution file from a newer clauth is never saved over, and while
/// this life holds an entry it could not save, D's turn never folds, not even
/// after D left the window.
#[test]
fn a_newer_attribution_file_is_never_saved_over_and_holds_the_fold() {
    let home = HomeSandbox::new();
    reg(None);
    write_lines(
        &iso_kerry(home.home()).join("n.jsonl"),
        &[
            user("2026-09-21T12:00:00.000Z", "n"),
            assistant("2026-09-21T12:00:20.000Z", "n", "msg_n", 2000),
            user("2026-09-21T12:05:00.000Z", "n"),
        ],
    );
    let clauth = home.home().join(".clauth");
    std::fs::create_dir_all(&clauth).unwrap();
    let tl = clauth.join("tps_attribution.json");
    let newer = br#"{"version":99,"entries":[]}"#;
    std::fs::write(&tl, newer).unwrap();
    let mut p = Producer::new();
    p.tick(ms("2026-09-21T12:06:00.000Z"));
    p.tick(ms("2026-09-22T00:00:10.000Z"));
    p.tick(ms("2026-09-23T00:00:10.000Z"));
    let ledger = clauth.join("token_speed.json");
    assert!(
        !ledger.exists() || ledger_day_output(home.home(), "2026-09-21") == 0,
        "D folded while the timeline could not be saved"
    );
    assert_eq!(
        std::fs::read(&tl).unwrap(),
        newer,
        "saved over a newer file"
    );
}

/// One kerry transcript with a finished turn, so a pass over it owes a day and
/// holds an unsaved timeline entry.
fn write_kerry_turn(home: &HomeSandbox) {
    reg(None);
    write_lines(
        &iso_kerry(home.home()).join("l.jsonl"),
        &[
            user("2026-09-21T12:00:00.000Z", "l"),
            assistant("2026-09-21T12:00:20.000Z", "l", "msg_l", 2000),
            user("2026-09-21T12:05:00.000Z", "l"),
        ],
    );
}

/// A pass whose fetch tick lost the lease writes neither the owed-day list nor
/// the attribution timeline nor the speed ledger; the same pass under a current
/// ticket writes the first two.
#[test]
fn a_pass_under_a_spent_ticket_writes_none_of_the_producers_files() {
    let home = HomeSandbox::new();
    write_kerry_turn(&home);
    let clauth = home.home().join(".clauth");
    let lease = crate::daemon::FetchLease::new();
    let spent = crate::testutil::spent_lease_ticket(&lease);

    Producer::new().tick_leased(ms("2026-09-21T12:06:00.000Z"), Some(spent));

    for file in [TPS_DAYS_FILE, TPS_ATTRIBUTION_FILE, "token_speed.json"] {
        assert!(
            !clauth.join(file).exists(),
            "a pass under a spent ticket wrote {file}"
        );
    }

    let current = lease.acquire_ticket().expect("retake the fetch lease");
    Producer::new().tick_leased(ms("2026-09-21T12:06:00.000Z"), Some(current));
    for file in [TPS_DAYS_FILE, TPS_ATTRIBUTION_FILE] {
        assert!(
            clauth.join(file).exists(),
            "control: a pass under a current ticket writes {file}"
        );
    }
}

/// The fold of a day that left the window is a speed-ledger write too: a pass
/// under a spent ticket leaves the day owed, and the same pass under a current
/// ticket folds it.
#[test]
fn a_fold_under_a_spent_ticket_writes_no_speed_ledger() {
    let home = HomeSandbox::new();
    write_kerry_turn(&home);
    Producer::new().tick(ms("2026-09-21T12:06:00.000Z"));
    let ledger = home.home().join(".clauth/token_speed.json");
    let lease = crate::daemon::FetchLease::new();
    let spent = crate::testutil::spent_lease_ticket(&lease);

    Producer::new().tick_leased(ms("2026-09-23T00:00:10.000Z"), Some(spent));
    assert!(
        !ledger.exists() || ledger_day_output(home.home(), "2026-09-21") == 0,
        "a day folded under a spent ticket wrote the ledger"
    );

    let current = lease.acquire_ticket().expect("retake the fetch lease");
    Producer::new().tick_leased(ms("2026-09-23T00:00:10.000Z"), Some(current));
    assert_eq!(
        ledger_day_output(home.home(), "2026-09-21"),
        2000,
        "control: the same pass under a current ticket folds the day"
    );
}

/// The names of the `tps_days.json.corrupt-*` files beside the owed-day list.
fn days_asides(clauth: &Path) -> Vec<String> {
    std::fs::read_dir(clauth)
        .unwrap()
        .filter_map(|e| e.ok()?.file_name().into_string().ok())
        .filter(|n| n.starts_with("tps_days.json.corrupt-"))
        .collect()
}

/// `tps_days.json` as JSON.
fn owed_file(clauth: &Path) -> serde_json::Value {
    serde_json::from_slice(&std::fs::read(clauth.join(TPS_DAYS_FILE)).unwrap()).unwrap()
}

/// An owed-day list that does not parse is renamed aside once, never saved
/// over: two saves later (the list changes at midnight) there is one aside
/// copy holding the old bytes, and the new file (owner-only, version 1) names
/// the owed days and holds nothing else.
#[test]
fn an_unparseable_owed_day_list_is_renamed_aside_once() {
    let home = HomeSandbox::new();
    reg(None);
    let clauth = home.home().join(".clauth");
    std::fs::create_dir_all(&clauth).unwrap();
    std::fs::write(clauth.join(TPS_DAYS_FILE), b"{not json").unwrap();
    write_lines(
        &iso_kerry(home.home()).join("d.jsonl"),
        &[user(START, "d"), assistant(END, "d", "msg_d", 1222)],
    );
    let mut p = Producer::new();
    p.tick(ms(END) + 1_000);
    assert_eq!(
        owed_file(&clauth),
        serde_json::json!({"version": 1, "owed": ["2026-09-21", "2026-09-22"]}),
        "the first save"
    );
    p.tick(ms("2026-09-23T00:00:10.000Z"));
    let aside = days_asides(&clauth);
    assert_eq!(aside.len(), 1, "renamed aside once: {aside:?}");
    assert_eq!(
        std::fs::read(clauth.join(&aside[0])).unwrap(),
        b"{not json",
        "the aside copy keeps the bytes"
    );
    assert_eq!(
        owed_file(&clauth),
        serde_json::json!({"version": 1, "owed": ["2026-09-22", "2026-09-23"]}),
        "the list after midnight"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(clauth.join(TPS_DAYS_FILE))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "owner-only");
    }
}

/// An owed-day list from a newer clauth is never saved over, and never read:
/// the day it names (09-10, its transcript present) is never rebuilt.
#[test]
fn a_newer_owed_day_list_is_never_saved_over_or_read() {
    let home = HomeSandbox::new();
    reg(None);
    let clauth = home.home().join(".clauth");
    std::fs::create_dir_all(&clauth).unwrap();
    let newer = br#"{"version":99,"owed":["2026-09-10"]}"#;
    std::fs::write(clauth.join(TPS_DAYS_FILE), newer).unwrap();
    day_transcript(home.home(), "2026-09-10", 5, 6);
    write_lines(
        &iso_kerry(home.home()).join("n.jsonl"),
        &[user(START, "n"), assistant(END, "n", "msg_n", 1222)],
    );
    let mut p = Producer::new();
    p.tick(ms(END) + 1_000);
    p.tick(ms("2026-09-23T00:00:10.000Z"));
    assert_eq!(
        std::fs::read(clauth.join(TPS_DAYS_FILE)).unwrap(),
        newer,
        "saved over a newer file"
    );
    assert_eq!(
        ledger_day_total(home.home(), "2026-09-10"),
        0,
        "a newer file's day folded"
    );
}

/// An owed-day list this life cannot read is never saved over; once it
/// reads, its past day (7 on 09-19) is rebuilt from its transcript and folds,
/// and the producer saves its own list.
#[cfg(unix)]
#[test]
fn an_unreadable_owed_day_list_is_taken_back_once_it_reads() {
    use std::os::unix::fs::PermissionsExt;
    if unsafe { libc::geteuid() } == 0 {
        eprintln!("skipped: root ignores permission bits");
        return;
    }
    let home = HomeSandbox::new();
    reg(None);
    let clauth = home.home().join(".clauth");
    day_transcript(home.home(), "2026-09-19", 7, 3);
    write_owed(home.home(), &["2026-09-19"]);
    let file = clauth.join(TPS_DAYS_FILE);
    let stored = std::fs::read(&file).unwrap();
    write_lines(
        &iso_kerry(home.home()).join("r.jsonl"),
        &[user(START, "r"), assistant(END, "r", "msg_r", 1222)],
    );
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o000)).unwrap();
    let mut p = Producer::new();
    p.tick(ms(END) + 1_000);
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(
        std::fs::read(&file).unwrap(),
        stored,
        "saved over an unreadable file"
    );
    assert_eq!(
        ledger_day_total(home.home(), "2026-09-19"),
        0,
        "precondition"
    );
    p.tick(ms(END) + 2_000);
    assert_eq!(
        ledger_day_total(home.home(), "2026-09-19"),
        7,
        "folded once it read"
    );
    assert_eq!(
        owed_file(&clauth),
        serde_json::json!({"version": 1, "owed": ["2026-09-21", "2026-09-22"]}),
        "the producer's own list"
    );
}

/// An owed-day list entry that is not a `YYYY-MM-DD` date (one sorting after
/// every real day) is dropped at load, never kept waiting as a future day.
#[test]
fn an_owed_day_that_is_not_a_date_is_dropped() {
    let home = HomeSandbox::new();
    reg(None);
    let clauth = home.home().join(".clauth");
    write_owed(home.home(), &["2026-13-45"]);
    write_lines(
        &iso_kerry(home.home()).join("nd.jsonl"),
        &[user(START, "nd"), assistant(END, "nd", "msg_nd", 1222)],
    );
    Producer::new().tick(ms(END) + 1_000);
    assert_eq!(
        owed_file(&clauth),
        serde_json::json!({"version": 1, "owed": ["2026-09-21", "2026-09-22"]}),
        "the list after the first save"
    );
}

/// The owed-day list holds no attribution, so it is saved while the
/// attribution timeline cannot be (a directory at its path): the life still
/// leaves its days owed, and the next life folds them from the transcripts.
#[test]
fn the_owed_day_list_is_saved_while_an_attribution_save_is_pending() {
    let home = HomeSandbox::new();
    reg(None);
    let clauth = home.home().join(".clauth");
    let timeline = clauth.join(TPS_ATTRIBUTION_FILE);
    std::fs::create_dir_all(timeline.join("blocker")).unwrap();
    write_lines(
        &iso_kerry(home.home()).join("pd.jsonl"),
        &[user(START, "pd"), assistant(END, "pd", "msg_pd", 1222)],
    );
    Producer::new().tick(ms(END) + 1_000);
    assert!(timeline.is_dir(), "precondition: the timeline never saved");
    assert_eq!(
        owed_file(&clauth),
        serde_json::json!({"version": 1, "owed": ["2026-09-21", "2026-09-22"]}),
        "saved while the attribution save was pending"
    );
    std::fs::remove_dir_all(&timeline).unwrap();
    Producer::new().tick(ms("2026-09-24T00:00:10.000Z"));
    assert_eq!(
        ledger_day_total(home.home(), "2026-09-22"),
        1222,
        "the next life folds the owed day"
    );
}

/// The output one account holds on one day of a snapshot, summed over models.
fn snap_account_day(s: &Snapshot, day: &str, account: &str) -> u64 {
    let wire = serde_json::to_value(s).unwrap();
    wire["days"][day][account]
        .as_object()
        .into_iter()
        .flat_map(|m| m.values())
        .map(|cell| cell["output"].as_u64().unwrap_or(0))
        .sum()
}

/// The isolated kerry session `4242-1`, swapped onto `member` at `at_ms`.
fn reg_swapped(member: &str, at_ms: u64) {
    crate::live_sessions::register(&crate::live_sessions::LiveSession {
        isolated: true,
        current_member: Some(member.to_owned()),
        last_swap_at: Some(at_ms),
        ..crate::testutil::live_row("4242-1", "kerry")
    })
    .unwrap();
}

/// A turn whose first block line (800, the final usage) lands at D 23:59:58
/// and whose last lands at D+1 00:00:05, with a restart between them: it ends
/// on D+1, so it counts 800 once, on D+1.
#[test]
fn a_turn_spanning_midnight_counts_once_on_its_end_day_across_a_restart() {
    let home = HomeSandbox::new();
    reg(None);
    let path = iso_kerry(home.home()).join("mid.jsonl");
    write_lines(
        &path,
        &[
            user("2026-09-21T23:59:50.000Z", "mid"),
            assistant("2026-09-21T23:59:58.000Z", "mid", "msg_mid", 800),
        ],
    );
    let mut p1 = Producer::new();
    write_cache(home.home(), &p1.tick(ms("2026-09-21T23:59:59.000Z")));
    drop(p1);
    append_lines(
        &path,
        &[assistant("2026-09-22T00:00:05.000Z", "mid", "msg_mid", 800)],
    );
    let s = Producer::new().tick(ms("2026-09-22T00:01:00.000Z"));
    assert_eq!(snap_day_output(&s, "2026-09-21"), 0, "D");
    assert_eq!(snap_day_output(&s, "2026-09-22"), 800, "D+1");
}

/// A swap at 12:00 that no producer saw: the first producer reads a response
/// spent at 11:50 (1000) and one spent at 12:05 (500). Each lands on the member
/// in force when it started, by the swap's recorded instant: kerry 1000,
/// mommy 500.
#[test]
fn a_swap_no_producer_saw_splits_responses_at_its_recorded_instant() {
    let home = HomeSandbox::new();
    reg_swapped("mommy", ms("2026-09-21T12:00:00.000Z"));
    write_lines(
        &iso_kerry(home.home()).join("sw.jsonl"),
        &[
            user("2026-09-21T11:50:00.000Z", "sw"),
            assistant("2026-09-21T11:50:20.000Z", "sw", "msg_pre", 1000),
            user("2026-09-21T12:05:00.000Z", "sw"),
            assistant("2026-09-21T12:05:20.000Z", "sw", "msg_post", 500),
        ],
    );
    let s = Producer::new().tick(ms("2026-09-21T12:06:00.000Z"));
    assert_eq!(snap_account_day(&s, "2026-09-21", "kerry"), 1000, "kerry");
    assert_eq!(snap_account_day(&s, "2026-09-21", "mommy"), 500, "mommy");
}

/// A restart reads the 1 h live figure exactly as the producer it replaces
/// would: a response that ended 30 min before the last cache write, before a
/// swap, stays on the member that spent it.
#[test]
fn a_restart_keeps_the_hour_figure_on_the_member_that_spent_it() {
    let home = HomeSandbox::new();
    let path = iso_kerry(home.home()).join("h1.jsonl");
    reg(None);
    write_lines(
        &path,
        &[user(START, "h1"), assistant(END, "h1", "msg_a", 1222)],
    );
    let mut p = Producer::new();
    p.tick(ms(END) + 1_000);
    reg(Some("mommy"));
    let last_write = ms(END) + 30 * 60_000;
    write_cache(home.home(), &p.tick(last_write));

    let at = last_write + 60_000;
    let kept = p.tick(at);
    let restarted = Producer::new().tick(at);
    assert!(
        kept.accounts.contains_key("kerry") && !kept.accounts.contains_key("mommy"),
        "the running producer is the reference: kerry spent msg_a"
    );
    assert_eq!(
        serde_json::to_value(&restarted.accounts).unwrap(),
        serde_json::to_value(&kept.accounts).unwrap(),
        "the restart moved a pre-swap response's hour figure to the new member"
    );
}

/// The swap is seen at 12:01, then the clock steps back an hour and a
/// response written after the swap lands stamped 11:05 (500): responses order
/// by clock, so it reads under the old member until the clock passes the swap.
/// A known limitation: kerry 1500, mommy 0, live and after a restart.
#[test]
fn a_step_back_across_a_swap_reads_post_swap_turns_under_the_old_member() {
    let home = HomeSandbox::new();
    reg(None);
    let path = iso_kerry(home.home()).join("sb.jsonl");
    write_lines(
        &path,
        &[
            user("2026-09-21T11:40:00.000Z", "sb"),
            assistant("2026-09-21T11:50:00.000Z", "sb", "msg_a", 1000),
        ],
    );
    let mut p = Producer::new();
    p.tick(ms("2026-09-21T11:51:00.000Z"));
    reg_swapped("mommy", ms("2026-09-21T12:00:00.000Z"));
    p.tick(ms("2026-09-21T12:01:00.000Z"));
    append_lines(
        &path,
        &[
            user("2026-09-21T11:04:00.000Z", "sb"),
            assistant("2026-09-21T11:05:00.000Z", "sb", "msg_b", 500),
        ],
    );
    let live = p.tick(ms("2026-09-21T11:06:00.000Z"));
    drop(p);
    let s = Producer::new().tick(ms("2026-09-21T11:07:00.000Z"));
    for (what, snap) in [("live", &live), ("restarted", &s)] {
        assert_eq!(
            snap_account_day(snap, "2026-09-21", "kerry"),
            1500,
            "{what}: kerry"
        );
        assert_eq!(
            snap_account_day(snap, "2026-09-21", "mommy"),
            0,
            "{what}: mommy"
        );
    }
}

/// An attribution file that does not parse is renamed aside, never saved
/// over; the new one is written owner-only.
#[test]
fn an_unparseable_attribution_file_is_renamed_aside() {
    let home = HomeSandbox::new();
    reg(None);
    let clauth = home.home().join(".clauth");
    std::fs::create_dir_all(&clauth).unwrap();
    std::fs::write(clauth.join("tps_attribution.json"), b"{not json").unwrap();
    write_lines(
        &iso_kerry(home.home()).join("c.jsonl"),
        &[user(START, "c"), assistant(END, "c", "msg_c", 1222)],
    );
    Producer::new().tick(ms(END) + 1_000);
    let aside: Vec<_> = std::fs::read_dir(&clauth)
        .unwrap()
        .filter_map(|e| e.ok()?.file_name().into_string().ok())
        .filter(|n| n.starts_with("tps_attribution.json.corrupt-"))
        .collect();
    assert_eq!(aside.len(), 1, "renamed aside: {aside:?}");
    assert_eq!(
        std::fs::read(clauth.join(&aside[0])).unwrap(),
        b"{not json",
        "the aside copy keeps the bytes"
    );
    let fresh = std::fs::read(clauth.join("tps_attribution.json")).unwrap();
    serde_json::from_slice::<serde_json::Value>(&fresh).expect("the new file parses");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(clauth.join("tps_attribution.json"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "owner-only");
    }
}

/// A fork copies a response (same message id) into a second file: it counts
/// once, under the earliest-created file's session, on every restart. The
/// original is unattributed (`*`), the fork sits in kerry's isolated store.
/// Read order alone would pick either file about half the time, so 12 runs red
/// such a rule with probability 1 - (1/2)^12.
#[test]
fn a_forked_response_counts_once_under_the_original_on_every_restart() {
    for run in 0..12 {
        let home = HomeSandbox::new();
        reg(None);
        write_lines(
            &global_store(home.home()).join("orig.jsonl"),
            &[user(START, "orig"), assistant(END, "orig", "msg_f", 1222)],
        );
        write_lines(
            &iso_kerry(home.home()).join("fork.jsonl"),
            &[user(START, "fork"), assistant(END, "fork", "msg_f", 1222)],
        );
        let s = Producer::new().tick(ms(END) + 1_000);
        assert_eq!(day_total(&s), 1222, "run {run}: once");
        assert_eq!(
            snap_account_day(&s, "2026-09-22", "*"),
            1222,
            "run {run}: the original's session"
        );
    }
}

/// A finished day still waits for the window: no fold at D+1 00:00:10, D =
/// 2000 once D leaves the window, and a restarted producer's later fold adds
/// nothing.
#[test]
fn a_day_folds_on_leaving_the_window_and_a_refold_adds_nothing() {
    let home = HomeSandbox::new();
    reg(None);
    write_lines(
        &iso_kerry(home.home()).join("f.jsonl"),
        &[
            user("2026-09-21T12:00:00.000Z", "f"),
            assistant("2026-09-21T12:00:20.000Z", "f", "msg_a", 2000),
            user("2026-09-21T12:05:00.000Z", "f"),
        ],
    );
    let mut p = Producer::new();
    p.tick(ms("2026-09-21T12:01:00.000Z"));
    p.tick(ms("2026-09-22T00:00:10.000Z"));
    let ledger = home.home().join(".clauth/token_speed.json");
    assert!(
        !ledger.exists() || ledger_day_output(home.home(), "2026-09-21") == 0,
        "D folded while it was yesterday"
    );
    p.tick(ms("2026-09-23T00:05:00.000Z"));
    assert_eq!(
        ledger_day_output(home.home(), "2026-09-21"),
        2000,
        "the fold on leaving the window"
    );
    drop(p);
    Producer::new().tick(ms("2026-09-24T00:05:00.000Z"));
    assert_eq!(ledger_day_output(home.home(), "2026-09-21"), 2000, "refold");
}

/// A cache an older clauth wrote (version 1, its own field set) holds a day the
/// ledger lacks: that day never folds (only transcripts count),
/// and the producer's own cache is version 2.
#[test]
fn an_older_clauths_cache_days_never_fold_and_the_cache_is_version_two() {
    let home = HomeSandbox::new();
    reg(None);
    let now = ms("2026-09-21T12:00:00.000Z");
    let mut wire = serde_json::to_value(Producer::new().tick(now - 60_000)).unwrap();
    let obj = wire.as_object_mut().unwrap();
    obj.retain(|k, _| {
        matches!(
            k.as_str(),
            "generated_at" | "total" | "accounts" | "sessions" | "days" | "frozen"
        )
    });
    obj.insert("version".into(), serde_json::json!(1));
    obj.insert(
        "days".into(),
        serde_json::json!({"2026-09-18": {"kerry": {"claude-opus-5-5": {"output": 500, "gen_ms": 10_000}}}}),
    );
    let path = home.home().join(".clauth").join(TPS_CACHE_FILE);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, serde_json::to_vec(&wire).unwrap()).unwrap();
    let s = Producer::new().tick(now);
    assert_eq!(
        ledger_day_total(home.home(), "2026-09-18"),
        0,
        "never folded"
    );
    assert_eq!(
        serde_json::to_value(&s).unwrap()["version"],
        serde_json::json!(2),
        "the cache version"
    );
}

/// A file rewritten in place at its exact length between two ticks (its mtime
/// moved): the new line counts, 200 then 200 + 300.
#[test]
fn a_file_rewritten_at_the_same_length_counts_the_new_line() {
    let home = HomeSandbox::new();
    reg(None);
    let x = ms(START);
    let path = iso_kerry(home.home()).join("e.jsonl");
    let at = iso(x + 10_000);
    write_lines(&path, &[user(START, "e"), assistant(&at, "e", "m1", 200)]);
    let mut p = Producer::new();
    assert_eq!(day_total(&p.tick(x + 60_000)), 200, "before the rewrite");
    let len = std::fs::metadata(&path).unwrap().len();
    write_lines(&path, &[user(START, "e"), assistant(&at, "e", "m2", 300)]);
    assert_eq!(std::fs::metadata(&path).unwrap().len(), len, "same length");
    crate::testutil::set_mtime(
        &path,
        std::time::UNIX_EPOCH + std::time::Duration::from_millis(x + 61_000),
    );
    assert_eq!(day_total(&p.tick(x + 62_000)), 500, "after the rewrite");
}

/// A turn's first block line (800) lands at D 23:59:58 and a streamed tool
/// result at 23:59:59.5, the producer ticks past midnight, the next block of
/// the same message lands at D+1 00:00:02, then a user line. The turn ends on
/// D+1, so D never holds it: ledger D 0 and D+1 800 once both left the window,
/// live and across a restart after the rollover tick.
#[test]
fn a_turn_still_streaming_at_midnight_lands_once_on_its_end_day() {
    for restart in [false, true] {
        let home = HomeSandbox::new();
        reg(None);
        let path = iso_kerry(home.home()).join("mid.jsonl");
        write_lines(
            &path,
            &[
                user("2026-09-21T23:59:50.000Z", "mid"),
                assistant("2026-09-21T23:59:58.000Z", "mid", "msg_mid", 800),
                user("2026-09-21T23:59:59.500Z", "mid"),
            ],
        );
        let mut p = Producer::new();
        p.tick(ms("2026-09-21T23:59:59.000Z"));
        write_cache(home.home(), &p.tick(ms("2026-09-22T00:00:01.000Z")));
        if restart {
            drop(p);
            p = Producer::new();
        }
        append_lines(
            &path,
            &[
                assistant("2026-09-22T00:00:02.000Z", "mid", "msg_mid", 800),
                user("2026-09-22T00:00:30.000Z", "mid"),
            ],
        );
        let s = p.tick(ms("2026-09-22T00:01:00.000Z"));
        assert_eq!(snap_day_output(&s, "2026-09-21"), 0, "restart {restart}: D");
        assert_eq!(
            snap_day_output(&s, "2026-09-22"),
            800,
            "restart {restart}: D+1"
        );
        p.tick(ms("2026-09-23T00:00:10.000Z"));
        p.tick(ms("2026-09-24T00:00:10.000Z"));
        assert_eq!(
            ledger_day_output(home.home(), "2026-09-21"),
            0,
            "restart {restart}: ledger D"
        );
        assert_eq!(
            ledger_day_output(home.home(), "2026-09-22"),
            800,
            "restart {restart}: ledger D+1"
        );
    }
}

/// A day with a turn that never sees another line (a killed session) does not
/// fold when it stops being today, and folds when it leaves the window.
#[test]
fn an_unfinished_turn_holds_its_days_first_fold_until_the_day_leaves_the_window() {
    let home = HomeSandbox::new();
    reg(None);
    write_lines(
        &iso_kerry(home.home()).join("open.jsonl"),
        &[
            user("2026-09-21T12:00:00.000Z", "open"),
            assistant("2026-09-21T12:00:20.000Z", "open", "msg_open", 2000),
        ],
    );
    let mut p = Producer::new();
    p.tick(ms("2026-09-21T12:01:00.000Z"));
    p.tick(ms("2026-09-22T00:00:10.000Z"));
    let ledger = home.home().join(".clauth/token_speed.json");
    assert!(
        !ledger.exists() || ledger_day_output(home.home(), "2026-09-21") == 0,
        "D folded while its turn was unfinished"
    );
    p.tick(ms("2026-09-23T00:00:10.000Z"));
    assert_eq!(
        ledger_day_output(home.home(), "2026-09-21"),
        2000,
        "D folded on leaving the window"
    );
}

/// A pre-swap turn (t0+0.2 s .. t0+0.7 s) still unread when the swap lands at
/// t0+0.9 s, then a post-swap turn, both read by the tick that observes the
/// swap: kerry keeps the pre-swap turn, live and after a restart.
#[test]
fn a_turn_written_before_a_swap_but_read_with_it_stays_on_the_old_member() {
    let home = HomeSandbox::new();
    reg(None);
    let t0 = ms("2026-09-21T12:00:00.000Z");
    let path = iso_kerry(home.home()).join("gap.jsonl");
    write_lines(
        &path,
        &[
            user(&iso(t0 - 60_000), "gap"),
            assistant(&iso(t0 - 50_000), "gap", "msg_a", 1000),
        ],
    );
    let mut p = Producer::new();
    p.tick(t0);
    append_lines(
        &path,
        &[
            user(&iso(t0 + 200), "gap"),
            assistant(&iso(t0 + 700), "gap", "msg_b", 500),
        ],
    );
    reg_swapped("mommy", t0 + 900);
    append_lines(
        &path,
        &[
            user(&iso(t0 + 950), "gap"),
            assistant(&iso(t0 + 990), "gap", "msg_c", 300),
        ],
    );
    let live = p.tick(t0 + 1_000);
    drop(p);
    let restarted = Producer::new().tick(t0 + 2_000);
    for (what, snap) in [("live", &live), ("restarted", &restarted)] {
        assert_eq!(
            snap_account_day(snap, "2026-09-21", "kerry"),
            1500,
            "{what}: kerry"
        );
        assert_eq!(
            snap_account_day(snap, "2026-09-21", "mommy"),
            300,
            "{what}: mommy"
        );
    }
}

/// A swap seen at 12:01, then a clock step back and a subagent response written
/// after the swap, stamped 11:05 (500): it orders by clock through its parent
/// session, so it reads under the old member — a known limitation.
#[test]
fn a_subagent_line_after_a_swap_reads_under_the_old_member_after_a_step_back() {
    let home = HomeSandbox::new();
    reg(None);
    write_lines(
        &iso_kerry(home.home()).join("sb.jsonl"),
        &[
            user("2026-09-21T11:40:00.000Z", "sb"),
            assistant("2026-09-21T11:50:00.000Z", "sb", "msg_a", 1000),
        ],
    );
    let mut p = Producer::new();
    p.tick(ms("2026-09-21T11:51:00.000Z"));
    reg_swapped("mommy", ms("2026-09-21T12:00:00.000Z"));
    p.tick(ms("2026-09-21T12:01:00.000Z"));
    write_lines(
        &iso_kerry(home.home()).join("sb/subagents/agent-x.jsonl"),
        &[
            user("2026-09-21T11:04:00.000Z", "sb"),
            assistant("2026-09-21T11:05:00.000Z", "sb", "msg_b", 500),
        ],
    );
    let live = p.tick(ms("2026-09-21T11:06:00.000Z"));
    assert_eq!(
        snap_account_day(&live, "2026-09-21", "kerry"),
        1500,
        "kerry"
    );
    assert_eq!(snap_account_day(&live, "2026-09-21", "mommy"), 0, "mommy");
}

/// An owner-store-only session changes owner kerry -> mommy while the
/// attribution file cannot be saved (a directory sits at its path), D's turns
/// finish, and the process dies after D stops being today; the restart finds
/// only the seed. No fold landed ahead of the timeline, so D is counted once:
/// the ledger ends at 1500.
#[test]
fn a_failed_attribution_save_holds_the_fold_so_a_restart_counts_once() {
    let home = HomeSandbox::new();
    reg(None);
    let clauth = home.home().join(".clauth");
    std::fs::create_dir_all(&clauth).unwrap();
    crate::sessions::stamp_exact_owner("bare", "kerry");
    let path = global_store(home.home()).join("bare.jsonl");
    write_lines(
        &path,
        &[
            user("2026-09-21T10:00:00.000Z", "bare"),
            assistant("2026-09-21T10:00:20.000Z", "bare", "msg_a", 1000),
        ],
    );
    let mut p = Producer::new();
    p.tick(ms("2026-09-21T10:01:00.000Z"));
    let tl = clauth.join("tps_attribution.json");
    let seed = std::fs::read(&tl).unwrap();
    std::fs::remove_file(&tl).unwrap();
    std::fs::create_dir_all(tl.join("blocker")).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(20));
    crate::sessions::stamp_exact_owner("bare", "mommy");
    p.tick(ms("2026-09-21T11:00:00.000Z"));
    append_lines(
        &path,
        &[
            user("2026-09-21T11:10:00.000Z", "bare"),
            assistant("2026-09-21T11:10:20.000Z", "bare", "msg_b", 500),
            user("2026-09-21T11:15:00.000Z", "bare"),
        ],
    );
    p.tick(ms("2026-09-21T11:16:00.000Z"));
    p.tick(ms("2026-09-22T00:00:05.000Z"));
    drop(p);
    std::fs::remove_dir_all(&tl).unwrap();
    std::fs::write(&tl, &seed).unwrap();
    let mut p2 = Producer::new();
    p2.tick(ms("2026-09-22T00:01:00.000Z"));
    p2.tick(ms("2026-09-23T00:00:10.000Z"));
    assert_eq!(
        ledger_day_output(home.home(), "2026-09-21"),
        1500,
        "ledger D after D left"
    );
}

/// A ledger unreadable when the producer starts, with nothing to fold: once it
/// reads again, `ever` holds the stored day's peak (2000 over 20 s: 100 t/s).
#[cfg(unix)]
#[test]
fn a_ledger_unreadable_at_start_rejoins_ever_with_nothing_to_fold() {
    use std::os::unix::fs::PermissionsExt;
    let home = HomeSandbox::new();
    reg(None);
    write_lines(
        &iso_kerry(home.home()).join("v.jsonl"),
        &[
            user("2026-09-21T12:00:00.000Z", "v"),
            assistant("2026-09-21T12:00:20.000Z", "v", "msg_v", 2000),
            user("2026-09-21T12:05:00.000Z", "v"),
        ],
    );
    let mut p = Producer::new();
    p.tick(ms("2026-09-21T12:06:00.000Z"));
    p.tick(ms("2026-09-23T00:05:00.000Z"));
    assert_eq!(ledger_day_output(home.home(), "2026-09-21"), 2000, "stored");
    drop(p);
    let ledger = home.home().join(".clauth/token_speed.json");
    std::fs::set_permissions(&ledger, std::fs::Permissions::from_mode(0o000)).unwrap();
    let mut p2 = Producer::new();
    p2.tick(ms("2026-09-25T00:05:00.000Z"));
    std::fs::set_permissions(&ledger, std::fs::Permissions::from_mode(0o600)).unwrap();
    let s = p2.tick(ms("2026-09-25T00:12:00.000Z"));
    assert_eq!(
        peaks(&s)["ever"]["speed"]["value"],
        serde_json::json!(100.0),
        "ever after the ledger reads again"
    );
}

/// This build stops on D holding D-1 (2000) and D (1000), both unfolded; an
/// older clauth then overwrites `tps_cache.json` with its own version-1 cache
/// that lacks them. This build returns on D+2, when both files are cold: both
/// days are still owed in `tps_days.json`, which an older clauth never touches,
/// so both rebuild from their transcripts and fold.
#[test]
fn days_held_when_an_older_clauth_overwrites_the_cache_still_fold() {
    let home = HomeSandbox::new();
    reg(None);
    let store = iso_kerry(home.home());
    for (name, day, id, out) in [
        ("p.jsonl", "2026-09-20", "msg_p", 2000),
        ("q.jsonl", "2026-09-21", "msg_q", 1000),
    ] {
        let path = store.join(name);
        let at = format!("{day}T12:00:20.000Z");
        write_lines(
            &path,
            &[
                user(&format!("{day}T12:00:00.000Z"), "pq"),
                assistant(&at, "pq", id, out),
                user(&format!("{day}T12:05:00.000Z"), "pq"),
            ],
        );
        crate::testutil::set_mtime(
            &path,
            std::time::UNIX_EPOCH + std::time::Duration::from_millis(ms(&at) + 280_000),
        );
    }
    let mut p = Producer::new();
    write_cache(home.home(), &p.tick(ms("2026-09-21T12:10:00.000Z")));
    drop(p);
    let older = serde_json::json!({
        "version": 1,
        "generated_at": "2026-09-22T12:00:00.000+00:00",
        "days": {"2026-09-22": {}},
    });
    std::fs::write(
        home.home().join(".clauth").join(TPS_CACHE_FILE),
        serde_json::to_vec(&older).unwrap(),
    )
    .unwrap();
    Producer::new().tick(ms("2026-09-23T12:00:00.000Z"));
    assert_eq!(ledger_day_output(home.home(), "2026-09-20"), 2000, "D-1");
    assert_eq!(ledger_day_output(home.home(), "2026-09-21"), 1000, "D");
}

/// A session swapped kerry -> mommy ends; its hook record still holds the
/// pre-swap answer (kerry, stamped before the swap). Entries order by their
/// stamps, so that stale answer sorts before the swap and the post-swap turn
/// stays on mommy: kerry 1000, mommy 500, live, after the row goes, and after a
/// restart.
#[test]
fn a_stale_hook_stamp_after_the_session_ends_never_reorders_a_swap() {
    let home = HomeSandbox::new();
    reg(None);
    let t0 = ms("2026-09-21T12:00:00.000Z");
    let record = crate::hook_note::record_path("end", None).unwrap();
    std::fs::create_dir_all(record.parent().unwrap()).unwrap();
    let secs = (t0 - 55_000) / 1000;
    std::fs::write(
        &record,
        format!(
            r#"{{"resolved":"kerry","resolved_at":{{"secs_since_epoch":{secs},"nanos_since_epoch":0}}}}"#
        ),
    )
    .unwrap();
    let path = iso_kerry(home.home()).join("end.jsonl");
    write_lines(
        &path,
        &[
            user(&iso(t0 - 60_000), "end"),
            assistant(&iso(t0 - 50_000), "end", "msg_a", 1000),
            user(&iso(t0 - 45_000), "end"),
        ],
    );
    let mut p = Producer::new();
    p.tick(t0);
    reg_swapped("mommy", t0 + 1_000);
    append_lines(
        &path,
        &[
            user(&iso(t0 + 2_000), "end"),
            assistant(&iso(t0 + 3_000), "end", "msg_b", 500),
            user(&iso(t0 + 4_000), "end"),
        ],
    );
    let live = p.tick(t0 + 5_000);
    crate::live_sessions::unregister("4242-1").unwrap();
    let ended = p.tick(t0 + 6_000);
    drop(p);
    let restarted = Producer::new().tick(t0 + 7_000);
    for (what, snap) in [
        ("live", &live),
        ("ended", &ended),
        ("restarted", &restarted),
    ] {
        assert_eq!(
            snap_account_day(snap, "2026-09-21", "kerry"),
            1000,
            "{what}: kerry"
        );
        assert_eq!(
            snap_account_day(snap, "2026-09-21", "mommy"),
            500,
            "{what}: mommy"
        );
    }
}

/// The output `token_speed.json` holds for one account on one day, summed over
/// models; 0 when the ledger, the day or the account is absent.
fn ledger_account_day(home: &Path, day: &str, account: &str) -> u64 {
    let disk: serde_json::Value = std::fs::read(home.join(".clauth").join("token_speed.json"))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    disk["days"][day][account]
        .as_object()
        .into_iter()
        .flat_map(|m| m.values())
        .map(|cell| cell["output"].as_u64().unwrap_or(0))
        .sum()
}

/// The output `token_speed.json` holds on one day over every account; 0 when
/// the ledger or the day is absent.
fn ledger_day_total(home: &Path, day: &str) -> u64 {
    let disk: serde_json::Value = std::fs::read(home.join(".clauth").join("token_speed.json"))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    disk["days"][day]
        .as_object()
        .into_iter()
        .flat_map(|a| a.values())
        .filter_map(|m| m.as_object())
        .flat_map(|m| m.values())
        .map(|cell| cell["output"].as_u64().unwrap_or(0))
        .sum()
}

/// A streamed `tool_result` user line: it lands between two block lines of one
/// message, so it never marks the message finished.
fn tool_result(ts: &str, session: &str) -> String {
    serde_json::json!({
        "type": "user",
        "timestamp": ts,
        "sessionId": session,
        "uuid": format!("tr-{ts}"),
        "message": {
            "role": "user",
            "content": [{"type": "tool_result", "tool_use_id": "t1", "content": "ok"}],
        },
    })
    .to_string()
}

/// A turn is mid-stream at midnight (block 800 at D 23:59:58, a tool_result at
/// 23:59:59.5) when the producer stops; its next block line lands at D+1
/// 00:00:02 while no producer runs. Whether the restart comes on D+1 or after
/// D has left the window, the turn ends on D+1 and counts 800 once, there.
#[test]
fn a_turn_mid_stream_at_midnight_then_down_past_the_window_counts_once_on_its_end_day() {
    for (label, stop, restart) in [
        (
            "restart on D+1",
            "2026-09-21T23:59:59.000Z",
            "2026-09-22T00:01:00.000Z",
        ),
        (
            "stop on D, restart on D+2",
            "2026-09-21T23:59:59.000Z",
            "2026-09-23T00:00:10.000Z",
        ),
        (
            "stop on D+1, restart on D+2",
            "2026-09-22T00:00:01.000Z",
            "2026-09-23T00:00:10.000Z",
        ),
    ] {
        let home = HomeSandbox::new();
        reg(None);
        let path = iso_kerry(home.home()).join("mid.jsonl");
        write_lines(
            &path,
            &[
                user("2026-09-21T23:59:50.000Z", "mid"),
                assistant("2026-09-21T23:59:58.000Z", "mid", "msg_mid", 800),
                tool_result("2026-09-21T23:59:59.500Z", "mid"),
            ],
        );
        let mut p = Producer::new();
        p.tick(ms("2026-09-21T23:59:59.000Z"));
        if stop != "2026-09-21T23:59:59.000Z" {
            p.tick(ms(stop));
        }
        drop(p);
        append_lines(
            &path,
            &[
                assistant("2026-09-22T00:00:02.000Z", "mid", "msg_mid", 800),
                user("2026-09-22T00:00:30.000Z", "mid"),
            ],
        );
        let mut p2 = Producer::new();
        p2.tick(ms(restart));
        p2.tick(ms("2026-09-24T00:00:10.000Z"));
        assert_eq!(ledger_day_total(home.home(), "2026-09-21"), 0, "{label}: D");
        assert_eq!(
            ledger_day_total(home.home(), "2026-09-22"),
            800,
            "{label}: D+1"
        );
    }
}

/// A swap splits D (kerry 1000, mommy 500); an older clauth then holds the
/// lease on D+1 and writes its own version-1 cache, whose D may carry any
/// split (here: the same one, or the whole day on mommy, as `b89ffad2`
/// attributes it). This build returns on D+2: D is rebuilt from the
/// transcripts and the timeline, never from the version-1 cache.
#[test]
fn an_older_clauths_cache_after_a_swap_never_moves_a_day_between_members() {
    for leg in ["no version-1 cache", "same split", "all on mommy"] {
        let home = HomeSandbox::new();
        reg(None);
        let t0 = ms("2026-09-21T12:00:00.000Z");
        let path = iso_kerry(home.home()).join("ow.jsonl");
        write_lines(
            &path,
            &[
                user(&iso(t0 - 60_000), "ow"),
                assistant(&iso(t0 - 50_000), "ow", "msg_a", 1000),
                user(&iso(t0 - 45_000), "ow"),
            ],
        );
        let mut p = Producer::new();
        p.tick(t0);
        reg_swapped("mommy", t0 + 1_000);
        append_lines(
            &path,
            &[
                user(&iso(t0 + 2_000), "ow"),
                assistant(&iso(t0 + 3_000), "ow", "msg_b", 500),
                user(&iso(t0 + 4_000), "ow"),
            ],
        );
        let s = p.tick(t0 + 5_000);
        drop(p);
        assert_eq!(snap_account_day(&s, "2026-09-21", "kerry"), 1000, "{leg}");
        assert_eq!(snap_account_day(&s, "2026-09-21", "mommy"), 500, "{leg}");
        let cell =
            |output: u64, gen_ms: u64| serde_json::json!({"output": output, "gen_ms": gen_ms});
        let v1_d = match leg {
            "same split" => Some(serde_json::json!({
                "kerry": {MODEL: cell(1000, 10_000)},
                "mommy": {MODEL: cell(500, 1_000)},
            })),
            "all on mommy" => Some(serde_json::json!({"mommy": {MODEL: cell(1500, 11_000)}})),
            _ => None,
        };
        if let Some(d) = v1_d {
            let v1 = serde_json::json!({
                "version": 1,
                "generated_at": iso_ms_utc(ms("2026-09-22T23:00:00.000Z")),
                "total": {}, "accounts": {}, "sessions": {},
                "days": {"2026-09-21": d, "2026-09-22": {}},
                "frozen": {},
            });
            std::fs::write(
                home.home().join(".clauth").join(TPS_CACHE_FILE),
                serde_json::to_vec(&v1).unwrap(),
            )
            .unwrap();
        }
        Producer::new().tick(ms("2026-09-23T00:05:00.000Z"));
        assert_eq!(
            ledger_account_day(home.home(), "2026-09-21", "kerry"),
            1000,
            "{leg}: kerry"
        );
        assert_eq!(
            ledger_account_day(home.home(), "2026-09-21", "mommy"),
            500,
            "{leg}: mommy"
        );
        assert_eq!(ledger_day_total(home.home(), "2026-09-21"), 1500, "{leg}");
    }
}

/// The stale-hook schedule above, but the session's last turn (msg_c, 300,
/// spent on mommy after the swap) is written and the live row goes before the
/// producer reads it: msg_c stays on mommy once the session ends and after a
/// restart.
#[test]
fn a_turn_unread_when_a_swapped_session_ends_stays_on_the_new_member() {
    let home = HomeSandbox::new();
    reg(None);
    let t0 = ms("2026-09-21T12:00:00.000Z");
    let record = crate::hook_note::record_path("end", None).unwrap();
    std::fs::create_dir_all(record.parent().unwrap()).unwrap();
    let secs = (t0 - 55_000) / 1000;
    std::fs::write(
        &record,
        format!(
            r#"{{"resolved":"kerry","resolved_at":{{"secs_since_epoch":{secs},"nanos_since_epoch":0}}}}"#
        ),
    )
    .unwrap();
    let path = iso_kerry(home.home()).join("end.jsonl");
    write_lines(
        &path,
        &[
            user(&iso(t0 - 60_000), "end"),
            assistant(&iso(t0 - 50_000), "end", "msg_a", 1000),
            user(&iso(t0 - 45_000), "end"),
        ],
    );
    let mut p = Producer::new();
    p.tick(t0);
    reg_swapped("mommy", t0 + 1_000);
    append_lines(
        &path,
        &[
            user(&iso(t0 + 2_000), "end"),
            assistant(&iso(t0 + 3_000), "end", "msg_b", 500),
            user(&iso(t0 + 4_000), "end"),
        ],
    );
    let live = p.tick(t0 + 5_000);
    assert_eq!(snap_account_day(&live, "2026-09-21", "kerry"), 1000, "live");
    assert_eq!(snap_account_day(&live, "2026-09-21", "mommy"), 500, "live");
    append_lines(
        &path,
        &[
            user(&iso(t0 + 5_200), "end"),
            assistant(&iso(t0 + 5_500), "end", "msg_c", 300),
        ],
    );
    crate::live_sessions::unregister("4242-1").unwrap();
    let ended = p.tick(t0 + 6_000);
    drop(p);
    let restarted = Producer::new().tick(t0 + 7_000);
    for (what, snap) in [("ended", &ended), ("restarted", &restarted)] {
        assert_eq!(
            snap_account_day(snap, "2026-09-21", "kerry"),
            1000,
            "{what}: kerry"
        );
        assert_eq!(
            snap_account_day(snap, "2026-09-21", "mommy"),
            800,
            "{what}: mommy"
        );
    }
}

/// A subagent transcript last written two days before a swap (its response,
/// 500 at D-2 10:00, spent on kerry) is re-read from byte 0 when the clock
/// steps back two days after the swap: the response stays on kerry, live and
/// after a restart.
#[test]
fn a_cold_subagent_reread_under_a_step_back_stays_on_the_member_that_spent_it() {
    let home = HomeSandbox::new();
    reg(None);
    let main = iso_kerry(home.home()).join("cs.jsonl");
    write_lines(
        &main,
        &[
            user("2026-09-21T11:40:00.000Z", "cs"),
            assistant("2026-09-21T11:50:00.000Z", "cs", "msg_a", 1000),
            user("2026-09-21T11:51:00.000Z", "cs"),
        ],
    );
    let sub = iso_kerry(home.home()).join("cs/subagents/agent-old.jsonl");
    write_lines(
        &sub,
        &[
            user("2026-09-19T09:59:00.000Z", "cs"),
            assistant("2026-09-19T10:00:00.000Z", "cs", "msg_old", 500),
        ],
    );
    crate::testutil::set_mtime(
        &sub,
        std::time::UNIX_EPOCH + std::time::Duration::from_millis(ms("2026-09-19T10:00:30.000Z")),
    );
    let mut p = Producer::new();
    p.tick(ms("2026-09-21T11:52:00.000Z"));
    reg_swapped("mommy", ms("2026-09-21T12:00:00.000Z"));
    p.tick(ms("2026-09-21T12:01:00.000Z"));
    let live = p.tick(ms("2026-09-19T12:00:00.000Z"));
    drop(p);
    let restarted = Producer::new().tick(ms("2026-09-19T12:01:00.000Z"));
    for (what, snap) in [("live", &live), ("restarted", &restarted)] {
        assert_eq!(
            snap_account_day(snap, "2026-09-19", "kerry"),
            500,
            "{what}: kerry"
        );
        assert_eq!(
            snap_account_day(snap, "2026-09-19", "mommy"),
            0,
            "{what}: mommy"
        );
    }
}

/// A life under a clock 3 days fast leaves a future day F (2000) unfolded; the
/// clock is corrected and lives start on F-3 and F-1. F folds only once it has
/// entered the window and left it, at F+2, and then holds its 2000.
#[test]
fn a_stored_future_day_waits_for_the_window_before_it_folds() {
    let home = HomeSandbox::new();
    reg(None);
    let path = iso_kerry(home.home()).join("fu.jsonl");
    write_lines(
        &path,
        &[
            user("2026-09-24T12:00:00.000Z", "fu"),
            assistant("2026-09-24T12:00:20.000Z", "fu", "msg_f", 2000),
            user("2026-09-24T12:05:00.000Z", "fu"),
        ],
    );
    Producer::new().tick(ms("2026-09-24T12:06:00.000Z"));
    Producer::new().tick(ms("2026-09-21T12:00:00.000Z"));
    assert_eq!(ledger_day_total(home.home(), "2026-09-24"), 0, "on F-3");
    let mut p = Producer::new();
    p.tick(ms("2026-09-23T12:00:00.000Z"));
    assert_eq!(ledger_day_total(home.home(), "2026-09-24"), 0, "on F-1");
    let mut t = ms("2026-09-23T13:00:00.000Z");
    while t < ms("2026-09-26T00:05:00.000Z") {
        p.tick(t);
        t += 6 * 3_600_000;
    }
    assert_eq!(ledger_day_total(home.home(), "2026-09-24"), 0, "before F+2");
    p.tick(ms("2026-09-26T00:05:00.000Z"));
    assert_eq!(ledger_day_total(home.home(), "2026-09-24"), 2000, "on F+2");
}

/// A′: a life on D leaves D owed (unfolded); no producer runs until a restart
/// 13, 14 or 15 days later. The restart re-reads the transcripts touched within
/// 14 days: 13 and 14 days on, D is rebuilt and folds (14 days back is not
/// older than 14); 15 days on, D gets no speed rows.
#[test]
fn an_owed_day_older_than_fourteen_days_gets_no_rows_and_a_thirteen_day_one_folds() {
    for (restart, want) in [
        ("2026-09-14T12:00:00.000Z", 1000),
        ("2026-09-15T12:00:00.000Z", 1000),
        ("2026-09-16T12:00:00.000Z", 0),
    ] {
        let home = HomeSandbox::new();
        reg(None);
        let path = iso_kerry(home.home()).join("ow14.jsonl");
        write_lines(
            &path,
            &[
                user("2026-09-01T12:00:00.000Z", "ow14"),
                assistant("2026-09-01T12:00:20.000Z", "ow14", "msg_o", 1000),
                user("2026-09-01T12:05:00.000Z", "ow14"),
            ],
        );
        crate::testutil::set_mtime(
            &path,
            std::time::UNIX_EPOCH
                + std::time::Duration::from_millis(ms("2026-09-01T12:05:00.000Z")),
        );
        let s = Producer::new().tick(ms("2026-09-01T12:10:00.000Z"));
        assert_eq!(snap_day_output(&s, "2026-09-01"), 1000, "D is live on D");
        assert_eq!(ledger_day_total(home.home(), "2026-09-01"), 0, "D owed");
        Producer::new().tick(ms(restart));
        assert_eq!(
            ledger_day_total(home.home(), "2026-09-01"),
            want,
            "restart {restart}"
        );
    }
}

/// A swap splits D (kerry 1000, mommy 500) in this build's only life on D,
/// which stops before any fold lands (its ledger saves fail, so it leaves no
/// version-2 ledger behind). An older clauth then holds the lease
/// past D's window exit and folds D into `token_speed.json` as version 1,
/// all on the current member (mommy 1500), but only where the ledger is absent
/// or version 1: it never saves over a newer one. This build returns on D+3:
/// D reads kerry 1000, mommy 500, 1500 in all.
#[test]
fn an_older_clauths_ledger_fold_after_a_swap_never_doubles_the_day() {
    for leg in [
        "no older ledger",
        "older fold all on mommy",
        "older fold, same split",
    ] {
        let home = HomeSandbox::new();
        reg(None);
        let t0 = ms("2026-09-21T12:00:00.000Z");
        let path = iso_kerry(home.home()).join("ol.jsonl");
        write_lines(
            &path,
            &[
                user(&iso(t0 - 60_000), "ol"),
                assistant(&iso(t0 - 50_000), "ol", "msg_a", 1000),
                user(&iso(t0 - 45_000), "ol"),
            ],
        );
        // This life's ledger saves fail, so no version-2 ledger stops the
        // older clauth from writing.
        block_ledger_saves(home.home());
        let mut p = Producer::new();
        p.tick(t0);
        reg_swapped("mommy", t0 + 1_000);
        append_lines(
            &path,
            &[
                user(&iso(t0 + 2_000), "ol"),
                assistant(&iso(t0 + 3_000), "ol", "msg_b", 500),
                user(&iso(t0 + 4_000), "ol"),
            ],
        );
        let s = p.tick(t0 + 5_000);
        drop(p);
        assert_eq!(snap_account_day(&s, "2026-09-21", "kerry"), 1000, "{leg}");
        assert_eq!(snap_account_day(&s, "2026-09-21", "mommy"), 500, "{leg}");
        let ledger = home.home().join(".clauth").join("token_speed.json");
        std::fs::remove_dir_all(&ledger).unwrap();
        let on_disk: Option<serde_json::Value> = std::fs::read(&ledger)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok());
        let older_may_write = on_disk
            .as_ref()
            .is_none_or(|v| v["version"].as_u64() == Some(1));
        let cell =
            |output: u64, gen_ms: u64| serde_json::json!({"output": output, "gen_ms": gen_ms});
        let older_d = match leg {
            "older fold all on mommy" => {
                Some(serde_json::json!({"mommy": {MODEL: cell(1500, 11_000)}}))
            }
            "older fold, same split" => Some(serde_json::json!({
                "kerry": {MODEL: cell(1000, 10_000)},
                "mommy": {MODEL: cell(500, 1_000)},
            })),
            _ => None,
        };
        assert!(
            older_may_write,
            "{leg}: no version-2 ledger on disk before the older clauth"
        );
        if let Some(d) = older_d {
            let mut wire = on_disk.unwrap_or_else(|| serde_json::json!({"version": 1, "days": {}}));
            wire["days"]["2026-09-21"] = d;
            std::fs::write(&ledger, serde_json::to_vec(&wire).unwrap()).unwrap();
        }
        Producer::new().tick(ms("2026-09-24T00:05:00.000Z"));
        assert_eq!(
            ledger_account_day(home.home(), "2026-09-21", "kerry"),
            1000,
            "{leg}: kerry"
        );
        assert_eq!(
            ledger_account_day(home.home(), "2026-09-21", "mommy"),
            500,
            "{leg}: mommy"
        );
        assert_eq!(ledger_day_total(home.home(), "2026-09-21"), 1500, "{leg}");
    }
}

/// Swap 1 to mommy at t0+1 s is observed; the clock steps back an hour and a
/// real swap 2 back to kerry is stamped t0-3500 s, before swap 1's stamp. Each
/// change reads by its own stamp: msg_c (300, read in the tick observing swap
/// 2) and msg_d (200, a tick later) land on kerry, msg_b (500, spent after
/// swap 1) stays on mommy, live and after a restart.
#[test]
fn a_real_swap_observed_after_a_step_back_reads_by_its_own_stamp() {
    let home = HomeSandbox::new();
    reg(None);
    let t0 = ms("2026-09-21T12:00:00.000Z");
    let path = iso_kerry(home.home()).join("st.jsonl");
    write_lines(
        &path,
        &[
            user(&iso(t0 - 60_000), "st"),
            assistant(&iso(t0 - 50_000), "st", "msg_a", 1000),
            user(&iso(t0 - 45_000), "st"),
        ],
    );
    let mut p = Producer::new();
    p.tick(t0);
    reg_swapped("mommy", t0 + 1_000);
    append_lines(
        &path,
        &[
            user(&iso(t0 + 2_000), "st"),
            assistant(&iso(t0 + 3_000), "st", "msg_b", 500),
            user(&iso(t0 + 4_000), "st"),
        ],
    );
    let before = p.tick(t0 + 5_000);
    assert_eq!(
        snap_account_day(&before, "2026-09-21", "mommy"),
        500,
        "swap 1 observed"
    );
    let back = t0 - 3_600_000;
    reg_swapped("kerry", back + 100_000);
    append_lines(
        &path,
        &[
            user(&iso(back + 101_000), "st"),
            assistant(&iso(back + 102_000), "st", "msg_c", 300),
            user(&iso(back + 103_000), "st"),
        ],
    );
    let observing = p.tick(back + 110_000);
    append_lines(
        &path,
        &[
            assistant(&iso(back + 120_000), "st", "msg_d", 200),
            user(&iso(back + 121_000), "st"),
        ],
    );
    let next = p.tick(back + 130_000);
    drop(p);
    let restarted = Producer::new().tick(back + 140_000);
    for (what, snap, kerry) in [
        ("observing tick", &observing, 1300),
        ("next tick", &next, 1500),
        ("restarted", &restarted, 1500),
    ] {
        assert_eq!(
            snap_account_day(snap, "2026-09-21", "kerry"),
            kerry,
            "{what}: kerry"
        );
        assert_eq!(
            snap_account_day(snap, "2026-09-21", "mommy"),
            500,
            "{what}: mommy"
        );
    }
}

/// One session key with a file in each store: a global `r.jsonl` (msg_g,
/// 300) and the isolated runtime's `r.jsonl` extending it (msg_r, 2000). The
/// isolated store names the runtime whichever file a tick reads first, so in
/// every one of 32 fresh lives the whole session (2300) is kerry's, on the
/// first tick, the next one and after a restart.
#[test]
fn a_session_in_two_stores_takes_its_runtimes_member_in_every_life() {
    for life in 0..32 {
        let home = HomeSandbox::new();
        reg(None);
        let x = ms(START) + 30 * 60_000;
        let global = [
            user(&iso(x), "r"),
            assistant(&iso(x + 20_000), "r", "msg_g", 300),
        ];
        write_lines(&global_store(home.home()).join("r.jsonl"), &global);
        let mut isolated = global.to_vec();
        isolated.push(user(&iso(x + 30_000), "r"));
        isolated.push(assistant(&iso(x + 50_000), "r", "msg_r", 2000));
        write_lines(&iso_kerry(home.home()).join("r.jsonl"), &isolated);
        let mut p = Producer::new();
        let first = p.tick(x + 60_000);
        let next = p.tick(x + 70_000);
        drop(p);
        let restarted = Producer::new().tick(x + 80_000);
        for (what, snap) in [
            ("first", &first),
            ("next", &next),
            ("restarted", &restarted),
        ] {
            assert_eq!(
                snap_account_day(snap, "2026-09-22", "kerry"),
                2300,
                "life {life}, {what}: kerry"
            );
            assert_eq!(
                snap_account_day(snap, "2026-09-22", UNATTRIBUTED),
                0,
                "life {life}, {what}: unattributed"
            );
        }
    }
}

/// D (2000) is owed; the first tick of D+1 reads a D+1 turn (300) and the
/// process dies between that tick's owed-day list save and its attribution
/// save. No producer runs until D+1 has left the window; the restart on D+3
/// folds D 2000 and D+1 300.
#[test]
fn a_day_first_seen_by_a_tick_that_dies_after_the_owed_list_save_still_folds() {
    let home = HomeSandbox::new();
    reg(None);
    let path = iso_kerry(home.home()).join("cw.jsonl");
    write_lines(
        &path,
        &[
            user("2026-09-21T12:00:00.000Z", "cw"),
            assistant("2026-09-21T12:00:20.000Z", "cw", "msg_a", 2000),
            user("2026-09-21T12:05:00.000Z", "cw"),
        ],
    );
    let mut p = Producer::new();
    p.tick(ms("2026-09-21T12:06:00.000Z"));
    append_lines(
        &path,
        &[
            user("2026-09-22T00:00:01.000Z", "cw"),
            assistant("2026-09-22T00:00:03.000Z", "cw", "msg_c", 300),
        ],
    );
    {
        let _halt = halt_seam::arm();
        p.tick(ms("2026-09-22T00:00:05.000Z"));
    }
    assert!(
        p.responses.values().any(|r| r.output == 300),
        "the dying tick read D+1's turn"
    );
    drop(p);
    Producer::new().tick(ms("2026-09-24T00:06:00.000Z"));
    assert_eq!(ledger_day_total(home.home(), "2026-09-21"), 2000, "D");
    assert_eq!(ledger_day_total(home.home(), "2026-09-22"), 300, "D+1");
}

/// An attribution file from a newer clauth keeps every attribution save
/// pending, so no fold can land: two starts each leave the owed 09-20
/// unbuilt (no re-read of its transcripts) and keep it in the list.
#[test]
fn an_owed_day_waits_for_its_rebuild_while_the_attribution_file_is_newer() {
    let home = HomeSandbox::new();
    let clauth = home.home().join(".clauth");
    std::fs::create_dir_all(&clauth).unwrap();
    std::fs::write(
        clauth.join(TPS_ATTRIBUTION_FILE),
        br#"{"version":99,"entries":[]}"#,
    )
    .unwrap();
    day_transcript(home.home(), "2026-09-20", 7, 3);
    write_owed(home.home(), &["2026-09-20"]);
    for at in ["2026-09-22T00:01:00.000Z", "2026-09-22T00:02:00.000Z"] {
        let s = Producer::new().tick(ms(at));
        assert!(
            !s.days.contains_key("2026-09-20"),
            "{at}: rebuilt while no fold could land"
        );
    }
    assert_eq!(
        owed_file(&clauth)["owed"],
        serde_json::json!(["2026-09-20", "2026-09-21", "2026-09-22"]),
        "still owed"
    );
}

/// A version-1 speed ledger (an older clauth's) is saved as version 2 at the
/// producer's first tick, its day kept, before any fold: an older clauth then
/// reads it as newer and never folds into it.
#[test]
fn a_version_one_speed_ledger_is_claimed_as_version_two_at_the_first_tick() {
    let home = HomeSandbox::new();
    let clauth = home.home().join(".clauth");
    std::fs::create_dir_all(&clauth).unwrap();
    std::fs::write(
        clauth.join("token_speed.json"),
        r#"{"version":1,"days":{"2026-09-10":{"kerry":{"claude-opus-5-5":{"output":1,"gen_ms":2}}}}}"#,
    )
    .unwrap();
    Producer::new().tick(ms("2026-09-22T12:00:00.000Z"));
    let disk: serde_json::Value =
        serde_json::from_slice(&std::fs::read(clauth.join("token_speed.json")).unwrap()).unwrap();
    assert_eq!(disk["version"], 2, "claimed: {disk}");
    assert_eq!(
        disk["days"]["2026-09-10"]["kerry"][MODEL],
        serde_json::json!({"output": 1, "gen_ms": 2}),
        "its day kept: {disk}"
    );
}

/// A future owed day F (a line stamped ahead of the box clock) is never
/// rebuilt at start: X, mid-stream at F's midnight, ends on F+1 once its next
/// block line lands, so F counts 0 and F+1 800.
#[test]
fn a_future_owed_day_is_never_held_at_start() {
    let home = HomeSandbox::new();
    reg(None);
    let path = iso_kerry(home.home()).join("fx.jsonl");
    write_lines(
        &path,
        &[
            user("2026-09-24T23:59:50.000Z", "fx"),
            assistant("2026-09-24T23:59:58.000Z", "fx", "msg_x", 800),
        ],
    );
    Producer::new().tick(ms("2026-09-21T12:00:00.000Z"));
    assert_eq!(
        owed_file(&home.home().join(".clauth"))["owed"],
        serde_json::json!(["2026-09-20", "2026-09-21", "2026-09-24"]),
        "F owed"
    );
    let mut p = Producer::new();
    p.tick(ms("2026-09-21T12:10:00.000Z"));
    append_lines(
        &path,
        &[assistant("2026-09-25T00:00:02.000Z", "fx", "msg_x", 800)],
    );
    let mut t = ms("2026-09-21T13:00:00.000Z");
    while t < ms("2026-09-27T00:05:00.000Z") {
        p.tick(t);
        t += 6 * 3_600_000;
    }
    p.tick(ms("2026-09-27T00:05:00.000Z"));
    assert_eq!(ledger_day_total(home.home(), "2026-09-24"), 0, "F");
    assert_eq!(ledger_day_total(home.home(), "2026-09-25"), 800, "F+1");
}

/// A held day (its fold refused: ledger saves blocked) that a clock step back
/// returns to yesterday never folds while it is in the window, even once the
/// ledger saves again.
#[test]
fn a_held_day_back_in_the_window_waits() {
    let home = HomeSandbox::new();
    reg(None);
    let path = iso_kerry(home.home()).join("hw.jsonl");
    write_lines(
        &path,
        &[
            user("2026-09-21T12:00:00.000Z", "hw"),
            assistant("2026-09-21T12:00:20.000Z", "hw", "msg_a", 2000),
            user("2026-09-21T12:05:00.000Z", "hw"),
        ],
    );
    let mut p = Producer::new();
    p.tick(ms("2026-09-21T12:06:00.000Z"));
    block_ledger_saves(home.home());
    p.tick(ms("2026-09-23T00:05:00.000Z"));
    assert!(p.held.contains_key("2026-09-21"), "D held");
    p.tick(ms("2026-09-22T13:00:00.000Z"));
    std::fs::remove_dir_all(home.home().join(".clauth/token_speed.json")).unwrap();
    p.tick(ms("2026-09-22T13:10:00.000Z"));
    p.tick(ms("2026-09-22T13:20:00.000Z"));
    assert_eq!(
        ledger_day_total(home.home(), "2026-09-21"),
        0,
        "folded while yesterday"
    );
}

/// An attribution file this life cannot read (a directory at its path) keeps
/// the owed 09-20 unbuilt; a tick with nothing to fold still lands its empty
/// fold, and 09-20 stays in the list. Once the file reads, 09-20 is rebuilt
/// and folds (7).
#[test]
fn an_owed_day_waits_in_the_list_until_the_attribution_file_reads() {
    let home = HomeSandbox::new();
    let clauth = home.home().join(".clauth");
    let timeline = clauth.join(TPS_ATTRIBUTION_FILE);
    std::fs::create_dir_all(timeline.join("blocker")).unwrap();
    day_transcript(home.home(), "2026-09-20", 7, 3);
    write_owed(home.home(), &["2026-09-20"]);
    let mut p = Producer::new();
    let s = p.tick(ms("2026-09-22T00:01:00.000Z"));
    assert!(
        !s.days.contains_key("2026-09-20"),
        "rebuilt while the attribution file was unreadable"
    );
    assert_eq!(
        owed_file(&clauth)["owed"],
        serde_json::json!(["2026-09-20", "2026-09-21", "2026-09-22"]),
        "still owed"
    );
    std::fs::remove_dir_all(&timeline).unwrap();
    p.tick(ms("2026-09-22T00:02:00.000Z"));
    assert_eq!(
        ledger_day_total(home.home(), "2026-09-20"),
        7,
        "folded once the file read"
    );
}

/// Swap 1 to mommy (stamped t0+1 s), a tick at t0+5 s, a 1 h clock step back,
/// then a real swap 2 back to kerry stamped back+100 s. Kerry spends msg_c
/// (300) under the stepped clock, msg_e (40) once the clock passes t0+5 s
/// again and msg_f (20) 2 h later. Clock order alone, no step-back bound
/// (the step-back class residual): msg_c reads under swap 2 (kerry), while
/// msg_e and msg_f start after swap 1's stamp, the newest entry at or before
/// them, so they read under mommy, live and after a restart. The day total
/// stays exact (1860) and no response counts twice.
#[test]
fn a_second_swap_after_a_step_back_reads_clock_order_once_the_clock_passes_the_first() {
    let home = HomeSandbox::new();
    reg(None);
    let d = "2026-09-21";
    let t0 = ms("2026-09-21T12:00:00.000Z");
    let path = iso_kerry(home.home()).join("st.jsonl");
    write_lines(
        &path,
        &[
            user(&iso(t0 - 60_000), "st"),
            assistant(&iso(t0 - 50_000), "st", "msg_a", 1000),
            user(&iso(t0 - 45_000), "st"),
        ],
    );
    let mut p = Producer::new();
    p.tick(t0);
    reg_swapped("mommy", t0 + 1_000);
    append_lines(
        &path,
        &[
            user(&iso(t0 + 2_000), "st"),
            assistant(&iso(t0 + 3_000), "st", "msg_b", 500),
            user(&iso(t0 + 4_000), "st"),
        ],
    );
    p.tick(t0 + 5_000);
    let back = t0 - 3_600_000;
    reg_swapped("kerry", back + 100_000);
    append_lines(
        &path,
        &[
            user(&iso(back + 101_000), "st"),
            assistant(&iso(back + 102_000), "st", "msg_c", 300),
            user(&iso(back + 103_000), "st"),
        ],
    );
    let observing = p.tick(back + 110_000);
    let mut t = back + 120_000;
    while t < t0 + 10_000 {
        p.tick(t);
        t += 300_000;
    }
    append_lines(
        &path,
        &[
            user(&iso(t0 + 11_000), "st"),
            assistant(&iso(t0 + 12_000), "st", "msg_e", 40),
            user(&iso(t0 + 13_000), "st"),
        ],
    );
    let passed = p.tick(t0 + 15_000);
    let mut t = t0 + 20_000;
    while t < t0 + 7_200_000 {
        p.tick(t);
        t += 600_000;
    }
    append_lines(
        &path,
        &[
            user(&iso(t0 + 7_200_000), "st"),
            assistant(&iso(t0 + 7_201_000), "st", "msg_f", 20),
            user(&iso(t0 + 7_202_000), "st"),
        ],
    );
    let later = p.tick(t0 + 7_210_000);
    drop(p);
    let restarted = Producer::new().tick(t0 + 7_220_000);
    for (what, snap, mommy) in [
        ("observing", &observing, 500),
        ("past swap 1", &passed, 540),
        ("2 h later", &later, 560),
        ("restarted", &restarted, 560),
    ] {
        assert_eq!(snap_account_day(snap, d, "kerry"), 1300, "{what}: kerry");
        assert_eq!(snap_account_day(snap, d, "mommy"), mommy, "{what}: mommy");
    }
}

/// The stale-hook schedule with the hook record stamped in the swap's own ms
/// (t0+1 s): on equal stamps the earlier-added entry (the swap) holds, so the
/// post-swap turn stays on mommy live, after the session ends and after a
/// restart.
#[test]
fn a_stale_hook_stamp_tied_with_the_swap_keeps_the_swap() {
    let home = HomeSandbox::new();
    reg(None);
    let t0 = ms("2026-09-21T12:00:00.000Z");
    let record = crate::hook_note::record_path("end", None).unwrap();
    std::fs::create_dir_all(record.parent().unwrap()).unwrap();
    let secs = (t0 + 1_000) / 1000;
    std::fs::write(
        &record,
        format!(
            r#"{{"resolved":"kerry","resolved_at":{{"secs_since_epoch":{secs},"nanos_since_epoch":0}}}}"#
        ),
    )
    .unwrap();
    let path = iso_kerry(home.home()).join("end.jsonl");
    write_lines(
        &path,
        &[
            user(&iso(t0 - 60_000), "end"),
            assistant(&iso(t0 - 50_000), "end", "msg_a", 1000),
            user(&iso(t0 - 45_000), "end"),
        ],
    );
    let mut p = Producer::new();
    p.tick(t0);
    reg_swapped("mommy", t0 + 1_000);
    append_lines(
        &path,
        &[
            user(&iso(t0 + 2_000), "end"),
            assistant(&iso(t0 + 3_000), "end", "msg_b", 500),
            user(&iso(t0 + 4_000), "end"),
        ],
    );
    let live = p.tick(t0 + 5_000);
    crate::live_sessions::unregister("4242-1").unwrap();
    let ended = p.tick(t0 + 6_000);
    drop(p);
    let restarted = Producer::new().tick(t0 + 7_000);
    for (what, snap) in [
        ("live", &live),
        ("ended", &ended),
        ("restarted", &restarted),
    ] {
        assert_eq!(
            snap_account_day(snap, "2026-09-21", "kerry"),
            1000,
            "{what}: kerry"
        );
        assert_eq!(
            snap_account_day(snap, "2026-09-21", "mommy"),
            500,
            "{what}: mommy"
        );
    }
}

/// A tick on which D leaves the window halts after its owed-list save and
/// before the attribution save: D's removal waits for its fold, so D stays
/// owed and the restart folds it (2000).
#[test]
fn a_halt_on_the_tick_a_day_leaves_the_window_keeps_it_owed() {
    let home = HomeSandbox::new();
    reg(None);
    let path = iso_kerry(home.home()).join("hl.jsonl");
    write_lines(
        &path,
        &[
            user("2026-09-21T12:00:00.000Z", "hl"),
            assistant("2026-09-21T12:00:20.000Z", "hl", "msg_a", 2000),
            user("2026-09-21T12:05:00.000Z", "hl"),
        ],
    );
    let mut p = Producer::new();
    p.tick(ms("2026-09-21T12:06:00.000Z"));
    p.tick(ms("2026-09-22T12:00:00.000Z"));
    {
        let _halt = halt_seam::arm();
        p.tick(ms("2026-09-23T00:05:00.000Z"));
    }
    drop(p);
    Producer::new().tick(ms("2026-09-25T00:06:00.000Z"));
    assert_eq!(ledger_day_total(home.home(), "2026-09-21"), 2000, "D");
}

/// One session key in two isolated runtimes with no live row, mommy's file
/// created first though its path sorts after kerry's: the earliest-created
/// file's runtime names the member in every life (mommy 2300).
#[test]
fn the_earliest_created_isolated_file_names_the_runtime() {
    for life in 0..8 {
        let home = HomeSandbox::new();
        let x = ms(START) + 30 * 60_000;
        let mp = home
            .home()
            .join(".clauth/profiles/mommy/runtime-isolated-5353-1/projects/-home-u-proj/r.jsonl");
        write_lines(
            &mp,
            &[
                user(&iso(x + 30_000), "r"),
                assistant(&iso(x + 50_000), "r", "msg_m", 2000),
            ],
        );
        std::thread::sleep(std::time::Duration::from_millis(15));
        write_lines(
            &iso_kerry(home.home()).join("r.jsonl"),
            &[
                user(&iso(x), "r"),
                assistant(&iso(x + 20_000), "r", "msg_k", 300),
            ],
        );
        let s = Producer::new().tick(x + 60_000);
        assert_eq!(
            snap_account_day(&s, "2026-09-22", "mommy"),
            2300,
            "life {life}"
        );
    }
}

// ── a live runtime names the member; a nested halt arm ──

/// One session key in two isolated runtimes, kerry's (no live row) created
/// first, mommy's (live) 15 ms later: the runtime with the live row names the
/// member in every life (mommy 2300), never the dead runtime's launch profile.
#[test]
fn a_live_isolated_runtime_names_the_member_over_an_earlier_created_dead_one() {
    for life in 0..8 {
        let home = HomeSandbox::new();
        crate::live_sessions::register(&crate::testutil::live_row("5353-1", "mommy")).unwrap();
        let x = ms(START) + 30 * 60_000;
        write_lines(
            &iso_kerry(home.home()).join("r.jsonl"),
            &[
                user(&iso(x), "r"),
                assistant(&iso(x + 20_000), "r", "msg_k", 300),
            ],
        );
        std::thread::sleep(std::time::Duration::from_millis(15));
        let mp = home
            .home()
            .join(".clauth/profiles/mommy/runtime-isolated-5353-1/projects/-home-u-proj/r.jsonl");
        write_lines(
            &mp,
            &[
                user(&iso(x + 30_000), "r"),
                assistant(&iso(x + 50_000), "r", "msg_m", 2000),
            ],
        );
        let s = Producer::new().tick(x + 60_000);
        assert_eq!(
            snap_account_day(&s, "2026-09-22", "mommy"),
            2300,
            "life {life}"
        );
    }
}

/// Two nested halt arms: the inner one dropping leaves the halt armed until
/// the outer one drops.
#[test]
fn a_nested_halt_arm_dropping_leaves_the_outer_one_armed() {
    let outer = halt_seam::arm();
    drop(halt_seam::arm());
    assert!(halt_seam::armed(), "the inner drop disarmed the outer arm");
    drop(outer);
    assert!(!halt_seam::armed(), "armed after both dropped");
}

// ── a save refused at its temp builds no content ──

/// A read-only `.clauth` whose timeline already holds every session (a
/// writable life saved it) owes 09-20: every tick asks for a ledger save
/// before the rebuild, and a save refused at its temp file never builds the
/// ledger's content. The first tick the dir is writable again saves, rebuilds
/// 09-20 and folds it: two content builds, ledger 7. The count sees only
/// `SpeedLedger::content`: a serialize or clone of the ledger's days or rate
/// peaks outside it, before the temp opens, goes unseen.
#[test]
fn a_ledger_save_refused_at_its_temp_file_builds_no_content_until_writable() {
    let home = HomeSandbox::new();
    reg(None);
    let clauth = home.home().join(".clauth");
    write_lines(
        &iso_kerry(home.home()).join("ro.jsonl"),
        &[
            user("2026-09-22T00:00:10.000Z", "ro"),
            assistant("2026-09-22T00:00:20.000Z", "ro", "msg_ro", 100),
        ],
    );
    Producer::new().tick(ms("2026-09-22T00:00:30.000Z"));
    day_transcript(home.home(), "2026-09-20", 7, 3);
    write_owed(home.home(), &["2026-09-20", "2026-09-21", "2026-09-22"]);

    let unwritable = crate::testutil::UnwritableDir::new(&clauth);
    let mut p = Producer::new();
    let before = crate::token_ledger::serializations::taken();
    for i in 0..5 {
        p.tick(ms("2026-09-22T00:01:00.000Z") + i * 1_000);
    }
    assert!(
        p.unbuilt.contains("2026-09-20") && p.folds_can_land(),
        "precondition: only the ledger save holds the rebuild"
    );
    assert_eq!(
        crate::token_ledger::serializations::taken() - before,
        0,
        "a refused save built the ledger's content"
    );
    drop(unwritable);
    p.tick(ms("2026-09-22T00:01:10.000Z"));
    assert_eq!(
        crate::token_ledger::serializations::taken() - before,
        2,
        "the writable tick: the save before the rebuild, then the fold"
    );
    assert_eq!(ledger_account_day(home.home(), "2026-09-20", "kerry"), 7);
}

/// Kerry spends msg_a (1000, D 12:00), msg_b (500, D 18:00) and a subagent's
/// msg_s (70, D 19:00); D folds at D+2 00:05 (1570). The clock steps back 33 h
/// to D 15:00 and the session swaps to mommy at D 15:01, so D re-enters the
/// window and its later responses read under mommy. The refold compares per
/// (day, model) total and replaces the split, so D stays 1570 live, after a
/// restart, and on a restart that is itself the refold; with kerry spending
/// msg_c (300) before the swap and mommy msg_d (200) after it, D reads 2070;
/// with no swap, 1570.
#[test]
fn a_refold_after_a_deep_step_back_and_a_swap_keeps_the_day_total() {
    let msg = |path: &Path, start: u64, id: &str, output: u64| {
        if !path.exists() {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, b"").unwrap();
        }
        append_lines(
            path,
            &[
                user(&iso(start), "rf"),
                assistant(&iso(start + 1_000), "rf", id, output),
                user(&iso(start + 2_000), "rf"),
            ],
        );
    };
    let run = |p: &mut Producer, from: u64, to: u64| {
        let mut t = from;
        while t < to {
            p.tick(t);
            t += 3_600_000;
        }
        p.tick(to);
    };
    let d = "2026-09-21";
    let fold_at = ms("2026-09-23T00:05:00.000Z");
    for (leg, want) in [
        ("live", 1570),
        ("restart", 1570),
        ("spend", 2070),
        ("noswap", 1570),
    ] {
        let home = HomeSandbox::new();
        reg(None);
        let path = iso_kerry(home.home()).join("rf.jsonl");
        let sub = iso_kerry(home.home()).join("rf/subagents/agent-s.jsonl");
        msg(&path, ms("2026-09-21T12:00:00.000Z"), "msg_a", 1000);
        msg(&path, ms("2026-09-21T18:00:00.000Z"), "msg_b", 500);
        msg(&sub, ms("2026-09-21T19:00:00.000Z"), "msg_s", 70);
        let mut p = Producer::new();
        run(&mut p, ms("2026-09-21T19:10:00.000Z"), fold_at);
        assert_eq!(ledger_day_total(home.home(), d), 1570, "{leg}: first fold");
        let back = ms("2026-09-21T15:00:00.000Z");
        p.tick(back);
        if leg == "spend" {
            msg(&path, back + 10_000, "msg_c", 300);
            p.tick(back + 50_000);
        }
        if leg != "noswap" {
            reg_swapped("mommy", back + 60_000);
        }
        if leg == "spend" {
            msg(&path, back + 120_000, "msg_d", 200);
        }
        p.tick(back + 180_000);
        if leg == "restart" {
            drop(p);
            let mut q = Producer::new();
            q.tick(fold_at);
            q.tick(fold_at + 60_000);
            assert_eq!(ledger_day_total(home.home(), d), want, "{leg}: refold");
        } else {
            run(&mut p, back + 3_600_000, fold_at);
            assert_eq!(ledger_day_total(home.home(), d), want, "{leg}: refold");
            drop(p);
            Producer::new().tick(fold_at + 300_000);
            assert_eq!(ledger_day_total(home.home(), d), want, "{leg}: restarted");
        }
    }
}

// ── refold per model total: a day that lost a transcript, a held day ──

/// The output a producer holds for one account on a held day (a day that left
/// the window while its fold cannot land), summed over models; 0 when the day
/// or the account is absent.
fn held_account_day(p: &Producer, day: &str, account: &str) -> u64 {
    p.held
        .get(day)
        .and_then(|a| a.get(account))
        .into_iter()
        .flat_map(|m| m.values())
        .map(|s| s.output)
        .sum()
}

/// Kerry spends msg_a (1000, D 12:00) and, in another session's file, msg_b
/// (500, D 18:00); D leaves the window at D+2 00:05 as kerry 1500, folded, or
/// held while ledger saves fail. msg_a's file is deleted, the clock steps back
/// 33 h to D 15:00 and the runtime swaps to mommy at D 15:01, so D re-enters
/// the window holding msg_b alone, under mommy. The refold's model total (500)
/// is below the stored one (1500), so the stored model stays whole: kerry
/// 1500, mommy nothing, D 1500, in the held day once D leaves again, in the
/// ledger, and after a restart; never kerry 1500 + mommy 500, never mommy 500
/// alone.
#[test]
fn a_refold_of_a_day_that_lost_a_transcript_keeps_the_stored_model() {
    let run = |p: &mut Producer, from: u64, to: u64| {
        let mut t = from;
        while t < to {
            p.tick(t);
            t += 3_600_000;
        }
        p.tick(to);
    };
    let d = "2026-09-21";
    let fold_at = ms("2026-09-23T00:05:00.000Z");
    for leg in ["folded", "held"] {
        let home = HomeSandbox::new();
        reg(None);
        let lost = iso_kerry(home.home()).join("la.jsonl");
        let kept = iso_kerry(home.home()).join("lb.jsonl");
        for (path, start, id, output) in [
            (&lost, ms("2026-09-21T12:00:00.000Z"), "msg_a", 1000),
            (&kept, ms("2026-09-21T18:00:00.000Z"), "msg_b", 500),
        ] {
            write_lines(
                path,
                &[
                    user(&iso(start), "lt"),
                    assistant(&iso(start + 1_000), "lt", id, output),
                    user(&iso(start + 2_000), "lt"),
                ],
            );
        }
        let mut p = Producer::new();
        p.tick(ms("2026-09-21T18:10:00.000Z"));
        if leg == "held" {
            block_ledger_saves(home.home());
        }
        run(&mut p, ms("2026-09-21T19:10:00.000Z"), fold_at);
        let first = if leg == "held" {
            held_account_day(&p, d, "kerry")
        } else {
            ledger_account_day(home.home(), d, "kerry")
        };
        assert_eq!(first, 1500, "{leg}: the first leave");
        std::fs::remove_file(&lost).unwrap();
        let back = ms("2026-09-21T15:00:00.000Z");
        p.tick(back);
        reg_swapped("mommy", back + 60_000);
        let live = p.tick(back + 180_000);
        assert_eq!(
            (
                snap_account_day(&live, d, "kerry"),
                snap_account_day(&live, d, "mommy"),
            ),
            (0, 500),
            "{leg}: live D after the step back and the swap"
        );
        run(&mut p, back + 3_600_000, fold_at);
        if leg == "held" {
            assert_eq!(
                (
                    held_account_day(&p, d, "kerry"),
                    held_account_day(&p, d, "mommy"),
                ),
                (1500, 0),
                "held: the stored model stays held"
            );
            std::fs::remove_dir_all(home.home().join(".clauth/token_speed.json")).unwrap();
            p.tick(fold_at + 60_000);
        }
        for life in ["refold", "restarted"] {
            assert_eq!(
                (
                    ledger_account_day(home.home(), d, "kerry"),
                    ledger_account_day(home.home(), d, "mommy"),
                ),
                (1500, 0),
                "{leg} {life}: the stored model stays"
            );
            assert_eq!(ledger_day_total(home.home(), d), 1500, "{leg} {life}: D");
            drop(p);
            p = Producer::new();
            p.tick(fold_at + 300_000);
        }
    }
}

/// Kerry spends msg_a (1000, D 12:00), msg_b (500, D 18:00) and msg_s (70,
/// D 19:00) while ledger saves fail, so D leaves the window at D+2 00:05 and
/// stays held (kerry 1570). The clock steps back 33 h to D 15:00 and the
/// runtime swaps to mommy at D 15:01, so D re-enters the window with msg_b and
/// msg_s under mommy; when it leaves again the held day refolds per model
/// total, holding kerry 1000 + mommy 570, never kerry 1570 + mommy 570. Once
/// saves land, D folds as 1570, and a restart keeps it.
#[test]
fn a_held_day_rederived_after_a_step_back_and_a_swap_keeps_the_day_total() {
    let held_total = |p: &Producer, day: &str| -> u64 {
        p.held
            .get(day)
            .into_iter()
            .flat_map(|a| a.values())
            .flat_map(|m| m.values())
            .map(|s| s.output)
            .sum()
    };
    let d = "2026-09-21";
    let leave = ms("2026-09-23T00:05:00.000Z");
    let home = HomeSandbox::new();
    reg(None);
    let path = iso_kerry(home.home()).join("rh.jsonl");
    let mut lines = Vec::new();
    for (start, id, output) in [
        (ms("2026-09-21T12:00:00.000Z"), "msg_a", 1000),
        (ms("2026-09-21T18:00:00.000Z"), "msg_b", 500),
        (ms("2026-09-21T19:00:00.000Z"), "msg_s", 70),
    ] {
        lines.extend([
            user(&iso(start), "rh"),
            assistant(&iso(start + 1_000), "rh", id, output),
            user(&iso(start + 2_000), "rh"),
        ]);
    }
    write_lines(&path, &lines);
    let mut p = Producer::new();
    p.tick(ms("2026-09-21T19:10:00.000Z"));
    block_ledger_saves(home.home());
    p.tick(leave);
    assert_eq!(
        (held_account_day(&p, d, "kerry"), held_total(&p, d)),
        (1570, 1570),
        "held after the first leave"
    );
    let back = ms("2026-09-21T15:00:00.000Z");
    p.tick(back);
    reg_swapped("mommy", back + 60_000);
    p.tick(back + 180_000);
    p.tick(leave);
    assert_eq!(
        (
            held_account_day(&p, d, "kerry"),
            held_account_day(&p, d, "mommy"),
            held_total(&p, d)
        ),
        (1000, 570, 1570),
        "held after the second leave"
    );
    std::fs::remove_dir_all(home.home().join(".clauth/token_speed.json")).unwrap();
    p.tick(leave + 60_000);
    assert_eq!(ledger_day_total(home.home(), d), 1570, "landed");
    drop(p);
    Producer::new().tick(leave + 300_000);
    assert_eq!(ledger_day_total(home.home(), d), 1570, "restarted");
}

// ── the all-time peaks walk the ledger only after it changes ──

/// A version-2 `token_speed.json` of `days` days ending 2026-09-19: each day
/// kerry and mommy on `MODEL` and `OTHER_MODEL`, every cell with a speed peak
/// under 60 t/s and every grouping with a rate peak, at the day's noon.
fn write_large_ledger(home: &Path, days: u64) {
    let last = chrono::NaiveDate::from_ymd_opt(2026, 9, 19).unwrap();
    let mut wire_days = serde_json::Map::new();
    let mut wire_rates = serde_json::Map::new();
    for i in 0..days {
        let key = (last - chrono::Days::new(i)).format("%Y-%m-%d").to_string();
        let peak = serde_json::json!({"value": 10.0 + (i % 50) as f64, "at": ms(&format!("{key}T12:00:00.000Z"))});
        let cell = serde_json::json!({"output": 1000, "gen_ms": 100_000, "speed": peak});
        wire_days.insert(
            key.clone(),
            serde_json::json!({
                "kerry": {MODEL: cell, OTHER_MODEL: cell},
                "mommy": {MODEL: cell, OTHER_MODEL: cell},
            }),
        );
        wire_rates.insert(
            key,
            serde_json::json!({
                "total": peak,
                "accounts": {"kerry": peak, "mommy": peak},
                "models": {MODEL: peak, OTHER_MODEL: peak},
            }),
        );
    }
    let clauth = home.join(".clauth");
    std::fs::create_dir_all(&clauth).unwrap();
    let wire = serde_json::json!({"version": 2, "days": wire_days, "rate_peaks": wire_rates});
    std::fs::write(clauth.join("token_speed.json"), wire.to_string()).unwrap();
}

/// One kerry response of `output` tokens over the 2 s ending at `end`, in
/// session `w`'s transcript.
fn append_two_second_response(path: &Path, end: u64, id: &str, output: u64) {
    append_lines(
        path,
        &[
            user(&iso(end - 2_000), "w"),
            assistant(&iso(end), "w", id, output),
        ],
    );
}

/// A 365-day ledger: each tick a faster kerry response lands, so `ever`'s
/// speed moves every tick, and no tick walks the stored days again; the
/// tick after 09-21's fold saved walks them once, and a tick active after
/// it none. `ever` keeps the models only the ledger holds throughout.
#[test]
fn an_active_tick_walks_no_stored_day_and_a_saved_fold_walks_them_once() {
    let home = HomeSandbox::new();
    reg(None);
    write_large_ledger(home.home(), 365);
    let path = iso_kerry(home.home()).join("w.jsonl");
    let t0 = ms("2026-09-21T12:00:00.000Z");
    write_lines(&path, &[user(&iso(t0 - 2_000), "w")]);
    append_lines(&path, &[assistant(&iso(t0), "w", "msg_w0", 1000)]);
    let mut p = Producer::new();
    let start = ledger_walks::taken();
    p.tick(t0 + 500);
    assert_eq!(ledger_walks::taken() - start, 1, "the first tick");
    for k in 1..=5u64 {
        let end = t0 + 10_000 * k;
        append_two_second_response(&path, end, &format!("msg_w{k}"), 1000 + 100 * k);
        let s = p.tick(end + 500);
        assert_eq!(
            peaks(&s)["ever"]["speed"],
            speed_peak((1000 + 100 * k) as f64 / 2.0, end, "kerry", MODEL),
            "ever moved at active tick {k}"
        );
        assert_eq!(
            ledger_walks::taken() - start,
            1,
            "active tick {k} walked the ledger"
        );
    }
    let leave = ms("2026-09-23T00:05:00.000Z");
    p.tick(leave);
    assert_eq!(
        ledger_day_total(home.home(), "2026-09-21"),
        7500,
        "09-21 folded"
    );
    let s = p.tick(leave + 60_000);
    assert_eq!(
        ledger_walks::taken() - start,
        2,
        "a saved fold, then one tick"
    );
    assert_eq!(
        peaks(&s)["ever"]["speed"],
        speed_peak(750.0, t0 + 50_000, "kerry", MODEL),
        "ever after the fold"
    );
    let end = leave + 120_000;
    write_lines(
        &iso_kerry(home.home()).join("w2.jsonl"),
        &[user(&iso(end - 2_000), "w2")],
    );
    append_lines(
        &iso_kerry(home.home()).join("w2.jsonl"),
        &[assistant(&iso(end), "w2", "msg_w2", 2000)],
    );
    let s = p.tick(end + 500);
    assert_eq!(
        peaks(&s)["ever"]["speed"],
        speed_peak(1000.0, end, "kerry", MODEL),
        "ever moved after the fold"
    );
    assert_eq!(
        ledger_walks::taken() - start,
        2,
        "an active tick after the fold walked the ledger"
    );
    assert!(
        peaks(&s)["ever"]["models"][OTHER_MODEL]["speed"].is_object(),
        "the ledger's own model: {}",
        peaks(&s)["ever"]
    );
}

/// A ledger unreadable at start (a directory at its path) whose file then
/// reads: the tick that reloads it walks the stored days once, and `ever`
/// gains the model only the ledger holds; the active ticks before and after
/// walk none.
#[test]
fn a_ledger_that_reads_again_is_walked_once_and_active_ticks_walk_none() {
    let home = HomeSandbox::new();
    reg(None);
    block_ledger_saves(home.home());
    let path = iso_kerry(home.home()).join("w.jsonl");
    let t0 = ms("2026-09-21T12:00:00.000Z");
    write_lines(&path, &[user(&iso(t0 - 2_000), "w")]);
    append_lines(&path, &[assistant(&iso(t0), "w", "msg_w0", 1000)]);
    let mut p = Producer::new();
    p.tick(t0 + 500);
    p.tick(t0 + 1_500);
    let settled = ledger_walks::taken();
    let s = p.tick(t0 + 2_500);
    assert!(
        peaks(&s)["ever"]["models"].get(OTHER_MODEL).is_none(),
        "precondition: the ledger is unread: {}",
        peaks(&s)["ever"]
    );
    for k in 1..=3u64 {
        let end = t0 + 10_000 * k;
        append_two_second_response(&path, end, &format!("msg_w{k}"), 1000 + 100 * k);
        p.tick(end + 500);
    }
    assert_eq!(
        ledger_walks::taken() - settled,
        0,
        "active ticks on an unread ledger"
    );
    std::fs::remove_dir_all(home.home().join(".clauth/token_speed.json")).unwrap();
    write_large_ledger(home.home(), 365);
    let s = p.tick(t0 + 40_500);
    assert_eq!(ledger_walks::taken() - settled, 1, "the reload");
    assert!(
        peaks(&s)["ever"]["models"][OTHER_MODEL]["speed"].is_object(),
        "the reloaded ledger's own model: {}",
        peaks(&s)["ever"]
    );
    for k in 5..=7u64 {
        let end = t0 + 10_000 * k;
        append_two_second_response(&path, end, &format!("msg_w{k}"), 1000 + 100 * k);
        let s = p.tick(end + 500);
        assert_eq!(
            peaks(&s)["ever"]["speed"],
            speed_peak((1000 + 100 * k) as f64 / 2.0, end, "kerry", MODEL),
            "ever moved at active tick {k}"
        );
    }
    assert_eq!(
        ledger_walks::taken() - settled,
        1,
        "active ticks after the reload"
    );
}

/// `ever` written out apart from the producer's walk: per grouping, the first
/// cell at the best (value, at) in one scan over the live days, the held days,
/// then the ledger's, and likewise for the rates; `*` counts toward the total
/// only.
fn ever_from_scratch(p: &Producer) -> PeaksHalf {
    let day_sets = [&p.days, &p.held, &p.speed_ledger.days];
    let rate_sets = [
        &p.rate_peaks,
        &p.held_rate_peaks,
        &p.speed_ledger.rate_peaks,
    ];
    let speed = |keep_account: Option<&str>, keep_model: Option<&str>| {
        let mut best: Option<(PeakAt, &str, &str)> = None;
        for days in day_sets {
            for accounts in days.values() {
                for (account, models) in accounts {
                    if keep_account.is_some_and(|a| a != account.as_str()) {
                        continue;
                    }
                    for (model, sum) in models {
                        if keep_model.is_some_and(|m| m != model.as_str()) {
                            continue;
                        }
                        if let Some(peak) = sum.speed
                            && best.is_none_or(|(b, _, _)| oracle_better(&peak, &b))
                        {
                            best = Some((peak, account.as_str(), model.as_str()));
                        }
                    }
                }
            }
        }
        best.map(|(peak, a, m)| SpeedPeak {
            value: peak.value,
            at: at(peak.at),
            account: (a != UNATTRIBUTED).then(|| a.to_owned()),
            model: m.to_owned(),
        })
    };
    let rate = |keep_account: Option<&str>, keep_model: Option<&str>| {
        let mut best: Option<PeakAt> = None;
        for set in rate_sets {
            for day in set.values() {
                let peak = match (keep_account, keep_model) {
                    (Some(a), _) => day.accounts.get(a),
                    (None, Some(m)) => day.models.get(m),
                    (None, None) => day.total.as_ref(),
                };
                if let Some(peak) = peak
                    && best.is_none_or(|b| oracle_better(peak, &b))
                {
                    best = Some(*peak);
                }
            }
        }
        best.map(|peak| RatePeak {
            value: peak.value,
            at: at(peak.at),
        })
    };
    let mut accounts = BTreeSet::new();
    let mut models = BTreeSet::new();
    for days in day_sets {
        for (account, by_model) in days.values().flatten() {
            accounts.insert(account.as_str());
            models.extend(by_model.keys().map(String::as_str));
        }
    }
    for set in rate_sets {
        for day in set.values() {
            accounts.extend(day.accounts.keys().map(String::as_str));
            models.extend(day.models.keys().map(String::as_str));
        }
    }
    accounts.remove(UNATTRIBUTED);
    let mut half = PeaksHalf {
        speed: speed(None, None),
        rate: rate(None, None),
        ..PeaksHalf::default()
    };
    for a in accounts {
        let g = GroupPeaks {
            speed: speed(Some(a), None),
            rate: rate(Some(a), None),
        };
        if g.speed.is_some() || g.rate.is_some() {
            half.accounts.insert(a.to_owned(), g);
        }
    }
    for m in models {
        let g = GroupPeaks {
            speed: speed(None, Some(m)),
            rate: rate(None, Some(m)),
        };
        if g.speed.is_some() || g.rate.is_some() {
            half.models.insert(m.to_owned(), g);
        }
    }
    half
}

/// The cached `ever` (read through the feed's builder) equals
/// [`ever_from_scratch`] over the producer's state; `snap`, when given, is
/// the tick's own feed, which a tick that folded built before its fold.
fn assert_ever_from_scratch(p: &mut Producer, snap: Option<&Snapshot>, now: u64, what: &str) {
    let want = ever_from_scratch(p);
    if let Some(s) = snap {
        assert_eq!(s.peaks.ever, want, "{what}: the feed");
    }
    assert_eq!(
        p.build_peaks(&day_of_ms(now)).ever,
        want,
        "{what}: the cached ever"
    );
}

/// A stored peak in `token_speed.json`'s wire shape.
fn stored_peak(value: f64, at: u64) -> serde_json::Value {
    serde_json::json!({"value": value, "at": at})
}

/// A stored cell of `output` tokens over 10 s with its speed peak.
fn stored_cell(output: u64, value: f64, at: u64) -> serde_json::Value {
    serde_json::json!({"output": output, "gen_ms": 10_000, "speed": stored_peak(value, at)})
}

/// The ledger holds 09-10 (kerry 150, mommy 110 on `MODEL`) and 09-12 (`*` 50
/// on `MODEL`) with their rate peaks, and on 09-21 itself mommy's stored 200
/// t/s `OTHER_MODEL` cell at T (a previous life's save of a day still live)
/// plus a stored 90 t/s `OTHER_MODEL` rate peak at 18:00, the day of its own
/// `at`; live on 09-21: kerry 2000 over 10 s ending at T (200 t/s, a tie with
/// that stored cell), mommy 120, `*` 90 on `OTHER_MODEL` (its window at
/// 12:00:50 ties the stored 90). The live cell wins the speed tie for the
/// total; mommy and `OTHER_MODEL` keep the stored 200. The rate tie goes to
/// the earlier window, the live 12:00:50, not the stored 18:00. Then more
/// spend, 09-21 held while ledger saves fail, its fold, and spend on 09-23:
/// after every tick `ever` equals the from-scratch walk.
#[test]
fn the_cached_ever_equals_ever_from_scratch_across_live_held_and_stored_days() {
    let home = HomeSandbox::new();
    reg(None);
    let clauth = home.home().join(".clauth");
    std::fs::create_dir_all(&clauth).unwrap();
    let tie = ms("2026-09-21T12:00:10.000Z");
    let a10 = ms("2026-09-10T12:00:10.000Z");
    let a12 = ms("2026-09-12T12:00:10.000Z");
    let a21_late = ms("2026-09-21T18:00:00.000Z");
    let ledger = serde_json::json!({
        "version": 2,
        "days": {
            "2026-09-10": {
                "kerry": {MODEL: stored_cell(1500, 150.0, a10)},
                "mommy": {MODEL: stored_cell(1100, 110.0, a10 + 60_000)},
            },
            "2026-09-12": {
                UNATTRIBUTED: {MODEL: stored_cell(500, 50.0, a12)},
            },
            "2026-09-21": {
                "mommy": {OTHER_MODEL: stored_cell(2000, 200.0, tie)},
            },
        },
        "rate_peaks": {
            "2026-09-10": {
                "total": stored_peak(50.0, a10),
                "accounts": {"kerry": stored_peak(30.0, a10), "mommy": stored_peak(40.0, a10)},
                "models": {MODEL: stored_peak(45.0, a10)},
            },
            "2026-09-12": {
                "total": stored_peak(300.0, a12),
                "accounts": {"mommy": stored_peak(300.0, a12)},
                "models": {MODEL: stored_peak(50.0, a12)},
            },
            "2026-09-21": {
                "models": {OTHER_MODEL: stored_peak(90.0, a21_late)},
            },
        },
    });
    std::fs::write(clauth.join("token_speed.json"), ledger.to_string()).unwrap();
    crate::sessions::stamp_exact_owner("m", "mommy");
    let kerry = iso_kerry(home.home()).join("e.jsonl");
    let mommy = global_store(home.home()).join("m.jsonl");
    let bare = global_store(home.home()).join("g.jsonl");
    write_lines(
        &kerry,
        &[
            user("2026-09-21T12:00:00.000Z", "e"),
            assistant("2026-09-21T12:00:10.000Z", "e", "msg_e1", 2000),
        ],
    );
    write_lines(
        &mommy,
        &[
            user("2026-09-21T12:00:20.000Z", "m"),
            assistant("2026-09-21T12:00:30.000Z", "m", "msg_m1", 1200),
        ],
    );
    write_lines(
        &bare,
        &[
            user("2026-09-21T12:00:40.000Z", "g"),
            assistant_model("2026-09-21T12:00:50.000Z", "g", "msg_g1", OTHER_MODEL, 900),
        ],
    );
    let mut p = Producer::new();
    let now = ms("2026-09-21T12:01:00.000Z");
    let s = p.tick(now);
    let ever = &peaks(&s)["ever"];
    assert_eq!(
        ever["speed"],
        speed_peak(200.0, tie, "kerry", MODEL),
        "the live cell wins the tie: {ever}"
    );
    assert_eq!(
        ever["accounts"]["mommy"]["speed"],
        speed_peak(200.0, tie, "mommy", OTHER_MODEL),
        "mommy keeps the stored cell: {ever}"
    );
    assert_eq!(
        ever["models"][OTHER_MODEL]["speed"],
        speed_peak(200.0, tie, "mommy", OTHER_MODEL),
        "{OTHER_MODEL} keeps the stored cell: {ever}"
    );
    assert_eq!(
        ever["models"][MODEL]["speed"],
        speed_peak(200.0, tie, "kerry", MODEL),
        "{MODEL}: {ever}"
    );
    // The live 10 s window (900 tokens) at 12:00:50 ties the ledger's 90 t/s at 18:00; an equal value keeps the earlier `at` (`peak_better`), so the live one names it.
    assert_eq!(
        ever["models"][OTHER_MODEL]["rate"],
        serde_json::json!({"value": 90.0, "at": at(tie + 40_000)}),
        "{OTHER_MODEL} rate tie: {ever}"
    );
    assert_ever_from_scratch(&mut p, Some(&s), now, "first tick");
    append_lines(
        &kerry,
        &[
            user("2026-09-21T12:02:00.000Z", "e"),
            assistant("2026-09-21T12:02:10.000Z", "e", "msg_e2", 1500),
        ],
    );
    append_lines(
        &bare,
        &[
            user("2026-09-21T12:02:20.000Z", "g"),
            assistant_model("2026-09-21T12:02:30.000Z", "g", "msg_g2", OTHER_MODEL, 1900),
        ],
    );
    let now = ms("2026-09-21T12:03:00.000Z");
    let s = p.tick(now);
    assert_ever_from_scratch(&mut p, Some(&s), now, "more spend");
    block_ledger_saves(home.home());
    let leave = ms("2026-09-23T00:05:00.000Z");
    p.tick(leave);
    assert!(
        p.held.contains_key("2026-09-21"),
        "precondition: 09-21 held"
    );
    assert_ever_from_scratch(&mut p, None, leave, "held");
    let s = p.tick(leave + 60_000);
    assert_ever_from_scratch(&mut p, Some(&s), leave + 60_000, "still held");
    std::fs::remove_dir_all(clauth.join("token_speed.json")).unwrap();
    p.tick(leave + 120_000);
    assert!(
        !p.held.contains_key("2026-09-21"),
        "precondition: 09-21 folded"
    );
    assert_eq!(
        ledger_day_total(home.home(), "2026-09-21"),
        7500,
        "09-21 stored"
    );
    assert_ever_from_scratch(&mut p, None, leave + 120_000, "folded");
    let s = p.tick(leave + 180_000);
    assert_ever_from_scratch(&mut p, Some(&s), leave + 180_000, "after the fold");
    append_lines(
        &kerry,
        &[
            user("2026-09-23T00:08:00.000Z", "e"),
            assistant("2026-09-23T00:08:10.000Z", "e", "msg_e3", 1200),
        ],
    );
    append_lines(
        &mommy,
        &[
            user("2026-09-23T00:08:20.000Z", "m"),
            assistant("2026-09-23T00:08:30.000Z", "m", "msg_m3", 2500),
        ],
    );
    let now = ms("2026-09-23T00:09:00.000Z");
    let s = p.tick(now);
    assert_eq!(
        peaks(&s)["ever"]["speed"],
        speed_peak(250.0, ms("2026-09-23T00:08:30.000Z"), "mommy", MODEL),
        "a new live peak"
    );
    assert_ever_from_scratch(&mut p, Some(&s), now, "spend after the fold");
}

/// The ledger holds 09-21 as this life derives it (kerry 2000 over 10 s ending
/// at T, on `MODEL`) plus anna's stored 200 t/s `OTHER_MODEL` cell at T, two
/// sources' cells ending on that day's one millisecond. While 09-21 is live
/// its own cell wins the tie; once it leaves the window, its fold changes
/// nothing and drops the held day, and the next tick's `ever`, an idle one,
/// names the first stored cell at T, anna's.
#[test]
fn a_held_day_an_unchanged_fold_drops_leaves_ever_at_once() {
    let home = HomeSandbox::new();
    reg(None);
    let clauth = home.home().join(".clauth");
    std::fs::create_dir_all(&clauth).unwrap();
    let tie = ms("2026-09-21T12:00:10.000Z");
    let high = stored_peak(10_000.0, tie);
    let ledger = serde_json::json!({
        "version": 2,
        "days": {
            "2026-09-21": {
                "anna": {OTHER_MODEL: stored_cell(2000, 200.0, tie)},
                "kerry": {MODEL: stored_cell(2000, 200.0, tie)},
            },
        },
        "rate_peaks": {
            "2026-09-21": {
                "total": high,
                "accounts": {"kerry": high},
                "models": {MODEL: high},
            },
        },
    });
    std::fs::write(clauth.join("token_speed.json"), ledger.to_string()).unwrap();
    write_lines(
        &iso_kerry(home.home()).join("e.jsonl"),
        &[
            user("2026-09-21T12:00:00.000Z", "e"),
            assistant("2026-09-21T12:00:10.000Z", "e", "msg_e1", 2000),
        ],
    );
    let mut p = Producer::new();
    let s = p.tick(ms("2026-09-21T12:01:00.000Z"));
    assert_eq!(
        peaks(&s)["ever"]["speed"],
        speed_peak(200.0, tie, "kerry", MODEL),
        "live: the live cell wins the tie"
    );
    let stored = std::fs::read(clauth.join("token_speed.json")).unwrap();
    let leave = ms("2026-09-23T00:05:00.000Z");
    p.tick(leave);
    assert!(
        !p.held.contains_key("2026-09-21"),
        "precondition: 09-21 dropped"
    );
    assert_eq!(
        std::fs::read(clauth.join("token_speed.json")).unwrap(),
        stored,
        "precondition: the fold saved nothing"
    );
    let s = p.tick(leave + 60_000);
    assert_eq!(
        peaks(&s)["ever"]["speed"],
        speed_peak(200.0, tie, "anna", OTHER_MODEL),
        "stored: the first stored cell at T"
    );
    assert_eq!(s.peaks.ever, ever_from_scratch(&p), "from scratch");
}

// ── the producer files' corrupt asides are owner-only ──

/// The mode bits of the one `name.corrupt-<ms>` aside in `clauth`.
#[cfg(unix)]
fn aside_mode(clauth: &Path, name: &str) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    let prefix = format!("{name}.corrupt-");
    let asides: Vec<_> = std::fs::read_dir(clauth)
        .unwrap()
        .filter_map(|e| e.ok()?.file_name().into_string().ok())
        .filter(|n| n.starts_with(&prefix))
        .collect();
    assert_eq!(asides.len(), 1, "one aside: {asides:?}");
    std::fs::metadata(clauth.join(&asides[0]))
        .unwrap()
        .permissions()
        .mode()
        & 0o777
}

/// `name` in `clauth` holding bytes that do not parse, at 0644 (as an editor
/// or a `cp` under umask 022 leaves it), so a rename alone would keep it
/// world-readable.
#[cfg(unix)]
fn world_readable_corrupt(clauth: &Path, name: &str) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(clauth).unwrap();
    let path = clauth.join(name);
    std::fs::write(&path, b"{not json").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o644, "precondition: the corrupt file is 0644");
}

/// A 0644 owed-day list that does not parse is moved aside 0600.
#[cfg(unix)]
#[test]
fn a_world_readable_corrupt_owed_day_list_is_moved_aside_owner_only() {
    let home = HomeSandbox::new();
    reg(None);
    let clauth = home.home().join(".clauth");
    world_readable_corrupt(&clauth, TPS_DAYS_FILE);
    write_lines(
        &iso_kerry(home.home()).join("d.jsonl"),
        &[user(START, "d"), assistant(END, "d", "msg_d", 1222)],
    );
    Producer::new().tick(ms(END) + 1_000);
    assert_eq!(
        aside_mode(&clauth, TPS_DAYS_FILE),
        0o600,
        "the aside is 0600"
    );
}

/// A 0644 attribution file that does not parse is moved aside 0600.
#[cfg(unix)]
#[test]
fn a_world_readable_corrupt_attribution_file_is_moved_aside_owner_only() {
    let home = HomeSandbox::new();
    reg(None);
    let clauth = home.home().join(".clauth");
    world_readable_corrupt(&clauth, TPS_ATTRIBUTION_FILE);
    write_lines(
        &iso_kerry(home.home()).join("c.jsonl"),
        &[user(START, "c"), assistant(END, "c", "msg_c", 1222)],
    );
    Producer::new().tick(ms(END) + 1_000);
    assert_eq!(
        aside_mode(&clauth, TPS_ATTRIBUTION_FILE),
        0o600,
        "the aside is 0600"
    );
}

// ── spend-cap line order: the transcript read life ──────────────────────────

/// A timestamp-less progress line with `chars` bytes of filler: a JSON-valid
/// line `parse_line` ignores, for growing or shrinking a transcript without a
/// fact line.
fn progress_pad(chars: usize) -> String {
    serde_json::json!({"type": "progress", "pad": "z".repeat(chars)}).to_string()
}

/// The read life the producer holds for `path`.
fn file_life(p: &Producer, path: &Path) -> Option<FileLife> {
    p.files.get(path).map(|entry| entry.life)
}

/// Flip one byte inside the last line's filler, keeping the file's length and
/// inode: the stored tail hash no longer matches the bytes it covers.
fn flip_a_pad_byte(path: &Path) {
    let mut bytes = std::fs::read(path).unwrap();
    let at = bytes
        .iter()
        .position(|&b| b == b'z')
        .expect("the pad's filler byte");
    bytes[at] = b'y';
    std::fs::write(path, bytes).unwrap();
}

/// `life` names a file's read life; a step-back re-read whose verified bytes
/// still match keeps it, a re-read over changed bytes starts a new one.
#[test]
fn a_verified_step_back_reread_keeps_the_files_life() {
    let home = HomeSandbox::new();
    let path = global_store(home.home()).join("life-stepback.jsonl");
    write_lines(&path, &[user(START, "life-stepback"), progress_pad(120)]);
    let mut p = Producer::new();
    let now = ms(END) + 62_000;
    p.tick(now);
    let life = file_life(&p, &path).expect("the file joined the walk");
    let armed = p.whole_since;
    p.tick(now - DAY_MS);
    assert_ne!(
        p.whole_since, armed,
        "the step back armed a byte-0 re-read of the hot set"
    );
    assert_eq!(
        file_life(&p, &path),
        Some(life),
        "the verified re-read keeps the life: the bytes before the old offset still hash to the stored tail"
    );
}

/// The other side of the verification: the same step-back re-read over bytes
/// that no longer hash to the stored tail starts a new life, so no position
/// from the old life is compared against the new bytes.
#[test]
fn a_step_back_reread_over_changed_bytes_starts_a_new_life() {
    let home = HomeSandbox::new();
    let path = global_store(home.home()).join("life-changed.jsonl");
    write_lines(&path, &[user(START, "life-changed"), progress_pad(120)]);
    let mut p = Producer::new();
    let now = ms(END) + 62_000;
    p.tick(now);
    let life = file_life(&p, &path).expect("the file joined the walk");
    flip_a_pad_byte(&path);
    p.tick(now - DAY_MS);
    assert_ne!(
        file_life(&p, &path),
        Some(life),
        "changed bytes before the old offset start a new life"
    );
}

/// A shrink cannot vouch for the bytes past the new length: the next read
/// starts a new life.
#[test]
fn a_shrink_starts_a_new_file_life() {
    let home = HomeSandbox::new();
    let path = global_store(home.home()).join("life-shrink.jsonl");
    write_lines(
        &path,
        &[
            user(START, "life-shrink"),
            assistant(END, "life-shrink", "msg_shrink", 1222),
        ],
    );
    let mut p = Producer::new();
    let now = ms(END) + 62_000;
    p.tick(now);
    let life = file_life(&p, &path).expect("the file joined the walk");
    write_lines(&path, &[user(START, "life-shrink")]);
    p.tick(now + 10_000);
    assert_ne!(
        file_life(&p, &path),
        Some(life),
        "a shrink starts a new life"
    );
}

/// A replacement at the same path is a different file identity even at the
/// same length: the next read starts a new life.
#[test]
fn an_identity_change_starts_a_new_file_life() {
    let home = HomeSandbox::new();
    let path = global_store(home.home()).join("life-ident.jsonl");
    write_lines(
        &path,
        &[
            user(START, "life-ident"),
            assistant(END, "life-ident", "msg_ident", 1222),
        ],
    );
    let mut p = Producer::new();
    let now = ms(END) + 62_000;
    p.tick(now);
    let life = file_life(&p, &path).expect("the file joined the walk");
    let same = std::fs::read(&path).unwrap();
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, same).unwrap();
    std::fs::rename(&tmp, &path).unwrap();
    p.tick(now + 10_000);
    assert_ne!(
        file_life(&p, &path),
        Some(life),
        "an identity change starts a new life"
    );
}

/// An in-place rewrite that keeps the length growing but changes the bytes
/// before the old offset reads as rewritten: the retry from byte 0 starts a
/// new life.
#[test]
fn a_rewrite_starts_a_new_file_life() {
    let home = HomeSandbox::new();
    let path = global_store(home.home()).join("life-rewrite.jsonl");
    write_lines(
        &path,
        &[
            user(START, "life-rewrite"),
            assistant(END, "life-rewrite", "msg_rewrite", 1222),
        ],
    );
    let mut p = Producer::new();
    let now = ms(END) + 62_000;
    p.tick(now);
    let life = file_life(&p, &path).expect("the file joined the walk");
    write_lines(
        &path,
        &[
            progress_pad(400),
            user(&iso(ms(END) + 1_000), "life-rewrite"),
        ],
    );
    p.tick(now + 10_000);
    assert_ne!(
        file_life(&p, &path),
        Some(life),
        "a rewrite starts a new life"
    );
}

/// A transcript that leaves one store walk and returns is a new entry: its
/// read starts a new life.
#[test]
fn a_walk_miss_starts_a_new_file_life() {
    let home = HomeSandbox::new();
    let path = global_store(home.home()).join("life-walk.jsonl");
    write_lines(&path, &[user(START, "life-walk"), progress_pad(40)]);
    let mut p = Producer::new();
    let now = ms(END) + 62_000;
    p.tick(now);
    let life = file_life(&p, &path).expect("the file joined the walk");
    let away = path.with_extension("away");
    std::fs::rename(&path, &away).unwrap();
    p.tick(now + 6_000);
    std::fs::rename(&away, &path).unwrap();
    p.tick(now + 12_000);
    assert_ne!(
        file_life(&p, &path),
        Some(life),
        "a walk miss re-entry starts a new life"
    );
}

/// A deduped 429 line re-read in a new life of the same path moves the
/// position its latch pulls, and never re-latches: the scheduler writes that
/// position onto the latch holding its uuid before it judges any replayed
/// success.
#[test]
fn a_deduped_429_re_read_in_a_new_life_moves_the_pulled_position_and_mints_no_pending() {
    let home = HomeSandbox::new();
    let path = global_store(home.home()).join("life-reloc.jsonl");
    let cap_ts = ms(END) + 2_000;
    let uuid = format!("429-{}", iso(cap_ts));
    let cap = spend_cap_429_uuid(&iso(cap_ts));
    let head = user(START, "life-reloc");
    let cap_end = (head.len() + 1 + cap.len() + 1) as u64;
    write_lines(&path, &[head.clone(), cap.clone(), progress_pad(20)]);
    let mut p = Producer::new();
    let now = ms(END) + 62_000;
    p.tick(now);
    assert_eq!(
        p.drain_spend_cap().pending.get("life-reloc").map(|f| f.ts),
        Some(cap_ts),
        "the 429 delivers once"
    );
    let life = file_life(&p, &path).expect("the file joined the walk");
    write_lines(&path, &[head, cap]);
    p.tick(now + 10_000);
    let new_life = file_life(&p, &path).expect("the file stayed in the walk");
    assert_ne!(new_life, life, "the shrink starts a new life");
    let drained = p.drain_spend_cap();
    assert!(
        drained.pending.is_empty(),
        "the deduped 429 re-latched: {:?}",
        drained.pending
    );
    assert_eq!(
        p.spend_cap_position("life-reloc", &uuid, &path),
        Some((new_life, cap_end)),
        "the delivered line's position follows the read into the new life"
    );
}

/// A drained 429 line re-read at another path (a rescue copy) never moves the
/// position a latch pulls: the producer answers nothing at the new path, and
/// the line's original record stays where it was, because a latch is never
/// moved to a different transcript file.
#[test]
fn a_rescue_move_moves_no_pulled_position() {
    let home = HomeSandbox::new();
    reg(None);
    let src = iso_kerry(home.home()).join("life-rescue.jsonl");
    let uuid = format!("429-{}", iso(ms(END)));
    write_lines(
        &src,
        &[
            user(START, "life-rescue"),
            spend_cap_429_uuid(&iso(ms(END))),
        ],
    );
    let mut p = Producer::new();
    let now = ms(END) + 62_000;
    p.tick(now);
    assert_eq!(
        p.drain_spend_cap().pending.get("life-rescue").map(|f| f.ts),
        Some(ms(END)),
        "the 429 delivers once"
    );
    let dst = global_store(home.home()).join("life-rescue.rescued-1.jsonl");
    std::fs::create_dir_all(dst.parent().unwrap()).unwrap();
    std::fs::rename(&src, &dst).unwrap();
    p.tick(now + 6_000);
    assert!(
        p.drain_spend_cap().pending.is_empty(),
        "the rescue re-delivered the drained 429"
    );
    assert_eq!(
        p.spend_cap_position("life-rescue", &uuid, &dst),
        None,
        "a rescue copy must not move a latch to its new path"
    );
    assert!(
        p.spend_cap_position("life-rescue", &uuid, &src).is_some(),
        "the rescue copy left the original line's record alone"
    );
}

/// A read whose position bound skips an already-delivered 429 line still moves
/// that line's recorded position: the rewrite retry reads from byte 0 under the
/// old offset, and the latch pulls the line's new-life position while its own
/// delivery stays deduped.
#[test]
fn a_bound_skipped_reread_moves_a_delivered_429s_position() {
    let home = HomeSandbox::new();
    let path = global_store(home.home()).join("life-bound.jsonl");
    let cap_ts = ms(END) + 2_000;
    let uuid = format!("429-{}", iso(cap_ts));
    let cap = spend_cap_429_uuid(&iso(cap_ts));
    let head = user(START, "life-bound");
    let cap_end = (head.len() + 1 + cap.len() + 1) as u64;
    write_lines(&path, &[head.clone(), cap.clone(), progress_pad(20)]);
    let mut p = Producer::new();
    let now = ms(END) + 62_000;
    p.tick(now);
    assert_eq!(
        p.drain_spend_cap().pending.get("life-bound").map(|f| f.ts),
        Some(cap_ts),
        "the 429 delivers once"
    );
    let life = file_life(&p, &path).expect("the file joined the walk");
    // Grow the file while changing bytes before the old offset: the tail hash
    // fails, and the byte-0 retry keeps the old offset as its bound, which
    // skips the 429 line.
    write_lines(
        &path,
        &[
            head,
            cap,
            progress_pad(20),
            assistant(&iso(ms(END) + 3_000), "life-bound", "msg_bound", 400),
        ],
    );
    let mut bytes = std::fs::read(&path).unwrap();
    let at = bytes
        .iter()
        .position(|&b| b == b'z')
        .expect("the pad's filler byte");
    bytes[at] = b'y';
    std::fs::write(&path, bytes).unwrap();
    p.tick(now + 10_000);
    let new_life = file_life(&p, &path).expect("the file stayed in the walk");
    assert_ne!(new_life, life, "the rewrite starts a new life");
    assert_eq!(
        p.spend_cap_position("life-bound", &uuid, &path),
        Some((new_life, cap_end)),
        "the bound that skips the line still moves its recorded position"
    );
}

/// The other side: a bound that skips a 429 line this conversation never
/// delivered records nothing, so the line stays available to a later read that
/// passes the bound (here a shrink). Recording it would suppress a real 429.
#[test]
fn a_bound_skipped_undelivered_429_is_not_recorded_and_later_delivers() {
    let home = HomeSandbox::new();
    let path = global_store(home.home()).join("life-skip.jsonl");
    let cap_ts = ms(END) + 2_000;
    let uuid = format!("429-{}", iso(cap_ts));
    let cap = spend_cap_429_uuid(&iso(cap_ts));
    let head = user(START, "life-skip");
    write_lines(&path, &[head.clone(), progress_pad(600)]);
    let mut p = Producer::new();
    let now = ms(END) + 62_000;
    p.tick(now);
    assert!(
        p.drain_spend_cap().pending.is_empty(),
        "the first read carries no 429"
    );
    let old = std::fs::metadata(&path).unwrap().len();
    // Rewrite to [user, 429, pad] longer than the old offset: the rewrite
    // retry reads from byte 0 under that offset and skips the 429 line.
    write_lines(&path, &[head, cap.clone(), progress_pad(600)]);
    assert!(
        std::fs::metadata(&path).unwrap().len() > old,
        "the rewrite keeps the old offset as the bound"
    );
    p.tick(now + 10_000);
    assert_eq!(
        p.spend_cap_position("life-skip", &uuid, &path),
        None,
        "a line the bound skipped was never delivered, so nothing is recorded"
    );
    let drained = p.drain_spend_cap();
    assert!(
        drained.pending.is_empty(),
        "a line the bound skipped does not deliver: {:?}",
        drained.pending
    );
    // A shrink re-reads from byte 0 with no bound: the line delivers now.
    write_lines(&path, &[cap]);
    p.tick(now + 20_000);
    assert_eq!(
        p.drain_spend_cap().pending.get("life-skip").map(|f| f.ts),
        Some(cap_ts),
        "the skipped line still delivers once a read passes the bound"
    );
}

/// One conversation's pending 429s are kept per transcript file, and the drain
/// hands over each file's later line before it picks across files: the highest
/// stamp wins, ties broken by path. A total order, so the same facts pick the
/// same 429 whatever order the reads inserted the files in.
#[test]
fn two_files_429s_pick_the_same_429_in_either_insertion_order() {
    let fact = |file: &str, ts: u64, uuid: &str| SpendCap429Fact {
        ts,
        file: PathBuf::from(file),
        life: FileLife(1),
        end: 0,
        uuid: Some(uuid.to_owned()),
    };
    let x = fact("/p/x.jsonl", 10, "x");
    let y = fact("/p/y.jsonl", 5, "y");
    for order in [[&x, &y], [&y, &x]] {
        let mut files: std::collections::HashMap<PathBuf, SpendCap429Fact> =
            std::collections::HashMap::new();
        for f in order {
            files.insert(f.file.clone(), f.clone());
        }
        assert_eq!(
            pending_winner(&files).and_then(|f| f.uuid.as_deref()),
            Some("x"),
            "the higher stamp wins in either insertion order"
        );
    }
    let a = fact("/p/a.jsonl", 5, "a");
    let b = fact("/p/b.jsonl", 5, "b");
    // The comparator itself, in both argument orders: a removed path tie-break
    // answers `Equal` here on every run, whatever the map order.
    assert_eq!(pending_order(&a, &b), std::cmp::Ordering::Less);
    assert_eq!(pending_order(&b, &a), std::cmp::Ordering::Greater);
    // Each fresh map gets its own `RandomState`, so a tie-break-less pick
    // follows a whole map's iteration order: over 64 maps a removed path
    // tie-break survives with probability at most 2^-64. This half pins the
    // pick, so a `pending_winner` that skips the comparator still reds.
    for _ in 0..64 {
        let mut files: std::collections::HashMap<PathBuf, SpendCap429Fact> =
            std::collections::HashMap::new();
        files.insert(a.file.clone(), a.clone());
        files.insert(b.file.clone(), b.clone());
        assert_eq!(
            pending_winner(&files).and_then(|f| f.uuid.as_deref()),
            Some("b"),
            "an equal stamp breaks by path"
        );
    }
}

// ── a re-taken producer re-joins the shared files ────────────────────────────

fn retake_m_turn(home: &HomeSandbox) {
    write_lines(
        &iso_kerry(home.home()).join("m.jsonl"),
        &[
            user("2026-09-22T12:00:00.000Z", "m"),
            assistant("2026-09-22T12:00:20.000Z", "m", "msg_m", 3000),
            user("2026-09-22T12:05:00.000Z", "m"),
        ],
    );
}

/// A loses the lease, B holds and folds 09-22, B exits, A re-joins its files
/// and folds 09-21: the day B folded survives A's next save.
#[test]
fn a_retaken_producer_keeps_a_day_another_holder_folded() {
    let home = HomeSandbox::new();
    write_kerry_turn(&home);
    let mut a = Producer::new();
    a.tick(ms("2026-09-21T12:06:00.000Z"));
    retake_m_turn(&home);
    let mut b = Producer::new();
    b.tick(ms("2026-09-22T12:06:00.000Z"));
    b.tick(ms("2026-09-24T00:00:10.000Z"));
    let folded_by_b = ledger_day_output(home.home(), "2026-09-22");
    assert_eq!(folded_by_b, 3000, "fixture control: B folded 09-22");
    drop(b);
    a.rejoin_files(ms("2026-09-24T00:00:20.000Z"));
    a.tick(ms("2026-09-24T00:00:20.000Z"));
    assert_eq!(
        ledger_day_output(home.home(), "2026-09-21"),
        2000,
        "fixture control: A folded its own 09-21"
    );
    assert_eq!(
        ledger_day_output(home.home(), "2026-09-22"),
        3000,
        "A's re-take dropped the day B folded"
    );
}

/// Same shape on a fresh-life control: a Producer loaded after B's fold keeps it.
#[test]
fn a_fresh_producer_keeps_a_day_another_holder_folded() {
    let home = HomeSandbox::new();
    write_kerry_turn(&home);
    Producer::new().tick(ms("2026-09-21T12:06:00.000Z"));
    retake_m_turn(&home);
    let mut b = Producer::new();
    b.tick(ms("2026-09-22T12:06:00.000Z"));
    b.tick(ms("2026-09-24T00:00:10.000Z"));
    assert_eq!(ledger_day_output(home.home(), "2026-09-22"), 3000);
    drop(b);
    let mut a = Producer::new();
    a.tick(ms("2026-09-24T00:00:20.000Z"));
    assert_eq!(
        ledger_day_output(home.home(), "2026-09-22"),
        3000,
        "control: a fresh life keeps it"
    );
}

/// B dies owing 09-22 (it left no fold); A re-joins and keeps the owed day.
#[test]
fn a_retaken_producer_keeps_a_day_another_holder_owed() {
    let home = HomeSandbox::new();
    write_kerry_turn(&home);
    let clauth = home.home().join(".clauth");
    let mut a = Producer::new();
    a.tick(ms("2026-09-21T12:06:00.000Z"));
    retake_m_turn(&home);
    let mut b = Producer::new();
    b.tick(ms("2026-09-22T12:06:00.000Z"));
    drop(b);
    let owed_before = owed_file(&clauth)["owed"].to_string();
    assert!(
        owed_before.contains("2026-09-22"),
        "control: B owes 09-22: {owed_before}"
    );
    a.rejoin_files(ms("2026-09-25T00:00:20.000Z"));
    a.tick(ms("2026-09-25T00:00:20.000Z"));
    let owed_after = owed_file(&clauth)["owed"].to_string();
    let mut c = Producer::new();
    c.tick(ms("2026-09-25T00:00:30.000Z"));
    assert!(
        owed_after.contains("2026-09-22") || ledger_day_output(home.home(), "2026-09-22") == 3000,
        "A's re-take dropped the day B owed: before {owed_before}, after {owed_after}, ledger {}",
        ledger_day_output(home.home(), "2026-09-22")
    );
}

/// A re-take whose timeline reload cannot read the file keeps the
/// pre-stand-down copy only to refuse its saves: the unsaved entries of a life
/// whose save failed never land over the other holder's file, and a readable
/// reload still replaces the copy.
#[cfg(unix)]
#[test]
fn a_retake_whose_timeline_reload_is_unreadable_writes_nothing_over_the_file() {
    use std::os::unix::fs::PermissionsExt;
    // The named guard: root ignores permission bits, so the chmod-000 leg
    // cannot fail and this test proves nothing.
    if unsafe { libc::geteuid() } == 0 {
        eprintln!("skipped: root ignores permission bits");
        return;
    }
    let home = HomeSandbox::new();
    write_kerry_turn(&home);
    let clauth = home.home().join(".clauth");
    std::fs::create_dir_all(&clauth).unwrap();

    // A's pass cannot save (a .clauth that refuses new files), so A holds its
    // session's entry as unsaved.
    let mut a = Producer::new();
    {
        let _unwritable = crate::testutil::UnwritableDir::new(&clauth);
        a.tick(ms("2026-09-21T12:06:00.000Z"));
    }
    assert!(
        a.timeline.unsaved,
        "fixture control: A's timeline save failed"
    );
    assert!(
        !a.timeline.by_session.is_empty(),
        "fixture control: A holds an unsaved entry"
    );

    // Another holder writes its own timeline after A stood down.
    retake_m_turn(&home);
    let mut b = Producer::new();
    b.tick(ms("2026-09-22T12:06:00.000Z"));
    drop(b);
    let tl = clauth.join(TPS_ATTRIBUTION_FILE);
    let theirs = std::fs::read(&tl).expect("B wrote the timeline");
    assert!(!theirs.is_empty(), "fixture control: B's file");

    // The re-take's reload cannot read the file, then a save pass runs while
    // it still cannot: a write would land (the publish is a rename over it).
    std::fs::set_permissions(&tl, std::fs::Permissions::from_mode(0o000)).unwrap();
    a.rejoin_files(ms("2026-09-24T00:00:20.000Z"));
    a.tick(ms("2026-09-24T00:00:20.000Z"));
    std::fs::set_permissions(&tl, std::fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(
        std::fs::read(&tl).unwrap(),
        theirs,
        "A wrote its stale timeline over the other holder's file"
    );

    // A readable reload still replaces the kept copy.
    a.rejoin_files(ms("2026-09-24T00:00:30.000Z"));
    assert_eq!(
        a.timeline.disk,
        DiskState::Usable,
        "the readable reload is kept"
    );
    assert!(
        !a.timeline.unsaved,
        "A's unsaved entries drop with the old copy"
    );
    assert_eq!(
        std::fs::read(&tl).unwrap(),
        theirs,
        "the reload itself writes nothing"
    );
}

// ── the speed-ledger gate on a spent ticket ──────────────────────────────────

/// Doc on tick_leased: "checked before every write this pass makes (... the
/// speed ledger)". An owed-day rebuild probes `saves_land`, which itself
/// writes `token_speed.json` — a spent ticket must not.
#[test]
fn a_rebuild_under_a_spent_ticket_writes_no_speed_ledger() {
    let home = HomeSandbox::new();
    let clauth = home.home().join(".clauth");
    std::fs::create_dir_all(&clauth).unwrap();
    std::fs::write(
        clauth.join(TPS_DAYS_FILE),
        br#"{"version":1,"owed":["2026-09-20"]}"#,
    )
    .unwrap();
    let lease = crate::daemon::FetchLease::new();
    let spent = crate::testutil::spent_lease_ticket(&lease);
    Producer::new().tick_leased(ms("2026-09-22T12:06:00.000Z"), Some(spent));
    assert!(
        !clauth.join("token_speed.json").exists(),
        "an owed-day rebuild under a spent ticket wrote token_speed.json"
    );

    // Control: the same rebuild under a live (ticket-less) pass writes it.
    Producer::new().tick_leased(ms("2026-09-22T12:06:00.000Z"), None);
    assert!(
        clauth.join("token_speed.json").exists(),
        "control: a live rebuild writes the speed ledger"
    );
}
