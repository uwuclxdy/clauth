//! The managed shunt gateway's engine: the `~/.clauth/gateway.toml` record,
//! the admin token file, config discovery, bind resolution, the env file
//! reader, the admin-key edit behind its `shunt check` gate, the `/health`
//! version floor, the standalone-store move and the admin pool read naming
//! the `chatgpt_oauth` providers with no login.
//!
//! No function here spawns, signals or supervises a process other than the
//! bounded children [`run_bounded`] runs (`shunt check`, and a clauth proxy's
//! `manifest` for `crate::proxy`); the daemon and the Services card call these.
//! Every edit of the user's config lands only after `shunt check` passed on
//! a sibling copy run with the gateway's env file and stores over
//! the calling process's own env; the supervisor must spawn with the same
//! [`GatewayEnv::apply`] over the same inherited env.

#![expect(
    dead_code,
    reason = "`GatewayEnv::get` and `CheckStderr::text` are surfaces no caller reaches yet"
)]

use std::collections::{BTreeMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::Write as _;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use toml_edit::{Array, ArrayOfTables, DocumentMut, InlineTable, Item, Table, TableLike, Value};

use crate::lock::{StateLockHeld, with_state_lock};
use crate::profile::{
    atomic_write_600, clauth_dir, home_dir, mkdir_700, read_toml_file, tmp_sibling,
};

/// shunt's own bind when neither `SHUNT_SERVER__BIND` nor `[server].bind`
/// sets one (`Config::default`, shunt `config.rs`).
pub(crate) const SHUNT_DEFAULT_BIND: SocketAddr =
    SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(127, 0, 0, 1), 3001));

/// The env override shunt's figment layer maps onto `[server].bind`.
pub(crate) const BIND_ENV: &str = "SHUNT_SERVER__BIND";

const CONFIG_BIND: &str = "[server].bind";

/// The oldest shunt clauth supervises: 0.48.0 is the first release carrying
/// antigravity pooling (`SHUNT_ANTIGRAVITY_ACCOUNTS_DIR`,
/// `/admin/api/accounts/antigravity`).
pub(crate) const VERSION_FLOOR: ShuntVersion = ShuntVersion {
    major: 0,
    minor: 48,
    patch: 0,
    pre_release: false,
};

/// The `/health` probe's connect bound. Windows reports a loopback connect to
/// a closed port as refused only after 2012.8-2088.7 ms (measured 2026-09-27
/// on Windows 11 24H2, 40 connects over 127.0.0.1 and ::1), so a shorter
/// bound reads an empty port there as a timeout; 4 s is about 1.9 times that
/// maximum, and the usage fetch's connect bound too.
pub(crate) const HEALTH_CONNECT_SECS: u64 = 4;

/// The probe's response bound once connected: shunt answers `/health`
/// outside its concurrency gate, so a live gateway answers in milliseconds,
/// and this is what keeps a wedged answerer from parking the caller.
pub(crate) const HEALTH_RESPONSE_SECS: u64 = 2;

/// The probe's end-to-end ceiling, the two phase bounds added: ureq re-arms
/// its response deadline before every wait (`oauth::TOKEN_HTTP_DEADLINES`),
/// so the sum is enforced as one global bound as well.
pub(crate) const HEALTH_PROBE_TIMEOUT: Duration =
    Duration::from_secs(HEALTH_CONNECT_SECS + HEALTH_RESPONSE_SECS);

/// shunt's `/health` body is 34 bytes; a body reaching this is not shunt.
const HEALTH_BODY_LIMIT: u64 = 4096;

/// The `id` of clauth's own `[[server.admin.write_keys]]` entry.
pub(crate) const WRITE_KEY_ID: &str = "clauth";

/// shunt refuses a `write_keys` key shorter than this (`admin_keys.rs`).
pub(crate) const MIN_ADMIN_KEY_LEN: usize = 32;

/// shunt's `CONFIG_FILENAMES`, probed in this order in every search dir.
const CONFIG_FILENAMES: [&str; 3] = ["shunt.toml", "shunt.yaml", "shunt.yml"];

// ── the record ──────────────────────────────────────────────────────────────

/// `~/.clauth/gateway.toml`: which shunt config clauth adopted and how it
/// runs the gateway over it. Never named `shunt.toml`: shunt's own cwd
/// discovery would find it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct GatewayRecord {
    config: PathBuf,
    /// The shunt binary; `None` runs `shunt` off `PATH`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) binary: Option<PathBuf>,
    /// An EnvironmentFile the gateway's env is read from at each spawn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) env_file: Option<PathBuf>,
    #[serde(default)]
    pub(crate) disabled: bool,
}

impl GatewayRecord {
    /// A record adopting `config`, which must be absolute, TOML, and exist.
    /// It holds the config's canonical target: a symlinked config is run and
    /// edited at its target, so the user's link survives every edit, and a
    /// re-pointed link takes a fresh adoption.
    pub(crate) fn new(config: PathBuf) -> Result<Self> {
        check_adoptable(&config)?;
        let config = std::fs::canonicalize(&config)
            .with_context(|| format!("cannot resolve the shunt config {}", config.display()))?;
        check_adoptable(&config)?;
        Ok(Self {
            config,
            binary: None,
            env_file: None,
            disabled: false,
        })
    }

    /// The adopted config, always absolute.
    pub(crate) fn config(&self) -> &Path {
        &self.config
    }

    /// The binary `shunt check` and the gateway run as.
    pub(crate) fn shunt_binary(&self) -> &Path {
        self.binary.as_deref().unwrap_or(Path::new("shunt"))
    }

    /// The record as it is on disk; `None` before any adoption. A reader's
    /// snapshot: every write goes through [`GatewayRecord::update`].
    pub(crate) fn load() -> Result<Option<Self>> {
        let path = record_path()?;
        if !path
            .try_exists()
            .with_context(|| format!("failed to inspect {}", path.display()))?
        {
            return Ok(None);
        }
        let record: Self = read_toml_file(&path)?;
        check_record(&record)
            .with_context(|| format!("invalid gateway record {}", path.display()))?;
        Ok(Some(record))
    }

    /// The one write path, [`crate::codex_profiles::CodexState::update`]'s
    /// shape: under the state flock, load the record (`None` before any
    /// adoption), hand the closure that slot, and save what it left there
    /// only when it changed, so a no-op neither rewrites a hand-edited file
    /// nor moves its mtime. [`GatewayRecord::save`] is reachable from here
    /// alone, so no snapshot loaded outside the hold ever lands. Adoption
    /// fills the empty slot; a closure that empties a held one refuses, since
    /// an update replaces the record and never removes it.
    pub(crate) fn update<T>(f: impl FnOnce(&mut Option<GatewayRecord>) -> Result<T>) -> Result<T> {
        with_state_lock(|held| {
            let before = Self::load()?;
            let mut slot = before.clone();
            let out = f(&mut slot)?;
            match (&before, &slot) {
                (Some(_), None) => bail!(
                    "an update never removes the gateway record {}",
                    record_path()?.display()
                ),
                (_, Some(record)) if slot != before => record.save(held)?,
                _ => {}
            }
            Ok(out)
        })
    }

    /// Persist, witness-gated; called by [`GatewayRecord::update`] alone.
    fn save(&self, _held: &StateLockHeld) -> Result<()> {
        check_record(self)?;
        atomic_write_600(&record_path()?, toml::to_string_pretty(self)?)
            .context("failed to write gateway.toml")
    }
}

pub(crate) fn record_path() -> Result<PathBuf> {
    Ok(clauth_dir()?.join("gateway.toml"))
}

// ── the hold ────────────────────────────────────────────────────────────────

/// The daemon instance a gateway hold names: the daemon's pid and that pid's
/// process start time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct DaemonIdentity {
    pub(crate) pid: u32,
    /// The process start time ([`crate::daemon::gateway::process_start_time`]);
    /// `None` where it could not be read, which no identity match ever names.
    pub(crate) start: Option<String>,
}

impl DaemonIdentity {
    /// Whether a hold naming `other` names this same daemon instance. Identity
    /// is pid AND start token, never the pid alone: a hold naming this pid
    /// with another token (or no token) names another instance.
    pub(crate) fn is_named_by(&self, other: &DaemonIdentity) -> bool {
        self.start.is_some() && self.pid == other.pid && self.start == other.start
    }
}

/// `~/.clauth/gateway-hold`: the TUI's transient "stop the gateway until the
/// next daemon start". Holds the [`DaemonIdentity`] of the daemon it names.
const HOLD_FILE: &str = "gateway-hold";

pub(crate) fn hold_path() -> Result<PathBuf> {
    Ok(clauth_dir()?.join(HOLD_FILE))
}

/// Why the daemon holding the singleton cannot be named: either none holds it,
/// or one holds it (or presence is unreadable) but its identity cannot be
/// read. [`recorded_env_for_live_daemon`] branches on the first to fall back
/// to the caller's own env; every other caller treats both as an error.
#[derive(Debug)]
enum NoHolder {
    /// No daemon holds the singleton.
    NotHeld,
    /// A daemon holds (or presence is unreadable) but its identity cannot be
    /// read.
    Unreadable(anyhow::Error),
}

impl std::fmt::Display for NoHolder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NoHolder::NotHeld => f.write_str("no daemon holds the clauth singleton"),
            NoHolder::Unreadable(e) => e.fmt(f),
        }
    }
}

impl std::error::Error for NoHolder {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            // The head already prints `e`'s own message, so the chain goes on
            // from `e`'s cause; returning `e` would print it twice under `{:#}`.
            NoHolder::Unreadable(e) => e.source(),
            NoHolder::NotHeld => None,
        }
    }
}

/// The identity of the daemon that holds the singleton now: presence by the
/// flock first ([`crate::daemon::singleton_held`]), then the pid sidecar, then
/// that pid's process start time, which narrows the recycled-pid window. The
/// one way every writer names the running daemon — [`write_hold`] and the
/// start-time env record — and the one way [`inherited_env`] reads it.
fn current_holder_identity() -> Result<DaemonIdentity, NoHolder> {
    // The sidecar is informational and never removed: a stale pid left by a
    // dead daemon, or recycled onto an unrelated process, must not read as a
    // daemon to name. Presence is the flock (`status_probe`'s precedent).
    match crate::daemon::singleton_held() {
        Ok(true) => {}
        Ok(false) => return Err(NoHolder::NotHeld),
        Err(e) => return Err(NoHolder::Unreadable(e)),
    }
    let pid = crate::daemon::holder_pid().ok_or_else(|| {
        NoHolder::Unreadable(anyhow!(
            "a clauth daemon is running but its pid is unreadable"
        ))
    })?;
    let start = crate::daemon::gateway::process_start_time(pid).ok_or_else(|| {
        NoHolder::Unreadable(anyhow!(
            "cannot read the start time of the daemon (pid {pid})"
        ))
    })?;
    Ok(DaemonIdentity {
        pid,
        start: Some(start),
    })
}

/// Write a hold naming the daemon that holds the singleton now. The TUI's
/// transient "stop the gateway" writes it; that daemon honours it and any
/// later daemon ignores and removes it, so a restart clears it by
/// construction. Errors when no daemon holds the singleton, its pid sidecar is
/// unreadable, or its start time cannot be read.
pub(crate) fn write_hold() -> Result<()> {
    let identity = current_holder_identity()?;
    let path = hold_path()?;
    atomic_write_600(&path, serde_json::to_vec(&identity)?)
        .with_context(|| format!("failed to write {}", path.display()))
}

/// Remove the hold; a missing one is fine (a release before a hold was ever
/// written).
pub(crate) fn remove_hold() -> Result<()> {
    let path = hold_path()?;
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("failed to remove {}", path.display())),
    }
}

// ── the daemon's start-time env ─────────────────────────────────────────────

/// `~/.clauth/gateway-env.json`: the env the running daemon inherited for the
/// keys the store move reads, recorded at its start with the
/// [`DaemonIdentity`] of the daemon that wrote them, so a reader can tell the
/// running daemon's record from a dead one's.
const ENV_FILE: &str = "gateway-env.json";

/// Every env key the store plan, the move and the move's silent-proof bind
/// read from the inherited env. One list: the daemon's record stores exactly
/// these (each present, losslessly) and the no-daemon fallback reads exactly
/// these from the caller's env, so a value only the daemon inherited is seen,
/// and a value only the caller's shell holds cannot make the plan and the
/// proof disagree. None is a secret: two homes, a Windows home and a bind
/// address.
const INHERITED_KEYS: [&str; 4] = ["CODEX_HOME", "HOME", "USERPROFILE", BIND_ENV];

fn daemon_env_path() -> Result<PathBuf> {
    Ok(clauth_dir()?.join(ENV_FILE))
}

/// The daemon's start-time env record. Each value is stored as its
/// platform-native bytes ([`env_value_bytes`]), never lossy-converted, so a
/// non-UTF-8 `HOME` survives the record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct DaemonEnvRecord {
    identity: DaemonIdentity,
    /// Each key from [`INHERITED_KEYS`] the daemon inherited, as its
    /// platform-native bytes; an unset key is absent.
    env: Vec<(String, Vec<u8>)>,
}

/// Write the daemon's start-time env record, naming the daemon that holds the
/// singleton now. The daemon calls this at start; the store plan and move read
/// it through [`inherited_env`]. Best-effort at its call site: a daemon that
/// cannot write the record still runs, and the plan refuses until it restarts.
pub(crate) fn write_daemon_env() -> Result<()> {
    write_daemon_env_from(|key| std::env::var_os(key))
}

/// [`write_daemon_env`] over an injected env source, so a test drives the
/// capture (its key list and bytes) without reading the process's own env.
fn write_daemon_env_from(var: impl Fn(&str) -> Option<OsString>) -> Result<()> {
    write_env_record(DaemonEnvRecord {
        identity: current_holder_identity()?,
        env: INHERITED_KEYS
            .iter()
            .filter_map(|key| var(key).map(|value| ((*key).to_string(), env_value_bytes(&value))))
            .collect(),
    })
}

fn write_env_record(record: DaemonEnvRecord) -> Result<()> {
    let path = daemon_env_path()?;
    atomic_write_600(&path, serde_json::to_vec(&record)?)
        .with_context(|| format!("failed to write {}", path.display()))
}

/// The identity the daemon's env record names, `None` when no record reads.
/// Whether that daemon still holds the singleton is not checked here
/// ([`inherited_env`] checks it); a caller tracking which env the plan
/// resolves against compares it across reads.
pub(crate) fn recorded_daemon_identity() -> Option<DaemonIdentity> {
    read_env_record()
        .ok()
        .flatten()
        .map(|record| record.identity)
}

fn read_env_record() -> Result<Option<DaemonEnvRecord>> {
    let path = daemon_env_path()?;
    match std::fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .with_context(|| format!("failed to parse {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("failed to read {}", path.display())),
    }
}

/// The daemon's recorded env as (key, value) pairs, only when a record exists
/// and names the daemon holding the singleton now ([`current_holder_identity`]'s
/// one way). `None` means no daemon holds the singleton: this process's own
/// env is the inherited one. A daemon that holds the singleton (or whose
/// presence cannot be read) but left no matching record — missing, stale,
/// unreadable, unparseable, or with a value that does not decode — refuses the
/// plan and move with [`StoreMoveRefusal::DaemonEnvUnrecorded`], never silently
/// reading the caller's env under a live daemon.
fn recorded_env_for_live_daemon() -> Result<Option<Vec<(OsString, OsString)>>> {
    // One presence probe: `current_holder_identity` reads the flock. No daemon
    // holds it -> this process's own env is the inherited one. A holder (or
    // presence unreadable) needs a record naming it, else the plan refuses.
    let current = match current_holder_identity() {
        Err(NoHolder::NotHeld) => return Ok(None),
        Err(NoHolder::Unreadable(_)) => return Err(StoreMoveRefusal::DaemonEnvUnrecorded.into()),
        Ok(identity) => identity,
    };
    match read_env_record() {
        Ok(Some(record)) if record.identity.is_named_by(&current) => recorded_pairs(&record)
            .map(Some)
            .map_err(|_| StoreMoveRefusal::DaemonEnvUnrecorded.into()),
        // Missing, stale, unreadable or unparseable: refuse until this daemon
        // restarts and records its env.
        _ => Err(StoreMoveRefusal::DaemonEnvUnrecorded.into()),
    }
}

/// The inherited env the store plan and move resolve against, and where a
/// `CODEX_HOME` it holds came from: the running daemon's record, else this
/// process's own env, with a clauth codex home scrubbed the way `start daemon`
/// scrubs it ([`crate::runtime::scrub_clauth_homes`]). A daemon that holds the
/// singleton but recorded no env refuses with
/// [`StoreMoveRefusal::DaemonEnvUnrecorded`].
pub(crate) fn inherited_env() -> Result<(Vec<(OsString, OsString)>, CodexHomeSource)> {
    match recorded_env_for_live_daemon()? {
        Some(pairs) => Ok((pairs, CodexHomeSource::DaemonRecord)),
        None => Ok((own_env(inherited_var), CodexHomeSource::Inherited)),
    }
}

/// The recorded env as (key, value) pairs, each value back to an [`OsString`]
/// losslessly; a value that does not decode (a corrupt record) refuses.
fn recorded_pairs(record: &DaemonEnvRecord) -> Result<Vec<(OsString, OsString)>> {
    record
        .env
        .iter()
        .map(|(name, bytes)| Ok((OsString::from(name), env_value_from_bytes(bytes)?)))
        .collect()
}

/// This process's own env, restricted to [`INHERITED_KEYS`], with a clauth
/// codex home scrubbed the way `start daemon` scrubs it. `var` is the env
/// source, so a test drives the production body over hand-built pairs without
/// reading the process's own env.
fn own_env(var: impl Fn(&str) -> Option<OsString>) -> Vec<(OsString, OsString)> {
    INHERITED_KEYS
        .iter()
        .filter_map(|key| {
            let value = var(key)?;
            let value = if *key == "CODEX_HOME" {
                crate::runtime::scrubbed_codex_home(Some(value))?
            } else {
                value
            };
            Some((OsString::from(*key), value))
        })
        .collect()
}

/// The source [`inherited_env`] passes to [`own_env`] for the no-daemon
/// fallback — the test-build fence lives HERE, never in [`own_env`]'s body.
/// `HOME` and `USERPROFILE` are the homes every standalone store's default
/// resolves under: in a test build they resolve from the sandbox home
/// ([`crate::profile::home_dir`]), whose `HOME_OVERRIDE` panic covers the
/// no-sandbox case process-wide (worker threads included), so a move/adopt
/// test that forgets its `testutil::HomeSandbox` panics instead of planning
/// and moving the operator's real `~/.shunt`. The non-test build reads the
/// process env straight through.
#[cfg(not(test))]
fn inherited_var(key: &str) -> Option<OsString> {
    std::env::var_os(key)
}

#[cfg(test)]
fn inherited_var(key: &str) -> Option<OsString> {
    match key {
        "HOME" | "USERPROFILE" => crate::profile::home_dir()
            .ok()
            .map(std::path::PathBuf::into_os_string),
        _ => std::env::var_os(key),
    }
}

/// An env value's bytes as the record stores them: raw on unix, UTF-16LE on
/// Windows, lossless in both directions.
#[cfg(unix)]
fn env_value_bytes(value: &OsStr) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt as _;
    value.as_bytes().to_vec()
}

