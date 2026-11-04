//! The herdr resume-command report leg of `clauth
//! hook-profile-changed-note`: tell the herdr pane a conversation runs in how
//! to bring that conversation back, so a herdr restart restores it as
//! `clauth resume <session> --profile <account>` instead of herdr's builtin
//! bare `claude --resume <id>` — which loses the account and answers `No
//! conversation found` for isolated-store sessions.
//!
//! The hook is the only component that natively knows both halves of that
//! command: the CC session id arrives in the hook payload, and the resolved
//! account is the same tier-walk answer the account note and the exact-owner
//! stamp carry. As a Claude Code child inside the pane it also inherits
//! `HERDR_PANE_ID`/`HERDR_BIN_PATH`, which the herdr plugin's own watcher
//! cannot see (it joins on pids and only ever learns clauth's minted ids).
//!
//! # The launch mode comes back with the conversation
//!
//! The report must keep the mode the conversation STARTED in, which the hook
//! reads off its own `CLAUDE_CONFIG_DIR` rather than off the payload:
//! - no variable, or the global `~/.claude` — a bare `claude` restores as the
//!   hidden `clauth __restore <sid>`, which runs `claude --resume <sid>` with
//!   the restore marker set, never pinned to whichever account happens to be
//!   global at restore time;
//! - a per-session runtime dir whose registry row follows the chain — a
//!   `clauth start --with-fallback` restores as `clauth resume <sid> --profile
//!   <current member> --with-fallback`;
//! - every other runtime dir — today's `clauth resume <sid> --profile <name>`.
//!
//! A bare restore relaunches `claude --resume <sid>` with none of the options
//! the conversation was started with, so on the conversation's first
//! `SessionStart` (a startup, a `/clear`, or a fork) this module also records
//! the launch's option NAMES — never a positional word, nor a value unless it
//! is itself shaped like an option, so no credential passed on the command
//! line reaches the record or the notice
//! (bare sessions only: clauth's own per-spawn flags make a `clauth start`
//! argv incomparable). A resume arms a notice — refused at the restored
//! process's first main-scope prompt, once — only when `clauth __restore`
//! launched it
//! (its `CLAUTH_HERDR_RESTORE` marker names this conversation and the pid
//! `__restore` ran as, which the hook reads back off `CLAUDE_PID`), only on
//! linux and macOS (the only targets whose process argv the hook can read, so
//! elsewhere the notice never arms), only while
//! `[herdr] restore_notice` is on, and only when the resumed process lacks a
//! launch option name; the notice names the dropped names and nothing else.
//! A resume that arms nothing clears a standing notice only when it names no
//! process or names this fire's own: one stamped for another process — a
//! restored pane still live elsewhere — is left standing.
//!
//! A `clauth resume --with-fallback` that degrades to pinned rides a
//! different channel: the launcher hands the claude it spawns the notice in
//! its own env ([`DEGRADE_NOTICE_ENV`], the text, and [`DEGRADE_MARKER_ENV`],
//! the conversation and the launcher's pid), and the take refuses that
//! claude's first main-scope prompt straight off the env, recording that
//! claude's `CLAUDE_PID` in the record's
//! [`crate::hook_note::NoteRecord::degrade_told`] list as the mark that it was
//! already told. Nothing sits on the record before the spawn, so a launcher
//! killed mid-session leaves no notice behind. Both channels show a refusal
//! only when the record write that remembers it lands: the record is the
//! suppression mechanism, so a spend or an already-told mark that cannot be
//! stored (a full disk, a read-only records dir) means the take answers
//! nothing, and a record notice whose spend failed stays armed for the next
//! writable prompt.
//!
//! Fires on `SessionStart`, on a real attribution change (the session id or
//! the profile moved), while herdr has refused the last report, and whenever
//! the record's stored argv is not the one this fire would report — a mode that
//! changed underneath a pane an older clauth reported. Outside a herdr pane (no
//! `HERDR_PANE_ID`, or no herdr binary), on any failure of the bounded report
//! spawn, and on a pinned or chain fire with no attributable account (a bare
//! restore names none), the leg is silence: herdr's builtin plan stays as the
//! fallback, which is exactly the restore behavior a box had before this leg
//! existed.
//!
//! The report is `pane report-agent-session` with no state and no session id.
//! herdr records a resume argv from any source while no reporter holds the
//! pane's state and the named agent is the one it detects there, and a session
//! report without a session id moves neither the pane's state nor its session
//! owner, so the chip stays herdr's own screen detection. It must never be
//! `pane report-agent`: herdr 0.9 keeps a state reported from a non-herdr source
//! as the pane's lifecycle authority, above its own detection, until the agent
//! exits, and refuses every later state report from that source once its own
//! Claude Code integration has reported the session — a `--state working`
//! report that landed first pinned the pane at `working` for the rest of its
//! life, with only a visible blocker showing through.
//!
//! A `SessionStart` can reach herdr before its detector has seen Claude in the
//! pane; herdr then answers `resume_not_accepted` and records nothing. The
//! hook never waits for detection inside a fire, which would hold the
//! conversation's start: the refusal arms a re-send on the conversation's
//! note record, and the next main-scope fire (a prompt or a tool call) sends
//! the report again with a fresh seq, until one is accepted. herdr answers the
//! same code for causes that never clear (another source holding the pane's
//! state, a non-claude agent detected there), so on such a pane every prompt
//! and every fire re-sends: one bounded spawn each, logged once when armed.
//! Only that code arms it; any other failure disarms it, so a broken or hung
//! herdr is never re-spawned on every fire, and one failed re-send leaves the
//! report to the next attribution change.
//!
//! The seq is minted epoch-ms and persisted on the conversation's note
//! record, because herdr drops a report whose seq is not strictly newer than
//! the stored one — a fresh process whose first mint landed on the same
//! millisecond as the previous process's last report would otherwise be
//! silently lost.

