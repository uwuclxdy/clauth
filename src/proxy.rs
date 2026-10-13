//! clauth proxies: the `~/.clauth/proxies.toml` registry, the files clauth
//! owns inside each proxy's state dir, PATH discovery of
//! `clauth-<service>-proxy` binaries and their `manifest`, and
//! `clauth proxy enable|disable`.
//!
//! Nothing here runs a proxy; the daemon does. The one child spawned here is
//! `<binary> manifest`, bounded the way `shunt check` is.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, TcpListener, TcpStream};
use std::ops::RangeInclusive;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};

use crate::gateway::{
    Bounded, MIN_ADMIN_KEY_LEN, SecretToken, ensure_token_file, run_bounded,
    run_bounded_cancellable,
};
use crate::lock::{StateLockHeld, with_state_lock};
use crate::plugin_probe::is_executable;
use crate::profile::{atomic_write_600, clauth_dir, mkdir_700, read_toml_file};

// ── the service name ────────────────────────────────────────────────────────

/// The `<service>` in `clauth-<service>-proxy`, validated against the core's
/// `SERVICE_RE` (`^[a-z0-9][a-z0-9-]{0,31}$`) before any path is built from
/// it, so a separator or a `..` never reaches one.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct Service(String);

/// The core's `SERVICE_RE`, in words.
const SERVICE_RULE: &str =
    "a service is 1 to 32 characters of a-z, 0-9 and -, the first a letter or digit";
const SERVICE_MAX_LEN: usize = 32;

impl Service {
    pub(crate) fn parse(raw: &str) -> Result<Self, InvalidService> {
        let allowed = |b: u8| b.is_ascii_lowercase() || b.is_ascii_digit();
        let mut bytes = raw.bytes();
        let valid = raw.len() <= SERVICE_MAX_LEN
            && bytes.next().is_some_and(allowed)
            && bytes.all(|b| allowed(b) || b == b'-');
        if valid {
            Ok(Self(raw.to_string()))
        } else {
            Err(InvalidService {
                raw: raw.to_string(),
            })
        }
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }

    /// `clauth-<service>-proxy`, the proxy's program name on `PATH`.
    pub(crate) fn binary_name(&self) -> String {
        format!("clauth-{}-proxy", self.0)
    }
}

/// Printed bare: the charset [`Service::parse`] admits holds nothing a
/// message or a command line would need quoted.
impl std::fmt::Display for Service {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// A name that is not a proxy service.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct InvalidService {
    raw: String,
}

impl std::fmt::Display for InvalidService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "invalid proxy service {:?}: {SERVICE_RULE}", self.raw)
    }
}

impl std::error::Error for InvalidService {}

/// A service named on the command line: a bad one is a usage error.
fn service_arg(raw: &str) -> Result<Service> {
    Service::parse(raw).map_err(|e| crate::usage_error(e.to_string()))
}

// ── files ───────────────────────────────────────────────────────────────────

/// `~/.clauth/proxies.toml`, the registry.
pub(crate) fn registry_path() -> Result<PathBuf> {
    Ok(clauth_dir()?.join("proxies.toml"))
}

/// `~/.clauth/proxies/<service>/`, the proxy's `CLAUTH_PROXY_STATE_DIR`. The
/// proxy keeps its own `accounts/` and `config.json` here; every file clauth
/// owns for the proxy sits here too, named `clauth-*` so it never collides
/// with one of the proxy's.
pub(crate) fn state_dir(service: &Service) -> Result<PathBuf> {
    Ok(clauth_dir()?.join("proxies").join(service.as_str()))
}

/// `~/.clauth/proxies/`, the root holding one state dir per service.
pub(crate) fn proxies_dir() -> Result<PathBuf> {
    Ok(clauth_dir()?.join("proxies"))
}

/// The proxy's admin token, its `CLAUTH_PROXY_ADMIN_TOKEN_FILE`.
pub(crate) fn admin_token_path(service: &Service) -> Result<PathBuf> {
    Ok(state_dir(service)?.join("clauth-admin-token"))
}

/// The supervisor's record of the proxy it spawned.
pub(crate) fn child_marker_path(service: &Service) -> Result<PathBuf> {
    Ok(state_dir(service)?.join("clauth-child.json"))
}

