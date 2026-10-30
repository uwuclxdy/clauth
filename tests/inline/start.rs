#![allow(clippy::unwrap_used, clippy::expect_used)]
use super::*;
use std::fs;
use std::process::Command;

use crate::profile::{AppState, Profile};
use crate::runtime::SwapUnsupported;
use crate::testutil::HomeSandbox;

#[cfg(unix)]
fn signal_status(signal: i32) -> ExitStatus {
    use std::os::unix::process::ExitStatusExt;

    ExitStatus::from_raw(signal)
}

#[test]
fn status_code_preserves_plain_exit_code() {
    // A stock Windows PATH has no `sh` (it comes with Git Bash).
    #[cfg(not(windows))]
    let (program, args) = ("sh", ["-c", "exit 7"]);
    #[cfg(windows)]
    let (program, args) = ("cmd", ["/D", "/C", "exit 7"]);
    let status = Command::new(program).args(args).status().expect("status");

    assert_eq!(status_code(status, None), 7);
}

#[cfg(unix)]
#[test]
fn status_code_preserves_child_signal_code() {
    assert_eq!(status_code(signal_status(15), None), 143);
}

#[cfg(unix)]
#[test]
fn status_code_reports_parent_signal_after_successful_child_exit() {
    let status = Command::new("sh")
        .args(["-c", "exit 0"])
        .status()
        .expect("status");

    assert_eq!(status_code(status, Some(SIGINT)), 130);
}

// ── apply_spawn_cwd: the resume-cwd primitive (part A) ──

/// `Some(workspace)` pins the child's cwd to that dir — the load-bearing resume
/// guarantee: the resumed `claude` runs in the session's recorded workspace.
#[test]
fn apply_spawn_cwd_pins_child_to_workspace() {
    let mut cmd = Command::new("true");
    let ws = std::path::Path::new("/tmp/clauth-resume-ws");
    let resolved = apply_spawn_cwd(&mut cmd, Some(ws));
    assert_eq!(
        cmd.get_current_dir(),
        Some(ws),
        "the child cwd must equal the recorded workspace"
    );
    assert_eq!(resolved.as_deref(), Some(ws));
}

/// `None` sets no explicit cwd, so the child inherits this process's — byte-for-
/// byte the pre-resume behavior. `get_current_dir` is `None` (unset) in that case.
#[test]
fn apply_spawn_cwd_none_inherits_process_cwd() {
    let mut cmd = Command::new("true");
    let resolved = apply_spawn_cwd(&mut cmd, None);
    assert_eq!(
        cmd.get_current_dir(),
        None,
        "None must not pin the child's cwd"
    );
    assert_eq!(resolved, std::env::current_dir().ok());
}

// ── rescue: the isolated-store teardown every isolated run gets ──

/// A bare `clauth start --isolated`, with no flag and no config key asked for,
/// lifts both legs into the global store: the transcript becomes resumable
/// (mirrored `<slug>/<id>.jsonl`) and the session sidecars follow it, each moved
/// rather than copied — the isolated tree is discarded right after.
#[test]
fn an_isolated_teardown_moves_the_session_into_the_global_store() {
    let sb = HomeSandbox::new();
    let iso = sb.home().join(".clauth/profiles/iso/runtime-isolated");
    let claude_home = sb.home().join(".claude");
    let src = iso.join("projects/-w-iso/s1.jsonl");
    fs::create_dir_all(src.parent().unwrap()).unwrap();
    fs::write(&src, "transcript").unwrap();
    let snap = iso.join("shell-snapshots/snap.sh");
    fs::create_dir_all(snap.parent().unwrap()).unwrap();
    fs::write(&snap, "iso shell").unwrap();

    assert_eq!(
        teardown(&iso, &claude_home),
        (1, 1),
        "an isolated run rescues both legs with nothing asked for"
    );

    let landed = claude_home.join("projects/-w-iso/s1.jsonl");
    assert_eq!(
        fs::read(&landed).unwrap(),
        b"transcript",
        "the transcript lands in the resumable global store"
    );
    assert!(!src.exists(), "source moved, not copied");
    assert_eq!(
        fs::read_to_string(claude_home.join("shell-snapshots/snap.sh")).unwrap(),
        "iso shell",
        "the sidecar state a resumed session needs follows it"
    );
    assert!(!snap.exists(), "the sidecar moved too");
}

/// `run`'s rescue leg cannot be driven without a real `claude` child, which this
/// crate never fakes, so it is pinned over the source: the teardown tests own
/// whether the move works, and this owns what stands between the stamp leg and
/// the end of the teardown region.
///
/// An EQUALITY rather than a `contains`, because a needle that only looks for the
/// condition is blind to every spelling that leaves the condition's own bytes
/// intact — a shadowing `let isolated = isolated && <key>;` hoisted above it, an
/// outer `if` wrapping the leg, an early return between the two. Each disables
/// the rescue for a stock user. Measured over the real source text: the
/// `contains` form this replaced caught 2 of 5 mutation spellings, the equality
/// catches 5 of 5 and does not red the code as it stands.
///
/// What it cannot decide, stated because a scan reads stronger than it is: an
/// early `return` or `?` placed BEFORE the stamp leg, i.e. above this window. A
/// window pin never sees above its own window, and nothing else here covers it.
#[test]
fn the_start_teardown_tail_is_the_rescue_leg_gated_on_isolation_alone() {
    let src = include_str!("../../src/start.rs");
    let body = src
        .split_once("pub(crate) fn run(")
        .expect("run is defined")
        .1;
    // Bounded at the drop that discards the tree, i.e. the end of the teardown
    // legs: past it lies the exit-code tail this test has no business reading.
    let legs = body
        .split_once("drop(runtime);")
        .expect("the runtime is dropped after the teardown legs")
        .0;
    let dense: String = legs
        .lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .flat_map(str::chars)
        .filter(|c| !c.is_whitespace())
        .collect();
    let tail = dense
        .rsplit_once("run_start);}")
        .expect("the stamp leg precedes the rescue leg")
        .1;
    assert_eq!(
        tail,
        "ifisolated&&letOk(claude_home)=crate::profile::claude_dir(){\
         rescue_teardown(runtime.config_dir(),runtime.sessions_dir(),&claude_home);}",
        "between the stamp leg and the end of the teardown region there must be \
         the rescue leg and nothing else, gated on the isolation alone"
    );
}

/// The production teardown plus the sessions dir it reads, so a change to either
/// shows up here. A fresh sessions dir holds one live marker: this session's.
fn teardown(iso_root: &std::path::Path, claude_home: &std::path::Path) -> (usize, usize) {
    let sessions = iso_root.with_file_name("sessions-isolated");
    fs::create_dir_all(&sessions).unwrap();
    let _self_marker = live_marker(&sessions.join("1234-0"));
    rescue_teardown(iso_root, &sessions, claude_home)
}

/// A live session's liveness marker: an open file holding the same exclusive
/// flock `ProfileRuntime::acquire` takes, so `live_sessions_at` counts it alive
/// (a second fd's `try_lock` conflicts even within one process). The returned
/// handle must stay in scope.
fn live_marker(path: &std::path::Path) -> fs::File {
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .unwrap();
    file.lock().unwrap();
    file
}

/// The guard's `> 1` arm as defence in depth: a same-sid collision or a legacy
/// shared dir leaves a sibling's live marker in the sessions dir, and an exit
/// while a sibling is still live must move nothing. Rescuing `shell-snapshots/`
/// out from under a running Claude Code would break its Bash tool mid-session.
/// A normal isolated session owns its dir, so the `None` arm is the other path
/// to the refusal: it fires when the marker dir could not be read.
#[test]
fn rescue_moves_nothing_while_a_sibling_session_is_live() {
    let sb = HomeSandbox::new();
    let iso = sb.home().join(".clauth/profiles/iso/runtime-isolated");
    let claude_home = sb.home().join(".claude");
    let sessions = sb.home().join(".clauth/profiles/iso/sessions-isolated");
    fs::create_dir_all(&sessions).unwrap();
    fs::create_dir_all(iso.join("shell-snapshots")).unwrap();
    fs::write(iso.join("shell-snapshots/snap.sh"), "live shell").unwrap();
    fs::create_dir_all(iso.join("projects/-w-iso")).unwrap();
    fs::write(iso.join("projects/-w-iso/s1.jsonl"), "transcript").unwrap();
    let _me = live_marker(&sessions.join("1234-0"));
    let _sibling = live_marker(&sessions.join("5678-0"));

    assert_eq!(
        rescue_teardown(&iso, &sessions, &claude_home),
        (0, 0),
        "a live sibling blocks both legs"
    );
    assert_eq!(
        fs::read_to_string(iso.join("shell-snapshots/snap.sh")).unwrap(),
        "live shell",
        "the live session's state stays where it is reading it"
    );
    assert!(iso.join("projects/-w-iso/s1.jsonl").exists());
    assert!(!claude_home.join("shell-snapshots").exists());
    assert!(!claude_home.join("projects").exists());

    // The sibling exits: the last session out rescues both legs.
    drop(_sibling);
    fs::remove_file(sessions.join("5678-0")).unwrap();
    assert_eq!(rescue_teardown(&iso, &sessions, &claude_home), (1, 1));
}

/// A sidecar entry that cannot move (its global parent is occupied by a FILE)
/// is logged and skipped: teardown still completes, the rest of the sidecars
/// move, and the transcript leg's result is untouched.
#[test]
fn sidecar_failure_leaves_teardown_and_transcript_rescue_intact() {
    let sb = HomeSandbox::new();
    let iso = sb.home().join(".clauth/profiles/iso/runtime-isolated");
    let claude_home = sb.home().join(".claude");
    fs::create_dir_all(iso.join("projects/-w-iso")).unwrap();
    fs::write(iso.join("projects/-w-iso/s1.jsonl"), "transcript").unwrap();
    fs::create_dir_all(iso.join("file-history/sess-a")).unwrap();
    fs::write(iso.join("file-history/sess-a/edit-1.json"), "blocked").unwrap();
    fs::write(iso.join("file-history/ok.json"), "moves").unwrap();
    // A regular file where the rescue needs a directory: every move under it
    // fails at `create_dir_all`.
    fs::create_dir_all(claude_home.join("file-history")).unwrap();
    fs::write(claude_home.join("file-history/sess-a"), "in the way").unwrap();

    let (transcripts, sidecars) = teardown(&iso, &claude_home);

    assert_eq!(
        (transcripts, sidecars),
        (1, 1),
        "only the blocked entry fails"
    );
    assert_eq!(
        fs::read_to_string(claude_home.join("projects/-w-iso/s1.jsonl")).unwrap(),
        "transcript",
        "the transcript rescue's result stands"
    );
    assert_eq!(
        fs::read_to_string(claude_home.join("file-history/ok.json")).unwrap(),
        "moves"
    );
    assert_eq!(
        fs::read_to_string(claude_home.join("file-history/sess-a")).unwrap(),
        "in the way",
        "the blocking file is never replaced"
    );
    assert!(
        iso.join("file-history/sess-a/edit-1.json").exists(),
        "a failed move leaves its source in place, to be discarded"
    );
}

// ── `--with-fallback` eligibility ───────────────────────────────────────────

/// A config eligible for `--with-fallback` in every respect: OAuth, enabled, and
/// a fallback-chain member with somewhere to go. Each refusal below breaks exactly
/// ONE of those and keeps the passing twin, so an assertion cannot pass because
/// the setup was wrong and the call refused for some other reason.
///
/// The chain carries a SECOND member on purpose: a chain whose only entry is this
/// profile is one the chain walk can never move off, so building the eligible twin
/// that way would make every positive control a setup that ships the very defect
/// the gates exist to refuse.
fn chain_ready_config(name: &str) -> AppConfig {
    AppConfig {
        state: AppState {
            fallback_chain: vec![name.into(), "spare".into()],
            ..AppState::default()
        },
        profiles: vec![
            Profile::new(name.to_string(), None, None),
            Profile::new("spare".to_string(), None, None),
        ],
    }
}

/// The profile dir a `--with-fallback` start's transport probe would materialize.
/// Nothing else in these tests creates it, so its absence is what proves the probe
/// never ran.
fn profile_dir_of(name: &str) -> std::path::PathBuf {
    crate::profile::profile_dir(&crate::profile::ProfileName::from(name)).expect("profile dir")
}

/// Assert the eligible twin cleared every gate under test — the twin each refusal
/// below keeps to prove it refused for the reason it names and not for a broken
/// setup.
///
/// On a host that shares one runtime tree across a profile's sessions (Windows
/// outside Developer Mode, which probes into `LinkMode::Fake`) the twin is refused
/// anyway, by the transport probe. That probe runs LAST, after every gate these
/// tests break one at a time, so reaching it concedes exactly what `Ok` concedes
/// about the gates above — and demanding `Ok` there asserts the right thing about
/// the wrong host. Nothing is weakened where the swap is supported: the probe
/// answers `None` and this is the `expect` it replaced.
///
/// Call it only on the twin, never on a refusal under test: it reads the transport
/// probe, which materializes the profile dir.
#[track_caller]
fn the_eligible_twin_clears_every_gate(verdict: Result<()>, name: &str) {
    match crate::runtime::unsupported_swap_transport(&crate::profile::ProfileName::from(name))
        .expect("probe the transport")
    {
        None => {
            verdict.expect("the eligible twin must clear every gate");
        }
        Some(why) => assert_eq!(
            verdict
                .expect_err("a shared-tree host refuses the twin at the transport probe")
                .to_string(),
            unsupported_host_refusal(&crate::profile::ProfileName::from(name), why),
            "the twin must reach the transport probe and be refused only there"
        ),
    }
}

/// The unsupported-host refusal copy, as a literal. `LinkMode::Fake` is
/// unreachable through the gate on a real-symlink host, so the render is pinned
/// here and the wiring by the test below it.
#[test]
fn the_unsupported_host_refusal_names_each_cause() {
    assert_eq!(
        unsupported_host_refusal(
            &crate::profile::ProfileName::from("acme"),
            SwapUnsupported::SharedRuntimeTree
        ),
        "'acme': --with-fallback needs a per-session credential swap, but this host \
         shares one runtime tree across the profile's sessions; start without it"
    );
}

/// The freshness gate the decision leg runs reads only the OAuth store, which is
/// sound ONLY because a third-party-launched session gets a chain the walk cannot
/// move it off. Without this refusal such a session opts in and then silently
/// never follows anything.
#[test]
fn with_fallback_refuses_a_non_oauth_profile() {
    let _sb = HomeSandbox::new();
    let _daemon = crate::daemon::hold_daemon_lock();
    let mut third_party = chain_ready_config("thirdparty");
    third_party.profiles[0].base_url = Some("https://api.example.com".to_string());
    let profile = third_party
        .find(&crate::profile::ProfileName::from("thirdparty"))
        .expect("fixture profile");

    let err = refuse_unless_chain_eligible(&third_party, profile, Isolation::Shared)
        .expect_err("a custom endpoint must refuse");
    assert_eq!(
        err.to_string(),
        "'thirdparty': --with-fallback needs an OAuth account, but this one carries \
         a custom endpoint; start without it"
    );

    let oauth = chain_ready_config("thirdparty");
    let profile = oauth
        .find(&crate::profile::ProfileName::from("thirdparty"))
        .expect("fixture profile");
    the_eligible_twin_clears_every_gate(
        refuse_unless_chain_eligible(&oauth, profile, Isolation::Shared),
        "thirdparty",
    );
}

/// A session's chain snapshot returns `None` for a member outside the chain, so
/// the row would be skipped every tick with nothing said. The flag names the fix
/// instead of shipping a silent no-op.
#[test]
fn with_fallback_refuses_a_profile_outside_the_fallback_chain() {
    let _sb = HomeSandbox::new();
    let _daemon = crate::daemon::hold_daemon_lock();
    let mut loner = chain_ready_config("loner");
    loner.state.fallback_chain.clear();
    let profile = loner
        .find(&crate::profile::ProfileName::from("loner"))
        .expect("fixture profile");

    let err = refuse_unless_chain_eligible(&loner, profile, Isolation::Shared)
        .expect_err("a non-member must refuse");
    assert_eq!(
        err.to_string(),
        "'loner': --with-fallback needs a fallback-chain member; add 'loner' on the \
         fallback tab, or start without it"
    );

    let member = chain_ready_config("loner");
    let profile = member
        .find(&crate::profile::ProfileName::from("loner"))
        .expect("fixture profile");
    the_eligible_twin_clears_every_gate(
        refuse_unless_chain_eligible(&member, profile, Isolation::Shared),
        "loner",
    );
}

/// Nothing writes a session's `intended_member` but the daemon's decision leg, so
/// without a daemon the flag is inert and the session sits on its launch account
/// with nothing saying so. Refused rather than auto-spawned: a detached daemon
/// started behind the user's back is a process they never asked for.
#[test]
fn with_fallback_refuses_when_no_daemon_is_running() {
    let _sb = HomeSandbox::new();
    let config = chain_ready_config("undaemoned");
    let profile = config
        .find(&crate::profile::ProfileName::from("undaemoned"))
        .expect("fixture profile");

    let err = refuse_unless_chain_eligible(&config, profile, Isolation::Shared)
        .expect_err("no daemon must refuse");
    assert_eq!(
        err.to_string(),
        "'undaemoned': --with-fallback needs a running daemon to decide switches, \
         run `clauth daemon`"
    );

    let _daemon = crate::daemon::hold_daemon_lock();
    the_eligible_twin_clears_every_gate(
        refuse_unless_chain_eligible(&config, profile, Isolation::Shared),
        "undaemoned",
    );
}

