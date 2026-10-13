//! The per-proxy supervisor: one [`Proxy`] kind over the generic machine in
//! [`super::gateway`], the `proxies[]` slot, and the coordinator that starts a
//! supervisor per enabled registry row and stops them all on a daemon stop.
//!
//! Every enabled `~/.clauth/proxies.toml` row runs `<binary> serve` as its own
//! supervised child, with the gateway's stop discipline per proxy (restart
//! backoff, foreign-listener refusal on its port, one SIGTERM on stop, the next
//! daemon finishing a still-draining proxy by its recorded deadline, the
//! Windows reclaim). The coordinator re-reads the registry each round so an
//! enable or a new row starts without a daemon restart; a disabled or removed
//! row stops its child through the supervisor's own round.

use std::collections::BTreeMap;
use std::fs::File;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use super::gateway::{
    ChildVerdict, ExitReport, ForeignVerdict, Identity, Intent, LiveState, Running, SlotState,
    Supervised, Supervisor, SupervisorThread, start_kind,
};
use super::log_rotate::{LOG_KEEP_BYTES, LOG_MAX_BYTES, rotate_log_if_large};
use super::probe::{DaemonLock, terminate_pid};
use crate::gateway::{ProxyProbe, probe_proxy_health};
use crate::lockorder::{RankedMutex, rank};
use crate::logline::logline;
use crate::profile::open_append_600;
use crate::proxy::{
    Manifest, ManifestRefusal, ProxyRow, Registry, SPOKEN_MAJOR, Service, admin_token_path,
    child_marker_path, contract_version, log_path, proxies_dir, read_manifest_cancellable,
    state_dir,
};

/// A proxy's drain bound is its manifest `drain_secs` plus this: the process
/// exit after the drain. The core closes its SSE streams and poll timers, then
/// waits its drain timer (exactly `drain_secs`, resolved earlier when streams
/// drain); the margin covers the exit itself, the role [`super::gateway`]'s
/// `STOP_MARGIN` plays for shunt past its drain and blocking grace.
const PROXY_STOP_MARGIN: Duration = Duration::from_secs(5);

// ── the slot ────────────────────────────────────────────────────────────────

/// One proxy's state, a closed set.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ProxyState {
    /// No registry row.
    #[default]
    Absent,
    /// The row's `enabled` flag is off.
    Disabled,
    /// The binary is not there to run.
    BinaryMissing,
    /// The admin token file is missing; clauth mints none.
    NoToken,
    /// `manifest` was refused; held until the binary or the row changes.
    ManifestRefused,
    /// The row's binary or log cannot be read into a spawn; `reason` says
    /// which. Retried on the fixed health cadence, the gateway's
    /// `misconfigured` precedent, never held.
    Misconfigured,
    /// Something clauth did not spawn answers on the port.
    Foreign,
    Starting,
    Healthy,
    /// Running, but `/health` does not answer; never killed for it.
    Unhealthy,
    /// It served a contract major clauth does not speak, or a `/health`
    /// lacking its service, so clauth stopped it and holds off.
    ContractMismatch,
    Restarting,
    Stopping,
    /// The record-only slot with no supervisor running.
    Unobserved,
}

/// What answered `/health` on the port of a [`ProxyState::Foreign`] proxy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ProxyAnswerer {
    /// Another proxy answered; `service` names it (`None` when its body
    /// omitted the field), its `version` and `contract` ride the slot's
    /// fields.
    Proxy { service: Option<String> },
    /// An HTTP answer that is not a proxy `/health`.
    NotProxy,
    /// A listener that took the connection and never answered.
    NoAnswer,
}

/// The `proxies[]` object: one per registry row, in the registry's service
/// order. Service, port, pid, versions and states only; never a token, an env
/// value or a file's content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub(crate) struct ProxySlot {
    /// The proxy's service, the `<service>` in its binary name.
    pub(crate) service: String,
    pub(crate) state: ProxyState,
    /// The binary the proxy runs as: the row's recorded path while it exists,
    /// else the `clauth-<service>-proxy` on `PATH`.
    #[schema(required = true)]
    pub(crate) binary: Option<String>,
    /// The port the proxy binds, or the port a foreign answerer holds.
    #[schema(required = true)]
    pub(crate) port: Option<u16>,
    /// The pid of the proxy clauth spawned, while one runs.
    #[schema(required = true)]
    pub(crate) pid: Option<u32>,
    /// The version `/health` reported: the proxy's, or a foreign answerer's.
    #[schema(required = true)]
    pub(crate) version: Option<String>,
    /// The contract `/health` reported, beside `version`.
    #[schema(required = true)]
    pub(crate) contract: Option<String>,
    /// What answered on the port of a `foreign` proxy.
    #[schema(required = true)]
    pub(crate) answerer: Option<ProxyAnswerer>,
    /// Restarts after an exit clauth did not ask for, since the daemon
    /// started.
    pub(crate) restarts: u32,
    /// How the last proxy process ended, `null` before any has.
    #[schema(required = true)]
    pub(crate) last_exit: Option<ExitReport>,
    /// Why the proxy cannot start or was stopped.
    #[schema(required = true)]
    pub(crate) reason: Option<String>,
    /// ISO-8601 UTC stamp of when `state` last changed; `null` without a
    /// supervisor.
    #[schema(required = true)]
    pub(crate) since: Option<String>,
}

