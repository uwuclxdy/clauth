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

#[test]
fn transcript_placeholder_models_keep_messages_without_token_buckets() {
    let sb = HomeSandbox::new();
    let claude_dir = make_claude_dir(&sb);
    write_stats_cache(
        &claude_dir,
        r#"{"lastComputedDate":"2026-06-10","totalSessions":0,"totalMessages":0,"dailyActivity":[],"dailyModelTokens":[],"modelUsage":{},"hourCounts":{}}"#,
    );
    let project = claude_dir.join("projects/p1");
    std::fs::create_dir_all(&project).expect("create project");
    let path = project.join("session.jsonl");
    std::fs::write(
        &path,
        [
            jsonl_line("2026-06-11T10:00:00+00:00", "<synthetic>", 0, 0, 0, 0),
            jsonl_line("2026-06-11T10:01:00+00:00", "claude-opus-4", 120, 30, 0, 0),
            jsonl_line(
                "2026-06-11T10:02:00+00:00",
                "model<placeholder>",
                4,
                3,
                0,
                0,
            ),
            jsonl_line(
                "2026-06-11T10:03:00+00:00",
                "minimax/minimax-m3:free",
                8,
                2,
                0,
                0,
            ),
        ]
        .join("\n"),
    )
    .expect("write transcript");
    let stats = load(&claude_dir).expect("load stats");
    assert_eq!(stats.total_messages, 4);
    assert_eq!(
        stats
            .models
            .iter()
            .map(|m| m.model.as_str())
            .collect::<Vec<_>>(),
        vec!["claude-opus-4", "minimax/minimax-m3:free"]
    );
    assert_eq!(stats.daily.len(), 1);
    assert_eq!(stats.daily[0].date, "2026-06-11");
    assert_eq!(stats.daily[0].tokens, 160);
    assert_eq!(
        stats
            .daily_models
            .iter()
            .map(|m| m.model.as_str())
            .collect::<Vec<_>>(),
        vec!["claude-opus-4", "minimax/minimax-m3:free"]
    );
    assert_eq!(stats.total_input, 128);
    assert_eq!(stats.total_output, 32);
    let weekly = period_models(&stats.daily_models, "2026-06-08", "2026-06-14");
    assert_eq!(weekly.len(), 2);
    assert_eq!(
        weekly.iter().map(|m| m.model.as_str()).collect::<Vec<_>>(),
        vec!["claude-opus-4", "minimax/minimax-m3:free"]
    );
}

#[test]
fn todays_placeholder_keeps_the_message_and_no_today_model() {
    let sb = HomeSandbox::new();
    let claude_dir = make_claude_dir(&sb);
    write_stats_cache(
        &claude_dir,
        r#"{"lastComputedDate":"2026-06-10","totalSessions":0,"totalMessages":0,"dailyActivity":[],"dailyModelTokens":[],"modelUsage":{},"hourCounts":{}}"#,
    );
    let project = claude_dir.join("projects/p1");
    std::fs::create_dir_all(&project).expect("create project");
    let date = today_date();
    std::fs::write(
        project.join("session.jsonl"),
        [
            jsonl_line(&format!("{date}T10:00:00+00:00"), "<synthetic>", 0, 0, 0, 0),
            jsonl_line(
                &format!("{date}T10:01:00+00:00"),
                "minimax/minimax-m3:free",
                8,
                2,
                0,
                0,
            ),
        ]
        .join("\n"),
    )
    .expect("write transcript");
    let stats = load(&claude_dir).expect("load stats");
    let today = stats.today.expect("today summary");
    assert_eq!(today.messages, 2);
    assert_eq!(today.models.len(), 1);
    assert_eq!(today.models[0].model, "minimax/minimax-m3:free");
    assert_eq!((today.input, today.output), (8, 2));
}

#[test]
fn placeholder_streamed_deltas_count_as_one_message() {
    let sb = HomeSandbox::new();
    let claude_dir = make_claude_dir(&sb);
    write_stats_cache(
        &claude_dir,
        r#"{"lastComputedDate":"2026-06-10","totalSessions":0,"totalMessages":0,"dailyActivity":[],"dailyModelTokens":[],"modelUsage":{},"hourCounts":{}}"#,
    );
    let project = claude_dir.join("projects/p1");
    std::fs::create_dir_all(&project).expect("create project");
    std::fs::write(
        project.join("session.jsonl"),
        [
            streamed_line(
                "2026-06-11T10:00:00+00:00",
                "msg_1",
                "u1",
                "<synthetic>",
                0,
                0,
            ),
            streamed_line(
                "2026-06-11T10:00:01+00:00",
                "msg_1",
                "u2",
                "<synthetic>",
                0,
                0,
            ),
        ]
        .join("\n"),
    )
    .expect("write transcript");
    let stats = load(&claude_dir).expect("load stats");
    assert_eq!(stats.total_messages, 1);
    assert!(stats.models.is_empty());
    assert!(stats.daily_models.is_empty());
}

#[test]
fn a_fast_line_marks_its_hour_buckets_fast() {
    let sb = HomeSandbox::new();
    let path = sb.home().join("fast.jsonl");
    std::fs::write(
        &path,
        [
            r#"{"timestamp":"2026-10-28T09:00:00+00:00","message":{"id":"msg_fast","model":"claude-opus-5-5","usage":{"input_tokens":10,"output_tokens":20,"cache_read_input_tokens":5,"cache_creation_input_tokens":2,"speed":"fast"}}}"#,
            r#"{"timestamp":"2026-10-28T15:00:00+00:00","message":{"id":"msg_std","model":"claude-opus-5-5","usage":{"input_tokens":30,"output_tokens":40,"cache_read_input_tokens":7,"cache_creation_input_tokens":3,"speed":"standard"}}}"#,
            // An older client writes no speed at all: standard, never guessed.
            r#"{"timestamp":"2026-10-28T16:00:00+00:00","message":{"id":"msg_old","model":"claude-opus-5-5","usage":{"input_tokens":1,"output_tokens":2}}}"#,
        ]
        .join("\n"),
    )
    .expect("write transcript");
    let recs = parse_file(&path);
    assert_eq!(recs.len(), 3);
    let by_key = |key: &str| {
        recs.iter()
            .find(|r| r.tok_key == key)
            .unwrap_or_else(|| panic!("no rec for {key}"))
    };
    assert!(by_key("msg_fast").fast, "usage.speed fast marks the line");
    assert!(!by_key("msg_std").fast, "a standard line stays standard");
    assert!(
        !by_key("msg_old").fast,
        "an absent speed is standard, not guessed"
    );

    // The fast subset rides the same hour buckets the totals do.
    let mut acc = ModelDayAcc {
        flat: ModelTokens {
            model: "claude-opus-5-5".into(),
            input: 41,
            output: 62,
            cache_read: 12,
            cache_create: 5,
            shape: UsageShape::Healthy,
            ..Default::default()
        },
        hours: [HourTokens::default(); 24],
    };
    for r in &recs {
        add_to_hour(&mut acc.hours[r.hour as usize], r);
    }
    assert_eq!(acc.hours[9].fast_output, 20);
    assert_eq!(acc.hours[9].output, 20);
    assert_eq!(acc.hours[9].fast_input, 10);
    assert_eq!(acc.hours[9].fast_cache_read, 5);
    assert_eq!(acc.hours[15].output, 40);
    assert_eq!(acc.hours[15].fast_output, 0);
    assert_eq!(acc.hours[16].output, 2, "an absent speed adds nothing fast");
}

/// The fast subset rides the flat aggregates beside the hour buckets: the
/// lifetime model rows the merge accumulates, the pushed day rows' splits,
/// and the buckets themselves always agree.
#[test]
fn a_fast_line_fills_the_flat_lifetime_aggregate_fast() {
    let sb = HomeSandbox::new();
    let claude_dir = make_claude_dir(&sb);
    write_stats_cache(
        &claude_dir,
        r#"{"lastComputedDate":"2026-06-10","totalSessions":0,"totalMessages":0,"dailyActivity":[],"dailyModelTokens":[],"modelUsage":{},"hourCounts":{}}"#,
    );
    let project = claude_dir.join("projects/p1");
    std::fs::create_dir_all(&project).expect("create project");
    std::fs::write(
        project.join("session.jsonl"),
        [
            r#"{"timestamp":"2026-06-11T10:00:00+00:00","message":{"id":"msg_fast","model":"claude-opus-5-5","usage":{"input_tokens":10,"output_tokens":20,"cache_read_input_tokens":5,"cache_creation_input_tokens":2,"speed":"fast"}}}"#,
            r#"{"timestamp":"2026-06-11T15:00:00+00:00","message":{"id":"msg_std","model":"claude-opus-5-5","usage":{"input_tokens":30,"output_tokens":40,"cache_read_input_tokens":7,"cache_creation_input_tokens":3}}}"#,
        ]
        .join("\n"),
    )
    .expect("write transcript");
    let stats = load(&claude_dir).expect("load stats");

    let m = stats
        .models
        .iter()
        .find(|m| m.model == "claude-opus-5-5")
        .expect("lifetime model row");
    assert_eq!(
        (m.input, m.output, m.cache_read, m.cache_create),
        (40, 60, 12, 5)
    );
    assert_eq!(
        (
            m.fast_input,
            m.fast_output,
            m.fast_cache_read,
            m.fast_cache_create
        ),
        (10, 20, 5, 2),
        "the fast subset rides the lifetime aggregate beside the totals"
    );

    let row = stats
        .daily_models
        .iter()
        .find(|d| d.model == "claude-opus-5-5")
        .expect("day row");
    let split = row.split.as_ref().expect("split");
    assert_eq!((split.fast_input, split.fast_output), (10, 20));
    assert_eq!(
        row.hours.as_ref().expect("hours")[10].fast_output,
        20,
        "the bucket and the flat split agree on the fast subset"
    );
}

#[test]
fn transcript_usage_without_a_model_keeps_unknown_bucket() {
    let sb = HomeSandbox::new();
    let dir = make_claude_dir(&sb);
    let path = dir.join("unknown.jsonl");
    std::fs::write(
        &path,
        r#"{"timestamp":"2026-06-11T12:00:00+00:00","message":{"usage":{"input_tokens":3,"output_tokens":1}}}"#,
    )
    .expect("write transcript");
    let records = parse_file(&path);
    assert_eq!(records.len(), 1);
    assert!(records[0].has_usage);
    assert_eq!(records[0].model, "unknown");
    assert_eq!((records[0].input, records[0].output), (3, 1));
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
            shape: Default::default(),
            ..Default::default()
        },
        ModelTokens {
            model: "gpt-5.5".to_owned(),
            input: 200,
            output: 100,
            cache_read: 0,
            cache_create: 0,
            shape: Default::default(),
            ..Default::default()
        },
        ModelTokens {
            model: "gemini-3-flash".to_owned(),
            input: 300,
            output: 150,
            cache_read: 0,
            cache_create: 0,
            shape: Default::default(),
            ..Default::default()
        },
        ModelTokens {
            model: "claude-sonnet-4".to_owned(),
            input: 500,
            output: 250,
            cache_read: 50,
            cache_create: 25,
            shape: Default::default(),
            ..Default::default()
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
        shape: Default::default(),
        ..Default::default()
    }];
    let grouped = group_models(&models);
    assert!(grouped.iter().all(|m| m.model != "others"));
}

#[test]
fn group_models_empty_input() {
    assert!(group_models(&[]).is_empty());
}

