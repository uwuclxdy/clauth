//! The agentgear [`PluginHost`] derive plus the four lifecycle wrappers clauth
//! calls: the Services tab's install fix, the SessionStart self-heal hook, the
//! `clauth start` pre-flight, and the throttled detached heal `clauth mcp` and
//! the daemon share. The hook cannot be the migration trigger — a marketplace
//! that fails to load means the plugin never loads, so the hook never fires —
//! which is why the pre-flight exists. Every heal caller — the hook, the
//! pre-flight, the detached heal — runs agentgear's spawn-free pre-check
//! ([`PluginHost::heal_needed_report`]) first: one verdict per configured
//! agent read from CC's registry files beside agentgear's own marker, so a
//! healthy registration spawns no `claude` and a broken, divergent or stale
//! one still heals. Before that pre-check each caller also stands down when
//! this process's own executable was replaced or removed since it started (the
//! path recorded at start now stats to a different inode, length or mtime, or
//! is gone): the current binary — its hooks, its `start`, its daemon — converges
//! the registration instead, and a registration too broken to load fires no
//! hook, so only the current binary's `clauth start`, or a daemon started from
//! it, converges it.
//! The rule is shim-proof (it reads this process's own image, never a PATH
//! entry); a developer's worktree build — its own image unchanged — heals too
//! and converges the registration onto that build's embedded tree. Beside the
//! heal runs the installPath convergence leg: CC records plugin installPaths
//! through the installing session's runtime tree, which dies with the
//! session, so every boundary (pre-flight, detached heal, `self-heal` after a
//! binary install) re-roots the dead ones to their `~/.claude` twins through
//! agentgear's byte-surgical re-point. That leg never gates the heal — a
//! re-point failure still heals, and a healthy registration still re-points.
//!
//! clauth's plugin tree lives in `plugins/` (not the default `plugin/`), so the
//! derive's `tree` attr and `build.rs`'s `assert_plugin_version_at` both name
//! it. The tree itself stays a stock Claude Code plugin — `plugin.json` + the
//! `hooks/` dir — and agentgear supplies the lifecycle around it: materialize
//! the tree, drive `claude plugin marketplace add` + `plugin install`, verify
//! through `plugin list --json`, and stamp a marker self-heal keys on.
//!
//! The `claude`-shelling paths here are the ONLY lifecycle call sites;
//! nothing else in the crate shells out to `claude plugin` (the Services tab's
//! probe reads the registry files directly, and the manual `mcpServers` fallback
//! is a settings write). The lifecycle is pinned hermetically by the
//! fake-`claude` tests in `tests/inline/tui_app.rs` and the self-heal pin in
//! `tests/inline/plugin_host.rs` — both `#[cfg(unix)]` (the fake CLI is a
//! shell shim), so a Windows CI leg does not run them.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use agentgear::{Outcome, PluginHost, Scope, Source};

/// The plugin host for the committed `plugins/` tree. Claude-only: the default
/// `agents` list already names just `claude`, so no agent feature flags beyond
/// the crate defaults (derive + claude + embed) are enabled.
#[derive(PluginHost)]
#[plugin(name = "clauth", tree = "$CARGO_MANIFEST_DIR/plugins")]
pub(crate) struct ClauthPlugin;

/// The Services tab's install fix: a user-scope install from the embedded
/// tree. The single spelling site — the tab's confirm handler and its pin test
/// both go through here, so `Scope::User` + `Source::Embedded` live in one
/// place and the copy-paste hint they replace has no other home to drift into.
pub(crate) fn install() -> anyhow::Result<Outcome> {
    Ok(ClauthPlugin::install(Scope::User, Source::Embedded)?)
}

/// The SessionStart hook body (`clauth self-heal`): the same gated heal the
/// `clauth start` pre-flight runs. agentgear's spawn-free pre-check runs before
/// the heal, so a healthy registration spawns no `claude` and the hook reaches
/// agentgear only when a verdict needs converging (a broken, divergent or stale
/// registration). The heal still never resurrects an uninstall — agentgear's
/// marker gate makes a deliberately removed plugin stay removed — and a repair
/// is worth saying out loud where a healthy session prints nothing. The
/// installPath convergence leg runs beside the heal: a CC install recorded
/// through a dead runtime tree dangles even when clauth's own registration is
/// healthy, so neither leg gates the other.
pub(crate) fn self_heal() -> anyhow::Result<()> {
    let (repoint, heal) = gated_heal();
    // The heal's error wins when both fail: it is the migration trigger the
    // hook exists for. The re-point error still surfaces when the heal
    // succeeded, and rides as context when both failed, so neither leg fails
    // silently.
    match heal {
        Err(e) => match repoint {
            Err(r) => Err(e.context(format!("plugin path re-point also failed: {r:#}"))),
            Ok(()) => Err(e),
        },
        Ok(()) => repoint,
    }
}

