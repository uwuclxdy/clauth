//! Inline tests for `plugin_host`. No environment needed: these pin the
//! compile-time wiring (derive metadata, the embedded tree), the committed
//! SessionStart hook that points at `clauth self-heal`, and the `clauth start`
//! pre-flight gate's registry shapes. The lifecycle itself (the real `claude`
//! CLI as transaction boundary) is pinned hermetically by the fake-claude
//! install test in `tui_app.rs` and exercised for real in the scratch-profile
//! verifies.

use super::ClauthPlugin;
use agentgear::PluginHost;

/// The derive and the one-line `build.rs` are the whole of the agentgear
/// wiring; if either silently broke (name drift, a version guard that stopped
/// pinning, an embed that stopped baking the tree), these fail without
/// spawning the binary.
#[test]
fn derive_metadata_is_wired() {
    assert_eq!(ClauthPlugin::NAME, "clauth");
    assert_eq!(ClauthPlugin::MARKETPLACE, "clauth");
    assert_eq!(ClauthPlugin::AGENTS, &["claude"]);
    // build.rs pins plugins/.claude-plugin/plugin.json `version` to this, so
    // the const equals the crate version.
    assert_eq!(ClauthPlugin::VERSION, env!("CARGO_PKG_VERSION"));

    let descriptor = ClauthPlugin::descriptor();
    assert_eq!(descriptor.name, "clauth");
    assert_eq!(descriptor.id(), "clauth@clauth");
    assert_eq!(descriptor.version, env!("CARGO_PKG_VERSION"));
}

#[test]
fn embedded_tree_is_baked_in() {
    // The `embed` feature compresses `plugins/` into the binary; an empty blob
    // would mean `install(Scope::User, Source::Embedded)` errors at
    // materialize instead of installing.
    assert!(
        !ClauthPlugin::embedded_blob().is_empty(),
        "the plugin tree was not embedded"
    );
}

/// The SessionStart wiring the self-heal rides on: the committed hooks.json
/// must carry BOTH hooks — the profile-change note keeps working, and the new
/// self-heal entry points at the hidden `clauth self-heal` subcommand. A drift
/// here (someone edits hooks.json and drops one command) silently disables a
/// session behavior, which is exactly what this test exists to catch.
#[test]
fn session_start_hook_wires_self_heal_beside_the_note() {
    let hooks: serde_json::Value =
        serde_json::from_str(include_str!("../../plugins/hooks/hooks.json"))
            .expect("plugins/hooks/hooks.json parses");
    let commands: Vec<String> = hooks["hooks"]["SessionStart"]
        .as_array()
        .expect("SessionStart is an array")
        .iter()
        .filter_map(|group| group["hooks"].as_array())
        .flatten()
        .filter_map(|hook| hook["command"].as_str())
        .map(str::to_string)
        .collect();
    assert!(
        commands
            .iter()
            .any(|c| c == "clauth hook-profile-changed-note"),
        "the profile-change note must keep its SessionStart slot: {commands:?}"
    );
    assert!(
        commands.iter().any(|c| c == "clauth self-heal"),
        "the self-heal hook is not wired into SessionStart: {commands:?}"
    );
}

/// The hook's output contract: a line appears only when the heal changed
/// something. Healthy means silent — the planted `if true` mutation that made
/// every heal print reds here — and a real transition (a stale marker cleared
/// after the registration vanished) prints exactly once, in the hook's own
/// wording. Runs hermetically through the fake-`claude` harness in
/// `testutil`.
#[cfg(unix)]
#[test]
fn self_heal_says_nothing_when_healthy_and_reports_changes() {
    use crate::testutil::{ConfigDirSandbox, FakeClaude, HomeSandbox};

    let home = HomeSandbox::new();
    let config = home.home().join(".claude-config");
    std::fs::create_dir_all(&config).expect("config dir");
    let _config = ConfigDirSandbox::new(&home, &config);
    let fake = FakeClaude::new(&home);

    // A fresh install through the same host the TUI fix calls: registry entry
    // present, marker stamped.
    assert!(
        matches!(
            super::install().expect("install"),
            agentgear::Outcome::Installed
        ),
        "the fixture install must land"
    );

    // Healthy + marker present: nothing to say.
    assert_eq!(
        super::self_heal_line().expect("heal"),
        None,
        "a healthy install prints nothing"
    );

    // The registration's backing vanishes (registry entry + shim state), the
    // marker stays: the heal clears the stale marker and says so once.
    std::fs::remove_file(config.join("plugins").join("installed_plugins.json"))
        .expect("remove registry");
    std::fs::remove_file(std::env::var_os("CLAUDE_SHIM_STATE").expect("shim state pin"))
        .expect("remove shim state");
    assert_eq!(
        super::self_heal_line().expect("heal"),
        Some("clauth self-heal: cleared stale marker".to_string()),
        "a heal that changed something says so, in the hook's own wording"
    );

    // Marker cleared + nothing registered: silent again.
    assert_eq!(
        super::self_heal_line().expect("heal"),
        None,
        "after the clear there is nothing left to say"
    );
    let _ = &fake;
}