/// The "others" fold carries the fast subset beside the totals: a folded row
/// holds the same fast split its sources do, so a fold never drops a fast
/// token.
#[test]
fn group_models_others_fold_keeps_the_fast_tokens() {
    let models = vec![
        ModelTokens {
            model: "tiny-a".to_owned(),
            input: 200,
            output: 100,
            fast_input: 120,
            fast_output: 60,
            fast_cache_read: 30,
            fast_cache_create: 3,
            ..Default::default()
        },
        ModelTokens {
            model: "tiny-b".to_owned(),
            input: 100,
            output: 50,
            fast_input: 40,
            fast_output: 20,
            fast_cache_read: 10,
            fast_cache_create: 1,
            ..Default::default()
        },
    ];
    let grouped = group_models(&models);
    let others = grouped
        .iter()
        .find(|m| m.model == "others")
        .expect("small models fold into others");
    assert_eq!(others.input, 300);
    assert_eq!(others.output, 150);
    assert_eq!(others.fast_input, 160); // 120 + 40
    assert_eq!(others.fast_output, 80); // 60 + 20
    assert_eq!(others.fast_cache_read, 40); // 30 + 10
    assert_eq!(others.fast_cache_create, 4); // 3 + 1
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
            shape: Default::default(),
            ..Default::default()
        },
        // < 1M total → folds into "others".
        ModelTokens {
            model: "tiny-model".to_owned(),
            input: 100,
            output: 50,
            cache_read: 0,
            cache_create: 0,
            shape: Default::default(),
            ..Default::default()
        },
        ModelTokens {
            model: "claude-opus-4-8".to_owned(),
            input: 500,
            output: 250,
            cache_read: 0,
            cache_create: 0,
            shape: Default::default(),
            ..Default::default()
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
        speed_days: Default::default(),
        speed_cells: Default::default(),
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
        speed_days: Default::default(),
        speed_cells: Default::default(),
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

// ── 7. deepseek served-model mapping ──────────────────────────────────────────

fn write_owner_store(home: &std::path::Path, json: &str) {
    let dir = home.join(".clauth");
    std::fs::create_dir_all(&dir).expect("create .clauth");
    std::fs::write(dir.join("session_profiles.json"), json).expect("write owner store");
}

fn save_base_url_profile(name: &str, base_url: &str) {
    let mut profile = crate::testutil::blank_profile(&name.into());
    profile.base_url = Some(base_url.to_owned());
    crate::profile::save_profile(&profile).expect("save profile");
}

/// A stats-cache, a deepseek-owned conversation `sess` and its transcript
/// holding `lines`: the attribution the map must ignore, set up so a mapping
/// keyed on the owner would fire.
fn deepseek_owned_transcript(sb: &HomeSandbox, lines: &[String]) -> PathBuf {
    let claude_dir = make_claude_dir(sb);
    write_stats_cache(
        &claude_dir,
        r#"{"lastComputedDate":"2026-06-10","totalSessions":0,"totalMessages":0,"dailyActivity":[],"dailyModelTokens":[],"modelUsage":{},"hourCounts":{}}"#,
    );
    write_owner_store(sb.home(), r#"{"sessions":{"sess":{"known":"D1"}}}"#);
    save_base_url_profile("D1", "https://api.deepseek.com");
    let proj_dir = claude_dir.join("projects").join("p1");
    std::fs::create_dir_all(&proj_dir).expect("create project dir");
    std::fs::write(proj_dir.join("sess.jsonl"), lines.join("\n")).expect("write transcript");
    claude_dir
}

fn model_splits(stats: &TokenStats) -> Vec<(&str, u64, u64)> {
    let mut rows: Vec<_> = stats
        .models
        .iter()
        .map(|m| (m.model.as_str(), m.input, m.output))
        .collect();
    rows.sort_unstable();
    rows
}

#[test]
fn a_uuid_message_id_maps_requested_claude_ids_to_the_served_deepseek_model() {
    let sb = HomeSandbox::new();
    let claude_dir = make_claude_dir(&sb);
    write_stats_cache(
        &claude_dir,
        r#"{"lastComputedDate":"2026-06-10","totalSessions":0,"totalMessages":0,"dailyActivity":[],"dailyModelTokens":[],"modelUsage":{},"hourCounts":{}}"#,
    );
    // A contested conversation and one an anthropic profile owns: the ids
    // alone decide, whatever the owner.
    write_owner_store(
        sb.home(),
        r#"{"sessions":{"sess":"contested","sess2":{"known":"cld"}}}"#,
    );
    let proj_dir = claude_dir.join("projects").join("p1");
    std::fs::create_dir_all(&proj_dir).expect("create project dir");
    std::fs::write(
        proj_dir.join("sess2.jsonl"),
        jsonl_line_with_ids(
            "2026-06-11T10:03:00+00:00",
            "r4",
            "3a1b2c3d-4e5f-4a6b-8c7d-9e0f1a2b3c4d",
            "claude-opus-5-5",
            5,
            2,
        ),
    )
    .expect("write transcript");
    std::fs::write(
        proj_dir.join("sess.jsonl"),
        [
            jsonl_line_with_ids(
                "2026-06-11T10:00:00+00:00",
                "r1",
                "0a1b2c3d-4e5f-4a6b-8c7d-9e0f1a2b3c4d",
                "claude-opus-5-5",
                100,
                50,
            ),
            jsonl_line_with_ids(
                "2026-06-11T10:01:00+00:00",
                "r2",
                "1a1b2c3d-4e5f-4a6b-8c7d-9e0f1a2b3c4d",
                "claude-sonnet-5-5",
                40,
                20,
            ),
            // A served deepseek id passes untouched.
            jsonl_line_with_ids(
                "2026-06-11T10:02:00+00:00",
                "r3",
                "2a1b2c3d-4e5f-4a6b-8c7d-9e0f1a2b3c4d",
                "deepseek-v4-flash",
                30,
                10,
            ),
        ]
        .join("\n"),
    )
    .expect("write transcript");
    let stats = load(&claude_dir).expect("load stats");
    assert_eq!(
        model_splits(&stats),
        vec![
            ("deepseek-flash", 40, 20),
            ("deepseek-v4-flash", 30, 10),
            ("deepseek-v4-pro", 105, 52)
        ]
    );
}

#[test]
fn an_anthropic_message_id_keeps_its_claude_id_in_a_deepseek_owned_conversation() {
    let sb = HomeSandbox::new();
    let claude_dir = deepseek_owned_transcript(
        &sb,
        &[
            jsonl_line_with_ids(
                "2026-06-11T10:00:00+00:00",
                "r1",
                "msg_01AbCdEfGhIjKlMnOpQrStUv",
                "claude-opus-5-5",
                100,
                50,
            ),
            jsonl_line_with_ids(
                "2026-06-11T10:01:00+00:00",
                "r2",
                "3a1b2c3d-4e5f-4a6b-8c7d-9e0f1a2b3c4d",
                "claude-opus-5-5",
                7,
                3,
            ),
        ],
    );
    let stats = load(&claude_dir).expect("load stats");
    assert_eq!(
        model_splits(&stats),
        vec![("claude-opus-5-5", 100, 50), ("deepseek-v4-pro", 7, 3)]
    );
}

#[test]
fn an_id_less_line_never_maps() {
    let sb = HomeSandbox::new();
    let claude_dir = deepseek_owned_transcript(
        &sb,
        &[jsonl_line(
            "2026-06-11T10:00:00+00:00",
            "claude-opus-5-5",
            100,
            50,
            10,
            0,
        )],
    );
    let stats = load(&claude_dir).expect("load stats");
    assert_eq!(model_splits(&stats), vec![("claude-opus-5-5", 100, 50)]);
}

#[test]
fn only_a_lowercase_hyphenated_uuid_reads_as_a_deepseek_message_id() {
    let cases = [
        (
            "0a1b2c3d-4e5f-4a6b-8c7d-9e0f1a2b3c4d",
            Some("deepseek-v4-pro"),
        ),
        ("0A1B2C3D-4E5F-4A6B-8C7D-9E0F1A2B3C4D", None),
        ("0a1b2c3d4e5f4a6b8c7d9e0f1a2b3c4d", None),
        ("0a1b2c3d-4e5f-4a6b-8c7d-9e0f1a2b3c4", None),
        ("0a1b2c3d-4e5f-4a6b-8c7d-9e0f1a2b3c4de", None),
        ("0a1b2c3d4-e5f-4a6b-8c7d-9e0f1a2b3c4d", None),
        ("0a1b2c3d-4e5f-4a6b-8c7d-9e0f1a2b3c4g", None),
        ("0a1b2c3d-4e5f-4a6b-8c7d-9e0f1a2b3c-d", None),
        ("msg_01AbCdEfGhIjKlMnOpQrStUv", None),
        ("msg_20261009abcdef0123456789abcd", None),
        ("", None),
    ];
    for (id, want) in cases {
        assert_eq!(
            crate::providers::deepseek_served_model("claude-opus-5-5", id),
            want,
            "{id:?}"
        );
    }
}

#[test]
fn a_deepseek_message_id_maps_only_requested_claude_ids() {
    let id = "0a1b2c3d-4e5f-4a6b-8c7d-9e0f1a2b3c4d";
    let cases = [
        ("claude-opus-5-5", Some("deepseek-v4-pro")),
        ("claude-opus-5", Some("deepseek-v4-pro")),
        ("claude-sonnet-5-5", Some("deepseek-flash")),
        ("claude-haiku-5-5", Some("deepseek-flash")),
        ("claude-foo-1", Some("deepseek-flash")),
        ("deepseek-v4-pro", None),
        ("deepseek-flash", None),
        ("<synthetic>", None),
        ("glm-5.3-flash", None),
    ];
    for (requested, want) in cases {
        assert_eq!(
            crate::providers::deepseek_served_model(requested, id),
            want,
            "{requested:?}"
        );
    }
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
        shape: Default::default(),
        ..Default::default()
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
        shape: Default::default(),
        ..Default::default()
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
        ..Default::default()
    };
    let mut h07 = [HourTokens::default(); 24];
    h07[9] = HourTokens {
        input: 12,
        output: 8,
        cache_read: 0,
        cache_create: 0,
        ..Default::default()
    };
    let split_03 = ModelTokens {
        model: "claude-opus-4".into(),
        input: 30,
        output: 20,
        cache_read: 500,
        cache_create: 5,
        shape: Default::default(),
        ..Default::default()
    };
    let split_07 = ModelTokens {
        model: "claude-opus-4".into(),
        input: 12,
        output: 8,
        cache_read: 0,
        cache_create: 0,
        shape: Default::default(),
        ..Default::default()
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

/// The period aggregate's `split` folds each day's fast subset beside the
/// totals, so it stays consistent with its own `days` rows: a period over one
/// fast day carries the fast subset in `split`.
#[test]
fn period_models_split_carries_the_fast_token_subset() {
    let fast_day = ModelTokens {
        model: "claude-opus-4".into(),
        input: 30,
        output: 20,
        cache_read: 500,
        cache_create: 5,
        fast_input: 12,
        fast_output: 8,
        fast_cache_read: 120,
        fast_cache_create: 2,
        shape: Default::default(),
    };
    let days = vec![day_model(
        "2026-07-07",
        "claude-opus-4",
        50,
        Some(fast_day),
        None,
    )];
    let rows = period_models(&days, "2026-07-01", "2026-07-09");
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].days[0].split.fast_input, 12,
        "the day row carries the fast subset"
    );
    // The fold keeps the subset beside the totals: saturating, in-bounds.
    assert_eq!(rows[0].split.fast_input, 12);
    assert_eq!(rows[0].split.fast_output, 8);
    assert_eq!(rows[0].split.fast_cache_read, 120);
    assert_eq!(rows[0].split.fast_cache_create, 2);
    // Totals unchanged by the subset fold.
    assert_eq!(rows[0].split.input, 30);
    assert_eq!(rows[0].split.output, 20);
    assert_eq!(rows[0].split.cache_read, 500);
    assert_eq!(rows[0].split.cache_create, 5);
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
            input: 350,
            output: 110,
            cache_read: 20,
            cache_create: 10,
            shape: Default::default(),
            ..Default::default()
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
    // A live session's transcript, touched after the watermark day: the
    // stored totals were recorded from its 06-15 line too, so the sweep must
    // read it or the day never matches.
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
    assert_eq!(row.in_out, 460, "flat totals unchanged");
    let split = row.split.as_ref().expect("split");
    assert_eq!(split.input, 350);
    assert_eq!(split.output, 110);
    assert_eq!(split.cache_read, 20);
    assert_eq!(split.cache_create, 10);
    let hours = row.hours.expect("hours filled");
    assert_eq!(hours[10].input, 300);
    assert_eq!(hours[10].output, 100);
    assert_eq!(hours[10].cache_read, 20);
    assert_eq!(hours[10].cache_create, 10);
    assert_eq!(hours[11].input, 50);
    assert_eq!(hours[11].output, 10);
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
            shape: Default::default(),
            ..Default::default()
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
    // Touched on and after the watermark day: a file's mtime never gates
    // it, its lines route by their own dates.
    set_mtime(&p, epoch_day("2026-06-16") + Duration::from_secs(12 * 3600));
    let late = proj_dir.join("late.jsonl");
    let b_line = jsonl_line("2026-06-14T14:00:00+00:00", "claude-opus-4", 5, 2, 0, 0);
    std::fs::write(&late, format!("{b_line}\n")).expect("write");
    set_mtime(&late, epoch_day("2026-06-17"));

    let sweep = backfill_corpus(&claude_dir, "2026-06-16", |_, _| {});
    assert_eq!(sweep.files_parsed, 2);
    let day14 = sweep
        .derived
        .get(&("2026-06-14".to_owned(), "claude-opus-4".to_owned()))
        .expect("the file touched after the watermark day is read");
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
            shape: Default::default(),
            ..Default::default()
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

    let sweep = backfill_corpus(&claude_dir, "2026-06-16", |_, _| {});
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
            shape: Default::default(),
            ..Default::default()
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
            shape: Default::default(),
            ..Default::default()
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
            shape: Default::default(),
            ..Default::default()
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
// ── usage-shape classification ─────────────────────────────────────

/// Load one committed shape fixture (trimmed real transcript bytes, the
/// `wire-parity.md` precedent: golden shape from real usage, not synthesized).
fn shape_fixture(name: &str) -> std::path::PathBuf {
    let mut p = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push("tests/fixtures");
    p.push(name);
    p
}

fn fixture_totals(path: &std::path::Path) -> (u64, u64, u64, u64, UsageShape, usize) {
    let recs = super::parse_file(path);
    let usage: Vec<&super::LineRec> = recs.iter().filter(|r| r.has_usage).collect();
    assert_eq!(usage.len(), 12, "fixture holds 12 usage rows");
    let input: u64 = usage.iter().map(|r| r.input).sum();
    let output: u64 = usage.iter().map(|r| r.output).sum();
    let cr: u64 = usage.iter().map(|r| r.cache_read).sum();
    let cc: u64 = usage.iter().map(|r| r.cache_create).sum();
    let shape = usage
        .iter()
        .map(|r| r.shape)
        .max_by_key(|s| super::shape_rank(*s))
        .unwrap();
    (input, output, cr, cc, shape, usage.len())
}

/// Raw (pre-correction) sums read straight off the fixture bytes, so the
/// test asserts the correction against the provider's own numbers.
fn raw_sums(path: &std::path::Path) -> (u64, u64, u64, u64) {
    let raw = std::fs::read_to_string(path).expect("read fixture");
    let mut input = 0u64;
    let mut output = 0u64;
    let mut cr = 0u64;
    let mut cc = 0u64;
    for line in raw.lines() {
        let v: serde_json::Value = serde_json::from_str(line).expect("fixture line");
        let u = &v["message"]["usage"];
        input += u["input_tokens"].as_u64().unwrap_or(0);
        output += u["output_tokens"].as_u64().unwrap_or(0);
        cr += u["cache_read_input_tokens"].as_u64().unwrap_or(0);
        cc += u["cache_creation_input_tokens"].as_u64().unwrap_or(0);
    }
    (input, output, cr, cc)
}

#[test]
fn a1_fixture_corrects_input_to_uncached_tail() {
    let path = shape_fixture("tokens-shape-a1.jsonl");
    let (raw_in, raw_out, raw_cr, raw_cc) = raw_sums(&path);
    assert_eq!(
        raw_cc, 0,
        "a whole-prompt reporter never reports cache writes"
    );
    let (input, output, cr, cc, shape, _) = fixture_totals(&path);
    assert_eq!(shape, UsageShape::WholePromptInput);
    assert_eq!(
        input,
        raw_in - raw_cr,
        "input corrected: raw minus cache_read"
    );
    assert_eq!(output, raw_out);
    assert_eq!(cr, raw_cr);
    assert_eq!(cc, 0);
}

#[test]
fn healthy_fixture_is_left_byte_identical() {
    let path = shape_fixture("tokens-shape-healthy.jsonl");
    let (raw_in, raw_out, raw_cr, raw_cc) = raw_sums(&path);
    let (input, output, cr, cc, shape, _) = fixture_totals(&path);
    assert_eq!(shape, UsageShape::Healthy);
    assert_eq!((input, output, cr, cc), (raw_in, raw_out, raw_cr, raw_cc));
}

#[test]
fn a2_fixture_keeps_numbers_marks_shape() {
    let path = shape_fixture("tokens-shape-a2.jsonl");
    let (raw_in, raw_out, raw_cr, raw_cc) = raw_sums(&path);
    let (input, output, cr, cc, shape, _) = fixture_totals(&path);
    assert_eq!(shape, UsageShape::NoCacheWrites);
    assert_eq!((input, output, cr, cc), (raw_in, raw_out, raw_cr, raw_cc));
    assert_eq!(cc, 0);
}

#[test]
fn b_fixture_reads_no_cache_reporting() {
    let path = shape_fixture("tokens-shape-b.jsonl");
    let (input, output, cr, cc, shape, _) = fixture_totals(&path);
    assert_eq!(shape, UsageShape::NoCacheReporting);
    assert_eq!((cr, cc), (0, 0));
    assert!(input > 0 && output > 0);
}

/// Pin both whole-prompt discriminators on files a sum-ratio reading cannot
/// separate: constant cache reads under growing input classify whole-prompt
/// (the chain never accumulates), rows with `input < cache_read` classify
/// NoCacheWrites (a whole-prompt reporter's input contains its cached prefix).
/// Same row count so only the row structure decides.
#[test]
fn ladder_pins_whole_prompt_vs_anthropic() {
    let sb = HomeSandbox::new();
    let claude_dir = make_claude_dir(&sb);
    let proj = claude_dir.join("projects/p1");
    std::fs::create_dir_all(&proj).expect("mkdir");

    // 8 rows; per-row input = cr + 500 keeps the in-row subtraction safe.
    // below: sum(cr)=440_000, sum(in)=444_428 → 0.99 < 2 → whole-prompt under the old
    // ratio classifier; cr stays flat while input grows, so the accumulation
    // identity never holds.
    let mut below = String::new();
    for i in 0..8 {
        let cr = 55_000;
        below.push_str(&jsonl_line(
            &format!("2026-06-11T0{i}:00:00+00:00"),
            "m",
            cr + 500 + i,
            1,
            cr,
            0,
        ));
        below.push('\n');
    }
    let p_below = proj.join("below.jsonl");
    std::fs::write(&p_below, below).expect("write");
    let recs = super::parse_file(&p_below);
    assert_eq!(
        recs.iter()
            .filter(|r| r.has_usage)
            .map(|r| r.shape)
            .max_by_key(|s| super::shape_rank(*s))
            .unwrap(),
        UsageShape::WholePromptInput
    );

    // above: sum(cr)=1_600_000, sum(in)=80_428 → ratio ≥ 2 → write-missing. Per-row
    // input stays below its cache_read, the healthy-tail inversion it shows.
    let mut above = String::new();
    for i in 0..8 {
        let cr = 200_000;
        above.push_str(&jsonl_line(
            &format!("2026-06-12T0{i}:00:00+00:00"),
            "m",
            10_000 + i,
            1,
            cr,
            0,
        ));
        above.push('\n');
    }
    let p_above = proj.join("above.jsonl");
    std::fs::write(&p_above, above).expect("write");
    let recs = super::parse_file(&p_above);
    assert_eq!(
        recs.iter()
            .filter(|r| r.has_usage)
            .map(|r| r.shape)
            .max_by_key(|s| super::shape_rank(*s))
            .unwrap(),
        UsageShape::NoCacheWrites
    );
}

/// C: the gpt-5.6-sol accumulator family, trimmed real usage bytes from
/// `27e9bbc1…/subagents/agent-a91f441e8262888f9.jsonl` — small Anthropic
/// tails under a growing `cache_read`, interleaved with huge field-absent
/// rows that drag the file-wide read sum low. Every read-bearing row dips
/// below its `cache_read`, so the evidence dominates and the input stays
/// unreduced inside the sum zone the old ratio classifier subtracted in.
#[test]
fn c_fixture_low_ratio_rows_below_cache_read_stay_anthropic() {
    let path = shape_fixture("tokens-shape-c.jsonl");
    let (raw_in, _, raw_cr, raw_cc) = raw_sums(&path);
    assert!(raw_cr < 2 * raw_in, "fixture sits in the old subtract zone");
    let recs = super::parse_file(&path);
    let usage: Vec<&super::LineRec> = recs.iter().filter(|r| r.has_usage).collect();
    assert!(
        usage.iter().any(|r| r.input < r.cache_read),
        "fixture carries rows only an Anthropic-shaped reporter emits"
    );
    assert_eq!(
        usage
            .iter()
            .map(|r| r.shape)
            .max_by_key(|s| super::shape_rank(*s))
            .unwrap(),
        UsageShape::NoCacheWrites
    );
    let input: u64 = usage.iter().map(|r| r.input).sum();
    assert_eq!(
        input, raw_in,
        "input stays the uncached tail the reporter sent"
    );
    assert_eq!(raw_cc, 0);
}

/// D: an Anthropic-shaped reporter whose every turn's tail exceeds its
/// cached prefix — no row dips below `cache_read`, file-wide ratio under any
/// whole-prompt reading. No corpus instance of this shape has been observed
/// (2026-09-10, full-corpus sweep); the test pins the accumulation-identity
/// safety net that classifies it anyway.
#[test]
fn d_inline_tails_above_cache_chain_accumulates() {
    let sb = HomeSandbox::new();
    let claude_dir = make_claude_dir(&sb);
    let proj = claude_dir.join("projects/p1");
    std::fs::create_dir_all(&proj).expect("mkdir");
    let mut s = String::new();
    // cr(t+1) = cr(t) + in(t) exactly, with in(t+1) > cr(t+1) every turn.
    let rows: [(u64, u64); 12] = [
        (10_000, 0),
        (15_000, 10_000),
        (25_500, 25_000),
        (51_000, 50_500),
        (102_000, 101_500),
        (204_000, 203_500),
        (408_000, 407_500),
        (816_000, 815_500),
        (1_632_000, 1_631_500),
        (3_264_000, 3_263_500),
        (6_528_000, 6_527_500),
        (13_056_000, 13_055_500),
    ];
    for (i, (inp, cr)) in rows.iter().enumerate() {
        s.push_str(&jsonl_line(
            &format!("2026-06-11T{:02}:00:00+00:00", i),
            "glm-5.3",
            *inp,
            1,
            *cr,
            0,
        ));
        s.push('\n');
    }
    let p = proj.join("d.jsonl");
    std::fs::write(&p, s).expect("write");
    let recs = super::parse_file(&p);
    let usage: Vec<&super::LineRec> = recs.iter().filter(|r| r.has_usage).collect();
    assert_eq!(usage.len(), 12);
    let raw_in: u64 = usage.iter().map(|r| r.input).sum();
    let raw_cr: u64 = usage.iter().map(|r| r.cache_read).sum();
    assert!(raw_cr < 2 * raw_in, "fixture sits in the old subtract zone");
    assert!(
        usage.iter().all(|r| r.input >= r.cache_read),
        "fixture never dips a row below its cache_read"
    );
    assert_eq!(
        usage
            .iter()
            .map(|r| r.shape)
            .max_by_key(|s| super::shape_rank(*s))
            .unwrap(),
        UsageShape::NoCacheWrites
    );
    assert_eq!(
        usage.iter().map(|r| r.input).sum::<u64>(),
        rows.iter().map(|r| r.0).sum::<u64>(),
        "input stays the uncached tail the reporter sent"
    );
}

/// The dip fraction gates at half the read-bearing rows, inclusively: a
/// mixed file whose anthropic prefix is a minority (endpoint swap
/// mid-session) keeps its whole-prompt majority classification, while the
/// same file one dip richer classifies NoCacheWrites.
#[test]
fn dip_fraction_boundary_is_inclusive_half() {
    let sb = HomeSandbox::new();
    let claude_dir = make_claude_dir(&sb);
    let proj = claude_dir.join("projects/p1");
    std::fs::create_dir_all(&proj).expect("mkdir");
    // Six read-bearing rows: the first three dip below their cache_read,
    // the last three sit above it; two field-absent rows pad past the floor.
    // cache_read never accumulates, so the identity cannot rescue the file.
    let read_rows: [(u64, u64); 6] = [
        (100_000, 1_000_000),
        (200_000, 1_000_000),
        (300_000, 1_000_000),
        (900_000, 500_000),
        (800_000, 500_000),
        (700_000, 500_000),
    ];
    for (name, third_row, expect) in [
        ("half", (300_000, 1_000_000), UsageShape::NoCacheWrites),
        (
            "under",
            (1_300_000, 1_000_000),
            UsageShape::WholePromptInput,
        ),
    ] {
        let mut rows = read_rows;
        rows[2] = third_row;
        let mut s = String::new();
        for (i, (inp, cr)) in rows.iter().enumerate() {
            s.push_str(&jsonl_line(
                &format!("2026-06-11T{:02}:30:00+00:00", i),
                "glm-5.3",
                *inp,
                1,
                *cr,
                0,
            ));
            s.push('\n');
        }
        for i in 0..2 {
            s.push_str(&jsonl_line(
                &format!("2026-06-12T{:02}:00:00+00:00", i),
                "glm-5.3",
                400_000 + i,
                0,
                0,
                0,
            ));
            s.push('\n');
        }
        let p = proj.join(format!("{name}.jsonl"));
        std::fs::write(&p, s).expect("write");
        let recs = super::parse_file(&p);
        assert_eq!(
            recs.iter()
                .filter(|r| r.has_usage)
                .map(|r| r.shape)
                .max_by_key(|s| super::shape_rank(*s))
                .unwrap(),
            expect,
            "{name}: three-of-six dipping rows must sit exactly on the boundary"
        );
    }
}

/// E: a fully-cached whole-prompt reporter emits `input == cache_read`
/// exactly, per row — the correction zeroes input and keeps the read.
#[test]
fn e_fixture_fully_cached_whole_prompt_zeroes_input() {
    let path = shape_fixture("tokens-shape-e.jsonl");
    let (raw_in, raw_out, raw_cr, raw_cc) = raw_sums(&path);
    assert!(raw_in == raw_cr, "fixture is fully cached by construction");
    let (input, output, cr, cc, shape, _) = fixture_totals(&path);
    assert_eq!(shape, UsageShape::WholePromptInput);
    assert_eq!(input, 0, "the whole prompt rides cache_read alone");
    assert_eq!((output, cr, cc), (raw_out, raw_cr, raw_cc));
}

/// The exact `input == cache_read` signature decides even below the row
/// floor: it is per-row arithmetic, not statistical evidence.
#[test]
fn fully_cached_signature_decides_below_row_floor() {
    let sb = HomeSandbox::new();
    let claude_dir = make_claude_dir(&sb);
    let proj = claude_dir.join("projects/p1");
    std::fs::create_dir_all(&proj).expect("mkdir");
    let mut s = String::new();
    for i in 0..4 {
        s.push_str(&jsonl_line(
            &format!("2026-06-11T0{i}:00:00+00:00"),
            "z-ai/glm-5.3-free",
            1920 + i,
            1,
            1920 + i,
            0,
        ));
        s.push('\n');
    }
    let p = proj.join("eq.jsonl");
    std::fs::write(&p, s).expect("write");
    let recs = super::parse_file(&p);
    let usage: Vec<&super::LineRec> = recs.iter().filter(|r| r.has_usage).collect();
    assert_eq!(usage.len(), 4);
    assert!(
        usage
            .iter()
            .all(|r| r.shape == UsageShape::WholePromptInput)
    );
    assert!(usage.iter().all(|r| r.input == 0));
    let cr_sum: u64 = usage.iter().map(|r| r.cache_read).sum();
    assert_eq!(cr_sum, 1920 + 1921 + 1922 + 1923);
}

/// The streamed-turn collapse keeps first-occurrence order: the shape
/// ladder's chain arm reads consecutive turns, and a dedup map's iteration
/// order is not an order.
#[test]
fn collapse_keeps_first_occurrence_order() {
    let sb = HomeSandbox::new();
    let claude_dir = make_claude_dir(&sb);
    let proj = claude_dir.join("projects/p1");
    std::fs::create_dir_all(&proj).expect("mkdir");
    let mut s = String::new();
    // Three distinct ids in file order; the second emits a larger delta
    // afterwards, which must replace it in place rather than move it.
    s.push_str(&jsonl_line_with_ids(
        "2026-06-11T01:00:00+00:00",
        "r1",
        "m1",
        "m",
        100,
        1,
    ));
    s.push('\n');
    s.push_str(&jsonl_line_with_ids(
        "2026-06-11T02:00:00+00:00",
        "r2",
        "m2",
        "m",
        200,
        1,
    ));
    s.push('\n');
    s.push_str(&jsonl_line_with_ids(
        "2026-06-11T03:00:00+00:00",
        "r3",
        "m3",
        "m",
        300,
        1,
    ));
    s.push('\n');
    s.push_str(&jsonl_line_with_ids(
        "2026-06-11T02:00:00+00:00",
        "r2",
        "m2",
        "m",
        200,
        500,
    ));
    s.push('\n');
    let p = proj.join("order.jsonl");
    std::fs::write(&p, s).expect("write");
    let recs = super::parse_file(&p);
    let usage: Vec<&super::LineRec> = recs.iter().filter(|r| r.has_usage).collect();
    assert_eq!(usage.len(), 3);
    assert_eq!(
        usage.iter().map(|r| r.input).collect::<Vec<_>>(),
        vec![100, 200, 300],
        "ids stay in file order across the in-place replacement"
    );
    assert_eq!(usage[1].output, 500, "the larger delta won its slot");
}

/// A minimal usage row for the shape-helper unit tests.
fn lrec(input: u64, cache_read: u64) -> super::LineRec {
    super::LineRec {
        date: "2026-06-11".to_owned(),
        hour: 0,
        uuid: String::new(),
        session: String::new(),
        is_message: true,
        has_usage: true,
        tok_key: String::new(),
        model: "m".to_owned(),
        input,
        output: 1,
        cache_read,
        cache_create: 0,
        shape: UsageShape::Healthy,
        fast: false,
        tool_calls: 0,
        advisor: false,
    }
}

/// The accumulation identity's absolute slack floor: block-quantized
/// counters (multiples of 64) on small early pairs deviate past the relative
/// bound on rounding alone, and the floor keeps those pairs passing.
#[test]
fn identity_slack_floor_covers_small_early_pairs() {
    // Every pair deviates by exactly 128 tokens; the first pairs' expected
    // values sit under 2_560, where 5% is less than 128, so only the floor
    // passes them.
    let mut rows = vec![lrec(100, 1_000)];
    for _ in 0..7 {
        let prev = rows.last().expect("non-empty");
        rows.push(lrec(200, prev.cache_read + prev.input + 128));
    }
    let idxs: Vec<usize> = (0..rows.len()).collect();
    assert!(super::majority_pairs_cache_read_accumulates(
        &rows,
        &idxs[..]
    ));
}

/// A pair whose earlier row reports no read metric is not counted: the
/// identity degenerates to `cache_read(t+1) ≈ input(t)`, which a whole-prompt
/// reporter that also caches the prior response satisfies within its output.
#[test]
fn identity_skips_pairs_after_a_read_absent_row() {
    // One exact pair after a read-absent row: the majority is 1/1 with the
    // skip, and the read-absent pair's failure would make it 1/2 without.
    let rows = [
        lrec(100_000, 0),
        lrec(50_000, 50_000),
        lrec(60_000, 100_000),
    ];
    let idxs = [0usize, 1, 2];
    assert!(super::majority_pairs_cache_read_accumulates(&rows, &idxs));
}

/// Fewer than 8 usage rows classifies Healthy even with whole-prompt-shaped numbers:
/// never correct on thin evidence.
#[test]
fn thin_evidence_classifies_healthy() {
    let sb = HomeSandbox::new();
    let claude_dir = make_claude_dir(&sb);
    let proj = claude_dir.join("projects/p1");
    std::fs::create_dir_all(&proj).expect("mkdir");
    let mut s = String::new();
    for i in 0..7 {
        // Ratio ~0.5 (whole-prompt-shaped) but only 7 rows; distinct input per row so
        // the id-less composite dedup key keeps all 7.
        s.push_str(&jsonl_line(
            &format!("2026-06-11T0{i}:00:00+00:00"),
            "m",
            100 + i,
            1,
            50,
            0,
        ));
        s.push('\n');
    }
    let p = proj.join("thin.jsonl");
    std::fs::write(&p, s).expect("write");
    let recs = super::parse_file(&p);
    let usage: Vec<&super::LineRec> = recs.iter().filter(|r| r.has_usage).collect();
    assert_eq!(usage.len(), 7);
    assert_eq!(usage[0].shape, UsageShape::Healthy);
    // Nothing corrected: every row's input sits past its cache_read (50)
    // exactly as written.
    assert!(
        usage.iter().all(|r| r.input > r.cache_read),
        "thin evidence: nothing corrected"
    );
}

/// A model can be whole-prompt in one file and healthy in another (official z.ai vs
/// tokenrouter, same model id): per-(file, model) classification keeps them
/// separate, and an aggregate merging both carries the strongest shape.
#[test]
fn same_model_two_files_two_shapes() {
    let sb = HomeSandbox::new();
    let claude_dir = make_claude_dir(&sb);
    let proj = claude_dir.join("projects/p1");
    std::fs::create_dir_all(&proj).expect("mkdir");

    let mut a1 = String::new();
    for i in 0..10 {
        a1.push_str(&jsonl_line(
            &format!("2026-06-11T0{i}:00:00+00:00"),
            "glm-5.3",
            100_000 + i,
            1,
            50_000,
            0,
        ));
        a1.push('\n');
    }
    std::fs::write(proj.join("a1.jsonl"), a1).expect("write");

    let mut healthy = String::new();
    for i in 0..10 {
        healthy.push_str(&jsonl_line(
            &format!("2026-06-12T0{i}:00:00+00:00"),
            "glm-5.3",
            100_000,
            1,
            50_000,
            5,
        ));
        healthy.push('\n');
    }
    std::fs::write(proj.join("healthy.jsonl"), healthy).expect("write");

    let a1_recs = super::parse_file(&proj.join("a1.jsonl"));
    assert_eq!(
        a1_recs
            .iter()
            .filter(|r| r.has_usage)
            .map(|r| r.shape)
            .max_by_key(|s| super::shape_rank(*s))
            .unwrap(),
        UsageShape::WholePromptInput
    );
    let h_recs = super::parse_file(&proj.join("healthy.jsonl"));
    assert_eq!(
        h_recs
            .iter()
            .filter(|r| r.has_usage)
            .map(|r| r.shape)
            .max_by_key(|s| super::shape_rank(*s))
            .unwrap(),
        UsageShape::Healthy
    );
    assert_eq!(
        UsageShape::Healthy.merge(UsageShape::WholePromptInput),
        UsageShape::WholePromptInput,
        "aggregate carries the strongest shape"
    );
}

// ── shape re-derive (older-classifier ledger days) ──────────────────────────

/// Write a pre-classifier ledger row: no `shape` key on the wire, so it
/// loads `Healthy` and owes the re-derive pass.
fn write_v0_ledger_day(
    clauth_dir: &std::path::Path,
    recorded_through: &str,
    day: &str,
    model: &str,
    values: (u64, u64, u64, u64),
) {
    let (input, output, cache_read, cache_create) = values;
    std::fs::create_dir_all(clauth_dir).expect("mkdir");
    let json = r#"{"recorded_through":"RT","days":{"D":{"M":{"input":I,"output":O,"cache_read":C,"cache_create":K}}}}"#
        .replace("RT", recorded_through)
        .replace("D", day)
        .replace("M", model)
        .replace("I", &input.to_string())
        .replace("O", &output.to_string())
        .replace("C", &cache_read.to_string())
        .replace("K", &cache_create.to_string());
    std::fs::write(clauth_dir.join("token_ledger.json"), json).expect("write ledger");
}

/// Write a ledger row carrying an explicit `shape` — a day recorded under the
/// v1 ratio classifier.
fn write_v1_ledger_day(
    clauth_dir: &std::path::Path,
    recorded_through: &str,
    day: &str,
    model: &str,
    values: (u64, u64, u64, u64),
    shape: &str,
) {
    let (input, output, cache_read, cache_create) = values;
    std::fs::create_dir_all(clauth_dir).expect("mkdir");
    let json = r#"{"recorded_through":"RT","days":{"D":{"M":{"input":I,"output":O,"cache_read":C,"cache_create":K,"shape":"S"}}}}"#
        .replace("RT", recorded_through)
        .replace("D", day)
        .replace("M", model)
        .replace("I", &input.to_string())
        .replace("O", &output.to_string())
        .replace("C", &cache_read.to_string())
        .replace("K", &cache_create.to_string())
        .replace("S", shape);
    std::fs::write(clauth_dir.join("token_ledger.json"), json).expect("write ledger");
}

/// A stored pre-classifier day whose corpus re-derives as whole-prompt with exactly
/// `stored_input - stored_cache_read` gets the corrected values, hours, and
/// the WholePromptInput marker.
#[test]
fn rederive_corrects_a1_day_from_corpus() {
    let sb = HomeSandbox::new();
    let claude_dir = make_claude_dir(&sb);
    let clauth_dir = sb.home().join(".clauth");
    // Stored (raw, poisoned) row: input contains the cached prefix.
    write_v0_ledger_day(
        &clauth_dir,
        "2026-06-16",
        "2026-06-15",
        "glm-5.3",
        (1_000_000, 50, 400_000, 0),
    );

    // Corpus: 8 whole-prompt-shaped rows (input holds cache_read; distinct ids), summed
    // input=1_000_000, cr=400_000, output=50 — exactly the stored row. Row
    // magnitudes sit above the identity's absolute slack floor, so flat
    // cache_read under growing input never reads as accumulation.
    let proj = claude_dir.join("projects/p1");
    std::fs::create_dir_all(&proj).expect("mkdir");
    let mut s = String::new();
    // sum: in=1_000_000, out=48, cr=400_000 — exactly the stored raw row.
    let rows = [
        (125_000u64, 6u64, 50_000u64),
        (125_000, 6, 50_000),
        (125_000, 6, 50_000),
        (125_000, 6, 50_000),
        (125_000, 7, 50_000),
        (125_000, 7, 50_000),
        (125_000, 6, 50_000),
        (125_000, 6, 50_000),
    ];
    for (i, (inp, out, cr)) in rows.iter().enumerate() {
        s.push_str(
            r#"{"timestamp":"2026-06-15T0I:00:00+00:00","message":{"id":"msg_I","model":"glm-5.3","role":"assistant","usage":{"input_tokens":IP,"output_tokens":OP,"cache_read_input_tokens":CR,"cache_creation_input_tokens":0}}}"#
                .replace("IP", &inp.to_string())
                .replace("OP", &out.to_string())
                .replace("CR", &cr.to_string())
                .replace("I", &i.to_string()).as_str(),
        );
        s.push('\n');
    }
    std::fs::write(proj.join("sess.jsonl"), s).expect("write");
    set_mtime(
        &proj.join("sess.jsonl"),
        epoch_day("2026-06-15") + Duration::from_secs(60),
    );

    let mut ledger = crate::token_ledger::Ledger::load(&clauth_dir);
    let today = "2026-06-20";
    let mut ran = false;
    let mut progress = |d: usize, t: usize| {
        let _ = (d, t);
    };
    // drive the worker's leg directly
    if super::run_rederive(&claude_dir, &mut ledger, today, &mut progress) {
        ledger.save(&clauth_dir);
        ran = true;
    }
    assert!(ran, "the re-derive pass ran");
    assert_eq!(ledger.shape_clf(), crate::token_ledger::SHAPE_CLF_VERSION);

    let (input, output, cr, cc, shape, has_hours) = ledger
        .wire_model_fields("2026-06-15", "glm-5.3")
        .expect("day row present");
    assert_eq!(
        input, 600_000,
        "corrected: stored 1_000_000 minus cr 400_000"
    );
    assert_eq!(cr, 400_000);
    assert_eq!(output, 50);
    assert_eq!(cc, 0);
    assert_eq!(shape, crate::tokens::UsageShape::WholePromptInput);
    assert!(has_hours, "corrected row gains hourly buckets");
}

/// A stored day whose corpus re-derives equal on all four fields (a healthy
/// day) is left byte-identical and only gains the re-derived shape marker.
#[test]
fn rederive_leaves_equal_day_untouched() {
    let sb = HomeSandbox::new();
    let claude_dir = make_claude_dir(&sb);
    let clauth_dir = sb.home().join(".clauth");
    write_v0_ledger_day(
        &clauth_dir,
        "2026-06-16",
        "2026-06-15",
        "claude-opus-4",
        (100, 50, 20, 10),
    );

    let proj = claude_dir.join("projects/p1");
    std::fs::create_dir_all(&proj).expect("mkdir");
    let mut s = String::new();
    let rows = [
        (12u64, 6u64, 2u64, 1u64),
        (12, 6, 2, 1),
        (12, 6, 2, 1),
        (12, 6, 2, 1),
        (12, 6, 2, 1),
        (12, 6, 2, 1),
        (12, 6, 2, 1),
        (16, 8, 6, 3),
    ];
    // sum: in=100, out=50, cr=20, cc=10 ✓ (any_cache_create>0 → Healthy)
    for (i, (inp, out, cr, cc)) in rows.iter().enumerate() {
        s.push_str(
            r#"{"timestamp":"2026-06-15T0I:00:00+00:00","message":{"id":"msg_I","model":"claude-opus-4","role":"assistant","usage":{"input_tokens":IP,"output_tokens":OP,"cache_read_input_tokens":CR,"cache_creation_input_tokens":CC}}}"#
                .replace("IP", &inp.to_string())
                .replace("OP", &out.to_string())
                .replace("CR", &cr.to_string())
                .replace("CC", &cc.to_string())
                .replace("I", &i.to_string()).as_str(),
        );
        s.push('\n');
    }
    std::fs::write(proj.join("sess.jsonl"), s).expect("write");
    set_mtime(
        &proj.join("sess.jsonl"),
        epoch_day("2026-06-15") + Duration::from_secs(60),
    );

    let mut ledger = crate::token_ledger::Ledger::load(&clauth_dir);
    let mut progress = |_: usize, _: usize| {};
    assert!(super::run_rederive(
        &claude_dir,
        &mut ledger,
        "2026-06-20",
        &mut progress
    ));
    let (input, output, cr, cc, shape, _) = ledger
        .wire_model_fields("2026-06-15", "claude-opus-4")
        .expect("row");
    assert_eq!((input, output, cr, cc), (100, 50, 20, 10));
    assert_eq!(shape, crate::tokens::UsageShape::Healthy);
}

/// A day the corpus no longer covers (pruned) keeps its recorded values and
/// the never-correcting Healthy marker, and the pass still marks done.
#[test]
fn rederive_marks_pruned_day_unverifiable() {
    let sb = HomeSandbox::new();
    let claude_dir = make_claude_dir(&sb);
    let clauth_dir = sb.home().join(".clauth");
    write_v0_ledger_day(
        &clauth_dir,
        "2026-06-16",
        "2026-06-15",
        "glm-5.3",
        (1_000, 50, 400, 0),
    );
    // No projects dir at all: nothing covers the day.

    let mut ledger = crate::token_ledger::Ledger::load(&clauth_dir);
    let mut progress = |_: usize, _: usize| {};
    assert!(super::run_rederive(
        &claude_dir,
        &mut ledger,
        "2026-06-20",
        &mut progress
    ));
    assert_eq!(ledger.shape_clf(), crate::token_ledger::SHAPE_CLF_VERSION);
    let (input, _, _, _, shape, _) = ledger
        .wire_model_fields("2026-06-15", "glm-5.3")
        .expect("row");
    assert_eq!(input, 1_000, "unverifiable: values kept");
    assert_eq!(shape, crate::tokens::UsageShape::Healthy);
}

/// A stored v1-classifier day the ratio arm over-subtracted — recorded with
/// `input = true_input - cache_read` and the WholePromptInput marker — whose
/// corpus re-derives as a never-correcting shape gets its input back, the
/// re-derived shape, and hourly buckets.
#[test]
fn rederive_unpoisons_over_subtracted_day() {
    let sb = HomeSandbox::new();
    let claude_dir = make_claude_dir(&sb);
    let clauth_dir = sb.home().join(".clauth");
    // 7 rows (in=125, cr=50) + 1 row (in=50, cr=400): sum in=925, out=50,
    // cr=750. The last row's in < cr is Anthropic-shaped evidence, so the
    // corpus classifies NoCacheWrites and keeps input unreduced.
    write_v1_ledger_day(
        &clauth_dir,
        "2026-06-16",
        "2026-06-15",
        "gpt-5.6-sol",
        (175, 50, 750, 0),
        "whole_prompt_input",
    );

    let proj = claude_dir.join("projects/p1");
    std::fs::create_dir_all(&proj).expect("mkdir");
    let mut s = String::new();
    for i in 0..7 {
        s.push_str(
            r#"{"timestamp":"2026-06-15T0I:00:00+00:00","message":{"id":"msg_I","model":"gpt-5.6-sol","role":"assistant","usage":{"input_tokens":125,"output_tokens":6,"cache_read_input_tokens":50,"cache_creation_input_tokens":0}}}"#
                .replace("I", &i.to_string())
                .as_str(),
        );
        s.push('\n');
    }
    s.push_str(
        r#"{"timestamp":"2026-06-15T07:00:00+00:00","message":{"id":"msg_7","model":"gpt-5.6-sol","role":"assistant","usage":{"input_tokens":50,"output_tokens":8,"cache_read_input_tokens":400,"cache_creation_input_tokens":0}}}"#,
    );
    s.push('\n');
    std::fs::write(proj.join("sess.jsonl"), s).expect("write");
    set_mtime(
        &proj.join("sess.jsonl"),
        epoch_day("2026-06-15") + Duration::from_secs(60),
    );

    let mut ledger = crate::token_ledger::Ledger::load(&clauth_dir);
    let mut progress = |_: usize, _: usize| {};
    assert!(super::run_rederive(
        &claude_dir,
        &mut ledger,
        "2026-06-20",
        &mut progress
    ));
    assert_eq!(ledger.shape_clf(), crate::token_ledger::SHAPE_CLF_VERSION);
    let (input, output, cr, cc, shape, has_hours) = ledger
        .wire_model_fields("2026-06-15", "gpt-5.6-sol")
        .expect("day row present");
    assert_eq!(input, 925, "the v1 subtraction is undone");
    assert_eq!((output, cr, cc), (50, 750, 0));
    assert_eq!(shape, crate::tokens::UsageShape::NoCacheWrites);
    assert!(has_hours, "un-poisoned row gains hourly buckets");
}

/// A stored pre-classifier day whose correction delta is MIXED — one file's
/// whole-prompt subtraction, not the day's whole cache_read — matches no
/// per-direction arithmetic (stored − cr and stored + cr both miss), so only
/// the coverage gate can adopt it.
#[test]
fn rederive_adopts_mixed_day_beyond_exact_arithmetic() {
    let sb = HomeSandbox::new();
    let claude_dir = make_claude_dir(&sb);
    let clauth_dir = sb.home().join(".clauth");
    // Stored raw row: in=1_199_999, out=48, cr=499_995 (the corpus sums).
    write_v0_ledger_day(
        &clauth_dir,
        "2026-06-16",
        "2026-06-15",
        "glm-5.3",
        (1_199_999, 48, 499_995, 0),
    );

    let proj = claude_dir.join("projects/p1");
    std::fs::create_dir_all(&proj).expect("mkdir");
    let mut s = String::new();
    // Whole-prompt file: flat cache_read under constant input, no dips, no
    // accumulation — classifies WholePromptInput and contributes
    // in - cr = 400_000 of its 800_000 raw input.
    for i in 0..8 {
        s.push_str(
            r#"{"timestamp":"2026-06-15T0I:00:00+00:00","message":{"id":"wI","model":"glm-5.3","role":"assistant","usage":{"input_tokens":100000,"output_tokens":3,"cache_read_input_tokens":50000,"cache_creation_input_tokens":0}}}"#
                .replace("I", &i.to_string())
                .as_str(),
        );
        s.push('\n');
    }
    std::fs::write(proj.join("sess-w.jsonl"), s).expect("write");
    set_mtime(
        &proj.join("sess-w.jsonl"),
        epoch_day("2026-06-15") + Duration::from_secs(60),
    );
    // The anthropic rows sit in their own file: classification is per
    // (file, model), and one 16-row group would merge the two shapes.
    let mut a = String::new();
    a.push_str(
        r#"{"timestamp":"2026-06-15T08:00:00+00:00","message":{"id":"a0","model":"glm-5.3","role":"assistant","usage":{"input_tokens":390003,"output_tokens":3,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}}"#,
    );
    a.push('\n');
    for i in 1..8 {
        a.push_str(
            r#"{"timestamp":"2026-06-15T0I:30:00+00:00","message":{"id":"aI","model":"glm-5.3","role":"assistant","usage":{"input_tokens":1428,"output_tokens":3,"cache_read_input_tokens":14285,"cache_creation_input_tokens":0}}}"#
                .replace("I", &i.to_string())
                .as_str(),
        );
        a.push('\n');
    }
    std::fs::write(proj.join("sess-a.jsonl"), a).expect("write");
    set_mtime(
        &proj.join("sess-a.jsonl"),
        epoch_day("2026-06-15") + Duration::from_secs(90),
    );

    let mut ledger = crate::token_ledger::Ledger::load(&clauth_dir);
    let mut progress = |_: usize, _: usize| {};
    assert!(super::run_rederive(
        &claude_dir,
        &mut ledger,
        "2026-06-20",
        &mut progress
    ));
    assert_eq!(ledger.shape_clf(), crate::token_ledger::SHAPE_CLF_VERSION);
    let (input, output, cr, cc, shape, has_hours) = ledger
        .wire_model_fields("2026-06-15", "glm-5.3")
        .expect("day row present");
    // The delta is 400_000 (the whole-prompt file's cache_read), not the
    // day's 499_995 — stored - cr and stored + cr both miss it.
    assert_eq!(input, 799_999, "the mixed correction is adopted");
    assert_eq!((output, cr, cc), (48, 499_995, 0));
    assert_eq!(shape, crate::tokens::UsageShape::WholePromptInput);
    assert!(has_hours, "adopted row gains hourly buckets");
}

/// A v1 file whose `rederive_done` flag read true still owes the versioned
/// pass: the dropped key is ignored on load and the version reads 0.
#[test]
fn rederive_v1_flag_file_owes_versioned_pass() {
    let sb = HomeSandbox::new();
    let clauth_dir = sb.home().join(".clauth");
    std::fs::create_dir_all(&clauth_dir).expect("mkdir");
    std::fs::write(
        clauth_dir.join("token_ledger.json"),
        r#"{"recorded_through":"2026-06-15","days":{"2026-06-14":{"glm-5.3":{"input":100,"output":5,"cache_read":40,"cache_create":0,"shape":"no_cache_writes"}}},"rederive_done":true}"#,
    )
    .expect("write ledger");
    let ledger = crate::token_ledger::Ledger::load(&clauth_dir);
    assert_eq!(
        ledger.rederive_through("2026-06-20").as_deref(),
        Some("2026-06-15"),
        "the v1 flag no longer short-circuits the pass"
    );
    let (input, _, cr, _, shape, _) = ledger
        .wire_model_fields("2026-06-14", "glm-5.3")
        .expect("day row present");
    assert_eq!((input, cr), (100, 40));
    assert_eq!(shape, crate::tokens::UsageShape::NoCacheWrites);
}

/// The pass runs at most once: after the version stamp, `rederive_through`
/// returns None even when uncorrected rows remain.
#[test]
fn rederive_runs_once() {
    let sb = HomeSandbox::new();
    let claude_dir = make_claude_dir(&sb);
    let clauth_dir = sb.home().join(".clauth");
    write_v0_ledger_day(
        &clauth_dir,
        "2026-06-16",
        "2026-06-15",
        "glm-5.3",
        (1_000, 50, 400, 0),
    );
    let mut ledger = crate::token_ledger::Ledger::load(&clauth_dir);
    let mut progress = |_: usize, _: usize| {};
    assert!(super::run_rederive(
        &claude_dir,
        &mut ledger,
        "2026-06-20",
        &mut progress
    ));
    let mut progress2 = |_: usize, _: usize| {};
    assert!(
        !super::run_rederive(&claude_dir, &mut ledger, "2026-06-21", &mut progress2),
        "second run is a no-op"
    );
}

// ── served-model re-derive (days recorded under the requested alias) ─────────

/// Days recorded before the served-model map counted DeepSeek-served lines
/// under the requested `claude-*` alias. The one-shot pass re-keys a day the
/// corpus still fully covers onto the served model, whatever file holds its
/// lines (one here was touched after the watermark day, as a live session's
/// transcript is), and leaves a day the corpus no longer covers as recorded.
#[test]
fn persist_ledger_rekeys_covered_days_onto_the_served_model() {
    let sb = HomeSandbox::new();
    let claude_dir = make_claude_dir(&sb);
    let clauth_dir = sb.home().join(".clauth");
    std::fs::create_dir_all(&clauth_dir).expect("mkdir");
    std::fs::write(
        clauth_dir.join("token_ledger.json"),
        r#"{
            "recorded_through": "2026-06-16",
            "backfill_done": true,
            "shape_clf": 3,
            "days": {
                "2026-06-14": {
                    "claude-opus-5-5": {"input": 50, "output": 20, "cache_read": 0, "cache_create": 0}
                },
                "2026-06-15": {
                    "claude-opus-5-5": {"input": 107, "output": 53, "cache_read": 0, "cache_create": 0}
                }
            }
        }"#,
    )
    .expect("write ledger");

    let proj = claude_dir.join("projects").join("p1");
    std::fs::create_dir_all(&proj).expect("mkdir");
    let anthropic = proj.join("a.jsonl");
    std::fs::write(
        &anthropic,
        jsonl_line_with_ids(
            "2026-06-15T10:00:00+00:00",
            "r1",
            "msg_01AbCdEfGhIjKlMnOpQrStUv",
            "claude-opus-5-5",
            100,
            50,
        ),
    )
    .expect("write");
    set_mtime(
        &anthropic,
        epoch_day("2026-06-15") + Duration::from_secs(11 * 3600),
    );
    let live = proj.join("b.jsonl");
    std::fs::write(
        &live,
        [
            jsonl_line_with_ids(
                "2026-06-15T11:00:00+00:00",
                "r2",
                "0a1b2c3d-4e5f-4a6b-8c7d-9e0f1a2b3c4d",
                "claude-opus-5-5",
                7,
                3,
            ),
            // The 06-14 day lost its transcript: only part of it survives.
            jsonl_line_with_ids(
                "2026-06-14T09:00:00+00:00",
                "r3",
                "1a1b2c3d-4e5f-4a6b-8c7d-9e0f1a2b3c4d",
                "claude-opus-5-5",
                30,
                12,
            ),
        ]
        .join("\n"),
    )
    .expect("write");
    set_mtime(&live, SystemTime::now());

    let mut ledger = crate::token_ledger::Ledger::load(&clauth_dir);
    persist_ledger(
        &claude_dir,
        &mut ledger,
        &clauth_dir,
        &TokenStats::default(),
        "2026-06-20",
        |_, _| {},
    );

    let ledger = crate::token_ledger::Ledger::load(&clauth_dir);
    assert_eq!(
        ledger.wire_model_fields("2026-06-15", "claude-opus-5-5"),
        Some((100, 50, 0, 0, UsageShape::Healthy, true))
    );
    assert_eq!(
        ledger.wire_model_fields("2026-06-15", "deepseek-v4-pro"),
        Some((7, 3, 0, 0, UsageShape::Healthy, true))
    );
    assert_eq!(
        ledger.wire_model_fields("2026-06-14", "claude-opus-5-5"),
        Some((50, 20, 0, 0, UsageShape::Healthy, false))
    );
    assert_eq!(
        ledger.wire_model_fields("2026-06-14", "deepseek-v4-pro"),
        None
    );
}

// ── tps: the tokens worker only reads the speed ledger ────────────────────────

/// The tokens worker never writes `token_speed.json` (the producer folds it at
/// day rollover); `persist_ledger` leaves the file untouched and the worker
/// reads `TokenStats.speed_days` from it.
#[test]
fn persist_ledger_does_not_write_the_speed_ledger() {
    let sb = crate::testutil::HomeSandbox::new();
    let clauth = sb.home().join(".clauth");
    let claude = sb.home().join(".claude");
    std::fs::create_dir_all(&clauth).unwrap();
    std::fs::create_dir_all(&claude).unwrap();

    let mut ledger = crate::token_ledger::Ledger::load(&clauth);
    let base = TokenStats::default();
    persist_ledger(
        &claude,
        &mut ledger,
        &clauth,
        &base,
        "2026-10-15",
        |_, _| {},
    );

    assert!(
        !clauth.join("token_speed.json").exists(),
        "the tokens worker never writes the speed ledger"
    );
    assert!(
        crate::token_ledger::SpeedLedger::load(&clauth)
            .days
            .is_empty(),
        "an empty speed ledger loads empty"
    );
}

// ── advisor iterations ───────────────────────────────────────────────────────

/// A real-shaped `message` iteration: the entries whose sum is the line's
/// top-level usage.
fn message_iteration(input: u64, output: u64) -> String {
    format!(
        r#"{{"type":"message","input_tokens":{input},"output_tokens":{output},"cache_read_input_tokens":0,"cache_creation_input_tokens":0,"cache_creation":{{"ephemeral_5m_input_tokens":0,"ephemeral_1h_input_tokens":0}}}}"#
    )
}

/// A server-side advisor iteration on `model`, with the same extra keys a
/// real transcript carries (ignored by the reader).
fn advisor_iteration(model: &str, input: u64, output: u64) -> String {
    format!(
        r#"{{"type":"advisor_message","model":"{model}","input_tokens":{input},"output_tokens":{output},"cache_read_input_tokens":0,"cache_creation_input_tokens":0,"cache_creation":{{"ephemeral_5m_input_tokens":0,"ephemeral_1h_input_tokens":0}}}}"#
    )
}

/// One assistant line on `claude-sonnet-4-5` whose top-level usage is `top`
/// (input, output) and whose `iterations[]` is `iterations`; `extra_usage`
/// is spliced into the usage object (`,"speed":"fast"`).
fn iterations_line(
    timestamp: &str,
    msg_id: &str,
    uuid: &str,
    top: (u64, u64),
    iterations: &[String],
    extra_usage: &str,
) -> String {
    format!(
        r#"{{"timestamp":"{timestamp}","uuid":"{uuid}","sessionId":"s1","message":{{"id":"{msg_id}","role":"assistant","model":"claude-sonnet-4-5","content":[{{"type":"tool_use"}}],"usage":{{"input_tokens":{},"output_tokens":{},"cache_read_input_tokens":0,"cache_creation_input_tokens":0{extra_usage},"iterations":[{}]}}}}}}"#,
        top.0,
        top.1,
        iterations.join(",")
    )
}

/// A stats-cache whose cutoff is 2026-06-10, so every 2026-06-11 transcript
/// line is post-cutoff.
fn cutoff_stats_cache(claude_dir: &std::path::Path) {
    write_stats_cache(
        claude_dir,
        r#"{"lastComputedDate":"2026-06-10","totalSessions":0,"totalMessages":0,"dailyActivity":[],"dailyModelTokens":[],"modelUsage":{},"hourCounts":{}}"#,
    );
}

fn write_project_file(claude_dir: &std::path::Path, name: &str, lines: &[String]) -> PathBuf {
    let project = claude_dir.join("projects/p1");
    std::fs::create_dir_all(&project).expect("create project");
    let path = project.join(name);
    std::fs::write(&path, lines.join("\n")).expect("write transcript");
    set_mtime(&path, SystemTime::now());
    path
}

fn lifetime_row<'a>(stats: &'a TokenStats, model: &str) -> Option<&'a ModelTokens> {
    stats.models.iter().find(|m| m.model == model)
}

