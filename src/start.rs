//! `clauth start <name>` — spawn `claude` against this session's own runtime
//! directory. See [`crate::runtime`] for the per-session runtime design; this
//! module is just the thin wrapper that owns the lifetime guard.

use std::path::{Path, PathBuf};
use std::process::ExitStatus;
#[cfg(unix)]
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
#[cfg(unix)]
use std::thread::JoinHandle;
#[cfg(unix)]
use std::time::Duration;
use std::time::SystemTime;

use anyhow::{Context, Result};
#[cfg(unix)]
use signal_hook::consts::signal::{SIGINT, SIGTERM};
#[cfg(unix)]
use signal_hook::iterator::{Handle as SignalHandle, Signals};

use crate::logline::logline;
use crate::out::errln;
use crate::profile::{AppConfig, Profile, ProfileName};
use crate::runtime::{BackgroundHold, Isolation, ProfileRuntime};
use crate::spinner::Spinner;

#[cfg(unix)]
const CHILD_WAIT_INTERVAL: Duration = Duration::from_millis(50);

struct ChildOutcome {
    status: ExitStatus,
    signal: Option<i32>,
}

/// Lift an exiting isolated session's state into the global store, gated on
/// being the only live marker in `sessions`: the count — not the keying — is
/// what proves nothing is reading the tree being emptied, since the sidecar leg
/// would otherwise pull `shell-snapshots/` out from under a live Claude Code
/// mid-session. Self holds its own marker, hence `> 1`. The move itself lives
/// in [`crate::runtime::rescue_runtime`], shared with the stale-runtime GC and
/// the teardown so an unrescued tree is lifted at its deletion site too.
///
/// Under real symlinks each session owns its tree and marker dir, and under
/// fake symlinks an isolated session owns them too, so the count is this
/// session alone and the `> 1` arm never fires in normal operation. Both arms
/// still earn their place. `None` means the marker dir could not be read;
/// deleting the guard would delete that refusal with no replacement.
/// `Some(n > 1)` is defence in depth against a same-sid collision and against a
/// legacy shared dir.
pub(crate) fn rescue_teardown(
    iso_root: &Path,
    sessions: &Path,
    claude_home: &Path,
) -> (usize, usize) {
    // An unreadable marker dir falls to "do not move": this leg pulls
    // `shell-snapshots/` out from under whatever is reading the tree, so an
    // unknown has to read the same way a live sibling does.
    if crate::runtime::live_sessions_at(sessions).is_none_or(|live| live > 1) {
        logline!("clauth: skipping rescue, another isolated session is still live");
        return (0, 0);
    }
    let (transcripts, sidecars) = crate::runtime::rescue_runtime(
        iso_root,
        claude_home,
        crate::sessions::SidecarSource::Isolated,
    );
    (transcripts.moved, sidecars.moved)
}

/// The refusal a `--with-fallback` start gets on a host that structurally cannot
/// execute a per-session credential swap. Split from the gate so the render is
/// exercisable from any run: [`LinkMode::Fake`] is unreachable on a real-symlink
/// host.
fn unsupported_host_refusal(name: &ProfileName, why: crate::runtime::SwapUnsupported) -> String {
    format!(
        "'{name}': --with-fallback needs a per-session credential swap, but {why}; start without it"
    )
}

/// Every reason `--with-fallback` cannot be honored for `name`, refused before
/// `acquire` builds a tree and long before `claude` is spawned. A flag that
/// silently leaves the session on its launch account is the one outcome the live
/// Claude Code probe exists to prevent, so none of these is a warning.
///
/// Every gate that can answer WITHOUT the disk runs first, in unfixable-first
/// order, and the transport probe runs last. That ordering is load-bearing: a
/// start refused for a cause the user can act on never materializes a profile
/// dir for an account that never launched.
fn refuse_unless_chain_eligible(
    config: &AppConfig,
    profile: &crate::profile::Profile,
    isolation: Isolation,
) -> Result<()> {
    let name = &profile.name;
    // clap already refuses the flag pair, so this is for a caller that bypasses
    // it: `chain_opt_in_survives` drops an isolated opt-in silently, which is the
    // one outcome every gate here exists to prevent.
    if isolation == Isolation::Isolated {
        anyhow::bail!(
            "'{name}': --with-fallback cannot be combined with --isolated, since an \
             isolated session follows no chain"
        );
    }
    // The decision leg's freshness gate reads only the OAuth status store. That
    // is sound because a third-party-launched session gets a chain the walk
    // cannot move it off — so an opted-in one would follow nothing, in silence.
    if !profile.is_oauth() {
        anyhow::bail!(
            "'{name}': --with-fallback needs an OAuth account, but this one carries \
             a custom endpoint; start without it"
        );
    }
    // `snapshot_session_chain` returns `None` for a member outside the chain, so
    // the row is skipped every tick with nothing said.
    if !config.state.fallback_chain.iter().any(|n| n == name) {
        anyhow::bail!(
            "'{name}': --with-fallback needs a fallback-chain member; add '{name}' on \
             the fallback tab, or start without it"
        );
    }
    // Membership alone is not enough: a chain holding only this profile gives
    // `next_auto_switch_target` nowhere to point, and both `Off` and a stay-put
    // `None` write nothing on the session path. Same silence as a non-member.
    if !config.state.fallback_chain.iter().any(|n| n != name) {
        anyhow::bail!(
            "'{name}': --with-fallback needs a second account in the fallback chain to \
             move to; add one on the fallback tab, or start without it"
        );
    }
    // Only the daemon's decision leg writes `intended_member`, so with no daemon
    // the flag is inert. `singleton_held` is the decision-side reader: it
    // separates "nobody there" from "can't tell", and a host that cannot be
    // checked cannot run the decider on it either, so both refuse.
    let held = crate::daemon::singleton_held().with_context(|| {
        format!("'{name}': --with-fallback needs a running daemon and this host could not be checked for one")
    })?;
    if !held {
        anyhow::bail!(
            "'{name}': --with-fallback needs a running daemon to decide switches, \
             run `clauth daemon`"
        );
    }
    // Last, because it is the only gate that touches disk.
    if let Some(why) = crate::runtime::unsupported_swap_transport(name)? {
        anyhow::bail!("{}", unsupported_host_refusal(name, why));
    }
    Ok(())
}

/// The refusals every start runs before any side effect, shared by [`run`] and
/// `cmd_start`'s explain/launch paths so `--explain` answers what a real launch
/// would do. The disabled gate is the authoritative one: every caller inherits
/// it here, before runtime acquire or spawn, so no caller can forget to check.
/// The background refusals (an `--isolated` launch, or a copy host) run here
/// too, so `--explain` refuses exactly what a launch would.
pub(crate) fn admit<'a>(
    config: &'a AppConfig,
    name: &ProfileName,
    isolation: Isolation,
    follows_chain: bool,
    claude_args: &[String],
) -> Result<&'a Profile> {
    crate::refuse_if_disabled(config, name)?;
    let profile = config.find(name).context("profile not found")?;
    if follows_chain {
        refuse_unless_chain_eligible(config, profile, isolation)?;
    }
    refuse_background_start(name, isolation, follows_chain, claude_args)?;
    Ok(profile)
}

/// The process-env launch keys a launcher can inherit, in slot order: the
/// headline model, the subagent model, then the advisor kill switch (not a
/// model, but read through the same scrub filter and tier precedence). The slot
/// order is the contract between [`process_model_env`] and
/// [`member_launch_demand`]'s scrub filter, so reading and filtering cannot
/// drift.
const PROCESS_MODEL_ENV_KEYS: [&str; 3] = [
    "ANTHROPIC_MODEL",
    "CLAUDE_CODE_SUBAGENT_MODEL",
    "CLAUDE_CODE_DISABLE_ADVISOR_TOOL",
];

