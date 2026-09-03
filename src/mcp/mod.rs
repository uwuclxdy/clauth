//! `clauth mcp` — MCP JSON-RPC 2.0 server over stdio (rmcp).
//!
//! Exposes clauth profiles to a live Claude Code session: list/usage, switch,
//! and delegate. The rest of the binary stays synchronous; [`serve`] builds a
//! scoped current-thread tokio runtime and blocks on the stdio server.
//!
//! All logging MUST go to stderr — stdout carries the JSON-RPC frame.

mod digest;
mod herdr_report;
/// Reachable outside this module because the TUI's delegates pane reads the same
/// store through the same parser: two readers of one store is a drift risk, and
/// a second parser would be the drift itself. The pane calls [`jobs::list`] and
/// [`jobs::running_liveness`] and writes nothing.
pub(crate) mod jobs;
mod render;

use std::collections::HashMap;
use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::Result;
use rmcp::{
    ErrorData, RoleServer, ServerHandler,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{
        CacheScope, CallToolResult, ContentBlock, DiscoverResult, Implementation, ListToolsResult,
        PaginatedRequestParams, ProtocolVersion, ServerCapabilities, ServerInfo,
    },
    schemars,
    service::RequestContext,
    tool, tool_handler, tool_router,
};
use serde::Deserialize;

use crate::logline::logline;
use crate::out::outln;
use crate::profile::{AppConfig, Profile, ProfileName, load_config};
use crate::profile_cache::{THIRD_PARTY_CACHE_FILE, USAGE_CACHE_FILE, load_profile_cache};
use crate::profile_json::{
    ProfileWindows, oauth_windows, profile_windows, profile_windows_for, provider_label, tier_label,
};
use crate::providers::ThirdPartyStats;
use crate::runtime::{Isolation, ProfileRuntime};
use crate::usage::{UsageInfo, UsageWindow, now_epoch_secs, now_ms};
use digest::{DigestMode, DigestTracker, WatchOutcome, WatchSet};
use render::{ProfileSnapshot, RosterRank};

/// Marks the `clauth mcp` child that [`crate::plugin_probe::mcp_boots`] spawns
/// for the Plugin tab's handshake check. clauth owns both sides of that spawn, so
/// an env marker beats inferring it from the client identity in a request.
pub(crate) const MCP_PROBE_ENV: &str = "CLAUTH_MCP_PROBE";

/// Hard ceiling (seconds) on either caller-supplied delegate deadline, and on
/// one `monitor` wait ([`MAX_WAIT_SECS`]). A wall clock cannot see whether the
/// child is producing anything, so a streaming run is given none at all and this
/// bounds only the two places a caller names a number.
const MAX_RUN_TIMEOUT_SECS: u64 = 3600;
/// Default idle deadline (seconds): kill only once the delegate has emitted
/// NOTHING for this long. Every streamed event resets it, so a working delegate
/// runs for as long as it keeps talking — this is a streaming run's ONLY
/// deadline. It must stay above the longest single blocking tool call a delegate
/// makes (a release build), since no event arrives while one runs.
const DEFAULT_IDLE_SECS: u64 = 300;
/// Cap on the salvaged assistant text carried back by a killed delegate. The
/// tail is kept: it is the part closest to a usable answer.
const PARTIAL_TEXT_CAP: usize = 8 * 1024;
/// Cap on the tail a RUNNING check carries. Far under [`PARTIAL_TEXT_CAP`]
/// because this rides a reply a model may fetch repeatedly, where the 8 KiB
/// salvage rides one terminal envelope.
const TAIL_CAP: usize = 400;
/// Throttle shared by the background heartbeat and `monitor`'s progress
/// notifications. Each heartbeat is an atomic tmp+rename (create, write,
/// rename) and token deltas arrive at tens per second, so 2 s bounds the store
/// to 0.5 writes/second/job — an order of magnitude inside a reader's own
/// cadence. Claude Code renders progress on a 700 ms throttle, so nothing
/// faster would be visible anyway.
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(2);
/// Raise the delegate's max output budget above CC's default so a long headless
/// build doesn't die on the 32k cap. Overridable via the `env` arg.
const DEFAULT_MAX_OUTPUT_TOKENS: &str = "64000";
/// Cap on one `prompt_file` in bytes. Well under Linux's ~128 KiB single-argument
/// ceiling (the prompt becomes one `-p` argv element), so a file that passes can
/// always be handed to `claude`, and far above any real reusable prompt.
const PROMPT_FILE_CAP: u64 = 64 * 1024;
/// Cap on one `profiles` fan-out. Each target is a real usage window with no
/// undo, so a runaway list is bounded here.
const MAX_FANOUT: usize = 8;

/// A throughput cache key that names a model, or `None` for the non-name the
/// store writes when the delegate named none: `default` is
/// [`crate::throughput`]'s placeholder key, and an empty or whitespace-only
/// key is the same non-name. Every surface shares this one rule, so a
/// placeholder can never render as a model name anywhere.
fn model_display_name(model: &str) -> Option<&str> {
    let name = model.trim();
    (!name.is_empty() && name != "default").then_some(name)
}

/// One roster throughput row, shared by [`throughput_warnings`] so every
/// surface describes a model the same way. The `model` field is absent when
/// the store held the placeholder non-name ([`model_display_name`]); how a
/// nameless row then renders is `render::throughput_prose`'s rule.
fn throughput_row(m: crate::throughput::ModelSummary) -> serde_json::Value {
    let mut row = serde_json::Map::new();
    if let Some(model) = model_display_name(&m.model) {
        row.insert("model".into(), model.into());
    }
    row.insert("tok_s".into(), ((m.tok_s * 10.0).round() / 10.0).into());
    row.insert("samples".into(), m.samples.into());
    row.insert("degraded".into(), m.degraded.into());
    row.insert("rate_limited_recent".into(), m.rate_limited_recent.into());
    row.insert("retry_after_s".into(), m.retry_after_s.into());
    serde_json::Value::Object(row)
}

/// The subset of the per-model summary a roster is worth spending tokens on:
/// only models a past `delegate` found degraded or recently rate-limited. A
/// healthy row tells a picker nothing it would act on, and one operator's 19
/// healthy rows measured 31% of the whole `profiles` response.
fn throughput_warnings(profile: &ProfileName, now: i64) -> Vec<serde_json::Value> {
    crate::throughput::summary(profile, now)
        .into_iter()
        .filter(|m| m.degraded || m.rate_limited_recent)
        .map(throughput_row)
        .collect()
}

/// Fresh-from-cache 5h/7d windows for a profile. Each call re-reads the disk
/// cache (no caching across tool calls per the design). The roster's own rank
/// reads this: it asks for the two figures it sorts on, and consults the
/// third-party cache itself for an account that has no such window.
fn load_windows(name: &ProfileName) -> (Option<UsageWindow>, Option<UsageWindow>) {
    match load_profile_cache::<UsageInfo>(name, USAGE_CACHE_FILE) {
        Some(u) => (u.five_hour, u.seven_day),
        None => (None, None),
    }
}

/// The discriminated headroom payload every MCP surface renders through
/// [`render::windows_prose`]: which cache answered and the figures it holds.
/// ONE carrier per account — an OAuth account's windows, or the balance/bars a
/// third-party account publishes in place of a window it does not have — so no
/// reply can print one account's figure twice or date it in a second place.
///
/// `provider_windows` carries the one thing the rendered `balance` string cannot
/// be asked for: whether this account's PROVIDER publishes usage windows of its
/// own, or answers with a scalar (a wallet, a counter). A windows-publishing
/// provider keeps the flag true even when one cached response carried no bars,
/// because the denial is a claim about the provider, not the response; a
/// generic endpoint falls back to whether its response carried bars. It rides
/// the payload because the prose layer never sees the stats — recovering it by
/// matching the rendered text would tie the copy to a substring of itself.
fn windows_payload(windows: &ProfileWindows) -> serde_json::Value {
    match windows {
        // An empty array is a missing FIGURE, the one case the prose reads as
        // `unknown`.
        ProfileWindows::Oauth { usage, .. } => serde_json::json!({
            "kind": "oauth",
            "windows": usage.as_deref().map(oauth_windows).unwrap_or_default(),
        }),
        ProfileWindows::ThirdParty {
            stats, provider, ..
        } => serde_json::json!({
            "kind": "third_party",
            "balance": stats.as_ref().map(render::third_party_headline),
            "provider_windows": provider.is_some_and(|p| p.publishes_windows())
                || stats.as_ref().is_some_and(|s| !s.bars.is_empty()),
        }),
    }
}

/// [`windows_payload`] plus the age of the cache its figures came from, for a
/// reply carrying no other freshness cue — a running check's `quota`, a folded
/// live-usage clause — on a server that refreshes no cache of its own. Each
/// field is omitted when it carries no news, so an absent `stale` means false.
fn dated_windows_payload(windows: &ProfileWindows) -> serde_json::Value {
    let mut payload = windows_payload(windows);
    if let Some(age) = windows.age_secs() {
        payload["fetched_secs_ago"] = serde_json::json!(age);
    }
    if windows.stale() {
        payload["stale"] = serde_json::json!(true);
    }
    payload
}

/// [`windows_payload`] for a ROSTER row: undated, because 27 rows read one cache
/// generation and this is the reply whose own description asks the model to call
/// it before every delegate. `stale` still rides — a stale row costs a wrong
/// routing decision rather than a slow one — and because the figure it dates
/// lives in this same object, it renders beside that figure rather than beside a
/// structural none.
fn row_windows_payload(windows: &ProfileWindows) -> serde_json::Value {
    let mut payload = windows_payload(windows);
    if windows.stale() {
        payload["stale"] = serde_json::json!(true);
    }
    payload
}

/// The headroom payload for a running check's `quota`: whichever cache the
/// target's own fetch leg writes, so a third-party target answers with its own
/// figures instead of the `usage unknown` an OAuth-only read can only ever
/// produce for it.
fn quota_payload(name: &ProfileName) -> serde_json::Value {
    dated_windows_payload(&profile_windows_for(name))
}

/// One roster row for `p`, the shape both `profiles` scopes render through
/// `profile_line`. The one builder keeps the all-scope roster and the
/// session-scope row from disagreeing about what a profile is called.
fn profile_row(p: &Profile, config: &AppConfig, now: i64) -> serde_json::Value {
    let name = &p.name;
    // One read of this account's own cache, feeding the one carrier its figures
    // ride in. Reading it twice (once to date the row, once for the headline)
    // cost a second parse of the same file on every row of the roster.
    let mut row = serde_json::json!({
        "name": name,
        "active": config.is_active(name),
        "provider": provider_label(p),
        "tier": tier_label(p),
        "windows": row_windows_payload(&profile_windows(p)),
    });
    // Host, not the full endpoint: the host is the identifying half, and the path
    // costs tokens on every row without adding to it. For which hosts then read
    // as local, see `render::host_locality`.
    //
    // Both endpoint halves, not the managed field alone: an account routing
    // through an operator-authored `[env] ANTHROPIC_BASE_URL` has no managed
    // `base_url`; a row reading only the managed field renders it as an
    // Anthropic account, which is exactly the read the cost model must not
    // make. `Profile::routing_endpoint` names the producer's precedence.
    if let Some(url) = p.routing_endpoint() {
        row["host"] = serde_json::json!(render::base_url_host(url));
    }
    // Both of these are absent unless they say something. Emitted
    // unconditionally they were 39% of a 27-profile response, nearly all of it
    // `false` and rows carrying no warning.
    if crate::runtime::has_live_session(name) {
        row["has_live_session"] = serde_json::json!(true);
    }
    let warnings = throughput_warnings(name, now);
    if !warnings.is_empty() {
        row["throughput"] = serde_json::Value::Array(warnings);
    }
    // A third-party profile with no inference auth source is a delegate target
    // that refuses at the spawn gate, so this flags it before the picker spends
    // the call. The predicate is the delegate guard's own, not the usage
    // predicate — see `preflight_target`.
    if p.is_third_party() && !crate::claude::has_inference_auth(p) {
        row["keyless"] = serde_json::json!(true);
    }
    // Also marked rather than filtered: a silently missing row reads exactly
    // like "that profile is gone", which the unknown-`names` refusal already
    // rejects.
    if p.is_disabled() {
        row["disabled"] = serde_json::json!(true);
    }
    // NOT a delegate refusal on an account that serves its own inference,
    // which delegates off its api key while the flag describes its usage chain
    // (`preflight_target`, owner ruling 2026-08-30). It stays on the row
    // because the picker is choosing where to spend and a dead chain means
    // this account's usage figures are stale.
    if config.is_auth_broken(name) {
        row["auth_broken"] = serde_json::json!(true);
    }
    // Informational, not a refusal: clauth has no cancel gate, and a canceled
    // account still delegates on whatever the org's post-cancellation plan
    // allows. It rides here because the picker is choosing where to spend.
    // `is_canceled_cached` is the one cancellation predicate every surface
    // asks; re-deriving it off this row's own cache read would save a parse
    // and fork the answer, which is the trade the `list` table already made
    // the other way.
    if crate::profile_json::is_canceled_cached(name) {
        row["canceled"] = serde_json::json!(true);
    }
    row
}

/// The roster's sort key for one profile. A real window first (5h, the pool a
/// `delegate` actually competes for, then 7d), then a third-party provider's own
/// cached bars, then the first funded wallet off its cached rows — zero-amount
/// wallets drop, the same selection the overview balance column makes.
fn roster_rank(name: &ProfileName) -> RosterRank {
    let (five_h, seven_d) = load_windows(name);
    if let Some(w) = five_h.or(seven_d) {
        return RosterRank::Window(100.0 - w.utilization);
    }
    let Some(stats) = load_profile_cache::<ThirdPartyStats>(name, THIRD_PARTY_CACHE_FILE) else {
        return RosterRank::Unknown;
    };
    if let Some(bar) = stats
        .bars
        .iter()
        .find(|b| b.label == "5h")
        .or_else(|| stats.bars.iter().find(|b| b.label == "7d"))
    {
        return RosterRank::Window(100.0 - bar.pct);
    }
    crate::providers::funded_wallets(&stats.rows)
        .into_iter()
        .next()
        .map_or(RosterRank::Unknown, |w| RosterRank::Balance {
            currency: w.currency,
            amount: w.amount,
        })
}

/// A `delegate` argument/validation refusal: one `{is_error, result}` envelope
/// in one content block. Prose reads as a sentence; the payload keeps the same
/// keys as every other delegate refusal.
fn delegate_refusal(reason: &str) -> CallToolResult {
    let payload = serde_json::json!({ "is_error": true, "result": reason });
    let prose = render::delegate_refusal_prose(&payload);
    CallToolResult::error(single_block(prose))
}

/// The one builder for every `profile not found` refusal: the caller's spelling
/// leads, then the fix clause. Placement rule 4's corollary makes the refusal
/// carry the whole lesson, and the clause is a closed set composed HERE so the
/// call sites cannot split the server's refusal vocabulary.
fn profile_not_found(names: &str, fix: ProfileNotFoundFix) -> String {
    let clause = match fix {
        ProfileNotFoundFix::CallProfiles => "call `profiles` for valid names",
        ProfileNotFoundFix::OmitFilter => "omit `names` for every account",
    };
    format!("profile not found: {names}; {clause}")
}

/// The fix clause a [`profile_not_found`] refusal ends with. Closed, so the
/// vocabulary lives in the builder: a call site composing its own clause is the
/// split the builder exists to close.
enum ProfileNotFoundFix {
    /// The caller is outside the roster: the `profiles` tool is the source of
    /// valid names.
    CallProfiles,
    /// The caller is INSIDE the `profiles` tool, filtering it: dropping the
    /// filter shows every account.
    OmitFilter,
}

/// The live-usage footer folded into a payload as data: which profile the
/// figures describe, and that profile's own headroom — an OAuth account's 5h/7d
/// share (null when uncached), or the balance a third-party account publishes in
/// place of a window it does not have — dated off the cache it was read from.
/// The throughput warning is added by the caller when the tool has one.
///
/// `windows` is `None` only when there is no profile to report on, which the
/// prose renders as a `none` rather than as a lost figure.
fn live_usage_json(profile: Option<&str>, windows: Option<&ProfileWindows>) -> serde_json::Value {
    let Some(windows) = windows else {
        return serde_json::json!({ "profile": profile });
    };
    let mut payload = dated_windows_payload(windows);
    payload["profile"] = serde_json::json!(profile);
    // The two shares an OAuth reader acts on directly, beside the window array
    // they were read from: this clause is a footer rather than a table, and 5h/7d
    // are the pools every other such figure in clauth refers to.
    if let ProfileWindows::Oauth { usage, .. } = windows {
        let usage = usage.as_deref();
        payload["5h_used_pct"] = serde_json::json!(
            usage
                .and_then(|u| u.five_hour.as_ref())
                .map(|w| w.utilization)
        );
        payload["7d_used_pct"] = serde_json::json!(
            usage
                .and_then(|u| u.seven_day.as_ref())
                .map(|w| w.utilization)
        );
    }
    payload
}

/// Collapse one reply to exactly one content block. The JSON payload is
/// internal — every renderer in `render.rs` reads it — and prose is the only
/// spelling a caller sees.
fn single_block(prose: String) -> Vec<ContentBlock> {
    vec![ContentBlock::text(prose)]
}

/// Fold the active profile's live usage into a payload, replacing the old
/// second-block footer. A non-object payload is wrapped under `result` first,
/// the same shape `fold_delegate_live_usage` uses: `serde_json`'s string-key
/// `IndexMut` auto-vivifies only `Null` and panics on every other non-object,
/// and the caller's payload must survive the fold.
///
/// The same fold is where the since-your-last-call digest belongs: beside
/// `live_usage`, under `since_your_last_call`, present only when something
/// moved since the last reply that reported one. `digest` decides whether this
/// reply reports or reseeds — see `digest::DigestMode`.
fn fold_active_live_usage(
    payload: serde_json::Value,
    config: &AppConfig,
    digest: DigestMode<'_>,
) -> serde_json::Value {
    let mut map = match payload {
        serde_json::Value::Object(map) => map,
        other => {
            let mut map = serde_json::Map::new();
            map.insert("result".to_string(), other);
            map
        }
    };
    let active = config.state.active_profile.as_ref();
    let windows = active.map(profile_windows_for);
    map.insert(
        "live_usage".to_string(),
        live_usage_json(active.map(|n| n.as_str()), windows.as_ref()),
    );
    if let Some(delta) = digest.folded() {
        map.insert("since_your_last_call".to_string(), delta);
    }
    serde_json::Value::Object(map)
}

/// The profile half of a delegate call's endpoint, name-keyed: the target
/// profile's stored endpoint in the roster's own host spelling, `anthropic`
/// only for an account routing through neither an `[env] ANTHROPIC_BASE_URL`
/// nor an effective managed `base_url`. `None` when clauth cannot read that
/// account's config, which the renderer treats as "cannot say" rather than as
/// Anthropic.
///
/// The question is "where did this request go", so it reads
/// [`crate::profile::stored_endpoint`] (both profile sources, env first) and
/// not `is_third_party` (which answers "is the provider one clauth has a
/// typed integration for"), not `usage_cache_is_third_party` (which answers
/// "which cache holds this account's figures"), and not `Profile::is_oauth`
/// (which reads the managed field alone). All four disagree somewhere, and
/// only this one bounds what `total_cost_usd` may claim.
///
/// [`delegate_call_endpoint`] layers the caller's own `env` override on top;
/// a caller wanting one answer for the whole call starts there. Name-keyed
/// rather than threaded from a resolved `Profile`, because `load_profile`
/// recovers a staged rotation under the cross-process state flock
/// (`rank::State`, 500), a serialization point no fold path should sit
/// inside.
fn target_endpoint(name: &ProfileName) -> Option<String> {
    match crate::profile::stored_endpoint(name) {
        crate::profile::StoredEndpoint::Anthropic => Some("anthropic".to_string()),
        crate::profile::StoredEndpoint::Custom(url) => {
            Some(render::base_url_host(&url).to_string())
        }
        crate::profile::StoredEndpoint::Unknown => None,
    }
}

/// Which endpoint one delegate CALL routes its child to: the caller's own
/// `env` entry first, then the target profile's stored endpoint. Resolved at
/// call time so the answer can travel with the job: the caller authored the
/// override in the same call; after the call ends nothing else can see it.
///
/// The caller half mirrors the producer's precedence: `apply_delegate_env`
/// scrubs the inherited profile env and layers `caller_env` over it, so an
/// explicit `ANTHROPIC_BASE_URL` there is what the spawned `claude` reads.
/// An entry that is blank once trimmed is no override, the same test
/// [`crate::profile::stored_endpoint`] applies to the profile half.
fn delegate_call_endpoint(target: &str, caller_env: &HashMap<String, String>) -> Option<String> {
    if let Some(url) = caller_env
        .get("ANTHROPIC_BASE_URL")
        .map(|v| v.trim())
        .filter(|v| !v.is_empty())
    {
        return Some(render::base_url_host(url).to_string());
    }
    target_endpoint(&ProfileName::from(target))
}

/// The serving-provider label for one endpoint url. Anthropic's own origin
/// reads `anthropic`. A recognised third-party origin reads that provider's
/// display name. Anything else reads `generic`.
fn serving_provider_label(url: &str) -> String {
    if crate::providers::url_matches_host(url, crate::usage::ANTHROPIC_ORIGIN) {
        return "anthropic".to_string();
    }
    crate::providers::Provider::from_base_url(url)
        .map(|p| p.display_name().to_string())
        .unwrap_or_else(|| "generic".to_string())
}

/// Which provider served one delegate CALL. The caller's own `env` entry wins,
/// then the target profile's stored endpoint. That is the same precedence
/// [`delegate_call_endpoint`] applies, because both answers describe the same
/// request. The label is read off the FULL url, not the host the sibling
/// stores: `Provider`'s origin matching needs the scheme, and the bare host
/// `target_endpoint` returns carries none.
///
/// The question is "who served this request", so it reads the call's own
/// resolution and not [`crate::profile_json::provider_label`] (the owner-ruled
/// label with the same three-word vocabulary, answering how the account is
/// TYPED off the managed field alone — which is why the reply spends a key of
/// its own on this one, `live_usage.served_by`), and not
/// [`crate::profile::stored_provider`] (the managed field's typed provider).
/// Both type the ACCOUNT, so an account an operator retargets through
/// `[env] ANTHROPIC_BASE_URL` answers `anthropic` there and the endpoint's
/// provider here.
///
/// `None` is "cannot say": the profile half resolved `Unknown`. The caller
/// wanting one answer for the whole call resolves it here at call time, so it
/// can ride the record the same way `endpoint` does.
fn delegate_call_provider(target: &str, caller_env: &HashMap<String, String>) -> Option<String> {
    if let Some(url) = caller_env
        .get("ANTHROPIC_BASE_URL")
        .map(|v| v.trim())
        .filter(|v| !v.is_empty())
    {
        return Some(serving_provider_label(url));
    }
    match crate::profile::stored_endpoint(&ProfileName::from(target)) {
        crate::profile::StoredEndpoint::Anthropic => Some("anthropic".to_string()),
        crate::profile::StoredEndpoint::Custom(url) => Some(serving_provider_label(&url)),
        crate::profile::StoredEndpoint::Unknown => None,
    }
}

/// Fold the target profile's live usage into a delegate envelope (the sync
/// `delegate` and `monitor` done-handoff paths share this). The
/// envelope is whatever `claude` printed, so it may be ANY json shape:
/// `parse_delegate_envelope` returns non-objects verbatim. A non-object is
/// wrapped under `result` (the documented self-report key) first — `serde_json`'s
/// string-key `IndexMut` auto-vivifies only `Null` and panics on every other
/// non-object, and the delegate's own output must survive the fold either way.
///
/// `endpoint` is the CALL's answer, resolved by the caller: the blocking and
/// background handler arms resolve it at call time
/// ([`delegate_call_endpoint`]), the collect and hook paths take it off the
/// job record. `None` is "cannot say"; the endpoint key then stays absent
/// rather than falling back to a name-keyed read of a profile the call may
/// never have routed through.
///
/// `served_by` is the same CALL's serving-provider label, resolved and carried
/// exactly the same way ([`delegate_call_provider`] at call time, the record
/// on the collect and hook paths). Both ride the call because a caller `env`
/// override retargets one run without touching the profile, and a name-keyed
/// read would assert the account's answer for a call that routed elsewhere.
///
/// It publishes under its own key rather than `provider`, which every other
/// reply in this server spends on how an ACCOUNT is typed
/// ([`crate::profile_json::provider_label`]). The two answer different
/// questions out of one three-word vocabulary, so one account can hold both
/// words at once — a profile whose managed `base_url` names Anthropic's own
/// origin is typed `generic` (`is_oauth` reads that field alone) and served by
/// `anthropic` (owner ruling 2026-09-03).
fn fold_delegate_live_usage(
    payload: serde_json::Value,
    profile: &ProfileName,
    endpoint: Option<String>,
    served_by: Option<String>,
    now: i64,
    digest: DigestMode<'_>,
) -> serde_json::Value {
    let mut map = match payload {
        serde_json::Value::Object(map) => map,
        other => {
            let mut map = serde_json::Map::new();
            map.insert("result".to_string(), other);
            map
        }
    };
    let windows = profile_windows_for(profile);
    let mut live = live_usage_json(Some(profile), Some(&windows));
    if let Some(endpoint) = endpoint {
        live["endpoint"] = serde_json::Value::String(endpoint);
    }
    if let Some(served_by) = served_by {
        live["served_by"] = serde_json::Value::String(served_by);
    }
    if let Some(note) = throughput_note(profile, now) {
        live["throughput_warning"] = serde_json::Value::String(note);
    }
    map.insert("live_usage".to_string(), live);
    if let Some(delta) = digest.folded() {
        map.insert("since_your_last_call".to_string(), delta);
    }
    serde_json::Value::Object(map)
}