#[test]
fn an_advisor_iteration_counts_under_its_own_model() {
    let sb = HomeSandbox::new();
    let path = sb.home().join("advisor.jsonl");
    std::fs::write(
        &path,
        iterations_line(
            "2026-06-11T10:00:00+00:00",
            "msg_x",
            "u1",
            (30, 7),
            &[
                message_iteration(10, 3),
                advisor_iteration("claude-opus-5-5", 301_544, 5_477),
                message_iteration(20, 4),
            ],
            "",
        ),
    )
    .expect("write transcript");
    let recs = parse_file(&path);
    assert_eq!(recs.len(), 2, "the line's own rec plus one advisor rec");

    let main = &recs[0];
    assert_eq!(main.tok_key, "msg_x");
    assert_eq!(main.model, "claude-sonnet-4-5");
    assert_eq!((main.input, main.output), (30, 7), "top-level is untouched");
    assert!(main.is_message && !main.advisor);

    let adv = &recs[1];
    assert_eq!(adv.tok_key, "msg_x#adv0");
    assert_eq!(adv.model, "claude-opus-5-5");
    assert_eq!(
        (adv.input, adv.output, adv.cache_read, adv.cache_create),
        (301_544, 5_477, 0, 0)
    );
    assert!(adv.has_usage && adv.advisor);
    assert!(!adv.is_message, "an advisor call is not a message");
    assert_eq!((adv.date.as_str(), adv.hour), ("2026-06-11", 10));
}

