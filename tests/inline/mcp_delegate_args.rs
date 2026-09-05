#![allow(clippy::unwrap_used, clippy::expect_used)]
#![allow(unsafe_code)]

//! Guard coverage for `delegate`'s two new inputs: `prompt_file` (a reusable
//! prompt read from disk, validated against the delegate's `cwd`) and
//! `profiles` (a fan-out that spends one window per account, blocking unless
//! `background` is set).
//!
//! Every refusal here is pinned on the reason it names, so a guard dropped
//! during a later edit fails its test rather than silently passing.

use super::*;
use crate::profile::{AppConfig, AppState};
use crate::testutil::HomeSandbox;
use std::io::{Seek, SeekFrom, Write};

/// A `DelegateArgs` with every optional field unset and JSON format, so each
/// test overrides only what it exercises.
fn base() -> DelegateArgs {
    DelegateArgs {
        profiles: None,
        prompt: None,
        prompt_file: None,
        model: None,
        cwd: None,
        env: None,
        args: None,
        timeout_secs: None,
        idle_secs: None,
        resume: None,
        isolated: None,
        background: None,
    }
}

/// Seed `names` on disk, optionally disabling each so a stray spawn refuses
/// before launching `claude`.
fn seed_profiles(names: &[&str], disabled: bool) {
    let mut config = AppConfig {
        state: AppState::default(),
        profiles: Vec::new(),
    };
    for name in names {
        crate::actions::create_blank_profile(&mut config, (*name).to_string(), None, None, None)
            .expect("create profile");
    }
    if disabled {
        for name in names {
            crate::actions::disable_profile(&mut config, &crate::profile::ProfileName::from(*name))
                .expect("disable profile");
        }
    }
}

/// Drive the async `delegate` tool with `CLAUTH_MCP_DEPTH` cleared, so the
/// recursion guard does not mask the argument guard under test. Every caller
/// holds a `HomeSandbox`, whose `HOME_TEST_LOCK` serializes the env mutation.
///
/// # Safety
/// `remove_var`/`set_var` are unsafe in Rust 2024 (not thread-safe); the lock
/// held by the caller's `HomeSandbox` is the serialization. Restored before this
/// returns, so no other lock-holder observes a torn value.
fn call_delegate(args: DelegateArgs) -> CallToolResult {
    let saved = std::env::var(MCP_DEPTH_ENV).ok();
    // SAFETY: test-only, serialized by the sandbox's HOME_TEST_LOCK.
    unsafe { std::env::remove_var(MCP_DEPTH_ENV) };

    let server = ClauthServer::new();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("runtime");
    let result = rt.block_on(async { server.delegate_with(args, ProgressSink::none()).await });

    // Join the fan-out's DETACHED background tasks while `rt` is still alive,
    // then drop it. `spawn_blocking` schedules non-mandatory work: a task still
    // queued when its runtime shuts down is discarded un-run, so dropping `rt`
    // here first leaves a job at `running` that nothing will ever finalize.
    // Measured under load: two tasks spawned, one never entered its closure,
    // its job still `running` after 120s.
    crate::testutil::join_background_tasks();
    drop(rt);

    // SAFETY: same as above — restore the prior value.
    unsafe {
        match &saved {
            Some(v) => std::env::set_var(MCP_DEPTH_ENV, v),
            None => std::env::remove_var(MCP_DEPTH_ENV),
        }
    }
    result.expect("delegate returns a tool result, never a transport error")
}

fn first_text(result: &CallToolResult) -> String {
    result
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.clone())
        .expect("first content block is text")
}

/// A refusal: one prose block naming every needle. The reason is what carries
/// the needles; a target-spelled refusal prefixes its sentence with them.
fn assert_refusal(result: &CallToolResult, needles: &[&str]) {
    assert_eq!(result.is_error, Some(true), "the refusal is a tool error");
    assert_eq!(
        result.content.len(),
        1,
        "the refusal is a single content block"
    );
    let text = first_text(result);
    for needle in needles {
        assert!(
            text.contains(needle),
            "the refusal names {needle:?}: {text}"
        );
    }
}

/// A prose-format refusal: one block, a sentence that is not JSON, naming every
/// needle.
fn assert_prose_refusal(result: &CallToolResult, needles: &[&str]) {
    assert_eq!(result.is_error, Some(true), "the refusal is a tool error");
    assert_eq!(
        result.content.len(),
        1,
        "the prose refusal is a single content block"
    );
    let text = first_text(result);
    assert!(
        serde_json::from_str::<serde_json::Value>(&text).is_err(),
        "the prose refusal must not be JSON"
    );
    for needle in needles {
        assert!(text.contains(needle), "the prose names {needle:?}: {text}");
    }
}

fn work_dir(home: &std::path::Path) -> std::path::PathBuf {
    let dir = home.join("work");
    std::fs::create_dir_all(&dir).expect("create work dir");
    dir
}

// ── prompt source: exactly one of `prompt` / `prompt_file` ───────────────────

#[test]
fn both_prompt_sources_are_refused_by_name() {
    let _home = HomeSandbox::new();
    let result = call_delegate(DelegateArgs {
        prompt: Some("hi".to_string()),
        prompt_file: Some("p.txt".to_string()),
        profiles: Some(vec!["solo".to_string()]),
        ..base()
    });
    assert_refusal(
        &result,
        &["exactly one of `prompt` or `prompt_file` must be given; both were"],
    );
}

#[test]
fn neither_prompt_source_is_refused_by_name() {
    let _home = HomeSandbox::new();
    let result = call_delegate(DelegateArgs {
        profiles: Some(vec!["solo".to_string()]),
        ..base()
    });
    assert_refusal(
        &result,
        &["exactly one of `prompt` or `prompt_file` must be given; neither was"],
    );
}

// ── target: `profiles` is the one field ──────────────────────────────────────

/// The `profile`/`profiles` pair collapsed onto `profiles: string[]`, so the
/// exactly-one-of-two guard went with it. What stays refusable is naming no
/// target at all.
#[test]
fn an_absent_target_is_refused_by_name() {
    let _home = HomeSandbox::new();
    let result = call_delegate(DelegateArgs {
        prompt: Some("hi".to_string()),
        ..base()
    });
    assert_refusal(&result, &["`profiles` is empty: name at least one profile"]);
}

