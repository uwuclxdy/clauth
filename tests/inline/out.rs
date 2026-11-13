//! Emitter tests. The `EPIPE` arm is driven through [`write_chunk`] against
//! writers that fail on demand — [`emit`] itself ends the process, so nothing
//! here can call it. What a caller actually sees is in `tests/closed_reader.rs`,
//! which spawns the binary.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;
use std::io::{Error, ErrorKind};
use std::path::Path;

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

/// [`emit_or_drop`]'s writes, on writers that fail on demand: a line after a
/// settled spend never stops the run. A gone reader drops it silently, any
/// other stdout failure is named on stderr, and a stderr that fails too is
/// dropped as well; none of them panics.
#[test]
fn a_dropping_write_never_panics_and_names_only_a_real_failure() {
    let mut err: Vec<u8> = Vec::new();
    write_or_drop(
        &mut Failing(ErrorKind::BrokenPipe),
        &mut err,
        format_args!("a line"),
        true,
    );
    assert!(err.is_empty(), "a gone reader is no failure to name");

    write_or_drop(
        &mut Failing(ErrorKind::StorageFull),
        &mut err,
        format_args!("a line"),
        true,
    );
    assert_eq!(
        String::from_utf8(err).unwrap(),
        format!(
            "clauth: failed printing to stdout: {}\n",
            Error::from(ErrorKind::StorageFull)
        ),
    );

    write_or_drop(
        &mut Failing(ErrorKind::StorageFull),
        &mut Failing(ErrorKind::StorageFull),
        format_args!("a line"),
        true,
    );

    let mut out: Vec<u8> = Vec::new();
    let mut err: Vec<u8> = Vec::new();
    write_or_drop(&mut out, &mut err, format_args!("hi {}", 1), true);
    assert_eq!(String::from_utf8(out).unwrap(), "hi 1\n");
    assert!(err.is_empty());
}