// ── the start pre-flight gate ─────────────────────────────────────────────

/// One gate fixture: the env harness comes up FIRST (so `expected_pointer`
/// resolves inside the sandbox), the seed closure builds the registry pair
/// against the sandbox's config dir and pointer, then the verdict runs. The
/// gate itself never spawns; the shim is never called.
#[cfg(unix)]
fn gate_verdict(
    seed: impl FnOnce(&std::path::Path, &std::path::Path) -> (serde_json::Value, serde_json::Value),
) -> bool {
    use crate::testutil::{ConfigDirSandbox, FakeClaude, HomeSandbox};

    let home = HomeSandbox::new();
    let claude = home.home().join(".claude-config");
    std::fs::create_dir_all(&claude).expect("config dir");
    let _config = ConfigDirSandbox::new(&home, &claude);
    let _fake = FakeClaude::new(&home);
    let expected = super::expected_pointer().expect("pointer");
    let (marketplaces, plugins) = seed(&claude, &expected);
    let dir = claude.join("plugins");
    std::fs::create_dir_all(&dir).expect("plugins dir");
    for (name, value) in [
        ("known_marketplaces.json", &marketplaces),
        ("installed_plugins.json", &plugins),
    ] {
        std::fs::write(
            dir.join(name),
            serde_json::to_vec_pretty(value).expect("seed json"),
        )
        .expect("seed registry");
    }
    super::preflight_gate()
}

/// The healthy pair: a directory-source `clauth` entry registered exactly at
/// the materialized pointer (created on disk, manifest included) plus a
/// user-scope plugin entry whose files resolve. Every shape test below breaks
/// exactly one half of this and keeps the other.
#[cfg(unix)]
fn healthy_registry(
    _claude: &std::path::Path,
    expected: &std::path::Path,
) -> (serde_json::Value, serde_json::Value) {
    std::fs::create_dir_all(expected.join(".claude-plugin")).expect("pointer tree");
    std::fs::write(
        expected.join(".claude-plugin").join("marketplace.json"),
        "{}",
    )
    .expect("manifest");
    let path = expected.to_string_lossy().into_owned();
    (
        serde_json::json!({
            "clauth": {
                "source": {"source": "directory", "path": path},
                "installLocation": path,
                "lastUpdated": "2026-08-26T00:00:00.000Z"
            }
        }),
        serde_json::json!({
            "version": 2,
            "plugins": {"clauth@clauth": [{"scope": "user", "installPath": path, "version": "0.14.1"}]}
        }),
    )
}

/// The decision's own pin: a healthy registration costs a start no heal, which
/// is what keeps the pre-flight spawn-free on the common path.
#[cfg(unix)]
#[test]
fn preflight_gate_stays_shut_on_a_healthy_registration() {
    let verdict = gate_verdict(healthy_registry);
    assert!(
        !verdict,
        "a registration at the materialized pointer with its manifest present must not heal"
    );
}

