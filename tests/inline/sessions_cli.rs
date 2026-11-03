//! `clauth sessions/resume/info` CLI surface tests. Fixture stores live under a
//! `HomeSandbox` so the global (`~/.claude/projects`) walk stays off the real
//! tree. Each transcript is named `<sessionId>.jsonl` (the id is the filename
//! stem). Pure helpers (`resume_profile_choice`, `sessions_json`) are exercised
//! directly; the exit-code contract goes through `crate::exit_code`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;
use std::fs;
use std::path::Path;
use std::time::{Duration, SystemTime};

use serde_json::json;

use crate::testutil::{HomeSandbox, set_mtime};

fn write_jsonl(path: &Path, lines: &[String]) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, lines.join("\n")).unwrap();
}

fn user_line(sid: &str, cwd: &str, text: &str) -> String {
    json!({"sessionId": sid, "cwd": cwd, "message": {"role": "user", "content": text}}).to_string()
}

/// An assistant usage line — the token-bearing row `file_hourly_model_tokens` reads.
fn usage_line(sid: &str, cwd: &str, msg_id: &str, model: &str, input: u64, output: u64) -> String {
    json!({
        "sessionId": sid, "cwd": cwd, "timestamp": "2026-06-11T10:30:00+00:00",
        "message": {
            "id": msg_id, "role": "assistant", "model": model,
            "usage": {
                "input_tokens": input, "output_tokens": output,
                "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0
            }
        }
    })
    .to_string()
}

/// A `PriceTable` from `(model_id, input_rate, output_rate)` rows; cache rates 0.
fn price_table(rows: &[(&str, f64, f64)]) -> crate::pricing::PriceTable {
    crate::pricing::PriceTable::capture(
        rows.iter()
            .map(|&(id, input, output)| crate::pricing::PricedModel {
                id: id.to_owned(),
                prices: vec![crate::pricing::PriceEntry {
                    input,
                    output,
                    cache_read: 0.0,
                    cache_write: 0.0,
                    constraint: None,
                    mode: None,
                    window_only: false,
                }],
                effective_at: None,
            })
            .collect(),
        Vec::new(),
        Vec::new(),
        crate::pricing::CanonicalMap::default(),
        crate::tokens::today_date(),
        0,
        Vec::new(),
    )
}

// ── clauth sessions --json ──

#[test]
fn sessions_json_has_exact_fields_newest_first_with_null_and_redaction() {
    let sb = HomeSandbox::new();

    // Newer session: a token-bearing usage row + a secret in the first message.
    let a = sb.home().join(".claude/projects/-w-a/aaaa-1111.jsonl");
    write_jsonl(
        &a,
        &[
            user_line(
                "aaaa-1111",
                "/ws/a",
                "my key sk-ant-api03-ABCDEFGHIJKLMNOPQRST here",
            ),
            usage_line("aaaa-1111", "/ws/a", "m1", "claude-sonnet-4", 100, 50),
        ],
    );

    // Older session: no usage row, so its token total stays absent (JSON null).
    let b = sb.home().join(".claude/projects/-w-b/bbbb-2222.jsonl");
    write_jsonl(&b, &[user_line("bbbb-2222", "/ws/b", "plain question")]);
    set_mtime(&b, SystemTime::now() - Duration::from_secs(3600));

    let mut groups = crate::sessions::build_index();
    let price = price_table(&[("claude-sonnet-4", 0.000003, 0.000015)]);
    crate::sessions::annotate_all(&mut groups, Some(&price));
    crate::sessions::annotate_owners(&mut groups);
    let flat = flatten_newest_first(&groups);
    let value = sessions_json(&flat);

    let arr = value.as_array().expect("json array");
    assert_eq!(arr.len(), 2, "both sessions present");

    // Newest-first: the token-bearing session (fresh mtime) leads.
    assert_eq!(arr[0]["id"], json!("aaaa-1111"), "newest session first");
    assert_eq!(arr[1]["id"], json!("bbbb-2222"));

    // Exactly the documented field set — no more, no less.
    let keys: std::collections::BTreeSet<&str> = arr[0]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    let want: std::collections::BTreeSet<&str> = [
        "id",
        "last_ran_profile",
        "workspace",
        "updated",
        "first_message",
        "last_message",
        "tokens",
        "cost",
    ]
    .into_iter()
    .collect();
    assert_eq!(
        keys, want,
        "json row must carry exactly the documented fields"
    );

    // Tokenless session ⇒ JSON null, never 0.
    assert_eq!(arr[1]["tokens"], json!(null), "absent tokens must be null");
    assert_eq!(arr[1]["cost"], json!(null), "absent cost must be null");

    // Priced token-bearing session ⇒ a real number, not null.
    assert!(
        arr[0]["tokens"].is_number(),
        "priced session has a token total"
    );
    assert!(arr[0]["cost"].is_number(), "priced session has a cost");

    // `updated` is an ISO-8601 UTC string.
    let updated = arr[0]["updated"].as_str().expect("updated is a string");
    assert!(
        updated.contains('T') && updated.ends_with("+00:00"),
        "updated must be ISO-8601 UTC, got {updated}"
    );

    // Redaction survives into the emitted preview.
    let preview = arr[0]["first_message"]
        .as_str()
        .expect("first_message string");
    assert!(
        preview.contains("[REDACTED]") && !preview.contains("sk-ant-api03-ABCDEFGHIJKLMNOPQRST"),
        "the secret must be redacted in the preview, got {preview}"
    );
}

// ── --tokens gates the full-transcript annotation ──

#[test]
fn the_listing_costs_a_full_read_of_every_transcript_only_under_tokens() {
    let sb = HomeSandbox::new();
    let a = sb.home().join(".claude/projects/-w-a/aaaa-1111.jsonl");
    write_jsonl(
        &a,
        &[
            user_line("aaaa-1111", "/ws/a", "hello"),
            usage_line("aaaa-1111", "/ws/a", "m1", "claude-sonnet-4", 100, 50),
        ],
    );

    // Default: the index only, so the figure the annotation would fill stays
    // absent. Its `None` is the same one a session with no usage row gets.
    let plain = build_listing(false);
    let row = &plain[0].sessions[0];
    assert_eq!(row.tokens, None, "no annotation ⇒ no token total");
    assert_eq!(
        row.first_message.as_deref(),
        Some("hello"),
        "the preview still comes from the index, which the flag never gates"
    );

    // Opt in and the same store yields a real total — so the `None` above is
    // the flag's doing, not a fixture that had nothing to report. Cost stays
    // absent either way here: the sandbox has no price cache, and a cold cache
    // prices nothing.
    let annotated = build_listing(true);
    let row = &annotated[0].sessions[0];
    assert_eq!(row.tokens, Some(150), "--tokens sums input+output");
}

#[test]
fn the_table_carries_the_token_columns_only_under_tokens() {
    let sb = HomeSandbox::new();
    let a = sb.home().join(".claude/projects/-w-a/aaaa-1111.jsonl");
    write_jsonl(
        &a,
        &[
            user_line("aaaa-1111", "/ws/a", "hello"),
            usage_line("aaaa-1111", "/ws/a", "m1", "claude-sonnet-4", 100, 50),
        ],
    );
    // A fixed mtime keeps the timestamp cell out of the substring assertions.
    set_mtime(&a, SystemTime::UNIX_EPOCH + Duration::from_secs(3_000));

    // Annotated through the same call the flag makes, so the row has real
    // figures to print and a missing column is the flag's doing, not an empty
    // fixture rendering blank either way. The rates put the cost at $1.05 —
    // above the row's two-decimal rounding floor, so the assertion pins the
    // VALUE, not just a dollar-shaped cell (a zeroed cost also renders "$0.00").
    let mut groups = build_listing(false);
    crate::sessions::annotate_all(
        &mut groups,
        Some(&price_table(&[("claude-sonnet-4", 0.003, 0.015)])),
    );
    let session = &groups[0].sessions[0];
    let now = SystemTime::UNIX_EPOCH + Duration::from_secs(3_000 + 3_600);

    let plain = session_row(session, false, now, Columns::FLOOR);
    assert!(
        !plain.contains("150") && !plain.contains('$'),
        "the default row carries neither figure: {plain}"
    );
    assert!(plain.contains("hello"), "it still carries the preview");

    let annotated = session_row(session, true, now, Columns::FLOOR);
    assert!(
        annotated.contains("150") && annotated.contains("$1.05"),
        "--tokens adds both cells: {annotated}"
    );
}

#[test]
fn the_table_preserves_distinct_subagent_ids() {
    let sb = HomeSandbox::new();
    let ids = ["agent-a1b2c3d4", "agent-e5f6a7b8"];
    for id in ids {
        let path = sb
            .home()
            .join(".claude/projects/-w-a/parent/subagents")
            .join(format!("{id}.jsonl"));
        write_jsonl(&path, &[user_line(id, "/ws/a", "inspect the same file")]);
        set_mtime(&path, SystemTime::UNIX_EPOCH + Duration::from_secs(3_000));
    }

    let groups = build_listing(false);
    let flat = flatten_newest_first(&groups);
    assert_eq!(flat.len(), 2);
    for (session, id) in flat.iter().zip(ids) {
        let row = session_row(session, false, SystemTime::UNIX_EPOCH, Columns::FLOOR);
        assert_eq!(row.split_whitespace().next(), Some(id), "{row}");
    }
    let json = sessions_json(&flat);
    assert_eq!(json[0]["id"], ids[0]);
    assert_eq!(json[1]["id"], ids[1]);
}

#[test]
fn only_uuid_session_ids_are_shortened() {
    for id in [
        "12345678-1234-4567-890a-123456789abc",
        "ABCDEF01-1234-4567-890A-123456789ABC",
    ] {
        assert_eq!(short_id(id), &id[..8]);
    }
    for id in [
        "agent-a1b2c3d4",
        "workflow-step-one",
        "12345678-not-a-uuid",
        "1234567812344567890a123456789abc----",
        "12345678-1234-4567-890a-123456789abg",
        "12345678-1234-4567-890a-123456789aé",
        "plain",
        "",
    ] {
        assert_eq!(short_id(id), id);
    }
}