/// The proxy's stdout and stderr, as the supervisor captures them.
pub(crate) fn log_path(service: &Service) -> Result<PathBuf> {
    Ok(state_dir(service)?.join("clauth.log"))
}

/// Where a proxy on `port` serves: loopback only, the one bind the contract
/// admits.
pub(crate) fn bind(port: u16) -> SocketAddr {
    SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, port))
}

// ── the registry ────────────────────────────────────────────────────────────

/// One proxy clauth runs, as its `[<service>]` table records it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProxyRow {
    /// Fixed once enabled: the proxy's profiles carry it in their `base_url`.
    pub(crate) port: u16,
    pub(crate) enabled: bool,
    /// The proxy binary; `None` runs `clauth-<service>-proxy` off `PATH`.
    pub(crate) binary: Option<PathBuf>,
}

/// A row as TOML holds it; every field is checked into a [`ProxyRow`] so a
/// bad one is refused by name rather than by the parser's position.
#[derive(Debug, Serialize, Deserialize)]
struct DiskRow {
    port: Option<i64>,
    #[serde(default)]
    enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    binary: Option<PathBuf>,
}

/// `~/.clauth/proxies.toml`, one table per service, in service order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Registry {
    rows: BTreeMap<Service, ProxyRow>,
}

impl Registry {
    /// The registry as it is on disk; empty before the first enable. A
    /// reader's snapshot: every write goes through [`Registry::update`].
    pub(crate) fn load() -> Result<Self> {
        let path = registry_path()?;
        if !path
            .try_exists()
            .with_context(|| format!("failed to inspect {}", path.display()))?
        {
            return Ok(Self::default());
        }
        let disk: BTreeMap<String, DiskRow> = read_toml_file(&path)?;
        Self::from_disk(disk).with_context(|| format!("invalid proxy registry {}", path.display()))
    }

    fn from_disk(disk: BTreeMap<String, DiskRow>) -> Result<Self> {
        let mut rows = BTreeMap::new();
        for (name, row) in disk {
            let service = Service::parse(&name).map_err(|e| anyhow!("row {name:?}: {e}"))?;
            let port = match row.port {
                None => bail!("row {name:?}: port is missing"),
                Some(port) => u16::try_from(port)
                    .ok()
                    .filter(|port| *port != 0)
                    .ok_or_else(|| anyhow!("row {name:?}: port {port} is outside 1..=65535"))?,
            };
            if let Some(binary) = row.binary.as_deref().filter(|path| !path.is_absolute()) {
                bail!(
                    "row {name:?}: binary must be an absolute path, got {}; drop the key to run {} from PATH",
                    binary.display(),
                    service.binary_name()
                );
            }
            rows.insert(
                service,
                ProxyRow {
                    port,
                    enabled: row.enabled,
                    binary: row.binary,
                },
            );
        }
        let registry = Self { rows };
        registry.check_ports()?;
        Ok(registry)
    }

    /// Two proxies on one port would each fail to bind or answer as the
    /// other.
    fn check_ports(&self) -> Result<()> {
        let mut holders: BTreeMap<u16, &Service> = BTreeMap::new();
        for (service, row) in &self.rows {
            if let Some(first) = holders.insert(row.port, service) {
                bail!(
                    "rows {:?} and {:?} both hold port {}",
                    first.as_str(),
                    service.as_str(),
                    row.port
                );
            }
        }
        Ok(())
    }

    pub(crate) fn get(&self, service: &Service) -> Option<&ProxyRow> {
        self.rows.get(service)
    }

    /// Every row, in service-name order, for the daemon's per-row supervisors.
    pub(crate) fn iter(&self) -> impl Iterator<Item = (&Service, &ProxyRow)> {
        self.rows.iter()
    }

    /// The service whose row holds `port`.
    fn holder(&self, port: u16) -> Option<&Service> {
        self.rows
            .iter()
            .find(|(_, row)| row.port == port)
            .map(|(service, _)| service)
    }

    /// The one write path, [`crate::gateway::GatewayRecord::update`]'s shape:
    /// under the state flock, load the registry, hand the closure it, and
    /// save what it left only when it changed, so a no-op neither rewrites a
    /// hand-edited file nor moves its mtime.
    pub(crate) fn update<T>(f: impl FnOnce(&mut Registry) -> Result<T>) -> Result<T> {
        with_state_lock(|held| {
            let before = Self::load()?;
            let mut registry = before.clone();
            let out = f(&mut registry)?;
            if registry != before {
                registry.save(held)?;
            }
            Ok(out)
        })
    }

