//! `clauth mcp` — MCP JSON-RPC 2.0 server over stdio (rmcp).
//!
//! Exposes clauth profiles to a live Claude Code session: list/usage, switch,
//! and delegate. The rest of the binary stays synchronous; [`serve`] builds a
//! scoped current-thread tokio runtime and blocks on the stdio server.
//!
//! All logging MUST go to stderr — stdout carries the JSON-RPC frame.

mod jobs;
mod render;

use std::collections::HashMap;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::Result;
use rmcp::{
    ErrorData, ServerHandler,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{CallToolResult, ContentBlock, ServerCapabilities, ServerInfo},
    schemars, tool, tool_handler, tool_router,
};
use serde::Deserialize;

use crate::profile::{AppConfig, load_config};
use crate::profile_cache::{THIRD_PARTY_CACHE_FILE, USAGE_CACHE_FILE, load_profile_cache};
use crate::profile_json::{provider_label, tier_label, windows_json};
use crate::providers::ThirdPartyStats;
use crate::runtime::{Isolation, ProfileRuntime};
use crate::usage::{UsageInfo, UsageWindow, now_epoch_secs, now_ms};
use render::ProfileSnapshot;

/// Default per-call delegate timeout (seconds) when the caller doesn't set one.
const DEFAULT_RUN_TIMEOUT_SECS: u64 = 300;
/// Hard ceiling on a caller-supplied delegate timeout (seconds).
const MAX_RUN_TIMEOUT_SECS: u64 = 3600;
/// Raise the delegate's max output budget above CC's default so a long headless
/// build doesn't die on the 32k cap. Overridable via the `env` arg.
const DEFAULT_MAX_OUTPUT_TOKENS: &str = "64000";

/// Compact per-model throughput rows for a profile (observed tok/s, degraded /
/// rate-limited flags). Empty array when clauth has launched no runs for it.
fn throughput_json(profile: &str, now: i64) -> serde_json::Value {
    let rows: Vec<serde_json::Value> = crate::throughput::summary(profile, now)
        .into_iter()
        .map(|m| {
            serde_json::json!({
                "model": m.model,
                "tok_s": (m.tok_s * 10.0).round() / 10.0,
                "samples": m.samples,
                "degraded": m.degraded,
                "rate_limited_recent": m.rate_limited_recent,
                "retry_after_s": m.retry_after_s,
            })
        })
        .collect();
    serde_json::Value::Array(rows)
}

/// Fresh-from-cache 5h/7d windows for a profile. Each call re-reads the disk
/// cache (no caching across tool calls per the design).
fn load_windows(name: &str) -> (Option<UsageWindow>, Option<UsageWindow>) {
    match load_profile_cache::<UsageInfo>(name, USAGE_CACHE_FILE) {
        Some(u) => (u.five_hour, u.seven_day),
        None => (None, None),
    }
}

/// Live footer for the current active profile, read fresh from cache.
fn active_footer(config: &AppConfig) -> String {
    let active = config.state.active_profile.as_deref();
    let (five_h, seven_d) = match active {
        Some(name) => load_windows(name),
        None => (None, None),
    };
    render::live_footer(active, five_h.as_ref(), seven_d.as_ref())
}

/// Append the live footer to a JSON text payload as a second content block.
fn with_footer(json: serde_json::Value, footer: String) -> Vec<ContentBlock> {
    vec![
        ContentBlock::text(json.to_string()),
        ContentBlock::text(footer),
    ]
}