#[cfg(windows)]
fn env_value_bytes(value: &OsStr) -> Vec<u8> {
    use std::os::windows::ffi::OsStrExt as _;
    value.encode_wide().flat_map(u16::to_le_bytes).collect()
}

#[cfg(unix)]
fn env_value_from_bytes(bytes: &[u8]) -> Result<OsString> {
    use std::os::unix::ffi::OsStringExt as _;
    Ok(OsString::from_vec(bytes.to_vec()))
}

/// On Windows an odd byte length is a corrupt record: refused, never silently
/// truncated to whole UTF-16 units.
#[cfg(windows)]
fn env_value_from_bytes(bytes: &[u8]) -> Result<OsString> {
    use std::os::windows::ffi::OsStringExt as _;
    let (pairs, rest) = bytes.as_chunks::<2>();
    if !rest.is_empty() {
        bail!("the daemon env record holds an odd-length value");
    }
    let wide: Vec<u16> = pairs.iter().map(|pair| u16::from_le_bytes(*pair)).collect();
    Ok(OsString::from_wide(&wide))
}

/// clauth edits the adopted config in place, so it must be TOML, and it is
/// passed as `--config` from whatever working directory the daemon has, so it
/// must be absolute.
fn check_adoptable(config: &Path) -> Result<()> {
    if is_yaml(config) {
        return Err(NotToml {
            path: config.to_path_buf(),
        }
        .into());
    }
    if !config.is_absolute() {
        bail!(
            "the shunt config must be an absolute path, got {}",
            config.display()
        );
    }
    Ok(())
}

/// Every path a record holds is read by callers with different working
/// directories (the TUI, the daemon), so each must be absolute.
fn check_record(record: &GatewayRecord) -> Result<()> {
    check_adoptable(&record.config)?;
    if let Some(binary) = record.binary.as_deref().filter(|path| !path.is_absolute()) {
        bail!(
            "the gateway's shunt binary must be an absolute path, got {}; drop the key to run shunt from PATH",
            binary.display()
        );
    }
    if let Some(env_file) = record
        .env_file
        .as_deref()
        .filter(|path| !path.is_absolute())
    {
        bail!(
            "the gateway's env file must be an absolute path, got {}",
            env_file.display()
        );
    }
    Ok(())
}

/// A record names a YAML config, which clauth cannot edit in place.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NotToml {
    pub(crate) path: PathBuf,
}

impl std::fmt::Display for NotToml {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "the shunt config must be TOML, and {} is YAML",
            self.path.display()
        )
    }
}

impl std::error::Error for NotToml {}

/// A config that does not parse as TOML: shunt could not load it either.
/// Carries the parse error's text, which names the line and never quotes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ConfigUnparsed(String);

impl std::fmt::Display for ConfigUnparsed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ConfigUnparsed {}

/// The record an adopt of `config` writes, refused where the gateway could
/// never run it: [`GatewayRecord::new`]'s checks, then the config's own parse
/// ([`ConfigUnparsed`]), which a bind from the env would otherwise skip.
pub(crate) fn adoptable_record(config: PathBuf) -> Result<GatewayRecord> {
    let record = GatewayRecord::new(config)?;
    parse_config(&read_config_text(record.config())?).map_err(|e| ConfigUnparsed(e.to_string()))?;
    Ok(record)
}

/// shunt's `ConfigFormat::from_path`: `.yaml`/`.yml` in any case is YAML,
/// every other name is TOML.
fn is_yaml(path: &Path) -> bool {
    path.extension()
        .and_then(OsStr::to_str)
        .is_some_and(|ext| ext.eq_ignore_ascii_case("yaml") || ext.eq_ignore_ascii_case("yml"))
}

// ── the admin token ─────────────────────────────────────────────────────────

/// An admin token (the gateway's, or a clauth proxy's), alone in a
/// clauth-owned 0600 file. No `Display`, a `Debug` that never prints the
/// value, and no `PartialEq` outside tests, where a plain compare would be a
/// timing side channel.
#[derive(Clone)]
#[cfg_attr(test, derive(PartialEq, Eq))]
pub(crate) struct AdminToken(String);

impl AdminToken {
    /// The token itself, for the admin API's header.
    pub(crate) fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for AdminToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AdminToken(<redacted>)")
    }
}

pub(crate) fn admin_token_path() -> Result<PathBuf> {
    Ok(clauth_dir()?.join("gateway-admin-token"))
}

/// The gateway's admin token, minted on first use.
pub(crate) fn ensure_admin_token() -> Result<AdminToken> {
    let path = admin_token_path()?;
    ensure_token_file(&path, |token| {
        if token.len() < MIN_ADMIN_KEY_LEN {
            bail!(
                "the gateway admin token in {} is shorter than {MIN_ADMIN_KEY_LEN} characters, which shunt refuses; delete the file and clauth mints a new one",
                path.display()
            );
        }
        Ok(())
    })
}

/// The token alone in `path`, trimmed and handed to `accept` when the file
/// exists, else minted from the CSPRNG (32 bytes, hex) and written 0600.
/// Under the state flock, so two first uses cannot mint two tokens.
pub(crate) fn ensure_token_file(
    path: &Path,
    accept: impl FnOnce(&str) -> Result<()>,
) -> Result<AdminToken> {
    with_state_lock(|_held| match std::fs::read_to_string(path) {
        Ok(text) => {
            // shunt trims a `${file:}` reference's contents too, and a clauth
            // proxy trims the token file it is handed.
            let token = text.trim();
            accept(token)?;
            Ok(AdminToken(token.to_string()))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let mut seed = [0u8; 32];
            getrandom::fill(&mut seed).map_err(|e| anyhow!("CSPRNG failure: {e}"))?;
            let token = AdminToken(hex::encode(seed));
            atomic_write_600(path, token.expose())
                .with_context(|| format!("failed to write {}", path.display()))?;
            Ok(token)
        }
        Err(e) => Err(e).with_context(|| format!("failed to read {}", path.display())),
    })
}

// ── discovery ───────────────────────────────────────────────────────────────

/// What shunt's `find_config_file` reads from its environment, supplied by
/// the caller so the search is a pure function of its inputs.
#[derive(Debug, Clone, Copy)]
pub(crate) struct DiscoveryInputs<'a> {
    /// The adopting process's working directory, absolute.
    pub(crate) cwd: &'a Path,
    pub(crate) xdg_config_home: Option<&'a OsStr>,
    pub(crate) home: Option<&'a Path>,
    pub(crate) homebrew_prefix: Option<&'a OsStr>,
}

/// Every path shunt would try, in its order (shunt `config_file_candidates`),
/// each resolved against `cwd` so the list is absolute.
pub(crate) fn config_candidates(inputs: DiscoveryInputs<'_>) -> Vec<PathBuf> {
    let cwd = inputs.cwd;
    let mut dirs = vec![cwd.to_path_buf()];
    let config_home = match inputs.xdg_config_home {
        Some(dir) if !dir.is_empty() => Some(cwd.join(dir)),
        _ => inputs.home.map(|home| cwd.join(home).join(".config")),
    };
    if let Some(dir) = config_home {
        dirs.push(dir.join("shunt"));
    }
    match inputs.homebrew_prefix.filter(|prefix| !prefix.is_empty()) {
        Some(prefix) => dirs.push(cwd.join(prefix).join("etc")),
        None => {
            dirs.push(PathBuf::from("/opt/homebrew").join("etc"));
            dirs.push(PathBuf::from("/usr/local").join("etc"));
        }
    }
    dirs.into_iter()
        .flat_map(|dir| CONFIG_FILENAMES.iter().map(move |name| dir.join(name)))
        .collect()
}

/// The first existing candidate, absolute; `None` when shunt would find none.
/// A YAML first hit refuses with [`YamlConfig`] rather than skipping to a
/// later TOML, which shunt itself would never load.
pub(crate) fn discover_config_in(inputs: DiscoveryInputs<'_>) -> Result<Option<PathBuf>> {
    let Some(hit) = config_candidates(inputs)
        .into_iter()
        .find(|path| path.is_file())
    else {
        return Ok(None);
    };
    if is_yaml(&hit) {
        return Err(YamlConfig { path: hit }.into());
    }
    Ok(Some(hit))
}

/// [`discover_config_in`] over this process's working directory and
/// environment.
pub(crate) fn discover_config() -> Result<Option<PathBuf>> {
    let cwd = std::env::current_dir().context("cannot read the working directory")?;
    discover_config_from(&cwd, |key| std::env::var_os(key))
}

/// [`discover_config_in`] over the env `var` reads, read the way shunt's
/// `find_config_file` reads it: `HOME` raw, never through the crate's home
/// resolver, which ignores `HOME` on Windows and replaces an empty one on
/// unix, where shunt then searches a cwd-relative `.config`.
fn discover_config_from(
    cwd: &Path,
    var: impl Fn(&str) -> Option<OsString>,
) -> Result<Option<PathBuf>> {
    let xdg_config_home = var("XDG_CONFIG_HOME");
    let home = var("HOME");
    let homebrew_prefix = var("HOMEBREW_PREFIX");
    discover_config_in(DiscoveryInputs {
        cwd,
        xdg_config_home: xdg_config_home.as_deref(),
        home: home.as_deref().map(Path::new),
        homebrew_prefix: homebrew_prefix.as_deref(),
    })
}

/// Adoption refused: the first config shunt would load is YAML.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct YamlConfig {
    pub(crate) path: PathBuf,
}

impl std::fmt::Display for YamlConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "shunt would load {}, a YAML config; clauth edits the adopted config in place and has no format-preserving YAML editor, so it adopts a TOML config only",
            self.path.display()
        )
    }
}

impl std::error::Error for YamlConfig {}

// ── bind ────────────────────────────────────────────────────────────────────

/// Where the gateway listens, and where clauth probes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct GatewayBind {
    pub(crate) configured: SocketAddr,
    /// `configured`, with a wildcard host swapped for loopback.
    pub(crate) probe: SocketAddr,
}

/// The bind from `SHUNT_SERVER__BIND` (`env_bind`), else `[server].bind` in
/// `config_text`, else shunt's default.
pub(crate) fn resolve_bind(config_text: &str, env_bind: Option<&str>) -> Result<GatewayBind> {
    let configured = match env_bind {
        Some(value) => parse_bind(value, BIND_ENV)?,
        None => {
            let doc = parse_config(config_text)?;
            match doc.get("server").and_then(|server| server.get("bind")) {
                None => SHUNT_DEFAULT_BIND,
                Some(bind) => {
                    let refused = |refusal| ConfigBindRefused {
                        refusal,
                        // A table-shaped bind converts to its inline form, so
                        // no comment or layout of the file rides along.
                        written: match bind.clone().into_value() {
                            Ok(value) => match value.as_str() {
                                Some(text) => text.to_string(),
                                None => value.decorated("", "").to_string(),
                            },
                            Err(_) => String::new(),
                        },
                    };
                    let value = bind.as_str().ok_or_else(|| {
                        refused(BindRefusal::NotAnAddress {
                            source: CONFIG_BIND,
                        })
                    })?;
                    if value.contains("${") {
                        return Err(refused(BindRefusal::ConfigReference).into());
                    }
                    parse_bind(value, CONFIG_BIND).map_err(refused)?
                }
            }
        }
    };
    let probe_ip = match configured.ip() {
        ip if !ip.is_unspecified() => ip,
        std::net::IpAddr::V4(_) => Ipv4Addr::LOCALHOST.into(),
        std::net::IpAddr::V6(_) => Ipv6Addr::LOCALHOST.into(),
    };
    Ok(GatewayBind {
        configured,
        probe: SocketAddr::new(probe_ip, configured.port()),
    })
}

fn parse_bind(value: &str, source: &'static str) -> Result<SocketAddr, BindRefusal> {
    let addr: SocketAddr = value
        .parse()
        .map_err(|_| BindRefusal::NotAnAddress { source })?;
    if addr.port() == 0 {
        return Err(BindRefusal::OsAssignedPort { source });
    }
    Ok(addr)
}

/// The bind value shunt's figment env layer hands `[server].bind`, mirrored
/// from figment 0.10.19 `value/parse.rs` `value()` on the string branches:
/// the leading whitespace skip is ASCII-only, a value figment's `[`-array
/// branch cannot parse falls back to the raw untrimmed string, and a value
/// wholly wrapped in one pair of double quotes unwraps its inner text. clauth
/// mirrors only the plain pair and returns `None` for a quoted value holding a
/// backslash (figment would unescape it), which the caller refuses rather than
/// misread.
fn normalize_bind_value(value: &str) -> Option<String> {
    // figment's leading `skip_while(is_whitespace)` is ASCII-only.
    let rest = value.trim_ascii_start();
    if let Some(rest) = rest.strip_prefix('"') {
        // string branch: unwrap one plain pair. An unterminated quote, or any
        // leftover after the closing quote, falls back to the raw value.
        let Some(end) = rest.find('"') else {
            return Some(value.to_string());
        };
        let inner = &rest[..end];
        if inner.contains('\\') {
            return None;
        }
        return if rest[end + 1..].trim_ascii_start().is_empty() {
            Some(inner.to_string())
        } else {
            Some(value.to_string())
        };
    }
    if rest.starts_with('[') {
        // array branch: an address is never a figment array, and a failed
        // array parse falls back to the raw value, so hand `parse_bind` the
        // raw value and let it judge (`[::1]:4997` parses, ` [::1]:4997 `
        // does not).
        return Some(value.to_string());
    }
    if rest.starts_with('{') || rest.starts_with('\'') {
        // dict / single-quote branches: neither yields an address, and a
        // failed parse falls back to the raw value.
        return Some(value.to_string());
    }
    // default branch: `take_while(is_not_separator)` then figment's own
    // (Unicode) trim; a leftover separator means the parse fell back to raw.
    let taken: String = rest
        .chars()
        .take_while(|c| !matches!(c, ',' | '{' | '}' | '[' | ']'))
        .collect();
    let left = &rest[taken.len()..];
    if left.trim_ascii_start().is_empty() {
        Some(taken.trim().to_string())
    } else {
        Some(value.to_string())
    }
}

/// [`resolve_bind`] for the adopted config, with `SHUNT_SERVER__BIND` read
/// from the env the gateway will be spawned with, as shunt's figment layer
/// reads it ([`spawned_env_value`]).
pub(crate) fn gateway_bind(record: &GatewayRecord, env: &GatewayEnv) -> Result<GatewayBind> {
    gateway_bind_in(record, env, std::env::vars_os())
}

/// [`gateway_bind`] over the caller's inherited env, so tests inject one
/// instead of reading the process's own.
fn gateway_bind_in(
    record: &GatewayRecord,
    env: &GatewayEnv,
    inherited: impl IntoIterator<Item = (OsString, OsString)>,
) -> Result<GatewayBind> {
    let text = read_config_text(record.config())?;
    let env_bind = match spawned_env_value(env, BIND_ENV, inherited) {
        Some(value) => {
            let value = value
                .into_string()
                .map_err(|_| BindRefusal::NotAnAddress { source: BIND_ENV })?;
            Some(read_bind_env(&value)?)
        }
        None => None,
    };
    resolve_bind(&text, env_bind.as_deref())
}

/// A `SHUNT_SERVER__BIND` value as shunt's figment layer hands it to
/// [`resolve_bind`]: one plain pair of quotes unwrapped, a quoted value
/// holding an escape refused rather than misread.
pub(crate) fn read_bind_env(value: &str) -> Result<String, BindRefusal> {
    normalize_bind_value(value).ok_or(BindRefusal::QuotedEscape { source: BIND_ENV })
}

/// The raw `SHUNT_SERVER__BIND` an adopt's probe reads ([`gateway_probe`] on
/// a record with no env file): [`inherited_env`]'s, decoded lossily, so a
/// value that does not decode still refuses as an address. Refuses as
/// [`inherited_env`] does, with [`StoreMoveRefusal::DaemonEnvUnrecorded`].
pub(crate) fn inherited_bind_env() -> Result<Option<String>> {
    Ok(
        spawned_env_value(&GatewayEnv::default(), BIND_ENV, inherited_env()?.0)
            .map(|value| value.to_string_lossy().into_owned()),
    )
}

/// The gateway's probe address, resolved from the inherited env the store plan
/// and move use ([`inherited_env`]), for minting the move's silent proof: the
/// same env means a `SHUNT_SERVER__BIND` set only in the caller's shell cannot
/// make the proof and the move disagree.
pub(crate) fn gateway_probe(record: &GatewayRecord) -> Result<SocketAddr> {
    let env = gateway_env(record)?;
    Ok(gateway_bind_in(record, &env, inherited_env()?.0)?.probe)
}