    /// Persist, witness-gated; called by [`Registry::update`] alone.
    fn save(&self, _held: &StateLockHeld) -> Result<()> {
        self.check_ports()?;
        let disk: BTreeMap<&str, DiskRow> = self
            .rows
            .iter()
            .map(|(service, row)| {
                (
                    service.as_str(),
                    DiskRow {
                        port: Some(i64::from(row.port)),
                        enabled: row.enabled,
                        binary: row.binary.clone(),
                    },
                )
            })
            .collect();
        atomic_write_600(&registry_path()?, toml::to_string_pretty(&disk)?)
            .context("failed to write proxies.toml")
    }
}

/// The refusal for a service with no row, naming the verb that writes one.
pub(crate) fn not_registered(service: &Service) -> anyhow::Error {
    crate::usage_error(format!(
        "no proxy {:?} is registered; register it with `clauth proxy enable {service}`",
        service.as_str()
    ))
}

// ── discovery ───────────────────────────────────────────────────────────────

/// A `clauth-<service>-proxy` found on `PATH`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Found {
    pub(crate) service: Service,
    pub(crate) binary: PathBuf,
}

/// The suffixes a proxy's file name may carry on Windows, best first: a
/// native `.exe`, then a `.cmd`/`.bat` shim, then an extensionless file, which
/// there is an npm-style sh shim no process spawn runs
/// (`runtime::resolve_cli_command` prefers `.exe` for the same reason).
const WINDOWS_SUFFIXES: &[&str] = &[".exe", ".cmd", ".bat", ""];

/// The suffixes discovery accepts on this platform, best first.
const DISCOVERY_SUFFIXES: &[&str] = if cfg!(windows) {
    WINDOWS_SUFFIXES
} else {
    &[""]
};

/// Every `clauth-<service>-proxy` program in the `PATH` value `path`, one per
/// service in service order: the first dir holding a service wins, as a
/// shell's lookup would, and inside one dir the best suffix in
/// [`DISCOVERY_SUFFIXES`]. A file that is not executable, or whose
/// `<service>` is not a service, is skipped, and so is a relative dir, which
/// resolves against whatever working directory the reader has.
pub(crate) fn discover(path: &OsStr) -> Vec<Found> {
    let mut found: BTreeMap<Service, PathBuf> = BTreeMap::new();
    for dir in std::env::split_paths(path).filter(|dir| dir.is_absolute()) {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        let candidates = entries.flatten().map(|entry| entry.path()).filter(|path| {
            path.file_name()
                .and_then(OsStr::to_str)
                .and_then(|name| service_of(name, DISCOVERY_SUFFIXES))
                .is_some()
                && is_executable(path)
        });
        for (service, binary) in best_in_dir(candidates, DISCOVERY_SUFFIXES) {
            found.entry(service).or_insert(binary);
        }
    }
    found
        .into_iter()
        .map(|(service, binary)| Found { service, binary })
        .collect()
}

/// Of one dir's `clauth-<service>-proxy` files, each service's with the
/// suffix earliest in `suffixes`, whatever order the dir lists them in.
fn best_in_dir(
    paths: impl IntoIterator<Item = PathBuf>,
    suffixes: &[&str],
) -> BTreeMap<Service, PathBuf> {
    let mut best: BTreeMap<Service, (usize, PathBuf)> = BTreeMap::new();
    for path in paths {
        let Some((service, rank)) = path
            .file_name()
            .and_then(OsStr::to_str)
            .and_then(|name| service_of(name, suffixes))
        else {
            continue;
        };
        if best.get(&service).is_none_or(|(held, _)| rank < *held) {
            best.insert(service, (rank, path));
        }
    }
    best.into_iter()
        .map(|(service, (_, path))| (service, path))
        .collect()
}

/// The service a `clauth-<service>-proxy[<suffix>]` file name carries, with
/// its suffix's rank in `suffixes`.
fn service_of(file_name: &str, suffixes: &[&str]) -> Option<(Service, usize)> {
    suffixes.iter().enumerate().find_map(|(rank, suffix)| {
        let split = file_name.len().checked_sub(suffix.len())?;
        let (stem, tail) = (file_name.get(..split)?, file_name.get(split..)?);
        if !tail.eq_ignore_ascii_case(suffix) {
            return None;
        }
        let name = stem.strip_prefix("clauth-")?.strip_suffix("-proxy")?;
        Service::parse(name).ok().map(|service| (service, rank))
    })
}