#[derive(Clone)]
pub(crate) struct ClauthServer {
    // consumed by the `#[tool_handler]` macro at dispatch time; rustc's
    // dead-code pass can't see through the macro plumbing.
    #[allow(dead_code)]
    tool_router: ToolRouter<Self>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub(crate) struct SwitchArgs {
    /// Profile name to relink the global active credentials to.
    name: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub(crate) struct DelegateArgs {
    /// Profile name to run the headless delegate session under.
    profile: String,
    /// Prompt passed to the delegated `claude -p` session.
    prompt: String,
    /// Optional model override for the delegated session.
    model: Option<String>,
    /// Working directory for the delegate (must exist). Defaults to the MCP
    /// server's cwd. Set a clean dir to keep the delegate from picking up a
    /// project `CLAUDE.md`.
    cwd: Option<String>,
    /// Extra environment variables for the delegate (e.g.
    /// `CLAUDE_CODE_MAX_OUTPUT_TOKENS`). `CLAUDE_CONFIG_DIR` and the depth guard
    /// are always set by clauth and cannot be overridden here.
    env: Option<HashMap<String, String>>,
    /// Extra arguments appended to the `claude` invocation (after clauth's own
    /// `-p`/`--output-format json`, the isolated-only `--strict-mcp-config`, and
    /// `--model <model>` when `model` is set).
    args: Option<Vec<String>>,
    /// Per-call timeout in seconds (1..=3600). Defaults to 300.
    timeout_secs: Option<u64>,
    /// Run authenticated but without operator memory/plugins/hooks (a clean
    /// blind session). Defaults to false.
    isolated: Option<bool>,
    /// Return a `{job_id}` immediately instead of blocking for the result. The
    /// delegate runs on a detached task; collect the result via the auto-delivery
    /// hook or `delegate_result({job_id})`. Defaults to false.
    background: Option<bool>,
    /// Opt into progress reporting for a `background` run: a `delegate_result`
    /// poll on the still-running job then also reports the target profile's live
    /// usage windows (`quota`) alongside `elapsed_secs`. No effect on a blocking
    /// call. Defaults to false.
    monitor: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub(crate) struct DelegateResultArgs {
    /// Job id returned by a `delegate` call made with `background: true`.
    job_id: String,
    /// Seconds to long-poll for completion before returning (0..=60, default 0 =
    /// reply instantly with the current state).
    wait_secs: Option<u64>,
}

#[tool_router]
impl ClauthServer {
    pub(crate) fn new() -> Self {
        Self {
            tool_router: Self::tool_router(),
        }
    }

    #[tool(
        description = "List all clauth profiles from disk cache (zero quota). Per profile: \
`name`, `active` (is this the currently active profile), `provider` (`anthropic` or a recognised \
third-party name), and `base_url` (endpoint URL, null for a default OAuth profile) identify it; \
`tier` is the account plan label (e.g. `Max 5x`, or `canceled` once the org's subscription is \
dead), null for a third-party/API-key profile or when no plan data is cached yet; \
`windows[]` carries the 5h, 7d, and per-model weekly (`7d <model>`) `{label, utilization_pct, \
resets_at}` where `utilization_pct` is the percent of that window already USED (higher = less \
headroom) and `resets_at` is ISO-8601; \
`has_live_session` = a clauth-managed `claude` session currently owns it; `throughput[]` = \
observed per-model `{model, tok_s, samples, degraded, rate_limited_recent, retry_after_s}` from \
past `delegate` calls; \
`third_party` = a cached one-line headline for provider-key profiles (deepseek/zai/…)"
    )]
    async fn list_profiles(&self) -> Result<CallToolResult, ErrorData> {
        let config = load_config().map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
        let now = now_epoch_secs();

        let profiles: Vec<serde_json::Value> = config
            .profiles
            .iter()
            .map(|p| {
                let name = p.name.as_str();
                let third_party = if p.is_third_party() {
                    load_profile_cache::<ThirdPartyStats>(name, THIRD_PARTY_CACHE_FILE)
                        .as_ref()
                        .map(render::third_party_headline)
                } else {
                    None
                };
                serde_json::json!({
                    "name": name,
                    // Per-slot truth (CDX-1): a codex profile reports the
                    // codex slot, a claude profile the claude slot.
                    "active": if p.is_codex() {
                        config.is_active_codex(name)
                    } else {
                        config.is_active(name)
                    },
                    "harness": if p.is_codex() { "codex" } else { "claude" },
                    "provider": provider_label(p),
                    "base_url": p.base_url,
                    "tier": tier_label(p),
                    "has_live_session": crate::runtime::has_live_session(name),
                    "windows": windows_json(name),
                    "third_party": third_party,
                    "throughput": throughput_json(name, now),
                })
            })
            .collect();

        let payload = serde_json::json!({ "profiles": profiles });
        Ok(CallToolResult::success(vec![ContentBlock::text(
            payload.to_string(),
        )]))
    }

    #[tool(
        description = "Report which profile owns the credentials this session loaded. `source` \
explains how it resolved: `refresh_match` (a profile's stored token matches the live credentials), \
`session_dir` (this session's runtime dir pins the profile), `credential_less_active` (the \
configured active profile, with no credentials on disk to match). Appends a live usage footer (% used)"
    )]
    async fn which(&self) -> Result<CallToolResult, ErrorData> {
        let config = load_config().map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
        let resolved = crate::which::resolve_active(&config);
        let throughput = resolved
            .as_ref()
            .map(|(name, _)| throughput_json(name, now_epoch_secs()))
            .unwrap_or_else(|| serde_json::Value::Array(Vec::new()));
        let tier = resolved.as_ref().and_then(|(name, _)| {
            config
                .profiles
                .iter()
                .find(|p| p.name.as_str() == name.as_str())
                .and_then(tier_label)
        });
        let payload = serde_json::json!({
            "profile": resolved.as_ref().map(|(name, _)| name),
            "source": resolved.as_ref().map(|(_, source)| source.as_str()),
            "tier": tier,
            "throughput": throughput,
        });
        Ok(CallToolResult::success(with_footer(
            payload,
            active_footer(&config),
        )))
    }

    #[tool(
        description = "Relink the global active profile (`~/.claude` credentials). A `clauth start` session is pinned to its own runtime and unaffected; a session on the global credentials adopts the change on its next token refresh"
    )]
    async fn switch(
        &self,
        Parameters(SwitchArgs { name }): Parameters<SwitchArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let config = load_config().map_err(|e| ErrorData::internal_error(e.to_string(), None))?;

        // Resolve the raw tool argument to a stored profile (case-insensitive)
        // BEFORE any mutation — the same guard the CLI applies. Skipping it lets an
        // unknown/wrong-case name reach `link_profile_credentials`, which strips the
        // live `.credentials.json` symlink and creates no replacement (it only errors
        // later at `finish_switch`), leaving the global session credential-less.
        let Some(name) = config.canonical_name(&name) else {
            let payload =
                serde_json::json!({ "ok": false, "reason": format!("profile not found: {name}") });
            return Ok(CallToolResult::error(with_footer(
                payload,
                active_footer(&config),
            )));
        };
        let on_divergence = config.state.default_divergence;

        // `switch_profile_noninteractive` can block on the macOS keychain deadline
        // (up to 20s) and may refresh the target over HTTP (its AUTH-1 gate);
        // keep it off the async worker so neither stalls the runtime. Mirrors
        // `delegate`'s `spawn_blocking`. The shared-handle wrap is what the
        // gate's refresh path requires (it must lock/unlock around HTTP).
        let (config, outcome) = tokio::task::spawn_blocking(move || {
            let config = std::sync::Arc::new(crate::lockorder::RankedMutex::new(config));
            let outcome = crate::actions::switch_profile_noninteractive(
                &config,
                &name,
                on_divergence,
                crate::oauth::refresh_result,
            );
            (config, outcome)
        })
        .await
        .map_err(|e| ErrorData::internal_error(format!("switch task failed: {e}"), None))?;
        #[allow(clippy::expect_used, reason = "mutex poisoning is unrecoverable")]
        let config = config.lock().expect("config mutex poisoned");

        match outcome {
            Ok((previous, active)) => {
                let payload = serde_json::json!({
                    "ok": true,
                    "previous": previous,
                    "active": active,
                });
                Ok(CallToolResult::success(with_footer(
                    payload,
                    active_footer(&config),
                )))
            }
            Err(e) => {
                let payload = serde_json::json!({ "ok": false, "reason": e.to_string() });
                Ok(CallToolResult::error(with_footer(
                    payload,
                    active_footer(&config),
                )))
            }
        }
    }

    #[tool(
        description = "Delegate a headless task to a profile; SPENDS that account's real usage \
window. The depth-1 cap blocks only a nested clauth `delegate` (a delegate cannot delegate again); \
in-delegate subagents run, but under the SAME delegated profile, not other accounts. For a \
one-shot task, prefer `isolated: true`: a clean blind session that skips the operator persona \
(the runtime's `CLAUDE.md`, plugins, hooks, skills) and loads no MCP servers, so it is cheaper \
(and on an API-key profile, fewer billed tokens). A shared delegate (the default) instead inherits \
that persona plus the runtime config-dir's MCP servers; use it only when the task needs repo tools \
/ codebase nav. Scope a shared delegate with `args:[\"--mcp-config\",\"<json|path>\",\"--strict-mcp-config\"]`. \
Separately, a delegate loads the project `CLAUDE.md` of its `cwd` (defaults to this server's cwd) \
regardless of `isolated`, so set `cwd` to a clean dir for a one-shot to avoid an unrelated \
project's house-style. Optional cwd/env/args/timeout_secs/isolated shape the spawned `claude`. \
Returns the delegate envelope (`result`, \
`is_error`, `total_cost_usd`, token usage): read `total_cost_usd`/usage to self-throttle; the \
`result` is the delegate's own self-report, so spot-verify it like any subagent. Set \
`background: true` to get a `{job_id}` back at once instead of blocking; the result auto-arrives \
via a hook, or fetch it with `delegate_result({job_id})`. Add `monitor: true` so a \
`delegate_result` poll on the still-running job reports `elapsed_secs` + the target's live `quota`"
    )]
    async fn delegate(
        &self,
        Parameters(DelegateArgs {
            profile,
            prompt,
            model,
            cwd,
            env,
            args,
            timeout_secs,
            isolated,
            background,
            monitor,
        }): Parameters<DelegateArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        // Fail closed: a present-but-unparseable value is treated as max depth
        // (refuse), so a corrupt env can never re-enable delegation. Only a truly
        // absent var is depth 0.
        let depth: u32 = match std::env::var(MCP_DEPTH_ENV) {
            Ok(v) => v.trim().parse().unwrap_or(u32::MAX),
            Err(_) => 0,
        };
        if depth >= 1 {
            let payload = serde_json::json!({
                "profile": profile,
                "is_error": true,
                "result": "delegation depth exceeded (max 1)",
            });
            return Ok(CallToolResult::error(vec![ContentBlock::text(
                payload.to_string(),
            )]));
        }

        let timeout = Duration::from_secs(
            timeout_secs
                .unwrap_or(DEFAULT_RUN_TIMEOUT_SECS)
                .clamp(1, MAX_RUN_TIMEOUT_SECS),
        );
        let isolation = if isolated.unwrap_or(false) {
            Isolation::Isolated
        } else {
            Isolation::Shared
        };

        // Background: persist a `running` job file, run the delegate on a detached
        // blocking task that finalizes the file on completion, and return the
        // handle now. The detached task outlives this call (it runs on the
        // blocking pool, not this turn's future) so N delegates overlap.
        if background.unwrap_or(false) {
            let started_at = now_ms();
            let job_id = jobs::new_job_id(started_at);
            jobs::write_running(&job_id, &profile, started_at, monitor.unwrap_or(false)).map_err(
                |e| ErrorData::internal_error(format!("failed to record job: {e}"), None),
            )?;

            let job_id_task = job_id.clone();
            let profile_task = profile.clone();
            tokio::task::spawn_blocking(move || {
                // Catch a panic in the detached task: the handle is dropped, so an
                // unwind would otherwise be swallowed and leave the job stuck
                // `running` until GC — the waiter would hang on its deadline. The
                // job file is always finalized, mirroring the sync contract.
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    run_delegate(DelegateOpts {
                        profile: &profile_task,
                        prompt: &prompt,
                        model: model.as_deref(),
                        cwd: cwd.as_deref(),
                        env: env.unwrap_or_default(),
                        extra_args: args.unwrap_or_default(),
                        timeout,
                        isolation,
                        depth,
                    })
                }));
                let envelope = match outcome {
                    Ok(Ok(v)) => v,
                    Ok(Err(reason)) => serde_json::json!({
                        "profile": profile_task,
                        "is_error": true,
                        "result": reason,
                    }),
                    Err(_) => serde_json::json!({
                        "profile": profile_task,
                        "is_error": true,
                        "result": "delegate task panicked",
                    }),
                };
                let _ = jobs::write_done(&job_id_task, &profile_task, started_at, envelope);
            });

            let payload = serde_json::json!({
                "job_id": job_id,
                "profile": profile,
                "started_at": started_at,
                "status": "running",
            });
            return Ok(CallToolResult::success(vec![ContentBlock::text(
                payload.to_string(),
            )]));
        }

        let target = profile.clone();
        let outcome = tokio::task::spawn_blocking(move || {
            run_delegate(DelegateOpts {
                profile: &target,
                prompt: &prompt,
                model: model.as_deref(),
                cwd: cwd.as_deref(),
                env: env.unwrap_or_default(),
                extra_args: args.unwrap_or_default(),
                timeout,
                isolation,
                depth,
            })
        })
        .await
        .map_err(|e| ErrorData::internal_error(format!("delegate task panicked: {e}"), None))?;

        let envelope = match outcome {
            Ok(v) => v,
            Err(reason) => serde_json::json!({
                "profile": profile,
                "is_error": true,
                "result": reason,
            }),
        };

        let (five_h, seven_d) = load_windows(&profile);
        let mut footer =
            render::live_footer(Some(profile.as_str()), five_h.as_ref(), seven_d.as_ref());
        if let Some(note) = throughput_note(&profile, now_epoch_secs()) {
            footer.push('\n');
            footer.push_str(&note);
        }
        let is_error = envelope
            .get("is_error")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let content = with_footer(envelope, footer);
        if is_error {
            Ok(CallToolResult::error(content))
        } else {
            Ok(CallToolResult::success(content))
        }
    }

    #[tool(
        description = "Fetch the result of a `delegate` call made with `background: true`, by \
`job_id`. `wait_secs` (0..=60, default 0) long-polls for completion. Returns the delegate \
envelope when done (same shape as a blocking `delegate`, with the live usage footer), \
`{job_id, status:\"running\", elapsed_secs, quota?}` if it hasn't finished (`quota` present only \
when the job's `delegate` call set `monitor: true`), or an error for an unknown `job_id`. Normally \
the result auto-arrives via a hook. Use this only when delegate hooks are disabled"
    )]
    async fn delegate_result(
        &self,
        Parameters(DelegateResultArgs { job_id, wait_secs }): Parameters<DelegateResultArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        if !jobs::is_safe_job_id(&job_id) {
            let payload = serde_json::json!({ "is_error": true, "result": "invalid job_id" });
            return Ok(CallToolResult::error(vec![ContentBlock::text(
                payload.to_string(),
            )]));
        }
        let wait = wait_secs.unwrap_or(0).min(MAX_RESULT_WAIT_SECS);
        let jid = job_id.clone();
        let outcome = tokio::task::spawn_blocking(move || wait_for_done(&jid, wait))
            .await
            .map_err(|e| ErrorData::internal_error(format!("wait task panicked: {e}"), None))?;

        match outcome {
            WaitOutcome::Unknown => {
                let payload = serde_json::json!({ "is_error": true, "result": format!("unknown job_id: {job_id}") });
                Ok(CallToolResult::error(vec![ContentBlock::text(
                    payload.to_string(),
                )]))
            }
            WaitOutcome::Running(record) => {
                let elapsed_secs = now_ms().saturating_sub(record.started_at) / 1000;
                let mut payload = serde_json::json!({
                    "job_id": job_id,
                    "status": "running",
                    "elapsed_secs": elapsed_secs,
                });
                // `monitor`-gated: attach the target's live usage windows so the
                // poller sees remaining headroom without a separate list_profiles.
                if record.monitor {
                    payload["quota"] = windows_json(&record.profile);
                }
                Ok(CallToolResult::success(vec![ContentBlock::text(
                    payload.to_string(),
                )]))
            }
            WaitOutcome::Done(record) => {
                // Fallback path delivered it — evict so the file doesn't linger
                // past its purpose (GC also reaps it on a TTL).
                jobs::remove(&job_id);
                let envelope = record.envelope.unwrap_or_else(|| {
                    serde_json::json!({
                        "profile": record.profile,
                        "is_error": true,
                        "result": "job finished without an envelope",
                    })
                });
                let (five_h, seven_d) = load_windows(&record.profile);
                let mut footer = render::live_footer(
                    Some(record.profile.as_str()),
                    five_h.as_ref(),
                    seven_d.as_ref(),
                );
                if let Some(note) = throughput_note(&record.profile, now_epoch_secs()) {
                    footer.push('\n');
                    footer.push_str(&note);
                }
                let is_error = envelope
                    .get("is_error")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                let content = with_footer(envelope, footer);
                if is_error {
                    Ok(CallToolResult::error(content))
                } else {
                    Ok(CallToolResult::success(content))
                }
            }
        }
    }
}

