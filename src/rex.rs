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

use anyhow::{Context, Result, bail};

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

/// The block install appends. Its first and last lines are what uninstall
/// matches, so they never change spelling between releases.
pub(crate) fn block() -> String {
    format!("{BLOCK_BEGIN}\nrequire(\"clauth\").setup{{}}\n{BLOCK_END}\n")
}

/// Where clauth's block sits in `init.lua`, by 0-based line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BlockAt {
    Absent,
    Whole {
        begin: usize,
        end: usize,
    },
    /// A begin marker with no end marker after it: a hand edit cut the end.
    Unterminated {
        begin: usize,
    },
}

/// Finds the block. Install and uninstall both read the markers through
/// here, so a line one of them takes for a marker the other does too (a
/// trailing space or a CRLF included).
fn block_at(existing: &str) -> BlockAt {
    let mut lines = existing.lines().map(str::trim_end).enumerate();
    let Some(begin) = lines
        .by_ref()
        .find_map(|(n, line)| (line == BLOCK_BEGIN).then_some(n))
    else {
        return BlockAt::Absent;
    };
    match lines.find_map(|(n, line)| (line == BLOCK_END).then_some(n)) {
        Some(end) => BlockAt::Whole { begin, end },
        None => BlockAt::Unterminated { begin },
    }
}

/// The first line outside clauth's block that loads the module, as a
/// hand-written `require("clauth").setup{ ... }` with the user's options
/// does: its 1-based number and its text. Comments load nothing, so a
/// `-- require("clauth")` or one inside `--[[ ]]` does not count.
fn loading_line(existing: &str) -> Option<(usize, &str)> {
    let ours = match block_at(existing) {
        BlockAt::Whole { begin, end } => Some(begin..=end),
        BlockAt::Absent | BlockAt::Unterminated { .. } => None,
    };
    let code = lua_code(existing);
    code.split('\n')
        .zip(existing.split('\n'))
        .enumerate()
        .find(|(n, (code, _))| {
            !ours.as_ref().is_some_and(|block| block.contains(n)) && requires_clauth(code)
        })
        .map(|(n, (_, line))| (n + 1, line.trim_end()))
}

/// Whether `init.lua` already loads the module outside clauth's block.
/// Install then leaves the file alone rather than loading it twice.
pub(crate) fn loads_plugin(existing: &str) -> bool {
    loading_line(existing).is_some()
}

/// Whether one line of Lua code (comments already blanked) calls
/// `require("clauth")`, in any of the call's spellings: `require "clauth"`,
/// `require'clauth'`, `require [[clauth]]`.
fn requires_clauth(code: &str) -> bool {
    code.match_indices("require").any(|(at, word)| {
        let ident = |c: char| c.is_alphanumeric() || c == '_';
        if code[..at].chars().next_back().is_some_and(ident) {
            return false;
        }
        let mut rest = code[at + word.len()..].trim_start();
        if let Some(inner) = rest.strip_prefix('(') {
            rest = inner.trim_start();
        }
        ["\"clauth\"", "'clauth'", "[[clauth]]"]
            .iter()
            .any(|arg| rest.starts_with(arg))
    })
}

/// `text` with its Lua comments blanked out and every line break kept, so
/// line `n` of the result is line `n` of `text`. Strings stay, since one
/// names the module in `require("clauth")`, and a `--` inside one starts no
/// comment.
fn lua_code(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(c) = rest.chars().next() {
        let (keep, len) = if let Some(body) = rest.strip_prefix("--") {
            // A comment runs to its long bracket's close (`--[[ ]]`,
            // `--[==[ ]==]`), else to the end of the line.
            let len = match long_open(body) {
                Some((open, level)) => open + long_close_end(&body[open..], level),
                None => body.find('\n').unwrap_or(body.len()),
            };
            (false, 2 + len)
        } else if let Some((open, level)) = long_open(rest) {
            (true, open + long_close_end(&rest[open..], level))
        } else if c == '"' || c == '\'' {
            (true, short_string_len(rest))
        } else {
            (true, c.len_utf8())
        };
        let (taken, tail) = rest.split_at(len);
        if keep {
            out.push_str(taken);
        } else {
            out.extend(taken.chars().filter(|&c| c == '\n'));
        }
        rest = tail;
    }
    out
}

