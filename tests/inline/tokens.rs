use super::*;

use std::io::Write as _;
use std::time::{Duration, SystemTime};

use crate::pricing::HourTokens;
use crate::testutil::{HomeSandbox, set_mtime};

// ── helpers ──────────────────────────────────────────────────────────────────

fn write_stats_cache(claude_dir: &std::path::Path, json: &str) {
    std::fs::write(claude_dir.join("stats-cache.json"), json).expect("write stats-cache");
}

fn make_claude_dir(sandbox: &HomeSandbox) -> std::path::PathBuf {
    let dir = sandbox.home().join(".claude");
    std::fs::create_dir_all(&dir).expect("create .claude");
    dir
}

// ── 1. base stats parsing ─────────────────────────────────────────────────────

#[test]
fn base_stats_parsed_correctly() {
    let sb = HomeSandbox::new();
    let claude_dir = make_claude_dir(&sb);

    write_stats_cache(
        &claude_dir,
        r#"{
            "version": 1,
            "lastComputedDate": "2026-06-10",
            "firstSessionDate": "2025-01-01T00:00:00+00:00",
            "totalSessions": 42,
            "totalMessages": 1000,
            "dailyActivity": [
                {"date": "2026-06-09", "messageCount": 50, "sessionCount": 3, "toolCallCount": 120},
                {"date": "2026-06-10", "messageCount": 70, "sessionCount": 4, "toolCallCount": 200}
            ],
            "dailyModelTokens": [
                {"date": "2026-06-09", "tokensByModel": {"claude-opus-4": 5000, "gpt-5": 1000}},
                {"date": "2026-06-10", "tokensByModel": {"claude-opus-4": 8000}}
            ],
            "modelUsage": {
                "claude-opus-4": {
                    "inputTokens": 10000,
                    "outputTokens": 5000,
                    "cacheReadInputTokens": 2000,
                    "cacheCreationInputTokens": 500
                },
                "gpt-5": {
                    "inputTokens": 3000,
                    "outputTokens": 1000,
                    "cacheReadInputTokens": 0,
                    "cacheCreationInputTokens": 0
                }
            },
            "hourCounts": {"0": 10, "12": 200, "23": 50}
        }"#,
    );

    let stats = load(&claude_dir).expect("load must succeed");

    // models sorted DESC by total
    assert_eq!(stats.models.len(), 2);
    assert_eq!(stats.models[0].model, "claude-opus-4");
    assert_eq!(stats.models[0].input, 10000);
    assert_eq!(stats.models[0].output, 5000);
    assert_eq!(stats.models[0].cache_read, 2000);
    assert_eq!(stats.models[0].cache_create, 500);
    assert_eq!(stats.models[1].model, "gpt-5");

    // daily summed across models, sorted ASC
    assert_eq!(stats.daily.len(), 2);
    assert_eq!(stats.daily[0].date, "2026-06-09");
    assert_eq!(stats.daily[0].tokens, 6000); // 5000 + 1000
    assert_eq!(stats.daily[1].date, "2026-06-10");
    assert_eq!(stats.daily[1].tokens, 8000);

    // activity sorted ASC
    assert_eq!(stats.activity.len(), 2);
    assert_eq!(stats.activity[0].date, "2026-06-09");
    assert_eq!(stats.activity[0].messages, 50);
    assert_eq!(stats.activity[0].sessions, 3);
    assert_eq!(stats.activity[0].tool_calls, 120);

    // hour_counts: present keys mapped, absent keys = 0
    assert_eq!(stats.hour_counts[0], 10);
    assert_eq!(stats.hour_counts[12], 200);
    assert_eq!(stats.hour_counts[23], 50);
    assert_eq!(stats.hour_counts[1], 0);
    assert_eq!(stats.hour_counts[11], 0);

    // totals
    assert_eq!(stats.total_input, 13000);
    assert_eq!(stats.total_output, 6000);
    assert_eq!(stats.total_cache_read, 2000);
    assert_eq!(stats.total_cache_create, 500);
    assert_eq!(stats.total_sessions, 42);
    assert_eq!(stats.total_messages, 1000);
    assert_eq!(
        stats.first_session_date.as_deref(),
        Some("2025-01-01T00:00:00+00:00")
    );
    assert_eq!(stats.last_computed_date.as_deref(), Some("2026-06-10"));
    assert!(stats.topped_up_through.is_none());
}

#[test]
fn load_returns_none_when_cache_absent() {
    let sb = HomeSandbox::new();
    let claude_dir = make_claude_dir(&sb);
    // no stats-cache.json written
    assert!(load(&claude_dir).is_none());
}

// ── 2. group_models ───────────────────────────────────────────────────────────

#[test]
fn group_models_keeps_claude_individual_folds_others() {
    let models = vec![
        ModelTokens {
            model: "claude-opus-4-8".to_owned(),
            input: 1000,
            output: 500,
            cache_read: 100,
            cache_create: 50,
        },
        ModelTokens {
            model: "gpt-5.5".to_owned(),
            input: 200,
            output: 100,
            cache_read: 0,
            cache_create: 0,
        },
        ModelTokens {
            model: "gemini-3-flash".to_owned(),
            input: 300,
            output: 150,
            cache_read: 0,
            cache_create: 0,
        },
        ModelTokens {
            model: "claude-sonnet-4".to_owned(),
            input: 500,
            output: 250,
            cache_read: 50,
            cache_create: 25,
        },
    ];

    let grouped = group_models(&models);

    // claude models individual, others folded
    let claude_rows: Vec<_> = grouped
        .iter()
        .filter(|m| m.model.starts_with("claude"))
        .collect();
    let others_rows: Vec<_> = grouped.iter().filter(|m| m.model == "others").collect();
    assert_eq!(claude_rows.len(), 2);
    assert_eq!(others_rows.len(), 1);

    let others = &others_rows[0];
    assert_eq!(others.input, 500); // 200 + 300
    assert_eq!(others.output, 250); // 100 + 150

    // sorted DESC by in+out (the dashboard basis)
    let in_outs: Vec<u64> = grouped.iter().map(|m| m.in_out()).collect();
    for pair in in_outs.windows(2) {
        assert!(pair[0] >= pair[1], "not sorted desc: {pair:?}");
    }
}

#[test]
fn group_models_no_others_when_all_claude() {
    let models = vec![ModelTokens {
        model: "claude-opus-4".to_owned(),
        input: 100,
        output: 50,
        cache_read: 0,
        cache_create: 0,
    }];
    let grouped = group_models(&models);
    assert!(grouped.iter().all(|m| m.model != "others"));
}

#[test]
fn group_models_empty_input() {
    assert!(group_models(&[]).is_empty());
}

#[test]
fn group_models_breaks_out_large_non_anthropic() {
    let models = vec![
        // > 1M total → shown individually even though non-Anthropic.
        ModelTokens {
            model: "gpt-5.5".to_owned(),
            input: 2_000_000,
            output: 100_000,
            cache_read: 0,
            cache_create: 0,
        },
        // < 1M total → folds into "others".
        ModelTokens {
            model: "tiny-model".to_owned(),
            input: 100,
            output: 50,
            cache_read: 0,
            cache_create: 0,
        },
        ModelTokens {
            model: "claude-opus-4-8".to_owned(),
            input: 500,
            output: 250,
            cache_read: 0,
            cache_create: 0,
        },
    ];
    let grouped = group_models(&models);
    assert!(
        grouped.iter().any(|m| m.model == "gpt-5.5"),
        "a >1M non-Anthropic model must show separately"
    );
    assert!(grouped.iter().any(|m| m.model == "claude-opus-4-8"));
    let others = grouped
        .iter()
        .find(|m| m.model == "others")
        .expect("the tiny model must fold into others");
    assert_eq!(others.in_out(), 150); // only tiny-model (100 + 50)
}

// ── 3. is_anthropic ───────────────────────────────────────────────────────────

#[test]
fn is_anthropic_recognition() {
    assert!(is_anthropic("claude-opus-4-8"));
    assert!(is_anthropic("claude-fable-5"));
    assert!(is_anthropic("claude-sonnet-4-20260101"));
    assert!(!is_anthropic("gpt-5.5"));
    assert!(!is_anthropic("gemini-3-flash"));
    assert!(!is_anthropic("deepseek-r2"));
    assert!(!is_anthropic(""));
}

#[test]
fn model_display_name_mapping() {
    assert_eq!(model_display_name("claude-opus-4-8"), "opus 4.8");
    assert_eq!(model_display_name("claude-sonnet-4-6"), "sonnet 4.6");
    assert_eq!(model_display_name("claude-haiku-4-5-20251001"), "haiku 4.5");
    assert_eq!(
        model_display_name("claude-sonnet-4-5-20250929"),
        "sonnet 4.5"
    );
    assert_eq!(
        model_display_name("claude-opus-4-6-thinking"),
        "opus 4.6 thinking"
    );
    assert_eq!(model_display_name("claude-sonnet-4.6"), "sonnet 4.6");
    assert_eq!(model_display_name("claude-fable-5"), "fable 5");
    // Non-Anthropic and the synthetic bucket pass through.
    assert_eq!(model_display_name("gpt-5.5"), "gpt-5.5");
    assert_eq!(model_display_name("others"), "others");
}

// ── 4. cache_hit_ratio ────────────────────────────────────────────────────────