/// Env var carrying the MCP delegation depth; the child `claude` inherits
/// `depth+1` so a delegate cannot itself delegate (hard cap at 1).
const MCP_DEPTH_ENV: &str = "CLAUTH_MCP_DEPTH";

/// Poll interval mirroring `start.rs`'s `wait_for_child` cadence.
const RUN_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Ceiling on `delegate_result`'s long-poll wait (seconds).
const MAX_RESULT_WAIT_SECS: u64 = 60;
/// Poll cadence for both `delegate_result` and the `mcp-await-job` hook.
const JOB_POLL_INTERVAL: Duration = Duration::from_millis(200);
/// Self-deadline for the `mcp-await-job` hook: outlast the max delegate timeout
/// plus slack so it never gives up before a legitimately long delegate finishes.
const AWAIT_JOB_DEADLINE_SECS: u64 = MAX_RUN_TIMEOUT_SECS + 600;

/// Result of polling a background job file.
enum WaitOutcome {
    Done(jobs::JobRecord),
    /// Present but not yet finished (the wait deadline elapsed first). Carries the
    /// record so the caller can report `elapsed_secs` / monitored `quota`.
    Running(jobs::JobRecord),
    /// No such job file (never created or already evicted).
    Unknown,
}

/// Poll a job file until it reports `done` or `deadline_secs` elapses. `Unknown`
/// when the file is absent (distinct from `Running` for a present-but-incomplete
/// job). Blocking; callers wrap it in `spawn_blocking`.
fn wait_for_done(job_id: &str, deadline_secs: u64) -> WaitOutcome {
    let start = Instant::now();
    let deadline = Duration::from_secs(deadline_secs);
    loop {
        match jobs::read(job_id) {
            Some(r) if r.state == jobs::JobState::Done => return WaitOutcome::Done(r),
            Some(r) if start.elapsed() >= deadline => return WaitOutcome::Running(r),
            Some(_) => {}
            None => return WaitOutcome::Unknown,
        }
        std::thread::sleep(JOB_POLL_INTERVAL);
    }
}