use std::path::Path;

use crate::hook_note::{Payload, ScopeLock, load_record, record_path, store_record};
use crate::start::{ResumeWord, scan_resume_forms};

/// The report's fixed agent fields: clauth speaks as source `clauth` about
/// the `claude` agent herdr detects in the pane. `herdr:claude` itself is
/// reserved for herdr's own CC integration; a distinct source is what lets
/// the report ride beside it instead of replacing it.
const SOURCE: &str = "clauth";
const AGENT: &str = "claude";

/// herdr's error code for a resume argv it cannot attach to the pane yet.
const NOT_ACCEPTED: &str = "resume_not_accepted";

/// How long the record's mint and re-send writes wait on the scope lock; see
/// [`ScopeLock::acquire_within`].
const LOCK_WAIT: std::time::Duration = std::time::Duration::from_millis(500);

/// What one report came back with, as far as the re-send cares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Sent {
    Accepted,
    /// herdr refused the argv as not attachable yet and recorded nothing.
    NotAccepted,
    Failed,
}

pub(crate) fn report_for_fire(payload: &Payload, current: Option<&str>, attribution_changed: bool) {
    // A subagent's fire is its own scope's reading, never the pane
    // conversation's attribution.
    if payload.agent_id.is_some() {
        return;
    }
    // Outside a herdr pane there is nothing to report to.
    let Some(pane_id) = std::env::var("HERDR_PANE_ID")
        .ok()
        .filter(|id| !id.trim().is_empty())
    else {
        return;
    };
    // The mode decides what the restore argv names, so it comes before the
    // account: a bare restore names no account and must report and record even
    // when none is attributable. A pinned or chain restore names one, so it
    // waits for the tier walk to answer.
    let Some(mode) = restore_mode(current) else {
        return;
    };
    let Ok(path) = record_path(&payload.session_id, None) else {
        return;
    };
    let record = load_record(&path).unwrap_or_default();
    let tail = resume_argv(&payload.session_id, &mode);
    // Only a `SessionStart`, a real attribution change, an armed re-send, or a
    // report this record's stored argv no longer matches can move the resume
    // command, so every other fire (prompts, cadence runs, agent spawns) is a
    // no-op. The last arm is what corrects a pane an older clauth reported
    // under another mode: herdr holds that argv until a newer report replaces
    // it, and a restore before the next `SessionStart` would pin the
    // conversation for good.
    let stale =
        record.resume_seq.is_some() && record.reported_argv.as_deref() != Some(tail.as_slice());
    let armed = record.resume_pending;
    if payload.event != "SessionStart" && !attribution_changed && !armed && !stale {
        return;
    }
    // The launch-args record runs before anything can fail its way out of the
    // report: it is the only copy of what the conversation started with, and a
    // missing herdr binary or minted seq must not cost it.
    if mode == RestoreMode::Bare {
        note_bare_launch(&path, payload);
    }
    let Some(bin) = crate::herdr::resolved_bin() else {
        return;
    };
    let Some(seq) = mint_seq(&path, &tail) else {
        return;
    };
    let args = build_report_args(&pane_id, &payload.session_id, &mode, seq);
    let refused = report(&bin, &args) == Sent::NotAccepted;
    if refused != armed {
        if refused {
            crate::logline::to_logfile(format_args!(
                "clauth: herdr has not accepted the resume command yet; re-sending on each fire until it does"
            ));
        }
        arm_resend(&path, refused);
    }
}

