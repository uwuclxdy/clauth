//! The REST API's TLS identity, taken from this host's lego certificate.
//!
//! lego writes a renewed certificate in place, so reading it at startup means a
//! renewal reaches the listener on the next daemon restart — deliberate: the
//! alternative is re-reading `/etc` on every connection, and a half-written
//! renewal would then take the listener down rather than one restart.
//!
//! Paths are derived from the host's own FQDN, matching lego's layout:
//!
//! ```text
//! /etc/lego/certificates/boson.example.org.crt
//! /etc/lego/certificates/boson.example.org.issuer.crt
//! /etc/lego/certificates/boson.example.org.key
//! ```
//!
//! Only two things here are platform-specific — which directory holds that
//! layout ([`default_cert_dir`]) and how the host's own FQDN is discovered
//! ([`FQDN_COMMAND`]). Everything below them is the same code on macOS, Linux,
//! and Windows.
//!
//! The directory is a default, not a rule: `~/.clauth/tls.json` carries it, is
//! written with this platform's default on first use, and is read back on every
//! start ([`cert_dir`]). That is what lets a Windows box whose lego lives
//! somewhere other than `%AppData%` — or a Linux one behind a packaging
//! convention of its own — serve TLS without a rebuild.
//!
//! The derivation itself can be wrong, though, and no directory setting fixes
//! that: on a tailnet node `hostname -f` answers a name no certificate covers,
//! and `tailscale cert` writes a `<name>.crt`/`<name>.key` pair with no issuer
//! file beside it and none of lego's naming. So the whole derivation is
//! skippable — `--cert`/`--key` name the two files outright ([`CertSource`]),
//! and nothing about the FQDN, the directory, or the issuer file is consulted.
//! A bind on a Tailscale range (100.64.0.0/10, fd7a:115c:a1e0::/48) whose lego
//! derivation still fails for want of a certificate refuses with the
//! `tailscale cert` route and the `--cert`/`--key` flags to pass, instead of
//! lego's missing-file error.

use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use rustls::ServerConfig;
use rustls::client::verify_server_name;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use rustls::server::ParsedCertificate;
use serde::{Deserialize, Serialize};

use crate::logline::logline;

/// Where lego keeps its certificates on a Unix box (macOS and Linux alike):
/// the one path every unit file on such a box already agrees with. The default
/// only — `tls.json` overrides it, see [`cert_dir`].
#[cfg(unix)]
const LEGO_DIR: &str = "/etc/lego/certificates";

/// The Windows equivalent, relative to `%AppData%` — see [`default_cert_dir`].
#[cfg(not(unix))]
const LEGO_SUBDIR: &str = r"lego\certificates";

/// Windows' per-user application-data root: `%AppData%`, which is
/// `C:\Users\<you>\AppData\Roaming` on a stock install.
///
/// The environment variable comes first because it is what the operator, and
/// every installer they might run, actually sees. `dirs::config_dir()` resolves
/// the same known folder through the API as a backstop, for a process started
/// without the user's environment block.
#[cfg(not(unix))]
fn appdata_dir() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os("AppData").filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(dir));
    }
    dirs::config_dir().context("cannot determine %AppData% to locate lego's certificates")
}

/// This platform's default certificate directory, used when `tls.json` has not
/// been edited to say otherwise.
///
/// Unix has one answer every unit file on the box already agrees with. Windows
/// has no `/etc`, and lego's own default there (`.lego` under the working
/// directory) is useless for a daemon, whose working directory is whatever
/// started it — so the per-user application-data root is used instead:
/// `%AppData%\lego\certificates`. Point lego at it with `--path`.
///
/// Per-user, matching where clauth keeps everything else (`~/.clauth`) rather
/// than the machine-wide `%ProgramData%`. A daemon run as the logged-in user
/// therefore finds it; one run as a Windows *service* under `LocalSystem` would
/// resolve a different `%AppData%` and need `tls.json` pointed somewhere both
/// accounts can read.
pub(crate) fn default_cert_dir() -> Result<PathBuf> {
    #[cfg(unix)]
    {
        Ok(PathBuf::from(LEGO_DIR))
    }
    #[cfg(not(unix))]
    {
        Ok(appdata_dir()?.join(LEGO_SUBDIR))
    }
}

/// Peer of `status.json` / `devices.json` in `~/.clauth`.
const TLS_FILE: &str = "tls.json";
/// Bumped only on a breaking change to the file's shape, like `status.json`.
const TLS_SCHEMA: u64 = 1;