#[derive(Clone)]
pub(crate) struct ClauthServer {
    tool_router: ToolRouter<Self>,
    /// `Some` only when the serve path resolved a herdr pane: `delegate` then
    /// reports `working`/`idle` as a pane metadata token. A server built
    /// without it is a silent no-op.
    herdr_pane: Option<herdr_report::PaneReporter>,
    /// The since-your-last-call baseline every clone shares (rmcp clones the
    /// handler per request; a per-clone baseline would report nothing
    /// forever). See `digest`.
    digest: DigestTracker,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub(crate) struct SwitchArgs {
    /// Account to re-link the global credentials to.
    name: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub(crate) struct ProfilesArgs {
    /// Filter the reply to these accounts. Omit it or pass an empty list, to
    /// leave `scope` as the only filter.
    names: Option<Vec<String>>,
    /// `scope: "all"` (default): all accounts.
    ///
    /// `scope: "session"`: the account THIS session is running on, with
    /// `source` saying how that resolved. This can change throughout the
    /// session.
    scope: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub(crate) struct DelegateArgs {
    /// Which account(s) to use. One or multiple; one delegate per account, all
    /// run in parallel with the same prompt.
    profiles: Option<Vec<String>>,
    /// The task for the delegate in plain text. Works the same way as prompting
    /// Claude Code; mention a file with `@path/file` to pull it directly into
    /// delegate's context, `/skill` to invoke a skill, and so on.
    /// This is the only thing it receives from you.
    ///
    /// To run the delegate as one of your `Agent` types, make
    /// `@"{type} (agent)"` the start of `prompt`. Needs `isolated: false`.
    ///
    /// Spell the type exactly as the `Agent` tool lists it. An unknown type is
    /// dropped with no error.
    prompt: Option<String>,
    /// Passes a txt/md file as the prompt (path relative to `cwd`). Use it for a
    /// prompt you reuse across turns, or one that changes only slightly between
    /// delegates.
    prompt_file: Option<String>,
    /// `isolated: false` (default): the delegate loads your `CLAUDE.md`, plugins,
    /// hooks, skills, MCP servers and tools the same as a normal session or a
    /// native agent. Use this for real work.
    ///
    /// `isolated: true`: none of that loads, only its `prompt` steers it. Use
    /// this to test what a stock `claude` does.
    isolated: Option<bool>,
    /// `background: false` (default): the call waits for the delegate to finish
    /// and returns its result. With multiple `profiles`, it waits for all of
    /// them before returning the results.
    ///
    /// `background: true`: the call returns a `{job_id}` and the delegate keeps
    /// running. Its result is delivered to you automatically when it finishes.
    /// You can check, collect or stop it with `monitor`.
    background: Option<bool>,
    /// Model for the delegated session.
    ///
    /// Unset (default): the profile's own default model.
    model: Option<String>,
    /// Directory the delegate runs in (must exist). The delegate reads
    /// `CLAUDE.md` from this directory unconditionally.
    ///
    /// Unset (default): where this session started.
    cwd: Option<String>,
    /// Continue a session by `session_id`, with `prompt` as the next message,
    /// in the session's original working directory. `cwd` is optional.
    /// `delegate` refuses to resume if it differs from the original directory.
    ///
    /// Without `profiles`, the delegate runs on the account this session last
    /// ran on (from the conversation record); name `profiles` to spend a
    /// different one.
    resume: Option<String>,
    /// Kill the delegate if it produces no output at all for this many seconds
    /// (max: 3600, default 300). It returns any text it had and a `session_id`
    /// you can pass to `resume`. This is the only time limit on a normal run.
    ///
    /// A delegate that keeps producing output runs to completion, no matter how
    /// long it takes. Raise it only when the task is expected to make a slow
    /// tool call (e.g. a long build).
    ///
    /// If `args` pins its own `--output-format`, this limit is off;
    /// `timeout_secs` is the deadline that applies instead.
    idle_secs: Option<u64>,
    /// Wall-clock limit in seconds (max: 3600). Applies only when `args` pins
    /// its own `--output-format`; leave it unset there for `idle_secs` to
    /// supply the limit instead. Ignored on any other run (a delegate that is
    /// still producing output keeps running).
    timeout_secs: Option<u64>,
    /// Additional environment variables passed to the delegate session. Values
    /// you set for `CLAUDE_CONFIG_DIR` and `CLAUTH_MCP_DEPTH` are replaced by
    /// clauth's own. Read `code.claude.com/docs/en/env-vars.md` to see what
    /// Claude Code supports.
    env: Option<HashMap<String, String>>,
    /// Extra CLI arguments that go after the `claude -p` clauth invokes. Your
    /// arguments come last, so they win where a flag repeats (including
    /// `--model` when `model` is set). Pinning `--output-format` here replaces
    /// clauth's own output shape, which switches the deadline to `timeout_secs`.
    ///
    /// A delegate that must write files needs
    /// `args: ["--dangerously-skip-permissions"]`; without it the session
    /// starts in a mode that refuses edits and it spends the run hunting for a
    /// writable path.
    args: Option<Vec<String>>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub(crate) struct MonitorArgs {
    /// Job ids to work with.
    job_ids: Option<Vec<String>>,
    /// Seconds to poll for job result before returning (max: 3600, default 0 =
    /// instant).
    ///
    /// Clamped to 1500 on a client that cannot receive progress notifications.
    ///
    /// With `job_ids` it bounds the wait for a job to finish; with none it
    /// bounds the wait on clauth's own state.
    wait_secs: Option<u64>,
    /// `return_on: "any"` (default): return as soon as one of the named jobs
    /// finishes.
    ///
    /// `return_on: "all"`: wait for the slowest. The default under
    /// `cancel: true`.
    return_on: Option<String>,
    /// `cancel: true`: ask the named jobs to stop. Keeps whatever they produced
    /// and tells how far each one got.
    ///
    /// `cancel: false` (default): the call checks and collects. It never stops
    /// a running job.
    cancel: Option<bool>,
}

/// Which lane ends a several-ids wait.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReturnOn {
    /// The first job to finish. An orchestrator polls a fan-out to react to
    /// whichever lane lands first, and waiting for the slowest makes every
    /// reply as slow as it.
    Any,
    /// Every job, which is what a caller collecting an already-finished set
    /// wants.
    All,
}

#[tool_router]
impl ClauthServer {
    pub(crate) fn new() -> Self {
        Self {
            tool_router: Self::tool_router(),
            herdr_pane: None,
            digest: DigestTracker::new(),
        }
    }

    /// Attach the pane reporter the serve path resolved at startup. Kept off
    /// `new()` so an in-process test, which builds its server directly, never
    /// inherits an ambient `HERDR_PANE_ID` and reports at the operator's live
    /// herdr socket. `tests/mcp_handshake.rs` does reach this path: it spawns
    /// the real binary, so it clears the herdr env on the child itself.
    pub(crate) fn with_herdr_pane(
        mut self,
        herdr_pane: Option<herdr_report::PaneReporter>,
    ) -> Self {
        self.herdr_pane = herdr_pane;
        self
    }

    #[tool(
        description = "List of clauth accounts with their cached usage headrooms. A window's \
percentage is how much of it is already used. Call it before picking a `delegate` target. A row \
can carry `disabled`, `login expired`, `no api key` or `subscription canceled`; when `delegate` \
refuses an account, its refusal names the state and the fix. `subscription canceled` never means \
a refusal, and `login expired` does not mean one on an account that has its own `host` and api \
key, which delegates on that key."
    )]
    async fn profiles(
        &self,
        Parameters(ProfilesArgs { names, scope }): Parameters<ProfilesArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let config = load_config().map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
        if let Some(raw) = scope.as_deref()
            && !matches!(raw, "all" | "session")
        {
            // An unrecognised scope is refused by name so a typo cannot
            // silently answer the wrong question.
            let payload = serde_json::json!({
                "ok": false,
                "reason": format!(
                    "unrecognized scope \"{raw}\": accepted \"all\" and \"session\""
                ),
            });
            let prose = render::profiles_prose(&payload);
            return Ok(CallToolResult::error(single_block(prose)));
        }
        if scope.as_deref() == Some("session") {
            // Cross-mode refusal, the same boundary rule as `monitor`'s
            // job/state seam: the session scope answers one account and cannot
            // be narrowed further, so a `names` list is a mistake worth naming
            // rather than silently ignoring — the all-scope arm would have
            // refused an unknown member by name.
            if let Some(names) = names.as_deref()
                && !names.is_empty()
            {
                let payload = serde_json::json!({
                    "ok": false,
                    "reason": "`names` cannot combine with `scope: \"session\"`: the session \
                               scope answers the one account this session runs on; drop `names`",
                });
                let prose = render::profiles_prose(&payload);
                return Ok(CallToolResult::error(single_block(prose)));
            }
            return self.profiles_session(&config);
        }
        let now = now_epoch_secs();

        // Resolve the filter before rendering anything. A name matching nothing
        // is a caller mistake, and silently dropping it would answer with a
        // roster that reads exactly like "that profile is gone".
        let wanted = match names.as_deref() {
            None | Some([]) => None,
            Some(raw) => {
                let (found, unknown): (Vec<_>, Vec<_>) = raw
                    .iter()
                    .map(|n| config.canonical_name(n).ok_or_else(|| n.clone()))
                    .partition(Result::is_ok);
                if !unknown.is_empty() {
                    let missing: Vec<String> =
                        unknown.into_iter().map(Result::unwrap_err).collect();
                    let payload = serde_json::json!({
                        "ok": false,
                        "reason": profile_not_found(
                            &missing.join(", "),
                            ProfileNotFoundFix::OmitFilter
                        ),
                    });
                    let prose = render::profiles_prose(&payload);
                    return Ok(CallToolResult::error(single_block(prose)));
                }
                Some(
                    found
                        .into_iter()
                        .map(Result::unwrap)
                        .collect::<Vec<String>>(),
                )
            }
        };

        let profiles: Vec<serde_json::Value> = config
            .profiles
            .iter()
            .filter(|p| {
                wanted
                    .as_ref()
                    .is_none_or(|w| w.iter().any(|n| n == p.name.as_str()))
            })
            .map(|p| profile_row(p, &config, now))
            .collect();

        let payload = serde_json::json!({ "profiles": profiles });
        let prose = render::profiles_prose(&payload);
        Ok(CallToolResult::success(single_block(prose)))
    }

    /// The `scope: "session"` arm: the one row the account THIS session runs on
    /// resolves to, through the same `which::resolve_active` tiers the session
    /// itself resolves by, plus `source`. Rendered through `profile_line` so the
    /// row carries the roster's own guards (the anthropic tier guard included).
    fn profiles_session(&self, config: &AppConfig) -> Result<CallToolResult, ErrorData> {
        let resolved = crate::which::resolve_active(config);
        let mut rows = Vec::with_capacity(1);
        if let Some((name, source)) = resolved.as_ref()
            && let Some(p) = config.find(&ProfileName::from(name.clone()))
        {
            let mut row = profile_row(p, config, now_epoch_secs());
            row["source"] = serde_json::json!(source.as_str());
            rows.push(row);
        }
        let payload = fold_active_live_usage(
            serde_json::json!({ "scope": "session", "profiles": rows }),
            config,
            DigestMode::Report(&self.digest),
        );
        let mut prose = render::profiles_prose(&payload);
        // Session facts ride this reply through the same renderers the
        // instructions block uses (placement rule 3: one renderer, two
        // carriers), so a client that drops the block still sees them.
        let auth = crate::which::session_auth();
        prose.push_str("\n\n");
        prose.push_str(&render::switch_effect_note(&auth));
        let probe = crate::runtime::link_mode_of(crate::which::session_config_dir().as_deref());
        if let Some(note) = render::runtime_paths_note(&auth, probe) {
            prose.push_str("\n\n");
            prose.push_str(&note);
        }
        Ok(CallToolResult::success(single_block(prose)))
    }

    #[tool(
        description = "Relink the global `~/.claude` credentials to another account. Whether THIS \
session follows depends on how it reads credentials: the reply says which case it is in, and \
`profiles({scope:\"session\"})` says so before you commit. To use another account without \
disturbing this session, use `delegate`."
    )]
    async fn switch_profile(
        &self,
        Parameters(SwitchArgs { name }): Parameters<SwitchArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let config = load_config().map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
        // The reply's session-effect note, resolved once: `session_auth` reads
        // the env this server was launched with, which no arm below can move.
        let session_note = render::switch_effect_note(&crate::which::session_auth());

        // Resolve the raw tool argument to a stored profile (case-insensitive)
        // BEFORE any mutation, so the refusal keeps the uniform
        // `profile_not_found` envelope. The authoritative gate — and the
        // half-switched hazard a late refusal guards against — is
        // `actions::ensure_switch_target_ok`.
        let Some(name) = config.canonical_name(&name) else {
            let payload = serde_json::json!({
                "ok": false,
                "reason": profile_not_found(&name, ProfileNotFoundFix::CallProfiles)
            });
            // Refused before any mutation ran, so nothing of ours moved:
            // report like the session-scope roster does (`DigestMode::Report`).
            let payload =
                fold_active_live_usage(payload, &config, DigestMode::Report(&self.digest));
            let mut prose = render::switch_profile_prose(&payload);
            prose.push_str("\n\n");
            prose.push_str(&session_note);
            return Ok(CallToolResult::error(single_block(prose)));
        };
        let on_divergence = config.state.default_divergence;

        // It can block — a `security` subprocess against its deadline, its
        // AUTH-1 refresh over HTTP — so keep it off the async worker so
        // neither stalls the runtime. Mirrors `delegate`'s `spawn_blocking`.
        // The blocking contract and the shared-handle wrap the refresh path
        // needs are `actions::switch_profile_noninteractive`'s; the deadline
        // constant is `keychain.rs`'s `SECURITY_TIMEOUT`.
        let (config, outcome) = tokio::task::spawn_blocking(move || {
            let config = std::sync::Arc::new(crate::lockorder::RankedMutex::new(config));
            let outcome = crate::actions::switch_profile_noninteractive(
                &config,
                &ProfileName::from(name.clone()),
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
                // The mutation ran: reseed silently (`DigestMode::Reseed`).
                let payload = fold_active_live_usage(
                    serde_json::json!({
                        "ok": true,
                        "previous": previous,
                        "active": active,
                    }),
                    &config,
                    DigestMode::Reseed(&self.digest),
                );
                let mut prose = render::switch_profile_prose(&payload);
                prose.push_str("\n\n");
                prose.push_str(&session_note);
                Ok(CallToolResult::success(single_block(prose)))
            }
            Err(e) => {
                // Failed AFTER the mutation ran, so it may have written on the
                // way out: same reseed (`DigestMode::Reseed`).
                let payload = fold_active_live_usage(
                    serde_json::json!({ "ok": false, "reason": e.to_string() }),
                    &config,
                    DigestMode::Reseed(&self.digest),
                );
                let mut prose = render::switch_profile_prose(&payload);
                prose.push_str("\n\n");
                prose.push_str(&session_note);
                Ok(CallToolResult::error(single_block(prose)))
            }
        }
    }

    #[tool(
        description = "Run a task on another clauth account. `delegate` starts a fresh `claude` \
session on that account and returns its final response. This is like the Agent tool, but the \
agent runs on a different account's login.\n\n\
The delegate knows nothing about this conversation. Put everything it needs into `prompt`.\n\n\
Delegating spends the target account, so pick the account with `profiles` first."
    )]
    async fn delegate(
        &self,
        Parameters(args): Parameters<DelegateArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        self.delegate_with(args, ProgressSink::from_context(&ctx))
            .await
    }

    /// The whole of `delegate`, minus the peer — the split `monitor_with` made,
    /// for the same two reasons: an in-process caller cannot construct a
    /// `Peer<RoleServer>`, and [`ProgressSink::none`] is exactly what a peer
    /// that sent no `progressToken` gets, so the inner entry is a real path.
    async fn delegate_with(
        &self,
        args: DelegateArgs,
        mut progress: ProgressSink,
    ) -> Result<CallToolResult, ErrorData> {
        let DelegateArgs {
            profiles,
            prompt,
            prompt_file,
            model,
            cwd,
            env,
            args,
            timeout_secs,
            idle_secs,
            resume,
            isolated,
            background,
        } = args;
        // Fail closed: a present-but-unparseable value is treated as max depth
        // (refuse), so a corrupt env can never re-enable delegation. Only a truly
        // absent var is depth 0.
        let depth: u32 = match std::env::var(MCP_DEPTH_ENV) {
            Ok(v) => v.trim().parse().unwrap_or(u32::MAX),
            Err(_) => 0,
        };
        if depth >= 1 {
            // The refusal fires before target validation, but the caller's own
            // spelling is known here: name the targets it asked for. `profiles`
            // is an optional key, present only when the caller named one.
            let payload = match &profiles {
                Some(names) => serde_json::json!({
                    "profiles": names,
                    "is_error": true,
                    "result": "delegation depth exceeded (max 1)",
                }),
                None => serde_json::json!({
                    "is_error": true,
                    "result": "delegation depth exceeded (max 1)",
                }),
            };
            let prose = render::delegate_refusal_prose(&payload);
            return Ok(CallToolResult::error(single_block(prose)));
        }

        // Exactly one prompt source. A prompt read from a file still costs the
        // target account once, but no longer costs the CALLING model its own
        // context to pass the same long prompt inline.
        if prompt.is_some() == prompt_file.is_some() {
            let reason = if prompt.is_some() {
                "exactly one of `prompt` or `prompt_file` must be given; both were"
            } else {
                "exactly one of `prompt` or `prompt_file` must be given; neither was"
            };
            return Ok(delegate_refusal(reason));
        }

        let config = load_config().map_err(|e| ErrorData::internal_error(e.to_string(), None))?;

        // Which accounts to spend, in canonical spelling, resolved BEFORE any
        // spawn or read. One name is one target (blocking unless `background`);
        // two or more fan out, one delegate per account, blocking unless
        // `background` is set.
        enum Target {
            One(String),
            Many(Vec<String>),
        }
        let raw: Vec<String> = profiles.unwrap_or_default();
        let target = if raw.len() == 1 {
            let Some(name) = config.canonical_name(&raw[0]) else {
                return Ok(delegate_refusal(&profile_not_found(
                    &raw[0],
                    ProfileNotFoundFix::CallProfiles,
                )));
            };
            Target::One(name)
        } else if raw.is_empty() {
            // With no name given, a `resume` can still name one — see
            // `hook_note::told_account` — and a resume is exactly "keep
            // spending where this session ran". The inferred name takes the
            // same `Target::One` path an explicit one does, so
            // canonicalization and preflight stay shared. No `resume`, or
            // none attributable, refuses — with the fix named, since the
            // reader is a model that can run it.
            match resume.as_deref() {
                Some(id) => match crate::hook_note::told_account(id) {
                    Some(name) => {
                        let Some(name) = config.canonical_name(&name) else {
                            return Ok(delegate_refusal(&profile_not_found(
                                &name,
                                ProfileNotFoundFix::CallProfiles,
                            )));
                        };
                        Target::One(name)
                    }
                    None => {
                        // The id is echoed into the refusal and used nowhere
                        // else, and this arm fires exactly for ids the record
                        // check refused — unbounded length included. Bound the
                        // echo the way every other error payload is bounded.
                        return Ok(delegate_refusal(&format!(
                            "can't tell which account session '{}' ran on; pass `profiles` \
                             naming the account to spend",
                            truncate(id, 64)
                        )));
                    }
                },
                None => {
                    return Ok(delegate_refusal(
                        "`profiles` is empty: name at least one profile",
                    ));
                }
            }
        } else {
            match resolve_fanout(&config, &raw) {
                Ok(names) => Target::Many(names),
                Err(reason) => return Ok(delegate_refusal(&reason)),
            }
        };

        // Resolve the prompt text once, before any spawn, so a fan-out reuses one
        // read across every account.
        let prompt: std::sync::Arc<str> = match prompt_file.as_deref() {
            Some(rel) => match read_prompt_file(cwd.as_deref(), rel) {
                Ok(text) => text.into(),
                Err(reason) => return Ok(delegate_refusal(&reason)),
            },
            None => prompt.as_deref().unwrap_or_default().to_string().into(),
        };

        // Both deadlines resolve inside `run_delegate`: whether there is a wall
        // clock at all depends on whether the child ends up streaming, which
        // only the composed arg list knows.
        let isolation = if isolated.unwrap_or(false) {
            Isolation::Isolated
        } else {
            Isolation::Shared
        };

        if background.unwrap_or(false) {
            match target {
                Target::One(name) => {
                    // Refuse a target `delegate` must not spend on BEFORE the
                    // job file is reserved: the caller gets the refusal
                    // synchronously, never a running job whose collected result
                    // carries it. The blocking path runs the same three gates
                    // inside `run_delegate`; `resolve_fanout` runs them per
                    // fan-out member.
                    let name_pn = ProfileName::from(name.clone());
                    let target = config.find(&name_pn).ok_or_else(|| {
                        ErrorData::internal_error(
                            "resolved target missing from config".to_string(),
                            None,
                        )
                    })?;
                    if let Err(reason) = preflight_target(target, &config, &name_pn) {
                        return Ok(delegate_refusal(&reason));
                    }
                    let extra_args = args.unwrap_or_default();
                    let streaming = !sets_output_format(&extra_args);
                    let opts = BackgroundOpts {
                        prompt,
                        model,
                        cwd,
                        env: env.unwrap_or_default(),
                        extra_args,
                        timeout_secs,
                        idle_secs,
                        resume,
                        isolation,
                        depth,
                    };
                    // Resolved once, at call time, so the handle and the job
                    // record agree with each other and with where the child
                    // actually routes. A later profile edit changes none of
                    // the three.
                    let endpoint = delegate_call_endpoint(&name, &opts.env);
                    let provider = delegate_call_provider(&name, &opts.env);
                    let reserved = reserve_background_job(
                        &name,
                        timeout_secs,
                        idle_secs,
                        streaming,
                        endpoint.clone(),
                        provider.clone(),
                        isolation,
                    )
                    .map_err(|e| ErrorData::internal_error(e, None))?;
                    let job_id = reserved.spec.job_id.clone();
                    let started_at = reserved.spec.started_at;
                    // Commits to launch: the job file is reserved and the task
                    // spawns next. `begin` marks one delegate in flight; the
                    // matching `idle` is `herdr_report::InFlightGuard`'s.
                    if let Some(pane) = &self.herdr_pane {
                        pane.begin();
                    }
                    launch_background_delegate(
                        name.clone(),
                        opts,
                        reserved,
                        self.herdr_pane.clone(),
                    );
                    // The handle carries the same footer the blocking reply
                    // does: a caller that takes the job id instead of the
                    // output must still hear what it just spent.
                    let payload = fold_delegate_live_usage(
                        serde_json::json!({
                            "job_id": job_id,
                            "profile": name,
                            "started_at": started_at,
                            "status": "running",
                        }),
                        &ProfileName::from(name.clone()),
                        endpoint,
                        provider,
                        now_epoch_secs(),
                        DigestMode::Report(&self.digest),
                    );
                    let prose = render::delegate_prose(&payload);
                    return Ok(CallToolResult::success(single_block(prose)));
                }
                Target::Many(names) => {
                    let extra_args = args.unwrap_or_default();
                    let streaming = !sets_output_format(&extra_args);
                    let opts = BackgroundOpts {
                        prompt,
                        model,
                        cwd,
                        env: env.unwrap_or_default(),
                        extra_args,
                        timeout_secs,
                        idle_secs,
                        resume,
                        isolation,
                        depth,
                    };
                    // Reserve every job file BEFORE the first spawn: the reserve
                    // is the only fallible step left here (ENOSPC / perms on the
                    // jobs dir; the target pre-flight already ran in
                    // `resolve_fanout`), so a failure spends no window and loses
                    // no job id. The ids already reserved exist nowhere else;
                    // drop them and keep the all-or-nothing contract.
                    let mut reserved = Vec::with_capacity(names.len());
                    for name in &names {
                        match reserve_background_job(
                            name,
                            timeout_secs,
                            idle_secs,
                            streaming,
                            delegate_call_endpoint(name, &opts.env),
                            delegate_call_provider(name, &opts.env),
                            isolation,
                        ) {
                            Ok(job) => reserved.push(job),
                            Err(reason) => {
                                for job in reserved {
                                    job.abandon();
                                }
                                return Ok(delegate_refusal(&reason));
                            }
                        }
                    }
                    let now = now_epoch_secs();
                    let mut jobs = Vec::with_capacity(names.len());
                    for (name, job) in names.iter().zip(reserved) {
                        if let Some(pane) = &self.herdr_pane {
                            pane.begin();
                        }
                        let job_id = job.spec.job_id.clone();
                        let started_at = job.spec.started_at;
                        // The reservation's own answer, so a row agrees with
                        // the record it names rather than re-resolving and
                        // hoping nothing moved.
                        let endpoint = job.spec.endpoint.clone();
                        let provider = job.spec.provider.clone();
                        launch_background_delegate(
                            name.clone(),
                            opts.clone(),
                            job,
                            self.herdr_pane.clone(),
                        );
                        // Each row carries its OWN target's headroom: the
                        // caller just spent one window per account and decides
                        // per account. `Skip`, never `Report` — a per-row
                        // report would consume the delta N times; see
                        // `DigestMode`.
                        jobs.push(fold_delegate_live_usage(
                            serde_json::json!({
                                "job_id": job_id,
                                "profile": name,
                                "started_at": started_at,
                                "status": "running",
                            }),
                            &ProfileName::from(name.clone()),
                            endpoint,
                            provider,
                            now,
                            DigestMode::Skip,
                        ));
                    }
                    let mut payload = serde_json::json!({ "jobs": jobs });
                    // One digest for the whole call, top-level beside `jobs`,
                    // exactly where the batch collect path carries its own.
                    if let Some(delta) = DigestMode::Report(&self.digest).folded() {
                        payload["since_your_last_call"] = delta;
                    }
                    let prose = render::delegate_fanout_prose(&payload);
                    return Ok(CallToolResult::success(single_block(prose)));
                }
            }
        }

        // Blocking delegates: one name runs one account, several run every
        // account at once and wait for all of them.
        let target = match target {
            Target::One(target) => target,
            Target::Many(names) => {
                let extra_args = args.unwrap_or_default();
                let streaming = !sets_output_format(&extra_args);
                let opts = BackgroundOpts {
                    prompt,
                    model,
                    cwd,
                    env: env.unwrap_or_default(),
                    extra_args,
                    timeout_secs,
                    idle_secs,
                    resume,
                    isolation,
                    depth,
                };
                // A Handoff and a spawn per member, joined as one; no job file
                // is minted on the happy path. `opts` clones per member so the
                // one prompt read serves every account.
                let mut handles = Vec::with_capacity(names.len());
                let mut handoffs = Vec::with_capacity(names.len());
                let mut starts = Vec::with_capacity(names.len());
                for name in &names {
                    if let Some(pane) = &self.herdr_pane {
                        pane.begin();
                    }
                    let started_at = now_ms();
                    starts.push(started_at);
                    let handoff = Handoff::blocking(MintSpec {
                        profile: name.clone(),
                        started_at,
                        timeout_secs,
                        idle_secs,
                        streaming,
                        endpoint: delegate_call_endpoint(name, &opts.env),
                        provider: delegate_call_provider(name, &opts.env),
                        isolation,
                    });
                    handles.push(spawn_delegate(
                        name.clone(),
                        opts.clone(),
                        std::sync::Arc::clone(&handoff),
                        self.herdr_pane.clone(),
                    ));
                    handoffs.push(handoff);
                }
                let ct = progress.cancel_token();
                let label = names.join("`, `");
                let joined = join_all_ticking(
                    handles,
                    &ct,
                    || hand_off_members(&names, &handoffs, &starts),
                    async move |elapsed: Duration| {
                        progress
                            .tick(|| format!("delegate on `{label}` · {}s", elapsed.as_secs()))
                            .await;
                    },
                )
                .await;

                // The reply's freshness is about when it is written, so the
                // epoch second is read after every run lands, matching the
                // single-delegate path.
                let now = now_epoch_secs();
                match joined {
                    JoinedAll::Ran { values, abandoned } => {
                        let rows = fold_fanout_rows(&names, &opts.env, values, now);
                        let is_error = fanout_is_error(&rows);
                        let mut payload = serde_json::json!({ "results": rows });
                        // One digest for the whole call, top-level beside
                        // `results`, exactly where the batch collect path
                        // carries its own.
                        if let Some(delta) = delegate_digest_mode(&self.digest, abandoned).folded()
                        {
                            payload["since_your_last_call"] = delta;
                        }
                        let prose = render::delegate_fanout_results_prose(&payload);
                        if is_error {
                            return Ok(CallToolResult::error(single_block(prose)));
                        }
                        return Ok(CallToolResult::success(single_block(prose)));
                    }
                    JoinedAll::HandedOff(members) => {
                        // The caller abandoned the fan-out while at least one
                        // child was still spending; every member that handed off
                        // keeps running as a background job. This reply is never
                        // sent (see `jobs`), so it is built for shape and
                        // `Skip` (`DigestMode`) keeps the digest from consuming
                        // a delta into a reply that does not exist.
                        let names_and_ids = members
                            .iter()
                            .map(|m| format!("`{}` as job `{}`", m.profile, m.job_id))
                            .collect::<Vec<_>>()
                            .join(", ");
                        logline!("clauth: abandoned delegate fan-out continues: {names_and_ids}");
                        let jobs = members
                            .into_iter()
                            .map(|m| {
                                let HandedOffMember {
                                    job_id,
                                    profile,
                                    started_at,
                                } = m;
                                fold_delegate_live_usage(
                                    serde_json::json!({
                                        "job_id": job_id,
                                        "profile": &profile,
                                        "started_at": started_at,
                                        "status": "running",
                                    }),
                                    &ProfileName::from(profile.clone()),
                                    delegate_call_endpoint(&profile, &opts.env),
                                    delegate_call_provider(&profile, &opts.env),
                                    now,
                                    DigestMode::Skip,
                                )
                            })
                            .collect::<Vec<_>>();
                        let payload = serde_json::json!({ "jobs": jobs });
                        return Ok(CallToolResult::success(single_block(
                            render::delegate_fanout_prose(&payload),
                        )));
                    }
                }
            }
        };
        let extra_args = args.unwrap_or_default();
        // Resolved out here as well as inside the run, because an abandoned call
        // mints this run's job file from the OUTSIDE and `resolve_deadlines`
        // forks on it.
        let streaming = !sets_output_format(&extra_args);
        let opts = BackgroundOpts {
            prompt,
            model,
            cwd,
            env: env.unwrap_or_default(),
            extra_args,
            timeout_secs,
            idle_secs,
            resume,
            isolation,
            depth,
        };
        // Resolved once, at call time, so the blocking reply and a job file
        // minted by a hand-off carry the same answer. A caller `env` override
        // retargets this one run without touching the profile.
        let endpoint = delegate_call_endpoint(&target, &opts.env);
        let provider = delegate_call_provider(&target, &opts.env);
        let started_at = now_ms();
        // A blocking run owns no job file yet. It gets one the moment its caller
        // walks away from a child that is already spending.
        let handoff = Handoff::blocking(MintSpec {
            profile: target.clone(),
            started_at,
            timeout_secs,
            idle_secs,
            streaming,
            endpoint: endpoint.clone(),
            provider: provider.clone(),
            isolation,
        });
        // Commits to spawn: from here the delegate is in flight. `begin` marks
        // one in flight; the matching `idle` is `herdr_report::InFlightGuard`'s.
        if let Some(pane) = &self.herdr_pane {
            pane.begin();
        }
        let handle = spawn_delegate(
            target.clone(),
            opts,
            std::sync::Arc::clone(&handoff),
            self.herdr_pane.clone(),
        );
        let ct = progress.cancel_token();
        let label = target.clone();
        let joined = join_ticking(
            handle,
            &ct,
            || handoff.hand_off(),
            async move |elapsed: Duration| {
                progress
                    .tick(|| format!("delegate on `{label}` · {}s", elapsed.as_secs()))
                    .await;
            },
        )
        .await
        .map_err(|e| ErrorData::internal_error(format!("delegate task panicked: {e}"), None))?;

        let (envelope, abandoned) = match joined {
            Joined::Ran { value, abandoned } => (value, abandoned),
            // The caller abandoned the call while the child was still spending,
            // so the run went on as a background job instead of throwing its
            // result away.
            //
            // This reply is never sent: rmcp (3.2.0, `service.rs`) removes the
            // request from `local_ct_pool` when the `notifications/cancelled`
            // arrives, and the response path drops any message whose id is no
            // longer in that pool — "dropping response for cancelled request" —
            // before it reaches the transport. So it is built for shape rather
            // than for a reader: the background path's own handle payload, the
            // honest answer if this ever does reach one, and `Skip`
            // (`DigestMode`) on the digest, which would otherwise consume a
            // delta into a reply that does not exist. The id reaches an
            // operator through the log line.
            Joined::HandedOff(job_id) => {
                logline!("clauth: abandoned delegate on `{target}` continues as job `{job_id}`");
                let payload = serde_json::json!({
                    "job_id": job_id,
                    "profile": target,
                    "started_at": started_at,
                    "status": "running",
                });
                return Ok(CallToolResult::success(single_block(
                    render::delegate_prose(&payload),
                )));
            }
        };

        let payload = fold_delegate_live_usage(
            envelope,
            &ProfileName::from(target.clone()),
            endpoint,
            provider,
            now_epoch_secs(),
            delegate_digest_mode(&self.digest, abandoned),
        );
        let is_error = payload
            .get("is_error")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let prose = render::delegate_prose(&payload);
        if is_error {
            Ok(CallToolResult::error(single_block(prose)))
        } else {
            Ok(CallToolResult::success(single_block(prose)))
        }
    }

    #[tool(
        description = "Check, collect, or stop a background `delegate` by providing `job_ids`. \
With no `job_ids` it blocks until clauth's own state moves: the active account, that account's \
usage cache, or the credentials on disk.\n\n\
With `job_ids`: a running job reports its account, elapsed time, how long before deadline kills \
it, and its latest output. A finished job also hands back its result.\n\n\
Without `job_ids`: lists at most 10 delegates clauth holds, live runs first. An interrupted \
blocking `delegate` (its caller walked away mid-run) keeps running as a background job, and that \
listing is where you find its id."
    )]
    async fn monitor(
        &self,
        Parameters(args): Parameters<MonitorArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        self.monitor_with(args, ProgressSink::from_context(&ctx))
            .await
    }

    /// The whole of `monitor`, minus the peer. Split out because an in-process
    /// caller cannot construct a `Peer<RoleServer>` — that is every test call
    /// site — and because [`ProgressSink::none`] is also exactly what a peer
    /// that sent no `progressToken` gets, so the split is a real path rather
    /// than a test-only one.
    async fn monitor_with(
        &self,
        args: MonitorArgs,
        mut progress: ProgressSink,
    ) -> Result<CallToolResult, ErrorData> {
        let MonitorArgs {
            job_ids,
            wait_secs,
            return_on,
            cancel,
        } = args;
        // Cross-mode and bad-value refusals, by name and before any waiting: a
        // rule the server refuses by name is one the description does not have
        // to teach (placement rule 4). Same shape as the `profiles` handler's
        // `scope` refusal.
        let refuse = |reason: &str| {
            let payload = serde_json::json!({ "is_error": true, "result": reason });
            Ok(CallToolResult::error(single_block(
                render::monitor_job_prose(&payload),
            )))
        };
        let cancel = cancel == Some(true);
        if cancel && job_ids.is_none() {
            return refuse(
                "`cancel` cannot combine with the state-waiting mode: it orders a set of jobs, \
                 so name `job_ids` or drop it",
            );
        }
        let return_on = match resolve_return_on(return_on.as_deref(), job_ids.is_some(), cancel) {
            Ok(chosen) => chosen,
            Err(reason) => return refuse(&reason),
        };
        // Structural validation, THEN the destructive op. A list this refuses
        // must not have stopped anything on its way to being refused, and a
        // cancel has no undo.
        if let Some(reason) = job_ids.as_deref().and_then(job_ids_refusal) {
            return refuse(&reason);
        }
        let wait = effective_wait(wait_secs, progress.can_receive_progress(), cancel);
        // Asked BEFORE the wait, so the runs are already stopping while it runs
        // and this reply carries whatever they reached.
        let mut watch = cancel.then(|| CancelWatch::ask(job_ids.as_deref().unwrap_or_default()));
        let reply = match job_ids {
            // One id keeps the single-job reply shape; several collect as a
            // batch. Both arms take a list `job_ids_refusal` already cleared.
            Some(ids) if ids.len() == 1 => {
                monitor_one(
                    ids.into_iter().next().unwrap_or_default(),
                    wait,
                    &self.digest,
                    &mut progress,
                    watch.as_mut(),
                )
                .await
            }
            Some(ids) => {
                monitor_batch(
                    ids,
                    wait,
                    return_on,
                    &self.digest,
                    &mut progress,
                    watch.as_mut(),
                )
                .await
            }
            // No ids: the state-waiting mode absorbed from the old `watch`
            // tool — the same digest, all three observables (see `WatchSet`)
            // — plus the listing, which is the one thing `job_ids` cannot ask
            // for because asking needs an id you do not have.
            None => {
                let outcome = self.digest.watch(WatchSet::ALL, wait, &mut progress).await;
                let mut payload = match outcome {
                    WatchOutcome::Armed => serde_json::json!({ "status": "armed" }),
                    WatchOutcome::Unchanged { waited_secs } => {
                        serde_json::json!({ "status": "unchanged", "waited_secs": waited_secs })
                    }
                    WatchOutcome::Changed(delta) => serde_json::json!({
                        "status": "changed",
                        "since_your_last_call": delta.to_json(),
                    }),
                };
                // Resolved AFTER the wait, so a caller that blocked for ten
                // minutes is told what the store holds now rather than what it
                // held when the call arrived.
                fold_jobs_listing(&mut payload, now_ms());
                let prose = render::monitor_state_prose(&payload);
                Ok(CallToolResult::success(single_block(prose)))
            }
        };
        // Composed AFTER the wait: the verdicts say what it observed, per job,
        // in the seconds this call actually waited for each.
        let note = watch.map(CancelWatch::note).filter(|note| !note.is_empty());
        match note {
            Some(note) => reply.map(|r| prepend_note(r, &note)),
            None => reply,
        }
    }
}