/// The process-env launch triple, read once per `cmd_start`.
/// [`crate::fallback::start_walk`] takes it as an argument rather than reading
/// the process environment itself, so its tests pass an explicit empty triple
/// and stay hermetic.
pub(crate) fn process_model_env() -> [Option<String>; 3] {
    PROCESS_MODEL_ENV_KEYS.map(|key| std::env::var(key).ok())
}

/// The `env_of` reader for [`member_launch_demand`]: maps a model env key to
/// its slot in the process-env triple, so the start and fallback paths pass one
/// closure instead of each writing the match.
pub(crate) fn process_model_env_of(
    env_models: &[Option<String>; 3],
) -> impl Fn(&str) -> Option<String> + '_ {
    move |key| match key {
        "ANTHROPIC_MODEL" => env_models[0].clone(),
        "CLAUDE_CODE_SUBAGENT_MODEL" => env_models[1].clone(),
        "CLAUDE_CODE_DISABLE_ADVISOR_TOOL" => env_models[2].clone(),
        _ => None,
    }
}

/// Where the `env_models` triple came from. The start path's inherited process
/// env is scrubbed before the child sees it, so a key reaches the child only
/// where [`crate::runtime::env_survives_scrub`] keeps it; the delegate's caller
/// env is layered AFTER the scrub ([`crate::mcp::apply_delegate_env`]), so it
/// always reaches the child. Both still lose to a settings `env` value for the
/// same key (Claude Code applies settings env at startup).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EnvLeg {
    Process,
    Caller,
}

/// The last `--model` value, the last RAW `--model` value, and the last
/// `--fallback-model` value in a passthrough arg list, in both spellings, read
/// in ONE walk. A `--model` consumes the next word as its value whatever it
/// holds, as Claude Code's commander reads it (a `--model` followed by
/// `--fallback-model` eats the flag, so no fallback flag was given). The
/// headline slot is the LAST `--model` value with a blank read as absent: a
/// blank value names no model, and an empty string would both erase the overlay
/// default and print an empty interpolation in the unplaceable-model refusal.
/// The raw slot keeps the blank, because the child runs a blank last `--model`,
/// so the drop rule ([`fallback_chain`]) compares against it and a flag equal
/// to an EARLIER `--model` still stands. The fallback slot is the last RAW
/// `--fallback-model` value; [`fallback_chain`] splits it and compares it whole
/// against the model the child runs.
pub(crate) fn parsed_model_args(
    claude_args: &[String],
) -> (Option<String>, Option<String>, Option<String>) {
    let mut headline = None;
    let mut last_raw_model = None;
    let mut fallback = None;
    let mut args = claude_args.iter();
    while let Some(a) = args.next() {
        if let Some(v) = a.strip_prefix("--model=") {
            headline = (!v.trim().is_empty()).then(|| v.to_owned());
            last_raw_model = Some(v.to_owned());
        } else if let Some(v) = a.strip_prefix("--fallback-model=") {
            fallback = Some(v.to_owned());
        } else if a == "--model"
            && let Some(v) = args.next()
        {
            headline = (!v.trim().is_empty()).then(|| v.clone());
            last_raw_model = Some(v.clone());
        } else if a == "--fallback-model"
            && let Some(v) = args.next()
        {
            fallback = Some(v.clone());
        }
    }
    (headline, last_raw_model, fallback)
}

/// The fallback models a launch's child may fall back to: ONE source's list,
/// by Claude Code's own precedence — the last `--fallback-model` value, its
/// whole raw string compared against the model the child runs and dropped when
/// equal so the settings chain applies, else the `--settings` passthrough's
/// `fallbackModel`, else the project settings', else the runtime overlay's.
/// Claude Code merges settings sources by keeping the higher source's list
/// rather than unioning them; an empty or non-array higher list replaces the
/// lower one, meaning no fallback. The winner is split, trimmed, deduped and
/// never capped: an entry Claude Code may filter out costs a candidate, never a
/// session. A `default` entry expands to the member's `[models] default` when
/// set; without one it stays literal and the returned flag reports the launch
/// holds the unknown demand a launch that names no model resolves to.
fn fallback_chain(
    model: Option<&str>,
    flag: Option<String>,
    sources: [&crate::claude::RuntimeModelSources; 3],
    default_model: Option<&str>,
) -> (Vec<String>, bool) {
    let list: Vec<String> = match flag.filter(|flag| Some(flag.as_str()) != model) {
        Some(flag) => flag.split(',').map(str::to_owned).collect(),
        None => sources
            .iter()
            .find_map(|source| source.fallbacks.clone())
            .unwrap_or_default(),
    };
    let mut chain = Vec::new();
    let mut default_unresolved = false;
    for entry in list {
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }
        let resolved = if entry == "default" {
            match default_model.filter(|m| !m.is_empty()) {
                Some(m) => m,
                None => {
                    default_unresolved = true;
                    entry
                }
            }
        } else {
            entry
        };
        if !chain.iter().any(|seen| seen == resolved) {
            chain.push(resolved.to_owned());
        }
    }
    (chain, default_unresolved)
}

/// The model strings a launch may run, plus the agent name a launch named with
/// no model to decide the headline. clauth reads no agent def, so such a launch
/// is an unknown demand, which the delegate pre-flight renders with its own
/// copy and the `--auto` walk renders as the unknown-demand rung.
#[derive(Debug)]
pub(crate) struct LaunchDemand {
    pub(crate) models: Vec<String>,
    pub(crate) agent_without_model: Option<String>,
    /// clauth cannot tell which model the child runs: a `default` entry with no
    /// member `[models] default`, or a launch naming no model on a member with
    /// no default. Judged like a no-model-named launch on every check (the
    /// blanket scoped window and the unknown credit rung), beside the narrowed
    /// judgment its known families get.
    pub(crate) unknown: bool,
}

/// The model strings one chain member's session may run:
///
/// - a named model (the last `--model` in `claude_args`, else the delegate's
///   `model`) decides the headline and the agent is irrelevant;
/// - else, an agent (the last `--agent`, else the settings `agent` key by tier:
///   `--settings` passthrough > project > overlay) with no named model makes
///   the HEADLINE unknown: clauth reads no agent def, so the model the agent
///   runs cannot be told, while the fallback side stays in the demand;
/// - else the session's own model is the headline;
/// - beside the headline, ONE fallback list ([`fallback_chain`]): the last
///   `--fallback-model` value, else the highest settings source's
///   `fallbackModel` (`--settings` passthrough > project > overlay), plus
///   `CLAUDE_CODE_SUBAGENT_MODEL` as the child sees it (passthrough env, else
///   project env, else overlay env, else inherited/caller where it survives);
/// - the server-side ADVISOR model joins that same fallback side: it runs on
///   the session's own account, so an account that cannot serve it strands the
///   advisor turn like a subagent. Its source is the last non-blank `--advisor`
///   value, else the highest settings source's `advisorModel` (`--settings`
///   passthrough > project > overlay, the overlay being the profile's
///   `[models] advisor` row), and it is dropped when
///   `CLAUDE_CODE_DISABLE_ADVISOR_TOOL` is truthy ([`cc_env_truthy`]) as the
///   child sees it. A `--fallback-model` equal to the headline drops the
///   settings fallback list, never the advisor;
/// - then a `[models]` alias (`opus`/`sonnet`/`haiku`/`fable`, bare or with a
///   `[1m]`-style context suffix) remaps onto the `ANTHROPIC_DEFAULT_*_MODEL`
///   env value the child sees; a composite (`opusplan`/`best`) expands to its
///   member aliases before that remap.
///
/// The headline plus that fallback side — a subagent shares the
/// parent's process-wide credential memo, so it spends the same account on
/// whatever model it runs, and selecting for the headline model alone strands
/// it (see [`crate::fallback::start_walk`]).
///
/// `env_of` answers the inherited (start) or caller (delegate) env value for a
/// model env key; the `env_leg` decides whether that value survives the scrub
/// into the child.
#[allow(clippy::too_many_arguments)]
pub(crate) fn member_launch_demand(
    profile: &Profile,
    isolation: Isolation,
    stale_env_keys: &[String],
    claude_args: &[String],
    delegate_model: Option<&str>,
    env_leg: EnvLeg,
    cwd: Option<&Path>,
    env_of: impl Fn(&str) -> Option<String>,
) -> LaunchDemand {
    member_launch_demand_with(
        profile,
        isolation,
        stale_env_keys,
        claude_args,
        delegate_model,
        env_leg,
        cwd,
        env_of,
        false,
    )
}

