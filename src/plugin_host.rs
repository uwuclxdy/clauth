//! The agentgear [`PluginHost`] derive plus the four lifecycle wrappers clauth
//! calls: the Plugin tab's one-key install, the SessionStart self-heal hook, the
//! `clauth start` pre-flight, and the throttled detached heal `clauth mcp` and
//! the daemon share. The hook cannot be the migration trigger — a marketplace
//! that fails to load means the plugin never loads, so the hook never fires —
//! which is why the pre-flight and the detached heal both key off the same gate.
//!
//! clauth's plugin tree lives in `plugins/` (not the default `plugin/`), so the
//! derive's `tree` attr and `build.rs`'s `assert_plugin_version_at` both name
//! it. The tree itself stays a stock Claude Code plugin — `plugin.json` + the
//! `hooks/` dir — and agentgear supplies the lifecycle around it: materialize
//! the tree, drive `claude plugin marketplace add` + `plugin install`, verify
//! through `plugin list --json`, and stamp a marker self-heal keys on.
//!
//! The `claude`-shelling paths here are the ONLY lifecycle call sites;
//! nothing else in the crate shells out to `claude plugin` (the Plugin tab's
//! probe reads the registry files directly, and the manual `mcpServers` fallback
//! is a settings write). The lifecycle is pinned hermetically by the
//! fake-`claude` tests in `tests/inline/tui_app.rs` and the self-heal pin in
//! `tests/inline/plugin_host.rs` — both `#[cfg(unix)]` (the fake CLI is a
//! shell shim), so a Windows CI leg does not run them.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use agentgear::{Outcome, PluginHost, Scope, Source};

/// The plugin host for the committed `plugins/` tree. Claude-only: the default
/// `agents` list already names just `claude`, so no agent feature flags beyond
/// the crate defaults (derive + claude + embed) are enabled.
#[derive(PluginHost)]
#[plugin(name = "clauth", tree = "$CARGO_MANIFEST_DIR/plugins")]
pub(crate) struct ClauthPlugin;

/// The Plugin tab's one-key install: a user-scope install from the embedded
/// tree. The single spelling site — the tab's confirm handler and its pin test
/// both go through here, so `Scope::User` + `Source::Embedded` live in one
/// place and the copy-paste hint they replace has no other home to drift into.
pub(crate) fn install() -> anyhow::Result<Outcome> {
    Ok(ClauthPlugin::install(Scope::User, Source::Embedded)?)
}

/// The SessionStart hook body (`clauth self-heal`). Repairs a broken
/// registration, never resurrects an uninstall — agentgear's marker gate makes
/// a deliberately removed plugin stay removed. A healthy session prints
/// nothing, so a hook that fires on every session start injects no noise into
/// the conversation; a repair (or a failure) is worth saying out loud.
pub(crate) fn self_heal() -> anyhow::Result<()> {
    if let Some(line) = self_heal_line()? {
        crate::out::outln!("{line}");
    }
    Ok(())
}

/// What the hook says, or `None` when there is nothing to say: the outcome
/// becomes a line only when the heal changed something. Split from
/// [`self_heal`] so a test can pin the contract without a terminal.
pub(crate) fn self_heal_line() -> anyhow::Result<Option<String>> {
    let outcome = ClauthPlugin::self_heal()?;
    Ok((!matches!(outcome, Outcome::NoOp)).then(|| format!("clauth self-heal: {outcome}")))
}

/// The `clauth start` pre-flight: the migration trigger that heals a broken or
/// divergent clauth marketplace registration before `claude` launches. The hook
/// self-heal cannot be this trigger — a marketplace that fails to load means the
/// plugin never loads, so the hook never fires.
///
/// The gate below is two plain registry reads; a healthy registration spawns
/// nothing. A heal failure is logged and never fails the start: the session
/// still launches, and the hook (once the plugin loads again) keeps trying.
pub(crate) fn preflight() {
    if !preflight_gate() {
        return;
    }
    if let Err(e) = self_heal() {
        crate::logline::logline!("clauth: plugin pre-flight heal failed: {e:#}");
    }
}