/// The gateway's bind for a display read on the UI thread: the adopted config
/// and its env file over the running daemon's recorded env, that record
/// matched to the singleton holder by pid alone ([`recorded_env_for_holder_pid`])
/// so no process is spawned. With no holder, or no record naming it, the
/// files alone decide.
pub(crate) fn displayed_gateway_bind(record: &GatewayRecord) -> Result<GatewayBind> {
    let env = gateway_env(record)?;
    gateway_bind_in(
        record,
        &env,
        recorded_env_for_holder_pid().unwrap_or_default(),
    )
}

/// The running daemon's recorded env, matched to the singleton holder by pid
/// alone: the start-time half of [`DaemonIdentity`] spawns `ps` or PowerShell
/// off linux, which a UI-thread read must not. A record a dead daemon left
/// under a pid the holder reuses passes this match, so it feeds display reads
/// only, never the plan or the move ([`inherited_env`]).
pub(crate) fn recorded_env_for_holder_pid() -> Option<Vec<(OsString, OsString)>> {
    if !crate::daemon::singleton_held().ok()? {
        return None;
    }
    let pid = crate::daemon::holder_pid()?;
    let record = read_env_record().ok()??;
    if record.identity.pid != pid {
        return None;
    }
    recorded_pairs(&record).ok()
}

/// Whether `url` (a profile's `base_url`) names the gateway bound at `bind`:
/// an `http`/`https` URL whose port, written or the scheme's default, is the
/// bind's, and whose host is the probe address, or `localhost` while that
/// address is loopback, or the unspecified address of the probe's family. A
/// wildcard bind is named through its loopback probe address only; a LAN
/// address of the box reaching it does not count.
pub(crate) fn url_names_bind(url: &str, bind: &GatewayBind) -> bool {
    let Ok(uri) = url.parse::<ureq::http::Uri>() else {
        return false;
    };
    let default_port = match uri.scheme_str() {
        Some(scheme) if scheme.eq_ignore_ascii_case("http") => 80,
        Some(scheme) if scheme.eq_ignore_ascii_case("https") => 443,
        Some(_) | None => return false,
    };
    if uri.port_u16().unwrap_or(default_port) != bind.probe.port() {
        return false;
    }
    let Some(host) = uri.host() else {
        return false;
    };
    // `Uri::host` keeps an IPv6 literal's brackets.
    let host = host
        .strip_prefix('[')
        .and_then(|inner| inner.strip_suffix(']'))
        .unwrap_or(host);
    if host.eq_ignore_ascii_case("localhost") {
        return bind.probe.ip().is_loopback();
    }
    let Ok(ip) = host.parse::<std::net::IpAddr>() else {
        return false;
    };
    // A connect to the unspecified address reaches the loopback listener on
    // linux and macOS, so it names the gateway as loopback does.
    let ip = match ip.to_canonical() {
        std::net::IpAddr::V4(v4) if v4.is_unspecified() => Ipv4Addr::LOCALHOST.into(),
        std::net::IpAddr::V6(v6) if v6.is_unspecified() => Ipv6Addr::LOCALHOST.into(),
        ip => ip,
    };
    ip == bind.probe.ip().to_canonical()
}

/// The env override shunt's figment layer maps onto
/// `[server].shutdown_timeout_seconds`.
pub(crate) const SHUTDOWN_TIMEOUT_ENV: &str = "SHUNT_SERVER__SHUTDOWN_TIMEOUT_SECONDS";

/// shunt's drain bound when neither the env nor the config sets one
/// (`default_shutdown_timeout_seconds`, shunt `config.rs`).
pub(crate) const SHUNT_DEFAULT_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(30);

/// The longest drain shunt accepts (`MAX_SHUTDOWN_TIMEOUT_SECONDS`); it
/// refuses to start on a value outside `1..=3600`.
pub(crate) const SHUNT_MAX_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(3600);

/// How long shunt drains after SIGTERM: `SHUNT_SERVER__SHUTDOWN_TIMEOUT_SECONDS`
/// (`env_value`), else `[server].shutdown_timeout_seconds` in `config_text`,
/// else shunt's 30 s. A value clauth cannot read counts as shunt's maximum,
/// so a stop bound derived from it never cuts a drain short.
pub(crate) fn resolve_shutdown_timeout(config_text: &str, env_value: Option<&str>) -> Duration {
    let read = match env_value {
        Some(value) => env_number(value),
        None => match parse_config(config_text) {
            Ok(doc) => match doc
                .get("server")
                .and_then(|server| server.get("shutdown_timeout_seconds"))
            {
                None => return SHUNT_DEFAULT_SHUTDOWN_TIMEOUT,
                Some(value) => value.as_integer().and_then(|secs| u64::try_from(secs).ok()),
            },
            Err(_) => None,
        },
    };
    read.map_or(SHUNT_MAX_SHUTDOWN_TIMEOUT, |secs| {
        Duration::from_secs(secs).min(SHUNT_MAX_SHUTDOWN_TIMEOUT)
    })
}

/// The number shunt's figment env layer reads from `value`, mirrored from
/// figment 0.10.19 `value/parse.rs` `value()`: an ASCII-only leading skip, then
/// the text up to a separator, trimmed of Unicode whitespace. A separator left
/// over makes figment fall back to the raw string, and a quoted value is a
/// string too; shunt refuses both, so neither is a number here.
fn env_number(value: &str) -> Option<u64> {
    let rest = value.trim_ascii_start();
    if rest.contains([',', '{', '}', '[', ']']) {
        return None;
    }
    rest.trim().parse().ok()
}

/// [`resolve_shutdown_timeout`] for the adopted config, the env value read
/// from the env the gateway will be spawned with, as shunt's figment layer
/// reads it ([`spawned_env_value`]).
pub(crate) fn gateway_shutdown_timeout(record: &GatewayRecord, env: &GatewayEnv) -> Duration {
    gateway_shutdown_timeout_in(record, env, std::env::vars_os())
}

/// [`gateway_shutdown_timeout`] over the caller's inherited env, so tests
/// inject one instead of reading the process's own.
fn gateway_shutdown_timeout_in(
    record: &GatewayRecord,
    env: &GatewayEnv,
    inherited: impl IntoIterator<Item = (OsString, OsString)>,
) -> Duration {
    if let Some(value) = spawned_env_value(env, SHUTDOWN_TIMEOUT_ENV, inherited) {
        // A value that is not UTF-8 is no number: it reads as shunt's maximum.
        return resolve_shutdown_timeout("", Some(value.to_str().unwrap_or("")));
    }
    read_config_text(record.config()).map_or(SHUNT_MAX_SHUTDOWN_TIMEOUT, |text| {
        resolve_shutdown_timeout(&text, None)
    })
}

/// The bind could not be turned into a probe address. Names where it came
/// from, never the value: an env-file value is the user's secret material.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BindRefusal {
    NotAnAddress {
        source: &'static str,
    },
    OsAssignedPort {
        source: &'static str,
    },
    /// `[server].bind` holds a `${...}` reference, which shunt substitutes
    /// before it parses the address and clauth does not.
    ConfigReference,
    /// The env override is quoted with a backslash inside, which figment
    /// unescapes (`\n`, `\u…`) and clauth does not.
    QuotedEscape {
        source: &'static str,
    },
}

impl std::fmt::Display for BindRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BindRefusal::NotAnAddress { source } => write!(
                f,
                "{source} is not an ip:port address; set it to an ip:port address like 127.0.0.1:3001"
            ),
            BindRefusal::ConfigReference => write!(
                f,
                "{CONFIG_BIND} is a ${{...}} reference, which clauth does not resolve; set {BIND_ENV} in the gateway's env file to the address instead"
            ),
            BindRefusal::OsAssignedPort { source } => write!(
                f,
                "{source} asks for an OS-assigned port, so clauth cannot know where the gateway listens; set it to a fixed port like 127.0.0.1:3001"
            ),
            BindRefusal::QuotedEscape { source } => write!(
                f,
                "{source} holds a backslash inside its quotes, which figment would unescape and clauth does not; write the value literally"
            ),
        }
    }
}

impl std::error::Error for BindRefusal {}

/// A `[server].bind` from the config that clauth refused, with the value the
/// config holds, for the Services row that names it: the config is the user's
/// own file, while an env-file value never rides a refusal. `Display` is the
/// refusal's alone, so no message carries the value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ConfigBindRefused {
    pub(crate) refusal: BindRefusal,
    /// The string itself, or any other value's TOML spelling, a table in its
    /// inline form, without the comment after it or the whitespace around it.
    pub(crate) written: String,
}

impl std::fmt::Display for ConfigBindRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.refusal.fmt(f)
    }
}

impl std::error::Error for ConfigBindRefused {}

// ── env ─────────────────────────────────────────────────────────────────────

/// Env pairs for the gateway process. `Debug` lists key names only.
#[derive(Clone, Default, PartialEq, Eq)]
pub(crate) struct GatewayEnv {
    vars: Vec<(String, OsString)>,
    skipped: Vec<usize>,
}

impl GatewayEnv {
    /// The value a spawn would see for `key`.
    pub(crate) fn get(&self, key: &str) -> Option<&OsStr> {
        self.vars
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, value)| value.as_os_str())
    }

    /// The key names, in order.
    pub(crate) fn keys(&self) -> impl Iterator<Item = &str> {
        self.vars.iter().map(|(key, _)| key.as_str())
    }

    /// The env file's lines that assigned nothing, by number: a name that is
    /// no variable name (`export KEY=...` included) or a line with no `=`.
    /// systemd skips them and loads the rest, and so does clauth; the caller
    /// surfaces them by number alone, since a line's text may be a secret.
    pub(crate) fn skipped_lines(&self) -> &[usize] {
        &self.skipped
    }

    /// Lay these pairs over `command`'s inherited env.
    pub(crate) fn apply(&self, command: &mut Command) {
        command.envs(self.vars.iter().map(|(key, value)| (key, value)));
    }

    /// Set `key` as the last assignment: a later value replaces every earlier
    /// one and takes its position, so `vars` order is last-assignment order
    /// and a Windows child's env (which folds case and keeps the last set)
    /// reads the last spelling in file order.
    fn set(&mut self, key: String, value: OsString) {
        self.vars.retain(|(name, _)| *name != key);
        self.vars.push((key, value));
    }

    /// Drop every pair whose name equals `key` ignoring ASCII case. The store
    /// pins call it before setting: on Windows a later case-variant spelling
    /// of a store key in the env file would otherwise replace the pin, and
    /// the exact spelling is dropped and re-pinned last, so the pin wins
    /// there too.
    fn drop_case_variants_of(&mut self, key: &str) {
        self.vars
            .retain(|(name, _)| !name.eq_ignore_ascii_case(key));
    }

    /// Load one parsed assignment the way systemd does: a name or value that
    /// is not UTF-8 refuses the whole file, a name that is no variable name
    /// skips the line.
    fn assign(&mut self, mut assignment: Assignment, trim_value: bool) -> Result<(), EnvFileError> {
        let line = assignment.line;
        if let Some(end) = assignment.key_trail {
            assignment.key.truncate(end);
        }
        if let Some(end) = assignment.value_trail.filter(|_| trim_value) {
            assignment.value.truncate(end);
        }
        let (Ok(key), Ok(value)) = (
            String::from_utf8(assignment.key),
            String::from_utf8(assignment.value),
        ) else {
            return Err(EnvFileError {
                line,
                kind: EnvFileErrorKind::NotUtf8,
            });
        };
        if is_env_name(&key) {
            self.set(key, OsString::from(value));
        } else {
            self.skipped.push(line);
        }
        Ok(())
    }
}

impl std::fmt::Debug for GatewayEnv {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GatewayEnv")
            .field("keys", &self.keys().collect::<Vec<_>>())
            .finish()
    }
}

/// systemd's `NEWLINE`, `WHITESPACE` and `COMMENTS` sets, and the bytes a
/// backslash unescapes inside double quotes (`SHELL_NEED_ESCAPE`).
const ENV_NEWLINE: &[u8] = b"\n\r";
const ENV_WHITESPACE: &[u8] = b" \t\n\r";
const ENV_COMMENT: &[u8] = b"#;";
const ENV_QUOTED_ESCAPES: &[u8] = b"\"\\`$";

/// Where [`parse_env_file`] is: systemd's parser states, one for one.
#[derive(Debug, Clone, Copy)]
enum EnvState {
    PreKey,
    Key,
    PreValue,
    Value,
    ValueEscape,
    SingleQuoted,
    DoubleQuoted,
    DoubleQuotedEscape,
    Comment,
}

/// The assignment [`parse_env_file`] is reading: the line its key starts
/// on, and where trailing whitespace starts in its key and in its unquoted
/// value. No `Debug`: its value bytes are the env file's secret material.
#[derive(Default)]
struct Assignment {
    line: usize,
    key: Vec<u8>,
    key_trail: Option<usize>,
    value: Vec<u8>,
    value_trail: Option<usize>,
}

impl Assignment {
    fn push_key(&mut self, byte: u8) {
        track_trail(&mut self.key_trail, self.key.len(), byte);
        self.key.push(byte);
    }

    fn push_value(&mut self, byte: u8) {
        track_trail(&mut self.value_trail, self.value.len(), byte);
        self.value.push(byte);
    }
}

fn track_trail(trail: &mut Option<usize>, at: usize, byte: u8) {
    if !ENV_WHITESPACE.contains(&byte) {
        *trail = None;
    } else if trail.is_none() {
        *trail = Some(at);
    }
}

/// systemd's `EnvironmentFile=` grammar, byte for byte (`parse_env_file` in
/// its `env-file.c`, pinned against systemd 261): the env file is the one a
/// standalone gateway's unit loaded, so clauth must read the same values.
///
/// - `#` or `;` opens a comment line; whitespace before a key, after a key
///   and after `=` goes.
/// - Unquoted, a value loses its trailing whitespace, `\c` reads `c`, and a
///   `\` before a newline continues the line.
/// - Inside double quotes only `\"` `\\` `` \` `` `\$` unescape, any other
///   `\c` stays whole, a `\` before a newline joins the lines, and a newline
///   is kept; single quotes are literal. Quoted and unquoted runs join, and
///   a quote left open runs to the end of the file.
/// - `\n` and `\r` each end a line.
/// - An assignment to a name that is no variable name, or a line with no
///   `=`, is skipped by number and the rest loads; a repeated key keeps its
///   last value; nothing expands.
/// - A NUL byte anywhere, or an assignment that is not UTF-8, refuses the
///   whole file.
pub(crate) fn parse_env_file(bytes: &[u8]) -> Result<GatewayEnv, EnvFileError> {
    if let Some(at) = bytes.iter().position(|&byte| byte == 0) {
        return Err(EnvFileError {
            line: bytes.iter().take(at).filter(|&&byte| byte == b'\n').count() + 1,
            kind: EnvFileErrorKind::NulByte,
        });
    }
    let mut env = GatewayEnv::default();
    let mut assignment = Assignment::default();
    let mut state = EnvState::PreKey;
    let mut line = 1;
    for &byte in bytes {
        let newline = ENV_NEWLINE.contains(&byte);
        let whitespace = ENV_WHITESPACE.contains(&byte);
        state = match state {
            EnvState::PreKey if ENV_COMMENT.contains(&byte) => EnvState::Comment,
            EnvState::PreKey if whitespace => EnvState::PreKey,
            EnvState::PreKey => {
                assignment = Assignment {
                    line,
                    ..Assignment::default()
                };
                assignment.push_key(byte);
                EnvState::Key
            }
            EnvState::Key if newline => {
                env.skipped.push(assignment.line);
                EnvState::PreKey
            }
            EnvState::Key if byte == b'=' => EnvState::PreValue,
            EnvState::Key => {
                assignment.push_key(byte);
                EnvState::Key
            }
            EnvState::PreValue if newline => {
                env.assign(std::mem::take(&mut assignment), false)?;
                EnvState::PreKey
            }
            EnvState::PreValue if byte == b'\'' => EnvState::SingleQuoted,
            EnvState::PreValue if byte == b'"' => EnvState::DoubleQuoted,
            EnvState::PreValue if byte == b'\\' => EnvState::ValueEscape,
            EnvState::PreValue if whitespace => EnvState::PreValue,
            EnvState::PreValue => {
                assignment.value.push(byte);
                EnvState::Value
            }
            EnvState::Value if newline => {
                env.assign(std::mem::take(&mut assignment), true)?;
                EnvState::PreKey
            }
            EnvState::Value if byte == b'\\' => {
                assignment.value_trail = None;
                EnvState::ValueEscape
            }
            EnvState::Value => {
                assignment.push_value(byte);
                EnvState::Value
            }
            EnvState::ValueEscape => {
                if !newline {
                    assignment.value.push(byte);
                }
                EnvState::Value
            }
            EnvState::SingleQuoted if byte == b'\'' => EnvState::PreValue,
            EnvState::DoubleQuoted if byte == b'"' => EnvState::PreValue,
            EnvState::DoubleQuoted if byte == b'\\' => EnvState::DoubleQuotedEscape,
            EnvState::SingleQuoted | EnvState::DoubleQuoted => {
                assignment.value.push(byte);
                state
            }
            EnvState::DoubleQuotedEscape => {
                if ENV_QUOTED_ESCAPES.contains(&byte) {
                    assignment.value.push(byte);
                } else if byte != b'\n' {
                    assignment.value.extend([b'\\', byte]);
                }
                EnvState::DoubleQuoted
            }
            EnvState::Comment if newline => EnvState::PreKey,
            EnvState::Comment => EnvState::Comment,
        };
        if byte == b'\n' {
            line += 1;
        }
    }
    match state {
        EnvState::Value => env.assign(assignment, true)?,
        EnvState::PreValue
        | EnvState::ValueEscape
        | EnvState::SingleQuoted
        | EnvState::DoubleQuoted
        | EnvState::DoubleQuotedEscape => env.assign(assignment, false)?,
        EnvState::Key => env.skipped.push(assignment.line),
        EnvState::PreKey | EnvState::Comment => {}
    }
    Ok(env)
}

