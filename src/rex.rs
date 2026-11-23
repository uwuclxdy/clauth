//! Rex (experimental): `clauth rex install|uninstall` for the plugin under
//! `rex-plugin/`, and the read-only state the Services tab's `rex` row shows.
//!
//! Rex has no plugin manager. A plugin is a Lua module beside Rex's own
//! `init.lua`, loaded with `require`. So install writes the bundled files to
//! `<rex config dir>/clauth/` and appends one marked `require` block to
//! `init.lua`; uninstall takes both back out. The files are compiled into the
//! binary, so the plugin a user runs always matches the clauth that wrote it,
//! and rerunning install after an upgrade refreshes it. The Services row's
//! reads ([`plugin_status`], [`watcher_running`]) write nothing.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};

use crate::out::{errln, outln};

/// The plugin, as `(file name, contents, executable)`.
const FILES: &[(&str, &str, bool)] = &[
    ("init.lua", include_str!("../rex-plugin/init.lua"), false),
    (
        "tag-watch.lua",
        include_str!("../rex-plugin/tag-watch.lua"),
        false,
    ),
    (
        "start-watch.sh",
        include_str!("../rex-plugin/start-watch.sh"),
        true,
    ),
    (
        "resolve-profile.sh",
        include_str!("../rex-plugin/resolve-profile.sh"),
        true,
    ),
];

/// The plugin dir's marker, and the first line a clauth-written one carries.
/// Install writes it so a later install or uninstall, and the Services row,
/// can tell a tree clauth owns from one the user put there (a checkout, their
/// own fork). The row reads the version off that first line.
const MARKER: &str = ".clauth-managed";
const MARKER_PREFIX: &str = "written by clauth ";

/// The marker and a pidfile are each a few dozen bytes; a FIFO or a runaway
/// file must neither block the read nor balloon it.
const MARKER_CAP: u64 = 4096;
const PIDFILE_CAP: u64 = 64;

/// The script whose argv marks a live pane-label watcher.
const WATCHER_SCRIPT: &str = "tag-watch.lua";

const BLOCK_BEGIN: &str =
    "-- clauth rex plugin: written by `clauth rex install`, removed by `clauth rex uninstall`";
const BLOCK_END: &str = "-- end clauth rex plugin";

#[cfg(test)]
thread_local! {
    /// How many Rex reads ran on this thread, so a test can assert the TUI's
    /// thread runs none, whatever path would call them.
    static READS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// How many Rex reads (`plugin_status`, `watcher_running`) ran on the calling
/// thread.
#[cfg(test)]
pub(crate) fn reads_on_this_thread() -> usize {
    READS.with(std::cell::Cell::get)
}

/// Where Rex reads its config and where the clauth plugin goes beside it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Paths {
    pub(crate) init: PathBuf,
    pub(crate) plugin: PathBuf,
}

impl Paths {
    /// Rex's own rule: `$REX_CONFIG` when set, else `rex/init.lua` under
    /// `$XDG_CONFIG_HOME`, else under `~/.config`. An empty variable is unset.
    pub(crate) fn resolve(
        rex_config: Option<OsString>,
        xdg_config_home: Option<OsString>,
        home: &Path,
    ) -> Self {
        let init = match rex_config.filter(|v| !v.is_empty()) {
            Some(path) => PathBuf::from(path),
            None => xdg_config_home
                .filter(|v| !v.is_empty())
                .map_or_else(|| home.join(".config"), PathBuf::from)
                .join("rex")
                .join("init.lua"),
        };
        let dir = init
            .parent()
            .map_or_else(|| PathBuf::from("."), Path::to_path_buf);
        Self {
            plugin: dir.join("clauth"),
            init,
        }
    }
}

/// What sits at the plugin dir.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PluginDir {
    /// Nothing there.
    Absent,
    /// A dir clauth wrote: the version its marker names.
    Managed { version: String },
    /// A symlink, such as one onto a checkout of the plugin source, and
    /// whether following it reaches nothing (a missing target, a link loop, an
    /// unreadable parent alike).
    Linked {
        target: Option<PathBuf>,
        missing: bool,
    },
    /// A symlink whose target resolves to something other than a folder (a
    /// regular file, a FIFO): Rex loads a folder, so this loads no plugin.
    LinkedNotDir { target: Option<PathBuf> },
    /// Something there that nothing proves clauth wrote: no dir, no readable
    /// marker, or a marker naming no version.
    Foreign,
}

/// The `rex` row's readout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RexStatus {
    pub(crate) plugin: PluginDir,
    pub(crate) watcher_running: bool,
}

/// The watcher state dir: `$XDG_STATE_HOME/clauth-rex`, else
/// `~/.local/state/clauth-rex`. An empty variable is unset.
pub(crate) fn state_dir(xdg_state_home: Option<OsString>, home: &Path) -> PathBuf {
    xdg_state_home
        .filter(|v| !v.is_empty())
        .map_or_else(|| home.join(".local").join("state"), PathBuf::from)
        .join("clauth-rex")
}

/// The plugin dir's state, `None` when Rex's config dir does not exist (the
/// row then does not render). Follows a symlink, so it can hang on a stalled
/// mount: the TUI runs it on a worker, never on the recompute.
pub(crate) fn plugin_status(paths: &Paths) -> Option<PluginDir> {
    #[cfg(test)]
    READS.with(|reads| reads.set(reads.get() + 1));
    if !paths.init.parent().is_some_and(Path::is_dir) {
        return None;
    }
    Some(plugin_at(&paths.plugin))
}