/// The fallback-side models alone of a launch whose HEADLINE clauth cannot
/// tell: an unpinnable resume, which runs whatever model the transcript clauth
/// could not pin names. Same sources and alias remap as a model-less agent
/// launch, minus the agent name (none is known).
#[allow(clippy::too_many_arguments)]
pub(crate) fn member_launch_unknown_headline_models(
    profile: &Profile,
    isolation: Isolation,
    stale_env_keys: &[String],
    claude_args: &[String],
    env_leg: EnvLeg,
    cwd: Option<&Path>,
    env_of: impl Fn(&str) -> Option<String>,
) -> Vec<String> {
    member_launch_demand_with(
        profile,
        isolation,
        stale_env_keys,
        claude_args,
        None,
        env_leg,
        cwd,
        env_of,
        true,
    )
    .models
}

#[allow(clippy::too_many_arguments)]
fn member_launch_demand_with(
    profile: &Profile,
    isolation: Isolation,
    stale_env_keys: &[String],
    claude_args: &[String],
    delegate_model: Option<&str>,
    env_leg: EnvLeg,
    cwd: Option<&Path>,
    env_of: impl Fn(&str) -> Option<String>,
    unknown_headline: bool,
) -> LaunchDemand {
    let base = crate::profile::claude_dir()
        .ok()
        .and_then(|home| crate::runtime::settings_base(&home, isolation));
    let overlay = crate::claude::runtime_model_sources(base.as_deref(), profile, stale_env_keys);
    let passthrough = passthrough_settings_sources(claude_args, cwd);
    let project = project_model_sources(cwd, claude_args);

    let (arg_model, last_raw_model, arg_fallback) = parsed_model_args(claude_args);
    // The args reach the child after the delegate's `--model`, so any args
    // `--model`, a blank one included, decides the headline.
    let named_model = if last_raw_model.is_some() {
        arg_model
    } else {
        delegate_model
            .filter(|m| !m.trim().is_empty())
            .map(|m| m.to_owned())
    };
    let drop_model = last_raw_model.as_deref().or(delegate_model);
    let (fallbacks, default_unresolved) = fallback_chain(
        drop_model,
        arg_fallback,
        [&passthrough, &project, &overlay],
        profile.models.default.as_deref(),
    );
    let survives = |key: &str| match env_leg {
        EnvLeg::Process => crate::runtime::env_survives_scrub(key, stale_env_keys),
        EnvLeg::Caller => true,
    };
    // The child's value for one model env key, by the same tier precedence for
    // every key: passthrough settings env, project settings env, overlay env,
    // then the inherited/caller env where it survives the scrub.
    let child_env = |key: &str| -> Option<String> {
        passthrough
            .env_value(key)
            .or_else(|| project.env_value(key))
            .or_else(|| overlay.env_value(key))
            .or_else(|| survives(key).then(|| env_of(key)).flatten())
    };
    // The advisor model by Claude Code's source precedence, off unless the
    // kill switch the child sees is falsy. A blank flag value was ignored by
    // `last_advisor_arg`, so it cannot shadow a settings tier.
    let advisor = crate::cc_exempt::last_advisor_arg(claude_args)
        .or_else(|| passthrough.advisor.clone())
        .or_else(|| project.advisor.clone())
        .or_else(|| overlay.advisor.clone())
        .filter(|_| !child_env(PROCESS_MODEL_ENV_KEYS[2]).is_some_and(|v| cc_env_truthy(&v)));
    let settings_model = passthrough
        .model
        .clone()
        .or_else(|| project.model.clone())
        .or_else(|| overlay.model.clone());
    let session_model = child_env(PROCESS_MODEL_ENV_KEYS[0]).or(settings_model);
    let no_named_model = named_model.is_none();
    let no_session_model = session_model.is_none();

    let mut models: Vec<String>;
    let mut agent_without_model: Option<String> = None;
    if let Some(named) = named_model {
        // A named model decides the headline; the agent (named or not) is
        // irrelevant.
        models = launch_models_from(
            fallbacks,
            [advisor.clone(), child_env(PROCESS_MODEL_ENV_KEYS[1])],
        );
        models.insert(0, named);
        remap_model_aliases(&mut models, &child_env);
    } else if unknown_headline {
        // The transcript clauth could not pin names the headline, so only the
        // fallback side is known — the shape a model-less agent launch resolves.
        models = launch_models_from(
            fallbacks,
            [advisor.clone(), child_env(PROCESS_MODEL_ENV_KEYS[1])],
        );
        remap_model_aliases(&mut models, &child_env);
    } else if let Some(agent) = parsed_agent_arg(claude_args).or_else(|| {
        passthrough
            .agent
            .clone()
            .or_else(|| project.agent.clone())
            .or_else(|| overlay.agent.clone())
    }) {
        // clauth reads no agent def: the agent's model is
        // the unknown HEADLINE. The fallback side stays in `models` so the
        // delegate pre-flight refuses a credit-billed fallback family first, and
        // `launch_families` still records those families.
        agent_without_model = Some(agent);
        models = launch_models_from(
            fallbacks,
            [advisor.clone(), child_env(PROCESS_MODEL_ENV_KEYS[1])],
        );
        remap_model_aliases(&mut models, &child_env);
    } else {
        models = launch_models_from(
            fallbacks,
            [advisor.clone(), child_env(PROCESS_MODEL_ENV_KEYS[1])],
        );
        if let Some(session) = session_model {
            models.insert(0, session);
        }
        remap_model_aliases(&mut models, &child_env);
    }
    // A launch clauth cannot resolve a model for — a `default` entry with no
    // member default, or no named model, no agent and no session model (the
    // member's default is unset) — is unknown, judged like a no-model-named
    // launch on every check.
    let unknown = default_unresolved
        || (no_named_model
            && agent_without_model.is_none()
            && !unknown_headline
            && no_session_model);
    LaunchDemand {
        models,
        agent_without_model,
        unknown,
    }
}

/// Whether Claude Code reads an env value as true: its own boolean parse
/// (`De` in the 2.1.314 bundle) lowercases and trims, then accepts `1`,
/// `true`, `yes` and `on`. Anything else, an empty value included, is false.
/// The one reading `CLAUDE_CODE_DISABLE_ADVISOR_TOOL` gets here, so the demand
/// drops the advisor exactly when the child does.
pub(crate) fn cc_env_truthy(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

/// The last `--agent` value in the args, both spellings. A blank value is
/// absent: it never clears an earlier flag.
pub(crate) fn parsed_agent_arg(claude_args: &[String]) -> Option<String> {
    let mut agent = None;
    let mut args = claude_args.iter();
    while let Some(a) = args.next() {
        if let Some(v) = a.strip_prefix("--agent=")
            && !v.trim().is_empty()
        {
            agent = Some(v.to_owned());
        } else if a == "--agent"
            && let Some(v) = args.next()
            && !v.trim().is_empty()
        {
            agent = Some(v.clone());
        }
    }
    agent
}

/// The agent a resume launch runs, as far as clauth can pin it. An exact-id
/// resume (`--resume <id>`, `--resume=<id>`, `-r <id>`, `-r<id>`) whose value
/// [`crate::sessions::find_session_top_level`] finds scans that whole
/// transcript: Claude Code re-runs its last `agent-setting` agent, so the launch
/// is an agent launch. Every other resume form — `--continue`/`-c`, a bare
/// `--resume` (a following flag is no value), a session name rather than an id,
/// an id with no top-level transcript here, `-r=<id>` (Claude Code receives the
/// value `=<id>`), `--from-pr`, `--teleport`, `--cloud` — leaves clauth unable
/// to say which transcript runs, and any one of them present makes the launch
/// unpinnable. An explicit `--agent` in the args outranks the transcript, so the
/// transcript is never read then.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ResumedAgent {
    NotResumed,
    Agent(String),
    /// A resume form clauth cannot pin to a transcript: the caller fails closed
    /// as an agent launch naming no model when no model is named.
    Unknown,
}