fn is_env_name(key: &str) -> bool {
    let mut chars = key.chars();
    chars
        .next()
        .is_some_and(|c| c == '_' || c.is_ascii_alphabetic())
        && chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
}

pub(crate) fn read_env_file(path: &Path) -> Result<GatewayEnv> {
    let bytes = std::fs::read(path)
        .with_context(|| format!("failed to read env file {}", path.display()))?;
    parse_env_file(&bytes).with_context(|| format!("in env file {}", path.display()))
}

/// The value shunt's figment env layer reads for `key` from the env a spawn
/// laying `env` over the caller's inherited env (`inherited`) hands it.
/// figment matches names in any case and its last match wins, and std hands a
/// child its env sorted by name, so on unix, where two spellings are two
/// variables, the greatest matching name wins; Windows folds the spellings
/// into one variable, the one set last.
fn spawned_env_value(
    env: &GatewayEnv,
    key: &str,
    inherited: impl IntoIterator<Item = (OsString, OsString)>,
) -> Option<OsString> {
    spawned_env_var(env, key, inherited).map(|(_, value)| value)
}

/// [`spawned_env_value`] with the name the winning variable is spelled as in
/// the spawned env, for a surface that names the variable.
pub(crate) fn spawned_env_var(
    env: &GatewayEnv,
    key: &str,
    inherited: impl IntoIterator<Item = (OsString, OsString)>,
) -> Option<(OsString, OsString)> {
    let mut spawned: BTreeMap<OsString, OsString> = inherited
        .into_iter()
        .map(|(name, value)| (spawned_env_name(name), value))
        .collect();
    spawned.extend(
        env.vars
            .iter()
            .map(|(name, value)| (spawned_env_name(OsString::from(name)), value.clone())),
    );
    spawned.into_iter().rev().find(|(name, _)| {
        name.to_str()
            .is_some_and(|name| name.eq_ignore_ascii_case(key))
    })
}

/// A variable's name as a spawned env keys it: as spelled on unix, folded
/// on Windows, whose env names ignore case.
fn spawned_env_name(name: OsString) -> OsString {
    if cfg!(windows) {
        name.to_ascii_uppercase()
    } else {
        name
    }
}

/// A variable's name as `std::env::var_os` matches it: exact spelling on
/// unix, case-folded on Windows, whose env names ignore case.
fn var_os_key(name: &str) -> String {
    if cfg!(windows) {
        name.to_ascii_uppercase()
    } else {
        name.to_string()
    }
}

/// The value of `key` among `vars`, matched under the fold `fold`: on unix
/// the fold is identity, so only the exact spelling matches; on Windows it
/// upper-cases, so the last matching spelling in `vars` order wins, matching
/// what a Windows child's env holds (a case-insensitive env where the last
/// insert wins).
fn env_value_folded<'a>(
    vars: &'a [(String, OsString)],
    key: &str,
    fold: fn(&str) -> String,
) -> Option<&'a OsStr> {
    vars.iter()
        .rev()
        .find(|(name, _)| fold(name) == fold(key))
        .map(|(_, value)| value.as_os_str())
}

/// The value the standalone's env held for `key`, as `std::env::var_os`
/// read it there: the env file's value (folded, last assignment wins, exact
/// spelling on unix), else the inherited env's last matching entry. `Some`
/// even for an empty value; `None` only when no source names `key`.
fn standalone_var<'a>(
    named: &'a GatewayEnv,
    inherited: &'a [(OsString, OsString)],
    key: &str,
) -> Option<&'a OsStr> {
    env_value_folded(&named.vars, key, var_os_key).or_else(|| {
        inherited
            .iter()
            .rev()
            .find(|(name, _)| {
                name.to_str()
                    .is_some_and(|name| var_os_key(name) == var_os_key(key))
            })
            .map(|(_, value)| value.as_os_str())
    })
}

/// The env the gateway runs with, over its inherited one: the record's env
/// file, then every store env, which win, so an env file can never point
/// the managed gateway at a standalone store or at another owner's login.
/// The env file's skipped lines ride along.
pub(crate) fn gateway_env(record: &GatewayRecord) -> Result<GatewayEnv> {
    let mut env = match &record.env_file {
        Some(path) => read_env_file(path)?,
        None => GatewayEnv::default(),
    };
    for (key, path) in store_env()? {
        env.drop_case_variants_of(key);
        env.set(key.to_string(), path.into_os_string());
    }
    Ok(env)
}

/// The working directory `shunt check` and the gateway run in: the adopted
/// config's own directory, so wherever the caller was started, the check and
/// the run resolve the same config the same way.
pub(crate) fn gateway_cwd(record: &GatewayRecord) -> Result<&Path> {
    record.config().parent().with_context(|| {
        format!(
            "the adopted config {} has no parent directory",
            record.config().display()
        )
    })
}

/// An env file systemd refuses whole: the line it refuses at, never the
/// line's text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct EnvFileError {
    pub(crate) line: usize,
    pub(crate) kind: EnvFileErrorKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum EnvFileErrorKind {
    /// An assignment's name or value is not UTF-8.
    NotUtf8,
    /// A NUL byte, anywhere in the file.
    NulByte,
}

impl std::fmt::Display for EnvFileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let line = self.line;
        match &self.kind {
            EnvFileErrorKind::NotUtf8 => write!(
                f,
                "the assignment on line {line} is not UTF-8; systemd refuses such a file whole, and so does clauth: save it as UTF-8"
            ),
            EnvFileErrorKind::NulByte => write!(
                f,
                "line {line} holds a NUL byte; systemd refuses such a file whole, and so does clauth: remove the byte"
            ),
        }
    }
}

impl std::error::Error for EnvFileError {}

// ── version floor + /health ─────────────────────────────────────────────────

/// A shunt crate version as `/health` reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ShuntVersion {
    pub(crate) major: u64,
    pub(crate) minor: u64,
    pub(crate) patch: u64,
    pub(crate) pre_release: bool,
}

impl ShuntVersion {
    /// Semver `MAJOR.MINOR.PATCH[-PRE][+BUILD]`, identifiers limited to
    /// `[0-9A-Za-z.-]`, so a parsed version is safe to print as read.
    fn parse(read: &str) -> Option<Self> {
        let identifiers = |s: &str| {
            !s.is_empty()
                && s.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-')
        };
        let (head, build) = match read.split_once('+') {
            Some((head, build)) => (head, Some(build)),
            None => (read, None),
        };
        if build.is_some_and(|build| !identifiers(build)) {
            return None;
        }
        let (core, pre) = match head.split_once('-') {
            Some((core, pre)) => (core, Some(pre)),
            None => (head, None),
        };
        if pre.is_some_and(|pre| !identifiers(pre)) {
            return None;
        }
        let mut parts = core.split('.');
        let mut number = || {
            parts
                .next()
                .filter(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()))?
                .parse::<u64>()
                .ok()
        };
        let version = Self {
            major: number()?,
            minor: number()?,
            patch: number()?,
            pre_release: pre.is_some(),
        };
        parts.next().is_none().then_some(version)
    }

    /// Semver precedence against a release `floor`: a pre-release sorts
    /// before the release it precedes.
    fn meets(self, floor: Self) -> bool {
        let core = (self.major, self.minor, self.patch);
        let floor_core = (floor.major, floor.minor, floor.patch);
        core > floor_core || (core == floor_core && !self.pre_release)
    }
}

impl std::fmt::Display for ShuntVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// A version below [`VERSION_FLOOR`], or one that does not read as a version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct VersionRefusal {
    pub(crate) read: String,
    pub(crate) floor: ShuntVersion,
    pub(crate) kind: VersionRefusalKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum VersionRefusalKind {
    BelowFloor,
    Unreadable,
}

impl std::fmt::Display for VersionRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (read, floor) = (&self.read, self.floor);
        match self.kind {
            VersionRefusalKind::BelowFloor => write!(
                f,
                "shunt {read} is older than {floor}, the oldest release clauth supervises"
            ),
            VersionRefusalKind::Unreadable => write!(
                f,
                "shunt reported version {read:?}, which does not read as a release; clauth supervises {floor} or newer"
            ),
        }
    }
}

impl std::error::Error for VersionRefusal {}

/// `Ok` when `read` (a `/health` `version`) is at or above [`VERSION_FLOOR`].
pub(crate) fn check_version_floor(read: &str) -> Result<(), VersionRefusal> {
    let refuse = |kind| VersionRefusal {
        read: read.to_string(),
        floor: VERSION_FLOOR,
        kind,
    };
    match ShuntVersion::parse(read) {
        None => Err(refuse(VersionRefusalKind::Unreadable)),
        Some(version) if version.meets(VERSION_FLOOR) => Ok(()),
        Some(_) => Err(refuse(VersionRefusalKind::BelowFloor)),
    }
}

/// What answered `GET /health` at an address.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Health {
    /// Nothing listens there: the connection was refused.
    Silent(GatewaySilent),
    /// A shunt-shaped answer carrying this version. `/health` carries no
    /// instance identity, so this says nothing about who runs it.
    Shunt { version: String },
    /// Something answered, but not with shunt's `/health` body.
    NotShunt { status: u16 },
}

/// Proof a probe of one address found nothing listening; only
/// [`probe_health`] mints one, and the store move consumes it, so one proof
/// gates one move.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct GatewaySilent {
    addr: SocketAddr,
}

impl GatewaySilent {
    /// The address the probe found silent.
    pub(crate) fn addr(&self) -> SocketAddr {
        self.addr
    }
}

#[cfg(test)]
impl GatewaySilent {
    /// A proof for tests driving the store move, whose own subject is not
    /// the probe.
    pub(crate) fn for_test() -> Self {
        Self {
            addr: SHUNT_DEFAULT_BIND,
        }
    }

    /// A proof naming an arbitrary address, for the mismatch refusal.
    pub(crate) fn for_test_at(addr: SocketAddr) -> Self {
        Self { addr }
    }
}

/// The `/health` probe's client, its connect and response phases bounded
/// apart and together; `clauth proxy check` holds a proxy to the same bounds.
pub(crate) fn health_agent() -> ureq::Agent {
    direct_config(
        ureq::Agent::config_builder(),
        Duration::from_secs(HEALTH_RESPONSE_SECS),
    )
    .build()
    .into()
}

/// `builder` bounded for a gateway request: the health probe's connect phase,
/// `response` for the response and the body, and their sum as one global
/// bound (ureq re-arms its response deadline before every wait,
/// `oauth::TOKEN_HTTP_DEADLINES`). No redirect is followed and no proxy is
/// used, whatever `builder` carried: through an env-configured proxy a
/// loopback target resolves on the proxy's own host, any other target's
/// delays and errors would be the proxy's, and the admin key would reach it.
fn direct_config(
    builder: ureq::config::ConfigBuilder<ureq::typestate::AgentScope>,
    response: Duration,
) -> ureq::config::ConfigBuilder<ureq::typestate::AgentScope> {
    let connect = Duration::from_secs(HEALTH_CONNECT_SECS);
    builder
        .timeout_connect(Some(connect))
        .timeout_recv_response(Some(response))
        .timeout_recv_body(Some(response))
        .timeout_global(Some(connect + response))
        .http_status_as_error(false)
        .max_redirects(0)
        .max_redirects_will_error(false)
        .proxy(None)
}

#[derive(Deserialize)]
struct HealthBody {
    version: String,
}

/// `GET http://<addr>/health` under [`health_agent`]'s bounds: the status and
/// body. Only a refused connection reads `None` (silent), on every platform: a
/// connect that times out, or a listener that takes the connection and never
/// answers, is an error, never an empty port. A non-200 status returns an
/// empty body (its caller never reads it). The two probes parse the returned
/// status/body their own way, so one trust-boundary HTTP read serves both.
fn get_health(addr: SocketAddr) -> Result<Option<(u16, Vec<u8>)>> {
    let agent = health_agent();
    let url = format!("http://{addr}/health");
    let mut response = match agent.get(&url).call() {
        Ok(response) => response,
        Err(ureq::Error::Io(e)) if e.kind() == std::io::ErrorKind::ConnectionRefused => {
            return Ok(None);
        }
        Err(e) => return Err(anyhow::Error::new(e).context(format!("GET {url}"))),
    };
    let status = response.status().as_u16();
    if status != 200 {
        return Ok(Some((status, Vec::new())));
    }
    let body = match response
        .body_mut()
        .with_config()
        .limit(HEALTH_BODY_LIMIT)
        .read_to_vec()
    {
        Ok(body) => body,
        Err(ureq::Error::BodyExceedsLimit(_)) => return Ok(Some((status, Vec::new()))),
        Err(e) => {
            return Err(anyhow::Error::new(e).context(format!("GET {url}: reading the body")));
        }
    };
    Ok(Some((status, body)))
}

/// `GET http://<addr>/health`, its connect and response phases bounded apart
/// and together by [`HEALTH_PROBE_TIMEOUT`]. Only a refused connection reads
/// as [`Health::Silent`], on every platform: a connect that times out, or a
/// listener that takes the connection and never answers, is an error, never
/// an empty port.
pub(crate) fn probe_health(addr: SocketAddr) -> Result<Health> {
    let Some((status, body)) = get_health(addr)? else {
        return Ok(Health::Silent(GatewaySilent { addr }));
    };
    if status != 200 {
        return Ok(Health::NotShunt { status });
    }
    Ok(match serde_json::from_slice::<HealthBody>(&body) {
        Ok(health) => Health::Shunt {
            version: health.version,
        },
        Err(_) => Health::NotShunt { status },
    })
}

// ── the proxy /health probe ────────────────────────────────────────────────

/// A clauth proxy's `/health` answer: `{status, service, version, contract}`,
/// the core's `server.ts`. `service` is `None` when the body omits it, which
/// the proxy supervisor refuses by name rather than treat as healthy.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct ProxyHealth {
    pub(crate) status: String,
    pub(crate) service: Option<String>,
    pub(crate) version: String,
    pub(crate) contract: String,
}

/// What answered `GET /health` at a proxy's bind address.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ProxyProbe {
    /// Nothing listens there: the connection was refused.
    Silent,
    /// A proxy-shaped `/health` body.
    Answered(ProxyHealth),
    /// Something answered, but not with a proxy `/health` body.
    NotProxy { status: u16 },
}

#[derive(Deserialize)]
struct ProxyHealthBody {
    status: String,
    #[serde(default)]
    service: Option<String>,
    version: String,
    contract: String,
}

/// `GET http://<addr>/health`, parsed as a proxy's body, under
/// [`health_agent`]'s bounds. Only a refused connection reads
/// [`ProxyProbe::Silent`]; a body missing a required field reads
/// [`ProxyProbe::NotProxy`], and a body missing `service` alone reads
/// [`ProxyProbe::Answered`] with `service: None`, which the caller refuses by
/// name.
pub(crate) fn probe_proxy_health(addr: SocketAddr) -> Result<ProxyProbe> {
    let Some((status, body)) = get_health(addr)? else {
        return Ok(ProxyProbe::Silent);
    };
    if status != 200 {
        return Ok(ProxyProbe::NotProxy { status });
    }
    Ok(match serde_json::from_slice::<ProxyHealthBody>(&body) {
        Ok(health) => ProxyProbe::Answered(ProxyHealth {
            status: health.status,
            service: health.service,
            version: health.version,
            contract: health.contract,
        }),
        Err(_) => ProxyProbe::NotProxy { status },
    })
}

// ── the admin entry ─────────────────────────────────────────────────────────

/// Which admin step the adopted config needs for clauth's write key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AdminNeed {
    /// clauth's entry is already there.
    Neither,
    /// `[server.admin]` exists without clauth's entry.
    WriteKey,
    /// No `[server.admin]` table: adding one enables shunt's admin API.
    AdminTable,
}

/// What an admin edit did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AdminEdit {
    Written,
    AlreadyPresent,
}

const SERVER_SHAPE: &str = "[server] is not a table";
const ADMIN_SHAPE: &str = "[server.admin] is not a table";
const WRITE_KEYS_SHAPE: &str = "[server.admin].write_keys is not an array of tables";

/// Why an admin edit wrote nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ConfigEditRefusal {
    /// The config needs the other step than the one asked for.
    Needs(AdminNeed),
    /// A write key already carries the id `clauth` with another key.
    ForeignClauthKey,
    /// `[server]`, `[server.admin]` or `write_keys` has a shape clauth does
    /// not edit.
    UnexpectedShape { what: &'static str },
    /// The token path cannot be spelled as a `${file:}` reference.
    TokenPathUnusable { path: PathBuf },
    /// The config is a symlink; renaming over it would replace the link.
    Symlink { path: PathBuf },
    /// The shunt binary is not there to run the check.
    ShuntMissing { binary: PathBuf },
    /// `shunt check` refused the candidate.
    CheckFailed {
        binary: PathBuf,
        config: PathBuf,
        code: Option<i32>,
        stderr: CheckStderr,
    },
    /// The config changed on disk while its edit was being checked.
    ChangedDuringEdit { path: PathBuf },
    /// `shunt check` outran its bound and was stopped.
    CheckTimedOut {
        binary: PathBuf,
        config: PathBuf,
        after: Duration,
    },
}

/// `shunt check`'s stderr, its first [`CHECK_STDERR_LIMIT`] bytes. shunt's
/// config errors quote substituted values, which may come from the env
/// file, so `Debug` never prints it (the [`AdminToken`] precedent).
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct CheckStderr(String);

impl CheckStderr {
    /// The text, for a surface that shows the user their own check's output.
    pub(crate) fn text(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for CheckStderr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("CheckStderr(<redacted>)")
    }
}