// ── the manifest ────────────────────────────────────────────────────────────

/// The contract major this clauth speaks: a manifest or a `/health` naming
/// another is refused, since no route shape clauth knows applies to it.
pub(crate) const SPOKEN_MAJOR: u64 = 1;

/// How long `<binary> manifest` may run before clauth stops it. Measured
/// 2026-09-29 on the core's fixture proxy under bun (`bun
/// test/fixtures/serve.ts manifest`): 28 to 39 ms over 10 runs at load
/// average 11. 5 s is two orders over the slowest, the margin
/// [`crate::gateway::CHECK_TIMEOUT`] keeps over `shunt check`'s cold run.
pub(crate) const MANIFEST_TIMEOUT: Duration = Duration::from_secs(5);

/// A manifest past this many bytes is not one: the fixture's is 111.
const MANIFEST_STDOUT_LIMIT: usize = 64 * 1024;

/// The contract's ceiling on `drain_secs`.
const DRAIN_SECS_MAX: i64 = 3600;

/// What `<binary> manifest` declares, checked against the contract.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Manifest {
    pub(crate) service: Service,
    pub(crate) display_name: String,
    pub(crate) description: Option<String>,
    pub(crate) version: String,
    /// `"MAJOR.MINOR"`, its major the one clauth speaks.
    pub(crate) contract: String,
    pub(crate) capabilities: Vec<String>,
    /// How long SIGTERM may take to drain, 0 when the manifest omits it.
    pub(crate) drain_secs: u32,
}

#[derive(Deserialize)]
struct ManifestBody {
    service: String,
    display_name: String,
    description: Option<String>,
    version: String,
    contract: String,
    capabilities: Vec<String>,
    drain_secs: Option<i64>,
}

/// Why a proxy's manifest was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ManifestRefusal {
    /// The binary is not there to run.
    Missing { binary: PathBuf },
    /// `manifest` outran its bound and was stopped.
    TimedOut { binary: PathBuf, after: Duration },
    /// `manifest` exited unsuccessfully.
    Failed { binary: PathBuf, code: Option<i32> },
    /// `manifest` printed more than a manifest could be.
    Oversize { binary: PathBuf, limit: usize },
    /// Its stdout is not the manifest JSON; the parser's reason.
    Unreadable { binary: PathBuf, reason: String },
    /// Its `service` is not the one its file name carries.
    ServiceMismatch {
        binary: PathBuf,
        named: Service,
        declared: String,
    },
    /// Its `contract` is not `"MAJOR.MINOR"`.
    BadContract { binary: PathBuf, contract: String },
    /// Its contract major is not the one clauth speaks.
    ForeignMajor {
        binary: PathBuf,
        contract: String,
        major: u64,
    },
    /// Its `drain_secs` is outside the contract's range.
    DrainOutOfRange { binary: PathBuf, drain_secs: i64 },
}

impl std::fmt::Display for ManifestRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ManifestRefusal::Missing { binary } => {
                write!(f, "cannot run {}: no such file", binary.display())
            }
            ManifestRefusal::TimedOut { binary, after } => write!(
                f,
                "`{} manifest` ran past {after:?} and was stopped",
                binary.display()
            ),
            ManifestRefusal::Failed { binary, code } => {
                let status = match code {
                    Some(code) => format!("exit {code}"),
                    None => "killed by a signal".to_string(),
                };
                write!(f, "`{} manifest` failed ({status})", binary.display())
            }
            ManifestRefusal::Oversize { binary, limit } => write!(
                f,
                "`{} manifest` printed more than {limit} bytes, which is no manifest",
                binary.display()
            ),
            ManifestRefusal::Unreadable { binary, reason } => write!(
                f,
                "`{} manifest` printed no contract manifest: {reason:?}",
                binary.display()
            ),
            ManifestRefusal::ServiceMismatch {
                binary,
                named,
                declared,
            } => write!(
                f,
                "{} declares service {declared:?} in its manifest, and its name says {:?}",
                binary.display(),
                named.as_str()
            ),
            ManifestRefusal::BadContract { binary, contract } => write!(
                f,
                "{} declares contract {contract:?}, which is not MAJOR.MINOR",
                binary.display()
            ),
            ManifestRefusal::ForeignMajor {
                binary,
                contract,
                major,
            } => write!(
                f,
                "{} speaks contract major {major} ({contract:?}), and clauth speaks major {SPOKEN_MAJOR}",
                binary.display()
            ),
            ManifestRefusal::DrainOutOfRange { binary, drain_secs } => write!(
                f,
                "{} declares drain_secs {drain_secs}, outside 0..={DRAIN_SECS_MAX}",
                binary.display()
            ),
        }
    }
}