// ── prompt_file boundary validation ──────────────────────────────────────────

#[test]
fn prompt_file_absolute_path_is_refused_by_name() {
    let home = HomeSandbox::new();
    seed_profiles(&["solo"], true);
    let cwd = work_dir(home.home());
    // The sandbox home is already absolute, so this is absolute on every
    // platform. A literal like `/etc/passwd` is not: on Windows it has a root
    // but no drive prefix, so `is_absolute()` is false there and the path
    // falls through to a file-not-found instead of the refusal under test.
    let abs = std::path::absolute(home.home().join("passwd"))
        .expect("absolute path")
        .to_str()
        .expect("sandbox path is UTF-8")
        .to_string();
    let result = call_delegate(DelegateArgs {
        profiles: Some(vec!["solo".to_string()]),
        prompt_file: Some(abs.clone()),
        cwd: Some(cwd.to_str().unwrap().to_string()),
        ..base()
    });
    assert_refusal(&result, &[&format!("prompt_file `{abs}`"), "absolute path"]);
}

/// On Windows `is_absolute()` needs BOTH a prefix and a root, so a
/// drive-relative (`C:foo`) and a root-relative (`\etc\passwd`) spelling pass
/// the check at the top of the join and arrive at the component loop. The
/// `RootDir | Prefix` arm must refuse each by name: dropping the component
/// re-roots the path under `cwd` and reads a different file than the caller
/// named.
#[cfg(windows)]
#[test]
fn prompt_file_drive_relative_path_is_refused_by_name() {
    let home = HomeSandbox::new();
    seed_profiles(&["solo"], true);
    let cwd = work_dir(home.home());
    for rel in ["C:foo", r"\etc\passwd"] {
        let result = call_delegate(DelegateArgs {
            profiles: Some(vec!["solo".to_string()]),
            prompt_file: Some(rel.to_string()),
            cwd: Some(cwd.to_str().unwrap().to_string()),
            ..base()
        });
        assert_refusal(&result, &[&format!("prompt_file `{rel}`"), "absolute path"]);
    }
}

#[test]
fn prompt_file_dotdot_escape_is_refused_by_name() {
    let home = HomeSandbox::new();
    seed_profiles(&["solo"], true);
    let cwd = work_dir(home.home());
    let result = call_delegate(DelegateArgs {
        profiles: Some(vec!["solo".to_string()]),
        prompt_file: Some("../secret.txt".to_string()),
        cwd: Some(cwd.to_str().unwrap().to_string()),
        ..base()
    });
    assert_refusal(&result, &["prompt_file `../secret.txt`", "escapes `cwd`"]);
}

#[cfg(unix)]
#[test]
fn prompt_file_symlink_escape_is_refused_by_name() {
    let home = HomeSandbox::new();
    seed_profiles(&["solo"], true);
    let cwd = work_dir(home.home());
    let outside = home.home().join("secret.txt");
    std::fs::write(&outside, "secret").expect("outside file");
    std::os::unix::fs::symlink(&outside, cwd.join("link.txt")).expect("symlink");

    let result = call_delegate(DelegateArgs {
        profiles: Some(vec!["solo".to_string()]),
        prompt_file: Some("link.txt".to_string()),
        cwd: Some(cwd.to_str().unwrap().to_string()),
        ..base()
    });
    assert_refusal(
        &result,
        &[
            "prompt_file `link.txt`",
            "symlink target resolves outside `cwd`",
        ],
    );
}

#[test]
fn prompt_file_oversize_is_refused_by_name() {
    let home = HomeSandbox::new();
    seed_profiles(&["solo"], true);
    let cwd = work_dir(home.home());
    std::fs::write(
        cwd.join("big.txt"),
        vec![b'a'; super::PROMPT_FILE_CAP as usize + 1],
    )
    .expect("oversize file");

    let result = call_delegate(DelegateArgs {
        profiles: Some(vec!["solo".to_string()]),
        prompt_file: Some("big.txt".to_string()),
        cwd: Some(cwd.to_str().unwrap().to_string()),
        ..base()
    });
    assert_refusal(
        &result,
        &["prompt_file `big.txt`", "bytes over the", "byte cap"],
    );
}

/// A directory used to end in an EISDIR-shaped refusal at read time. The type
/// check refuses it deliberately, by name, before any open.
#[test]
fn prompt_file_directory_is_refused_by_name() {
    let home = HomeSandbox::new();
    seed_profiles(&["solo"], true);
    let cwd = work_dir(home.home());

    let result = call_delegate(DelegateArgs {
        profiles: Some(vec!["solo".to_string()]),
        prompt_file: Some(".".to_string()),
        cwd: Some(cwd.to_str().unwrap().to_string()),
        ..base()
    });
    assert_refusal(&result, &["prompt_file `.`", "not a regular file"]);
}

/// A FIFO blocks a read-only open until a writer appears, and the MCP server
/// runs on the only thread of its current-thread runtime, so reading one as a
/// `prompt_file` would freeze every tool until the process dies. The type check
/// must refuse it without ever opening it. On a regression the call below hangs
/// forever; the receive timeout turns that hang into a failing test instead of
/// a wedged runner.
#[cfg(unix)]
#[test]
fn prompt_file_refuses_a_fifo_without_blocking() {
    let home = HomeSandbox::new();
    let cwd = work_dir(home.home());
    let fifo = cwd.join("pipe");
    let status = std::process::Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .expect("mkfifo runs");
    assert!(status.success(), "mkfifo creates the fifo");

    let (tx, rx) = std::sync::mpsc::channel();
    let cwd_str = cwd.to_str().unwrap().to_string();
    let handle = std::thread::spawn(move || {
        let _ = tx.send(super::read_prompt_file(Some(&cwd_str), "pipe"));
    });
    match rx.recv_timeout(std::time::Duration::from_secs(5)) {
        Ok(Err(reason)) => {
            assert!(
                reason.contains("not a regular file"),
                "the refusal names the file type: {reason}"
            );
        }
        Ok(Ok(_)) => panic!("a FIFO must never be read as a prompt"),
        Err(_) => panic!(
            "read_prompt_file blocked on a FIFO: the type check no longer refuses before the open"
        ),
    }
    handle.join().expect("reader thread joins");
}

