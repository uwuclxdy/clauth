//! Rex (experimental): the read-only state the Services tab's `rex` row shows.
//! Reads only; nothing here writes Rex's config, the plugin dir or the
//! watcher's state.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// The plugin dir's marker, and the first line a clauth-written one carries.
const MARKER: &str = ".clauth-managed";
const MARKER_PREFIX: &str = "written by clauth ";

/// The marker and a pidfile are each a few dozen bytes; a FIFO or a runaway
/// file must neither block the read nor balloon it.
const MARKER_CAP: u64 = 4096;
const PIDFILE_CAP: u64 = 64;

/// The script whose argv marks a live pane-label watcher.
const WATCHER_SCRIPT: &str = "tag-watch.lua";

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

#[cfg(test)]
#[path = "../tests/inline/rex.rs"]
mod tests;