/// The shared gated-heal body [`self_heal`] (the hook) and [`preflight`] (the
/// start pre-flight) both run: the installPath convergence leg, then the
/// binary-identity stand-down (a process whose own executable was replaced or
/// removed since it started skips the heal), then the spawn-free pre-check,
/// then the heal leg — each once, and neither leg gates the other (a re-point
/// failure still heals; a heal failure still re-pointed). The two callers
/// differ only in error handling: the hook returns the errors to its caller,
/// `preflight` logs each under its own message and never fails the start.
fn gated_heal() -> (anyhow::Result<()>, anyhow::Result<()>) {
    let repoint = repoint_registry().map(|outcome| outcome.line);
    if let Ok(Some(line)) = &repoint {
        crate::out::outln!("{line}");
    }
    // The re-point leg runs first and is binary-independent, so a process whose
    // own image was replaced or removed still gets its dangling paths
    // re-pointed; only the heal leg stands down, leaving the convergence to the
    // current binary.
    let heal = if heal_stands_down() {
        Ok(())
    } else if preflight_gate() {
        self_heal_line().map(|line| {
            if let Some(line) = line {
                crate::out::outln!("{line}");
            }
        })
    } else {
        Ok(())
    };
    (repoint.map(|_| ()), heal)
}

/// What one registry convergence pass reports. `line` is `Some` when the pass
/// rewrote or named anything; `changed` is true only for rewrites — a named
/// skip moves no bytes, so it reports without counting as a change.
#[derive(Debug)]
pub(crate) struct RepointOutcome {
    pub(crate) line: Option<String>,
    pub(crate) changed: bool,
}

/// The installPath convergence leg. CC records every plugin install's
/// `installPath` through the installing session's runtime tree (the tree's
/// `plugins/` symlinks onto the shared `~/.claude/plugins`, so the files
/// survive while the recorded path dies with the tree). No `claude plugin`
/// command re-spells a recorded path, so this rewrites the dead ones to their
/// `~/.claude` twins through agentgear's byte-surgical re-point: only the
/// mapped values change, the file is never reformatted, a concurrent change
/// restarts the pass, and a deleted registry is refused. A path that still
/// resolves (its tree lives) is left alone — the next pass after the tree
/// dies converges it. Rewrites and skips both name themselves in the line;
/// `changed` separates the two for call sites that rate-limit reporting.
pub(crate) fn repoint_registry() -> anyhow::Result<RepointOutcome> {
    let Ok(clauth) = crate::profile::clauth_dir() else {
        return Ok(RepointOutcome {
            line: None,
            changed: false,
        });
    };
    let Ok(claude) = crate::profile::claude_dir() else {
        return Ok(RepointOutcome {
            line: None,
            changed: false,
        });
    };
    let registry = claude.join("plugins").join("installed_plugins.json");
    // Hoisted: the two prefix spellings are one computation, not one per
    // quoted value the remap is asked about.
    let profiles = clauth.join("profiles");
    let prefix_fwd = format!("{}/", profiles.display());
    let prefix_back = format!("{}\\", profiles.display());
    let report = agentgear::repoint_install_paths(&registry, |path: &str| {
        registry_remap(path, &prefix_fwd, &prefix_back, &claude)
    })?;
    let changed = report.changed();
    if report.rewritten.is_empty() && report.skipped.is_empty() {
        return Ok(RepointOutcome {
            line: None,
            changed,
        });
    }
    // agentgear reports each path as the registry's raw JSON interior (on
    // windows every `\` doubled); the line names the decoded spelling. Only
    // decodable interiors reach the report, since `registry_remap` keeps an
    // undecodable one, so the raw fallback is a spelling of last resort, not a
    // path that fires today.
    let decoded = |raw: &str| json_decode(raw).unwrap_or_else(|| raw.to_string());
    let mut parts = Vec::new();
    for r in &report.rewritten {
        parts.push(format!(
            "re-pointed {} -> {}",
            decoded(&r.from),
            decoded(&r.to)
        ));
    }
    for s in &report.skipped {
        parts.push(format!("left {} ({})", decoded(&s.path), s.reason));
    }
    // The registry is another program's file: a decoded `\u001b` would reach
    // the terminal live, so control and bidi characters print escaped.
    let line = crate::tui::escape_control(&format!("clauth self-heal: {}", parts.join("; ")));
    Ok(RepointOutcome {
        line: Some(line),
        changed,
    })
}