/// A file grown past the cap after its size was checked must be refused by the
/// bounded read, never silently truncated: `take(cap + 1)` alone returns a
/// short Ok that reads as success.
#[test]
fn prompt_handle_growth_past_cap_is_refused_by_name() {
    let home = HomeSandbox::new();
    let path = home.home().join("grow.txt");
    let mut f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .expect("create grow.txt");
    f.write_all(&vec![b'a'; super::PROMPT_FILE_CAP as usize])
        .expect("cap bytes");
    // Grow past the cap on the same handle: a size check statting this file
    // before the growth sees a passing size; the read then sees past-cap bytes.
    f.write_all(b"more").expect("grow");
    f.seek(SeekFrom::Start(0)).expect("rewind");

    let reason = super::read_prompt_handle(f, "grow.txt")
        .expect_err("a past-cap read is refused, not truncated");
    for needle in [
        "prompt_file `grow.txt`",
        "grew past the",
        "byte cap",
        "during the read",
    ] {
        assert!(
            reason.contains(needle),
            "the reason names {needle:?}: {reason}"
        );
    }
}

/// The cap itself stays accepted: the growth refusal fires only past it.
#[test]
fn prompt_handle_at_cap_is_accepted() {
    let home = HomeSandbox::new();
    let path = home.home().join("exact.txt");
    let mut f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .expect("create exact.txt");
    f.write_all(&vec![b'a'; super::PROMPT_FILE_CAP as usize])
        .expect("cap bytes");
    f.seek(SeekFrom::Start(0)).expect("rewind");

    let text = super::read_prompt_handle(f, "exact.txt").expect("at-cap file is accepted");
    assert_eq!(
        text.len(),
        super::PROMPT_FILE_CAP as usize,
        "the at-cap file is read whole, not truncated"
    );
}

/// An invalid byte sequence must be refused by name with the byte offset of the
/// first invalid byte, never lossily decoded: a delegate spends a real window on
/// the prompt, so a mis-encoded file must not become a subtly wrong prompt.
/// The prefix carries multi-byte characters so the pinned offset can only be a
/// byte offset — a char-offset reading of the same failure would disagree.
#[test]
fn prompt_handle_invalid_utf8_is_refused_by_name() {
    let home = HomeSandbox::new();
    let path = home.home().join("bad.txt");
    let mut bytes = "valid préfix ☃".as_bytes().to_vec();
    bytes.push(0xFF);
    let expected_offset = bytes
        .iter()
        .position(|&b| b == 0xFF)
        .expect("bad byte present");
    std::fs::write(&path, &bytes).expect("write bad.txt");
    let file = std::fs::File::open(&path).expect("open bad.txt");

    let reason = super::read_prompt_handle(file, "bad.txt")
        .expect_err("an invalid UTF-8 prompt is refused, not decoded");
    for needle in [
        "prompt_file `bad.txt`",
        "invalid UTF-8",
        &format!("byte offset {expected_offset}"),
    ] {
        assert!(
            reason.contains(needle),
            "the reason names {needle:?}: {reason}"
        );
    }
}

/// Valid multi-byte UTF-8 reads unchanged: the strict decode refuses only what
/// is not UTF-8.
#[test]
fn prompt_handle_multibyte_utf8_is_accepted() {
    let home = HomeSandbox::new();
    let path = home.home().join("utf8.txt");
    std::fs::write(&path, "héllo ☃ £").expect("write utf8.txt");
    let file = std::fs::File::open(&path).expect("open utf8.txt");

    let text = super::read_prompt_handle(file, "utf8.txt").expect("valid UTF-8 is read");
    assert_eq!(text, "héllo ☃ £", "the prompt is read unchanged");
}

// ── profiles fan-out guards ──────────────────────────────────────────────────

#[test]
fn profiles_empty_is_refused_by_name() {
    let _home = HomeSandbox::new();
    let result = call_delegate(DelegateArgs {
        profiles: Some(vec![]),
        prompt: Some("hi".to_string()),
        background: Some(true),
        ..base()
    });
    assert_refusal(
        &result,
        &["`profiles` is empty", "name at least one profile"],
    );
}

#[test]
fn profiles_over_cap_is_refused_by_name() {
    let _home = HomeSandbox::new();
    let names: Vec<String> = (0..=super::MAX_FANOUT).map(|i| format!("p{i}")).collect();
    let result = call_delegate(DelegateArgs {
        profiles: Some(names),
        prompt: Some("hi".to_string()),
        background: Some(true),
        ..base()
    });
    // "fan-out capped at" / "names; got" pin the ceiling arm's wording; the
    // fix clause is pinned verbatim, rendered cap included, for the
    // placement rule 4's corollary reason: the refusal carries the whole lesson, so a reword
    // that drops the fix reds here.
    assert_refusal(
        &result,
        &[
            "fan-out capped at",
            "names; got",
            "split the names across calls of 8 or fewer",
        ],
    );
}

#[test]
fn profiles_duplicate_is_refused_by_name() {
    let _home = HomeSandbox::new();
    seed_profiles(&["solo"], true);
    let result = call_delegate(DelegateArgs {
        profiles: Some(vec!["solo".to_string(), "SOLO".to_string()]),
        prompt: Some("hi".to_string()),
        background: Some(true),
        ..base()
    });
    assert_refusal(
        &result,
        &[
            "duplicate profile in `profiles`: `SOLO`",
            "case-insensitive",
        ],
    );
}

#[test]
fn profiles_unknown_is_refused_by_name() {
    let _home = HomeSandbox::new();
    let result = call_delegate(DelegateArgs {
        profiles: Some(vec!["ghost".to_string()]),
        prompt: Some("hi".to_string()),
        background: Some(true),
        ..base()
    });
    assert_refusal(
        &result,
        &[
            "profile not found: ghost",
            "call `profiles` for valid names",
        ],
    );
}