/// A subagent id and a long profile name each widen their column for every
/// row, across workspace groups: the stamp cell starts at one offset in the
/// whole table. The widths are hand-counted: the agent id is 23 chars, the
/// profile 17, so the uuid prefix pads 15 and the `-` profile pads 16.
#[test]
fn the_table_columns_widen_to_their_widest_cell_in_every_row() {
    let sb = HomeSandbox::new();
    let uuid = "12345678-1234-4567-890a-123456789abc";
    let agent = "agent-a0c3ac273daf72f57";
    let uuid_path = sb
        .home()
        .join(format!(".claude/projects/-w-a/{uuid}.jsonl"));
    write_jsonl(&uuid_path, &[user_line(uuid, "/ws/a", "parent")]);
    set_mtime(
        &uuid_path,
        SystemTime::UNIX_EPOCH + Duration::from_secs(4_000),
    );
    let agent_path = sb.home().join(format!(
        ".claude/projects/-w-b/parent/subagents/{agent}.jsonl"
    ));
    write_jsonl(&agent_path, &[user_line(agent, "/ws/b", "child")]);
    set_mtime(
        &agent_path,
        SystemTime::UNIX_EPOCH + Duration::from_secs(3_000),
    );

    let mut groups = build_listing(false);
    assert_eq!(groups.len(), 2, "two workspaces, two groups");
    let parent = groups
        .iter_mut()
        .flat_map(|g| &mut g.sessions)
        .find(|s| s.id == uuid)
        .unwrap();
    parent.last_ran_profile = Some("work-account-long".to_string());

    let lines = table_lines(&groups, false, SystemTime::UNIX_EPOCH);
    let parent_row = lines.iter().find(|l| l.contains("parent")).unwrap();
    let child_row = lines.iter().find(|l| l.contains("child")).unwrap();
    let parent_cells = format!("  12345678{}  work-account-long  ", " ".repeat(15));
    let child_cells = format!("  agent-a0c3ac273daf72f57  -{}  ", " ".repeat(16));
    assert!(parent_row.starts_with(&parent_cells), "{parent_row:?}");
    assert!(child_row.starts_with(&child_cells), "{child_row:?}");
}

/// The profile column sizes and pads by drawn cells: a profile cell of eight
/// wide glyphs (sixteen cells) lays the table out exactly as a sixteen-letter
/// ASCII name does. The second row's fourteen-letter profile sits between the
/// wide cell's char count (8) and its cell count (16), so measuring by chars
/// sizes the column to 14 and shifts the rows apart, past the floor of 12.
#[test]
fn a_wide_script_profile_pads_like_its_ascii_twin() {
    const WIDE: &str = "漢字漢字漢字漢字";
    const TWIN: &str = "abcdefghijklmnop";
    let sb = HomeSandbox::new();
    let wide_id = "12345678-1234-4567-890a-123456789abc";
    let narrow_id = "87654321-1234-4567-890a-123456789abc";
    for (id, secs, preview) in [(wide_id, 4_000, "parent"), (narrow_id, 3_000, "sibling")] {
        let path = sb.home().join(format!(".claude/projects/-w-a/{id}.jsonl"));
        write_jsonl(&path, &[user_line(id, "/ws/a", preview)]);
        set_mtime(&path, SystemTime::UNIX_EPOCH + Duration::from_secs(secs));
    }

    let mut groups = build_listing(false);
    let mut lines_with = |profile: &str| {
        for session in groups.iter_mut().flat_map(|g| &mut g.sessions) {
            let name = if session.id == wide_id {
                profile
            } else {
                "abcdefghijklmn"
            };
            session.last_ran_profile = Some(name.to_string());
        }
        table_lines(&groups, false, SystemTime::UNIX_EPOCH)
    };
    let twin = lines_with(TWIN).join("\n");
    let wide = lines_with(WIDE).join("\n");
    assert!(twin.contains(TWIN), "the twin row renders the twin: {twin}");
    assert_eq!(wide, twin.replace(TWIN, WIDE));
}

/// A `--live` cwd is any path the session ran in, so its column sizes and
/// pads by drawn cells: the tag after a wide-script cwd starts at the same
/// column on every row.
#[test]
fn a_wide_script_live_cwd_keeps_the_tag_column_aligned() {
    let row = |sid: &str, ago_secs: u64, cwd: &str| crate::live_sessions::LiveSession {
        cwd: Some(std::path::PathBuf::from(cwd)),
        background: true,
        ..live_fixture_row(sid, "cld1", ago_secs)
    };
    let now = SystemTime::UNIX_EPOCH + Duration::from_secs(LIVE_NOW_SECS);
    assert_eq!(
        live_lines_of(
            &[row("100-0", 60, "/w/漢字"), row("200-0", 120, "/w/abc")],
            now
        ),
        [
            "100-0  cld1  claude  1m ago  -  /w/漢字  background",
            "200-0  cld1  claude  2m ago  -  /w/abc   background",
        ]
    );
}

/// A 5-char stem pads to the id floor of 8, the `-` profile to 12.
#[test]
fn a_table_with_no_wide_cell_keeps_the_floor_widths() {
    let sb = HomeSandbox::new();
    let path = sb.home().join(".claude/projects/-w-a/notes.jsonl");
    write_jsonl(&path, &[user_line("notes", "/ws/a", "only")]);
    set_mtime(&path, SystemTime::UNIX_EPOCH + Duration::from_secs(3_000));

    let lines = table_lines(&build_listing(false), false, SystemTime::UNIX_EPOCH);
    assert_eq!(lines.len(), 2, "{lines:?}");
    let cells = format!("  notes{}  -{}  ", " ".repeat(3), " ".repeat(11));
    assert!(lines[1].starts_with(&cells), "{:?}", lines[1]);
}

/// A named subagent's 45-char stem shows whole but widens the id column only
/// to the 23-char cap, so the uuid row pads 15, not 37.
#[test]
fn the_id_column_stops_widening_at_its_cap() {
    let sb = HomeSandbox::new();
    let uuid = "12345678-1234-4567-890a-123456789abc";
    let named = "agent-ap4-validation-persist-9802277861dd8fcf";
    let uuid_path = sb
        .home()
        .join(format!(".claude/projects/-w-a/{uuid}.jsonl"));
    write_jsonl(&uuid_path, &[user_line(uuid, "/ws/a", "parent")]);
    set_mtime(
        &uuid_path,
        SystemTime::UNIX_EPOCH + Duration::from_secs(4_000),
    );
    let named_path = sb.home().join(format!(
        ".claude/projects/-w-a/parent/subagents/{named}.jsonl"
    ));
    write_jsonl(&named_path, &[user_line(named, "/ws/a", "child")]);
    set_mtime(
        &named_path,
        SystemTime::UNIX_EPOCH + Duration::from_secs(3_000),
    );

    let lines = table_lines(&build_listing(false), false, SystemTime::UNIX_EPOCH);
    let parent_row = lines.iter().find(|l| l.contains("parent")).unwrap();
    let child_row = lines.iter().find(|l| l.contains("child")).unwrap();
    let parent_cells = format!("  12345678{}  -{}  ", " ".repeat(15), " ".repeat(11));
    let child_cells = format!("  {named}  -{}  ", " ".repeat(11));
    assert!(parent_row.starts_with(&parent_cells), "{parent_row:?}");
    assert!(child_row.starts_with(&child_cells), "{child_row:?}");
}

/// Under `--tokens`, a short and a long age, an 11-digit token total and a
/// 9-char cost each widen their column for both rows. Ages hand-derived:
/// 90 000 s is `1d 1h`, 5 s is `5s`; so the age column is 9 wide, tokens 11,
/// cost 9.
#[test]
fn the_age_tokens_and_cost_columns_widen_to_their_widest_cell() {
    let sb = HomeSandbox::new();
    let now = SystemTime::UNIX_EPOCH + Duration::from_secs(93_000);
    for (id, text, updated) in [("aaaa-1111", "alpha", 3_000), ("bbbb-2222", "beta", 92_995)] {
        let path = sb.home().join(format!(".claude/projects/-w-a/{id}.jsonl"));
        write_jsonl(&path, &[user_line(id, "/ws/a", text)]);
        set_mtime(&path, SystemTime::UNIX_EPOCH + Duration::from_secs(updated));
    }
    let mut groups = build_listing(false);
    for s in &mut groups[0].sessions {
        (s.tokens, s.cost) = if s.id == "aaaa-1111" {
            (Some(150), Some(1.05))
        } else {
            (Some(12_345_678_901), Some(12_345.67))
        };
    }

    let lines = table_lines(&groups, true, now);
    let alpha = lines.iter().find(|l| l.contains("alpha")).unwrap();
    let beta = lines.iter().find(|l| l.contains("beta")).unwrap();
    let alpha_tail = format!(
        "(1d ago)  {}150  {}$1.05  alpha | alpha",
        " ".repeat(10),
        " ".repeat(4)
    );
    let beta_tail = "(just now)  12345678901  $12345.67  beta | beta".to_string();
    assert!(alpha.ends_with(&alpha_tail), "{alpha:?}");
    assert!(beta.ends_with(&beta_tail), "{beta:?}");
}

// ── the stamp cell: local wall clock + age ──

/// The human row renders `updated` as LOCAL wall clock paired with its
/// relative age (the 2026-08-22 ruling) — never the ISO machine stamp the
/// JSON row keeps. The expected stamp derives through chrono's own `format`
/// rather than the crate's formatter, so the pin is a second derivation of
/// the same claim and holds in any zone; where the fixture instant's local
/// spelling differs from its UTC one, the UTC digits must be ABSENT (when
/// the two spellings coincide there is no second spelling to ban).
#[test]
fn the_table_stamp_renders_local_wall_clock_with_a_relative_age() {
    let sb = HomeSandbox::new();
    let a = sb.home().join(".claude/projects/-w-a/aaaa-1111.jsonl");
    write_jsonl(&a, &[user_line("aaaa-1111", "/ws/a", "hello")]);
    // 2026-06-21T00:00:00Z, with a `now` exactly 3h 12m later — the age cell
    // is deterministic by construction.
    let updated = SystemTime::UNIX_EPOCH + Duration::from_secs(1_782_000_000);
    let now = updated + Duration::from_secs(11_520);
    set_mtime(&a, updated);

    let groups = build_listing(false);
    let session = &groups[0].sessions[0];
    let row = session_row(session, false, now, Columns::FLOOR);

    let local = chrono::DateTime::from_timestamp(1_782_000_000, 0)
        .unwrap()
        .with_timezone(&chrono::Local)
        .format("%Y-%m-%d %H:%M:%S")
        .to_string();
    assert!(
        row.contains(&format!("{local} (3h ago)")),
        "the row renders the local stamp paired with its age: {row}"
    );
    assert!(
        !row.contains("+00:00"),
        "no machine offset marker survives in the human row: {row}"
    );
    // Compared spelling-on-spelling, never via a run-instant offset read: a
    // DST zone (Atlantic/Azores) reads +00 at the fixture instant, and the
    // run instant reads -01 only inside the winter standard-time window
    // (each year, roughly the last Sunday of October to the last Sunday of
    // March); that read would fire the guard over two identical spellings.
    let utc = chrono::DateTime::from_timestamp(1_782_000_000, 0)
        .unwrap()
        .format("%Y-%m-%d %H:%M:%S")
        .to_string();
    if local != utc {
        assert!(
            !row.contains(&utc),
            "the stamp renders UTC digits, not local wall clock: {row}"
        );
    }
}