/// Every shape the gate exists to catch, one break at a time from the healthy
/// twin: marketplace absent, github-sourced, path diverged, path missing,
/// manifest deleted, plugin installPath dead, plugin entry carrying load errors.
/// Each is the deadlock half of the migration — the plugin then loads 0 hooks
/// and the hook-side heal never fires.
#[cfg(unix)]
#[test]
fn preflight_gate_fires_on_every_broken_or_divergent_shape() {
    let broken = tempfile::tempdir().expect("tempdir");
    let broken_path = broken.path().join("registered");
    std::fs::create_dir_all(broken_path.join(".claude-plugin")).expect("broken dir");
    let broken_path = broken_path.to_string_lossy().into_owned();
    let healthy_plugins =
        |claude: &std::path::Path, expected: &std::path::Path| healthy_registry(claude, expected).1;
    let healthy_mkt =
        |claude: &std::path::Path, expected: &std::path::Path| healthy_registry(claude, expected).0;
    type Seed =
        Box<dyn Fn(&std::path::Path, &std::path::Path) -> (serde_json::Value, serde_json::Value)>;
    let cases: Vec<(&str, Seed)> = vec![
        (
            "marketplace entry absent",
            Box::new(move |claude, expected| {
                (serde_json::json!({}), healthy_plugins(claude, expected))
            }),
        ),
        (
            "marketplace entry github-sourced",
            Box::new(move |claude, expected| {
                (
                    serde_json::json!({"clauth": {"source": {"source": "github", "repo": "uwuclxdy/clauth"}}}),
                    healthy_plugins(claude, expected),
                )
            }),
        ),
        (
            "marketplace path diverged from the pointer",
            Box::new(move |claude, expected| {
                (
                    serde_json::json!({"clauth": {"source": {"source": "directory", "path": "/old/checkout/plugins"}}}),
                    healthy_plugins(claude, expected),
                )
            }),
        ),
        (
            "marketplace path missing",
            Box::new(move |claude, expected| {
                (
                    serde_json::json!({"clauth": {"source": {"source": "directory"}}}),
                    healthy_plugins(claude, expected),
                )
            }),
        ),
        (
            "marketplace manifest deleted",
            Box::new(move |claude, expected| {
                (
                    serde_json::json!({"clauth": {"source": {"source": "directory", "path": broken_path.clone()}}}),
                    healthy_plugins(claude, expected),
                )
            }),
        ),
        (
            "plugin entry installPath dead",
            Box::new(move |claude, expected| {
                (
                    healthy_mkt(claude, expected),
                    serde_json::json!({"version": 2, "plugins": {"clauth@clauth": [{"scope": "user", "installPath": "/gone/runtime/plugins/cache"}]}}),
                )
            }),
        ),
        (
            "plugin entry carries load errors",
            Box::new(move |claude, expected| {
                let healthy_mkt = healthy_mkt(claude, expected);
                let healthy_path = healthy_mkt["clauth"]["source"]["path"]
                    .as_str()
                    .expect("path")
                    .to_string();
                (
                    healthy_mkt,
                    serde_json::json!({
                        "version": 2,
                        "plugins": {"clauth@clauth": [{"scope": "user", "installPath": healthy_path, "errors": ["Marketplace clauth failed to load: cache-miss"]}]}
                    }),
                )
            }),
        ),
    ];
    for (name, seed) in cases {
        assert!(gate_verdict(seed), "{name} must trip the gate");
    }
}

/// A project-scope entry never trips the gate: the heal is user-scope and
/// cannot fix it, so counting it would churn one heal per start for nothing.
#[cfg(unix)]
#[test]
fn preflight_gate_ignores_project_scope_entries() {
    let verdict = gate_verdict(|claude, expected| {
        let (marketplaces, _) = healthy_registry(claude, expected);
        let plugins = serde_json::json!({
            "version": 2,
            "plugins": {"clauth@clauth": [{"scope": "project", "installPath": "/gone/runtime/plugins/cache"}]}
        });
        (marketplaces, plugins)
    });
    assert!(
        !verdict,
        "a dead project entry is not the user-scope heal's job"
    );
}

/// ABSENT registry files are the never-installed box, not "cannot tell": a
/// config dir that has never held a plugin has no `plugins/` directory at all,
/// so a gate keyed only on "parses, names nothing" would heal for exactly the
/// population the carve-out exists for. agentgear converges nothing there (no
/// marker, no entry), so the spawn buys a session-boot cost and no repair.
#[cfg(unix)]
#[test]
fn preflight_gate_stays_shut_when_the_registry_files_are_absent() {
    use crate::testutil::{ConfigDirSandbox, FakeClaude, HomeSandbox};

    let home = HomeSandbox::new();
    let claude = home.home().join(".claude-config");
    std::fs::create_dir_all(&claude).expect("config dir");
    let _config = ConfigDirSandbox::new(&home, &claude);
    let _fake = FakeClaude::new(&home);
    assert!(
        !super::preflight_gate(),
        "no plugins dir at all is a never-installed box, not a broken one"
    );
}