/// Decode the raw interior text agentgear's scanner hands the remap (the bytes
/// between a registry string's quotes) into the value it spells: every JSON
/// escape resolved. `None` when the text is not valid JSON string content —
/// the scanner only skips `\`-escape pairs, so a stray control byte can reach
/// here — and the caller then leaves the bytes alone rather than act on text
/// it cannot read.
fn json_decode(raw: &str) -> Option<String> {
    serde_json::from_str::<String>(&format!("\"{raw}\"")).ok()
}

/// JSON-encode a rewrite so agentgear can splice it verbatim between the
/// registry string's quotes. `serde_json::to_string` of a `&str` cannot fail,
/// so the interior is always the well-formed escape of `value`.
fn json_encode(value: &str) -> String {
    #[allow(
        clippy::expect_used,
        reason = "serde_json serializes any &str; the call cannot fail"
    )]
    let encoded = serde_json::to_string(value).expect("a &str serializes");
    encoded[1..encoded.len() - 1].to_string()
}

/// The remap decision for one recorded path. agentgear hands the remap the
/// registry string's raw JSON interior — on windows every `\` arrives doubled —
/// so matching, the liveness check and the twin build all run on the DECODED
/// value: the raw text never matches the single-`\` prefix spelling, and a raw
/// `\` rewrite written back verbatim would corrupt the JSON. The rewrite goes
/// back JSON-escaped. CC records `\`-spelled paths on windows and `/` on posix,
/// and the leg must converge either — a windows box runs the same
/// dangling-path shape through its symlink tree.
fn registry_remap(
    raw: &str,
    prefix_fwd: &str,
    prefix_back: &str,
    claude: &Path,
) -> agentgear::Remap {
    let Some(path) = json_decode(raw) else {
        return agentgear::Remap::Keep;
    };
    if !path.starts_with(prefix_fwd) && !path.starts_with(prefix_back) {
        return agentgear::Remap::Keep;
    }
    if Path::new(&path).exists() {
        // A live tree still resolves the path today; leave it, it converges
        // the day the tree dies.
        return agentgear::Remap::Keep;
    }
    let Some(suffix) = path
        .split_once("plugins/")
        .map(|(_, s)| s)
        .or_else(|| path.split_once("plugins\\").map(|(_, s)| s))
    else {
        return agentgear::Remap::Skip("no plugins/ segment".to_string());
    };
    // Component-wise: a one-string join carrying the forward-slash suffix
    // renders mixed separators on windows (`\plugins\cache/a/b`), which
    // resolves but is not the native spelling CC records — joining per
    // component yields the canonical per-platform path.
    let twin = suffix
        .split(['/', '\\'])
        .fold(claude.join("plugins"), |path, component| {
            path.join(component)
        });
    if twin.exists() {
        // Escaped: agentgear splices the rewrite verbatim between the quotes,
        // and the twin's backslashes would otherwise corrupt the JSON.
        agentgear::Remap::Rewrite(json_encode(&twin.display().to_string()))
    } else {
        agentgear::Remap::Skip(format!("no twin at {}", twin.display()))
    }
}

/// Skip-only reports are named once per process in the detached leg: the
/// daemon tick would otherwise repeat the same "left X (no twin)" line every
/// tick until a re-login lands the twin. Rewrites always report — each one
/// can only happen once.
static REPORTED_SKIPS: AtomicBool = AtomicBool::new(false);

/// The detached leg's repoint slice: rewrites pass through, a skip-only line
/// passes once per process. Split from [`heal_detached`] so a test can pin
/// the rate limit without a terminal.
fn detached_repoint_line() -> anyhow::Result<Option<String>> {
    let outcome = repoint_registry()?;
    let Some(line) = outcome.line else {
        return Ok(None);
    };
    if outcome.changed || !REPORTED_SKIPS.swap(true, Ordering::Relaxed) {
        Ok(Some(line))
    } else {
        Ok(None)
    }
}