/// One name without `background` is the ordinary blocking single delegate. Two
/// or more names without `background` fan out: the old background-only refusal
/// is gone, so a bad member now refuses at resolution like any other fan-out.
#[test]
fn a_blocking_single_delegate_is_not_a_fanout_and_a_fanout_resolves_members() {
    let _home = HomeSandbox::new();
    seed_profiles(&["solo"], false);

    // One name, blocking: reaches the prompt/target validation, so it must
    // NOT refuse with any fan-out guard. `solo` is real, so the refusal-free
    // path runs straight to the cwd gate.
    let single = call_delegate(DelegateArgs {
        profiles: Some(vec!["solo".to_string()]),
        prompt: Some("hi".to_string()),
        cwd: Some("/nonexistent-dir-for-the-cwd-gate".to_string()),
        ..base()
    });
    assert_refusal(&single, &["cwd does not exist or is not a directory"]);

    // Two names, blocking, one unknown: the fan-out resolves every member and
    // refuses the unknown one by name, never with the deleted guard.
    let fanout = call_delegate(DelegateArgs {
        profiles: Some(vec!["solo".to_string(), "vendor".to_string()]),
        prompt: Some("hi".to_string()),
        background: None,
        ..base()
    });
    assert_refusal(
        &fanout,
        &[
            "profile not found: vendor",
            "call `profiles` for valid names",
        ],
    );
}

/// A reserve failure refuses before any spawn: with the jobs dir replaced by a
/// regular file the first job-file write fails (ENOTDIR), and the fan-out must
/// name that failure and launch nothing rather than spending one window per
/// account mid-loop and losing the job ids.
#[test]
fn fanout_reserve_failure_is_refused_by_name() {
    let home = HomeSandbox::new();
    // Enabled members: a disabled one would refuse at the pre-flight before
    // the reserve this test pins.
    seed_profiles(&["solo", "vendor"], false);
    let jobs = home.home().join(".clauth").join("jobs");
    std::fs::create_dir_all(jobs.parent().unwrap()).expect("clauth dir");
    std::fs::write(&jobs, b"not a dir").expect("jobs path is a file");

    let result = call_delegate(DelegateArgs {
        profiles: Some(vec!["solo".to_string(), "vendor".to_string()]),
        prompt: Some("hi".to_string()),
        background: Some(true),
        ..base()
    });
    assert_refusal(&result, &["failed to record job"]);
}

// ── resume infers the account from the conversation record ──────────────────

/// Seed `~/.clauth/conversations/<id>.json` carrying `told`, written through
/// the crate's own atomic 0600 writer so the fixture is exactly the bytes the
/// hook itself writes.
fn seed_conversation_record(home: &std::path::Path, id: &str, told: Option<&str>) {
    let dir = home.join(".clauth").join("conversations");
    std::fs::create_dir_all(&dir).expect("records dir");
    let path = dir.join(format!("{id}.json"));
    let bytes = serde_json::to_vec(&serde_json::json!({ "told": told })).expect("record json");
    crate::profile::atomic_write_600(&path, bytes).expect("record write");
}

/// `resume` without `profiles` infers the account from the conversation record
/// the profile-change hook keeps. The refusal naming `solo` proves the record's
/// `told` resolved, was canonicalized (seeded as `SOLO`), and reached the same
/// pre-flight an explicit name gets.
#[test]
fn a_resume_without_profiles_resolves_the_account_from_the_conversation_record() {
    let home = HomeSandbox::new();
    seed_profiles(&["solo"], true);
    seed_conversation_record(home.home(), "conv-1", Some("SOLO"));

    let result = call_delegate(DelegateArgs {
        resume: Some("conv-1".to_string()),
        prompt: Some("hi".to_string()),
        background: Some(true),
        ..base()
    });
    assert_refusal(&result, &["profile is disabled: solo"]);
}

/// An id no record exists for cannot be attributed: the refusal names the fix,
/// carrying the whole lesson per placement rule 4's corollary.
#[test]
fn a_resume_with_no_record_refuses_naming_profiles() {
    let _home = HomeSandbox::new();
    let result = call_delegate(DelegateArgs {
        resume: Some("nope".to_string()),
        prompt: Some("hi".to_string()),
        background: Some(true),
        ..base()
    });
    assert_refusal(
        &result,
        &[
            "can't tell which account session 'nope' ran on",
            "pass `profiles`",
        ],
    );
}

/// A record that never established a baseline (`told: null`) is as
/// unattributable as a missing one: same refusal, same fix.
#[test]
fn a_resume_whose_record_has_no_told_refuses_naming_profiles() {
    let home = HomeSandbox::new();
    seed_conversation_record(home.home(), "conv-null", None);

    let result = call_delegate(DelegateArgs {
        resume: Some("conv-null".to_string()),
        prompt: Some("hi".to_string()),
        background: Some(true),
        ..base()
    });
    assert_refusal(
        &result,
        &[
            "can't tell which account session 'conv-null' ran on",
            "pass `profiles`",
        ],
    );
}

/// An explicit `profiles` with a `resume` behaves exactly as before: the name
/// wins over whatever the record says. The record names `solo`, the call names
/// `other`, and the refusal must prove `other` was the resolved target.
#[test]
fn an_explicit_profiles_wins_over_the_record_for_a_resume() {
    let home = HomeSandbox::new();
    seed_profiles(&["solo", "other"], true);
    seed_conversation_record(home.home(), "conv-1", Some("solo"));

    let result = call_delegate(DelegateArgs {
        profiles: Some(vec!["other".to_string()]),
        resume: Some("conv-1".to_string()),
        prompt: Some("hi".to_string()),
        background: Some(true),
        ..base()
    });
    assert_refusal(&result, &["profile is disabled: other"]);
    assert!(
        !first_text(&result).contains("solo"),
        "the record's account is not consulted: {}",
        first_text(&result)
    );
}

/// The record's `told` names an account clauth does not hold: the existing
/// not-found refusal, the same path an explicit unknown name takes.
#[test]
fn a_resume_record_naming_an_unknown_account_refuses_profile_not_found() {
    let home = HomeSandbox::new();
    seed_conversation_record(home.home(), "conv-g", Some("ghost"));

    let result = call_delegate(DelegateArgs {
        resume: Some("conv-g".to_string()),
        prompt: Some("hi".to_string()),
        background: Some(true),
        ..base()
    });
    assert_refusal(
        &result,
        &[
            "profile not found: ghost",
            "call `profiles` for valid names",
        ],
    );
}

