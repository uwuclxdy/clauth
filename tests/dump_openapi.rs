//! `clauth daemon --dump-openapi` against the real binary: the dump prints the
//! served OpenAPI document, leaves home alone, and a reader that left does not
//! change the run's exit code. Spawning is the only way to see the bytes a
//! shell would capture, the exit code it would get, and that the home dir stays
//! empty — `tests/inline/cli.rs` drives `write_openapi_document` into a buffer,
//! which proves nothing about the dispatch arm that runs first.
//!
//! Unix only, for the reason `tests/closed_reader.rs` gives: the child resolves
//! its home through `$HOME`, which only Unix lets a test point at a sandbox.
#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::process::{Command, Stdio};

use serde_json::Value;

/// `clauth daemon --dump-openapi` with its home pointed at `home` and nothing
/// inherited that names another.
fn dump(home: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_clauth"));
    cmd.args(["daemon", "--dump-openapi"])
        .env("HOME", home)
        .env_remove("CLAUDE_CONFIG_DIR")
        .env_remove("CODEX_HOME")
        .stdin(Stdio::null());
    cmd
}

/// The dump is the served OpenAPI document, on stdout, and it leaves the home
/// dir alone: the dump arm returns before anything touches home, so CI can pin
/// the spec without a daemon or a `.clauth` tree.
#[test]
fn dump_prints_the_document_and_leaves_home_empty() {
    let home = tempfile::tempdir().expect("home");
    let out = dump(home.path())
        .output()
        .expect("run clauth daemon --dump-openapi");
    assert_eq!(out.status.code(), Some(0), "the dump exits 0");

    let document: Value = serde_json::from_slice(&out.stdout).expect("stdout is JSON");
    assert!(
        document["openapi"]
            .as_str()
            .is_some_and(|v| v.starts_with("3.")),
        "the document's `openapi` field names a 3.x version"
    );
    assert!(
        document["paths"].get("/api/v1/status").is_some(),
        "the document's `paths` holds /api/v1/status"
    );

    assert!(
        std::fs::read_dir(home.path())
            .expect("read home")
            .next()
            .is_none(),
        "the dump must not create anything under HOME"
    );
}

/// A reader that left mid-dump does not change the run's exit code: `out.rs`
/// classifies the closed stdout as the reader's outcome, not this run failing,
/// so the run exits 0 exactly as a reader that stayed would.
#[test]
fn a_reader_that_left_gets_exit_0() {
    let home = tempfile::tempdir().expect("home");
    let mut child = dump(home.path())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn clauth daemon --dump-openapi");
    // Closing the read end before the child is near its write is what
    // `clauth daemon --dump-openapi | head -0` does to it.
    drop(child.stdout.take());
    let status = child.wait().expect("wait");
    assert_eq!(
        status.code(),
        Some(0),
        "a gone reader must not fail the dump"
    );
}