pub(crate) fn resumed_agent(claude_args: &[String]) -> ResumedAgent {
    if parsed_agent_arg(claude_args).is_some() {
        return ResumedAgent::NotResumed;
    }
    let (values, unpinnable) = resume_forms(claude_args);
    if unpinnable {
        return ResumedAgent::Unknown;
    }
    let mut pinned = None;
    for value in values {
        match crate::sessions::find_session_top_level(value.trim()) {
            Some(session) => pinned = Some(session),
            None => return ResumedAgent::Unknown,
        }
    }
    match pinned.and_then(|session| session.agent()) {
        Some(agent) => ResumedAgent::Agent(agent),
        None => ResumedAgent::NotResumed,
    }
}

/// Every resume form in the args: the exact-id values given (`--resume <v>`,
/// `--resume=<v>`, `-r <v>`, `-r<v>`), and whether any form clauth cannot pin
/// is present (`--continue`/`-c`, `--from-pr`, `--teleport`, `--cloud`, a bare
/// `--resume` with no value or a `-`-prefixed token beside it, `-r=<v>`, whose
/// `=` the short-option spelling hands Claude Code as part of the value). A
/// value-taking `-r` in a short-flag cluster consumes the rest of its own token,
/// else the next token; a `c` in the same cluster is `--continue` with getopt
/// semantics.
fn resume_forms(claude_args: &[String]) -> (Vec<&str>, bool) {
    let mut values = Vec::new();
    let mut unpinnable = false;
    let mut i = 0;
    while i < claude_args.len() {
        let arg = claude_args[i].as_str();
        if let Some(value) = arg.strip_prefix("--resume=") {
            values.push(value);
        } else if arg == "--resume" {
            // A token starting with `-` is a flag, never a resume value
            // (`--resume --verbose` is a bare `--resume`).
            match claude_args.get(i + 1).filter(|v| !v.starts_with('-')) {
                Some(value) => {
                    values.push(value.as_str());
                    i += 1;
                }
                None => unpinnable = true,
            }
        } else if arg == "--continue"
            || arg == "--from-pr"
            || arg.starts_with("--from-pr=")
            || arg == "--teleport"
            || arg.starts_with("--teleport=")
            || arg == "--cloud"
            || arg.starts_with("--cloud=")
        {
            unpinnable = true;
        } else if let Some(shorts) = arg.strip_prefix('-').filter(|s| !s.starts_with('-')) {
            match shorts.char_indices().find(|&(_, c)| c == 'r') {
                Some((at, _)) => {
                    // The glued value is taken literally, `=` included: Claude
                    // Code's short-option parser hands `-r=x` the value `=x`.
                    let rest = &shorts[at + 1..];
                    if !rest.is_empty() {
                        values.push(rest);
                    } else {
                        match claude_args.get(i + 1).filter(|v| !v.starts_with('-')) {
                            Some(value) => {
                                values.push(value.as_str());
                                i += 1;
                            }
                            None => unpinnable = true,
                        }
                    }
                    if shorts[..at].contains('c') {
                        unpinnable = true;
                    }
                }
                None if shorts.contains('c') => unpinnable = true,
                None => {}
            }
        }
        i += 1;
    }
    (values, unpinnable)
}

/// The model strings alone of [`member_launch_demand`]: what a call site that
/// only records the resolved families (the launch row) needs, without the agent
/// name.
#[allow(clippy::too_many_arguments)]
pub(crate) fn member_launch_models(
    profile: &Profile,
    isolation: Isolation,
    stale_env_keys: &[String],
    claude_args: &[String],
    delegate_model: Option<&str>,
    env_leg: EnvLeg,
    cwd: Option<&Path>,
    env_of: impl Fn(&str) -> Option<String>,
) -> Vec<String> {
    member_launch_demand(
        profile,
        isolation,
        stale_env_keys,
        claude_args,
        delegate_model,
        env_leg,
        cwd,
        env_of,
    )
    .models
}

/// The model families a start records on its session row: what
/// [`member_launch_models`] resolves, except that an unpinnable resume naming
/// no model records its fallback side alone
/// ([`member_launch_unknown_headline_models`]) — the transcript's headline is
/// unknown, so a settings or env model is never recorded as the model this
/// launch ran. An exact-id resume's agent is folded into the demand as
/// `--agent`, the same fold the `--auto` walk makes.
#[allow(clippy::too_many_arguments)]
pub(crate) fn launch_families_for(
    profile: &Profile,
    isolation: Isolation,
    stale_env_keys: &[String],
    claude_args: &[String],
    resumed: &ResumedAgent,
    named_model: bool,
    spawn_cwd: Option<&Path>,
    env_models: &[Option<String>; 3],
) -> Vec<String> {
    let env_of = process_model_env_of(env_models);
    if matches!(resumed, ResumedAgent::Unknown) && !named_model {
        return crate::fallback::demand_from(member_launch_unknown_headline_models(
            profile,
            isolation,
            stale_env_keys,
            claude_args,
            EnvLeg::Process,
            spawn_cwd,
            env_of,
        ));
    }
    let mut folded;
    let demand_args: &[String] = match resumed {
        ResumedAgent::Agent(name) => {
            folded = claude_args.to_vec();
            folded.extend_from_slice(&["--agent".to_owned(), name.clone()]);
            &folded
        }
        _ => claude_args,
    };
    crate::fallback::demand_from(member_launch_models(
        profile,
        isolation,
        stale_env_keys,
        demand_args,
        None,
        EnvLeg::Process,
        spawn_cwd,
        env_of,
    ))
}

/// Substitute a member's `[models]` alias remap for any demand string that
/// names one of the four bare aliases (a `[1m]`-style context suffix is kept):
/// what the alias actually runs is the `ANTHROPIC_DEFAULT_*_MODEL` env value
/// the child sees, resolved through `child_env` by the same precedence as
/// `ANTHROPIC_MODEL`. A composite (`opusplan`/`best`) expands to its member
/// aliases BEFORE the remap, so each member alias routes through the same
/// remap a bare spelling would. `strip_bracket_suffix` runs on the trimmed
/// ORIGINAL (it returns a prefix, so its length is a char boundary), and the
/// alias match is ASCII-case-insensitive.
fn remap_model_aliases(models: &mut Vec<String>, child_env: &impl Fn(&str) -> Option<String>) {
    const ALIAS_KEYS: [(&str, &str); 4] = [
        ("opus", "ANTHROPIC_DEFAULT_OPUS_MODEL"),
        ("sonnet", "ANTHROPIC_DEFAULT_SONNET_MODEL"),
        ("haiku", "ANTHROPIC_DEFAULT_HAIKU_MODEL"),
        ("fable", "ANTHROPIC_DEFAULT_FABLE_MODEL"),
    ];
    let mut expanded: Vec<String> = Vec::with_capacity(models.len());
    for m in models.iter() {
        let trimmed = m.trim();
        let base = crate::pricing::strip_bracket_suffix(trimmed);
        let suffix = &trimmed[base.len()..];
        let members: Option<&[&str]> = if base.eq_ignore_ascii_case("opusplan") {
            Some(&["opus", "sonnet"])
        } else if base.eq_ignore_ascii_case("best") {
            Some(&["fable", "opus"])
        } else {
            None
        };
        match members {
            Some(members) => {
                for alias in members {
                    expanded.push(format!("{alias}{suffix}"));
                }
            }
            None => expanded.push(m.clone()),
        }
    }

    for m in &mut expanded {
        let trimmed = m.trim();
        let base = crate::pricing::strip_bracket_suffix(trimmed);
        let suffix = &trimmed[base.len()..];
        let env_key = ALIAS_KEYS
            .iter()
            .find(|(alias, _)| base.eq_ignore_ascii_case(alias))
            .map(|(_, key)| *key);
        let Some(env_key) = env_key else {
            continue;
        };
        if let Some(id) = child_env(env_key) {
            *m = format!("{id}{suffix}");
        }
    }
    *models = expanded;
}