impl std::fmt::Display for ConfigEditRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConfigEditRefusal::Needs(AdminNeed::AdminTable) => f.write_str(
                "the config has no [server.admin] table; adding one enables shunt's admin API and is its own step",
            ),
            ConfigEditRefusal::Needs(AdminNeed::WriteKey) => f.write_str(
                "the config already has a [server.admin] table; clauth's write key goes into it instead",
            ),
            ConfigEditRefusal::Needs(AdminNeed::Neither) => {
                f.write_str("the config already carries clauth's write key")
            }
            ConfigEditRefusal::ForeignClauthKey => write!(
                f,
                "[server.admin] already has a write key with id {WRITE_KEY_ID:?} holding another key; clauth adds none beside it; remove that entry, then run the edit again"
            ),
            ConfigEditRefusal::UnexpectedShape { what } => write!(
                f,
                "{what}; clauth edits only a [server.admin] table and its write_keys array"
            ),
            ConfigEditRefusal::TokenPathUnusable { path } => write!(
                f,
                "the admin token path {} cannot be written as a ${{file:}} reference",
                path.display()
            ),
            ConfigEditRefusal::Symlink { path } => write!(
                f,
                "{} is a symlink; clauth lands its edit by renaming over the config, which would replace the link",
                path.display()
            ),
            ConfigEditRefusal::ShuntMissing { binary } => write!(
                f,
                "cannot run {}: no such file; install shunt (brew, a release binary or cargo install --git) or point the gateway at its binary",
                binary.display()
            ),
            ConfigEditRefusal::CheckFailed {
                binary,
                config,
                code,
                ..
            } => {
                let status = match code {
                    Some(code) => format!("exit {code}"),
                    None => "killed by a signal".to_string(),
                };
                write!(
                    f,
                    "`{} check` refused clauth's edit of {} ({status}); the config is unchanged",
                    binary.display(),
                    config.display()
                )
            }
            ConfigEditRefusal::ChangedDuringEdit { path } => write!(
                f,
                "{} changed while clauth's edit was being checked; nothing was written; run the edit again",
                path.display()
            ),
            ConfigEditRefusal::CheckTimedOut {
                binary,
                config,
                after,
            } => write!(
                f,
                "`{binary} check` ran past {after:?} and was stopped; the config is unchanged; run `{binary} check --config {config}` yourself to see why it does not finish, then try again",
                binary = binary.display(),
                config = config.display()
            ),
        }
    }
}

impl std::error::Error for ConfigEditRefusal {}

/// Which admin step `config` needs.
pub(crate) fn admin_need(config: &Path) -> Result<AdminNeed> {
    admin_need_of(&read_config_text(config)?, &admin_key_ref()?)
}

/// [`admin_need`] over a config's text and clauth's `${file:}` reference.
fn admin_need_of(text: &str, key_ref: &str) -> Result<AdminNeed> {
    Ok(need_in(&parse_config(text)?, key_ref)?)
}

/// The candidate text for `step`, or `None` when clauth's entry is present.
fn plan_admin_edit(text: &str, key_ref: &str, step: AdminNeed) -> Result<Option<String>> {
    let mut doc = parse_config(text)?;
    match (need_in(&doc, key_ref)?, step) {
        (AdminNeed::Neither, _) => return Ok(None),
        (AdminNeed::WriteKey, AdminNeed::WriteKey) => insert_write_key(&mut doc, key_ref)?,
        (AdminNeed::AdminTable, AdminNeed::AdminTable) => insert_admin_table(&mut doc, key_ref)?,
        (need, _) => return Err(ConfigEditRefusal::Needs(need).into()),
    }
    Ok(Some(doc.to_string()))
}

/// Add clauth's write key to an existing `[server.admin]`, landing the edit
/// only after `shunt check` passes. shunt hot-applies it.
pub(crate) fn add_admin_write_key(record: &GatewayRecord) -> Result<AdminEdit> {
    apply_admin_step(record, AdminNeed::WriteKey)
}

/// Add a `[server.admin]` table carrying clauth's write key, behind the same
/// check. shunt registers its admin routes at boot, so this one takes a
/// gateway restart.
pub(crate) fn add_admin_table(record: &GatewayRecord) -> Result<AdminEdit> {
    apply_admin_step(record, AdminNeed::AdminTable)
}

fn apply_admin_step(record: &GatewayRecord, step: AdminNeed) -> Result<AdminEdit> {
    let key_ref = admin_key_ref()?;
    // Even when the entry is already there: shunt resolves `${file:}` at
    // load, so the check needs the file, and a deleted one is re-minted.
    ensure_admin_token()?;
    let (original, text) = read_config_for_edit(record.config())?;
    let Some(candidate) = plan_admin_edit(&text, &key_ref, step)? else {
        return Ok(AdminEdit::AlreadyPresent);
    };
    write_checked(record, &original, &candidate)?;
    Ok(AdminEdit::Written)
}

/// `${file:<token path>}`: shunt requires the reference to be the key's
/// whole value, and its reference ends at the first `}`.
fn admin_key_ref() -> Result<String> {
    let path = admin_token_path()?;
    match path.to_str() {
        Some(spelled) if !spelled.contains('}') => Ok(format!("${{file:{spelled}}}")),
        _ => Err(ConfigEditRefusal::TokenPathUnusable { path }.into()),
    }
}

/// The config's bytes, the original [`write_checked`] compares against, and
/// their text, which an edit must be: UTF-8.
pub(crate) fn read_config_for_edit(config: &Path) -> Result<(Vec<u8>, String)> {
    let original =
        std::fs::read(config).with_context(|| format!("failed to read {}", config.display()))?;
    let text = String::from_utf8(original.clone())
        .with_context(|| format!("{} is not UTF-8", config.display()))?;
    Ok((original, text))
}

fn read_config_text(config: &Path) -> Result<String> {
    std::fs::read_to_string(config).with_context(|| format!("failed to read {}", config.display()))
}

/// The user's config may hold literal upstream keys, so a parse error names
/// the line and never quotes it the way the parser's own message does.
fn parse_config(text: &str) -> Result<DocumentMut> {
    text.parse::<DocumentMut>()
        .map_err(|e| config_parse_error(text, &e))
}

/// The error for a config `text` that does not parse: the line, never the
/// parser's own message, which quotes the text.
pub(crate) fn config_parse_error(text: &str, e: &toml_edit::TomlError) -> anyhow::Error {
    match e.span().and_then(|span| text.get(..span.start)) {
        Some(head) => anyhow!(
            "the shunt config does not parse as TOML (line {})",
            head.matches('\n').count() + 1
        ),
        None => anyhow!("the shunt config does not parse as TOML"),
    }
}

fn need_in(doc: &DocumentMut, key_ref: &str) -> Result<AdminNeed, ConfigEditRefusal> {
    let shape = |what| ConfigEditRefusal::UnexpectedShape { what };
    let Some(server) = doc.get("server") else {
        return Ok(AdminNeed::AdminTable);
    };
    let server = server.as_table_like().ok_or(shape(SERVER_SHAPE))?;
    let Some(admin) = server.get("admin") else {
        return Ok(AdminNeed::AdminTable);
    };
    let admin = admin.as_table_like().ok_or(shape(ADMIN_SHAPE))?;
    let Some(keys) = admin.get("write_keys") else {
        return Ok(AdminNeed::WriteKey);
    };
    let entries: Vec<&dyn TableLike> = match keys {
        Item::ArrayOfTables(keys) => keys.iter().map(|t| t as &dyn TableLike).collect(),
        Item::Value(Value::Array(keys)) => keys
            .iter()
            .map(|v| v.as_inline_table().map(|t| t as &dyn TableLike))
            .collect::<Option<_>>()
            .ok_or(shape(WRITE_KEYS_SHAPE))?,
        _ => return Err(shape(WRITE_KEYS_SHAPE)),
    };
    for entry in entries {
        if entry.get("id").and_then(Item::as_str) == Some(WRITE_KEY_ID) {
            return if entry.get("key").and_then(Item::as_str) == Some(key_ref) {
                Ok(AdminNeed::Neither)
            } else {
                Err(ConfigEditRefusal::ForeignClauthKey)
            };
        }
    }
    Ok(AdminNeed::WriteKey)
}

fn entry_table(key_ref: &str) -> Table {
    let mut entry = Table::new();
    entry.insert("id", toml_edit::value(WRITE_KEY_ID));
    entry.insert("key", toml_edit::value(key_ref));
    entry
}

fn entry_inline(key_ref: &str) -> InlineTable {
    let mut entry = InlineTable::new();
    entry.insert("id", Value::from(WRITE_KEY_ID));
    entry.insert("key", Value::from(key_ref));
    entry
}

fn inline_write_keys(key_ref: &str) -> Value {
    let mut keys = Array::new();
    keys.push(entry_inline(key_ref));
    Value::Array(keys)
}

fn insert_write_key(doc: &mut DocumentMut, key_ref: &str) -> Result<(), ConfigEditRefusal> {
    let shape = ConfigEditRefusal::UnexpectedShape { what: ADMIN_SHAPE };
    match doc.get_mut("server") {
        Some(Item::Table(server)) => match server.get_mut("admin") {
            Some(Item::Table(admin)) => push_to_table(admin, key_ref),
            Some(Item::Value(Value::InlineTable(admin))) => push_to_inline(admin, key_ref),
            _ => Err(shape),
        },
        Some(Item::Value(Value::InlineTable(server))) => match server.get_mut("admin") {
            Some(Value::InlineTable(admin)) => push_to_inline(admin, key_ref),
            _ => Err(shape),
        },
        _ => Err(shape),
    }
}

fn push_to_table(admin: &mut Table, key_ref: &str) -> Result<(), ConfigEditRefusal> {
    match admin.get_mut("write_keys") {
        None => {
            let mut keys = ArrayOfTables::new();
            keys.push(entry_table(key_ref));
            admin.insert("write_keys", Item::ArrayOfTables(keys));
        }
        Some(Item::ArrayOfTables(keys)) => keys.push(entry_table(key_ref)),
        Some(Item::Value(Value::Array(keys))) => keys.push(entry_inline(key_ref)),
        Some(_) => {
            return Err(ConfigEditRefusal::UnexpectedShape {
                what: WRITE_KEYS_SHAPE,
            });
        }
    }
    Ok(())
}

/// Insert `value` at `key` in an inline table, moving the last value's
/// pre-`}` space (its suffix decor) onto the inserted value's suffix, so the
/// comma the insert adds sits right after the last value instead of after a
/// stray space.
fn insert_inline(table: &mut InlineTable, key: &str, value: Value) {
    let moved = table.iter_mut().last().and_then(|(_, last)| {
        let decor = last.decor_mut();
        let suffix = decor.suffix().cloned();
        decor.set_suffix("");
        suffix
    });
    table.insert(key, value);
    if let Some(suffix) = moved
        && let Some(value) = table.get_mut(key)
    {
        value.decor_mut().set_suffix(suffix);
    }
}

fn push_to_inline(admin: &mut InlineTable, key_ref: &str) -> Result<(), ConfigEditRefusal> {
    match admin.get_mut("write_keys") {
        None => {
            insert_inline(admin, "write_keys", inline_write_keys(key_ref));
        }
        Some(Value::Array(keys)) => keys.push(entry_inline(key_ref)),
        Some(_) => {
            return Err(ConfigEditRefusal::UnexpectedShape {
                what: WRITE_KEYS_SHAPE,
            });
        }
    }
    Ok(())
}

fn insert_admin_table(doc: &mut DocumentMut, key_ref: &str) -> Result<(), ConfigEditRefusal> {
    let admin_table = || {
        let mut keys = ArrayOfTables::new();
        keys.push(entry_table(key_ref));
        let mut admin = Table::new();
        admin.insert("write_keys", Item::ArrayOfTables(keys));
        Item::Table(admin)
    };
    match doc.get_mut("server") {
        None => {
            // Implicit, so the file gains `[server.admin]` and no empty
            // `[server]` header above it.
            let mut server = Table::new();
            server.set_implicit(true);
            server.insert("admin", admin_table());
            doc.insert("server", Item::Table(server));
        }
        Some(Item::Table(server)) => {
            server.insert("admin", admin_table());
        }
        Some(Item::Value(Value::InlineTable(server))) => {
            let mut admin = InlineTable::new();
            admin.insert("write_keys", inline_write_keys(key_ref));
            insert_inline(server, "admin", Value::InlineTable(admin));
        }
        Some(_) => {
            return Err(ConfigEditRefusal::UnexpectedShape { what: SERVER_SHAPE });
        }
    }
    Ok(())
}

/// Land `candidate` over the adopted config: staged beside it with the
/// original's mode bits, `shunt check`ed under the gateway's env, and renamed
/// over the original only if the check passed and the original still holds
/// the bytes the candidate was built from. Any other outcome removes the
/// staging file and leaves the original byte-identical. Every clauth edit of
/// the adopted config lands here: the admin entry and the config editor
/// (`crate::gateway_edit`).
pub(crate) fn write_checked(
    record: &GatewayRecord,
    original: &[u8],
    candidate: &str,
) -> Result<()> {
    let config = record.config();
    let meta = std::fs::symlink_metadata(config)
        .with_context(|| format!("failed to inspect {}", config.display()))?;
    if meta.file_type().is_symlink() {
        return Err(ConfigEditRefusal::Symlink {
            path: config.to_path_buf(),
        }
        .into());
    }
    let env = gateway_env(record)?;
    let (staged, mut file) = Staged::create(config)
        .with_context(|| format!("failed to stage an edit beside {}", config.display()))?;
    file.write_all(candidate.as_bytes())
        .and_then(|()| file.sync_all())
        .with_context(|| format!("failed to write {}", staged.path().display()))?;
    drop(file);
    std::fs::set_permissions(staged.path(), meta.permissions())
        .with_context(|| format!("failed to set the mode of {}", staged.path().display()))?;
    run_check(
        record.shunt_binary(),
        staged.path(),
        config,
        gateway_cwd(record)?,
        &env,
        None,
    )?;
    let now =
        std::fs::read(config).with_context(|| format!("failed to re-read {}", config.display()))?;
    if now != original {
        return Err(ConfigEditRefusal::ChangedDuringEdit {
            path: config.to_path_buf(),
        }
        .into());
    }
    staged
        .rename_over(config)
        .with_context(|| format!("failed to replace {}", config.display()))
}

/// How long `shunt check` may run before clauth stops it. Measured
/// 2026-09-27 on shunt 0.47.0 with an admin `${file:}` key: 51 ms on a cold
/// first run, 2.2 ms warm (max 2.9 ms over 20 runs). 10 s is two orders over
/// the cold load, so only a wedged check reaches it, and no lock is held
/// while the caller waits.
pub(crate) const CHECK_TIMEOUT: Duration = Duration::from_secs(10);

/// How often a bounded child's exit is polled.
const CHECK_POLL_INTERVAL: Duration = Duration::from_millis(25);

fn check_timeout() -> Duration {
    #[cfg(test)]
    if let Some(bound) = CHECK_TIMEOUT_OVERRIDE.get() {
        return bound;
    }
    CHECK_TIMEOUT
}

#[cfg(test)]
thread_local! {
    static CHECK_TIMEOUT_OVERRIDE: std::cell::Cell<Option<Duration>> =
        const { std::cell::Cell::new(None) };
}

/// Test seam shortening the check's bound on this thread (the check runs on
/// its caller's thread), so a stuck check is posed without a real wait;
/// cleared on drop.
#[cfg(test)]
#[cfg_attr(
    not(unix),
    expect(dead_code, reason = "used only by the unix gate tests")
)]
pub(crate) struct CheckTimeoutOverride(());

#[cfg(test)]
impl CheckTimeoutOverride {
    #[cfg_attr(
        not(unix),
        expect(dead_code, reason = "used only by the unix gate tests")
    )]
    pub(crate) fn set(bound: Duration) -> Self {
        CHECK_TIMEOUT_OVERRIDE.set(Some(bound));
        Self(())
    }
}

#[cfg(test)]
impl Drop for CheckTimeoutOverride {
    fn drop(&mut self) {
        CHECK_TIMEOUT_OVERRIDE.set(None);
    }
}

/// `<binary> check --config <candidate>` in `cwd` under `env`, bounded by
/// [`CHECK_TIMEOUT`]: a check that outruns it is killed and reaped, it being
/// clauth's own child, and the edit refuses. `cancel`, when given, preempts
/// the bound: a flag set when the check starts (or while it runs) cancels it
/// at once instead of waiting it out.
fn run_check(
    binary: &Path,
    candidate: &Path,
    config: &Path,
    cwd: &Path,
    env: &GatewayEnv,
    cancel: Option<&std::sync::atomic::AtomicBool>,
) -> Result<()> {
    let mut command = Command::new(binary);
    command
        .arg("check")
        .arg("--config")
        .arg(candidate)
        .current_dir(cwd);
    env.apply(&mut command);
    let bound = check_timeout();
    let exited = match run_bounded_impl(&mut command, binary, bound, None, cancel)? {
        Bounded::Missing => {
            return Err(ConfigEditRefusal::ShuntMissing {
                binary: binary.to_path_buf(),
            }
            .into());
        }
        Bounded::TimedOut => {
            return Err(ConfigEditRefusal::CheckTimedOut {
                binary: binary.to_path_buf(),
                config: config.to_path_buf(),
                after: bound,
            }
            .into());
        }
        Bounded::Exited(exited) => exited,
    };
    if exited.status.success() {
        return Ok(());
    }
    Err(ConfigEditRefusal::CheckFailed {
        binary: binary.to_path_buf(),
        config: config.to_path_buf(),
        code: exited.status.code(),
        stderr: CheckStderr(String::from_utf8_lossy(&exited.stderr()).trim().to_string()),
    }
    .into())
}

