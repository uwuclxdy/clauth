//! Uncollected-results leg of `clauth hook-profile-changed-note`: on a
//! `SessionStart` resume or compaction, names the background delegates this
//! conversation started whose results still sit in the job store, riding the
//! envelope the other legs share.
//!
//! The `mcp-await-job` delivery hook is bound to the process that spawned the
//! job, so a result still uncollected when that session exited waits for the
//! resumed session's first turn end, and an idle resume never reaches one. A
//! compaction drops whatever a resume injected, so it names them again. The
//! store is the ledger of what is unread: both delivery paths evict a record
//! through `jobs::claim`, so a collected result is never named, and a record
//! the startup sweep is about to reap is never offered.
//!
//! Matched on `host_session` alone, strictly — the same untied-record rule
//! `hook_context`'s `spawned_here` applies: a record with no host is some
//! peer's, never this conversation's. The pid is no use either: a resume is a
//! new process, so a record's `host_pid` names the dead one. A fork gets a
//! new session id no record carries, so it is never told.
//!
//! The session half of the delivery hook lives here too. A hook ends when it
//! wakes, so `Stop` starts a watcher at every turn end, and four facts keep
//! that one process per session, one idle wake per idle hour, and one
//! lost-job wake per lost job: the watched set ([`own_jobs`]), the idle
//! count, which rides this conversation's own record ([`mark_turn_end`],
//! [`note_turn_start`]), the lease ([`WatchLease`]), and the reported-lost
//! set, which rides a sidecar beside the record, never the lease file's
//! content ([`record_lost`]).

use std::fs::{File, OpenOptions, TryLockError};
use std::path::Path;

use anyhow::Result;
use serde::Deserialize;

use crate::hook_note::{Payload, ScopeLock, load_record, record_path, store_record};
use crate::mcp::jobs::{self, JobPhase, RecordKind, StoredJob};

/// The most job ids one note names; the rest are counted. Bounds the injected
/// context on a fan-out's worth of results.
const NAMED_MAX: usize = 10;

pub(crate) fn note(payload: &Payload) -> Option<String> {
    note_at(payload, crate::usage::now_ms())
}

fn note_at(payload: &Payload, now: u64) -> Option<String> {
    if payload.event != "SessionStart"
        || !matches!(payload.source.as_deref(), Some("resume" | "compact"))
        || payload.agent_id.is_some()
    {
        return None;
    }
    let waiting: Vec<(String, String)> = jobs::list(now)
        .into_iter()
        .filter(|job| {
            job.phase() == JobPhase::Done
                && !jobs::done_is_expired(&job.record, now)
                && job.record.host_session.as_deref() == Some(payload.session_id.as_str())
        })
        .map(|job| (job.record.job_id, job.record.profile))
        .collect();
    render(&waiting)
}

/// The copy over `(job_id, profile)` pairs in the store's newest-first order.
fn render(waiting: &[(String, String)]) -> Option<String> {
    if waiting.is_empty() {
        return None;
    }
    let mut named = waiting
        .iter()
        .take(NAMED_MAX)
        .map(|(id, profile)| format!("`{id}` on `{profile}`"))
        .collect::<Vec<_>>()
        .join(", ");
    let rest = waiting.len().saturating_sub(NAMED_MAX);
    if rest > 0 {
        named.push_str(&format!(", and {rest} more (`clauth jobs` lists them)"));
    }
    Some(if waiting.len() == 1 {
        format!(
            "clauth note: a background delegate this session started has a result waiting uncollected: {named}. `monitor` with its job_id collects it."
        )
    } else {
        format!(
            "clauth note: background delegates this session started have results waiting uncollected: {named}. `monitor` with their job_ids collects them."
        )
    })
}

/// The jobs a session's watcher answers for: its running records, a finished
/// one no hook delivered, and an orphaned one whose lost-result wake it must
/// carry. The second half is what keeps an idle wake's promise, since the wake
/// ends its watcher and a job finishing inside the wake's own turn has nothing
/// else to deliver it; an orphan a watcher never adopts is a lost result no
/// wake ever reports.
///
/// One store read and nothing else: this runs at every turn end of every
/// install, and an empty answer ends the hook.
pub(crate) fn own_jobs(session_id: &str, now: u64) -> Vec<String> {
    jobs::list(now)
        .into_iter()
        .filter(|job| is_awaited(job, session_id, now))
        .map(|job| job.record.job_id)
        // The ids come from the store's own files, which the user's uid can
        // edit, so a record is not trusted input: only a safe id may reach
        // `jobs::read` and `jobs::claim`.
        .filter(|id| jobs::is_safe_job_id(id))
        .collect()
}