/// Mint the report seq for this conversation's main scope: epoch-ms, forced
/// past every seq this record has minted. herdr silently keeps the stored
/// argv for any report whose seq is not strictly newer than the last, so the
/// mint must be monotone across the separate hook processes one conversation
/// fires — the record is what turns the wall clock into a per-session clock.
/// `None` when the record cannot be persisted: an unrememberable seq cannot
/// promise monotonicity, and silence keeps herdr's existing argv standing.
///
/// The argv tail rides the same write whatever herdr answers, so the record
/// always names the last resume command it reported: a later fire compares
/// against it and re-reports a mode that changed underneath the pane.
fn mint_seq(path: &Path, argv: &[String]) -> Option<u64> {
    let _hold = ScopeLock::acquire_within(LOCK_WAIT);
    let mut record = load_record(path).unwrap_or_default();
    let seq = crate::usage::now_ms().max(record.resume_seq.unwrap_or(0).saturating_add(1));
    record.resume_seq = Some(seq);
    record.reported_argv = Some(argv.to_vec());
    store_record(path, &record).ok()?;
    Some(seq)
}

/// Arm or disarm the next fire's re-send. A failed write is a logged silence:
/// an arm lost costs the report until the next attribution change, a disarm
/// lost costs one redundant accepted report.
fn arm_resend(path: &Path, armed: bool) {
    let _hold = ScopeLock::acquire_within(LOCK_WAIT);
    let mut record = load_record(path).unwrap_or_default();
    record.resume_pending = armed;
    if let Err(e) = store_record(path, &record) {
        crate::logline::to_logfile(format_args!(
            "clauth: resume report re-send state not stored: {e}"
        ));
    }
}

/// The mode the conversation was launched in, as far as its restore command
/// cares. Decided from the hook process's own `CLAUDE_CONFIG_DIR`, never from
/// the payload or whether an account resolved: those answer "which account now"
/// rather than "how did this conversation start".
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RestoreMode {
    /// A bare `claude`: the global store, restored as a bare `claude`.
    Bare,
    /// A `clauth start <name>` that does not follow the chain: today's argv.
    Pinned(String),
    /// A `clauth start --with-fallback` whose registry row follows the chain.
    Chain(String),
}

/// [`RestoreMode`] for the account this fire resolved to. The global store —
/// no `CLAUDE_CONFIG_DIR`, or a dir that IS `~/.claude` — is a bare session; a
/// per-session runtime dir whose row follows the chain is a chain session; every
/// other case is pinned.
///
/// `None` when the mode names an account and the fire has none to name: a bare
/// restore's argv carries no account, so only it can report without one.
fn restore_mode(current: Option<&str>) -> Option<RestoreMode> {
    let Some(dir) = crate::which::session_config_dir() else {
        return Some(RestoreMode::Bare);
    };
    if crate::profile::claude_dir()
        .ok()
        .is_some_and(|global| same_dir(&dir, &global))
    {
        return Some(RestoreMode::Bare);
    }
    let chain = dir
        .file_name()
        .and_then(std::ffi::OsStr::to_str)
        .and_then(crate::runtime::sid_of_runtime_dir_name)
        .and_then(|sid| crate::live_sessions::get(&sid))
        .is_some_and(|row| row.follows_chain && !row.isolated);
    let profile = current?;
    Some(if chain {
        RestoreMode::Chain(profile.to_string())
    } else {
        RestoreMode::Pinned(profile.to_string())
    })
}