impl ProxySlot {
    fn of(state: ProxyState, service: &Service) -> Self {
        Self {
            service: service.as_str().to_string(),
            state,
            binary: None,
            port: None,
            pid: None,
            version: None,
            contract: None,
            answerer: None,
            restarts: 0,
            last_exit: None,
            reason: None,
            since: None,
        }
    }

    fn for_record(state: ProxyState, record: &ProxyRecord) -> Self {
        Self {
            binary: Some(record.binary.display().to_string()),
            port: Some(record.row.port),
            ..Self::of(state, &record.service)
        }
    }
}

impl SlotState for ProxySlot {
    fn same_state(&self, other: &Self) -> bool {
        self.state == other.state
    }

    fn since(&self) -> Option<String> {
        self.since.clone()
    }

    fn reason(&self) -> Option<&str> {
        self.reason.as_deref()
    }

    fn with_counts(
        &self,
        restarts: u32,
        last_exit: Option<ExitReport>,
        since: Option<String>,
    ) -> Self {
        Self {
            restarts,
            last_exit,
            since,
            ..self.clone()
        }
    }
}

/// The shared slots map every proxy supervisor publishes into.
pub(crate) type ProxySlots = Arc<RankedMutex<BTreeMap<Service, ProxySlot>, rank::ProxyPublished>>;

pub(crate) fn new_slots() -> ProxySlots {
    Arc::new(RankedMutex::new(BTreeMap::new()))
}

/// What one proxy's supervisor last published, or `None` before its first
/// publish.
#[cfg(all(test, unix))]
pub(crate) fn published(slots: &ProxySlots, service: &Service) -> Option<ProxySlot> {
    match slots.lock() {
        Ok(map) => map.get(service).cloned(),
        Err(poisoned) => poisoned.into_inner().get(service).cloned(),
    }
}

/// Every published slot, in service-name order.
pub(crate) fn slots(slots: &ProxySlots) -> Vec<ProxySlot> {
    match slots.lock() {
        Ok(map) => map.values().cloned().collect(),
        Err(poisoned) => poisoned.into_inner().values().cloned().collect(),
    }
}

/// The `proxies` array: one object per registry row in service-name order, the
/// supervisor's slot where one is published, else the row's record-only
/// verdict. `live` carries the published slots (already in service order).
///
/// A registry that does not read never reads as `[]` while children run: the
/// live slots are published instead (the failure is logged once per distinct
/// error), so the feed keeps showing the proxies the supervisors run.
pub(crate) fn entries(live: Option<&[ProxySlot]>) -> Vec<ProxySlot> {
    let registry = match Registry::load() {
        Ok(registry) => registry,
        Err(e) => {
            // Never drop the failure silently: the single-shot `status --json`
            // publishes `"proxies": []` but names the error on stderr, and the
            // daemon's feed keeps the live slots it holds.
            log_registry_error_once(&e);
            return live.map(<[ProxySlot]>::to_vec).unwrap_or_default();
        }
    };
    let live_map: BTreeMap<&str, &ProxySlot> = live
        .map(|slots| {
            slots
                .iter()
                .map(|slot| (slot.service.as_str(), slot))
                .collect()
        })
        .unwrap_or_default();
    registry
        .iter()
        .map(|(service, _row)| {
            live_map
                .get(service.as_str())
                .map(|slot| (*slot).clone())
                .unwrap_or_else(|| unsupervised_slot(service))
        })
        .collect()
}