fn is_awaited(job: &StoredJob, session_id: &str, now: u64) -> bool {
    job.kind == RecordKind::Collectable
        && job.record.host_session.as_deref() == Some(session_id)
        && match job.phase() {
            // Done keeps a result to deliver; Orphaned carries the lost-result
            // wake. The reported-lost set keeps a later turn end that adopts
            // the orphan again from waking twice.
            JobPhase::Done | JobPhase::Orphaned => true,
            // A running record silent past the store's own corpse window is
            // dead to the watcher as it is to the sweep, owned or ownerless,
            // so a hung job's wakes stop with it instead of repeating forever.
            JobPhase::Running => !jobs::running_is_silent(&job.record, now),
            JobPhase::Blocking => false,
        }
}

/// Whether `job_id` is a job [`own_jobs`] would find for this session: a hook
/// that names one may leave it to the session's watcher.
pub(crate) fn is_session_job(job_id: &str, session_id: &str) -> bool {
    jobs::read(job_id).is_some_and(|r| r.host_session.as_deref() == Some(session_id))
}

/// Extension of a [`WatchLease`] file, sitting beside the conversation's record.
pub(crate) const WATCH_LEASE_EXTENSION: &str = "watch";

/// Extension of the reported-lost sidecar, beside the conversation's record.
/// Its own file, never the lease file's content: Windows locks are mandatory
/// (`LockFileEx`), so a second watcher's handle cannot read bytes the holder
/// keeps locked — the daemon keeps its pid in a sidecar for the same reason.
pub(crate) const REPORTED_LOST_EXTENSION: &str = "lost";

/// The one watcher of a conversation: a flock on `<session_id>.watch`, held
/// until the hook process exits. The file itself carries nothing; the ids a
/// wake has reported lost live in a sidecar beside it ([`record_lost`]).
///
/// First-come and only ever tried, never waited on, so it sits outside the rank
/// order the way `FetchLease` does. The flock dies with its holder, so a killed
/// watcher blocks nothing. It is taken under [`ScopeLock`], which is what lets
/// [`reap_watch_lease`] unlink an idle file without splitting two takers across
/// two inodes.
pub(crate) struct WatchLease {
    session_id: String,
    /// Held open for the process's life and never read. `None` when the file
    /// could not be opened or locked.
    _held: Option<File>,
}

impl WatchLease {
    /// `None` when a live watcher holds this conversation. A file that cannot
    /// be opened or locked proceeds unleased: a second watcher costs a duplicate
    /// poll and `jobs::claim` still delivers once, where standing down could
    /// leave a job with no watcher at all.
    pub(crate) fn take(session_id: &str) -> Option<Self> {
        let _hold = ScopeLock::acquire();
        Self::take_under_scope_lock(session_id)
    }

    fn take_under_scope_lock(session_id: &str) -> Option<Self> {
        let held = match open_watch_lease(session_id) {
            Ok(file) => match file.try_lock() {
                Ok(()) => Some(file),
                Err(TryLockError::WouldBlock) => return None,
                Err(TryLockError::Error(e)) => {
                    crate::logline::to_logfile(format_args!(
                        "await-job: watching {session_id} unleased: {e}"
                    ));
                    None
                }
            },
            Err(e) => {
                crate::logline::to_logfile(format_args!(
                    "await-job: watching {session_id} unleased: {e:#}"
                ));
                None
            }
        };
        Some(Self {
            session_id: session_id.to_string(),
            _held: held,
        })
    }

    pub(crate) fn session_id(&self) -> &str {
        &self.session_id
    }
}

fn open_watch_lease(session_id: &str) -> Result<File> {
    let path = record_path(session_id, None)?.with_extension(WATCH_LEASE_EXTENSION);
    Ok(crate::profile::open_state_file(&path)?)
}