/// The resume id reaches a filename (the record path), so a path-shaped id is
/// refused at that boundary rather than read: the hook only ever writes records
/// for bare ids, and joining an unchecked id would escape the records dir.
#[test]
fn a_path_shaped_resume_id_is_refused_not_read() {
    let home = HomeSandbox::new();
    seed_profiles(&["solo"], true);
    // The bare-id check is the only thing between this id and the join, so
    // make the traversal physically reachable: `conversations/..` resolves
    // through the dir, and the kernel walk cannot pass a missing component.
    std::fs::create_dir_all(home.home().join(".clauth").join("conversations"))
        .expect("records dir");
    // Decoy at the traversal destination: with the bare-id check dropped,
    // `record_path` joins `conversations/../escape.json`, which resolves to
    // exactly this file — so the drop resolves the target and this test reds
    // on the wrong refusal instead of passing on a silent read failure.
    let decoy = home.home().join(".clauth").join("escape.json");
    let bytes = serde_json::to_vec(&serde_json::json!({ "told": "solo" })).expect("decoy json");
    crate::profile::atomic_write_600(&decoy, bytes).expect("decoy write");

    let result = call_delegate(DelegateArgs {
        resume: Some("../escape".to_string()),
        prompt: Some("hi".to_string()),
        background: Some(true),
        ..base()
    });
    assert_refusal(
        &result,
        &[
            "can't tell which account session '../escape' ran on",
            "pass `profiles`",
        ],
    );
}

/// The refusal echoes the id, and this arm fires precisely for ids the record
/// check refused — unbounded length included. The echo is truncated so a huge
/// id cannot inflate the reply: the truncated prefix appears, the tail never
/// does.
#[test]
fn an_overlong_resume_id_is_echoed_bounded() {
    let _home = HomeSandbox::new();
    let result = call_delegate(DelegateArgs {
        resume: Some("a".repeat(100)),
        prompt: Some("hi".to_string()),
        background: Some(true),
        ..base()
    });
    let text = first_text(&result);
    assert!(
        text.contains(&format!(
            "can't tell which account session '{}…' ran on",
            "a".repeat(64)
        )),
        "the refusal shows the truncated id: {text}"
    );
    assert!(
        !text.contains(&"a".repeat(70)),
        "the full id never reaches the reply: {text}"
    );
    assert!(text.contains("pass `profiles`"), "the fix is named: {text}");
}

// ── background pre-flight guards ─────────────────────────────────────────────

/// Seed `name` as a keyless third-party profile: a real DeepSeek endpoint with
/// no api key, so the pre-flight refuses it before any job is reserved.
fn seed_keyless_third_party(name: &str) {
    let mut config = AppConfig {
        state: AppState::default(),
        profiles: Vec::new(),
    };
    crate::actions::create_blank_profile(
        &mut config,
        name.to_string(),
        Some("https://api.deepseek.com".to_string()),
        None,
        None,
    )
    .expect("create profile");
}

/// A refusal never carries a job handle: nothing was reserved, so nothing may
/// read like one did.
fn assert_no_job_keys(result: &CallToolResult) {
    let text = first_text(result);
    assert!(
        !text.contains("job"),
        "no job handle in the refusal: {text}"
    );
}

/// Nothing was reserved: the sandbox jobs dir is absent or empty.
fn assert_no_job_files() {
    // `HomeSandbox` holds the home override for the caller's whole body, so a
    // resolution failure here is a harness break, not an absent reservation.
    let dir = jobs::jobs_dir().expect("jobs dir resolvable");
    if !dir.exists() {
        return;
    }
    let entries: Vec<_> = std::fs::read_dir(&dir)
        .expect("jobs dir readable")
        .flatten()
        .collect();
    assert!(
        entries.is_empty(),
        "a refused delegate reserves no job file"
    );
}

/// A background single delegate to a keyless third-party profile refuses
/// synchronously, before a job file exists: the caller must not get a
/// `running` job whose collected result later carries the refusal.
#[test]
fn background_single_keyless_third_party_refuses_before_reserving_a_job() {
    let _home = HomeSandbox::new();
    seed_keyless_third_party("zzbg-ds");

    let result = call_delegate(DelegateArgs {
        profiles: Some(vec!["zzbg-ds".to_string()]),
        prompt: Some("hi".to_string()),
        background: Some(true),
        ..base()
    });
    assert_refusal(
        &result,
        &["profile has no api key: zzbg-ds (run `clauth login zzbg-ds --api-key <key>`)"],
    );
    assert_no_job_keys(&result);
    assert_no_job_files();
}

/// The disabled sibling: a background single delegate to a disabled profile
/// refuses synchronously too, before a job file exists.
#[test]
fn background_single_disabled_target_refuses_before_reserving_a_job() {
    let _home = HomeSandbox::new();
    seed_profiles(&["zzbg-off"], true);

    let result = call_delegate(DelegateArgs {
        profiles: Some(vec!["zzbg-off".to_string()]),
        prompt: Some("hi".to_string()),
        background: Some(true),
        ..base()
    });
    assert_refusal(
        &result,
        &["profile is disabled: zzbg-off (run `clauth enable zzbg-off`)"],
    );
    assert_no_job_keys(&result);
    assert_no_job_files();
}

/// The quarantine sibling: a background single delegate to a profile whose
/// refresh token was rejected refuses synchronously, in `switch`'s own words,
/// before a job file exists. The nonexistent `cwd` is the fixture's control —
/// without the gate the job is reserved and its detached task stops at the cwd
/// check, which is what the red looked like.
#[test]
fn background_single_auth_broken_target_refuses_before_reserving_a_job() {
    let home = HomeSandbox::new();
    let mut config = AppConfig {
        state: AppState::default(),
        profiles: Vec::new(),
    };
    crate::actions::create_blank_profile(&mut config, "zzbg-dead".to_string(), None, None, None)
        .expect("create profile");
    assert!(
        config.set_auth_broken(&crate::profile::ProfileName::from("zzbg-dead"), true),
        "fixture control: the profile was not already quarantined",
    );
    crate::profile::save_app_state(&config.state).expect("persist the quarantine");

    let result = call_delegate(DelegateArgs {
        profiles: Some(vec!["zzbg-dead".to_string()]),
        prompt: Some("hi".to_string()),
        background: Some(true),
        cwd: Some(
            home.home()
                .join("does-not-exist")
                .to_str()
                .unwrap()
                .to_string(),
        ),
        ..base()
    });
    assert_refusal(
        &result,
        &[&crate::format::login_expired(&crate::profile::ProfileName::from("zzbg-dead")).line()],
    );
    assert_no_job_keys(&result);
    assert_no_job_files();
}

