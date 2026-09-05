//! Session-index core + redaction tests. Fixture stores live under a
//! `HomeSandbox` so the global (`~/.claude/projects`) and isolated
//! (`~/.clauth/profiles/<n>/runtime-isolated/projects`) walks stay off the real
//! tree. Every transcript file is named `<sessionId>.jsonl` because the session
//! id is keyed off the filename stem, not the head line.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;
use std::collections::HashSet;
use std::fs;
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

fn assistant_line(sid: &str, cwd: &str, text: &str) -> String {
    json!({"sessionId": sid, "cwd": cwd,
        "message": {"role": "assistant", "content": [{"type": "text", "text": text}]}})
    .to_string()
}

/// An assistant filler line padded to exactly `len` bytes (`len` must be >= the
/// base line length). The pad is unescaped ASCII, so the byte length is exact —
/// it places a user line precisely across a tail-chunk boundary.
fn assistant_line_of_len(sid: &str, cwd: &str, len: usize) -> String {
    let base = assistant_line(sid, cwd, "");
    let pad = len.saturating_sub(base.len());
    assistant_line(sid, cwd, &"y".repeat(pad))
}

/// A `role:user` line whose only block is a `tool_result` — carries no text, so
/// it must never surface as a first/last preview.
fn tool_result_line(sid: &str) -> String {
    json!({"sessionId": sid,
        "message": {"role": "user", "content": [{"type": "tool_result", "content": "out"}]}})
    .to_string()
}

/// An assistant usage line: carries a `message.id` (the token dedup key), a
/// model, and input/output token counts. `parse_file` requires a timestamp, so
/// one is always stamped.
fn usage_line(sid: &str, cwd: &str, msg_id: &str, model: &str, input: u64, output: u64) -> String {
    usage_line_at(
        sid,
        cwd,
        msg_id,
        model,
        input,
        output,
        "2026-06-11T10:30:00+00:00",
    )
}