#[test]
fn an_advisor_iteration_carries_all_four_token_fields() {
    let sb = HomeSandbox::new();
    let path = sb.home().join("advisor-four.jsonl");
    std::fs::write(
        &path,
        iterations_line(
            "2026-06-11T10:00:00+00:00",
            "msg_x",
            "u1",
            (1, 1),
            &[
                r#"{"type":"advisor_message","model":"claude-opus-5-5","input_tokens":11,"output_tokens":22,"cache_read_input_tokens":33,"cache_creation_input_tokens":44}"#
                    .to_owned(),
            ],
            "",
        ),
    )
    .expect("write transcript");
    let adv = parse_file(&path)
        .into_iter()
        .find(|r| r.advisor)
        .expect("advisor rec");
    assert_eq!(
        (adv.input, adv.output, adv.cache_read, adv.cache_create),
        (11, 22, 33, 44)
    );
}

#[test]
fn only_advisor_iterations_are_read_from_the_list() {
    let sb = HomeSandbox::new();
    let path = sb.home().join("kinds.jsonl");
    std::fs::write(
        &path,
        iterations_line(
            "2026-06-11T10:00:00+00:00",
            "msg_x",
            "u1",
            (10, 3),
            &[
                message_iteration(10, 3),
                // Another server-side tool's call, and a typeless entry: neither
                // is an advisor call, so neither is counted.
                r#"{"type":"web_search","model":"claude-opus-5-5","input_tokens":999,"output_tokens":999}"#
                    .to_owned(),
                r#"{"model":"claude-opus-5-5","input_tokens":999,"output_tokens":999}"#.to_owned(),
            ],
            "",
        ),
    )
    .expect("write transcript");
    let recs = parse_file(&path);
    assert_eq!(recs.len(), 1);
    assert!(!recs[0].advisor);
}