/// Clear the skip-report flag so a test can drive the detached leg fresh. The
/// statics serialize on `HOME_TEST_LOCK`; call under a `HomeSandbox`.
#[cfg(test)]
pub(crate) fn reset_skip_report_for_test() {
    assert!(
        crate::lockorder::holds::<crate::lockorder::rank::HomeTest>(),
        "skip-report statics serialize on `HOME_TEST_LOCK`; call under a `HomeSandbox`"
    );
    REPORTED_SKIPS.store(false, Ordering::Relaxed);
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
/// plugin never loads, so the hook never fires — but both run the same gated
/// heal, so a healthy registration costs the hook no `claude` spawn either.
///
/// A re-point failure is logged under its own message and a heal failure under
/// its own; neither ever fails the start: the session still launches, and the
/// next gated caller (the hook included) retries.
pub(crate) fn preflight() {
    let (repoint, heal) = gated_heal();
    if let Err(e) = repoint {
        crate::logline::logline!("clauth: plugin path re-point failed: {e:#}");
    }
    if let Err(e) = heal {
        crate::logline::logline!("clauth: plugin pre-flight heal failed: {e:#}");
    }
}

/// How long the floor between attempts holds. See [`HealThrottle`].
const HEAL_THROTTLE_MS: u64 = 30 * 60 * 1000;

pub(crate) static HEAL_THROTTLE: HealThrottle = HealThrottle::new();

/// One heal's attempt limiter: at most one attempt per process every
/// [`HEAL_THROTTLE_MS`], success or failure, and never two in flight. Both
/// detached callers (`clauth mcp` boot, daemon tick) can fire once per second
/// on a box whose `claude` is missing, and an untried retry every tick would
/// spawn + fail forever. The in-flight flag bounds overlap; the floor bounds
/// frequency from the attempt STARTING (not finishing), so a slow heal still
/// cannot stack a second one behind it. The claude heal and the herdr heal
/// each hold their own instance, so one healing never defers the other.
/// Fields are `pub(crate)` for the inline test's direct pinning; every writer
/// outside a test goes through [`HealThrottle::claim`].
pub(crate) struct HealThrottle {
    pub(crate) in_flight: AtomicBool,
    pub(crate) last_start_ms: AtomicU64,
}

/// A claimed in-flight flag. The caller builds it on the claiming thread and
/// moves it into the worker, so an early return, a panic, or the worker
/// finishing all clear the flag rather than wedging the throttle shut for the
/// rest of the process.
pub(crate) struct HealClaim<'a>(&'a AtomicBool);

impl Drop for HealClaim<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

impl HealThrottle {
    pub(crate) const fn new() -> Self {
        Self {
            in_flight: AtomicBool::new(false),
            last_start_ms: AtomicU64::new(0),
        }
    }

    /// Claim an attempt at `now_ms`: `None` when one is in flight or the last
    /// attempt started within the floor window. The floor stamps here, on the
    /// claiming thread, so the window between claim and spawn is covered too.
    pub(crate) fn claim(&self, now_ms: u64) -> Option<HealClaim<'_>> {
        if self.in_flight.swap(true, Ordering::AcqRel) {
            return None;
        }
        let claim = HealClaim(&self.in_flight);
        if now_ms.saturating_sub(self.last_start_ms.load(Ordering::Relaxed)) < HEAL_THROTTLE_MS {
            return None;
        }
        self.last_start_ms.store(now_ms, Ordering::Relaxed);
        Some(claim)
    }

    /// Clear both flags so a test can drive a heal fresh. The statics serialize
    /// on `HOME_TEST_LOCK`; call under a `HomeSandbox`.
    #[cfg(all(test, unix))]
    pub(crate) fn reset_for_test(&self) {
        assert!(
            crate::lockorder::holds::<crate::lockorder::rank::HomeTest>(),
            "heal throttle statics serialize on `HOME_TEST_LOCK`; call under a `HomeSandbox`"
        );
        self.in_flight.store(false, Ordering::Release);
        self.last_start_ms.store(0, Ordering::Relaxed);
    }

    /// Stamp the floor at now, so every attempt is refused for the next
    /// window. For a test that drives a caller of the heal (the daemon tick)
    /// and is about something else. The statics serialize on `HOME_TEST_LOCK`;
    /// call under a `HomeSandbox`.
    #[cfg(test)]
    pub(crate) fn arm_for_test(&self) {
        assert!(
            crate::lockorder::holds::<crate::lockorder::rank::HomeTest>(),
            "heal throttle statics serialize on `HOME_TEST_LOCK`; call under a `HomeSandbox`"
        );
        self.in_flight.store(false, Ordering::Release);
        self.last_start_ms
            .store(crate::usage::now_ms(), Ordering::Relaxed);
    }
}