/// The combined readout a test drives. Production runs each reading on its
/// own worker, so only a test pays for both in one call.
#[cfg(test)]
pub(crate) fn status_at(
    paths: &Paths,
    state_dir: &Path,
    procs: &dyn crate::which::ProcTable,
) -> Option<RexStatus> {
    Some(RexStatus {
        plugin: plugin_status(paths)?,
        watcher_running: watcher_running(state_dir, procs),
    })
}

fn plugin_at(plugin: &Path) -> PluginDir {
    let meta = match std::fs::symlink_metadata(plugin) {
        Ok(meta) => meta,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return PluginDir::Absent,
        // Something sits there that cannot be read, so nothing there proves
        // clauth wrote it.
        Err(_) => return PluginDir::Foreign,
    };
    if meta.file_type().is_symlink() {
        let target = std::fs::read_link(plugin).ok();
        return match std::fs::metadata(plugin) {
            Ok(target_meta) if target_meta.is_dir() => PluginDir::Linked {
                target,
                missing: false,
            },
            // The target resolves, but not to a folder: Rex loads a folder,
            // so this loads no plugin.
            Ok(_) => PluginDir::LinkedNotDir { target },
            Err(_) => PluginDir::Linked {
                target,
                missing: true,
            },
        };
    }
    if !meta.is_dir() {
        return PluginDir::Foreign;
    }
    let marker = plugin.join(MARKER);
    // Only a regular file carries a marker: a dir, a FIFO or a dangling link at
    // that name is nothing clauth wrote.
    if !std::fs::metadata(&marker).is_ok_and(|meta| meta.is_file()) {
        return PluginDir::Foreign;
    }
    match crate::runtime::read_regular_capped(&marker, MARKER_CAP) {
        Ok(Some(bytes)) => match marker_version(&bytes) {
            Some(version) => PluginDir::Managed { version },
            // A marker naming no version proves nothing about which clauth
            // wrote the dir.
            None => PluginDir::Foreign,
        },
        // The marker left between the stat and the open: nothing proves clauth
        // wrote the dir.
        Ok(None) => PluginDir::Foreign,
        // A regular marker this user cannot read proves nothing either.
        Err(()) => PluginDir::Foreign,
    }
}

fn marker_version(bytes: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(bytes);
    let version = text.lines().next()?.strip_prefix(MARKER_PREFIX)?;
    (!version.is_empty()).then(|| version.to_string())
}

/// Whether a pidfile under `state_dir` names a live pane-label watcher. Reads
/// the process table, which spawns `ps` off Linux, so callers cache it.
pub(crate) fn watcher_running(state_dir: &Path, procs: &dyn crate::which::ProcTable) -> bool {
    #[cfg(test)]
    READS.with(|reads| reads.set(reads.get() + 1));
    let Ok(entries) = std::fs::read_dir(state_dir) else {
        return false;
    };
    entries
        .flatten()
        .any(|entry| live_watcher(&entry.path(), procs).is_some())
}

/// A pane-label watcher start-watch.sh recorded, still running.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Watcher {
    pub(crate) pidfile: PathBuf,
    pub(crate) pid: u32,
}

/// The watchers start-watch.sh recorded under `state_dir`, one
/// `watch-<key>.pid` per Rex server, that still run. start-watch.sh exits
/// while its pidfile names a live watcher, so one left running would keep the
/// old files going until its Rex server restarts.
pub(crate) fn running_watchers(
    state_dir: &Path,
    procs: &dyn crate::which::ProcTable,
) -> Vec<Watcher> {
    let Ok(entries) = std::fs::read_dir(state_dir) else {
        return Vec::new();
    };
    let mut watchers: Vec<Watcher> = entries
        .flatten()
        .filter_map(|entry| live_watcher(&entry.path(), procs))
        .collect();
    watchers.sort_by_key(|watcher| watcher.pid);
    watchers
}

/// The watcher a `watch-*.pid` file names, when its pid still runs the
/// watcher script. A pid that runs anything else was reused, and is never
/// taken for one (nor signalled).
fn live_watcher(pidfile: &Path, procs: &dyn crate::which::ProcTable) -> Option<Watcher> {
    let name = pidfile.file_name()?.to_str()?;
    if !name.starts_with("watch-") || !name.ends_with(".pid") {
        return None;
    }
    let bytes = crate::runtime::read_regular_capped(pidfile, PIDFILE_CAP).ok()??;
    let pid = std::str::from_utf8(&bytes)
        .ok()?
        .trim()
        .parse::<u32>()
        .ok()?;
    runs_watcher(pid, procs).then(|| Watcher {
        pidfile: pidfile.to_path_buf(),
        pid,
    })
}

/// Whether `pid` runs the watcher script. A gone pid, a zombie and a failed
/// read all read false, so a broken probe never green-lights a signal.
fn runs_watcher(pid: u32, procs: &dyn crate::which::ProcTable) -> bool {
    procs
        .argv(pid)
        .is_some_and(|argv| argv.iter().any(|arg| arg.contains(WATCHER_SCRIPT)))
}

/// How long a stopped watcher gets to exit before install reloads Rex.
/// A `rex do` script ends at once on SIGTERM; this bounds a wedged one.
const WATCHER_STOP_TIMEOUT: Duration = Duration::from_secs(2);