/// Two paths naming one directory. Canonicalized through
/// [`crate::canonical`] when both resolve; a spelling-equal pair whose dirs do
/// not exist yet still reads equal.
fn same_dir(a: &Path, b: &Path) -> bool {
    match (crate::canonical::identity(a), crate::canonical::identity(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

/// The report argv, pane id first — herdr's hand-rolled parser reads the
/// first positional from args[0] and answers `unknown option` to anything
/// else in that slot. Everything after `--` is the resume command herdr
/// re-creates the pane with, in the mode the conversation launched in.
fn build_report_args(pane_id: &str, session_id: &str, mode: &RestoreMode, seq: u64) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "pane".into(),
        "report-agent-session".into(),
        pane_id.into(),
        "--source".into(),
        SOURCE.into(),
        "--agent".into(),
        AGENT.into(),
        "--seq".into(),
        seq.to_string(),
        "--".into(),
    ];
    args.extend(resume_argv(session_id, mode));
    args
}

/// The resume command herdr stores and re-runs, in the mode the conversation
/// launched in.
fn resume_argv(session_id: &str, mode: &RestoreMode) -> Vec<String> {
    let mut argv: Vec<String> = match mode {
        RestoreMode::Bare => vec!["clauth".into(), "__restore".into(), session_id.into()],
        RestoreMode::Pinned(profile) | RestoreMode::Chain(profile) => vec![
            "clauth".into(),
            "resume".into(),
            session_id.into(),
            "--profile".into(),
            profile.clone(),
        ],
    };
    if matches!(mode, RestoreMode::Chain(_)) {
        argv.push("--with-fallback".into());
    }
    argv
}

/// Record a bare conversation's launch option names, and arm the notice when
/// a herdr restore lost some. The record is the only place the launch survives
/// the process that spawned it, and a resume never overwrites what it is
/// compared against.
fn note_bare_launch(path: &Path, payload: &Payload) {
    if payload.event != "SessionStart" {
        return;
    }
    match payload.source.as_deref() {
        // A `/clear` or a fork starts a new conversation id in the same
        // process, so its first `SessionStart` is as much a launch as a
        // startup. The process argv can still carry the resume that started
        // it; those spellings are never part of what a later restore is
        // compared against.
        Some("startup" | "clear" | "fork") => {
            let Some(args) = claude_args() else {
                return;
            };
            let _hold = ScopeLock::acquire_within(LOCK_WAIT);
            let mut record = load_record(path).unwrap_or_default();
            if record.launch_options.is_some() {
                return;
            }
            record.launch_options = Some(option_names(&args));
            store_quietly(path, &record);
        }
        Some("resume") => {
            // Only a herdr restore earns the notice: `clauth __restore <sid>`
            // set [`RESTORE_MARKER_ENV`] to the conversation it resumes and
            // the pid it ran as, so a resume the user typed (no marker), one
            // whose marker names another conversation, or a nested `claude
            // --resume` inside the restored conversation (the inherited marker
            // carries another pid; unix only) arms nothing. The `[herdr] restore_notice`
            // knob gates the arm. An unreadable argv, no launch record, or
            // nothing dropped all clear a standing notice, so a lossless or
            // unexplained resume never leaves one armed.
            let claude_pid = claude_pid();
            let marker_matches = std::env::var(RESTORE_MARKER_ENV)
                .is_ok_and(|m| restore_marker_matches(&m, &payload.session_id, claude_pid));
            let resumed = claude_args();
            let _hold = ScopeLock::acquire_within(LOCK_WAIT);
            let mut record = load_record(path).unwrap_or_default();
            // A notice this process already spent stays spent: an in-process
            // `/resume` back into the restored conversation carries the same
            // marker and pid, and must not refuse a second prompt.
            let spent = record.restore_notice.is_none()
                && claude_pid.is_some()
                && record.restore_notice_pid == claude_pid;
            let notice = if marker_matches && restore_notice_on() && !spent {
                resumed.as_deref().and_then(|resumed| {
                    let kept = option_names(resumed);
                    let dropped: Vec<String> = record
                        .launch_options
                        .as_deref()?
                        .iter()
                        .filter(|name| !kept.contains(*name))
                        .cloned()
                        .collect();
                    (!dropped.is_empty())
                        .then(|| restore_notice_text(&payload.session_id, &dropped))
                })
            } else {
                None
            };
            match notice {
                Some(text) => {
                    if record.restore_notice.as_deref() != Some(text.as_str())
                        || record.restore_notice_pid != claude_pid
                    {
                        record.restore_notice = Some(text);
                        record.restore_notice_pid = claude_pid;
                        store_quietly(path, &record);
                    }
                }
                // A resume that arms nothing clears only a notice it still
                // owns — one naming no process, or naming this very process —
                // stamp and all: nothing was spent, so nothing marks this
                // process. One stamped for another process is that process's
                // to spend, or the take's to drop once it is gone.
                None if record.restore_notice.is_some()
                    && (record.restore_notice_pid.is_none()
                        || record.restore_notice_pid == claude_pid) =>
                {
                    record.restore_notice = None;
                    record.restore_notice_pid = None;
                    store_quietly(path, &record);
                }
                None => {}
            }
        }
        _ => {}
    }
}

/// The `claude` process running this hook, off the `CLAUDE_PID` Claude Code
/// exports to its hook commands: `None` where it is unset or unparsable.
fn claude_pid() -> Option<u32> {
    std::env::var("CLAUDE_PID").ok()?.parse().ok()
}

/// Whether [`RESTORE_MARKER_ENV`]'s `marker` names this process: the
/// conversation `clauth __restore` resumed and the pid it ran as, which the
/// unix `exec` hands to `claude` and `CLAUDE_PID` reports back. Absent, another
/// conversation, no pid, an unparsable pid and (on unix) another pid are all
/// no restore of this conversation.
fn restore_marker_matches(marker: &str, session_id: &str, claude_pid: Option<u32>) -> bool {
    let Some((session, pid)) = marker.rsplit_once(':') else {
        return false;
    };
    let Ok(restored_pid) = pid.parse::<u32>() else {
        return false;
    };
    // On a non-unix target `claude` runs under a child pid the marker cannot
    // name, and `claude_args()` reads no argv there (`process_argv` answers
    // only on linux and macOS), so the notice never arms: the marker rule can
    // only run on the way to a clear, never to an arm.
    session == session_id && (!cfg!(unix) || claude_pid == Some(restored_pid))
}

/// One record write under a held scope lock for the writes whose failure
/// costs a missing notice, leaves standing a notice that already belongs to
/// the process it would refuse, or leaves a leftover for the next take to
/// drop again — never a refusal that repeats — so a lost write is a logged
/// silence, never a fire taken down.
/// The take's own writes, where a refusal rides the write, check the store
/// result instead.
fn store_quietly(path: &Path, record: &crate::hook_note::NoteRecord) {
    if let Err(e) = store_record(path, record) {
        crate::logline::to_logfile(format_args!("clauth: restore notice state not stored: {e}"));
    }
}

/// The env var `clauth __restore` sets to `<session id>:<its own pid>`: the
/// conversation it resumes and the process it becomes. On unix the `exec`
/// keeps that pid, so it is also the `claude` the hook runs in.
pub(crate) const RESTORE_MARKER_ENV: &str = "CLAUTH_HERDR_RESTORE";

/// The env var a degraded `clauth resume --with-fallback` sets on the claude
/// it launches to the whole degrade notice text: the take refuses that
/// claude's first main-scope prompt with it, straight off its own env.
/// [`crate::runtime::scrub_profile_env`] and [`restore_command`] drop an
/// inherited one from the commands they build, and only the degrade arm sets
/// it, so no claude session clauth launches carries another launch's notice.
pub(crate) const DEGRADE_NOTICE_ENV: &str = "CLAUTH_DEGRADE_NOTICE";

/// The env var a degraded `clauth resume --with-fallback` sets on the claude
/// it launches to `<session id>:<the launcher's pid>`, the same
/// `<session>:<pid>` shape as [`RESTORE_MARKER_ENV`]: the conversation the
/// launcher resumed and the process that spawned this claude. On linux and
/// macOS the pid must also name this claude's parent, so a `claude` nested
/// inside the session — which inherits the env — never gets the notice. It
/// travels with [`DEGRADE_NOTICE_ENV`]: the same builders drop an inherited
/// one, and only the degrade arm sets it.
pub(crate) const DEGRADE_MARKER_ENV: &str = "CLAUTH_DEGRADE_MARKER";

/// `[herdr] restore_notice`, defaulting on when the state cannot be read —
/// the hook's copy of the `delegate_dot` read in `src/mcp/mod.rs`, because the
/// TUI toggle persists the knob without pushing anything live.
fn restore_notice_on() -> bool {
    crate::profile::load_app_state()
        .map(|state| state.herdr.restore_notice)
        .unwrap_or_else(|_| crate::profile::HerdrSettings::default().restore_notice)
}

/// The `claude --resume <sid>` command `clauth __restore <sid>` runs for a
/// bare pane, resolved through [`crate::runtime::claude_command`] (the Windows
/// shim resolution lives there), with [`RESTORE_MARKER_ENV`] naming the
/// conversation it resumes and the pid of the `__restore` process — the one
/// signal that tells the hook a herdr restore, not a typed resume, is starting.
/// The launched claude keeps every other variable it inherits, exactly as a
/// typed `claude --resume` does, but the degrade env pair
/// ([`DEGRADE_NOTICE_ENV`], [`DEGRADE_MARKER_ENV`]) is dropped: it belongs to
/// the claude a degraded resume launched, never this restore's.
pub(crate) fn restore_command(session_id: &str) -> std::process::Command {
    let mut cmd = crate::runtime::claude_command();
    cmd.arg("--resume").arg(session_id);
    cmd.env(
        RESTORE_MARKER_ENV,
        format!("{session_id}:{}", std::process::id()),
    );
    cmd.env_remove(DEGRADE_NOTICE_ENV);
    cmd.env_remove(DEGRADE_MARKER_ENV);
    cmd
}

/// Run the hidden `clauth __restore <sid>` herdr re-runs for a bare pane. On
/// unix the process `exec`s the `claude --resume <sid>` command, so the pane's
/// process IS claude: herdr's agent detection, the tty and the signals all
/// stay as for a typed `claude`, and the pid [`RESTORE_MARKER_ENV`] names is
/// the one the hook reads back off `CLAUDE_PID`. Other targets spawn it, wait,
/// and exit with the child's code. An exec or spawn failure is an error naming
/// `claude` and the cause.
pub(crate) fn run_restore(session_id: &str) -> anyhow::Result<()> {
    let mut cmd = restore_command(session_id);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let error = cmd.exec();
        anyhow::bail!("claude restore failed to start: {error}");
    }
    #[cfg(not(unix))]
    {
        let status = cmd
            .status()
            .map_err(|e| anyhow::anyhow!("claude restore failed to start: {e}"))?;
        std::process::exit(status.code().unwrap_or(1));
    }
}