/// `shunt check` on the adopted config as it now stands, through the same
/// [`run_check`] the edit path uses (env and cwd built exactly as
/// [`write_checked`]'s check does), with the caller's `cancel` so a daemon
/// shutdown preempts a wedged check instead of waiting [`CHECK_TIMEOUT`] out.
/// Refusals carry the same [`ConfigEditRefusal`] the edit path maps.
pub(crate) fn check_adopted_config(
    record: &GatewayRecord,
    cancel: &std::sync::atomic::AtomicBool,
) -> Result<()> {
    run_check(
        record.shunt_binary(),
        record.config(),
        record.config(),
        gateway_cwd(record)?,
        &gateway_env(record)?,
        Some(cancel),
    )
}

/// How a [`run_bounded`] child ended.
pub(crate) enum Bounded {
    /// `binary` is not there to run.
    Missing,
    /// The child outran its bound and was killed and reaped.
    TimedOut,
    Exited(Exited),
}

/// A bounded child that exited on its own, its output still arriving on the
/// drain threads.
pub(crate) struct Exited {
    pub(crate) status: std::process::ExitStatus,
    stdout: Option<Receiver<Vec<u8>>>,
    stderr: Option<Receiver<Vec<u8>>>,
    deadline: Instant,
}

impl Exited {
    /// The stdout that arrived before the run's deadline, up to the limit the
    /// run was given.
    pub(crate) fn stdout(&self) -> Vec<u8> {
        self.stdout
            .as_ref()
            .map(|chunks| collect_output(chunks, self.deadline))
            .unwrap_or_default()
    }

    /// The first [`CHECK_STDERR_LIMIT`] bytes of stderr that arrived before
    /// the run's deadline.
    pub(crate) fn stderr(&self) -> Vec<u8> {
        self.stderr
            .as_ref()
            .map(|chunks| collect_output(chunks, self.deadline))
            .unwrap_or_default()
    }
}

/// `command` run with stdin closed and stderr drained on its own thread, and
/// stdout too when `stdout_limit` is given (else discarded), each keeping its
/// first bytes up to its limit, bounded by `bound`: a child that outruns it is
/// killed and reaped, it being clauth's own child. `binary` names the program
/// in errors.
pub(crate) fn run_bounded(
    command: &mut Command,
    binary: &Path,
    bound: Duration,
    stdout_limit: Option<usize>,
) -> Result<Bounded> {
    run_bounded_impl(command, binary, bound, stdout_limit, None)
}

/// [`run_bounded`] under a cancellation flag: once `cancel` is set the child
/// is killed and reaped at once and the run reports the cancellation, instead
/// of waiting its bound out. `cancel` is the stop signal a supervisor shares
/// with its thread, so a shutdown preempts a proxy's `manifest` read.
pub(crate) fn run_bounded_cancellable(
    command: &mut Command,
    binary: &Path,
    bound: Duration,
    stdout_limit: Option<usize>,
    cancel: &std::sync::atomic::AtomicBool,
) -> Result<Bounded> {
    run_bounded_impl(command, binary, bound, stdout_limit, Some(cancel))
}

fn run_bounded_impl(
    command: &mut Command,
    binary: &Path,
    bound: Duration,
    stdout_limit: Option<usize>,
    cancel: Option<&std::sync::atomic::AtomicBool>,
) -> Result<Bounded> {
    use std::sync::atomic::Ordering;

    command
        .stdin(Stdio::null())
        .stdout(if stdout_limit.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stderr(Stdio::piped());
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Bounded::Missing),
        Err(e) => return Err(e).with_context(|| format!("cannot run {}", binary.display())),
    };
    let stdout = child
        .stdout
        .take()
        .zip(stdout_limit)
        .map(|(pipe, limit)| drain_output(pipe, limit));
    let stderr = child
        .stderr
        .take()
        .map(|pipe| drain_output(pipe, CHECK_STDERR_LIMIT));
    let deadline = Instant::now() + bound;
    let status = loop {
        if cancel.is_some_and(|cancel| cancel.load(Ordering::Acquire)) {
            // A stop asked for while the child ran: kill and reap it now, the
            // caller is ending and must not wait the bound out.
            let _ = child.kill();
            child
                .wait()
                .with_context(|| format!("failed to reap {}", binary.display()))?;
            return Err(anyhow!("{} was cancelled while it ran", binary.display()));
        }
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(CHECK_POLL_INTERVAL),
            outcome => {
                // Only a child that already exited refuses the kill; the wait
                // reaps it either way, so none is ever left running.
                let _ = child.kill();
                child
                    .wait()
                    .with_context(|| format!("failed to reap {}", binary.display()))?;
                return match outcome {
                    Err(e) => {
                        Err(e).with_context(|| format!("failed to wait on {}", binary.display()))
                    }
                    Ok(_) => Ok(Bounded::TimedOut),
                };
            }
        }
    };
    Ok(Bounded::Exited(Exited {
        status,
        stdout,
        stderr,
        deadline,
    }))
}

/// How much of a bounded child's stderr is kept. shunt 0.47.0's failing
/// `check` wrote 175 to 595 bytes (measured 2026-09-27, three broken
/// configs), so a real report stays whole and a runaway one stays bounded.
const CHECK_STDERR_LIMIT: usize = 64 * 1024;

/// Read a child's pipe on its own thread, so a chatty child never stalls on
/// a full pipe: its first `limit` bytes come back in chunks as they arrive,
/// and the rest is read and dropped.
fn drain_output(mut pipe: impl std::io::Read + Send + 'static, limit: usize) -> Receiver<Vec<u8>> {
    let (sender, chunks) = mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        let mut room = limit;
        loop {
            let read = match pipe.read(&mut buf) {
                Ok(0) => break,
                Ok(read) => read,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            };
            let kept = read.min(room);
            if let Some(chunk) = buf.get(..kept).filter(|chunk| !chunk.is_empty()) {
                room -= kept;
                // A caller that stopped listening leaves the rest to drain.
                let _ = sender.send(chunk.to_vec());
            }
        }
    });
    chunks
}

/// The output that arrives before `deadline`, the child's own bound: a
/// process the child started can hold the pipe open past the child's exit.
fn collect_output(chunks: &Receiver<Vec<u8>>, deadline: Instant) -> Vec<u8> {
    let mut bytes = Vec::new();
    while let Ok(chunk) = chunks.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
        bytes.extend(chunk);
    }
    bytes
}

/// An owner-only staging file beside its target, removed on drop unless it
/// was published over or as the target.
struct Staged {
    path: PathBuf,
    armed: bool,
}

impl Staged {
    fn create(target: &Path) -> std::io::Result<(Self, File)> {
        let path = tmp_sibling(target);
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let file = options.open(&path)?;
        Ok((Self { path, armed: true }, file))
    }

    fn path(&self) -> &Path {
        &self.path
    }

    /// Replace `target`.
    fn rename_over(mut self, target: &Path) -> std::io::Result<()> {
        std::fs::rename(&self.path, target)?;
        self.armed = false;
        Ok(())
    }

    /// Publish as `target`, which must not exist: a hard link refuses an
    /// existing name where a rename would replace it. The store move's
    /// publish, so a failure says whether the file reached `target`.
    fn link_as(mut self, target: &Path) -> Result<(), MoveFailure> {
        std::fs::hard_link(&self.path, target).map_err(MoveFailure::BeforeCopy)?;
        self.armed = false;
        std::fs::remove_file(&self.path).map_err(MoveFailure::AfterCopy)
    }
}

impl Drop for Staged {
    fn drop(&mut self) {
        if self.armed {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

// ── the stores ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StoreKind {
    Dir,
    File,
}

/// Where a standalone kept a store, as shunt reads the store's env var.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Source {
    /// The account dirs (shunt `env_path_override`): an empty or whitespace
    /// value is unset, so shunt's default under `~/.shunt` is the source.
    BlankIsUnset,
    /// `SHUNT_ANTIGRAVITY_AUTH_FILE`: only an empty value is unset.
    EmptyIsUnset,
    /// `SHUNT_XAI_AUTH_FILE`, `SHUNT_CURSOR_AUTH_FILE`: shunt takes the raw
    /// value, so an empty one names no store at all.
    Raw,
    /// `CODEX_AUTH_FILE`: the raw value, and a source only where the env
    /// file sets it. shunt's fallback, `~/.codex/auth.json`, is the codex
    /// CLI's own login or a clauth codex profile's link, never a standalone
    /// store.
    NamedOnly,
    /// `CLAUDE_CREDENTIALS`: pinned, so shunt's admin usage view never
    /// reads a clauth profile's login from its `~/.claude` fallback; never
    /// moved.
    PinnedOnly,
}

/// How a store family's shunt site resolves the home its default sits under:
/// `HOME` (non-empty) falling back to `USERPROFILE` (non-empty) for the
/// account dirs, cursor and antigravity (`home_dir`, shunt
/// `src/auth/shared.rs:237-244`), or raw `HOME` with no `USERPROFILE`
/// fallback for xai (shunt `src/auth/mod.rs:522-528`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HomeRule {
    HomeThenUserProfile,
    RawHome,
}

/// One credential location shunt reads: the env var the gateway reads it
/// from, and its path under both roots (`~/.shunt` standalone,
/// `~/.clauth/shunt` managed).
struct Store {
    env: &'static str,
    segments: &'static [&'static str],
    kind: StoreKind,
    source: Source,
    /// How the store's shunt site resolves its default home, and `None` for
    /// a store whose source is never shunt's default (`CODEX_AUTH_FILE`,
    /// `CLAUDE_CREDENTIALS`).
    home: Option<HomeRule>,
}

/// The store env key whose managed path holds the pinned single-file codex
/// login.
const CODEX_AUTH_FILE_ENV: &str = "CODEX_AUTH_FILE";

const STORES: [Store; 9] = [
    Store {
        env: "SHUNT_CLAUDE_ACCOUNTS_DIR",
        segments: &["accounts", "claude"],
        kind: StoreKind::Dir,
        source: Source::BlankIsUnset,
        home: Some(HomeRule::HomeThenUserProfile),
    },
    Store {
        env: "SHUNT_CODEX_ACCOUNTS_DIR",
        segments: &["accounts", "codex"],
        kind: StoreKind::Dir,
        source: Source::BlankIsUnset,
        home: Some(HomeRule::HomeThenUserProfile),
    },
    Store {
        env: "SHUNT_KIMI_ACCOUNTS_DIR",
        segments: &["accounts", "kimi"],
        kind: StoreKind::Dir,
        source: Source::BlankIsUnset,
        home: Some(HomeRule::HomeThenUserProfile),
    },
    Store {
        env: "SHUNT_ANTIGRAVITY_ACCOUNTS_DIR",
        segments: &["accounts", "antigravity"],
        kind: StoreKind::Dir,
        source: Source::BlankIsUnset,
        home: Some(HomeRule::HomeThenUserProfile),
    },
    Store {
        env: "SHUNT_XAI_AUTH_FILE",
        segments: &["xai-auth.json"],
        kind: StoreKind::File,
        source: Source::Raw,
        home: Some(HomeRule::RawHome),
    },
    Store {
        env: "SHUNT_CURSOR_AUTH_FILE",
        segments: &["cursor-auth.json"],
        kind: StoreKind::File,
        source: Source::Raw,
        home: Some(HomeRule::HomeThenUserProfile),
    },
    Store {
        env: "SHUNT_ANTIGRAVITY_AUTH_FILE",
        segments: &["antigravity-auth.json"],
        kind: StoreKind::File,
        source: Source::EmptyIsUnset,
        home: Some(HomeRule::HomeThenUserProfile),
    },
    Store {
        env: CODEX_AUTH_FILE_ENV,
        segments: &["codex-auth.json"],
        kind: StoreKind::File,
        source: Source::NamedOnly,
        home: None,
    },
    Store {
        env: "CLAUDE_CREDENTIALS",
        segments: &["claude-credentials.json"],
        kind: StoreKind::File,
        source: Source::PinnedOnly,
        home: None,
    },
];

fn under(root: &Path, segments: &[&str]) -> PathBuf {
    segments
        .iter()
        .fold(root.to_path_buf(), |path, segment| path.join(segment))
}

fn managed_store_root() -> Result<PathBuf> {
    Ok(clauth_dir()?.join("shunt"))
}

/// One credential moved out of a standalone store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MovedFile {
    pub(crate) from: PathBuf,
    pub(crate) to: PathBuf,
}

/// What a store move did: every credential it moved, and every one it left
/// at its source on purpose.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StoreMove {
    pub(crate) moved: Vec<MovedFile>,
    pub(crate) kept: Vec<KeptFile>,
}

/// A file or dir the move left at its source on purpose, named by path, or a
/// store it left out of the move, named by its env key — never an empty path
/// standing in for a store. The two shapes are distinct: a `NoHome` entry
/// names a store, every other entry a path, so an illegal pairing is
/// unrepresentable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum KeptFile {
    /// A file or dir left at this path, for `reason`.
    At { path: PathBuf, reason: KeptReason },
    /// A whole store left out of the move because its shunt site has a default
    /// but no source named a usable home, named by its env key.
    NoHome { store: &'static str },
}

/// Why the move left a file or dir at its source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum KeptReason {
    /// It is `~/.codex/auth.json` or `$CODEX_HOME/auth.json`, the codex CLI's
    /// own login, or a codex home (`~/.codex`, `$CODEX_HOME`) a dir store
    /// points at.
    CodexLogin,
    /// It lies under `~/.clauth`, which clauth owns.
    ClauthOwned,
    /// The file has more than one hard link, so another name may be another
    /// owner's login.
    HardLink,
    /// The file's link count could not be read, so another name may be
    /// another owner's login; kept fail-closed.
    LinkCountUnreadable,
    /// A subdir, link or non-account file inside a store dir that shunt does
    /// not serve, left in the old dir.
    LeftBehind,
    /// A later store key names the same source file as an earlier planned
    /// entry; the first entry moves it, this one is listed instead.
    DuplicateSource,
}

/// Which of the places clauth can read the codex CLI's `CODEX_HOME` set a
/// value: the recorded env file, the running daemon's env record, or clauth's
/// own inherited environment (the no-daemon fallback). The refusal copy for a
/// relative value names the right one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CodexHomeSource {
    EnvFile,
    DaemonRecord,
    Inherited,
}

/// Why a store move stopped.
#[derive(Debug)]
pub(crate) enum StoreMoveRefusal {
    /// Destinations already holding a file; nothing was moved.
    Collision { paths: Vec<PathBuf> },
    /// A store that is itself a link, and resolves onto no other owner's
    /// login; nothing moved.
    StoreLink { path: PathBuf },
    /// A dir store is not a directory; nothing moved.
    StoreNotDir { path: PathBuf },
    /// A single-file store is not a regular file; nothing moved.
    StoreNotFile { path: PathBuf },
    /// The env file names a store at a relative path; nothing moved.
    RelativeSource { key: &'static str },
    /// The store's default resolved under a home that is relative, so shunt's
    /// default for the store is working-directory-relative; nothing moved.
    RelativeHome { store: &'static str },
    /// The home the codex CLI's own `~/.codex` login resolves under is
    /// relative, so the guard keeping that login out of the move cannot name
    /// it; nothing moved.
    RelativeCodexLoginHome,
    /// `CODEX_HOME` is relative; clauth cannot tell which codex login it
    /// names, so nothing moved.
    RelativeCodexHome { source: CodexHomeSource },
    /// A daemon holds the singleton but no record names it (missing, stale,
    /// unreadable or unparseable); nothing moved.
    DaemonEnvUnrecorded,
    /// The `silent` proof was minted at a different address than the one the
    /// record's config says to probe; nothing moved.
    SilentMismatch {
        silent: SocketAddr,
        probe: SocketAddr,
    },
    /// A move failed before `failed` reached its destination: `moved`
    /// landed, `failed` is at its source only, the rest never started.
    FailedBeforeCopy {
        moved: Vec<MovedFile>,
        failed: PathBuf,
        cause: std::io::Error,
    },
    /// A move failed once `failed` was published at its destination, so it
    /// is at both places; `moved` landed before it, the rest never started.
    FailedAfterCopy {
        moved: Vec<MovedFile>,
        failed: MovedFile,
        cause: std::io::Error,
    },
    /// The fresh plan at move time differs from the plan the user confirmed;
    /// nothing moved.
    PlanChanged,
}

impl std::fmt::Display for StoreMoveRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StoreMoveRefusal::Collision { paths } => {
                let paths: Vec<String> = paths.iter().map(|p| p.display().to_string()).collect();
                write!(
                    f,
                    "the managed store already holds {}; nothing was moved; compare each with its standalone copy, remove the one you no longer need, then run the move again",
                    paths.join(", ")
                )
            }
            StoreMoveRefusal::StoreLink { path } => write!(
                f,
                "{} is a link, and the move takes over only a store that is the directory or file itself; nothing was moved; replace the link with what it points at, or point the env file at the target, then run the move again",
                path.display()
            ),
            StoreMoveRefusal::StoreNotDir { path } => write!(
                f,
                "{} is not a directory, and the env file or shunt's default names it as an account store; nothing was moved; point the store at a directory, then run the move again",
                path.display()
            ),
            StoreMoveRefusal::StoreNotFile { path } => write!(
                f,
                "{} is not a regular file, and the env file or shunt's default names it as an account store; nothing was moved; point the store at a regular file, then run the move again",
                path.display()
            ),
            StoreMoveRefusal::RelativeSource { key } => write!(
                f,
                "the env file sets {key} to a relative path, and clauth cannot tell which directory the standalone resolved it against; nothing was moved; set {key} to an absolute path in the env file, then run the move again"
            ),
            StoreMoveRefusal::RelativeHome { store } => write!(
                f,
                "the standalone's home names no absolute directory, so shunt's default for {store} is relative to the standalone's working directory, which clauth cannot tell; nothing was moved; set HOME to an absolute path in the env file, then run the move again"
            ),
            StoreMoveRefusal::RelativeCodexLoginHome => f.write_str(
                "the standalone's home names no absolute directory, so clauth cannot tell where the codex CLI's own login (~/.codex/auth.json) sits; nothing was moved; set HOME to an absolute path in the env file, then run the move again",
            ),
            StoreMoveRefusal::RelativeCodexHome {
                source: CodexHomeSource::EnvFile,
            } => f.write_str(
                "the env file sets CODEX_HOME to a relative path, and clauth cannot tell which codex login it names; nothing was moved; set CODEX_HOME to an absolute path in the env file, then run the move again",
            ),
            StoreMoveRefusal::RelativeCodexHome {
                source: CodexHomeSource::Inherited,
            } => f.write_str(
                "CODEX_HOME in clauth's own environment is a relative path, and clauth cannot tell which codex login it names; nothing was moved; set CODEX_HOME to an absolute path, then run the move again",
            ),
            StoreMoveRefusal::RelativeCodexHome {
                source: CodexHomeSource::DaemonRecord,
            } => f.write_str(
                "CODEX_HOME in the running daemon's environment is a relative path; restart the daemon with an absolute CODEX_HOME",
            ),
            StoreMoveRefusal::DaemonEnvUnrecorded => {
                f.write_str("the running daemon recorded no environment; restart it, then look at the plan again")
            }
            StoreMoveRefusal::SilentMismatch { silent, probe } => write!(
                f,
                "the silent proof was minted at {silent}, not the gateway's probe address {probe}; probe {probe} and run the move again"
            ),
            StoreMoveRefusal::FailedBeforeCopy {
                moved,
                failed,
                cause,
            } => write!(
                f,
                "moving {failed} failed ({cause}); {moved} file(s) had moved, {failed} is still at its source only, and every file after it was left untouched; fix the cause, then run the move again",
                failed = failed.display(),
                moved = moved.len()
            ),
            StoreMoveRefusal::FailedAfterCopy {
                moved,
                failed,
                cause,
            } => write!(
                f,
                "moving {from} failed after it was copied to {to} ({cause}): it is now at both places; delete {from} once {to} reads correctly, then run the move again; {moved} file(s) had moved before it, and every file after it was left untouched",
                from = failed.from.display(),
                to = failed.to.display(),
                moved = moved.len()
            ),
            StoreMoveRefusal::PlanChanged => {
                f.write_str("the stores changed since the plan was shown; look at the plan again")
            }
        }
    }
}