/// `clauth mcp-await-job` — the body of the bundled PostToolUse `asyncRewake`
/// hook. Reads the hook payload on stdin, finds the background job's `job_id`,
/// waits for the result, prints it to stdout, and exits 2 to wake the model. A
/// sync `delegate` (no `job_id` in the payload) is a no-op (exit 0). On its own
/// deadline it exits 2 with a nudge to call `delegate_result` instead.
pub(crate) fn await_job() -> ! {
    use std::io::Read;
    let mut input = String::new();
    let _ = std::io::stdin().read_to_string(&mut input);
    let job_id = serde_json::from_str::<serde_json::Value>(&input)
        .ok()
        .as_ref()
        .and_then(extract_job_id)
        .filter(|id| jobs::is_safe_job_id(id));
    let Some(job_id) = job_id else {
        std::process::exit(0); // sync delegate or unparseable input: nothing to deliver
    };

    let start = Instant::now();
    let deadline = Duration::from_secs(AWAIT_JOB_DEADLINE_SECS);
    loop {
        match jobs::read(&job_id) {
            Some(r) if r.state == jobs::JobState::Done => {
                let envelope = r.envelope.unwrap_or_else(|| {
                    serde_json::json!({
                        "profile": r.profile,
                        "is_error": true,
                        "result": "job finished without an envelope",
                    })
                });
                println!("{envelope}");
                std::process::exit(2); // wake the model with the result
            }
            Some(_) if start.elapsed() >= deadline => {
                println!(
                    "delegate job {job_id} still running; call `delegate_result` to retrieve it"
                );
                std::process::exit(2);
            }
            Some(_) => {}
            None => std::process::exit(0), // unknown / already evicted
        }
        std::thread::sleep(JOB_POLL_INTERVAL);
    }
}