/// A disabled fan-out member refuses the whole list synchronously, by name,
/// before the first job file is reserved. Same pre-flight as the
/// single-background arm, closing the fan-out's disabled gap.
#[test]
fn background_fanout_with_a_disabled_member_refuses_before_writing_jobs() {
    let _home = HomeSandbox::new();
    // One config for both members: `load_config` reads the roster from the app
    // state, so a second fresh config would overwrite the first member.
    let mut config = AppConfig {
        state: AppState::default(),
        profiles: Vec::new(),
    };
    crate::actions::create_blank_profile(&mut config, "zzbg-off".to_string(), None, None, None)
        .expect("create profile");
    crate::actions::disable_profile(&mut config, &crate::profile::ProfileName::from("zzbg-off"))
        .expect("disable profile");
    crate::actions::create_blank_profile(
        &mut config,
        "zzbg-ds".to_string(),
        Some("https://api.deepseek.com".to_string()),
        None,
        None,
    )
    .expect("create profile");

    // The disabled member comes FIRST: the pre-flight walks members in order,
    // so the refusal names it, not the keyless member behind it.
    let result = call_delegate(DelegateArgs {
        profiles: Some(vec!["zzbg-off".to_string(), "zzbg-ds".to_string()]),
        prompt: Some("hi".to_string()),
        background: Some(true),
        ..base()
    });
    assert_refusal(
        &result,
        &["profile is disabled: zzbg-off (run `clauth enable zzbg-off`)"],
    );
    assert_no_job_keys(&result);
    assert_no_job_files();
}

// ── happy path + format honouring ────────────────────────────────────────────

#[test]
fn a_valid_fanout_returns_one_job_per_account() {
    let home = HomeSandbox::new();
    seed_profiles(&["solo", "vendor"], false);

    // The members are enabled (a disabled member now refuses the fan-out at
    // the pre-flight); a nonexistent cwd stops each detached task at the cwd
    // gate so no stray claude spawns on the blank enabled profiles.
    let result = call_delegate(DelegateArgs {
        profiles: Some(vec!["solo".to_string(), "VENDOR".to_string()]),
        prompt: Some("hi".to_string()),
        background: Some(true),
        cwd: Some(
            home.home()
                .join("does-not-exist")
                .to_str()
                .unwrap()
                .to_string(),
        ),
        ..base()
    });
    assert_ne!(
        result.is_error,
        Some(true),
        "a valid fan-out is not an error"
    );
    assert_eq!(
        result.content.len(),
        1,
        "the fan-out reply is a single content block"
    );
    let text = first_text(&result);
    // The fan-out prose names each target with its job id — which is also the
    // echo of the resolved target list, wrong case canonicalised.
    assert!(
        text.starts_with("delegated to "),
        "the fan-out reply reads as a sentence: {text}",
    );
    assert!(
        text.contains("`solo` (job `d-") && text.contains("`vendor` (job `d-"),
        "one job per named account, each named with its id: {text}",
    );
    let ids = text
        .split("job `")
        .skip(1)
        .map(|rest| rest.split('`').next().unwrap_or_default().to_string())
        .collect::<Vec<_>>();
    assert_eq!(ids.len(), 2, "one job id per account");
    assert_ne!(ids[0], ids[1], "job ids are distinct");

    // Hold the sandbox until both detached tasks finish, so their `write_done`
    // lands under the sandbox and never the real `~/.clauth`.
    crate::testutil::assert_jobs_done(2);
}

/// Two or more names without `background` now fan out and wait for every
/// account, returning one row per account in the order named, all in one
/// content block. The nonexistent cwd stops each run at the cwd gate, so no
/// `claude` spawns on the blank enabled profiles.
#[test]
fn a_blocking_fanout_returns_one_row_per_account_in_order_named() {
    let home = HomeSandbox::new();
    seed_profiles(&["solo", "vendor"], false);

    let result = call_delegate(DelegateArgs {
        profiles: Some(vec!["solo".to_string(), "VENDOR".to_string()]),
        prompt: Some("hi".to_string()),
        background: None,
        cwd: Some(
            home.home()
                .join("does-not-exist")
                .to_str()
                .unwrap()
                .to_string(),
        ),
        ..base()
    });
    assert_eq!(
        result.content.len(),
        1,
        "one content block carries every row"
    );
    let text = first_text(&result);
    let rows: Vec<&str> = text.lines().collect();
    assert_eq!(rows.len(), 2, "one row per account: {text}");
    assert!(
        rows[0].starts_with("delegate to `solo` "),
        "the first row names the first account, in the order named: {text}",
    );
    assert!(
        rows[1].starts_with("delegate to `vendor` "),
        "the second row names the second account, case canonicalised: {text}",
    );
    assert!(
        text.contains("target `solo`: 5h unknown, 7d unknown")
            && text.contains("target `vendor`: 5h unknown, 7d unknown"),
        "each row carries its own headroom: {text}",
    );
}