impl std::error::Error for StoreMoveRefusal {}

/// The store env the gateway spawns with: every credential location shunt
/// reads, pointed under `~/.clauth/shunt/`, `CODEX_AUTH_FILE` and
/// `CLAUDE_CREDENTIALS` included, so neither fallback ever reaches another
/// owner's login.
pub(crate) fn store_env() -> Result<Vec<(&'static str, PathBuf)>> {
    let root = managed_store_root()?;
    Ok(STORES
        .iter()
        .map(|store| (store.env, under(&root, store.segments)))
        .collect())
}

/// What a store move will do, shown before the user's yes: every credential
/// it will move, and every one it will leave at its source on purpose.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StoreMovePlan {
    pub(crate) moved: Vec<MovedFile>,
    pub(crate) kept: Vec<KeptFile>,
}

/// Plan the standalone-store move over the inherited env [`inherited_env`]
/// returns (the running daemon's recorded env, else the caller's own). The
/// card shows the plan before the user's yes; [`move_standalone_stores`]
/// re-plans and refuses when the tree changed since.
pub(crate) fn plan_standalone_stores(record: &GatewayRecord) -> Result<StoreMovePlan> {
    let (inherited, source) = inherited_env()?;
    plan_standalone_stores_in(record, inherited, source)
}

/// [`plan_standalone_stores`] over the caller's inherited env, so tests inject
/// one (for `CODEX_HOME`) instead of reading the process's own or the
/// daemon's record.
///
/// Each store's source is the path the record's env file sets for its key,
/// read as shunt reads it ([`Source`]), else shunt's default under the
/// standalone's home (its shunt site's [`HomeRule`], the env file's `HOME`
/// over the inherited env, not clauth's own home). A store whose default home
/// no source names is listed in `kept` with its fix, never moved from a
/// guessed path, and the rest of the plan still proceeds. Another owner's
/// login never moves: the codex CLI's own login (`~/.codex/auth.json`,
/// `$CODEX_HOME/auth.json` from the env file or the inherited env) and
/// anything under `~/.clauth` stay at their source and are named in `kept`, as
/// does, on every platform, any file with another name (a hard link, whoever
/// holds the other name) or whose link count cannot be read; a dir store at a
/// codex home stays whole.
///
/// Refuses when a destination already holds a file, a `CODEX_HOME` is
/// relative, a store source is a relative path, the home the codex CLI's own
/// login or a store's default resolves under is relative, or a store itself is a link (one onto another owner's login is
/// kept instead) or not the directory or regular file its kind names.
fn plan_standalone_stores_in(
    record: &GatewayRecord,
    inherited: impl IntoIterator<Item = (OsString, OsString)>,
    source: CodexHomeSource,
) -> Result<StoreMovePlan> {
    let inherited: Vec<(OsString, OsString)> = inherited.into_iter().collect();
    let named = match &record.env_file {
        Some(path) => read_env_file(path)?,
        None => GatewayEnv::default(),
    };
    let codex = CodexOwnership::compute(&named, &inherited, source)?;
    let to_root = managed_store_root()?;
    let (moved, kept) = plan_stores(&named, &inherited, &codex, &to_root)?;
    refuse_collisions(&moved)?;
    Ok(StoreMovePlan { moved, kept })
}

/// Plan every store's move: `(moved, kept)` against `named` (the env file) and
/// `inherited` (the daemon's or caller's own env).
fn plan_stores(
    named: &GatewayEnv,
    inherited: &[(OsString, OsString)],
    codex: &CodexOwnership,
    to_root: &Path,
) -> Result<(Vec<MovedFile>, Vec<KeptFile>)> {
    let mut plan = Vec::new();
    let mut kept = Vec::new();
    let mut seen = HashSet::new();
    for store in &STORES {
        match store_source(store, named, inherited)? {
            StoreSource::Absent => {}
            StoreSource::NoHome => kept.push(KeptFile::NoHome { store: store.env }),
            StoreSource::Path(from) => plan_store(
                store.kind,
                &from,
                &under(to_root, store.segments),
                &mut plan,
                &mut kept,
                codex,
                &mut seen,
            )?,
        }
    }
    Ok((plan, kept))
}

/// Refuse the plan when a destination already holds a file, before anything
/// moves: the publish is a hard link that would refuse the name anyway, and
/// the check names every collision at once.
fn refuse_collisions(plan: &[MovedFile]) -> Result<()> {
    let mut collisions = Vec::new();
    for file in plan {
        match std::fs::symlink_metadata(&file.to) {
            Ok(_) => collisions.push(file.to.clone()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(e).with_context(|| format!("failed to inspect {}", file.to.display()));
            }
        }
    }
    if !collisions.is_empty() {
        return Err(StoreMoveRefusal::Collision { paths: collisions }.into());
    }
    Ok(())
}

/// Move the stores a confirmed [`StoreMovePlan`] names, file by file. The
/// `silent` proof must name the record's probe address. The move re-plans
/// against the inherited env [`inherited_env`] returns and refuses with
/// [`StoreMoveRefusal::PlanChanged`], moving nothing, when the fresh plan
/// differs from `confirmed`; a fresh-plan refusal (a collision, a relative
/// `CODEX_HOME`, a store that changed into a link or the wrong kind) refuses
/// as it would at plan time.
///
/// Each file is copied into an owner-only staging file, published as its
/// destination by a hard link that refuses an existing name, synced into its
/// dir, and only then removed at its source, so a failure at any step leaves
/// the credential at its source, its destination or both, never at neither.
/// Source dirs are left in place.
pub(crate) fn move_standalone_stores(
    record: &GatewayRecord,
    silent: GatewaySilent,
    confirmed: &StoreMovePlan,
) -> Result<StoreMove> {
    let (inherited, source) = inherited_env()?;
    move_standalone_stores_in(record, silent, confirmed, inherited, source)
}

/// [`move_standalone_stores`] over the caller's inherited env, so tests inject
/// one (for `CODEX_HOME`) instead of reading the process's own or the
/// daemon's record.
fn move_standalone_stores_in(
    record: &GatewayRecord,
    silent: GatewaySilent,
    confirmed: &StoreMovePlan,
    inherited: impl IntoIterator<Item = (OsString, OsString)>,
    source: CodexHomeSource,
) -> Result<StoreMove> {
    let inherited: Vec<(OsString, OsString)> = inherited.into_iter().collect();
    // The take-over's precondition: the proof must name the address the
    // record's config says to probe, else a proof minted at another address
    // could gate a move while a standalone is live there.
    let probe = gateway_bind_in(record, &gateway_env(record)?, inherited.iter().cloned())?.probe;
    if silent.addr() != probe {
        return Err(StoreMoveRefusal::SilentMismatch {
            silent: silent.addr(),
            probe,
        }
        .into());
    }
    let fresh = plan_standalone_stores_in(record, inherited.iter().cloned(), source)?;
    if &fresh != confirmed {
        return Err(StoreMoveRefusal::PlanChanged.into());
    }
    let mut moved = Vec::with_capacity(fresh.moved.len());
    for file in fresh.moved {
        match move_credential(&file.from, &file.to) {
            Ok(()) => moved.push(file),
            Err(MoveFailure::BeforeCopy(cause)) => {
                return Err(StoreMoveRefusal::FailedBeforeCopy {
                    moved,
                    failed: file.from,
                    cause,
                }
                .into());
            }
            Err(MoveFailure::AfterCopy(cause)) => {
                return Err(StoreMoveRefusal::FailedAfterCopy {
                    moved,
                    failed: file,
                    cause,
                }
                .into());
            }
        }
    }
    Ok(StoreMove {
        moved,
        kept: fresh.kept,
    })
}

/// Where the standalone kept `store`.
enum StoreSource {
    /// A path to plan: an explicit env-file value, or shunt's default under
    /// a resolved home.
    Path(PathBuf),
    /// shunt's default applies but no source named a usable home, so the
    /// default is working-directory-relative; the move lists the store.
    NoHome,
    /// No source names a store at all (a raw empty value, or a store that is
    /// never a default); skipped without listing.
    Absent,
}

/// Where the standalone kept `store`: the env file's value for its key read
/// under the store's [`Source`] rule, else shunt's default under the
/// standalone's home (its shunt site's rule). A relative path refuses, since
/// it resolved against a working directory clauth cannot know.
fn store_source(
    store: &Store,
    named: &GatewayEnv,
    inherited: &[(OsString, OsString)],
) -> Result<StoreSource> {
    let value = env_value_folded(&named.vars, store.env, var_os_key);
    let source = match store.source {
        Source::PinnedOnly => StoreSource::Absent,
        Source::BlankIsUnset => match value {
            Some(value) if !value.to_string_lossy().trim().is_empty() => {
                StoreSource::Path(PathBuf::from(value))
            }
            _ => match default_store_path(store, named, inherited)? {
                Some(path) => StoreSource::Path(path),
                None => StoreSource::NoHome,
            },
        },
        Source::EmptyIsUnset => match value {
            Some(value) if !value.is_empty() => StoreSource::Path(PathBuf::from(value)),
            _ => match default_store_path(store, named, inherited)? {
                Some(path) => StoreSource::Path(path),
                None => StoreSource::NoHome,
            },
        },
        Source::Raw => match value {
            None => match default_store_path(store, named, inherited)? {
                Some(path) => StoreSource::Path(path),
                None => StoreSource::NoHome,
            },
            Some(value) if value.is_empty() => StoreSource::Absent,
            Some(value) => StoreSource::Path(PathBuf::from(value)),
        },
        Source::NamedOnly => match value.filter(|value| !value.is_empty()) {
            Some(value) => StoreSource::Path(PathBuf::from(value)),
            None => StoreSource::Absent,
        },
    };
    match source {
        StoreSource::Path(path) if !path.is_absolute() => {
            Err(StoreMoveRefusal::RelativeSource { key: store.env }.into())
        }
        source => Ok(source),
    }
}

/// shunt's default for `store`: the family's home (its shunt site's
/// [`HomeRule`]) joined with `.shunt` and the store's segments. Refuses with
/// [`StoreMoveRefusal::RelativeHome`] when the home is relative, and returns
/// `None` when no source named a usable home (the move lists the store; it
/// never guesses a home).
fn default_store_path(
    store: &Store,
    named: &GatewayEnv,
    inherited: &[(OsString, OsString)],
) -> Result<Option<PathBuf>> {
    let Some(home) = resolve_home(store, named, inherited) else {
        return Ok(None);
    };
    if !home.is_absolute() {
        return Err(StoreMoveRefusal::RelativeHome { store: store.env }.into());
    }
    Ok(Some(under(&home.join(".shunt"), store.segments)))
}

/// The standalone's home for `store`'s default, read the way the store's
/// shunt site reads it: `HOME` non-empty, else `USERPROFILE` non-empty for
/// every store but xai (which has no `USERPROFILE` fallback), each from the
/// env file over the inherited env. An empty value counts as unset, so it
/// falls through to `USERPROFILE` (or, for xai, to no home). `None` when no
/// source named a usable home at all; shunt then reads the default as
/// working-directory-relative, which clauth cannot resolve, so the move
/// lists the store instead of guessing a home.
fn resolve_home(
    store: &Store,
    named: &GatewayEnv,
    inherited: &[(OsString, OsString)],
) -> Option<PathBuf> {
    match store.home {
        Some(HomeRule::HomeThenUserProfile) => resolved_home_else_userprofile(named, inherited),
        Some(HomeRule::RawHome) => standalone_var(named, inherited, "HOME")
            .filter(|home| !home.is_empty())
            .map(PathBuf::from),
        None => None,
    }
}