/// The stderr twin of [`write_or_drop`]: a line before work the run still owes
/// never stops that work, whatever the write hits.
#[test]
fn a_dropping_error_write_never_panics() {
    write_err_or_drop(&mut Failing(ErrorKind::StorageFull), format_args!("a line"));
    write_err_or_drop(&mut Failing(ErrorKind::BrokenPipe), format_args!("a line"));

    let mut err: Vec<u8> = Vec::new();
    write_err_or_drop(&mut err, format_args!("hi {}", 1));
    assert_eq!(String::from_utf8(err).unwrap(), "hi 1\n");
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
///
/// A third limit is reachable: it cannot see a hand-rolled `write!` or
/// `writeln!` to `std::io::stdout()` or `std::io::stderr()`, so which write
/// policy such a site takes stays a review call.
#[test]
fn no_bare_print_macro_under_src() {
    const BANNED: [&str; 4] = ["println!", "print!", "eprintln!", "eprint!"];
    let mut offenders = Vec::new();
    let scanned = crate::testutil::rs_files(&Path::new(env!("CARGO_MANIFEST_DIR")).join("src"));
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

/// The CLI prints plain ASCII text: no marks, separators, ellipses or
/// typographic dashes. Default-deny over every non-ASCII character in code
/// under `src/`, so a new glyph reds the moment it lands; `clap` renders
/// `src/cli.rs`'s `///` lines as `--help`, so those count as code there.
///
/// Out of scope, each named: the TUI (`src/tui/`) and the MCP server's replies
/// (`src/mcp/`) keep their own text rules, `src/testutil.rs` is test-only; [`GLYPH_EXEMPT`] lists the other
/// surfaces outside the CLI's own text (the generated `config.toml` comments
/// and the daemon API's OpenAPI document, which `daemon --dump-openapi` prints
/// verbatim). [`glyph_allowed`] holds the two glyph kinds the CLI keeps.
///
/// A `\u{...}` escape in a string counts as the character it spells. Limits shared with
/// `no_bare_print_macro_under_src`: a `//` inside a string literal hides the
/// rest of its line, and a glyph in a block comment reds. Its own limit: an
/// escape right after a `'` is read as a char literal and skipped, so a glyph
/// spelled `'\u{2014}'` and printed through `{}`, or a string spelling a quote
/// then an escape, passes.
#[test]
fn no_glyph_in_cli_text_under_src() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let scanned = crate::testutil::rs_files(&root.join("src"));
    assert!(
        scanned.len() > 10,
        "the scan found {} files under src/ — it is not reading the crate",
        scanned.len()
    );
    let mut offenders = Vec::new();
    for file in scanned {
        let rel = file
            .strip_prefix(root)
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        if rel.starts_with("src/tui/") || rel.starts_with("src/mcp/") || rel == "src/testutil.rs" {
            continue;
        }
        let text = std::fs::read_to_string(&file).unwrap();
        for (n, line) in text.lines().enumerate() {
            let help = rel == "src/cli.rs" && line.trim_start().starts_with("///");
            let code = if help {
                line
            } else {
                line.split("//").next().unwrap_or_default()
            };
            let exempt = GLYPH_EXEMPT
                .iter()
                .any(|(path, fragment)| rel == *path && line.contains(fragment));
            if exempt {
                continue;
            }
            if let Some(c) = code
                .chars()
                .chain(escaped_chars(code))
                .find(|&c| !c.is_ascii() && !glyph_allowed(c))
            {
                offenders.push(format!("{rel}:{}: {c:?} in {}", n + 1, code.trim()));
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "{} CLI text line(s) carry a non-ASCII glyph; write them in plain ASCII:\n{}",
        offenders.len(),
        offenders.join("\n")
    );
}

/// The characters `\u{...}` escapes in `code` spell, skipping any escape right
/// after a `'`: the char literals under `src/` today are matched or compared,
/// never printed.
fn escaped_chars(code: &str) -> impl Iterator<Item = char> + '_ {
    code.match_indices("\\u{").filter_map(|(at, _)| {
        if code[..at].ends_with('\'') {
            return None;
        }
        let hex = code[at + 3..].split('}').next()?;
        char::from_u32(u32::from_str_radix(hex, 16).ok()?)
    })
}

/// The glyphs the CLI keeps: the `→` arrow and the stderr spinner's braille
/// frames.
fn glyph_allowed(c: char) -> bool {
    c == '\u{2192}' || ('\u{2800}'..='\u{28FF}').contains(&c)
}

/// Lines outside the CLI's own text, by file and a fragment of the line.
const GLYPH_EXEMPT: [(&str, &str); 2] = [
    ("src/profile.rs", "out.push_str(\"# "),
    (
        "src/daemon/api/events.rs",
        "description = \"a server-sent-events stream",
    ),
];

/// What proves the guard's drop restored the real sink is that a SECOND install
/// succeeds at all: `capture_here` asserts the slot is free, so a drop that
/// left the first capture standing panics right there. Emitting into no capture
/// to check the same thing would write the line for real (the terminal running
/// the suite).
#[test]
fn a_capture_takes_the_writes_until_its_guard_drops() {
    let first = OutCapture::new();
    let guard = first.capture_here();
    outln!("line {}", 2);
    out!("prompt: ");
    assert_eq!(
        first.snapshot(),
        b"line 2\nprompt: ".to_vec(),
        "the capture must carry the bytes exactly as written, newline included"
    );
    drop(guard);

    let second = OutCapture::new();
    let _guard = second.capture_here();
    outln!("after the first guard");
    assert_eq!(
        second.snapshot(),
        b"after the first guard\n".to_vec(),
        "the write went somewhere other than the capture standing when it was raised"
    );
}

/// The stderr half captures through its own buffer: `emit_err` checks it the
/// way `emit` checks the stdout capture, so a foreground run's stderr is
/// pinnable without writing to the terminal running the suite.
#[test]
fn an_err_capture_takes_the_stderr_writes_until_its_guard_drops() {
    let first = ErrCapture::new();
    let guard = first.capture_here();
    errln!("err line {}", 2);
    assert_eq!(
        first.snapshot(),
        b"err line 2\n".to_vec(),
        "the capture must carry stderr bytes exactly as written, newline included"
    );
    drop(guard);

    let second = ErrCapture::new();
    let _guard = second.capture_here();
    errln!("after the first guard");
    assert_eq!(
        second.snapshot(),
        b"after the first guard\n".to_vec(),
        "the write went somewhere other than the capture standing when it was raised"
    );
}

/// The stderr capture's per-thread scope, asked the same way as the stdout
/// one's below: a thread that installed nothing is never handed another
/// thread's capture, and a sibling's install and drop leave this thread's own.
#[test]
fn an_err_capture_takes_only_the_writes_raised_on_its_own_thread() {
    let mine = ErrCapture::new();
    let _guard = mine.capture_here();
    errln!("raised before the sibling");

    std::thread::scope(|scope| {
        scope.spawn(|| {
            assert!(
                !captured_err(&format_args!("probed from the sibling thread")),
                "a thread that installed nothing was handed another thread's err capture"
            );
            let theirs = ErrCapture::new();
            let _guard = theirs.capture_here();
            errln!("raised on the sibling thread");
            assert_eq!(
                theirs.snapshot(),
                b"raised on the sibling thread\n".to_vec(),
                "the sibling's own err capture must take its write"
            );
        });
    });

    errln!("raised after the sibling");
    assert_eq!(
        mine.snapshot(),
        b"raised before the sibling\nraised after the sibling\n".to_vec(),
        "the sibling's install and its drop both reached across threads"
    );
}

/// An err guard is a claim on the thread that installed it, like the stdout
/// guard below.
#[test]
fn an_err_capture_guard_cannot_be_carried_to_another_thread() {
    use crate::testutil::{NotSend as _, Probe};

    assert!(Probe::<ErrCapture>::is_send(), "positive control");
    assert!(
        !Probe::<ErrCaptureGuard>::is_send(),
        "a guard that can cross threads restores the wrong thread's sink"
    );
}

/// The per-thread scope IS what keeps one test's capture off another's bytes
/// under the `cargo test` fallback, where every inline test is a thread of one
/// binary. A process-global sink passes every other test in this file, so the
/// discriminating question is what a thread that installed NOTHING sees.
///
/// Asked of [`captured`] directly rather than by raising a write there: a thread
/// with no capture routes to the real sink (the terminal running the suite).
/// The predicate is the same one [`emit`] branches on, and it appends nothing
/// when it answers false.
#[test]
fn a_capture_takes_only_the_writes_raised_on_its_own_thread() {
    let mine = OutCapture::new();
    let _guard = mine.capture_here();
    outln!("raised before the sibling");

    std::thread::scope(|scope| {
        scope.spawn(|| {
            assert!(
                !captured(&format_args!("probed from the sibling thread"), true),
                "a thread that installed nothing was handed another thread's capture"
            );
            let theirs = OutCapture::new();
            let _guard = theirs.capture_here();
            outln!("raised on the sibling thread");
            assert_eq!(
                theirs.snapshot(),
                b"raised on the sibling thread\n".to_vec(),
                "the sibling's own capture must take its write"
            );
        });
    });

    outln!("raised after the sibling");
    assert_eq!(
        mine.snapshot(),
        b"raised before the sibling\nraised after the sibling\n".to_vec(),
        "the sibling's install and its drop both reached across threads"
    );
}

/// A guard is a claim on the thread that installed it. Carried to another one
/// its drop clears THAT thread's empty slot and leaves the installer capturing
/// for the rest of the process, which surfaces as some unrelated test quietly
/// losing its bytes. The `PhantomData<*const ()>` field is the whole of what
/// prevents the move, and dropping that field breaks nothing else.
#[test]
fn a_capture_guard_cannot_be_carried_to_another_thread() {
    use crate::testutil::{NotSend as _, Probe};

    assert!(Probe::<OutCapture>::is_send(), "positive control");
    assert!(
        !Probe::<OutCaptureGuard>::is_send(),
        "a guard that can cross threads restores the wrong thread's sink"
    );
}