/// Extract a background job's id from a hook payload, preferring the documented
/// `tool_response` slot so a delegate prompt that happens to carry a `job_id`
/// can't shadow the real handle; fall back to a whole-payload scan only if it's
/// absent (the exact shape is not host-guaranteed).
fn extract_job_id(payload: &serde_json::Value) -> Option<String> {
    payload
        .get("tool_response")
        .and_then(find_job_id)
        .or_else(|| find_job_id(payload))
}

/// Recursively search a hook-payload JSON for a string `job_id` field. A string
/// that is itself JSON is parsed and descended (the MCP tool result nests the
/// response envelope as a JSON-encoded string), so this stays agnostic to the
/// exact `tool_response` shape, which the host does not pin down.
fn find_job_id(v: &serde_json::Value) -> Option<String> {
    match v {
        serde_json::Value::Object(map) => {
            if let Some(serde_json::Value::String(s)) = map.get("job_id") {
                return Some(s.clone());
            }
            map.values().find_map(find_job_id)
        }
        serde_json::Value::Array(arr) => arr.iter().find_map(find_job_id),
        serde_json::Value::String(s) => serde_json::from_str::<serde_json::Value>(s)
            .ok()
            .as_ref()
            .and_then(find_job_id),
        _ => None,
    }
}

/// Inputs for one delegated `delegate`. Grouped into a struct so `run_delegate`
/// avoids a too-many-arguments signature as the surface grew (cwd/env/args/
/// timeout/isolation).
struct DelegateOpts<'a> {
    profile: &'a str,
    prompt: &'a str,
    model: Option<&'a str>,
    cwd: Option<&'a str>,
    env: HashMap<String, String>,
    extra_args: Vec<String>,
    timeout: Duration,
    isolation: Isolation,
    depth: u32,
}