#[test]
fn cache_hit_ratio_math() {
    let stats = TokenStats {
        models: vec![],
        daily: vec![],
        daily_models: vec![],
        activity: vec![],
        hour_counts: [0; 24],
        total_input: 1000,
        total_output: 0,
        total_cache_read: 500,
        total_cache_create: 500,
        total_sessions: 0,
        total_messages: 0,
        first_session_date: None,
        last_computed_date: None,
        topped_up_through: None,
        today: None,
    };
    // cache_read / (cache_read + cache_create + input) = 500 / 2000 = 0.25
    let ratio = stats.cache_hit_ratio();
    assert!((ratio - 0.25).abs() < 1e-9, "expected 0.25 got {ratio}");
}

#[test]
fn cache_hit_ratio_zero_denominator() {
    let stats = TokenStats {
        models: vec![],
        daily: vec![],
        daily_models: vec![],
        activity: vec![],
        hour_counts: [0; 24],
        total_input: 0,
        total_output: 0,
        total_cache_read: 0,
        total_cache_create: 0,
        total_sessions: 0,
        total_messages: 0,
        first_session_date: None,
        last_computed_date: None,
        topped_up_through: None,
        today: None,
    };
    assert_eq!(stats.cache_hit_ratio(), 0.0);
}

// ── 5. top-up ────────────────────────────────────────────────────────────────

fn jsonl_line(
    timestamp: &str,
    model: &str,
    input: u64,
    output: u64,
    cache_read: u64,
    cache_create: u64,
) -> String {
    format!(
        r#"{{"timestamp":"{timestamp}","message":{{"model":"{model}","usage":{{"input_tokens":{input},"output_tokens":{output},"cache_read_input_tokens":{cache_read},"cache_creation_input_tokens":{cache_create}}}}}}}"#
    )
}

#[test]
fn top_up_adds_new_day_updates_model_and_sets_topped_up_through() {
    let sb = HomeSandbox::new();
    let claude_dir = make_claude_dir(&sb);

    write_stats_cache(
        &claude_dir,
        r#"{
            "lastComputedDate": "2026-06-10",
            "totalSessions": 5,
            "totalMessages": 100,
            "dailyActivity": [],
            "dailyModelTokens": [],
            "modelUsage": {
                "claude-opus-4": {
                    "inputTokens": 1000, "outputTokens": 500,
                    "cacheReadInputTokens": 200, "cacheCreationInputTokens": 50
                }
            },
            "hourCounts": {}
        }"#,
    );

    // Create projects/p1/sess.jsonl with a line dated AFTER lastComputedDate.
    let proj_dir = claude_dir.join("projects").join("p1");
    std::fs::create_dir_all(&proj_dir).expect("create project dir");
    let jsonl_path = proj_dir.join("sess.jsonl");

    let after_line = jsonl_line(
        "2026-06-11T10:30:00+00:00",
        "claude-opus-4",
        300,
        100,
        20,
        10,
    );
    // Also a line BEFORE cutoff — must NOT be counted.
    let before_line = jsonl_line(
        "2026-06-10T23:59:59+00:00",
        "claude-opus-4",
        9999,
        9999,
        9999,
        9999,
    );
    // Line equal to cutoff — must NOT be counted.
    let equal_line = jsonl_line(
        "2026-06-10T00:00:00+00:00",
        "claude-opus-4",
        8888,
        8888,
        8888,
        8888,
    );

    {
        let mut f = std::fs::File::create(&jsonl_path).expect("create jsonl");
        writeln!(f, "{before_line}").expect("write");
        writeln!(f, "{equal_line}").expect("write");
        writeln!(f, "{after_line}").expect("write");
    }

    // Set mtime to now (definitely after cutoff 2026-06-10T00:00 UTC).
    set_mtime(&jsonl_path, SystemTime::now());

    let stats = load(&claude_dir).expect("load");

    // New day 2026-06-11 must appear in daily.
    let day11 = stats
        .daily
        .iter()
        .find(|d| d.date == "2026-06-11")
        .expect("2026-06-11 must be in daily");
    assert_eq!(day11.tokens, 400); // 300 + 100

    // Model totals grew by the after_line amounts only.
    let opus = stats
        .models
        .iter()
        .find(|m| m.model == "claude-opus-4")
        .expect("opus");
    assert_eq!(opus.input, 1300); // 1000 + 300
    assert_eq!(opus.output, 600); // 500 + 100
    assert_eq!(opus.cache_read, 220); // 200 + 20
    assert_eq!(opus.cache_create, 60); // 50 + 10

    // topped_up_through set.
    assert_eq!(stats.topped_up_through.as_deref(), Some("2026-06-11"));
}

#[test]
fn today_bucket_aggregates_todays_transcript_lines() {
    let sb = HomeSandbox::new();
    let claude_dir = make_claude_dir(&sb);

    write_stats_cache(
        &claude_dir,
        r#"{
            "lastComputedDate": "2026-06-10",
            "totalSessions": 0, "totalMessages": 0,
            "dailyActivity": [], "dailyModelTokens": [],
            "modelUsage": {}, "hourCounts": {}
        }"#,
    );

    // Today's date computed exactly as the module does (same clock).
    let today = crate::usage::epoch_secs_to_iso(crate::usage::now_epoch_secs());
    let today_date = today[..10].to_owned();
    let ts = format!("{today_date}T12:00:00+00:00");

    let proj_dir = claude_dir.join("projects").join("p1");
    std::fs::create_dir_all(&proj_dir).expect("create project dir");
    let jsonl_path = proj_dir.join("sess.jsonl");
    let l1 = jsonl_line(&ts, "claude-opus-4", 100, 50, 20, 5);
    let l2 = jsonl_line(&ts, "claude-opus-4", 10, 5, 0, 0);
    std::fs::write(&jsonl_path, format!("{l1}\n{l2}\n")).expect("write");
    set_mtime(&jsonl_path, SystemTime::now());

    let stats = load(&claude_dir).expect("load");
    let today_s = stats.today.expect("today must be populated");
    assert_eq!(today_s.date, today_date);
    assert_eq!(today_s.messages, 2);
    assert_eq!(today_s.input, 110);
    assert_eq!(today_s.output, 55);
    assert_eq!(today_s.cache_read, 20);
    assert_eq!(today_s.cache_create, 5);
    assert_eq!(today_s.in_out(), 165);
    assert_eq!(today_s.total(), 190);
}

#[test]
fn top_up_skips_old_mtime_file() {
    let sb = HomeSandbox::new();
    let claude_dir = make_claude_dir(&sb);

    write_stats_cache(
        &claude_dir,
        r#"{
            "lastComputedDate": "2026-06-10",
            "totalSessions": 0, "totalMessages": 0,
            "dailyActivity": [], "dailyModelTokens": [],
            "modelUsage": {}, "hourCounts": {}
        }"#,
    );

    let proj_dir = claude_dir.join("projects").join("p2");
    std::fs::create_dir_all(&proj_dir).expect("create project dir");
    let jsonl_path = proj_dir.join("old.jsonl");
    let after_line = jsonl_line("2026-06-11T10:00:00+00:00", "claude-opus-4", 500, 200, 0, 0);
    std::fs::write(&jsonl_path, format!("{after_line}\n")).expect("write");

    // Set mtime to well before the cutoff (2026-01-01).
    let old_time = UNIX_EPOCH + Duration::from_secs(1_735_689_600); // 2026-01-01T00:00:00Z
    set_mtime(&jsonl_path, old_time);

    let stats = load(&claude_dir).expect("load");

    // No new days — old file was skipped by mtime guard.
    assert!(stats.daily.is_empty());
    assert!(stats.topped_up_through.is_none());
}

#[test]
fn top_up_none_when_no_last_computed_date() {
    let sb = HomeSandbox::new();
    let claude_dir = make_claude_dir(&sb);

    write_stats_cache(
        &claude_dir,
        r#"{
            "totalSessions": 0, "totalMessages": 0,
            "dailyActivity": [], "dailyModelTokens": [],
            "modelUsage": {}, "hourCounts": {}
        }"#,
    );

    let proj_dir = claude_dir.join("projects").join("p3");
    std::fs::create_dir_all(&proj_dir).expect("create project dir");
    let jsonl_path = proj_dir.join("sess.jsonl");
    let line = jsonl_line("2026-06-11T10:00:00+00:00", "claude-opus-4", 100, 50, 0, 0);
    std::fs::write(&jsonl_path, format!("{line}\n")).expect("write");
    set_mtime(&jsonl_path, SystemTime::now());

    let stats = load(&claude_dir).expect("load");
    // top-up skipped entirely — no last_computed_date.
    assert!(stats.topped_up_through.is_none());
    assert!(stats.daily.is_empty());
}

fn jsonl_line_with_ids(
    timestamp: &str,
    request_id: &str,
    msg_id: &str,
    model: &str,
    input: u64,
    output: u64,
) -> String {
    format!(
        r#"{{"timestamp":"{timestamp}","requestId":"{request_id}","message":{{"id":"{msg_id}","model":"{model}","usage":{{"input_tokens":{input},"output_tokens":{output},"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}}}}}"#
    )
}

