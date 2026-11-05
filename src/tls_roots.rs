//! One shared TLS root-store decision for every clauth HTTPS agent.
//!
//! clauth's agents trust the OS trust store — ureq's `platform-verifier`
//! feature (rustls-platform-verifier) — instead of the bundled WebPKI roots,
//! so a TLS-inspecting proxy the OS trusts reads clauth's traffic the way it
//! reads curl's. `SSL_CERT_FILE`, when set, REPLACES the store (curl/OpenSSL
//! semantics): exactly the certificates in it that rustls can parse are the
//! roots. `SSL_CERT_DIR` is not read while the file is set.
//!
//! A file that cannot be read, is not a regular file, exceeds
//! [`MAX_BUNDLE_BYTES`], or carries no certificate rustls accepts logs one
//! warning per process, naming the path, and leaves the decision to the
//! platform verifier — except on Linux, where that verifier would re-read the
//! same file unguarded (and `SSL_CERT_DIR` with it), so the decision is no
//! trust roots at all: curl's own outcome, failing closed until the file is
//! fixed or unset.
//!
//! `root_certs()` reads the env on every call, so tests drive it through the
//! env. The process-wide `LazyLock` agents (login/refresh, usage, codex usage)
//! keep the decision from their first build — a changed file's contents need
//! the process to start again there — while the per-call sites (pricing,
//! status, proxy check, update, gateway) read it on their next build.

use crate::logline::logline;
use ureq::tls::{Certificate, PemItem, RootCerts};

/// Largest `SSL_CERT_FILE` bundle read; real bundles are a few hundred KiB.
const MAX_BUNDLE_BYTES: u64 = 8 * 1024 * 1024;

static UNUSABLE_WARNED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

#[cfg(test)]
#[path = "../tests/inline/tls_roots.rs"]
mod tests;

/// The root-store decision every clauth HTTPS agent build site takes.
pub(crate) fn root_certs() -> RootCerts {
    let Some(path) = std::env::var_os("SSL_CERT_FILE") else {
        return RootCerts::PlatformVerifier;
    };
    match ssl_cert_file_roots(&path) {
        Some(certs) => certs.into(),
        None => {
            warn_unusable_once(&path);
            // On Linux the platform verifier resolves its store through
            // rustls-native-certs, which re-reads SSL_CERT_FILE unguarded and
            // unbounded, and picks SSL_CERT_DIR up too: an empty root set keeps
            // the failure closed and deterministic instead.
            #[cfg(target_os = "linux")]
            {
                RootCerts::Specific(Vec::<Certificate<'static>>::new().into())
            }
            #[cfg(not(target_os = "linux"))]
            {
                RootCerts::PlatformVerifier
            }
        }
    }
}

fn warn_unusable_once(path: &std::ffi::OsStr) {
    if UNUSABLE_WARNED.swap(true, std::sync::atomic::Ordering::Relaxed) {
        return;
    }
    logline!(
        "clauth: SSL_CERT_FILE {path:?} is unusable (unreadable, not a regular file, too large, or carrying no certificate rustls accepts); falling back to the platform verifier, except on Linux where no trust roots are left until it is fixed or unset"
    );
}

#[cfg(test)]
pub(crate) fn reset_unusable_warning_for_test() {
    UNUSABLE_WARNED.store(false, std::sync::atomic::Ordering::Relaxed);
}

/// The certificates `SSL_CERT_FILE` names that rustls accepts as roots, or
/// `None` when the file cannot be read, is not a regular file, exceeds
/// [`MAX_BUNDLE_BYTES`], or carries none. PEM blocks that are not certificates
/// are skipped, matching OpenSSL's scan-the-file behavior.
fn ssl_cert_file_roots(path: &std::ffi::OsStr) -> Option<Vec<Certificate<'static>>> {
    use std::io::Read as _;

    // A directory or FIFO never reaches the read; the opened handle's own
    // metadata closes the swap window between the two calls.
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() || meta.len() > MAX_BUNDLE_BYTES {
        return None;
    }
    let file = std::fs::File::open(path).ok()?;
    let meta = file.metadata().ok()?;
    if !meta.is_file() || meta.len() > MAX_BUNDLE_BYTES {
        return None;
    }
    let mut bytes = Vec::new();
    file.take(MAX_BUNDLE_BYTES + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() as u64 > MAX_BUNDLE_BYTES {
        return None;
    }

    // rustls is the arm that consumes these, so it decides which are roots:
    // ureq's `Specific` wiring keeps only certificates its own root store adds.
    let mut store = rustls::RootCertStore::empty();
    let certs: Vec<Certificate<'static>> = ureq::tls::parse_pem(&bytes)
        .filter_map(|item| {
            let Ok(PemItem::Certificate(cert)) = item else {
                return None;
            };
            store.add(cert.der().into()).is_ok().then_some(cert)
        })
        .collect();
    (!certs.is_empty()).then_some(certs)
}