/// `~/.clauth/tls.json`. One key today, and a struct rather than a bare string
/// so the next TLS knob is an additive field instead of a new file.
#[derive(Debug, Serialize, Deserialize)]
struct TlsConfigFile {
    schema: u64,
    /// Directory holding lego's `<fqdn>.crt`, `<fqdn>.issuer.crt`, `<fqdn>.key`.
    cert_dir: String,
}

fn tls_config_path() -> Result<PathBuf> {
    Ok(crate::profile::clauth_dir()?.join(TLS_FILE))
}

/// The configured certificate directory, writing the platform default on first
/// use so the operator has a file to edit rather than a documented path to
/// retype. Only [`server_config`] calls this, so `tls.json` appears when a
/// `--listen` daemon first starts.
///
/// Runs under the cross-process state flock, so two instances starting
/// together cannot both decide they are the one creating it.
///
/// A malformed file is a hard error, NOT a silent fall back to the default:
/// quietly ignoring an edited `cert_dir` would serve certificates from a
/// directory the operator believes they moved away from, and the only symptom
/// would be a confusing "no such file" naming a path they never configured.
pub(crate) fn cert_dir() -> Result<PathBuf> {
    let path = tls_config_path()?;
    crate::lock::with_state_lock(|_| {
        let Ok(body) = std::fs::read_to_string(&path) else {
            let dir = default_cert_dir()?;
            write_tls_config(&path, &dir)?;
            return Ok(dir);
        };
        let parsed: TlsConfigFile = serde_json::from_str(&body).map_err(|e| {
            crate::format::parse_error(path.display(), crate::format::json_parse_position(&e))
        })?;
        if parsed.schema > TLS_SCHEMA {
            // Written by a newer clauth. The one field this build reads is a
            // path either way, so take it and let the newer field set be.
            logline!(
                "clauth daemon: {TLS_FILE} is schema {} (this build knows {TLS_SCHEMA})",
                parsed.schema
            );
        }
        if parsed.cert_dir.trim().is_empty() {
            bail!(
                "{} has an empty cert_dir; set it to the directory holding \
                 <fqdn>.crt, or delete the file to get this platform's default",
                path.display()
            );
        }
        Ok(PathBuf::from(parsed.cert_dir))
    })
}

fn write_tls_config(path: &Path, dir: &Path) -> Result<()> {
    let file = TlsConfigFile {
        schema: TLS_SCHEMA,
        cert_dir: dir.to_string_lossy().into_owned(),
    };
    crate::profile::atomic_write_600(path, serde_json::to_vec_pretty(&file)?)
        .with_context(|| format!("failed to write {}", path.display()))
}

/// The command that reports this host's own fully-qualified name, as
/// `(program, args)`.
///
/// Unix: `hostname -f`, the answer every other service on the box already
/// agrees with.
///
/// Windows has no one-shot equivalent. Its `hostname` prints only the short
/// computer name, and `whoami /fqdn` — the obvious-looking candidate — reports
/// the *user's* Active Directory distinguished name (`CN=…,DC=…`), not the
/// machine's, and fails outright for a local account. So the lookup goes
/// through the resolver the .NET stack already exposes, which is the same
/// question `hostname -f` answers: resolve this computer's name and take the
/// canonical one that comes back.
#[cfg(unix)]
const FQDN_COMMAND: (&str, &[&str]) = ("hostname", &["-f"]);

#[cfg(not(unix))]
const FQDN_COMMAND: (&str, &[&str]) = (
    "powershell.exe",
    &[
        "-NoProfile",
        "-NonInteractive",
        "-Command",
        "[System.Net.Dns]::GetHostEntry($env:COMPUTERNAME).HostName",
    ],
);

/// The files one TLS identity is loaded from.
///
/// `issuer` is an `Option` and not a path that might not exist, because the two
/// cases are different: lego writes an issuer file and a missing one means the
/// leaf carried the chain, whereas `tailscale cert` has no such file at all and
/// never will. `None` says "do not look", which is what keeps an explicit
/// `--cert` from inventing a `<cert>.issuer.crt` the operator never named.
pub(crate) struct CertPaths {
    pub(crate) cert: PathBuf,
    pub(crate) issuer: Option<PathBuf>,
    pub(crate) key: PathBuf,
}

/// The three files for `fqdn`, in lego's naming.
pub(crate) fn lego_paths_in(dir: &Path, fqdn: &str) -> CertPaths {
    CertPaths {
        cert: dir.join(format!("{fqdn}.crt")),
        issuer: Some(dir.join(format!("{fqdn}.issuer.crt"))),
        key: dir.join(format!("{fqdn}.key")),
    }
}