#[test]
fn top_up_counts_nested_subagent_transcripts() {
    let sb = HomeSandbox::new();
    let claude_dir = make_claude_dir(&sb);

    write_stats_cache(
        &claude_dir,
        r#"{
            "lastComputedDate": "2026-06-10",
            "totalSessions": 0, "totalMessages": 0,
            "dailyActivity": [], "dailyModelTokens": [],
            "modelUsage": {}, "hourCounts": {}
        }"#,
    );

    // Main-session transcript: projects/p1/sess.jsonl
    let proj_dir = claude_dir.join("projects").join("p1");
    std::fs::create_dir_all(&proj_dir).expect("create project dir");
    let main_path = proj_dir.join("sess.jsonl");
    let main_line = jsonl_line("2026-06-11T10:00:00+00:00", "claude-opus-4", 100, 50, 0, 0);
    std::fs::write(&main_path, format!("{main_line}\n")).expect("write main");
    set_mtime(&main_path, SystemTime::now());

    // Subagent/workflow transcript nested under <session>/subagents/.
    let sub_dir = proj_dir.join("sess").join("subagents");
    std::fs::create_dir_all(&sub_dir).expect("create subagents dir");
    let sub_path = sub_dir.join("agent-x.jsonl");
    let sub_line = jsonl_line("2026-06-11T10:05:00+00:00", "claude-opus-4", 300, 200, 0, 0);
    std::fs::write(&sub_path, format!("{sub_line}\n")).expect("write subagent");
    set_mtime(&sub_path, SystemTime::now());

    let stats = load(&claude_dir).expect("load");

    // Day total includes the nested subagent line: (100+50) + (300+200) = 650.
    let day = stats
        .daily
        .iter()
        .find(|d| d.date == "2026-06-11")
        .expect("2026-06-11 must be in daily");
    assert_eq!(day.tokens, 650);

    let opus = stats
        .models
        .iter()
        .find(|m| m.model == "claude-opus-4")
        .expect("opus");
    assert_eq!(opus.input, 400); // 100 + 300
    assert_eq!(opus.output, 250); // 50 + 200
}

#[test]
fn top_up_dedupes_same_message_across_files() {
    let sb = HomeSandbox::new();
    let claude_dir = make_claude_dir(&sb);

    write_stats_cache(
        &claude_dir,
        r#"{
            "lastComputedDate": "2026-06-10",
            "totalSessions": 0, "totalMessages": 0,
            "dailyActivity": [], "dailyModelTokens": [],
            "modelUsage": {}, "hourCounts": {}
        }"#,
    );

    let proj_dir = claude_dir.join("projects").join("p1");
    std::fs::create_dir_all(&proj_dir).expect("create project dir");

    // Same (requestId, message.id) mirrored into two transcripts — e.g. a forked
    // or resumed session copying a line forward. Must be counted exactly once.
    let line = jsonl_line_with_ids(
        "2026-06-11T10:00:00+00:00",
        "req_1",
        "msg_1",
        "claude-opus-4",
        100,
        50,
    );
    let f1 = proj_dir.join("a.jsonl");
    let f2 = proj_dir.join("b.jsonl");
    std::fs::write(&f1, format!("{line}\n")).expect("write a");
    std::fs::write(&f2, format!("{line}\n")).expect("write b");
    set_mtime(&f1, SystemTime::now());
    set_mtime(&f2, SystemTime::now());

    let stats = load(&claude_dir).expect("load");

    // Counted once: 100 + 50 = 150, not 300.
    let day = stats
        .daily
        .iter()
        .find(|d| d.date == "2026-06-11")
        .expect("2026-06-11 must be in daily");
    assert_eq!(day.tokens, 150);

    let opus = stats
        .models
        .iter()
        .find(|m| m.model == "claude-opus-4")
        .expect("opus");
    assert_eq!(opus.input, 100);
    assert_eq!(opus.output, 50);
}

#[test]
fn top_up_dedupes_idless_usage_lines_by_content() {
    // A usage line with no message.id / requestId, mirrored into two transcripts,
    // must still count once. The old dedup keyed on (requestId, message.id) and
    // bypassed the guard whenever either was absent, double-counting such lines.
    let sb = HomeSandbox::new();
    let claude_dir = make_claude_dir(&sb);
    write_stats_cache(
        &claude_dir,
        r#"{
            "lastComputedDate": "2026-06-10",
            "totalSessions": 0, "totalMessages": 0,
            "dailyActivity": [], "dailyModelTokens": [],
            "modelUsage": {}, "hourCounts": {}
        }"#,
    );

    let proj_dir = claude_dir.join("projects").join("p1");
    std::fs::create_dir_all(&proj_dir).expect("create project dir");
    let line = jsonl_line("2026-06-11T10:00:00+00:00", "claude-opus-4", 100, 50, 0, 0);
    for name in ["a.jsonl", "b.jsonl"] {
        let p = proj_dir.join(name);
        std::fs::write(&p, format!("{line}\n")).expect("write");
        set_mtime(&p, SystemTime::now());
    }

    let stats = load(&claude_dir).expect("load");
    let day = stats
        .daily
        .iter()
        .find(|d| d.date == "2026-06-11")
        .expect("2026-06-11 must be in daily");
    assert_eq!(
        day.tokens, 150,
        "id-less duplicate counted once via composite key"
    );
}

/// One assistant-turn line as CC writes it during streaming: a distinct line
/// uuid per delta, the same message.id, full input/cache, growing output.
fn streamed_line(
    timestamp: &str,
    msg_id: &str,
    uuid: &str,
    model: &str,
    input: u64,
    output: u64,
) -> String {
    format!(
        r#"{{"timestamp":"{timestamp}","uuid":"{uuid}","message":{{"id":"{msg_id}","role":"assistant","model":"{model}","usage":{{"input_tokens":{input},"output_tokens":{output},"cache_read_input_tokens":500,"cache_creation_input_tokens":0}}}}}}"#
    )
}

#[test]
fn top_up_counts_streamed_turn_by_completed_delta_not_first() {
    // One assistant turn written as three streaming deltas sharing message.id:
    // output 0 -> 0 -> 272 on distinct line uuids. Tokens must count the final
    // 272 (not the first 0), and the turn must count as one message, not three.
    let sb = HomeSandbox::new();
    let claude_dir = make_claude_dir(&sb);
    write_stats_cache(
        &claude_dir,
        r#"{
            "lastComputedDate": "2026-06-10",
            "totalSessions": 0, "totalMessages": 0,
            "dailyActivity": [], "dailyModelTokens": [],
            "modelUsage": {}, "hourCounts": {}
        }"#,
    );

    let proj_dir = claude_dir.join("projects").join("p1");
    std::fs::create_dir_all(&proj_dir).expect("create project dir");
    let p = proj_dir.join("sess.jsonl");
    let lines = [
        streamed_line(
            "2026-06-11T10:00:00+00:00",
            "msg_1",
            "u1",
            "deepseek-v4-pro",
            1000,
            0,
        ),
        streamed_line(
            "2026-06-11T10:00:01+00:00",
            "msg_1",
            "u2",
            "deepseek-v4-pro",
            1000,
            0,
        ),
        streamed_line(
            "2026-06-11T10:00:02+00:00",
            "msg_1",
            "u3",
            "deepseek-v4-pro",
            1000,
            272,
        ),
    ];
    std::fs::write(&p, lines.join("\n")).expect("write");
    set_mtime(&p, SystemTime::now());

    let stats = load(&claude_dir).expect("load");

    // The completed delta's output, not the first delta's 0.
    let day = stats
        .daily
        .iter()
        .find(|d| d.date == "2026-06-11")
        .expect("2026-06-11 must be in daily");
    assert_eq!(day.tokens, 1000 + 272);
    let model = stats
        .models
        .iter()
        .find(|m| m.model == "deepseek-v4-pro")
        .expect("model");
    assert_eq!(model.input, 1000);
    assert_eq!(model.output, 272);
    assert_eq!(model.cache_read, 500);

    // One turn = one message, not three deltas.
    assert_eq!(stats.total_messages, 1);
}

/// A role/uuid/session message line with no token usage — drives the
/// message/session/hour reconstruction without touching token totals.
fn jsonl_msg_line(timestamp: &str, uuid: &str, session: &str, role: &str) -> String {
    format!(
        r#"{{"timestamp":"{timestamp}","uuid":"{uuid}","sessionId":"{session}","message":{{"role":"{role}"}}}}"#
    )
}

#[test]
fn top_up_reconstructs_messages_sessions_hours_after_cutoff() {
    let sb = HomeSandbox::new();
    let claude_dir = make_claude_dir(&sb);
    write_stats_cache(
        &claude_dir,
        r#"{
            "lastComputedDate": "2026-06-10",
            "totalSessions": 5, "totalMessages": 100,
            "dailyActivity": [], "dailyModelTokens": [],
            "modelUsage": {}, "hourCounts": {"9": 7}
        }"#,
    );

    let proj_dir = claude_dir.join("projects").join("p1");
    std::fs::create_dir_all(&proj_dir).expect("create project dir");
    let lines = [
        jsonl_msg_line("2026-06-11T14:00:00+00:00", "u1", "sessA", "user"),
        jsonl_msg_line("2026-06-11T14:05:00+00:00", "u2", "sessA", "assistant"),
        jsonl_msg_line("2026-06-11T14:30:00+00:00", "u3", "sessB", "user"),
        // Duplicate uuid (resumed/forked copy) — must count once.
        jsonl_msg_line("2026-06-11T14:31:00+00:00", "u3", "sessB", "user"),
        // Pre-cutoff line — must not count toward post-cutoff reconstruction.
        jsonl_msg_line("2026-06-09T14:00:00+00:00", "u0", "sessOld", "user"),
    ];
    let p = proj_dir.join("sess.jsonl");
    std::fs::write(&p, lines.join("\n")).expect("write");
    set_mtime(&p, SystemTime::now());

    let stats = load(&claude_dir).expect("load");
    // 3 distinct post-cutoff messages (u1, u2, u3) added to base 100.
    assert_eq!(stats.total_messages, 103);
    // 2 distinct post-cutoff sessions (sessA, sessB) added to base 5.
    assert_eq!(stats.total_sessions, 7);
    // hour 14 gains 3; base hour 9 stays.
    assert_eq!(stats.hour_counts[14], 3);
    assert_eq!(stats.hour_counts[9], 7);
    // Per-day activity appended for the new day.
    let day = stats
        .activity
        .iter()
        .find(|a| a.date == "2026-06-11")
        .expect("2026-06-11 activity");
    assert_eq!(day.messages, 3);
    assert_eq!(day.sessions, 2);
}

