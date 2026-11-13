#![allow(clippy::unwrap_used, clippy::expect_used)]

//! `GET /api/v1/sessions` and `GET /api/v1/sessions/{id}`: the page cut, the
//! cursor round trip, redaction, the verbatim history page, the id lookup.
//!
//! Every store lives under a [`HomeSandbox`]: the global one at
//! `~/.claude/projects`, a live isolated one under its profile's runtime with
//! the pid lock held so the runtime reads as live.

#![cfg(unix)]

use super::*;

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::daemon::api::devices::Tier;
use crate::testutil::{
    HomeSandbox, OTHER_TOKEN, TOKEN, body_json, call, ctx_with, req, seed_device, set_mtime,
};

const HISTORY: &[u8] = include_bytes!("../fixtures/sessions/history.jsonl");
const HISTORY_PAGE_1: &str = include_str!("../fixtures/sessions/history-page-1.json");
const HISTORY_PAGE_2: &str = include_str!("../fixtures/sessions/history-page-2.json");
const LISTING_PAGE_1: &str = include_str!("../fixtures/sessions/listing-page-1.json");
const LISTING_PAGE_2: &str = include_str!("../fixtures/sessions/listing-page-2.json");
/// The transcript stem `history.jsonl` is stored under: the `agent_session`
/// id the `w1N:p19` fixture pane carries, so the pane-to-history join is one
/// real id end to end.
const HISTORY_ID: &str = "1cb26556-3532-45e1-8b39-37f0b53a8e4f";

fn config() -> crate::profile::ConfigHandle {
    std::sync::Arc::new(crate::lockorder::RankedMutex::new(
        crate::profile::AppConfig {
            state: crate::profile::AppState::default(),
            profiles: Vec::new(),
        },
    ))
}

fn ctx() -> std::sync::Arc<ApiContext> {
    ctx_with(config())
}

fn write_lines(path: &Path, lines: &[String]) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, lines.join("\n")).unwrap();
}

fn user_line(sid: &str, cwd: &str, text: &str) -> String {
    serde_json::json!({"sessionId": sid, "cwd": cwd, "message": {"role": "user", "content": text}})
        .to_string()
}

/// A global-store transcript with the given mtime.
fn global_session(sb: &HomeSandbox, id: &str, lines: &[String], mtime_secs: u64) -> PathBuf {
    let path = sb
        .home()
        .join(format!(".claude/projects/-w-app/{id}.jsonl"));
    write_lines(&path, lines);
    set_mtime(
        &path,
        SystemTime::UNIX_EPOCH + Duration::from_secs(mtime_secs),
    );
    path
}