/// Log a registry that does not read, once per distinct error, so a caller of
/// [`entries`] (the single-shot `status --json` above all) does not swallow it.
fn log_registry_error_once(error: &anyhow::Error) {
    static LAST: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);
    let error = format!("{error:#}");
    let mut last = LAST.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if last.as_deref() != Some(error.as_str()) {
        logline!("clauth: cannot read the proxy registry: {error}");
        *last = Some(error);
    }
}

/// The slot with no supervisor to ask: the row's own verdict, or `unobserved`
/// for a row the proxy would run on.
pub(crate) fn unsupervised_slot(service: &Service) -> ProxySlot {
    match (Proxy {
        service: service.clone(),
    })
    .intent()
    {
        Intent::Idle(slot) => slot,
        Intent::Run(record) => ProxySlot::for_record(ProxyState::Unobserved, &record),
        Intent::Unchanged => ProxySlot::of(ProxyState::Unobserved, service),
    }
}

/// The TUI's `stop daemon`, once no daemon is left: stop the proxies the
/// stopped daemon could not (on Windows it ends by `taskkill /F` with no
/// chance to; on unix a SIGKILLed one never did), the gateway's
/// [`super::gateway::stop_left_behind_gateway`] twin. Each stop is recorded in
/// the proxy's child marker, so a later daemon still finishes a drain that
/// outlives its deadline. Walks the state dirs, not the registry, so a removed
/// row's draining proxy is stopped too.
pub(crate) fn stop_left_behind_proxies() {
    for service in state_dir_services() {
        let Ok(path) = child_marker_path(&service) else {
            continue;
        };
        let _ = super::gateway::stop_left_behind(
            &path,
            &format!("proxy {service}"),
            crate::usage::now_ms(),
        );
    }
}

/// Every service with a state dir under `~/.clauth/proxies/`, each dir name
/// validated as a service first: a name that fails is skipped, so no path is
/// ever built from an unvalidated name.
fn state_dir_services() -> Vec<Service> {
    let Ok(dir) = proxies_dir() else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name();
            let name = name.to_str()?;
            Service::parse(name).ok()
        })
        .collect()
}

// ── the proxy kind ──────────────────────────────────────────────────────────

/// One clauth proxy's kind value: the service its row and state dir carry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Proxy {
    pub(crate) service: Service,
}

/// What a round runs: the row, the service and the resolved binary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProxyRecord {
    pub(crate) service: Service,
    pub(crate) row: ProxyRow,
    pub(crate) binary: PathBuf,
}

impl Supervised for Proxy {
    type Slot = ProxySlot;
    type Handle = ProxySlots;
    type Record = ProxyRecord;
    type Probe = ProxyProbe;
    type Prepared = Manifest;
    type SpawnMark = ();

    fn publish(&self, handle: &ProxySlots, slot: ProxySlot) {
        match handle.lock() {
            Ok(mut map) => {
                map.insert(self.service.clone(), slot);
            }
            Err(poisoned) => {
                poisoned.into_inner().insert(self.service.clone(), slot);
            }
        }
    }

    fn intent(&self) -> Intent<ProxySlot, ProxyRecord> {
        let registry = match Registry::load() {
            Ok(registry) => registry,
            // An unreadable registry never changes what runs: keep the last
            // published slot and any running child, and stop nothing. The
            // coordinator logs the error once per distinct error.
            Err(_) => {
                return Intent::Unchanged;
            }
        };
        let Some(row) = registry.get(&self.service) else {
            return Intent::Idle(ProxySlot::of(ProxyState::Absent, &self.service));
        };
        let binary = resolve_binary(&self.service, row);
        let binary_name = binary
            .as_deref()
            .map(|path| path.display().to_string())
            .or_else(|| row.binary.as_deref().map(|path| path.display().to_string()))
            .unwrap_or_else(|| self.service.binary_name());
        if !row.enabled {
            return Intent::Idle(ProxySlot {
                binary: Some(binary_name),
                port: Some(row.port),
                ..ProxySlot::of(ProxyState::Disabled, &self.service)
            });
        }
        let Some(binary) = binary else {
            return Intent::Idle(ProxySlot {
                binary: Some(binary_name),
                port: Some(row.port),
                ..ProxySlot::of(ProxyState::BinaryMissing, &self.service)
            });
        };
        match admin_token_path(&self.service) {
            Ok(path) if path.try_exists().unwrap_or(false) => {}
            Ok(path) => {
                return Intent::Idle(ProxySlot {
                    binary: Some(binary.display().to_string()),
                    port: Some(row.port),
                    reason: Some(format!(
                        "the admin token of proxy {:?} in {} is missing; run `clauth proxy enable {}`",
                        self.service.as_str(),
                        path.display(),
                        self.service
                    )),
                    ..ProxySlot::of(ProxyState::NoToken, &self.service)
                });
            }
            Err(e) => {
                return Intent::Idle(ProxySlot {
                    binary: Some(binary.display().to_string()),
                    port: Some(row.port),
                    reason: Some(format!("{e:#}")),
                    ..ProxySlot::of(ProxyState::NoToken, &self.service)
                });
            }
        }
        Intent::Run(ProxyRecord {
            service: self.service.clone(),
            row: row.clone(),
            binary,
        })
    }