/// The option names in `argv`: every resume and conversation spelling goes the
/// way [`strip_resume_spellings`] drops it, then only option words survive.
/// A `--name=value` word names `--name`; a single-dash word names `-` plus its
/// second byte when that byte is an ASCII letter (`-nPhoenix` -> `-n`,
/// `-p"x"` -> `-p`, `-v` -> `-v`), so a glued or attached value never
/// survives, and any other single-dash word (`-`, `-1`) names nothing. A bare
/// `--` ends the options, every other word (a value, a positional) is dropped,
/// and each name appears once in first-seen order.
///
/// `--agent` is never named: a plain `claude --resume` restores the agent from
/// the transcript's own `agent-setting` record (measured 2026-11-03 on Claude
/// Code 2.1.317), so it is not an option the restore drops.
///
/// The names come from option-shaped words only, never from the values around
/// them, so an API key or token (a `_`, `.`, `=`, quote or space, or no
/// leading dash) never reaches the launch record or the notice. The residual
/// is a value that is itself shaped like an option (a letter right after one
/// dash, or a long name of ASCII letters, digits and dashes): it reads as a
/// name like any other.
pub(crate) fn option_names(argv: &[String]) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for word in strip_resume_spellings(argv) {
        if word == "--" {
            break;
        }
        let name = if let Some(rest) = word.strip_prefix("--") {
            let name = rest.split('=').next().unwrap_or("");
            if name == "agent"
                || name.is_empty()
                || !name
                    .bytes()
                    .next()
                    .is_some_and(|b| b.is_ascii_alphanumeric())
                || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
            {
                continue;
            }
            format!("--{name}")
        } else if let Some(letter) = word
            .strip_prefix('-')
            .and_then(|rest| rest.as_bytes().first())
            .filter(|b| b.is_ascii_alphabetic())
        {
            format!("-{}", char::from(*letter))
        } else {
            continue;
        };
        if !names.contains(&name) {
            names.push(name);
        }
    }
    names
}