/// Env var carrying the MCP delegation depth; the child `claude` inherits
/// `depth+1` so a delegate cannot itself delegate (hard cap at 1).
const MCP_DEPTH_ENV: &str = "CLAUTH_MCP_DEPTH";

/// Poll interval mirroring `start.rs`'s `wait_for_child` cadence.
const RUN_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Ceiling on `monitor`'s long-poll wait (seconds), sharing
/// [`MAX_RUN_TIMEOUT_SECS`] as the one number a caller may name for a duration.
/// It does not bound a delegate — a streaming run has no wall clock — so a wait
/// that ends at this ceiling is a wait to repeat, not a run that must be over.
/// One tool, one `wait_secs` parameter, so both waiting modes share one ceiling:
/// a tool cannot carry two limits on one parameter name.
const MAX_WAIT_SECS: u64 = MAX_RUN_TIMEOUT_SECS;
/// Ceiling for a peer that supplied no `progressToken`. The 3600 s cap above
/// depends on progress notifications re-anchoring Claude Code's 30-minute stdio
/// idle abort; a peer that sent no token cannot receive them, and the unclamped
/// cap would turn every long wait into a hard abort. The token IS the capability
/// probe — a config key would ask the operator to know their client's
/// idle-timeout behaviour, which is precisely the thing they cannot observe.
///
/// `pub(crate)` so a test can pin a RELATION against it, not because production
/// code outside `mcp` reads it. `runtime::ROTATION_LOCK_TIMEOUT` bounds the
/// pre-spawn rotation-lock wait, which is silent for exactly this reason — no
/// child exists yet to report progress on — and must sit inside this budget; that
/// is an inequality to assert, and asserting it needs the number.
pub(crate) const MAX_WAIT_SECS_NO_PROGRESS: u64 = 1500;

/// The wait this call actually gets: the requested seconds under whichever
/// ceiling this peer can survive.
fn clamp_wait(wait_secs: Option<u64>, can_receive_progress: bool) -> u64 {
    let cap = if can_receive_progress {
        MAX_WAIT_SECS
    } else {
        MAX_WAIT_SECS_NO_PROGRESS
    };
    wait_secs.unwrap_or(0).min(cap)
}

/// The wait one `monitor` call actually gets: [`clamp_wait`], floored by
/// [`CANCEL_GRACE_SECS`] when the call is stopping jobs.
///
/// A floor rather than a replacement — a caller who asked for longer keeps it —
/// and deliberately under the ceiling, so a cancel never buys a peer more silent
/// wait than it can survive.
fn effective_wait(wait_secs: Option<u64>, can_receive_progress: bool, cancel: bool) -> u64 {
    let wait = clamp_wait(wait_secs, can_receive_progress);
    if cancel {
        wait.max(CANCEL_GRACE_SECS)
    } else {
        wait
    }
}

/// Which lane ends a several-ids wait, or the refusal text for a value that
/// cannot mean anything in the mode it arrived in.
fn resolve_return_on(
    return_on: Option<&str>,
    has_job_ids: bool,
    cancel: bool,
) -> std::result::Result<ReturnOn, String> {
    match (return_on, has_job_ids) {
        // You asked to stop all of them, so you want to hear about all of them:
        // on `Any` the first lane to land ends the wait and the rest come back
        // as `running` rows under a reply that just cancelled them.
        (None, _) if cancel => Ok(ReturnOn::All),
        (None, _) => Ok(ReturnOn::Any),
        (Some(_), false) => Err(
            "`return_on` cannot combine with the state-waiting mode: it orders a set of jobs, \
             so name `job_ids` or drop it"
                .to_string(),
        ),
        (Some("any"), true) => Ok(ReturnOn::Any),
        (Some("all"), true) => Ok(ReturnOn::All),
        (Some(raw), true) => Err(format!(
            "unrecognized return_on \"{raw}\": accepted \"any\" and \"all\""
        )),
    }
}

/// Everything one `monitor` call needs from its own request: the peer plus the
/// progress token it supplied, the throttle clock and monotonic counter those
/// need (rmcp's `progress` field must strictly increase across one request's
/// notifications), and the cancellation token every wait loop races its sleep
/// against.
///
/// The cancel token lives here because this is already the one value threaded
/// through all three loops, and because it is the half of `RequestContext` a
/// test can construct — a `Peer<RoleServer>` is not.
///
/// A notification is best-effort. A dropped transport ends the request anyway,
/// and a failed one must never fail the wait it was describing.
pub(crate) struct ProgressSink {
    channel: Option<(rmcp::Peer<RoleServer>, rmcp::model::ProgressToken)>,
    /// Fired when the client sends `notifications/cancelled` for this request.
    /// rmcp cancels it but awaits the handler future bare, so nothing ends the
    /// call unless a loop reads this.
    ct: tokio_util::sync::CancellationToken,
    sent: f64,
    /// Tokio's clock, not the std one. Identical outside a paused runtime —
    /// every tick happens inside a wait loop tokio is already driving — and it
    /// is what lets a test advance the throttle window instead of sleeping
    /// through it.
    last: Option<tokio::time::Instant>,
    /// Test-only stand-in for the peer: what a tick would have put on the wire.
    /// `None` on every sink a real request builds, so the shipped shape is the
    /// two-field one above.
    #[cfg(test)]
    recorded: Option<Vec<String>>,
}

impl ProgressSink {
    /// A sink with no channel — the same state [`Self::from_context`] builds
    /// for a peer that sent no `progressToken`, reachable directly so an
    /// in-process caller, which cannot construct a `Peer<RoleServer>`, still
    /// drives the real handler.
    #[cfg(test)]
    pub(crate) fn none() -> Self {
        Self {
            channel: None,
            ct: tokio_util::sync::CancellationToken::new(),
            sent: 0.0,
            last: None,
            recorded: None,
        }
    }

    /// A sink that RECORDS what it would have sent instead of holding a peer.
    ///
    /// Without it, [`Self::tick`]'s throttle and the message it builds are both
    /// unreachable under test: `Peer<RoleServer>` cannot be constructed
    /// in-process, and a channel-less sink returns before either one runs. So
    /// this is not scaffolding around a tested path — it is the only way that
    /// path is executed at all.
    #[cfg(test)]
    pub(crate) fn recording() -> Self {
        Self {
            recorded: Some(Vec::new()),
            ..Self::none()
        }
    }

    /// Every message this sink has taken, in order.
    #[cfg(test)]
    pub(crate) fn recorded(&self) -> &[String] {
        self.recorded.as_deref().unwrap_or_default()
    }

    /// Whether anything is listening. Deliberately NOT
    /// [`Self::can_receive_progress`]: that one answers "did the peer supply a
    /// `progressToken`", which decides the wait ceiling and which a recording
    /// sink genuinely did not.
    fn has_destination(&self) -> bool {
        #[cfg(test)]
        {
            self.channel.is_some() || self.recorded.is_some()
        }
        #[cfg(not(test))]
        {
            self.channel.is_some()
        }
    }

    /// This call's cancellation token, for a loop that races something other
    /// than [`Self::sleep_or_cancelled`]'s sleep against it — and for a test
    /// that needs to fire one. The real token arrives on the request.
    pub(crate) fn cancel_token(&self) -> tokio_util::sync::CancellationToken {
        self.ct.clone()
    }

    fn from_context(ctx: &RequestContext<RoleServer>) -> Self {
        Self {
            channel: ctx
                .meta
                .get_progress_token()
                .map(|token| (ctx.peer.clone(), token)),
            ct: ctx.ct.clone(),
            sent: 0.0,
            last: None,
            #[cfg(test)]
            recorded: None,
        }
    }

    /// Sleep one poll slice, or wake the moment the client abandons the call.
    /// `true` = cancelled, which every loop treats as its deadline arriving:
    /// the response is discarded either way, so the cheapest correct thing is
    /// to stop reading disk and stop notifying a request id that is gone.
    async fn sleep_or_cancelled(&self, slice: Duration) -> bool {
        tokio::select! {
            () = tokio::time::sleep(slice) => false,
            () = self.ct.cancelled() => true,
        }
    }

    /// Whether this peer can receive progress at all, which is what decides the
    /// wait ceiling ([`clamp_wait`]).
    fn can_receive_progress(&self) -> bool {
        self.channel.is_some()
    }

    /// Send one progress line, at most once per [`HEARTBEAT_INTERVAL`]. The
    /// message is built lazily so a throttled tick costs nothing.
    async fn tick(&mut self, message: impl FnOnce() -> String) {
        let now = tokio::time::Instant::now();
        if !self.has_destination()
            || self
                .last
                .is_some_and(|t| now.duration_since(t) < HEARTBEAT_INTERVAL)
        {
            return;
        }
        self.last = Some(now);
        self.sent += 1.0;
        let message = message();
        // Test-only, and it returns: a recording sink holds no peer, so there
        // is nothing below it to run.
        #[cfg(test)]
        if let Some(log) = self.recorded.as_mut() {
            log.push(message);
            return;
        }
        if let Some((peer, token)) = self.channel.as_ref() {
            let param = rmcp::model::ProgressNotificationParam::new(token.clone(), self.sent)
                .with_message(message);
            let _ = peer.notify_progress(param).await;
        }
    }
}
/// Await a spawned delegate, sending the caller one progress line per
/// [`HEARTBEAT_INTERVAL`] until it lands.
///
/// Claude Code aborts a stdio tool call that has sent nothing for 30 minutes,
/// and every notification re-anchors that clock. A blocking `delegate` sent
/// none, which only ever mattered because a wall clock capped the run below the
/// abort; with a streaming run given no wall clock at all, a blocking run past
/// 30 minutes is expected rather than pathological, and without this the call
/// dies while the child keeps spending the target's window.
///
/// `tick` is a callback rather than the [`ProgressSink`] itself, for the reason
/// `read_stdout`'s heartbeat sink is one: an in-process caller cannot build a
/// `Peer<RoleServer>`, so a sink handed in here would swallow every notification
/// and the throttle would have nothing to prove itself against.
///
/// The send is awaited INSIDE the loop, so a backed-up transport leaves the join
/// unpolled for its duration and delays the finished envelope by that much.
/// Accepted rather than fixed: moving the send off the loop costs a spawned task
/// plus a channel, and a transport blocked long enough to matter has already
/// ended the request — a notification is best-effort for exactly that reason
/// ([`ProgressSink`]). It cannot drop the envelope, and it cannot orphan the
/// child, which keeps running on the blocking pool either way.
///
/// On `notifications/cancelled` the ticking stops — there is no point notifying
/// a request id that is gone — and `on_abandon` decides what becomes of the run.
/// A run with a child already spending the target's window is handed off to a
/// job file and this returns [`Joined::HandedOff`], leaving the task to finish
/// detached; anything else keeps the join and waits it out silently, because
/// rmcp awaits the tool handler bare and nothing else would ever read it.
async fn join_ticking<T>(
    handle: tokio::task::JoinHandle<T>,
    ct: &tokio_util::sync::CancellationToken,
    on_abandon: impl Fn() -> Abandoned,
    mut tick: impl AsyncFnMut(Duration),
) -> std::result::Result<Joined<T>, tokio::task::JoinError> {
    // Tokio's clock rather than the std one so a paused-time test measures the
    // schedule this loop actually keeps.
    let started = tokio::time::Instant::now();
    let mut handle = handle;
    let mut abandoned = false;
    loop {
        tokio::select! {
            joined = &mut handle => return joined.map(|value| Joined::Ran { value, abandoned }),
            () = ct.cancelled(), if !abandoned => {
                abandoned = true;
                if let Abandoned::HandedOff(job_id) = on_abandon() {
                    return Ok(Joined::HandedOff(job_id));
                }
            }
            () = tokio::time::sleep(HEARTBEAT_INTERVAL), if !abandoned => {
                tick(started.elapsed()).await;
            }
        }
    }
}

/// [`join_ticking`] for a set: joins every handle at once and returns one
/// `Result` per member, in the order the handles were passed in, so one
/// member's panic does not discard the siblings' spent windows. The result set
/// is pre-sized to the member count and filled by index, so a member with no
/// outcome still holds its slot and the caller's positional fold cannot shift a
/// later answer onto the wrong account. One tick for the whole set, one abandon
/// decision for the whole set — the caller's `on_abandon` hands off every member
/// still spending and returns them; non-empty means the wait is over, empty
/// means keep joining silently.
async fn join_all_ticking<T>(
    handles: Vec<tokio::task::JoinHandle<T>>,
    ct: &tokio_util::sync::CancellationToken,
    on_abandon: impl Fn() -> Vec<HandedOffMember>,
    mut tick: impl AsyncFnMut(Duration),
) -> JoinedAll<T>
where
    T: Send + 'static,
{
    // Tokio's clock rather than the std one so a paused-time test measures the
    // schedule this loop actually keeps.
    let started = tokio::time::Instant::now();
    let member_count = handles.len();
    let mut set = tokio::task::JoinSet::new();
    for (index, handle) in handles.into_iter().enumerate() {
        set.spawn(async move { (index, handle.await) });
    }
    let mut landed: Vec<Option<std::result::Result<T, String>>> =
        (0..member_count).map(|_| None).collect();
    let mut abandoned = false;
    while !set.is_empty() {
        tokio::select! {
            joined = set.join_next() => {
                match joined {
                    Some(Ok((index, Ok(value)))) => landed[index] = Some(Ok(value)),
                    Some(Ok((index, Err(e)))) => {
                        landed[index] = Some(Err(format!("delegate task panicked: {e}")));
                    }
                    // The wrapper task died without its index: unreachable (it
                    // only awaits a handle, and nothing aborts it), and a
                    // JoinSet-level error has no member to file it under. The
                    // slot stays `None` and folds into that member's own
                    // "result lost" row below, so the siblings keep their places.
                    Some(Err(e)) => logline!("clauth: delegate fan-out join error: {e}"),
                    None => {}
                }
            }
            () = ct.cancelled(), if !abandoned => {
                abandoned = true;
                let members = on_abandon();
                if !members.is_empty() {
                    return JoinedAll::HandedOff(members);
                }
            }
            () = tokio::time::sleep(HEARTBEAT_INTERVAL), if !abandoned => {
                tick(started.elapsed()).await;
            }
        }
    }
    let values = landed
        .into_iter()
        .map(|slot| slot.unwrap_or_else(|| Err("delegate result lost".to_string())))
        .collect();
    JoinedAll::Ran { values, abandoned }
}