/// The age half of the pairing: a zero or negative age renders `· just now`,
/// never `now ago` — the shared formatter spells ≤0 `just now`, so the generic
/// ` ago` pairing would stutter over a sub-second-fresh file or a future mtime,
/// both routine rows. A positive age keeps the one-unit `· 3h ago` pairing.
#[test]
fn the_stamp_age_renders_just_now_for_non_positive_ages_and_ago_for_positive() {
    let sb = HomeSandbox::new();
    let a = sb.home().join(".claude/projects/-w-a/aaaa-1111.jsonl");
    write_jsonl(&a, &[user_line("aaaa-1111", "/ws/a", "hello")]);
    let updated = SystemTime::UNIX_EPOCH + Duration::from_secs(1_782_000_000);
    set_mtime(&a, updated);

    let groups = build_listing(false);
    let session = &groups[0].sessions[0];

    // Zero age: `now` sits exactly on the mtime, the same whole-second a
    // sub-second-fresh file truncates to.
    let zero = session_row(session, false, updated, Columns::FLOOR);
    assert!(
        zero.contains("(just now)"),
        "zero age renders `just now`: {zero}"
    );
    assert!(!zero.contains("now ago"), "zero age never stutters: {zero}");

    // Negative age: a future mtime (the clock behind the file) is the same
    // ≤0 branch, not `now ago`.
    let behind = updated - Duration::from_secs(60);
    let future = session_row(session, false, behind, Columns::FLOOR);
    assert!(
        future.contains("(just now)"),
        "a future mtime renders `just now`: {future}"
    );
    assert!(
        !future.contains("now ago"),
        "a future mtime never stutters: {future}"
    );

    // Positive age: the full one-unit pairing survives the ≤0 branch.
    let ahead = updated + Duration::from_secs(11_520);
    let positive = session_row(session, false, ahead, Columns::FLOOR);
    assert!(
        positive.contains("(3h ago)"),
        "a positive age keeps the one-unit pairing: {positive}"
    );
}

/// The stamp-paired cell drops the age half once the age would be a date (30
/// days and past): the full local stamp already carries the date, so a second
/// copy reads as a duplicate. Younger rows keep the `stamp · age` pairing.
#[test]
fn the_stamp_cell_drops_the_age_half_once_it_would_be_a_date() {
    let sb = HomeSandbox::new();
    let a = sb.home().join(".claude/projects/-w-a/aaaa-1111.jsonl");
    write_jsonl(&a, &[user_line("aaaa-1111", "/ws/a", "hello")]);
    let updated = SystemTime::UNIX_EPOCH + Duration::from_secs(1_782_000_000);
    set_mtime(&a, updated);

    let groups = build_listing(false);
    let session = &groups[0].sessions[0];
    let local = chrono::DateTime::from_timestamp(1_782_000_000, 0)
        .unwrap()
        .with_timezone(&chrono::Local)
        .format("%Y-%m-%d %H:%M:%S")
        .to_string();

    // 29 days: the one-unit age pairs with the stamp (`4w ago`, never `4w 1d`).
    let twenty_nine = session_row(
        session,
        false,
        updated + Duration::from_secs(29 * 86_400),
        Columns::FLOOR,
    );
    assert!(
        twenty_nine.contains(&format!("{local} (4w ago)")),
        "29 days keeps the pairing: {twenty_nine}"
    );

    // 30 days: the age would repeat the stamp's own date, so the cell prints the
    // stamp alone — no `( )` age wrap, no date duplicate.
    let thirty = session_row(
        session,
        false,
        updated + Duration::from_secs(30 * 86_400),
        Columns::FLOOR,
    );
    assert!(thirty.contains(&local), "the stamp survives: {thirty}");
    assert!(
        !thirty.contains(" ("),
        "the age half drops once it would be a date: {thirty}"
    );
}

/// A mixed listing — one 40-day (stamp-alone) session, one 3-hour session —
/// keeps the stamp-alone row's token, cost and preview columns in the same
/// cells as the `stamp (age)` row's: the age column and `( )` wrap stay
/// reserved even though the old row's age half is empty, so whole-row equality
/// reds on a reservation that drops any of that width.
#[test]
fn the_stamp_alone_row_keeps_its_columns_aligned_with_its_age_sibling() {
    let sb = HomeSandbox::new();
    let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_000);
    for (id, text, age) in [
        ("aaaa-1111", "alpha", 40 * 86_400),
        ("bbbb-2222", "beta", 3 * 3600),
    ] {
        let path = sb.home().join(format!(".claude/projects/-w-a/{id}.jsonl"));
        write_jsonl(&path, &[user_line(id, "/ws/a", text)]);
        set_mtime(&path, now - Duration::from_secs(age as u64));
    }
    let mut groups = build_listing(false);
    for s in &mut groups[0].sessions {
        (s.tokens, s.cost) = if s.id == "aaaa-1111" {
            (Some(150), Some(1.05))
        } else {
            (Some(12_345_678_901), Some(12_345.67))
        };
    }

    // Columns measured over both rows: id 9 (both stems are non-uuid), profile
    // 12, age 6 (`3h ago`), tokens 11, cost 9. The young row's usage fills both
    // figure widths exactly, so it carries no left pad.
    let lines = table_lines(&groups, true, now);
    let young_stamp = chrono::DateTime::from_timestamp(1_800_000_000 - 3 * 3600, 0)
        .unwrap()
        .with_timezone(&chrono::Local)
        .format("%Y-%m-%d %H:%M:%S")
        .to_string();
    let old_stamp = chrono::DateTime::from_timestamp(1_800_000_000 - 40 * 86_400, 0)
        .unwrap()
        .with_timezone(&chrono::Local)
        .format("%Y-%m-%d %H:%M:%S")
        .to_string();
    let expected_young = format!(
        "  bbbb-2222  {prof}  {stamp} (3h ago)  12345678901  $12345.67  beta | beta",
        prof = "-".to_owned() + &" ".repeat(11),
        stamp = young_stamp,
    );
    // The stamp-alone cell reserves the `( )` wrap (3) + the age column (6), so
    // the old row's 3-digit token total left-pads 8 into the same 11-cell column.
    let expected_old = format!(
        "  aaaa-1111  {prof}  {stamp}{reserve}  {tok}150  {cost}$1.05  alpha | alpha",
        prof = "-".to_owned() + &" ".repeat(11),
        stamp = old_stamp,
        reserve = " ".repeat(3 + 6),
        tok = " ".repeat(8),
        cost = " ".repeat(4),
    );
    assert_eq!(
        lines,
        vec!["/ws/a".to_string(), expected_young, expected_old],
        "the stamp-alone row keeps the age sibling's column starts"
    );
}

/// A listing whose every session is ≥30 days prints the stamp alone with no
/// reserved age column: the preview follows the stamp after the normal column
/// gap, never `3 + age` blank cells when no row carries an age half.
#[test]
fn an_all_stamp_alone_listing_reserves_no_age_column() {
    let sb = HomeSandbox::new();
    let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_000);
    for (id, text, age) in [
        ("aaaa-1111", "alpha", 40 * 86_400),
        ("bbbb-2222", "beta", 31 * 86_400),
    ] {
        let path = sb.home().join(format!(".claude/projects/-w-a/{id}.jsonl"));
        write_jsonl(&path, &[user_line(id, "/ws/a", text)]);
        set_mtime(&path, now - Duration::from_secs(age as u64));
    }
    let groups = build_listing(false);
    let lines = table_lines(&groups, false, now);
    let beta_stamp = chrono::DateTime::from_timestamp(1_800_000_000 - 31 * 86_400, 0)
        .unwrap()
        .with_timezone(&chrono::Local)
        .format("%Y-%m-%d %H:%M:%S")
        .to_string();
    let alpha_stamp = chrono::DateTime::from_timestamp(1_800_000_000 - 40 * 86_400, 0)
        .unwrap()
        .with_timezone(&chrono::Local)
        .format("%Y-%m-%d %H:%M:%S")
        .to_string();
    // Newest first: the 31-day session leads. Each row is the stamp plus the
    // normal two-space column gap before the preview — no reserved age column.
    let expected_beta = format!(
        "  bbbb-2222  {prof}  {stamp}  beta | beta",
        prof = "-".to_owned() + &" ".repeat(11),
        stamp = beta_stamp,
    );
    let expected_alpha = format!(
        "  aaaa-1111  {prof}  {stamp}  alpha | alpha",
        prof = "-".to_owned() + &" ".repeat(11),
        stamp = alpha_stamp,
    );
    assert_eq!(
        lines,
        vec!["/ws/a".to_string(), expected_beta, expected_alpha],
        "an all-stamp-alone listing keeps only the normal column gap"
    );
}

/// The human row and the JSON `updated` field disagree on shape BY DESIGN
/// (owner ruling 2026-08-22: `--json` keeps the documented ISO-8601 UTC).
/// Pinned BOTH directions — the ISO spelling must not reach the human row,
/// the local spelling must not reach the JSON field — so an edit that
/// "aligns" either surface reds this test.
#[test]
fn the_human_row_and_json_disagree_on_the_stamp_shape_by_design() {
    let sb = HomeSandbox::new();
    let a = sb.home().join(".claude/projects/-w-a/aaaa-1111.jsonl");
    write_jsonl(&a, &[user_line("aaaa-1111", "/ws/a", "hello")]);
    let updated = SystemTime::UNIX_EPOCH + Duration::from_secs(1_782_000_000);
    let now = updated + Duration::from_secs(11_520);
    set_mtime(&a, updated);

    let groups = build_listing(false);
    let session = &groups[0].sessions[0];
    let human = session_row(session, false, now, Columns::FLOOR);
    let json_updated = session_json_row(session)["updated"]
        .as_str()
        .expect("updated is a string")
        .to_string();

    assert!(
        json_updated.contains('T') && json_updated.ends_with("+00:00"),
        "the json field keeps the documented ISO-8601 UTC shape: {json_updated}"
    );
    let local = chrono::DateTime::from_timestamp(1_782_000_000, 0)
        .unwrap()
        .with_timezone(&chrono::Local)
        .format("%Y-%m-%d %H:%M:%S")
        .to_string();
    assert!(
        human.contains(&format!("{local} (3h ago)")),
        "the human row renders the local stamp and its age: {human}"
    );
    assert!(
        !human.contains(&json_updated),
        "the ISO spelling must not reach the human row: {human}"
    );
    assert!(
        !json_updated.contains(&local),
        "the local spelling must not reach the json field: {json_updated}"
    );
}

// ── exit-code contract (0 / 1 / 2) ──

#[test]
fn no_sessions_found_maps_to_exit_one() {
    let _sb = HomeSandbox::new(); // empty tree ⇒ empty index
    let err = run_sessions(true, false, false).expect_err("empty index must error");
    assert!(
        err.to_string().contains("no sessions"),
        "error must say no sessions were found: {err}"
    );
    assert!(
        err.downcast_ref::<crate::UsageError>().is_none(),
        "an empty index is a runtime error, not a usage error"
    );
    assert_eq!(crate::exit_code(Err(err)), 1);
}