/// A heal may run at most once per process every 30 minutes, success or failure:
/// both detached callers (`clauth mcp` boot, daemon tick) can fire once per
/// second on a box whose `claude` is missing, and an untried retry every tick
/// would spawn + fail forever. The in-flight flag bounds overlap; the floor
/// bounds frequency from the attempt STARTING (not finishing), so a slow heal
/// still cannot stack a second one behind it.
const HEAL_THROTTLE_MS: u64 = 30 * 60 * 1000;
static HEAL_IN_FLIGHT: AtomicBool = AtomicBool::new(false);
static LAST_HEAL_START_MS: AtomicU64 = AtomicU64::new(0);

/// The shared detached heal: `clauth mcp` runs it before its stdio handshake and
/// the daemon once per tick. The gate runs INLINE (two registry reads, no
/// spawn); only a "heal" verdict spawns anything, and then on its own thread so
/// neither caller is ever blocked by a `claude plugin` spawn. The throttle lives
/// here, not at either call site, so a call site cannot forget it.
///
/// The outcome and any failure go through [`logline!`] — stderr for both callers
/// — never `out::outln!`: `clauth mcp`'s stdout is a JSON-RPC stream, and one
/// stray line corrupts the session.
pub(crate) fn heal_detached() {
    if !preflight_gate() {
        return;
    }
    let now = crate::usage::now_ms();
    if HEAL_IN_FLIGHT.swap(true, Ordering::AcqRel) {
        return;
    }
    // Built the moment the flag is claimed, on the CALLING thread, then moved
    // into the worker: every early return and every panic between here and the
    // spawn clears the flag through this drop rather than wedging the throttle
    // shut for the rest of the process.
    let inflight = HealInFlight;
    if now.saturating_sub(LAST_HEAL_START_MS.load(Ordering::Relaxed)) < HEAL_THROTTLE_MS {
        return;
    }
    // Fail closed in test builds. The worker shells out to `claude` and lets
    // agentgear write its marker tree, and BOTH resolve off the process
    // environment: `PATH`, `HOME`, `XDG_DATA_HOME`, `XDG_RUNTIME_DIR`. Only
    // `FakeClaude` pins those, which is why the predicate is its sentinel rather
    // than clauth's own home override — that override is real and is still not
    // the thing making this hermetic.
    #[cfg(test)]
    assert!(
        std::env::var_os("CLAUDE_SHIM_STATE").is_some(),
        "heal_detached would spawn the operator's real `claude` and write their \
         real agentgear tree — stage a `FakeClaude` beside the `HomeSandbox`, or \
         call `arm_heal_throttle_for_test` if the heal is not what the test is about"
    );
    LAST_HEAL_START_MS.store(now, Ordering::Relaxed);
    // Registered on THIS thread, before the spawn, the way the MCP background
    // delegate does it: `join_background_tasks` drains whatever is registered at
    // the moment it runs, so registering inside the worker lets a sandbox
    // teardown clear the home override with a heal still running — which then
    // resolves the operator's REAL `$HOME` and takes real locks under
    // `~/.clauth`.
    #[cfg(test)]
    let done = crate::testutil::register_background_task();
    std::thread::spawn(move || {
        let _inflight = inflight;
        match self_heal_line() {
            Ok(Some(line)) => crate::logline::logline!("{line}"),
            Ok(None) => {}
            Err(e) => crate::logline::logline!("clauth: plugin heal failed: {e:#}"),
        }
        // Last action, after every `$HOME`-touching step: `HealInFlight` touches
        // one atomic and nothing else, so its later drop is safe past the send.
        #[cfg(test)]
        let _ = done.send(());
    });
}