/// The shared detached heal: `clauth mcp` runs it before its stdio handshake and
/// the daemon once per tick. The same stand-down as [`gated_heal`] runs before
/// the gate — a process whose own executable was replaced or removed since it
/// started skips the heal — and then the gate is agentgear's spawn-free
/// pre-check (file reads, no `claude` spawn); only a "heal" verdict spawns
/// anything, and then on its own thread so neither caller is ever blocked by a
/// `claude plugin` spawn.
/// The throttle lives here, not at either call site, so a call site cannot
/// forget it.
///
/// The outcome and any failure go through [`logline!`] — stderr for both callers
/// — never `out::outln!`: `clauth mcp`'s stdout is a JSON-RPC stream, and one
/// stray line corrupts the session.
pub(crate) fn heal_detached() {
    match detached_repoint_line() {
        Ok(Some(line)) => crate::logline::logline!("{line}"),
        Ok(None) => {}
        Err(e) => crate::logline::logline!("clauth: plugin path re-point failed: {e:#}"),
    }
    if heal_stands_down() {
        return;
    }
    if !preflight_gate() {
        return;
    }
    let Some(claim) = HEAL_THROTTLE.claim(crate::usage::now_ms()) else {
        return;
    };
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
         real agentgear tree. Stage a `FakeClaude` beside the `HomeSandbox`, or \
         call `arm_heal_throttle_for_test` if the heal is not what the test is about"
    );
    // Registered on THIS thread, before the spawn, the way the MCP background
    // delegate does it: `join_background_tasks` drains whatever is registered at
    // the moment it runs, so registering inside the worker lets a sandbox
    // teardown clear the home override with a heal still running — which then
    // resolves the operator's REAL `$HOME` and takes real locks under
    // `~/.clauth`.
    #[cfg(test)]
    let done = crate::testutil::register_background_task();
    std::thread::spawn(move || {
        let _claim = claim;
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

/// `unix` because every caller is a `#[cfg(unix)]` test: the heal's fake
/// `claude` is a shell shim. A bare `cfg(test)` gate is dead code on Windows,
/// which `-D warnings` reds there and nowhere else.
#[cfg(all(test, unix))]
pub(crate) fn reset_heal_throttle_for_test() {
    HEAL_THROTTLE.reset_for_test();
}

/// Stamp the floor at now, so [`heal_detached`] refuses every attempt for the
/// next window. For a test that drives a caller of the heal (the daemon tick)
/// and is about something else: the gate's spawn-free pre-check reads agentgear's
/// marker and CC's registry files off the process env (the data dir, `HOME`,
/// `CLAUDE_CONFIG_DIR`), which no sandbox can pin on Windows, so a seeded-healthy
/// registry is a unix-only way to keep such a test spawn-free. Arming the floor
/// is the cross-platform one. Serialized with every other user by `HomeSandbox`'s
/// own lock.
#[cfg(test)]
pub(crate) fn arm_heal_throttle_for_test() {
    HEAL_THROTTLE.arm_for_test();
}

/// Whether a heal caller should run the heal: agentgear's spawn-free pre-check
/// ([`PluginHost::heal_needed_report`]), one verdict per configured agent read
/// from CC's registry files beside agentgear's own marker — never the marker
/// alone, and never a `claude` spawn. `needed()` is `true` when any agent would
/// be acted on (a broken, divergent or stale registration), `false` on a clean
/// install, a skipped (undetected) backend, or a box that holds nothing of ours.
/// An `Err` heals conservatively: the heal is idempotent, and a needless run
/// costs one heal's `claude plugin` spawns.
pub(crate) fn preflight_gate() -> bool {
    ClauthPlugin::heal_needed_report().map_or(true, |r| r.needed())
}

/// A file identity that changes when the file at the path changes. On unix it
/// is `(dev, ino, len, mtime)`, the mtime at full precision: a rename-replace
/// or a delete-and-recreate moves the inode, and an in-place rewrite moves
/// the length or the mtime — a metadata-only touch also reads as replaced,
/// skipping that process's heal and leaving it to the next fresh process.
/// Only a rewrite keeping the length and the exact mtime reads as unchanged.
/// Elsewhere it is `(len, mtime)` at the same precision: a replacement
/// preserving both reads as unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ExecutableIdentity {
    dev_ino: Option<(u64, u64)>,
    len: u64,
    mtime_nanos: u128,
}

impl ExecutableIdentity {
    /// Extract an identity from metadata already read for a regular file.
    fn of_metadata(meta: &std::fs::Metadata) -> Self {
        // Full precision: `cp` onto an existing file keeps the inode, and when
        // it lands in the recorded mtime's own second only the sub-second
        // part moves, which whole-second truncation would read as unchanged.
        let mtime_nanos = meta
            .modified()
            .ok()
            .and_then(|modified| modified.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |since_epoch| since_epoch.as_nanos());
        #[cfg(unix)]
        let dev_ino = {
            use std::os::unix::fs::MetadataExt;
            Some((meta.dev(), meta.ino()))
        };
        #[cfg(not(unix))]
        let dev_ino = None;
        Self {
            dev_ino,
            len: meta.len(),
            mtime_nanos,
        }
    }

    /// The identity of `path` when it is a live regular file.
    #[cfg(any(not(target_os = "linux"), all(test, unix)))]
    fn of_path(path: &Path) -> Option<Self> {
        let meta = std::fs::metadata(path).ok()?;
        if !meta.is_file() {
            return None;
        }
        Some(Self::of_metadata(&meta))
    }

    /// This process's running image identity on linux: `/proc/self/exe` stats
    /// the inode the kernel still runs even after the path has been replaced,
    /// where `current_exe()` would answer "<path> (deleted)".
    #[cfg(target_os = "linux")]
    fn of_proc_self_exe() -> Option<Self> {
        let meta = std::fs::metadata("/proc/self/exe").ok()?;
        Some(Self::of_metadata(&meta))
    }
}

/// How this process's executable changed since it started, read off the path
/// recorded at start.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExeChange {
    /// Something other than the recorded file sits at the path: a new binary,
    /// or anything the stat cannot vouch for as the old one.
    Replaced,
    /// Nothing sits at the path any more (the stat says `NotFound`), the shape
    /// an uninstall leaves.
    Removed,
}