/// SIGTERMs each watcher and waits up to `timeout` for it to go, then removes
/// its pidfile, so the config reload that follows install starts a fresh one.
/// Best effort: one that will not stop is reported, not fatal, and keeps its
/// pidfile, so that reload's start-watch.sh finds it live and starts no second
/// watcher beside it.
pub(crate) fn stop_watchers(
    watchers: &[Watcher],
    procs: &dyn crate::which::ProcTable,
    timeout: Duration,
) {
    for watcher in watchers {
        // The list was read before the prompt: the watcher may have exited
        // since, and its pid gone to another program.
        if !runs_watcher(watcher.pid, procs) {
            continue;
        }
        terminate(watcher.pid);
        let deadline = Instant::now() + timeout;
        let mut stopped = true;
        while runs_watcher(watcher.pid, procs) {
            if Instant::now() >= deadline {
                errln!(
                    "clauth: the Rex pane-label watcher (pid {}) is still running; \
                     stop it with `kill {}`",
                    watcher.pid,
                    watcher.pid
                );
                stopped = false;
                break;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        if stopped {
            remove_pidfile_naming(&watcher.pidfile, watcher.pid);
        }
    }
}

/// Removes `pidfile` while it still names `pid`. A config load since the stop
/// may have recorded a fresh watcher there, and that one's pidfile stays.
fn remove_pidfile_naming(pidfile: &Path, pid: u32) {
    let names_pid = crate::runtime::read_regular_capped(pidfile, PIDFILE_CAP)
        .ok()
        .flatten()
        .and_then(|bytes| String::from_utf8(bytes).ok())
        .is_some_and(|text| text.trim().parse::<u32>().ok() == Some(pid));
    if names_pid {
        let _ = std::fs::remove_file(pidfile);
    }
}

/// The plan line for stopping `watchers`, or `None` when none runs.
fn stop_line(watchers: &[Watcher]) -> Option<String> {
    match watchers.len() {
        0 => None,
        1 => Some("  stop the running pane-label watcher".to_string()),
        n => Some(format!("  stop the {n} running pane-label watchers")),
    }
}

/// SIGTERM through `kill(1)`, as `daemon::probe::terminate_pid` does, so
/// this spends no `unsafe`. A pid that exited meanwhile is not an error.
#[cfg(unix)]
fn terminate(pid: u32) {
    let _ = Command::new("kill")
        .args(["-s", "TERM", &pid.to_string()])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

#[cfg(not(unix))]
fn terminate(_pid: u32) {}

/// The block install appends. Uninstall matches all three of its lines, so
/// none of them changes spelling between releases.
pub(crate) fn block() -> String {
    format!("{BLOCK_BEGIN}\n{BLOCK_REQUIRE}\n{BLOCK_END}\n")
}

const BLOCK_REQUIRE: &str = "require(\"clauth\").setup{}";

/// One line of `init.lua` as Lua counts lines: a `\n` or a `\r` ends one, and
/// a `\r\n` or `\n\r` pair is a single line end.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Line {
    start: usize,
    /// Where the line's text stops, before its line end.
    end: usize,
    /// Where the next line starts, past the line end.
    next: usize,
}

fn lua_lines(text: &str) -> Vec<Line> {
    let src = text.as_bytes();
    let mut lines = Vec::new();
    let mut start = 0;
    let mut at = 0;
    while at < src.len() {
        if matches!(src[at], b'\n' | b'\r') {
            let next = past_line_end(src, at);
            lines.push(Line {
                start,
                end: at,
                next,
            });
            start = next;
            at = next;
        } else {
            at += 1;
        }
    }
    if start < src.len() || lines.is_empty() {
        lines.push(Line {
            start,
            end: src.len(),
            next: src.len(),
        });
    }
    lines
}

/// Past the line end at `at`, a pair counting as one; `at` itself when no
/// line end starts there.
fn past_line_end(src: &[u8], at: usize) -> usize {
    match (src.get(at), src.get(at + 1)) {
        (Some(b'\n'), Some(b'\r')) | (Some(b'\r'), Some(b'\n')) => at + 2,
        (Some(b'\n' | b'\r'), _) => at + 1,
        _ => at,
    }
}

/// Where the line holding `at` stops, before its line end.
fn line_end(src: &[u8], at: usize) -> usize {
    src[at..]
        .iter()
        .position(|&b| matches!(b, b'\n' | b'\r'))
        .map_or(src.len(), |n| at + n)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Marker {
    Begin,
    End,
}

/// What clauth keeps of a token: names, string values, numbers and markers.
/// The rest of the code is one `Punct` per byte, a `..` or `...` one in all,
/// which is enough to tell a `require(` from a `require` that something else
/// follows.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Lexeme {
    Name(String),
    /// A string literal's value, escapes decoded.
    Str(Vec<u8>),
    /// One of clauth's marker lines: a `--` comment that starts its line.
    Marker(Marker),
    /// A number, one token however many bytes it spans.
    Number,
    Punct(u8),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Token {
    lexeme: Lexeme,
    /// The byte offset it starts at.
    at: usize,
}

/// Lexes `init.lua` the way Rex's Lua (gopher-lua, which lexes Lua 5.1)
/// does, so a comment or a string ends exactly where Rex ends it: a line
/// comment and a quoted string stop at a lone `\r` as at a `\n`. A file it
/// rejects, Rex rejects too, and the error names the byte it starts at.
fn lex(text: &str) -> Result<Vec<Token>, (Why, usize)> {
    let src = text.as_bytes();
    let mut tokens = Vec::new();
    // Lua's file loader skips a first line that starts with `#`, as
    // `#!/usr/bin/env lua`, reading it up to a `\n` only: a lone `\r` on it
    // does not end it.
    let mut at = if src.first() == Some(&b'#') {
        src.iter().position(|&b| b == b'\n').unwrap_or(src.len())
    } else {
        0
    };
    while let Some(&c) = src.get(at) {
        let start = at;
        let lexeme = match c {
            b' ' | b'\t' | b'\n' | b'\r' => {
                at += 1;
                continue;
            }
            b'-' if src.get(at + 1) == Some(&b'-') => {
                at += 2;
                match long_bracket(src, at).map_err(|()| (Why::BadLongBracket, start))? {
                    Some(level) => {
                        at = long_string(src, at, level)
                            .ok_or((Why::UnfinishedLong, start))?
                            .1;
                        continue;
                    }
                    None => {
                        at = line_end(src, at);
                        let starts_line = start == 0 || matches!(src[start - 1], b'\n' | b'\r');
                        match text[start..at].trim_end() {
                            BLOCK_BEGIN if starts_line => Lexeme::Marker(Marker::Begin),
                            BLOCK_END if starts_line => Lexeme::Marker(Marker::End),
                            _ => continue,
                        }
                    }
                }
            }
            b'[' => match long_bracket(src, at).map_err(|()| (Why::BadLongBracket, start))? {
                Some(level) => {
                    let (value, end) =
                        long_string(src, at, level).ok_or((Why::UnfinishedLong, start))?;
                    at = end;
                    Lexeme::Str(value)
                }
                None => {
                    at += 1;
                    Lexeme::Punct(c)
                }
            },
            b'"' | b'\'' => {
                let (value, end) = short_string(src, at).ok_or((Why::UnfinishedString, start))?;
                at = end;
                Lexeme::Str(value)
            }
            c if c == b'_' || c.is_ascii_alphabetic() => {
                at += src[at..]
                    .iter()
                    .take_while(|&&b| b == b'_' || b.is_ascii_alphanumeric())
                    .count();
                Lexeme::Name(text[start..at].to_string())
            }
            b'.' if src.get(at + 1) == Some(&b'.') => {
                at += if src.get(at + 2) == Some(&b'.') { 3 } else { 2 };
                Lexeme::Punct(c)
            }
            c if c.is_ascii_digit()
                || (c == b'.' && src.get(at + 1).is_some_and(u8::is_ascii_digit)) =>
            {
                at = number_end(src, at);
                Lexeme::Number
            }
            c => {
                at += 1;
                Lexeme::Punct(c)
            }
        };
        tokens.push(Token { lexeme, at: start });
    }
    Ok(tokens)
}

/// Past the number starting at `at`, a digit or a `.` before one, as
/// gopher-lua's `scanNumber` scans it: a hex number takes hex digits only,
/// and an exponent mark takes its sign, then the next byte whatever it is (a
/// `\r\n` or `\n\r` pair as one), so `1e--` is one number and opens no
/// comment.
fn number_end(src: &[u8], at: usize) -> usize {
    let digits = |from: usize| {
        from + src[from..]
            .iter()
            .take_while(|b| b.is_ascii_digit())
            .count()
    };
    if src[at] == b'0' && matches!(src.get(at + 1), Some(b'x' | b'X')) {
        let hex = src[at + 2..]
            .iter()
            .take_while(|b| b.is_ascii_hexdigit())
            .count();
        return at + 2 + hex;
    }
    let mut end = digits(at + 1);
    if src.get(end) == Some(&b'.') {
        end = digits(end + 1);
    }
    if matches!(src.get(end), Some(b'e' | b'E')) {
        end += 1;
        if matches!(src.get(end), Some(b'+' | b'-')) {
            end += 1;
        }
        end = match src.get(end) {
            Some(b'\n' | b'\r') => past_line_end(src, end),
            Some(_) => end + 1,
            None => end,
        };
        end = digits(end);
    }
    end
}

/// The level of the long bracket opening at `at` (`[[` is 0, `[==[` is 2),
/// or `None` when no `[` there opens one. A `[=` that never reaches its
/// second `[` is an error, as Rex's lexer makes it.
fn long_bracket(src: &[u8], at: usize) -> Result<Option<usize>, ()> {
    if src.get(at) != Some(&b'[') {
        return Ok(None);
    }
    let level = src[at + 1..].iter().take_while(|&&b| b == b'=').count();
    match src.get(at + 1 + level) {
        Some(b'[') => Ok(Some(level)),
        _ if level == 0 => Ok(None),
        _ => Err(()),
    }
}

/// The long string (or long comment's body) whose bracket of `level` opens
/// at `at`: its value, line ends read as `\n`, and where it ends, past its
/// close. `None` when it never closes.
fn long_string(src: &[u8], at: usize, level: usize) -> Option<(Vec<u8>, usize)> {
    // A line end straight after the open bracket is no part of the value.
    let mut i = past_line_end(src, at + level + 2);
    let mut value = Vec::new();
    loop {
        let &b = src.get(i)?;
        if b == b']'
            && src[i + 1..].iter().take_while(|&&b| b == b'=').count() >= level
            && src.get(i + 1 + level) == Some(&b']')
        {
            return Some((value, i + level + 2));
        }
        if matches!(b, b'\n' | b'\r') {
            value.push(b'\n');
            i = past_line_end(src, i);
        } else {
            value.push(b);
            i += 1;
        }
    }
}

/// The quoted string opening at `at`: its value and where it ends, past its
/// closing quote. `None` when a line end or the file's end comes first. The
/// escapes are gopher-lua's, Lua 5.1's: `\x` and `\z` stand for the letter.
fn short_string(src: &[u8], at: usize) -> Option<(Vec<u8>, usize)> {
    let quote = src[at];
    let mut value = Vec::new();
    let mut i = at + 1;
    loop {
        let &b = src.get(i)?;
        i += 1;
        match b {
            b'\n' | b'\r' => return None,
            b'\\' => {
                let &escaped = src.get(i)?;
                if matches!(escaped, b'\n' | b'\r') {
                    value.push(b'\n');
                    i = past_line_end(src, i);
                } else if escaped.is_ascii_digit() {
                    let digits = src[i..]
                        .iter()
                        .take(3)
                        .take_while(|b| b.is_ascii_digit())
                        .count();
                    let code = src[i..i + digits]
                        .iter()
                        .fold(0u32, |n, d| n * 10 + u32::from(d - b'0'));
                    value.push(code.to_le_bytes()[0]);
                    i += digits;
                } else {
                    value.push(match escaped {
                        b'a' => 0x07,
                        b'b' => 0x08,
                        b'f' => 0x0c,
                        b'n' => b'\n',
                        b'r' => b'\r',
                        b't' => b'\t',
                        b'v' => 0x0b,
                        other => other,
                    });
                    i += 1;
                }
            }
            b if b == quote => return Some((value, i)),
            b => value.push(b),
        }
    }
}

/// Why install or uninstall stopped before changing `init.lua`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Why {
    /// A quoted string that a line end or the file's end cuts short.
    UnfinishedString,
    /// A long string or comment (`[[ ]]`, `--[==[ ]==]`) that never closes.
    UnfinishedLong,
    /// A `[=` that never reaches its second `[`.
    BadLongBracket,
    /// A begin line with no end line after it.
    Unterminated,
    /// A begin line whose next marker, at this 1-based line, begins again.
    BeginAgain { next: usize },
    /// An end line that no begin line opens.
    StrayEnd,
    /// A line inside clauth's block that clauth did not write.
    NotClauths,
    /// A string outside clauth's block that names the module.
    NamesClauth,
}

/// A refusal, naming the line it is about: its 1-based number and its text.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Refusal {
    line: usize,
    text: String,
    why: Why,
}

impl Refusal {
    /// The error `clauth rex <cmd>` prints for it.
    fn message(&self, init: &Path, cmd: &str) -> String {
        let unreadable = "so clauth cannot tell its code from its comments";
        let unreadable_fix =
            format!("fix that line, which Rex cannot load either, then rerun `clauth rex {cmd}`");
        let (what, fix) = match self.why {
            Why::UnfinishedString => (
                format!("has a string that does not end on its line, {unreadable}"),
                unreadable_fix,
            ),
            Why::UnfinishedLong => (
                format!("opens a long string or comment that never closes, {unreadable}"),
                unreadable_fix,
            ),
            Why::BadLongBracket => (
                format!("has a `[=` that opens no long bracket, {unreadable}"),
                unreadable_fix,
            ),
            Why::Unterminated => (
                "begins clauth's block, but no end line closes it".to_string(),
                format!(
                    "put its `{BLOCK_END}` line back or take the block out by hand, \
                     then rerun `clauth rex {cmd}`"
                ),
            ),
            Why::BeginAgain { next } => (
                format!("begins clauth's block, but line {next} begins another before it ends"),
                format!(
                    "take out the begin line that has no end line, then rerun `clauth rex {cmd}`"
                ),
            ),
            Why::StrayEnd => (
                "ends clauth's block, but no begin line opens it".to_string(),
                format!("take that line out, then rerun `clauth rex {cmd}`"),
            ),
            Why::NotClauths => (
                "is inside clauth's block, but clauth did not write it".to_string(),
                format!("move that line out of the block first, then rerun `clauth rex {cmd}`"),
            ),
            Why::NamesClauth => (
                "names clauth outside clauth's block, so it may still load the plugin".to_string(),
                format!(
                    "take that out first, then rerun `clauth rex {cmd}`; removing clauth/ \
                     would leave it naming a module that is gone"
                ),
            ),
        };
        format!(
            "{}:{} {what}:\n  {}\n  {fix}",
            init.display(),
            self.line,
            self.text
        )
    }
}

/// Rex's `init.lua` as clauth reads it, lexed the way Rex lexes it: a
/// marker or a `require` inside a comment or a string is neither, and
/// every begin line reaches its end line before the next begin.
#[derive(Debug)]
struct InitLua<'a> {
    text: &'a str,
    lines: Vec<Line>,
    tokens: Vec<Token>,
    /// clauth's blocks, each from its begin line through its end line, by
    /// 0-based line.
    blocks: Vec<(usize, usize)>,
}