/// Clears the in-flight flag however the claim ends — an early return, a panic,
/// or the heal thread finishing — so nothing wedges the throttle shut. Built by
/// the caller at the swap and moved into the worker, so the window between the
/// two is covered too.
struct HealInFlight;

impl Drop for HealInFlight {
    fn drop(&mut self) {
        HEAL_IN_FLIGHT.store(false, Ordering::Release);
    }
}

/// `unix` because every caller is a `#[cfg(unix)]` test: the heal's fake
/// `claude` is a shell shim. A bare `cfg(test)` gate is dead code on Windows,
/// which `-D warnings` reds there and nowhere else.
#[cfg(all(test, unix))]
pub(crate) fn reset_heal_throttle_for_test() {
    HEAL_IN_FLIGHT.store(false, Ordering::Release);
    LAST_HEAL_START_MS.store(0, Ordering::Relaxed);
}

/// Stamp the floor at now, so [`heal_detached`] refuses every attempt for the
/// next window. For a test that drives a caller of the heal (the daemon tick)
/// and is about something else: the gate's `expected_pointer` reads
/// `dirs::data_dir()`, which no sandbox can pin on Windows, so a seeded-healthy
/// registry is a unix-only way to keep such a test spawn-free. Arming the floor
/// is the cross-platform one. Serialized with every other user by `HomeSandbox`'s
/// own lock.
#[cfg(test)]
pub(crate) fn arm_heal_throttle_for_test() {
    HEAL_IN_FLIGHT.store(false, Ordering::Release);
    LAST_HEAL_START_MS.store(crate::usage::now_ms(), Ordering::Relaxed);
}

/// Whether the pre-flight should run the heal: `true` for every registry shape a
/// user-scope heal converges, `false` for a registration already sitting at
/// agentgear's materialized `current@claude` pointer with its generated manifest
/// present and every user-scope plugin entry's files resolvable, and `false` for
/// a box that holds nothing of ours in either registry. Read-only; a file this
/// cannot read counts as "heal" (conservative — the heal is idempotent, so a
/// needless run costs nothing but its own reads).
pub(crate) fn preflight_gate() -> bool {
    let Some(dir) = registry_dir() else {
        return true;
    };
    let Some(expected) = expected_pointer() else {
        return true;
    };
    let marketplaces = read_registry(&dir.join("plugins").join("known_marketplaces.json"));
    let installed = read_registry(&dir.join("plugins").join("installed_plugins.json"));

    // The "never installed" box. A false positive here makes every `clauth mcp`
    // boot and daemon tick spawn `claude plugin list --json` for nothing, and
    // agentgear can never install from a heal anyway, so it converges nothing.
    // ABSENT is the load-bearing half: a config dir that has never held a plugin
    // has no `plugins/` directory at all, so keying this on "parses, names
    // nothing" alone would miss exactly the population it is written for.
    // Unreadable is the other verdict, and it still heals.
    let marketplaces_empty = match &marketplaces {
        Registry::Missing => true,
        Registry::Unreadable => false,
        Registry::Parsed(doc) => doc.get("clauth").is_none(),
    };
    let installed_empty = match &installed {
        Registry::Missing => true,
        Registry::Unreadable => false,
        // Absence of the KEY, never "not an array": a foreign or corrupt value
        // under it is still something of ours registered, and the old gate healed
        // on it. Matches the marketplace conjunct above.
        Registry::Parsed(doc) => doc["plugins"]["clauth@clauth"].is_null(),
    };
    if marketplaces_empty && installed_empty {
        return false;
    }

    marketplace_needs_heal(marketplaces.doc(), &expected)
        || plugin_entries_need_heal(installed.doc())
}