/// `background`'s own doc promises a handle instead of the output, and two doc
/// lines promise `delegate` carries live usage, so the handle must not be the
/// uninformed reply. It carries the target's own headroom footer, and still
/// exactly one content block.
///
/// The earlier version of this comment cited a "prefer `background` for a slow
/// or third-party target" steer in the tool description as the reason this test
/// exists. The owner removed that steer on 2026-08-19 as an invented heuristic
/// (a third-party target is not inherently slow). What the test actually
/// asserts is the footer, so it survives the removal; only the rationale moved.
#[test]
fn a_background_handle_carries_the_targets_live_usage_footer() {
    let home = HomeSandbox::new();
    seed_profiles(&["solo"], false);

    let result = call_delegate(DelegateArgs {
        profiles: Some(vec!["solo".to_string()]),
        prompt: Some("hi".to_string()),
        background: Some(true),
        // Stops the detached task at the cwd gate, so no `claude` spawns on a
        // blank profile.
        cwd: Some(
            home.home()
                .join("does-not-exist")
                .to_str()
                .unwrap()
                .to_string(),
        ),
        ..base()
    });
    assert_ne!(
        result.is_error,
        Some(true),
        "a valid handle is not an error"
    );
    assert_eq!(
        result.content.len(),
        1,
        "the footer rides the same content block, never a second one"
    );
    let text = first_text(&result);
    assert!(
        text.starts_with("delegate to `solo` running, job `d-"),
        "the handle keeps its spelling: {text}",
    );
    assert!(
        text.contains("; target `solo`: 5h unknown, 7d unknown"),
        "the handle names the target's headroom: {text}",
    );

    crate::testutil::assert_jobs_done(1);
}

/// The fan-out sibling: every job row carries its OWN target's headroom, so a
/// caller that just spent N windows can see what is left on each.
#[test]
fn a_fanout_reply_carries_headroom_for_every_target() {
    let home = HomeSandbox::new();
    seed_profiles(&["solo", "vendor"], false);

    let result = call_delegate(DelegateArgs {
        profiles: Some(vec!["solo".to_string(), "vendor".to_string()]),
        prompt: Some("hi".to_string()),
        background: Some(true),
        cwd: Some(
            home.home()
                .join("does-not-exist")
                .to_str()
                .unwrap()
                .to_string(),
        ),
        ..base()
    });
    assert_eq!(
        result.content.len(),
        1,
        "the fan-out reply stays a single content block"
    );
    let text = first_text(&result);
    assert!(
        text.contains("target `solo`: 5h unknown, 7d unknown")
            && text.contains("target `vendor`: 5h unknown, 7d unknown"),
        "each target's own headroom rides the reply: {text}",
    );
    assert_eq!(
        text.lines().count(),
        1,
        "the fan-out reply is still one line: {text}",
    );

    crate::testutil::assert_jobs_done(2);
}

#[test]
fn prose_refusals_read_as_a_sentence_and_stay_one_block() {
    let _home = HomeSandbox::new();

    let both = call_delegate(DelegateArgs {
        prompt: Some("hi".to_string()),
        prompt_file: Some("p.txt".to_string()),
        profiles: Some(vec!["solo".to_string()]),
        ..base()
    });
    assert_prose_refusal(
        &both,
        &["delegate failed: exactly one of `prompt` or `prompt_file` must be given; both were"],
    );

    let blocking = call_delegate(DelegateArgs {
        profiles: Some(vec!["solo".to_string(), "vendor".to_string()]),
        prompt: Some("hi".to_string()),
        background: None,
        ..base()
    });
    assert_prose_refusal(
        &blocking,
        &[
            "delegate failed: profile not found: solo, vendor",
            "call `profiles` for valid names",
        ],
    );
}

#[test]
fn fanout_prose_names_each_target_with_its_job() {
    let home = HomeSandbox::new();
    seed_profiles(&["solo", "vendor"], false);

    // Enabled members plus a nonexistent cwd: same stray-spawn guard as the
    // JSON fan-out test above.
    let result = call_delegate(DelegateArgs {
        profiles: Some(vec!["solo".to_string(), "vendor".to_string()]),
        prompt: Some("hi".to_string()),
        background: Some(true),
        cwd: Some(
            home.home()
                .join("does-not-exist")
                .to_str()
                .unwrap()
                .to_string(),
        ),
        ..base()
    });
    assert_ne!(result.is_error, Some(true));
    assert_eq!(
        result.content.len(),
        1,
        "the prose fan-out is a single content block"
    );
    let text = first_text(&result);
    assert!(
        serde_json::from_str::<serde_json::Value>(&text).is_err(),
        "the prose fan-out must not be JSON"
    );
    assert!(
        text.starts_with("delegated to "),
        "prose reads as a sentence: {text}"
    );
    assert!(
        text.contains("`solo` (job `"),
        "names each target with its job: {text}"
    );
    assert!(
        text.contains("`vendor` (job `"),
        "names each target with its job: {text}"
    );

    crate::testutil::assert_jobs_done(2);
}

/// [`fanout_is_error`] is true only when every row errored: one bad account in
/// a fan-out must not hide the others' answers, and an empty set has nothing
/// to report.
#[test]
fn fanout_is_error_requires_every_row_errored() {
    let err = |profile: &str| {
        serde_json::json!({
            "profile": profile,
            "is_error": true,
            "result": "boom",
        })
    };
    let ok = |profile: &str| {
        serde_json::json!({
            "profile": profile,
            "is_error": false,
            "result": "fine",
        })
    };
    assert!(
        fanout_is_error(&[err("a"), err("b")]),
        "all errors is an error set"
    );
    assert!(
        !fanout_is_error(&[err("a"), ok("b")]),
        "one clean answer clears the set"
    );
    assert!(!fanout_is_error(&[]), "an empty set has nothing to report");
}

/// The row-building loop pairs each account's own envelope with its name: with
/// two distinguishable envelopes, a reversed zip would render the wrong answer
/// under each name.
#[test]
fn fold_fanout_rows_pairs_each_envelope_with_its_own_account() {
    let _home = HomeSandbox::new();
    let names = vec!["solo".to_string(), "vendor".to_string()];
    let rows = fold_fanout_rows(
        &names,
        &std::collections::HashMap::new(),
        vec![
            Ok(serde_json::json!({ "result": "solo-out" })),
            Ok(serde_json::json!({ "result": "vendor-out" })),
        ],
        0,
    );
    assert_eq!(rows.len(), 2, "one row per account");
    assert_eq!(
        rows[0]["result"].as_str(),
        Some("solo-out"),
        "the first row carries the first account's envelope",
    );
    assert_eq!(
        rows[1]["result"].as_str(),
        Some("vendor-out"),
        "the second row carries the second account's envelope",
    );
    assert_eq!(
        rows[0]["live_usage"]["profile"].as_str(),
        Some("solo"),
        "the first row is folded under the first name",
    );
    assert_eq!(
        rows[1]["live_usage"]["profile"].as_str(),
        Some("vendor"),
        "the second row is folded under the second name",
    );
}