impl<'a> InitLua<'a> {
    /// Reads `text`, or refuses a file whose markers do not pair up or that
    /// does not lex. Install and uninstall both read it through here, so a
    /// line one of them takes for a marker the other does too.
    fn read(text: &'a str) -> Result<Self, Refusal> {
        let lines = lua_lines(text);
        let mut init = Self {
            text,
            lines,
            tokens: Vec::new(),
            blocks: Vec::new(),
        };
        init.tokens = lex(text).map_err(|(why, at)| init.refusal(init.line_of(at), why))?;
        let mut open = None;
        for token in &init.tokens {
            let Lexeme::Marker(marker) = token.lexeme else {
                continue;
            };
            let line = init.line_of(token.at);
            open = match (marker, open) {
                (Marker::Begin, None) => Some(line),
                (Marker::Begin, Some(begin)) => {
                    return Err(init.refusal(begin, Why::BeginAgain { next: line + 1 }));
                }
                (Marker::End, Some(begin)) => {
                    init.blocks.push((begin, line));
                    None
                }
                (Marker::End, None) => return Err(init.refusal(line, Why::StrayEnd)),
            };
        }
        match open {
            Some(begin) => Err(init.refusal(begin, Why::Unterminated)),
            None => Ok(init),
        }
    }

    /// The 0-based line holding byte `at`.
    fn line_of(&self, at: usize) -> usize {
        self.lines.partition_point(|line| line.next <= at)
    }