// ── 6. period bucketing + per-day models ─────────────────────────────────────

#[test]
fn bucket_start_week_and_month() {
    // 2026-07-09 is a thursday; its week starts monday 2026-07-06.
    assert_eq!(bucket_start("2026-07-09", Bucket::Week), "2026-07-06");
    // A monday is its own week start; a sunday belongs to the preceding monday.
    assert_eq!(bucket_start("2026-07-06", Bucket::Week), "2026-07-06");
    assert_eq!(bucket_start("2026-07-12", Bucket::Week), "2026-07-06");
    // Year boundary: 2026-01-01 (thursday) → monday 2025-12-29.
    assert_eq!(bucket_start("2026-01-01", Bucket::Week), "2025-12-29");
    assert_eq!(bucket_start("2026-07-09", Bucket::Month), "2026-07-01");
    // Unparseable input degrades to itself instead of panicking.
    assert_eq!(bucket_start("garbage-date", Bucket::Week), "garbage-date");
    assert_eq!(bucket_start("abc", Bucket::Month), "abc");
}

#[test]
fn current_bucket_bounds_are_inclusive_start_to_today() {
    assert_eq!(
        current_bucket_bounds("2026-07-09", Bucket::Week),
        ("2026-07-06".to_owned(), "2026-07-09".to_owned())
    );
    assert_eq!(
        current_bucket_bounds("2026-07-09", Bucket::Month),
        ("2026-07-01".to_owned(), "2026-07-09".to_owned())
    );
}

#[test]
fn bucket_tokens_folds_days_into_calendar_buckets() {
    let days = vec![
        DayTokens {
            date: "2026-06-30".into(),
            tokens: 1,
        }, // week of 06-29
        DayTokens {
            date: "2026-07-01".into(),
            tokens: 2,
        }, // week of 06-29
        DayTokens {
            date: "2026-07-06".into(),
            tokens: 4,
        }, // week of 07-06
        DayTokens {
            date: "2026-07-07".into(),
            tokens: 8,
        }, // week of 07-06
    ];
    let weeks = bucket_tokens(&days, Bucket::Week);
    assert_eq!(weeks.len(), 2);
    assert_eq!(weeks[0].date, "2026-06-29");
    assert_eq!(weeks[0].tokens, 3);
    assert_eq!(weeks[1].date, "2026-07-06");
    assert_eq!(weeks[1].tokens, 12);

    let months = bucket_tokens(&days, Bucket::Month);
    assert_eq!(months.len(), 2);
    assert_eq!(months[0].date, "2026-06-01");
    assert_eq!(months[0].tokens, 1);
    assert_eq!(months[1].date, "2026-07-01");
    assert_eq!(months[1].tokens, 14);
}

#[test]
fn bucket_activity_sums_counts_under_the_bucket_key() {
    let days = vec![
        DayActivity {
            date: "2026-07-06".into(),
            messages: 10,
            sessions: 1,
            tool_calls: 5,
        },
        DayActivity {
            date: "2026-07-07".into(),
            messages: 20,
            sessions: 2,
            tool_calls: 7,
        },
    ];
    let weeks = bucket_activity(&days, Bucket::Week);
    assert_eq!(weeks.len(), 1);
    assert_eq!(weeks[0].date, "2026-07-06");
    assert_eq!(weeks[0].messages, 30);
    assert_eq!(weeks[0].sessions, 3);
    assert_eq!(weeks[0].tool_calls, 12);
}

fn day_model(
    date: &str,
    model: &str,
    in_out: u64,
    split: Option<ModelTokens>,
    hours: Option<[HourTokens; 24]>,
) -> DayModelTokens {
    DayModelTokens {
        date: date.into(),
        model: model.into(),
        in_out,
        split,
        hours,
    }
}

#[test]
fn period_models_aggregates_range_and_split_flags() {
    let split = ModelTokens {
        model: "claude-opus-4".into(),
        input: 30,
        output: 20,
        cache_read: 500,
        cache_create: 5,
    };
    let days = vec![
        // Outside the range — must not count.
        day_model("2026-06-30", "claude-opus-4", 999, None, None),
        // stats-cache day: in+out only.
        day_model("2026-07-01", "claude-opus-4", 100, None, None),
        // transcript day: full split.
        day_model("2026-07-07", "claude-opus-4", 50, Some(split.clone()), None),
        day_model(
            "2026-07-07",
            "gpt-5",
            10,
            Some(ModelTokens {
                model: "gpt-5".into(),
                input: 6,
                output: 4,
                ..Default::default()
            }),
            None,
        ),
    ];
    let rows = period_models(&days, "2026-07-01", "2026-07-09");
    assert_eq!(rows.len(), 2);
    // Ranked DESC by in+out.
    assert_eq!(rows[0].model, "claude-opus-4");
    assert_eq!(rows[0].in_out, 150);
    // The split sums only the split-bearing day and is flagged incomplete.
    assert!(!rows[0].split_complete);
    assert_eq!(rows[0].split.input, 30);
    assert_eq!(rows[0].split.cache_read, 500);
    assert_eq!(rows[1].model, "gpt-5");
    assert!(rows[1].split_complete);
    assert_eq!(rows[1].in_out, 10);

    // One incomplete row pins the whole list to the in+out basis.
    assert!(!effective_cache_basis(&rows, true));
    assert!(effective_cache_basis(&rows[1..], true));
    assert!(!effective_cache_basis(&rows[1..], false));
}

#[test]
fn period_model_metric_honors_split_completeness() {
    let m = ModelTokens {
        model: "claude-opus-4".into(),
        input: 10,
        output: 5,
        cache_read: 100,
        cache_create: 1,
    };
    let full = PeriodModel::from_full(&m);
    assert_eq!(full.metric(false), 15);
    assert_eq!(full.metric(true), 116);
    let partial = PeriodModel {
        split_complete: false,
        ..full
    };
    assert_eq!(partial.metric(true), 15);
}

#[test]
fn load_populates_daily_models_from_stats_cache_and_topup() {
    let sb = HomeSandbox::new();
    let claude_dir = make_claude_dir(&sb);

    write_stats_cache(
        &claude_dir,
        r#"{
            "lastComputedDate": "2026-06-10",
            "totalSessions": 0, "totalMessages": 0,
            "dailyActivity": [],
            "dailyModelTokens": [
                {"date": "2026-06-09", "tokensByModel": {"claude-opus-4": 500}}
            ],
            "modelUsage": {}, "hourCounts": {}
        }"#,
    );

    let proj_dir = claude_dir.join("projects").join("p1");
    std::fs::create_dir_all(&proj_dir).expect("create project dir");
    let p = proj_dir.join("sess.jsonl");
    let line = jsonl_line(
        "2026-06-11T10:30:00+00:00",
        "claude-opus-4",
        300,
        100,
        20,
        10,
    );
    std::fs::write(&p, format!("{line}\n")).expect("write");
    set_mtime(&p, SystemTime::now());

    let stats = load(&claude_dir).expect("load");
    // The stats-cache day carries no split; the transcript day carries a full one.
    let cached = stats
        .daily_models
        .iter()
        .find(|d| d.date == "2026-06-09")
        .expect("stats-cache day");
    assert_eq!(cached.model, "claude-opus-4");
    assert_eq!(cached.in_out, 500);
    assert!(cached.split.is_none());
    assert!(
        cached.hours.is_none(),
        "stats-cache days carry no hourly axis"
    );
    let live = stats
        .daily_models
        .iter()
        .find(|d| d.date == "2026-06-11")
        .expect("top-up day");
    assert_eq!(live.in_out, 400);
    let split = live.split.as_ref().expect("split");
    assert_eq!(split.input, 300);
    assert_eq!(split.output, 100);
    assert_eq!(split.cache_read, 20);
    assert_eq!(split.cache_create, 10);
    // The 10:30 timestamp buckets the whole line into hour 10.
    let live_hours = live.hours.expect("transcript days carry per-hour buckets");
    assert_eq!(live_hours[10].input, 300);
    assert_eq!(live_hours[10].output, 100);
    assert_eq!(live_hours[10].cache_read, 20);
    assert_eq!(live_hours[10].cache_create, 10);
    assert_eq!(live_hours[9].input, 0);
}

#[test]
fn today_hours_track_todays_messages() {
    let sb = HomeSandbox::new();
    let claude_dir = make_claude_dir(&sb);
    write_stats_cache(
        &claude_dir,
        r#"{
            "lastComputedDate": "2026-06-10",
            "totalSessions": 0, "totalMessages": 0,
            "dailyActivity": [], "dailyModelTokens": [],
            "modelUsage": {}, "hourCounts": {}
        }"#,
    );

    let today = crate::usage::epoch_secs_to_iso(crate::usage::now_epoch_secs());
    let today_date = today[..10].to_owned();
    let proj_dir = claude_dir.join("projects").join("p1");
    std::fs::create_dir_all(&proj_dir).expect("create project dir");
    let p = proj_dir.join("sess.jsonl");
    let l1 = jsonl_line(
        &format!("{today_date}T12:00:00+00:00"),
        "claude-opus-4",
        1,
        1,
        0,
        0,
    );
    let l2 = jsonl_line(
        &format!("{today_date}T12:30:00+00:00"),
        "claude-opus-4",
        2,
        2,
        0,
        0,
    );
    let l3 = jsonl_line(
        &format!("{today_date}T03:00:00+00:00"),
        "claude-opus-4",
        3,
        3,
        0,
        0,
    );
    std::fs::write(&p, format!("{l1}\n{l2}\n{l3}\n")).expect("write");
    set_mtime(&p, SystemTime::now());

    let stats = load(&claude_dir).expect("load");
    let t = stats.today.expect("today");
    assert_eq!(t.hours[12], 2);
    assert_eq!(t.hours[3], 1);
    assert_eq!(t.hours.iter().sum::<u64>(), 3);
}

