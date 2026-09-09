//! What a bare `clauth` does when stdout is not a terminal, driven against the
//! real binary.
//!
//! The bare (no-subcommand) invocation is the TUI entry, and its non-TTY arm
//! (clap's command help on stderr, exit 2) cannot be exercised from inside a
//! test process: the dispatch arm reads `std::io::stdout().is_terminal()`
//! live, so an in-process pin would depend on the runner's own terminal — a
//! `--nocapture` run on a pty would take the TUI arm and hijack it. Spawning
//! with piped stdio makes the arm deterministic whatever the runner runs on,
//! the same reason `closed_reader.rs` spawns.
//!
//! Unix only, for the same reason as `closed_reader.rs`: the child resolves
//! its home through `dirs`, which on Windows reads `FOLDERID_Profile` from the
//! shell API and no environment variable at all, so the run could not be
//! pointed away from the operator's real `~/.clauth` — and the pre-fix arm
//! that reached `cmd_tui` ran `gc_stale_runtimes` and `load_config` there.
#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::process::{Command, Stdio};
use tempfile::TempDir;

/// A bare `clauth` with stdout piped prints clap's command help on stderr and
/// exits 2 — the missing-subcommand convention, per the owner ruling. Never
/// the pre-fix terminal-init crash (`Failed to initialize the terminal`, os
/// error 6), and stdout stays clean.
#[test]
fn bare_clauth_with_piped_stdout_prints_help_and_exits_2() {
    let home = TempDir::new().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_clauth"))
        .env("HOME", home.path())
        .env_remove("CLAUDE_CONFIG_DIR")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("spawn clauth");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        output.status.code(),
        Some(2),
        "bare clauth on a pipe must exit 2 with the help, got stderr: {stderr}"
    );
    assert!(
        stderr.contains("Usage: clauth") && stderr.contains("start"),
        "clap's command help must reach stderr, got: {stderr}"
    );
    assert!(
        !stderr.contains("Failed to initialize the terminal"),
        "no terminal init may run on this path: {stderr}"
    );
    assert!(
        output.stdout.is_empty(),
        "the help goes to stderr (clap's missing-subcommand convention)"
    );
}