/// `singleton_held` separates "nobody there" from "can't tell" precisely so a
/// decision path can refuse on the second. A host whose lock file cannot be read
/// cannot run the decider on it either, so allowing the flag there ships exactly
/// the silent non-switch the gate exists to prevent.
#[test]
fn with_fallback_refuses_when_the_daemon_lock_cannot_be_read() {
    let _sb = HomeSandbox::new();
    // A directory where the lock file belongs: `open` fails EISDIR, which is the
    // "can't tell" `singleton_held` reports as an error rather than a `no`.
    let lock_path = crate::daemon::daemon_lock_path();
    fs::create_dir_all(&lock_path).expect("mkdir over the lock path");
    let config = chain_ready_config("unreadable");
    let profile = config
        .find(&crate::profile::ProfileName::from("unreadable"))
        .expect("fixture profile");

    let err = refuse_unless_chain_eligible(&config, profile, Isolation::Shared)
        .expect_err("an unreadable daemon lock must refuse");
    assert_eq!(
        err.to_string(),
        "'unreadable': --with-fallback needs a running daemon and this host could \
         not be checked for one"
    );

    fs::remove_dir(&lock_path).expect("clear the lock path");
    let _daemon = crate::daemon::hold_daemon_lock();
    the_eligible_twin_clears_every_gate(
        refuse_unless_chain_eligible(&config, profile, Isolation::Shared),
        "unreadable",
    );
}

/// A chain whose only member is this profile is accepted by a bare membership
/// check and then cannot move: `next_auto_switch_target` has nowhere to point, so
/// the leg writes nothing and the session stays put every tick. Same user-visible
/// silence as the non-member case, which is why it gets the same refusal.
#[test]
fn with_fallback_refuses_a_chain_with_nowhere_to_go() {
    let _sb = HomeSandbox::new();
    let _daemon = crate::daemon::hold_daemon_lock();
    let mut lone = chain_ready_config("onlyone");
    lone.state
        .fallback_chain
        .retain(|n| n.as_str() == "onlyone");
    let profile = lone
        .find(&crate::profile::ProfileName::from("onlyone"))
        .expect("fixture profile");

    let err = refuse_unless_chain_eligible(&lone, profile, Isolation::Shared)
        .expect_err("a chain of one must refuse");
    assert_eq!(
        err.to_string(),
        "'onlyone': --with-fallback needs a second account in the fallback chain to \
         move to; add one on the fallback tab, or start without it"
    );

    let paired = chain_ready_config("onlyone");
    let profile = paired
        .find(&crate::profile::ProfileName::from("onlyone"))
        .expect("fixture profile");
    the_eligible_twin_clears_every_gate(
        refuse_unless_chain_eligible(&paired, profile, Isolation::Shared),
        "onlyone",
    );
}

/// `--isolated` is refused by clap, which is where the user meets it. But `run` is
/// the authoritative chokepoint every session-spawn path inherits, and
/// `chain_opt_in_survives` drops an isolated opt-in SILENTLY — so a new caller
/// passing both would get exactly the no-op the other gates exist to prevent.
#[test]
fn with_fallback_refuses_an_isolated_session() {
    let _sb = HomeSandbox::new();
    let _daemon = crate::daemon::hold_daemon_lock();
    let config = chain_ready_config("throwaway");
    let profile = config
        .find(&crate::profile::ProfileName::from("throwaway"))
        .expect("fixture profile");

    let err = refuse_unless_chain_eligible(&config, profile, Isolation::Isolated)
        .expect_err("an isolated session must refuse");
    assert_eq!(
        err.to_string(),
        "'throwaway': --with-fallback cannot be combined with --isolated, since an \
         isolated session follows no chain"
    );

    the_eligible_twin_clears_every_gate(
        refuse_unless_chain_eligible(&config, profile, Isolation::Shared),
        "throwaway",
    );
}

/// Every gate that can answer without the disk runs BEFORE the transport probe,
/// which is the only leg that writes. So a start refused for a cause the user can
/// act on never materializes a profile dir for an account that never launched.
#[test]
fn a_refused_with_fallback_start_never_probes_the_disk() {
    let _sb = HomeSandbox::new();
    // No daemon held: the last pure gate refuses.
    let config = chain_ready_config("untouched");
    let profile = config
        .find(&crate::profile::ProfileName::from("untouched"))
        .expect("fixture profile");
    let err = refuse_unless_chain_eligible(&config, profile, Isolation::Shared)
        .expect_err("no daemon must refuse");
    // WHICH gate refused is the whole subject here: a fixture that drifted into
    // refusing at the oauth or membership gate would leave the dir absent too and
    // stop proving that the LAST pure gate still precedes the probe.
    assert_eq!(
        err.to_string(),
        "'untouched': --with-fallback needs a running daemon to decide switches, \
         run `clauth daemon`"
    );
    assert!(
        !profile_dir_of("untouched").exists(),
        "a refusal the user can act on must not create the profile dir"
    );

    // The eligible path DOES probe — otherwise the assertions above pass for a
    // gate that simply never runs the probe at all. Read off
    // `refuse_unless_chain_eligible` alone: `the_eligible_twin_clears_every_gate`
    // reads the transport probe itself, which materializes the dir, so routing
    // this control through it would assert against the helper's own side effect.
    // Past every pure gate the only refusal left is the probe's own, and it
    // materializes the dir before it refuses, so the dir is there either way.
    let _daemon = crate::daemon::hold_daemon_lock();
    match refuse_unless_chain_eligible(&config, profile, Isolation::Shared) {
        Ok(()) => {}
        Err(e) => assert_eq!(
            e.to_string(),
            unsupported_host_refusal(
                &crate::profile::ProfileName::from("untouched"),
                SwapUnsupported::SharedRuntimeTree
            ),
            "a host that cleared every pure gate can only be refused by the probe"
        ),
    }
    assert!(
        profile_dir_of("untouched").is_dir(),
        "the transport probe runs once everything else has cleared"
    );
}

/// The gate has to be WIRED into the one chokepoint every session-spawn path
/// funnels through, and only for a session that asked for the chain. Both halves
/// stop before `claude` is spawned, so the errors are what tells them apart:
/// the opted-in run dies on its own refusal, the bare one gets all the way to
/// `acquire` and dies on the missing `~/.claude` this sandbox has no business
/// creating.
#[test]
fn run_applies_the_chain_gate_only_to_an_opted_in_start() {
    let _sb = HomeSandbox::new();
    let _daemon = crate::daemon::hold_daemon_lock();
    let mut loner = chain_ready_config("wired");
    loner.state.fallback_chain.clear();

    let err = run(
        &loner,
        &crate::profile::ProfileName::from("wired"),
        &[],
        Isolation::Shared,
        None,
        true,
        None,
    )
    .expect_err("an opted-in start must be gated");
    assert_eq!(
        err.to_string(),
        "'wired': --with-fallback needs a fallback-chain member; add 'wired' on the \
         fallback tab, or start without it"
    );

    let err = run(
        &loner,
        &crate::profile::ProfileName::from("wired"),
        &[],
        Isolation::Shared,
        None,
        false,
        None,
    )
    .expect_err("the sandbox has no ~/.claude to launch against");
    assert_eq!(
        err.to_string(),
        "~/.claude not found; install Claude Code first",
        "a bare start must skip the gate entirely"
    );
}

// ── the plugin pre-flight ─────────────────────────────────────────────────

/// The migration trigger's own pin: a `clauth start` heals a broken plugin
/// registration before `claude` launches, and a healthy one costs no `claude
/// plugin` spawn at all. Both halves drive the real `run` chokepoint against
/// the fake-`claude` harness, so the shim's call log is the evidence.
#[cfg(unix)]
#[test]
fn start_heals_the_plugin_registry_only_when_it_is_broken() {
    use crate::testutil::{ConfigDirSandbox, FakeClaude, HomeSandbox};

    let sb = HomeSandbox::new();
    let claude = sb.home().join(".claude");
    std::fs::create_dir_all(&claude).expect("~/.claude");
    let _config = ConfigDirSandbox::new(&sb, &claude);
    let fake = FakeClaude::new(&sb);
    // `acquire` re-reads the account record from disk under the state flock, so
    // the sandbox needs the profile the run starts under persisted the way
    // `load_config` walks it.
    let clauth_dir = crate::profile::clauth_dir().expect("clauth dir");
    std::fs::create_dir_all(&clauth_dir).expect("clauth dir");
    std::fs::write(
        clauth_dir.join("profiles.toml"),
        "profiles = [\"wired\", \"spare\"]\n",
    )
    .expect("profiles.toml");
    let config = chain_ready_config("wired");

    // Healthy: the shared fixture drives a real install and seeds the registry
    // files CC writes that the fake CLI does not.
    crate::testutil::healthy_install(&claude);

    let baseline = fake.log();
    run(
        &config,
        &crate::profile::ProfileName::from("wired"),
        &[],
        Isolation::Shared,
        None,
        false,
        None,
    )
    .expect("healthy start");
    let after = fake.log();
    let grown = after
        .strip_prefix(baseline.as_str())
        .expect("the shim log only appends");
    assert!(
        !grown.contains("plugin"),
        "a healthy start must spawn no claude plugin calls, got:\n{grown}"
    );

    // Broken: the marketplace registration vanishes, and the next start heals
    // it through the lifecycle before claude launches.
    let plugins_dir = claude.join("plugins");
    std::fs::remove_file(plugins_dir.join("known_marketplaces.json")).expect("remove registry");
    let baseline = fake.log();
    run(
        &config,
        &crate::profile::ProfileName::from("wired"),
        &[],
        Isolation::Shared,
        None,
        false,
        None,
    )
    .expect("broken start");
    let after = fake.log();
    let grown = after.strip_prefix(baseline.as_str()).unwrap_or_default();
    assert!(
        grown.contains("plugin list --json"),
        "a broken registration must heal at start, got:\n{grown}"
    );
}

// ── the shared-store skip line ─────────────────────────────────────────────

/// A `clauth start` of a shared profile under a `~/.claude` that refuses new
/// dirs still succeeds, and tells the terminal once per store it could not
/// create, naming only that store's own consequence. The build hands the
/// lines back and `start::run` prints them through `errln!`, captured here.
/// Linked trees only: a host that cannot symlink copies `~/.claude` instead
/// and creates no store, so it cannot pose the failure.
#[cfg(unix)]
#[test]
fn start_prints_one_line_per_store_it_cannot_create() {
    use crate::testutil::{ConfigDirSandbox, FakeClaude, HomeSandbox};

    let probe = tempfile::tempdir().expect("probe dir");
    if std::os::unix::fs::symlink(probe.path(), probe.path().join("link")).is_err() {
        return;
    }

    // Both stores fail.
    {
        let sb = HomeSandbox::new();
        let claude = sb.home().join(".claude");
        std::fs::create_dir_all(&claude).expect("~/.claude");
        let _config = ConfigDirSandbox::new(&sb, &claude);
        let fake = FakeClaude::new(&sb);
        // `acquire` re-reads the account record from disk under the state
        // flock, so the sandbox needs the profile the run starts under.
        let clauth_dir = crate::profile::clauth_dir().expect("clauth dir");
        std::fs::create_dir_all(&clauth_dir).expect("clauth dir");
        std::fs::write(
            clauth_dir.join("profiles.toml"),
            "profiles = [\"wired\", \"spare\"]\n",
        )
        .expect("profiles.toml");
        let config = chain_ready_config("wired");

        let unwritable = crate::testutil::UnwritableDir::new(&claude);
        let refused = std::fs::create_dir(claude.join("probe"))
            .expect_err("the unwritable dir refuses a new dir")
            .to_string();
        let capture = crate::out::ErrCapture::new();
        let guard = capture.capture_here();

        run(
            &config,
            &crate::profile::ProfileName::from("wired"),
            &[],
            Isolation::Shared,
            None,
            false,
            None,
        )
        .expect("a shared start succeeds when stores cannot be created");
        drop(guard);
        drop(unwritable);
        drop(fake);

        let expected = [
            format!(
                "clauth: could not create {}: {refused}; this session's background jobs \
                 stay in its own tree, where `claude agents` outside it cannot see them\n",
                claude.join("jobs").display()
            ),
            format!(
                "clauth: could not create {}: {refused}; this session's transcripts \
                 stay in its own tree and are lost when it ends\n",
                claude.join("projects").display()
            ),
        ];
        assert_eq!(
            String::from_utf8(capture.snapshot()).expect("captured stderr is utf-8"),
            expected.join(""),
            "stderr carries exactly one line per uncreatable store, nothing else"
        );
    }

    // Only `jobs` fails: `projects` pre-exists as a real dir before arming, so
    // the run names only the store whose records stay in the tree.
    {
        let sb = HomeSandbox::new();
        let claude = sb.home().join(".claude");
        std::fs::create_dir_all(claude.join("projects")).expect("~/.claude/projects");
        let _config = ConfigDirSandbox::new(&sb, &claude);
        let fake = FakeClaude::new(&sb);
        let clauth_dir = crate::profile::clauth_dir().expect("clauth dir");
        std::fs::create_dir_all(&clauth_dir).expect("clauth dir");
        std::fs::write(
            clauth_dir.join("profiles.toml"),
            "profiles = [\"wired\", \"spare\"]\n",
        )
        .expect("profiles.toml");
        let config = chain_ready_config("wired");

        let unwritable = crate::testutil::UnwritableDir::new(&claude);
        let refused = std::fs::create_dir(claude.join("probe"))
            .expect_err("the unwritable dir refuses a new dir")
            .to_string();
        let capture = crate::out::ErrCapture::new();
        let guard = capture.capture_here();

        run(
            &config,
            &crate::profile::ProfileName::from("wired"),
            &[],
            Isolation::Shared,
            None,
            false,
            None,
        )
        .expect("a shared start succeeds when the jobs store cannot be created");
        drop(guard);
        drop(unwritable);
        drop(fake);

        let expected = format!(
            "clauth: could not create {}: {refused}; this session's background jobs \
             stay in its own tree, where `claude agents` outside it cannot see them\n",
            claude.join("jobs").display()
        );
        assert_eq!(
            String::from_utf8(capture.snapshot()).expect("captured stderr is utf-8"),
            expected,
            "stderr carries exactly the jobs line, nothing else"
        );
    }
}

/// The codex spawn's wire facts, pinned without spawning: the CODEX_HOME pin,
/// the forced file store as the FIRST -c (a caller's later -c wins in codex's
/// layering, which is their own foot-gun to aim), and the passthrough args
/// after it. The store value carries its TOML quotes as literal bytes — a
/// well-formed TOML string to codex's -c parser, not a bare-word fallback.
#[test]
fn the_codex_spawn_command_carries_the_wire_facts() {
    let home = crate::testutil::HomeSandbox::new();
    let session_home = home.home().join(".clauth/profiles/cx/codex-home-4242-0");
    let cmd = codex_spawn_command(
        &session_home,
        &["exec".to_string(), "--full-auto".to_string()],
        &[],
    );

    assert_eq!(
        cmd.get_program(),
        crate::runtime::codex_command().get_program()
    );
    let env = crate::testutil::env_overrides(&cmd);
    assert_eq!(
        env.get("CODEX_HOME").and_then(|v| v.as_deref()),
        session_home.to_str(),
        "the home pin is the session's own home"
    );
    let args: Vec<String> = cmd
        .get_args()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        args,
        [
            "-c",
            "cli_auth_credentials_store=\"file\"",
            "-c",
            &format!("sqlite_home='{}'", session_home.display()),
            "exec",
            "--full-auto"
        ],
        "forced store and state-DB home first, passthrough after"
    );
}

/// The state-DB pin exists to outrank a `sqlite_home` the COPIED config.toml
/// carries: scrubbing `CODEX_SQLITE_HOME` cannot reach a config key, and an
/// operator's absolute path there would pool every profile's goals/memories/
/// state DBs in one directory while the home's durable links sit unopened.
#[test]
fn the_spawn_pins_the_state_db_home_past_a_copied_config_key() {
    let home = crate::testutil::HomeSandbox::new();
    let session_home = home.home().join(".clauth/profiles/cx/codex-home-4242-0");
    std::fs::create_dir_all(&session_home).expect("mkdir home");
    // The operator's own setting, faithfully copied into the session home by
    // `build_codex_home` — codex would resolve its DBs there, not here.
    std::fs::write(
        session_home.join("config.toml"),
        b"sqlite_home = \"/tmp/one-shared-dir\"\n",
    )
    .expect("write config");

    let cmd = codex_spawn_command(&session_home, &[], &[]);
    let args: Vec<String> = cmd
        .get_args()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    let pinned = format!("sqlite_home='{}'", session_home.display());
    assert!(
        args.windows(2).any(|w| w[0] == "-c" && w[1] == pinned),
        "the session's own home is pinned past the copied key: {args:?}"
    );
    assert!(
        !args.iter().any(|a| a.contains("/tmp/one-shared-dir")),
        "the operator's path is never what the spawn names"
    );
}