#[test]
fn a_null_iterations_line_still_counts_as_a_message() {
    let sb = HomeSandbox::new();
    let path = sb.home().join("null-iterations.jsonl");
    // The shape of a `<synthetic>` refusal line: `"iterations":null`.
    std::fs::write(
        &path,
        r#"{"timestamp":"2026-06-11T10:00:00+00:00","uuid":"u1","sessionId":"s1","message":{"id":"msg_x","role":"assistant","model":"claude-sonnet-4-5","content":[],"usage":{"input_tokens":30,"output_tokens":7,"cache_read_input_tokens":0,"cache_creation_input_tokens":0,"iterations":null}}}"#,
    )
    .expect("write transcript");
    let recs = parse_file(&path);
    assert_eq!(recs.len(), 1, "the line survives and adds no advisor rec");
    assert!(recs[0].is_message && !recs[0].advisor);
    assert_eq!((recs[0].input, recs[0].output), (30, 7));
    assert_eq!(recs[0].session, "s1");
}

#[test]
fn an_empty_iterations_list_counts_as_a_message() {
    let sb = HomeSandbox::new();
    let path = sb.home().join("empty-iterations.jsonl");
    std::fs::write(
        &path,
        iterations_line("2026-06-11T10:00:00+00:00", "msg_x", "u1", (30, 7), &[], ""),
    )
    .expect("write transcript");
    let recs = parse_file(&path);
    assert_eq!(recs.len(), 1);
    assert!(recs[0].is_message);
    assert_eq!((recs[0].input, recs[0].output), (30, 7));
}