/// How a [`join_all_ticking`] wait ended.
#[derive(Debug, PartialEq, Eq)]
enum JoinedAll<T> {
    /// Every run finished with this wait still on it, one `Result` per member
    /// in the order the handles were passed in: a panic becomes that member's
    /// own `Err`, and a member with no outcome reads "result lost", so the set
    /// is always as long as the member list.
    ///
    /// `abandoned` is true when the caller had ALREADY gone and there was
    /// nothing to hand off for any member, so the results are real but the
    /// reply built from them is never sent.
    Ran {
        values: Vec<std::result::Result<T, String>>,
        abandoned: bool,
    },
    /// The caller went away and at least one member outlived them; every member
    /// that handed off owns a job file under its id now and finishes on its own.
    HandedOff(Vec<HandedOffMember>),
}

/// How a [`join_ticking`] wait ended.
#[derive(Debug, PartialEq, Eq)]
enum Joined<T> {
    /// The run finished with this wait still on it; its result is here.
    ///
    /// `abandoned` is true when the caller had ALREADY gone and there was simply
    /// nothing to hand off — no child yet, the run landing first, or a failed
    /// mint. The result is real, but the reply built from it is never sent, so
    /// anything that reply would CONSUME has to be left for the next real one.
    Ran { value: T, abandoned: bool },
    /// The caller went away and the run outlived them, so it owns a job file
    /// under this id now and finishes on its own.
    HandedOff(String),
}

/// One fan-out member whose run outlived the caller: the job id it continues
/// under, plus the account and start time its reply names. Captured at hand-off
/// time because the run can finalize and move its reservation out of `Converted`
/// before the abandoned reply is built, hiding the id from a later re-read.
#[derive(Debug, Clone, PartialEq, Eq)]
struct HandedOffMember {
    job_id: String,
    profile: String,
    started_at: u64,
}

/// Hand off every member still spending, collecting the ones that landed a job
/// file. `starts` runs parallel to `handoffs` and carries each member's mint
/// time, so a reply row names the account and its start time without re-reading
/// a `Handoff` whose state moves as its run lands.
fn hand_off_members(
    names: &[String],
    handoffs: &[std::sync::Arc<Handoff>],
    starts: &[u64],
) -> Vec<HandedOffMember> {
    names
        .iter()
        .zip(handoffs.iter().zip(starts))
        .filter_map(|(name, (handoff, started_at))| match handoff.hand_off() {
            Abandoned::HandedOff(job_id) => Some(HandedOffMember {
                job_id,
                profile: name.clone(),
                started_at: *started_at,
            }),
            Abandoned::Kept => None,
        })
        .collect()
}

/// Which digest mode a blocking `delegate` reply gets.
///
/// Reporting CONSUMES the delta (see `digest`), and a reply to an abandoned
/// request is dropped by rmcp before the transport, so reporting into one
/// spends news that no reader ever sees and leaves the next real reply missing
/// it. Its own function for the reason `effective_wait` and `resolve_return_on`
/// are: folded into the call site, deleting the condition was invisible to the
/// whole suite.
fn delegate_digest_mode(digest: &DigestTracker, abandoned: bool) -> DigestMode<'_> {
    if abandoned {
        DigestMode::Skip
    } else {
        DigestMode::Report(digest)
    }
}

/// Fold a fan-out's per-member results into reply rows, in the order named. An
/// `Err` member becomes its own `is_error` row — a panic or a lost result
/// — rather than discarding every sibling's spent window. The pure split exists
/// so a reversed pairing reds a test instead of rendering two identical rows.
fn fold_fanout_rows(
    names: &[String],
    caller_env: &HashMap<String, String>,
    values: Vec<std::result::Result<serde_json::Value, String>>,
    now: i64,
) -> Vec<serde_json::Value> {
    names
        .iter()
        .zip(values)
        .map(|(name, result)| {
            let envelope = match result {
                Ok(value) => value,
                Err(reason) => serde_json::json!({
                    "profile": name,
                    "is_error": true,
                    "result": reason,
                }),
            };
            fold_delegate_live_usage(
                envelope,
                &ProfileName::from(name.clone()),
                delegate_call_endpoint(name, caller_env),
                delegate_call_provider(name, caller_env),
                now,
                DigestMode::Skip,
            )
        })
        .collect()
}

/// Whether a fan-out's whole result set is error rows. True only when every
/// row carries `is_error: true` and there is at least one: one bad account in
/// a fan-out must not hide the rest, and an empty set has nothing to report.
fn fanout_is_error(rows: &[serde_json::Value]) -> bool {
    !rows.is_empty()
        && rows.iter().all(|row| {
            row.get("is_error")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
        })
}

/// Floor on a cancelling `monitor`'s wait, so the common case is one call rather
/// than two.
///
/// It is a budget, not a guarantee: the reply carries whatever the jobs reached
/// inside it. Once a child exists the kill itself lands within one supervision
/// tick ([`RUN_POLL_INTERVAL`], 50 ms) and this covers the teardown that follows
/// — `crate::sessions::stamp_run_sessions`, plus an isolated
/// `crate::start::rescue_teardown`. It bounds nothing on the other side of the
/// spawn: a run still inside `ProfileRuntime::acquire` ends when that acquire
/// returns, and neither of that wait's legs is this constant's business — the
/// rotation-lock leg is bounded by `runtime::ROTATION_LOCK_TIMEOUT` whoever
/// holds it, and the acquire's own recursive `~/.claude` copy by nothing at all.
const CANCEL_GRACE_SECS: u64 = 10;

/// Process-local cancel registry: `job_id` → the flag that job's supervision
/// loop reads once per tick.
///
/// In-process rather than a flag in the job file, because the detached task runs
/// in this same process and an entry is therefore a direct handle with no
/// polling. A file flag would make the supervision loop read the job file every
/// 50 ms, per job, for a case that almost never fires, and would let any process
/// on the box cancel any job.
///
/// A LEAF with no `lockorder` rank, matching the job store's own posture: NEVER
/// acquire another lock while holding this one. Every caller takes it, does one
/// map operation, and drops it, so there is no ordering for the rank table to
/// police.
static CANCEL_REGISTRY: std::sync::LazyLock<
    std::sync::Mutex<HashMap<String, std::sync::Arc<AtomicBool>>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(HashMap::new()));

/// A running delegate's entry in [`CANCEL_REGISTRY`], removed on drop.
///
/// RAII because `run_delegate` exits from many places, and an entry outliving
/// its run would let a cancel report a stop that never happened — or stop a
/// later job minted under the same id.
struct CancelGuard {
    job_id: String,
    flag: std::sync::Arc<AtomicBool>,
}

impl CancelGuard {
    /// Register the flag the RUN reads, rather than minting one here.
    ///
    /// The caller holds it first because a run that is already in flight is
    /// already reading its own — a blocking delegate handed off by
    /// [`Handoff::hand_off`] is the case — and a fresh `Arc` here would leave
    /// its id cancellable in name only: `cancel_job` would set a flag nothing
    /// reads.
    fn register(job_id: &str, flag: std::sync::Arc<AtomicBool>) -> Self {
        CANCEL_REGISTRY
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(job_id.to_string(), std::sync::Arc::clone(&flag));
        Self {
            job_id: job_id.to_string(),
            flag,
        }
    }
}

impl Drop for CancelGuard {
    fn drop(&mut self) {
        CANCEL_REGISTRY
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.job_id);
    }
}

/// Ask a running delegate to stop. `true` when this server holds a run under
/// that id, which is the only thing the caller can be told for certain.
fn cancel_job(job_id: &str) -> bool {
    let registry = CANCEL_REGISTRY.lock().unwrap_or_else(|e| e.into_inner());
    match registry.get(job_id) {
        Some(flag) => {
            flag.store(true, Ordering::Relaxed);
            true
        }
        None => false,
    }
}

/// A cancelling `monitor`'s ask, and the deaths its wait observed happen.
///
/// The ask is set before the wait so the runs are already stopping while it
/// runs; the verdict is read after it, and only as observed. Death instants
/// are recorded where the wait loops first read a job file `Done` — the
/// collect evicts a done file before the reply assembles, so that read is the
/// only surviving witness — and are DATED off that file's mtime: a Done
/// file's only writer is the finalize's atomic rename and everything after it
/// removes the file, so the mtime is the moment the job finished. A death
/// strictly before the ask renders no verdict at all — a death whose age the
/// monotonic clock cannot represent included, since that can only predate
/// this process: that job's collect row already reports its outcome, and a
/// `killed` there would claim this call caused what it only witnessed. A job
/// with no recorded death was alive when
/// the wait gave up on it, whatever the flag intends: the flag is
/// read by the supervision loop, and between the registry entry and that loop
/// sit `load_config`, the pre-flight and `ProfileRuntime::acquire` — whose
/// rotation-lock wait queues behind a same-profile rotation or session start for
/// up to `runtime::ROTATION_LOCK_TIMEOUT`, and whose own recursive `~/.claude`
/// copy on a fake-symlink host is bounded by nothing at all. Either outlasts this
/// call's grace. So the verdict says what was seen, never "stopped".
///
/// An id the registry does not hold is NAMED rather than left to come back as a
/// plain `running` row, which reads as "the cancel did nothing". Its causes are
/// hedged the way [`unknown_job_reason`] hedges its own, because nothing here
/// can tell them apart: the run may already be finalizing (its registry entry
/// drops only after the result is on disk — [`Handoff::finalize`]), or it may
/// belong to an earlier server process whose registry went with it. No verdict
/// renders for an unheld id: there is no run here to observe.
struct CancelWatch {
    /// The instant the ask completed — every flag set, so no verdict can
    /// claim a death that preceded its own flag — and the point every per-job
    /// figure counts from, so the reply's seconds are what this call waited,
    /// never the grace floor.
    asked_at: Instant,
    asked: Vec<String>,
    unheld: Vec<String>,
    /// One entry per id the wait saw report `Done`. `None` inside an entry:
    /// the death is real but its dating cannot place it at or after the ask,
    /// so it renders no verdict. An id absent from the map never died here.
    deaths: HashMap<String, Option<Instant>>,
}

impl CancelWatch {
    /// Set every flag this call can set, and split the ids by whether this
    /// server holds a run. Only over ids that could name a job file at all: a
    /// registry key is always a minted id, so an unsafe one is not an unheld
    /// job, and the batch already reports it as `unknown`.
    fn ask(ids: &[String]) -> Self {
        let (asked, unheld): (Vec<String>, Vec<String>) = ids
            .iter()
            .filter(|id| jobs::is_safe_job_id(id))
            .cloned()
            .partition(|id| cancel_job(id));
        Self {
            // Sampled once the last flag is set rather than when the ask
            // begins: the gap is nothing to a seconds-truncated figure, and a
            // death inside it must not read as caused.
            asked_at: Instant::now(),
            asked,
            unheld,
            deaths: HashMap::new(),
        }
    }

    /// The wait loops' half: the first read that found this id `Done`, dated
    /// off the file's mtime and rebuilt against the stamping read's clocks so
    /// it compares with `asked_at` in the monotonic domain. `checked_sub`,
    /// never `-`: an age older than the monotonic clock's origin is
    /// unrepresentable, the subtraction would panic inside the tool handler
    /// on a backing that cannot go below its origin, and such an age can only
    /// mean the finalize preceded this process — so the stamp stays undated
    /// and the no-verdict rule applies. An UNREADABLE mtime — a peer's
    /// collect evicted the file between the read and the stamp — falls back
    /// to the observation itself instead: the `Done` read already earned a
    /// verdict, and `failed to kill` would be the false claim there. First
    /// stamp wins, so a later re-read cannot move a death.
    fn saw_done(&mut self, job_id: &str) {
        let dated = jobs::collectable_mtime_ms(job_id).map(|at| {
            Instant::now().checked_sub(Duration::from_millis(now_ms().saturating_sub(at)))
        });
        let died_at = match dated {
            Some(dated) => dated,
            None => Some(Instant::now()),
        };
        self.deaths.entry(job_id.to_string()).or_insert(died_at);
    }

    /// The line a cancelling `monitor` opens with: the ask, then one verdict
    /// per asked job that died at or after the ask, in the order named, then
    /// the unheld hedge. The `failed`
    /// figure runs to here because the note renders at the wait's own end,
    /// which is the moment the call gave up on every job still unaccounted.
    fn note(self) -> String {
        let list = |ids: &[String]| {
            ids.iter()
                .map(|id| format!("`{id}`"))
                .collect::<Vec<_>>()
                .join(", ")
        };
        let gave_up_at = Instant::now();
        let mut clauses = Vec::new();
        if !self.asked.is_empty() {
            clauses.push(format!(
                "asked {} to stop; each hands back whatever it had produced",
                list(&self.asked)
            ));
            let verdicts = self
                .asked
                .iter()
                .filter_map(|id| match self.deaths.get(id) {
                    Some(Some(died_at)) if *died_at >= self.asked_at => Some(render::kill_verdict(
                        id,
                        true,
                        died_at.duration_since(self.asked_at).as_secs(),
                    )),
                    // A death dated before the ask, or one the dating cannot
                    // place: the struct doc's no-verdict rule.
                    Some(_) => None,
                    None => Some(render::kill_verdict(
                        id,
                        false,
                        gave_up_at.duration_since(self.asked_at).as_secs(),
                    )),
                })
                .collect::<Vec<_>>();
            if !verdicts.is_empty() {
                clauses.push(verdicts.join("; "));
            }
        }
        if !self.unheld.is_empty() {
            clauses.push(format!(
                "no running delegate here for {}: it may already be finishing, or it may have been \
                 started by an earlier server process",
                list(&self.unheld)
            ));
        }
        if clauses.is_empty() {
            return String::new();
        }
        format!("{}.", clauses.join(". "))
    }
}

/// Put a cancel report ahead of a reply's own prose, inside the SAME content
/// block: every `monitor` arm returns exactly one, refusals included.
fn prepend_note(mut result: CallToolResult, note: &str) -> CallToolResult {
    let body = result
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.clone())
        .unwrap_or_default();
    result.content = single_block(format!("{note}\n{body}"));
    result
}

/// Poll cadence for both `monitor` modes and the `mcp-await-job` hook.
const JOB_POLL_INTERVAL: Duration = Duration::from_millis(200);
/// Self-deadline for the `mcp-await-job` hook.
///
/// A delivery window, NOT a bound on the delegate: a streaming run has no wall
/// clock, so no hook deadline can promise to outlast one and this number must
/// not be read as trying to. It buys the common case — a run that finishes
/// inside it is delivered without the model spending a turn — and on expiry the
/// hook still exits 2, waking the model with a nudge to call `monitor`, so a
/// longer run costs one deliberate check rather than a lost result.
///
/// Its own literal rather than a `MAX_RUN_TIMEOUT_SECS` offset, because that
/// constant now bounds only what a caller may type. `plugins/hooks/hooks.json`
/// carries the outer bound at 4260 s, and this must stay under it: the hook
/// process is killed at that one, and a kill delivers nothing.
const AWAIT_JOB_DEADLINE_SECS: u64 = 4200;

/// Most rows the state mode's listing names before it stops naming them.
///
/// Spending hundreds of rows on a caller who asked whether clauth's state had
/// moved is the cost this surface was reworked to refuse.
///
/// **This bound deliberately CUTS ACROSS the store's retention rule, and that is
/// the point rather than a cost.** The rows arrive from [`jobs::list_banded`],
/// so a stale live run is kept and a fresher finished one dropped — the exact
/// trade the retention rule would refuse, because retention answers "which record
/// is least worth keeping" while this answers "which row must a reader not
/// lose". An earlier version of this comment argued the opposite, that the bound
/// agreed with retention; that reasoning is what shipped a listing which evicted
/// the long-running delegate the mode exists to name, so do not restore it.
///
/// An operator wanting all of them runs `clauth jobs`, which bands the same way
/// and caps nothing.
const LISTING_MAX: usize = 10;

/// Fold the delegate jobs clauth is holding into a state-mode reply.
///
/// Adds nothing at all when the store is empty, so a session that has never
/// delegated pays nothing for a listing it has no use for — the same
/// only-when-true rule the roster's flags render by.
///
/// **This is safe to return a blocking run's content from, and the reason is
/// structural**: [`jobs::list_banded`] takes NO id, and neither does the
/// [`jobs::list`] it sorts. They enumerate the directory and return what they
/// find, so nothing a caller spells selects a file, which is
/// what [`jobs::RecordKind`] documents as the condition a new reader has to
/// meet. Filtering this listing by a caller-supplied id would be the exact
/// shape that type forbids; an id-keyed lookup belongs on `jobs::read`.
///
/// Each row carries an id, an account, a state and ONE age. No tail, no quota,
/// no deadline countdown: this exists so a caller can name a job, and
/// `monitor({job_ids})` is the check that reports what one is doing.
fn fold_jobs_listing(payload: &mut serde_json::Value, now: u64) {
    // Banded, never raw — see [`LISTING_MAX`] and `jobs::list_banded`.
    let stored = jobs::list_banded(now);
    if stored.is_empty() {
        return;
    }
    let rows: Vec<serde_json::Value> = stored
        .iter()
        .take(LISTING_MAX)
        .map(|job| listing_row(job, now))
        .collect();
    payload["jobs"] = serde_json::Value::Array(rows);
    let rest = stored.len().saturating_sub(LISTING_MAX);
    if rest > 0 {
        payload["jobs_not_listed"] = serde_json::json!(rest);
    }
}

/// One listing row. A live run is dated by how long it has been going, a
/// finished or orphaned one by how long since its record last mattered — two
/// different questions, so two different keys rather than one that means
/// whichever the state implies.
fn listing_row(job: &jobs::StoredJob, now: u64) -> serde_json::Value {
    let phase = job.phase();
    let mut row = serde_json::json!({
        "job_id": job.record.job_id,
        "profile": job.record.profile,
        "state": phase.label(),
    });
    if phase.is_live() {
        row["elapsed_secs"] =
            serde_json::json!(jobs::running_liveness(&job.record, now).elapsed_secs);
    } else {
        row["since_secs"] = serde_json::json!(job.age_secs(now));
    }
    row
}

/// Everything wrong with a `job_ids` list that the list alone decides, in the
/// exact words the reply carries.
///
/// The ONE producer of these three refusals, because `monitor` has to run them
/// BEFORE it cancels anything and the arms below still have to honour them: two
/// spellings of one rule is how the two drift, and the drift would show up as a
/// list refused in one place after being acted on in the other.
///
/// An unsafe id refuses only on the ONE-id spelling, which is where it always
/// did. A several-ids call resolves one to `unknown` in its own slot rather than
/// failing the whole batch, and `wait_for_batch` keeps it away from the path
/// join.
fn job_ids_refusal(job_ids: &[String]) -> Option<String> {
    // A bound on one response, no longer a mirror of the store's own retention:
    // the store can hold more than this many records (the TTLs bound it alone),
    // but a call naming thousands of ids is a response-size footgun and this
    // keeps it from growing without limit.
    if job_ids.len() > jobs::MAX_RETAINED {
        // The fix clause names the split the caller can make, and formats the
        // SAME `MAX_RETAINED` value the ceiling does, never a hardcoded
        // literal: the two halves of the sentence cannot drift when the
        // constant moves.
        return Some(format!(
            "`job_ids` capped at {} ids; got {} — split the ids across calls of {} or fewer",
            jobs::MAX_RETAINED,
            job_ids.len(),
            jobs::MAX_RETAINED,
        ));
    }
    // An empty list passes every per-id check vacuously and would return a
    // success-shaped `{"results": []}` that collected nothing.
    if job_ids.is_empty() {
        return Some("`job_ids` is empty: name at least one job_id".to_string());
    }
    match job_ids {
        [only] if !jobs::is_safe_job_id(only) => Some("invalid job_id".to_string()),
        _ => None,
    }
}

/// The one-id half of `monitor`'s job mode, byte-compatible with the
/// pre-merge single-`job_id` spelling: one envelope/status/error in one block,
/// an unknown id refused by name.
async fn monitor_one(
    job_id: String,
    wait: u64,
    digest: &DigestTracker,
    progress: &mut ProgressSink,
    watch: Option<&mut CancelWatch>,
) -> Result<CallToolResult, ErrorData> {
    // The path join below is guarded by [`job_ids_refusal`], which every caller
    // runs before this one and which refuses a one-id list that is not a safe
    // path component.
    debug_assert!(jobs::is_safe_job_id(&job_id));
    let now = now_ms();
    // READ FIRST: the sweep below destroys the very record this call came for
    // when that record is a corpse, and the `session_id` it carried is the
    // handle the caller needs. Captured before the sweep, it is what answers
    // the `Unknown` arm below instead of the aged branch.
    let before_sweep = jobs::read(&job_id);
    // A collect is the other moment a corpse matters — see
    // `jobs::gc_running_corpses`.
    jobs::gc_running_corpses(now);
    let outcome = wait_for_done(&job_id, wait, progress, watch).await;

    match outcome {
        WaitOutcome::Unknown => {
            // A corpse the sweep just reaped answers with the handle it carried;
            // anything else keeps the hedged branches. The silence check is the
            // sweep's own predicate, so this arm names exactly what the sweep
            // removed and nothing a concurrent collect made vanish.
            let reason = match &before_sweep {
                Some(record)
                    if record.state == jobs::JobState::Running
                        && jobs::running_is_silent(record, now) =>
                {
                    orphan_job_reason(&job_id, record)
                        .unwrap_or_else(|| unknown_job_reason(&job_id, now))
                }
                _ => unknown_job_reason(&job_id, now),
            };
            let payload = serde_json::json!({
                "is_error": true,
                "result": reason,
            });
            let prose = render::monitor_job_prose(&payload);
            Ok(CallToolResult::error(single_block(prose)))
        }
        WaitOutcome::Running(record) => {
            let payload = running_payload(&job_id, &record, now_ms());
            let prose = render::monitor_job_prose(&payload);
            Ok(CallToolResult::success(single_block(prose)))
        }
        WaitOutcome::Done(record) => {
            let (blocks, is_error) = render_done_envelope(record, digest);
            // Fallback path delivered it — evict only now that the envelope
            // is safely rendered, so the file doesn't linger past its
            // purpose (GC also reaps it on a TTL) while a panic inside
            // `render_done_envelope` still leaves the job file as the
            // recoverable copy.
            jobs::remove(&job_id);
            if is_error {
                Ok(CallToolResult::error(blocks))
            } else {
                Ok(CallToolResult::success(blocks))
            }
        }
    }
}