impl std::error::Error for ManifestRefusal {}

/// `<binary> manifest`, bounded by [`MANIFEST_TIMEOUT`], checked against the
/// contract and against `service`, the one `binary`'s name carries.
pub(crate) fn read_manifest(service: &Service, binary: &Path) -> Result<Manifest> {
    read_manifest_within(service, binary, MANIFEST_TIMEOUT)
}

/// [`read_manifest`] under a cancellation flag: a stop set while the `manifest`
/// child runs kills and reaps it at once, so a shutdown preempts a wedged read
/// instead of waiting its bound out.
pub(crate) fn read_manifest_cancellable(
    service: &Service,
    binary: &Path,
    cancel: &std::sync::atomic::AtomicBool,
) -> Result<Manifest> {
    read_manifest_within_cancel(service, binary, MANIFEST_TIMEOUT, Some(cancel))
}

/// [`read_manifest`] under `bound`.
fn read_manifest_within(service: &Service, binary: &Path, bound: Duration) -> Result<Manifest> {
    read_manifest_within_cancel(service, binary, bound, None)
}

/// [`read_manifest_within`] under an optional cancellation flag.
fn read_manifest_within_cancel(
    service: &Service,
    binary: &Path,
    bound: Duration,
    cancel: Option<&std::sync::atomic::AtomicBool>,
) -> Result<Manifest> {
    let refuse = |refusal: ManifestRefusal| Err(refusal.into());
    let path = binary.to_path_buf();
    let mut command = Command::new(binary);
    command.arg("manifest");
    let bounded = match cancel {
        Some(cancel) => run_bounded_cancellable(
            &mut command,
            binary,
            bound,
            Some(MANIFEST_STDOUT_LIMIT + 1),
            cancel,
        ),
        None => run_bounded(&mut command, binary, bound, Some(MANIFEST_STDOUT_LIMIT + 1)),
    };
    let exited = match bounded? {
        Bounded::Missing => return refuse(ManifestRefusal::Missing { binary: path }),
        Bounded::TimedOut => {
            return refuse(ManifestRefusal::TimedOut {
                binary: path,
                after: bound,
            });
        }
        Bounded::Exited(exited) => exited,
    };
    if !exited.status.success() {
        return refuse(ManifestRefusal::Failed {
            binary: path,
            code: exited.status.code(),
        });
    }
    let stdout = exited.stdout();
    if stdout.len() > MANIFEST_STDOUT_LIMIT {
        return refuse(ManifestRefusal::Oversize {
            binary: path,
            limit: MANIFEST_STDOUT_LIMIT,
        });
    }
    let body: ManifestBody = match serde_json::from_slice(&stdout) {
        Ok(body) => body,
        Err(e) => {
            return refuse(ManifestRefusal::Unreadable {
                binary: path,
                reason: e.to_string(),
            });
        }
    };
    if body.service != service.as_str() {
        return refuse(ManifestRefusal::ServiceMismatch {
            binary: path,
            named: service.clone(),
            declared: body.service,
        });
    }
    let Some((major, _minor)) = contract_version(&body.contract) else {
        return refuse(ManifestRefusal::BadContract {
            binary: path,
            contract: body.contract,
        });
    };
    if major != SPOKEN_MAJOR {
        return refuse(ManifestRefusal::ForeignMajor {
            binary: path,
            contract: body.contract,
            major,
        });
    }
    let drain_secs = body.drain_secs.unwrap_or(0);
    let Some(drain_secs) = u32::try_from(drain_secs)
        .ok()
        .filter(|secs| i64::from(*secs) <= DRAIN_SECS_MAX)
    else {
        return refuse(ManifestRefusal::DrainOutOfRange {
            binary: path,
            drain_secs,
        });
    };
    Ok(Manifest {
        service: service.clone(),
        display_name: body.display_name,
        description: body.description,
        version: body.version,
        contract: body.contract,
        capabilities: body.capabilities,
        drain_secs,
    })
}

