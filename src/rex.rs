//! `clauth rex install|uninstall`: the Rex plugin under `rex-plugin/`.
//!
//! Rex has no plugin manager. A plugin is a Lua module beside Rex's own
//! `init.lua`, loaded with `require`. So install writes the bundled files to
//! `<rex config dir>/clauth/` and appends one marked `require` block to
//! `init.lua`; uninstall takes both back out. The files are compiled into the
//! binary, so the plugin a user runs always matches the clauth that wrote it,
//! and rerunning install after an upgrade refreshes it.

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

/// Written into the plugin directory so a later install or uninstall can tell
/// a tree clauth owns from one the user put there (a checkout, their own fork).
const MARKER: &str = ".clauth-managed";

const BLOCK_BEGIN: &str =
    "-- clauth rex plugin: written by `clauth rex install`, removed by `clauth rex uninstall`";
const BLOCK_END: &str = "-- end clauth rex plugin";

/// Where Rex reads its config and where the plugin goes beside it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Paths {
    pub(crate) init: PathBuf,
    pub(crate) plugin: PathBuf,
}

impl Paths {
    /// Rex's own rule: `$REX_CONFIG` when set, else `rex/init.lua` under
    /// `$XDG_CONFIG_HOME`, else under `~/.config` (macOS and Linux alike).
    /// Pure, so the rule is tested without touching the environment.
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

/// What the plugin directory holds now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PluginDir {
    Absent,
    /// A tree an earlier `clauth rex install` wrote.
    Managed,
    /// A symlink, such as one onto a checkout of `rex-plugin/`.
    Linked,
    /// A directory clauth did not write.
    Foreign,
}

pub(crate) fn plugin_dir_state(dir: &Path) -> PluginDir {
    match std::fs::symlink_metadata(dir) {
        Err(_) => PluginDir::Absent,
        Ok(meta) if meta.file_type().is_symlink() => PluginDir::Linked,
        Ok(_) if dir.join(MARKER).is_file() => PluginDir::Managed,
        Ok(_) => PluginDir::Foreign,
    }
}

/// Writes the plugin and wires `init.lua`, after the confirm. Returns false
/// when the user declined. Split from [`install`] so tests drive it against
/// a temp dir without a Rex to reload.
pub(crate) fn install_at(paths: &Paths, yes: bool) -> Result<bool> {
    match plugin_dir_state(&paths.plugin) {
        PluginDir::Linked => bail!(
            "{} is a symlink, likely onto a checkout of rex-plugin/; \
             leave it to run that checkout, or remove the link and rerun",
            paths.plugin.display()
        ),
        PluginDir::Foreign => bail!(
            "{} exists and clauth did not write it; move it aside and rerun",
            paths.plugin.display()
        ),
        PluginDir::Absent | PluginDir::Managed => {}
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
    let marker = format!("written by clauth {}\n", env!("CARGO_PKG_VERSION"));
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
    let dir = plugin_dir_state(&paths.plugin);
    if !had_block && dir != PluginDir::Managed {
        outln!("clauth: no Rex plugin from clauth to remove");
        match dir {
            PluginDir::Linked | PluginDir::Foreign => {
                outln!("  {} is not clauth's; left alone", paths.plugin.display());
            }
            PluginDir::Absent | PluginDir::Managed => {}
        }
        return Ok(false);
    }

    outln!("clauth: uninstall the Rex plugin:");
    if had_block {
        outln!("  take clauth's block out of {}", paths.init.display());
    }
    match dir {
        PluginDir::Managed => {
            outln!("  remove {}", paths.plugin.display())
        }
        PluginDir::Linked | PluginDir::Foreign => {
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
    if dir == PluginDir::Managed {
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