/// UNREADABLE is the other verdict and keeps healing: a truncated or
/// permission-denied registry says only that this pass cannot tell, and the heal
/// is idempotent. The pair is what makes the absent case above safe to carve out.
#[cfg(unix)]
#[test]
fn preflight_gate_heals_when_a_registry_file_is_unparseable() {
    use crate::testutil::{ConfigDirSandbox, FakeClaude, HomeSandbox};

    let home = HomeSandbox::new();
    let claude = home.home().join(".claude-config");
    std::fs::create_dir_all(&claude).expect("config dir");
    let _config = ConfigDirSandbox::new(&home, &claude);
    let _fake = FakeClaude::new(&home);
    let dir = claude.join("plugins");
    std::fs::create_dir_all(&dir).expect("plugins dir");
    std::fs::write(dir.join("known_marketplaces.json"), "{\"clauth\":").expect("truncated");
    std::fs::write(dir.join("installed_plugins.json"), "{}").expect("installed");
    assert!(
        super::preflight_gate(),
        "a registry file this pass cannot parse must heal, conservatively"
    );
}

// ── the detached heal ──────────────────────────────────────────────────────

/// The migration's own first-run gate: a box that never installed the plugin
/// (both registry files parse, neither names `clauth`) must read "nothing to
/// heal", not "heal". A false positive here makes every `clauth mcp` boot and
/// daemon tick spawn `claude plugin list --json` for nothing.
#[cfg(unix)]
#[test]
fn preflight_gate_stays_shut_when_nothing_of_ours_is_registered() {
    let verdict = gate_verdict(|_claude, _expected| {
        (
            serde_json::json!({"other": {"source": {"source": "github", "repo": "x/y"}}}),
            serde_json::json!({"version": 2, "plugins": {"other@other": [{"scope": "user"}]}}),
        )
    });
    assert!(
        !verdict,
        "no clauth marketplace + no clauth@clauth row must not heal"
    );
}

/// The detached heal's "only when the gate says heal" half: a healthy
/// registration makes `heal_detached()` a no-op that spawns no `claude`. This is
/// the shape `clauth mcp` and the daemon hit on a working box.
#[cfg(unix)]
#[test]
fn heal_detached_skips_when_the_gate_says_healthy() {
    use crate::testutil::{ConfigDirSandbox, FakeClaude, HomeSandbox, join_background_tasks};

    // Without this the throttle any earlier test in the process stamped refuses
    // the heal on its own, and the assertion below goes green over a gate that
    // spawns on every call. Tests share one process under `cargo test`.
    let home = HomeSandbox::new();
    super::reset_heal_throttle_for_test();
    let config = home.home().join(".claude-config");
    std::fs::create_dir_all(&config).expect("config dir");
    let _config = ConfigDirSandbox::new(&home, &config);
    let fake = FakeClaude::new(&home);

    let expected = super::expected_pointer().expect("pointer");
    let (marketplaces, plugins) = healthy_registry(&config, &expected);
    let dir = config.join("plugins");
    std::fs::create_dir_all(&dir).expect("plugins dir");
    for (name, value) in [
        ("known_marketplaces.json", &marketplaces),
        ("installed_plugins.json", &plugins),
    ] {
        std::fs::write(
            dir.join(name),
            serde_json::to_vec_pretty(value).expect("seed json"),
        )
        .expect("seed registry");
    }

    super::heal_detached();
    join_background_tasks();
    assert!(
        fake.log().is_empty(),
        "a healthy registration must spawn no claude, got:\n{}",
        fake.log()
    );
}