/// A `"MAJOR.MINOR"` contract version, each part decimal digits alone: the
/// one reading of `contract` that `enable` and `clauth proxy check` share.
pub(crate) fn contract_version(raw: &str) -> Option<(u64, u64)> {
    let (major, minor) = raw.split_once('.')?;
    let part = |digits: &str| {
        (!digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()))
            .then(|| digits.parse::<u64>().ok())
            .flatten()
    };
    Some((part(major)?, part(minor)?))
}

// ── enable and disable ──────────────────────────────────────────────────────

/// Where `enable` picks a port: below every OS's ephemeral range, so a restart never finds its port held by an outgoing connection.
const PORT_RANGE: RangeInclusive<u16> = 9101..=9199;

/// `clauth proxy enable <service> [--port N]`: check the proxy's manifest,
/// settle its port, create its state dir, mint its admin token and write its
/// row enabled. Answers the bind the proxy serves on. `path` is the `PATH`
/// value the binary is resolved in; the row records that absolute hit, so a
/// daemon with a narrower `PATH` still finds it, and with no hit keeps the
/// binary it recorded before while that still exists.
pub(crate) fn enable(raw: &str, port: Option<u16>, path: Option<&OsStr>) -> Result<SocketAddr> {
    enable_with(raw, port, path, probe_port)
}

/// [`enable`] with its port probe handed in.
fn enable_with(
    raw: &str,
    port: Option<u16>,
    path: Option<&OsStr>,
    probe: impl Fn(u16) -> Result<(), PortBusy>,
) -> Result<SocketAddr> {
    let service = service_arg(raw)?;
    let snapshot = Registry::load()?;
    let found = path.and_then(|path| {
        discover(path)
            .into_iter()
            .find(|found| found.service == service)
    });
    let binary = match found {
        Some(found) => found.binary,
        None => match snapshot.get(&service).and_then(|row| row.binary.clone()) {
            Some(recorded) if recorded.exists() => recorded,
            Some(recorded) => {
                return Err(crate::usage_error(format!(
                    "no {} on PATH, and the recorded {} is gone; install the proxy, then enable it again",
                    service.binary_name(),
                    recorded.display()
                )));
            }
            None => {
                return Err(crate::usage_error(format!(
                    "no {} on PATH; install the proxy, then enable it again",
                    service.binary_name()
                )));
            }
        },
    };
    // The child and the port probes run with no lock held: each can take
    // seconds, and every other writer waits on the flock. The write re-checks
    // the chosen port against the rows as they are under it.
    read_manifest(&service, &binary)?;
    let chosen = pick_port(&snapshot, &service, port, probe)?;
    Registry::update(|registry| {
        let port = claim_port(registry, &service, port, chosen)?;
        let dir = state_dir(&service)?;
        mkdir_700(&dir).with_context(|| format!("failed to create {}", dir.display()))?;
        ensure_proxy_token(&service)?;
        registry.rows.insert(
            service.clone(),
            ProxyRow {
                port,
                enabled: true,
                binary: Some(binary),
            },
        );
        Ok(bind(port))
    })
}

/// `clauth proxy disable <service>`: the row stays, with its port, and so do
/// the token and the state dir, so the proxy's profiles work again after a
/// later enable.
pub(crate) fn disable(raw: &str) -> Result<()> {
    let service = service_arg(raw)?;
    Registry::update(|registry| match registry.rows.get_mut(&service) {
        Some(row) => {
            row.enabled = false;
            Ok(())
        }
        None => Err(not_registered(&service)),
    })
}

/// The proxy's admin token, minted on first use.
pub(crate) fn ensure_proxy_token(service: &Service) -> Result<SecretToken> {
    let path = admin_token_path(service)?;
    ensure_token_file(&path, |token| {
        if token.len() < MIN_ADMIN_KEY_LEN {
            bail!(
                "the admin token of proxy {:?} in {} is shorter than {MIN_ADMIN_KEY_LEN} characters; delete the file and clauth mints a new one",
                service.as_str(),
                path.display()
            );
        }
        Ok(())
    })
}