    fn line_text(&self, n: usize) -> &'a str {
        let line = self.lines[n];
        &self.text[line.start..line.end]
    }

    fn refusal(&self, n: usize, why: Why) -> Refusal {
        Refusal {
            line: n + 1,
            text: self.line_text(n).trim_end().to_string(),
            why,
        }
    }

    fn in_block(&self, n: usize) -> bool {
        self.blocks
            .iter()
            .any(|&(begin, end)| (begin..=end).contains(&n))
    }

    /// The first line outside clauth's blocks that loads the module, as a
    /// hand-written `require("clauth").setup{ ... }` with the user's options
    /// does, in any of the call's spellings: `require "clauth"`,
    /// `require'clauth'`, `require [[clauth]]`, or split over lines.
    fn loading_line(&self) -> Option<usize> {
        let code: Vec<&Token> = self
            .tokens
            .iter()
            .filter(|token| !matches!(token.lexeme, Lexeme::Marker(_)))
            .collect();
        code.iter().enumerate().find_map(|(i, token)| {
            if !matches!(&token.lexeme, Lexeme::Name(name) if name == "require") {
                return None;
            }
            let line = self.line_of(token.at);
            if self.in_block(line) {
                return None;
            }
            let mut rest = code[i + 1..].iter().map(|token| &token.lexeme);
            let mut arg = rest.next();
            if arg == Some(&Lexeme::Punct(b'(')) {
                arg = rest.next();
            }
            matches!(arg, Some(Lexeme::Str(value)) if value == b"clauth").then_some(line)
        })
    }

    /// Whether the file already loads the module: through a block of
    /// clauth's, or a `require` of the user's own. Install asks for no more
    /// than a `require`, since a string naming clauth for another reason (a
    /// pane command running `clauth`) must not keep it from wiring the
    /// plugin.
    fn loads_plugin(&self) -> bool {
        !self.blocks.is_empty() || self.loading_line().is_some()
    }

    /// The file with clauth's block appended, or `None` when it already
    /// loads the module.
    fn with_block(&self) -> Option<String> {
        if self.loads_plugin() {
            return None;
        }
        let mut text = self.text.to_string();
        let mut block = block();
        if !text.is_empty() && !text.ends_with(['\n', '\r']) {
            // The block needs a line of its own. It then ends without a
            // final newline of its own, which tells uninstall to take this
            // one back.
            text.push('\n');
            block.pop();
        }
        text.push_str(&block);
        Some(text)
    }

    /// The file without clauth's blocks: each goes from the start of its
    /// begin line through its end line's line end. Refuses a block holding a
    /// line clauth did not write, which would go with it: one the user added,
    /// or clauth's `require` edited to pass options.
    fn without_blocks(&self) -> Result<String, Refusal> {
        for &(begin, end) in &self.blocks {
            if let Some(n) =
                (begin + 1..end).find(|&n| self.line_text(n).trim_end() != BLOCK_REQUIRE)
            {
                return Err(self.refusal(n, Why::NotClauths));
            }
        }
        let mut text = String::with_capacity(self.text.len());
        let mut kept = 0;
        for &(begin, end) in &self.blocks {
            text.push_str(&self.text[kept..self.lines[begin].start]);
            kept = self.lines[end].next;
        }
        let rest = &self.text[kept..];
        let ends_file_bare = self
            .blocks
            .last()
            .is_some_and(|&(_, end)| self.lines[end].end == self.text.len());
        if ends_file_bare && text.ends_with('\n') && !text.ends_with("\r\n") {
            // The block ends the file with no final newline: install put it
            // on a line of its own after a file that had none (see
            // `with_block`).
            text.pop();
        }
        text.push_str(rest);
        Ok(text)
    }

    /// Why uninstall will not remove a managed plugin dir yet: `init.lua`
    /// may still load the module once the dir is gone, and Rex's next config
    /// load would then fail on a `require` of nothing. Any string outside a
    /// comment and outside clauth's blocks that names clauth counts, so
    /// every spelling of a load does: `require(m)` after `local m =
    /// "clauth"`, a `dofile` of a path into `clauth/`. A string naming it for
    /// another reason only costs a refusal.
    fn still_loaded(&self) -> Option<Refusal> {
        self.tokens.iter().find_map(|token| match &token.lexeme {
            Lexeme::Str(value) if names_clauth(value) => {
                let line = self.line_of(token.at);
                (!self.in_block(line)).then(|| self.refusal(line, Why::NamesClauth))
            }
            _ => None,
        })
    }
}