/// The several-ids half of `monitor`'s job mode: one result per requested id
/// in the order given. An absent id is its own `unknown` result, never a
/// batch-level failure; the reply tail carries ONE unknown-count clause for
/// the whole batch, however many rows read `unknown`. A done id is evicted
/// only after the whole batch rendered, so a mid-fold panic leaves every done
/// file as its recoverable copy. The protocol-level error flag mirrors the
/// per-result flags: any failed done envelope makes the whole batch an error.
async fn monitor_batch(
    job_ids: Vec<String>,
    wait: u64,
    return_on: ReturnOn,
    digest: &DigestTracker,
    progress: &mut ProgressSink,
    watch: Option<&mut CancelWatch>,
) -> Result<CallToolResult, ErrorData> {
    // The cap and the empty-list rule are [`job_ids_refusal`]'s, run by every
    // caller before this one and before any cancel.
    debug_assert!(job_ids_refusal(&job_ids).is_none());

    let now = now_ms();
    // READ FIRST, for the same reason the one-id arm does: the sweep below
    // reaps a corpse before the wait reads it, and the handle it carried is
    // the whole point of polling the id. An unsafe id can never name a job
    // file and resolves `Unknown` like the wait resolves it, so it is not
    // read at all here — the join below must stay off a caller's string.
    let before_sweep: Vec<Option<jobs::JobRecord>> = job_ids
        .iter()
        .map(|id| jobs::is_safe_job_id(id).then(|| jobs::read(id)).flatten())
        .collect();
    // Same reason as the one-id arm, and the same narrow scope.
    jobs::gc_running_corpses(now);
    let outcomes = wait_for_batch(&job_ids, wait, return_on, progress, watch).await;

    let mut results = Vec::with_capacity(outcomes.len());
    let mut delivered = Vec::new();
    let mut any_error = false;
    let mut unknown_job_id_count = 0u64;
    // The owner-ruled orphan copy, one line per reaped corpse with a handle,
    // in the order asked. Rendered after the rows because the row for a
    // missing file is the batch's bare `unknown` verdict — which stays TRUE,
    // the sweep did remove it — and this line says why it is missing.
    let mut orphan_reasons: Vec<String> = Vec::new();
    for ((id, outcome), prior) in outcomes.into_iter().zip(&before_sweep) {
        let entry = match outcome {
            WaitOutcome::Unknown => {
                unknown_job_id_count += 1;
                if let Some(record) = prior
                    && record.state == jobs::JobState::Running
                    && jobs::running_is_silent(record, now)
                    && let Some(reason) = orphan_job_reason(&id, record)
                {
                    orphan_reasons.push(reason);
                }
                serde_json::json!({ "job_id": id, "status": "unknown" })
            }
            WaitOutcome::Running(record) => running_payload(&id, &record, now_ms()),
            WaitOutcome::Done(record) => {
                // No per-result digest: one rides the whole reply below.
                let (mut payload, is_error) = fold_done_envelope(&record, DigestMode::Skip);
                any_error |= is_error;
                // The folded envelope is always an object (a non-object
                // self-report is wrapped under `result` first), so the caller's
                // per-id markers cannot collide with delegate output.
                if let serde_json::Value::Object(map) = &mut payload {
                    map.insert("job_id".to_string(), serde_json::Value::String(id.clone()));
                    map.insert(
                        "status".to_string(),
                        serde_json::Value::String("done".to_string()),
                    );
                }
                // Evict only when the file self-reports the id it was fetched
                // under, and evict by that caller-supplied id, never the
                // stored one: `jobs::remove` joins the id into a path without
                // a safety check, so a mismatched self-report (a hand-written
                // file) must never pick the eviction path.
                if record.job_id == id {
                    delivered.push(id);
                }
                payload
            }
        };
        results.push(entry);
    }
    let mut payload = serde_json::json!({ "results": results });
    // Present only when some id was unknown, like `since_your_last_call`
    // below: the prose spelling reads the count and names the cause ONCE at
    // the tail, so an all-unknown cap batch grows the reply by exactly that
    // one clause instead of one hedged cause per row.
    if unknown_job_id_count > 0 {
        payload["unknown_job_id_count"] = serde_json::json!(unknown_job_id_count);
    }
    // One digest for the whole call, top-level beside `results` where every
    // other surface carries it: a batch IS one call, and the per-result folds
    // run `DigestMode::Skip` because a per-result report would consume the
    // change into a place the prose spelling never renders.
    if let Some(delta) = DigestMode::Report(digest).folded() {
        payload["since_your_last_call"] = delta;
    }
    let mut prose = render::monitor_batch_prose(&payload);
    for reason in &orphan_reasons {
        prose.push('\n');
        prose.push_str(reason);
    }
    let blocks = single_block(prose);
    for id in delivered {
        jobs::remove(&id);
    }
    // The batch-level error flag mirrors the per-result flags: any failed
    // delegate makes the whole batch an error, so a client branching on
    // `isError` reads a failed job the same way in both spellings.
    if any_error {
        Ok(CallToolResult::error(blocks))
    } else {
        Ok(CallToolResult::success(blocks))
    }
}

/// Fold a finished job's envelope the way every delivery path does, returning
/// the payload and its error flag. Pure of the job store: the caller evicts the
/// file only after its render, so a panic inside leaves the job file as the
/// recoverable copy of the delegate's result.
fn fold_done_envelope(
    record: &jobs::JobRecord,
    digest: DigestMode<'_>,
) -> (serde_json::Value, bool) {
    // A crashed tombstone renders the owner's copy raw, never the envelope
    // fallback: the run's lifetime ended, just with no result to collect.
    if record.crashed
        && let Some(reason) = crashed_job_reason(&record.job_id, record)
    {
        return (
            serde_json::json!({ "crashed": true, "result": reason }),
            true,
        );
    }
    // A shared tombstone with no session id has no handle to promise, so
    // the envelope fallback below still answers it.
    let payload = fold_delegate_live_usage(
        record.envelope.clone().unwrap_or_else(|| {
            serde_json::json!({
                "profile": record.profile,
                "is_error": true,
                "result": "job finished without an envelope",
            })
        }),
        &ProfileName::from(record.profile.clone()),
        // The call's own answer, recorded at the mint. Absent on a record an
        // older server wrote, which the fold reads as "cannot say": a
        // name-keyed read would assert the managed field's answer for a call
        // that may have been retargeted by its own `env` argument.
        record.endpoint.clone(),
        record.provider.clone(),
        now_epoch_secs(),
        digest,
    );
    let is_error = payload
        .get("is_error")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    (payload, is_error)
}

/// Render a finished job's envelope into its response blocks and error flag.
fn render_done_envelope(
    record: jobs::JobRecord,
    digest: &DigestTracker,
) -> (Vec<ContentBlock>, bool) {
    let (payload, is_error) = fold_done_envelope(&record, DigestMode::Report(digest));
    let prose = render::monitor_job_prose(&payload);
    (single_block(prose), is_error)
}

/// Result of polling a background job file.
enum WaitOutcome {
    Done(jobs::JobRecord),
    /// Present but not yet finished (the wait deadline elapsed first). Carries the
    /// record so the caller can report `elapsed_secs`.
    Running(jobs::JobRecord),
    /// No such job file (never created or already evicted).
    Unknown,
}

/// The running-check payload both `monitor` arms render, so the one-id and
/// several-ids spellings cannot drift. `now` is epoch ms.
///
/// A field clauth structurally cannot have is ABSENT rather than `unknown`: no
/// `last_output_secs_ago` before the first line arrives, no `idle_kill_in_secs`
/// when the idle leg is off, no `wall_kill_in_secs` on a streaming run (which
/// has no wall clock), no tail when there is none.
///
/// Which makes a zero `timeout_secs` two different facts, and `idle_secs`
/// separates them: WITH one, this is a healthy streaming run whose only deadline
/// is the idle guard; WITHOUT one, the record predates these fields entirely and
/// the whole liveness set is dropped rather than counted down from defaults.
/// [`jobs::RunningLiveness`] holds that arithmetic, and holds it for the TUI's
/// delegates pane as well, so the two surfaces cannot report one record
/// differently. The throttle's accuracy bound is documented there.
fn running_payload(job_id: &str, record: &jobs::JobRecord, now: u64) -> serde_json::Value {
    let live = jobs::running_liveness(record, now);
    let mut payload = serde_json::json!({
        "job_id": job_id,
        "status": "running",
        "profile": record.profile,
        "elapsed_secs": live.elapsed_secs,
        "quota": quota_payload(&ProfileName::from(record.profile.clone())),
    });
    if !live.recorded {
        return payload;
    }
    if let Some(secs) = live.wall_kill_in_secs {
        payload["wall_kill_in_secs"] = serde_json::json!(secs);
    }
    if let Some(secs) = live.last_output_secs_ago {
        payload["last_output_secs_ago"] = serde_json::json!(secs);
    }
    if let Some(secs) = live.idle_kill_in_secs {
        payload["idle_kill_in_secs"] = serde_json::json!(secs);
    }
    if !record.tail.is_empty() {
        payload["tail"] = serde_json::json!(record.tail);
    }
    payload
}

/// [`running_payload`] for the TUI's agreement pin, which has to compare what
/// the delegates pane draws against what `monitor` tells the model about the
/// same record. Test-only on purpose: the pane derives its own figures from
/// [`jobs::running_liveness`], which is the half both surfaces share, while this
/// one also reads the target's usage cache off disk for the `quota` clause.
#[cfg(test)]
pub(crate) fn running_payload_for_test(
    job_id: &str,
    record: &jobs::JobRecord,
    now: u64,
) -> serde_json::Value {
    running_payload(job_id, record, now)
}

/// The epoch-ms a job id's stamp segment decodes to, base-36, `None` for a
/// token off the mint SHAPE. It answers what the stamp says and never whether
/// clauth minted the id: the shape gate admits any lowercase stamp, so a
/// pre-shortening all-digit id decodes far-future while a word like `d-day-1`
/// decodes to 1970. Those land in OPPOSITE age branches of [`unknown_job_reason`],
/// which is why both of them hedge the mint rather than presupposing a job.
fn job_id_minted_at(token: &str) -> Option<u64> {
    token_is_job_id(token)
        .then(|| token.split('-').nth(1))
        .flatten()
        .and_then(|ms| u64::from_str_radix(ms, 36).ok())
}

/// Why a `monitor` call naming a corpse's id is answered after the collect's
/// sweep reaped the record. The caller polls a run whose server died without
/// finishing it: the sweep removed the record the moment before the read, so
/// this answers for that sweep where [`unknown_job_reason`] would have hedged
/// "already collected … swept a day after it finished" — false for a crash —
/// and dropped the handle with it.
///
/// `None` when the record carries no `session_id` (a file written before the
/// field existed): the caller then keeps the existing [`unknown_job_reason`]
/// branch unchanged (owner ruling 2026-09-02), since the orphan copy names a
/// handle it has nothing to put there.
///
/// The copy is owner-ruled (2026-09-02, verbatim), never reworded. The split is
/// the record's own isolation flag: a shared run's transcript is in the global
/// store and its handle resolves, an isolated one's left with its throwaway
/// tree — the crash skipped the rescue — so offering that handle would promise
/// a resume `delegate` then refuses.
fn orphan_job_reason(job_id: &str, record: &jobs::JobRecord) -> Option<String> {
    let session_id = record.session_id.as_deref()?;
    Some(if record.isolated {
        format!(
            "unknown job_id: {job_id}. it died without finishing and its record was removed; \
             its transcript lived in an isolated store and left with it, so the run cannot be resumed."
        )
    } else {
        format!(
            "unknown job_id: {job_id}. it died without finishing and its record was removed. \
             it's still resumable from its session id: {session_id}"
        )
    })
}

/// The owner-ruled copy (verbatim, never reworded) for a crashed run whose
/// tombstone is STILL ON DISK ([`jobs::JobRecord::crashed`]): the sweep
/// converted the silent blocking run's liveness record into a `Done` record
/// with no envelope, keeping the handle and isolation flag. The shared arm
/// names the handle; the isolated arm cannot, because its transcript left with
/// the throwaway tree. A shared tombstone with no `session_id` returns `None` —
/// it has no handle to promise — and the caller keeps the envelope fallback.
fn crashed_job_reason(job_id: &str, record: &jobs::JobRecord) -> Option<String> {
    Some(if record.isolated {
        format!(
            "job {job_id} died without finishing and left no result; \
             its transcript lived in an isolated store and left with it, so the run cannot be resumed."
        )
    } else {
        let session_id = record.session_id.as_deref()?;
        format!(
            "job {job_id} died without finishing and left no result. \
             it's still resumable from its session id: {session_id}"
        )
    })
}

/// Why an id names no job file, and what the caller can do about it.
///
/// Only the FIRST branch is a derivation, and only of the SHAPE: a token that is
/// not `d-<base36>-<digits>` was never a clauth job at all. Past that gate the
/// stamp bounds a job's age and nothing more — it cannot say which cause fired,
/// since a job minted a day ago may equally have been collected five minutes
/// ago, and it cannot even say the id was minted, because the base-36 stamp
/// admits any lowercase word. So both age branches hedge every cause they name
/// AND carry the never-minted one, rather than asserting a cause and telling the
/// caller to spend another window on it. Which of the two a caller lands in is
/// the stamp's accident: the aged branch (the stamp older than
/// [`jobs::DONE_TTL_MS`]) is the only one a sweep can explain — neither reap
/// runs from less than a day back ([`jobs::RUNNING_TTL_MS`] adds a 600 s
/// grace on top), so a younger id cannot have been swept — and collection
/// leads there because every collect evicts while the sweep runs at startup
/// alone.
fn unknown_job_reason(job_id: &str, now: u64) -> String {
    // Checked FIRST, because it is the one cause this function can actually
    // know. Everything below hedges; this does not. A blocking delegate's record
    // is real and minted, so the three generic clauses are each false for it —
    // never collected, never dropped, and certainly minted — and answering an id
    // clauth is holding a live run under with "clauth may never have minted it"
    // is the mirror of the M5 defect where it asserted a mint it could not know.
    if jobs::liveness_exists(job_id) {
        return format!(
            "job_id {job_id} names a blocking `delegate` that is still running: \
             its result goes back through the call that started it, so there is \
             nothing here for `monitor` to collect"
        );
    }
    let Some(minted_at) = job_id_minted_at(job_id) else {
        return format!(
            "unknown job_id: {job_id} — clauth never minted it (a real id reads \
             `d-<base36-ms>-<counter>`); check the id `delegate` handed back"
        );
    };
    let collected = "already collected by an earlier `monitor` call or delivered by clauth's \
                     auto-delivery hook";
    // Neither age branch can tell a real id from a token clauth never minted,
    // so both carry this rather than presupposing a job existed. The stamp is
    // what splits them and it discriminates nothing: at a 2026 clock `d-day-1`
    // decodes to 1970 and takes the aged branch while `d-notebook-1` decodes
    // past today and takes the fresh one, and every pre-M5 all-digit id decodes
    // far-future into that same fresh branch. Putting it on the aged branch
    // alone would answer the rarer half.
    let unminted = "clauth may never have minted it at all (a real id reads \
                    `d-<base36-ms>-<counter>`)";
    if now.saturating_sub(minted_at) > jobs::DONE_TTL_MS {
        // Collection leads even here. Every collect evicts through
        // `jobs::remove`; the day-after-finish sweep runs at startup alone
        // (`jobs::gc`), so on a session that has been up a while the sweep is
        // the rarer of the two rather than the likelier.
        return format!(
            "unknown job_id: {job_id} — most likely {collected}; its stamp reads over a day \
             old, so it may also have been swept a day after it finished. {unminted}. check \
             this session's earlier replies before re-running the delegate"
        );
    }
    format!(
        "unknown job_id: {job_id} — most likely {collected}. \
         {unminted}. check this session's earlier replies for the result"
    )
}

/// Poll a job file until it reports `done`, `deadline_secs` elapses, or the
/// client abandons the call, ticking progress each slice off the freshest
/// running record. `Unknown` when the file is absent (distinct from `Running`
/// for a present-but-incomplete job).
async fn wait_for_done(
    job_id: &str,
    deadline_secs: u64,
    progress: &mut ProgressSink,
    mut watch: Option<&mut CancelWatch>,
) -> WaitOutcome {
    let start = Instant::now();
    let deadline = Duration::from_secs(deadline_secs);
    let mut cancelled = false;
    loop {
        match jobs::read(job_id) {
            Some(r) if r.state == jobs::JobState::Done => {
                if let Some(w) = watch.as_deref_mut() {
                    w.saw_done(job_id);
                }
                return WaitOutcome::Done(r);
            }
            Some(r) if cancelled || start.elapsed() >= deadline => {
                return WaitOutcome::Running(r);
            }
            Some(r) => {
                progress
                    .tick(|| render::running_status_prose(&running_payload(job_id, &r, now_ms())))
                    .await;
            }
            None => return WaitOutcome::Unknown,
        }
        cancelled = progress.sleep_or_cancelled(JOB_POLL_INTERVAL).await;
    }
}

/// Poll every id until the wait ends, mirroring `await_job_outcomes`'s
/// semantics: a done file resolves at once, an absent file resolves at once (it
/// never appears for a caller-supplied id), and a running file holds. One
/// outcome per id, in the order given.
///
/// `ReturnOn::Any` ends the wait on the first job to finish, so the reply is not
/// paced by the slowest lane. That break leaves slots unresolved, and the
/// deadline can cross mid-pass under either mode, so a final pass resolves every
/// remaining slot by its own state. The invariant it protects: `Unknown` belongs
/// to a MISSING file only — a running id must never fall out as one.
async fn wait_for_batch(
    job_ids: &[String],
    deadline_secs: u64,
    return_on: ReturnOn,
    progress: &mut ProgressSink,
    mut watch: Option<&mut CancelWatch>,
) -> Vec<(String, WaitOutcome)> {
    let start = Instant::now();
    let deadline = Duration::from_secs(deadline_secs);
    // `None` = unresolved. An unsafe id can never name a job file
    // (`new_job_id` mints only safe ids), so it resolves to `Unknown` upfront
    // and never reaches the path join.
    let mut outcomes: Vec<Option<WaitOutcome>> = job_ids
        .iter()
        .map(|id| (!jobs::is_safe_job_id(id)).then_some(WaitOutcome::Unknown))
        .collect();
    let mut any_done = false;
    let mut cancelled = false;
    loop {
        let mut unresolved = false;
        let mut newest: Option<jobs::JobRecord> = None;
        for (id, slot) in job_ids.iter().zip(&mut outcomes) {
            if slot.is_some() {
                continue;
            }
            match jobs::read(id) {
                Some(r) if r.state == jobs::JobState::Done => {
                    any_done = true;
                    if let Some(w) = watch.as_deref_mut() {
                        w.saw_done(id);
                    }
                    *slot = Some(WaitOutcome::Done(r));
                }
                Some(r) if cancelled || start.elapsed() >= deadline => {
                    *slot = Some(WaitOutcome::Running(r));
                }
                Some(r) => {
                    unresolved = true;
                    newest = Some(r);
                }
                None => *slot = Some(WaitOutcome::Unknown),
            }
        }
        if !unresolved || (return_on == ReturnOn::Any && any_done) {
            break;
        }
        if let Some(record) = &newest {
            progress
                .tick(|| {
                    render::running_status_prose(&running_payload(&record.job_id, record, now_ms()))
                })
                .await;
        }
        cancelled = progress.sleep_or_cancelled(JOB_POLL_INTERVAL).await;
    }
    job_ids
        .iter()
        .zip(outcomes)
        .map(|(id, slot)| {
            let outcome = slot.unwrap_or_else(|| match jobs::read(id) {
                Some(r) if r.state == jobs::JobState::Done => {
                    if let Some(w) = watch.as_deref_mut() {
                        w.saw_done(id);
                    }
                    WaitOutcome::Done(r)
                }
                Some(r) => WaitOutcome::Running(r),
                None => WaitOutcome::Unknown,
            });
            (id.clone(), outcome)
        })
        .collect()
}

/// `clauth mcp-await-job` — the body of the bundled PostToolUse `asyncRewake`
/// hook. Reads the hook payload on stdin, finds every background `job_id` in it,
/// waits for each, prints each delivered envelope's prose (prefixed with the
/// account it spent, the same opener the collect reply uses) to stdout, and
/// exits 2 to wake the model. A sync `delegate` (no `job_id` in the payload)
/// is a no-op (exit 0). On its own deadline it exits 2 with a nudge to call
/// `monitor` instead.
pub(crate) fn await_job() -> ! {
    use std::io::Read;
    let mut input = String::new();
    let _ = std::io::stdin().read_to_string(&mut input);
    let job_ids = serde_json::from_str::<serde_json::Value>(&input)
        .ok()
        .as_ref()
        .map(extract_job_ids)
        .unwrap_or_default()
        .into_iter()
        .filter(|id| jobs::is_safe_job_id(id))
        .collect::<Vec<_>>();
    if job_ids.is_empty() {
        std::process::exit(0); // sync delegate or unparseable input: nothing to deliver
    }

    let (delivered, pending) =
        await_job_outcomes(&job_ids, Duration::from_secs(AWAIT_JOB_DEADLINE_SECS));
    for envelope in &delivered {
        // One line per delivered envelope, each opening with its account: a
        // fan-out delivers N lines in one hook run; a bare cost figure names
        // nobody to charge it to.
        let profile = envelope
            .get("live_usage")
            .and_then(|lu| lu.get("profile"))
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        outln!(
            "delegate to `{profile}` {}",
            render::envelope_prose(envelope)
        );
    }
    if delivered.is_empty() {
        std::process::exit(0); // every id already gone: nothing was delivered
    }
    if pending.is_empty() {
        std::process::exit(2); // wake the model with the result(s)
    }
    let noun = if pending.len() == 1 { "job" } else { "jobs" };
    outln!(
        "delegate {noun} `{}` still running; call `monitor` to retrieve {}",
        pending.join("`, `"),
        if pending.len() == 1 { "it" } else { "them" }
    );
    std::process::exit(2);
}

/// Poll every id in `job_ids` until each is `done` or gone, or `deadline`
/// passes. Returns the delivered envelopes, folded the way every collect
/// folds them ([`fold_done_envelope`]: live-usage footer, cost endpoint, and
/// the no-envelope fallback), plus the ids still `running` at the deadline.
/// An absent id is dropped silently (its file was GC'd or already
/// collected). Blocking; the hook calls it directly on its own thread.
fn await_job_outcomes(
    job_ids: &[String],
    deadline: Duration,
) -> (Vec<serde_json::Value>, Vec<String>) {
    let start = Instant::now();
    let mut delivered = Vec::new();
    let mut pending: Vec<&String> = job_ids.iter().collect();
    loop {
        pending.retain(|id| match jobs::read(id) {
            Some(r) if r.state == jobs::JobState::Done => {
                let (envelope, _is_error) = fold_done_envelope(&r, DigestMode::Skip);
                delivered.push(envelope);
                false
            }
            Some(_) => true, // still running: the loop exit decides on the deadline
            None => false,
        });
        if pending.is_empty() || start.elapsed() >= deadline {
            return (delivered, pending.into_iter().cloned().collect());
        }
        std::thread::sleep(JOB_POLL_INTERVAL);
    }
}

/// Extract every background job id from a hook payload, preferring the
/// documented `tool_response` slot so a delegate prompt that happens to carry a
/// `job_id` can't shadow the real handles; fall back to a whole-payload scan
/// only if that slot yields none (the exact shape is not host-guaranteed).
fn extract_job_ids(payload: &serde_json::Value) -> Vec<String> {
    let ids = payload
        .get("tool_response")
        .and_then(|tr| {
            let found = find_job_ids(tr);
            (!found.is_empty()).then_some(found)
        })
        .unwrap_or_else(|| find_job_ids(payload));
    let mut seen: Vec<String> = Vec::with_capacity(ids.len());
    for id in ids {
        if !seen.contains(&id) {
            seen.push(id);
        }
    }
    seen
}

/// Recursively collect every job id from a hook-payload JSON, in document
/// order. A string `job_id` field is collected wherever it sits; a string that
/// is itself JSON is parsed and descended (the MCP tool result nests the
/// response envelope as a JSON-encoded string), so this stays agnostic to the
/// exact `tool_response` shape, which the host does not pin down.
fn find_job_ids(v: &serde_json::Value) -> Vec<String> {
    let mut out = Vec::new();
    collect_job_ids(v, &mut out);
    out
}

fn collect_job_ids(v: &serde_json::Value, out: &mut Vec<String>) {
    match v {
        serde_json::Value::Object(map) => {
            // A `job_id` value is the id itself, not a container to descend (and
            // not text to scan): collected once, never re-scanned as a token.
            let mut ids = Vec::new();
            for (key, value) in map {
                if key == "job_id" {
                    ids.push(value);
                } else {
                    collect_job_ids(value, out);
                }
            }
            for value in ids {
                if let serde_json::Value::String(s) = value {
                    out.push(s.clone());
                }
            }
        }
        serde_json::Value::Array(arr) => {
            for item in arr {
                collect_job_ids(item, out);
            }
        }
        serde_json::Value::String(s) => match serde_json::from_str::<serde_json::Value>(s) {
            Ok(parsed) => collect_job_ids(&parsed, out),
            // Not JSON: the prose spelling. `render::delegate_fanout_prose`
            // carries no `job_id` KEY, so the `d-<base36-ms>-<n>` tokens it
            // prints are the only way those jobs auto-arrive.
            Err(_) => out.extend(scan_job_ids(s)),
        },
        _ => {}
    }
}

/// Real job ids are `d-<base36-ms>-<n>`. Scan a plain string for such tokens so
/// a prose tool reply still yields every job of a fan-out. The stamp is base-36
/// rather than digits, so a lowercase `d-`-prefixed word such as `d-day-1` now
/// matches too; that widening is deliberate — a length floor on the stamp would
/// break the day the encoding width changes, and the digits-only gate already
/// matched `d-2024-1`.
fn scan_job_ids(s: &str) -> Vec<String> {
    s.split(|c: char| !(c.is_ascii_alphanumeric() || c == '-'))
        .filter(|token| token_is_job_id(token))
        .map(str::to_string)
        .collect()
}

/// `d-<base36>-<digits>`, the exact [`jobs::new_job_id`] shape: a base-36
/// `[0-9a-z]` stamp then a decimal counter. A legacy all-digit stamp is a valid
/// base-36 spelling, so pre-shortening ids keep matching.
fn token_is_job_id(token: &str) -> bool {
    let mut parts = token.split('-');
    matches!(parts.next(), Some("d"))
        && parts.next().is_some_and(|p| {
            !p.is_empty()
                && p.bytes()
                    .all(|b| b.is_ascii_digit() || b.is_ascii_lowercase())
        })
        && parts
            .next()
            .is_some_and(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
        && parts.next().is_none()
}

/// Inputs for one delegated `delegate`. Grouped into a struct so `run_delegate`
/// avoids a too-many-arguments signature as the surface grew (cwd/env/args/
/// timeouts/isolation). Both deadlines stay raw here: their defaults depend on
/// whether the composed arg list leaves the child streaming.
struct DelegateOpts<'a> {
    profile: &'a str,
    prompt: &'a str,
    model: Option<&'a str>,
    cwd: Option<&'a str>,
    env: HashMap<String, String>,
    extra_args: Vec<String>,
    timeout_secs: Option<u64>,
    idle_secs: Option<u64>,
    resume: Option<&'a str>,
    isolation: Isolation,
    depth: u32,
    /// Where this run's result goes and how a caller reaches it: the job record
    /// it heartbeats into, and the flag a stop reads. `None` only for a run
    /// nobody can reach — no job file, no cancel — which is a test fixture
    /// rather than a shape the server produces.
    handoff: Option<std::sync::Arc<Handoff>>,
}

/// Owned twin of [`DelegateOpts`] for a background launch: the detached task is
/// `'static`, so it owns its inputs rather than borrowing the handler's locals.
/// Grouped so `launch_background_delegate` keeps a short signature and a fan-out
/// clones the whole set once per account instead of field by field.
#[derive(Clone)]
struct BackgroundOpts {
    prompt: std::sync::Arc<str>,
    model: Option<String>,
    cwd: Option<String>,
    env: HashMap<String, String>,
    extra_args: Vec<String>,
    timeout_secs: Option<u64>,
    idle_secs: Option<u64>,
    resume: Option<String>,
    isolation: Isolation,
    depth: u32,
}

