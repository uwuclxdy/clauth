//! `clauth herdr install`: the one command that sets the herdr plugin up.
//!
//! herdr owns plugin installation and prints its own preview of every command a
//! plugin would run before registering it, so this shells out with stdio
//! inherited rather than reimplementing that gate or silencing it. The half
//! herdr gives a plugin no way to declare is the reason a setup command exists
//! at all: a keybinding and a sidebar row template both live in the user's own
//! `config.toml`, so both were manual paste steps.
//!
//! Everything written into that config is validated by `herdr config check`
//! against a temporary copy first, which is what catches a `--key` herdr would
//! otherwise disable on load. The real write lands in place rather than through
//! a rename, so the file keeps the mode and inode herdr's config already has.

use std::ffi::OsStr;
use std::io::{IsTerminal as _, Read as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde_json::Value;

use crate::out::{errln, out, outln};

/// The manifest `id`, and the prefix of every qualified action id.
const PLUGIN_ID: &str = "clauth";
/// The action a keybinding points at: opens the dashboard popup.
const OPEN_ACTION: &str = "clauth.open";
/// `owner/repo/subdir`, the only source shape `herdr plugin install` accepts.
const GITHUB_SOURCE: &str = "uwuclxdy/clauth/herdr-plugin";
/// Offered when `--key` is absent. `prefix+` is herdr's own leader.
pub(crate) const DEFAULT_KEY: &str = "prefix+a";
/// The pane-metadata name `report-profile.sh` publishes the account under.
const TOKEN: &str = "$clauth";
/// The pane-metadata name the MCP server publishes delegate state under. The
/// sidebar row `install` writes is the only template that renders it, so the
/// row names it only while the `delegate_row_text` knob is on.
const DELEGATE_TOKEN: &str = "$clauth_delegate";
/// Marks this crate's additions inside a file clauth does not own.
const MARKER: &str = "# clauth herdr plugin";

/// Where the plugin comes from. A checkout gets linked in place so an edit is
/// live on the next open; anyone else fetches the published subdir.
enum Source {
    Link(PathBuf),
    Github,
}

pub(crate) fn install(
    key: Option<&str>,
    no_config: bool,
    yes: bool,
    delegate_row_text: bool,
) -> Result<()> {
    // `herdr-plugin/herdr-plugin.toml` declares linux and macos, because its
    // entrypoints are POSIX shell. herdr links a plugin whose platforms exclude
    // the host and refuses each entrypoint at invocation instead, so without
    // this the command lands a plugin that answers `platform_unsupported` to
    // every key the same command just bound.
    if cfg!(windows) {
        bail!("the herdr plugin is linux and macos only: its entrypoints are POSIX shell scripts");
    }

    let bin = herdr_bin();

    match plugin_source() {
        Source::Link(path) => {
            outln!("clauth: linking {} into herdr", path.display());
            // Stamp before linking: herdr derives the linked plugin's version
            // from this manifest, so it must match the running binary's version.
            let manifest = path.join("herdr-plugin.toml");
            stamp_manifest_version(&manifest)
                .with_context(|| format!("failed to stamp {}", manifest.display()))?;
            let path = path.to_string_lossy().into_owned();
            // `plugin link` answers with the whole parsed manifest as one JSON
            // line and asks nothing, so it is swallowed unless it fails.
            run_quiet(&bin, &["plugin", "link", &path])?;
        }
        Source::Github => {
            outln!("clauth: installing {GITHUB_SOURCE} into herdr");
            let mut args = vec!["plugin", "install", GITHUB_SOURCE];
            // herdr's preview is the user's chance to read what a plugin will
            // run as them. Only skip it when this command was already answered.
            if yes {
                args.push("--yes");
            }
            run(&bin, &args)?;
        }
    }

    // Ahead of the --no-config branch: that path prints a block to paste, and
    // a key that breaks the file breaks it just as thoroughly by hand.
    let key = resolve_key(key, yes)?;

    if no_config {
        outln!("clauth: herdr's config left alone (--no-config)");
        print_manual(&key, delegate_row_text);
        return Ok(());
    }

    let path = config_path(&bin)?;
    let existing = read_config(&path)?;
    let (text, plan, removed, noop) = install_resync(&existing, &key, delegate_row_text)?;

    for note in &plan.notes {
        outln!("clauth: {note}");
    }

    if noop {
        outln!("clauth: herdr's config already carries everything clauth would add");
        return Ok(());
    }

    outln!("");
    outln!("{}:", path.display());
    for line in plan.append.trim_start_matches('\n').lines() {
        outln!("+ {line}");
    }
    // A resync also DROPS the blocks it stripped — a knob toggle replaces the
    // old row, and a hand-owned binding re-adds nothing. Show the removal
    // half whenever something was stripped, the way `uninstall` does, so the
    // diff the user confirms shows both sides.
    let mut diff = removed.clone();
    if diff.first().is_some_and(String::is_empty) {
        diff.remove(0);
    }
    for line in &diff {
        outln!("- {line}");
    }
    outln!("");

    if !confirm("write these to herdr's config?", yes)? {
        outln!("clauth: nothing written");
        print_manual(&key, delegate_row_text);
        return Ok(());
    }

    write_validated(&path, &existing, &text, &bin)?;

    outln!("clauth: wrote {}", path.display());
    outln!("clauth: press {key} in herdr to open the dashboard");
    Ok(())
}

/// The running herdr when clauth was launched from one of its panes, else
/// whatever is on `PATH`. Inside a pane the injected path names the binary that
/// owns the session being configured, which a bare name can miss.
pub(crate) fn herdr_bin() -> String {
    std::env::var("HERDR_BIN_PATH").unwrap_or_else(|_| "herdr".to_string())
}

/// A checkout is recognized by the manifest rather than by a repo name, so a
/// fork or a rename still links.
fn plugin_source() -> Source {
    let dir = std::env::current_dir()
        .unwrap_or_default()
        .join("herdr-plugin");
    if dir.join("herdr-plugin.toml").is_file() {
        return Source::Link(dir);
    }
    Source::Github
}

fn run(bin: &str, args: &[&str]) -> Result<()> {
    let status = Command::new(bin).args(args).status().with_context(|| {
        format!(
            "could not run `{bin} {}`; is herdr installed and on PATH?",
            args.join(" ")
        )
    })?;
    if !status.success() {
        bail!("`{bin} {}` failed", args.join(" "));
    }
    Ok(())
}

/// Same, for a command that neither prompts nor prints anything a user wants.
/// Its output still reaches them when it fails, which is the only time it says
/// something they can act on.
fn run_quiet(bin: &str, args: &[&str]) -> Result<()> {
    let out = Command::new(bin).args(args).output().with_context(|| {
        format!(
            "could not run `{bin} {}`; is herdr installed and on PATH?",
            args.join(" ")
        )
    })?;
    quiet_outcome(bin, args, &out)
}

/// Same, bounded: a stalled fetch (git has no default timeout) must not wedge
/// the detached heal's in-flight claim forever. The next attempt retries.
fn run_quiet_bounded(bin: &str, args: &[&str], timeout: Duration) -> Result<()> {
    let Some(out) = bounded_output_for(bin, args, &[], timeout) else {
        bail!(
            "`{bin} {}` timed out after {}s",
            args.join(" "),
            timeout.as_secs()
        );
    };
    quiet_outcome(bin, args, &out)
}

/// The shared failure read for the quiet runners: a nonzero exit becomes one
/// error carrying whatever the child printed, stdout first.
fn quiet_outcome(bin: &str, args: &[&str], out: &Output) -> Result<()> {
    if out.status.success() {
        return Ok(());
    }
    let mut why = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let err = String::from_utf8_lossy(&out.stderr);
    if !err.trim().is_empty() {
        why.push('\n');
        why.push_str(err.trim());
    }
    bail!("`{bin} {}` failed:\n{why}", args.join(" "));
}

/// herdr resolves its own config root per OS and exposes no command that prints
/// it, so this derives the root from the one path command it does have: a
/// plugin config dir is always `<root>/plugins/config/<component>`.
pub(crate) fn config_path(bin: &str) -> Result<PathBuf> {
    if let Ok(explicit) = std::env::var("HERDR_CONFIG_PATH")
        && !explicit.is_empty()
    {
        return Ok(PathBuf::from(explicit));
    }

    let out = bounded_output(bin, &["plugin", "config-dir", PLUGIN_ID], &[])
        .with_context(|| format!("`{bin} plugin config-dir` timed out or could not run"))?;
    let printed = String::from_utf8_lossy(&out.stdout);
    // Guessing a second location is worse than failing here: a guess that
    // misses writes a config file herdr never reads, and the user is told they
    // are set up. `dirs::config_dir()` is exactly that guess on macOS.
    config_path_from_plugin_dir(printed.trim()).with_context(|| {
        format!(
            "could not work out where herdr keeps its config (`{bin} plugin config-dir` printed \
             {printed:?}); pass the file yourself with HERDR_CONFIG_PATH"
        )
    })
}

/// herdr prints `<root>/plugins/config/<component>`, and `<root>` is where its
/// `config.toml` lives. Three components off the end, and the result has to
/// still be a real prefix rather than the empty path a relative print leaves.
fn config_path_from_plugin_dir(printed: &str) -> Option<PathBuf> {
    if printed.is_empty() {
        return None;
    }
    let root = PathBuf::from(printed).ancestors().nth(3)?.to_path_buf();
    // The empty path comes off a relative print, and a root with no parent of
    // its own is the filesystem root: neither is a directory herdr keeps a
    // config in, and both would have this write somewhere nobody reads.
    if root.as_os_str().is_empty() || root.parent().is_none() {
        return None;
    }
    Some(root.join("config.toml"))
}

/// Reads herdr's config for the callers that edit it. A missing file is an absent config and reads as empty; any other failure is a real error, since writing an empty string back would destroy a config that merely failed to read.
pub(crate) fn read_config(path: &Path) -> Result<String> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(e).with_context(|| {
            format!(
                "cannot read herdr's config at {} (encoding or permissions); fix it before clauth edits it",
                path.display()
            )
        }),
    }
}