/// Unlink a lease whose conversation's record the sweep has reaped, unless a
/// watcher still holds it, and the reported-lost sidecar beside it: both die
/// with the conversation they name, and a later session just makes its own
/// again. The caller holds [`ScopeLock`], so no taker can open the file
/// between this try and the unlink. The handle closes before the unlink, because
/// Windows refuses to remove an open file.
pub(crate) fn reap_watch_lease(path: &Path) {
    if path.with_extension("json").exists() {
        return;
    }
    let Ok(file) = OpenOptions::new().read(true).write(true).open(path) else {
        return;
    };
    if file.try_lock().is_ok() {
        drop(file);
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(path.with_extension(REPORTED_LOST_EXTENSION));
    }
}

/// A main-scope turn end with work out: restart the idle count, then try for
/// the lease in the same hold. `None` when a live watcher holds the lease,
/// which reads the new stamp at its next poll.
pub(crate) fn mark_turn_end(payload: &Payload, now: u64) -> Option<WatchLease> {
    let _hold = ScopeLock::acquire();
    if let Ok(path) = record_path(&payload.session_id, None) {
        let mut record = load_record(&path).unwrap_or_default();
        record.turn_ended_at = Some(now);
        if payload.transcript.is_some() {
            record.transcript = payload.transcript.clone();
        }
        if let Err(e) = store_record(&path, &record) {
            crate::logline::to_logfile(format_args!(
                "await-job: cannot stamp the turn end on {}: {e:#}; no idle wake this turn",
                path.display()
            ));
        }
    }
    WatchLease::take_under_scope_lock(&payload.session_id)
}

/// The `UserPromptSubmit` leg of the idle count: a turn is starting, so nothing
/// is idle until its `Stop`. A subagent's fire says nothing about the main
/// conversation.
pub(crate) fn note_turn_start(payload: &Payload) {
    if payload.event == "UserPromptSubmit" && payload.agent_id.is_none() {
        clear_turn_end(&payload.session_id);
    }
}

/// Stop the idle count until the next turn end. Peeks unlocked first, since a
/// prompt in a conversation with nothing out has no record to change.
pub(crate) fn clear_turn_end(session_id: &str) {
    if turn_ended_at(session_id).is_none() {
        return;
    }
    let Ok(path) = record_path(session_id, None) else {
        return;
    };
    let _hold = ScopeLock::acquire();
    let Some(mut record) = load_record(&path) else {
        return;
    };
    record.turn_ended_at = None;
    if let Err(e) = store_record(&path, &record) {
        crate::logline::to_logfile(format_args!(
            "await-job: cannot clear the turn end on {}: {e:#}",
            path.display()
        ));
    }
}

/// When the conversation's idle count started, `None` while a turn runs. A
/// plain read under no lock: writers replace the record atomically.
pub(crate) fn turn_ended_at(session_id: &str) -> Option<u64> {
    load_record(&record_path(session_id, None).ok()?)?.turn_ended_at
}

/// Whether the idle count has reached `window_ms`. A clock stepped behind the
/// stamp reads as not yet.
pub(crate) fn idle_wake_due(turn_ended_at: Option<u64>, now: u64, window_ms: u64) -> bool {
    turn_ended_at.is_some_and(|at| now.saturating_sub(at) >= window_ms)
}

/// One prompt-cache TTL. Claude Code 2.1.319's schema admits `5m` and `1h`
/// only (measured 2026-11-07).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CacheTtl {
    FiveMinutes,
    OneHour,
}

impl CacheTtl {
    /// The value spelling Claude Code's schema admits.
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::FiveMinutes => "5m",
            Self::OneHour => "1h",
        }
    }
}

/// Which resolution step answered, in Claude Code's own order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TtlSource {
    /// `FORCE_PROMPT_CACHING_5M` is a true flag.
    Force5mEnv,
    /// The TTL env var of the conversation.
    Env,
    /// The TTL settings key.
    Setting,
    /// `ENABLE_PROMPT_CACHING_1H` is a true flag.
    Enable1hEnv,
    /// Nothing named a TTL: the account decides (main), or 5 min (subagent).
    Automatic,
}