/// The throttle the detached heal holds so no call site can forget it: one heal
/// may run, and a fresh attempt is refused within the 30-minute floor whether the
/// attempt succeeded or failed. The spawn decision is synchronous (the atomics
/// are set on the calling thread before any worker spawns), so pinning the
/// timestamps is deterministic. The shim log is the second half: it reds if the
/// worker stops reaching the heal at all, which a timestamp assertion alone
/// cannot see.
#[cfg(unix)]
#[test]
fn heal_detached_throttles_to_one_heal_per_window() {
    use std::sync::atomic::Ordering;

    use crate::testutil::{ConfigDirSandbox, FakeClaude, HomeSandbox, join_background_tasks};

    let home = HomeSandbox::new();
    super::reset_heal_throttle_for_test();
    let config = home.home().join(".claude-config");
    std::fs::create_dir_all(&config).expect("config dir");
    let _config = ConfigDirSandbox::new(&home, &config);
    // The worker shells out for real, so the fake `claude` must be on `PATH`
    // before the spawn — without it the heal reaches the operator's own binary.
    let fake = FakeClaude::new(&home);

    // Broken: a user-scope row whose installPath is gone, so the gate says heal.
    let dir = config.join("plugins");
    std::fs::create_dir_all(&dir).expect("plugins dir");
    std::fs::write(dir.join("known_marketplaces.json"), "{}").expect("marketplaces");
    std::fs::write(
        dir.join("installed_plugins.json"),
        serde_json::to_vec(
            &serde_json::json!({
                "version": 2,
                "plugins": {"clauth@clauth": [{"scope": "user", "installPath": "/gone/runtime/plugins/cache"}]}
            }),
        )
        .expect("seed json"),
    )
    .expect("installed");

    // First attempt: the spawn path is entered, so the floor timestamp advances.
    super::heal_detached();
    assert!(
        super::HEAL_THROTTLE.last_start_ms.load(Ordering::Relaxed) > 0,
        "the first heal must enter the spawn path"
    );
    join_background_tasks();
    let first_start = super::HEAL_THROTTLE.last_start_ms.load(Ordering::Relaxed);
    let after_first = fake.log();
    assert!(
        !after_first.is_empty(),
        "the worker must actually reach the heal and spawn `claude`"
    );
    // Advance past the millisecond resolution of `now_ms` so a broken floor
    // (which would re-stamp the timestamp) can't hide behind a same-ms collision.
    std::thread::sleep(std::time::Duration::from_millis(5));

    // A fresh attempt within the window is refused before spawning: the floor
    // timestamp does not advance.
    super::heal_detached();
    assert_eq!(
        super::HEAL_THROTTLE.last_start_ms.load(Ordering::Relaxed),
        first_start,
        "the 30-minute floor must refuse a fresh attempt"
    );
    join_background_tasks();
    assert_eq!(
        fake.log(),
        after_first,
        "a refused attempt must spawn nothing, got:\n{}",
        fake.log()
    );

    // An in-flight heal also refuses. The floor is CLEARED first, so the flag is
    // the only guard left standing: with the floor still armed, disabling the
    // swap changes nothing and this block pins nothing.
    super::HEAL_THROTTLE
        .last_start_ms
        .store(0, Ordering::Relaxed);
    super::HEAL_THROTTLE
        .in_flight
        .store(true, Ordering::Release);
    super::heal_detached();
    assert_eq!(
        super::HEAL_THROTTLE.last_start_ms.load(Ordering::Relaxed),
        0,
        "an in-flight heal must refuse before it stamps the floor"
    );
    join_background_tasks();
    assert_eq!(
        fake.log(),
        after_first,
        "an in-flight heal must spawn nothing, got:\n{}",
        fake.log()
    );
    super::HEAL_THROTTLE
        .in_flight
        .store(false, Ordering::Release);
}

/// The reset mutates process-global statics that serialize on
/// `HOME_TEST_LOCK`, so it must refuse to run with no sandbox held.
#[cfg(all(unix, debug_assertions))]
#[test]
#[should_panic(expected = "call under a `HomeSandbox`")]
fn reset_heal_throttle_without_a_sandbox_panics() {
    super::reset_heal_throttle_for_test();
}

/// The committed root marketplace manifest must stay agentgear-conformant, or a
/// directory-source install silently drifts. The marketplace name is the install
/// key, the single entry's `source` names the embedded tree, and — agentgear
/// versioning rule 1 — a marketplace entry must NOT carry its own `version`
/// (that would mask drift in `plugins/.claude-plugin/plugin.json`, the one
/// version that must equal the crate).
#[test]
fn committed_root_marketplace_matches_agentgear_rules() {
    let marketplace: serde_json::Value =
        serde_json::from_str(include_str!("../../.claude-plugin/marketplace.json"))
            .expect("marketplace.json parses");
    assert_eq!(marketplace["name"].as_str(), Some("clauth"));
    let plugins = marketplace["plugins"]
        .as_array()
        .expect("plugins is an array");
    assert_eq!(plugins.len(), 1, "exactly one marketplace entry");
    let entry = &plugins[0];
    assert_eq!(entry["name"].as_str(), Some("clauth"));
    assert_eq!(entry["source"].as_str(), Some("./plugins"));
    assert!(
        entry.get("version").is_none(),
        "a marketplace-entry version would mask drift (agentgear versioning rule 1)"
    );

    let plugin: serde_json::Value =
        serde_json::from_str(include_str!("../../plugins/.claude-plugin/plugin.json"))
            .expect("plugin.json parses");
    assert_eq!(plugin["name"].as_str(), Some("clauth"));
    assert_eq!(plugin["version"].as_str(), Some(env!("CARGO_PKG_VERSION")));
}