/// The materialized pointer agentgear's `materialize` publishes: its locked
/// layout is `<data_dir>/<plugin>/current@<client>` (agentgear design
/// §materialize), which the lifecycle's own re-point logic compares against
/// too. Derived here rather than called because clauth builds against the
/// published agentgear crate, and the layout is the contract either way.
pub(crate) fn expected_pointer() -> Option<PathBuf> {
    dirs::data_dir().map(|base| base.join("clauth").join("current@claude"))
}

/// The config dir the heal itself operates on. agentgear's CLI wrapper keeps
/// `CLAUDE_CONFIG_DIR` in the child env, so the gate must read the same dir CC
/// resolves: the non-empty override when one is set, else `~/.claude`. An empty
/// override is "cannot tell" — the heal's own guard refuses it with a named
/// error, which is the report a start should surface rather than a silent skip.
fn registry_dir() -> Option<PathBuf> {
    match std::env::var_os("CLAUDE_CONFIG_DIR") {
        Some(v) if !v.is_empty() => Some(PathBuf::from(v)),
        Some(_) => None,
        None => crate::profile::claude_dir().ok(),
    }
}

/// What one registry-file read answers. `Missing` is a positive fact — nothing
/// was ever registered there — where `Unreadable` (a permission error, a
/// truncated write, a half-parsed file) says only that this pass cannot tell.
/// Collapsing the two is what made the gate heal on every never-installed box.
enum Registry {
    Missing,
    Unreadable,
    Parsed(serde_json::Value),
}

impl Registry {
    /// The parsed document, or `None` for a file this pass could not read. The
    /// two need-heal predicates take that shape because both treat "cannot
    /// read" and "absent" alike; only the never-installed check separates them.
    fn doc(&self) -> Option<&serde_json::Value> {
        match self {
            Registry::Parsed(doc) => Some(doc),
            Registry::Missing | Registry::Unreadable => None,
        }
    }
}

fn read_registry(path: &Path) -> Registry {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Registry::Missing,
        Err(_) => return Registry::Unreadable,
    };
    serde_json::from_slice(&bytes).map_or(Registry::Unreadable, Registry::Parsed)
}

/// The marketplace half of the gate: the `clauth` entry must be a directory
/// source registered exactly at the materialized pointer, with its generated
/// manifest present. Absent, github-sourced, diverged, or manifest-deleted all
/// heal — that is the deadlock the migration exists to break (a github entry's
/// next catalog refresh pulls a tree without the manifest and the plugin loads 0
/// hooks).
fn marketplace_needs_heal(doc: Option<&serde_json::Value>, expected: &Path) -> bool {
    let Some(doc) = doc else {
        return true;
    };
    let Some(entry) = doc.get("clauth") else {
        return true;
    };
    if entry["source"]["source"].as_str() != Some("directory") {
        return true;
    }
    let Some(path) = entry["source"]["path"].as_str() else {
        return true;
    };
    Path::new(path) != expected
        || !Path::new(path)
            .join(".claude-plugin")
            .join("marketplace.json")
            .exists()
}

/// The plugin half of the gate: a user-scope `clauth@clauth` entry whose files or
/// load state are gone. A per-session config dir leaves exactly this behind when
/// its runtime tree is collected — the entry survives, its `installPath` dies —
/// and only the heal can rewrite it. Project-scope entries never decide here: the
/// heal is user-scope and cannot fix them, so counting them would churn a heal
/// every start for nothing.
fn plugin_entries_need_heal(doc: Option<&serde_json::Value>) -> bool {
    let Some(doc) = doc else {
        return true;
    };
    let Some(rows) = doc["plugins"]["clauth@clauth"].as_array() else {
        return false;
    };
    rows.iter()
        .filter(|row| row["scope"].as_str().unwrap_or("user") == "user")
        .any(|row| {
            !row["errors"].as_array().is_none_or(Vec::is_empty)
                || row["installPath"]
                    .as_str()
                    .is_none_or(|p| !Path::new(p).exists())
        })
}

#[cfg(test)]
#[path = "../tests/inline/plugin_host.rs"]
mod tests;