/// One clauth entry from `herdr plugin list --json`. Every field is optional: herdr's schema is read leniently, so a shape change degrades to "unknown" rather than an error, the same way the Plugin tab reads CC's registry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RegistryEntry {
    pub(crate) enabled: bool,
    pub(crate) version: Option<String>,
    pub(crate) min_herdr_version: Option<String>,
    pub(crate) plugin_root: Option<String>,
    pub(crate) source_kind: Option<String>,
    pub(crate) warnings: Vec<String>,
}

/// Everything the Plugin tab's herdr row needs that costs a subprocess.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HerdrProbe {
    /// The version token after `herdr ` in `herdr --version`.
    pub(crate) version: Option<String>,
    /// `None` when clauth is not in the registry.
    pub(crate) entry: Option<RegistryEntry>,
    pub(crate) config_path: Option<PathBuf>,
    /// Registry read failed (not "absent"); `None` when it was only absent.
    pub(crate) error: Option<String>,
}

/// Probes the installed herdr. `None` when herdr does not resolve, so the caller renders no row at all.
pub(crate) fn probe() -> Option<HerdrProbe> {
    let bin = resolved_bin()?;
    let bin = bin.to_string_lossy();

    let version = version_command(&bin);
    let (entry, error) = registry_probe(&bin);
    let config_path = config_path(&bin).ok();

    Some(HerdrProbe {
        version,
        entry,
        config_path,
        error,
    })
}

/// The herdr binary to drive: `HERDR_BIN_PATH` when it names an existing file,
/// else a `PATH`-resolved herdr. `None` when herdr is not installed. Shared by
/// the Plugin tab probe, the pane reporter, and the auto-update heal, so all
/// three resolve one name.
pub(crate) fn resolved_bin() -> Option<PathBuf> {
    let raw = herdr_bin();
    let candidate = Path::new(&raw);
    // Path-like (absolute or carrying a separator): must exist as a file, the
    // way the pane reporter reads a stale injected path.
    if candidate.components().count() > 1 {
        return candidate.is_file().then(|| candidate.to_path_buf());
    }
    // Bare name: first executable hit on PATH (exec bit on Unix, the usual
    // extensions on Windows).
    crate::plugin_probe::on_path(&raw)
}

/// Bounds one herdr subprocess on the probe path (construction in herdr mode,
/// `r` refreshes), on the validated-write path (`check_config`), and on the
/// TUI's knob push (`crate::tui::app::push_herdr_knob_change`): a hung herdr must
/// delay the caller, never hang the first paint or a heal behind an open
/// modal. Same kill-on-deadline shape as the pane reporter's `report`
/// (`herdr_report.rs`); `probe()` runs its three calls sequentially, so the
/// worst case is three times this bound. A child that floods its own pipe
/// before the deadline is killed with it, same as one that never exits.
/// `run_quiet` deliberately stays unbounded: its caller is `plugin link`, a
/// local registry write; the network-fetching `plugin install` runs through
/// `run` with inherited stdio, where the user watches any stall.
pub(crate) const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

pub(crate) fn bounded_output(bin: &str, args: &[&str], envs: &[(&str, &OsStr)]) -> Option<Output> {
    bounded_output_for(bin, args, envs, PROBE_TIMEOUT)
}

/// The same bounded spawn with an explicit deadline; the detached heal's
/// install is the one caller that needs a longer bound than [`PROBE_TIMEOUT`].
pub(crate) fn bounded_output_for(
    bin: &str,
    args: &[&str],
    envs: &[(&str, &OsStr)],
    timeout: Duration,
) -> Option<Output> {
    let mut cmd = Command::new(bin);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in envs {
        cmd.env(k, v);
    }
    let mut child = cmd.spawn().ok()?;
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
            // A failed waitpid leaves a zombie otherwise; reap like the
            // reporter does.
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    };
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    if let Some(mut pipe) = child.stdout.take() {
        let _ = pipe.read_to_end(&mut stdout);
    }
    if let Some(mut pipe) = child.stderr.take() {
        let _ = pipe.read_to_end(&mut stderr);
    }
    Some(Output {
        status,
        stdout,
        stderr,
    })
}