/// codex layers its managed config ABOVE the session's `-c` flags, so a key
/// set there defeats the forced store and state-DB home the spawn pins, and
/// nothing clauth passes can outrank it. The two keys that kill the chain
/// refuse the spawn with a line naming the file, the key, its value and the
/// fix; the moved state-DB home warns; anything else, absent or unparseable
/// included, is clear.
#[test]
fn the_managed_config_verdict_refuses_the_chain_killers_and_warns_on_the_rest() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("managed_config.toml");
    let file = path.display();

    assert_eq!(
        managed_config_verdict(&path),
        ManagedConfigVerdict::Clear,
        "absent"
    );

    fs::write(
        &path,
        "model = \"o3\"\ncli_auth_credentials_store = \"keyring\"\n",
    )
    .expect("write");
    assert_eq!(
        managed_config_verdict(&path),
        ManagedConfigVerdict::Refuse(format!(
            "{file} sets cli_auth_credentials_store = \"keyring\", and a managed config outranks \
             the file store clauth forces at spawn, so codex would ignore this session's \
             linked auth.json. ask whoever manages this machine to remove the key or set it \
             to \"file\"; clauth cannot override a managed config"
        ))
    );

    fs::write(&path, "cli_auth_credentials_store = \"file\"\n").expect("write");
    assert_eq!(
        managed_config_verdict(&path),
        ManagedConfigVerdict::Clear,
        "the file store is what the spawn forces anyway"
    );

    fs::write(&path, "[debug]\nconfig_lockfile = { load_path = \"/x\" }\n").expect("write");
    assert_eq!(
        managed_config_verdict(&path),
        ManagedConfigVerdict::Refuse(format!(
            "{file} sets debug.config_lockfile.load_path = \"/x\", and a managed config \
             outranks the flags clauth passes at spawn, so codex would replay that lockfile \
             as its whole config and drop the file store this session's linked auth.json \
             depends on. ask whoever manages this machine to remove the key; clauth cannot \
             override a managed config"
        ))
    );

    fs::write(
        &path,
        "[debug]\nconfig_lockfile = { export_dir = \"/e\" }\n",
    )
    .expect("write");
    assert_eq!(
        managed_config_verdict(&path),
        ManagedConfigVerdict::Clear,
        "an export dir writes lockfiles and replays none"
    );

    fs::write(&path, "sqlite_home = \"/y\"\n").expect("write");
    assert_eq!(
        managed_config_verdict(&path),
        ManagedConfigVerdict::Warn(format!(
            "{file} sets sqlite_home = \"/y\", which outranks the per-session home \
             clauth pins at spawn, so every profile's state dbs land in that one directory"
        ))
    );

    fs::write(
        &path,
        "sqlite_home = \"/y\"\ncli_auth_credentials_store = \"auto\"\n",
    )
    .expect("write");
    assert!(
        matches!(
            managed_config_verdict(&path),
            ManagedConfigVerdict::Refuse(_)
        ),
        "a chain killer outranks a warning"
    );

    fs::write(&path, "model = \"o3\"\n[unclosed\n").expect("write");
    assert_eq!(
        managed_config_verdict(&path),
        ManagedConfigVerdict::Clear,
        "a file codex cannot parse is codex's own refusal"
    );
}

/// The spawn site consults the verdict before anything else: a refusing
/// managed config ends `run_codex` with the verdict's own line. The profile
/// root is walled off with a file so a spawn-site regression fails on the
/// wall (`acquire` cannot create the profile dir) instead of launching
/// whatever `codex` is on PATH.
#[test]
fn run_codex_refuses_on_the_managed_config_before_building_a_home() {
    let sb = HomeSandbox::new();
    fs::create_dir_all(sb.home().join(".clauth")).expect("mkdir .clauth");
    fs::write(sb.home().join(".clauth/profiles"), b"").expect("wall off the profile root");
    let managed = sb.home().join("managed_config.toml");
    fs::write(&managed, "cli_auth_credentials_store = \"keyring\"\n").expect("write managed");
    let _managed = ManagedConfigSandbox::new(&sb, &managed);
    let config = AppConfig {
        state: AppState::default(),
        profiles: Vec::new(),
    };

    let err = run_codex(&config, "cx", &[], Isolation::Shared)
        .expect_err("a managed keyring store refuses the spawn");
    assert_eq!(
        err.to_string(),
        format!(
            "{} sets cli_auth_credentials_store = \"keyring\", and a managed config outranks \
             the file store clauth forces at spawn, so codex would ignore this session's \
             linked auth.json. ask whoever manages this machine to remove the key or set it \
             to \"file\"; clauth cannot override a managed config",
            managed.display()
        )
    );
}

/// A home whose path carries an apostrophe cannot ride a TOML literal string,
/// so the override falls back to a basic string rather than emitting a value
/// codex's `-c` parser would read as truncated.
#[test]
fn a_quoted_home_path_falls_back_to_a_basic_toml_string() {
    assert_eq!(
        toml_path_value(std::path::Path::new("/Users/o'brien/.clauth")),
        "\"/Users/o'brien/.clauth\""
    );
    assert_eq!(
        toml_path_value(std::path::Path::new("/Users/plain/.clauth")),
        "'/Users/plain/.clauth'"
    );
}

// ── the runtime settings merge's strip list ──────────────────────────────

/// The runtime settings merge at session start strips the OUTGOING
/// activation's custom env from the shared base — `actions::outgoing_env_keys`,
/// which with no marker to read answers every configured profile's keys.
/// Passing an empty list there (what the site did before) leaves the departed
/// account's `[env]` entries in the base untouched, and the merge then pairs
/// them with the started account's endpoint in the runtime settings.
/// Reach: `switch_off` (clears the marker, never the file), then
/// `clauth start <other>`.
///
/// Drives the real `run` against a slow shim, so the list under test is the
/// one `run` itself computes, and reads the runtime settings.json MID-run:
/// the drop removes the tree once the child exits, leaving nothing to assert.
#[cfg(unix)]
#[test]
#[cfg(unix)]
fn a_start_after_a_switch_off_does_not_pair_the_departed_key_with_the_started_endpoint() {
    use crate::testutil::HomeSandbox;

    let sb = HomeSandbox::new();
    let claude_home = sb.home().join(".claude");
    fs::create_dir_all(&claude_home).expect("~/.claude");

    let mut departing = Profile::new("departing".to_string(), None, None);
    departing.env.insert(
        "ANTHROPIC_API_KEY".to_string(),
        "departing-token".to_string(),
    );
    departing.credentials = Some(crate::profile::ClaudeCredentials {
        claude_ai_oauth: Some(crate::profile::OAuthToken {
            access_token: "at-departing".to_string(),
            refresh_token: Some("rt-departing".to_string()),
            expires_at: None,
            scopes: None,
            subscription_type: None,
            ..crate::profile::OAuthToken::default_extra()
        }),
    });
    let target = Profile::new(
        "target".to_string(),
        Some("https://api.deepseek.com/anthropic".to_string()),
        Some("sk-target".to_string()),
    );
    crate::profile::save_profile(&departing).expect("save departing");
    crate::profile::save_profile(&target).expect("save target");
    let config = AppConfig {
        state: AppState {
            profiles: vec!["departing".into(), "target".into()],
            active_profile: Some("departing".into()),
            ..AppState::default()
        },
        profiles: vec![departing, target],
    };
    crate::profile::save_app_state(&config.state).expect("persist state");
    let departing_ref = config
        .find(&crate::profile::ProfileName::from("departing"))
        .expect("profile");
    crate::claude::apply_profile_to_claude_settings(departing_ref, &[])
        .expect("seed the departing account's env into the live settings");

    let config = crate::testutil::through_handle(config, |h| {
        crate::actions::switch_off(h).expect("switch off")
    })
    .0;
    assert_eq!(
        config.state.active_profile, None,
        "fixture: the marker must be cleared, which is what the start then reads"
    );

    let shim = crate::testutil::SlowClaude::new(&sb);
    let target_dir = crate::profile::profile_dir(&crate::profile::ProfileName::from("target"))
        .expect("profile dir");
    let observer = std::thread::spawn(move || crate::testutil::runtime_settings_until(&target_dir));
    run(
        &config,
        &crate::profile::ProfileName::from("target"),
        &[],
        Isolation::Shared,
        None,
        false,
        None,
    )
    .expect("start");
    let settings = observer
        .join()
        .expect("observer thread")
        .expect("the runtime settings never appeared");
    drop(shim);

    let json: serde_json::Value = serde_json::from_str(&settings).expect("parse settings");
    assert_eq!(
        json["env"]["ANTHROPIC_BASE_URL"],
        serde_json::json!("https://api.deepseek.com/anthropic"),
        "fixture: the merge ran, pointing the runtime at the started endpoint"
    );
    assert!(
        json["env"].get("ANTHROPIC_API_KEY").is_none(),
        "a departed account's env key must not survive in front of the started \
         account's endpoint: {settings}"
    );
}

// ── the start producer's launcher choice ─────────────────────────────────

/// A `clauth start` row reads own: the producer passes `Launcher::Own`, so a
/// reader on the registry mid-run must see the start row and must never read
/// it as a delegate. Drives the real `run` under `SlowClaude` — the row is
/// unregistered at teardown, so it is read inside the shim's window — through
/// the same shared reader the mcp producer pin uses.
#[cfg(unix)]
#[test]
fn a_clauth_start_row_is_seen_and_reads_own() {
    let sb = HomeSandbox::new();
    let claude_home = sb.home().join(".claude");
    fs::create_dir_all(&claude_home).expect("~/.claude");

    let target = Profile::new(
        "target".to_string(),
        Some("https://api.deepseek.com/anthropic".to_string()),
        Some("sk-target".to_string()),
    );
    crate::profile::save_profile(&target).expect("save target");
    let config = AppConfig {
        state: AppState {
            profiles: vec!["target".into()],
            active_profile: Some("target".into()),
            ..AppState::default()
        },
        profiles: vec![target],
    };
    crate::profile::save_app_state(&config.state).expect("persist state");

    let dir = crate::profile::clauth_dir()
        .expect("clauth dir")
        .join("live_sessions");
    let shim = crate::testutil::SlowClaude::new(&sb);
    let observer = std::thread::spawn(move || {
        crate::testutil::live_session_rows_until(
            &dir,
            |row| row.start_profile == "target",
            std::time::Duration::from_secs(20),
        )
    });
    run(
        &config,
        &crate::profile::ProfileName::from("target"),
        &[],
        Isolation::Shared,
        None,
        false,
        None,
    )
    .expect("start");
    let seen = observer.join().expect("observer thread");
    drop(shim);

    let start_rows: Vec<_> = seen
        .iter()
        .filter(|row| row.start_profile == "target")
        .collect();
    assert!(
        !start_rows.is_empty(),
        "the start row must be observed mid-run"
    );
    assert!(
        start_rows.iter().all(|row| !row.is_delegate),
        "a clauth start row reads own: {start_rows:?}"
    );
}

/// The spend-cap decision leg reads the launch demand off the row `run` writes,
/// never re-deriving it. Drives the real `run` against the slow shim and reads
/// the live row MID-run (its drop removes the tree once the child exits):
/// the recorded `launch_families` must name what THIS launch ran — the outgoing
/// profile's `[env] ANTHROPIC_MODEL` must not leak into an opus launch, and a
/// `--model fable` passthrough on a member with no default must record fable —
/// and the recorded `claude_pid` must be the spawned child, and the row must
/// record the credit-leave capability its relax leg gates on.
#[cfg(unix)]
#[test]
fn a_start_run_records_the_launch_demand_and_the_child_pid() {
    use crate::testutil::{HomeSandbox, SlowClaude};

    // Poll the registry until the child pid lands (the write happens right
    // after the spawn), then hand back the recorded demand and pid. Bounded so
    // a run that never records fails the test instead of hanging the suite.
    let observe =
        |target: &'static str| -> std::thread::JoinHandle<(Option<Vec<String>>, Option<u32>, bool)>
        {
            std::thread::spawn(move || {
                let dir = crate::profile::clauth_dir()
                    .expect("clauth dir")
                    .join("live_sessions");
                let seen = crate::testutil::live_session_rows_until(
                    &dir,
                    |r| r.start_profile == target && r.claude_pid.is_some(),
                    std::time::Duration::from_secs(20),
                );
                seen.into_iter()
                    .find(|r| r.start_profile == target && r.claude_pid.is_some())
                    .map(|r| (r.launch_families, r.claude_pid, r.credit_leave_aware))
                    .unwrap_or((None, None, false))
            })
        };

    // The outgoing (active) profile pins ANTHROPIC_MODEL=fable in its `[env]`,
    // so `outgoing_env_keys` scrubs it from the started session.
    let stage = |a_default: Option<&str>| {
        let sb = HomeSandbox::new();
        fs::create_dir_all(sb.home().join(".claude")).expect("~/.claude");
        let mut x = Profile::new("x".to_string(), None, None);
        x.env
            .insert("ANTHROPIC_MODEL".to_string(), "fable".to_string());
        let mut a = Profile::new("a".to_string(), None, None);
        a.models.default = a_default.map(str::to_string);
        crate::profile::save_profile(&x).expect("save x");
        crate::profile::save_profile(&a).expect("save a");
        crate::claude::apply_profile_to_claude_settings(&x, &[]).expect("seed x's env live");
        let config = AppConfig {
            state: AppState {
                profiles: vec!["a".into(), "x".into()],
                active_profile: Some("x".into()),
                ..AppState::default()
            },
            profiles: vec![a, x],
        };
        crate::profile::save_app_state(&config.state).expect("persist state");
        (sb, config)
    };

    // Scenario 1: an opus launch — the outgoing profile's fable env must not
    // make the recorded demand credit-billed.
    {
        let (sb, config) = stage(Some("opus"));
        let shim = SlowClaude::new(&sb);
        let observer = observe("a");
        run(
            &config,
            &crate::profile::ProfileName::from("a"),
            &[],
            Isolation::Shared,
            None,
            false,
            None,
        )
        .expect("start");
        let (families, pid, aware) = observer.join().expect("observer");
        let shim_pid = std::fs::read_to_string(sb.home().join(".slow-claude.pid"))
            .ok()
            .and_then(|s| s.trim().parse::<u32>().ok());
        assert!(pid.is_some(), "the row records the spawned claude pid");
        assert_eq!(
            pid, shim_pid,
            "the recorded pid is the spawned claude, not clauth's own"
        );
        assert_eq!(
            families,
            Some(vec!["opus".to_string()]),
            "an opus launch must not be credit-billed off the outgoing profile's env model"
        );
        assert!(
            aware,
            "the row records that this session understands the credit-leave reason"
        );
        drop(shim);
    }

    // Scenario 2: a member with no default, started with `--model fable` — the
    // passthrough is the launch demand.
    {
        let (sb, config) = stage(None);
        let shim = SlowClaude::new(&sb);
        let observer = observe("a");
        run(
            &config,
            &crate::profile::ProfileName::from("a"),
            &["--model".to_owned(), "fable".to_owned()],
            Isolation::Shared,
            None,
            false,
            None,
        )
        .expect("start");
        let (families, pid, aware) = observer.join().expect("observer");
        let shim_pid = std::fs::read_to_string(sb.home().join(".slow-claude.pid"))
            .ok()
            .and_then(|s| s.trim().parse::<u32>().ok());
        assert!(pid.is_some(), "the row records the spawned claude pid");
        assert_eq!(
            pid, shim_pid,
            "the recorded pid is the spawned claude, not clauth's own"
        );
        assert_eq!(
            families,
            Some(vec!["fable".to_string()]),
            "a --model fable passthrough must record fable as the launch demand"
        );
        assert!(
            aware,
            "the row records that this session understands the credit-leave reason"
        );
        drop(shim);
    }
}

// ── start-time model demand + the extracted admit ───────────────────────────

#[test]
fn parsed_model_args_reads_both_spellings() {
    let split = vec!["--model".to_owned(), "claude-fable-5-1".to_owned()];
    assert_eq!(
        parsed_model_args(&split),
        (
            Some("claude-fable-5-1".into()),
            Some("claude-fable-5-1".into()),
            None,
        )
    );
    let joined = vec!["--model=opus".to_owned()];
    assert_eq!(
        parsed_model_args(&joined),
        (Some("opus".into()), Some("opus".into()), None)
    );
    let none = vec!["-p".to_owned(), "hi".to_owned()];
    assert_eq!(parsed_model_args(&none), (None, None, None));
    let fb_split = vec!["--fallback-model".to_owned(), "sonnet,haiku".to_owned()];
    assert_eq!(
        parsed_model_args(&fb_split),
        (None, None, Some("sonnet,haiku".into()))
    );
    let fb_joined = vec!["--fallback-model=opus, sonnet".to_owned()];
    assert_eq!(
        parsed_model_args(&fb_joined),
        (None, None, Some("opus, sonnet".into()))
    );
    assert_eq!(
        parsed_model_args(&["--fallback-model".to_owned()]),
        (None, None, None)
    );
}

#[test]
fn a_dangling_model_flag_yields_nothing() {
    assert_eq!(
        parsed_model_args(&["--model".to_owned()]),
        (None, None, None)
    );
}