#[test]
fn sessions_bad_flag_maps_to_exit_two() {
    // Through the real grammar: an unknown `sessions` flag never reaches
    // dispatch, and clap's own parse-failure code is the same 2 the
    // sessions-surface `UsageError` maps to, so the contract holds either way.
    use clap::Parser as _;
    let err = crate::cli::Cli::try_parse_from(["clauth", "sessions", "--bogus"])
        .expect_err("bad flag must error");
    assert_eq!(err.exit_code(), 2);
}

#[test]
fn success_maps_to_exit_zero() {
    assert_eq!(crate::exit_code(Ok(())), 0);
}

// ── resume_profile_choice: the four branches ──

#[test]
fn resume_profile_choice_explicit_flag_forces_no_prompt() {
    // A flag wins regardless of tty or a known last-ran profile.
    assert_eq!(
        resume_profile_choice(Some("chosen"), true, Some("lastran"), "active"),
        ("chosen".to_string(), false)
    );
    assert_eq!(
        resume_profile_choice(Some("chosen"), false, None, "active"),
        ("chosen".to_string(), false)
    );
}

#[test]
fn resume_profile_choice_piped_no_flag_uses_active_forced() {
    assert_eq!(
        resume_profile_choice(None, false, Some("lastran"), "active"),
        ("active".to_string(), false)
    );
}

#[test]
fn resume_profile_choice_tty_known_last_ran_prompts_defaulting_to_it() {
    // Mutation target: if this branch returned `active`, this test fails.
    assert_eq!(
        resume_profile_choice(None, true, Some("lastran"), "active"),
        ("lastran".to_string(), true)
    );
}

#[test]
fn resume_profile_choice_tty_unknown_prompts_defaulting_to_active() {
    assert_eq!(
        resume_profile_choice(None, true, None, "active"),
        ("active".to_string(), true)
    );
}

// ── resolve_session: the targeted lookup behind resume/info ──

/// `resume latest` and the emitted listing must name the same session whenever
/// the listing's first row is one a resume can open. They read the store two
/// different ways now — a filename-and-mtime walk vs the full index — so the
/// agreement is a real invariant, not a tautology. The one exception is pinned by
/// `newest_session_skips_the_nested_transcripts_a_resume_cannot_open`.
#[test]
fn latest_resolves_to_the_first_row_the_listing_emits() {
    let sb = HomeSandbox::new();
    let older = sb.home().join(".claude/projects/-w-a/s-older.jsonl");
    let newer = sb.home().join(".claude/projects/-w-b/s-newer.jsonl");
    write_jsonl(&older, &[user_line("s-older", "/ws/a", "older")]);
    write_jsonl(&newer, &[user_line("s-newer", "/ws/b", "newer")]);
    set_mtime(&older, SystemTime::UNIX_EPOCH + Duration::from_secs(1_000));
    set_mtime(&newer, SystemTime::UNIX_EPOCH + Duration::from_secs(2_000));

    let groups = build_listing(false);
    let flat = flatten_newest_first(&groups);
    let Resolved::Ready(latest) = resolve_session("latest") else {
        panic!("`latest` must resolve against a store with no live isolated run");
    };
    assert_eq!(
        Some(latest.id.as_str()),
        flat.first().map(|s| s.id.as_str()),
        "`latest` must be the session `clauth sessions` lists first"
    );
    assert_eq!(latest.id, "s-newer");
}

/// Seed a live isolated runtime for `profile` holding one transcript, and return
/// its lock file — the runtime reads as live only while that stays held.
fn live_isolated_session(
    sb: &HomeSandbox,
    profile: &str,
    id: &str,
    workspace: &str,
    mtime_secs: u64,
) -> std::fs::File {
    let iso = sb.home().join(format!(
        ".clauth/profiles/{profile}/runtime-isolated/projects/-w-iso/{id}.jsonl"
    ));
    write_jsonl(&iso, &[user_line(id, workspace, "hi iso")]);
    set_mtime(
        &iso,
        SystemTime::UNIX_EPOCH + Duration::from_secs(mtime_secs),
    );
    let sessions_dir = sb
        .home()
        .join(format!(".clauth/profiles/{profile}/sessions-isolated"));
    fs::create_dir_all(&sessions_dir).unwrap();
    let lock_file = crate::runtime::open_pid_file(&sessions_dir.join("12345")).unwrap();
    lock_file.lock().unwrap();
    lock_file
}

/// An id that only exists in a live isolated runtime is not resumable: the spawn
/// is `Isolation::Shared`, so Claude Code would look in the shared store and
/// answer `No conversation found`. Refusing it is right; calling it missing is
/// not, when `clauth sessions` lists it two lines up.
#[test]
fn resume_refuses_an_isolated_held_session_without_calling_it_missing() {
    let sb = HomeSandbox::new();
    let ws = sb.home().join("workspace");
    fs::create_dir_all(&ws).unwrap();
    let lock_file = live_isolated_session(&sb, "iso", "siso", &ws.to_string_lossy(), 5_000);

    // The listing still browses it — only the resume path refuses.
    assert!(
        flatten_newest_first(&build_listing(false))
            .iter()
            .any(|s| s.id == "siso"),
        "the listing covers a live isolated store"
    );

    let err = run_resume("siso", None).expect_err("an isolated-held id must be refused");
    drop(lock_file);
    let msg = err.to_string();
    assert!(
        msg.contains("'siso'") && msg.contains("profile 'iso'"),
        "the refusal must name the session and the run holding it: {msg}"
    );
    assert!(
        !msg.contains("no session found"),
        "a listed session is not missing: {msg}"
    );
    assert!(
        msg.contains("it moves into the shared store when that run ends"),
        "and must say what makes it reachable — waiting, with nothing to turn on: {msg}"
    );
    assert!(
        !msg.contains("rescue") && !msg.contains("auto_rescue"),
        "the rescue is unconditional, so the refusal names no switch: {msg}"
    );
    assert_eq!(crate::exit_code(Err(err)), 1);
}

/// `latest` means the newest session. When a live isolated run holds that one,
/// silently resolving to the second newest would spend an account window on a
/// conversation the operator never named — and `clauth sessions` would still be
/// listing the one they meant, first.
#[test]
fn latest_refuses_rather_than_substituting_when_an_isolated_run_holds_the_newest() {
    let sb = HomeSandbox::new();
    let ws = sb.home().join("workspace");
    fs::create_dir_all(&ws).unwrap();
    let global = sb.home().join(".claude/projects/-w-g/sglobal.jsonl");
    write_jsonl(
        &global,
        &[user_line("sglobal", &ws.to_string_lossy(), "hi")],
    );
    set_mtime(&global, SystemTime::UNIX_EPOCH + Duration::from_secs(1_000));

    // Isolated transcript NEWER than the reachable one.
    let lock_file = live_isolated_session(&sb, "iso", "siso", &ws.to_string_lossy(), 5_000);
    let err = run_resume("latest", None).expect_err("a shadowed `latest` must be refused");
    let msg = err.to_string();
    assert!(
        msg.contains("'siso'") && msg.contains("profile 'iso'"),
        "the refusal names the session that shadowed `latest`: {msg}"
    );
    assert!(
        !msg.contains("sglobal"),
        "and never quietly offers the second newest: {msg}"
    );

    // Same fixture, isolated transcript OLDER: `latest` is the reachable one and
    // resolves normally. Proves the refusal is the shadowing, not the presence
    // of any live isolated run at all.
    set_mtime(
        &sb.home()
            .join(".clauth/profiles/iso/runtime-isolated/projects/-w-iso/siso.jsonl"),
        SystemTime::UNIX_EPOCH + Duration::from_secs(500),
    );
    let resolved = resolve_session("latest");
    drop(lock_file);
    assert!(
        matches!(resolved, Resolved::Ready(ref s) if s.id == "sglobal"),
        "an older isolated transcript shadows nothing"
    );
}

/// A workflow journal is a run log the listing never shows, so naming one an
/// isolated run holds answers "no session found", never "held"; the agent
/// transcript beside it stays a held session.
#[test]
fn an_isolated_workflow_journal_is_no_held_session() {
    let sb = HomeSandbox::new();
    let ws = sb.home().join("workspace");
    fs::create_dir_all(&ws).unwrap();
    let workflow = sb.home().join(
        ".clauth/profiles/iso/runtime-isolated/projects/-w-iso/siso/subagents/workflows/wf_y",
    );
    write_jsonl(
        &workflow.join("journal.jsonl"),
        &[json!({"agentId": "agent-e5f6a7b8", "key": "y", "type": "started"}).to_string()],
    );
    write_jsonl(
        &workflow.join("agent-e5f6a7b8.jsonl"),
        &[user_line("siso", &ws.to_string_lossy(), "subagent")],
    );
    let sessions_dir = sb.home().join(".clauth/profiles/iso/sessions-isolated");
    fs::create_dir_all(&sessions_dir).unwrap();
    let lock_file = crate::runtime::open_pid_file(&sessions_dir.join("12345")).unwrap();
    lock_file.lock().unwrap(); // held so the runtime reads as live

    let journal = resolve_session("journal");
    let agent = resolve_session("agent-e5f6a7b8");
    drop(lock_file);

    assert!(
        matches!(journal, Resolved::Missing),
        "a workflow journal an isolated run holds is no session"
    );
    assert!(
        matches!(agent, Resolved::Held(ref h) if h.session.id == "agent-e5f6a7b8"),
        "the agent transcript beside it is still reported held"
    );
}