/// A transcript in a live isolated runtime's own store; the returned lock is
/// what makes the runtime read as live, so the caller holds it for the call.
fn live_isolated_session(
    sb: &HomeSandbox,
    profile: &str,
    id: &str,
    lines: &[String],
    mtime_secs: u64,
) -> fs::File {
    let path = sb.home().join(format!(
        ".clauth/profiles/{profile}/runtime-isolated/projects/-w-iso/{id}.jsonl"
    ));
    write_lines(&path, lines);
    set_mtime(
        &path,
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

/// The captured transcript, stored under the global store as [`HISTORY_ID`].
fn seed_history(sb: &HomeSandbox) -> PathBuf {
    let path = sb.home().join(format!(
        ".claude/projects/-home-user-repos-app/{HISTORY_ID}.jsonl"
    ));
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, HISTORY).unwrap();
    path
}

fn body_text(resp: &Response) -> String {
    String::from_utf8(resp.body.clone()).expect("a utf-8 body")
}

/// Two pages of two rows across a global store and a live isolated store:
/// the listing order, the `store` tag, the owner stamp, a redacted preview,
/// no `tokens`/`cost`, and the cursor carrying the page boundary exactly.
#[test]
fn the_listing_pages_newest_first_across_both_stores_with_a_round_trip_cursor() {
    let sb = HomeSandbox::new();
    let ctx = ctx();
    global_session(
        &sb,
        "s-newest",
        &[
            user_line(
                "s-newest",
                "/w/app",
                "start sk-ant-api03-AbCdEfGhIjKlMnOpQrStUvWxYz0123456789 here",
            ),
            user_line("s-newest", "/w/app", "end"),
        ],
        3_000,
    );
    global_session(
        &sb,
        "s-middle",
        &[user_line("s-middle", "/w/app", "middle question")],
        2_000,
    );
    let mut oldest = global_session(
        &sb,
        "s-oldest",
        &[user_line("s-oldest", "/w/old", "old")],
        1_000,
    );
    oldest.set_file_name("s-tie.jsonl");
    write_lines(&oldest, &[user_line("s-tie", "/w/old", "tie")]);
    set_mtime(&oldest, SystemTime::UNIX_EPOCH + Duration::from_secs(1_000));
    let lock = live_isolated_session(
        &sb,
        "iso",
        "s-iso",
        &[user_line("s-iso", "/w/iso", "hi iso")],
        1_500,
    );
    crate::sessions::stamp_exact_owner("s-middle", "alpha");

    let first = call(
        &ctx,
        &req("GET", "/api/v1/sessions?limit=2", Some(TOKEN), ""),
    );
    assert_eq!(first.status, 200);
    assert_eq!(body_text(&first), LISTING_PAGE_1.trim_end());
    let first_body = body_json(&first);
    crate::testutil::schema_agrees_with_type::<SessionsBody>(&first_body);
    let cursor = first_body["next_before"]
        .as_str()
        .expect("a cursor")
        .to_string();

    let second = call(
        &ctx,
        &req(
            "GET",
            &format!("/api/v1/sessions?limit=2&before={cursor}"),
            Some(TOKEN),
            "",
        ),
    );
    assert_eq!(second.status, 200);
    assert_eq!(body_text(&second), LISTING_PAGE_2.trim_end());
    let cursor = body_json(&second)["next_before"]
        .as_str()
        .expect("a cursor")
        .to_string();

    let third = call(
        &ctx,
        &req(
            "GET",
            &format!("/api/v1/sessions?before={cursor}"),
            Some(TOKEN),
            "",
        ),
    );
    assert_eq!(third.status, 200);
    let third_body = body_json(&third);
    assert_eq!(
        third_body,
        serde_json::json!({
            "ok": true,
            "sessions": [{
                "id": "s-tie",
                "last_ran_profile": null,
                "workspace": "/w/old",
                "updated": "1970-01-01T00:16:40+00:00",
                "first_message": "tie",
                "last_message": "tie",
                "title": null,
                "git_branch": null,
                "store": "global",
                "via": [],
                "lacks": [],
            }],
            "next_before": null,
        })
    );
    drop(lock);

    // A view device reads the same page.
    seed_device("phone", Tier::View, OTHER_TOKEN);
    let viewed = call(
        &ctx,
        &req("GET", "/api/v1/sessions?limit=2", Some(OTHER_TOKEN), ""),
    );
    assert_eq!(viewed.status, 200);
    assert_eq!(body_text(&viewed), LISTING_PAGE_1.trim_end());
}

/// A row carries the session's name, the newest `custom-title` record Claude
/// Code wrote (sanitized like a preview), found even when it sits before the
/// last user message; and the git branch off the newest user record that
/// names one. A session that wrote neither carries `null` for both.
#[test]
fn a_row_carries_the_newest_title_and_git_branch() {
    let sb = HomeSandbox::new();
    let ctx = ctx();
    let titled = |title: &str| {
        serde_json::json!({"type": "custom-title", "customTitle": title, "sessionId": "s-named"})
            .to_string()
    };
    let on_branch = |text: &str, branch: &str| {
        serde_json::json!({"sessionId": "s-named", "cwd": "/w/app", "gitBranch": branch, "message": {"role": "user", "content": text}})
            .to_string()
    };
    global_session(
        &sb,
        "s-named",
        &[
            titled("old-name"),
            on_branch("first", "main"),
            titled("new\u{1b}name"),
            on_branch("last", "mommy"),
        ],
        2_000,
    );
    global_session(
        &sb,
        "s-plain",
        &[user_line("s-plain", "/w/app", "hi")],
        1_000,
    );

    let listed = call(&ctx, &req("GET", "/api/v1/sessions", Some(TOKEN), ""));
    assert_eq!(listed.status, 200);
    let rows = body_json(&listed)["sessions"].clone();
    assert_eq!(rows[0]["id"], "s-named");
    assert_eq!(rows[0]["title"], "new name");
    assert_eq!(rows[0]["git_branch"], "mommy");
    assert_eq!(rows[0]["last_message"], "last");
    assert_eq!(rows[1]["id"], "s-plain");
    assert_eq!(rows[1]["title"], serde_json::Value::Null);
    assert_eq!(rows[1]["git_branch"], serde_json::Value::Null);
}

/// The newest `custom-title` record decides even when it names nothing usable:
/// the row reads `null`, never an older name. The branch comes from the newest
/// user record that names one, so a newer record without a branch falls back
/// to an older one; it reaches the phone with control and bidi characters
/// dropped and nothing else changed, so it still names the branch.
#[test]
fn a_row_skips_an_unnamed_newest_title_and_a_branchless_newest_message() {
    let sb = HomeSandbox::new();
    let ctx = ctx();
    let titled = |title: &str| {
        serde_json::json!({"type": "custom-title", "customTitle": title, "sessionId": "s-odd"})
            .to_string()
    };
    let user = |text: &str, branch: Option<&str>| {
        let mut record = serde_json::json!({"sessionId": "s-odd", "cwd": "/w/app", "message": {"role": "user", "content": text}});
        if let Some(branch) = branch {
            record["gitBranch"] = serde_json::json!(branch);
        }
        record.to_string()
    };
    global_session(
        &sb,
        "s-odd",
        &[
            titled("kept-name"),
            user("first", Some("fix\u{202e}evil")),
            titled("\u{1b}"),
            user("middle", Some("")),
            user("last", None),
            serde_json::json!({"sessionId": "s-odd", "type": "assistant", "gitBranch": "other", "message": {"role": "assistant", "content": "x"}})
                .to_string(),
        ],
        2_000,
    );

    let listed = call(&ctx, &req("GET", "/api/v1/sessions", Some(TOKEN), ""));
    assert_eq!(listed.status, 200);
    let row = body_json(&listed)["sessions"][0].clone();
    assert_eq!(row["id"], "s-odd");
    assert_eq!(row["last_message"], "last");
    assert_eq!(row["title"], serde_json::Value::Null);
    assert_eq!(row["git_branch"], "fixevil");
}

/// `next_before` is `null` on the last page that carries rows, never only on
/// an empty page after it: a store of exactly `limit` rows is one page, and
/// one of `limit + 1` rows ends on a one-row second page.
#[test]
fn next_before_is_null_on_the_last_page_that_carries_rows() {
    let sb = HomeSandbox::new();
    let ctx = ctx();
    global_session(&sb, "s-a", &[user_line("s-a", "/w", "a")], 2_000);
    global_session(&sb, "s-b", &[user_line("s-b", "/w", "b")], 1_000);

    let exact = body_json(&call(
        &ctx,
        &req("GET", "/api/v1/sessions?limit=2", Some(TOKEN), ""),
    ));
    let ids = |page: &serde_json::Value| -> Vec<String> {
        page["sessions"]
            .as_array()
            .expect("rows")
            .iter()
            .map(|row| row["id"].as_str().expect("id").to_string())
            .collect()
    };
    assert_eq!(ids(&exact), vec!["s-a", "s-b"]);
    assert_eq!(exact["next_before"], serde_json::Value::Null);

    global_session(&sb, "s-c", &[user_line("s-c", "/w", "c")], 500);
    let first = body_json(&call(
        &ctx,
        &req("GET", "/api/v1/sessions?limit=2", Some(TOKEN), ""),
    ));
    assert_eq!(ids(&first), vec!["s-a", "s-b"]);
    let cursor = first["next_before"].as_str().expect("a third row follows");
    let second = body_json(&call(
        &ctx,
        &req(
            "GET",
            &format!("/api/v1/sessions?limit=2&before={cursor}"),
            Some(TOKEN),
            "",
        ),
    ));
    assert_eq!(ids(&second), vec!["s-c"]);
    assert_eq!(second["next_before"], serde_json::Value::Null);
}

/// An empty store is an empty page, not an error, and the default limit
/// serves up to 50 rows.
#[test]
fn an_empty_store_lists_nothing_and_the_default_limit_is_fifty() {
    let sb = HomeSandbox::new();
    let ctx = ctx();
    let empty = call(&ctx, &req("GET", "/api/v1/sessions", Some(TOKEN), ""));
    assert_eq!(empty.status, 200);
    assert_eq!(
        body_json(&empty),
        serde_json::json!({"ok": true, "sessions": [], "next_before": null})
    );

    for n in 0..51u64 {
        let id = format!("s-{n:02}");
        global_session(&sb, &id, &[user_line(&id, "/w", "q")], 10_000 + n);
    }
    let page = body_json(&call(
        &ctx,
        &req("GET", "/api/v1/sessions", Some(TOKEN), ""),
    ));
    let rows = page["sessions"].as_array().expect("rows");
    assert_eq!(rows.len(), 50);
    assert_eq!(rows[0]["id"], serde_json::json!("s-50"));
    assert_eq!(rows[49]["id"], serde_json::json!("s-01"));
    assert!(page["next_before"].is_string(), "one row remains");
    assert!(
        rows[0].get("tokens").is_none() && rows[0].get("cost").is_none(),
        "the full-parse figures are not served"
    );
}

/// A cursor no page issued, and a limit outside its bounds, are 400 before
/// any store is walked.
#[test]
fn a_bad_cursor_or_limit_is_400() {
    let _sb = HomeSandbox::new();
    let ctx = ctx();
    for query in [
        "before=nope",
        "before=",
        "before=NTo",
        "before=MjAwMDAwMDAwMDAwMDpzLW1pZGRsZQ%3D",
        "limit=0",
        "limit=201",
        "limit=abc",
        "limit=",
        "limit=-1",
    ] {
        let resp = call(
            &ctx,
            &req("GET", &format!("/api/v1/sessions?{query}"), Some(TOKEN), ""),
        );
        assert_eq!(
            (resp.status, body_json(&resp)),
            (
                400,
                serde_json::json!({"ok": false, "error": "bad_request"})
            ),
            "{query}"
        );
    }
    let ok = call(
        &ctx,
        &req(
            "GET",
            "/api/v1/sessions?limit=200&before=MjAwMDAwMDAwMDAwMDpzLW1pZGRsZQ",
            Some(TOKEN),
            "",
        ),
    );
    assert_eq!(
        ok.status, 200,
        "the bounds are inclusive and the cursor shape is accepted"
    );
}

/// The captured transcript paged in two: the newest four records, then the
/// three older ones, byte for byte the pinned answers — key order and all —
/// with the torn trailing line counted once and never served.
#[test]
fn a_history_page_serves_the_records_verbatim_paged_backward() {
    let sb = HomeSandbox::new();
    let ctx = ctx();
    seed_history(&sb);

    let first = call(
        &ctx,
        &req(
            "GET",
            &format!("/api/v1/sessions/{HISTORY_ID}?limit=4"),
            Some(TOKEN),
            "",
        ),
    );
    assert_eq!(first.status, 200);
    assert_eq!(body_text(&first), HISTORY_PAGE_1.trim_end());
    let next_before = body_json(&first)["next_before"]
        .as_u64()
        .expect("an older page remains");

    let second = call(
        &ctx,
        &req(
            "GET",
            &format!("/api/v1/sessions/{HISTORY_ID}?limit=4&before={next_before}"),
            Some(TOKEN),
            "",
        ),
    );
    assert_eq!(second.status, 200);
    assert_eq!(body_text(&second), HISTORY_PAGE_2.trim_end());

    // The envelope agrees with the documented schema; the records are the
    // free-form object the document declares, so the walk sees them blanked.
    let mut envelope = body_json(&first);
    for record in envelope["records"].as_array_mut().expect("records") {
        record["record"] = serde_json::json!({});
    }
    crate::testutil::schema_agrees_with_type::<HistoryBody>(&envelope);

    // The default page: every record, oldest first, nothing older.
    let whole = body_json(&call(
        &ctx,
        &req(
            "GET",
            &format!("/api/v1/sessions/{HISTORY_ID}"),
            Some(TOKEN),
            "",
        ),
    ));
    let offsets: Vec<u64> = whole["records"]
        .as_array()
        .expect("records")
        .iter()
        .map(|record| record["offset"].as_u64().expect("offset"))
        .collect();
    assert_eq!(offsets, vec![0, 148, 325, 776, 978, 1404, 1544]);
    assert_eq!(whole["next_before"], serde_json::Value::Null);
    assert_eq!(whole["malformed"], serde_json::json!(1));
}

/// A transcript a live isolated run holds is paged like a shared one; its id
/// resolves to that store's file.
#[test]
fn a_live_isolated_transcript_is_paged_by_id() {
    let sb = HomeSandbox::new();
    let ctx = ctx();
    let lock = live_isolated_session(
        &sb,
        "iso",
        "s-iso",
        &[
            user_line("s-iso", "/w/iso", "hi iso"),
            user_line("s-iso", "/w/iso", "bye iso"),
        ],
        1_500,
    );
    let resp = call(&ctx, &req("GET", "/api/v1/sessions/s-iso", Some(TOKEN), ""));
    drop(lock);
    assert_eq!(resp.status, 200);
    assert_eq!(
        body_json(&resp),
        serde_json::json!({
            "ok": true,
            "id": "s-iso",
            "records": [
                {"offset": 0, "record": {"sessionId": "s-iso", "cwd": "/w/iso", "message": {"role": "user", "content": "hi iso"}}},
                {"offset": 82, "record": {"sessionId": "s-iso", "cwd": "/w/iso", "message": {"role": "user", "content": "bye iso"}}},
            ],
            "next_before": null,
            "malformed": 0,
        })
    );
}

/// A transcript the walk lists but the read cannot open answers the same 404
/// as an unknown id, with the failure on the log line, pinned by equality
/// against the very `io::Error` the read path propagates. The fixture is a
/// regular transcript with its mode cleared: the walk takes any `.jsonl`
/// entry that is not a directory (`collect_jsonl` recurses into a directory
/// of that name rather than listing it, so a directory cannot pose this),
/// `stat` still yields the mtime, and `File::open` fails on both unix
/// platforms without a privilege trick. Rights that ignore the mode (root)
/// cannot pose it: the open is probed first and the test says so and stops.
/// The id carries a newline (sent as `%0A`), so the log line also proves the
/// id is flattened before it is logged: unflattened, it would forge a second
/// log entry.
#[test]
fn a_transcript_that_cannot_be_opened_is_404_and_logged() {
    use std::os::unix::fs::PermissionsExt;

    let sb = HomeSandbox::new();
    let ctx = ctx();
    let id = "lo\ncked";
    let path = global_session(&sb, id, &[user_line(id, "/w", "sealed")], 1_000);
    fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).unwrap();
    let denied = match fs::File::open(&path) {
        Ok(_) => {
            eprintln!(
                "SKIPPING: this process opens a mode-0 file, so the read failure cannot be posed"
            );
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
            return;
        }
        Err(e) => e,
    };
    assert!(
        crate::sessions::locate(id).is_some(),
        "the walk lists the transcript from its name and mtime alone"
    );

    let lines = crate::logline::LogLines::new();
    let _capture = lines.capture_here();
    let resp = call(
        &ctx,
        &req("GET", "/api/v1/sessions/lo%0Acked", Some(TOKEN), ""),
    );
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(
        (resp.status, body_json(&resp)),
        (
            404,
            serde_json::json!({
                "ok": false,
                "error": "session_not_found",
                "reason": SESSION_NOT_FOUND,
            })
        )
    );
    assert_eq!(
        lines.snapshot(),
        vec![format!(
            "clauth api: session 'lo cked' failed to read: {denied}"
        )]
    );
}