/// Whether a string's value names clauth, in any case: macOS's default file
/// system ignores case, so a `require("Clauth")` finds `clauth/` there.
fn names_clauth(value: &[u8]) -> bool {
    value
        .windows("clauth".len())
        .any(|window| window.eq_ignore_ascii_case(b"clauth"))
}

/// Writes the plugin and wires `init.lua`, after the confirm, then stops any
/// watcher running the old files. Returns false when the user declined. Split
/// from [`install`] so tests drive it against a temp dir without a Rex to
/// reload.
pub(crate) fn install_at(paths: &Paths, state_dir: &Path, yes: bool) -> Result<bool> {
    match plugin_at(&paths.plugin) {
        PluginDir::Linked { .. } | PluginDir::LinkedNotDir { .. } => bail!(
            "{} is a symlink, likely onto a checkout of rex-plugin/; \
             leave it to run that checkout, or remove the link and rerun",
            paths.plugin.display()
        ),
        PluginDir::Foreign => bail!(
            "{} exists and clauth did not write it; move it aside and rerun",
            paths.plugin.display()
        ),
        PluginDir::Absent | PluginDir::Managed { .. } => {}
    }
    let existing = read_optional(&paths.init)?;
    let wired = InitLua::read(&existing)
        .map_err(|refusal| anyhow!(refusal.message(&paths.init, "install")))?
        .with_block();
    let procs = crate::which::SystemProcs;
    let watchers = running_watchers(state_dir, &procs);

    outln!("clauth: install the Rex plugin:");
    outln!(
        "  write {} files to {}",
        FILES.len(),
        paths.plugin.display()
    );
    match &wired {
        Some(_) => outln!(
            "  append a require(\"clauth\") block to {}",
            paths.init.display()
        ),
        None => outln!(
            "  {} already loads it; leave it as is",
            paths.init.display()
        ),
    }
    if let Some(line) = stop_line(&watchers) {
        outln!("{line} on the old files; the config reload starts a fresh one");
    }
    outln!("  the plugin runs a pane-label watcher (`rex do`) for each Rex server");
    if !crate::herdr::confirm("go ahead?", yes)? {
        return Ok(false);
    }

    std::fs::create_dir_all(&paths.plugin)
        .with_context(|| format!("create {}", paths.plugin.display()))?;
    // The marker goes first: an install cut short after it leaves a dir a
    // rerun recognizes as clauth's and finishes, where one cut short before
    // it would leave a dir every rerun refuses as not written by clauth.
    let marker = format!("{MARKER_PREFIX}{}\n", env!("CARGO_PKG_VERSION"));
    crate::profile::atomic_write(&paths.plugin.join(MARKER), marker)
        .with_context(|| format!("write {}", paths.plugin.join(MARKER).display()))?;
    for (name, contents, executable) in FILES {
        let path = paths.plugin.join(name);
        crate::profile::atomic_write(&path, contents)
            .with_context(|| format!("write {}", path.display()))?;
        if *executable {
            make_executable(&path)?;
        }
    }
    if let Some(text) = wired {
        write_init(&paths.init, &text)?;
    }
    stop_watchers(&watchers, &procs, WATCHER_STOP_TIMEOUT);
    Ok(true)
}