/// Where the listener's TLS identity comes from.
///
/// [`Lego`](CertSource::Lego) is the default and needs no flags: this host's
/// FQDN and the configured directory name the files between them.
/// [`Explicit`](CertSource::Explicit) is `--cert`/`--key`, for the hosts where
/// that derivation cannot work at all — see the module docs.
pub(crate) enum CertSource {
    Lego,
    Explicit(CertPaths),
}

impl CertSource {
    /// `--cert`/`--key` if both are present, lego otherwise. The CLI requires
    /// them together, so a half-set pair cannot reach here.
    pub(crate) fn from_flags(cert: Option<PathBuf>, key: Option<PathBuf>) -> Self {
        match (cert, key) {
            (Some(cert), Some(key)) => Self::Explicit(CertPaths {
                cert,
                issuer: None,
                key,
            }),
            _ => Self::Lego,
        }
    }
}

/// Test-only override for [`fqdn`]: `Some` is the failure message the lookup
/// returns instead of shelling out, so a test can drive the failure without
/// the real command. Set while holding a `testutil::HomeSandbox` — the same
/// `HOME_TEST_LOCK` serialization every other process-global test seam uses —
/// so a forced value never bleeds into a concurrent test that consults the
/// real lookup. Never compiled into the binary.
#[cfg(test)]
static FQDN_OVERRIDE: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// This host's fully-qualified name, from [`FQDN_COMMAND`].
///
/// Shelling out rather than resolving in-process, on every platform: the FQDN
/// is a resolver-and-configuration question (`/etc/hosts`, the search domain,
/// the canonical name from DNS), the platform command is the answer everything
/// else on the box already agrees with, and the in-process equivalent would
/// need `getaddrinfo` through `unsafe`, which the crate denies.
fn fqdn() -> Result<String> {
    #[cfg(test)]
    if let Some(failure) = FQDN_OVERRIDE.lock().ok().and_then(|g| g.clone()) {
        bail!("{failure}");
    }
    let (program, args) = FQDN_COMMAND;
    let out = std::process::Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::null())
        .output()
        .with_context(|| format!("could not run `{program}` to determine this host's FQDN"))?;
    if !out.status.success() {
        // The stderr is the whole diagnosis when this fails (an unresolvable
        // computer name, a missing shell), and it is otherwise discarded.
        let why = String::from_utf8_lossy(&out.stderr).trim().to_string();
        if why.is_empty() {
            bail!("`{program}` failed with {}", out.status);
        }
        bail!("`{program}` failed with {}: {why}", out.status);
    }
    let name = String::from_utf8_lossy(&out.stdout).trim().to_string();
    validate_fqdn(&name)?;
    Ok(name)
}

/// Reject anything that is not plausibly a hostname BEFORE it is joined into a
/// path — this value names a file in a system directory, so a separator, a
/// `..`, or a NUL in it would be a traversal. Belt and braces: the FQDN command
/// is not attacker-controlled on a sane box, but "not attacker-controlled" is
/// exactly the assumption that stops holding first.
///
/// The character set is deliberately the same on every platform, and narrower
/// than what Windows would accept in a filename: `\` is refused here as firmly
/// as `/`, so a Windows path separator cannot ride in either.
fn validate_fqdn(name: &str) -> Result<()> {
    let plausible = !name.is_empty()
        && name.len() <= 253
        && !name.starts_with(['-', '.'])
        && !name.ends_with(['-', '.'])
        && !name.contains("..")
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-');
    if !plausible {
        bail!("the host FQDN lookup returned {name:?}, which is not a usable hostname");
    }
    Ok(())
}

/// The certificate chain: the leaf file, plus any certificate from the issuer
/// file that it does not already carry.
///
/// lego usually writes the full chain into `<fqdn>.crt`, in which case the
/// issuer file is a duplicate — and a chain that repeats a certificate is
/// malformed, so the de-dup is what makes reading both safe rather than
/// optional. A missing issuer file is fine (the leaf carried the chain); an
/// unreadable one is not silently ignored.
pub(crate) fn load_chain(
    cert: &Path,
    issuer: Option<&Path>,
) -> Result<Vec<CertificateDer<'static>>> {
    let mut chain: Vec<CertificateDer<'static>> = CertificateDer::pem_file_iter(cert)
        .with_context(|| format!("failed to read the TLS certificate {}", cert.display()))?
        .collect::<std::result::Result<_, _>>()
        .with_context(|| format!("failed to parse the TLS certificate {}", cert.display()))?;
    if chain.is_empty() {
        bail!("{} contains no certificate", cert.display());
    }

    if let Some(issuer) = issuer.filter(|p| p.exists()) {
        let extra: Vec<CertificateDer<'static>> = CertificateDer::pem_file_iter(issuer)
            .with_context(|| format!("failed to read the issuer chain {}", issuer.display()))?
            .collect::<std::result::Result<_, _>>()
            .with_context(|| format!("failed to parse the issuer chain {}", issuer.display()))?;
        for cert in extra {
            if !chain.contains(&cert) {
                chain.push(cert);
            }
        }
    }
    Ok(chain)
}