/// The shadowing check ranges over the same kind of file `latest` itself does.
/// A nested transcript in a live isolated store can be newer than everything and
/// is still never anybody's `latest`, so refusing on one would strand the resume
/// waiting for a rescue that changes nothing. Named exactly, it is still held —
/// the listing shows it, so "no session found" would be false.
#[test]
fn a_nested_isolated_transcript_does_not_shadow_latest() {
    let sb = HomeSandbox::new();
    let ws = sb.home().join("workspace");
    fs::create_dir_all(&ws).unwrap();
    let global = sb.home().join(".claude/projects/-w-g/sglobal.jsonl");
    write_jsonl(
        &global,
        &[user_line("sglobal", &ws.to_string_lossy(), "hi")],
    );
    set_mtime(&global, SystemTime::UNIX_EPOCH + Duration::from_secs(1_000));

    let iso = sb
        .home()
        .join(".clauth/profiles/iso/runtime-isolated/projects/-w-iso");
    let nested = iso.join("siso/subagents/agent-abc.jsonl");
    write_jsonl(
        &nested,
        &[user_line("siso", &ws.to_string_lossy(), "nested")],
    );
    set_mtime(&nested, SystemTime::UNIX_EPOCH + Duration::from_secs(9_000));
    let sessions_dir = sb.home().join(".clauth/profiles/iso/sessions-isolated");
    fs::create_dir_all(&sessions_dir).unwrap();
    let lock_file = crate::runtime::open_pid_file(&sessions_dir.join("12345")).unwrap();
    lock_file.lock().unwrap(); // held so the runtime reads as live

    let resolved = resolve_session("latest");
    let by_name = resolve_session("agent-abc");
    // Control: the same store's TOP-LEVEL transcript at that mtime does shadow,
    // so the pass above is the nesting and not an isolated run reading as dead.
    let top = iso.join("siso.jsonl");
    write_jsonl(&top, &[user_line("siso", &ws.to_string_lossy(), "top")]);
    set_mtime(&top, SystemTime::UNIX_EPOCH + Duration::from_secs(9_000));
    let shadowed = resolve_session("latest");
    drop(lock_file);

    assert!(
        matches!(resolved, Resolved::Ready(ref s) if s.id == "sglobal"),
        "a nested isolated transcript shadows nothing"
    );
    assert!(
        matches!(by_name, Resolved::Held(ref h) if h.session.id == "agent-abc"),
        "but naming it exactly still reports the run holding it"
    );
    assert!(
        matches!(shadowed, Resolved::Held(_)),
        "the same store's top-level transcript still shadows"
    );
}

// ── clauth info ──

#[test]
fn info_prints_the_resume_command_workspace_and_storage() {
    let sb = HomeSandbox::new();
    // Componentwise: `storage:` prints the index walk's own `DirEntry::path`,
    // which is natively separated. A one-shot `join(".claude/projects/…")`
    // stores its `/` verbatim and mismatches that spelling on windows.
    let path = sb
        .home()
        .join(".claude")
        .join("projects")
        .join("-w")
        .join("known-session.jsonl");
    write_jsonl(&path, &[user_line("known-session", "/ws/a", "hi")]);

    let Resolved::Ready(session) = resolve_session("known-session") else {
        panic!("the id must resolve");
    };
    assert_eq!(
        info_lines(&session, None),
        format!(
            "resume:    clauth resume known-session\nworkspace: /ws/a\nstorage:   {}",
            path.display()
        )
    );
}

/// A held session is reportable where it is not resumable: `info` launches
/// nothing, and its storage path is the only place any surface says where the
/// transcript actually lives. What it must NOT print is a resume command that
/// Claude Code would refuse.
#[test]
fn info_reports_a_held_session_without_offering_a_resume_command() {
    let sb = HomeSandbox::new();
    let lock_file = live_isolated_session(&sb, "iso", "siso", "/w/iso", 5_000);
    let resolved = resolve_session("siso");
    drop(lock_file);

    let Resolved::Held(hold) = resolved else {
        panic!("a live isolated run must resolve as held");
    };
    let lines = info_lines(&hold.session, Some(&hold.profile));
    assert!(
        !lines.contains("clauth resume"),
        "no resume command for a session a resume can't reach: {lines}"
    );
    assert!(
        lines.contains("under 'iso'"),
        "the holding profile is named: {lines}"
    );
    assert!(
        lines.contains("workspace: /w/iso"),
        "the recorded workspace still comes from the transcript: {lines}"
    );
    assert!(
        lines.contains("runtime-isolated"),
        "and the storage path points into the isolated store: {lines}"
    );
}

// ── resume <unknown id> ──

#[test]
fn resume_unknown_id_errors_naming_it_at_exit_one() {
    let sb = HomeSandbox::new();
    // A real session so the index isn't empty — the error must be "unknown id",
    // not "no sessions".
    let path = sb.home().join(".claude/projects/-w/known-session.jsonl");
    write_jsonl(&path, &[user_line("known-session", "/ws", "hi")]);

    let err = run_resume("ghost-id", None).expect_err("unknown id must error");
    assert!(
        err.to_string().contains("ghost-id"),
        "the error must name the unknown id: {err}"
    );
    assert!(
        err.downcast_ref::<crate::UsageError>().is_none(),
        "an unknown id is a runtime error, not a usage error"
    );
    assert_eq!(crate::exit_code(Err(err)), 1);
}

// ── resume refuses a disabled target ─────────────────────────────────────
//
// `run_resume` never spawns directly — it always funnels through
// `crate::start::run`, whose first line is the authoritative
// "never a live session for a disabled account" gate (mirrors
// `cli.rs::disabled_target_refusal`'s `cmd_start` regression test).

/// Seed `enabled` as plain profiles and `disabled` as disabled ones, all
/// under one `AppConfig` so each `create_blank_profile` call's
/// `save_app_state` persists the growing name list instead of a fresh call
/// clobbering an earlier profile's entry.
fn seed_profiles(enabled: &[&str], disabled: &[&str]) {
    let mut config = crate::profile::AppConfig {
        state: crate::profile::AppState::default(),
        profiles: Vec::new(),
    };
    for name in enabled.iter().chain(disabled.iter()) {
        crate::actions::create_blank_profile(
            &mut config,
            (*name).to_string(),
            None,
            None,
            Default::default(),
        )
        .expect("create profile");
    }
    for name in disabled {
        crate::actions::disable_profile(&mut config, &crate::profile::ProfileName::from(*name))
            .expect("disable profile");
    }
}

#[test]
fn resume_refuses_an_explicit_disabled_profile_before_any_spawn() {
    let sb = HomeSandbox::new();
    seed_profiles(&[], &["off"]);

    let ws = sb.home().join("workspace");
    fs::create_dir_all(&ws).unwrap();
    let ws_str = ws.to_string_lossy().into_owned();
    let path = sb.home().join(".claude/projects/-w/known-session.jsonl");
    write_jsonl(&path, &[user_line("known-session", &ws_str, "hi")]);

    let err =
        run_resume("known-session", Some("off")).expect_err("a disabled target must be refused");
    assert_eq!(err.to_string(), "'off': account is disabled");
    assert!(
        !sb.home().join(".clauth/profiles/off/runtime").exists(),
        "the refusal must happen before any runtime is acquired"
    );
}

// ── resume_candidates: the interactive picker never offers a disabled account ──

#[test]
fn resume_candidates_excludes_disabled_accounts() {
    let _sb = HomeSandbox::new();
    seed_profiles(&["acme"], &["off"]);
    let config = crate::profile::load_config().expect("reload");

    let (enabled, _) = resume_candidates(&config, "acme");

    assert_eq!(
        enabled,
        vec!["acme"],
        "a disabled account must never be an offered candidate"
    );
}

#[test]
fn resume_candidates_falls_back_when_the_default_is_disabled() {
    let _sb = HomeSandbox::new();
    seed_profiles(&["acme"], &["off"]);
    let config = crate::profile::load_config().expect("reload");

    // A stale last-ran profile that's since been disabled must not be shown
    // as the bracketed default for a name that isn't even in the list.
    let (enabled, default) = resume_candidates(&config, "off");

    assert_eq!(enabled, vec!["acme"]);
    assert_eq!(
        default, "acme",
        "a disabled default must fall back to an enabled name"
    );
}

// ── clauth switch <sid> <profile> ───────────────────────────────────────────
//
// The verb expresses INTENT only: it moves the registry row's intended member
// through the same `live_sessions::update_as_daemon` seam the daemon's
// decision leg writes, and the session's own executor stays the safety gate.

/// File a registry row for a session `sid` launched on `profile` WITHOUT
/// holding its liveness marker — dead until a marker is held on top, which is
/// exactly what [`live_claude_row`] adds.
fn registry_row(sid: &str, profile: &str, harness: crate::harness::Harness) {
    let row = crate::live_sessions::LiveSession {
        session_id: sid.to_string(),
        start_profile: profile.to_string(),
        harness,
        pid: 4242,
        claude_pid: None,
        launch_families: None,
        started_at: 0,
        cwd: None,
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
    };
    crate::live_sessions::register(&row).expect("register row");
}

/// A live claude session's row: registered plus its liveness marker held, the
/// state a running `clauth start` keeps.
fn live_claude_row(sid: &str, profile: &str) -> std::fs::File {
    registry_row(sid, profile, crate::harness::Harness::Claude);
    crate::runtime::hold_session_row_marker(&crate::profile::ProfileName::from(profile), false, sid)
        .expect("hold the session's marker")
}

/// A configured api-key profile, the member shape the swap executor's own
/// grounds refuse (`NotOauth`) for a session launched on an OAuth account.
fn add_keyed_profile(name: &str) {
    let mut config = crate::profile::load_config().expect("load config");
    crate::actions::create_blank_profile(
        &mut config,
        name.to_string(),
        Some("https://api.example.internal".to_string()),
        Some("sk-test-not-a-real-key".to_string()),
        Default::default(),
    )
    .expect("create keyed profile");
}

#[test]
fn switch_refuses_an_unknown_sid_with_the_listing_hint() {
    let _sb = HomeSandbox::new();

    // A transcript-shaped id and a sid-shaped one: neither names a live
    // session, and both get the same named refusal.
    for sid in ["nope", "999999-9"] {
        let err = run_switch(sid, "work").expect_err("an unknown sid must be refused");
        assert_eq!(
            err.to_string(),
            format!("no live session '{sid}'\nsee `clauth sessions --live`"),
            "the refusal names the sid and the listing verb"
        );
        assert!(
            err.downcast_ref::<crate::UsageError>().is_none(),
            "an unknown sid is a runtime error, not a usage error"
        );
        assert_eq!(crate::exit_code(Err(err)), 1);
    }
}

/// The same name can be a configured profile: arity alone chose the session
/// form, so a sid that misses while the name resolves to a profile points at
/// the spelling that switches the global account instead of leaving the
/// operator guessing.
#[test]
fn switch_refuses_an_unknown_sid_that_is_a_profile_with_the_global_hint() {
    let _sb = HomeSandbox::new();
    seed_profiles(&["work"], &[]);

    let err = run_switch("work", "spare").expect_err("a profile name is not a live sid");
    assert_eq!(
        err.to_string(),
        "no live session 'work'\nsee `clauth sessions --live`\nto switch the global account: `clauth switch work`"
    );
    assert_eq!(crate::exit_code(Err(err)), 1);
}

/// The hint resolves against BOTH rosters, exactly as `cmd_switch` does: a
/// codex-only name is a working `clauth switch <name>` target too (it moves
/// the codex active marker), so the refusal must name the fix there as well.
#[test]
fn switch_refuses_an_unknown_sid_that_is_a_codex_profile_with_the_global_hint() {
    let _sb = HomeSandbox::new();
    crate::codex_profiles::CodexState::update(|state| state.add_profile("cx"))
        .expect("seed codex roster");

    let err = run_switch("cx", "spare").expect_err("a codex name is not a live sid");
    assert_eq!(
        err.to_string(),
        "no live session 'cx'\nsee `clauth sessions --live`\nto switch the global account: `clauth switch cx`"
    );
    assert_eq!(crate::exit_code(Err(err)), 1);
}