/// The notice a herdr restore earns when it came back without some launch
/// options, naming the dropped option NAMES only, as [`option_names`] reads
/// them — never a positional word, nor a value unless it is itself shaped
/// like an option, so a credential passed on the command line cannot reach it.
pub(crate) fn restore_notice_text(session_id: &str, dropped: &[String]) -> String {
    format!(
        "clauth: this conversation resumed as `claude --resume {session_id}`, without the \
         options it was started with: {}. This prompt was not sent: submit it again to \
         continue as restored, or restart the conversation with those options.",
        dropped.join(", "),
    )
}

/// `argv` without every spelling that names the conversation rather than an
/// option, as [`crate::start::scan_resume_forms`] models them: a whole
/// `--resume`, `--resume=X` or `--continue` token, `-r` with the value it
/// takes, the `c`/`r` letters inside a short cluster (whose other flags
/// survive as `-<rest>`), and `--session-id <id>` / `--fork-session`, which
/// name the conversation a restore resumes. They say how the process got here
/// or which conversation it is, never which options it runs with.
/// `--from-pr`, `--teleport` and `--cloud` start another conversation, so
/// they stay.
fn strip_resume_spellings(argv: &[String]) -> Vec<String> {
    scan_resume_forms(argv)
        .words
        .iter()
        .zip(argv)
        .filter_map(|(word, original)| match word {
            ResumeWord::Kept => Some(original.clone()),
            ResumeWord::Dropped => None,
            ResumeWord::Remainder(rest) => Some(format!("-{rest}")),
        })
        .collect()
}

/// The argv this process's `claude` runs with, argv0 dropped, read through
/// `CLAUDE_PID` (Claude Code sets it for hook commands). `None` where the pid
/// is unset or unreadable, where argv cannot be read on this OS, or where a
/// word is not UTF-8 — an unreadable launch must silence the notice rather
/// than name a wrong one.
pub(crate) fn claude_args() -> Option<Vec<String>> {
    #[cfg(all(test, unix))]
    {
        if let Some(args) = CLAUDE_ARGS_OVERRIDE.with(|o| o.borrow().clone()) {
            return Some(args);
        }
    }
    let pid = claude_pid()?;
    // A child read before its exec shows the PARENT's argv; the empty-cmdline
    // window is just after exec, and it is real — measured 2026-11-03:
    // 1777-2940 of 3000 empty first reads of a just-spawned `sleep 30`,
    // load-dependent. In production `CLAUDE_PID` names the `claude` that has
    // run this conversation, so the first read usually answers; the retry
    // keeps a torn pid (or a process that never gets to exec) from costing the
    // hook a wait.
    let mut argv = crate::runtime::process_argv(pid)?;
    if argv.is_empty() {
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(50);
        while argv.is_empty() && std::time::Instant::now() < deadline {
            std::thread::yield_now();
            argv = crate::runtime::process_argv(pid)?;
        }
    }
    argv.into_iter()
        .skip(1)
        .map(|word| String::from_utf8(word).ok())
        .collect()
}