/// A blank `--model` value is absent, never a headline: the overlay default
/// stays and the unplaceable refusal never prints an empty model.
#[test]
fn a_blank_model_value_is_absent() {
    assert_eq!(
        parsed_model_args(&["--model=".to_owned()]),
        (None, Some(String::new()), None)
    );
    assert_eq!(
        parsed_model_args(&["--model".to_owned(), String::new()]),
        (None, Some(String::new()), None)
    );
}

#[test]
fn launch_models_unions_the_settings_models_and_the_env_models() {
    let settings = vec![
        "opus".to_owned(),
        "claude-sonnet-5".to_owned(),
        "claude-haiku-4-5".to_owned(),
        "haiku".to_owned(),
    ];
    assert_eq!(
        launch_models_from(settings, [Some("gemini".to_owned()), None]),
        [
            "opus",
            "claude-sonnet-5",
            "claude-haiku-4-5",
            "haiku",
            "gemini",
        ]
    );
    assert!(
        launch_models_from(Vec::new(), [None, Some("".to_owned())]).is_empty(),
        "an empty env value is dropped, never a family"
    );
}

/// The refusals `run` runs are the same ones `cmd_start` runs before printing:
/// `admit` is what `--explain` shares with a real launch, so a `--with-fallback`
/// target a launch would refuse is refused here too, and the same member clears
/// without the flag.
#[test]
fn admit_runs_the_with_fallback_refusals_and_clears_without_the_flag() {
    let _sb = HomeSandbox::new();
    let _daemon = crate::daemon::hold_daemon_lock();
    let mut third_party = chain_ready_config("thirdparty");
    third_party.profiles[0].base_url = Some("https://api.example.com".to_owned());

    let err = admit(
        &third_party,
        &crate::profile::ProfileName::from("thirdparty"),
        Isolation::Shared,
        true,
        &[],
    )
    .expect_err("a custom endpoint must refuse under --with-fallback");
    assert_eq!(
        err.to_string(),
        "'thirdparty': --with-fallback needs an OAuth account, but this one carries \
         a custom endpoint; start without it"
    );

    admit(
        &third_party,
        &crate::profile::ProfileName::from("thirdparty"),
        Isolation::Shared,
        false,
        &[],
    )
    .expect("no flag, no refusal");
}

/// A background launch on a box that never ran `claude --bg` (no
/// `~/.claude/jobs` yet) still leaves its job record in the one global store,
/// naming the tree it runs on, so the launcher's teardown spares that tree.
/// Claude Code writes the record through its own `CLAUDE_CONFIG_DIR`, so a
/// tree without a `jobs` link would keep the record where no clauth reader
/// looks and lose the tree under the running session. The transcript store is
/// the same case: with no `~/.claude/projects`, a transcript written through
/// the tree is unindexed and dies with it.
///
/// The red-first pin: on the base it places no store link.
#[cfg(unix)]
#[test]
fn a_background_start_keeps_its_job_in_the_global_store_on_a_fresh_box() {
    let sb = HomeSandbox::new();
    let claude = sb.home().join(".claude");
    fs::create_dir_all(&claude).expect("~/.claude");
    let _config_dir = crate::testutil::ConfigDirSandbox::new(&sb, &claude);
    let shim = crate::testutil::FakeClaude::new(&sb);
    let clauth_dir = crate::profile::clauth_dir().expect("clauth dir");
    fs::create_dir_all(&clauth_dir).expect("clauth dir");
    fs::write(
        clauth_dir.join("profiles.toml"),
        "profiles = [\"wired\", \"spare\"]\n",
    )
    .expect("profiles.toml");

    run(
        &chain_ready_config("wired"),
        &crate::profile::ProfileName::from("wired"),
        &["--bg".to_string()],
        Isolation::Shared,
        None,
        false,
        None,
    )
    .expect("background start");

    let tree: Vec<String> = shim
        .spawns()
        .into_iter()
        .filter(|(_, argv)| argv.split(' ').any(|word| word == "--bg"))
        .map(|(dir, _)| dir)
        .collect();
    assert_eq!(tree.len(), 1, "one background launch");
    let tree = std::path::PathBuf::from(&tree[0]);
    let jobs: Vec<std::path::PathBuf> = fs::read_dir(claude.join("jobs"))
        .map(|dir| dir.flatten().map(|e| e.path()).collect())
        .unwrap_or_default();
    assert_eq!(jobs.len(), 1, "the job is in the global store");
    let record: serde_json::Value =
        serde_json::from_slice(&fs::read(jobs[0].join("state.json")).expect("job record"))
            .expect("parse job record");
    assert_eq!(
        record["providerEnv"]["CLAUDE_CONFIG_DIR"].as_str(),
        Some(tree.to_string_lossy().as_ref()),
        "the record names the tree"
    );
    assert!(tree.is_dir(), "the tree outlives its launcher");
    let transcripts: Vec<std::path::PathBuf> = fs::read_dir(claude.join("projects"))
        .map(|dir| dir.flatten().map(|e| e.path()).collect())
        .unwrap_or_default();
    assert_eq!(
        transcripts.len(),
        1,
        "the transcript is in the global store the session index reads"
    );
}

// ── T33b1: the per-account background tree ────────────────────────────────

/// A sandboxed `clauth start` rig for the background tests: `~/.claude`, the
/// fake `claude`, and a `profiles.toml` naming the chain `chain_ready_config`
/// builds.
#[cfg(unix)]
fn background_rig(sb: &HomeSandbox) -> crate::testutil::ConfigDirSandbox<'_> {
    let claude = sb.home().join(".claude");
    fs::create_dir_all(&claude).expect("~/.claude");
    let config = crate::testutil::ConfigDirSandbox::new(sb, &claude);
    let clauth_dir = crate::profile::clauth_dir().expect("clauth dir");
    fs::create_dir_all(&clauth_dir).expect("clauth dir");
    fs::write(
        clauth_dir.join("profiles.toml"),
        "profiles = [\"wired\", \"spare\"]\n",
    )
    .expect("profiles.toml");
    config
}

/// The `CLAUDE_CONFIG_DIR` of every shim call whose argv carries `flag` as a
/// whole word.
#[cfg(unix)]
fn config_dirs_of(fake: &crate::testutil::FakeClaude<'_>, flag: &str) -> Vec<String> {
    fake.spawns()
        .into_iter()
        .filter(|(_, argv)| argv.split(' ').any(|word| word == flag))
        .map(|(dir, _)| dir)
        .collect()
}

/// T33b1 check 1: both spellings of the background flag launch `claude` on
/// the account's one background tree, `profiles/<p>/runtime-bg`, never a
/// per-launch `runtime-<sid>`; the launcher's own marker in `sessions-bg/`
/// lives only for the launch, and the tree outlives it because the job the
/// launch left names it.
#[cfg(unix)]
#[test]
fn a_background_start_launches_on_the_accounts_background_tree() {
    for flag in ["--bg", "--background"] {
        let sb = HomeSandbox::new();
        let _config_dir = background_rig(&sb);
        let fake = crate::testutil::FakeClaude::new(&sb);
        let name = crate::profile::ProfileName::from("wired");

        run(
            &chain_ready_config("wired"),
            &name,
            &[flag.to_string()],
            Isolation::Shared,
            None,
            false,
            None,
        )
        .expect("background start");

        let tree = profile_dir_of("wired").join("runtime-bg");
        let dirs = config_dirs_of(&fake, flag);
        assert_eq!(
            dirs,
            [tree.to_string_lossy().into_owned()],
            "{flag}: claude ran on the account's background tree"
        );
        assert!(
            !crate::runtime::has_live_marker(&name),
            "{flag}: the launcher's marker dies with the launch"
        );
        assert!(
            tree.is_dir(),
            "{flag}: the job the launch left keeps its tree"
        );
        let marker_dirs: Vec<String> = fs::read_dir(profile_dir_of("wired"))
            .expect("profile dir")
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with("sessions"))
            .collect();
        assert_eq!(
            marker_dirs,
            ["sessions-bg"],
            "{flag}: the launcher kept its marker in the background pair"
        );
    }
}

/// T33b1 check 2: a launch while another holder (the daemon) keeps a marker
/// in `sessions-bg/` enters the same tree: no `failed to claim session
/// marker`, no wipe of the live tree, and the other holder's marker stays.
#[cfg(unix)]
#[test]
fn a_background_start_enters_the_tree_another_holder_keeps() {
    let sb = HomeSandbox::new();
    let _config_dir = background_rig(&sb);
    let fake = crate::testutil::FakeClaude::new(&sb);
    let name = crate::profile::ProfileName::from("wired");
    let config = chain_ready_config("wired");
    let start = || {
        run(
            &config,
            &name,
            &["--bg".to_string()],
            Isolation::Shared,
            None,
            false,
            None,
        )
    };
    start().expect("first background start builds the tree");
    // `claude rm` the first launch's job, so the other holder's marker is the
    // only thing standing between the second launch and a wipe of the tree.
    let jobs = sb.home().join(".claude").join("jobs");
    for job in fs::read_dir(&jobs)
        .expect("the first launch left a job")
        .flatten()
    {
        fs::remove_dir_all(job.path()).expect("claude rm");
    }

    let tree = profile_dir_of("wired").join("runtime-bg");
    let sessions = profile_dir_of("wired").join("sessions-bg");
    fs::create_dir_all(&tree).expect("runtime-bg");
    fs::create_dir_all(&sessions).expect("sessions-bg");
    let holder = sessions.join("4242-0");
    let held = crate::runtime::open_pid_file(&holder).expect("holder marker");
    held.lock().expect("hold the marker");
    let sentinel = tree.join(".t33b1.tmp.keep");
    fs::write(&sentinel, b"kept").expect("sentinel");

    start().expect("a second launch enters the held tree");

    let tree_dir = tree.to_string_lossy().into_owned();
    assert_eq!(
        config_dirs_of(&fake, "--bg"),
        [tree_dir.clone(), tree_dir],
        "both launches ran on the one background tree"
    );
    assert_eq!(
        fs::read(&sentinel).expect("the live tree was not wiped"),
        b"kept"
    );
    assert!(holder.is_file(), "the other holder's marker stays");
    drop(held);
}

/// T33b1 N1 (cloudy 2026-10-22, option A): a host that copies runtime trees
/// instead of linking them would copy a `claude rm`ed job's record back into
/// the store, so both spellings refuse there before anything spawns or any
/// tree is built, with the copy cloudy approved.
#[cfg(unix)]
#[test]
fn a_background_start_on_a_copy_host_is_refused_before_anything_spawns() {
    for flag in ["--bg", "--background"] {
        let sb = HomeSandbox::new();
        let _config_dir = background_rig(&sb);
        let fake = crate::testutil::FakeClaude::new(&sb);
        let _fake_mode = crate::runtime::ForcedFakeLinkMode::new();

        let err = run(
            &chain_ready_config("wired"),
            &crate::profile::ProfileName::from("wired"),
            &[flag.to_string()],
            Isolation::Shared,
            None,
            false,
            None,
        )
        .expect_err("a background start on a copy host is refused");

        assert_eq!(
            err.to_string(),
            format!(
                "'wired': {flag} cannot run on this host, since it copies runtime trees \
                 instead of linking them and a background session would outlive the \
                 credentials clauth rotates (on Windows, turn on Developer Mode)"
            )
        );
        assert_eq!(fake.spawns(), Vec::new(), "{flag}: nothing spawned");
        let leftovers: Vec<_> = fs::read_dir(profile_dir_of("wired"))
            .map(|dir| dir.flatten().map(|e| e.file_name()).collect())
            .unwrap_or_default();
        assert!(
            leftovers
                .iter()
                .all(|n| !n.to_string_lossy().starts_with("runtime")),
            "{flag}: no runtime tree was built: {leftovers:?}"
        );
    }
}

/// T33b1 check 3: an isolated tree links no `jobs/`, so a background session
/// started in one is invisible to clauth; both spellings refuse before
/// anything spawns, with the copy cloudy approved 2026-10-21.
#[cfg(unix)]
#[test]
fn an_isolated_background_start_is_refused_before_anything_spawns() {
    for flag in ["--bg", "--background"] {
        let sb = HomeSandbox::new();
        let _config_dir = background_rig(&sb);
        let fake = crate::testutil::FakeClaude::new(&sb);

        let err = run(
            &chain_ready_config("wired"),
            &crate::profile::ProfileName::from("wired"),
            &[flag.to_string()],
            Isolation::Isolated,
            None,
            false,
            None,
        )
        .expect_err("an isolated background start is refused");

        assert_eq!(
            err.to_string(),
            format!(
                "'wired': {flag} cannot be combined with --isolated, since background \
                 sessions run in the account's shared background tree"
            )
        );
        assert_eq!(fake.spawns(), Vec::new(), "{flag}: nothing spawned");
        let leftovers: Vec<_> = fs::read_dir(profile_dir_of("wired"))
            .map(|dir| dir.flatten().map(|e| e.file_name()).collect())
            .unwrap_or_default();
        assert!(
            leftovers
                .iter()
                .all(|n| !n.to_string_lossy().starts_with("runtime")),
            "{flag}: no runtime tree was built: {leftovers:?}"
        );
    }
}

/// The copy-host refusal sits BEFORE the plugin preflight: on a copy host with a
/// divergent plugin registration the gate fires and the heal would spawn
/// `claude plugin list --json`, so an empty shim call log proves the refusal
/// ran first.
#[cfg(unix)]
#[test]
fn a_copy_host_background_refusal_precedes_the_plugin_preflight() {
    let sb = HomeSandbox::new();
    let _config_dir = background_rig(&sb);
    let fake = crate::testutil::FakeClaude::new(&sb);
    // A divergent registration: the preflight gate fires and the heal spawns a
    // `claude plugin list --json` when the launch reaches the preflight.
    let plugins_dir = sb.home().join(".claude").join("plugins");
    fs::create_dir_all(&plugins_dir).expect("plugins dir");
    fs::write(plugins_dir.join("known_marketplaces.json"), "{\"clauth\":").expect("truncated");
    fs::write(plugins_dir.join("installed_plugins.json"), "{}").expect("installed");
    let _fake_mode = crate::runtime::ForcedFakeLinkMode::new();

    let err = run(
        &chain_ready_config("wired"),
        &crate::profile::ProfileName::from("wired"),
        &["--bg".to_string()],
        Isolation::Shared,
        None,
        false,
        None,
    )
    .expect_err("a background start on a copy host is refused");

    assert_eq!(
        err.to_string(),
        "'wired': --bg cannot run on this host, since it copies runtime trees \
         instead of linking them and a background session would outlive the \
         credentials clauth rotates (on Windows, turn on Developer Mode)"
    );
    assert_eq!(
        fake.spawns(),
        Vec::new(),
        "the copy-host refusal precedes the plugin preflight"
    );
}

/// When BOTH an `--isolated` launch and a copy host apply, the `--isolated`
/// refusal wins, because it is ordered first.
#[cfg(unix)]
#[test]
fn an_isolated_background_start_stays_first_on_a_copy_host() {
    let sb = HomeSandbox::new();
    let _config_dir = background_rig(&sb);
    let fake = crate::testutil::FakeClaude::new(&sb);
    let _fake_mode = crate::runtime::ForcedFakeLinkMode::new();

    let err = run(
        &chain_ready_config("wired"),
        &crate::profile::ProfileName::from("wired"),
        &["--bg".to_string()],
        Isolation::Isolated,
        None,
        false,
        None,
    )
    .expect_err("an isolated background start is refused");

    assert_eq!(
        err.to_string(),
        "'wired': --bg cannot be combined with --isolated, since background \
         sessions run in the account's shared background tree"
    );
    assert_eq!(fake.spawns(), Vec::new(), "nothing spawned");
}

// ── the agent demand without a def reader ────────────────────────────────

#[test]
fn parsed_agent_arg_reads_both_spellings_and_the_last_wins() {
    assert_eq!(
        parsed_agent_arg(&["--agent".to_owned(), "rev".to_owned()]),
        Some("rev".to_owned())
    );
    assert_eq!(
        parsed_agent_arg(&["--agent=rev".to_owned()]),
        Some("rev".to_owned())
    );
    assert_eq!(
        parsed_agent_arg(&[
            "--agent=first".to_owned(),
            "--agent".to_owned(),
            "last".to_owned(),
        ]),
        Some("last".to_owned()),
        "the last --agent wins"
    );
    assert_eq!(parsed_agent_arg(&["--agent=".to_owned()]), None);
    assert_eq!(
        parsed_agent_arg(&["--agent".to_owned(), "".to_owned()]),
        None,
        "a blank separate-arg value is absent"
    );
    assert_eq!(
        parsed_agent_arg(&[
            "--agent".to_owned(),
            "rev".to_owned(),
            "--agent".to_owned(),
            "".to_owned(),
        ]),
        Some("rev".to_owned()),
        "a blank never clears an earlier flag"
    );
    assert_eq!(parsed_agent_arg(&["-p".to_owned(), "hi".to_owned()]), None);
}

/// An agent launch naming no model is an unknown demand: the resolver never
/// opens an agent def.
#[test]
fn an_agent_run_naming_no_model_is_an_unknown_demand() {
    let _home = HomeSandbox::new();
    let p = Profile::new("blank".to_string(), None, None);
    let demand = member_launch_demand(
        &p,
        Isolation::Shared,
        &[],
        &["--agent".to_owned(), "rev".to_owned()],
        None,
        EnvLeg::Caller,
        None,
        |_| None,
    );
    assert_eq!(demand.agent_without_model.as_deref(), Some("rev"));
    assert!(demand.models.is_empty(), "{:?}", demand.models);
}