/// Why the supervision loop stopped waiting on the child, when it was not the
/// child exiting. Both arms leave the loop rather than returning, so the stdout
/// reader thread is joined on every path out of `run_delegate`.
enum WaitEnd {
    /// A deadline fired; the child was killed and hands back what it wrote.
    Expired(Expiry),
    /// The caller stopped the run through `monitor({cancel: true})`. Its own arm
    /// rather than a third [`Expiry`]: a cancel is not a deadline, and the
    /// envelope must not claim one.
    Cancelled,
    /// `try_wait` itself failed, so clauth no longer knows the child's state.
    Failed(String),
}

/// Why the supervision loop should stop waiting on the child this tick, `None`
/// to keep waiting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StopReason {
    /// The caller asked for this run to stop.
    Cancelled,
    /// One of the two deadlines fired.
    Expired(Expiry),
}

/// The whole stop decision for one supervision tick, cancel flag beside the two
/// deadlines. One pure function because all three end the same run the same way,
/// and because a decision that lives in the loop can only be tested with a child
/// process — which this crate deliberately never fakes.
///
/// The cancel is read first: an explicit stop and a deadline can land in the same
/// tick, and reporting the clock there would tell the caller their cancel did
/// nothing.
fn stop_reason(
    cancelled: bool,
    elapsed: Duration,
    last_progress: Duration,
    wall: Option<Duration>,
    idle: Duration,
    streaming: bool,
) -> Option<StopReason> {
    if cancelled {
        return Some(StopReason::Cancelled);
    }
    expiry(elapsed, last_progress, wall, idle, streaming).map(StopReason::Expired)
}

/// Which deadline killed a delegate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Expiry {
    /// Nothing arrived on stdout for the idle window.
    Idle,
    /// The run outlived the wall-clock ceiling a pinned `--output-format` gave
    /// it. Only that shape has one.
    Wall,
}

/// Resolve a delegate's `(wall, idle)` deadlines, `None` for a run that has no
/// wall clock at all.
///
/// A STREAMING run never gets one. The stream is a liveness signal, so the idle
/// guard already ends every stuck run, and a wall clock on top of it can only
/// ever kill a delegate that is working — mid-answer, at a cost the target
/// account has already paid. `timeout_secs` is therefore ignored there, which is
/// what the tool description says.
///
/// Without the stream there is no liveness signal at all, so silence carries no
/// information and the idle leg is off; a wall clock is then the only thing that
/// can end a hung child, and an unset one falls back to the idle value rather
/// than leaving it to sit forever.
fn resolve_deadlines(
    timeout_secs: Option<u64>,
    idle_secs: Option<u64>,
    streaming: bool,
) -> (Option<Duration>, Duration) {
    let idle = idle_secs
        .unwrap_or(DEFAULT_IDLE_SECS)
        .clamp(1, MAX_RUN_TIMEOUT_SECS);
    let wall = if streaming {
        None
    } else {
        Some(timeout_secs.map_or(idle, |secs| secs.clamp(1, MAX_RUN_TIMEOUT_SECS)))
    };
    (wall.map(Duration::from_secs), Duration::from_secs(idle))
}

/// Which deadline (if either) a still-running delegate has tripped.
/// `last_progress` is how far into the run its most recent output arrived. Each
/// leg is off in exactly the mode where its signal means nothing: no idle leg
/// without the stream, and no wall clock with it.
fn expiry(
    elapsed: Duration,
    last_progress: Duration,
    wall: Option<Duration>,
    idle: Duration,
    streaming: bool,
) -> Option<Expiry> {
    // Wall clock first: where there is one it is the outer bound, and a delegate
    // that stalls near the ceiling trips both in the same poll.
    if wall.is_some_and(|wall| elapsed >= wall) {
        return Some(Expiry::Wall);
    }
    (streaming && elapsed.saturating_sub(last_progress) >= idle).then_some(Expiry::Idle)
}

/// True when the caller pins its own `--output-format` in `args`. clauth then
/// spawns no format flag of its own, and the child's output shape is unknown, so
/// the idle deadline is off (silence would no longer mean "stuck").
fn sets_output_format(extra_args: &[String]) -> bool {
    extra_args
        .iter()
        .any(|a| a == "--output-format" || a.starts_with("--output-format="))
}

/// What the stdout reader keeps from a streamed delegate. The transcript runs to
/// megabytes and only the terminal envelope is wanted, so lines are inspected and
/// dropped rather than buffered, alongside a bounded tail of the assistant text
/// that lets a killed run still return something.
#[derive(Default)]
struct StreamCapture {
    /// The child's own session id, from the first event carrying one. The handle
    /// a later `resume` needs, and the only way to get it out of a run that never
    /// reached its terminal envelope.
    session_id: Option<String>,
    /// The newest `rate_limit_event` line. Kept because stdout is no longer
    /// buffered whole: without it a throttle that shows up only there would stop
    /// being detectable on a non-zero exit.
    rate_limit_line: Option<String>,
    /// Last line tagged `type:"result"`.
    result_line: Option<String>,
    /// Last parseable non-delta line, whatever its type: the same fallback
    /// [`result_event`] applies to a transcript array.
    last_line: String,
    /// Assistant text from completed message blocks.
    text: String,
    /// Deltas of the block still in flight. Cleared by that block's own
    /// `assistant` event, which carries the same text, so nothing lands twice.
    pending: String,
}

impl StreamCapture {
    /// The whole of stdout as one buffer, for a caller-pinned output format.
    fn from_raw(bytes: &[u8]) -> Self {
        Self {
            last_line: String::from_utf8_lossy(bytes).into_owned(),
            ..Self::default()
        }
    }

    /// The bytes to parse as the delegate's terminal envelope.
    fn envelope_src(&self) -> &str {
        self.result_line.as_deref().unwrap_or(&self.last_line)
    }

    /// Assistant text produced so far: completed blocks plus the in-flight one.
    fn partial_text(&self) -> String {
        format!("{}{}", self.text, self.pending)
    }

    fn push_line(&mut self, line: &str) {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            return;
        };
        // Every event carries it, deltas included, so this is read before the
        // type match returns early for one.
        if self.session_id.is_none()
            && let Some(id) = value.get("session_id").and_then(serde_json::Value::as_str)
        {
            self.session_id = Some(id.to_string());
        }
        match value.get("type").and_then(serde_json::Value::as_str) {
            Some("rate_limit_event") => {
                self.rate_limit_line = Some(line.to_string());
                return;
            }
            // Token-level deltas: liveness plus the in-flight block's text. Kept
            // out of `last_line` so a parse-failure report shows a real event.
            Some("stream_event") => {
                self.push_delta(&value);
                return;
            }
            Some("result") => self.result_line = Some(line.to_string()),
            Some("assistant") => self.push_assistant(&value),
            _ => {}
        }
        self.last_line = line.to_string();
    }

    /// Fold a completed assistant message's text blocks into the salvage buffer.
    fn push_assistant(&mut self, value: &serde_json::Value) {
        self.pending.clear();
        let Some(blocks) = value.pointer("/message/content").and_then(|c| c.as_array()) else {
            return;
        };
        for block in blocks {
            if block.get("type").and_then(serde_json::Value::as_str) == Some("text")
                && let Some(text) = block.get("text").and_then(serde_json::Value::as_str)
            {
                self.text.push_str(text);
            }
        }
        keep_tail(&mut self.text, PARTIAL_TEXT_CAP);
    }

    /// Append a `content_block_delta` chunk. Thinking deltas are skipped: the
    /// salvage is the answer, not the reasoning behind it.
    fn push_delta(&mut self, value: &serde_json::Value) {
        if value
            .pointer("/event/delta/type")
            .and_then(serde_json::Value::as_str)
            == Some("text_delta")
            && let Some(text) = value
                .pointer("/event/delta/text")
                .and_then(serde_json::Value::as_str)
        {
            self.pending.push_str(text);
            keep_tail(&mut self.pending, PARTIAL_TEXT_CAP);
        }
    }
}

/// Trim a salvage buffer to its last `cap` bytes, on a char boundary.
fn keep_tail(s: &mut String, cap: usize) {
    if s.len() <= cap {
        return;
    }
    let mut start = s.len() - cap;
    while !s.is_char_boundary(start) {
        start += 1;
    }
    s.replace_range(..start, "");
}

/// The delegate's newest assistant text as one bounded display line: the last
/// [`TAIL_CAP`] bytes (on a char boundary), every whitespace run collapsed to a
/// single space and the ends trimmed. A running status is a status, and a
/// delegate's answer is full of newlines.
fn tail_line(capture: &StreamCapture) -> String {
    let mut text = capture.partial_text();
    keep_tail(&mut text, TAIL_CAP);
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// A run's "write this now" callback. It is handed the capture and nothing else:
/// the run-relative clock `read_stdout` keeps is anchored at the child's spawn
/// while the job file's `started_at` is anchored at the run's own start, and
/// passing one where the other is meant is the skew this signature removes.
///
/// The sink resolves its target record per call rather than closing over one,
/// because a blocking run acquires a record mid-read when its caller abandons
/// the call (`Handoff::spec`). A beat that finds none writes nothing.
type HeartbeatSink<'a> = &'a mut dyn FnMut(&StreamCapture);

/// Read the child's stdout to EOF, stamping `progress` with the elapsed
/// milliseconds at every line so the wait loop can tell a working delegate from
/// a stalled one. Non-streaming mode drains the pipe whole (there is nothing to
/// stamp until the child exits, and so nothing to heartbeat either).
///
/// `heartbeat` is the run's "write this now" callback, called at most once per
/// [`HEARTBEAT_INTERVAL`]. The throttle lives HERE rather than in the sink so it
/// is testable in one place and every sink stays pure. The sink is a closure on
/// this thread rather than a read from the supervision loop because the tail
/// text lives inside `StreamCapture`, which this thread owns exclusively:
/// handing it over would mean a `Mutex<String>` written once per token delta on
/// the hottest path in the run, which is a lock the MCP layer is not allowed to
/// add and buys nothing.
///
/// EVERY server-produced run passes a sink, blocking ones included — a blocking
/// run can acquire a job record mid-read, and one that never does simply has
/// every beat resolve to no record. `None` is a test fixture calling this
/// directly, which is what keeps the function pure under test. "Heartbeats are
/// background-only" used to be structural here and no longer is; what bounds a
/// blocking run's writes is that it has no record to write into until its caller
/// abandons it.
fn read_stdout<R: std::io::Read>(
    reader: R,
    streaming: bool,
    start: Instant,
    progress: &AtomicU64,
    mut heartbeat: Option<HeartbeatSink<'_>>,
) -> StreamCapture {
    let mut reader = reader;
    if !streaming {
        return StreamCapture::from_raw(&drain_pipe(&mut reader));
    }
    let mut buffered = std::io::BufReader::new(reader);
    let mut capture = StreamCapture::default();
    let mut raw = Vec::new();
    let mut last_beat: Option<Instant> = None;
    loop {
        raw.clear();
        // read_until over lines(): a single event can carry a multi-megabyte tool
        // result, and invalid UTF-8 must not end the capture early.
        match std::io::BufRead::read_until(&mut buffered, b'\n', &mut raw) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        let stamp = elapsed_ms(start);
        progress.store(stamp, Ordering::Relaxed);
        capture.push_line(String::from_utf8_lossy(&raw).trim());
        if let Some(sink) = heartbeat.as_mut() {
            let now = Instant::now();
            if last_beat.is_none_or(|t| now.duration_since(t) >= HEARTBEAT_INTERVAL) {
                last_beat = Some(now);
                sink(&capture);
            }
        }
    }
    capture
}

/// Milliseconds since `start`, saturating (the `u128` only exceeds a `u64` after
/// some 584 million years, so the cast never truncates in practice).
fn elapsed_ms(start: Instant) -> u64 {
    u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// Every envelope for a run that handed back no clean result: its own `reason`
/// text, then the text the run had already produced and the handle that finishes
/// it without paying twice.
///
/// ONE builder for the kill path, the non-zero exit and the unparseable
/// envelope. The argument was won for the kill path and written into its own
/// doc comment — the target account's window is spent whether or not clauth
/// keeps the output, so discarding it is a second loss on top of the first — and
/// nothing about the other two exits makes it less true. Three copies of the
/// clause rule is how they drift.
///
/// The handle turns on the session id alone: a shared run's transcript is already
/// in the global store, and an isolated one's is lifted into it on teardown. That
/// lift is best-effort and this builder cannot observe it — see
/// [`crate::start::rescue_teardown`], which defers to a live sibling — so the
/// reply hands back the handle and never tells a caller its transcript is gone.
/// Both cases say where they stand: a handle, or a run that ended before any
/// event named a session. The silent second arm was finding 18 — a description
/// promising a handle, answered with nothing.
fn salvage_envelope(
    profile: &str,
    mut reason: String,
    capture: &StreamCapture,
) -> serde_json::Value {
    let partial = capture.partial_text();
    if !partial.is_empty() {
        reason.push_str(". the text it had written is in `partial_result`");
    }
    // The clause and the field it promises are decided together, so a reply can
    // never offer a handle it did not attach. No id means no handle, whatever
    // the isolation was: a run that died in 200ms without one has no transcript
    // clauth ever saw, so it is told that and nothing else.
    let handle = match &capture.session_id {
        None => {
            reason.push_str(". no session id ever reached clauth, so there is no resume handle");
            None
        }
        Some(id) => {
            reason.push_str(". pick the run back up with `resume: \"<session_id>\"`");
            Some(id.clone())
        }
    };
    let mut payload = serde_json::json!({
        "profile": profile,
        "is_error": true,
        "result": reason,
    });
    if !partial.is_empty() {
        payload["partial_result"] = serde_json::Value::String(partial);
    }
    if let Some(id) = handle {
        payload["session_id"] = serde_json::Value::String(id);
    }
    payload
}

/// Envelope for a delegate clauth killed on one of its deadlines: the salvage
/// plus which clock fired and how long the run got.
fn timeout_envelope(
    profile: &str,
    expiry: Expiry,
    elapsed: Duration,
    limit: Duration,
    capture: &StreamCapture,
) -> serde_json::Value {
    let elapsed_secs = elapsed.as_secs();
    let limit_secs = limit.as_secs();
    let (kind, reason) = match expiry {
        Expiry::Idle => (
            "idle",
            format!(
                "delegate killed after {elapsed_secs}s: it produced no output for {limit_secs}s. \
                 raise `idle_secs` if the task makes one blocking call longer than that"
            ),
        ),
        Expiry::Wall => (
            "wall_clock",
            format!(
                "delegate killed at its {limit_secs}s wall-clock ceiling. \
                 raise `timeout_secs` for a longer run"
            ),
        ),
    };
    let mut payload = salvage_envelope(profile, reason, capture);
    payload["timed_out"] = serde_json::json!(kind);
    payload["elapsed_secs"] = serde_json::json!(elapsed_secs);
    payload
}

/// Envelope for a delegate the caller stopped through `monitor({cancel: true})`.
///
/// `cancelled` rather than a third `timed_out` value: a cancel is a decision,
/// not a clock, and every reader branching on `timed_out` prints the deadline
/// that fired.
///
/// `reason` is the caller's, because the two stages differ in the one fact a
/// caller acts on: a cancel caught before the spawn spent nothing, while one
/// caught by the supervision loop spent whatever the run had already used. The
/// fields do not differ, so they are stamped here.
fn cancelled_envelope(
    profile: &str,
    reason: String,
    elapsed: Duration,
    capture: &StreamCapture,
) -> serde_json::Value {
    let mut payload = salvage_envelope(profile, reason, capture);
    payload["cancelled"] = serde_json::json!(true);
    payload["elapsed_secs"] = serde_json::json!(elapsed.as_secs());
    payload
}

/// Resolve a `resume` id to the workspace its transcript was recorded under.
/// Claude Code resolves `--resume <id>` only within `projects/<slug-of-cwd>/`, so
/// a resume spawned anywhere else is told the conversation does not exist.
///
/// `latest` is refused, though `clauth resume` takes it: the newest session in
/// the whole store is usually the operator's own live one, and spending an
/// account's window continuing that is never what a delegate meant by it.
fn resolve_resume_workspace(session_id: &str) -> std::result::Result<std::path::PathBuf, String> {
    if session_id == "latest" {
        return Err(
            "resume needs an exact session id; `latest` is a `clauth resume` shorthand".to_string(),
        );
    }
    let workspace = crate::sessions::workspace_of(session_id).ok_or_else(|| {
        format!("can't resume '{session_id}': no transcript for it, or none recording a workspace")
    })?;
    if !workspace.is_dir() {
        return Err(format!(
            "can't resume '{session_id}': workspace '{}' no longer exists",
            workspace.display()
        ));
    }
    Ok(workspace)
}

/// Refuse a `cwd` that disagrees with the workspace a `resume` must run in,
/// rather than spawning where Claude Code will not find the transcript. Both
/// sides are canonicalized: one spelling of a path is not the same string as
/// another spelling of it.
fn check_resume_cwd(given: &str, workspace: &std::path::Path) -> std::result::Result<(), String> {
    let given_real = std::fs::canonicalize(given)
        .map_err(|e| format!("cwd '{given}' cannot be resolved: {e}"))?;
    let workspace_real = std::fs::canonicalize(workspace).map_err(|e| {
        format!(
            "workspace '{}' cannot be resolved: {e}",
            workspace.display()
        )
    })?;
    if given_real != workspace_real {
        return Err(format!(
            "cwd '{given}' is not the workspace this session was recorded in ('{}'); \
             drop `cwd` and clauth uses the recorded one",
            workspace.display()
        ));
    }
    Ok(())
}

/// Compose a delegate's environment on `command`: drop inherited provider
/// routing + the outgoing activation's custom env keys
/// ([`crate::runtime::scrub_profile_env`]), layer the caller's `env`, then
/// clauth's own keys which always win. `CLAUDE_CONFIG_DIR` and the depth guard
/// can't be overridden, and `CLAUDE_CODE_MAX_OUTPUT_TOKENS` only defaults when
/// the caller didn't set it.
fn apply_delegate_env(
    command: &mut Command,
    caller_env: &HashMap<String, String>,
    stale_env_keys: &[String],
    config_dir: &std::path::Path,
    depth: u32,
) {
    crate::runtime::scrub_profile_env(command, stale_env_keys);
    command.envs(caller_env);
    if !caller_env.contains_key("CLAUDE_CODE_MAX_OUTPUT_TOKENS") {
        command.env("CLAUDE_CODE_MAX_OUTPUT_TOKENS", DEFAULT_MAX_OUTPUT_TOKENS);
    }
    command
        .env("CLAUDE_CONFIG_DIR", config_dir)
        .env(MCP_DEPTH_ENV, (depth + 1).to_string());
}

/// Blocking delegate: acquire the target profile's runtime, spawn a headless
/// `claude -p` with piped stdio, enforce whichever deadlines that run has, and
/// parse its JSON envelope. Returns `Ok(envelope)` on a clean parse and a
/// [`salvage_envelope`] for every other way a SPAWNED run can end — killed,
/// cancelled, non-zero exit, unreadable output — because each of those spent the
/// window and each has output worth handing back. `Err(reason)` is reserved for
/// the refusals BEFORE the spawn, which carry no capture; the caller wraps one
/// in an `is_error` envelope.
/// Records observed throughput / rate-limit hits as a side effect, and runs
/// `crate::sessions::stamp_run_sessions` and (isolated)
/// `crate::start::rescue_teardown` on the way out.
/// Never bubbles a transport-level error.
fn run_delegate(opts: DelegateOpts<'_>) -> std::result::Result<serde_json::Value, String> {
    // Anchors the pre-spawn arm's `elapsed_secs`, which measures the time spent
    // getting to a spawn rather than the run's own: nothing has run yet.
    let entered = Instant::now();
    let handoff = opts.handoff.clone();
    let config = load_config().map_err(|e| format!("failed to load config: {e}"))?;
    let profile_name = ProfileName::from(opts.profile);
    let target = config
        .find(&profile_name)
        .ok_or_else(|| profile_not_found(opts.profile, ProfileNotFoundFix::CallProfiles))?;
    // Mirrors `disable_profile`'s own live-session refusal from the other
    // direction: that guard stops disabling a profile mid-session, this one
    // stops opening a brand-new session on one already disabled. Also the
    // backstop for a background job whose target changed after its pre-flight,
    // since the config is re-loaded here. Guard rationale: `preflight_target`.
    preflight_target(target, &config, &profile_name)?;

    if let Some(dir) = opts.cwd
        && !std::path::Path::new(dir).is_dir()
    {
        return Err(format!("cwd does not exist or is not a directory: {dir}"));
    }

    // A resume must land in the workspace its transcript was recorded under, so
    // that resolution REPLACES the caller's cwd instead of sitting beside it; a
    // `cwd` that disagrees is a mistake worth naming, not one to spawn into.
    let workspace = match opts.resume {
        Some(id) => {
            let workspace = resolve_resume_workspace(id)?;
            if let Some(dir) = opts.cwd {
                check_resume_cwd(dir, &workspace)?;
            }
            Some(workspace)
        }
        None => None,
    };

    // Strip the outgoing profile's custom env so a delegate for `<target>` does
    // not inherit whoever was globally active — or, with no marker to read
    // (`switch_off` clears it without touching the file), the departed
    // account whose entries are still in the live settings (mirrors
    // `clauth start`).
    let stale_env_keys = crate::actions::outgoing_env_keys(&config);

    // Guard kept alive across spawn+wait; dropped on return for RAII teardown.
    // A delegate is a one-shot headless run against a named account, so it never
    // follows the chain — moving it mid-prompt would change who answered.
    let runtime = ProfileRuntime::acquire(target, opts.isolation, &stale_env_keys, false)
        .map_err(|e| format!("failed to acquire runtime: {e}"))?;

    // The acquire above is the longest thing that can happen before a child
    // exists, and the supervision loop that reads this flag does not exist until
    // one does: its rotation-lock wait queues behind a same-profile rotation or
    // session start (bounded by `runtime::ROTATION_LOCK_TIMEOUT`, tens of seconds),
    // and a copy-mode host mirrors `~/.claude`
    // inside it. A cancel that landed in there must not now spawn the run it
    // cancelled. Nothing has been spent at this point, which is the fact the
    // caller acts on — and the fact that decides [`Handoff::hand_off`] stops an
    // abandoned run here rather than minting a job file for it.
    if handoff.as_ref().is_some_and(|h| h.is_cancelled()) {
        // One read of the clock: two would let the prose and `elapsed_secs`
        // straddle a second boundary and disagree in the same envelope.
        let waited = entered.elapsed();
        return Ok(cancelled_envelope(
            opts.profile,
            format!(
                "delegate cancelled after {}s, before it spawned: the account's window was \
                 not spent",
                waited.as_secs()
            ),
            waited,
            &StreamCapture::default(),
        ));
    }

    let mut command = crate::runtime::claude_command();
    apply_delegate_env(
        &mut command,
        &opts.env,
        &stale_env_keys,
        runtime.config_dir(),
        opts.depth,
    );
    // Stream the child's events as NDJSON instead of waiting for one terminal
    // blob: the wait loop needs a liveness signal to tell a working delegate from
    // a hung one, and a killed run must still hand back the text it wrote.
    // `stream-json` refuses to run under `-p` without `--verbose`;
    // `--include-partial-messages` adds the token deltas, so a single long
    // generation counts as progress instead of reading as silence.
    let streaming = !sets_output_format(&opts.extra_args);
    command
        .args(["-p", opts.prompt])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::null());
    if streaming {
        command.args([
            "--output-format",
            "stream-json",
            "--verbose",
            "--include-partial-messages",
        ]);
    }
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
    if let Some(id) = opts.resume {
        command.args(["--resume", id]);
    }
    // Resolve the cwd the spawned `claude` will actually run in: a resume's
    // recorded workspace, else the caller's override, else this process's own cwd
    // (inherited like `start.rs`). If it's the real `$HOME`, guard against the
    // project-settings leak.
    let explicit_cwd = workspace.or_else(|| opts.cwd.map(std::path::PathBuf::from));
    if let Some(dir) = explicit_cwd.as_deref() {
        command.current_dir(dir);
    }
    let effective_cwd = explicit_cwd.or_else(|| std::env::current_dir().ok());
    if let Some(dir) = effective_cwd.as_deref() {
        crate::runtime::guard_home_project_settings(&mut command, dir);
    }
    command.args(&opts.extra_args);

    let run_start = std::time::SystemTime::now();

    let mut child = command
        .spawn()
        .map_err(|e| format!("failed to spawn claude: {e}"))?;
    // A child exists, so the target's window is being spent from here: a caller
    // that walks away now gets this run handed off to a job file rather than
    // stopped, which is the whole difference [`Handoff::hand_off`] reads.
    if let Some(handoff) = handoff.as_ref() {
        handoff.mark_spawned();
    }

    // Drain both pipes on their own threads from the moment of spawn. A bare
    // try_wait loop never reads, so a >~64KiB result blocks the child on a full
    // pipe and it never exits — a false timeout that drops a valid result. Killing
    // the child closes the write ends, the readers hit EOF, and the joins return.
    let start = Instant::now();
    let progress = std::sync::Arc::new(AtomicU64::new(0));
    let beat_handoff = handoff.clone();
    let stdout_reader = child.stdout.take().map(|h| {
        let progress = std::sync::Arc::clone(&progress);
        std::thread::spawn(move || match beat_handoff {
            // A run that owns a job file rewrites it as it reads, so a `monitor`
            // check sees liveness that would otherwise die with this task.
            // Best-effort: a failed heartbeat costs one stale check, and must
            // never end the capture.
            //
            // The record is resolved PER BEAT rather than once here, because a
            // blocking run gets its file mid-run when its caller walks away and
            // this thread was spawned before it existed. What keeps that record
            // alive until the first beat lands — or forever, on a pinned
            // `--output-format` run, which never beats at all — is its
            // `recorded_at` mint stamp, not this closure.
            Some(handoff) => {
                let mut beat = |capture: &StreamCapture| {
                    if let Some(spec) = handoff.spec() {
                        // The session id arrives HERE, mid-run, and rides the
                        // capture from its first event on: the record this
                        // thread rewrites is the only place a crashed run's
                        // resume handle survives, so every beat carries it.
                        let _ = jobs::write_heartbeat_with_session(
                            &spec,
                            now_ms(),
                            &tail_line(capture),
                            capture.session_id.as_deref(),
                        );
                    }
                };
                read_stdout(h, streaming, start, &progress, Some(&mut beat))
            }
            None => read_stdout(h, streaming, start, &progress, None),
        })
    });
    let stderr_reader = child
        .stderr
        .take()
        .map(|mut h| std::thread::spawn(move || drain_pipe(&mut h)));

    let (wall, idle) = resolve_deadlines(opts.timeout_secs, opts.idle_secs, streaming);

    // Nothing between the spawn above and the join below may return: the reader
    // thread would outlive this call, the child would keep writing into it
    // (`Child::drop` does not kill), and its heartbeats would overwrite the
    // `write_done` the caller makes next — leaving a finished job polling
    // `running` until GC and an `mcp-await-job` blocked on a terminal state that
    // never arrives. So a supervision failure kills and falls through to the
    // same join every other path takes, carrying its reason.
    // `run_delegate_never_returns_between_spawning_the_reader_and_joining_it`
    // is the guard; this comment only says why it is there.
    let outcome = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) => {
                let last_progress = Duration::from_millis(progress.load(Ordering::Relaxed));
                let stopped = handoff.as_ref().is_some_and(|h| h.is_cancelled());
                if let Some(stop) = stop_reason(
                    stopped,
                    start.elapsed(),
                    last_progress,
                    wall,
                    idle,
                    streaming,
                ) {
                    let _ = child.kill();
                    let _ = child.wait();
                    break Err(match stop {
                        StopReason::Cancelled => WaitEnd::Cancelled,
                        StopReason::Expired(expiry) => WaitEnd::Expired(expiry),
                    });
                }
                std::thread::sleep(RUN_POLL_INTERVAL);
            }
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                break Err(WaitEnd::Failed(format!("failed to wait for claude: {e}")));
            }
        }
    };

    // Joined before the timeout branch returns: the kill above closed the write
    // ends, so the readers are at EOF and the capture holds everything the run
    // produced before it died.
    let capture = stdout_reader
        .and_then(|h| h.join().ok())
        .unwrap_or_default();
    let stderr_bytes = join_reader(stderr_reader);

    // Mirrors `start::run`'s own teardown legs, in the same window: the child
    // has exited and the guard is still alive, so the tree is there to read.
    // See `crate::sessions::stamp_run_sessions` and, on an isolated run,
    // `crate::start::rescue_teardown`.
    let isolated = opts.isolation == Isolation::Isolated;
    let projects_dir = if isolated {
        Some(runtime.config_dir().join("projects"))
    } else {
        crate::profile::claude_dir()
            .ok()
            .map(|d| d.join("projects"))
    };
    if let Some(projects_dir) = projects_dir {
        crate::sessions::stamp_run_sessions(opts.profile, &projects_dir, isolated, run_start);
    }
    if isolated && let Ok(claude_home) = crate::profile::claude_dir() {
        crate::start::rescue_teardown(runtime.config_dir(), runtime.sessions_dir(), &claude_home);
    }

    let status = match outcome {
        Ok(status) => status,
        Err(WaitEnd::Failed(reason)) => return Err(reason),
        Err(WaitEnd::Cancelled) => {
            // Read once, for the reason the pre-spawn arm reads once.
            let ran_for = start.elapsed();
            return Ok(cancelled_envelope(
                opts.profile,
                format!("delegate cancelled after {}s", ran_for.as_secs()),
                ran_for,
                &capture,
            ));
        }
        Err(WaitEnd::Expired(expiry)) => {
            let limit = match expiry {
                Expiry::Idle => idle,
                // `expiry` only ever reports `Wall` off a `Some` comparison, so
                // the fallback is a convention rather than a reachable arm: it
                // keeps the envelope quoting a real ceiling if the two drift.
                Expiry::Wall => wall.unwrap_or(idle),
            };
            return Ok(timeout_envelope(
                opts.profile,
                expiry,
                start.elapsed(),
                limit,
                &capture,
            ));
        }
    };
    let now = now_epoch_secs();
    match classify_run(status, &stderr_bytes, &capture, opts.profile) {
        RunOutcome::Exited {
            envelope,
            throttle_scan,
        } => {
            // A non-zero exit can be a throttle; record it so `profiles` can flag
            // the model as rate-limited (clauth never sees inference 429s any
            // other way).
            if let RateLimit::Yes { retry_after_s } = rate_limit_hint(&throttle_scan) {
                crate::throughput::record_rate_limit(
                    &ProfileName::from(opts.profile),
                    opts.model,
                    retry_after_s,
                    now,
                );
            }
            Ok(envelope)
        }
        RunOutcome::Unparseable(envelope) => Ok(envelope),
        RunOutcome::Envelope(envelope) => {
            // A clean exit can still carry an in-band error envelope (rate limit
            // shows up there with `--output-format json`); branch on `is_error`
            // so a throttle is recorded as one, not as a (bogus) throughput
            // sample.
            if envelope
                .get("is_error")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false)
            {
                if let RateLimit::Yes { retry_after_s } = rate_limit_hint(&envelope.to_string()) {
                    crate::throughput::record_rate_limit(
                        &ProfileName::from(opts.profile),
                        opts.model,
                        retry_after_s,
                        now,
                    );
                }
            } else {
                record_throughput_from_envelope(opts.profile, opts.model, &envelope, now);
            }
            Ok(envelope)
        }
    }
}

