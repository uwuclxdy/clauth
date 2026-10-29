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

    fn from_env() -> Result<Self> {
        Ok(Self::resolve(
            std::env::var_os("REX_CONFIG"),
            std::env::var_os("XDG_CONFIG_HOME"),
            &crate::profile::home_dir()?,
        ))
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
    entries.flatten().any(|entry| {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            return false;
        };
        if !name.starts_with("watch-") || !name.ends_with(".pid") {
            return false;
        }
        let Ok(Some(bytes)) = crate::runtime::read_regular_capped(&entry.path(), PIDFILE_CAP)
        else {
            return false;
        };
        let Some(pid) = std::str::from_utf8(&bytes)
            .ok()
            .and_then(|body| body.trim().parse::<u32>().ok())
        else {
            return false;
        };
        procs
            .argv(pid)
            .is_some_and(|argv| argv.iter().any(|arg| arg.contains(WATCHER_SCRIPT)))
    })
}

/// The block install appends. Its first and last lines are what uninstall
/// matches, so they never change spelling between releases.
pub(crate) fn block() -> String {
    format!("{BLOCK_BEGIN}\nrequire(\"clauth\").setup{{}}\n{BLOCK_END}\n")
}

/// Whether `init.lua` already loads the module outside clauth's block, as a
/// hand-written `require("clauth").setup{ ... }` with the user's options does.
/// Install then leaves the file alone rather than loading it twice.
pub(crate) fn loads_plugin(existing: &str) -> bool {
    without_block(existing).0.lines().any(|line| {
        let code = line.split("--").next().unwrap_or_default();
        [
            "require(\"clauth\")",
            "require('clauth')",
            "require \"clauth\"",
            "require 'clauth'",
        ]
        .iter()
        .any(|form| code.contains(form))
    })
}

/// `existing` with clauth's block appended, or `None` when it is already
/// there or the user loads the module themselves.
pub(crate) fn with_block(existing: &str) -> Option<String> {
    if existing.lines().any(|line| line == BLOCK_BEGIN) || loads_plugin(existing) {
        return None;
    }
    let mut text = existing.to_string();
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str(&block());
    Some(text)
}

/// `existing` without clauth's block, and whether one was there. Only the
/// lines from the begin marker through the end marker go; an unterminated
/// block (a hand edit cut the end line) is left whole rather than guessed at.
pub(crate) fn without_block(existing: &str) -> (String, bool) {
    let lines: Vec<&str> = existing.split_inclusive('\n').collect();
    let Some(begin) = lines.iter().position(|l| l.trim_end() == BLOCK_BEGIN) else {
        return (existing.to_string(), false);
    };
    let Some(len) = lines[begin..]
        .iter()
        .position(|l| l.trim_end() == BLOCK_END)
    else {
        return (existing.to_string(), false);
    };
    let mut text: String = lines[..begin].concat();
    text.push_str(&lines[begin + len + 1..].concat());
    (text, true)
}

/// Writes the plugin and wires `init.lua`, after the confirm. Returns false
/// when the user declined. Split from [`install`] so tests drive it against
/// a temp dir without a Rex to reload.
pub(crate) fn install_at(paths: &Paths, yes: bool) -> Result<bool> {
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
    outln!("  the plugin runs a pane-label watcher (`rex do`) for each Rex server");
    if !crate::herdr::confirm("go ahead?", yes)? {
        return Ok(false);
    }

    std::fs::create_dir_all(&paths.plugin)
        .with_context(|| format!("create {}", paths.plugin.display()))?;
    for (name, contents, executable) in FILES {
        let path = paths.plugin.join(name);
        crate::profile::atomic_write(&path, contents)
            .with_context(|| format!("write {}", path.display()))?;
        if *executable {
            make_executable(&path)?;
        }
    }
    let marker = format!("{MARKER_PREFIX}{}\n", env!("CARGO_PKG_VERSION"));
    crate::profile::atomic_write(&paths.plugin.join(MARKER), marker)
        .with_context(|| format!("write {}", paths.plugin.join(MARKER).display()))?;
    if let Some(text) = wired {
        crate::profile::atomic_write(&paths.init, text)
            .with_context(|| format!("write {}", paths.init.display()))?;
    }
    Ok(true)
}

/// Takes the block out of `init.lua` and removes a managed plugin directory.
/// Returns false when the user declined or there was nothing to remove.
pub(crate) fn uninstall_at(paths: &Paths, yes: bool) -> Result<bool> {
    let existing = read_optional(&paths.init)?;
    let (unwired, had_block) = without_block(&existing);
    let dir = plugin_at(&paths.plugin);
    let managed = matches!(dir, PluginDir::Managed { .. });
    if !had_block && !managed {
        outln!("clauth: no Rex plugin from clauth to remove");
        if dir != PluginDir::Absent {
            outln!("  {} is not clauth's; left alone", paths.plugin.display());
        }
        return Ok(false);
    }

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
    if !crate::herdr::confirm("go ahead?", yes)? {
        return Ok(false);
    }

    // The config goes first: a Rex that reloads between the two steps then
    // finds no require, rather than a require of a module that is gone.
    if had_block {
        crate::profile::atomic_write(&paths.init, unwired)
            .with_context(|| format!("write {}", paths.init.display()))?;
    }
    if managed {
        std::fs::remove_dir_all(&paths.plugin)
            .with_context(|| format!("remove {}", paths.plugin.display()))?;
    }
    Ok(true)
}

pub(crate) fn install(yes: bool) -> Result<()> {
    if cfg!(windows) {
        bail!("the Rex plugin runs POSIX sh scripts; it is not available on Windows");
    }
    let paths = Paths::from_env()?;
    if install_at(&paths, yes)? {
        outln!(
            "clauth: installed the Rex plugin in {}",
            paths.plugin.display()
        );
        reload();
    }
    Ok(())
}

pub(crate) fn uninstall(yes: bool) -> Result<()> {
    let paths = Paths::from_env()?;
    if uninstall_at(&paths, yes)? {
        outln!("clauth: removed the Rex plugin");
        reload();
    }
    Ok(())
}

/// Asks the local Rex server to read its config again. Best effort: outside a
/// Rex terminal on macOS the CLI is usually not on PATH (it ships inside the
/// app bundle), and the next server start reads the config anyway.
fn reload() {
    let bin = std::env::var_os("REX_BIN_DIR")
        .map(|dir| PathBuf::from(dir).join("rex"))
        .filter(|path| path.is_file())
        .unwrap_or_else(|| PathBuf::from("rex"));
    match Command::new(&bin).args(["config", "reload"]).output() {
        Ok(out) if out.status.success() => {
            outln!("clauth: Rex reloaded its config")
        }
        Ok(out) => errln!(
            "clauth: `rex config reload` failed: {}; run it in a Rex terminal",
            String::from_utf8_lossy(&out.stderr).trim()
        ),
        Err(_) => outln!("clauth: run `rex config reload` in a Rex terminal to load it now"),
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