// ── a resumed/continued session runs its transcript's agent ──────────────

fn write_resume_transcript(home: &std::path::Path, id: &str, cwd: &str, agents: &[&str]) {
    write_resume_transcript_padded(home, id, 0, cwd, agents);
}

/// The same transcript with `pad` ~1 KiB filler user lines between the first
/// line and the agent records, so a record can sit past a bounded head window.
fn write_resume_transcript_padded(
    home: &std::path::Path,
    id: &str,
    pad: usize,
    cwd: &str,
    agents: &[&str],
) {
    let dir = home.join(".claude/projects/-w-res");
    std::fs::create_dir_all(&dir).unwrap();
    let mut lines = vec![
        serde_json::json!({
            "sessionId": id,
            "cwd": cwd,
            "message": {"role": "user", "content": "hi"},
        })
        .to_string(),
    ];
    let filler = "f".repeat(1024);
    for _ in 0..pad {
        lines.push(
            serde_json::json!({"sessionId": id, "cwd": cwd, "message": {"role": "user", "content": filler}})
                .to_string(),
        );
    }
    for a in agents {
        lines.push(
            serde_json::json!({"type": "agent-setting", "agentSetting": a, "sessionId": id})
                .to_string(),
        );
    }
    std::fs::write(dir.join(format!("{id}.jsonl")), lines.join("\n")).unwrap();
}

/// `--resume <id>` reads the transcript's LAST `agent-setting` record, so the
/// session resumes as that agent.
#[test]
fn resumed_agent_reads_the_transcripts_last_agent_setting() {
    let sb = HomeSandbox::new();
    write_resume_transcript(sb.home(), "sagent", "/w/res", &["old", "pa"]);
    assert_eq!(
        resumed_agent(&["--resume".to_owned(), "sagent".to_owned()]),
        ResumedAgent::Agent("pa".to_owned()),
        "the last agent-setting record names the resumed agent"
    );
}

/// An explicit `--agent` outranks the resumed session's agent: the transcript
/// is never read, and the resolver names the explicit agent.
#[test]
fn an_explicit_agent_outranks_a_resumed_sessions_agent() {
    let sb = HomeSandbox::new();
    write_resume_transcript(sb.home(), "sagent", "/w/res", &["pa"]);
    assert_eq!(
        resumed_agent(&[
            "--resume".to_owned(),
            "sagent".to_owned(),
            "--agent".to_owned(),
            "rev".to_owned(),
        ]),
        ResumedAgent::NotResumed,
        "an explicit --agent keeps the transcript unread"
    );
    let p = Profile::new("blank".to_string(), None, None);
    let demand = member_launch_demand(
        &p,
        Isolation::Shared,
        &[],
        &[
            "--resume".to_owned(),
            "sagent".to_owned(),
            "--agent".to_owned(),
            "rev".to_owned(),
        ],
        None,
        EnvLeg::Caller,
        None,
        |_| None,
    );
    assert_eq!(demand.agent_without_model.as_deref(), Some("rev"));
}

/// Every resume form whose transcript clauth cannot pin is unknown, so the
/// caller treats it as an agent launch naming no model — whatever the store
/// holds. A `--continue` names no transcript at all here: Claude Code picks the
/// cwd's newest conversation, which clauth never mirrors.
#[test]
fn a_continue_and_every_unpinnable_resume_form_fail_closed() {
    let sb = HomeSandbox::new();
    let args = |form: &[&str]| form.iter().map(|a| (*a).to_owned()).collect::<Vec<_>>();
    assert_eq!(
        resumed_agent(&args(&["--continue"])),
        ResumedAgent::Unknown,
        "an empty store pins no continue"
    );
    write_resume_transcript(sb.home(), "scont", "/w/res", &["pa"]);
    for form in [
        &["--continue"][..],
        &["-c"][..],
        &["--resume"][..],
        &["--resume", "--verbose"][..],
        &["--resume", "probename"][..],
        &["-r", "probename"][..],
        &["--from-pr"][..],
        &["--from-pr", "12"][..],
    ] {
        assert_eq!(
            resumed_agent(&args(form)),
            ResumedAgent::Unknown,
            "{form:?} cannot be pinned to a transcript"
        );
    }
}

/// A non-UTF-8 line is one bad record: the whole-transcript agent scan skips it
/// and keeps reading, so a record after it still names the resumed agent.
#[test]
fn a_non_utf8_line_does_not_stop_the_agent_scan() {
    let sb = HomeSandbox::new();
    let dir = sb.home().join(".claude/projects/-w-res");
    fs::create_dir_all(&dir).unwrap();
    let mut bytes = Vec::new();
    bytes.extend_from_slice(
        serde_json::json!({
            "sessionId": "sbad",
            "cwd": "/w/res",
            "message": {"role": "user", "content": "hi"},
        })
        .to_string()
        .as_bytes(),
    );
    bytes.push(b'\n');
    bytes.extend_from_slice(b"\xff\xfe not utf8\n");
    bytes.extend_from_slice(
        serde_json::json!({"type": "agent-setting", "agentSetting": "pa", "sessionId": "sbad"})
            .to_string()
            .as_bytes(),
    );
    fs::write(dir.join("sbad.jsonl"), bytes).unwrap();
    assert_eq!(
        resumed_agent(&["--resume".to_owned(), "sbad".to_owned()]),
        ResumedAgent::Agent("pa".to_owned()),
        "the record after the bad line still names the agent"
    );
}

/// `-r` as the last token is a bare resume (no value), which is unpinnable: a
/// token starting with `-` is never a resume value.
#[test]
fn a_bare_resume_short_flag_at_the_end_is_unpinnable() {
    let _sb = HomeSandbox::new();
    assert_eq!(
        resumed_agent(&["-r".to_owned()]),
        ResumedAgent::Unknown,
        "a trailing -r takes no value"
    );
}

/// The equals spelling of an exact-id resume pins the transcript like the
/// spaced one, and scans the WHOLE transcript: a record past any bounded head
/// window still makes it an agent launch.
#[test]
fn an_equals_spelling_resume_scans_the_whole_transcript() {
    let sb = HomeSandbox::new();
    write_resume_transcript_padded(sb.home(), "sagent", 300, "/w/res", &["pa"]);
    assert_eq!(
        resumed_agent(&["--resume=sagent".to_owned()]),
        ResumedAgent::Agent("pa".to_owned())
    );
}

/// An unpinnable resume recording no model keeps its union side on the recorded
/// launch families — `--fallback-model fable` here — and never the settings or
/// env headline: clauth cannot tell which model the un-pinned transcript runs.
#[test]
fn an_unpinnable_resume_records_its_union_families_not_the_headline() {
    let _home = HomeSandbox::new();
    let p = Profile::new("blank".to_string(), None, None);
    let families = launch_families_for(
        &p,
        Isolation::Shared,
        &[],
        &["--fallback-model".to_owned(), "fable".to_owned()],
        &ResumedAgent::Unknown,
        false,
        None,
        &[Some("opus".to_owned()), None, None],
    );
    assert_eq!(
        families,
        vec!["fable".to_owned()],
        "the union side is recorded, the unknown headline is not"
    );
}

/// An agent launch records its union side: the agent's own model is unknown
/// (clauth reads no def), so the launch is recorded on its `--fallback-model`
/// and never on the process env's model.
#[test]
fn an_agent_launch_records_its_fallback_not_the_env_headline() {
    let _home = HomeSandbox::new();
    let p = Profile::new("blank".to_string(), None, None);
    let families = launch_families_for(
        &p,
        Isolation::Shared,
        &[],
        &["--fallback-model".to_owned(), "fable".to_owned()],
        &ResumedAgent::Agent("pa".to_owned()),
        false,
        None,
        &[Some("opus".to_owned()), None, None],
    );
    assert_eq!(
        families,
        vec!["fable".to_owned()],
        "the union side is recorded, the env headline is not"
    );
}

/// A short-flag cluster: a glued `-r<id>` is an exact-id resume Claude Code
/// opens (`-r<uuid>` resumes), while `-r=<id>` hands it the value `=<id>` and a
/// `c` in the cluster is `--continue`. The first pins an agent session, the
/// other two are unpinnable.
#[test]
fn a_short_flag_cluster_glues_its_resume_value_or_continues() {
    let sb = HomeSandbox::new();
    write_resume_transcript(sb.home(), "sagent", "/w/res", &["pa"]);
    let args = |form: &[&str]| form.iter().map(|a| (*a).to_owned()).collect::<Vec<_>>();
    assert_eq!(
        resumed_agent(&args(&["-rsagent"])),
        ResumedAgent::Agent("pa".to_owned()),
        "the glued tail is the resume id"
    );
    for form in [&["-r=sagent"][..], &["-cv"][..]] {
        assert_eq!(
            resumed_agent(&args(form)),
            ResumedAgent::Unknown,
            "{form:?} cannot be pinned to a transcript"
        );
    }
}
/// A def file and a subscription env model change nothing: clauth reads no def,
/// so a def's model never reaches the demand and it stays unknown.
#[test]
fn a_def_file_never_steers_an_agent_run() {
    let home = HomeSandbox::new();
    let agents = home.home().join(".claude").join("agents");
    fs::create_dir_all(&agents).unwrap();
    fs::write(
        agents.join("rev.md"),
        "---\nname: rev\ndescription: d\nmodel: fable\n---\nYou review.\n",
    )
    .unwrap();
    let p = Profile::new("blank".to_string(), None, None);
    let demand = member_launch_demand(
        &p,
        Isolation::Shared,
        &[],
        &["--agent".to_owned(), "rev".to_owned()],
        None,
        EnvLeg::Caller,
        None,
        |k| {
            if k == "ANTHROPIC_MODEL" {
                Some("opus".to_owned())
            } else {
                None
            }
        },
    );
    assert_eq!(demand.agent_without_model.as_deref(), Some("rev"));
    assert!(
        demand.models.is_empty(),
        "no def is read: even a fable def and an opus env are unknown: {:?}",
        demand.models
    );
}