/// This process's executable as recorded at process start: its identity and the
/// path that produced it. The heal-time check stats the path — never
/// `current_exe()` again — so a same-path replacement (new identity) reads as
/// replaced and an empty path as removed.
#[derive(Debug, Clone)]
struct RecordedSelfExe {
    identity: ExecutableIdentity,
    path: PathBuf,
}

/// Record this process's executable identity and its path. On linux the
/// identity comes from `/proc/self/exe` (the running inode, even after a
/// replacement); the path is what `current_exe()` returned at that moment.
/// Elsewhere both come from `current_exe()`. `None` when the read fails; the
/// stand-down then fails open.
fn record_self_exe() -> Option<RecordedSelfExe> {
    #[cfg(target_os = "linux")]
    {
        let path = std::env::current_exe().ok()?;
        let identity = ExecutableIdentity::of_proc_self_exe()?;
        Some(RecordedSelfExe { identity, path })
    }
    #[cfg(not(target_os = "linux"))]
    {
        let path = std::env::current_exe().ok()?;
        let identity = ExecutableIdentity::of_path(&path)?;
        Some(RecordedSelfExe { identity, path })
    }
}

/// This process's own executable, recorded once (lazily here, so a test binary
/// that never runs `main` records on its first heal). `None` when the record
/// failed.
static SELF_EXE: OnceLock<Option<RecordedSelfExe>> = OnceLock::new();

/// Force [`SELF_EXE`] at process start, before any replacement can land, so a
/// heal later compares against the identity this process STARTED as, never a
/// first-use read taken after the replacement. `main` calls this first.
pub(crate) fn record_self_exe_at_start() {
    let _ = SELF_EXE.get_or_init(record_self_exe);
}

fn recorded_self_exe() -> Option<RecordedSelfExe> {
    SELF_EXE.get_or_init(record_self_exe).clone()
}