/// What a finished delegate's joined pieces mean, with none of the recording
/// they imply: a throttle hit and a throughput sample are side effects on shared
/// state, and [`run_delegate`] keeps them.
///
/// Split out because the live spawn paths have no unit test by standing decision
/// — this crate never fakes a `claude` on PATH, since a fake binary would assert
/// nothing about the real envelope contract — so handing this function a real
/// `ExitStatus` is the only way to drive the two lossy arms at all.
enum RunOutcome {
    /// A clean exit whose terminal envelope parsed: the delegate's own
    /// self-report, verbatim.
    Envelope(serde_json::Value),
    /// A non-zero exit, salvaged. `throttle_scan` is everything a rate-limit
    /// hint could be hiding in.
    Exited {
        envelope: serde_json::Value,
        throttle_scan: String,
    },
    /// A clean exit whose output was no envelope clauth could read, salvaged.
    Unparseable(serde_json::Value),
}

fn classify_run(
    status: std::process::ExitStatus,
    stderr_bytes: &[u8],
    capture: &StreamCapture,
    profile: &str,
) -> RunOutcome {
    let stdout = capture.envelope_src();
    if !status.success() {
        let stderr = String::from_utf8_lossy(stderr_bytes);
        let throttle_scan = format!(
            "{stderr}{stdout}{}",
            capture.rate_limit_line.as_deref().unwrap_or_default()
        );
        let reason = format!(
            "claude exited with {}: {}",
            status
                .code()
                .map_or_else(|| "signal".to_string(), |c| c.to_string()),
            truncate(stderr.trim(), 2000)
        );
        return RunOutcome::Exited {
            envelope: salvage_envelope(profile, reason, capture),
            throttle_scan,
        };
    }
    match parse_delegate_envelope(stdout.trim()) {
        Ok(envelope) => RunOutcome::Envelope(envelope),
        Err(reason) => RunOutcome::Unparseable(salvage_envelope(profile, reason, capture)),
    }
}

/// Refuse a resolved target that `delegate` must not spend on: a profile the
/// operator disabled, a recognised third-party profile whose inference has
/// nothing to authenticate with (which
/// would spawn a `claude` that dies on an empty envelope), or a quarantined
/// one with nothing but that dead chain to authenticate with. The keyless test is
/// `has_inference_auth`, the predicate derived from
/// `build_claude_settings_json` (a validated api key, or a profile `env` entry
/// carrying `ANTHROPIC_AUTH_TOKEN` / `ANTHROPIC_API_KEY`) — NOT the usage
/// predicate `third_party_credentialed`, whose Alibaba exemption reads the
/// console session that authenticates the quota gateway only. `is_third_party`
/// scopes the check: an OAuth account has no provider.
///
/// Every refusal names its fix, because the reader is a model that can run the
/// command: an unnamed one costs a turn to look up.
///
/// The quarantine gate is `config.is_auth_broken`, a pure in-memory read of
/// `AppState::auth_broken`, and it is deliberately NOT a refresh attempt: the
/// MCP layer takes no rotation lock. It sits AFTER the disabled bail for the
/// reason `switch` orders them the same way — a disabled, clock-expired target
/// must be refused before anything can rotate its single-use refresh token.
/// It skips any target whose own endpoint and credential serve inference
/// (`claude::has_own_inference_endpoint`, owner ruling 2026-08-30): that
/// account's chain feeds usage polling alone, so a quarantine is no reason to
/// refuse its spawn. Everything else still takes the arm — an OAuth account,
/// an endpoint with no credential, and a credential with no endpoint alike:
/// the exemption is for an account that demonstrably routes and authenticates
/// on its own, never a judgement that the others have nothing. The keyless arm
/// sits ABOVE it and is load-bearing
/// there: a keyless third-party target fails that predicate too, and must be
/// told about the key rather than sent to a browser login.
///
/// Called from every path that refuses before a spawn: the single-background
/// arm and `resolve_fanout` up front, and `run_delegate` as the blocking
/// path's own check plus the backstop for a target that changed after its
/// pre-flight (the config is re-loaded there).
fn preflight_target(
    profile: &Profile,
    config: &AppConfig,
    name: &ProfileName,
) -> std::result::Result<(), String> {
    if profile.is_disabled() {
        return Err(format!(
            "profile is disabled: {name} (run `clauth enable {name}`)"
        ));
    }
    // BEFORE the quarantine arm, and that order is the whole of what the
    // deleted quarantined+keyless arm used to say: this target's spawn is
    // stopped by the missing key, and a key is what its fix has to name. The
    // quarantine sentence below would send it to a browser login that leaves
    // the key missing.
    if profile.is_third_party() && !crate::claude::has_inference_auth(profile) {
        return Err(crate::format::third_party_keyless(name));
    }
    // A quarantined target whose own endpoint and credential serve inference
    // is ADMITTED (owner ruling 2026-08-30, "let the delegate run"): the dead
    // chain feeds usage polling, the spawned `claude` never reads it, so the
    // run would have succeeded. `has_own_inference_endpoint` is the shared
    // predicate — whether clauth RECOGNISES the host says nothing about
    // whether inference works against it. What is left is the account with
    // nothing but the dead chain, refused with `switch`'s own sentence
    // (`actions.rs`, its AUTH-1 arm) so the two surfaces cannot spell that
    // quarantine two ways.
    if config.is_auth_broken(name) && !crate::claude::has_own_inference_endpoint(profile) {
        return Err(crate::format::login_expired(name).line());
    }
    Ok(())
}

/// Resolve a `profiles` fan-out list to canonical target names. Refuses by name:
/// a list over [`MAX_FANOUT`], a duplicate (case-insensitive, the same rule a
/// single `profile` resolves under), a name resolving to no account, or
/// anything [`preflight_target`] refuses — a disabled member, a recognised
/// third-party member with no inference auth source, a quarantined one that
/// does not serve its own inference.
/// Runs before any spawn: N delegates is N real usage windows with no undo.
fn resolve_fanout(config: &AppConfig, raw: &[String]) -> std::result::Result<Vec<String>, String> {
    // An empty list passes every check below vacuously and would return a
    // success-shaped `{"jobs": []}` that spent nothing and spawned nothing.
    if raw.is_empty() {
        return Err("`profiles` is empty: name at least one profile".to_string());
    }
    if raw.len() > MAX_FANOUT {
        // The fix clause names the split the caller can make, and formats the
        // SAME `MAX_FANOUT` the ceiling does, never a hardcoded literal: the
        // two halves of the sentence cannot drift when the constant moves.
        return Err(format!(
            "`profiles` fan-out capped at {MAX_FANOUT} names; got {} — split the names across calls of {MAX_FANOUT} or fewer",
            raw.len()
        ));
    }
    let mut seen = std::collections::HashSet::with_capacity(raw.len());
    for name in raw {
        if !seen.insert(name.to_ascii_lowercase()) {
            return Err(format!(
                "duplicate profile in `profiles`: `{name}` (case-insensitive)"
            ));
        }
    }
    let mut resolved = Vec::with_capacity(raw.len());
    let mut missing = Vec::new();
    for name in raw {
        match config.canonical_name(name) {
            Some(canonical) => resolved.push(canonical),
            None => missing.push(name.clone()),
        }
    }
    if !missing.is_empty() {
        return Err(profile_not_found(
            &missing.join(", "),
            ProfileNotFoundFix::CallProfiles,
        ));
    }
    // A member that cannot be spent on refuses the whole fan-out before the
    // first spawn, like an unknown name does: the spend has no undo. Same
    // pre-flight as the single-background arm (`preflight_target`, rationale
    // there): disabled by the operator, or a recognised third-party profile
    // with nothing to authenticate inference.
    for name in &resolved {
        let profile = config
            .find(&ProfileName::from(name.clone()))
            .ok_or_else(|| profile_not_found(name, ProfileNotFoundFix::CallProfiles))?;
        preflight_target(profile, config, &ProfileName::from(name.clone()))?;
    }
    Ok(resolved)
}

/// Join a relative path onto `base` lexically, resolving `.` and `..` without
/// touching the filesystem. Refuses an absolute path and a `..` that escapes
/// `base`. `base` is already canonical, so the result is lexically under it;
/// the caller re-checks symlinks right before the read.
fn normalize_join(
    base: &std::path::Path,
    rel: &str,
) -> std::result::Result<std::path::PathBuf, String> {
    if std::path::Path::new(rel).is_absolute() {
        return Err(format!(
            "prompt_file `{rel}` refused: absolute path (must be relative to `cwd`)"
        ));
    }
    let mut parts: Vec<std::ffi::OsString> = Vec::new();
    for comp in std::path::Path::new(rel).components() {
        match comp {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                if parts.pop().is_none() {
                    return Err(format!("prompt_file `{rel}` refused: path escapes `cwd`"));
                }
            }
            std::path::Component::Normal(part) => parts.push(part.to_os_string()),
            // On Windows `is_absolute()` needs BOTH a prefix and a root, so a
            // drive-relative `C:foo` and a root-relative `\foo` both pass the
            // check above and arrive here. Dropping either component silently
            // re-roots the path under `base` and reads a different file than
            // the caller named, so refuse by name.
            std::path::Component::RootDir | std::path::Component::Prefix(_) => {
                return Err(format!(
                    "prompt_file `{rel}` refused: absolute path (must be relative to `cwd`)"
                ));
            }
        }
    }
    let mut out = base.to_path_buf();
    for part in parts {
        out.push(part);
    }
    Ok(out)
}

/// Resolve and read a `prompt_file` relative to the delegate's `cwd`, validating
/// at the boundary and re-checking immediately before the read. The path is
/// canonicalized and checked against `cwd` in one place, then opened and read
/// with no work in between, so the thing checked is the thing read. Only a
/// regular file is accepted. Returns the prompt text.
fn read_prompt_file(cwd: Option<&str>, rel: &str) -> std::result::Result<String, String> {
    let base = match cwd {
        Some(dir) => std::path::PathBuf::from(dir),
        None => std::env::current_dir().map_err(|e| format!("cwd cannot be resolved: {e}"))?,
    };
    let base_real = std::fs::canonicalize(&base)
        .map_err(|e| format!("cwd '{}' cannot be resolved: {e}", base.display()))?;
    let candidate = normalize_join(&base_real, rel)?;
    // Re-check immediately before the read: canonicalize resolves any symlink, so
    // a link pointing outside `cwd` fails the starts_with check, and the resolved
    // path is the file opened below.
    let real = std::fs::canonicalize(&candidate)
        .map_err(|e| format!("prompt_file `{rel}` refused: {e}"))?;
    if !real.starts_with(&base_real) {
        return Err(format!(
            "prompt_file `{rel}` refused: symlink target resolves outside `cwd`"
        ));
    }
    // Type check BEFORE the open: `metadata` is a stat and never opens the path,
    // so a FIFO is refused here instead of freezing the read-only open (which
    // blocks until a writer appears) on the server's only thread. A directory
    // used to slip through to an EISDIR-shaped refusal at read time; it is now
    // refused by type too.
    let meta = std::fs::metadata(&real).map_err(|e| format!("prompt_file `{rel}` refused: {e}"))?;
    if !meta.is_file() {
        return Err(format!("prompt_file `{rel}` refused: not a regular file"));
    }
    let file =
        std::fs::File::open(&real).map_err(|e| format!("prompt_file `{rel}` refused: {e}"))?;
    // The check that binds, on the opened handle: a path swapped between the
    // stat above and the open cannot sneak a non-regular file past it.
    let meta = file
        .metadata()
        .map_err(|e| format!("prompt_file `{rel}` refused: {e}"))?;
    if !meta.is_file() {
        return Err(format!("prompt_file `{rel}` refused: not a regular file"));
    }
    let size = meta.len();
    if size > PROMPT_FILE_CAP {
        return Err(format!(
            "prompt_file `{rel}` refused: {size} bytes over the {PROMPT_FILE_CAP} byte cap"
        ));
    }
    read_prompt_handle(file, rel)
}

/// Read the validated prompt handle with a hard byte ceiling. A file can grow
/// past the cap between the size check above and the read; `take` bounds the
/// read to cap + 1, and a read that actually hit the bound is refused by name
/// instead of silently truncating the prompt. Invalid UTF-8 is refused by name
/// at the byte offset, never lossily decoded.
fn read_prompt_handle(file: std::fs::File, rel: &str) -> std::result::Result<String, String> {
    let mut reader = file.take(PROMPT_FILE_CAP + 1);
    let mut buf = Vec::new();
    std::io::Read::read_to_end(&mut reader, &mut buf)
        .map_err(|e| format!("prompt_file `{rel}` refused: {e}"))?;
    if buf.len() > PROMPT_FILE_CAP as usize {
        return Err(format!(
            "prompt_file `{rel}` refused: grew past the {PROMPT_FILE_CAP} byte cap during the read"
        ));
    }
    let text = std::str::from_utf8(&buf).map_err(|e| {
        format!(
            "prompt_file `{rel}` refused: invalid UTF-8 at byte offset {}",
            e.valid_up_to()
        )
    })?;
    Ok(text.to_string())
}

/// A reserved background job: the spec every later write of it goes through,
/// and the registry entry a cancel reaches it through.
///
/// The two travel together because they are minted together: an id in the
/// caller's hands with no entry behind it is a cancel that silently does
/// nothing. Dropping this removes the registry entry and NOTHING else — the
/// `running` file it also minted outlives the drop, which is why giving a
/// reservation up is [`Self::abandon`] rather than a `drop`.
struct ReservedJob {
    spec: jobs::RunningSpec,
    cancel: CancelGuard,
}

impl ReservedJob {
    /// Give a reservation up: remove the `running` file it minted, then release
    /// the registry entry with the guard.
    ///
    /// A consuming method rather than a `Drop` impl because the success path
    /// destructures this struct and Rust refuses to destructure a `Drop` type
    /// (E0509). It exists so the undo lives ON the reservation instead of as a
    /// sweep some other function remembers to run over its ids.
    fn abandon(self) {
        jobs::remove(&self.spec.job_id);
    }
}

/// What one job record is minted FROM: the run's identity plus the raw deadlines
/// `resolve_deadlines` folds. Kept together so a reservation can be minted
/// somewhere other than where its run started.
///
/// `started_at` is carried rather than re-read at the mint. A blocking run handed
/// off mid-flight has been going since long before its file existed, and a fresh
/// stamp would report it as brand new on every `monitor` check while resetting
/// the retention anchor with it.
#[derive(Clone)]
struct MintSpec {
    profile: String,
    started_at: u64,
    timeout_secs: Option<u64>,
    idle_secs: Option<u64>,
    streaming: bool,
    /// The call's resolved endpoint, so a record minted at the hand-off carries
    /// the same answer the blocking reply folded with.
    endpoint: Option<String>,
    /// The call's resolved serving provider, carried for the same reason: the
    /// hand-off mint and the blocking reply must name the same provider.
    provider: Option<String>,
    /// Whether the run launches isolated, so a record minted at the hand-off
    /// carries the same answer the run itself launched under.
    isolation: Isolation,
}

/// Record ONE background job's `running` file and return the reservation. This
/// is the only fallible step left after the pre-flight refusal; the spawn that
/// follows cannot fail, so a fan-out reserves every job before launching any.
///
/// The deadlines are resolved HERE so the first running record already carries
/// them and `resolve_deadlines`' streaming fork is applied exactly once.
///
/// The cancel entry is minted HERE, with the id, rather than when the blocking
/// pool picks the task up: the caller holds the id the moment this returns, and
/// a cancel naming it in between used to be a silent no-op that left the run to
/// whichever deadline it launched under — and the default streaming shape has no
/// wall clock at all. A `write_running` failure drops the reservation with the
/// entry, so an id that never reached the caller leaves nothing behind.
fn reserve_background_job(
    profile: &str,
    timeout_secs: Option<u64>,
    idle_secs: Option<u64>,
    streaming: bool,
    endpoint: Option<String>,
    provider: Option<String>,
    isolation: Isolation,
) -> std::result::Result<ReservedJob, String> {
    reserve_job(
        &MintSpec {
            profile: profile.to_string(),
            started_at: now_ms(),
            timeout_secs,
            idle_secs,
            streaming,
            endpoint,
            provider,
            isolation,
        },
        std::sync::Arc::new(AtomicBool::new(false)),
    )
}

/// The minting itself, shared by the reserve above and by a blocking run's spawn
/// — which reaches it with a `started_at` from before the mint, and which must
/// NOT re-run the pre-flight or the runtime acquire that run is already past.
///
/// [`resolve_deadlines`] runs HERE and nowhere else per run, so its streaming
/// fork is applied exactly once and every later write of the record inherits the
/// same answer.
fn mint_spec(mint: &MintSpec, kind: jobs::RecordKind) -> jobs::RunningSpec {
    let (wall, idle) = resolve_deadlines(mint.timeout_secs, mint.idle_secs, mint.streaming);
    jobs::RunningSpec {
        job_id: jobs::new_job_id(mint.started_at),
        profile: mint.profile.clone(),
        started_at: mint.started_at,
        // The record's own age, which is the run's only for a job that started
        // out background. A blocking run mints at its SPAWN, which is after the
        // pre-flight and the runtime acquire, and anchoring its retention on
        // `started_at` would spend that whole delay out of its silence budget.
        recorded_at: now_ms(),
        // `0` = no wall clock, which is what a streaming run launches under.
        // Paired with the `idle_secs` below it stays distinguishable from a
        // record an older server wrote, which carries neither.
        timeout_secs: wall.map_or(0, |w| w.as_secs()),
        // Without the event stream the idle leg is off entirely, so there is no
        // such deadline to count down to rather than an unknown one.
        idle_secs: mint.streaming.then_some(idle.as_secs()),
        endpoint: mint.endpoint.clone(),
        provider: mint.provider.clone(),
        isolated: mint.isolation == Isolation::Isolated,
        kind,
    }
}

/// Mint a COLLECTABLE record plus its cancel entry: the background shape.
fn reserve_job(
    mint: &MintSpec,
    cancel: std::sync::Arc<AtomicBool>,
) -> std::result::Result<ReservedJob, String> {
    let spec = mint_spec(mint, jobs::RecordKind::Collectable);
    let cancel = CancelGuard::register(&spec.job_id, cancel);
    jobs::write_running(&spec).map_err(|e| format!("failed to record job: {e}"))?;
    Ok(ReservedJob { spec, cancel })
}

/// Where one delegate's result goes, and who may still change that answer.
///
/// A BACKGROUND run starts on the far side of it: `reserve_background_job` minted
/// its file before the task existed, so it heartbeats and finalizes into that
/// file from its first line and nothing here ever moves. A BLOCKING run starts on
/// the near side, its handler holding the join, and CROSSES when that caller goes
/// away: the handler mints a reservation for the run already in flight, hands
/// back the id, and the run carries on as an ordinary background job — same
/// heartbeats, same cancel registry, and resolvable through `monitor` BY ANYONE
/// WHO KNOWS THE ID. Nothing currently tells the model that id: the reply
/// carrying it is dropped as a cancelled request's, and the bundled
/// `PostToolUse` hook does not fire for a cancelled call (see [`jobs`]). What
/// this buys today is that the spent window's result exists at all — before it,
/// abandoning a blocking call left a child spending a window whose result was
/// dropped with the handler's future, bounded only by the idle guard.
struct Handoff {
    /// The flag `run_delegate` reads each tick — held here from the start, and
    /// REGISTERED (under the id minted at the crossing) only there. A blocking
    /// run has no id to name until then, so nothing can reach it, and it ends up
    /// with exactly one registry entry either way.
    cancel: std::sync::Arc<AtomicBool>,
    /// A LEAF, matching [`CANCEL_REGISTRY`]'s posture: never acquire another
    /// lock while holding it, which is why [`Handoff::hand_off`] mints outside
    /// it and why [`Handoff::finalize`] writes outside it.
    state: std::sync::Mutex<HandoffState>,
}

/// Which side of a [`Handoff`] a run is on.
enum HandoffState {
    /// A caller still holds the join and takes the envelope from there.
    Attached(AttachedRun),
    /// The run owns this reservation and finalizes into it.
    Converted(ReservedJob),
    /// The run is over; there is nothing left to hand off.
    Finished,
}