    fn initial_run_slot(&self, record: &ProxyRecord) -> ProxySlot {
        ProxySlot::for_record(ProxyState::Starting, record)
    }

    fn unreadable_slot(&self) -> ProxySlot {
        ProxySlot::of(ProxyState::Unobserved, &self.service)
    }

    fn live_slot(
        &self,
        record: &ProxyRecord,
        state: LiveState,
        port: u16,
        pid: u32,
        version: Option<String>,
        contract: Option<String>,
    ) -> ProxySlot {
        let proxy_state = match state {
            LiveState::Starting => ProxyState::Starting,
            LiveState::Healthy => ProxyState::Healthy,
            LiveState::Unhealthy => ProxyState::Unhealthy,
            LiveState::Stopping => ProxyState::Stopping,
        };
        ProxySlot {
            state: proxy_state,
            port: Some(port),
            pid: Some(pid),
            version,
            contract,
            ..ProxySlot::for_record(proxy_state, record)
        }
    }

    fn restarting_slot(&self, record: &ProxyRecord, port: u16) -> ProxySlot {
        ProxySlot {
            port: Some(port),
            ..ProxySlot::for_record(ProxyState::Restarting, record)
        }
    }

    fn binary_missing_slot(&self, record: &ProxyRecord, port: u16) -> ProxySlot {
        ProxySlot {
            port: Some(port),
            ..ProxySlot::for_record(ProxyState::BinaryMissing, record)
        }
    }

    fn spawn_error_slot(&self, record: &ProxyRecord, port: u16, reason: String) -> ProxySlot {
        ProxySlot {
            port: Some(port),
            reason: Some(reason),
            ..ProxySlot::for_record(ProxyState::Misconfigured, record)
        }
    }

    fn orphan_slot(&self, pid: u32) -> ProxySlot {
        ProxySlot {
            pid: Some(pid),
            ..ProxySlot::of(ProxyState::Stopping, &self.service)
        }
    }

    fn idle_stops_child(&self, slot: &ProxySlot) -> bool {
        matches!(slot.state, ProxyState::Absent | ProxyState::Disabled)
    }

    fn is_foreign(&self, slot: &ProxySlot) -> bool {
        slot.state == ProxyState::Foreign
    }

    fn probe(&self, addr: SocketAddr) -> Result<ProxyProbe> {
        probe_proxy_health(addr)
    }

    fn stop_bound(&self, _record: &ProxyRecord, manifest: &Manifest) -> Duration {
        Duration::from_secs(u64::from(manifest.drain_secs)).saturating_add(PROXY_STOP_MARGIN)
    }

    fn prepare(
        &self,
        record: &ProxyRecord,
        memo: Option<&Manifest>,
        cancel: &std::sync::atomic::AtomicBool,
    ) -> Result<(Manifest, SocketAddr, u16, File)> {
        // The manifest is read once per binary identity (path + mtime) and
        // reused across retry rounds: a `manifest` subprocess of up to 5 s
        // must not run once per round while a foreign listener holds the port
        // or a spawn error repeats. The read is cancellable, so a shutdown
        // landing while it runs kills and reaps the child at once instead of
        // waiting the bound out.
        let manifest = match memo {
            Some(manifest) => manifest.clone(),
            None => read_manifest_cancellable(&record.service, &record.binary, cancel)?,
        };
        let addr = crate::proxy::bind(record.row.port);
        let log = log_path(&record.service)?;
        let log =
            open_append_600(&log).with_context(|| format!("failed to open {}", log.display()))?;
        Ok((manifest, addr, record.row.port, log))
    }