// The test seam for the stand-down decision: a per-thread pin read first in a
// test build. It injects only the RECORD — the recorded identity + recorded
// path — never a verdict or a "current" identity; the decision always stats
// the recorded path for real, so a test drives "replaced" and "gone" by
// mutating that file. `SelfExeOverride::production()` opts back into the real
// process record; an unpinned caller reads "unchanged" without touching it.
#[cfg(all(test, unix))]
#[derive(Clone)]
enum SelfExeSeam {
    /// A test-injected record: `Some` is the recorded identity + path; `None`
    /// poses "recording failed at start".
    Record(Option<RecordedSelfExe>),
    /// The real process record (the production read).
    Production,
}

#[cfg(all(test, unix))]
thread_local! {
    static SELF_EXE_OVERRIDE: std::cell::RefCell<Option<SelfExeSeam>> =
        const { std::cell::RefCell::new(None) };
}

/// RAII half of the self-exe seam: injects a record (or the production read)
/// until dropped. `!Send` by construction, like [`crate::logline::LogCapture`]:
/// a pin claims the thread that installed it. Test-only and unix-gated because
/// only the unix heal tests drive it.
#[cfg(all(test, unix))]
pub(crate) struct SelfExeOverride {
    prev: Option<SelfExeSeam>,
}

#[cfg(all(test, unix))]
impl SelfExeOverride {
    /// Inject the record read from `recorded` (`None` = "recording failed at
    /// start"). The identity comes from the file itself, and the decision stats
    /// the same path later — so "unchanged" leaves the file, "replaced" is any
    /// real change to it (a rename-replace or an in-place rewrite), and
    /// "gone" deletes it.
    pub(crate) fn pin(recorded: Option<&Path>) -> Self {
        // An unreadable recorded path reads as "recording failed", the same
        // fail-open as the production record read.
        let record = recorded.and_then(|path| {
            ExecutableIdentity::of_path(path).map(|identity| RecordedSelfExe {
                identity,
                path: path.to_path_buf(),
            })
        });
        Self::install(SelfExeSeam::Record(record))
    }

    /// Opt into the real process record (the production read), so the no-seam
    /// test drives `record_self_exe` on this test binary while unpinned callers
    /// default to "unchanged".
    pub(crate) fn production() -> Self {
        Self::install(SelfExeSeam::Production)
    }

    fn install(seam: SelfExeSeam) -> Self {
        let prev = SELF_EXE_OVERRIDE.with(|cell| cell.replace(Some(seam)));
        Self { prev }
    }
}

#[cfg(all(test, unix))]
impl Drop for SelfExeOverride {
    fn drop(&mut self) {
        // `try_with`, so a drop during thread teardown restores nothing rather
        // than panicking the thread it exists to report on.
        let _ = SELF_EXE_OVERRIDE.try_with(|cell| cell.replace(self.prev.take()));
    }
}

/// The seam's read: `None` (no pin) means "unchanged" without touching the
/// process's own record, `Some(None)` means "recording failed", and
/// `Some(Some(record))` is the record the decision stats.
#[cfg(all(test, unix))]
fn seam_record_read() -> Option<Option<RecordedSelfExe>> {
    match SELF_EXE_OVERRIDE.with(|cell| cell.borrow().clone()) {
        None => None,
        Some(SelfExeSeam::Record(record)) => Some(record),
        Some(SelfExeSeam::Production) => Some(recorded_self_exe()),
    }
}

/// The record the stand-down compares against, fed by the test seam or the
/// production read: `Some(None)` is "recording failed at start", `None` (an
/// unpinned test caller only) is "unchanged" without touching the process's
/// own record, so a mid-run rebuild of the test binary cannot stand down
/// unrelated heal tests.
#[cfg(all(test, unix))]
fn self_exe_read() -> Option<Option<RecordedSelfExe>> {
    seam_record_read()
}

#[cfg(not(all(test, unix)))]
fn self_exe_read() -> Option<Option<RecordedSelfExe>> {
    Some(recorded_self_exe())
}

