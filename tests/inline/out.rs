//! Emitter tests. The `EPIPE` arm is driven through [`write_chunk`] against
//! writers that fail on demand — [`emit`] itself ends the process, so nothing
//! here can call it. What a caller actually sees is in `tests/closed_reader.rs`,
//! which spawns the binary.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;
use std::io::{Error, ErrorKind};
use std::path::{Path, PathBuf};

/// A writer that fails every write with `kind`, the way a pipe whose reader has
/// exited fails a real one.
struct Failing(ErrorKind);

impl Write for Failing {
    fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
        Err(Error::from(self.0))
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Err(Error::from(self.0))
    }
}

/// A writer that takes the bytes but fails the flush, which is where a prompt's
/// `EPIPE` surfaces: the chunk carries no newline, so nothing forces it out
/// until the explicit flush.
struct FlushFails(ErrorKind);

impl Write for FlushFails {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Err(Error::from(self.0))
    }
}

/// The regression: `clauth sessions | head -3` exited 101 with a Rust panic on
/// stderr because `println!` panics on `EPIPE`. The reader leaving has to come
/// back as an outcome the caller can act on.
#[test]
fn a_closed_reader_is_an_outcome_not_a_panic() {
    let mut w = Failing(ErrorKind::BrokenPipe);
    assert_eq!(
        write_chunk(&mut w, format_args!("a line"), true, "stdout"),
        Wrote::ReaderGone
    );
    assert_eq!(
        write_chunk(&mut w, format_args!("prompt: "), false, "stdout"),
        Wrote::ReaderGone,
        "the prompt form reports it too"
    );
    assert_eq!(
        write_chunk(
            &mut FlushFails(ErrorKind::BrokenPipe),
            format_args!("p: "),
            false,
            "stdout"
        ),
        Wrote::ReaderGone,
        "including when only the flush is what hits the closed pipe"
    );
    assert_eq!(
        write_chunk(&mut w, format_args!("a line"), true, "stderr"),
        Wrote::ReaderGone,
        "the stderr half classifies it the same — `emit_err` then drops the \
         line and returns, so the run keeps its own exit code"
    );
}

/// Only `EPIPE` is the reader leaving. Every other write failure is this run
/// failing, and swallowing one would turn a full disk behind a redirect into a
/// silent success.
#[test]
#[should_panic(expected = "failed printing to stdout")]
fn any_other_write_failure_still_panics() {
    write_chunk(
        &mut Failing(ErrorKind::StorageFull),
        format_args!("a line"),
        true,
        "stdout",
    );
}

/// The stderr half draws the same line, and names its own stream when it does.
#[test]
#[should_panic(expected = "failed printing to stderr")]
fn a_full_disk_behind_a_stderr_redirect_still_panics() {
    write_chunk(
        &mut Failing(ErrorKind::StorageFull),
        format_args!("a line"),
        true,
        "stderr",
    );
}

#[test]
fn a_reachable_reader_gets_the_bytes() {
    let mut buf: Vec<u8> = Vec::new();
    assert_eq!(
        write_chunk(&mut buf, format_args!("hi {}", 1), true, "stdout"),
        Wrote::Yes
    );
    assert_eq!(
        write_chunk(&mut buf, format_args!("p: "), false, "stdout"),
        Wrote::Yes
    );
    assert_eq!(String::from_utf8(buf).unwrap(), "hi 1\np: ");
}

/// Both bugs come back the moment a bare macro lands in `src/`: `println!`
/// exits 101 on a gone reader, `eprintln!` panics on one. Routing is the whole
/// mechanism, so it is the thing worth failing on. No file is exempt, `out.rs`
/// included — it names the macros only in comments, which the strip below
/// drops, and it is the file most likely to be edited by someone touching the
/// emitters.
///
/// Two known limits, neither reachable today: the strip cuts at the first
/// `//`, so a `//` inside a string literal hides a real call later on that same
/// line, and a banned name inside a block comment or a string reds.
#[test]
fn no_bare_print_macro_under_src() {
    const BANNED: [&str; 4] = ["println!", "print!", "eprintln!", "eprint!"];
    let mut offenders = Vec::new();
    let scanned = rs_files(&Path::new(env!("CARGO_MANIFEST_DIR")).join("src"));
    // An unreadable `src/` would leave the scan empty and the assertion below
    // vacuous, which is the one way this test could pass while blind.
    assert!(
        scanned.len() > 10,
        "the scan found {} files under src/ — it is not reading the crate",
        scanned.len()
    );
    for file in scanned {
        let text = std::fs::read_to_string(&file).unwrap();
        for (n, line) in text.lines().enumerate() {
            // Comments name the macros freely; only a call counts.
            let code = line.split("//").next().unwrap_or_default();
            for macro_name in BANNED {
                if calls(code, macro_name) {
                    offenders.push(format!("{}:{}: {macro_name}", file.display(), n + 1));
                }
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "print through `outln!` / `errln!`, never the std macros: {offenders:?}"
    );
}

/// Whether `code` invokes `macro_name` rather than merely ending with its
/// name — `eprintln!` contains `println!`, and `crate::out::errln!` is a call.
fn calls(code: &str, macro_name: &str) -> bool {
    code.match_indices(macro_name).any(|(at, _)| {
        at == 0
            || !code[..at]
                .chars()
                .next_back()
                .is_some_and(|c| c.is_alphanumeric() || c == '_')
    })
}

/// Every `.rs` file under `dir`, recursively.
fn rs_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            out.extend(rs_files(&path));
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
    out
}