#[test]
fn a_null_token_field_on_an_advisor_iteration_reads_as_zero() {
    let sb = HomeSandbox::new();
    let path = sb.home().join("null-token.jsonl");
    std::fs::write(
        &path,
        iterations_line(
            "2026-06-11T10:00:00+00:00",
            "msg_x",
            "u1",
            (30, 7),
            &[
                r#"{"type":"advisor_message","model":"claude-opus-5-5","input_tokens":null,"output_tokens":9,"cache_read_input_tokens":null,"cache_creation_input_tokens":null}"#
                    .to_owned(),
                // null `type` and `model` are no advisor entry and no model.
                r#"{"type":null,"model":null,"input_tokens":5,"output_tokens":5}"#.to_owned(),
            ],
            "",
        ),
    )
    .expect("write transcript");
    let recs = parse_file(&path);
    assert_eq!(recs.len(), 2, "the line plus the one advisor rec");
    assert!(recs[0].is_message);
    assert_eq!((recs[0].input, recs[0].output), (30, 7));
    let adv = &recs[1];
    assert!(adv.advisor);
    assert_eq!(adv.model, "claude-opus-5-5");
    assert_eq!(
        (adv.input, adv.output, adv.cache_read, adv.cache_create),
        (0, 9, 0, 0)
    );
}

#[test]
fn a_zero_token_advisor_iteration_adds_no_rec() {
    let sb = HomeSandbox::new();
    let path = sb.home().join("zero.jsonl");
    std::fs::write(
        &path,
        iterations_line(
            "2026-06-11T10:00:00+00:00",
            "msg_x",
            "u1",
            (10, 3),
            &[advisor_iteration("claude-opus-5-5", 0, 0)],
            "",
        ),
    )
    .expect("write transcript");
    assert_eq!(parse_file(&path).len(), 1);
}

#[test]
fn an_advisor_iteration_without_a_model_counts_as_unknown() {
    let sb = HomeSandbox::new();
    let path = sb.home().join("nomodel.jsonl");
    std::fs::write(
        &path,
        iterations_line(
            "2026-06-11T10:00:00+00:00",
            "msg_x",
            "u1",
            (10, 3),
            &[
                r#"{"type":"advisor_message","input_tokens":5,"output_tokens":1}"#.to_owned(),
                r#"{"type":"advisor_message","model":"   ","input_tokens":7,"output_tokens":2}"#
                    .to_owned(),
            ],
            "",
        ),
    )
    .expect("write transcript");
    let advisors: Vec<_> = parse_file(&path)
        .into_iter()
        .filter(|r| r.advisor)
        .collect();
    assert_eq!(advisors.len(), 2);
    assert!(
        advisors.iter().all(|r| r.model == "unknown"),
        "an absent or blank model keeps the unknown bucket, as a main line does"
    );
}