/// The long bracket opening `text` (`[[`, `[=[`, ...): its length and level.
fn long_open(text: &str) -> Option<(usize, usize)> {
    let body = text.strip_prefix('[')?;
    let level = body.len() - body.trim_start_matches('=').len();
    body[level..].starts_with('[').then_some((level + 2, level))
}

/// How far into `text` a long bracket of `level` closes, past its `]=*]`,
/// or all of `text` when it never does.
fn long_close_end(text: &str, level: usize) -> usize {
    let close = format!("]{}]", "=".repeat(level));
    text.find(&close).map_or(text.len(), |at| at + close.len())
}

/// The length of the quoted string opening `text`, through its closing
/// quote, or to the end of the line when it never closes. A backslash
/// escapes the byte after it.
fn short_string_len(text: &str) -> usize {
    let bytes = text.as_bytes();
    let quote = bytes[0];
    let mut i = 1;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 2,
            b'\n' => return i,
            b if b == quote => return i + 1,
            _ => i += 1,
        }
    }
    // Every stop above is an ASCII byte, so a char boundary; this one is
    // the end of `text`.
    text.len()
}

/// `existing` with clauth's block appended, or `None` when a block is
/// already there (whole or cut short) or the user loads the module
/// themselves.
pub(crate) fn with_block(existing: &str) -> Option<String> {
    if block_at(existing) != BlockAt::Absent || loads_plugin(existing) {
        return None;
    }
    let mut text = existing.to_string();
    let mut block = block();
    if !text.is_empty() && !text.ends_with('\n') {
        // The block needs a line of its own. It then ends without a final
        // newline of its own, which tells uninstall to take this one back.
        text.push('\n');
        block.pop();
    }
    text.push_str(&block);
    Some(text)
}

/// `existing` without clauth's block, and whether one was there. Only the
/// lines from the begin marker through the end marker go; an unterminated
/// block (a hand edit cut the end line) is left whole rather than guessed at.
pub(crate) fn without_block(existing: &str) -> (String, bool) {
    let BlockAt::Whole { begin, end } = block_at(existing) else {
        return (existing.to_string(), false);
    };
    let lines: Vec<&str> = existing.split_inclusive('\n').collect();
    let mut text: String = lines[..begin].concat();
    let after = lines[end + 1..].concat();
    if after.is_empty() && !lines[end].ends_with('\n') && text.ends_with('\n') {
        // The block ends the file with no final newline: install put it on
        // a line of its own after a file that had none (see `with_block`).
        text.pop();
    }
    text.push_str(&after);
    (text, true)
}

/// Why uninstall will not remove a managed plugin dir yet, when `init.lua`
/// would still load the module once the dir is gone: Rex's next config load
/// would then fail on a `require` of nothing.
fn still_loaded(init: &Path, existing: &str) -> Option<String> {
    if let BlockAt::Unterminated { begin } = block_at(existing) {
        return Some(format!(
            "{} has clauth's block starting at line {} but no `{BLOCK_END}` line after it; \
             take that block out by hand, then rerun",
            init.display(),
            begin + 1
        ));
    }
    let (line, text) = loading_line(existing)?;
    Some(format!(
        "{} still loads the plugin outside clauth's block, at line {line}:\n    {text}\n\
         take that out first, then rerun; removing the plugin would leave it requiring a \
         module that is gone",
        init.display()
    ))
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
    let wired = with_block(&existing);
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
/// Refuses, before it changes anything, to remove a managed directory that
/// `init.lua` would still load. Returns false when the user declined or there
/// was nothing to remove.
pub(crate) fn uninstall_at(paths: &Paths, state_dir: &Path, yes: bool) -> Result<bool> {
    let existing = read_optional(&paths.init)?;
    let (unwired, had_block) = without_block(&existing);
    let dir = plugin_at(&paths.plugin);
    let managed = matches!(dir, PluginDir::Managed { .. });
    if managed && let Some(reason) = still_loaded(&paths.init, &existing) {
        bail!("{reason}");
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