/// Writes Rex's `init.lua` in place, the way `clauth herdr install` writes
/// herdr's `config.toml`. A temp-and-rename would replace a symlinked
/// `init.lua` (a dotfiles checkout) with a plain file and reset its mode, so
/// the checkout would never see the edit. The cost is the one herdr's write
/// takes: a crash or a full disk mid-write can leave the file short.
fn write_init(path: &Path, text: &str) -> Result<()> {
    std::fs::write(path, text).with_context(|| format!("write {}", path.display()))
}

/// Takes the block out of `init.lua`, removes a managed plugin directory, and
/// stops the watcher and removes its state directory, which would otherwise
/// keep labelling panes with no resolver beside it. A linked or foreign
/// plugin directory keeps its watcher and state: that checkout runs them.
/// Refuses, before it changes anything, an `init.lua` it cannot read (see
/// [`InitLua::read`]), and to remove a managed directory that `init.lua` would
/// still load. Returns false when the user declined or there was nothing to
/// remove.
pub(crate) fn uninstall_at(paths: &Paths, state_dir: &Path, yes: bool) -> Result<bool> {
    let existing = read_optional(&paths.init)?;
    let refuse = |refusal: Refusal| anyhow!(refusal.message(&paths.init, "uninstall"));
    let init = InitLua::read(&existing).map_err(refuse)?;
    let unwired = init.without_blocks().map_err(refuse)?;
    let had_block = !init.blocks.is_empty();
    let dir = plugin_at(&paths.plugin);
    let managed = matches!(dir, PluginDir::Managed { .. });
    if managed && let Some(refusal) = init.still_loaded() {
        return Err(refuse(refusal));
    }
    if !had_block && !managed {
        outln!("clauth: no Rex plugin from clauth to remove");
        if dir != PluginDir::Absent {
            outln!("  {} is not clauth's; left alone", paths.plugin.display());
        }
        return Ok(false);
    }

    let ours = managed || dir == PluginDir::Absent;
    let procs = crate::which::SystemProcs;
    let watchers = if ours {
        running_watchers(state_dir, &procs)
    } else {
        Vec::new()
    };
    let has_state = ours && state_dir.exists();

    outln!("clauth: uninstall the Rex plugin:");
    if had_block {
        outln!("  take clauth's block out of {}", paths.init.display());
    }
    match dir {
        PluginDir::Managed { .. } => {
            outln!("  remove {}", paths.plugin.display())
        }
        PluginDir::Linked { .. } | PluginDir::LinkedNotDir { .. } | PluginDir::Foreign => {
            outln!(
                "  leave {} alone: clauth did not write it",
                paths.plugin.display()
            );
        }
        PluginDir::Absent => {}
    }
    if let Some(line) = stop_line(&watchers) {
        outln!("{line}");
    }
    if has_state {
        outln!("  remove {}", state_dir.display());
    }
    if !crate::herdr::confirm("go ahead?", yes)? {
        return Ok(false);
    }

    // The config goes first: a Rex that reloads between the two steps then
    // finds no require, rather than a require of a module that is gone.
    if had_block {
        write_init(&paths.init, &unwired)?;
    }
    stop_watchers(&watchers, &procs, WATCHER_STOP_TIMEOUT);
    if managed {
        std::fs::remove_dir_all(&paths.plugin)
            .with_context(|| format!("remove {}", paths.plugin.display()))?;
    }
    if has_state {
        std::fs::remove_dir_all(state_dir)
            .with_context(|| format!("remove {}", state_dir.display()))?;
    }
    Ok(true)
}