/// The account leg of the automatic step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CacheAccount {
    /// A claude.ai subscription. Overage only changes the automatic step: an
    /// explicit TTL stays in overage.
    Subscription {
        /// The last cached usage reading shows a live window pinned at the cap.
        overage: bool,
    },
    /// An api key, a cloud provider, or an account that cannot resolve.
    Other,
}

/// The inputs of one TTL resolution, in Claude Code 2.1.319's own order
/// (measured 2026-11-07): `FORCE_PROMPT_CACHING_5M`, the TTL env var, the TTL
/// setting, `ENABLE_PROMPT_CACHING_1H`, then the automatic fallback. A flag
/// is true when its value, trimmed and lowercased, is `1`, `true`, `yes` or
/// `on`; a value that is neither `5m` nor `1h` is ignored and the next step
/// decides.
#[derive(Debug)]
pub(crate) struct CacheInputs<'a> {
    pub(crate) force_5m: Option<&'a str>,
    pub(crate) env_ttl: Option<&'a str>,
    pub(crate) setting: Option<&'a str>,
    pub(crate) enable_1h: Option<&'a str>,
    pub(crate) account: CacheAccount,
}

/// The shared resolution: the same four steps for a main conversation and a
/// subagent, one `main` flag deciding the automatic fallback, so the two can
/// never drift into different orders.
pub(crate) fn cache_ttl(inputs: &CacheInputs<'_>, main: bool) -> (CacheTtl, TtlSource) {
    if is_true_flag(inputs.force_5m) {
        return (CacheTtl::FiveMinutes, TtlSource::Force5mEnv);
    }
    if let Some(ttl) = ttl_value(inputs.env_ttl) {
        return (ttl, TtlSource::Env);
    }
    if let Some(ttl) = ttl_value(inputs.setting) {
        return (ttl, TtlSource::Setting);
    }
    if is_true_flag(inputs.enable_1h) {
        return (CacheTtl::OneHour, TtlSource::Enable1hEnv);
    }
    if main
        && matches!(
            inputs.account,
            CacheAccount::Subscription { overage: false }
        )
    {
        (CacheTtl::OneHour, TtlSource::Automatic)
    } else {
        (CacheTtl::FiveMinutes, TtlSource::Automatic)
    }
}

/// The main conversation's TTL: the account decides the automatic step.
pub(crate) fn main_cache_ttl(inputs: &CacheInputs<'_>) -> (CacheTtl, TtlSource) {
    cache_ttl(inputs, true)
}

/// The subagent conversation's TTL: the same order over the subagent env var
/// and setting, and a 5 min automatic fallback — [`main_cache_ttl`] never
/// applies to it, and a clauth delegate is its own `claude -p` main
/// conversation.
pub(crate) fn subagent_cache_ttl(inputs: &CacheInputs<'_>) -> (CacheTtl, TtlSource) {
    cache_ttl(inputs, false)
}