/// Compose a delegate's environment on `command`: drop inherited provider
/// routing + the active profile's custom env
/// ([`crate::runtime::scrub_profile_env`]), layer the caller's `env`, then
/// clauth's own keys which always win. `CLAUDE_CONFIG_DIR` and the depth guard
/// can't be overridden, and `CLAUDE_CODE_MAX_OUTPUT_TOKENS` only defaults when
/// the caller didn't set it.
fn apply_delegate_env(
    command: &mut Command,
    caller_env: &HashMap<String, String>,
    active_env_keys: &[String],
    config_dir: &std::path::Path,
    depth: u32,
) {
    crate::runtime::scrub_profile_env(command, active_env_keys);
    command.envs(caller_env);
    if !caller_env.contains_key("CLAUDE_CODE_MAX_OUTPUT_TOKENS") {
        command.env("CLAUDE_CODE_MAX_OUTPUT_TOKENS", DEFAULT_MAX_OUTPUT_TOKENS);
    }
    command
        .env("CLAUDE_CONFIG_DIR", config_dir)
        .env(MCP_DEPTH_ENV, (depth + 1).to_string());
}

/// Blocking delegate: acquire the target profile's runtime, spawn a headless
/// `claude -p` with piped stdio, enforce the timeout, and parse its JSON
/// envelope. Returns `Ok(envelope)` on a clean parse, or `Err(reason)` for a
/// timeout, non-zero exit, or unparseable output (the caller wraps it in an
/// `is_error` envelope). Records observed throughput / rate-limit hits as a side
/// effect. Never bubbles a transport-level error.
fn run_delegate(opts: DelegateOpts<'_>) -> std::result::Result<serde_json::Value, String> {
    let config = load_config().map_err(|e| format!("failed to load config: {e}"))?;
    let target = config
        .find(opts.profile)
        .ok_or_else(|| format!("profile not found: {}", opts.profile))?;
    // Mirrors `disable_profile`'s own live-session refusal from the other
    // direction: that guard stops disabling a profile mid-session, this one
    // stops opening a brand-new session on one already disabled.
    if target.is_disabled() {
        return Err(format!("profile is disabled: {}", opts.profile));
    }

    if let Some(dir) = opts.cwd
        && !std::path::Path::new(dir).is_dir()
    {
        return Err(format!("cwd does not exist or is not a directory: {dir}"));
    }

    // Strip the active profile's custom env so a delegate for `<target>` does
    // not inherit whoever is globally active (mirrors `clauth start`).
    let active_env_keys: Vec<String> = config
        .state
        .active_profile
        .as_deref()
        .and_then(|n| config.find(n))
        .map(|p| p.env.keys().cloned().collect())
        .unwrap_or_default();

    // Guard kept alive across spawn+wait; dropped on return for RAII teardown.
    let runtime = ProfileRuntime::acquire(target, opts.isolation, &active_env_keys)
        .map_err(|e| format!("failed to acquire runtime: {e}"))?;

    let mut command = crate::runtime::claude_command();
    apply_delegate_env(
        &mut command,
        &opts.env,
        &active_env_keys,
        runtime.config_dir(),
        opts.depth,
    );
    command
        .args(["-p", opts.prompt, "--output-format", "json"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::null());
    // Isolated only: suppress operator/project MCP servers for a clean blind
    // session (mirrors `start.rs`). A shared delegate inherits its config-dir's
    // MCP servers so it can do research/nav. Recursion stays capped either way:
    // the `CLAUTH_MCP_DEPTH` guard refuses a nested `delegate` even when the child
    // loads clauth's own server. Callers can still pass `--mcp-config` (and
    // `--strict-mcp-config`) via `args` to scope a shared delegate.
    if opts.isolation == Isolation::Isolated {
        command.arg("--strict-mcp-config");
    }
    if let Some(m) = opts.model {
        command.args(["--model", m]);
    }
    if let Some(dir) = opts.cwd {
        command.current_dir(dir);
    }
    // Resolve the cwd the spawned `claude` will actually run in: the caller's
    // override, else this process's own cwd (inherited like `start.rs`). If
    // it's the real `$HOME`, guard against the project-settings leak.
    let effective_cwd = match opts.cwd {
        Some(dir) => Some(std::path::PathBuf::from(dir)),
        None => std::env::current_dir().ok(),
    };
    if let Some(dir) = effective_cwd.as_deref() {
        crate::runtime::guard_home_project_settings(&mut command, dir);
    }
    command.args(&opts.extra_args);

    let mut child = command
        .spawn()
        .map_err(|e| format!("failed to spawn claude: {e}"))?;

    // Drain both pipes on their own threads from the moment of spawn. A bare
    // try_wait loop never reads, so a >~64KiB result blocks the child on a full
    // pipe and it never exits — a false timeout that drops a valid result. Killing
    // the child closes the write ends, the readers hit EOF, and the joins return.
    let stdout_reader = child
        .stdout
        .take()
        .map(|mut h| std::thread::spawn(move || drain_pipe(&mut h)));
    let stderr_reader = child
        .stderr
        .take()
        .map(|mut h| std::thread::spawn(move || drain_pipe(&mut h)));

    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if start.elapsed() >= opts.timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!(
                        "delegate timed out after {}s",
                        opts.timeout.as_secs()
                    ));
                }
                std::thread::sleep(RUN_POLL_INTERVAL);
            }
            Err(e) => return Err(format!("failed to wait for claude: {e}")),
        }
    };

    let stdout_bytes = join_reader(stdout_reader);
    let stderr_bytes = join_reader(stderr_reader);
    let stdout = String::from_utf8_lossy(&stdout_bytes);
    let now = now_epoch_secs();
    if !status.success() {
        let stderr = String::from_utf8_lossy(&stderr_bytes);
        // A non-zero exit can be a throttle; record it so `which`/`list_profiles`
        // can flag the model as rate-limited (clauth never sees inference 429s
        // any other way).
        if let Some(retry_after) = rate_limit_hint(&format!("{stderr}{stdout}")) {
            crate::throughput::record_rate_limit(opts.profile, opts.model, retry_after, now);
        }
        return Err(format!(
            "claude exited with {}: {}",
            status
                .code()
                .map_or_else(|| "signal".to_string(), |c| c.to_string()),
            truncate(stderr.trim(), 2000)
        ));
    }
    let envelope = parse_delegate_envelope(stdout.trim())?;
    // A clean exit can still carry an in-band error envelope (rate limit shows up
    // there with `--output-format json`); branch on `is_error` so a throttle is
    // recorded as one, not as a (bogus) throughput sample.
    if envelope
        .get("is_error")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
    {
        if let Some(retry_after) = rate_limit_hint(&envelope.to_string()) {
            crate::throughput::record_rate_limit(opts.profile, opts.model, retry_after, now);
        }
    } else {
        record_throughput_from_envelope(opts.profile, opts.model, &envelope, now);
    }
    Ok(envelope)
}