/// The model slots a passthrough `--settings <file|json>` sets, the top
/// settings tier: above project-local. Parsed through the same helpers the
/// spawn-side settings fold uses, so one value is read one way. Empty when the
/// args carry no `--settings` or the value fails to parse (the spawn path warns;
/// the demand just reads nothing from it). Inline JSON is self-contained and
/// reads with no cwd; only the relative file form resolves against the cwd.
fn passthrough_settings_sources(
    claude_args: &[String],
    cwd: Option<&Path>,
) -> crate::claude::RuntimeModelSources {
    let Ok((Some(value), _)) = crate::cc_exempt::fold_settings_args(claude_args) else {
        return crate::claude::RuntimeModelSources::default();
    };
    let inline = crate::cc_exempt::is_inline_settings(&value);
    let parsed = if inline {
        crate::cc_exempt::parse_settings_value(&value, std::path::Path::new("."), true)
    } else {
        // An absolute path resolves without the spawn cwd; `parse_settings_value`
        // joins the base only for a relative one.
        let dir = match cwd {
            Some(dir) => dir,
            None if std::path::Path::new(value.trim()).is_absolute() => std::path::Path::new("."),
            None => return crate::claude::RuntimeModelSources::default(),
        };
        crate::cc_exempt::parse_settings_value(&value, dir, false)
    };
    let Ok(map) = parsed else {
        return crate::claude::RuntimeModelSources::default();
    };
    crate::claude::model_sources_from_json(&serde_json::Value::Object(map))
}

/// The project-tier sources a `--setting-sources` list enables, as
/// `(project, local)`. The LAST flag governs; no flag enables both (Claude
/// Code reads both project files by default). A list naming neither still
/// reads neither project file: the runtime injects `--setting-sources user`
/// only at the real `$HOME`, which [`project_model_sources`]' own home check
/// already skips, so the `user` tier is not judged here.
fn project_setting_sources(claude_args: &[String]) -> (bool, bool) {
    let mut enabled = (true, true);
    let mut args = claude_args.iter();
    while let Some(a) = args.next() {
        let value = if let Some(v) = a.strip_prefix("--setting-sources=") {
            Some(v)
        } else if a == "--setting-sources" {
            args.next().map(String::as_str)
        } else {
            None
        };
        let Some(value) = value else {
            continue;
        };
        let list: Vec<&str> = value.split(',').map(str::trim).collect();
        enabled = (list.contains(&"project"), list.contains(&"local"));
    }
    enabled
}

/// The model strings a project's committed settings put in effect for a spawn
/// in `cwd`, by Claude Code's own tier precedence (`.claude/settings.local.json`
/// over `.claude/settings.json`), per key. Empty when `cwd` is `None`, the
/// real `$HOME` (the runtime passes `--setting-sources user` there, so the
/// project tiers never apply), or the passthrough's `--setting-sources` list
/// disables a tier. A test build reads no project dir outside its sandbox
/// home, so a real checkout-root `.claude/settings*.json` never steers the
/// demand.
fn project_model_sources(
    cwd: Option<&Path>,
    claude_args: &[String],
) -> crate::claude::RuntimeModelSources {
    let mut out = crate::claude::RuntimeModelSources::default();
    let Some(dir) = cwd.filter(|dir| !crate::runtime::cwd_is_real_home(dir)) else {
        return out;
    };
    // A test build's only home is the sandbox: skip a project dir not under
    // it (mirrors `plugin_probe::on_path`). `home_dir()` panics when no
    // sandbox is held, so the `else` arm only satisfies the `Result` type.
    #[cfg(test)]
    {
        let Ok(home) = crate::profile::home_dir() else {
            return out;
        };
        if !dir.starts_with(&home) {
            return out;
        }
    }
    let (project, local) = project_setting_sources(claude_args);
    // Local outranks project, so it is read FIRST and the project tier only
    // fills keys the local tier left unset.
    for file in [
        local.then(|| dir.join(".claude").join("settings.local.json")),
        project.then(|| dir.join(".claude").join("settings.json")),
    ]
    .into_iter()
    .flatten()
    {
        let Ok(settings) = crate::profile::read_json_file::<serde_json::Value>(&file) else {
            continue;
        };
        let src = crate::claude::model_sources_from_json(&settings);
        if out.model.is_none() {
            out.model = src.model;
        }
        if out.agent.is_none() {
            out.agent = src.agent;
        }
        if out.fallbacks.is_none() {
            out.fallbacks = src.fallbacks;
        }
        if out.advisor.is_none() {
            out.advisor = src.advisor;
        }
        if out.env_anthropic.is_none() {
            out.env_anthropic = src.env_anthropic;
        }
        if out.env_subagent.is_none() {
            out.env_subagent = src.env_subagent;
        }
        if out.env_disable_advisor.is_none() {
            out.env_disable_advisor = src.env_disable_advisor;
        }
        if out.env_opus.is_none() {
            out.env_opus = src.env_opus;
        }
        if out.env_sonnet.is_none() {
            out.env_sonnet = src.env_sonnet;
        }
        if out.env_haiku.is_none() {
            out.env_haiku = src.env_haiku;
        }
        if out.env_fable.is_none() {
            out.env_fable = src.env_fable;
        }
    }
    out
}

/// The one fallback list a launch resolved, beside the advisor model and the
/// subagent env string (in that slot order), held here so the fold is testable
/// without a home or a process environment. An empty value is dropped, never a
/// family.
pub(crate) fn launch_models_from(settings: Vec<String>, env: [Option<String>; 2]) -> Vec<String> {
    let mut out = settings;
    out.extend(env.into_iter().flatten().filter(|v| !v.trim().is_empty()));
    out
}

/// The Claude Code background flag in `claude_args`, either spelling, matched
/// as a WHOLE argv word: its value is the flag itself, never a prefix of a
/// model name or a prompt.
fn background_flag(claude_args: &[String]) -> Option<&str> {
    claude_args
        .iter()
        .map(String::as_str)
        .find(|arg| *arg == "--bg" || *arg == "--background")
}

/// The refusals a background launch runs before any side effect, shared by
/// [`admit`] (so `--explain` answers what a real launch would do) and every
/// launch path through [`run`]. Order is load-bearing: the `--isolated`
/// refusal is first, and the copy-host probe (the only leg that writes) is
/// last, so a start refused for `--isolated` never materializes a profile dir.
fn refuse_background_start(
    name: &ProfileName,
    isolation: Isolation,
    follows_chain: bool,
    claude_args: &[String],
) -> Result<()> {
    // An isolated tree has no shared background pair: the daemon holds the
    // account's FIXED `runtime-bg`/`sessions-bg` only, so a session launched
    // into a throwaway isolated tree would keep no watchdog after its launcher
    // exits. Refused before the plugin preflight, so nothing spawns.
    if isolation == Isolation::Isolated
        && let Some(flag) = background_flag(claude_args)
    {
        anyhow::bail!(
            "'{name}': {flag} cannot be combined with --isolated, since background sessions \
             run in the account's shared background tree"
        );
    }

    // The fixed background pair is a COPY on a host that cannot symlink, and a
    // copied tree cannot follow a rotation that relinks the credentials a
    // background session holds — it would outlive them — so a background launch
    // is refused there before anything spawns or any tree is built. Only the
    // background path (`!follows_chain`) builds the fixed pair; a
    // `--with-fallback` launch is already refused on a copy host by
    // `refuse_unless_chain_eligible`. The isolated refusal above stays first
    // when both apply.
    if !follows_chain
        && let Some(flag) = background_flag(claude_args)
        && crate::runtime::background_copy_host(name)?
    {
        anyhow::bail!(
            "{}",
            crate::runtime::copy_host_background_refusal(name.as_str(), flag)
        );
    }
    Ok(())
}