#[cfg(all(test, unix))]
thread_local! {
    static CLAUDE_ARGS_OVERRIDE: std::cell::RefCell<Option<Vec<String>>> =
        const { std::cell::RefCell::new(None) };
}

/// Poses this thread's [`claude_args`] answer, restored on drop — the seam
/// tests use instead of spawning a `claude` with known argv. Unix-only because
/// the herdr hook tests that pose it are.
#[cfg(all(test, unix))]
pub(crate) struct ClaudeArgsOverride {
    prev: Option<Vec<String>>,
}

#[cfg(all(test, unix))]
impl ClaudeArgsOverride {
    pub(crate) fn new(words: &[&str]) -> Self {
        let prev = CLAUDE_ARGS_OVERRIDE
            .with(|o| o.replace(Some(words.iter().map(|w| (*w).to_string()).collect())));
        Self { prev }
    }
}

#[cfg(all(test, unix))]
impl Drop for ClaudeArgsOverride {
    fn drop(&mut self) {
        let prev = self.prev.take();
        CLAUDE_ARGS_OVERRIDE.with(|o| *o.borrow_mut() = prev);
    }
}

/// The degrade notice this process's env hands it, when it names this
/// conversation and this process: the marker's conversation must match, its
/// pid must parse, and on linux and macOS — the targets whose parent chain is
/// readable — that pid must name this claude's parent, the launcher that
/// spawned it. A `claude` nested inside the session inherits the env without
/// that parent. Where no platform reader exists the conversation and pid
/// match alone deliver. A `CLAUDE_PID` that is absent or unparsable delivers
/// nothing. The answer pairs the notice with the `CLAUDE_PID` it was
/// validated against, which the take writes as the already-told mark.
fn env_degrade_notice(session_id: &str, claude_pid: Option<u32>) -> Option<(String, u32)> {
    let notice = std::env::var(DEGRADE_NOTICE_ENV).ok()?;
    let marker = std::env::var(DEGRADE_MARKER_ENV).ok()?;
    let (session, pid) = marker.rsplit_once(':')?;
    if session != session_id {
        return None;
    }
    let claude_pid = claude_pid?;
    let launcher_pid: u32 = pid.parse().ok()?;
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    if crate::session_record::parent_pid(claude_pid) != Some(launcher_pid) {
        return None;
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    let _ = launcher_pid;
    Some((notice, claude_pid))
}

/// Take the armed notice: only the conversation's own `UserPromptSubmit` may
/// spend it, so a tool call or a subagent's fire leaves it armed. The degrade
/// notice rides this claude's own env, set by the degraded resume that
/// launched it, and reaches only that claude ([`env_degrade_notice`] names
/// it); its delivery records that claude's pid in
/// [`crate::hook_note::NoteRecord::degrade_told`], and a claude already on
/// that list gets nothing, so the notice is refused once per launched claude
/// whatever other processes of the conversation do in between. A degraded
/// claude already on the told list is answered off an unlocked peek, before
/// the scope lock; the locked re-read stays the authority for a first
/// delivery. The env branch never reads or writes
/// [`crate::hook_note::NoteRecord::restore_notice`] or its pid. A record
/// notice reaches only the process
/// [`crate::hook_note::NoteRecord::restore_notice_pid`] names; one stamped for
/// another process or for none was left by a launch that no longer owns it,
/// so the take drops it and answers nothing. Spent and dropped under the
/// scope lock, so two racing prompts spend it once. A delivery is shown only
/// when the write that remembers it lands: the record is the suppression
/// mechanism, so a spend or an already-told mark that cannot be stored (a
/// full disk, a read-only records dir) means the take answers nothing and
/// the notice stays armed for the next writable prompt. `[herdr]
/// restore_notice` gates only the herdr restore notice's arm, never this
/// take: the degrade refusal reads the env, never the knob.
pub(crate) fn take_restore_notice(payload: &Payload) -> Option<String> {
    if payload.event != "UserPromptSubmit" || payload.agent_id.is_some() {
        return None;
    }
    let claude_pid = claude_pid();
    let env_notice = env_degrade_notice(&payload.session_id, claude_pid);
    let path = record_path(&payload.session_id, None).ok()?;
    // Peek before the flock: a fire with neither an env notice nor a record
    // notice, and a degraded claude already on the told list, must not pay
    // the lock, an `mkdir_700` and an open on every prompt. The locked
    // re-read re-checks under the hold, so two racing prompts still spend
    // the notice once.
    if let Some((_, pid)) = &env_notice {
        if load_record(&path).is_some_and(|record| record.degrade_told.contains(pid)) {
            return None;
        }
    } else {
        load_record(&path)?.restore_notice.as_ref()?;
    }
    let _hold = ScopeLock::acquire_within(LOCK_WAIT);
    // A fresh conversation has no record yet; the env notice must deliver (and
    // write its already-told mark) without one.
    let mut record = load_record(&path).unwrap_or_default();
    if let Some((notice, pid)) = env_notice {
        // The env notice is refused once per launched claude: the pid is the
        // record's own already-told mark, so a second prompt of the same
        // process — or an in-process `/resume` back into the conversation —
        // gets nothing.
        if record.degrade_told.contains(&pid) {
            return None;
        }
        record.degrade_told.push(pid);
        prune_degrade_told(&mut record.degrade_told);
        if store_record(&path, &record).is_err() {
            // The already-told mark IS the suppression: a refusal whose mark
            // cannot be remembered would be refused again on every later
            // prompt of this claude for the life of the process, so it is
            // not shown. The env keeps the notice; the next prompt that can
            // store the mark is refused once.
            crate::logline::to_logfile(format_args!(
                "clauth: cannot persist the degrade already-told mark for {}; staying silent",
                path.display()
            ));
            return None;
        }
        return Some(notice);
    }
    let notice = record.restore_notice?;
    if record
        .restore_notice_pid
        .is_some_and(|owner| Some(owner) == claude_pid)
    {
        record.restore_notice = None;
        if store_record(&path, &record).is_err() {
            // The record IS the suppression mechanism: a spend that cannot be
            // remembered would be refused again on every prompt, so it is not
            // shown. The write left the record untouched, so the notice stays
            // armed for the next writable prompt.
            crate::logline::to_logfile(format_args!(
                "clauth: cannot persist the spent restore notice for {}; staying silent",
                path.display()
            ));
            return None;
        }
        return Some(notice);
    }
    // A notice stamped for another pid, or for none, is a leftover no live
    // process of this conversation owns: drop it and answer nothing.
    record.restore_notice = None;
    record.restore_notice_pid = None;
    store_quietly(&path, &record);
    None
}

/// How many told pids a list keeps
/// ([`crate::hook_note::NoteRecord::degrade_told`]): the newest win.
const DEGRADE_TOLD_CAP: usize = 16;

/// Keep [`crate::hook_note::NoteRecord::degrade_told`] bounded. On unix a pid
/// that no longer names a running process is dropped, so a reused pid can
/// lose a notice but never refuse a prompt its process was not told.
/// [`crate::runtime::namespaced_keychain_ledger::pid_alive`] answers false for
/// every pid off unix, which would empty the list on every write, so the
/// count bound runs on every target after the liveness pass.
fn prune_degrade_told(told: &mut Vec<u32>) {
    #[cfg(unix)]
    told.retain(|pid| crate::runtime::namespaced_keychain_ledger::pid_alive(*pid));
    cap_degrade_told(told);
}

/// Keep the newest [`DEGRADE_TOLD_CAP`] entries of
/// [`crate::hook_note::NoteRecord::degrade_told`], dropping the oldest first
/// and keeping the told order.
fn cap_degrade_told(told: &mut Vec<u32>) {
    if told.len() > DEGRADE_TOLD_CAP {
        told.drain(..told.len() - DEGRADE_TOLD_CAP);
    }
}

/// One bounded spawn of the herdr CLI; every failure but herdr's
/// not-yet-accepted refusal is a logged silence, the same posture the account
/// note keeps. The caller logs that refusal once, when it arms the re-send.
/// [`crate::herdr::bounded_output`]
/// kills a child that hangs past its deadline, so a stuck herdr costs the
/// hook at most that bound.
fn report(bin: &Path, args: &[String]) -> Sent {
    let Some(bin) = bin.to_str() else {
        crate::logline::to_logfile(format_args!(
            "clauth: herdr bin path is not valid UTF-8; resume command not reported"
        ));
        return Sent::Failed;
    };
    let argv: Vec<&str> = args.iter().map(String::as_str).collect();
    match crate::herdr::bounded_output(bin, &argv, &[]) {
        Some(output) if output.status.success() => Sent::Accepted,
        Some(output) => {
            let status = output.status;
            let out = crate::daemon::api::panes::HerdrOut::from(output);
            if crate::daemon::api::agent::error_code(&out).as_deref() == Some(NOT_ACCEPTED) {
                return Sent::NotAccepted;
            }
            crate::logline::to_logfile(format_args!(
                "clauth: herdr pane report-agent-session exited {status} (resume command not reported)"
            ));
            Sent::Failed
        }
        None => {
            crate::logline::to_logfile(format_args!(
                "clauth: herdr pane report-agent-session failed to spawn or timed out (resume command not reported)"
            ));
            Sent::Failed
        }
    }
}

#[cfg(test)]
#[path = "../tests/inline/hook_resume.rs"]
mod tests;