/// A run whose caller is still waiting.
struct AttachedRun {
    /// What a mint needs, carried because the run is long past the code that
    /// resolved it.
    mint: MintSpec,
    /// The liveness record, `None` until a child exists.
    ///
    /// It doubles as the "has this run spawned" fact, which is what makes the
    /// two impossible to disagree: before the spawn nothing has been spent, so
    /// an abandoned call STOPS the run instead of minting a job to collect a
    /// window nobody paid for, and that decision reads the same field the record
    /// lives in. Installed under the state lock, so a `hand_off` racing the
    /// spawn either sees no record and cancels — killing a child ~50 ms in, the
    /// same loss the pre-spawn arm already reports — or sees one and crosses.
    live: Option<jobs::RunningSpec>,
}

/// What became of a run whose caller went away.
enum Abandoned {
    /// It owns a job file under this id now: the wait ends and the task finishes
    /// detached.
    HandedOff(String),
    /// Nothing was handed off — no child exists yet (so nothing was spent, and
    /// the run is being stopped instead), the run finished first, or the mint
    /// failed. Whatever comes back through the join is still the waiter's.
    Kept,
}

impl Handoff {
    /// A blocking run's seam: nothing minted, nothing registered, a caller
    /// holding the join.
    fn blocking(mint: MintSpec) -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self {
            cancel: std::sync::Arc::new(AtomicBool::new(false)),
            state: std::sync::Mutex::new(HandoffState::Attached(AttachedRun { mint, live: None })),
        })
    }

    /// A background run's seam: already across it, reading the reservation's own
    /// flag so the entry minted with the id is the one this run consults.
    fn reserved(job: ReservedJob) -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self {
            cancel: std::sync::Arc::clone(&job.cancel.flag),
            state: std::sync::Mutex::new(HandoffState::Converted(job)),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HandoffState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// A child exists: from here an abandoned caller hands this run off rather
    /// than stopping it, and the operator can see the run.
    ///
    /// The liveness record is minted HERE rather than at construction because
    /// this is the boundary a child exists at — the same one [`Self::hand_off`]
    /// already reads. A run refused by the pre-flight, or still blocked inside
    /// `ProfileRuntime::acquire`, has spent nothing and must leave no file
    /// behind to say otherwise.
    ///
    /// A background run reaches here already `Converted`, its record minted at
    /// the reserve, and mints nothing.
    fn mark_spawned(&self) {
        let spec = {
            let mut state = self.lock();
            match &mut *state {
                HandoffState::Attached(run) if run.live.is_none() => {
                    let spec = mint_spec(&run.mint, jobs::RecordKind::Liveness);
                    run.live = Some(spec.clone());
                    Some(spec)
                }
                HandoffState::Attached(_) | HandoffState::Converted(_) | HandoffState::Finished => {
                    None
                }
            }
        };
        // Outside the lock: this leaf is never held across IO. Best-effort — a
        // failed write costs the operator sight of this run, and `hand_off`'s
        // own write fallback still preserves its result.
        if let Some(spec) = spec {
            let _ = jobs::write_running(&spec);
        }
    }

    /// Whether this run has been asked to stop.
    fn is_cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    /// The record this run heartbeats into, `None` while it has none. An
    /// attached run answers with its liveness record from the spawn on, which is
    /// what puts a blocking delegate's heartbeat on disk at all.
    fn spec(&self) -> Option<jobs::RunningSpec> {
        match &*self.lock() {
            HandoffState::Converted(job) => Some(job.spec.clone()),
            HandoffState::Attached(run) => run.live.clone(),
            HandoffState::Finished => None,
        }
    }

    /// The caller went away. Hand the run off to a job file if it has already
    /// started spending, and stop it if it has not.
    fn hand_off(&self) -> Abandoned {
        let live = match &*self.lock() {
            HandoffState::Attached(run) => run.live.clone(),
            // Already across (a background run), or over.
            HandoffState::Converted(_) | HandoffState::Finished => return Abandoned::Kept,
        };
        // The same boundary `run_delegate` reads right after
        // `ProfileRuntime::acquire` returns, from the other side: with no child
        // there is nothing to collect and nothing was billed, so a job file
        // would only promise a result that is never coming.
        let Some(live) = live else {
            self.cancel.store(true, Ordering::Relaxed);
            return Abandoned::Kept;
        };
        // The run keeps its own id across the crossing; only the spelling moves.
        let spec = jobs::RunningSpec {
            kind: jobs::RecordKind::Collectable,
            ..live
        };
        // Registered OUTSIDE the state lock, because `CancelGuard::register`
        // takes CANCEL_REGISTRY's: both stay TRUE leaves only while neither is
        // ever held across the other. What that costs is the window `install`
        // exists to close.
        let cancel = CancelGuard::register(&spec.job_id, std::sync::Arc::clone(&self.cancel));
        match jobs::promote(&spec) {
            Ok(()) => self.install(ReservedJob { spec, cancel }),
            Err(reason) => {
                logline!("clauth: delegate hand-off failed, its result is lost: {reason}");
                Abandoned::Kept
            }
        }
    }

    /// Give a freshly minted reservation to the run, or give the reservation up
    /// because the run landed while it was being minted.
    ///
    /// Its own function because that second arm is a pure race — [`Self::hand_off`]
    /// re-reads the state first, so nothing single-threaded reaches it through
    /// there — and a branch no test can enter is a branch that rots. What it
    /// decides is the whole of what the race costs: a `running` record for a run
    /// that will never finalize, versus none.
    fn install(&self, reserved: ReservedJob) -> Abandoned {
        let mut state = self.lock();
        match &*state {
            HandoffState::Attached(_) => {
                let job_id = reserved.spec.job_id.clone();
                *state = HandoffState::Converted(reserved);
                Abandoned::HandedOff(job_id)
            }
            // The run's envelope is already on its way back through the join,
            // to a caller who has gone — so it is lost either way, and the
            // record just minted for it would only sit `running` until GC. Give
            // the reservation up, and say so for the same reason the failed-mint
            // arm does: a spent window's result went nowhere.
            HandoffState::Converted(_) | HandoffState::Finished => {
                let job_id = reserved.spec.job_id.clone();
                drop(state);
                reserved.abandon();
                logline!(
                    "clauth: delegate landed while `{job_id}` was being minted; its result is lost"
                );
                Abandoned::Kept
            }
        }
    }

    /// The run is over: write its envelope into the job file, if it owns one.
    ///
    /// Nothing can heartbeat past this point on either shape — `run_delegate`
    /// joins the stdout reader before it returns — so the last heartbeat
    /// strictly precedes this write.
    fn finalize(&self, envelope: &serde_json::Value) {
        // The old state is bound OUT of the guard's scope before anything drops
        // it, rather than being dropped as a `mem::replace` temporary while the
        // guard is still alive. That is free today, since no remaining variant
        // owns a `CancelGuard` — but the day one does, dropping it under this
        // lock would nest `CANCEL_REGISTRY` inside `state` with no rank to catch
        // it, which is precisely the two-lock order both leaves exist to avoid.
        let previous = {
            let mut state = self.lock();
            std::mem::replace(&mut *state, HandoffState::Finished)
        };
        let owned = match previous {
            HandoffState::Converted(job) => Some(job),
            // A caller is still holding the join and takes the envelope from
            // there, so this run's liveness record has no result left to offer.
            // Leaving it would advertise a job nothing will ever collect, and
            // writing a `done` record into it would deliver one result twice.
            // Outside the lock, like every other write here.
            HandoffState::Attached(run) => {
                if let Some(spec) = run.live {
                    jobs::remove_liveness(&spec.job_id);
                }
                None
            }
            HandoffState::Finished => {
                debug_assert!(false, "one finalize per run");
                None
            }
        };
        // Outside the lock: this leaf is never held across IO.
        if let Some(ReservedJob { spec, cancel }) = owned {
            // Any liveness record still standing under this id is provably an
            // orphan, and clearing it here is what makes the crossing safe at
            // all. The rename in `promote` is atomic, but the WRITER racing it
            // is not bounded by it: the stdout reader resolves `spec()` and only
            // then does its IO, so a beat that resolved the liveness spelling
            // lands after the rename and recreates the file. `mark_spawned` has
            // the same shape, installing the spec under the lock and writing
            // outside it. Neither window can be closed by ordering the rename.
            // What closes both is WHEN this runs: `run_delegate` joins the
            // reader thread before it returns and this is called after it does,
            // so no beat exists to lose a second race to. A no-op for a run that
            // started out background and never had the spelling.
            jobs::remove_liveness(&spec.job_id);
            let _ = jobs::write_done(
                &spec.job_id,
                &spec.profile,
                spec.started_at,
                spec.endpoint.clone(),
                spec.provider.clone(),
                spec.isolated,
                envelope.clone(),
            );
            // Deregistered only now, AFTER the result is on disk: a cancel
            // landing in the finalize window is answered by the very envelope
            // the collect then returns, where dropping the entry first would
            // answer it with the unheld hedge instead.
            drop(cancel);
        }
    }
}

/// Launch ONE background delegate on the blocking pool for a reservation.
/// Infallible: `spawn_blocking` cannot fail, so every failure path lives in
/// [`reserve_background_job`].
///
/// The reservation's cancel entry moves into the detached task and is dropped
/// there, AFTER the job file is finalized.
fn launch_background_delegate(
    profile: String,
    opts: BackgroundOpts,
    reserved: ReservedJob,
    herdr_pane: Option<herdr_report::PaneReporter>,
) {
    // Dropping the handle DETACHES the task rather than stopping it, which is
    // what a background job wants: its result lands in its file, and nothing
    // here waits for it.
    drop(spawn_delegate(
        profile,
        opts,
        Handoff::reserved(reserved),
        herdr_pane,
    ));
}

/// Run ONE delegate on the blocking pool against a [`Handoff`], and route its
/// envelope both ways: into the job file when the run owns one, and back through
/// the returned handle for a caller still waiting on it. `opts.prompt` is an
/// `Arc<str>` so a fan-out reads the prompt once and reuses it across N accounts.
///
/// One spawn for both shapes, because a blocking run is only a background one
/// whose caller has not left yet: after the hand-off landed, keeping two spawns
/// meant two panic guards, two finalizes and two teardown orders to hold in step.
fn spawn_delegate(
    profile: String,
    opts: BackgroundOpts,
    handoff: std::sync::Arc<Handoff>,
    herdr_pane: Option<herdr_report::PaneReporter>,
) -> tokio::task::JoinHandle<serde_json::Value> {
    let profile_task = profile;
    // Registered so a test's `HomeSandbox::drop` can block on this task BEFORE
    // it clears the home override: a background task detaches with no handle
    // kept, and an abandoned blocking one outlives the handler that held its
    // handle, so neither is joinable by a sandbox teardown. A task still running
    // when the override clears resolves the operator's REAL `$HOME` (filed
    // 2026-08-14, F1).
    #[cfg(test)]
    let done_tx = crate::testutil::register_background_task();
    tokio::task::spawn_blocking(move || {
        // Test-only: block here if a test armed the start gate, forcing this
        // task to still be in flight at the moment its `HomeSandbox` drops
        // instead of racing tokio's blocking-pool scheduler for that timing.
        #[cfg(test)]
        detach_test_gate();
        // The decrement-and-`idle` rule, and the created-first placement, are
        // `herdr_report::InFlightGuard`'s. What this site adds is the
        // placement reason: the guard sits in the TASK rather than beside the
        // handler's `begin` because a handed-off delegate is still spending
        // after its caller left, and a pane reading `idle` under a live run is
        // wrong in the direction that matters.
        let _pane_end = herdr_pane.map(herdr_report::InFlightGuard::end_only);
        // Catch a panic in the task: its handle may be dropped (a background
        // run, or a blocking one handed off), so an unwind would otherwise be
        // swallowed and leave the job stuck `running` until GC — the waiter
        // would hang on its deadline. The job file is always finalized.
        let BackgroundOpts {
            prompt,
            model,
            cwd,
            env,
            extra_args,
            timeout_secs,
            idle_secs,
            resume,
            isolation,
            depth,
        } = opts;
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            run_delegate(DelegateOpts {
                profile: &profile_task,
                prompt: prompt.as_ref(),
                model: model.as_deref(),
                cwd: cwd.as_deref(),
                env,
                extra_args,
                timeout_secs,
                idle_secs,
                resume: resume.as_deref(),
                isolation,
                depth,
                handoff: Some(std::sync::Arc::clone(&handoff)),
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
        // `run_delegate` has returned, so it has already joined the reader
        // thread: the last heartbeat strictly precedes this finalize. A run
        // still attached to a waiting caller writes nothing and hands the
        // envelope back below instead.
        handoff.finalize(&envelope);
        // Dropped explicitly so the completion signal below is genuinely this
        // task's last action. A guard bound in the closure drops in reverse
        // declaration order, i.e. AFTER the send, which would let a test's
        // teardown clear the home override while this one still runs. Harmless
        // for THIS guard, whose report shells out and reads the process env
        // rather than clauth's override — but the registry's contract is that
        // nothing touching `$HOME` outlives the send, and a guard that silently
        // sits outside it is how that contract goes false later.
        drop(_pane_end);
        #[cfg(test)]
        let _ = done_tx.send(());
        envelope
    })
}

/// Test-only start gate for the NEXT detached background task: once armed,
/// blocks that task at the top of its closure until the test releases it.
/// The only way to prove `HomeSandbox` teardown ordering without racing
/// tokio's blocking-pool scheduler — an unforced test is green by luck, since
/// the task usually hasn't even started by the time a sandbox drops. Never
/// compiled into the binary.
#[cfg(test)]
static DETACH_START_GATE: std::sync::Mutex<Option<std::sync::mpsc::Receiver<()>>> =
    std::sync::Mutex::new(None);

/// Arm the gate; returns the sender the test releases it with. A single
/// global slot shared by every test, so callers must hold
/// `profile::HOME_TEST_LOCK` (via a live `HomeSandbox`) for as long as it
/// stays armed — otherwise an unrelated test's own background task could be
/// the one that gets gated.
#[cfg(test)]
fn arm_detach_gate() -> std::sync::mpsc::Sender<()> {
    let (tx, rx) = std::sync::mpsc::channel();
    *DETACH_START_GATE.lock().unwrap_or_else(|e| e.into_inner()) = Some(rx);
    tx
}

/// Block if a test armed [`arm_detach_gate`]; a no-op otherwise (production,
/// or a test that never arms it).
///
/// Bounded, and it says so when the bound fires. A test that arms the gate and
/// then dies before releasing it would otherwise park this task forever, and
/// the sandbox teardown waiting on the task turns that into a CI run that times
/// out naming nothing. Releasing ourselves after the bound lets the run reach
/// its real assertion instead. The number is a hang detector: the only caller
/// releases within ~100ms.
#[cfg(test)]
fn detach_test_gate() {
    detach_test_gate_with(DETACH_GATE_TIMEOUT);
}

/// How long [`detach_test_gate`] waits for its release. A hang detector, not a
/// race bound: the only caller releases within ~100ms.
#[cfg(test)]
const DETACH_GATE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

/// [`detach_test_gate`] against a caller-supplied bound, so the timeout branch
/// can be exercised without a test that waits out the real one.
#[cfg(test)]
fn detach_test_gate_with(timeout: std::time::Duration) {
    let armed = DETACH_START_GATE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take();
    if let Some(rx) = armed
        && let Err(std::sync::mpsc::RecvTimeoutError::Timeout) = rx.recv_timeout(timeout)
    {
        crate::out::errln!(
            "clauth: the detach start gate was never released ({timeout:?}) — a test armed \
             `arm_detach_gate` and did not send; proceeding so the run fails on its own \
             assertion instead of hanging"
        );
    }
}

/// Reduce `claude`'s captured stdout to its single terminal `type:"result"`
/// envelope. Under clauth's own `stream-json` the reader already retained just
/// that line, but a caller-pinned `--output-format json` emits the bare object
/// and a `--verbose` one the full transcript ARRAY (every `system`
/// thinking-token / tool-io / `assistant` event) — valid input that would
/// otherwise be stored and dumped into the caller's context verbatim (a
/// multi-minute run leaks ~1000x the envelope). Collapse all three to the
/// terminal result object so the delegate envelope stays the documented shape
/// regardless of caller `args`.
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
    crate::throughput::record_success(
        &ProfileName::from(profile),
        model,
        output_tokens,
        duration_ms,
        now,
    );
}

/// What a delegate-output scan found: whether it carries a rate-limit / 429
/// signature, and any Retry-After hint it named.
enum RateLimit {
    /// No rate-limit / 429 signature.
    No,
    /// Rate-limited. `retry_after_s` is `None` when the output carried no
    /// Retry-After hint.
    Yes { retry_after_s: Option<u64> },
}

/// Detect a rate-limit / 429 signature in a delegate's output.
fn rate_limit_hint(text: &str) -> RateLimit {
    let lower = text.to_lowercase();
    let limited = lower.contains("rate limit")
        || lower.contains("rate_limit")
        || lower.contains("429")
        || lower.contains("overloaded");
    if !limited {
        return RateLimit::No;
    }
    let retry_after = lower.find("retry").and_then(|i| {
        lower[i..]
            .split(|c: char| !c.is_ascii_digit())
            .find(|s| !s.is_empty())
            .and_then(|s| s.parse::<u64>().ok())
    });
    RateLimit::Yes {
        retry_after_s: retry_after,
    }
}

/// One-line throughput warning folded into a delegate payload's `live_usage`
/// object, or `None` when nothing is degraded or rate-limited.
fn throughput_note(profile: &str, now: i64) -> Option<String> {
    let flagged: Vec<String> = crate::throughput::summary(&ProfileName::from(profile), now)
        .into_iter()
        .filter(|m| m.degraded || m.rate_limited_recent)
        .map(|m| {
            let name = match model_display_name(&m.model) {
                Some(name) => format!("{name} "),
                None => String::new(),
            };
            if m.rate_limited_recent {
                match m.retry_after_s {
                    Some(s) => format!("{name}rate-limited (retry ~{s}s)"),
                    None => format!("{name}rate-limited"),
                }
            } else {
                format!("{name}slow (~{:.0} tok/s)", m.tok_s)
            }
        })
        .collect();
    (!flagged.is_empty()).then(|| format!("⚠ {}", flagged.join(", ")))
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

/// How long a client may treat `server/discover` and `tools/list` as fresh. Both
/// are fixed for the process — the tool set is compile-time and the instructions
/// block is built once at startup — so a cached copy is never staler than the
/// server's own. rmcp defaults both to `0`, which makes a conforming client
/// re-fetch on every use.
const CACHE_TTL_MS: u64 = 5 * 60 * 1000;

// `router = self.tool_router` dispatches from the stored router. Left off, the
// macro's default rebuilds `Self::tool_router()` on every call.
#[tool_handler(router = self.tool_router)]
impl ServerHandler for ClauthServer {
    fn get_info(&self) -> ServerInfo {
        // Both of these are wrong by default. Empty capabilities make a
        // spec-compliant client (Claude Code) expose no tools at all, even
        // though the server still answers a forced `tools/list`; and rmcp's
        // default `Implementation` reads its OWN build env, so the server
        // introduces itself to every client as "rmcp".
        //
        // The protocol version stays at rmcp's default. It is only the fallback
        // for an `initialize` caller asking for a revision this SDK does not
        // know — a legacy client, which a 2026-07-28 answer would break —
        // while `server/discover` advertises the full supported set instead.
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                env!("CARGO_PKG_NAME"),
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(build_instructions())
    }

    async fn discover(
        &self,
        _context: RequestContext<RoleServer>,
    ) -> Result<DiscoverResult, ErrorData> {
        Ok(DiscoverResult::from_server_info(
            self.supported_protocol_versions().into_owned(),
            self.get_info(),
        )
        .with_ttl_ms(CACHE_TTL_MS)
        // The instructions block names the operator's profiles, so a cached
        // copy must not cross an authorization context.
        .with_cache_scope(CacheScope::Private))
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let result = ListToolsResult::with_all_items(self.tool_router.list_all());
        // Cache hints arrived with 2026-07-28; a legacy peer gets the old shape.
        let hinted = context
            .protocol_version()
            .is_some_and(|v| v >= ProtocolVersion::V_2026_07_28);
        Ok(if hinted {
            result
                .with_ttl_ms(CACHE_TTL_MS)
                .with_cache_scope(CacheScope::Public)
        } else {
            result
        })
    }
}

/// Build the init-time `instructions` block once from the on-demand config and
/// usage disk cache. Best-effort: a config load failure degrades to a prose-only
/// block rather than failing the handshake.
fn build_instructions() -> String {
    let Ok(config) = load_config() else {
        return "clauth manages multiple Claude Code accounts (\"profiles\"). \
            Call `profiles` for live usage figures."
            .to_string();
    };
    let snapshots: Vec<ProfileSnapshot> = config
        .profiles
        .iter()
        .map(|p| ProfileSnapshot {
            name: p.name.to_string(),
            active: config.is_active(&p.name),
            provider: provider_label(p),
            base_url: p.base_url.clone(),
            sub_type: tier_label(p),
            rank: roster_rank(&p.name),
        })
        .collect();

    let auth = crate::which::session_auth();
    let probe = crate::runtime::link_mode_of(crate::which::session_config_dir().as_deref());
    render::instructions_block(&snapshots, &auth, probe)
}

/// Whether this `clauth mcp` process should hold a bare-session marker. Pure, so
/// both refusals are exercised without an env or a spawn.
///
/// `Global` is the whole signal: a server reading the global `~/.claude`
/// credentials is the MCP half of a bare `claude`, while every isolated tier
/// reads its own file — a supervised `clauth start` session, already registered,
/// or a `delegate` child, which gets `CLAUDE_CONFIG_DIR` in the same builder as
/// its depth marker and so needs no depth check of its own here.
fn bare_marker_wanted(auth: &crate::which::SessionAuth, is_probe: bool) -> bool {
    matches!(auth, crate::which::SessionAuth::Global) && !is_probe
}

/// This server's bare-session marker, or `None` when the process is not a bare
/// session or the registration failed. A failure is logged and never fatal: the
/// tally is a display feature riding on the MCP server, and a broken count must
/// not take the server down.
fn hold_bare_session_marker() -> Option<std::fs::File> {
    let is_probe = std::env::var_os(MCP_PROBE_ENV).is_some();
    if !bare_marker_wanted(&crate::which::session_auth(), is_probe) {
        return None;
    }
    match crate::runtime::register_bare_session() {
        Ok(file) => Some(file),
        Err(e) => {
            logline!("clauth: bare-session marker not registered: {e:#}");
            None
        }
    }
}

/// `serve`'s pre-handshake work, split out so a test can drive it without standing
/// up the stdio transport. Returns the bare-session marker because `serve` has to
/// hold it across `block_on`.
fn startup() -> Option<std::fs::File> {
    crate::runtime::gc_stale_runtimes();
    jobs::gc(now_ms());
    // Converge a broken plugin registration without ever blocking the stdio
    // handshake: the gate is two registry reads inline, and a needed heal runs
    // on its own thread (throttled inside `heal_detached`), never on stdout.
    //
    // Not under the Plugin tab's boot probe, which spawns a real `clauth mcp`
    // and kills it within seconds: a heal started there is a mutating lifecycle
    // call the tab never confirmed, torn off mid-sequence, with the `claude`
    // grandchild left to finish its registry write unsignalled.
    if std::env::var_os(MCP_PROBE_ENV).is_none() {
        crate::plugin_host::heal_detached();
    }
    // Held across `block_on`, so the flock drops with the process however it dies
    // — a bare `claude` runs no clauth teardown, SIGKILL least of all.
    hold_bare_session_marker()
}

pub(crate) fn serve() -> Result<()> {
    let _bare_marker = startup();
    // The delegate-dot knob, read once at startup from the on-demand config.
    // A missing or unreadable profiles.toml answers the default (dot on), so
    // the knob can never fail the server.
    let delegate_dot = load_config()
        .map(|config| config.state.herdr.delegate_dot)
        .unwrap_or_else(|_| crate::profile::HerdrSettings::default().delegate_dot);
    // rmcp's service loop arms a Tokio timer (needs `enable_time`), so a bare
    // current-thread runtime panics right after the first reply. `enable_all`
    // also turns on the I/O driver, covering a future transport that polls a real
    // fd or any added tokio net/process path.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    rt.block_on(run_server(delegate_dot))
}

async fn run_server(delegate_dot: bool) -> Result<()> {
    use rmcp::{ServiceExt, transport::stdio};
    // Resolve the pane reporter once, at startup: the pane env is what this
    // process inherited from herdr, and a per-call re-read would race a
    // delegate with a changed environment.
    let server =
        ClauthServer::new().with_herdr_pane(herdr_report::PaneReporter::resolve(delegate_dot));
    let service = server.serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}

#[cfg(test)]
#[path = "../../tests/inline/mcp_run.rs"]
mod tests;

#[cfg(test)]
#[path = "../../tests/inline/mcp_switch_tool.rs"]
mod switch_tool_tests;

#[cfg(test)]
#[path = "../../tests/inline/mcp_profiles_tool.rs"]
mod profiles_tool_tests;

#[cfg(test)]
#[path = "../../tests/inline/mcp_format.rs"]
mod format_tests;

#[cfg(test)]
#[path = "../../tests/inline/mcp_delegate_args.rs"]
mod delegate_args_tests;

#[cfg(test)]
#[path = "../../tests/inline/mcp_herdr_report.rs"]
mod herdr_report_tests;

#[cfg(test)]
#[path = "../../tests/inline/mcp_digest.rs"]
mod digest_tests;

#[cfg(test)]
#[path = "../../tests/inline/mcp_background_sandbox.rs"]
mod background_sandbox_tests;

#[cfg(test)]
#[path = "../../tests/inline/mcp_startup.rs"]
mod startup_tests;