// ── 7. hourly axis ────────────────────────────────────────────────────────────

/// Recs at different hours accumulate into the right `[24]` slots — today's
/// rollup (per-day and per-model) and post-cutoff `day_models` rows alike,
/// hour 23 (the last slot) included.
#[test]
fn hourly_buckets_accumulate_into_hour_slots() {
    let sb = HomeSandbox::new();
    let claude_dir = make_claude_dir(&sb);
    write_stats_cache(
        &claude_dir,
        r#"{
            "lastComputedDate": "2026-06-10",
            "totalSessions": 0, "totalMessages": 0,
            "dailyActivity": [], "dailyModelTokens": [],
            "modelUsage": {}, "hourCounts": {}
        }"#,
    );

    let today = crate::usage::epoch_secs_to_iso(crate::usage::now_epoch_secs());
    let today_date = today[..10].to_owned();

    let proj_dir = claude_dir.join("projects").join("p1");
    std::fs::create_dir_all(&proj_dir).expect("create project dir");
    let p = proj_dir.join("sess.jsonl");
    let lines = [
        // Today, hour 12: two distinct lines accumulate into one slot.
        jsonl_line(
            &format!("{today_date}T12:00:00+00:00"),
            "claude-opus-4",
            100,
            50,
            20,
            5,
        ),
        jsonl_line(
            &format!("{today_date}T12:30:00+00:00"),
            "claude-opus-4",
            10,
            5,
            0,
            0,
        ),
        // Today, hour 23 — the last slot.
        jsonl_line(
            &format!("{today_date}T23:00:00+00:00"),
            "claude-opus-4",
            7,
            3,
            1,
            1,
        ),
        // Today, a second model at its own hour.
        jsonl_line(&format!("{today_date}T14:00:00+00:00"), "gpt-5", 1, 1, 0, 0),
        // Post-cutoff day 2026-06-11: hours 01 and 23, two models.
        jsonl_line(
            "2026-06-11T01:00:00+00:00",
            "claude-opus-4",
            300,
            100,
            20,
            10,
        ),
        jsonl_line("2026-06-11T23:30:00+00:00", "claude-opus-4", 5, 2, 0, 0),
        jsonl_line("2026-06-11T23:45:00+00:00", "gpt-5", 9, 9, 0, 0),
    ];
    std::fs::write(&p, lines.join("\n")).expect("write");
    set_mtime(&p, SystemTime::now());

    let stats = load(&claude_dir).expect("load");

    // Today's rollup: hour 12 sums both lines, hour 23 holds its line.
    let t = stats.today.as_ref().expect("today");
    assert_eq!(t.token_hours[12].input, 110);
    assert_eq!(t.token_hours[12].output, 55);
    assert_eq!(t.token_hours[12].cache_read, 20);
    assert_eq!(t.token_hours[12].cache_create, 5);
    assert_eq!(t.token_hours[23].input, 7);
    assert_eq!(t.token_hours[23].output, 3);
    assert_eq!(t.token_hours[23].cache_read, 1);
    assert_eq!(t.token_hours[0].input, 0, "unused slots stay empty");
    // Bucket totals equal the flat day totals.
    assert_eq!(t.token_hours.iter().map(|h| h.input).sum::<u64>(), t.input);
    assert_eq!(
        t.token_hours.iter().map(|h| h.output).sum::<u64>(),
        t.output
    );
    assert_eq!(
        t.token_hours.iter().map(|h| h.cache_read).sum::<u64>(),
        t.cache_read
    );
    assert_eq!(
        t.token_hours.iter().map(|h| h.cache_create).sum::<u64>(),
        t.cache_create
    );

    // model_hours carries one entry per model, in the same DESC-total order
    // as `models`.
    assert_eq!(t.models.len(), 2);
    assert_eq!(t.model_hours.len(), 2);
    assert_eq!(t.model_hours[0].model, "claude-opus-4");
    assert_eq!(t.model_hours[0].hours[12].input, 110);
    assert_eq!(t.model_hours[0].hours[23].input, 7);
    assert_eq!(t.model_hours[1].model, "gpt-5");
    assert_eq!(t.model_hours[1].hours[14].input, 1);
    assert_eq!(t.model_hours[1].hours[0].output, 0);

    // Post-cutoff day rows carry per-hour splits, hour 23 included.
    let day = stats
        .daily_models
        .iter()
        .find(|d| d.date == "2026-06-11" && d.model == "claude-opus-4")
        .expect("day row");
    let hours = day.hours.expect("transcript rows carry hours");
    assert_eq!(hours[1].input, 300);
    assert_eq!(hours[1].output, 100);
    assert_eq!(hours[1].cache_read, 20);
    assert_eq!(hours[1].cache_create, 10);
    assert_eq!(hours[23].input, 5);
    assert_eq!(hours[23].output, 2);
    assert_eq!(hours[12].input, 0);
    // The day's hourly buckets sum to its flat split exactly.
    let day_split = day.split.as_ref().expect("split");
    assert_eq!(hours.iter().map(|h| h.input).sum::<u64>(), day_split.input);
    assert_eq!(
        hours.iter().map(|h| h.output).sum::<u64>(),
        day_split.output
    );
    assert_eq!(
        hours.iter().map(|h| h.cache_read).sum::<u64>(),
        day_split.cache_read
    );
    assert_eq!(
        hours.iter().map(|h| h.cache_create).sum::<u64>(),
        day_split.cache_create
    );
    let gpt = stats
        .daily_models
        .iter()
        .find(|d| d.date == "2026-06-11" && d.model == "gpt-5")
        .expect("gpt row");
    assert_eq!(gpt.hours.expect("hours")[23].input, 9);
    assert_eq!(gpt.hours.expect("hours")[23].output, 9);
    assert_eq!(
        gpt.hours
            .expect("hours")
            .iter()
            .map(|h| h.input)
            .sum::<u64>(),
        gpt.split.as_ref().expect("split").input
    );
}

/// Today's hour buckets dedupe exactly like the flat fields: a response
/// mirrored into two transcripts counts once in `token_hours` and once in
/// `model_hours`, not per copy.
#[test]
fn today_hour_buckets_dedupe_like_the_flat_fields() {
    let sb = HomeSandbox::new();
    let claude_dir = make_claude_dir(&sb);
    write_stats_cache(
        &claude_dir,
        r#"{
            "lastComputedDate": "2026-06-10",
            "totalSessions": 0, "totalMessages": 0,
            "dailyActivity": [], "dailyModelTokens": [],
            "modelUsage": {}, "hourCounts": {}
        }"#,
    );

    let today = crate::usage::epoch_secs_to_iso(crate::usage::now_epoch_secs());
    let today_date = today[..10].to_owned();
    let ts = format!("{today_date}T12:00:00+00:00");

    let proj_dir = claude_dir.join("projects").join("p1");
    std::fs::create_dir_all(&proj_dir).expect("create project dir");
    let line = jsonl_line_with_ids(&ts, "req_1", "msg_1", "claude-opus-4", 100, 50);
    for name in ["a.jsonl", "b.jsonl"] {
        let f = proj_dir.join(name);
        std::fs::write(&f, format!("{line}\n")).expect("write");
        set_mtime(&f, SystemTime::now());
    }

    let stats = load(&claude_dir).expect("load");
    let t = stats.today.as_ref().expect("today");
    assert_eq!(t.input, 100);
    assert_eq!(t.output, 50);
    assert_eq!(
        t.token_hours[12].input, 100,
        "hour buckets follow the same dedup"
    );
    assert_eq!(t.token_hours[12].output, 50);
    assert_eq!(t.token_hours.iter().map(|h| h.input).sum::<u64>(), 100);
    assert_eq!(t.models.len(), 1);
    assert_eq!(t.model_hours.len(), 1);
    assert_eq!(t.model_hours[0].model, "claude-opus-4");
    assert_eq!(t.model_hours[0].hours[12].input, 100);
    assert_eq!(t.model_hours[0].hours[12].output, 50);
    assert_eq!(t.model_hours[0].hours[0].output, 0);
}

/// A response mirrored into two transcripts with differing timestamps lands in
/// the path-sorted first file's hour — the cross-file dedup winner is pinned by
/// file order, not by whatever HashMap order a run happens to produce (which
/// would flip the hour, and with it the peak/off-peak cost, between runs).
#[test]
fn cross_file_duplicate_lands_in_the_path_sorted_winners_hour() {
    let sb = HomeSandbox::new();
    let claude_dir = make_claude_dir(&sb);
    write_stats_cache(
        &claude_dir,
        r#"{
            "lastComputedDate": "2026-06-10",
            "totalSessions": 0, "totalMessages": 0,
            "dailyActivity": [], "dailyModelTokens": [],
            "modelUsage": {}, "hourCounts": {}
        }"#,
    );

    let proj_dir = claude_dir.join("projects").join("p1");
    std::fs::create_dir_all(&proj_dir).expect("create project dir");
    // Same message.id, identical usage, hour 12 in `a.jsonl` (path-sorted
    // first) and hour 13 in `b.jsonl`.
    let line_a = jsonl_line_with_ids(
        "2026-06-11T12:00:00+00:00",
        "req_1",
        "msg_1",
        "claude-opus-4",
        100,
        50,
    );
    let line_b = jsonl_line_with_ids(
        "2026-06-11T13:00:00+00:00",
        "req_1",
        "msg_1",
        "claude-opus-4",
        100,
        50,
    );
    for (name, line) in [("a.jsonl", &line_a), ("b.jsonl", &line_b)] {
        let f = proj_dir.join(name);
        std::fs::write(&f, format!("{line}\n")).expect("write");
        set_mtime(&f, SystemTime::now());
    }

    let stats = load(&claude_dir).expect("load");
    let day = stats
        .daily_models
        .iter()
        .find(|d| d.date == "2026-06-11")
        .expect("day row");
    assert_eq!(day.in_out, 150, "counted once");
    let hours = day.hours.expect("hours");
    assert_eq!(
        hours[12].input, 100,
        "the path-sorted first copy's hour wins"
    );
    assert_eq!(hours[12].output, 50);
    assert_eq!(hours[13].input, 0, "the later copy's hour stays empty");
}