/// Reduce `claude`'s captured stdout to its single terminal `type:"result"`
/// envelope. Plain `--output-format json` already emits exactly that object, but
/// a caller-supplied `--verbose` flips the format to the full transcript ARRAY
/// (every `system` thinking-token / tool-io / `assistant` event) and `stream-json`
/// emits the same events as NDJSON — either is valid input that would otherwise be
/// stored and dumped into the caller's context verbatim (a multi-minute run leaks
/// ~1000x the envelope). Collapse all three to the terminal result object so the
/// delegate envelope stays the documented shape regardless of caller `args`.
fn parse_delegate_envelope(stdout: &str) -> std::result::Result<serde_json::Value, String> {
    match serde_json::from_str::<serde_json::Value>(stdout) {
        Ok(serde_json::Value::Array(items)) => result_event(items).ok_or_else(|| {
            format!(
                "no result event in claude output: {}",
                truncate(stdout, 2000)
            )
        }),
        Ok(other) => Ok(other),
        // NDJSON (`stream-json`): not a single JSON value — recover the terminal
        // result event from the per-line events.
        Err(e) => {
            let items = stdout
                .lines()
                .filter_map(|l| serde_json::from_str::<serde_json::Value>(l.trim()).ok())
                .collect();
            result_event(items).ok_or_else(|| {
                format!(
                    "failed to parse claude output: {e}: {}",
                    truncate(stdout, 2000)
                )
            })
        }
    }
}

/// The last `type:"result"` element of a parsed claude event list (its terminal
/// envelope), falling back to the last element when none is tagged. `None` for an
/// empty list.
fn result_event(mut items: Vec<serde_json::Value>) -> Option<serde_json::Value> {
    match items
        .iter()
        .rposition(|v| v.get("type").and_then(serde_json::Value::as_str) == Some("result"))
    {
        Some(i) => Some(items.swap_remove(i)),
        None => items.pop(),
    }
}

/// Pull output-token throughput from a successful `claude` JSON envelope and
/// record it. Best-effort: a missing usage/duration block records nothing.
fn record_throughput_from_envelope(
    profile: &str,
    model: Option<&str>,
    envelope: &serde_json::Value,
    now: i64,
) {
    let output_tokens = envelope
        .get("usage")
        .and_then(|u| u.get("output_tokens"))
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    let duration_ms = envelope
        .get("duration_api_ms")
        .or_else(|| envelope.get("duration_ms"))
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    crate::throughput::record_success(profile, model, output_tokens, duration_ms, now);
}