impl RecordedSelfExe {
    /// The one stand-down decision: stat the recorded path for real and compare
    /// its identity. Only a `NotFound` stat is a removal; a directory, a
    /// different identity or any other stat error is a replacement.
    fn change(&self) -> Option<ExeChange> {
        match std::fs::metadata(&self.path) {
            Ok(meta)
                if meta.is_file() && ExecutableIdentity::of_metadata(&meta) == self.identity =>
            {
                None
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Some(ExeChange::Removed),
            Ok(_) | Err(_) => Some(ExeChange::Replaced),
        }
    }
}

/// Whether the heal leg must stand down because this process's own executable
/// was replaced or removed since it started, reporting once per process per
/// kind with the recorded path (the stand-down line for that change) or, when
/// recording failed, the fail-open line once per process. `false` -> heal.
fn heal_stands_down() -> bool {
    match self_exe_read() {
        None => false,
        Some(None) => {
            report_record_failure_once();
            false
        }
        Some(Some(record)) => {
            let Some(change) = record.change() else {
                return false;
            };
            report_stand_down_once(&record.path, change);
            true
        }
    }
}

/// How this process's own executable changed since it started: the heal's
/// stand-down decision asked by a caller that is not a heal leg (the status
/// feed's build, the TUI), so it reports nothing. `None` when unchanged, and
/// when the record failed (the heal's own fail-open).
pub(crate) fn self_exe_change() -> Option<ExeChange> {
    match self_exe_read() {
        Some(Some(record)) => record.change(),
        None | Some(None) => None,
    }
}

/// Whether the heal-leg stand-down was already reported this process, one flag
/// per kind, so each kind's line is said once, not once per caller (a daemon
/// tick would otherwise repeat it every tick). Per kind because a removal can
/// precede a replacement (a Windows self-update leaves the path empty mid-swap),
/// and the replacement's line must still be said.
static REPORTED_STAND_DOWN_REPLACED: AtomicBool = AtomicBool::new(false);
static REPORTED_STAND_DOWN_REMOVED: AtomicBool = AtomicBool::new(false);

/// Report the stand-down once per process per kind, naming the recorded path
/// and the change the heal skipped on.
fn report_stand_down_once(path: &Path, change: ExeChange) {
    let reported = match change {
        ExeChange::Replaced => &REPORTED_STAND_DOWN_REPLACED,
        ExeChange::Removed => &REPORTED_STAND_DOWN_REMOVED,
    };
    if reported.swap(true, Ordering::Relaxed) {
        return;
    }
    match change {
        ExeChange::Replaced => crate::logline::logline!(
            "clauth: skipping the plugin heal because this process is still running the \
             old binary ({})",
            path.display()
        ),
        ExeChange::Removed => crate::logline::logline!(
            "clauth: skipping the plugin heal because this process's binary ({}) was \
             removed",
            path.display()
        ),
    }
}

/// Whether the record-failure (fail-open) note was already reported this
/// process.
static REPORTED_RECORD_FAILURE: AtomicBool = AtomicBool::new(false);

/// Report the record failure once per process: the identity could not be read
/// at start, so the stand-down fails open and the heal still runs.
fn report_record_failure_once() {
    if !REPORTED_RECORD_FAILURE.swap(true, Ordering::Relaxed) {
        crate::logline::logline!("clauth: could not find this process's binary at start");
    }
}

/// Clear the stand-down report so a test can drive the heal leg fresh. The
/// statics serialize on `HOME_TEST_LOCK`; call under a `HomeSandbox`.
#[cfg(all(test, unix))]
pub(crate) fn reset_stand_down_report_for_test() {
    assert!(
        crate::lockorder::holds::<crate::lockorder::rank::HomeTest>(),
        "stand-down report statics serialize on `HOME_TEST_LOCK`; call under a `HomeSandbox`"
    );
    REPORTED_STAND_DOWN_REPLACED.store(false, Ordering::Relaxed);
    REPORTED_STAND_DOWN_REMOVED.store(false, Ordering::Relaxed);
}

/// Clear the record-failure report so a test can drive the fail-open note
/// fresh. The statics serialize on `HOME_TEST_LOCK`; call under a
/// `HomeSandbox`.
#[cfg(all(test, unix))]
pub(crate) fn reset_record_failure_report_for_test() {
    assert!(
        crate::lockorder::holds::<crate::lockorder::rank::HomeTest>(),
        "record-failure report statics serialize on `HOME_TEST_LOCK`; call under a `HomeSandbox`"
    );
    REPORTED_RECORD_FAILURE.store(false, Ordering::Relaxed);
}

#[cfg(test)]
#[path = "../tests/inline/plugin_host.rs"]
mod tests;