pub(crate) fn load_key(path: &Path) -> Result<PrivateKeyDer<'static>> {
    // Handles PKCS#8, PKCS#1 and SEC1 without the caller branching on which
    // ACME client wrote it.
    PrivateKeyDer::from_pem_file(path)
        .with_context(|| format!("failed to read the TLS private key {}", path.display()))
}

/// Build the server's TLS configuration from an explicit set of paths: the
/// tests' listener fixture, which needs no published name.
#[cfg(all(test, unix))]
pub(crate) fn server_config_from(paths: &CertPaths) -> Result<Arc<ServerConfig>> {
    load_served(paths).map(|(config, _leaf)| config)
}

/// A loaded TLS identity: the configuration the listener serves, and the name
/// a pairing link can carry for it.
pub(crate) struct Identity {
    pub(crate) config: Arc<ServerConfig>,
    /// The name the certificate's files were found by (the FQDN for lego, the
    /// `--cert` file's stem, as `tailscale cert` names it) when the leaf covers
    /// it as a DNS name, else the leaf's own first non-wildcard DNS name;
    /// `None` when it carries none.
    pub(crate) name: Option<String>,
}

/// `candidate` when `leaf` covers it as a DNS name, else `None`: rustls' own
/// name check, so the name published is one a client's handshake accepts.
fn covered_name(leaf: &CertificateDer<'_>, candidate: &str) -> Option<String> {
    let name = ServerName::try_from(candidate).ok()?;
    let ServerName::DnsName(_) = name else {
        return None;
    };
    let parsed = ParsedCertificate::try_from(leaf).ok()?;
    verify_server_name(&parsed, &name).ok()?;
    Some(candidate.to_owned())
}

/// The first DNS name `leaf` carries that names a host: a wildcard entry names
/// a pattern, not a host, so it is skipped. `None` when every name it carries
/// is one.
fn leaf_dns_name(leaf: &CertificateDer<'_>) -> Option<String> {
    webpki::EndEntityCert::try_from(leaf)
        .ok()?
        .valid_dns_names()
        .find(|name| !name.starts_with("*."))
        .map(str::to_owned)
}

/// The name a pairing link publishes for `leaf`: `candidate` (the lego FQDN or
/// the `--cert` file's stem) when the leaf covers it, else the leaf's own first
/// non-wildcard DNS name. `None` when it carries none.
fn published_name(leaf: &CertificateDer<'_>, candidate: Option<&str>) -> Option<String> {
    candidate
        .and_then(|candidate| covered_name(leaf, candidate))
        .or_else(|| leaf_dns_name(leaf))
}

/// The server's TLS configuration from an explicit set of paths, and the leaf
/// certificate it serves.
fn load_served(paths: &CertPaths) -> Result<(Arc<ServerConfig>, CertificateDer<'static>)> {
    let chain = load_chain(&paths.cert, paths.issuer.as_deref())?;
    let leaf = chain
        .first()
        .cloned()
        .with_context(|| format!("{} contains no certificate", paths.cert.display()))?;
    let key = load_key(&paths.key)?;

    // Pin the provider rather than taking the process-wide default: ureq also
    // builds rustls in this binary, and whichever of us installs a default
    // first would otherwise decide the other's crypto backend.
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut config = ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .context("failed to select TLS protocol versions")?
        .with_no_client_auth()
        .with_single_cert(chain, key)
        .context("the TLS certificate and private key do not match")?;
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok((Arc::new(config), leaf))
}

/// Tailscale's IPv4 node-address range (`net/tsaddr`'s `CGNATRange`). It is
/// also RFC 6598 carrier-grade NAT space, so an address in it does not prove a
/// tailnet — the refusal below states the range fact, never the identity.
const TAILSCALE_IPV4_RANGE: &str = "100.64.0.0/10";