#[test]
fn switch_refuses_a_dead_session_row() {
    let _sb = HomeSandbox::new();
    // Registered but marker-less: the session ended, its row awaits GC.
    registry_row("4242-0", "work", crate::harness::Harness::Claude);

    let err = run_switch("4242-0", "work").expect_err("a dead row must be refused");
    assert_eq!(
        err.to_string(),
        "session '4242-0' is no longer running\nits row is reaped by the next `clauth daemon` or `clauth resume`"
    );
    assert!(
        err.downcast_ref::<crate::UsageError>().is_none(),
        "a dead row is a runtime error, not a usage error"
    );
    assert_eq!(crate::exit_code(Err(err)), 1);
}

/// A background session is fixed-account (cloudy's T33b1 ruling): both its
/// rows, the daemon's hold and a launcher's, refuse with the copy cloudy
/// approved 2026-10-22, and no intent is written for the executor to refuse.
#[test]
fn switch_refuses_a_background_session_row() {
    for hold in [true, false] {
        let _sb = HomeSandbox::new();
        let row = crate::live_sessions::LiveSession {
            session_id: "4242-0".to_string(),
            start_profile: "work".to_string(),
            harness: crate::harness::Harness::Claude,
            pid: 4242,
            claude_pid: None,
            launch_families: None,
            started_at: 0,
            cwd: None,
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
            background: true,
            background_hold: hold,
            visited_members: vec![],
        };
        crate::live_sessions::register(&row).expect("register row");
        let sessions = crate::profile::profile_dir(&crate::profile::ProfileName::from("work"))
            .expect("profile dir")
            .join("sessions-bg");
        std::fs::create_dir_all(&sessions).expect("sessions-bg");
        let marker = crate::runtime::open_pid_file(&sessions.join("4242-0")).expect("marker");
        marker.lock().expect("hold the marker");

        let err = run_switch("4242-0", "spare").expect_err("a background row must be refused");
        assert_eq!(
            err.to_string(),
            "session '4242-0' is a background session and cannot be switched from 'work'",
            "hold: {hold}"
        );
        assert_eq!(crate::exit_code(Err(err)), 1);
        assert!(
            crate::live_sessions::get("4242-0")
                .expect("row")
                .intended_member
                .is_none(),
            "hold: {hold}: a refused swap writes nothing"
        );
    }
}

#[test]
fn switch_refuses_a_codex_session_row() {
    let _sb = HomeSandbox::new();
    registry_row("4242-0", "work", crate::harness::Harness::Codex);
    // Marker held, so this pins the codex arm itself, not the liveness one.
    let _marker = crate::runtime::hold_session_row_marker(
        &crate::profile::ProfileName::from("work"),
        false,
        "4242-0",
    )
    .expect("hold the marker");

    let err = run_switch("4242-0", "spare").expect_err("a codex row must be refused");
    assert_eq!(
        err.to_string(),
        "session '4242-0' is a codex session; switch is claude-only"
    );
    assert_eq!(crate::exit_code(Err(err)), 1);
    // The intent must not have landed either: a codex session has no executor,
    // so a written intent would stand as a silent no-op forever.
    assert!(
        crate::live_sessions::get("4242-0")
            .expect("row")
            .intended_member
            .is_none(),
        "a refused swap writes nothing"
    );
}

#[test]
fn switch_refuses_an_unknown_profile() {
    let _sb = HomeSandbox::new();
    seed_profiles(&["work"], &[]);
    let _marker = live_claude_row("4242-0", "work");

    let err = run_switch("4242-0", "nope").expect_err("an unknown profile must be refused");
    assert_eq!(err.to_string(), "profile 'nope' not found\navailable: work");
    assert_eq!(crate::exit_code(Err(err)), 1);
    assert!(
        crate::live_sessions::get("4242-0")
            .expect("row")
            .intended_member
            .is_none(),
        "a refused swap writes nothing"
    );
}

#[test]
fn switch_points_the_row_at_the_named_profile_through_the_registry_seam() {
    let _sb = HomeSandbox::new();
    seed_profiles(&["work", "spare"], &[]);
    let _marker = live_claude_row("4242-0", "work");

    run_switch("4242-0", "spare").expect("the switch records the intent");

    let row = crate::live_sessions::get("4242-0").expect("row survives the write");
    assert_eq!(
        row.intended_member.as_deref(),
        Some("spare"),
        "the registry row's intended member moved"
    );
    assert_eq!(
        row.current_member, None,
        "the CLI installs nothing: current_member is the executor's to write"
    );
    assert_eq!(row.last_swap_at, None, "no swap is recorded as executed");
    assert_eq!(
        row.chain_cursor, None,
        "the cursor stays the daemon decision leg's field"
    );
}

/// A manual move is never a credit-cap leave: it clears a stale daemon reason,
/// so the executor keeps the models-equal rule whatever the last daemon write
/// recorded.
#[test]
fn switch_clears_a_stale_credit_leave_reason() {
    let _sb = HomeSandbox::new();
    seed_profiles(&["work", "spare"], &[]);
    let _marker = live_claude_row("4242-0", "work");
    crate::live_sessions::update_as_daemon("4242-0", |d| d.set_credit_leave(true))
        .expect("seed the daemon's credit-leave reason");
    assert!(
        crate::live_sessions::get("4242-0")
            .expect("row")
            .credit_leave,
        "fixture control: the row carries the reason"
    );

    run_switch("4242-0", "spare").expect("the switch records the intent");

    let row = crate::live_sessions::get("4242-0").expect("row");
    assert_eq!(row.intended_member.as_deref(), Some("spare"));
    assert!(
        !row.credit_leave,
        "a manual switch is never a credit leave, so the models-equal rule stands"
    );
}

#[test]
fn switch_receipt_names_when_the_move_lands() {
    assert_eq!(
        switch_receipt("4242-0", "spare"),
        "clauth: pointed session '4242-0' at 'spare'\nthe switch lands at the session's next \
         request, never before it. A refused move is logged and the session stays put"
    );
}

#[test]
fn switch_is_a_no_op_for_a_session_already_on_the_profile() {
    let _sb = HomeSandbox::new();
    seed_profiles(&["work"], &[]);
    let _marker = live_claude_row("4242-0", "work");

    run_switch("4242-0", "work").expect("already-on is a satisfied ask, not an error");

    assert_eq!(
        crate::live_sessions::get("4242-0")
            .expect("row")
            .intended_member,
        None,
        "no intent is written for a session already on the profile: the executor \
         treats an intent equal to the current member as the steady state"
    );
    assert_eq!(
        already_on_line("4242-0", "work"),
        "clauth: session '4242-0' is already on 'work'"
    );
}

/// The executor gate stays the decider: the CLI records an intent even for a
/// member the executor's own config grounds refuse, and nothing executes —
/// the session's executor says no, and no stays no.
#[test]
fn switch_leaves_the_go_no_go_with_the_sessions_executor() {
    let _sb = HomeSandbox::new();
    seed_profiles(&["work"], &[]);
    add_keyed_profile("keyed");
    let launch = crate::profile::load_profile(&crate::profile::ProfileName::from("work"))
        .expect("load work");
    let keyed = crate::profile::load_profile(&crate::profile::ProfileName::from("keyed"))
        .expect("load keyed");
    let _marker = live_claude_row("4242-0", "work");

    run_switch("4242-0", "keyed")
        .expect("the CLI records the intent without pre-judging eligibility");

    let row = crate::live_sessions::get("4242-0").expect("row");
    assert_eq!(
        row.intended_member.as_deref(),
        Some("keyed"),
        "the intent is recorded even for a member the executor will refuse"
    );
    assert_eq!(
        row.current_member, None,
        "nothing executed: this process never swaps"
    );
    assert_eq!(
        crate::runtime::swap_eligible(
            &keyed,
            &crate::runtime::LaunchTransport::of(&launch),
            crate::runtime::ModelsRule::Equal,
        ),
        Err(crate::runtime::SwapRefused::NotOauth),
        "the executor's own grounds still refuse this member, so the intent \
         stays an intent"
    );
}

#[test]
fn the_sessions_listing_rejects_extra_arguments() {
    use clap::Parser as _;

    let bare =
        crate::cli::Cli::try_parse_from(["clauth", "sessions"]).expect("bare sessions parses");
    match bare.command {
        Some(crate::cli::Command::Sessions { .. }) => {}
        other => panic!("bare `clauth sessions` must stay the listing: {other:?}"),
    }

    // The listing's flags belong to the bare form; passing one before an extra
    // argument is a usage error, never a listing that silently ignores it.
    let err = crate::cli::Cli::try_parse_from(["clauth", "sessions", "--json", "extra"])
        .expect_err("extra args must be a usage error");
    assert_eq!(err.exit_code(), 2);
}

// ── clauth sessions --live ──────────────────────────────────────────────────
//
// The live roster reads the registry and Claude Code's per-pid session
// records only; transcripts stay closed.

/// Fixed instant every `--live` fixture is aged against: 2027-01-15T08:00:00Z.
const LIVE_NOW_SECS: u64 = 1_800_000_000;

/// A registry row started `ago_secs` before [`LIVE_NOW_SECS`], dead until a
/// marker is held for it.
fn live_fixture_row(sid: &str, profile: &str, ago_secs: u64) -> crate::live_sessions::LiveSession {
    crate::live_sessions::LiveSession {
        session_id: sid.to_string(),
        start_profile: profile.to_string(),
        harness: crate::harness::Harness::Claude,
        pid: 4242,
        claude_pid: None,
        launch_families: None,
        started_at: (LIVE_NOW_SECS - ago_secs) * 1000,
        cwd: None,
        isolated: false,
        follows_chain: false,
        intended_member: None,
        chain_cursor: None,
        credit_leave: false,
        credit_leave_aware: false,
        current_member: None,
        visited_members: vec![],
        last_swap_at: None,
        launch_store: None,
        boot_transport: None,
        is_delegate: false,
        background: false,
        background_hold: false,
    }
}

/// Register `row` and hold its marker under `marker_profile` (where a running
/// session holds it: its current member once moved, else its start profile).
fn register_live(row: &crate::live_sessions::LiveSession, marker_profile: &str) -> std::fs::File {
    crate::live_sessions::register(row).expect("register row");
    crate::runtime::hold_session_row_marker(
        &crate::profile::ProfileName::from(marker_profile),
        false,
        &row.session_id,
    )
    .expect("hold the session's marker")
}

/// Claude Code's own record for the process `pid`, naming its transcript.
fn seed_cc_record(sb: &HomeSandbox, pid: u32, transcript: &str) {
    let dir = sb.home().join(".claude").join("sessions");
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join(format!("{pid}.json")),
        json!({ "sessionId": transcript, "pid": pid }).to_string(),
    )
    .unwrap();
}