/// `period_models` carries one [`PeriodDay`] row per split-bearing day, in
/// date order regardless of input order, with the hours passed through when
/// the source row carried them. Days without a split keep the existing
/// `split_complete = false` floor semantics and produce no row.
#[test]
fn period_models_carries_per_day_split_rows_in_date_order() {
    let mut h03 = [HourTokens::default(); 24];
    h03[2] = HourTokens {
        input: 30,
        output: 20,
        cache_read: 500,
        cache_create: 5,
    };
    let mut h07 = [HourTokens::default(); 24];
    h07[9] = HourTokens {
        input: 12,
        output: 8,
        cache_read: 0,
        cache_create: 0,
    };
    let split_03 = ModelTokens {
        model: "claude-opus-4".into(),
        input: 30,
        output: 20,
        cache_read: 500,
        cache_create: 5,
    };
    let split_07 = ModelTokens {
        model: "claude-opus-4".into(),
        input: 12,
        output: 8,
        cache_read: 0,
        cache_create: 0,
    };

    let days = vec![
        // Split-bearing, out of date order on purpose.
        day_model("2026-07-07", "claude-opus-4", 20, Some(split_07), Some(h07)),
        // stats-cache day: contributes in+out and the incomplete flag only.
        day_model("2026-07-01", "claude-opus-4", 100, None, None),
        // Split-bearing without hours (a v1-ledger row) — hours stay None.
        day_model(
            "2026-07-05",
            "gpt-5",
            10,
            Some(ModelTokens {
                model: "gpt-5".into(),
                input: 6,
                output: 4,
                ..Default::default()
            }),
            None,
        ),
        day_model("2026-07-03", "claude-opus-4", 50, Some(split_03), Some(h03)),
    ];
    let rows = period_models(&days, "2026-07-01", "2026-07-09");
    assert_eq!(rows.len(), 2);

    let opus = rows
        .iter()
        .find(|r| r.model == "claude-opus-4")
        .expect("opus");
    assert_eq!(opus.in_out, 170); // 100 + 50 + 20
    assert!(
        !opus.split_complete,
        "the stats-cache day keeps the floor semantics"
    );
    assert_eq!(
        opus.days.len(),
        2,
        "only split-bearing days get PeriodDay rows"
    );
    assert_eq!(
        opus.days[0].date, "2026-07-03",
        "date order regardless of input order"
    );
    assert_eq!(opus.days[1].date, "2026-07-07");
    assert_eq!(opus.days[0].split.input, 30);
    assert_eq!(opus.days[0].hours.expect("hours")[2].cache_read, 500);
    assert_eq!(opus.days[1].hours.expect("hours")[9].input, 12);
    // Each PeriodDay's hourly buckets sum to its own split exactly.
    for day in &opus.days {
        let hs = day.hours.expect("hours");
        assert_eq!(hs.iter().map(|h| h.input).sum::<u64>(), day.split.input);
        assert_eq!(hs.iter().map(|h| h.output).sum::<u64>(), day.split.output);
        assert_eq!(
            hs.iter().map(|h| h.cache_read).sum::<u64>(),
            day.split.cache_read
        );
        assert_eq!(
            hs.iter().map(|h| h.cache_create).sum::<u64>(),
            day.split.cache_create
        );
    }
    // split sums the two split-bearing days only.
    assert_eq!(opus.split.input, 42);
    assert_eq!(opus.split.output, 28);

    let gpt = rows.iter().find(|r| r.model == "gpt-5").expect("gpt");
    assert!(gpt.split_complete);
    assert_eq!(gpt.days.len(), 1);
    assert_eq!(gpt.days[0].date, "2026-07-05");
    assert_eq!(gpt.days[0].split.input, 6);
    assert!(
        gpt.days[0].hours.is_none(),
        "a split-bearing row without hours keeps None"
    );
}

/// [`file_hourly_model_tokens`] splits one multi-day transcript file by
/// (model, day) with the per-hour slots each line's timestamp lands in.
#[test]
fn file_hourly_model_tokens_splits_by_model_and_day() {
    let sb = HomeSandbox::new();
    let claude_dir = make_claude_dir(&sb);
    let proj_dir = claude_dir.join("projects").join("p1");
    std::fs::create_dir_all(&proj_dir).expect("create project dir");
    let p = proj_dir.join("sess.jsonl");
    let lines = [
        jsonl_line("2026-06-11T10:00:00+00:00", "claude-opus-4", 100, 50, 0, 0),
        jsonl_line("2026-06-11T10:30:00+00:00", "claude-opus-4", 20, 5, 0, 0),
        jsonl_line("2026-06-11T22:00:00+00:00", "gpt-5", 7, 3, 0, 0),
        jsonl_line("2026-06-12T01:00:00+00:00", "claude-opus-4", 400, 100, 0, 0),
    ];
    std::fs::write(&p, lines.join("\n")).expect("write");
    set_mtime(&p, SystemTime::now());

    let rows = file_hourly_model_tokens(&p);
    assert_eq!(rows.len(), 3, "one row per (model, day) pair");
    let opus_11 = rows
        .iter()
        .find(|r| r.model == "claude-opus-4" && r.day == "2026-06-11")
        .expect("opus day 11");
    assert_eq!(opus_11.hours[10].input, 120, "both hour-10 lines sum");
    assert_eq!(opus_11.hours[10].output, 55);
    assert_eq!(opus_11.hours[22].input, 0, "opus never ran at 22:00");
    let opus_12 = rows
        .iter()
        .find(|r| r.model == "claude-opus-4" && r.day == "2026-06-12")
        .expect("opus day 12");
    assert_eq!(opus_12.hours[1].input, 400);
    assert_eq!(opus_12.hours[0].input, 0);
    let gpt = rows.iter().find(|r| r.model == "gpt-5").expect("gpt row");
    assert_eq!(gpt.day, "2026-06-11");
    assert_eq!(gpt.hours[22].input, 7);
    assert_eq!(gpt.hours[22].output, 3);
    assert_eq!(gpt.hours[10].input, 0);
}

// ── 8. ledger backfill (slice E) ─────────────────────────────────────────────

/// Write a v1-shaped ledger (flat totals, no `hours`, no `backfill_done`)
/// holding one (day, model) row from `m`'s flat fields.
fn write_v1_ledger(
    clauth_dir: &std::path::Path,
    recorded_through: &str,
    day: &str,
    m: &ModelTokens,
) {
    std::fs::create_dir_all(clauth_dir).expect("create .clauth");
    let json = format!(
        r#"{{"recorded_through":"{recorded_through}","days":{{"{day}":{{"{}":{{"input":{},"output":{},"cache_read":{},"cache_create":{}}}}}}}}}"#,
        m.model, m.input, m.output, m.cache_read, m.cache_create
    );
    std::fs::write(clauth_dir.join("token_ledger.json"), json).expect("write ledger");
}

/// 00:00 UTC of a "YYYY-MM-DD" date as a `SystemTime`, for mtime fixtures.
fn epoch_day(date: &str) -> SystemTime {
    let secs =
        crate::usage::iso_to_epoch_secs(&format!("{date}T00:00:00+00:00")).expect("parse date");
    UNIX_EPOCH + Duration::from_secs(secs as u64)
}