/// Claude Code's flag test: the value, trimmed and lowercased, is `1`,
/// `true`, `yes` or `on`.
fn is_true_flag(value: Option<&str>) -> bool {
    value
        .map(str::trim)
        .is_some_and(|v| matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"))
}

/// The two TTLs Claude Code's schema admits; anything else is ignored.
fn ttl_value(value: Option<&str>) -> Option<CacheTtl> {
    match value {
        Some("5m") => Some(CacheTtl::FiveMinutes),
        Some("1h") => Some(CacheTtl::OneHour),
        _ => None,
    }
}

/// Whether this session's MAIN prompt cache lives 1 h — the one TTL the
/// 55 min idle wake keeps alive. The answer is Claude Code 2.1.319's own
/// resolution over this process's real inputs: the hook runs inside the
/// session, so it inherits the session's env; the setting reads the same
/// settings file as before; the account is the clauth account this session
/// resolves to.
pub(crate) fn prompt_cache_is_1h() -> bool {
    let force_5m = std::env::var("FORCE_PROMPT_CACHING_5M").ok();
    let env_ttl = std::env::var("CLAUDE_CODE_PROMPT_CACHE_TTL").ok();
    let setting = settings_cache_ttl();
    let enable_1h = std::env::var("ENABLE_PROMPT_CACHING_1H").ok();
    let mut inputs = CacheInputs {
        force_5m: force_5m.as_deref(),
        env_ttl: env_ttl.as_deref(),
        setting: setting.as_deref(),
        enable_1h: enable_1h.as_deref(),
        // The account resolves only when the automatic step is reached: an
        // explicit env or setting decides with no profile read, so a turn end
        // on a decided box costs no dir creation or permission walk.
        account: CacheAccount::Other,
    };
    if main_cache_ttl(&inputs).1 == TtlSource::Automatic {
        inputs.account = session_cache_account();
    }
    main_cache_ttl(&inputs).0 == CacheTtl::OneHour
}

/// The account leg of the automatic step, off this session's real inputs: a
/// clauth account resolving to an OAuth login with no api key and no routing
/// endpoint — and no env pointing the host at an api key, a base URL or a
/// cloud provider, which Claude Code then bills — reads as a subscription;
/// overage is the last cached usage reading's verdict. An account that cannot
/// resolve, one that holds no login, and any read error, is
/// [`CacheAccount::Other`].
fn session_cache_account() -> CacheAccount {
    // The no-rewrite load: the hook answers before it serves, and a profile
    // rewrite or a `.pending` write-through has no place at a turn end.
    let Ok(config) = crate::profile::load_config_without_rewrite() else {
        return CacheAccount::Other;
    };
    let Some((name, _)) = crate::which::resolve_active(&config) else {
        return CacheAccount::Other;
    };
    let Some(profile) = config.profiles.iter().find(|p| p.name.as_str() == name) else {
        return CacheAccount::Other;
    };
    if profile.api_key.is_some() || profile.routing_endpoint().is_some() || !profile.holds_login() {
        return CacheAccount::Other;
    }
    if std::env::var_os("ANTHROPIC_API_KEY").is_some()
        || std::env::var_os("ANTHROPIC_AUTH_TOKEN").is_some()
        || std::env::var_os("ANTHROPIC_BASE_URL").is_some()
        || std::env::var_os("CLAUDE_CODE_USE_BEDROCK").is_some()
        || std::env::var_os("CLAUDE_CODE_USE_VERTEX").is_some()
        || std::env::var_os("CLAUDE_CODE_USE_FOUNDRY").is_some()
    {
        return CacheAccount::Other;
    }
    CacheAccount::Subscription {
        overage: cached_reading_maxed(&profile.name),
    }
}

/// Whether the account's last cached usage reading shows a live window pinned
/// at the 100% cap — the same spent predicate the fetch scheduler reads.
/// Never fetches, never writes: no reading reads as not spent.
fn cached_reading_maxed(name: &crate::profile::ProfileName) -> bool {
    crate::profile_cache::load_profile_cache::<crate::usage::UsageInfo>(
        name,
        crate::profile_cache::USAGE_CACHE_FILE,
    )
    .is_some_and(|info| crate::usage::windows_maxed(&info, (crate::usage::now_ms() / 1000) as i64))
}

#[derive(Deserialize, Default)]
struct CacheSettings {
    #[serde(default, rename = "promptCacheTtl", deserialize_with = "ttl_field")]
    ttl: Option<String>,
    #[serde(
        default,
        rename = "subagentPromptCacheTtl",
        deserialize_with = "ttl_field"
    )]
    subagent_ttl: Option<String>,
}

/// One TTL key read on its own: a value that is not a string (a number, an
/// object) reads as unset for that key alone, so a wrong-typed subagent key
/// never drops the valid main TTL beside it.
fn ttl_field<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(match value {
        serde_json::Value::String(s) => Some(s),
        _ => None,
    })
}

fn settings_file() -> Option<std::path::PathBuf> {
    crate::which::session_config_dir_or_global().map(|dir| dir.join("settings.json"))
}

fn settings_cache() -> Option<CacheSettings> {
    let bytes = std::fs::read(settings_file()?).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn settings_cache_ttl() -> Option<String> {
    settings_cache().and_then(|settings| settings.ttl)
}

/// The prompt-cache TTL inputs the Services `plugin` row renders: this
/// process's env vars and the settings file the idle wake reads. The account
/// half of the automatic step is not carried — the row is global, sessions
/// run on different accounts — so the main resolution leaves it automatic.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct CacheView {
    pub(crate) force_5m: Option<String>,
    pub(crate) main_env: Option<String>,
    pub(crate) main_setting: Option<String>,
    pub(crate) enable_1h: Option<String>,
    pub(crate) subagent_env: Option<String>,
    pub(crate) subagent_setting: Option<String>,
}