/// Detect a rate-limit / 429 signature in a delegate's output. `Some(retry)`
/// when it looks rate-limited (inner `None` = no Retry-After hint found),
/// `None` when it doesn't.
fn rate_limit_hint(text: &str) -> Option<Option<u64>> {
    let lower = text.to_lowercase();
    let limited = lower.contains("rate limit")
        || lower.contains("rate_limit")
        || lower.contains("429")
        || lower.contains("overloaded");
    if !limited {
        return None;
    }
    let retry_after = lower.find("retry").and_then(|i| {
        lower[i..]
            .split(|c: char| !c.is_ascii_digit())
            .find(|s| !s.is_empty())
            .and_then(|s| s.parse::<u64>().ok())
    });
    Some(retry_after)
}

/// One-line throughput warning for the live footer, or `None` when nothing is
/// degraded or rate-limited.
fn throughput_note(profile: &str, now: i64) -> Option<String> {
    let flagged: Vec<String> = crate::throughput::summary(profile, now)
        .into_iter()
        .filter(|m| m.degraded || m.rate_limited_recent)
        .map(|m| {
            if m.rate_limited_recent {
                match m.retry_after_s {
                    Some(s) => format!("{} rate-limited (retry ~{s}s)", m.model),
                    None => format!("{} rate-limited", m.model),
                }
            } else {
                format!("{} slow (~{:.0} tok/s)", m.model, m.tok_s)
            }
        })
        .collect();
    (!flagged.is_empty()).then(|| format!("⚠ throughput: {}", flagged.join(", ")))
}

/// Read a child pipe to EOF into a buffer, swallowing read errors (a partial
/// buffer is more useful than a hard failure for an error envelope).
fn drain_pipe<R: std::io::Read>(reader: &mut R) -> Vec<u8> {
    let mut buf = Vec::new();
    let _ = reader.read_to_end(&mut buf);
    buf
}

/// Join a reader thread, returning its drained bytes (empty on a join panic or
/// an absent pipe).
fn join_reader(handle: Option<std::thread::JoinHandle<Vec<u8>>>) -> Vec<u8> {
    handle.and_then(|h| h.join().ok()).unwrap_or_default()
}

/// Truncate a string to `max` bytes (on a char boundary) for an error payload,
/// appending an ellipsis when clipped.
fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

#[tool_handler]
impl ServerHandler for ClauthServer {
    fn get_info(&self) -> ServerInfo {
        // ServerInfo is #[non_exhaustive]; build from default then set fields.
        // Tools capability must be advertised explicitly: ServerInfo::default() leaves
        // capabilities empty, so a spec-compliant client (Claude Code) exposes no tools
        // at all even though the server can answer tools/list.
        let mut info = ServerInfo::default();
        info.capabilities = ServerCapabilities::builder().enable_tools().build();
        info.instructions = Some(build_instructions());
        info
    }
}

/// Build the init-time `instructions` block once from the on-demand config and
/// usage disk cache. Best-effort: a config load failure degrades to a prose-only
/// block rather than failing the handshake.
fn build_instructions() -> String {
    let Ok(config) = load_config() else {
        return "clauth manages multiple Claude Code accounts (\"profiles\"). \
            Call `list_profiles` for live usage figures."
            .to_string();
    };
    let snapshots: Vec<ProfileSnapshot> = config
        .profiles
        .iter()
        .map(|p| {
            let name = p.name.as_str();
            ProfileSnapshot {
                name: name.to_string(),
                active: config.is_active(name),
                provider: provider_label(p),
                base_url: p.base_url.clone(),
                sub_type: tier_label(p),
            }
        })
        .collect();

    render::instructions_block(&snapshots, &crate::which::session_auth())
}

pub(crate) fn serve() -> Result<()> {
    // Housekeeping OFF the startup path (timeout-sweep 2026-07-18): the
    // initialize reply must come promptly — the TUI's MCP probe
    // (`plugin_probe::mcp_boots`) budgets 3 s, while `gc_stale_runtimes` can
    // legitimately run long (it deletes crashed sessions' ISOLATED runtime
    // trees — multi-GB `~/.claude` copies). Running gc synchronously here made
    // the probe kill a healthy server and report MCP as broken. Both sweeps
    // are lock-guarded and documented safe at any entry point, so a detached
    // thread alongside the server is fine.
    std::thread::spawn(|| {
        crate::runtime::gc_stale_runtimes();
        jobs::gc(now_ms());
    });
    // rmcp's service loop arms a Tokio timer (needs `enable_time`), so a bare
    // current-thread runtime panics right after the initialize reply. `enable_all`
    // also turns on the I/O driver, covering a future transport that polls a real
    // fd or any added tokio net/process path.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    rt.block_on(run_server())
}

async fn run_server() -> Result<()> {
    use rmcp::{ServiceExt, transport::stdio};
    let service = ClauthServer::new().serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}

#[cfg(test)]
#[path = "../../tests/inline/mcp_run.rs"]
mod tests;

#[cfg(test)]
#[path = "../../tests/inline/mcp_switch_tool.rs"]
mod switch_tool_tests;