/// An id no store holds and an id spelled as a traversal answer 404; a bad
/// query is 400.
#[test]
fn an_unknown_or_path_shaped_id_is_404_and_a_bad_query_400() {
    let sb = HomeSandbox::new();
    let ctx = ctx();
    seed_history(&sb);
    let not_found = serde_json::json!({
        "ok": false,
        "error": "session_not_found",
        "reason": SESSION_NOT_FOUND,
    });
    for id in [
        "ghost",
        "..",
        ".",
        "..%2Fx",
        "1CB26556-3532-45E1-8B39-37F0B53A8E4F",
    ] {
        let resp = call(
            &ctx,
            &req("GET", &format!("/api/v1/sessions/{id}"), Some(TOKEN), ""),
        );
        assert_eq!(
            (resp.status, body_json(&resp)),
            (404, not_found.clone()),
            "{id}"
        );
    }
    // A slash inside the id is another path, which the table does not hold.
    let routed = call(&ctx, &req("GET", "/api/v1/sessions/../x", Some(TOKEN), ""));
    assert_eq!(
        (routed.status, body_json(&routed)),
        (404, serde_json::json!({"ok": false, "error": "not_found"}))
    );
    for query in [
        "limit=0",
        "limit=501",
        "limit=x",
        "before=x",
        "before=-1",
        "before=",
    ] {
        let resp = call(
            &ctx,
            &req(
                "GET",
                &format!("/api/v1/sessions/{HISTORY_ID}?{query}"),
                Some(TOKEN),
                "",
            ),
        );
        assert_eq!(
            (resp.status, body_json(&resp)),
            (
                400,
                serde_json::json!({"ok": false, "error": "bad_request"})
            ),
            "{query}"
        );
    }
    // A percent-encoded id decodes once at the binding, so a uuid a client
    // encoded (its dashes as `%2D`) round-trips to the same transcript.
    let encoded = call(
        &ctx,
        &req(
            "GET",
            &format!("/api/v1/sessions/{}", HISTORY_ID.replace('-', "%2D")),
            Some(TOKEN),
            "",
        ),
    );
    assert_eq!(encoded.status, 200);
    assert_eq!(body_json(&encoded)["id"], serde_json::json!(HISTORY_ID));
    let past_the_end = call(
        &ctx,
        &req(
            "GET",
            &format!("/api/v1/sessions/{HISTORY_ID}?limit=500&before=99999"),
            Some(TOKEN),
            "",
        ),
    );
    assert_eq!(past_the_end.status, 200, "a cursor past the end is the end");
    assert_eq!(
        body_json(&past_the_end)["records"]
            .as_array()
            .expect("records")
            .len(),
        7
    );
}