/// A settings `agent` key with an empty string is absent, never an agent run.
#[test]
fn a_blank_settings_agent_value_is_ignored() {
    let _home = HomeSandbox::new();
    let p = Profile::new("blank".to_string(), None, None);
    let demand = member_launch_demand(
        &p,
        Isolation::Shared,
        &[],
        &["--settings".to_owned(), r#"{"agent":""}"#.to_owned()],
        None,
        EnvLeg::Caller,
        None,
        |_| None,
    );
    assert_eq!(demand.agent_without_model, None);
}

/// The fallback chain a session can fall back to is ONE list, never a union:
/// Claude Code takes `--fallback-model` when given and different from
/// `--model`, else the highest settings source's `fallbackModel` (a
/// `--settings` file over the user settings), so the demand counts that list
/// alone.
#[test]
fn the_fallback_flag_replaces_the_settings_chain_and_settings_sources_do_not_union() {
    let home = HomeSandbox::new();
    let claude = home.home().join(".claude");
    fs::create_dir_all(&claude).unwrap();
    fs::write(
        claude.join("settings.json"),
        r#"{"fallbackModel":["fable"]}"#,
    )
    .unwrap();
    let passthrough = home.home().join("pass.json");
    fs::write(&passthrough, r#"{"fallbackModel":["sonnet"]}"#).unwrap();
    let p = Profile::new("blank".to_string(), None, None);
    let models = |args: &[&str]| {
        let args: Vec<String> = args.iter().map(|a| (*a).to_owned()).collect();
        member_launch_models(
            &p,
            Isolation::Shared,
            &[],
            &args,
            None,
            EnvLeg::Caller,
            None,
            |_| None,
        )
    };
    let pass = passthrough.to_str().unwrap();

    assert_eq!(
        models(&["--model", "opus", "--fallback-model", "haiku"]),
        ["opus", "haiku"],
        "the flag replaces the user settings chain"
    );
    assert_eq!(
        models(&["--model", "opus"]),
        ["opus", "fable"],
        "with no flag the user settings chain applies"
    );
    assert_eq!(
        models(&["--model", "opus", "--settings", pass]),
        ["opus", "sonnet"],
        "a --settings chain replaces the user settings chain"
    );
    assert_eq!(
        models(&[
            "--model",
            "opus",
            "--settings",
            pass,
            "--fallback-model",
            "haiku"
        ]),
        ["opus", "haiku"],
        "the flag outranks every settings chain"
    );
    assert_eq!(
        models(&["--model", "opus", "--fallback-model", "opus"]),
        ["opus", "fable"],
        "a flag equal to --model is dropped, so the settings chain applies"
    );
}

/// The fallback list follows Claude Code's own pick on every input that
/// shapes it: a repeated flag keeps its last value; the flag is dropped only
/// when its raw string equals the `--model` the child runs, a delegate's
/// `model` included; an empty flag or an empty higher settings list means no
/// fallback; the project tiers count (local over project); entries dedupe and
/// are never capped.
#[test]
fn the_fallback_list_follows_claude_codes_pick_on_every_input() {
    let home = HomeSandbox::new();
    let claude = home.home().join(".claude");
    fs::create_dir_all(&claude).unwrap();
    fs::write(
        claude.join("settings.json"),
        r#"{"fallbackModel":["fable"]}"#,
    )
    .unwrap();
    let empty = home.home().join("empty.json");
    fs::write(&empty, r#"{"fallbackModel":[]}"#).unwrap();
    let both = home.home().join("both");
    fs::create_dir_all(both.join(".claude")).unwrap();
    fs::write(
        both.join(".claude").join("settings.json"),
        r#"{"fallbackModel":["sonnet"]}"#,
    )
    .unwrap();
    fs::write(
        both.join(".claude").join("settings.local.json"),
        r#"{"fallbackModel":["haiku"]}"#,
    )
    .unwrap();
    let shared = home.home().join("shared");
    fs::create_dir_all(shared.join(".claude")).unwrap();
    fs::write(
        shared.join(".claude").join("settings.json"),
        r#"{"fallbackModel":["sonnet"]}"#,
    )
    .unwrap();
    let p = Profile::new("blank".to_string(), None, None);
    let models = |args: &[&str], delegate: Option<&str>, cwd: Option<&std::path::Path>| {
        let args: Vec<String> = args.iter().map(|a| (*a).to_owned()).collect();
        member_launch_models(
            &p,
            Isolation::Shared,
            &[],
            &args,
            delegate,
            EnvLeg::Caller,
            cwd,
            |_| None,
        )
    };
    let empty = empty.to_str().unwrap();

    assert_eq!(
        models(
            &[
                "--model",
                "opus",
                "--fallback-model",
                "haiku",
                "--fallback-model",
                "sonnet"
            ],
            None,
            None
        ),
        ["opus", "sonnet"],
        "a repeated flag keeps its last value"
    );
    assert_eq!(
        models(
            &[
                "--model",
                "opus",
                "--fallback-model",
                "haiku",
                "--fallback-model",
                "opus"
            ],
            None,
            None
        ),
        ["opus", "fable"],
        "the last flag value equal to --model is dropped, so the settings chain applies"
    );
    assert_eq!(
        models(&["--fallback-model", "opus"], Some("opus"), None),
        ["opus", "fable"],
        "a delegate's model reaches the child as --model, so an equal flag is dropped"
    );
    assert_eq!(
        models(
            &["--model", "opus", "--fallback-model", "opus,"],
            None,
            None
        ),
        ["opus", "opus"],
        "the drop compares the raw strings, so `opus,` stands"
    );
    assert_eq!(
        models(&["--model", "opus", "--fallback-model="], None, None),
        ["opus"],
        "an empty flag means no fallback"
    );
    assert_eq!(
        models(&["--model", "opus", "--settings", empty], None, None),
        ["opus"],
        "an empty --settings list replaces the user settings chain"
    );
    assert_eq!(
        models(&["--model", "opus"], None, Some(&both)),
        ["opus", "haiku"],
        "the project local tier outranks the project tier and the user settings"
    );
    assert_eq!(
        models(&["--model", "opus"], None, Some(&shared)),
        ["opus", "sonnet"],
        "the project tier outranks the user settings"
    );
    assert_eq!(
        models(
            &["--model", "opus", "--fallback-model", "haiku,haiku,sonnet"],
            None,
            None
        ),
        ["opus", "haiku", "sonnet"],
        "entries dedupe"
    );
    assert_eq!(
        models(
            &[
                "--model",
                "opus",
                "--fallback-model",
                "haiku,sonnet,fable,claude-sonnet-4-5"
            ],
            None,
            None
        ),
        ["opus", "haiku", "sonnet", "fable", "claude-sonnet-4-5"],
        "no cap: an entry Claude Code may filter out costs a candidate, never a session"
    );
}

/// The flag parse and the drop follow Claude Code's own argv reading: a
/// `--model` with a flag-looking next word takes it as its value (so no
/// fallback flag was given); a blank later `--model` is the model the child
/// runs, so a flag equal to an earlier one stands; a `default` entry counts
/// what an unnamed `--model` would (the member's own default); a local `[]`
/// blocks the project tier.
#[test]
fn the_fallback_list_reads_argv_and_default_as_claude_code_does() {
    let home = HomeSandbox::new();
    let claude = home.home().join(".claude");
    fs::create_dir_all(&claude).unwrap();
    fs::write(
        claude.join("settings.json"),
        r#"{"fallbackModel":["fable"]}"#,
    )
    .unwrap();
    let blocked = home.home().join("blocked");
    fs::create_dir_all(blocked.join(".claude")).unwrap();
    fs::write(
        blocked.join(".claude").join("settings.json"),
        r#"{"fallbackModel":["sonnet"]}"#,
    )
    .unwrap();
    fs::write(
        blocked.join(".claude").join("settings.local.json"),
        r#"{"fallbackModel":[]}"#,
    )
    .unwrap();
    let blank = Profile::new("blank".to_string(), None, None);
    let mut own = Profile::new("own".to_string(), None, None);
    own.models.default = Some("fable".to_string());
    let models = |p: &Profile, args: &[&str], cwd: Option<&std::path::Path>| {
        let args: Vec<String> = args.iter().map(|a| (*a).to_owned()).collect();
        member_launch_models(
            p,
            Isolation::Shared,
            &[],
            &args,
            None,
            EnvLeg::Caller,
            cwd,
            |_| None,
        )
    };

    assert_eq!(
        models(&blank, &["--model", "--fallback-model", "x"], None),
        ["--fallback-model", "fable"],
        "`--model` takes the next word as its value, so the settings chain applies"
    );
    assert_eq!(
        models(
            &blank,
            &["--model", "opus", "--model", "", "--fallback-model", "opus"],
            None
        ),
        ["opus"],
        "the child runs the blank last --model, so the equal-to-an-earlier flag stands"
    );
    assert_eq!(
        models(
            &own,
            &["--model", "haiku", "--fallback-model", "default"],
            None
        ),
        ["haiku", "fable"],
        "a `default` entry counts the member's own default model"
    );
    assert_eq!(
        models(&blank, &["--model", "opus"], Some(&blocked)),
        ["opus"],
        "a project-local empty list blocks the project tier and the user settings"
    );
}

/// The isolated runtime's overlay holds no agent key: the sandbox user
/// `settings.json` agent is ignored there.
#[test]
fn an_isolated_runtime_ignores_the_user_settings_agent_key() {
    let home = HomeSandbox::new();
    fs::create_dir_all(home.home().join(".claude")).unwrap();
    fs::write(
        home.home().join(".claude").join("settings.json"),
        r#"{"agent":"rev"}"#,
    )
    .unwrap();
    let p = Profile::new("blank".to_string(), None, None);
    let demand = member_launch_demand(
        &p,
        Isolation::Isolated,
        &[],
        &[],
        None,
        EnvLeg::Caller,
        None,
        |_| None,
    );
    assert_eq!(
        demand.agent_without_model, None,
        "the isolated overlay holds no agent key"
    );
}

/// The shared runtime's overlay reads the user settings `agent` key, which
/// makes an agent run with no named model.
#[test]
fn an_overlay_settings_agent_key_is_the_sessions_agent() {
    let home = HomeSandbox::new();
    fs::create_dir_all(home.home().join(".claude")).unwrap();
    fs::write(
        home.home().join(".claude").join("settings.json"),
        r#"{"agent":"rev"}"#,
    )
    .unwrap();
    let p = Profile::new("blank".to_string(), None, None);
    let demand = member_launch_demand(
        &p,
        Isolation::Shared,
        &[],
        &[],
        None,
        EnvLeg::Caller,
        None,
        |_| None,
    );
    assert_eq!(demand.agent_without_model.as_deref(), Some("rev"));
    assert!(demand.models.is_empty());
}

/// The runtime tree a `--bg` launch handed to Claude Code's background
/// service, as the fake-`claude` shim recorded it.
#[cfg(unix)]
fn background_tree(shim: &crate::testutil::FakeClaude) -> std::path::PathBuf {
    shim.spawns()
        .iter()
        .find(|(_, argv)| argv.split(' ').any(|word| word == "--bg"))
        .map(|(dir, _)| std::path::PathBuf::from(dir))
        .expect("one background launch")
}

/// collecting a background tree first
/// SIGTERMs the tree's OWN supervisor, verified by the pid its `daemon.lock`
/// records — its recorded start time, `daemon run` argv, and `CLAUDE_CONFIG_DIR`
/// = the tree — never through the `claude daemon stop` CLI, which would also
/// stop an installed home service. A tree with no supervisor files collects
/// with nothing signalled.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn collecting_a_background_tree_stops_its_supervisor_first() {
    for posed in [true, false] {
        let sb = crate::testutil::HomeSandbox::new();
        let claude = sb.home().join(".claude");
        fs::create_dir_all(&claude).expect("~/.claude");
        let _config_dir = crate::testutil::ConfigDirSandbox::new(&sb, &claude);
        let shim = crate::testutil::FakeClaude::new(&sb);
        let clauth_dir = crate::profile::clauth_dir().expect("clauth dir");
        fs::create_dir_all(&clauth_dir).expect("clauth dir");
        fs::write(
            clauth_dir.join("profiles.toml"),
            "profiles = [\"wired\", \"spare\"]\n",
        )
        .expect("profiles.toml");

        run(
            &chain_ready_config("wired"),
            &crate::profile::ProfileName::from("wired"),
            &["--bg".to_string()],
            Isolation::Shared,
            None,
            false,
            None,
        )
        .expect("background start");
        let tree = background_tree(&shim);
        assert!(tree.is_dir(), "posed: {posed}: the job keeps the tree");

        let supervisor = posed.then(|| crate::testutil::FakeSupervisor::spawn(&tree, false));
        if let Some(sup) = &supervisor {
            crate::testutil::write_daemon_lock(&tree, sup.pid, &sup.proc_start);
        }

        // `claude rm`: the job dir goes, the supervisor (posed above) stays.
        for job in fs::read_dir(claude.join("jobs")).expect("jobs").flatten() {
            fs::remove_dir_all(job.path()).expect("claude rm");
        }

        crate::runtime::gc_stale_runtimes();

        // The stop never goes through the `claude` CLI (which the shim would
        // log); the posed supervisor is SIGTERMed and reaped.
        let stops: Vec<String> = shim
            .spawns()
            .into_iter()
            .map(|(_, argv)| argv)
            .filter(|argv| argv.contains("daemon stop"))
            .collect();
        assert!(
            stops.is_empty(),
            "posed: {posed}: no `claude daemon stop` CLI spawn: {stops:?}"
        );
        if let Some(mut sup) = supervisor {
            let status = crate::testutil::reap_bounded(&mut sup.child);
            assert_eq!(
                std::os::unix::process::ExitStatusExt::signal(&status),
                Some(libc::SIGTERM),
                "posed: {posed}: the supervisor was SIGTERMed exactly"
            );
        }
        assert!(!tree.exists(), "posed: {posed}: the tree was collected");
    }
}

/// a `daemon.lock` that is a SYMLINK was linked by an older build to
/// the shared home's lock, whose supervisor used the home roster — it names no
/// private supervisor, so the tree collects with no stop and no spare line,
/// and `remove_dir_all` unlinks the link, leaving the target intact.
#[cfg(unix)]
#[test]
fn collecting_a_tree_with_a_symlinked_daemon_lock_collects_without_a_signal() {
    let sb = crate::testutil::HomeSandbox::new();
    let claude = sb.home().join(".claude");
    fs::create_dir_all(&claude).expect("~/.claude");
    let _config_dir = crate::testutil::ConfigDirSandbox::new(&sb, &claude);
    let shim = crate::testutil::FakeClaude::new(&sb);
    let clauth_dir = crate::profile::clauth_dir().expect("clauth dir");
    fs::create_dir_all(&clauth_dir).expect("clauth dir");
    fs::write(
        clauth_dir.join("profiles.toml"),
        "profiles = [\"wired\", \"spare\"]\n",
    )
    .expect("profiles.toml");

    run(
        &chain_ready_config("wired"),
        &crate::profile::ProfileName::from("wired"),
        &["--bg".to_string()],
        Isolation::Shared,
        None,
        false,
        None,
    )
    .expect("background start");
    let tree = background_tree(&shim);
    let target = sb.home().join("lock-target");
    fs::write(&target, b"{}").expect("the symlink target");
    std::os::unix::fs::symlink(&target, tree.join("daemon.lock")).expect("symlinked daemon.lock");
    for job in fs::read_dir(claude.join("jobs")).expect("jobs").flatten() {
        fs::remove_dir_all(job.path()).expect("claude rm");
    }

    let lines = crate::logline::LogLines::new();
    let _capture = lines.capture_here();
    crate::runtime::gc_stale_runtimes();

    let stops: Vec<String> = shim
        .spawns()
        .into_iter()
        .map(|(_, argv)| argv)
        .filter(|argv| argv.contains("daemon stop"))
        .collect();
    assert!(
        stops.is_empty(),
        "a symlinked daemon.lock must never stop the shared home roster: {stops:?}"
    );
    assert!(!tree.exists(), "the tree was still collected");
    assert!(
        target.is_file(),
        "the symlink target survived the collection"
    );
    assert!(
        lines.snapshot().is_empty(),
        "a symlinked lock collects with no spare line: {:?}",
        lines.snapshot()
    );
}

/// a `daemon.lock` whose pid was recycled (a live process with a
/// different field-22 start) means the recorded supervisor is GONE and the pid
/// is reused — the tree collects and the reused pid is never signalled. The
/// start-time compare is the guard: removing it would let a reused pid fall
/// through to the argv/env checks (a `sleep` fails both) and spare the tree.
/// Linux-only: the reused start is a field-22 `/proc/<pid>/stat` spelling,
/// which macOS's asctime `procStart` form does not share.
#[cfg(target_os = "linux")]
#[test]
fn collecting_a_tree_whose_lock_names_a_reused_pid_collects_without_a_signal() {
    let sb = crate::testutil::HomeSandbox::new();
    let claude = sb.home().join(".claude");
    fs::create_dir_all(&claude).expect("~/.claude");
    let _config_dir = crate::testutil::ConfigDirSandbox::new(&sb, &claude);
    let shim = crate::testutil::FakeClaude::new(&sb);
    let clauth_dir = crate::profile::clauth_dir().expect("clauth dir");
    fs::create_dir_all(&clauth_dir).expect("clauth dir");
    fs::write(
        clauth_dir.join("profiles.toml"),
        "profiles = [\"wired\", \"spare\"]\n",
    )
    .expect("profiles.toml");

    run(
        &chain_ready_config("wired"),
        &crate::profile::ProfileName::from("wired"),
        &["--bg".to_string()],
        Isolation::Shared,
        None,
        false,
        None,
    )
    .expect("background start");
    let tree = background_tree(&shim);
    // A live process holding the pid, with the lock recording the WRONG start.
    let mut reused = crate::testutil::Owned(
        Command::new("sleep")
            .args(["100000"])
            .spawn()
            .expect("spawn the reused pid"),
    );
    crate::testutil::write_daemon_lock(&tree, reused.0.id(), "0");
    for job in fs::read_dir(claude.join("jobs")).expect("jobs").flatten() {
        fs::remove_dir_all(job.path()).expect("claude rm");
    }

    crate::runtime::gc_stale_runtimes();

    assert!(
        !tree.exists(),
        "a reused pid means the supervisor is gone; the tree collects"
    );
    // The reused `sleep` is untouched: the Gone path never signals.
    assert!(
        reused.0.try_wait().is_ok_and(|status| status.is_none()),
        "the reused pid must not be signalled"
    );
}

/// a `daemon.lock` naming a pid that no longer exists is a
/// crashed supervisor whose tree must COLLECT — `kill(0)` false is the only
/// positively-dead reading, and dropping that check would fall through to an
/// unreadable live-process read and spare the tree forever.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn collecting_a_tree_whose_lock_names_a_dead_pid_collects() {
    let sb = crate::testutil::HomeSandbox::new();
    let claude = sb.home().join(".claude");
    fs::create_dir_all(&claude).expect("~/.claude");
    let _config_dir = crate::testutil::ConfigDirSandbox::new(&sb, &claude);
    let shim = crate::testutil::FakeClaude::new(&sb);
    let clauth_dir = crate::profile::clauth_dir().expect("clauth dir");
    fs::create_dir_all(&clauth_dir).expect("clauth dir");
    fs::write(
        clauth_dir.join("profiles.toml"),
        "profiles = [\"wired\", \"spare\"]\n",
    )
    .expect("profiles.toml");

    run(
        &chain_ready_config("wired"),
        &crate::profile::ProfileName::from("wired"),
        &["--bg".to_string()],
        Isolation::Shared,
        None,
        false,
        None,
    )
    .expect("background start");
    let tree = background_tree(&shim);
    // A real child that has exited and been reaped: the pid is positively gone.
    let mut dead = Command::new("true").spawn().expect("spawn true");
    let dead_pid = dead.id();
    let start = crate::testutil::proc_start_of(dead_pid).expect("the dead pid's live start");
    let _ = dead.wait();
    crate::testutil::write_daemon_lock(&tree, dead_pid, &start);
    for job in fs::read_dir(claude.join("jobs")).expect("jobs").flatten() {
        fs::remove_dir_all(job.path()).expect("claude rm");
    }

    crate::runtime::gc_stale_runtimes();

    assert!(
        !tree.exists(),
        "a lock naming a dead pid means the supervisor is gone; the tree collects"
    );
}

/// a live pid whose `/proc/<pid>/stat` cannot be read (hidepid,
/// `EACCES`) spares the tree — never signalled, never collected. Posed through
/// the `UnreadableProcStatGuard` seam, since a parent's own live child's stat
/// is always readable here. Linux-only: the seam forces a `/proc/<pid>/stat`
/// read, which macOS replaces with a `proc_bsdinfo` read.
#[cfg(target_os = "linux")]
#[test]
fn collecting_a_tree_whose_supervisor_stat_is_unreadable_spares_it() {
    let sb = crate::testutil::HomeSandbox::new();
    let claude = sb.home().join(".claude");
    fs::create_dir_all(&claude).expect("~/.claude");
    let _config_dir = crate::testutil::ConfigDirSandbox::new(&sb, &claude);
    let shim = crate::testutil::FakeClaude::new(&sb);
    let clauth_dir = crate::profile::clauth_dir().expect("clauth dir");
    fs::create_dir_all(&clauth_dir).expect("clauth dir");
    fs::write(
        clauth_dir.join("profiles.toml"),
        "profiles = [\"wired\", \"spare\"]\n",
    )
    .expect("profiles.toml");

    run(
        &chain_ready_config("wired"),
        &crate::profile::ProfileName::from("wired"),
        &["--bg".to_string()],
        Isolation::Shared,
        None,
        false,
        None,
    )
    .expect("background start");
    let tree = background_tree(&shim);
    let mut sup = crate::testutil::FakeSupervisor::spawn(&tree, false);
    crate::testutil::write_daemon_lock(&tree, sup.pid, &sup.proc_start);
    for job in fs::read_dir(claude.join("jobs")).expect("jobs").flatten() {
        fs::remove_dir_all(job.path()).expect("claude rm");
    }

    let _seam = crate::runtime::UnreadableProcStatGuard::set(sup.pid);
    let lines = crate::logline::LogLines::new();
    let _capture = lines.capture_here();
    crate::runtime::gc_stale_runtimes();

    assert!(
        tree.exists(),
        "an unreadable stat must spare the tree, never collect it"
    );
    assert!(
        sup.child.try_wait().is_ok_and(|status| status.is_none()),
        "an unreadable stat must never signal the pid"
    );
    assert_eq!(
        lines.snapshot(),
        vec![format!(
            "clauth: {}: sparing: its supervisor could not be verified as this tree's own",
            tree.display()
        )],
        "the unreadable-stat spare logged exactly one line"
    );
}

/// a supervisor whose `CLAUDE_CONFIG_DIR` resolves elsewhere is
/// another tree's — never signalled, the tree is spared.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn collecting_a_tree_whose_supervisor_config_dir_differs_spares_it() {
    let sb = crate::testutil::HomeSandbox::new();
    let claude = sb.home().join(".claude");
    fs::create_dir_all(&claude).expect("~/.claude");
    let _config_dir = crate::testutil::ConfigDirSandbox::new(&sb, &claude);
    let shim = crate::testutil::FakeClaude::new(&sb);
    let clauth_dir = crate::profile::clauth_dir().expect("clauth dir");
    fs::create_dir_all(&clauth_dir).expect("clauth dir");
    fs::write(
        clauth_dir.join("profiles.toml"),
        "profiles = [\"wired\", \"spare\"]\n",
    )
    .expect("profiles.toml");

    run(
        &chain_ready_config("wired"),
        &crate::profile::ProfileName::from("wired"),
        &["--bg".to_string()],
        Isolation::Shared,
        None,
        false,
        None,
    )
    .expect("background start");
    let tree = background_tree(&shim);
    let other = sb.home().join("other-tree");
    fs::create_dir_all(&other).expect("the other tree");
    let sup = crate::testutil::FakeSupervisor::spawn(&other, false);
    crate::testutil::write_daemon_lock(&tree, sup.pid, &sup.proc_start);
    for job in fs::read_dir(claude.join("jobs")).expect("jobs").flatten() {
        fs::remove_dir_all(job.path()).expect("claude rm");
    }

    let lines = crate::logline::LogLines::new();
    let _capture = lines.capture_here();
    crate::runtime::gc_stale_runtimes();

    assert!(
        tree.exists(),
        "a supervisor keyed on another config dir must not be signalled; the tree is spared"
    );
    assert_eq!(
        lines.snapshot(),
        vec![format!(
            "clauth: {}: sparing: its supervisor could not be verified as this tree's own",
            tree.display()
        )],
        "the unverifiable supervisor logged exactly one line"
    );
}