/// Tailscale's IPv6 node-address range (`net/tsaddr`'s `TailscaleULARange`).
const TAILSCALE_IPV6_RANGE: &str = "fd7a:115c:a1e0::/48";

/// Which Tailscale range `ip` sits in, if any.
///
/// Canonicalized first, so an IPv4-mapped IPv6 bind (`::ffff:100.64.1.2`)
/// counts as the IPv4 address it spells — the form `--listen` would have been
/// given as anyway.
fn tailscale_range(ip: IpAddr) -> Option<&'static str> {
    match ip.to_canonical() {
        IpAddr::V4(v4) => {
            let [a, b, _, _] = v4.octets();
            (a == 100 && (b & 0b1100_0000) == 0b0100_0000).then_some(TAILSCALE_IPV4_RANGE)
        }
        IpAddr::V6(v6) => {
            let [s0, s1, s2, ..] = v6.segments();
            (s0 == 0xfd7a && s1 == 0x115c && s2 == 0xa1e0).then_some(TAILSCALE_IPV6_RANGE)
        }
    }
}

/// The tailnet refusal for a bind on a Tailscale range whose lego identity
/// cannot be produced. States the range fact and the two things the operator
/// must run, and keeps the original failure in the chain so the path that was
/// looked for is not lost. Any other bind passes the cause through untouched.
fn refuse_on_tailnet(listen: IpAddr, cause: anyhow::Error) -> anyhow::Error {
    match tailscale_range(listen) {
        Some(range) => cause.context(format!(
            "{listen} is in the {range} range Tailscale assigns addresses from, \
             and this host's lego certificate is not available; run \
             `tailscale cert <machine>.<tailnet>.ts.net` and pass \
             `--cert <machine>.<tailnet>.ts.net.crt --key <machine>.<tailnet>.ts.net.key` \
             to `clauth daemon --listen`"
        )),
        None => cause,
    }
}

/// The lego arm's load: read the identity [`CertPaths`] names, and turn the
/// one "no certificate here" failure — the derived `.crt` is genuinely absent
/// — on a Tailscale-range bind into the tailnet refusal. Absence is decided by
/// [`Path::try_exists`], so on unix a stat failure that is not a not-found
/// (EACCES, ENOTDIR) is not mistaken for one. Windows maps more errors to
/// not-found than a missing file (a path through a regular file among them),
/// so there those read as absent. Every other failure (a `.crt` that exists
/// but does not parse, a missing key, a bad `tls.json`, an unreadable parent)
/// stays exactly as today, as does a bind outside the ranges. The leaf comes
/// back beside the configuration, for the name [`Identity`] publishes.
pub(crate) fn load_lego_or_refuse(
    listen: IpAddr,
    paths: &CertPaths,
) -> Result<(Arc<ServerConfig>, CertificateDer<'static>)> {
    load_served(paths).map_err(|cause| {
        if matches!(paths.cert.try_exists(), Ok(false)) {
            refuse_on_tailnet(listen, cause)
        } else {
            cause
        }
    })
}

/// The production entry point.
///
/// `listen` decides only the tailnet refusal below and never what is loaded.
/// For [`CertSource::Lego`] this resolves the host's FQDN and the configured
/// certificate directory and loads what they name between them; where that
/// cannot produce an identity because the derived `.crt` is absent or the FQDN
/// lookup fails, a bind on a Tailscale range refuses with the `tailscale cert`
/// route instead of lego's missing-file error. For [`CertSource::Explicit`]
/// none of that runs: no `hostname -f`, no `tls.json`, no issuer file, no
/// tailnet refusal — the two named files are read and that is all.
pub(crate) fn server_config(source: &CertSource, listen: IpAddr) -> Result<Identity> {
    match source {
        CertSource::Lego => {
            let fqdn = fqdn().map_err(|cause| refuse_on_tailnet(listen, cause))?;
            let paths = lego_paths_in(&cert_dir()?, &fqdn);
            let (config, leaf) = load_lego_or_refuse(listen, &paths)?;
            Ok(Identity {
                config,
                name: published_name(&leaf, Some(&fqdn)),
            })
        }
        CertSource::Explicit(paths) => {
            let (config, leaf) = load_served(paths)?;
            let stem = paths.cert.file_stem().and_then(|stem| stem.to_str());
            Ok(Identity {
                config,
                name: published_name(&leaf, stem),
            })
        }
    }
}

#[cfg(test)]
#[path = "../../../tests/inline/daemon_api_tls.rs"]
mod tests;