/// A member whose task died becomes its own error row; the siblings' envelopes
/// pass through untouched.
#[test]
fn fold_fanout_rows_turns_one_members_error_into_its_own_row() {
    let _home = HomeSandbox::new();
    let names = vec!["solo".to_string(), "vendor".to_string()];
    let rows = fold_fanout_rows(
        &names,
        &std::collections::HashMap::new(),
        vec![
            Ok(serde_json::json!({ "result": "fine" })),
            Err("delegate task panicked: task 0 panicked".to_string()),
        ],
        0,
    );
    assert_eq!(rows.len(), 2, "both members produce a row");
    assert_ne!(
        rows[0].get("is_error").and_then(|v| v.as_bool()),
        Some(true),
        "the healthy member stays clean",
    );
    assert_eq!(
        rows[1].get("is_error").and_then(|v| v.as_bool()),
        Some(true),
        "the failed member is an error row",
    );
    assert!(
        rows[1]["result"]
            .as_str()
            .is_some_and(|s| s.contains("delegate task panicked")),
        "the error row names the panic: {}",
        rows[1]["result"],
    );
    assert_eq!(
        rows[1]["live_usage"]["profile"].as_str(),
        Some("vendor"),
        "the error row still names its own account",
    );
}

/// A member with no outcome at all (its join slot never filled) still holds
/// its place in the result set, so a later answer cannot shift onto its name.
#[test]
fn fold_fanout_rows_keeps_a_lost_member_in_its_own_slot() {
    let _home = HomeSandbox::new();
    let names = vec![
        "solo".to_string(),
        "vendor".to_string(),
        "kerry".to_string(),
    ];
    let rows = fold_fanout_rows(
        &names,
        &std::collections::HashMap::new(),
        vec![
            Ok(serde_json::json!({ "result": "solo-out" })),
            Err("delegate result lost".to_string()),
            Ok(serde_json::json!({ "result": "kerry-out" })),
        ],
        0,
    );
    assert_eq!(rows.len(), 3, "one row per account, none shifted away");
    assert_eq!(
        rows[0]["result"].as_str(),
        Some("solo-out"),
        "the first account keeps its own envelope",
    );
    assert_eq!(
        rows[1].get("is_error").and_then(|v| v.as_bool()),
        Some(true),
        "the lost member is its own error row",
    );
    assert_eq!(
        rows[1]["result"].as_str(),
        Some("delegate result lost"),
        "the lost row names what happened",
    );
    assert_eq!(
        rows[2]["result"].as_str(),
        Some("kerry-out"),
        "the third account's envelope did not shift onto the second name",
    );
    assert_eq!(
        rows[2]["live_usage"]["profile"].as_str(),
        Some("kerry"),
        "the third row is still folded under the third name",
    );
}

/// The fold uses the reply's own time for throughput freshness: a rate-limit
/// recorded `now` reads as recent, and one past the recent window does not.
/// This is what a pre-run `now` would get wrong on a long fan-out.
#[test]
fn fold_fanout_rows_ages_a_rate_limit_off_after_the_recent_window() {
    let _home = HomeSandbox::new();
    crate::testutil::register_names(&["solo"]);
    crate::throughput::record_rate_limit(
        &crate::profile::ProfileName::from("solo"),
        Some("claude-opus"),
        Some(10),
        1_000,
    );
    let names = vec!["solo".to_string()];
    let envelope = || Ok(serde_json::json!({ "result": "boom" }));
    let fresh = fold_fanout_rows(
        &names,
        &std::collections::HashMap::new(),
        vec![envelope()],
        1_000,
    );
    assert!(
        fresh[0]["live_usage"]["throughput_warning"]
            .as_str()
            .is_some_and(|s| s.contains("rate-limited")),
        "a rate-limit inside the recent window is flagged: {}",
        fresh[0]["live_usage"],
    );
    let stale = fold_fanout_rows(
        &names,
        &std::collections::HashMap::new(),
        vec![envelope()],
        2_000,
    );
    assert!(
        stale[0]["live_usage"].get("throughput_warning").is_none(),
        "a rate-limit past the recent window ages off: {}",
        stale[0]["live_usage"],
    );
}

// ── the fan-out's detached tasks ─────────────────────────────────────────────

/// `call_delegate` must join the fan-out's detached tasks while its runtime is
/// still alive.
///
/// `tokio::task::spawn_blocking` schedules NON-MANDATORY work: a task still
/// queued when its runtime shuts down is discarded un-run. Measured on a loaded
/// box, twice, on two different fan-out tests: two tasks spawned, one never
/// entered its closure, and its job sat at `running` past 120s. Through the
/// 10s wall clock the module used to poll, that read as a timeout — always
/// within 60ms of the ceiling, one whole-suite release run in three, in a
/// module the diff under test never touched. The deadline was the symptom.
///
/// Pinned on the join rather than on the job states, because the job states
/// only red when the race happens to bite; an empty registry reds every time
/// the join is gone.
#[test]
fn a_fanout_joins_its_detached_tasks_before_the_driver_returns() {
    let home = HomeSandbox::new();
    seed_profiles(&["solo", "vendor"], false);

    let result = call_delegate(DelegateArgs {
        profiles: Some(vec!["solo".to_string(), "vendor".to_string()]),
        prompt: Some("hi".to_string()),
        background: Some(true),
        // Stops each detached task at the cwd gate, so no `claude` spawns.
        cwd: Some(
            home.home()
                .join("does-not-exist")
                .to_str()
                .unwrap()
                .to_string(),
        ),
        ..base()
    });
    assert_ne!(
        result.is_error,
        Some(true),
        "a valid fan-out is not an error"
    );

    assert_eq!(
        crate::testutil::pending_background_tasks(),
        0,
        "the driver joined both detached tasks before dropping its runtime; \
         leaving them to teardown lets the runtime discard a queued one un-run"
    );
    crate::testutil::assert_jobs_done(2);
}