/// What `clauth rex install|uninstall` read off the environment: Rex's
/// config, the watcher state, and the Rex CLI that reloads the config. A test
/// builds one over a temp dir and a stand-in `rex`.
pub(crate) struct Setup {
    pub(crate) paths: Paths,
    pub(crate) state: PathBuf,
    pub(crate) rex: PathBuf,
}

impl Setup {
    fn from_env() -> Result<Self> {
        let home = crate::profile::home_dir()?;
        Ok(Self {
            paths: Paths::resolve(
                std::env::var_os("REX_CONFIG"),
                std::env::var_os("XDG_CONFIG_HOME"),
                &home,
            ),
            state: state_dir(std::env::var_os("XDG_STATE_HOME"), &home),
            rex: rex_cli(std::env::var_os("REX_BIN_DIR")),
        })
    }
}

/// The Rex CLI to reload with. Rex sets `REX_BIN_DIR` in each of its panes to
/// the directory holding its server's own `rex`, so a reload from a pane
/// reaches that pane's server even when another Rex version's CLI comes first
/// on `PATH`. Anywhere else, the `rex` on `PATH`.
pub(crate) fn rex_cli(bin_dir: Option<OsString>) -> PathBuf {
    bin_dir
        .filter(|dir| !dir.is_empty())
        .map(|dir| PathBuf::from(dir).join("rex"))
        .filter(|path| path.is_file())
        .unwrap_or_else(|| PathBuf::from("rex"))
}

pub(crate) fn install(yes: bool) -> Result<()> {
    install_with(&Setup::from_env()?, yes)
}

pub(crate) fn uninstall(yes: bool) -> Result<()> {
    uninstall_with(&Setup::from_env()?, yes)
}

pub(crate) fn install_with(setup: &Setup, yes: bool) -> Result<()> {
    if cfg!(windows) {
        bail!("the Rex plugin runs POSIX sh scripts; it is not available on Windows");
    }
    if install_at(&setup.paths, &setup.state, yes)? {
        outln!(
            "clauth: installed the Rex plugin in {}",
            setup.paths.plugin.display()
        );
        report_reload(&reload(&setup.rex, RELOAD_TIMEOUT));
    }
    Ok(())
}

pub(crate) fn uninstall_with(setup: &Setup, yes: bool) -> Result<()> {
    if uninstall_at(&setup.paths, &setup.state, yes)? {
        outln!("clauth: removed the Rex plugin");
        report_reload(&reload(&setup.rex, RELOAD_TIMEOUT));
    }
    Ok(())
}

/// How long `rex config reload` gets. The Rex CLI bounds each request at 10 s
/// by default, so this only cuts off a CLI wedged past its own limit, and
/// lets the CLI's own error win otherwise.
const RELOAD_TIMEOUT: Duration = Duration::from_secs(15);

/// How `rex config reload` went.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Reload {
    Done,
    /// It ran and failed: its stderr.
    Failed(String),
    /// No Rex CLI to run.
    NoCli,
    /// It was still running at the deadline, and was killed.
    TimedOut,
}

/// Asks the Rex server to read its config again, killing the CLI at
/// `timeout`. Best effort: outside a Rex terminal on macOS the CLI is usually
/// not on PATH (it ships inside the app bundle), and the next server start
/// reads the config anyway.
pub(crate) fn reload(bin: &Path, timeout: Duration) -> Reload {
    let child = Command::new(bin)
        .args(["config", "reload"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn();
    let Ok(child) = child else {
        return Reload::NoCli;
    };
    match crate::herdr::run_bounded(child, timeout) {
        Some(out) if out.status.success() => Reload::Done,
        Some(out) => Reload::Failed(String::from_utf8_lossy(&out.stderr).trim().to_string()),
        None => Reload::TimedOut,
    }
}

fn report_reload(reload: &Reload) {
    match reload {
        Reload::Done => outln!("clauth: Rex reloaded its config"),
        Reload::Failed(stderr) => {
            errln!("clauth: `rex config reload` failed: {stderr}; run it in a Rex terminal")
        }
        Reload::NoCli => {
            outln!("clauth: run `rex config reload` in a Rex terminal to load it now")
        }
        Reload::TimedOut => errln!(
            "clauth: `rex config reload` did not finish in {} s; run it in a Rex terminal",
            RELOAD_TIMEOUT.as_secs()
        ),
    }
}

fn read_optional(path: &Path) -> Result<String> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(e).with_context(|| format!("read {}", path.display())),
    }
}

#[cfg(unix)]
fn make_executable(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
        .with_context(|| format!("chmod {}", path.display()))
}

#[cfg(not(unix))]
fn make_executable(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
#[path = "../tests/inline/rex.rs"]
mod tests;