#[test]
fn a_placeholder_advisor_model_is_dropped_like_a_placeholder_main_model() {
    let sb = HomeSandbox::new();
    let claude_dir = make_claude_dir(&sb);
    cutoff_stats_cache(&claude_dir);
    write_project_file(
        &claude_dir,
        "s.jsonl",
        &[iterations_line(
            "2026-06-11T10:00:00+00:00",
            "msg_x",
            "u1",
            (10, 3),
            &[advisor_iteration("<synthetic>", 500, 50)],
            "",
        )],
    );
    let stats = load(&claude_dir).expect("load");
    assert!(stats.models.iter().all(|m| !m.model.contains('<')));
    assert_eq!(stats.models.len(), 1);
}

#[test]
fn an_idless_line_keys_its_advisor_iteration_off_the_composite_key() {
    let sb = HomeSandbox::new();
    let claude_dir = make_claude_dir(&sb);
    cutoff_stats_cache(&claude_dir);
    let line = format!(
        r#"{{"timestamp":"2026-06-11T10:00:00+00:00","message":{{"model":"claude-sonnet-4-5","usage":{{"input_tokens":10,"output_tokens":3,"iterations":[{}]}}}}}}"#,
        advisor_iteration("claude-opus-5-5", 100, 10)
    );
    // The same id-less line in two files: the content-derived key dedups the
    // main rec, and the advisor key hangs off it, so the advisor dedups too.
    write_project_file(&claude_dir, "a.jsonl", std::slice::from_ref(&line));
    write_project_file(&claude_dir, "b.jsonl", &[line]);
    let stats = load(&claude_dir).expect("load");
    let opus = lifetime_row(&stats, "claude-opus-5-5").expect("advisor row");
    assert_eq!((opus.input, opus.output), (100, 10));
}

#[test]
fn advisor_iterations_dedupe_across_streamed_copies_and_files() {
    let sb = HomeSandbox::new();
    let claude_dir = make_claude_dir(&sb);
    cutoff_stats_cache(&claude_dir);
    // Three streamed copies of one turn in file A (one line per content block,
    // each repeating the same iterations), and the turn carried forward into a
    // second file under the same message id.
    let iterations = [
        message_iteration(30, 7),
        advisor_iteration("claude-opus-5-5", 301_544, 5_477),
    ];
    let copies: Vec<String> = ["u1", "u2", "u3"]
        .iter()
        .enumerate()
        .map(|(i, uuid)| {
            iterations_line(
                &format!("2026-06-11T10:00:0{i}+00:00"),
                "msg_x",
                uuid,
                (30, 7),
                &iterations,
                "",
            )
        })
        .collect();
    write_project_file(&claude_dir, "a.jsonl", &copies);
    write_project_file(
        &claude_dir,
        "b.jsonl",
        &[iterations_line(
            "2026-06-11T10:00:09+00:00",
            "msg_x",
            "u4",
            (30, 7),
            &iterations,
            "",
        )],
    );

    let stats = load(&claude_dir).expect("load");
    let opus = lifetime_row(&stats, "claude-opus-5-5").expect("advisor lifetime row");
    assert_eq!((opus.input, opus.output), (301_544, 5_477), "counted once");
    let day_rows: Vec<_> = stats
        .daily_models
        .iter()
        .filter(|d| d.model == "claude-opus-5-5")
        .collect();
    assert_eq!(day_rows.len(), 1);
    assert_eq!(day_rows[0].date, "2026-06-11");
    assert_eq!(day_rows[0].in_out, 301_544 + 5_477);

    // The main model's numbers are exactly what they were before the advisor
    // was read: one turn, top-level usage only.
    let sonnet = lifetime_row(&stats, "claude-sonnet-4-5").expect("main row");
    assert_eq!((sonnet.input, sonnet.output), (30, 7));
}

#[test]
fn a_growing_iterations_list_counts_each_advisor_call_once() {
    let sb = HomeSandbox::new();
    let claude_dir = make_claude_dir(&sb);
    cutoff_stats_cache(&claude_dir);
    let first = [
        message_iteration(30, 7),
        advisor_iteration("claude-opus-5-5", 100, 10),
    ];
    let grown = [
        message_iteration(30, 7),
        advisor_iteration("claude-opus-5-5", 100, 10),
        message_iteration(5, 1),
        advisor_iteration("claude-opus-5-5", 50, 5),
    ];
    write_project_file(
        &claude_dir,
        "a.jsonl",
        &[
            iterations_line(
                "2026-06-11T10:00:00+00:00",
                "msg_x",
                "u1",
                (30, 7),
                &first,
                "",
            ),
            iterations_line(
                "2026-06-11T10:00:01+00:00",
                "msg_x",
                "u2",
                (35, 8),
                &grown,
                "",
            ),
        ],
    );
    let stats = load(&claude_dir).expect("load");
    let opus = lifetime_row(&stats, "claude-opus-5-5").expect("advisor row");
    assert_eq!(
        (opus.input, opus.output),
        (150, 15),
        "adv0 counted once across both copies, adv1 once from the later one"
    );
}

#[test]
fn reordered_non_advisor_entries_keep_the_advisor_key() {
    let sb = HomeSandbox::new();
    let claude_dir = make_claude_dir(&sb);
    cutoff_stats_cache(&claude_dir);
    let advisor = advisor_iteration("claude-opus-5-5", 100, 10);
    // The ordinal counts advisor entries only, so moving the advisor entry
    // within the list, or adding message entries around it, does not move it
    // to a new key.
    let early = [advisor.clone(), message_iteration(30, 7)];
    let late = [
        message_iteration(1, 1),
        message_iteration(2, 2),
        advisor,
        message_iteration(3, 3),
    ];
    write_project_file(
        &claude_dir,
        "a.jsonl",
        &[
            iterations_line(
                "2026-06-11T10:00:00+00:00",
                "msg_x",
                "u1",
                (30, 7),
                &early,
                "",
            ),
            iterations_line(
                "2026-06-11T10:00:01+00:00",
                "msg_x",
                "u2",
                (30, 7),
                &late,
                "",
            ),
        ],
    );
    let stats = load(&claude_dir).expect("load");
    let opus = lifetime_row(&stats, "claude-opus-5-5").expect("advisor row");
    assert_eq!((opus.input, opus.output), (100, 10));
}

#[test]
fn differing_advisor_totals_for_one_key_keep_the_larger() {
    let sb = HomeSandbox::new();
    let claude_dir = make_claude_dir(&sb);
    cutoff_stats_cache(&claude_dir);
    // A streamed copy written while the advisor call was still accumulating,
    // then the settled one: the max-total record wins, as for a main line.
    write_project_file(
        &claude_dir,
        "a.jsonl",
        &[
            iterations_line(
                "2026-06-11T10:00:00+00:00",
                "msg_x",
                "u1",
                (30, 7),
                &[advisor_iteration("claude-opus-5-5", 100, 2)],
                "",
            ),
            iterations_line(
                "2026-06-11T10:00:01+00:00",
                "msg_x",
                "u2",
                (30, 7),
                &[advisor_iteration("claude-opus-5-5", 100, 40)],
                "",
            ),
            iterations_line(
                "2026-06-11T10:00:02+00:00",
                "msg_x",
                "u3",
                (30, 7),
                &[advisor_iteration("claude-opus-5-5", 100, 9)],
                "",
            ),
        ],
    );
    let stats = load(&claude_dir).expect("load");
    let opus = lifetime_row(&stats, "claude-opus-5-5").expect("advisor row");
    assert_eq!((opus.input, opus.output), (100, 40));
}

#[test]
fn the_same_message_id_in_two_files_counts_its_advisor_call_once() {
    let sb = HomeSandbox::new();
    let claude_dir = make_claude_dir(&sb);
    cutoff_stats_cache(&claude_dir);
    let line = |uuid: &str, advisor_out: u64| {
        iterations_line(
            "2026-06-11T10:00:00+00:00",
            "msg_x",
            uuid,
            (30, 7),
            &[advisor_iteration("claude-opus-5-5", 100, advisor_out)],
            "",
        )
    };
    // The files merge in path order and the first-seen key wins across files
    // (the rule main lines follow too), so the differing copy in `b` is not
    // added on top.
    write_project_file(&claude_dir, "a.jsonl", &[line("u1", 10)]);
    write_project_file(&claude_dir, "b.jsonl", &[line("u2", 25)]);
    let stats = load(&claude_dir).expect("load");
    let opus = lifetime_row(&stats, "claude-opus-5-5").expect("advisor row");
    assert_eq!((opus.input, opus.output), (100, 10));
}

#[test]
fn two_advisor_iterations_on_one_line_count_once_each() {
    let sb = HomeSandbox::new();
    let claude_dir = make_claude_dir(&sb);
    cutoff_stats_cache(&claude_dir);
    write_project_file(
        &claude_dir,
        "a.jsonl",
        &[iterations_line(
            "2026-06-11T10:00:00+00:00",
            "msg_x",
            "u1",
            (30, 7),
            &[
                message_iteration(30, 7),
                advisor_iteration("claude-opus-5-5", 100, 10),
                advisor_iteration("claude-opus-5-5", 200, 20),
                advisor_iteration("claude-haiku-4-5", 7, 3),
            ],
            "",
        )],
    );
    let stats = load(&claude_dir).expect("load");
    let opus = lifetime_row(&stats, "claude-opus-5-5").expect("opus row");
    assert_eq!(
        (opus.input, opus.output),
        (300, 30),
        "same-model iterations sum into the model's row"
    );
    let haiku = lifetime_row(&stats, "claude-haiku-4-5").expect("haiku row");
    assert_eq!((haiku.input, haiku.output), (7, 3));
}

/// An advisor iteration that records its own `speed`.
fn advisor_iteration_at(model: &str, input: u64, output: u64, speed: &str) -> String {
    format!(
        r#"{{"type":"advisor_message","model":"{model}","speed":"{speed}","input_tokens":{input},"output_tokens":{output},"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}"#
    )
}

#[test]
fn a_fast_line_leaves_its_advisor_iteration_standard() {
    let sb = HomeSandbox::new();
    let claude_dir = make_claude_dir(&sb);
    cutoff_stats_cache(&claude_dir);
    write_project_file(
        &claude_dir,
        "a.jsonl",
        &[
            iterations_line(
                "2026-06-11T09:00:00+00:00",
                "msg_fast",
                "u1",
                (30, 7),
                &[advisor_iteration("claude-opus-5-5", 100, 10)],
                r#","speed":"fast""#,
            ),
            iterations_line(
                "2026-06-11T15:00:00+00:00",
                "msg_std",
                "u2",
                (30, 7),
                &[advisor_iteration("claude-opus-5-5", 40, 4)],
                r#","speed":"standard""#,
            ),
        ],
    );
    let stats = load(&claude_dir).expect("load");
    let opus = lifetime_row(&stats, "claude-opus-5-5").expect("advisor row");
    assert_eq!((opus.input, opus.output), (140, 14));
    assert_eq!(
        (opus.fast_input, opus.fast_output),
        (0, 0),
        "the line's speed is not the advisor's: no iteration speed, no fast subset"
    );
    // The main model's own fast line keeps its fast subset.
    let sonnet = lifetime_row(&stats, "claude-sonnet-4-5").expect("main row");
    assert_eq!((sonnet.fast_input, sonnet.fast_output), (30, 7));
}

#[test]
fn an_advisor_iteration_carrying_its_own_speed_is_priced_at_it() {
    let sb = HomeSandbox::new();
    let claude_dir = make_claude_dir(&sb);
    cutoff_stats_cache(&claude_dir);
    write_project_file(
        &claude_dir,
        "a.jsonl",
        &[
            // A standard line whose advisor call is fast.
            iterations_line(
                "2026-06-11T09:00:00+00:00",
                "msg_a",
                "u1",
                (30, 7),
                &[advisor_iteration_at("claude-opus-5-5", 100, 10, "fast")],
                "",
            ),
            // A fast line whose advisor call says standard.
            iterations_line(
                "2026-06-11T15:00:00+00:00",
                "msg_b",
                "u2",
                (30, 7),
                &[advisor_iteration_at("claude-opus-5-5", 40, 4, "standard")],
                r#","speed":"fast""#,
            ),
        ],
    );
    let stats = load(&claude_dir).expect("load");
    let opus = lifetime_row(&stats, "claude-opus-5-5").expect("advisor row");
    assert_eq!((opus.input, opus.output), (140, 14));
    assert_eq!((opus.fast_input, opus.fast_output), (100, 10));
    let row = stats
        .daily_models
        .iter()
        .find(|d| d.model == "claude-opus-5-5")
        .expect("day row");
    let hours = row.hours.as_ref().expect("hours");
    assert_eq!((hours[9].fast_input, hours[9].fast_output), (100, 10));
    assert_eq!((hours[15].input, hours[15].fast_input), (40, 0));
}

#[test]
fn advisor_rows_never_classify_or_correct_the_shape() {
    let sb = HomeSandbox::new();
    let path = sb.home().join("shape.jsonl");
    // Nine lines whose advisor iteration reports input and no cache fields on
    // a model with no main rows: over the row floor, that would classify as a
    // provider that reports no cache. The main model beside it is a plain
    // healthy cache-writing model.
    let lines: Vec<String> = (0..9)
        .map(|i| {
            iterations_line(
                "2026-06-11T10:00:00+00:00",
                &format!("msg_{i}"),
                &format!("u{i}"),
                (30, 7),
                &[advisor_iteration("claude-opus-5-5", 100, 10)],
                "",
            )
            .replace(
                r#""cache_creation_input_tokens":0,"iterations""#,
                r#""cache_creation_input_tokens":5,"iterations""#,
            )
        })
        .collect();
    std::fs::write(&path, lines.join("\n")).expect("write transcript");
    let recs = parse_file(&path);
    let advisors: Vec<_> = recs.iter().filter(|r| r.advisor).collect();
    assert_eq!(advisors.len(), 9);
    assert!(
        advisors
            .iter()
            .all(|r| r.shape == UsageShape::Healthy && r.input == 100),
        "advisor rows are never classified and never input-corrected"
    );
    let main: Vec<_> = recs.iter().filter(|r| !r.advisor).collect();
    assert!(main.iter().all(|r| r.shape == UsageShape::Healthy));
}