fn version_command(bin: &str) -> Option<String> {
    let out = bounded_output(bin, &["--version"], &[])?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    version_from(text.lines().next()?)
}

/// `herdr 0.8.0` -> `Some("0.8.0")`. Pure, so the test feeds the real line.
fn version_from(line: &str) -> Option<String> {
    line.trim()
        .strip_prefix("herdr ")?
        .split_whitespace()
        .next()
        .map(str::to_string)
}

fn registry_probe(bin: &str) -> (Option<RegistryEntry>, Option<String>) {
    let out = match bounded_output(bin, &["plugin", "list", "--json"], &[]) {
        Some(out) => out,
        None => {
            return (
                None,
                Some(format!(
                    "`{bin} plugin list --json` timed out or could not run"
                )),
            );
        }
    };
    if !out.status.success() {
        let why = String::from_utf8_lossy(&out.stderr);
        let why = why.trim();
        return (
            None,
            Some(if why.is_empty() {
                format!("`{bin} plugin list --json` failed")
            } else {
                format!("`{bin} plugin list --json` failed: {why}")
            }),
        );
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let root: Value = match serde_json::from_str(&text) {
        Ok(root) => root,
        Err(e) => {
            return (
                None,
                Some(format!("herdr's plugin list did not parse: {e}")),
            );
        }
    };
    (registry_entry_from_value(&root), None)
}

/// The pure half of the registry read, split out so tests feed it the real bytes with no subprocess.
#[cfg(test)]
pub(crate) fn registry_entry_from(json: &str) -> Option<RegistryEntry> {
    let root: Value = serde_json::from_str(json).ok()?;
    registry_entry_from_value(&root)
}

/// One entry wrapped in the envelope `herdr plugin list --json` prints around it.
#[cfg(test)]
pub(crate) fn plugin_list_json(entry: &str) -> String {
    format!(r#"{{"id":"cli:plugin","result":{{"plugins":[{entry}],"type":"plugin_list"}}}}"#)
}

// Real `herdr plugin list --json` entries, captured against 0.8.0 on 2026-08-13. They live here rather than in one test file because every consumer that reads a field off a `RegistryEntry` has to pin its reading against herdr's own spelling: a hand-built fixture agrees with whatever the reader guessed, which is how `source_kind` was first read as `link` when herdr emits `local`.
#[cfg(test)]
pub(crate) const LINKED: &str = r#"{"enabled":true,"manifest_path":"/home/uwuclxdy/repos/rs/clauth/herdr-plugin/herdr-plugin.toml","min_herdr_version":"0.8.0","name":"clauth","platforms":["linux","macos"],"plugin_id":"clauth","plugin_root":"/home/uwuclxdy/repos/rs/clauth/herdr-plugin","source":{"kind":"local"},"version":"0.1.0"}"#;
#[cfg(test)]
pub(crate) const GITHUB: &str = r#"{"enabled":true,"min_herdr_version":"0.8.0","name":"clauth","platforms":["linux","macos"],"plugin_id":"clauth","source":{"kind":"github","owner":"uwuclxdy","repo":"clauth","resolved_commit":"abc123","managed_path":"/home/u/.config/herdr/plugins/clauth","installed_unix_ms":1784231727746},"version":"0.1.0"}"#;
#[cfg(test)]
pub(crate) const DISABLED: &str = r#"{"enabled":false,"min_herdr_version":"0.8.0","name":"clauth","platforms":["linux","macos"],"plugin_id":"clauth","plugin_root":"/home/uwuclxdy/repos/rs/clauth/herdr-plugin","source":{"kind":"local"},"version":"0.1.0"}"#;
#[cfg(test)]
pub(crate) const STALE: &str = r#"{"enabled":true,"manifest_path":"/home/uwuclxdy/repos/rs/clauth/herdr-plugin/herdr-plugin.toml","min_herdr_version":"0.8.0","name":"clauth","platforms":["linux","macos"],"plugin_id":"clauth","plugin_root":"/gone/clauth/herdr-plugin","source":{"kind":"local"},"version":"0.1.0","warnings":["manifest unavailable: No such file or directory (os error 2)"]}"#;

fn registry_entry_from_value(root: &Value) -> Option<RegistryEntry> {
    let entry = root
        .get("result")?
        .get("plugins")?
        .as_array()?
        .iter()
        .find(|e| e.get("plugin_id").and_then(Value::as_str) == Some(PLUGIN_ID))?;

    let field = |key: &str| entry.get(key).and_then(Value::as_str).map(str::to_string);
    Some(RegistryEntry {
        // A listed plugin is enabled unless herdr says otherwise, so an absent `enabled` reads as enabled rather than disabled.
        enabled: entry
            .get("enabled")
            .and_then(Value::as_bool)
            .unwrap_or(true),
        version: field("version"),
        min_herdr_version: field("min_herdr_version"),
        plugin_root: field("plugin_root"),
        source_kind: entry
            .get("source")
            .and_then(|source| source.get("kind"))
            .and_then(Value::as_str)
            .map(str::to_string),
        warnings: entry
            .get("warnings")
            .and_then(Value::as_array)
            .map(|warnings| {
                warnings
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default(),
    })
}

/// The herdr heal's own attempt limiter; a second [`HealThrottle`] instance,
/// so a herdr heal never defers the claude one or the other way round.
static HEAL_THROTTLE: crate::plugin_host::HealThrottle = crate::plugin_host::HealThrottle::new();

/// The throttled detached heal for the herdr plugin. herdr has no `plugin
/// update`, so an install over an existing github source IS the update: herdr
/// replaces the managed checkout and re-registers. Callers: the daemon tick and
/// `clauth mcp` startup, mirroring the claude plugin's detached heal. Success
/// and failure both log through `logline!`, never stdout.
pub(crate) fn heal_detached() {
    // The plugin is linux and macos only (its entrypoints are POSIX shell), so
    // there is nothing to heal on Windows.
    if cfg!(windows) {
        return;
    }
    // This heal is a network update (herdr's install fetches from GitHub), so
    // the same opt-out that gates clauth's own binary update gates it, before
    // the throttle claim so a disabled box never even claims an attempt.
    if !crate::update::updates_enabled() {
        return;
    }
    let Some(claim) = HEAL_THROTTLE.claim(crate::usage::now_ms()) else {
        return;
    };
    // Fail closed in test builds: the worker probes and reinstalls through the
    // resolved binary, and herdr panes inject `HERDR_BIN_PATH` with the
    // operator's real herdr, so a test run inside a pane sees the assert
    // satisfied with no shim staged. Only the test fake sets `HERDR_SHIM_STATE`,
    // so it is the sentinel; a test that drives a caller of this heal arms the
    // throttle instead when the heal is not what the test is about.
    #[cfg(test)]
    assert!(
        std::env::var_os("HERDR_SHIM_STATE").is_some(),
        "heal_detached would probe the operator's real `herdr` and reinstall \
         their real plugin — stage a herdr shim beside a `HERDR_SHIM_STATE` pin, or call \
         `arm_heal_throttle_for_test` if the heal is not what the test is about"
    );
    #[cfg(test)]
    let done = crate::testutil::register_background_task();
    std::thread::spawn(move || {
        let _claim = claim;
        match plugin_heal_line() {
            Ok(Some(line)) => crate::logline::logline!("{line}"),
            Ok(None) => {}
            Err(e) => crate::logline::logline!("clauth: herdr plugin heal failed: {e:#}"),
        }
        // Last action, after every env-touching step: the claim's drop clears
        // one atomic and nothing else, so it is safe past the send.
        #[cfg(test)]
        let _ = done.send(());
    });
}

/// Whether the installed entry needs the update heal: a github-sourced, enabled
/// entry whose manifest version sits below this binary's. Never true for an
/// absent entry (never resurrect an uninstall), a disabled one (never
/// re-enable), a linked checkout (the developer's own live tree), or a version
/// that cannot be compared (a lenient read degrades to no-op).
pub(crate) fn plugin_update_needed(entry: &RegistryEntry) -> bool {
    if !entry.enabled {
        return false;
    }
    if entry.source_kind.as_deref() != Some("github") {
        return false;
    }
    entry
        .version
        .as_deref()
        .is_some_and(|installed| crate::update::is_newer(crate::update::CURRENT_VERSION, installed))
}

/// How long one detached reinstall may take: a stalled fetch must release the
/// heal's in-flight claim rather than wedge it for the process lifetime.
const HEAL_INSTALL_TIMEOUT: Duration = Duration::from_secs(5 * 60);

/// The gate + update behind [`heal_detached`], split out so a test can pin the
/// contract without a thread: `None` when there is nothing to do, `Some` when
/// the update landed, `Err` when the probe or the install failed.
pub(crate) fn plugin_heal_line() -> anyhow::Result<Option<String>> {
    plugin_heal_line_with(HEAL_INSTALL_TIMEOUT)
}

/// The deadline seam under [`plugin_heal_line`]: the shipped call uses
/// [`HEAL_INSTALL_TIMEOUT`], a test drives the same path with a short one so
/// the heal's use of the bound is pinned, not just the helper's. After a
/// reinstall it re-probes herdr and stamps the installed manifest when the
/// fresh entry still trails this binary's version.
pub(crate) fn plugin_heal_line_with(timeout: Duration) -> anyhow::Result<Option<String>> {
    let Some(bin) = resolved_bin() else {
        return Ok(None);
    };
    let bin = bin.to_string_lossy();
    let (entry, error) = registry_probe(&bin);
    if let Some(error) = error {
        bail!("{error}");
    }
    let Some(entry) = entry else {
        return Ok(None);
    };
    if !plugin_update_needed(&entry) {
        return Ok(None);
    }
    let old = entry.version.unwrap_or_default();
    // `--yes` skips the interactive preview: the source was already vetted at
    // install time, and a detached caller has no tty to answer it on anyway.
    // The install is bounded: herdr's fetch has no timeout of its own, and a
    // stall must not wedge the in-flight claim for the process lifetime.
    run_quiet_bounded(
        &bin,
        &["plugin", "install", GITHUB_SOURCE, "--yes"],
        timeout,
    )?;

    // A fresh probe drives the stamp: the reinstall may fetch a manifest whose
    // version still trails this binary, and only then does the heal stamp the
    // installed manifest. A probe error here is not a heal failure — the
    // reinstall landed, and the next window's probe retries the read.
    let mut stamped = false;
    if let (Some(fresh), None) = registry_probe(&bin)
        && plugin_update_needed(&fresh)
        && let Some(root) = fresh.plugin_root.as_deref()
    {
        let manifest = Path::new(root).join("herdr-plugin.toml");
        stamped = stamp_manifest_version(&manifest)
            .with_context(|| format!("failed to stamp {}", manifest.display()))?;
    }

    let mut line = format!("reinstalled the herdr plugin from {GITHUB_SOURCE} (was {old})");
    if stamped {
        line.push_str(&format!(
            "; stamped the manifest version to {}",
            crate::update::CURRENT_VERSION
        ));
    }
    Ok(Some(line))
}

/// Stamps the top-level `version = "..."` line of a herdr plugin manifest to
/// [`crate::update::CURRENT_VERSION`], preserving every other byte. A manifest
/// already at the crate version returns `Ok(false)` with no write, so a
/// matching file stays byte-identical.
///
/// The write is atomic: the staged sibling carries the original file's
/// permissions before the rename, so a reader sees the old or the new bytes and
/// the manifest keeps its mode.
fn stamp_manifest_version(path: &Path) -> Result<bool> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("cannot read {}", path.display()))?;
    let doc: toml::Value = toml::from_str(&text)
        .with_context(|| format!("{} does not parse as toml", path.display()))?;
    let current = doc
        .get("version")
        .and_then(toml::Value::as_str)
        .with_context(|| format!("{} carries no version", path.display()))?;
    if current == crate::update::CURRENT_VERSION {
        return Ok(false);
    }
    let stamped = rewrite_version_line(&text, current).with_context(|| {
        format!(
            "{} has no top-level `version = \"{current}\"` line",
            path.display()
        )
    })?;
    publish_preserving_perms(path, stamped.as_bytes())
        .with_context(|| format!("failed to stamp {}", path.display()))?;
    Ok(true)
}

/// Replaces the top-level `version = "old"` line in place, leaving every
/// comment and other key untouched, so a reserialization cannot reformat the
/// manifest. `Err` when no top-level line matched: a write that changed nothing
/// must not read as a successful stamp.
fn rewrite_version_line(text: &str, old: &str) -> Result<String> {
    let needle = format!("version = \"{old}\"");
    let mut replaced = false;
    let out: Vec<String> = text
        .lines()
        .map(|line| {
            if !replaced && !line.starts_with(' ') && line.trim() == needle.as_str() {
                replaced = true;
                format!("version = \"{}\"", crate::update::CURRENT_VERSION)
            } else {
                line.to_string()
            }
        })
        .collect();
    if !replaced {
        bail!("no top-level `{needle}` line");
    }
    let mut out = out.join("\n");
    if text.ends_with('\n') {
        out.push('\n');
    }
    Ok(out)
}

/// Atomic publish that keeps the destination's permissions: the staged sibling
/// takes the original's mode before the rename, and the rename stays within one
/// directory. Shared by the install stamp and the heal stamp.
fn publish_preserving_perms(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let perms = std::fs::metadata(path).map(|m| m.permissions()).ok();
    let tmp = crate::profile::tmp_sibling(path);
    std::fs::write(&tmp, bytes)?;
    if let Some(perms) = perms {
        std::fs::set_permissions(&tmp, perms)?;
    }
    match std::fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

/// Stamp the floor at now, so [`heal_detached`] refuses every attempt for the
/// next window. For a test that drives a caller of the heal (the daemon tick,
/// `clauth mcp` startup) and is about something else.
#[cfg(test)]
pub(crate) fn arm_heal_throttle_for_test() {
    HEAL_THROTTLE.arm_for_test();
}

/// Clear both flags so a test can drive a heal fresh.
#[cfg(all(test, unix))]
pub(crate) fn reset_heal_throttle_for_test() {
    HEAL_THROTTLE.reset_for_test();
}

fn resolve_key(key: Option<&str>, yes: bool) -> Result<String> {
    let key = match key {
        Some(k) => k.trim().to_string(),
        None if yes || !is_tty() => DEFAULT_KEY.to_string(),
        None => {
            out!("clauth: key that opens the dashboard [{DEFAULT_KEY}] ");
            let mut line = String::new();
            std::io::stdin().read_line(&mut line)?;
            match line.trim() {
                "" => DEFAULT_KEY.to_string(),
                answer => answer.to_string(),
            }
        }
    };
    validate_key(&key)?;
    Ok(key)
}

/// Bounds only what could break the file. herdr is the authority on whether a
/// spec means anything, and `config check` reports the ones it would disable
/// before any of this reaches the real config.
fn validate_key(key: &str) -> Result<()> {
    if key.is_empty() || key.len() > 64 {
        bail!("key '{key}' is empty or too long; expected a herdr spec like `{DEFAULT_KEY}`");
    }
    if key.chars().any(|c| c == '"' || c == '\\' || c.is_control()) {
        bail!("key '{key}' carries a quote, a backslash, or a control character");
    }
    Ok(())
}

fn is_tty() -> bool {
    std::io::stdin().is_terminal() && std::io::stdout().is_terminal()
}

/// Default-no: every caller changes something clauth does not own. The question is the caller's, since one of them adds to a config and the other removes a plugin as well as config lines.
fn confirm(question: &str, yes: bool) -> Result<bool> {
    if yes {
        return Ok(true);
    }
    if !is_tty() {
        errln!("clauth: not a terminal, so nothing was changed; rerun with --yes");
        return Ok(false);
    }
    out!("clauth: {question} [y/N] ");
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    let answer = line.trim();
    Ok(answer.eq_ignore_ascii_case("y") || answer.eq_ignore_ascii_case("yes"))
}

fn print_manual(key: &str, delegate_row_text: bool) {
    outln!("clauth: add these to herdr's config.toml yourself:");
    outln!("");
    outln!("{}", binding_block(key));
    outln!("{}", sidebar_block(delegate_row_text));
}

fn binding_block(key: &str) -> String {
    format!(
        "{MARKER}\n[[keys.command]]\nkey = \"{key}\"\ntype = \"plugin_action\"\ncommand = \"{OPEN_ACTION}\"\ndescription = \"clauth accounts\"\n"
    )
}

fn sidebar_block(delegate_row_text: bool) -> String {
    format!(
        "{MARKER}: `{TOKEN}` renders the account each Claude Code pane burns\n[ui.sidebar.agents.rows_by_agent]\n{}\n",
        sidebar_row(delegate_row_text)
    )
}

/// The claude row template, the knob's only effect: with `delegate_row_text`
/// on, the agent group also names `$clauth_delegate`, so a running delegate
/// reads as text beside the row. Off is today's row, byte for byte.
fn sidebar_row(delegate_row_text: bool) -> String {
    let agent = if delegate_row_text {
        format!(r#"["agent", "{TOKEN}", "{DELEGATE_TOKEN}"]"#)
    } else {
        format!(r#"["agent", "{TOKEN}"]"#)
    };
    format!(
        r#"claude = [["state_icon", "workspace", "tab"], ["terminal_title_stripped"], {agent}]"#
    )
}

/// What `plan_config` decided: text to append, plus what it refused to touch.
struct ConfigPlan {
    append: String,
    notes: Vec<String>,
}

impl ConfigPlan {
    /// Takes a block only when the file it lands in still parses with it.
    ///
    /// Walking the parsed tree answers "is this defined", never "can a header
    /// for it be appended": `ui = { sidebar = ... }` reads as an absent
    /// `ui.sidebar.agents.rows_by_agent` and rejects the header that would
    /// extend it, and a plain `[keys.command]` table rejects an appended
    /// `[[keys.command]]`. Both are valid TOML nobody would call unusual, so
    /// the block is tried against the real text and handed over on a miss.
    fn try_append(&mut self, existing: &str, block: &str, what: &str) {
        let candidate = with_append(existing, &format!("{}{block}", self.append));
        if toml::from_str::<toml::Value>(&candidate).is_ok() {
            self.append.push_str(block);
            return;
        }
        self.notes.push(format!(
            "your config spells the table {what} belongs in a way clauth cannot extend by appending, so add it yourself:\n{}",
            block.trim_start_matches('\n')
        ));
    }
}

/// The one place the two halves are glued, so a test that pins the seam is
/// pinning what `install` runs rather than a second copy of it.
fn with_append(existing: &str, append: &str) -> String {
    let mut text = existing.to_string();
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str(append);
    text
}

/// What the sidebar half of the config says. Maps one-to-one onto the four arms `plan_config` matches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SidebarState {
    /// The claude row already renders the token.
    Templated,
    /// The claude row exists but does not render the token.
    OtherClaudeRow,
    /// `rows_by_agent` covers other agents but has no claude row.
    OtherAgentsOnly,
    /// No `rows_by_agent` table at all.
    Absent,
}

/// The config-side verdicts the Plugin tab's herdr row shows, read straight from the parsed document. `parsed` is false when the file does not parse.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ConfigStatus {
    pub(crate) parsed: bool,
    /// The key spelling bound to `clauth.open`, when one is.
    pub(crate) bound_key: Option<String>,
    pub(crate) sidebar: SidebarState,
}

/// Pure string -> verdict. The caller does the file read, so the row can show a missing or unreadable file without a second parse.
pub(crate) fn config_status(existing: &str) -> ConfigStatus {
    match toml::from_str::<toml::Value>(existing) {
        Ok(doc) => ConfigStatus {
            parsed: true,
            bound_key: bound_key(&doc),
            sidebar: sidebar_state(&doc),
        },
        Err(_) => ConfigStatus {
            parsed: false,
            bound_key: None,
            sidebar: SidebarState::Absent,
        },
    }
}

/// The key spelling of the entry bound to `clauth.open`, if any.
fn bound_key(doc: &toml::Value) -> Option<String> {
    doc.get("keys")
        .and_then(|k| k.get("command"))
        .and_then(toml::Value::as_array)
        .and_then(|entries| {
            entries
                .iter()
                .find(|e| e.get("command").and_then(toml::Value::as_str) == Some(OPEN_ACTION))
        })
        .and_then(|e| e.get("key").and_then(toml::Value::as_str))
        .map(str::to_string)
}

fn sidebar_state(doc: &toml::Value) -> SidebarState {
    let Some(table) = doc
        .get("ui")
        .and_then(|u| u.get("sidebar"))
        .and_then(|s| s.get("agents"))
        .and_then(|a| a.get("rows_by_agent"))
    else {
        return SidebarState::Absent;
    };
    match table.get("claude") {
        Some(row) if mentions_token(row) => SidebarState::Templated,
        Some(_) => SidebarState::OtherClaudeRow,
        None => SidebarState::OtherAgentsOnly,
    }
}

/// Decides what an existing herdr config is missing. Append-only by design: a
/// table the config already defines is reported for the user to merge rather
/// than emitted twice, since a duplicate key is a parse error, and rewriting
/// the file structurally would drop their comments and ordering.
///
/// Both verdicts route through the same helpers `config_status` uses, so the row and the install plan cannot drift.
///
/// The resync callers (`install`, `heal`) strip clauth's marked blocks before
/// planning, so a knob toggle plans against the base the strip left and
/// re-appends the row the knob now asks for. A block the strip kept as
/// user-owned comes through here intact, and the verdicts below report it
/// as hand-owned instead of re-adding it.
fn plan_config(existing: &str, key: &str, delegate_row_text: bool) -> Result<ConfigPlan> {
    let doc: toml::Value = toml::from_str(existing)
        .context("herdr's config.toml does not parse; fix it before wiring clauth into it")?;

    let mut plan = ConfigPlan {
        append: String::new(),
        notes: Vec::new(),
    };

    if bound_key(&doc).is_some() {
        plan.notes.push(format!(
            "`{OPEN_ACTION}` is already bound, so the keybinding is left alone"
        ));
    } else {
        plan.try_append(
            existing,
            &format!("\n{}", binding_block(key)),
            "the keybinding",
        );
    }

    match sidebar_state(&doc) {
        SidebarState::Templated => plan.notes.push(
            "the sidebar already renders the account, so the rows are left alone".to_string(),
        ),
        SidebarState::OtherClaudeRow => plan.notes.push(format!(
            "your `[ui.sidebar.agents.rows_by_agent]` already sets a claude row, so add `\"{TOKEN}\"` to one of its groups yourself: {}",
            sidebar_row(delegate_row_text)
        )),
        SidebarState::OtherAgentsOnly => plan.notes.push(format!(
            "your `[ui.sidebar.agents.rows_by_agent]` covers other agents, so add this line under it yourself: {}",
            sidebar_row(delegate_row_text)
        )),
        SidebarState::Absent => {
            plan.try_append(
                existing,
                &format!("\n{}", sidebar_block(delegate_row_text)),
                "the sidebar row",
            );
        }
    }

    Ok(plan)
}

/// Appends whatever `plan_config` says is missing, after stripping the blocks
/// a previous run wrote: the strip is what makes a knob toggle rewrite
/// exactly clauth's own blocks, and an unchanged knob reconstructs the same
/// text, so the write (and the `herdr config check` behind it) is skipped.
/// Returns the plan's notes (the pieces it refused to touch), empty when it
/// wrote everything.
pub(crate) fn heal(
    config_path: &Path,
    key: &str,
    bin: &str,
    delegate_row_text: bool,
) -> Result<Vec<String>> {
    let existing = read_config(config_path)?;
    // An existing `clauth.open` binding is the user's key choice: heal
    // refreshes what clauth wrote, it must not re-key the binding. The
    // caller's key applies only when nothing binds the action yet.
    let plan_key = config_status(&existing)
        .bound_key
        .unwrap_or_else(|| key.to_string());
    let (text, plan, _) = resync_text(&existing, &plan_key, delegate_row_text)?;
    if text != existing {
        write_validated(config_path, &existing, &text, bin)?;
    }
    Ok(plan.notes)
}

/// The resync seam `install` and `heal` write through: strip the blocks
/// clauth wrote, plan on what is left, append the plan. A knob toggle then
/// rewrites exactly the blocks clauth wrote — the strip takes the old blocks
/// off, the plan re-adds the row the knob now asks for, nothing user-owned
/// moves — while an unchanged knob reconstructs the text byte for byte,
/// which is what keeps the callers' writes a no-op. A block the user edited
/// is kept; when a kept block follows a stripped one, re-appending the
/// stripped block would move it past the kept one, so the resync then plans
/// against the file as it is and reports what it sees through the
/// hand-owned notes. Split out so a test that pins the seam is pinning what
/// the two callers run rather than a second copy of it, the same reason
/// `with_append` is one function.
fn resync_text(
    existing: &str,
    key: &str,
    delegate_row_text: bool,
) -> Result<(String, ConfigPlan, Vec<String>)> {
    let (base, removed, kept_after_stripped) = strip_marked_blocks(existing);
    let (base, removed) = if kept_after_stripped {
        (existing.to_string(), Vec::new())
    } else {
        (base, removed)
    };
    let plan = plan_config(&base, key, delegate_row_text)?;
    let text = with_append(&base, &plan.append);
    Ok((text, plan, removed))
}

/// `resync_text` plus the no-op verdict `install` branches on: an unchanged
/// knob reconstructs the file byte for byte (the round-trip pins hold that
/// half), so `text == existing` is the skip. Split out so a test pins the
/// verdict both ways instead of the branch living unbacked in `install`,
/// where a TTY and herdr's installer keep tests out.
fn install_resync(
    existing: &str,
    key: &str,
    delegate_row_text: bool,
) -> Result<(String, ConfigPlan, Vec<String>, bool)> {
    let (text, plan, removed) = resync_text(existing, key, delegate_row_text)?;
    let noop = text == existing;
    Ok((text, plan, removed, noop))
}

/// Test seam over [`strip_marked_blocks`], so the round-trip tests name the rule they pin.
#[cfg(test)]
fn without_marked_blocks(existing: &str) -> String {
    strip_marked_blocks(existing).0
}

/// Drops every block this crate wrote, nothing else, and returns the removed lines in order so `uninstall` can print a `- ` diff that mirrors the `+ ` one `install` prints. "Wrote" is judged by content: a buffered block is stripped only when it equals a block the generators emit — both knob variants of the sidebar block, or the binding block modulo its `key =` line, so the key `install` was run under does not make the binding read as an edit. An edited or interrupted block is kept whole, and the third return is `true` when a kept block follows an already-stripped one — the one arrangement in which re-appending the stripped block would move the kept block out of place; `resync_text` keeps the whole file in that case.
///
/// A block is real only when a `[`-leading line follows the marker, since that header is what `install` always writes; the blank `install` prepends before it is dropped too. A marker standing alone drops itself and leaves the next line on the normal path. The residue is a marker inside a multi-line string whose next line happens to begin with `[`; telling that apart needs a TOML parser, and this strip only runs over lines `install` wrote, where the header always follows.
/// The line prefixes a marked block's own tables use. The strip ends a block
/// at the first line outside this vocab, so a user key glued directly to the
/// block's last line (no blank separator, still valid TOML in the same table)
/// survives the resync instead of being eaten with the block.
/// A marked block's own table lines, keyed on which table the header names.
/// The split matters: a `claude = ` glued to the keys block's end is a user
/// key in `[[keys.command]]`, not the sidebar row, and the union vocab would
/// eat it.
fn is_block_line(header: &str, lead: &str) -> bool {
    if header.contains("rows_by_agent") {
        lead.starts_with("claude = ")
    } else {
        ["key = ", "type = ", "command = ", "description = "]
            .iter()
            .any(|p| lead.starts_with(p))
    }
}

fn strip_marked_blocks(existing: &str) -> (String, Vec<String>, bool) {
    let mut out: Vec<String> = Vec::new();
    let mut removed: Vec<String> = Vec::new();
    // The third return: a kept block follows an already-stripped one, the
    // one arrangement in which re-appending the stripped block would move
    // the kept block out of place. `resync_text` keys its keep-everything
    // rule on this.
    let mut stripped_block = false;
    let mut kept_after_stripped = false;
    // The block being skipped: marker + header + table lines, buffered rather
    // than committed line by line, because a line clauth did not write turns
    // the whole block user-owned and it must be restored, header included.
    // Committing early strands the tail lines under a table whose header was
    // already removed.
    let mut block: Vec<String> = Vec::new();
    let mut header: Option<String> = None;
    let mut skipping = false;
    let mut lines = existing.split_inclusive('\n').peekable();

    while let Some(raw) = lines.next() {
        let content = raw.strip_suffix('\n').unwrap_or(raw);
        let lead = content.trim_start();

        if skipping {
            if lead.is_empty() || lead.starts_with('[') {
                skipping = false;
                // A block one of the generators would write is stripped; an
                // edited block is the user's now and is restored whole.
                if is_clauth_block(&block) {
                    removed.extend(stripped(&mut block));
                    stripped_block = true;
                } else {
                    kept_after_stripped |= stripped_block;
                    out.append(&mut block);
                }
                out.push(raw.to_string());
            } else if is_block_line(header.as_deref().unwrap_or(""), lead) {
                block.push(raw.to_string());
            } else {
                // A line clauth did not write: the block is user-owned, so
                // the strip keeps everything it would have removed. The plan
                // then sees the binding as hand-owned and re-adds nothing.
                skipping = false;
                kept_after_stripped |= stripped_block;
                out.append(&mut block);
                out.push(raw.to_string());
            }
            continue;
        }

        if lead.starts_with(MARKER) {
            // Pop the blank install prepends only when a real block follows, so
            // a standalone marker keeps the line above it too. The popped blank
            // joins the buffer: an interrupted block restores it with the rest.
            if lines
                .peek()
                .is_some_and(|next| next.trim_start().starts_with('['))
                && out.last().is_some_and(|last| last.trim().is_empty())
                && let Some(blank) = out.pop()
            {
                block.push(blank);
            }
            block.push(raw.to_string());
            // `next_if` leaves a non-`[` line for the normal path rather than
            // consuming it as a header.
            if let Some(h) = lines.next_if(|next| next.trim_start().starts_with('[')) {
                header = Some(h.strip_suffix('\n').unwrap_or(h).to_string());
                block.push(h.to_string());
                skipping = true;
            } else {
                // A standalone marker: clauth's marker, nothing below it.
                removed.extend(stripped(&mut block));
            }
            continue;
        }

        out.push(raw.to_string());
    }
    // A block running to the file's end is clauth's unless a user edited it.
    if skipping {
        if is_clauth_block(&block) {
            removed.extend(stripped(&mut block));
        } else {
            kept_after_stripped |= stripped_block;
            out.append(&mut block);
        }
    }

    (out.concat(), removed, kept_after_stripped)
}

/// The display list's shape: one entry per line, newlines stripped. `out`
/// keeps raw lines (the newlines are the file), `removed` is what the diffs
/// print.
fn stripped(block: &mut Vec<String>) -> impl Iterator<Item = String> + '_ {
    block
        .drain(..)
        .map(|l| l.strip_suffix('\n').unwrap_or(&l).to_string())
}

/// Whether a buffered block is one clauth itself would write: the sidebar
/// block under either knob variant (a toggle still strips the old row), or
/// the binding block with the key read back off its own `key =` line, so a
/// binding is clauth's whatever key `install` was run under. The block may
/// open with the blank the marker branch pops into the buffer (`install`
/// prepends it before its blocks), and a block ending the file may carry no
/// trailing newline; the candidates carry neither.
fn is_clauth_block(block: &[String]) -> bool {
    let text = block.concat();
    let text = text.strip_prefix('\n').unwrap_or(&text);
    let text = text.strip_suffix('\n').unwrap_or(text);

    let on = sidebar_block(true);
    let off = sidebar_block(false);
    if text == on.strip_suffix('\n').unwrap_or(&on)
        || text == off.strip_suffix('\n').unwrap_or(&off)
    {
        return true;
    }
    let Some(binding_key) = key_from_block(block) else {
        return false;
    };
    let candidate = binding_block(&binding_key);
    text == candidate.strip_suffix('\n').unwrap_or(&candidate)
}

/// The key a binding block binds, read back off its own `key = "..."` line.
/// The binding comparison runs modulo this key, so a block is clauth's
/// whatever key `install` was run under, while any other edit keeps it.
fn key_from_block(block: &[String]) -> Option<String> {
    block.iter().find_map(|raw| {
        let line = raw.strip_suffix('\n').unwrap_or(raw).trim_start();
        let inner = line.strip_prefix("key = \"")?;
        inner.strip_suffix('"').map(str::to_string)
    })
}

pub(crate) fn uninstall(no_config: bool, yes: bool) -> Result<()> {
    let bin = herdr_bin();

    // Read and strip before touching herdr, so one confirm covers both halves
    // and a decline leaves the plugin and the config both untouched.
    let config_edit: Option<(PathBuf, String, String, Vec<String>)> = if no_config {
        None
    } else {
        let path = config_path(&bin)?;
        let previous = read_config(&path)?;
        let (text, removed, _) = strip_marked_blocks(&previous);
        (text != previous).then_some((path, previous, text, removed))
    };

    outln!("clauth: this removes the clauth plugin from herdr");
    if let Some((path, _, _, removed)) = &config_edit {
        let mut diff = removed.clone();
        // The first removed line is the blank `install` prepends before its first block; `install`'s diff trims it, so this one does too.
        if diff.first().is_some_and(String::is_empty) {
            diff.remove(0);
        }
        outln!("");
        outln!("{}:", path.display());
        for line in &diff {
            outln!("- {line}");
        }
        outln!("");
    }

    let question = if config_edit.is_some() {
        "remove the plugin and these config lines?"
    } else {
        "remove the clauth plugin from herdr?"
    };
    if !confirm(question, yes)? {
        outln!("clauth: nothing changed");
        return Ok(());
    }

    match uninstall_plugin(&bin)? {
        PluginUninstall::Done => outln!("clauth: uninstalled the herdr plugin"),
        PluginUninstall::NotInstalled => {
            outln!("clauth: herdr had no clauth plugin to uninstall (plugin not installed)")
        }
    }

    if let Some((path, previous, text, _)) = config_edit {
        write_validated(&path, &previous, &text, &bin)?;
        outln!("clauth: removed clauth's additions from {}", path.display());
    }

    Ok(())
}

enum PluginUninstall {
    Done,
    NotInstalled,
}

/// `herdr plugin uninstall clauth`. herdr exits 1 with a `plugin not installed` line when there is nothing to remove; the caller treats that as a no-op. The phrase must start a line, so a real failure that merely mentions it still fails.
fn uninstall_plugin(bin: &str) -> Result<PluginUninstall> {
    let out = Command::new(bin)
        .args(["plugin", "uninstall", PLUGIN_ID])
        .output()
        .with_context(|| {
            format!(
                "could not run `{bin} plugin uninstall {PLUGIN_ID}`; is herdr installed and on PATH?"
            )
        })?;
    if out.status.success() {
        return Ok(PluginUninstall::Done);
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let not_installed = format!("{stdout}\n{stderr}")
        .lines()
        .any(|line| line.trim_start().starts_with("plugin not installed"));
    if out.status.code() == Some(1) && not_installed {
        return Ok(PluginUninstall::NotInstalled);
    }
    let mut why = stdout.trim().to_string();
    let err = stderr.trim();
    if !err.is_empty() {
        why.push('\n');
        why.push_str(err);
    }
    bail!("`{bin} plugin uninstall {PLUGIN_ID}` failed:\n{why}");
}

/// Walks a row template looking for the token. A row is an array of arrays of
/// strings, so a plain string search over a rendering would depend on how the
/// toml crate happens to format one.
fn mentions_token(value: &toml::Value) -> bool {
    match value {
        toml::Value::String(s) => s == TOKEN,
        toml::Value::Array(items) => items.iter().any(mentions_token),
        _ => false,
    }
}

/// Writes only what herdr accepts. The check runs against a copy, so a rejected
/// edit never reaches the real file, and the real write lands in place, so the
/// file keeps its own mode.
///
/// A config already carrying a complaint of its own still gets wired. `herdr
/// config check` diagnoses the whole file, so refusing on its exit code alone
/// locks anyone with one stale key out of this command over something that
/// predates it. Only a diagnostic this edit ADDS is clauth's to refuse over.
fn write_validated(path: &Path, previous: &str, text: &str, bin: &str) -> Result<()> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(dir).with_context(|| format!("failed to create {}", dir.display()))?;
    let probe = tempfile::Builder::new()
        .prefix(".clauth-herdr")
        .tempfile_in(dir)?;

    let before = check_config(bin, probe.path(), previous)?;
    let after = check_config(bin, probe.path(), text)?;
    let added = added_diagnostics(&before, &after);
    if !added.is_empty() {
        bail!(
            "herdr rejected what clauth would add, so nothing changed:\n{}",
            added.join("\n")
        );
    }
    for stale in &before {
        errln!("clauth: herdr already says this about your config: {stale}");
    }

    // Shortcut, with its ceiling: a truncating in-place write is what keeps the
    // file's mode and inode, and its cost is that a crash or a full disk mid-
    // write leaves the config short. The upgrade is write-temp-then-rename with
    // the original's mode read and restored onto the temp first, which is worth
    // doing the day this writes anything a user cannot retype.
    std::fs::write(path, text).with_context(|| format!("failed to write {}", path.display()))
}

/// Diagnostics `after` carries that `before` did not, which is the only set
/// this command answers for. Order-preserving, and a line repeated in `after`
/// counts once, matching how the message reads.
fn added_diagnostics<'a>(before: &[String], after: &'a [String]) -> Vec<&'a str> {
    let mut added: Vec<&str> = Vec::new();
    for line in after {
        if !before.contains(line) && !added.contains(&line.as_str()) {
            added.push(line);
        }
    }
    added
}