/// a verified supervisor that ignores SIGTERM spares the tree at the
/// deadline, with exactly one log line.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn collecting_a_tree_whose_supervisor_ignores_sigterm_spares_it() {
    let sb = crate::testutil::HomeSandbox::new();
    let claude = sb.home().join(".claude");
    fs::create_dir_all(&claude).expect("~/.claude");
    let _config_dir = crate::testutil::ConfigDirSandbox::new(&sb, &claude);
    let shim = crate::testutil::FakeClaude::new(&sb);
    let clauth_dir = crate::profile::clauth_dir().expect("clauth dir");
    fs::create_dir_all(&clauth_dir).expect("clauth dir");
    fs::write(
        clauth_dir.join("profiles.toml"),
        "profiles = [\"wired\", \"spare\"]\n",
    )
    .expect("profiles.toml");

    run(
        &chain_ready_config("wired"),
        &crate::profile::ProfileName::from("wired"),
        &["--bg".to_string()],
        Isolation::Shared,
        None,
        false,
        None,
    )
    .expect("background start");
    let tree = background_tree(&shim);
    let sup = crate::testutil::FakeSupervisor::spawn(&tree, true);
    crate::testutil::write_daemon_lock(&tree, sup.pid, &sup.proc_start);
    for job in fs::read_dir(claude.join("jobs")).expect("jobs").flatten() {
        fs::remove_dir_all(job.path()).expect("claude rm");
    }

    let _deadline =
        crate::runtime::SupervisorStopDeadlineGuard::set(std::time::Duration::from_millis(300));
    let lines = crate::logline::LogLines::new();
    let _capture = lines.capture_here();
    crate::runtime::gc_stale_runtimes();

    assert!(
        tree.exists(),
        "a supervisor that ignores SIGTERM spares the tree at the deadline"
    );
    assert_eq!(
        lines.snapshot(),
        vec![format!(
            "clauth: {}: sparing: its supervisor pid {} is still alive 15s after SIGTERM",
            tree.display(),
            sup.pid
        )],
        "the still-alive supervisor logged exactly one line"
    );
}

/// a `daemon` SYMLINK was linked by an older build to
/// the shared `~/.claude/daemon`; it reads as no private supervisor, so the
/// tree collects with no signal and `remove_dir_all` unlinks the link, leaving
/// the shared target intact.
#[cfg(unix)]
#[test]
fn collecting_a_tree_with_a_symlinked_daemon_dir_collects_with_no_signal() {
    let sb = crate::testutil::HomeSandbox::new();
    let claude = sb.home().join(".claude");
    fs::create_dir_all(&claude).expect("~/.claude");
    let _config_dir = crate::testutil::ConfigDirSandbox::new(&sb, &claude);
    let shim = crate::testutil::FakeClaude::new(&sb);
    let clauth_dir = crate::profile::clauth_dir().expect("clauth dir");
    fs::create_dir_all(&clauth_dir).expect("clauth dir");
    fs::write(
        clauth_dir.join("profiles.toml"),
        "profiles = [\"wired\", \"spare\"]\n",
    )
    .expect("profiles.toml");

    run(
        &chain_ready_config("wired"),
        &crate::profile::ProfileName::from("wired"),
        &["--bg".to_string()],
        Isolation::Shared,
        None,
        false,
        None,
    )
    .expect("background start");
    let tree = background_tree(&shim);
    let target = sb.home().join("shared-daemon");
    fs::create_dir_all(&target).expect("the symlink target");
    std::os::unix::fs::symlink(&target, tree.join("daemon")).expect("symlinked daemon");
    for job in fs::read_dir(claude.join("jobs")).expect("jobs").flatten() {
        fs::remove_dir_all(job.path()).expect("claude rm");
    }

    crate::runtime::gc_stale_runtimes();

    let stops: Vec<String> = shim
        .spawns()
        .into_iter()
        .map(|(_, argv)| argv)
        .filter(|argv| argv.contains("daemon stop"))
        .collect();
    assert!(
        stops.is_empty(),
        "a symlinked daemon must never stop the shared roster: {stops:?}"
    );
    assert!(!tree.exists(), "the tree was still collected");
    assert!(
        target.is_dir(),
        "the symlink target survived the collection"
    );
}

/// teardown (`Drop`) SIGTERMs a posed
/// supervisor before it collects the tree — the stop-before-remove order, not
/// a removal the stop should precede.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn teardown_stops_a_posed_supervisor_before_collecting_the_tree() {
    let sb = crate::testutil::HomeSandbox::new();
    let claude = sb.home().join(".claude");
    fs::create_dir_all(&claude).expect("~/.claude");
    let _config_dir = crate::testutil::ConfigDirSandbox::new(&sb, &claude);
    let _shim = crate::testutil::FakeClaude::new(&sb);
    let clauth_dir = crate::profile::clauth_dir().expect("clauth dir");
    fs::create_dir_all(&clauth_dir).expect("clauth dir");
    fs::write(
        clauth_dir.join("profiles.toml"),
        "profiles = [\"wired\", \"spare\"]\n",
    )
    .expect("profiles.toml");

    let config = chain_ready_config("wired");
    let profile = admit(
        &config,
        &crate::profile::ProfileName::from("wired"),
        Isolation::Shared,
        false,
        &[],
    )
    .expect("admit");
    let rt = crate::runtime::ProfileRuntime::acquire(
        profile,
        Isolation::Shared,
        &[],
        false,
        crate::live_sessions::Launcher::Own,
    )
    .expect("acquire");
    let tree = rt.config_dir().to_path_buf();
    let mut sup = crate::testutil::FakeSupervisor::spawn(&tree, false);
    crate::testutil::write_daemon_lock(&tree, sup.pid, &sup.proc_start);

    drop(rt);

    let status = crate::testutil::reap_bounded(&mut sup.child);
    assert_eq!(
        std::os::unix::process::ExitStatusExt::signal(&status),
        Some(libc::SIGTERM),
        "the teardown SIGTERMed the tree's supervisor"
    );
    assert!(!tree.exists(), "the teardown collected the tree");
}

/// a supervisor that ignores SIGTERM spares the tree at
/// the deadline — the Drop twin of the failed-stop spare.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn teardown_spares_a_tree_whose_supervisor_ignores_sigterm() {
    let sb = crate::testutil::HomeSandbox::new();
    let claude = sb.home().join(".claude");
    fs::create_dir_all(&claude).expect("~/.claude");
    let _config_dir = crate::testutil::ConfigDirSandbox::new(&sb, &claude);
    let _shim = crate::testutil::FakeClaude::new(&sb);
    let clauth_dir = crate::profile::clauth_dir().expect("clauth dir");
    fs::create_dir_all(&clauth_dir).expect("clauth dir");
    fs::write(
        clauth_dir.join("profiles.toml"),
        "profiles = [\"wired\", \"spare\"]\n",
    )
    .expect("profiles.toml");

    let config = chain_ready_config("wired");
    let profile = admit(
        &config,
        &crate::profile::ProfileName::from("wired"),
        Isolation::Shared,
        false,
        &[],
    )
    .expect("admit");
    let rt = crate::runtime::ProfileRuntime::acquire(
        profile,
        Isolation::Shared,
        &[],
        false,
        crate::live_sessions::Launcher::Own,
    )
    .expect("acquire");
    let tree = rt.config_dir().to_path_buf();
    let sup = crate::testutil::FakeSupervisor::spawn(&tree, true);
    crate::testutil::write_daemon_lock(&tree, sup.pid, &sup.proc_start);

    let _deadline =
        crate::runtime::SupervisorStopDeadlineGuard::set(std::time::Duration::from_millis(300));
    let lines = crate::logline::LogLines::new();
    let _capture = lines.capture_here();
    drop(rt);

    assert!(
        tree.exists(),
        "a supervisor that ignores SIGTERM spares the tree at the deadline"
    );
    assert_eq!(
        lines.snapshot(),
        vec![format!(
            "clauth: {}: sparing: its supervisor pid {} is still alive 15s after SIGTERM",
            tree.display(),
            sup.pid
        )],
        "the still-alive supervisor logged exactly one line"
    );
}

/// a live pid with the recorded start time and the
/// tree's config dir, but argv that does NOT hold `daemon run`, is another
/// process holding the pid — never signalled, the tree is spared. The argv
/// guard is the only remaining gate once start and config dir agree, so
/// removing it would SIGTERM the live pid and collect the tree.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn collecting_a_tree_whose_supervisor_lacks_daemon_run_argv_spares_it() {
    let sb = crate::testutil::HomeSandbox::new();
    let claude = sb.home().join(".claude");
    fs::create_dir_all(&claude).expect("~/.claude");
    let _config_dir = crate::testutil::ConfigDirSandbox::new(&sb, &claude);
    let shim = crate::testutil::FakeClaude::new(&sb);
    let clauth_dir = crate::profile::clauth_dir().expect("clauth dir");
    fs::create_dir_all(&clauth_dir).expect("clauth dir");
    fs::write(
        clauth_dir.join("profiles.toml"),
        "profiles = [\"wired\", \"spare\"]\n",
    )
    .expect("profiles.toml");

    run(
        &chain_ready_config("wired"),
        &crate::profile::ProfileName::from("wired"),
        &["--bg".to_string()],
        Isolation::Shared,
        None,
        false,
        None,
    )
    .expect("background start");
    let tree = background_tree(&shim);
    // A live process with the right config dir and the recorded (real) start,
    // but argv that is NOT `daemon run`.
    let mut foreign = crate::testutil::spawn_parked(&tree, &["not", "a", "supervisor"]);
    let start = crate::testutil::proc_start_of(foreign.0.id()).expect("the foreign start");
    crate::testutil::write_daemon_lock(&tree, foreign.0.id(), &start);
    for job in fs::read_dir(claude.join("jobs")).expect("jobs").flatten() {
        fs::remove_dir_all(job.path()).expect("claude rm");
    }

    let lines = crate::logline::LogLines::new();
    let _capture = lines.capture_here();
    crate::runtime::gc_stale_runtimes();

    assert!(
        tree.exists(),
        "a pid without `daemon run` argv must not be signalled; the tree is spared"
    );
    assert!(
        foreign.0.try_wait().is_ok_and(|status| status.is_none()),
        "the foreign pid must not be signalled"
    );
    assert_eq!(
        lines.snapshot(),
        vec![format!(
            "clauth: {}: sparing: its supervisor could not be verified as this tree's own",
            tree.display()
        )],
        "the unverifiable supervisor logged exactly one line"
    );
}

/// a tree spared every pass logs its spare line ONCE per process
/// — two GC passes over the same unverifiable tree produce one line, not one
/// per pass.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn a_spared_tree_announces_once_across_passes() {
    let sb = crate::testutil::HomeSandbox::new();
    let claude = sb.home().join(".claude");
    fs::create_dir_all(&claude).expect("~/.claude");
    let _config_dir = crate::testutil::ConfigDirSandbox::new(&sb, &claude);
    let shim = crate::testutil::FakeClaude::new(&sb);
    let clauth_dir = crate::profile::clauth_dir().expect("clauth dir");
    fs::create_dir_all(&clauth_dir).expect("clauth dir");
    fs::write(
        clauth_dir.join("profiles.toml"),
        "profiles = [\"wired\", \"spare\"]\n",
    )
    .expect("profiles.toml");

    run(
        &chain_ready_config("wired"),
        &crate::profile::ProfileName::from("wired"),
        &["--bg".to_string()],
        Isolation::Shared,
        None,
        false,
        None,
    )
    .expect("background start");
    let tree = background_tree(&shim);
    // A live pid with the recorded start time but argv that is not `daemon run`
    // — unverifiable, spared on every pass. (A symlink no longer spares: it
    // reads as absent and the tree collects.)
    let foreign = crate::testutil::Owned(
        Command::new("sleep")
            .args(["100000"])
            .env("CLAUDE_CONFIG_DIR", &tree)
            .spawn()
            .expect("spawn foreign pid"),
    );
    let start = crate::testutil::proc_start_of(foreign.0.id()).expect("the live start");
    crate::testutil::write_daemon_lock(&tree, foreign.0.id(), &start);
    for job in fs::read_dir(claude.join("jobs")).expect("jobs").flatten() {
        fs::remove_dir_all(job.path()).expect("claude rm");
    }

    let lines = crate::logline::LogLines::new();
    let _capture = lines.capture_here();
    crate::runtime::gc_stale_runtimes();
    crate::runtime::gc_stale_runtimes();

    assert_eq!(
        lines.snapshot(),
        vec![format!(
            "clauth: {}: sparing: its supervisor could not be verified as this tree's own",
            tree.display()
        )],
        "two passes over one spared tree logged exactly one line"
    );
}

// ── the unknown demand an unresolvable default or no-model launch carries ──

/// The raw model strings a launch may run, with the given member `[models]
/// default`, args and delegate `model`, under a user settings fallback of
/// fable.
fn demand_models(default: Option<&str>, args: &[&str], delegate: Option<&str>) -> Vec<String> {
    let home = HomeSandbox::new();
    let claude = home.home().join(".claude");
    fs::create_dir_all(&claude).unwrap();
    fs::write(
        claude.join("settings.json"),
        r#"{"fallbackModel":["fable"]}"#,
    )
    .unwrap();
    let mut profile = Profile::new("p".to_string(), None, None);
    profile.models.default = default.map(str::to_owned);
    let args: Vec<String> = args.iter().map(|a| (*a).to_owned()).collect();
    member_launch_models(
        &profile,
        Isolation::Shared,
        &[],
        &args,
        delegate,
        EnvLeg::Caller,
        None,
        |_| None,
    )
}

/// A delegate `model` plus a blank `--model` in the args follows what the child
/// runs: the member's default, else the unknown demand's fallback side alone.
#[test]
fn a_delegate_model_follows_a_blank_args_model() {
    assert_eq!(
        demand_models(Some("sonnet"), &["--model", ""], Some("opus")),
        ["sonnet", "fable"],
        "the blank args model names no model, so the member default is the headline"
    );
    assert_eq!(
        demand_models(None, &["--model", ""], Some("opus")),
        ["fable"],
        "with no member default the headline is unknown and the fallback side remains"
    );
}

/// The drop compares the fallback flag against the last RAW `--model` (a blank
/// kept), never the headline (absent when the model is blank): a flag equal to
/// the blank model is dropped, so the settings chain applies.
#[test]
fn the_drop_compares_against_the_last_raw_model_not_the_headline() {
    assert_eq!(
        demand_models(None, &["--model", "", "--fallback-model="], Some("opus")),
        ["fable"],
        "the empty flag equals the blank model the child runs, so the settings chain applies"
    );
}

/// A `default` entry with no member `[models] default` stays literal: it is the
/// unknown demand, never dropped (a dropped entry would read as a known launch).
#[test]
fn a_default_without_member_default_stays_literal() {
    assert_eq!(
        demand_models(
            None,
            &["--model", "haiku", "--fallback-model", "default"],
            None
        ),
        ["haiku", "default"]
    );
}

// ── advisor model in the launch demand ───────────────────────────────────────

/// A launch's demand models for the advisor tests: `args` as the passthrough,
/// `env` as the inherited or caller env (`leg`), `stale` as the outgoing
/// profile's stale env keys, under `~/.claude/settings.json` as the shared
/// base.
fn advisor_demand(
    profile: &Profile,
    args: &[&str],
    cwd: Option<&std::path::Path>,
    leg: EnvLeg,
    stale: &[&str],
    env: &[(&str, &str)],
) -> Vec<String> {
    let args: Vec<String> = args.iter().map(|a| (*a).to_owned()).collect();
    let stale: Vec<String> = stale.iter().map(|k| (*k).to_owned()).collect();
    let env: Vec<(String, String)> = env
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect();
    member_launch_models(
        profile,
        Isolation::Shared,
        &stale,
        &args,
        None,
        leg,
        cwd,
        |key| env.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone()),
    )
}

/// A launch with no env, no stale keys and no project dir.
fn plain_advisor_demand(profile: &Profile, args: &[&str]) -> Vec<String> {
    advisor_demand(profile, args, None, EnvLeg::Caller, &[], &[])
}

/// A project dir under the sandbox home holding `.claude/settings.json` and
/// `.claude/settings.local.json` bodies (`None` writes no file).
fn project_with(
    home: &HomeSandbox,
    name: &str,
    shared: Option<&str>,
    local: Option<&str>,
) -> std::path::PathBuf {
    let dir = home.home().join(name);
    fs::create_dir_all(dir.join(".claude")).unwrap();
    if let Some(body) = shared {
        fs::write(dir.join(".claude").join("settings.json"), body).unwrap();
    }
    if let Some(body) = local {
        fs::write(dir.join(".claude").join("settings.local.json"), body).unwrap();
    }
    dir
}

