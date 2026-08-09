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
//! /etc/lego/certificates/boson.cygnusx-1.org.crt
//! /etc/lego/certificates/boson.cygnusx-1.org.issuer.crt
//! /etc/lego/certificates/boson.cygnusx-1.org.key
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

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use rustls::ServerConfig;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
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

/// Peer of `status.json` / `auth_token.json` in `~/.clauth`.
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
/// `--listen` daemon first starts and not when `--print-token` runs.
///
/// Runs under the cross-process state flock for the same reason
/// [`token::load_or_create`](super::token::load_or_create) does: two instances
/// starting together must not both decide they are the one creating it.
///
/// A malformed file is a hard error, NOT a silent fall back to the default.
/// That is the opposite of how `auth_token.json` treats a bad file, and
/// deliberately so: regenerating a token is recoverable, whereas quietly
/// ignoring an edited `cert_dir` would serve certificates from a directory the
/// operator believes they moved away from, and the only symptom would be a
/// confusing "no such file" naming a path they never configured.
pub(crate) fn cert_dir() -> Result<PathBuf> {
    let path = tls_config_path()?;
    crate::lock::with_state_lock(|| {
        let Ok(body) = std::fs::read_to_string(&path) else {
            let dir = default_cert_dir()?;
            write_tls_config(&path, &dir)?;
            return Ok(dir);
        };
        let parsed: TlsConfigFile = serde_json::from_str(&body)
            .with_context(|| format!("failed to parse {}", path.display()))?;
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

/// The three files for `fqdn`, in lego's naming.
pub(crate) struct LegoPaths {
    pub(crate) cert: PathBuf,
    pub(crate) issuer: PathBuf,
    pub(crate) key: PathBuf,
}

pub(crate) fn lego_paths_in(dir: &Path, fqdn: &str) -> LegoPaths {
    LegoPaths {
        cert: dir.join(format!("{fqdn}.crt")),
        issuer: dir.join(format!("{fqdn}.issuer.crt")),
        key: dir.join(format!("{fqdn}.key")),
    }
}

/// This host's fully-qualified name, from [`FQDN_COMMAND`].
///
/// Shelling out rather than resolving in-process, on every platform: the FQDN
/// is a resolver-and-configuration question (`/etc/hosts`, the search domain,
/// the canonical name from DNS), the platform command is the answer everything
/// else on the box already agrees with, and the in-process equivalent would
/// need `getaddrinfo` through `unsafe`, which the crate denies.
fn fqdn() -> Result<String> {
    let (program, args) = FQDN_COMMAND;
    let out = std::process::Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::null())
        .output()
        .with_context(|| {
            format!("could not run `{program}` to determine this host's FQDN")
        })?;
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
pub(crate) fn load_chain(cert: &Path, issuer: &Path) -> Result<Vec<CertificateDer<'static>>> {
    let mut chain: Vec<CertificateDer<'static>> = CertificateDer::pem_file_iter(cert)
        .with_context(|| format!("failed to read the TLS certificate {}", cert.display()))?
        .collect::<std::result::Result<_, _>>()
        .with_context(|| format!("failed to parse the TLS certificate {}", cert.display()))?;
    if chain.is_empty() {
        bail!("{} contains no certificate", cert.display());
    }

    if issuer.exists() {
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

/// Build the server's TLS configuration from an explicit set of paths.
pub(crate) fn server_config_from(paths: &LegoPaths) -> Result<Arc<ServerConfig>> {
    let chain = load_chain(&paths.cert, &paths.issuer)?;
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
    Ok(Arc::new(config))
}

/// The production entry point: resolve this host's FQDN and the configured
/// certificate directory, then load the lego certificate they name.
pub(crate) fn server_config() -> Result<Arc<ServerConfig>> {
    let fqdn = fqdn()?;
    let paths = lego_paths_in(&cert_dir()?, &fqdn);
    server_config_from(&paths)
}

#[cfg(test)]
#[path = "../../../tests/inline/daemon_api_tls.rs"]
mod tests;