    fn prepare_refusal(&self, record: &ProxyRecord, error: &anyhow::Error) -> (ProxySlot, bool) {
        let reason = format!("{error:#}");
        let is_manifest_refusal = error
            .chain()
            .any(|cause| cause.downcast_ref::<ManifestRefusal>().is_some());
        if is_manifest_refusal {
            // A refused manifest is held until the binary or the row changes.
            (
                ProxySlot {
                    reason: Some(reason),
                    ..ProxySlot::for_record(ProxyState::ManifestRefused, record)
                },
                true,
            )
        } else {
            // A transient io failure (a log that cannot open, a bind that
            // fails) is retried on the fixed health cadence, the gateway's
            // `misconfigured` precedent, never held.
            (
                ProxySlot {
                    reason: Some(reason),
                    ..ProxySlot::for_record(ProxyState::Misconfigured, record)
                },
                false,
            )
        }
    }

    fn spawn(
        &self,
        record: &ProxyRecord,
        _manifest: &Manifest,
        log: File,
    ) -> std::io::Result<Child> {
        let state_dir = state_dir(&record.service).map_err(std::io::Error::other)?;
        let token_path = admin_token_path(&record.service).map_err(std::io::Error::other)?;
        let mut command = Command::new(&record.binary);
        command
            .arg("serve")
            .current_dir(&state_dir)
            .env(
                "CLAUTH_PROXY_BIND",
                crate::proxy::bind(record.row.port).to_string(),
            )
            .env("CLAUTH_PROXY_STATE_DIR", &state_dir)
            .env("CLAUTH_PROXY_ADMIN_TOKEN_FILE", &token_path)
            .stdin(Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log);
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt as _;
            command.process_group(0);
        }
        command.spawn()
    }

    fn spawn_error_reason(&self, record: &ProxyRecord, error: &std::io::Error) -> String {
        format!("cannot run {}: {error}", record.binary.display())
    }

    fn missing_binary_note(&self, record: &ProxyRecord) -> String {
        format!(
            "{} not found; install the proxy or run `clauth proxy enable {}`",
            record.binary.display(),
            record.service
        )
    }

    fn identity(&self, record: &ProxyRecord) -> Identity<Proxy> {
        let mtime = std::fs::metadata(&record.binary)
            .and_then(|meta| meta.modified())
            .ok();
        Identity {
            record: record.clone(),
            binary: Some(record.binary.clone()),
            mtime,
        }
    }

    fn respawn_inputs_changed(&self, a: &ProxyRecord, b: &ProxyRecord) -> bool {
        a != b
    }

    fn classify_child(
        &self,
        running: &Running<Proxy>,
        answer: Result<ProxyProbe>,
        _tick: super::gateway::Tick,
    ) -> ChildVerdict<ProxySlot> {
        let Ok(ProxyProbe::Answered(health)) = answer else {
            return ChildVerdict::Unhealthy;
        };
        let refused = |why: String, version: String, contract: String| {
            let slot = ProxySlot {
                port: Some(running.port),
                version: Some(version.clone()),
                contract: Some(contract),
                reason: Some(why.clone()),
                ..ProxySlot::for_record(ProxyState::ContractMismatch, &running.identity.record)
            };
            ChildVerdict::Refused {
                why,
                version: Some(version),
                slot,
            }
        };
        let Some(declared) = health.service else {
            logline!(
                "clauth daemon: proxy {} answered /health without a service; stopping it (pid {})",
                self.service,
                running.pid
            );
            return refused(
                "its /health names no service".to_string(),
                health.version,
                health.contract,
            );
        };
        if declared != self.service.as_str() {
            logline!(
                "clauth daemon: proxy {} answered /health as {declared:?}; stopping it (pid {})",
                self.service,
                running.pid
            );
            return refused(
                format!("its /health names service {declared:?}"),
                health.version,
                health.contract,
            );
        }
        let Some((major, _minor)) = contract_version(&health.contract) else {
            logline!(
                "clauth daemon: proxy {} answered /health with contract {:?}, which is not MAJOR.MINOR; stopping it (pid {})",
                self.service,
                health.contract,
                running.pid
            );
            return refused(
                format!(
                    "it declared contract {:?}, which is not MAJOR.MINOR",
                    health.contract
                ),
                health.version,
                health.contract,
            );
        };
        if major != SPOKEN_MAJOR {
            logline!(
                "clauth daemon: proxy {} speaks contract major {major} ({:?}), and clauth speaks major {SPOKEN_MAJOR}; stopping it (pid {})",
                self.service,
                health.contract,
                running.pid
            );
            return refused(
                format!(
                    "it declared contract major {major} ({:?}), and clauth speaks major {SPOKEN_MAJOR}",
                    health.contract
                ),
                health.version,
                health.contract,
            );
        }
        if health.status != "ok" {
            return ChildVerdict::Unhealthy;
        }
        ChildVerdict::Healthy {
            version: health.version,
            contract: Some(health.contract),
        }
    }

    fn classify_foreign(
        &self,
        record: &ProxyRecord,
        port: u16,
        answer: Result<ProxyProbe>,
    ) -> ForeignVerdict<ProxySlot> {
        let (answerer, version, contract, note) = match answer {
            Ok(ProxyProbe::Silent) => return ForeignVerdict::Spawn,
            Ok(ProxyProbe::Answered(health)) => {
                let service = health.service.clone();
                let named = service
                    .as_deref()
                    .map(|service| format!("{service:?}"))
                    .unwrap_or_else(|| "no service".to_string());
                (
                    ProxyAnswerer::Proxy { service },
                    Some(health.version.clone()),
                    Some(health.contract.clone()),
                    format!(
                        "port {port} already answers /health (proxy {named} {:?}, contract {:?}); not starting the proxy beside it",
                        health.version, health.contract
                    ),
                )
            }
            Ok(ProxyProbe::NotProxy { status }) => (
                ProxyAnswerer::NotProxy,
                None,
                None,
                format!(
                    "port {port} already answers /health (a non-proxy status {status}); not starting the proxy beside it"
                ),
            ),
            Err(_) => (
                ProxyAnswerer::NoAnswer,
                None,
                None,
                format!(
                    "port {port} already answers /health (no answer); not starting the proxy beside it"
                ),
            ),
        };
        ForeignVerdict::Foreign {
            slot: ProxySlot {
                port: Some(port),
                version,
                contract,
                answerer: Some(answerer),
                ..ProxySlot::for_record(ProxyState::Foreign, record)
            },
            note,
        }
    }

    fn skipped_memo(
        &self,
        _record: &ProxyRecord,
        _prepared: &Manifest,
    ) -> Option<(PathBuf, Vec<usize>)> {
        None
    }

    fn marker_path(&self) -> Result<PathBuf> {
        child_marker_path(&self.service)
    }

    fn trim_log(&self) {
        if let Ok(path) = log_path(&self.service) {
            let _ = rotate_log_if_large(&path, LOG_MAX_BYTES, LOG_KEEP_BYTES);
        }
    }

    fn name(&self) -> String {
        format!("proxy {}", self.service)
    }

    fn idle_stop_why(&self, slot: &ProxySlot) -> String {
        format!("proxy {} is now {:?}", self.service, slot.state)
    }

    fn respawn_why(&self) -> String {
        "its row or binary changed".to_string()
    }
}