pub(crate) fn run(
    config: &AppConfig,
    name: &ProfileName,
    claude_args: &[String],
    isolation: Isolation,
    workspace: Option<&Path>,
    follows_chain: bool,
    announce: Option<&str>,
) -> Result<()> {
    let profile = admit(config, name, isolation, follows_chain, claude_args)?;

    // Announced after the refusals, so a start that `admit` refuses never
    // announces first.
    if let Some(line) = announce {
        errln!("{line}");
    }

    // The plugin-migration pre-flight: heal a broken or divergent clauth
    // marketplace registration before the session launches, so the session loads
    // its hooks and MCP. A healthy registration costs this nothing — the gate is
    // agentgear's spawn-free pre-check (file reads, no `claude` spawn).
    // Best-effort: a failed heal is logged, never fails the start. `claude` the
    // binary (not just the plugin CLI) needs to be on PATH anyway for the spawn
    // below to succeed, and an uninstalled plugin clears its stale marker
    // once, then reads Clean.
    crate::plugin_host::preflight();

    // Strip the outgoing profile's custom env from the inherited base so a
    // `clauth start <other>` session doesn't inherit it. The live
    // `settings.json` is owned by whoever is active; starting that same profile
    // passes its own keys, which the merge re-inserts (no-op). With no active
    // marker to read (`switch_off` clears it without touching the file) the
    // helper answers every configured profile's keys, so a departed account's
    // entries are stripped too rather than landing in front of the started
    // account's endpoint.
    let stale_env_keys = crate::actions::outgoing_env_keys(config);

    let runtime = {
        let _spinner = Spinner::start("clauth: preparing runtime");
        // A background launch lands on the account's one fixed tree, which the
        // daemon adopts on its next tick. `--with-fallback` keeps its own
        // per-launch tree: the fixed tree never follows the chain.
        if background_flag(claude_args).is_some() && !follows_chain {
            ProfileRuntime::acquire_background(
                &profile.name,
                BackgroundHold::Launcher,
                &stale_env_keys,
                background_flag(claude_args),
            )?
        } else {
            ProfileRuntime::acquire(
                profile,
                isolation,
                &stale_env_keys,
                follows_chain,
                crate::live_sessions::Launcher::Own,
            )?
        }
    };
    // A shared store `~/.claude` refused to create would otherwise fail
    // silently: surface the consequence once the spinner is off, the same
    // `errln!` shape the `cc_exempt` warning below uses.
    for note in runtime.store_notes() {
        errln!("{note}");
    }

    #[cfg(unix)]
    let signal_watcher = SignalWatcher::new()?;

    // Through the runtime-spawn seam: this is a claude session, and the codex
    // engine plugs into the same three calls when its runtime lands.
    let engine: &dyn crate::harness::HarnessEngine = &crate::harness::ClaudeEngine;
    let mut command = engine.command();
    // Scrub clauth-managed + outgoing custom env so a session started under
    // profile B doesn't inherit profile A's endpoint/auth/model overrides from
    // the parent process env. The target's runtime settings.json re-supplies
    // whichever it defines. Mirrors the delegate path (run_delegate).
    engine.scrub_env(&mut command, &stale_env_keys);
    // A resume pins `claude` to the session's workspace; a normal start inherits
    // this process's cwd. Either way the resolved dir feeds the home-project
    // settings guard: when it is the real `$HOME`, its project-tier settings
    // lookup would hit the real `~/.claude/settings.json` and re-leak the
    // globally active profile's env, outranking the runtime settings.json below.
    let spawn_cwd = apply_spawn_cwd(&mut command, workspace);
    if let Some(cwd) = spawn_cwd.as_deref() {
        crate::runtime::guard_home_project_settings(&mut command, cwd);
    }
    command.env(engine.home_env_key(), runtime.config_dir());
    // Isolated: also suppress global/project MCP servers wired through
    // `.claude.json`, so the only extension surface is what the caller passes.
    // Deliberately NOT `--safe-mode`. The cross-account leak (the operator's
    // `~/.claude/plugins`) is already gone under the empty config dir. What
    // remains is a cwd `.claude/skills/*` plugin: project-local and trust-gated,
    // loading the same regardless of active account (like project CLAUDE.md).
    // `--safe-mode` would also nuke cwd CLAUDE.md + skills, so it stays off.
    if isolation == Isolation::Isolated {
        command.arg("--strict-mcp-config");
    }
    // Marks this run's window: on the shared global store, only sessions touched
    // at or after this instant are attributed to `name` (see stamp below).
    let run_start = SystemTime::now();
    if let Some(cwd) = spawn_cwd.as_deref() {
        let settings_file =
            crate::runtime::spawn_settings_file(runtime.config_dir(), runtime.session_id());
        if let Some(warning) = crate::cc_exempt::apply_spawn_settings(
            &mut command,
            runtime.config_dir(),
            cwd,
            claude_args,
            &crate::cc_exempt::hook_command(),
            &settings_file,
        ) {
            errln!("{warning}");
        }
    } else {
        command.args(claude_args);
    }
    // Resolve the launch demand's model families before the spawn so the
    // post-spawn flock hold covers only the field writes: the row's update
    // below takes the state flock, and the demand must name what this launch
    // actually ran.
    let resumed = resumed_agent(claude_args);
    let named_model = parsed_model_args(claude_args).0.is_some();
    let env_models = process_model_env();
    let launch_families = launch_families_for(
        profile,
        isolation,
        &stale_env_keys,
        claude_args,
        &resumed,
        named_model,
        spawn_cwd.as_deref(),
        &env_models,
    );
    let mut child = command.spawn().context("failed to spawn claude")?;
    // Record the child's pid and the launch demand's resolved families on the
    // row once the child exists: the daemon's spend-cap decision leg joins a
    // 429 transcript to this session through the pid, and decides a
    // credit-billed leave off the recorded families — it has no process env or
    // passthrough args, so re-deriving the demand there would read today's
    // config instead of what this launch actually ran.
    let _ = crate::live_sessions::update_as_session(runtime.session_id(), |fields| {
        fields.set_claude_pid(child.id());
        fields.set_launch_families(launch_families);
        fields.set_credit_leave_aware();
    });

    #[cfg(unix)]
    let outcome = wait_for_child(&mut child, signal_watcher.receiver())?;

    #[cfg(not(unix))]
    let outcome = ChildOutcome {
        status: child
            .wait()
            .context("failed to wait for the session child")?,
        signal: None,
    };

    // Record which sessions ran under this profile before teardown — an isolated
    // store is discarded on drop, so its stamp must happen while `runtime` lives.
    // Isolated: the store is exclusive, so every transcript maps to `name`.
    // Shared: transcripts land in the global store, so only this run's window is.
    // Best-effort; never fails the completed session.
    let isolated = isolation == Isolation::Isolated;
    let projects_dir = if isolated {
        Some(runtime.config_dir().join("projects"))
    } else {
        crate::profile::claude_dir()
            .ok()
            .map(|d| d.join("projects"))
    };
    if let Some(projects_dir) = projects_dir {
        crate::sessions::stamp_run_sessions(name, &projects_dir, isolated, run_start);
    }

    // Rescue (isolated only): the throwaway isolated store leaves its live name
    // on `drop(runtime)`, so lift the session's state into the global store
    // first; the drop keeps whatever this rescue could not land for the next
    // sweep. A shared start needs nothing here — its transcripts already live
    // in the global store.
    if isolated && let Ok(claude_home) = crate::profile::claude_dir() {
        rescue_teardown(runtime.config_dir(), runtime.sessions_dir(), &claude_home);
    }

    // Drop runtime before process::exit so final sync + refcount cleanup runs.
    drop(runtime);

    let code = status_code(outcome.status, outcome.signal);
    if code != 0 {
        std::process::exit(code);
    }
    Ok(())
}