/// The port `service` serves on: its row's, else `asked`, else the first of
/// [`PORT_RANGE`] that no row holds and `probe` finds free.
fn pick_port(
    registry: &Registry,
    service: &Service,
    asked: Option<u16>,
    probe: impl Fn(u16) -> Result<(), PortBusy>,
) -> Result<u16> {
    if let Some(port) = recorded_port(registry, service, asked)? {
        return Ok(port);
    }
    if let Some(asked) = asked {
        if let Some(holder) = registry.holder(asked) {
            return Err(crate::usage_error(format!(
                "port {asked} is recorded for proxy {:?}; pick another --port",
                holder.as_str()
            )));
        }
        return match probe(asked) {
            Ok(()) => Ok(asked),
            Err(PortBusy::Answers) => Err(crate::usage_error(format!(
                "port {asked} already answers on 127.0.0.1; pick another --port, or drop it for a free one"
            ))),
            Err(PortBusy::Unbindable(kind)) => Err(crate::usage_error(format!(
                "port {asked} cannot be bound on 127.0.0.1 ({kind}); pick another --port, or drop it for a free one"
            ))),
        };
    }
    PORT_RANGE
        .into_iter()
        .find(|port| registry.holder(*port).is_none() && probe(*port).is_ok())
        .ok_or_else(|| {
            crate::usage_error(format!(
                "every port in {}..={} is recorded for a proxy or taken on 127.0.0.1; pick one with --port",
                PORT_RANGE.start(),
                PORT_RANGE.end()
            ))
        })
}

/// The port `service` takes as `registry` stands under the flock: its row's,
/// which may have appeared since the pick, else `chosen`, unless a row
/// recorded it meanwhile.
fn claim_port(
    registry: &Registry,
    service: &Service,
    asked: Option<u16>,
    chosen: u16,
) -> Result<u16> {
    if let Some(port) = recorded_port(registry, service, asked)? {
        return Ok(port);
    }
    if let Some(holder) = registry.holder(chosen) {
        bail!(
            "port {chosen} was recorded for proxy {:?} while clauth checked it; run the enable again",
            holder.as_str()
        );
    }
    Ok(chosen)
}

/// The port `service`'s row records, fixed since its profiles carry it; a
/// different `asked` one is refused.
fn recorded_port(
    registry: &Registry,
    service: &Service,
    asked: Option<u16>,
) -> Result<Option<u16>> {
    let Some(row) = registry.get(service) else {
        return Ok(None);
    };
    match asked {
        Some(asked) if asked != row.port => Err(crate::usage_error(format!(
            "proxy {:?} keeps its recorded port {}, which its profiles' base_url carries; drop --port",
            service.as_str(),
            row.port
        ))),
        _ => Ok(Some(row.port)),
    }
}

/// Why a loopback port is not free.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PortBusy {
    /// A connect to it is accepted, or never answered.
    Answers,
    /// Nothing answers, but a listener cannot bind it.
    Unbindable(std::io::ErrorKind),
}

/// Whether `port` is free on loopback: nothing answers a connect there, and a
/// listener can bind it. The bind alone is not enough where a specific bind
/// is admitted beside another program's wildcard listener (`SO_REUSEADDR` on
/// macOS and the BSDs). Up to [`crate::gateway::HEALTH_CONNECT_SECS`] per
/// port, so it never runs under the state flock.
fn probe_port(port: u16) -> Result<(), PortBusy> {
    if !loopback_refuses(port) {
        return Err(PortBusy::Answers);
    }
    TcpListener::bind(bind(port))
        .map(drop)
        .map_err(|e| PortBusy::Unbindable(e.kind()))
}

/// Whether a connect to `127.0.0.1:port` is refused, bounded like the
/// `/health` probe's connect (Windows refuses only after about 2 s); a connect
/// that is accepted or times out means something holds the port.
fn loopback_refuses(port: u16) -> bool {
    matches!(
        TcpStream::connect_timeout(
            &bind(port),
            Duration::from_secs(crate::gateway::HEALTH_CONNECT_SECS)
        ),
        Err(e) if e.kind() == std::io::ErrorKind::ConnectionRefused
    )
}

#[cfg(test)]
#[path = "../tests/inline/proxy.rs"]
mod tests;