impl Supervisor<Proxy> {
    #[cfg(all(test, unix))]
    pub(crate) fn for_proxy(kind: Proxy, slots: ProxySlots) -> Self {
        Self::build(kind, slots)
    }
}

/// The binary a row runs, and what `list` resolves too: its recorded path
/// while that file exists, else the `clauth-<service>-proxy` on `PATH`.
pub(crate) fn resolve_binary(service: &Service, row: &ProxyRow) -> Option<PathBuf> {
    match &row.binary {
        Some(path) if path.try_exists().unwrap_or(false) => Some(path.clone()),
        _ => crate::plugin_probe::on_path(&service.binary_name()),
    }
}

// ── the coordinator ─────────────────────────────────────────────────────────

/// The proxy supervisors, started per enabled row and fanned out to a stop.
pub(crate) struct ProxySupervision {
    stop: Option<Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

/// Start the coordinator that runs one supervisor per enabled row, published
/// into `slots`. The coordinator re-reads the registry each round so a new
/// enable starts without a daemon restart, reclaims the orphan any state dir
/// without a running supervisor left behind, and reaps a removed row's
/// supervisor once it is done. The singleton witness is taken like
/// [`super::gateway::start`]'s: proxies run only under the active daemon.
pub(crate) fn start(slots: ProxySlots, _singleton: &DaemonLock) -> Result<ProxySupervision> {
    let (stop_tx, stop_rx) = channel();
    let thread = std::thread::Builder::new()
        .name("clauth-proxies".into())
        .spawn(move || manage(slots, stop_rx))
        .context("failed to spawn the proxy supervisor thread")?;
    Ok(ProxySupervision {
        stop: Some(stop_tx),
        thread: Some(thread),
    })
}

impl ProxySupervision {
    /// No coordinator: the daemon's signal path when the coordinator could not
    /// start (nothing to stop, nothing to join).
    pub(crate) fn idle() -> Self {
        Self {
            stop: None,
            thread: None,
        }
    }