impl CacheView {
    /// This process's real inputs: its env and the settings file.
    pub(crate) fn read() -> Self {
        let settings = settings_cache();
        Self {
            force_5m: std::env::var("FORCE_PROMPT_CACHING_5M").ok(),
            main_env: std::env::var("CLAUDE_CODE_PROMPT_CACHE_TTL").ok(),
            main_setting: settings.as_ref().and_then(|settings| settings.ttl.clone()),
            enable_1h: std::env::var("ENABLE_PROMPT_CACHING_1H").ok(),
            subagent_env: std::env::var("CLAUDE_CODE_SUBAGENT_PROMPT_CACHE_TTL").ok(),
            subagent_setting: settings.and_then(|settings| settings.subagent_ttl),
        }
    }

    /// The main conversation's resolution over the view.
    pub(crate) fn main_ttl(&self) -> (CacheTtl, TtlSource) {
        main_cache_ttl(&CacheInputs {
            force_5m: self.force_5m.as_deref(),
            env_ttl: self.main_env.as_deref(),
            setting: self.main_setting.as_deref(),
            enable_1h: self.enable_1h.as_deref(),
            account: CacheAccount::Other,
        })
    }

    /// The subagent conversation's resolution over the view.
    pub(crate) fn subagent_ttl(&self) -> (CacheTtl, TtlSource) {
        subagent_cache_ttl(&CacheInputs {
            force_5m: self.force_5m.as_deref(),
            env_ttl: self.subagent_env.as_deref(),
            setting: self.subagent_setting.as_deref(),
            enable_1h: self.enable_1h.as_deref(),
            account: CacheAccount::Other,
        })
    }
}

/// The lost-job ids this conversation's watcher has already reported, read
/// from the sidecar beside the record. A file that cannot be read (never
/// written, or not a file at all) reads as none.
pub(crate) fn reported_lost(session_id: &str) -> Vec<String> {
    let Ok(path) = record_path(session_id, None) else {
        return Vec::new();
    };
    std::fs::read(path.with_extension(REPORTED_LOST_EXTENSION))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

/// Record the lost-job ids a wake just reported, so no later wait of this
/// session names them again. Lives in a sidecar beside the conversation's
/// record, written atomically; the sweep reaps it with the lease. A write
/// that fails degrades the once to the process, which still ends after the
/// wake.
pub(crate) fn record_lost(session_id: &str, ids: &[String]) {
    let Ok(path) = record_path(session_id, None) else {
        return;
    };
    let path = path.with_extension(REPORTED_LOST_EXTENSION);
    let mut reported = reported_lost(session_id);
    let mut changed = false;
    for id in ids {
        if !reported.contains(id) {
            reported.push(id.clone());
            changed = true;
        }
    }
    if !changed {
        return;
    }
    if let Err(e) =
        crate::profile::atomic_write_600(&path, serde_json::to_vec(&reported).unwrap_or_default())
    {
        crate::logline::to_logfile(format_args!(
            "await-job: cannot record the lost jobs of {session_id} on {}: {e:#}",
            path.display()
        ));
    }
}

/// Whether the host's current session id has moved off `session_id`: the
/// `claude` process this hook command runs under (`CLAUDE_PID`, set for hook
/// commands) writes its CURRENT id to `<config dir>/sessions/<pid>.json`, and
/// `/clear`, `/fork` and `/branch` move it while the process lives. An unset
/// or unparsable pid and an unreadable record read as not moved, so the idle
/// wake then behaves as before.
pub(crate) fn host_session_moved(session_id: &str) -> bool {
    let Some(pid) = std::env::var("CLAUDE_PID")
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
    else {
        return false;
    };
    let Some(dir) = crate::which::session_config_dir_or_global().map(|dir| dir.join("sessions"))
    else {
        return false;
    };
    crate::session_record::session_record(&dir, pid)
        .is_some_and(|record| record.session_id.as_str() != session_id)
}

#[cfg(test)]
#[path = "../tests/inline/hook_jobs.rs"]
mod tests;