/// Seeds every row shape the roster must tell apart and returns the marker
/// handles that keep the live ones live:
/// - `100-0` moved `cld1 → cld2` (marker under `cld2`), transcript known
/// - `200-0` never moved, no Claude Code record yet, no cwd
/// - `300-1` codex
/// - `400-0` delegate, joined on its re-keyed `pid`
/// - `500-0` background
/// - `600-0` dead (no marker), `700-0` the daemon's hold on the account's fixed
///   background tree, `800-0` torn
fn seed_live_roster(sb: &HomeSandbox) -> Vec<std::fs::File> {
    let mut held = Vec::new();

    let mut moved = live_fixture_row("100-0", "cld1", 7200);
    moved.current_member = Some("cld2".to_string());
    moved.claude_pid = Some(7001);
    moved.cwd = Some(std::path::PathBuf::from("/w/a"));
    held.push(register_live(&moved, "cld2"));
    seed_cc_record(sb, 7001, "aaaaaaaa-1111-4111-8111-aaaaaaaaaaaa");

    let mut fresh = live_fixture_row("200-0", "cld1", 720);
    fresh.claude_pid = Some(7002);
    held.push(register_live(&fresh, "cld1"));

    let mut codex = live_fixture_row("300-1", "cx0", 240);
    codex.harness = crate::harness::Harness::Codex;
    codex.cwd = Some(std::path::PathBuf::from("/w/c"));
    held.push(register_live(&codex, "cx0"));

    let mut delegate = live_fixture_row("400-0", "D1", 180);
    delegate.is_delegate = true;
    delegate.pid = 7004;
    delegate.cwd = Some(std::path::PathBuf::from("/w/d"));
    held.push(register_live(&delegate, "D1"));
    seed_cc_record(sb, 7004, "dddddddd-4444-4444-8444-dddddddddddd");

    let mut background = live_fixture_row("500-0", "cld1", 60);
    background.background = true;
    background.claude_pid = Some(7005);
    background.cwd = Some(std::path::PathBuf::from("/w/e"));
    held.push(register_live(&background, "cld1"));

    crate::live_sessions::register(&live_fixture_row("600-0", "cld1", 30)).expect("dead row");

    let mut hold = live_fixture_row("700-0", "cld1", 30);
    hold.background = true;
    hold.background_hold = true;
    held.push(register_live(&hold, "cld1"));

    let registry = sb.home().join(".clauth").join("live_sessions");
    fs::write(
        registry.join("800-0.json"),
        "{\"session_id\": \"800-0\", \"start_pro",
    )
    .unwrap();

    held
}

/// The reader keeps exactly the rows whose session still runs: a dead row, the
/// daemon's background hold and a torn file drop out; a moved session counts
/// by the marker its current member holds.
#[test]
fn live_rows_keep_only_the_running_sessions() {
    let sb = HomeSandbox::new();
    let _held = seed_live_roster(&sb);

    let mut sids: Vec<String> = crate::live_sessions::live_rows()
        .expect("the registry lists")
        .into_iter()
        .map(|row| row.session_id)
        .collect();
    sids.sort();
    assert_eq!(sids, ["100-0", "200-0", "300-1", "400-0", "500-0"]);
}

/// A `--with-fallback` background session the daemon adopted is a session
/// following the chain, which `clauth switch <sid>` moves, so the roster lists
/// it: the daemon's hold on it is not the account's fixed background tree.
#[test]
fn live_rows_list_an_adopted_with_fallback_session() {
    let _sb = HomeSandbox::new();
    let mut adopted = live_fixture_row("900-0", "cld1", 30);
    adopted.follows_chain = true;
    adopted.background_hold = true;
    let _held = register_live(&adopted, "cld1");
    let mut fixed = live_fixture_row("950-0", "cld1", 30);
    fixed.background = true;
    fixed.background_hold = true;
    let _fixed = register_live(&fixed, "cld1");

    let sids: Vec<String> = crate::live_sessions::live_rows()
        .expect("the registry lists")
        .into_iter()
        .map(|row| row.session_id)
        .collect();
    assert_eq!(sids, ["900-0"]);
}

/// One line per live session, newest first: sid, account (`start → current`
/// once moved), harness, age, transcript id, cwd, and a `delegate` or
/// `background` tag naming the session's kind; `-` fills a missing transcript
/// id or cwd.
#[test]
fn live_table_lists_each_running_session_newest_first() {
    let sb = HomeSandbox::new();
    let _held = seed_live_roster(&sb);

    let now = SystemTime::UNIX_EPOCH + Duration::from_secs(LIVE_NOW_SECS);
    assert_eq!(
        live_lines(now),
        [
            "500-0  cld1         claude  1m ago   -                                     /w/e  background",
            "400-0  D1           claude  3m ago   dddddddd-4444-4444-8444-dddddddddddd  /w/d  delegate",
            "300-1  cx0          codex   4m ago   -                                     /w/c",
            "200-0  cld1         claude  12m ago  -                                     -",
            "100-0  cld1 → cld2  claude  2h ago   aaaaaaaa-1111-4111-8111-aaaaaaaaaaaa  /w/a",
        ]
    );
}

/// `--json` carries the same rows, newest first, with absent cells as `null`.
#[test]
fn live_json_carries_the_same_rows() {
    let sb = HomeSandbox::new();
    let _held = seed_live_roster(&sb);

    let row = |sid: &str,
               start: &str,
               current: &str,
               harness: &str,
               cwd: Option<&str>,
               started: &str,
               transcript: Option<&str>,
               delegate: bool,
               background: bool| {
        json!({
            "sid": sid,
            "start_profile": start,
            "current_profile": current,
            "harness": harness,
            "cwd": cwd,
            "started_at": started,
            "transcript_id": transcript,
            "delegate": delegate,
            "background": background,
        })
    };
    assert_eq!(
        live_json(),
        json!([
            row(
                "500-0",
                "cld1",
                "cld1",
                "claude",
                Some("/w/e"),
                "2027-01-15T07:59:00+00:00",
                None,
                false,
                true
            ),
            row(
                "400-0",
                "D1",
                "D1",
                "claude",
                Some("/w/d"),
                "2027-01-15T07:57:00+00:00",
                Some("dddddddd-4444-4444-8444-dddddddddddd"),
                true,
                false
            ),
            row(
                "300-1",
                "cx0",
                "cx0",
                "codex",
                Some("/w/c"),
                "2027-01-15T07:56:00+00:00",
                None,
                false,
                false
            ),
            row(
                "200-0",
                "cld1",
                "cld1",
                "claude",
                None,
                "2027-01-15T07:48:00+00:00",
                None,
                false,
                false
            ),
            row(
                "100-0",
                "cld1",
                "cld2",
                "claude",
                Some("/w/a"),
                "2027-01-15T06:00:00+00:00",
                Some("aaaaaaaa-1111-4111-8111-aaaaaaaaaaaa"),
                false,
                false
            ),
        ])
    );
}

/// No live session is the listing's empty case: exit 1 with a named error,
/// the same scripting contract as an empty `clauth sessions`.
#[test]
fn live_listing_with_nothing_running_is_a_named_error() {
    let _sb = HomeSandbox::new();
    crate::live_sessions::register(&live_fixture_row("600-0", "cld1", 30)).expect("dead row");

    let err = run_sessions(false, false, true).expect_err("an empty roster is an error");
    assert_eq!(err.to_string(), "no live sessions found");
    assert_eq!(crate::exit_code(Err(err)), 1);
}

/// A registry that cannot be listed is not an empty one: the reader says so,
/// and the listing names the directory instead of claiming nothing runs.
#[test]
fn live_listing_names_an_unreadable_registry() {
    let sb = HomeSandbox::new();
    let clauth = sb.home().join(".clauth");
    fs::create_dir_all(&clauth).unwrap();
    let registry = clauth.join("live_sessions");
    fs::write(&registry, "not a directory").unwrap();

    assert!(crate::live_sessions::live_rows().is_none());
    for json in [false, true] {
        let err = run_sessions(json, false, true).expect_err("an unlistable registry is an error");
        assert_eq!(
            err.to_string(),
            format!(
                "cannot list live sessions: {} is unreadable",
                registry.display()
            ),
            "json {json}"
        );
        assert_eq!(crate::exit_code(Err(err)), 1);
    }
}

/// A cwd that is not UTF-8 still lists: both shapes show it lossily instead
/// of failing the whole listing.
#[cfg(unix)]
#[test]
fn live_listing_shows_a_non_utf8_cwd_lossily() {
    use std::os::unix::ffi::OsStrExt as _;

    let _sb = HomeSandbox::new();
    let mut row = live_fixture_row("950-0", "cld1", 60);
    row.cwd = Some(std::path::PathBuf::from(std::ffi::OsStr::from_bytes(
        b"/w/\xff",
    )));
    let _held = register_live(&row, "cld1");

    assert_eq!(live_json()[0]["cwd"], json!("/w/\u{fffd}"));
    let now = SystemTime::UNIX_EPOCH + Duration::from_secs(LIVE_NOW_SECS);
    assert_eq!(
        live_lines(now),
        ["950-0  cld1  claude  1m ago  -  /w/\u{fffd}"]
    );
}

/// Sessions started in the same millisecond keep one order on every run:
/// newest first, then by sid.
#[test]
fn live_rows_started_together_order_by_sid() {
    let _sb = HomeSandbox::new();
    let mut held = Vec::new();
    for sid in ["930-0", "910-0", "920-0"] {
        held.push(register_live(&live_fixture_row(sid, "cld1", 60), "cld1"));
    }
    held.push(register_live(
        &live_fixture_row("900-0", "cld1", 30),
        "cld1",
    ));

    let sids: Vec<String> = live_json()
        .as_array()
        .expect("an array")
        .iter()
        .map(|row| row["sid"].as_str().expect("a sid").to_string())
        .collect();
    assert_eq!(sids, ["900-0", "910-0", "920-0", "930-0"]);
    let now = SystemTime::UNIX_EPOCH + Duration::from_secs(LIVE_NOW_SECS);
    let line_sids: Vec<String> = live_lines(now)
        .iter()
        .map(|line| line[..5].to_string())
        .collect();
    assert_eq!(line_sids, sids);
}

/// `--live` reads no transcript, so it cannot carry `--tokens`, which reads
/// every one in full.
#[test]
fn the_live_listing_refuses_tokens() {
    use clap::Parser as _;

    let live = crate::cli::Cli::try_parse_from(["clauth", "sessions", "--live", "--json"])
        .expect("--live --json parses");
    match live.command {
        Some(crate::cli::Command::Sessions {
            live: true,
            json: true,
            tokens: false,
        }) => {}
        other => panic!("`sessions --live --json` must parse to the live listing: {other:?}"),
    }
    let err = crate::cli::Cli::try_parse_from(["clauth", "sessions", "--live", "--tokens"])
        .expect_err("--live with --tokens must refuse");
    assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
    assert_eq!(err.exit_code(), 2);
}