/// A v1 ledger day whose corpus reproduces the stored flat totals exactly
/// gains hour buckets through the worker's own gate, with the flat totals
/// untouched — the day now prices peak/off-peak exactly.
#[test]
fn backfill_fills_hours_when_corpus_totals_match() {
    let sb = HomeSandbox::new();
    let claude_dir = make_claude_dir(&sb);
    let clauth_dir = sb.home().join(".clauth");
    write_v1_ledger(
        &clauth_dir,
        "2026-06-16",
        "2026-06-15",
        &ModelTokens {
            model: "claude-opus-4".into(),
            input: 300,
            output: 100,
            cache_read: 20,
            cache_create: 10,
        },
    );

    let proj_dir = claude_dir.join("projects").join("p1");
    std::fs::create_dir_all(&proj_dir).expect("create project dir");
    let p = proj_dir.join("sess.jsonl");
    let line = jsonl_line(
        "2026-06-15T10:30:00+00:00",
        "claude-opus-4",
        300,
        100,
        20,
        10,
    );
    std::fs::write(&p, format!("{line}\n")).expect("write");
    // mtime before the recorded_through cutoff (00:00 UTC 2026-06-16), so the
    // backfill owns the file (the top-up owns anything newer).
    set_mtime(&p, epoch_day("2026-06-15") + Duration::from_secs(60));
    // A file the mtime guard must exclude: touched after the widened
    // end-of-watermark-day bound (00:00 UTC of the day after the watermark),
    // holding a pre-cutoff line that would push the derived totals past the
    // stored ones (and thereby block the fill) if the guard ever stopped
    // filtering.
    let late = proj_dir.join("late.jsonl");
    let late_line = jsonl_line("2026-06-15T11:00:00+00:00", "claude-opus-4", 50, 10, 0, 0);
    std::fs::write(&late, format!("{late_line}\n")).expect("write");
    set_mtime(&late, epoch_day("2026-06-17") + Duration::from_secs(1));

    let today = today_date();
    let mut ledger = crate::token_ledger::Ledger::load(&clauth_dir);
    assert!(
        ledger.backfill_through(&today).is_some(),
        "a pre-today v1 row arms the pass"
    );
    assert!(run_backfill(&claude_dir, &mut ledger, &today, |_, _| {}));
    ledger.save(&clauth_dir);

    // The filled row reaches the render path with hours and unchanged flats.
    let mut base = TokenStats::default();
    ledger.apply_to_base(&mut base, Some("2026-06-01"));
    let row = base
        .daily_models
        .iter()
        .find(|d| d.date == "2026-06-15" && d.model == "claude-opus-4")
        .expect("ledger day folded");
    assert_eq!(row.in_out, 400, "flat totals unchanged");
    let split = row.split.as_ref().expect("split");
    assert_eq!(split.input, 300);
    assert_eq!(split.output, 100);
    assert_eq!(split.cache_read, 20);
    assert_eq!(split.cache_create, 10);
    let hours = row.hours.expect("hours filled");
    assert_eq!(hours[10].input, 300);
    assert_eq!(hours[10].output, 100);
    assert_eq!(hours[10].cache_read, 20);
    assert_eq!(hours[10].cache_create, 10);
    assert_eq!(hours[9].input, 0);

    // The flag + hours survive the save/reload round trip. Pinned on the
    // file bytes because the filled hours alone already make
    // `backfill_through` None — only the persisted flag proves the pass was
    // marked done (the state that stops the mismatch case re-sweeping).
    let raw = std::fs::read_to_string(clauth_dir.join("token_ledger.json")).expect("read ledger");
    assert!(
        raw.contains("\"backfill_done\":true"),
        "the done flag persists: {raw}"
    );
    let reloaded = crate::token_ledger::Ledger::load(&clauth_dir);
    assert_eq!(reloaded.backfill_through(&today), None);
}

/// When the corpus no longer reproduces the stored totals (a pruned
/// transcript), the row keeps its v1 shape — hours stay `None`, the flat
/// totals are untouched — and the pass still marks itself done, so the
/// one-shot sweep never re-runs.
#[test]
fn backfill_leaves_mismatched_rows_untouched_and_marks_done() {
    let sb = HomeSandbox::new();
    let claude_dir = make_claude_dir(&sb);
    let clauth_dir = sb.home().join(".clauth");
    write_v1_ledger(
        &clauth_dir,
        "2026-06-16",
        "2026-06-15",
        &ModelTokens {
            model: "claude-opus-4".into(),
            input: 300,
            output: 100,
            cache_read: 20,
            cache_create: 10,
        },
    );

    // The corpus holds only part of the day (the rest was pruned): the
    // re-derived totals no longer match the stored ones.
    let proj_dir = claude_dir.join("projects").join("p1");
    std::fs::create_dir_all(&proj_dir).expect("create project dir");
    let p = proj_dir.join("sess.jsonl");
    let line = jsonl_line("2026-06-15T10:30:00+00:00", "claude-opus-4", 100, 50, 0, 0);
    std::fs::write(&p, format!("{line}\n")).expect("write");
    set_mtime(&p, epoch_day("2026-06-15") + Duration::from_secs(60));

    let today = today_date();
    let mut ledger = crate::token_ledger::Ledger::load(&clauth_dir);
    assert!(run_backfill(&claude_dir, &mut ledger, &today, |_, _| {}));
    ledger.save(&clauth_dir);

    let mut base = TokenStats::default();
    ledger.apply_to_base(&mut base, Some("2026-06-01"));
    let row = base
        .daily_models
        .iter()
        .find(|d| d.date == "2026-06-15" && d.model == "claude-opus-4")
        .expect("ledger day folded");
    assert!(row.hours.is_none(), "a mismatch never fills");
    let split = row.split.as_ref().expect("split");
    assert_eq!(split.input, 300, "stored v1 totals untouched");
    assert_eq!(split.output, 100);
    assert_eq!(split.cache_read, 20);
    assert_eq!(split.cache_create, 10);
    assert_eq!(
        ledger.backfill_through(&today),
        None,
        "the pass is done even when nothing filled — the still-hours-less row would re-arm it otherwise"
    );
}

/// A file touched ON the recorded_through day contributes by LINE date:
/// lines dated the day before, ON, and after the watermark route to their
/// own dates — the post-cutoff line is dropped (the regular top-up owns
/// it), the on-cutoff line still counts.
#[test]
fn backfill_routes_crossing_cutoff_lines_by_line_date() {
    let sb = HomeSandbox::new();
    let claude_dir = make_claude_dir(&sb);
    let clauth_dir = sb.home().join(".clauth");
    std::fs::create_dir_all(&clauth_dir).expect("create .clauth");
    std::fs::write(
        clauth_dir.join("token_ledger.json"),
        r#"{
            "recorded_through": "2026-06-16",
            "days": {
                "2026-06-14": {
                    "claude-opus-4": {"input": 5, "output": 2, "cache_read": 0, "cache_create": 0}
                },
                "2026-06-15": {
                    "claude-opus-4": {"input": 100, "output": 50, "cache_read": 0, "cache_create": 0}
                },
                "2026-06-16": {
                    "claude-opus-4": {"input": 7, "output": 3, "cache_read": 0, "cache_create": 0}
                }
            }
        }"#,
    )
    .expect("write v1 ledger");

    let proj_dir = claude_dir.join("projects").join("p1");
    std::fs::create_dir_all(&proj_dir).expect("create project dir");
    let p = proj_dir.join("sess.jsonl");
    let before = jsonl_line("2026-06-15T22:00:00+00:00", "claude-opus-4", 100, 50, 0, 0);
    let on_cutoff = jsonl_line("2026-06-16T08:00:00+00:00", "claude-opus-4", 7, 3, 0, 0);
    let after = jsonl_line("2026-06-17T10:00:00+00:00", "claude-opus-4", 999, 999, 0, 0);
    std::fs::write(&p, format!("{before}\n{on_cutoff}\n{after}\n")).expect("write");
    // mtime ON the watermark day (noon 2026-06-16): admitted by the widened
    // end-of-watermark-day bound, and exactly the case that bound exists for
    // — a file touched on the watermark day whose pre-watermark lines the
    // stored v1 totals include. An 00:00-instant bound would exclude it and
    // leave both days unfilled forever.
    set_mtime(&p, epoch_day("2026-06-16") + Duration::from_secs(12 * 3600));
    // A second file whose mtime sits EXACTLY at the end-of-watermark-day
    // instant (00:00 UTC 2026-06-17 == the widened bound): admitted by the
    // `>` comparison. Flipping it to `>=` would exclude this file and leave
    // day 06-14 unfilled.
    let boundary = proj_dir.join("boundary.jsonl");
    let b_line = jsonl_line("2026-06-14T14:00:00+00:00", "claude-opus-4", 5, 2, 0, 0);
    std::fs::write(&boundary, format!("{b_line}\n")).expect("write");
    set_mtime(&boundary, epoch_day("2026-06-17"));

    let sweep = backfill_corpus(&claude_dir, "2026-06-16", |_, _| {}).expect("cutoff parseable");
    assert_eq!(sweep.files_parsed, 2);
    let day14 = sweep
        .derived
        .get(&("2026-06-14".to_owned(), "claude-opus-4".to_owned()))
        .expect("the boundary-instant file is admitted");
    assert_eq!(day14.flat.input, 5);
    assert_eq!(day14.hours[14].input, 5);
    let day15 = sweep
        .derived
        .get(&("2026-06-15".to_owned(), "claude-opus-4".to_owned()))
        .expect("pre-cutoff line aggregated under its own date");
    assert_eq!(day15.flat.input, 100);
    assert_eq!(
        day15.hours[22].input, 100,
        "the line's hour wins over the file mtime's day"
    );
    let day16 = sweep
        .derived
        .get(&("2026-06-16".to_owned(), "claude-opus-4".to_owned()))
        .expect("the on-cutoff line counts under its own date");
    assert_eq!(day16.flat.input, 7);
    assert_eq!(day16.hours[8].input, 7);
    assert!(
        !sweep.derived.keys().any(|(date, _)| date == "2026-06-17"),
        "the post-cutoff line is dropped — the top-up owns it"
    );

    // End to end: each line's day gains hours in its own line's hour.
    let today = today_date();
    let mut ledger = crate::token_ledger::Ledger::load(&clauth_dir);
    assert!(run_backfill(&claude_dir, &mut ledger, &today, |_, _| {}));
    let mut base = TokenStats::default();
    ledger.apply_to_base(&mut base, Some("2026-06-01"));
    let row14 = base
        .daily_models
        .iter()
        .find(|d| d.date == "2026-06-14" && d.model == "claude-opus-4")
        .expect("ledger day folded");
    let hours14 = row14.hours.expect("hours filled");
    assert_eq!(hours14[14].input, 5);
    let row15 = base
        .daily_models
        .iter()
        .find(|d| d.date == "2026-06-15" && d.model == "claude-opus-4")
        .expect("ledger day folded");
    let hours15 = row15.hours.expect("hours filled");
    assert_eq!(hours15[22].input, 100);
    assert_eq!(hours15[10].input, 0);
    let row16 = base
        .daily_models
        .iter()
        .find(|d| d.date == "2026-06-16" && d.model == "claude-opus-4")
        .expect("ledger day folded");
    let hours16 = row16.hours.expect("hours filled");
    assert_eq!(hours16[8].input, 7);
}