/// `HOME` non-empty, else `USERPROFILE` non-empty, each from the env file over
/// the inherited env — the resolution the `HomeThenUserProfile` store defaults
/// and the codex CLI's `~` share.
fn resolved_home_else_userprofile(
    named: &GatewayEnv,
    inherited: &[(OsString, OsString)],
) -> Option<PathBuf> {
    let home = standalone_var(named, inherited, "HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from);
    let userprofile = || {
        standalone_var(named, inherited, "USERPROFILE")
            .filter(|home| !home.is_empty())
            .map(PathBuf::from)
    };
    home.or_else(userprofile)
}

/// The codex CLI's default home `~/.codex`, resolved the way the codex CLI
/// resolves `~` for the move: `HOME` non-empty, else `USERPROFILE` non-empty,
/// each from the env file over the inherited env ([`resolved_home_else_userprofile`]),
/// so a daemon or env file whose home differs from clauth's own still guards
/// the right login. `None` when no source names a home; clauth's own home is
/// then the only guess, as it was before the daemon's env was recorded. A
/// relative home refuses with [`StoreMoveRefusal::RelativeCodexLoginHome`]
/// rather than a guard that silently stops matching: `~/.codex` under a
/// relative home names no absolute login.
fn default_codex_home(
    named: &GatewayEnv,
    inherited: &[(OsString, OsString)],
) -> Result<Option<PathBuf>> {
    let Some(home) = resolved_home_else_userprofile(named, inherited) else {
        return Ok(None);
    };
    if !home.is_absolute() {
        return Err(StoreMoveRefusal::RelativeCodexLoginHome.into());
    }
    Ok(Some(home.join(".codex")))
}

/// The codex CLI's own home and login, computed once per move.
struct CodexOwnership {
    /// `~/.codex`, and `$CODEX_HOME` when the env file or the inherited env
    /// sets one.
    homes: Vec<PathBuf>,
    /// Each home's `auth.json` login file.
    logins: Vec<PathBuf>,
}

impl CodexOwnership {
    fn compute(
        named: &GatewayEnv,
        inherited: &[(OsString, OsString)],
        inherited_source: CodexHomeSource,
    ) -> Result<Self> {
        let mut homes = vec![home_dir()?.join(".codex")];
        let mut logins = vec![home_dir()?.join(".codex").join("auth.json")];
        // clauth's own home's `.codex` is guarded unconditionally: the codex
        // CLI's login there stays whoever else's home the env file or the
        // daemon's record names. The resolved default home (`~/.codex` under
        // the inherited/env-file HOME) is a second guard, so a home that
        // differs from clauth's own still guards the right login; the dedupe
        // keeps the two from double-listing when they name one dir.
        if let Some(default_home) = default_codex_home(named, inherited)?
            && !homes.contains(&default_home)
        {
            logins.push(default_home.join("auth.json"));
            homes.push(default_home);
        }
        // The codex CLI reads `CODEX_HOME` from its own env, which a standalone
        // could have set in either of the two places clauth can see: the
        // recorded env file, or the env the daemon/this process inherited.
        // Both are guarded, neither outranking the other.
        if let Some(value) = env_value_folded(&named.vars, "CODEX_HOME", var_os_key) {
            push_codex_home(&mut homes, &mut logins, value, CodexHomeSource::EnvFile)?;
        }
        // Every inherited entry is guarded: an env block can hold the name
        // more than once, and which one codex reads is its own lookup's call.
        for (_, value) in inherited.iter().filter(|(name, _)| {
            name.to_str()
                .is_some_and(|name| var_os_key(name) == var_os_key("CODEX_HOME"))
        }) {
            push_codex_home(&mut homes, &mut logins, value, inherited_source)?;
        }
        Ok(Self { homes, logins })
    }
}

/// Guard one `CODEX_HOME` value: only an empty one adds no home (the codex
/// CLI treats an empty `CODEX_HOME` as unset, which is `~/.codex`, guarded
/// already), and a relative one refuses before anything moves; a
/// whitespace-only value is not absolute, so it refuses as any relative value
/// does. A duplicate is harmless.
fn push_codex_home(
    homes: &mut Vec<PathBuf>,
    logins: &mut Vec<PathBuf>,
    value: &OsStr,
    source: CodexHomeSource,
) -> Result<()> {
    if value.is_empty() {
        return Ok(());
    }
    let home = PathBuf::from(value);
    if !home.is_absolute() {
        return Err(StoreMoveRefusal::RelativeCodexHome { source }.into());
    }
    if !homes.contains(&home) {
        logins.push(home.join("auth.json"));
        homes.push(home);
    }
    Ok(())
}

/// Why `source` is another owner's login, compared by canonical path so a
/// link onto one is caught, and for a file by link count on every platform,
/// so a file with another name is kept whoever holds that name, and one whose
/// count cannot be read is kept too. `is_dir` picks which codex paths apply:
/// a file is the login only at a login file, a dir store stays whole only at
/// a codex home.
fn another_owners_login(
    source: &Path,
    is_dir: bool,
    codex: &CodexOwnership,
) -> Result<Option<KeptReason>> {
    let Some(source) = canonical(source)? else {
        return Ok(None);
    };
    // The codex-login match comes before the ~/.clauth match: `~/.codex/auth.json`
    // linked onto a clauth profile is the codex login, not a clauth-owned file.
    let targets: &[PathBuf] = if is_dir { &codex.homes } else { &codex.logins };
    for target in targets {
        if canonical(target)?.is_some_and(|target| target == source) {
            return Ok(Some(KeptReason::CodexLogin));
        }
    }
    if canonical(&clauth_dir()?)?.is_some_and(|clauth| source.starts_with(clauth)) {
        return Ok(Some(KeptReason::ClauthOwned));
    }
    if !is_dir {
        match has_another_name(&source) {
            Ok(true) => return Ok(Some(KeptReason::HardLink)),
            Ok(false) => {}
            Err(_) => return Ok(Some(KeptReason::LinkCountUnreadable)),
        }
    }
    Ok(None)
}

/// `path` canonicalized; `None` when it does not exist.
fn canonical(path: &Path) -> Result<Option<PathBuf>> {
    match std::fs::canonicalize(path) {
        Ok(path) => Ok(Some(path)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("failed to resolve {}", path.display())),
    }
}

/// Whether `source` is another name's file too (a hard link, whoever holds the
/// other name): the link count on unix, the file handle's link count on
/// Windows, and only for a regular file (a directory's own link count is never
/// another owner's login). Fail-closed: an unreadable count keeps the file,
/// never moves it.
fn has_another_name(source: &Path) -> std::io::Result<bool> {
    #[cfg(test)]
    if LINK_COUNT_UNREADABLE.get() {
        return Err(std::io::Error::other("forced unreadable link count"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        let meta = std::fs::metadata(source)?;
        Ok(meta.is_file() && meta.nlink() > 1)
    }
    #[cfg(windows)]
    {
        use winapi_util::{Handle, file};
        if !std::fs::metadata(source)?.is_file() {
            return Ok(false);
        }
        Ok(file::information(&Handle::from_path_any(source)?)?.number_of_links() > 1)
    }
}

#[cfg(test)]
thread_local! {
    static LINK_COUNT_UNREADABLE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Test seam forcing [`has_another_name`] to read as unreadable, so the
/// fail-closed keep is pinned; cleared on drop.
#[cfg(test)]
pub(crate) struct UnreadableLinkCount(());

#[cfg(test)]
impl UnreadableLinkCount {
    pub(crate) fn set() -> Self {
        LINK_COUNT_UNREADABLE.set(true);
        Self(())
    }
}

#[cfg(test)]
impl Drop for UnreadableLinkCount {
    fn drop(&mut self) {
        LINK_COUNT_UNREADABLE.set(false);
    }
}

fn plan_store(
    kind: StoreKind,
    from: &Path,
    to: &Path,
    plan: &mut Vec<MovedFile>,
    kept: &mut Vec<KeptFile>,
    codex: &CodexOwnership,
    seen: &mut HashSet<PathBuf>,
) -> Result<()> {
    // The owner-login check runs before the file type: a symlink onto the
    // codex login canonicalizes to the login, so it is kept by path, not
    // refused as a non-regular store.
    if let Some(reason) = another_owners_login(from, kind == StoreKind::Dir, codex)? {
        kept.push(KeptFile::At {
            path: from.to_path_buf(),
            reason,
        });
        return Ok(());
    }
    let meta = match std::fs::symlink_metadata(from) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e).with_context(|| format!("failed to inspect {}", from.display())),
    };
    if meta.file_type().is_symlink() {
        return Err(StoreMoveRefusal::StoreLink {
            path: from.to_path_buf(),
        }
        .into());
    }
    match kind {
        StoreKind::Dir if meta.is_dir() => plan_dir(from, to, plan, kept, codex, seen),
        StoreKind::File if meta.is_file() => {
            push_planned(from.to_path_buf(), to.to_path_buf(), plan, kept, seen)?;
            Ok(())
        }
        StoreKind::Dir => Err(StoreMoveRefusal::StoreNotDir {
            path: from.to_path_buf(),
        }
        .into()),
        StoreKind::File => Err(StoreMoveRefusal::StoreNotFile {
            path: from.to_path_buf(),
        }
        .into()),
    }
}

/// shunt serves only top-level regular `<[a-z0-9-]+>.json` account files
/// (`account_files`, shunt `src/auth/shared.rs`): a subdir, link or any other
/// file stays behind in the old dir and is listed as left behind.
fn plan_dir(
    from: &Path,
    to: &Path,
    plan: &mut Vec<MovedFile>,
    kept: &mut Vec<KeptFile>,
    codex: &CodexOwnership,
    seen: &mut HashSet<PathBuf>,
) -> Result<()> {
    let mut entries = std::fs::read_dir(from)
        .and_then(|entries| entries.collect::<std::io::Result<Vec<_>>>())
        .with_context(|| format!("failed to list {}", from.display()))?;
    entries.sort_by_key(std::fs::DirEntry::file_name);
    for entry in entries {
        let source = entry.path();
        let file_type = entry
            .file_type()
            .with_context(|| format!("failed to inspect {}", source.display()))?;
        if file_type.is_file() && is_account_file(&source) {
            if let Some(reason) = another_owners_login(&source, false, codex)? {
                kept.push(KeptFile::At {
                    path: source,
                    reason,
                });
            } else {
                push_planned(source, to.join(entry.file_name()), plan, kept, seen)?;
            }
        } else {
            kept.push(KeptFile::At {
                path: source,
                reason: KeptReason::LeftBehind,
            });
        }
    }
    Ok(())
}

/// Plan one credential's move, unless an earlier planned entry already names
/// the same source file (two store keys naming one path): the later one is
/// listed, never copied twice.
fn push_planned(
    from: PathBuf,
    to: PathBuf,
    plan: &mut Vec<MovedFile>,
    kept: &mut Vec<KeptFile>,
    seen: &mut HashSet<PathBuf>,
) -> Result<()> {
    if let Some(canonical) = canonical(&from)?
        && !seen.insert(canonical)
    {
        kept.push(KeptFile::At {
            path: from,
            reason: KeptReason::DuplicateSource,
        });
        return Ok(());
    }
    plan.push(MovedFile { from, to });
    Ok(())
}

/// shunt's account-file shape: a regular `*.json` whose stem is `[a-z0-9-]+`
/// (`validate_account_name`).
fn is_account_file(path: &Path) -> bool {
    path.extension().and_then(OsStr::to_str) == Some("json")
        && path
            .file_stem()
            .and_then(OsStr::to_str)
            .is_some_and(is_account_name)
}

fn is_account_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

/// Where one credential's move stopped.
#[derive(Debug)]
enum MoveFailure {
    /// Before it reached its destination: it is at its source only.
    BeforeCopy(std::io::Error),
    /// Once it was published at its destination: it is at both places.
    AfterCopy(std::io::Error),
}

fn move_credential(from: &Path, to: &Path) -> Result<(), MoveFailure> {
    stage_copy(from, to)
        .map_err(MoveFailure::BeforeCopy)?
        .link_as(to)?;
    // The link lives in the destination dir's entries: synced before the
    // source goes, so a crash in between leaves the credential at both
    // places, never at neither.
    #[cfg(unix)]
    if let Some(parent) = to.parent() {
        File::open(parent)
            .and_then(|dir| dir.sync_all())
            .map_err(MoveFailure::AfterCopy)?;
    }
    std::fs::remove_file(from).map_err(MoveFailure::AfterCopy)
}

/// `from` copied into an owner-only staging file beside `to`, synced.
fn stage_copy(from: &Path, to: &Path) -> std::io::Result<Staged> {
    if let Some(parent) = to.parent() {
        mkdir_700(parent)?;
    }
    let mut source = File::open(from)?;
    let (staged, mut file) = Staged::create(to)?;
    std::io::copy(&mut source, &mut file)?;
    file.sync_all()?;
    Ok(staged)
}

// ── chatgpt_oauth providers without a pool login ───────────────────────────

/// The pool body read cap. `GET /admin/api/pool` returns
/// `{"providers":[{"provider": ..., "auth": ..., "accounts": [...]}]}`, of
/// which clauth reads only the name, the auth string and the account count; a
/// legitimate body is a few KB even with a large pool, so 1 MiB still bounds
/// a hostile or misconfigured peer.
const POOL_BODY_LIMIT: u64 = 1024 * 1024;

/// The pool route's response bound, above the 8 s the handler budgets for one
/// request's account backfill (shunt `plan::BackfillBudgets::default().total`,
/// `src/admin/plan.rs:168`). The handler's ceiling is `total + (n - 1) *
/// min_slice` (2 s per extra oauth provider) plus its unbudgeted store scans,
/// so 20 s covers up to six oauth providers; past that a slow backfill reads
/// as [`ChatgptOauthProvidersError::Unreachable`], and a wedged gateway is
/// still bounded.
const POOL_RESPONSE_SECS: u64 = 20;

/// Why the Services card cannot name the `chatgpt_oauth` providers that need
/// their own pool login.
#[derive(Debug)]
pub(crate) enum ChatgptOauthProvidersError {
    /// clauth's gateway admin token file does not exist; the card's read
    /// never mints one.
    NoAdminToken,
    /// The gateway did not answer: a refused connection, a timeout, or any
    /// other transport failure.
    Unreachable { addr: SocketAddr },
    /// The gateway did not accept clauth's admin token (shunt also answers 401
    /// when a reload disabled its admin surface).
    Unauthorized { status: u16 },
    /// The gateway has no `[server.admin]` surface, or no pool route.
    NotFound { status: u16 },
    /// The gateway answered 500: shunt fails the whole pool list when one
    /// provider's pool cannot be resolved (a scoped account that is gone),
    /// and its log names that provider.
    PoolUnreadable,
    /// The gateway answered with a status other than 200, 401, 403, 404 or
    /// 500.
    UnexpectedStatus { status: u16 },
    /// The pool body is not JSON of the pool's shape.
    Unparseable,
    /// The pool body exceeds [`POOL_BODY_LIMIT`].
    TooLarge { limit: u64 },
    /// A 200 whose body stopped before its end (the gateway died or stalled
    /// mid-response).
    BodyBroken,
    /// A daemon holds the singleton but no env record names it, so the
    /// gateway's address cannot be resolved the way the daemon spawned it.
    DaemonEnvUnrecorded,
}

impl std::fmt::Display for ChatgptOauthProvidersError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ChatgptOauthProvidersError::NoAdminToken => {
                write!(f, "no gateway admin token")
            }
            ChatgptOauthProvidersError::Unreachable { addr } => {
                write!(f, "the gateway at {addr} did not answer")
            }
            ChatgptOauthProvidersError::Unauthorized { status } => {
                write!(
                    f,
                    "the gateway did not accept clauth's admin token (status {status})"
                )
            }
            ChatgptOauthProvidersError::NotFound { status } => {
                write!(f, "the gateway has no admin pool route (status {status})")
            }
            ChatgptOauthProvidersError::PoolUnreadable => f.write_str(
                "the gateway could not read its pool (status 500); its log names the provider",
            ),
            ChatgptOauthProvidersError::UnexpectedStatus { status } => {
                write!(f, "the gateway's pool answered status {status}")
            }
            ChatgptOauthProvidersError::Unparseable => {
                write!(f, "the gateway's pool response does not parse")
            }
            ChatgptOauthProvidersError::TooLarge { limit } => {
                write!(f, "the gateway's pool response exceeds {limit} bytes")
            }
            ChatgptOauthProvidersError::BodyBroken => {
                write!(f, "the gateway's pool response broke off")
            }
            ChatgptOauthProvidersError::DaemonEnvUnrecorded => {
                write!(f, "the running daemon recorded no environment; restart it")
            }
        }
    }
}

impl std::error::Error for ChatgptOauthProvidersError {}

/// The `chatgpt_oauth` providers the running gateway serves while no pool
/// account logs in for them, in the gateway's own `/admin/api/pool` order.
/// Empty when the pinned `CODEX_AUTH_FILE` exists as a regular file (shunt
/// serves those providers from it then), or when the gateway reports none. A
/// missing admin token, an unrecorded daemon, an unanswerable gateway, a
/// refused token, a missing route, a non-200 status, a body that does not
/// parse, breaks off or exceeds [`POOL_BODY_LIMIT`] is
/// [`ChatgptOauthProvidersError`], never an empty list.
pub(crate) fn chatgpt_oauth_providers_needing_login(record: &GatewayRecord) -> Result<Vec<String>> {
    if managed_codex_auth_file()?.is_file() {
        return Ok(Vec::new());
    }
    let addr = gateway_probe(record).map_err(|e| match e.downcast_ref::<StoreMoveRefusal>() {
        Some(StoreMoveRefusal::DaemonEnvUnrecorded) => {
            ChatgptOauthProvidersError::DaemonEnvUnrecorded.into()
        }
        _ => e,
    })?;
    let token = read_admin_token()?;
    let body = pool_body(addr, &token)?;
    let parsed: PoolBody =
        serde_json::from_slice(&body).map_err(|_| ChatgptOauthProvidersError::Unparseable)?;
    Ok(parsed
        .providers
        .into_iter()
        .filter(|provider| provider.auth == "chatgpt_oauth" && provider.accounts.is_empty())
        .map(|provider| provider.provider)
        .collect())
}

/// The pinned `CODEX_AUTH_FILE` path under `~/.clauth/shunt`, from
/// [`store_env`] so it cannot drift from the gateway's spawn env.
fn managed_codex_auth_file() -> Result<PathBuf> {
    let env = store_env()?;
    env.into_iter()
        .find(|(key, _)| *key == CODEX_AUTH_FILE_ENV)
        .map(|(_, path)| path)
        .ok_or_else(|| anyhow!("the store env holds no {CODEX_AUTH_FILE_ENV}"))
}

/// The gateway's admin token for a read, trimmed the way [`ensure_token_file`]
/// reads it. Never mints one: the Services card's pool read must not write,
/// so a missing token file is [`ChatgptOauthProvidersError::NoAdminToken`].
fn read_admin_token() -> Result<AdminToken> {
    let path = admin_token_path()?;
    match std::fs::read_to_string(&path) {
        Ok(text) => Ok(AdminToken(text.trim().to_string())),
        Err(cause) if cause.kind() == std::io::ErrorKind::NotFound => {
            Err(ChatgptOauthProvidersError::NoAdminToken.into())
        }
        Err(cause) => Err(cause).with_context(|| format!("failed to read {}", path.display())),
    }
}

/// `GET http://<addr>/admin/api/pool` with the token in `x-api-key`, under
/// [`pool_agent`]'s bounds: the body for status 200, or a
/// [`ChatgptOauthProvidersError`] for every other outcome. The error never
/// carries the response body, a header value or the token.
fn pool_body(addr: SocketAddr, token: &AdminToken) -> Result<Vec<u8>> {
    let agent = pool_agent();
    let url = format!("http://{addr}/admin/api/pool");
    let mut response = match agent.get(&url).header("x-api-key", token.expose()).call() {
        Ok(response) => response,
        Err(_) => return Err(ChatgptOauthProvidersError::Unreachable { addr }.into()),
    };
    let status = response.status().as_u16();
    match status {
        200 => {}
        401 | 403 => return Err(ChatgptOauthProvidersError::Unauthorized { status }.into()),
        404 => return Err(ChatgptOauthProvidersError::NotFound { status }.into()),
        500 => return Err(ChatgptOauthProvidersError::PoolUnreadable.into()),
        other => return Err(ChatgptOauthProvidersError::UnexpectedStatus { status: other }.into()),
    }
    match response
        .body_mut()
        .with_config()
        .limit(POOL_BODY_LIMIT)
        .read_to_vec()
    {
        Ok(body) => Ok(body),
        Err(ureq::Error::BodyExceedsLimit(_)) => Err(ChatgptOauthProvidersError::TooLarge {
            limit: POOL_BODY_LIMIT,
        }
        .into()),
        Err(_) => Err(ChatgptOauthProvidersError::BodyBroken.into()),
    }
}

/// The `/admin/api/pool` client: [`direct_config`] with a response bound
/// above the handler's own budget ([`POOL_RESPONSE_SECS`]).
fn pool_agent() -> ureq::Agent {
    direct_config(
        ureq::Agent::config_builder(),
        Duration::from_secs(POOL_RESPONSE_SECS),
    )
    .build()
    .into()
}

/// `GET /admin/api/pool`'s body, read only for the provider name, the auth
/// mode and the account count; every other field is ignored.
#[derive(Deserialize)]
struct PoolBody {
    providers: Vec<PoolProvider>,
}

#[derive(Deserialize)]
struct PoolProvider {
    provider: String,
    /// A string, never a mirror of shunt's `AuthMode`: an unknown future mode
    /// still parses.
    auth: String,
    /// Count only; the account objects are never read.
    accounts: Vec<serde::de::IgnoredAny>,
}

#[cfg(test)]
#[path = "../tests/inline/gateway.rs"]
mod tests;