/// `--live` opens no transcript: the registry and Claude Code's per-pid
/// session records are its only reads. A thread-local ledger counts every
/// preview/history transcript read in [`crate::sessions`]; the live listing
/// must leave it at zero while the full listing over the same fixture moves it
/// — the control that proves the ledger counts. Opens through `crate::tokens`
/// / `crate::tps` and the rescue/move paths are not counted.
#[test]
fn live_listing_opens_no_transcript() {
    let sb = HomeSandbox::new();
    let _held = seed_live_roster(&sb);

    // Real transcripts named for the LIVE rows' own ids, so a by-id read of a
    // live row's transcript id actually opens one and moves the ledger.
    for (id, slug, cwd) in [
        ("aaaaaaaa-1111-4111-8111-aaaaaaaaaaaa", "-w-a", "/w/a"),
        ("dddddddd-4444-4444-8444-dddddddddddd", "-w-d", "/w/d"),
    ] {
        let path = sb
            .home()
            .join(format!(".claude/projects/{slug}/{id}.jsonl"));
        write_jsonl(&path, &[user_line(id, cwd, "hi")]);
    }

    crate::sessions::reset_transcript_opens();
    let now = SystemTime::UNIX_EPOCH + Duration::from_secs(LIVE_NOW_SECS);
    let _ = live_lines(now);
    let _ = live_json();
    run_sessions(false, false, true).expect("a non-empty live roster lists");
    run_sessions(true, false, true).expect("a non-empty live roster lists as json");
    assert_eq!(
        crate::sessions::transcript_opens(),
        0,
        "the live listing must open no transcript"
    );

    // Control: the full listing reads the same fixture's transcripts, so the
    // same ledger reports the opens — proving it would have caught one.
    let _ = build_listing(false);
    assert!(
        crate::sessions::transcript_opens() > 0,
        "the full listing opens a transcript, so the ledger counts"
    );
}

/// Claude Code's own record for the process `pid` under `rel` (relative to the
/// sandbox home), naming its transcript — the form for fixtures whose record
/// lives in a runtime tree.
fn seed_cc_record_in(sb: &HomeSandbox, rel: &str, pid: u32, transcript: &str) {
    let dir = sb.home().join(rel);
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join(format!("{pid}.json")),
        json!({ "sessionId": transcript, "pid": pid }).to_string(),
    )
    .unwrap();
}

/// A moved session's record lives in the tree of its START profile (the swap
/// re-stamps markers, never the config dir), and an isolated session's in the
/// isolated tree: the join must pick those, not `current_member`'s tree or the
/// global store.
#[test]
fn live_rows_read_their_runtime_tree_records() {
    let sb = HomeSandbox::new();

    let mut moved = live_fixture_row("111-0", "cld1", 7200);
    moved.current_member = Some("cld2".to_string());
    moved.claude_pid = Some(7101);
    let _moved_held = register_live(&moved, "cld2");
    seed_cc_record_in(
        &sb,
        ".clauth/profiles/cld1/runtime-111-0/sessions",
        7101,
        "eeeeeeee-1111-4111-8111-eeeeeeeeeeee",
    );

    let mut isolated = live_fixture_row("222-0", "cld1", 3600);
    isolated.isolated = true;
    isolated.claude_pid = Some(7102);
    crate::live_sessions::register(&isolated).expect("register row");
    let _isolated_held = crate::runtime::hold_session_row_marker(
        &crate::profile::ProfileName::from("cld1"),
        true,
        "222-0",
    )
    .expect("hold the session's marker");
    seed_cc_record_in(
        &sb,
        ".clauth/profiles/cld1/runtime-isolated-222-0/sessions",
        7102,
        "ffffffff-2222-4222-8222-ffffffffffff",
    );

    let rows = live_json();
    let transcript = |sid: &str| {
        rows.as_array()
            .expect("an array")
            .iter()
            .find(|row| row["sid"] == sid)
            .expect("row")["transcript_id"]
            .as_str()
            .expect("a transcript id")
            .to_string()
    };
    assert_eq!(transcript("111-0"), "eeeeeeee-1111-4111-8111-eeeeeeeeeeee");
    assert_eq!(transcript("222-0"), "ffffffff-2222-4222-8222-ffffffffffff");
}

/// A session that moved its working dir after launch (`--worktree`) lists the
/// dir its Claude Code record names, never the launcher's; a record with an
/// empty `cwd` falls back to the dir the session was launched from.
#[test]
fn live_rows_list_the_cwd_their_record_names() {
    let sb = HomeSandbox::new();
    let dir = sb.home().join(".claude").join("sessions");
    fs::create_dir_all(&dir).unwrap();

    let mut moved = live_fixture_row("100-0", "cld1", 120);
    moved.claude_pid = Some(7201);
    moved.cwd = Some(std::path::PathBuf::from("/w/launch"));
    let _moved_held = register_live(&moved, "cld1");
    fs::write(
        dir.join("7201.json"),
        json!({
            "sessionId": "aaaaaaaa-1111-4111-8111-aaaaaaaaaaaa",
            "pid": 7201,
            "cwd": "/w/launch/.claude/worktrees/x",
        })
        .to_string(),
    )
    .unwrap();

    let mut blank = live_fixture_row("200-0", "cld1", 60);
    blank.claude_pid = Some(7202);
    blank.cwd = Some(std::path::PathBuf::from("/w/b"));
    let _blank_held = register_live(&blank, "cld1");
    fs::write(
        dir.join("7202.json"),
        json!({
            "sessionId": "bbbbbbbb-2222-4222-8222-bbbbbbbbbbbb",
            "pid": 7202,
            "cwd": "",
        })
        .to_string(),
    )
    .unwrap();

    let mut retyped = live_fixture_row("300-0", "cld1", 180);
    retyped.claude_pid = Some(7203);
    retyped.cwd = Some(std::path::PathBuf::from("/w/r"));
    let _retyped_held = register_live(&retyped, "cld1");
    fs::write(
        dir.join("7203.json"),
        json!({
            "sessionId": "cccccccc-3333-4333-8333-cccccccccccc",
            "pid": 7203,
            "cwd": 7,
        })
        .to_string(),
    )
    .unwrap();

    let now = SystemTime::UNIX_EPOCH + Duration::from_secs(LIVE_NOW_SECS);
    assert_eq!(
        live_lines(now),
        [
            "200-0  cld1  claude  1m ago  bbbbbbbb-2222-4222-8222-bbbbbbbbbbbb  /w/b",
            "100-0  cld1  claude  2m ago  aaaaaaaa-1111-4111-8111-aaaaaaaaaaaa  /w/launch/.claude/worktrees/x",
            "300-0  cld1  claude  3m ago  cccccccc-3333-4333-8333-cccccccccccc  /w/r",
        ]
    );
    let rows = live_json();
    let cwd = |sid: &str| {
        rows.as_array()
            .expect("an array")
            .iter()
            .find(|row| row["sid"] == sid)
            .expect("row")["cwd"]
            .clone()
    };
    assert_eq!(cwd("100-0"), json!("/w/launch/.claude/worktrees/x"));
    assert_eq!(cwd("200-0"), json!("/w/b"));
    assert_eq!(cwd("300-0"), json!("/w/r"));
}

/// The table prints a record's cwd and transcript id with control characters
/// dropped, since another program writes that record: a dir name carrying an
/// escape sequence never reaches the terminal.
#[test]
fn live_table_drops_control_characters_from_record_cells() {
    let sb = HomeSandbox::new();
    let dir = sb.home().join(".claude").join("sessions");
    fs::create_dir_all(&dir).unwrap();
    let mut row = live_fixture_row("100-0", "cld1", 120);
    row.claude_pid = Some(7301);
    row.cwd = Some(std::path::PathBuf::from("/w/launch"));
    let _held = register_live(&row, "cld1");
    fs::write(
        dir.join("7301.json"),
        json!({
            "sessionId": "aaaaaaaa-1111-4111-8111-aaaaaaaa\u{7}aaaa",
            "pid": 7301,
            "cwd": "/w/\u{1b}]0;pwn\u{7}x",
        })
        .to_string(),
    )
    .unwrap();

    let now = SystemTime::UNIX_EPOCH + Duration::from_secs(LIVE_NOW_SECS);
    assert_eq!(
        live_lines(now),
        ["100-0  cld1  claude  2m ago  aaaaaaaa-1111-4111-8111-aaaaaaaaaaaa  /w/]0;pwnx"]
    );
}

/// A background row's record is found through the account's fixed
/// `runtime-bg` tree.
#[test]
fn live_rows_find_a_background_rows_record_in_the_background_tree() {
    let sb = HomeSandbox::new();

    let mut background = live_fixture_row("333-0", "cld1", 60);
    background.background = true;
    background.claude_pid = Some(7103);
    let _held = register_live(&background, "cld1");
    seed_cc_record_in(
        &sb,
        ".clauth/profiles/cld1/runtime-bg/sessions",
        7103,
        "cccccccc-3333-4333-8333-cccccccccccc",
    );

    let rows = crate::live_sessions::live_rows().expect("the registry lists");
    let row = rows.iter().find(|r| r.session_id == "333-0").expect("row");
    assert_eq!(
        crate::session_record::live_record(row).map(|r| r.session_id),
        Some("cccccccc-3333-4333-8333-cccccccccccc".to_string()),
    );
}

/// The `--live` table pads its account and cwd columns by terminal cells: a CJK
/// account and cwd keep the tag column on one screen column across rows.
#[test]
fn a_cjk_live_account_and_cwd_keep_the_tag_column_aligned() {
    let _sb = HomeSandbox::new();
    let now = SystemTime::UNIX_EPOCH + Duration::from_secs(10_000);
    // 14_000_000 s ago pins `started_at` at 1_786_000_000_000 ms, after `now`,
    // so both rows age as `just now`.
    let row = |sid: &str, account: &str, cwd: &str| {
        let mut r = live_fixture_row(sid, account, 14_000_000);
        r.cwd = Some(std::path::PathBuf::from(cwd));
        r.is_delegate = true;
        r
    };

    let lines = live_lines_of(
        &[
            row("s-cjk", "漢字", "/ws/漢字"),
            row("s-asc", "ascii-prof", "/ws/ascii-project"),
        ],
        now,
    );

    // ACCOUNT 10 cells, CWD 17 cells; the age is `just now` on both rows.
    assert_eq!(
        lines[0],
        format!(
            "s-cjk  漢字{}  claude  just now  -  /ws/漢字{}  delegate",
            " ".repeat(6),
            " ".repeat(9)
        ),
        "the CJK account or cwd overruns its column"
    );
    assert_eq!(
        lines[1], "s-asc  ascii-prof  claude  just now  -  /ws/ascii-project  delegate",
        "the ASCII row pads the CJK columns' widths"
    );
}