/// `herdr config check` over `text`, as its diagnostic lines. An accepted
/// config answers with none, so callers compare two runs rather than two exit
/// codes.
fn check_config(bin: &str, probe: &Path, text: &str) -> Result<Vec<String>> {
    std::fs::write(probe, text)?;
    let out = bounded_output(
        bin,
        &["config", "check"],
        &[("HERDR_CONFIG_PATH", probe.as_os_str())],
    )
    .with_context(|| format!("`{bin} config check` timed out or could not run"))?;
    if out.status.success() {
        return Ok(Vec::new());
    }
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .chain(String::from_utf8_lossy(&out.stderr).lines())
        .map(str::trim)
        // The header names the outcome rather than an issue, and it reads the
        // same on both runs, so it would never survive the diff anyway.
        .filter(|line| !line.is_empty() && *line != "config: issues found")
        .map(str::to_string)
        .collect())
}

// ── Knob read path (`clauth herdr config get`) ─────────────────────────────

/// `clauth herdr config get <key>` — the plugin scripts' read path for the
/// knobs persisted under `[herdr]` in profiles.toml. One value per line in a
/// shell shape: `fit|half|split-right|split-top` for `popup_width`, `on|off`
/// for the bools, the
/// bare number for `tag_watch_secs`, so a caller never parses help prose. A
/// missing profiles.toml answers the defaults; an unknown key is a usage error
/// (exit 2) naming the valid keys.
pub(crate) fn config_get(key: &str) -> Result<()> {
    let config = crate::profile::load_config()?;
    outln!("{}", herdr_value(&config.state.herdr, key)?);
    Ok(())
}

/// The pure half of [`config_get`]: the one-line value for `key`. Split out so
/// the knob table and the error surface are pinned without capturing stdout.
fn herdr_value(herdr: &crate::profile::HerdrSettings, key: &str) -> Result<String> {
    let on_off = |b: bool| if b { "on" } else { "off" };
    match key {
        "popup_width" => Ok(herdr.popup_width.as_str().to_string()),
        "pane_tag" => Ok(on_off(herdr.pane_tag).to_string()),
        "tag_watch_secs" => Ok(herdr.tag_watch_secs.to_string()),
        "border_label" => Ok(on_off(herdr.border_label).to_string()),
        "delegate_dot" => Ok(on_off(herdr.delegate_dot).to_string()),
        "delegate_row_text" => Ok(on_off(herdr.delegate_row_text).to_string()),
        _ => Err(crate::UsageError(format!(
            "unknown herdr config key '{key}'; valid keys: popup_width, pane_tag, \
             tag_watch_secs, border_label, delegate_dot, delegate_row_text"
        ))
        .into()),
    }
}

#[cfg(test)]
#[path = "../tests/inline/herdr.rs"]
mod tests;