/// Resolve the directory the spawned `claude` runs in and pin `command` to it.
/// `Some(dir)` sets the child's cwd to that workspace (a resume); `None` leaves
/// `command` inheriting this process's cwd (a normal start), so the `None` path
/// is byte-for-byte the pre-resume behavior. Returns the resolved dir so the
/// caller feeds the same path to the home-project settings guard, whose lookup
/// is cwd-based.
fn apply_spawn_cwd(
    command: &mut std::process::Command,
    workspace: Option<&Path>,
) -> Option<PathBuf> {
    match workspace {
        Some(dir) => {
            command.current_dir(dir);
            Some(dir.to_path_buf())
        }
        None => std::env::current_dir().ok(),
    }
}

fn status_code(status: ExitStatus, signal: Option<i32>) -> i32 {
    if status.success() {
        return signal.map_or(0, |s| 128 + s);
    }

    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        status
            .code()
            .unwrap_or_else(|| status.signal().map(|s| 128 + s).unwrap_or(1))
    }
    #[cfg(not(unix))]
    status.code().unwrap_or(1)
}

#[cfg(unix)]
struct SignalWatcher {
    handle: SignalHandle,
    thread: Option<JoinHandle<()>>,
    rx: Receiver<i32>,
}

#[cfg(unix)]
impl SignalWatcher {
    fn new() -> Result<Self> {
        let mut signals =
            Signals::new([SIGINT, SIGTERM]).context("failed to install signal handlers")?;
        let handle = signals.handle();
        let (tx, rx) = channel();
        #[allow(clippy::expect_used, reason = "thread spawn failure is unrecoverable")]
        let thread = std::thread::Builder::new()
            .name("clauth-sig".into())
            .spawn(move || {
                for signal in signals.forever() {
                    if tx.send(signal).is_err() {
                        break;
                    }
                }
            })
            .expect("failed to spawn signal watcher thread");
        Ok(Self {
            handle,
            thread: Some(thread),
            rx,
        })
    }

    fn receiver(&self) -> &Receiver<i32> {
        &self.rx
    }
}