fn profile_with_advisor(advisor: Option<&str>) -> Profile {
    let mut p = Profile::new("adv".to_string(), None, None);
    p.models.advisor = advisor.map(str::to_owned);
    p
}

/// The advisor joins the demand beside the headline, from the last non-blank
/// `--advisor` in either spelling, and only where claude would read the flag
/// as the advisor flag.
#[test]
fn the_advisor_flag_joins_the_demand() {
    let _home = HomeSandbox::new();
    let p = profile_with_advisor(None);
    assert_eq!(
        plain_advisor_demand(&p, &["--model", "sonnet", "--advisor", "opus"]),
        ["sonnet", "opus"]
    );
    assert_eq!(
        plain_advisor_demand(&p, &["--model", "sonnet", "--advisor=opus"]),
        ["sonnet", "opus"],
        "the equals spelling reads the same"
    );
    assert_eq!(
        plain_advisor_demand(
            &p,
            &["--model", "sonnet", "--advisor", "haiku", "--advisor=opus"]
        ),
        ["sonnet", "opus"],
        "the last flag wins, across spellings"
    );
    assert_eq!(
        plain_advisor_demand(&p, &["--model", "sonnet", "--advisor", ""]),
        ["sonnet"],
        "a blank value names no model"
    );
    assert_eq!(
        plain_advisor_demand(
            &p,
            &["--model", "sonnet", "--advisor", "opus", "--advisor", ""]
        ),
        ["sonnet", "opus"],
        "a blank later flag does not clear an earlier one"
    );
    assert_eq!(
        plain_advisor_demand(&p, &["--model", "--advisor", "opus"]),
        ["--advisor"],
        "`--model` takes `--advisor` as its value, so no advisor flag was given"
    );
    assert_eq!(
        plain_advisor_demand(&p, &["--model", "sonnet", "--name", "--advisor", "opus"]),
        ["sonnet"],
        "`--name` takes `--advisor` as its value"
    );
    assert_eq!(
        plain_advisor_demand(&p, &["--model", "sonnet", "--advisor"]),
        ["sonnet"],
        "a trailing `--advisor` has no value"
    );
    assert_eq!(
        plain_advisor_demand(&p, &["--model", "sonnet", "--", "--advisor", "opus"]),
        ["sonnet"],
        "nothing past the separator is a flag"
    );
}

/// A blank `--advisor` shadows nothing: the settings tiers still apply.
#[test]
fn a_blank_advisor_flag_leaves_the_settings_tiers_in_force() {
    let home = HomeSandbox::new();
    let project = project_with(&home, "proj", Some(r#"{"advisorModel":"haiku"}"#), None);
    let p = profile_with_advisor(None);
    let demand = |args: &[&str]| advisor_demand(&p, args, Some(&project), EnvLeg::Caller, &[], &[]);
    assert_eq!(
        demand(&["--model", "sonnet", "--advisor", ""]),
        ["sonnet", "haiku"]
    );
    assert_eq!(
        demand(&["--model", "sonnet", "--advisor", "opus"]),
        ["sonnet", "opus"],
        "a non-blank flag outranks the project tier"
    );
}

/// Each settings tier supplies the advisor in Claude Code's order: the
/// `--settings` passthrough, then the project's local file, its shared file,
/// then the overlay (the profile's `[models] advisor` row), with the flag over
/// all of them.
#[test]
fn the_advisor_model_is_read_from_each_settings_tier_in_precedence_order() {
    let home = HomeSandbox::new();
    let inline = r#"{"advisorModel":"fable"}"#;
    let file = home.home().join("pass.json");
    fs::write(&file, r#"{"advisorModel":"fable"}"#).unwrap();
    let both = project_with(
        &home,
        "both",
        Some(r#"{"advisorModel":"sonnet"}"#),
        Some(r#"{"advisorModel":"haiku"}"#),
    );
    let shared_only = project_with(&home, "shared", Some(r#"{"advisorModel":"sonnet"}"#), None);
    let row = profile_with_advisor(Some("opus"));
    let blank = profile_with_advisor(None);
    let demand = |p: &Profile, args: &[&str], cwd: Option<&std::path::Path>| {
        let mut full = vec!["--model", "m"];
        full.extend_from_slice(args);
        advisor_demand(p, &full, cwd, EnvLeg::Caller, &[], &[])
    };

    assert_eq!(demand(&row, &[], None), ["m", "opus"], "the overlay row");
    assert_eq!(
        demand(&blank, &["--settings", inline], None),
        ["m", "fable"],
        "inline --settings JSON"
    );
    assert_eq!(
        demand(&blank, &["--settings", file.to_str().unwrap()], None),
        ["m", "fable"],
        "a --settings file"
    );
    assert_eq!(
        demand(&blank, &[], Some(&shared_only)),
        ["m", "sonnet"],
        "the project's shared file"
    );
    assert_eq!(
        demand(&blank, &[], Some(&both)),
        ["m", "haiku"],
        "the project's local file outranks its shared file"
    );
    assert_eq!(
        demand(&row, &[], Some(&both)),
        ["m", "haiku"],
        "a project tier outranks the overlay"
    );
    assert_eq!(
        demand(&row, &["--settings", inline], Some(&both)),
        ["m", "fable"],
        "the --settings passthrough outranks the project tiers"
    );
    assert_eq!(
        demand(
            &row,
            &["--settings", inline, "--advisor", "opus"],
            Some(&both)
        ),
        ["m", "opus"],
        "the flag outranks every settings tier"
    );
    assert_eq!(
        demand(&blank, &["--setting-sources", "user"], Some(&both)),
        ["m"],
        "`--setting-sources user` leaves the project tiers unread"
    );
    assert_eq!(
        demand(&blank, &["--setting-sources", "project"], Some(&both)),
        ["m", "sonnet"],
        "only the enabled tier counts"
    );
}

/// The overlay strips a global `advisorModel` when the profile has no row (the
/// writer is clauth-owned, like `model`), so the global key never reaches the
/// demand; an isolated runtime has no base at all, and only the row applies.
#[test]
fn a_global_advisor_model_never_counts_without_a_profile_row() {
    let home = HomeSandbox::new();
    let claude = home.home().join(".claude");
    fs::create_dir_all(&claude).unwrap();
    fs::write(claude.join("settings.json"), r#"{"advisorModel":"opus"}"#).unwrap();
    let args = ["--model".to_owned(), "m".to_owned()];
    let demand = |p: &Profile, isolation: Isolation| {
        member_launch_models(p, isolation, &[], &args, None, EnvLeg::Caller, None, |_| {
            None
        })
    };

    assert_eq!(
        demand(&profile_with_advisor(None), Isolation::Shared),
        ["m"],
        "no row: the shared base's advisorModel is removed by the overlay"
    );
    assert_eq!(
        demand(&profile_with_advisor(Some("haiku")), Isolation::Shared),
        ["m", "haiku"],
        "a row replaces the base value"
    );
    assert_eq!(
        demand(&profile_with_advisor(Some("haiku")), Isolation::Isolated),
        ["m", "haiku"],
        "an isolated runtime reads the row alone"
    );
    assert_eq!(
        demand(&profile_with_advisor(None), Isolation::Isolated),
        ["m"]
    );
}

/// The advisor resolves through the child's `ANTHROPIC_DEFAULT_*_MODEL` alias
/// remap like any other demand string, composites expanded first.
#[test]
fn the_advisor_alias_remaps_through_the_childs_default_model_env() {
    let _home = HomeSandbox::new();
    let mut p = profile_with_advisor(Some("opus"));
    p.models.opus = Some("claude-opus-5-5".to_owned());
    p.models.fable = Some("claude-fable-5".to_owned());
    assert_eq!(
        plain_advisor_demand(&p, &["--model", "m"]),
        ["m", "claude-opus-5-5"]
    );
    assert_eq!(
        plain_advisor_demand(&p, &["--model", "m", "--advisor", "best"]),
        ["m", "claude-fable-5", "claude-opus-5-5"],
        "a composite expands to its member aliases, each remapped"
    );
    assert_eq!(
        plain_advisor_demand(&p, &["--model", "m", "--advisor", "opus[1m]"]),
        ["m", "claude-opus-5-5[1m]"],
        "a context suffix is kept"
    );
    assert_eq!(
        plain_advisor_demand(&p, &["--model", "m", "--advisor", "claude-sonnet-5"]),
        ["m", "claude-sonnet-5"],
        "a full id is demanded as typed"
    );
}

#[test]
fn the_advisor_is_a_fallback_side_model_beside_the_subagent_and_the_fallback_list() {
    let _home = HomeSandbox::new();
    let mut p = profile_with_advisor(Some("opus"));
    p.models.subagent = Some("haiku".to_owned());
    assert_eq!(
        plain_advisor_demand(&p, &["--model", "sonnet", "--fallback-model", "fable"]),
        ["sonnet", "fable", "opus", "haiku"],
        "headline, the fallback list, the advisor, then the subagent"
    );
    assert_eq!(
        crate::fallback::demand_from(plain_advisor_demand(&p, &["--model", "opus"])),
        ["opus", "haiku"],
        "an advisor equal to the headline is one family once the walk dedups"
    );
}

/// `cc_env_truthy` is Claude Code's own boolean parse of an env value.
#[test]
fn cc_env_truthy_matches_claude_codes_boolean_parse() {
    for yes in ["1", "true", "TRUE", "True", " yes ", "on", "ON", "\tOn\n"] {
        assert!(cc_env_truthy(yes), "{yes:?}");
    }
    for no in [
        "", " ", "0", "false", "no", "off", "2", "y", "enabled", "truee",
    ] {
        assert!(!cc_env_truthy(no), "{no:?}");
    }
}

/// A truthy `CLAUDE_CODE_DISABLE_ADVISOR_TOOL` the child sees drops the
/// advisor, whichever source carries the flag and the model; a falsy one keeps
/// it.
#[test]
fn a_truthy_disable_flag_drops_the_advisor() {
    let home = HomeSandbox::new();
    let key = "CLAUDE_CODE_DISABLE_ADVISOR_TOOL";
    let row = profile_with_advisor(Some("opus"));
    let kept = |p: &Profile, args: &[&str], env: &[(&str, &str)]| {
        advisor_demand(p, args, None, EnvLeg::Caller, &[], env) == ["m", "opus"]
    };
    let args = ["--model", "m", "--advisor", "opus"];

    for truthy in ["1", "true", "TRUE", " yes ", "on"] {
        assert!(
            !kept(&row, &args, &[(key, truthy)]),
            "caller env {truthy:?} drops the advisor"
        );
        let inline = format!(r#"{{"env":{{"{key}":"{truthy}"}}}}"#);
        let mut with_settings = args.to_vec();
        with_settings.extend(["--settings", inline.as_str()]);
        assert!(
            !kept(&row, &with_settings, &[]),
            "--settings env {truthy:?} drops the advisor"
        );
        let project = project_with(
            &home,
            "killed",
            Some(&format!(r#"{{"env":{{"{key}":"{truthy}"}}}}"#)),
            None,
        );
        assert_eq!(
            advisor_demand(&row, &args, Some(&project), EnvLeg::Caller, &[], &[]),
            ["m"],
            "project env {truthy:?} drops the advisor"
        );
        let mut profile_env = row.clone();
        profile_env.env.insert(key.to_owned(), truthy.to_owned());
        assert!(
            !kept(&profile_env, &args, &[]),
            "profile [env] {truthy:?} drops the advisor"
        );
    }
    for falsy in ["0", "false", "no", "off", ""] {
        assert!(
            kept(&row, &args, &[(key, falsy)]),
            "caller env {falsy:?} keeps the advisor"
        );
    }
    assert!(kept(&row, &args, &[]), "an unset flag keeps the advisor");
}

/// The kill switch and the model resolve through separate tiers: the highest
/// tier that sets the switch decides, wherever the model came from.
#[test]
fn the_disable_flag_and_the_advisor_model_resolve_through_separate_tiers() {
    let home = HomeSandbox::new();
    let key = "CLAUDE_CODE_DISABLE_ADVISOR_TOOL";
    let off = format!(r#"{{"env":{{"{key}":"1"}}}}"#);
    let on = format!(r#"{{"env":{{"{key}":"0"}}}}"#);
    let project_off = project_with(&home, "off", Some(&off), None);
    let project_on = project_with(&home, "on", Some(&on), None);
    let mut row = profile_with_advisor(Some("opus"));
    let demand =
        |p: &Profile, args: &[&str], cwd: Option<&std::path::Path>, env: &[(&str, &str)]| {
            let mut full = vec!["--model", "m"];
            full.extend_from_slice(args);
            advisor_demand(p, &full, cwd, EnvLeg::Caller, &[], env)
        };

    assert_eq!(
        demand(&row, &[], Some(&project_off), &[]),
        ["m"],
        "the model from the overlay, the switch from the project tier"
    );
    assert_eq!(
        demand(&row, &["--advisor", "haiku"], Some(&project_off), &[]),
        ["m"],
        "the flag names a model but the project switch still turns the tool off"
    );
    assert_eq!(
        demand(&row, &["--settings", &on], Some(&project_off), &[]),
        ["m", "opus"],
        "a higher tier's falsy switch beats a lower tier's truthy one"
    );
    assert_eq!(
        demand(&row, &[], Some(&project_on), &[(key, "1")]),
        ["m", "opus"],
        "a project env value beats the caller env"
    );
    row.env.insert(key.to_owned(), "1".to_owned());
    assert_eq!(
        demand(&row, &[], Some(&project_on), &[]),
        ["m", "opus"],
        "the project env beats the overlay env"
    );
    assert_eq!(
        demand(&row, &[], None, &[(key, "0")]),
        ["m"],
        "the overlay env beats the caller env"
    );

    // `--setting-sources` narrows the project tiers for the switch as it does
    // for the model: a project switch in a tier Claude Code never reads is no
    // switch at all.
    let row = profile_with_advisor(Some("opus"));
    assert_eq!(
        demand(
            &row,
            &["--setting-sources", "user"],
            Some(&project_off),
            &[]
        ),
        ["m", "opus"],
        "`--setting-sources user` leaves the project switch unread, so the advisor stays"
    );
    assert_eq!(
        demand(
            &row,
            &["--setting-sources", "local"],
            Some(&project_off),
            &[]
        ),
        ["m", "opus"],
        "the shared project file is not in a `local`-only list"
    );
    assert_eq!(
        demand(
            &row,
            &["--setting-sources", "project"],
            Some(&project_off),
            &[]
        ),
        ["m"],
        "an enabled project tier's switch still turns the advisor off"
    );
}

/// On the start path the kill switch rides the process env and reaches the
/// child only where the scrub keeps it: a key the outgoing profile's `[env]`
/// carried is stale, so the child never sees it and the advisor stays.
#[test]
fn a_stale_process_env_kill_switch_does_not_drop_the_advisor() {
    let _home = HomeSandbox::new();
    let key = "CLAUDE_CODE_DISABLE_ADVISOR_TOOL";
    let row = profile_with_advisor(Some("opus"));
    let args = ["--model", "m"];
    assert_eq!(
        advisor_demand(&row, &args, None, EnvLeg::Process, &[], &[(key, "1")]),
        ["m"],
        "an inherited truthy switch survives the scrub and turns the advisor off"
    );
    assert_eq!(
        advisor_demand(&row, &args, None, EnvLeg::Process, &[key], &[(key, "1")]),
        ["m", "opus"],
        "a stale key is scrubbed, so the advisor stays"
    );
    assert_eq!(
        advisor_demand(&row, &args, None, EnvLeg::Caller, &[key], &[(key, "1")]),
        ["m"],
        "the delegate's caller env is layered after the scrub, so it always reaches the child"
    );
}

/// A launch whose headline is unknown still demands the advisor beside the
/// fallback side: an agent with no model, and an unpinnable resume.
#[test]
fn an_agent_launch_keeps_the_advisor_on_the_fallback_side() {
    let _home = HomeSandbox::new();
    let row = profile_with_advisor(Some("opus"));
    let args: Vec<String> = ["--agent", "reviewer"].map(str::to_owned).to_vec();
    let demand = member_launch_demand(
        &row,
        Isolation::Shared,
        &[],
        &args,
        None,
        EnvLeg::Caller,
        None,
        |_| None,
    );
    assert_eq!(demand.agent_without_model.as_deref(), Some("reviewer"));
    assert_eq!(
        demand.models,
        ["opus"],
        "the advisor is known whatever the agent runs"
    );

    let unknown = member_launch_unknown_headline_models(
        &row,
        Isolation::Shared,
        &[],
        &[],
        EnvLeg::Caller,
        None,
        |_| None,
    );
    assert_eq!(unknown, ["opus"]);
}

/// The advisor never changes whether the headline is unknown: a launch naming
/// no model with no member default stays unknown with an advisor set.
#[test]
fn the_advisor_does_not_resolve_an_unknown_headline() {
    let _home = HomeSandbox::new();
    let row = profile_with_advisor(Some("opus"));
    let demand = member_launch_demand(
        &row,
        Isolation::Shared,
        &[],
        &[],
        None,
        EnvLeg::Caller,
        None,
        |_| None,
    );
    assert!(demand.unknown);
    assert_eq!(demand.models, ["opus"]);
}