    /// Stop every proxy and end the coordinator, the daemon's signal path.
    #[cfg(unix)]
    pub(crate) fn shutdown(mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for ProxySupervision {
    /// A drop without a shutdown (a start error, or Windows `taskkill /F`)
    /// still ends the coordinator thread best-effort, so a daemon that dies
    /// hard leaves its proxies to the next daemon's reclaim rather than to a
    /// parked thread.
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// The coordinator's per-round state: the live supervisors, the supervisors a
/// removed row reaped off-thread, and the last registry error logged.
struct Coordinator {
    threads: BTreeMap<Service, SupervisorThread<Proxy>>,
    /// Detached joiners for a removed row's supervisor, keyed by service so the
    /// orphan sweep skips a service whose stop the reaper owns. Each stops its
    /// own child off the coordinator thread and is joined by `shutdown_threads`
    /// so the daemon's stop waits for every such stop inside the one aggregate
    /// budget.
    reaping: BTreeMap<Service, JoinHandle<()>>,
    last_registry_error: Option<String>,
}

fn manage(slots: ProxySlots, stop: Receiver<()>) {
    let mut coordinator = Coordinator {
        threads: BTreeMap::new(),
        reaping: BTreeMap::new(),
        last_registry_error: None,
    };
    loop {
        let now_ms = crate::usage::now_ms();
        manage_round(&mut coordinator, &slots, now_ms);
        match stop.recv_timeout(Duration::from_secs(1)) {
            Ok(()) | Err(RecvTimeoutError::Disconnected) => {
                shutdown_threads(coordinator.threads, coordinator.reaping);
                return;
            }
            Err(RecvTimeoutError::Timeout) => {}
        }
    }
}

/// One coordinator round: read the registry, start a supervisor per enabled
/// row that has none, reap a removed row's supervisor off-thread, drop every
/// slot no supervisor it owns published, log an unreadable registry once per
/// distinct error, and reclaim orphans. Split out of [`manage`] so a test
/// drives rounds deterministically, never by a wall clock.
fn manage_round(coordinator: &mut Coordinator, slots: &ProxySlots, now_ms: u64) {
    match Registry::load() {
        Ok(registry) => {
            coordinator.last_registry_error = None;
            for (service, row) in registry.iter() {
                if row.enabled && !coordinator.threads.contains_key(service) {
                    match start_kind(
                        Proxy {
                            service: service.clone(),
                        },
                        Arc::clone(slots),
                        "clauth-proxy",
                    ) {
                        Ok(supervisor) => {
                            coordinator.threads.insert(service.clone(), supervisor);
                        }
                        Err(e) => {
                            logline!("clauth daemon: {e:#}; proxy {service} is not supervised")
                        }
                    }
                }
            }
            reap_removed(coordinator, &registry);
        }
        // An unreadable registry never changes what runs: the supervisors and
        // this coordinator keep their last good set, logged once per distinct
        // error.
        Err(e) => {
            let error = format!("{e:#}");
            if coordinator.last_registry_error.as_deref() != Some(error.as_str()) {
                logline!("clauth daemon: cannot read the proxy registry: {error}");
                coordinator.last_registry_error = Some(error);
            }
        }
    }
    sweep_unowned_slots(slots, &coordinator.threads);
    // Finished reapers drop their handle; keep the map bounded so a daemon
    // that removes rows for its whole life does not accumulate join handles.
    coordinator.reaping.retain(|_, join| !join.is_finished());
    reclaim_unwatched_orphans(&coordinator.threads, &coordinator.reaping, now_ms);
}

/// Reap a removed row's supervisor once the registry no longer names its
/// service: hand its stop to a detached joiner (so a draining child never
/// blocks the coordinator's round). The joiner is recorded by service, so the
/// orphan sweep skips the service until its reaper has sent the one SIGTERM.
/// Its published slot leaves the map through the round's slot sweep, not here.
fn reap_removed(coordinator: &mut Coordinator, registry: &Registry) {
    let removed: Vec<Service> = coordinator
        .threads
        .keys()
        .filter(|service| registry.get(service).is_none())
        .cloned()
        .collect();
    for service in removed {
        let supervisor = coordinator.threads.remove(&service);
        // On unix the reaped supervisor's stop runs on a detached joiner, so a
        // draining child never blocks the coordinator's round. Windows ends the
        // daemon by `taskkill /F` and never fans a stop out, so it drops the
        // supervisor in place (its `Drop` stops the child) as it always did.
        #[cfg(unix)]
        if let Some(supervisor) = supervisor {
            #[cfg(test)]
            let parked = take_reaper_park(&service);
            coordinator.reaping.insert(
                service.clone(),
                std::thread::spawn(move || {
                    #[cfg(test)]
                    if let Some(go) = parked {
                        let _ = go.recv();
                    }
                    supervisor.shutdown(super::gateway::DAEMON_STOP_BUDGET);
                }),
            );
        }
        #[cfg(not(unix))]
        drop(supervisor);
    }
}

/// Test-only: a reaper for a service registered here waits for its receiver
/// before it stops the child, so a test can run the coordinator's own sweep
/// while the reaper provably owns the stop. Keyed by service because tests run
/// in parallel over distinct sandboxes.
#[cfg(all(test, unix))]
static REAPER_PARKS: std::sync::LazyLock<
    std::sync::Mutex<BTreeMap<Service, std::sync::mpsc::Receiver<()>>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(BTreeMap::new()));

#[cfg(all(test, unix))]
fn take_reaper_park(service: &Service) -> Option<std::sync::mpsc::Receiver<()>> {
    REAPER_PARKS
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .remove(service)
}

/// Remove every published slot whose service the coordinator no longer owns.
/// A removed row's supervisor can republish its slot after
/// [`reap_removed`]: its stop runs off-thread and its in-flight round
/// publishes last, so a one-shot drop races it. Retaining only `threads`'
/// services every round removes any such late publish by the next round. Keyed
/// on ownership, never the registry, so a re-enabled service's fresh slot
/// survives: its new supervisor is in `threads`.
fn sweep_unowned_slots(slots: &ProxySlots, threads: &BTreeMap<Service, SupervisorThread<Proxy>>) {
    match slots.lock() {
        Ok(mut map) => map.retain(|service, _| threads.contains_key(service)),
        Err(poisoned) => poisoned
            .into_inner()
            .retain(|service, _| threads.contains_key(service)),
    }
}

/// Stop the orphan a previous daemon left in any state dir whose service has
/// no running supervisor and no reaper mid-stop — a disabled or removed row's
/// proxy, which nothing else reclaims. Each dir name passes [`Service::parse`]
/// before any path is built from it; a name that fails is skipped. The recorded
/// deadline is honoured: a child still draining past it is killed.
fn reclaim_unwatched_orphans(
    threads: &BTreeMap<Service, SupervisorThread<Proxy>>,
    reaping: &BTreeMap<Service, JoinHandle<()>>,
    now_ms: u64,
) {
    for service in state_dir_services() {
        // A live supervisor, or a removed row's reaper mid-stop, owns the
        // child's stop: the sweep must never signal either, since a second
        // SIGTERM makes the child skip its drain.
        if threads.contains_key(&service) || reaping.contains_key(&service) {
            continue;
        }
        let Ok(path) = child_marker_path(&service) else {
            continue;
        };
        let Some(left) =
            super::gateway::stop_left_behind(&path, &format!("proxy {service}"), now_ms)
        else {
            continue;
        };
        if now_ms >= left.deadline_ms {
            terminate_pid(left.pid, true);
        }
    }
}

/// Send every proxy its one SIGTERM, each on its own thread so they drain
/// concurrently, then join them all — the live supervisors and the reaped
/// ones already stopping on their own threads — within the one budget.
#[cfg(unix)]
fn shutdown_threads(
    threads: BTreeMap<Service, SupervisorThread<Proxy>>,
    reaping: BTreeMap<Service, JoinHandle<()>>,
) {
    let mut joins = Vec::new();
    for (_, supervisor) in threads {
        joins.push(std::thread::spawn(move || {
            supervisor.shutdown(super::gateway::DAEMON_STOP_BUDGET);
        }));
    }
    joins.extend(reaping.into_values());
    for join in joins {
        let _ = join.join();
    }
}

/// Windows ends the daemon by `taskkill /F`, so the children outlive the
/// daemon and the next daemon reclaims them. The `drop` does stop each child
/// in series (`SupervisorThread::drop`), but no signal watcher ever sends a
/// stop on Windows, so this arm is unreachable in practice.
#[cfg(not(unix))]
fn shutdown_threads(
    threads: BTreeMap<Service, SupervisorThread<Proxy>>,
    reaping: BTreeMap<Service, JoinHandle<()>>,
) {
    drop(threads);
    for (_, join) in reaping {
        let _ = join.join();
    }
}

#[cfg(test)]
#[path = "../../tests/inline/daemon_proxy.rs"]
mod tests;