#[cfg(unix)]
impl Drop for SignalWatcher {
    fn drop(&mut self) {
        self.handle.close();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(unix)]
fn wait_for_child(
    child: &mut std::process::Child,
    signals: &Receiver<i32>,
) -> Result<ChildOutcome> {
    loop {
        if let Some(status) = child
            .try_wait()
            .context("failed to wait for the session child")?
        {
            return Ok(ChildOutcome {
                status,
                signal: next_signal(signals),
            });
        }

        match signals.recv_timeout(CHILD_WAIT_INTERVAL) {
            Ok(signal) => {
                forward_signal_or_warn(child, signal);
                return wait_after_signal(child, signals, signal);
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => std::thread::sleep(CHILD_WAIT_INTERVAL),
        }
    }
}

#[cfg(unix)]
fn wait_after_signal(
    child: &mut std::process::Child,
    signals: &Receiver<i32>,
    first_signal: i32,
) -> Result<ChildOutcome> {
    let mut signal = first_signal;
    loop {
        match child
            .try_wait()
            .context("failed to wait for the session child")?
        {
            Some(status) => {
                return Ok(ChildOutcome {
                    status,
                    signal: Some(signal),
                });
            }
            None => match signals.recv_timeout(CHILD_WAIT_INTERVAL) {
                Ok(next) => {
                    signal = next;
                    forward_signal_or_warn(child, next);
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => std::thread::sleep(CHILD_WAIT_INTERVAL),
            },
        }
    }
}

#[cfg(unix)]
fn next_signal(signals: &Receiver<i32>) -> Option<i32> {
    signals.try_recv().ok()
}

#[cfg(unix)]
fn forward_signal_or_warn(child: &std::process::Child, signal: i32) {
    if let Err(e) = forward_signal(child, signal)
        && e.raw_os_error() != Some(libc::ESRCH)
    {
        logline!("clauth: failed to forward signal to claude: {e}");
    }
}

#[cfg(unix)]
#[allow(unsafe_code)]
fn forward_signal(child: &std::process::Child, signal: i32) -> std::io::Result<()> {
    // SAFETY: `child.id()` is the OS pid for this live child; `signal` comes from signal-hook.
    let result = unsafe { libc::kill(child.id() as libc::pid_t, signal) };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// A path rendered as a TOML string for a `-c key=<value>` override. Literal
/// (single-quoted) is preferred: it processes no escapes, so a Windows path's
/// backslashes survive verbatim. A path carrying a single quote cannot be a
/// literal string at all, so it falls back to a basic string with the two
/// characters TOML requires escaped there.
fn toml_path_value(path: &Path) -> String {
    let raw = path.display().to_string();
    if raw.contains('\'') {
        format!("\"{}\"", raw.replace('\\', "\\\\").replace('"', "\\\""))
    } else {
        format!("'{raw}'")
    }
}

/// The spawn command for a codex session on `home`, split from [`run_codex`]
/// so the wire facts are pinned without spawning anything: the `CODEX_HOME`
/// pin, the scrub, and the forced file store BEFORE the passthrough args.
///
/// The store override (decision 6): the session's config.toml is a COPY of
/// the operator's, and a keyring/auto setting in it would make codex ignore
/// the linked auth.json — and delete it on the first refresh. Forced on every
/// clauth spawn, never demanded of the operator's own config (the capture
/// path owns that refusal). The value carries its TOML quotes as literal arg
/// bytes, so codex's `-c` override parser reads a well-formed TOML string
/// rather than leaning on its bare-word fallback. Caller args come AFTER, so
/// a later `-c` of the same key wins in codex's layering — overriding the
/// store re-breaks the linked auth.json for that one run, the same class of
/// self-inflicted foot-gun as `claude --settings` against a clauth runtime.
///
/// The state-DB override, the same reasoning one layer down: the store the
/// sqlite DBs live in is a config KEY (`sqlite_home`) as well as an env var,
/// and the session's config.toml is that same copy of the operator's. Left
/// alone, every profile's goals/logs/memories/state DBs land in whichever one
/// directory the operator named — the home's durable links are never opened
/// through, and two accounts share one conversation history. Scrubbing the env
/// cannot reach it, since the key outranks the variable, so it is pinned to the
/// home clauth just set: exactly what codex resolves when neither is spelled.
fn codex_spawn_command(
    home: &Path,
    codex_args: &[String],
    active_env_keys: &[String],
) -> std::process::Command {
    let engine = crate::harness::Harness::Codex.engine();
    let mut command = engine.command();
    engine.scrub_env(&mut command, active_env_keys);
    command.env(engine.home_env_key(), home);
    command.arg("-c").arg("cli_auth_credentials_store=\"file\"");
    command
        .arg("-c")
        .arg(format!("sqlite_home={}", toml_path_value(home)));
    command.args(codex_args);
    command
}

/// What codex's managed config does to a clauth-built home. Read before a
/// codex spawn: the managed layer sits ABOVE the session's `-c` flags in
/// codex's config stack (`config_layer_source.rs`: session flags 30, the
/// managed file 40), so a key set there defeats the forced store and state-DB
/// home that [`codex_spawn_command`] pins, and nothing clauth passes can
/// outrank it.
#[derive(Debug, PartialEq, Eq)]
enum ManagedConfigVerdict {
    /// Nothing there reaches the keys clauth forces or strips.
    Clear,
    /// The spawn proceeds; the line goes to stderr.
    Warn(String),
    /// The spawn is refused with the line.
    Refuse(String),
}

/// Where codex reads its managed config: the system path on unix. On windows
/// codex defaults it to `<CODEX_HOME>/managed_config.toml`, and the session's
/// `CODEX_HOME` is the home clauth just built, which holds none.
fn managed_config_path() -> Option<PathBuf> {
    #[cfg(test)]
    if let Some(path) = MANAGED_CONFIG_OVERRIDE.lock().ok().and_then(|g| g.clone()) {
        return Some(path);
    }
    #[cfg(unix)]
    {
        Some(PathBuf::from("/etc/codex/managed_config.toml"))
    }
    #[cfg(not(unix))]
    {
        None
    }
}

/// Test-only [`managed_config_path`] override: the system path is root-owned,
/// so the spawn-site read in [`run_codex`] is otherwise unreachable from a
/// test. Serialized by `profile::HOME_TEST_LOCK`. Never compiled into the
/// binary.
#[cfg(test)]
static MANAGED_CONFIG_OVERRIDE: std::sync::Mutex<Option<PathBuf>> = std::sync::Mutex::new(None);

#[cfg(test)]
fn set_managed_config_override(path: &Path) {
    if let Ok(mut guard) = MANAGED_CONFIG_OVERRIDE.lock() {
        *guard = Some(path.to_path_buf());
    }
}

#[cfg(test)]
fn clear_managed_config_override() {
    if let Ok(mut guard) = MANAGED_CONFIG_OVERRIDE.lock() {
        *guard = None;
    }
}

/// Test-only RAII: point the spawn-site managed-config read at `path` for the
/// guard's lifetime, clearing the process-global override on drop even if the
/// test panics. It BORROWS the [`HomeSandbox`](crate::testutil::HomeSandbox)
/// for the reason `testutil::EndpointSandbox` does: the override is serialized
/// by `HOME_TEST_LOCK`, which the home sandbox holds, so dropping the home
/// first is E0505 at compile time instead of a race nothing checks.
#[cfg(test)]
struct ManagedConfigSandbox<'a>(std::marker::PhantomData<&'a crate::testutil::HomeSandbox>);

#[cfg(test)]
impl<'a> ManagedConfigSandbox<'a> {
    fn new(_home: &'a crate::testutil::HomeSandbox, path: &Path) -> Self {
        set_managed_config_override(path);
        Self(std::marker::PhantomData)
    }
}

#[cfg(test)]
impl Drop for ManagedConfigSandbox<'_> {
    fn drop(&mut self) {
        clear_managed_config_override();
    }
}

/// The verdict over the managed file at `path`. Absent, unreadable or
/// unparseable is [`ManagedConfigVerdict::Clear`]: there is nothing to
/// outrank the spawn with, and a file codex cannot parse is codex's own
/// refusal. The keys are exactly the ones [`codex_spawn_command`] forces and
/// `runtime::copy_codex_config` strips: the two that kill the chain refuse
/// (a non-file store unbinds the linked `auth.json`: codex reads the keyring
/// or memory past it, and `keyring`/`auto` delete it on their first save; a
/// lockfile `load_path` replays that file as the WHOLE config, erasing the
/// `-c` layer, where the table's other keys only export), and a moved
/// state-DB home warns, since the session still runs on its own chain.
fn managed_config_verdict(path: &Path) -> ManagedConfigVerdict {
    let Ok(raw) = std::fs::read_to_string(path) else {
        return ManagedConfigVerdict::Clear;
    };
    let Ok(parsed) = toml::from_str::<toml::Value>(&raw) else {
        return ManagedConfigVerdict::Clear;
    };
    let Some(table) = parsed.as_table() else {
        return ManagedConfigVerdict::Clear;
    };
    let file = path.display();
    if let Some(store) = table.get("cli_auth_credentials_store")
        && store.as_str() != Some("file")
    {
        return ManagedConfigVerdict::Refuse(format!(
            "{file} sets cli_auth_credentials_store = {store}, and a managed config outranks \
             the file store clauth forces at spawn, so codex would ignore this session's \
             linked auth.json. ask whoever manages this machine to remove the key or set it \
             to \"file\"; clauth cannot override a managed config"
        ));
    }
    if let Some(load_path) = table
        .get("debug")
        .and_then(toml::Value::as_table)
        .and_then(|debug| debug.get("config_lockfile"))
        .and_then(toml::Value::as_table)
        .and_then(|lockfile| lockfile.get("load_path"))
    {
        return ManagedConfigVerdict::Refuse(format!(
            "{file} sets debug.config_lockfile.load_path = {load_path}, and a managed config \
             outranks the flags clauth passes at spawn, so codex would replay that lockfile \
             as its whole config and drop the file store this session's linked auth.json \
             depends on. ask whoever manages this machine to remove the key; clauth cannot \
             override a managed config"
        ));
    }
    if let Some(sqlite_home) = table.get("sqlite_home") {
        return ManagedConfigVerdict::Warn(format!(
            "{file} sets sqlite_home = {sqlite_home}, which outranks the per-session home \
             clauth pins at spawn, so every profile's state dbs land in that one directory"
        ));
    }
    ManagedConfigVerdict::Clear
}

/// `clauth start <codex-profile>` — spawn an interactive `codex` against this
/// session's own clauth-built home. The claude start's extras have no codex
/// counterpart and are absent on purpose: no usage priming (codex usage is
/// passive), no run-window transcript stamping or rescue (codex owns its own
/// `sessions/`, which the shared flavor links into the profile store), no
/// fallback watchdog (a codex chain lands at the next start), no home-project
/// settings guard (project-tier settings are a Claude Code concept).
///
/// `codex_args` pass through verbatim, AFTER clauth's own `-c` store override
/// — later `-c` occurrences win in codex's config layering, but overriding
/// the store mode simply re-breaks the linked `auth.json` for that one run,
/// the same class of self-inflicted foot-gun as `claude --settings` against a
/// clauth runtime.
pub(crate) fn run_codex(
    config: &AppConfig,
    name: &str,
    codex_args: &[String],
    isolation: Isolation,
) -> Result<()> {
    // Before the runtime exists: a refused start never materializes a home.
    if let Some(path) = managed_config_path() {
        match managed_config_verdict(&path) {
            ManagedConfigVerdict::Clear => {}
            ManagedConfigVerdict::Warn(line) => errln!("clauth: {line}"),
            ManagedConfigVerdict::Refuse(line) => anyhow::bail!(line),
        }
    }

    // The ACTIVE CLAUDE profile's custom env, scrubbed like any spawn: those
    // keys reached this process from the live settings.json and describe a
    // claude account, not this codex session.
    let active_env_keys: Vec<String> = config
        .state
        .active_profile
        .as_deref()
        .map(crate::profile::ProfileName::from)
        .and_then(|n| config.find(&n))
        .map(|p| p.env.keys().cloned().collect())
        .unwrap_or_default();

    let runtime = {
        let _spinner = Spinner::start("clauth: preparing codex home");
        crate::runtime::CodexRuntime::acquire(name, isolation)?
    };

    let mut command = codex_spawn_command(runtime.home(), codex_args, &active_env_keys);

    // The same signal discipline as the claude start: without it a SIGTERM to
    // clauth skips the teardown — its carry-backs are lost, the flock releases
    // with the codex child still RUNNING, and a rotation then reads the
    // account as idle while a live session holds its chain, which is the
    // precise burn the marker exists to prevent.
    #[cfg(unix)]
    let signal_watcher = SignalWatcher::new()?;

    let mut child = command.spawn().with_context(|| {
        "failed to launch codex. Is the `codex` CLI installed and on PATH?".to_string()
    })?;

    #[cfg(unix)]
    let outcome = wait_for_child(&mut child, signal_watcher.receiver())?;
    #[cfg(not(unix))]
    let outcome = ChildOutcome {
        status: child
            .wait()
            .context("failed to wait for the session child")?,
        signal: None,
    };

    // Teardown before the exit so the carry-backs and marker release run.
    drop(runtime);

    let code = status_code(outcome.status, outcome.signal);
    if code != 0 {
        std::process::exit(code);
    }
    Ok(())
}

#[cfg(test)]
#[path = "../tests/inline/start.rs"]
mod tests;
