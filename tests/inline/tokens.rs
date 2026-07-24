use super::*;

use std::io::Write as _;
use std::time::{Duration, SystemTime};

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

fn day_model(date: &str, model: &str, in_out: u64, split: Option<ModelTokens>) -> DayModelTokens {
    DayModelTokens {
        date: date.into(),
        model: model.into(),
        in_out,
        split,
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
        day_model("2026-06-30", "claude-opus-4", 999, None),
        // stats-cache day: in+out only.
        day_model("2026-07-01", "claude-opus-4", 100, None),
        // transcript day: full split.
        day_model("2026-07-07", "claude-opus-4", 50, Some(split.clone())),
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

// ── 7. tokens.json snapshot builder (TOK-3) ──────────────────────────────────
//
// Every case here exercises the PURE builder — no worker thread, no disk. The
// fixture `today` is 2026-07-09 (a thursday), so week starts 2026-07-06 and month
// starts 2026-07-01, matching the bucketing tests above.

use crate::pricing::{ModelRate, PriceTable};

/// `PriceTable` from `(id, input, output, cache_read, cache_write)` per-token rows.
fn price_table(rows: &[(&str, f64, f64, f64, f64)]) -> PriceTable {
    let mut rates = std::collections::HashMap::new();
    for &(id, input, output, cache_read, cache_write) in rows {
        rates.insert(
            id.to_owned(),
            ModelRate {
                input,
                output,
                cache_read,
                cache_write,
            },
        );
    }
    PriceTable::from_rates(rates)
}

fn mt(model: &str, input: u64, output: u64, cache_read: u64, cache_create: u64) -> ModelTokens {
    ModelTokens {
        model: model.to_owned(),
        input,
        output,
        cache_read,
        cache_create,
    }
}

#[test]
fn snapshot_has_schema_version_and_all_four_periods() {
    let stats = TokenStats::default();
    let snap = build_tokens_snapshot(&stats, None, "2026-07-09");

    assert_eq!(snap["schema"], 1);
    assert!(snap["generated_at"].is_string());
    assert!(snap["clauth_version"].is_string());
    assert!(snap["topped_up_through"].is_null());

    let periods = &snap["periods"];
    for lens in ["today", "week", "month", "lifetime"] {
        assert!(periods[lens].is_object(), "missing period {lens}");
    }
    // today is a single day; lifetime is unbounded; week/month are the current bucket.
    assert_eq!(periods["today"]["from"], "2026-07-09");
    assert_eq!(periods["today"]["to"], "2026-07-09");
    assert!(periods["lifetime"]["from"].is_null());
    assert!(periods["lifetime"]["to"].is_null());
    assert_eq!(periods["week"]["from"], "2026-07-06");
    assert_eq!(periods["week"]["to"], "2026-07-09");
    assert_eq!(periods["month"]["from"], "2026-07-01");
    assert_eq!(periods["month"]["to"], "2026-07-09");
}

#[test]
fn week_and_month_windows_filter_daily_models() {
    let daily_models = vec![
        // Before the month → in neither window.
        day_model(
            "2026-06-30",
            "claude-opus-4",
            999,
            Some(mt("claude-opus-4", 500, 499, 0, 0)),
        ),
        // In the month, before the week → month only.
        day_model(
            "2026-07-02",
            "claude-opus-4",
            50,
            Some(mt("claude-opus-4", 30, 20, 0, 0)),
        ),
        // In the current week → both windows.
        day_model(
            "2026-07-07",
            "claude-opus-4",
            100,
            Some(mt("claude-opus-4", 60, 40, 0, 0)),
        ),
    ];
    let stats = TokenStats {
        daily_models,
        ..Default::default()
    };
    let snap = build_tokens_snapshot(&stats, None, "2026-07-09");

    let week = &snap["periods"]["week"];
    assert_eq!(week["in_out"], 100);
    assert_eq!(week["input"], 60);
    assert_eq!(week["output"], 40);
    assert_eq!(week["complete"], true);

    let month = &snap["periods"]["month"];
    assert_eq!(month["in_out"], 150); // 100 + 50
    assert_eq!(month["input"], 90); // 60 + 30
    assert_eq!(month["output"], 60); // 40 + 20
}

#[test]
fn incomplete_split_marks_period_and_cost_as_floor() {
    let daily_models = vec![
        // Full split (transcript-derived).
        day_model(
            "2026-07-07",
            "claude-opus-4",
            100,
            Some(mt("claude-opus-4", 60, 40, 0, 0)),
        ),
        // in+out only (stats-cache day) — no split → floors the period.
        day_model("2026-07-08", "gpt-5", 30, None),
    ];
    let stats = TokenStats {
        daily_models,
        ..Default::default()
    };
    let prices = price_table(&[
        ("claude-opus-4", 1e-6, 2e-6, 0.0, 0.0),
        ("gpt-5", 1e-6, 2e-6, 0.0, 0.0),
    ]);
    let snap = build_tokens_snapshot(&stats, Some(&prices), "2026-07-09");

    let week = &snap["periods"]["week"];
    assert_eq!(week["in_out"], 130); // always-known combined metric
    assert_eq!(week["input"], 60); // only the split-bearing row contributes
    assert_eq!(week["complete"], false);
    assert_eq!(week["cost_is_floor"], true);
    // Cost sums only the known split: 60*1e-6 + 40*2e-6 = 1.4e-4.
    let cost = week["cost_usd"].as_f64().expect("cost present");
    assert!((cost - 1.4e-4).abs() < 1e-12, "got {cost}");
}

#[test]
fn unpriced_model_with_tokens_marks_cost_floor() {
    let today = DaySummary {
        date: "2026-07-09".into(),
        models: vec![mt("claude-opus-4", 100, 0, 0, 0), mt("gpt-5", 50, 0, 0, 0)],
        ..Default::default()
    };
    let stats = TokenStats {
        today: Some(today),
        ..Default::default()
    };
    let prices = price_table(&[("claude-opus-4", 1e-6, 2e-6, 0.0, 0.0)]);
    let snap = build_tokens_snapshot(&stats, Some(&prices), "2026-07-09");

    let t = &snap["periods"]["today"];
    // Splits are full — the period is complete; only pricing is missing.
    assert_eq!(t["complete"], true);
    assert_eq!(t["cost_is_floor"], true); // gpt-5 carries tokens with no rate
    let cost = t["cost_usd"].as_f64().expect("cost");
    assert!((cost - 1e-4).abs() < 1e-12, "got {cost}"); // 100 * 1e-6, opus only

    let models = t["models"].as_array().expect("models");
    let opus = models
        .iter()
        .find(|m| m["model"] == "claude-opus-4")
        .expect("opus row");
    let gpt = models
        .iter()
        .find(|m| m["model"] == "gpt-5")
        .expect("gpt row");
    assert!(opus["cost_usd"].as_f64().is_some());
    assert!(gpt["cost_usd"].is_null());
}

#[test]
fn caps_period_at_eight_rows_folding_the_tail_into_others() {
    // 10 distinct claude models → group_models keeps all individual (no fold),
    // then the 8-row cap folds the smallest 3 into one trailing "others".
    let models: Vec<ModelTokens> = (0..10)
        .map(|i| mt(&format!("claude-m{i}"), (10 - i as u64) * 100, 0, 0, 0))
        .collect();
    let stats = TokenStats {
        total_input: models.iter().map(|m| m.input).sum(),
        models,
        ..Default::default()
    };
    let snap = build_tokens_snapshot(&stats, None, "2026-07-09");

    let life = &snap["periods"]["lifetime"];
    let rows = life["models"].as_array().expect("models");
    assert_eq!(rows.len(), 8, "capped to 8 rows");
    assert_eq!(
        rows.iter().filter(|m| m["model"] == "others").count(),
        1,
        "exactly one others row"
    );
    assert_eq!(rows.last().unwrap()["model"], "others", "others trails");
    // Folded tail = m7 + m8 + m9 = 300 + 200 + 100.
    assert_eq!(rows.last().unwrap()["in_out"], 600);
    // The fold preserves the period total: 1000 + 900 + … + 100 = 5500.
    assert_eq!(life["in_out"], 5500);
}

#[test]
fn today_none_emits_a_zero_complete_period() {
    let stats = TokenStats::default(); // today: None
    let prices = price_table(&[("claude-opus-4", 1e-6, 2e-6, 0.0, 0.0)]);
    let snap = build_tokens_snapshot(&stats, Some(&prices), "2026-07-09");

    let t = &snap["periods"]["today"];
    assert_eq!(t["in_out"], 0);
    assert_eq!(t["total"], 0);
    assert_eq!(t["input"], 0);
    assert_eq!(t["complete"], true);
    // Price table present → an explicit 0.0, not null; nothing unpriced → not a floor.
    assert_eq!(t["cost_usd"].as_f64(), Some(0.0));
    assert_eq!(t["cost_is_floor"], false);
    assert!(t["models"].as_array().expect("models").is_empty());
    assert_eq!(t["from"], "2026-07-09");
    assert_eq!(t["to"], "2026-07-09");
}

#[test]
fn cost_is_null_without_a_price_table() {
    let today = DaySummary {
        date: "2026-07-09".into(),
        models: vec![mt("claude-opus-4", 100, 50, 0, 0)],
        ..Default::default()
    };
    let stats = TokenStats {
        today: Some(today),
        ..Default::default()
    };
    let snap = build_tokens_snapshot(&stats, None, "2026-07-09");

    for lens in ["today", "week", "month", "lifetime"] {
        assert!(
            snap["periods"][lens]["cost_usd"].is_null(),
            "{lens} cost should be null with no price table"
        );
        assert_eq!(snap["periods"][lens]["cost_is_floor"], false);
    }
    let m = &snap["periods"]["today"]["models"][0];
    assert!(m["cost_usd"].is_null());
    // Display name mapping is wired through the builder.
    assert_eq!(m["display"], "opus 4");
}

#[test]
fn lifetime_totals_and_cost_count_cache() {
    let stats = TokenStats {
        total_input: 1_000_000,
        total_output: 1_000_000,
        total_cache_read: 1_000_000,
        total_cache_create: 1_000_000,
        models: vec![mt(
            "claude-opus-4",
            1_000_000,
            1_000_000,
            1_000_000,
            1_000_000,
        )],
        ..Default::default()
    };
    let prices = price_table(&[("claude-opus-4", 1e-6, 2e-6, 1e-7, 1.25e-6)]);
    let snap = build_tokens_snapshot(&stats, Some(&prices), "2026-07-09");

    let life = &snap["periods"]["lifetime"];
    assert_eq!(life["input"], 1_000_000);
    assert_eq!(life["output"], 1_000_000);
    assert_eq!(life["cache_read"], 1_000_000);
    assert_eq!(life["cache_create"], 1_000_000);
    assert_eq!(life["in_out"], 2_000_000);
    assert_eq!(life["total"], 4_000_000);
    assert_eq!(life["complete"], true);
    assert_eq!(life["cost_is_floor"], false);
    // Cache always counts: 1.0 + 2.0 + 0.10 + 1.25 = 4.35.
    let cost = life["cost_usd"].as_f64().expect("cost");
    assert!((cost - 4.35).abs() < 1e-9, "got {cost}");
}