/// The cross-file dedup mirrors merge_topup: a tok_key mirrored into two
/// files lands in the path-sorted first copy's hour, so a filled day equals
/// what the top-up would have recorded.
#[test]
fn backfill_dedup_lands_in_the_path_sorted_winners_hour() {
    let sb = HomeSandbox::new();
    let claude_dir = make_claude_dir(&sb);
    let clauth_dir = sb.home().join(".clauth");
    write_v1_ledger(
        &clauth_dir,
        "2026-06-16",
        "2026-06-15",
        &ModelTokens {
            model: "claude-opus-4".into(),
            input: 100,
            output: 50,
            cache_read: 0,
            cache_create: 0,
        },
    );

    let proj_dir = claude_dir.join("projects").join("p1");
    std::fs::create_dir_all(&proj_dir).expect("create project dir");
    let line_a = jsonl_line_with_ids(
        "2026-06-15T12:00:00+00:00",
        "req_1",
        "msg_1",
        "claude-opus-4",
        100,
        50,
    );
    let line_b = jsonl_line_with_ids(
        "2026-06-15T13:00:00+00:00",
        "req_1",
        "msg_1",
        "claude-opus-4",
        100,
        50,
    );
    for (name, line) in [("a.jsonl", &line_a), ("b.jsonl", &line_b)] {
        let f = proj_dir.join(name);
        std::fs::write(&f, format!("{line}\n")).expect("write");
        set_mtime(&f, epoch_day("2026-06-15") + Duration::from_secs(60));
    }

    let sweep = backfill_corpus(&claude_dir, "2026-06-16", |_, _| {}).expect("cutoff parseable");
    assert_eq!(sweep.files_parsed, 2);
    let acc = sweep
        .derived
        .get(&("2026-06-15".to_owned(), "claude-opus-4".to_owned()))
        .expect("day row");
    assert_eq!(acc.flat.input, 100, "counted once");
    assert_eq!(acc.flat.output, 50);
    assert_eq!(
        acc.hours[12].input, 100,
        "the path-sorted first copy's hour wins"
    );
    assert_eq!(acc.hours[13].input, 0, "the later copy's hour stays empty");
}

/// Once the pass persisted `backfill_done`, a second run visits nothing —
/// the gate returns before any file is walked — and the ledger file stays
/// byte-identical.
#[test]
fn backfill_runs_once_and_second_run_visits_nothing() {
    let sb = HomeSandbox::new();
    let claude_dir = make_claude_dir(&sb);
    let clauth_dir = sb.home().join(".clauth");
    write_v1_ledger(
        &clauth_dir,
        "2026-06-16",
        "2026-06-15",
        &ModelTokens {
            model: "claude-opus-4".into(),
            input: 300,
            output: 100,
            cache_read: 20,
            cache_create: 10,
        },
    );

    let proj_dir = claude_dir.join("projects").join("p1");
    std::fs::create_dir_all(&proj_dir).expect("create project dir");
    let p = proj_dir.join("sess.jsonl");
    let line = jsonl_line(
        "2026-06-15T10:30:00+00:00",
        "claude-opus-4",
        300,
        100,
        20,
        10,
    );
    std::fs::write(&p, format!("{line}\n")).expect("write");
    set_mtime(&p, epoch_day("2026-06-15") + Duration::from_secs(60));

    let today = today_date();
    let mut ledger = crate::token_ledger::Ledger::load(&clauth_dir);
    let mut visited = 0usize;
    assert!(run_backfill(&claude_dir, &mut ledger, &today, |_, _| {
        visited += 1
    }));
    assert_eq!(visited, 1, "the sweep walked the one corpus file");
    ledger.save(&clauth_dir);
    let bytes_after_first =
        std::fs::read(clauth_dir.join("token_ledger.json")).expect("read ledger");

    // Second run, the way the worker gates it: the persisted flag ends the
    // pass before a single file is visited, and nothing is re-saved.
    let mut ledger2 = crate::token_ledger::Ledger::load(&clauth_dir);
    assert_eq!(ledger2.backfill_through(&today), None);
    let mut visited2 = 0usize;
    assert!(!run_backfill(&claude_dir, &mut ledger2, &today, |_, _| {
        visited2 += 1
    }));
    assert_eq!(visited2, 0, "the done flag skips the sweep entirely");
    let bytes_after_second =
        std::fs::read(clauth_dir.join("token_ledger.json")).expect("read ledger");
    assert_eq!(bytes_after_first, bytes_after_second);
}

/// The backfill's save is independent of `record`'s: on an idle cycle
/// (watermark already at yesterday, nothing new to record) the done flag and
/// filled hours must still persist — otherwise the pass re-sweeps the whole
/// corpus every 90s until a day records.
#[test]
fn backfill_persists_when_record_has_nothing_new() {
    let sb = HomeSandbox::new();
    let claude_dir = make_claude_dir(&sb);
    let clauth_dir = sb.home().join(".clauth");
    let today = today_date();
    let yesterday = {
        let iso = crate::usage::epoch_secs_to_iso(crate::usage::now_epoch_secs() - 86_400);
        iso.get(..10).map(str::to_owned).unwrap_or(iso)
    };
    // A v1 ledger whose watermark is ALREADY at yesterday: record() has
    // nothing to advance or write, so its own save never fires.
    write_v1_ledger(
        &clauth_dir,
        &yesterday,
        "2026-06-15",
        &ModelTokens {
            model: "claude-opus-4".into(),
            input: 300,
            output: 100,
            cache_read: 20,
            cache_create: 10,
        },
    );

    let proj_dir = claude_dir.join("projects").join("p1");
    std::fs::create_dir_all(&proj_dir).expect("create project dir");
    let p = proj_dir.join("sess.jsonl");
    let line = jsonl_line(
        "2026-06-15T10:30:00+00:00",
        "claude-opus-4",
        300,
        100,
        20,
        10,
    );
    std::fs::write(&p, format!("{line}\n")).expect("write");
    set_mtime(&p, epoch_day("2026-06-15") + Duration::from_secs(60));

    let mut ledger = crate::token_ledger::Ledger::load(&clauth_dir);
    let base = TokenStats::default(); // nothing mergeable
    assert!(
        !ledger.record(&base, &today),
        "fixture: record() must have nothing to do"
    );
    persist_ledger(
        &claude_dir,
        &mut ledger,
        &clauth_dir,
        &base,
        &today,
        |_, _| {},
    );

    // The flag and the filled hours landed on disk without any record.
    let raw = std::fs::read_to_string(clauth_dir.join("token_ledger.json")).expect("read ledger");
    assert!(
        raw.contains("\"backfill_done\":true"),
        "flag persisted without a record: {raw}"
    );
    let reloaded = crate::token_ledger::Ledger::load(&clauth_dir);
    let mut fresh = TokenStats::default();
    reloaded.apply_to_base(&mut fresh, Some("2026-06-01"));
    let row = fresh
        .daily_models
        .iter()
        .find(|d| d.date == "2026-06-15" && d.model == "claude-opus-4")
        .expect("ledger day folded");
    assert_eq!(
        row.hours.expect("hours persisted without a record")[10].input,
        300
    );
}

/// The zero-fill persistence leg end to end: a pass that fills nothing (the
/// corpus no longer matches the stored day) still persists `backfill_done`
/// to disk through `persist_ledger` — otherwise the next cycle re-sweeps
/// the corpus forever.
#[test]
fn backfill_persists_flag_on_disk_when_nothing_fills() {
    let sb = HomeSandbox::new();
    let claude_dir = make_claude_dir(&sb);
    let clauth_dir = sb.home().join(".clauth");
    let today = today_date();
    let yesterday = {
        let iso = crate::usage::epoch_secs_to_iso(crate::usage::now_epoch_secs() - 86_400);
        iso.get(..10).map(str::to_owned).unwrap_or(iso)
    };
    write_v1_ledger(
        &clauth_dir,
        &yesterday,
        "2026-06-15",
        &ModelTokens {
            model: "claude-opus-4".into(),
            input: 300,
            output: 100,
            cache_read: 20,
            cache_create: 10,
        },
    );

    // The corpus holds only part of the stored day: nothing fills.
    let proj_dir = claude_dir.join("projects").join("p1");
    std::fs::create_dir_all(&proj_dir).expect("create project dir");
    let p = proj_dir.join("sess.jsonl");
    let line = jsonl_line("2026-06-15T10:30:00+00:00", "claude-opus-4", 100, 50, 0, 0);
    std::fs::write(&p, format!("{line}\n")).expect("write");
    set_mtime(&p, epoch_day("2026-06-15") + Duration::from_secs(60));

    let mut ledger = crate::token_ledger::Ledger::load(&clauth_dir);
    persist_ledger(
        &claude_dir,
        &mut ledger,
        &clauth_dir,
        &TokenStats::default(),
        &today,
        |_, _| {},
    );

    // The flag reached disk even though the pass filled nothing.
    let raw = std::fs::read_to_string(clauth_dir.join("token_ledger.json")).expect("read ledger");
    assert!(
        raw.contains("\"backfill_done\":true"),
        "a zero-fill pass still persists the flag: {raw}"
    );
    let reloaded = crate::token_ledger::Ledger::load(&clauth_dir);
    let mut fresh = TokenStats::default();
    reloaded.apply_to_base(&mut fresh, Some("2026-06-01"));
    let row = fresh
        .daily_models
        .iter()
        .find(|d| d.date == "2026-06-15" && d.model == "claude-opus-4")
        .expect("ledger day folded");
    assert!(row.hours.is_none(), "nothing filled, nothing changed");
}