#[test]
fn advisor_rows_do_not_count_toward_a_main_models_row_floor() {
    let sb = HomeSandbox::new();
    let path = sb.home().join("floor.jsonl");
    // Four main turns on one model, reading cache and writing none: under the
    // row floor (8) that classifies as healthy. Each line also carries an
    // advisor call on the SAME model id, so if those rows joined the model's
    // group it would reach the floor and the main rows would be re-classified
    // as not writing cache.
    let lines: Vec<String> = (0..4)
        .map(|i| {
            iterations_line(
                "2026-06-11T10:00:00+00:00",
                &format!("msg_{i}"),
                &format!("u{i}"),
                (100, 10),
                &[
                    message_iteration(100, 10),
                    advisor_iteration("claude-sonnet-4-5", 5, 1),
                ],
                "",
            )
            .replace(
                r#""cache_read_input_tokens":0,"cache_creation_input_tokens":0,"iterations""#,
                r#""cache_read_input_tokens":1000,"cache_creation_input_tokens":0,"iterations""#,
            )
        })
        .collect();
    std::fs::write(&path, lines.join("\n")).expect("write transcript");
    let recs = parse_file(&path);
    let main: Vec<_> = recs.iter().filter(|r| !r.advisor).collect();
    assert_eq!(main.len(), 4);
    assert_eq!(recs.len(), 8);
    assert!(
        main.iter()
            .all(|r| r.shape == UsageShape::Healthy && r.input == 100),
        "four main rows stay under the floor however many advisor rows share the model"
    );
}

#[test]
fn advisor_iterations_leave_message_and_tool_counts_alone() {
    let sb = HomeSandbox::new();
    let claude_dir = make_claude_dir(&sb);
    cutoff_stats_cache(&claude_dir);
    let build = |advisor: bool| -> Vec<String> {
        (0..3)
            .map(|i| {
                let iterations = if advisor {
                    vec![
                        message_iteration(30, 7),
                        advisor_iteration("claude-opus-5-5", 100, 10),
                    ]
                } else {
                    vec![message_iteration(30, 7)]
                };
                iterations_line(
                    &format!("2026-06-11T1{i}:00:00+00:00"),
                    &format!("msg_{i}"),
                    &format!("u{i}"),
                    (30, 7),
                    &iterations,
                    "",
                )
            })
            .collect()
    };
    write_project_file(&claude_dir, "s.jsonl", &build(true));
    let with = load(&claude_dir).expect("load");
    write_project_file(&claude_dir, "s.jsonl", &build(false));
    let without = load(&claude_dir).expect("load");

    let day = |s: &TokenStats| {
        s.activity
            .iter()
            .find(|a| a.date == "2026-06-11")
            .map(|a| (a.messages, a.sessions, a.tool_calls))
    };
    assert_eq!(day(&with), day(&without));
    assert_eq!(day(&with), Some((3, 1, 3)));
    assert_eq!(with.total_messages, without.total_messages);
    assert_eq!(with.hour_counts, without.hour_counts);
}

#[test]
fn file_hourly_model_tokens_includes_advisor_rows() {
    let sb = HomeSandbox::new();
    let path = sb.home().join("sessions.jsonl");
    std::fs::write(
        &path,
        iterations_line(
            "2026-06-11T10:30:00+00:00",
            "msg_x",
            "u1",
            (30, 7),
            &[advisor_iteration("claude-opus-5-5", 100, 10)],
            "",
        ),
    )
    .expect("write transcript");
    let rows = file_hourly_model_tokens(&path);
    let opus = rows
        .iter()
        .find(|r| r.model == "claude-opus-5-5")
        .expect("the sessions cost view prices the advisor's model");
    assert_eq!(opus.day, "2026-06-11");
    assert_eq!(opus.hours[10].input, 100);
    assert_eq!(opus.hours[10].output, 10);
    let sonnet = rows
        .iter()
        .find(|r| r.model == "claude-sonnet-4-5")
        .expect("main row");
    assert_eq!(sonnet.hours[10].input, 30);
}

/// The first-paint view is stats-cache only: no advisor tokens. The topped-up
/// view then carries exactly one advisor contribution for each date class the
/// merge treats differently — today (cutoff day, and a later day), and a
/// post-cutoff day that is not today.
#[test]
fn the_first_paint_has_no_advisor_tokens_and_each_merged_view_exactly_one() {
    let sb = HomeSandbox::new();
    let claude_dir = make_claude_dir(&sb);
    cutoff_stats_cache(&claude_dir);
    let iterations = [advisor_iteration("claude-opus-5-5", 100, 10)];
    write_project_file(
        &claude_dir,
        "a.jsonl",
        &[
            iterations_line(
                "2026-06-10T10:00:00+00:00",
                "msg_cutoff",
                "u1",
                (30, 7),
                &iterations,
                "",
            ),
            iterations_line(
                "2026-06-11T10:00:00+00:00",
                "msg_post",
                "u2",
                (30, 7),
                &iterations,
                "",
            ),
            iterations_line(
                "2026-06-12T10:00:00+00:00",
                "msg_later",
                "u3",
                (30, 7),
                &iterations,
                "",
            ),
        ],
    );

    let base = load_base(&claude_dir).expect("base");
    assert!(
        lifetime_row(&base, "claude-opus-5-5").is_none() && base.today.is_none(),
        "the stats-cache first paint never carries advisor tokens"
    );

    let mut cache = TopUpCache::default();
    refresh_topup_cache(&claude_dir, Some("2026-06-10"), &mut cache, |_, _| {});
    let advisor_in = |s: &TokenStats| {
        s.today.as_ref().map_or(0, |t| {
            t.models
                .iter()
                .filter(|m| m.model == "claude-opus-5-5")
                .map(|m| m.input)
                .sum::<u64>()
        })
    };
    let day_input = |s: &TokenStats, date: &str| {
        s.daily_models
            .iter()
            .filter(|d| d.date == date && d.model == "claude-opus-5-5")
            .filter_map(|d| d.split.as_ref())
            .map(|m| m.input)
            .sum::<u64>()
    };

    // Today IS the cutoff day: the cutoff day's lines feed today's rollup only
    // (the stats-cache owns that day's history), the later days feed day rows.
    let mut merged = load_base(&claude_dir).expect("base");
    merge_topup(&mut merged, &cache, Some("2026-06-10"), "2026-06-10");
    assert_eq!(advisor_in(&merged), 100);
    assert_eq!(day_input(&merged, "2026-06-10"), 0);
    assert_eq!(day_input(&merged, "2026-06-11"), 100);
    assert_eq!(day_input(&merged, "2026-06-12"), 100);
    assert_eq!(
        lifetime_row(&merged, "claude-opus-5-5").map(|m| m.input),
        Some(200)
    );

    // Today is a post-cutoff day: it lands in today's rollup and in its day
    // row, once each; the other post-cutoff day stays a day row only.
    let mut merged = load_base(&claude_dir).expect("base");
    merge_topup(&mut merged, &cache, Some("2026-06-10"), "2026-06-12");
    assert_eq!(advisor_in(&merged), 100);
    assert_eq!(day_input(&merged, "2026-06-11"), 100);
    assert_eq!(day_input(&merged, "2026-06-12"), 100);
    assert_eq!(
        lifetime_row(&merged, "claude-opus-5-5").map(|m| m.input),
        Some(200)
    );

    // Today with no advisor lines: nothing is attributed to it.
    let mut merged = load_base(&claude_dir).expect("base");
    merge_topup(&mut merged, &cache, Some("2026-06-10"), "2026-06-20");
    assert_eq!(advisor_in(&merged), 0);
    assert_eq!(day_input(&merged, "2026-06-11"), 100);
    assert_eq!(day_input(&merged, "2026-06-12"), 100);
}

// ── advisor pricing ──────────────────────────────────────────────────────────

fn advisor_price_table(
    opus_entries: Vec<crate::pricing::PriceEntry>,
) -> crate::pricing::PriceTable {
    let priced = |id: &str, entries| crate::pricing::PricedModel {
        id: id.to_owned(),
        prices: entries,
        effective_at: None,
    };
    let entry = |input, output, mode| crate::pricing::PriceEntry {
        input,
        output,
        cache_read: 0.0,
        cache_write: 0.0,
        constraint: None,
        mode,
        window_only: false,
    };
    crate::pricing::PriceTable::capture(
        vec![
            priced("claude-opus-5-5", opus_entries),
            priced("claude-sonnet-4-5", vec![entry(0.001, 0.002, None)]),
        ],
        Vec::new(),
        Vec::new(),
        crate::pricing::CanonicalMap::default(),
        "2026-06-01".to_owned(),
        0,
        Vec::new(),
    )
}

fn advisor_rate_entry(
    input: f64,
    output: f64,
    mode: Option<crate::pricing::SpeedMode>,
) -> crate::pricing::PriceEntry {
    crate::pricing::PriceEntry {
        input,
        output,
        cache_read: 0.0,
        cache_write: 0.0,
        constraint: None,
        mode,
        window_only: false,
    }
}

/// Load one advisor-bearing transcript (main line on sonnet, advisor on
/// `advisor_model`, `extra_usage` spliced into the usage) and return the
/// advisor model's day row.
fn advisor_day_row(advisor_model: &str, extra_usage: &str) -> DayModelTokens {
    advisor_day_row_with(
        advisor_model,
        &advisor_iteration(advisor_model, 1_000, 100),
        extra_usage,
    )
}

/// [`advisor_day_row`] with the advisor iteration given verbatim.
fn advisor_day_row_with(advisor_model: &str, iteration: &str, extra_usage: &str) -> DayModelTokens {
    let sb = HomeSandbox::new();
    let claude_dir = make_claude_dir(&sb);
    cutoff_stats_cache(&claude_dir);
    write_project_file(
        &claude_dir,
        "a.jsonl",
        &[iterations_line(
            "2026-06-11T10:00:00+00:00",
            "msg_x",
            "u1",
            (30, 7),
            &[iteration.to_owned()],
            extra_usage,
        )],
    );
    load(&claude_dir)
        .expect("load")
        .daily_models
        .into_iter()
        .find(|d| d.model == advisor_model)
        .expect("advisor day row")
}

fn day_cost(table: &crate::pricing::PriceTable, row: &DayModelTokens) -> Option<f64> {
    table.cost_day(&row.model, &row.date, row.hours.as_ref().expect("hours"))
}

#[test]
fn advisor_spend_prices_at_the_advisor_models_rate() {
    use crate::pricing::SpeedMode;
    // Opus and sonnet carry distinct rates; the advisor's 1,000 in / 100 out
    // must price at opus's, not at the sonnet line it rode in on.
    let table = advisor_price_table(vec![advisor_rate_entry(0.01, 0.05, None)]);
    let row = advisor_day_row("claude-opus-5-5", "");
    let cost = day_cost(&table, &row).expect("priced");
    assert!((cost - (1_000.0 * 0.01 + 100.0 * 0.05)).abs() < 1e-9);

    // An advisor call with its own fast speed: the fast subset prices at the
    // fast entry.
    let fast_call = advisor_iteration_at("claude-opus-5-5", 1_000, 100, "fast");
    let table = advisor_price_table(vec![
        advisor_rate_entry(0.01, 0.05, None),
        advisor_rate_entry(0.02, 0.10, Some(SpeedMode::Fast)),
    ]);
    let row = advisor_day_row_with("claude-opus-5-5", &fast_call, "");
    let cost = day_cost(&table, &row).expect("priced");
    assert!((cost - (1_000.0 * 0.02 + 100.0 * 0.10)).abs() < 1e-9);

    // A fast line does not make its advisor call fast: it prices at the
    // standard rate even though a fast rate exists.
    let row = advisor_day_row("claude-opus-5-5", r#","speed":"fast""#);
    let cost = day_cost(&table, &row).expect("priced");
    assert!((cost - (1_000.0 * 0.01 + 100.0 * 0.05)).abs() < 1e-9);

    // Fast call, model with a standard-only entry: no fast rate exists, so the
    // row is unpriced exactly as a fast main-model row would be.
    let table = advisor_price_table(vec![advisor_rate_entry(
        0.01,
        0.05,
        Some(SpeedMode::Standard),
    )]);
    let row = advisor_day_row_with("claude-opus-5-5", &fast_call, "");
    assert_eq!(day_cost(&table, &row), None);
    // The same table prices a standard call, and a call on a fast line.
    let row = advisor_day_row("claude-opus-5-5", "");
    assert!(day_cost(&table, &row).is_some());
    let row = advisor_day_row("claude-opus-5-5", r#","speed":"fast""#);
    assert!(day_cost(&table, &row).is_some());
}

#[test]
fn an_unknown_advisor_model_is_unpriced_and_a_free_id_costs_nothing() {
    let table = advisor_price_table(vec![advisor_rate_entry(0.01, 0.05, None)]);
    let unknown = advisor_day_row("claude-mystery-9", "");
    assert_eq!(day_cost(&table, &unknown), None, "no rate, no price");

    let free = advisor_day_row("acme/advisor-model:free", "");
    assert_eq!(day_cost(&table, &free), Some(0.0));
}