/// `usage_line` at an explicit UTC timestamp — the hour byte (offset 11) drives
/// the peak/off-peak tier.
fn usage_line_at(
    sid: &str,
    cwd: &str,
    msg_id: &str,
    model: &str,
    input: u64,
    output: u64,
    timestamp: &str,
) -> String {
    json!({
        "sessionId": sid, "cwd": cwd, "timestamp": timestamp,
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

// ── price-table builders ─────────────────────────────────────────────────────

/// One unconstrained price entry at the given input/output rates (cache 0).
fn flat_entry(input: f64, output: f64) -> crate::pricing::PriceEntry {
    crate::pricing::PriceEntry {
        input,
        output,
        cache_read: 0.0,
        cache_write: 0.0,
        constraint: None,
    }
}

/// An exact-match model from its price entries.
fn priced_model(id: &str, entries: Vec<crate::pricing::PriceEntry>) -> crate::pricing::PricedModel {
    crate::pricing::PricedModel {
        id: id.to_owned(),
        prices: entries,
        effective_at: None,
    }
}

/// A `PriceTable` from `(model_id, input_rate, output_rate)` rows; cache rates 0.
fn price_table(rows: &[(&str, f64, f64)]) -> crate::pricing::PriceTable {
    crate::pricing::PriceTable::capture(
        rows.iter()
            .map(|&(id, input, output)| priced_model(id, vec![flat_entry(input, output)]))
            .collect(),
        Vec::new(),
        Vec::new(),
        crate::pricing::CanonicalMap::default(),
        crate::tokens::today_date(),
        0,
        Vec::new(),
    )
}

/// (model id, peak input/output, off-peak input/output) per-token rates.
type WindowRates<'a> = (&'a str, (f64, f64), (f64, f64));

/// deepseek-chat-shaped pricing: an off-peak fallback entry plus a peak
/// `00:30–16:30Z` time-window entry, which reversed-entry selection prefers
/// while it is active. Hour 0 prices off-peak and hour 16 peak.
fn windowed_table(rows: &[WindowRates<'_>]) -> crate::pricing::PriceTable {
    use crate::pricing::Constraint;
    crate::pricing::PriceTable::capture(
        rows.iter()
            .map(|&(id, peak, off_peak)| {
                priced_model(
                    id,
                    vec![
                        flat_entry(off_peak.0, off_peak.1),
                        crate::pricing::PriceEntry {
                            input: peak.0,
                            output: peak.1,
                            cache_read: 0.0,
                            cache_write: 0.0,
                            constraint: Some(Constraint::TimeWindow {
                                start: "00:30:00Z".to_owned(),
                                end: "16:30:00Z".to_owned(),
                            }),
                        },
                    ],
                )
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

fn find<'a>(groups: &'a [WorkspaceGroup], id: &str) -> Option<&'a SessionInfo> {
    groups
        .iter()
        .flat_map(|g| g.sessions.iter())
        .find(|s| s.id == id)
}

/// Minimal groups carrying only the ids under test — enough to drive
/// `annotate_owners`, which reads `id` and writes `last_ran_profile` and touches
/// nothing else. Decouples the owner-store tests from `build_index`/liveness.
fn groups_of(ids: &[&str]) -> Vec<WorkspaceGroup> {
    let sessions = ids
        .iter()
        .map(|id| SessionInfo {
            id: (*id).to_owned(),
            workspace: String::new(),
            path: std::path::PathBuf::new(),
            updated: SystemTime::UNIX_EPOCH,
            first_message: None,
            last_message: None,
            source: SessionSource::Global,
            tokens: None,
            cost: None,
            last_ran_profile: None,
        })
        .collect();
    vec![WorkspaceGroup {
        workspace: String::new(),
        sessions,
    }]
}

#[test]
fn redact_secrets_masks_secret_shapes_and_keeps_context() {
    // sk- API key — whole token masked, surrounding words survive.
    assert_eq!(
        redact_secrets("prefix sk-ant-api03-ABCDEFGHIJKLMNOPQRST suffix"),
        "prefix [REDACTED] suffix"
    );
    // Bearer token — the "Bearer " marker stays, the token is masked.
    assert_eq!(
        redact_secrets("auth: Bearer abcDEF123456ghi789 done"),
        "auth: Bearer [REDACTED] done"
    );
    // JSON key/value — the key name stays visible, only the value is masked.
    assert_eq!(
        redact_secrets(r#"cfg {"api_key": "supersecretvalue"} end"#),
        r#"cfg {"api_key": "[REDACTED]"} end"#
    );
    // Bare high-entropy hex/base64 blob (>= 24 chars, mixed digit+letter).
    assert_eq!(
        redact_secrets("hash 0123456789abcdef0123456789abcdef done"),
        "hash [REDACTED] done"
    );
}

#[test]
fn redact_secrets_masks_provider_token_bypasses() {
    // GitHub token — a leading `_` is a word char, so a `\b`-anchored generic
    // blob would miss `ghp_...` entirely.
    let s = redact_secrets("token ghp_16C7e42F292c6912E7710c838347Ae178B4a here");
    assert!(
        !s.contains("ghp_16C7e42F292c6912E7710c838347Ae178B4a"),
        "github token leaked: {s}"
    );
    assert!(s.contains("[REDACTED]"), "{s}");

    // Fine-grained GitHub PAT.
    let s = redact_secrets("pat github_pat_11ABCDE0000aBcDeFgHiJkLmNoPqRsTuVwXyZ done");
    assert!(
        !s.contains("github_pat_11ABCDE0000aBcDeFgHiJkLmNoPqRsTuVwXyZ"),
        "github pat leaked: {s}"
    );
    assert!(s.contains("[REDACTED]"), "{s}");

    // Slack bot token — dash-split, `-` is not a word char.
    let s = redact_secrets("slack xoxb-EXAMPLE-fake-slack-token end");
    assert!(!s.contains("xoxb-EXAMPLE"), "slack token leaked: {s}");
    assert!(s.contains("[REDACTED]"), "{s}");

    // URL credentials — password masked, host + user context kept.
    let s = redact_secrets("clone https://alice:hunter2secretpw@host.example/repo.git");
    assert!(!s.contains("hunter2secretpw"), "url password leaked: {s}");
    assert!(s.contains("host.example"), "host must survive: {s}");
    assert!(s.contains("[REDACTED]"), "{s}");

    // Bare JWT — masked as one unit.
    let s = redact_secrets(
        "jwt eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dozjgNryP4J3jVmNHl0w done",
    );
    assert!(!s.contains("eyJhbGciOiJIUzI1NiJ9"), "jwt leaked: {s}");
    assert!(s.contains("[REDACTED]"), "{s}");

    // AWS access key id.
    let s = redact_secrets("aws AKIAIOSFODNN7EXAMPLE key");
    assert!(!s.contains("AKIAIOSFODNN7EXAMPLE"), "aws key leaked: {s}");
    assert!(s.contains("[REDACTED]"), "{s}");
}

#[test]
fn redact_secrets_spares_pathlike_prose() {
    // A path-ish, digit-free preview must not be over-redacted: the entropy
    // catch-all only masks a run that mixes a digit with a letter.
    let s = "see docs/gettingstartedguide/readme for setup";
    assert_eq!(redact_secrets(s), s, "path-ish prose must stay unchanged");
}

#[test]
fn build_index_redacts_preview_without_touching_source() {
    let sb = HomeSandbox::new();
    let path = sb.home().join(".claude/projects/-w-sec/ssec.jsonl");
    let secret = "here is my key sk-ant-api03-ABCDEFGHIJKLMNOP and more";
    write_jsonl(&path, &[user_line("ssec", "/w/sec", secret)]);
    let before = fs::read(&path).unwrap();

    let groups = build_index();

    // The source JSONL is read-only to the index — never rewritten.
    let after = fs::read(&path).unwrap();
    assert_eq!(
        before, after,
        "source file must be byte-identical after indexing"
    );

    let info = find(&groups, "ssec").expect("session indexed");
    let preview = info.first_message.as_deref().expect("first message");
    assert!(
        preview.contains("[REDACTED]"),
        "preview redacted: {preview}"
    );
    assert!(
        !preview.contains("sk-ant-api03-ABCDEFGHIJKLMNOP"),
        "secret leaked into preview: {preview}"
    );
    assert!(
        preview.contains("here is my key"),
        "non-secret text survived: {preview}"
    );
}

#[test]
fn session_id_comes_from_file_stem_not_first_line() {
    let sb = HomeSandbox::new();
    // File named by the real session id; its first line carries a DIFFERENT
    // (parent) sessionId that a resume copy carried forward — it must not key
    // the session.
    let path = sb
        .home()
        .join(".claude/projects/-w-stem/child-session.jsonl");
    write_jsonl(
        &path,
        &[
            user_line("parent-session", "/w/stem", "head msg"),
            user_line("parent-session", "/w/stem", "tail msg"),
        ],
    );

    let groups = build_index();
    assert!(
        find(&groups, "parent-session").is_none(),
        "first-line sessionId must not key the session"
    );
    let info = find(&groups, "child-session").expect("keyed by file stem");
    assert_eq!(info.workspace, "/w/stem");
    assert_eq!(info.first_message.as_deref(), Some("head msg"));
    assert_eq!(info.last_message.as_deref(), Some("tail msg"));
}

#[test]
fn last_user_message_comes_from_the_tail_not_the_head() {
    let sb = HomeSandbox::new();
    let path = sb.home().join(".claude/projects/-w-tail/stail.jsonl");
    let mut lines = vec![user_line("stail", "/w/tail", "first question")];
    // Bury the final user turn deep so a head-only read would miss it.
    for i in 0..50 {
        lines.push(assistant_line(
            "stail",
            "/w/tail",
            &format!("filler answer {i}"),
        ));
    }
    lines.push(user_line("stail", "/w/tail", "final question here"));
    write_jsonl(&path, &lines);

    let groups = build_index();
    let info = find(&groups, "stail").expect("session indexed");
    assert_eq!(info.first_message.as_deref(), Some("first question"));
    assert_eq!(info.last_message.as_deref(), Some("final question here"));
}

#[test]
fn bounded_head_and_tail_windows_recover_first_and_last_user() {
    let sb = HomeSandbox::new();
    let path = sb.home().join(".claude/projects/-w-big/big-session.jsonl");

    let head = user_line("sbig", "/w/big", "the head question");
    let tail = user_line("sbig", "/w/big", "the tail question");
    let filler = assistant_line("sbig", "/w/big", &"x".repeat(900));

    let mut lines = vec![head.clone()];
    // > HEAD_MAX_BYTES of filler between head and tail: a head-only read can't
    // reach the tail, and the head cap is exercised.
    let mut mid = 0usize;
    while mid <= HEAD_MAX_BYTES as usize {
        lines.push(filler.clone());
        mid += filler.len() + 1;
    }
    lines.push(tail.clone());
    // One trailing filler line sized so `len - TAIL_CHUNK` lands INSIDE the tail
    // user line: the first 64 KiB window drops it as a partial first line,
    // forcing the tail window to grow before the tail is recovered whole.
    lines.push(assistant_line_of_len(
        "sbig",
        "/w/big",
        TAIL_CHUNK as usize - 40,
    ));
    write_jsonl(&path, &lines);

    let groups = build_index();
    let info = find(&groups, "big-session").expect("session indexed");
    assert_eq!(info.first_message.as_deref(), Some("the head question"));
    assert_eq!(info.last_message.as_deref(), Some("the tail question"));
}

#[test]
fn duplicate_session_id_collapses_to_newest_file() {
    let sb = HomeSandbox::new();
    // Same session id (== stem) copied into two project-slug dirs.
    let older = sb.home().join(".claude/projects/-w-dup-a/sdup.jsonl");
    let newer = sb.home().join(".claude/projects/-w-dup-b/sdup.jsonl");
    write_jsonl(
        &older,
        &[
            user_line("sdup", "/w/dup", "old first"),
            user_line("sdup", "/w/dup", "old last"),
        ],
    );
    write_jsonl(
        &newer,
        &[
            user_line("sdup", "/w/dup", "new first"),
            user_line("sdup", "/w/dup", "new last"),
        ],
    );
    set_mtime(&older, SystemTime::UNIX_EPOCH + Duration::from_secs(1_000));
    set_mtime(&newer, SystemTime::UNIX_EPOCH + Duration::from_secs(2_000));

    let groups = build_index();
    let dups: Vec<&SessionInfo> = groups
        .iter()
        .flat_map(|g| g.sessions.iter())
        .filter(|s| s.id == "sdup")
        .collect();
    assert_eq!(dups.len(), 1, "same id across files collapses to one");
    assert_eq!(dups[0].first_message.as_deref(), Some("new first"));
    assert_eq!(dups[0].last_message.as_deref(), Some("new last"));
}

#[test]
fn duplicate_equal_mtime_breaks_tie_by_greater_path() {
    let sb = HomeSandbox::new();
    // Same session id in two slug dirs at an identical mtime — the pick must be
    // deterministic regardless of `read_dir` order.
    let a = sb.home().join(".claude/projects/-w-tie-a/stie.jsonl");
    let b = sb.home().join(".claude/projects/-w-tie-b/stie.jsonl");
    write_jsonl(&a, &[user_line("stie", "/w/tie", "from a")]);
    write_jsonl(&b, &[user_line("stie", "/w/tie", "from b")]);
    let when = SystemTime::UNIX_EPOCH + Duration::from_secs(5_000);
    set_mtime(&a, when);
    set_mtime(&b, when);

    let groups = build_index();
    let hits: Vec<&SessionInfo> = groups
        .iter()
        .flat_map(|g| g.sessions.iter())
        .filter(|s| s.id == "stie")
        .collect();
    assert_eq!(hits.len(), 1, "equal-mtime duplicate collapses to one");
    // `-w-tie-b/...` > `-w-tie-a/...` lexicographically, so b wins.
    assert_eq!(hits[0].first_message.as_deref(), Some("from b"));
}

#[test]
fn build_index_covers_global_and_isolated_and_indexes_corrupt() {
    let sb = HomeSandbox::new();

    // Global session (stem == sessionId) with a tool-result turn between the two
    // user turns.
    let g = sb.home().join(".claude/projects/-w-global/sg.jsonl");
    write_jsonl(
        &g,
        &[
            user_line("sg", "/w/global", "hi global"),
            assistant_line("sg", "/w/global", "reply"),
            tool_result_line("sg"),
            user_line("sg", "/w/global", "bye global"),
        ],
    );

    // Corrupt / non-transcript file: no readable head, so it is indexed under
    // its stem with best-effort empty metadata rather than dropped — the
    // fail-soft that also keeps summary-first and oversized-head sessions.
    let bad = sb.home().join(".claude/projects/-w-global/bad.jsonl");
    fs::create_dir_all(bad.parent().unwrap()).unwrap();
    fs::write(
        &bad,
        b"{\"sessionId\":\"broken\" this is not valid json\n\xff\xfe garbage".as_slice(),
    )
    .unwrap();

    // Resume copy: one file (stem `sr`) carrying two session ids. Keyed by the
    // stem; head + tail messages regardless of the id change mid-file.
    let r = sb.home().join(".claude/projects/-w-resume/sr.jsonl");
    write_jsonl(
        &r,
        &[
            user_line("sr", "/w/resume", "resume head"),
            assistant_line("sr", "/w/resume", "reply"),
            user_line("sr2", "/w/resume", "carried forward"),
            user_line("sr2", "/w/resume", "resume tail"),
        ],
    );

    // Live isolated session in its own throwaway store.
    let iso = sb
        .home()
        .join(".clauth/profiles/iso/runtime-isolated/projects/-w-iso/si.jsonl");
    write_jsonl(
        &iso,
        &[
            user_line("si", "/w/iso", "hi iso"),
            user_line("si", "/w/iso", "bye iso"),
        ],
    );
    let sessions_dir = sb.home().join(".clauth/profiles/iso/sessions-isolated");
    fs::create_dir_all(&sessions_dir).unwrap();
    let lock_file = crate::runtime::open_pid_file(&sessions_dir.join("12345")).unwrap();
    lock_file.lock().unwrap(); // held for the walk so the runtime reads as live

    // Distinct mtimes fix the newest-first order: global > resume > iso > corrupt.
    set_mtime(&g, SystemTime::UNIX_EPOCH + Duration::from_secs(3_000));
    set_mtime(&r, SystemTime::UNIX_EPOCH + Duration::from_secs(2_000));
    set_mtime(&iso, SystemTime::UNIX_EPOCH + Duration::from_secs(1_000));
    set_mtime(&bad, SystemTime::UNIX_EPOCH + Duration::from_secs(500));

    let groups = build_index();
    drop(lock_file);

    // The corrupt file has no `cwd`, so it groups under the empty workspace,
    // last by mtime.
    let workspaces: Vec<&str> = groups.iter().map(|g| g.workspace.as_str()).collect();
    assert_eq!(workspaces, vec!["/w/global", "/w/resume", "/w/iso", ""]);

    let all: Vec<&SessionInfo> = groups.iter().flat_map(|g| g.sessions.iter()).collect();
    let ids: HashSet<&str> = all.iter().map(|s| s.id.as_str()).collect();
    assert_eq!(ids, HashSet::from(["sg", "sr", "si", "bad"]));
    assert!(
        !ids.contains("sr2"),
        "in-file id is not used; the stem keys the session"
    );
    assert!(
        !ids.contains("broken"),
        "in-file sessionId is never the key"
    );

    let sg = find(&groups, "sg").unwrap();
    assert_eq!(sg.workspace, "/w/global");
    assert_eq!(sg.source, SessionSource::Global);
    assert_eq!(sg.first_message.as_deref(), Some("hi global"));
    assert_eq!(sg.last_message.as_deref(), Some("bye global"));
    assert!(sg.tokens.is_none());
    assert!(sg.cost.is_none());
    assert!(sg.last_ran_profile.is_none());

    let sr = find(&groups, "sr").unwrap();
    assert_eq!(sr.workspace, "/w/resume");
    assert_eq!(sr.source, SessionSource::Global);
    assert_eq!(sr.first_message.as_deref(), Some("resume head"));
    assert_eq!(sr.last_message.as_deref(), Some("resume tail"));

    let si = find(&groups, "si").unwrap();
    assert_eq!(si.workspace, "/w/iso");
    assert_eq!(
        si.source,
        SessionSource::Isolated {
            profile: "iso".to_string()
        }
    );
    assert_eq!(si.first_message.as_deref(), Some("hi iso"));
    assert_eq!(si.last_message.as_deref(), Some("bye iso"));

    // Corrupt file: indexed under its stem, empty workspace, no previews.
    let bad = find(&groups, "bad").unwrap();
    assert_eq!(bad.workspace, "");
    assert!(bad.first_message.is_none());
    assert!(bad.last_message.is_none());
}

#[test]
fn annotate_sums_tokens_and_cost_across_models() {
    let sb = HomeSandbox::new();
    let path = sb.home().join(".claude/projects/-w-tok/stok.jsonl");
    write_jsonl(
        &path,
        &[
            usage_line("stok", "/w/tok", "m1", "claude-opus-4-8", 1000, 500),
            usage_line("stok", "/w/tok", "m2", "claude-sonnet-4-5", 2000, 1000),
        ],
    );
    // $1 in / $2 out per million for both models.
    let table = price_table(&[
        ("claude-opus-4-8", 1e-6, 2e-6),
        ("claude-sonnet-4-5", 1e-6, 2e-6),
    ]);

    let mut groups = build_index();
    annotate_all(&mut groups, Some(&table));

    let info = find(&groups, "stok").expect("session indexed");
    // in+out across both models: (1000+500) + (2000+1000) = 4500. Cache excluded.
    assert_eq!(info.tokens, Some(4500));
    // opus 1000*1e-6 + 500*2e-6 = 0.002; sonnet 2000*1e-6 + 1000*2e-6 = 0.004.
    let cost = info.cost.expect("priced");
    assert!((cost - 0.006).abs() < 1e-9, "got {cost}");
}

#[test]
fn annotate_prices_peak_and_off_peak_hours_at_their_tiers() {
    let sb = HomeSandbox::new();
    let path = sb.home().join(".claude/projects/-w-win/swin.jsonl");
    // Two responses an hour apart: hour 0 prices off-peak, hour 16 peak —
    // per the settled hour-granularity formula (window 00:30–16:30Z sampled
    // at the hour's start).
    write_jsonl(
        &path,
        &[
            usage_line_at(
                "swin",
                "/w/win",
                "w1",
                "deepseek-chat",
                1000,
                0,
                "2026-06-11T00:10:00+00:00",
            ),
            usage_line_at(
                "swin",
                "/w/win",
                "w2",
                "deepseek-chat",
                1000,
                0,
                "2026-06-11T16:10:00+00:00",
            ),
        ],
    );
    let table = windowed_table(&[("deepseek-chat", (2e-6, 4e-6), (1e-6, 2e-6))]);

    let mut groups = build_index();
    annotate_all(&mut groups, Some(&table));

    let info = find(&groups, "swin").expect("session indexed");
    assert_eq!(info.tokens, Some(2000));
    // 1000 * 1e-6 (off-peak) + 1000 * 2e-6 (peak) = $0.003 — neither the
    // all-off-peak ($0.002) nor the all-peak ($0.004) flat total.
    let cost = info.cost.expect("priced");
    assert!((cost - 0.003).abs() < 1e-9, "got {cost}");
}

#[test]
fn annotate_priced_zero_cost_session_is_some_zero() {
    let sb = HomeSandbox::new();
    let path = sb.home().join(".claude/projects/-w-zero/szero.jsonl");
    // A token-bearing row with zero counts on a priced model: the session has
    // usage, the model has a rate ⇒ Some(0.0), distinct from all-unpriced None.
    write_jsonl(
        &path,
        &[usage_line(
            "szero",
            "/w/zero",
            "z1",
            "claude-opus-4-8",
            0,
            0,
        )],
    );
    let table = price_table(&[("claude-opus-4-8", 1e-6, 2e-6)]);

    let mut groups = build_index();
    annotate_all(&mut groups, Some(&table));

    let info = find(&groups, "szero").expect("session indexed");
    assert_eq!(info.tokens, Some(0));
    assert_eq!(info.cost, Some(0.0));
}

#[test]
fn annotate_prices_each_day_at_its_dated_rate() {
    let sb = HomeSandbox::new();
    let path = sb.home().join(".claude/projects/-w-dated/sdated.jsonl");
    // Two usage days straddling a 06-12 rate change: each (model, day) pair
    // prices at the snapshot live on its own date — 06-11 at the old rate,
    // 06-13 at the new one.
    write_jsonl(
        &path,
        &[
            usage_line_at(
                "sdated",
                "/w/dated",
                "d1",
                "m",
                1000,
                0,
                "2026-06-11T10:30:00+00:00",
            ),
            usage_line_at(
                "sdated",
                "/w/dated",
                "d2",
                "m",
                1000,
                0,
                "2026-06-13T10:30:00+00:00",
            ),
        ],
    );
    let cheap = priced_model("m", vec![flat_entry(1e-6, 0.0)]);
    let dear = priced_model("m", vec![flat_entry(2e-6, 0.0)]);
    let table = crate::pricing::PriceTable::capture(
        vec![dear.clone()],
        Vec::new(),
        Vec::new(),
        crate::pricing::CanonicalMap::default(),
        crate::tokens::today_date(),
        0,
        vec![
            crate::pricing::RateSnapshot {
                captured: "2026-06-01".to_owned(),
                models: vec![cheap],
            },
            crate::pricing::RateSnapshot {
                captured: "2026-06-12".to_owned(),
                models: vec![dear],
            },
        ],
    );

    let mut groups = build_index();
    annotate_all(&mut groups, Some(&table));

    let info = find(&groups, "sdated").expect("session indexed");
    assert_eq!(info.tokens, Some(2000));
    // 1000 * 1e-6 (06-11) + 1000 * 2e-6 (06-13) = $0.003, not $0.004 at the
    // newest rate for both.
    let cost = info.cost.expect("priced");
    assert!((cost - 0.003).abs() < 1e-9, "got {cost}");
}

#[test]
fn annotate_leaves_tokenless_session_blank() {
    let sb = HomeSandbox::new();
    let path = sb.home().join(".claude/projects/-w-none/snone.jsonl");
    write_jsonl(
        &path,
        &[
            user_line("snone", "/w/none", "just chatting"),
            assistant_line("snone", "/w/none", "no usage recorded"),
        ],
    );
    let table = price_table(&[("claude-opus-4-8", 1e-6, 2e-6)]);

    let mut groups = build_index();
    annotate_all(&mut groups, Some(&table));

    let info = find(&groups, "snone").expect("session indexed");
    // No usage line ⇒ blank, never Some(0), even with a price table present.
    assert_eq!(info.tokens, None);
    assert_eq!(info.cost, None);
}

#[test]
fn annotate_unpriced_model_has_tokens_but_no_cost() {
    let sb = HomeSandbox::new();
    let path = sb.home().join(".claude/projects/-w-unp/sunp.jsonl");
    write_jsonl(
        &path,
        &[usage_line("sunp", "/w/unp", "u1", "gpt-5", 700, 300)],
    );
    // Table prices only opus — gpt-5 has no matching rate.
    let table = price_table(&[("claude-opus-4-8", 1e-6, 2e-6)]);

    let mut groups = build_index();
    annotate_all(&mut groups, Some(&table));

    let info = find(&groups, "sunp").expect("session indexed");
    assert_eq!(info.tokens, Some(1000)); // 700 + 300, tokens still counted
    assert_eq!(info.cost, None); // model unpriced ⇒ None, not Some(0.0)
}

#[test]
fn annotate_dedupes_carried_forward_line_by_tok_key() {
    let sb = HomeSandbox::new();
    let path = sb.home().join(".claude/projects/-w-dup/sdupe.jsonl");
    // Same response (message.id "m1") twice — the shape a resumed or branched
    // session produces when it copies its parent's history forward. Count ONCE.
    write_jsonl(
        &path,
        &[
            usage_line("sdupe", "/w/dup", "m1", "claude-opus-4-8", 1000, 500),
            usage_line("sdupe", "/w/dup", "m1", "claude-opus-4-8", 1000, 500),
        ],
    );

    let mut groups = build_index();
    annotate_all(&mut groups, None);

    let info = find(&groups, "sdupe").expect("session indexed");
    // Single-counted: 1000 + 500, NOT doubled to 3000.
    assert_eq!(info.tokens, Some(1500));
    // No table ⇒ cost stays None, however many tokens the file carries.
    assert_eq!(info.cost, None);
}

// ── A3: session → last-ran-profile stamp/read ────────────────────────────────

#[test]
fn stamp_isolated_owns_all_sessions_ignoring_mtime() {
    let sb = HomeSandbox::new();
    // An isolated store is exclusive to the profile: every transcript maps to it
    // regardless of mtime, so no run window applies.
    let projects = sb
        .home()
        .join(".clauth/profiles/iso/runtime-isolated/projects");
    let a = projects.join("-w-a/isoA.jsonl");
    let b = projects.join("-w-b/isoB.jsonl");
    write_jsonl(&a, &[user_line("isoA", "/w/a", "hi")]);
    write_jsonl(&b, &[user_line("isoB", "/w/b", "yo")]);
    // Far in the past: proves the mtime window is not consulted for isolated.
    let ancient = SystemTime::UNIX_EPOCH + Duration::from_secs(1);
    set_mtime(&a, ancient);
    set_mtime(&b, ancient);

    stamp_run_sessions("iso", &projects, true, SystemTime::now());

    let mut groups = groups_of(&["isoA", "isoB"]);
    annotate_owners(&mut groups);
    assert_eq!(
        find(&groups, "isoA").unwrap().last_ran_profile.as_deref(),
        Some("iso")
    );
    assert_eq!(
        find(&groups, "isoB").unwrap().last_ran_profile.as_deref(),
        Some("iso")
    );
}

#[test]
fn stamp_shared_respects_run_window() {
    let sb = HomeSandbox::new();
    let projects = sb.home().join(".claude/projects");
    let fresh = projects.join("-w-new/freshS.jsonl");
    let stale = projects.join("-w-old/staleS.jsonl");
    write_jsonl(&fresh, &[user_line("freshS", "/w/new", "new")]);
    write_jsonl(&stale, &[user_line("staleS", "/w/old", "old")]);

    let run_start = SystemTime::now();
    // `fresh` touched during the run (>= run_start); `stale` predates it and
    // belongs to some earlier session, not this one.
    set_mtime(&fresh, run_start + Duration::from_secs(1));
    set_mtime(&stale, run_start - Duration::from_secs(60));

    stamp_run_sessions("shared", &projects, false, run_start);

    let mut groups = groups_of(&["freshS", "staleS"]);
    annotate_owners(&mut groups);
    assert_eq!(
        find(&groups, "freshS").unwrap().last_ran_profile.as_deref(),
        Some("shared")
    );
    assert_eq!(
        find(&groups, "staleS").unwrap().last_ran_profile,
        None,
        "a pre-window shared session is not this run's"
    );
}

#[test]
fn contested_shared_session_reads_back_unknown() {
    let sb = HomeSandbox::new();
    let projects = sb.home().join(".claude/projects");
    let s = projects.join("-w-c/contested.jsonl");
    write_jsonl(&s, &[user_line("contested", "/w/c", "shared work")]);

    let t0 = SystemTime::UNIX_EPOCH + Duration::from_secs(10_000);
    set_mtime(&s, t0);

    // Two different profiles touch the SAME shared session within their windows.
    stamp_run_sessions("A", &projects, false, t0);
    stamp_run_sessions("B", &projects, false, t0);

    let mut groups = groups_of(&["contested"]);
    annotate_owners(&mut groups);
    // Genuinely unknown: never resolves to A, never to B.
    assert_eq!(
        find(&groups, "contested").unwrap().last_ran_profile,
        None,
        "two owners must collapse to unknown, not the last writer"
    );
}

/// Stage the exact per-conversation record the hook writes:
/// `~/.clauth/conversations/<id>.json`, carrying the account the hook resolved.
/// `resolved: null` is the shape of a record that never attributed an account.
fn stage_record(sb: &HomeSandbox, id: &str, resolved: Option<&str>) {
    let dir = sb.home().join(".clauth/conversations");
    fs::create_dir_all(&dir).unwrap();
    crate::profile::atomic_write_600(
        &dir.join(format!("{id}.json")),
        serde_json::to_vec(&json!({ "resolved": resolved })).unwrap(),
    )
    .unwrap();
}

#[test]
fn exact_observation_survives_both_runs_and_shows_in_listing() {
    let sb = HomeSandbox::new();
    let projects = sb.home().join(".claude/projects");
    let s = projects.join("-w-exact/exactE.jsonl");
    write_jsonl(&s, &[user_line("exactE", "/w/exact", "one profile's work")]);
    let t0 = SystemTime::UNIX_EPOCH + Duration::from_secs(10_000);
    set_mtime(&s, t0);
    stage_record(&sb, "exactE", Some("exact"));

    // Both concurrent runs' exits sweep the same shared-store transcript: its
    // mtime is inside each run's window, so each sweep would claim it.
    stamp_run_sessions("A", &projects, false, t0);
    stamp_run_sessions("B", &projects, false, t0);

    // The exact per-conversation observation is the attribution, whatever the
    // sweeps saw — and the listing shows it.
    let mut groups = groups_of(&["exactE"]);
    annotate_owners(&mut groups);
    assert_eq!(
        find(&groups, "exactE").unwrap().last_ran_profile.as_deref(),
        Some("exact")
    );
    assert_eq!(owner_of("exactE").as_deref(), Some("exact"));
    // The sweep skipped the id: no Known, no Contested stamp for it at all.
    let store = load_store(&store_path().unwrap());
    assert_eq!(store.sessions.get("exactE"), None);
}

#[test]
fn exact_observation_outranks_a_stale_store_entry() {
    let sb = HomeSandbox::new();
    // A store entry a pre-fix sweep stamped, naming a different profile than
    // the exact observation does today.
    let path = store_path().unwrap();
    let mut store = SessionProfiles::default();
    store
        .sessions
        .insert("exactE".into(), SessionOwner::Known("old".into()));
    save_store(&path, &store).unwrap();
    stage_record(&sb, "exactE", Some("exact"));

    let mut groups = groups_of(&["exactE"]);
    annotate_owners(&mut groups);
    assert_eq!(
        find(&groups, "exactE").unwrap().last_ran_profile.as_deref(),
        Some("exact"),
        "the exact observation is authoritative over a stale stored stamp"
    );
}

#[test]
fn sweep_still_stamps_ids_the_exact_writer_never_saw() {
    let sb = HomeSandbox::new();
    let projects = sb.home().join(".claude/projects");
    let main = projects.join("-w-nr/never.jsonl");
    let agent = projects.join("-w-nr/agent-1a2b3c4d.jsonl");
    write_jsonl(&main, &[user_line("never", "/w/nr", "unrecorded")]);
    write_jsonl(&agent, &[user_line("agent-1a2b3c4d", "/w/nr", "sub work")]);
    let t0 = SystemTime::UNIX_EPOCH + Duration::from_secs(10_000);
    set_mtime(&main, t0);
    set_mtime(&agent, t0);

    // No record for either id — the exact writer never saw them, so the sweep
    // stays their only observer, including the contested fold.
    stamp_run_sessions("A", &projects, false, t0);
    stamp_run_sessions("B", &projects, false, t0);

    let mut groups = groups_of(&["never", "agent-1a2b3c4d"]);
    annotate_owners(&mut groups);
    assert_eq!(find(&groups, "never").unwrap().last_ran_profile, None);
    assert_eq!(
        find(&groups, "agent-1a2b3c4d").unwrap().last_ran_profile,
        None
    );
    let store = load_store(&store_path().unwrap());
    assert_eq!(store.sessions.get("never"), Some(&SessionOwner::Contested));
    assert_eq!(
        store.sessions.get("agent-1a2b3c4d"),
        Some(&SessionOwner::Contested)
    );
}

#[test]
fn record_without_attribution_does_not_skip_the_sweep() {
    let sb = HomeSandbox::new();
    let projects = sb.home().join(".claude/projects");
    let s = projects.join("-w-bl/blankB.jsonl");
    write_jsonl(&s, &[user_line("blankB", "/w/bl", "attribution failed")]);
    let t0 = SystemTime::UNIX_EPOCH + Duration::from_secs(10_000);
    set_mtime(&s, t0);
    // The record exists but never attributed an account (an unattributable
    // reading): the exact writer does not own the id, so the sweep stamps it.
    stage_record(&sb, "blankB", None);

    stamp_run_sessions("B", &projects, false, t0);

    let store = load_store(&store_path().unwrap());
    assert_eq!(
        store.sessions.get("blankB"),
        Some(&SessionOwner::Known("B".into()))
    );
    assert_eq!(owner_of("blankB").as_deref(), Some("B"));
}

#[test]
fn annotate_owners_sets_only_known_entries() {
    let _sb = HomeSandbox::new();
    // Build the store directly: one Known, one Contested; "absent" is never
    // inserted. `atomic_write_600` creates the 0o700 `.clauth` dir as needed.
    let path = store_path().unwrap();
    let mut store = SessionProfiles::default();
    store
        .sessions
        .insert("known".into(), SessionOwner::Known("P".into()));
    store
        .sessions
        .insert("contest".into(), SessionOwner::Contested);
    save_store(&path, &store).unwrap();

    let mut groups = groups_of(&["known", "contest", "absent"]);
    annotate_owners(&mut groups);
    assert_eq!(
        find(&groups, "known").unwrap().last_ran_profile.as_deref(),
        Some("P")
    );
    assert_eq!(find(&groups, "contest").unwrap().last_ran_profile, None);
    assert_eq!(find(&groups, "absent").unwrap().last_ran_profile, None);
}

// ── owner-store prune ────────────────────────────────────────────────────────

/// The prune walks the full depth, not the resume-visible depth: a nested
/// per-session tree (`subagents/`) holds real transcripts deeper than
/// `TOP_LEVEL_DEPTH`, and pruning on the shallow walk would reap them. The
/// "gone" id has no transcript at all. Driven through `stamp_run_sessions`, the
/// production wiring, with both transcripts outside the run window so the prune
/// — not the fold — is the only writer.
#[test]
fn prune_removes_a_gone_transcript_and_keeps_a_nested_one() {
    let sb = HomeSandbox::new();
    let path = store_path().unwrap();
    let mut store = SessionProfiles::default();
    store
        .sessions
        .insert("gone".into(), SessionOwner::Known("A".into()));
    store
        .sessions
        .insert("agent-nested".into(), SessionOwner::Known("A".into()));
    save_store(&path, &store).unwrap();

    // A top-level transcript keeps the walk non-empty under a shallow-walk
    // mutation, so the empty-walk guard cannot mask it. The nested transcript
    // below is depth 5, invisible to TOP_LEVEL_DEPTH and visible to
    // WALK_MAX_DEPTH.
    let projects = global_projects(&sb);
    let top = projects.join("-w-n/top.jsonl");
    write_jsonl(&top, &[user_line("top", "/w/n", "top work")]);
    let nested = projects.join("-w-n/s-main/subagents/agent-nested.jsonl");
    write_jsonl(&nested, &[user_line("agent-nested", "/w/n", "sub work")]);

    // Both predate the run window, so the run folds no ids and the nested keep
    // can only come from the full-depth walk. A TOP_LEVEL_DEPTH walk would miss
    // `agent-nested` and reap it.
    let t0 = SystemTime::UNIX_EPOCH + Duration::from_secs(10_000);
    set_mtime(&top, t0);
    set_mtime(&nested, t0);
    stamp_run_sessions("shared", &projects, false, t0 + Duration::from_secs(60));

    let reloaded = load_store(&path);
    assert_eq!(
        reloaded.sessions.get("gone"),
        None,
        "a gone transcript is reaped"
    );
    assert_eq!(
        reloaded.sessions.get("agent-nested"),
        Some(&SessionOwner::Known("A".into())),
        "a transcript nested past the resume depth is still live"
    );
}

/// The grace keep: an id with no global transcript is kept while its main-scope
/// record last fired within the missing-transcript grace, and reaped once the
/// record has gone silent past it. Deleting the grace clause reaps the fresh id
/// too, since the walked tree holds no transcript for it.
#[test]
fn prune_keeps_a_grace_record_and_reaps_a_silent_one() {
    let sb = HomeSandbox::new();
    let path = store_path().unwrap();
    let mut store = SessionProfiles::default();
    store
        .sessions
        .insert("grace-fresh".into(), SessionOwner::Known("A".into()));
    store
        .sessions
        .insert("grace-silent".into(), SessionOwner::Known("A".into()));
    save_store(&path, &store).unwrap();

    // A non-empty global walk, so the empty-walk guard cannot mask the keep.
    let projects = global_projects(&sb);
    let top = projects.join("-w-top/top.jsonl");
    write_jsonl(&top, &[user_line("top", "/w/top", "top work")]);

    // Neither id has a transcript in the walked tree; the keep must come from
    // the record mtime alone. `grace-fresh` fired now, `grace-silent` long ago.
    stage_record(&sb, "grace-fresh", Some("A"));
    stage_record(&sb, "grace-silent", Some("A"));
    set_mtime(
        &sb.home().join(".clauth/conversations/grace-silent.json"),
        SystemTime::UNIX_EPOCH,
    );

    let mut paths = Vec::new();
    let walk_complete = collect_jsonl(&projects, WALK_MAX_DEPTH, &mut paths);
    assert!(walk_complete);

    let mut live = load_store(&path);
    prune_owner_store(&mut live, &paths, walk_complete);

    assert_eq!(
        live.sessions.get("grace-fresh"),
        Some(&SessionOwner::Known("A".into())),
        "a record fired within the grace keeps its owner with no transcript"
    );
    assert_eq!(
        live.sessions.get("grace-silent"),
        None,
        "a record silent past the grace is reaped"
    );
}

/// The isolated keep: an id whose only transcript sits in a LIVE isolated store
/// is kept even though the walked global tree has no transcript for it; the same
/// id is reaped once no live isolated store holds it. Deleting the isolated
/// clause reaps the id while its holder is still live.
#[test]
fn prune_keeps_a_live_isolated_owner_and_reaps_it_once_dead() {
    let sb = HomeSandbox::new();
    let path = store_path().unwrap();
    let mut store = SessionProfiles::default();
    store
        .sessions
        .insert("iso-hold".into(), SessionOwner::Known("A".into()));
    save_store(&path, &store).unwrap();

    // A non-empty global walk, so the empty-walk guard cannot mask the keep.
    let projects = global_projects(&sb);
    let top = projects.join("-w-top/top.jsonl");
    write_jsonl(&top, &[user_line("top", "/w/top", "top work")]);

    // The id's only transcript is inside a live isolated store.
    let iso = sb
        .home()
        .join(".clauth/profiles/iso/runtime-isolated/projects/-w-iso/iso-hold.jsonl");
    write_jsonl(&iso, &[user_line("iso-hold", "/w/iso", "iso work")]);
    let sessions_dir = sb.home().join(".clauth/profiles/iso/sessions-isolated");
    fs::create_dir_all(&sessions_dir).unwrap();
    let lock_file = crate::runtime::open_pid_file(&sessions_dir.join("12345")).unwrap();
    lock_file.lock().unwrap(); // held so the runtime reads as live

    let mut paths = Vec::new();
    let walk_complete = collect_jsonl(&projects, WALK_MAX_DEPTH, &mut paths);
    assert!(walk_complete);

    let mut live = load_store(&path);
    prune_owner_store(&mut live, &paths, walk_complete);
    assert_eq!(
        live.sessions.get("iso-hold"),
        Some(&SessionOwner::Known("A".into())),
        "a live isolated store keeps the owner with no global transcript"
    );

    // Release the liveness marker: the same store now holds nothing live.
    drop(lock_file);

    let mut live = load_store(&path);
    prune_owner_store(&mut live, &paths, walk_complete);
    assert_eq!(
        live.sessions.get("iso-hold"),
        None,
        "the owner is reaped once no live isolated store holds it"
    );
}

/// `collect_jsonl` fails soft on an unreadable dir by returning an empty vec,
/// which reads identically to "no transcripts exist" — pruning then would wipe
/// the store. The guard refuses an empty walk.
#[test]
fn prune_refuses_an_empty_walk() {
    let _sb = HomeSandbox::new();
    let path = store_path().unwrap();
    let mut store = SessionProfiles::default();
    store
        .sessions
        .insert("any-id".into(), SessionOwner::Known("A".into()));
    save_store(&path, &store).unwrap();

    // No global projects dir exists, so the walk returns zero paths.
    let mut live = load_store(&path);
    let changed = prune_owner_store(&mut live, &[], true);
    assert!(!changed, "an empty walk must be refused");
    assert_eq!(
        live.sessions.get("any-id"),
        Some(&SessionOwner::Known("A".into())),
        "the refused prune left the store untouched"
    );

    let reloaded = load_store(&path);
    assert_eq!(
        reloaded.sessions.get("any-id"),
        Some(&SessionOwner::Known("A".into())),
        "an empty walk must never wipe the store"
    );
}

/// The guard lives on the RUN, not on what the walk happened to return. An
/// isolated run walks a throwaway tree, so its prune must be skipped outright —
/// even while the global store is non-empty and would otherwise prune. Driven
/// through `stamp_run_sessions`, the production wiring.
#[test]
fn an_isolated_run_performs_no_prune() {
    let sb = HomeSandbox::new();
    let path = store_path().unwrap();
    let mut store = SessionProfiles::default();
    store
        .sessions
        .insert("gone".into(), SessionOwner::Known("A".into()));
    save_store(&path, &store).unwrap();

    // A non-empty GLOBAL walk, so dropping the isolated guard would prune
    // `gone`. The isolated run's own stamp walks a different tree.
    let global = global_projects(&sb).join("-w-live/keep.jsonl");
    write_jsonl(&global, &[user_line("keep", "/w/live", "still here")]);

    let projects = iso_projects(&sb);
    let iso_t = projects.join("-w-iso/isos.jsonl");
    write_jsonl(&iso_t, &[user_line("isos", "/w/iso", "iso work")]);
    let t0 = SystemTime::UNIX_EPOCH + Duration::from_secs(10_000);
    set_mtime(&iso_t, t0);

    stamp_run_sessions("iso", &projects, true, t0);

    let reloaded = load_store(&path);
    assert_eq!(
        reloaded.sessions.get("gone"),
        Some(&SessionOwner::Known("A".into())),
        "an isolated run must perform no prune"
    );
    assert_eq!(
        reloaded.sessions.get("isos"),
        Some(&SessionOwner::Known("iso".into())),
        "and it still stamps its own sessions"
    );
}

/// The prune is wired into the shared-run teardown leg: `stamp_run_sessions`
/// with `isolated == false` drops a stale owner alongside folding this run's
/// sessions in.
#[test]
fn a_shared_run_prunes_stale_owners() {
    let sb = HomeSandbox::new();
    let path = store_path().unwrap();
    let mut store = SessionProfiles::default();
    store
        .sessions
        .insert("gone".into(), SessionOwner::Known("A".into()));
    save_store(&path, &store).unwrap();

    let projects = global_projects(&sb);
    let fresh = projects.join("-w-new/freshS.jsonl");
    write_jsonl(&fresh, &[user_line("freshS", "/w/new", "new")]);
    let run_start = SystemTime::now();
    set_mtime(&fresh, run_start + Duration::from_secs(1));

    stamp_run_sessions("shared", &projects, false, run_start);

    let reloaded = load_store(&path);
    assert_eq!(
        reloaded.sessions.get("gone"),
        None,
        "the shared run pruned the stale owner"
    );
    assert_eq!(
        reloaded.sessions.get("freshS"),
        Some(&SessionOwner::Known("shared".into())),
        "and stamped this run's session"
    );
}

/// A shared teardown whose window caught no ids still prunes: the prune is not
/// gated on folding anything. A stale owner whose transcript is gone is reaped
/// even while the walk finds only transcripts outside the run's window.
#[test]
fn a_shared_run_with_no_in_window_ids_still_prunes() {
    let sb = HomeSandbox::new();
    let path = store_path().unwrap();
    let mut store = SessionProfiles::default();
    store
        .sessions
        .insert("gone".into(), SessionOwner::Known("A".into()));
    store
        .sessions
        .insert("old".into(), SessionOwner::Known("A".into()));
    save_store(&path, &store).unwrap();

    let projects = global_projects(&sb);
    // One transcript OUTSIDE the run's window, so the run folds no ids — but
    // the walk is non-empty, so the prune still runs and reaps `gone` (whose
    // transcript is absent) while keeping `old` (whose transcript the walk saw).
    let old = projects.join("-w-old/old.jsonl");
    write_jsonl(&old, &[user_line("old", "/w/old", "pre-window work")]);
    let t0 = SystemTime::UNIX_EPOCH + Duration::from_secs(10_000);
    set_mtime(&old, t0);

    stamp_run_sessions("shared", &projects, false, t0 + Duration::from_secs(60));

    let reloaded = load_store(&path);
    assert_eq!(
        reloaded.sessions.get("gone"),
        None,
        "a window with no ids still prunes the gone owner"
    );
    assert_eq!(
        reloaded.sessions.get("old"),
        Some(&SessionOwner::Known("A".into())),
        "and keeps the owner whose transcript the walk saw"
    );
}

/// The walk reports a subtree it could not read. The prune must refuse that
/// incomplete walk rather than reaping every owner whose transcript sat in the
/// unseen subtree — a partial walk reading as a complete one is a bulk reap.
#[cfg(unix)]
#[test]
fn prune_refuses_an_incomplete_walk() {
    use std::os::unix::fs::PermissionsExt;

    let sb = HomeSandbox::new();
    let path = store_path().unwrap();
    let mut store = SessionProfiles::default();
    store
        .sessions
        .insert("live".into(), SessionOwner::Known("A".into()));
    store
        .sessions
        .insert("gone".into(), SessionOwner::Known("A".into()));
    save_store(&path, &store).unwrap();

    let projects = global_projects(&sb);
    // A top-level transcript keeps the walk's non-empty guard from firing, so
    // the refusal below can only come from the incomplete guard.
    let top = projects.join("-w-p/top.jsonl");
    write_jsonl(&top, &[user_line("top", "/w/p", "top work")]);
    // A nested dir the walk cannot read, holding `gone`'s transcript: a bulk
    // reap would drop `gone` because the walk never saw it.
    let unreadable = projects.join("-w-p/s-main/subagents/unreadable-dir");
    write_jsonl(
        &unreadable.join("gone.jsonl"),
        &[user_line("gone", "/w/p", "hidden")],
    );
    fs::set_permissions(&unreadable, fs::Permissions::from_mode(0o000)).unwrap();
    if fs::read_dir(&unreadable).is_ok() {
        // Running with rights that ignore the mode (root): the pose cannot be
        // posed, so assert nothing rather than pass vacuously.
        fs::set_permissions(&unreadable, fs::Permissions::from_mode(0o700)).unwrap();
        return;
    }

    let mut paths = Vec::new();
    let walk_complete = collect_jsonl(&projects, WALK_MAX_DEPTH, &mut paths);
    assert!(
        !walk_complete,
        "the unreadable subtree marks the walk incomplete"
    );

    let mut live = load_store(&path);
    let changed = prune_owner_store(&mut live, &paths, walk_complete);
    assert!(
        !changed,
        "an incomplete walk must be refused, never bulk-reaped"
    );

    let reloaded = load_store(&path);
    assert_eq!(
        reloaded.sessions.get("gone"),
        Some(&SessionOwner::Known("A".into())),
        "the refused prune leaves the stale owner standing"
    );
    assert_eq!(
        reloaded.sessions.get("live"),
        Some(&SessionOwner::Known("A".into())),
        "and the live owner is untouched"
    );

    fs::set_permissions(&unreadable, fs::Permissions::from_mode(0o700)).unwrap();
}

/// A symlinked project dir hides its subtree from the walk, which never follows
/// links. `collect_jsonl` must mark that walk incomplete so the prune refuses
/// rather than reaping every owner whose transcript sits under the link.
#[cfg(unix)]
#[test]
fn prune_refuses_a_walk_that_skipped_a_symlinked_project_dir() {
    let sb = HomeSandbox::new();
    let path = store_path().unwrap();
    let mut store = SessionProfiles::default();
    store
        .sessions
        .insert("hidden".into(), SessionOwner::Known("A".into()));
    store
        .sessions
        .insert("top".into(), SessionOwner::Known("A".into()));
    save_store(&path, &store).unwrap();

    let projects = global_projects(&sb);
    let top = projects.join("-w-top/top.jsonl");
    write_jsonl(&top, &[user_line("top", "/w/top", "top work")]);

    // The linked-away project dir holds `hidden`'s only transcript.
    let outside = sb.home().join("linked-store");
    let hidden = outside.join("hidden.jsonl");
    write_jsonl(&hidden, &[user_line("hidden", "/w/link", "hidden work")]);
    std::os::unix::fs::symlink(&outside, projects.join("-w-link")).unwrap();

    let mut paths = Vec::new();
    let walk_complete = collect_jsonl(&projects, WALK_MAX_DEPTH, &mut paths);
    assert!(
        !walk_complete,
        "a skipped symlinked dir marks the walk incomplete"
    );

    let mut live = load_store(&path);
    let changed = prune_owner_store(&mut live, &paths, walk_complete);
    assert!(!changed, "an incomplete walk must be refused");
    assert_eq!(
        live.sessions.get("hidden"),
        Some(&SessionOwner::Known("A".into())),
        "the owner under the symlink is never reaped"
    );
}

/// A walk capped at depth 0 cannot descend into its target dir at all, so it did
/// not see everything under it and must report incomplete — the flag the prune
/// relies on to refuse rather than bulk-reap.
#[test]
fn collect_jsonl_reports_a_depth_cap_truncation() {
    let sb = HomeSandbox::new();
    let projects = global_projects(&sb);
    let deep = projects.join("-w-d/d1/deep.jsonl");
    write_jsonl(&deep, &[user_line("deep", "/w/d", "deep work")]);

    let mut paths = Vec::new();
    assert!(
        !collect_jsonl(&projects, 0, &mut paths),
        "a walk capped before it can descend did not see everything"
    );
}

// ── Session rescue: move an isolated transcript into the global store ─────────

/// Isolated `<profile>/runtime-isolated/projects` root under the sandbox.
fn iso_projects(sb: &HomeSandbox) -> PathBuf {
    sb.home()
        .join(".clauth/profiles/iso/runtime-isolated/projects")
}

/// The global `~/.claude/projects` root under the sandbox.
fn global_projects(sb: &HomeSandbox) -> PathBuf {
    sb.home().join(".claude/projects")
}

#[test]
fn rescue_moves_isolated_session_into_global_store_preserving_slug() {
    let sb = HomeSandbox::new();
    let iso_root = iso_projects(&sb);
    let global_root = global_projects(&sb);
    let src = iso_root.join("-w-iso/rescueme.jsonl");
    write_jsonl(&src, &[user_line("rescueme", "/w/iso", "hello from iso")]);
    let original = fs::read(&src).unwrap();

    let landed = rescue_session_transcript(&src, &iso_root, &global_root).unwrap();

    // Lands at the mirrored `<slug>/<id>.jsonl` in the global store.
    assert_eq!(landed, global_root.join("-w-iso/rescueme.jsonl"));
    assert_eq!(
        fs::read(&landed).unwrap(),
        original,
        "landed copy byte-identical"
    );
    assert!(!src.exists(), "source removed only after the verified copy");
}

#[test]
fn rescue_identical_target_drops_source_without_duplicating() {
    let sb = HomeSandbox::new();
    let iso_root = iso_projects(&sb);
    let global_root = global_projects(&sb);
    let src = iso_root.join("-w-iso/dup.jsonl");
    let target = global_root.join("-w-iso/dup.jsonl");
    let lines = [user_line("dup", "/w/iso", "same bytes both stores")];
    write_jsonl(&src, &lines);
    write_jsonl(&target, &lines);

    let landed = rescue_session_transcript(&src, &iso_root, &global_root).unwrap();

    assert_eq!(landed, target, "returns the existing target");
    assert!(!src.exists(), "source dropped (idempotent)");
    // No `<id>.rescued-N` sibling was created — the store holds exactly one copy.
    let siblings: Vec<String> = fs::read_dir(target.parent().unwrap())
        .unwrap()
        .filter_map(Result::ok)
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.contains("rescued"))
        .collect();
    assert!(siblings.is_empty(), "no duplicate created: {siblings:?}");
}

#[test]
fn rescue_differing_target_lands_beside_without_overwriting() {
    let sb = HomeSandbox::new();
    let iso_root = iso_projects(&sb);
    let global_root = global_projects(&sb);
    let src = iso_root.join("-w-iso/clash.jsonl");
    let target = global_root.join("-w-iso/clash.jsonl");
    write_jsonl(&src, &[user_line("clash", "/w/iso", "the rescued session")]);
    // A DIFFERENT session already holds the same id in the global store.
    write_jsonl(
        &target,
        &[user_line("clash", "/w/other", "a different session")],
    );
    let src_bytes = fs::read(&src).unwrap();
    let target_before = fs::read(&target).unwrap();

    let landed = rescue_session_transcript(&src, &iso_root, &global_root).unwrap();

    // Landed beside the original as `<id>.rescued-0.jsonl`.
    assert_eq!(landed, global_root.join("-w-iso/clash.rescued-0.jsonl"));
    assert_eq!(
        fs::read(&landed).unwrap(),
        src_bytes,
        "rescued content preserved"
    );
    // The pre-existing target is byte-for-byte untouched — the data-loss guard.
    assert_eq!(
        fs::read(&target).unwrap(),
        target_before,
        "existing target must never be overwritten"
    );
    assert!(!src.exists(), "source removed after the sibling landed");
}

#[test]
fn rescue_move_verifies_then_removes_and_noops_same_path() {
    let sb = HomeSandbox::new();
    let src = sb.home().join("src/a.jsonl");
    let dst = sb.home().join("dst/deep/a.jsonl");
    write_jsonl(&src, &[user_line("a", "/w", "payload")]);
    let original = fs::read(&src).unwrap();

    rescue_move(&src, &dst).unwrap();
    assert_eq!(
        fs::read(&dst).unwrap(),
        original,
        "dst matches src's original bytes"
    );
    assert!(!src.exists(), "src gone after the verified move");

    // Same-path no-op: the file must survive untouched.
    rescue_move(&dst, &dst).unwrap();
    assert_eq!(
        fs::read(&dst).unwrap(),
        original,
        "same-path no-op leaves file intact"
    );
    assert!(dst.exists());
}

/// `rescue_move` creates a not-yet-present parent dir. `~/.claude/sessions/` and
/// its kin must land owner-only like the files inside them, not at the process
/// umask (typically 0755) — a world-traversable tree still lets another local
/// user list session ids even though the files themselves stay 0600.
#[cfg(unix)]
#[test]
fn rescue_move_creates_parent_dir_owner_only() {
    use std::os::unix::fs::PermissionsExt;

    let sb = HomeSandbox::new();
    let src = sb.home().join("src/b.jsonl");
    let dst = sb.home().join("dst/deep/b.jsonl");
    write_jsonl(&src, &[user_line("b", "/w", "payload")]);

    rescue_move(&src, &dst).unwrap();

    let mode = fs::metadata(dst.parent().unwrap())
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(
        mode, 0o700,
        "a parent dir rescue_move creates must not land at the process umask"
    );
}

// ── Sidecar rescue: CC's session state next to the transcripts ──
//
// Roots here are the runtime ROOT (the CC config dir), not its `projects/`
// subdir: the sidecar leg walks everything else the isolated run wrote.

/// The isolated runtime root under the sandbox — `projects/`'s parent.
fn iso_root(sb: &HomeSandbox) -> PathBuf {
    sb.home().join(".clauth/profiles/iso/runtime-isolated")
}

/// The global CC config dir under the sandbox — `~/.claude/projects`'s parent.
fn global_root(sb: &HomeSandbox) -> PathBuf {
    sb.home().join(".claude")
}

fn write_file(path: &Path, body: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, body).unwrap();
}

/// The admission rule, pinned directly: the known session-state trees are
/// rescued, and everything else CC leaves in the config dir is not — including
/// the secret-bearing (`daemon/control.key`), snapshot-bearing (`backups/`) and
/// machine-scoped cache trees, which are CC-authored but not session state.
#[test]
fn rescuable_sidecar_admits_only_known_session_state_trees() {
    use std::ffi::OsStr;

    for tree in [
        "shell-snapshots",
        "file-history",
        "tasks",
        "plans",
        "sessions",
        "paste-cache",
        "session-env",
        "todos",
    ] {
        assert!(
            rescuable_sidecar(OsStr::new(tree)),
            "{tree} is session state a rescued session needs"
        );
    }
    for other in [
        ".claude.json",
        ".credentials.json",
        "backups",
        "daemon",
        "debug",
        "history.jsonl",
        "ide",
        "projects",
        "security",
        "settings.json",
        "statsig",
        "stats-cache.json",
        "telemetry",
        "some-future-cc-dir",
    ] {
        assert!(
            !rescuable_sidecar(OsStr::new(other)),
            "{other} must never be rescued"
        );
    }
}

/// A sidecar tree lands in the global store with its nesting and bytes intact,
/// merged per entry: the operator's own file in the same dir survives.
#[test]
fn sidecar_trees_land_in_global_store_with_contents_intact() {
    let sb = HomeSandbox::new();
    let iso = iso_root(&sb);
    let global = global_root(&sb);
    write_file(&iso.join("shell-snapshots/snapshot-bash-1.sh"), "iso shell");
    write_file(&iso.join("file-history/sess-a/edit-1.json"), "{\"e\":1}");
    write_file(&iso.join("plans/p1.md"), "the plan");
    // The operator's own snapshot dir already exists and must be merged into.
    write_file(&global.join("shell-snapshots/mine.sh"), "operator shell");

    let moved = rescue_isolated_sidecars(&iso, &global);

    assert_eq!(moved, 3, "three sidecar files moved");
    assert_eq!(
        fs::read_to_string(global.join("shell-snapshots/snapshot-bash-1.sh")).unwrap(),
        "iso shell"
    );
    assert_eq!(
        fs::read_to_string(global.join("file-history/sess-a/edit-1.json")).unwrap(),
        "{\"e\":1}",
        "nesting under the tree is preserved"
    );
    assert_eq!(
        fs::read_to_string(global.join("plans/p1.md")).unwrap(),
        "the plan"
    );
    assert_eq!(
        fs::read_to_string(global.join("shell-snapshots/mine.sh")).unwrap(),
        "operator shell",
        "the operator's own entry in a merged dir is untouched"
    );
    assert!(
        !iso.join("shell-snapshots/snapshot-bash-1.sh").exists(),
        "sources moved, not copied"
    );
    assert!(!iso.join("plans/p1.md").exists());
}

/// Per-entry collision safety: a differing global entry is never overwritten
/// (the rescue lands beside it), a byte-identical one is deduped, and an
/// extension-less name keeps its shape.
#[test]
fn sidecar_collision_lands_beside_without_clobbering() {
    let sb = HomeSandbox::new();
    let iso = iso_root(&sb);
    let global = global_root(&sb);
    write_file(&iso.join("tasks/t1.json"), "iso task");
    write_file(&global.join("tasks/t1.json"), "operator task");
    write_file(&iso.join("tasks/same.json"), "identical");
    write_file(&global.join("tasks/same.json"), "identical");
    write_file(&iso.join("session-env/envfile"), "ISO=1");
    write_file(&global.join("session-env/envfile"), "OPERATOR=1");

    let moved = rescue_isolated_sidecars(&iso, &global);

    // Counts entries whose state ended up in the global store, matching the
    // transcript leg — the deduped one is there too, by the copy already present.
    assert_eq!(moved, 3);
    assert_eq!(
        fs::read_to_string(global.join("tasks/t1.json")).unwrap(),
        "operator task",
        "an occupied entry is never overwritten"
    );
    assert_eq!(
        fs::read_to_string(global.join("tasks/t1.rescued-0.json")).unwrap(),
        "iso task",
        "the rescue lands beside it"
    );
    assert_eq!(
        fs::read_to_string(global.join("session-env/envfile")).unwrap(),
        "OPERATOR=1"
    );
    assert_eq!(
        fs::read_to_string(global.join("session-env/envfile.rescued-0")).unwrap(),
        "ISO=1",
        "an extension-less name gains no invented extension"
    );
    // The identical pair collapsed to the one existing copy, no sibling.
    assert_eq!(
        fs::read_to_string(global.join("tasks/same.json")).unwrap(),
        "identical"
    );
    assert!(!global.join("tasks/same.rescued-0.json").exists());
    assert!(!iso.join("tasks/same.json").exists(), "duplicate dropped");
}

/// The allowlist on disk: clauth-owned state, `projects/` (the transcript
/// leg's), the secret- and cache-bearing CC trees and the top-level singleton
/// files all stay in the isolated tree, to be discarded with it.
#[test]
fn sidecar_rescue_leaves_everything_off_the_allowlist() {
    let sb = HomeSandbox::new();
    let iso = iso_root(&sb);
    let global = global_root(&sb);
    let left = [
        ".credentials.json",
        "settings.json",
        ".claude.json",
        "history.jsonl",
        "projects/-w-iso/s1.jsonl",
        "daemon/control.key",
        "backups/.claude.json.backup.1784537349681",
        "security/agent-sdk-venv/pyvenv.cfg",
        "statsig/statsig.session_id.2656965060",
        "ide/12345.lock",
        "debug/mcp-logs/log.txt",
        "telemetry/events.jsonl",
    ];
    for path in left {
        write_file(&iso.join(path), "content");
    }

    let moved = rescue_isolated_sidecars(&iso, &global);

    assert_eq!(moved, 0, "nothing off the allowlist moves");
    for path in left {
        assert!(iso.join(path).exists(), "{path} stays in the isolated tree");
        assert!(
            !global.join(path).exists(),
            "{path} must never land in the global store"
        );
    }
    // Not even the containing dirs are created in the operator's store.
    for dir in [
        "daemon",
        "backups",
        "security",
        "statsig",
        "ide",
        "telemetry",
    ] {
        assert!(!global.join(dir).exists(), "{dir} was created globally");
    }
}

/// A SOURCE symlink is skipped, never followed: an isolated runtime links
/// nothing, so a link is anomalous and walking one could move the operator's own
/// store out from under them. Both walk levels guard it, so both are exercised —
/// an allowlisted name at the top, and an entry inside a rescued tree.
#[cfg(unix)]
#[test]
fn sidecar_rescue_never_follows_a_symlink_into_the_global_store() {
    for link_at in ["sessions", "tasks/link"] {
        let sb = HomeSandbox::new();
        let iso = iso_root(&sb);
        let global = global_root(&sb);
        write_file(&global.join("projects/-w-real/keep.jsonl"), "operator data");
        fs::create_dir_all(iso.join("tasks")).unwrap();
        std::os::unix::fs::symlink(global.join("projects"), iso.join(link_at)).unwrap();

        let moved = rescue_isolated_sidecars(&iso, &global);

        assert_eq!(moved, 0, "a symlinked tree at {link_at} is not walked");
        assert_eq!(
            fs::read_to_string(global.join("projects/-w-real/keep.jsonl")).unwrap(),
            "operator data",
            "the operator's store is untouched"
        );
        assert!(
            !global.join(link_at).exists(),
            "nothing lands through the link"
        );
    }
}

/// A DESTINATION entry that is a symlink is left alone: the real `~/.claude`
/// holds operator links pointing outside the store (`skills -> ~/.agents/…`),
/// and writing through one would land rescued files in the operator's repos.
#[cfg(unix)]
#[test]
fn sidecar_rescue_never_writes_through_a_symlinked_destination() {
    let sb = HomeSandbox::new();
    let iso = iso_root(&sb);
    let global = global_root(&sb);
    let outside = sb.home().join("elsewhere");
    fs::create_dir_all(&outside).unwrap();
    fs::create_dir_all(&global).unwrap();
    // A whole linked-away tree, and a linked single entry inside a real tree.
    std::os::unix::fs::symlink(&outside, global.join("plans")).unwrap();
    write_file(&iso.join("plans/p1.md"), "iso plan");
    write_file(&iso.join("tasks/t1.json"), "iso task");
    fs::create_dir_all(global.join("tasks")).unwrap();
    std::os::unix::fs::symlink(outside.join("t1.json"), global.join("tasks/t1.json")).unwrap();

    let moved = rescue_isolated_sidecars(&iso, &global);

    assert_eq!(moved, 0, "neither link is written through");
    assert!(
        !outside.join("p1.md").exists() && !outside.join("t1.json").exists(),
        "nothing escaped the global store"
    );
    assert!(iso.join("plans/p1.md").exists(), "sources stay put");
    assert!(iso.join("tasks/t1.json").exists());
}

/// The depth cap, both directions: a tree at the deepest ALLOWED nesting still
/// moves, and one level past it is truncated with a log rather than recursed —
/// what the cap drops is state in a tree about to be discarded.
#[test]
fn sidecar_rescue_moves_up_to_the_depth_cap_and_stops_past_it() {
    let sb = HomeSandbox::new();
    let iso = iso_root(&sb);
    let global = global_root(&sb);
    // `file-history` itself is level 1, so its deepest reachable leaf sits at
    // SIDECAR_MAX_DEPTH counting from the runtime root.
    let mut deepest = iso.join("file-history");
    for _ in 0..(SIDECAR_MAX_DEPTH - 2) {
        deepest = deepest.join("d");
    }
    write_file(&deepest.join("leaf.json"), "at the cap");
    write_file(&deepest.join("d/too-deep.json"), "past the cap");
    write_file(&iso.join("file-history/shallow.json"), "near the top");

    let moved = rescue_isolated_sidecars(&iso, &global);

    assert_eq!(moved, 2, "everything within the cap moves, nothing past it");
    let landed = deepest
        .strip_prefix(&iso)
        .map(|rel| global.join(rel))
        .unwrap();
    assert_eq!(
        fs::read_to_string(landed.join("leaf.json")).unwrap(),
        "at the cap",
        "the deepest allowed leaf still lands"
    );
    assert_eq!(
        fs::read_to_string(global.join("file-history/shallow.json")).unwrap(),
        "near the top"
    );
    assert!(
        deepest.join("d/too-deep.json").exists(),
        "one level past the cap is left in the isolated tree"
    );
    assert!(!landed.join("d").exists(), "and nothing lands for it");
}

/// The transcript leg carries modes too — that is where the real store keeps
/// thousands of 0600 files, and the leg predates the sidecar work.
#[cfg(unix)]
#[test]
fn transcript_rescue_preserves_the_source_file_mode() {
    use std::os::unix::fs::PermissionsExt;

    let sb = HomeSandbox::new();
    let iso = iso_projects(&sb);
    let global = global_projects(&sb);
    let src = iso.join("-w-iso/s1.jsonl");
    write_jsonl(&src, &[user_line("s1", "/w/iso", "owner-only transcript")]);
    fs::set_permissions(&src, fs::Permissions::from_mode(0o600)).unwrap();

    assert_eq!(rescue_isolated_store(&iso, &global), 1);

    let mode = fs::metadata(global.join("-w-iso/s1.jsonl"))
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(
        mode, 0o600,
        "a 0600 transcript must not land world-readable"
    );
}

/// Modes are carried over, not recreated: CC writes transcripts and paste-cache
/// entries 0600, and a umask-masked 0644 would publish them to every account on
/// the machine.
#[cfg(unix)]
#[test]
fn rescue_preserves_the_source_file_mode() {
    use std::os::unix::fs::PermissionsExt;

    let sb = HomeSandbox::new();
    let iso = iso_root(&sb);
    let global = global_root(&sb);
    write_file(&iso.join("paste-cache/secret"), "pasted content");
    write_file(&iso.join("shell-snapshots/snap.sh"), "#!/bin/sh");
    fs::set_permissions(
        iso.join("paste-cache/secret"),
        fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    fs::set_permissions(
        iso.join("shell-snapshots/snap.sh"),
        fs::Permissions::from_mode(0o755),
    )
    .unwrap();

    assert_eq!(rescue_isolated_sidecars(&iso, &global), 2);

    let mode = |p: PathBuf| fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(
        mode(global.join("paste-cache/secret")),
        0o600,
        "an owner-only source must not land world-readable"
    );
    assert_eq!(
        mode(global.join("shell-snapshots/snap.sh")),
        0o755,
        "the mode is copied, not hardcoded"
    );
}

// ── targeted lookup (`clauth resume`/`info` and the `delegate` resume path) ──

/// `--resume` only resolves a session under its own workspace, so the delegate
/// path needs one transcript's recorded `cwd` and nothing else. This lookup is
/// deliberately not `build_index`, which head- and tail-reads every transcript in
/// the store to build previews it would then throw away.
#[test]
fn workspace_of_finds_a_sessions_recorded_cwd() {
    let sb = HomeSandbox::new();
    write_jsonl(
        &sb.home().join(".claude/projects/-w-res/sres.jsonl"),
        &[user_line("sres", "/w/res", "hello")],
    );
    write_jsonl(
        &sb.home().join(".claude/projects/-w-other/sother.jsonl"),
        &[user_line("sother", "/w/other", "unrelated")],
    );

    assert_eq!(
        workspace_of("sres"),
        Some(PathBuf::from("/w/res")),
        "the workspace comes from the transcript, not from the lossy dir slug"
    );
    assert_eq!(
        workspace_of("sother"),
        Some(PathBuf::from("/w/other")),
        "a store with several sessions still resolves each to its own"
    );
    assert_eq!(workspace_of("nosuchsession"), None);
}

/// A transcript recording no `cwd` leaves a resume nowhere to run, which is the
/// same dead end as no transcript at all.
#[test]
fn workspace_of_treats_a_cwd_less_transcript_as_unresolvable() {
    let sb = HomeSandbox::new();
    write_jsonl(
        &sb.home().join(".claude/projects/-w-bare/sbare.jsonl"),
        &[json!({"sessionId": "sbare", "message": {"role": "user", "content": "hi"}}).to_string()],
    );
    assert_eq!(workspace_of("sbare"), None);
}

/// A duplicated id must resolve to the same file the index would pick, or
/// `clauth info` prints one path while `clauth sessions` lists another.
#[test]
fn find_session_picks_the_same_duplicate_the_index_does() {
    let sb = HomeSandbox::new();
    let old = sb.home().join(".claude/projects/-w-a/sdup.jsonl");
    let new = sb.home().join(".claude/projects/-w-b/sdup.jsonl");
    write_jsonl(&old, &[user_line("sdup", "/w/a", "older copy")]);
    write_jsonl(&new, &[user_line("sdup", "/w/b", "newer copy")]);
    set_mtime(&old, SystemTime::UNIX_EPOCH + Duration::from_secs(1_000));
    set_mtime(&new, SystemTime::UNIX_EPOCH + Duration::from_secs(2_000));

    let found = find_session("sdup").expect("the id resolves");
    assert_eq!(found.path, new, "the newest copy of an id wins");
    assert_eq!(
        found.workspace(),
        Some(PathBuf::from("/w/b")),
        "the workspace comes from the file that won"
    );
    let indexed = build_index();
    assert_eq!(
        found.workspace(),
        find(&indexed, "sdup").map(|s| PathBuf::from(&s.workspace)),
        "the targeted lookup and the index must agree on one id"
    );

    // Equal mtimes fall back to the greater path, matching `insert_newest`.
    set_mtime(&old, SystemTime::UNIX_EPOCH + Duration::from_secs(2_000));
    assert_eq!(
        find_session("sdup").map(|f| f.path),
        Some(new),
        "an mtime tie takes the lexicographically greater path"
    );

    assert!(find_session("nosuchsession").is_none());
}

/// The `latest` target. Its ordering is the index's own newest-first key, which
/// `sessions_cli` pins against the emitted listing.
#[test]
fn newest_session_takes_the_greatest_mtime_then_the_smallest_id() {
    let sb = HomeSandbox::new();
    // The smaller id sits at the smaller path, so the id rule and the path rule
    // below it disagree: a fixture where they agree stays green even with the
    // id tie-break deleted outright.
    let smaller_id = sb.home().join(".claude/projects/-w-a/s-aaa.jsonl");
    let greater_path = sb.home().join(".claude/projects/-w-b/s-bbb.jsonl");
    write_jsonl(&smaller_id, &[user_line("s-aaa", "/w/a", "aaa")]);
    write_jsonl(&greater_path, &[user_line("s-bbb", "/w/b", "bbb")]);
    set_mtime(
        &smaller_id,
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_000),
    );
    set_mtime(
        &greater_path,
        SystemTime::UNIX_EPOCH + Duration::from_secs(2_000),
    );

    assert_eq!(
        newest_session().map(|s| s.id).as_deref(),
        Some("s-bbb"),
        "the greatest mtime wins over both tie-breaks"
    );

    // On an mtime tie the index's own key breaks it by smallest id, so the
    // pick can't depend on `read_dir` order.
    set_mtime(
        &smaller_id,
        SystemTime::UNIX_EPOCH + Duration::from_secs(2_000),
    );
    assert_eq!(
        newest_session().map(|s| s.id).as_deref(),
        Some("s-aaa"),
        "an mtime tie takes the smallest id, not the greater path"
    );
}

/// Claude Code resolves `--resume` by UUID or session title, so the stems of the
/// transcripts it nests under a conversation (`agent-<hex>`, a workflow's
/// `journal`) match nothing: `--print` errors out and the interactive path opens
/// the session picker with no match (CC 2.1.221). `latest` has to end in a
/// session CC will open, so it takes the newest of those even when a nested file
/// is newer — while the listing and the exact-id lookup keep carrying them.
#[test]
fn newest_session_skips_the_nested_transcripts_a_resume_cannot_open() {
    let sb = HomeSandbox::new();
    let projects = sb.home().join(".claude/projects/-w-a");
    let top = projects.join("s-top.jsonl");
    let agent = projects.join("s-top/subagents/agent-a0722a48260618b8a.jsonl");
    let journal = projects.join("s-top/subagents/workflows/wf-1/journal.jsonl");
    write_jsonl(&top, &[user_line("s-top", "/w/a", "top")]);
    write_jsonl(&agent, &[user_line("s-top", "/w/a", "subagent")]);
    write_jsonl(&journal, &[user_line("s-top", "/w/a", "workflow")]);
    set_mtime(&top, SystemTime::UNIX_EPOCH + Duration::from_secs(1_000));
    set_mtime(&agent, SystemTime::UNIX_EPOCH + Duration::from_secs(9_000));
    set_mtime(
        &journal,
        SystemTime::UNIX_EPOCH + Duration::from_secs(8_000),
    );

    assert_eq!(
        newest_session().map(|s| s.id).as_deref(),
        Some("s-top"),
        "`latest` is the newest RESUMABLE session, not the newest file"
    );
    assert_eq!(
        find_session("agent-a0722a48260618b8a").map(|s| s.path),
        Some(agent),
        "the exact-id lookup still reaches a nested transcript"
    );
    assert!(
        find(&build_index(), "agent-a0722a48260618b8a").is_some(),
        "and so does the listing"
    );
}

/// A live isolated store is invisible to the lookups, so its transcripts have to
/// be reachable some other way — otherwise a resume can only report a session
/// `clauth sessions` just listed as "not found".
#[test]
fn live_isolated_holds_reports_what_the_lookup_excludes() {
    let sb = HomeSandbox::new();
    let iso = sb
        .home()
        .join(".clauth/profiles/iso/runtime-isolated/projects/-w-iso/siso.jsonl");
    write_jsonl(&iso, &[user_line("siso", "/w/iso", "hi iso")]);
    set_mtime(&iso, SystemTime::UNIX_EPOCH + Duration::from_secs(5_000));
    let global = sb.home().join(".claude/projects/-w-g/sglobal.jsonl");
    write_jsonl(&global, &[user_line("sglobal", "/w/g", "hi global")]);

    let sessions_dir = sb.home().join(".clauth/profiles/iso/sessions-isolated");
    fs::create_dir_all(&sessions_dir).unwrap();
    let lock_file = crate::runtime::open_pid_file(&sessions_dir.join("12345")).unwrap();
    lock_file.lock().unwrap(); // held so the runtime reads as live

    let holds = live_isolated_holds();
    drop(lock_file);

    assert_eq!(holds.len(), 1, "the global store is not a hold");
    assert_eq!(holds[0].session.id, "siso");
    assert_eq!(holds[0].profile, "iso", "the owning profile is named");
    assert_eq!(holds[0].session.path, iso);
    assert_eq!(
        holds[0].session.updated,
        SystemTime::UNIX_EPOCH + Duration::from_secs(5_000),
        "the mtime a `latest` comparison needs comes back with it"
    );
}

/// A dead isolated runtime's store is GC territory, not a hold: reporting one
/// would tell the operator to wait for a run that already ended.
#[test]
fn live_isolated_holds_ignores_a_runtime_with_no_live_session() {
    let sb = HomeSandbox::new();
    write_jsonl(
        &sb.home()
            .join(".clauth/profiles/iso/runtime-isolated/projects/-w-iso/sdead.jsonl"),
        &[user_line("sdead", "/w/iso", "over")],
    );
    fs::create_dir_all(sb.home().join(".clauth/profiles/iso/sessions-isolated")).unwrap();

    assert!(live_isolated_holds().is_empty());
}

/// The targeted lookup reads the GLOBAL store only, unlike `build_index`.
/// `clauth resume` spawns against the shared store, so an id that lives only in
/// a live isolated runtime is one Claude Code would answer `No conversation
/// found` for — refusing it by name beats spawning a session that can't work.
#[test]
fn the_targeted_lookup_never_reaches_a_live_isolated_store() {
    let sb = HomeSandbox::new();
    let iso = sb
        .home()
        .join(".clauth/profiles/iso/runtime-isolated/projects/-w-iso/siso.jsonl");
    write_jsonl(&iso, &[user_line("siso", "/w/iso", "hi iso")]);
    let sessions_dir = sb.home().join(".clauth/profiles/iso/sessions-isolated");
    fs::create_dir_all(&sessions_dir).unwrap();
    let lock_file = crate::runtime::open_pid_file(&sessions_dir.join("12345")).unwrap();
    lock_file.lock().unwrap(); // held so the runtime reads as live

    // The index does see it, so the fixture is a live isolated store and not a
    // dead one the walk would have skipped anyway.
    assert!(
        find(&build_index(), "siso").is_some(),
        "the index covers a live isolated store"
    );
    assert!(find_session("siso").is_none(), "the lookup does not");
    assert!(newest_session().is_none(), "not even as the newest session");
    drop(lock_file);
}
