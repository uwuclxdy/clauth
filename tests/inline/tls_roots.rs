#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;
use crate::testutil::{EnvPin, HomeSandbox};
use ureq::tls::RootCerts;

const WARNING: &str = " is unusable (unreadable, not a regular file, too large, or carrying no certificate rustls accepts); falling back to the platform verifier, except on Linux where no trust roots are left until it is fixed or unset";

fn warning_for(path: &std::path::Path) -> String {
    format!("clauth: SSL_CERT_FILE {path:?}{WARNING}")
}

/// The unusable arm's decision: no trust roots on Linux — the platform
/// verifier would re-read the file unguarded there — and the OS store
/// everywhere else.
fn assert_no_trust_roots(roots: RootCerts) {
    #[cfg(target_os = "linux")]
    match roots {
        RootCerts::Specific(certs) => {
            assert_eq!(certs.len(), 0, "Linux fails closed with no roots")
        }
        other => panic!("Linux must resolve to an empty root set, got {other:?}"),
    }
    #[cfg(not(target_os = "linux"))]
    assert!(
        matches!(roots, RootCerts::PlatformVerifier),
        "the OS store answers outside Linux"
    );
}

#[test]
fn ssl_cert_file_set_and_parseable_replaces_the_store() {
    let home = HomeSandbox::new();
    let dir = tempfile::tempdir().expect("tempdir");
    let file = dir.path().join("bundle.pem");
    std::fs::write(&file, include_str!("../fixtures/tls-root-bundle/ca.crt")).expect("write pem");
    let _env = EnvPin::new(&home, &[("SSL_CERT_FILE", Some(file.as_os_str()))]);

    let certs = match root_certs() {
        RootCerts::Specific(certs) => certs,
        other => panic!("a parseable SSL_CERT_FILE must replace the store, got {other:?}"),
    };
    assert_eq!(certs.len(), 1, "the one certificate the file carries");
}

#[test]
fn ssl_cert_file_unset_uses_the_platform_verifier() {
    let home = HomeSandbox::new();
    let _env = EnvPin::new(&home, &[("SSL_CERT_FILE", None)]);

    assert!(
        matches!(root_certs(), RootCerts::PlatformVerifier),
        "an unset SSL_CERT_FILE must resolve to the platform verifier"
    );
}

#[test]
fn ssl_cert_file_unreadable_falls_back_and_logs_the_path() {
    let home = HomeSandbox::new();
    let dir = tempfile::tempdir().expect("tempdir");
    let missing = dir.path().join("missing-bundle.pem");
    reset_unusable_warning_for_test();
    let _env = EnvPin::new(&home, &[("SSL_CERT_FILE", Some(missing.as_os_str()))]);

    // The capture is per-thread, so the warning must be asserted from the
    // thread whose call emitted it.
    let capture = crate::logline::LogLines::new();
    let _capture = capture.capture_here();
    assert_no_trust_roots(root_certs());
    assert_eq!(
        capture.snapshot(),
        vec![warning_for(&missing)],
        "exactly one warning, naming the path"
    );
}

#[test]
fn ssl_cert_file_with_no_certificate_falls_back_and_logs_once() {
    let home = HomeSandbox::new();
    let dir = tempfile::tempdir().expect("tempdir");
    let empty = dir.path().join("empty-bundle.pem");
    std::fs::write(&empty, "").expect("write empty");
    reset_unusable_warning_for_test();
    let _env = EnvPin::new(&home, &[("SSL_CERT_FILE", Some(empty.as_os_str()))]);

    let capture = crate::logline::LogLines::new();
    let _capture = capture.capture_here();
    assert_no_trust_roots(root_certs());
    assert_eq!(
        capture.snapshot(),
        vec![warning_for(&empty)],
        "exactly one warning, naming the path"
    );

    // The once-per-process guard: a second build neither re-logs nor changes
    // the decision.
    assert_no_trust_roots(root_certs());
    assert_eq!(
        capture.snapshot(),
        vec![warning_for(&empty)],
        "the warning does not repeat per agent build"
    );
}

#[test]
fn ssl_cert_file_with_only_unparsable_certificates_falls_back() {
    let home = HomeSandbox::new();
    let dir = tempfile::tempdir().expect("tempdir");
    // A syntactically valid certificate block whose base64 body is not a
    // certificate: parse_pem accepts it, rustls rejects it as a root.
    let broken = dir.path().join("broken-bundle.pem");
    std::fs::write(
        &broken,
        concat!(
            "-----BEGIN CERTIFICATE-----\n",
            "Y2E=\n",
            "-----END CERTIFICATE-----\n",
        ),
    )
    .expect("write broken pem");
    reset_unusable_warning_for_test();
    let _env = EnvPin::new(&home, &[("SSL_CERT_FILE", Some(broken.as_os_str()))]);

    let capture = crate::logline::LogLines::new();
    let _capture = capture.capture_here();
    assert_no_trust_roots(root_certs());
    assert_eq!(
        capture.snapshot(),
        vec![warning_for(&broken)],
        "exactly one warning, naming the path"
    );
}

/// A file over the bundle cap is unusable and never reaches the parser.
#[test]
fn ssl_cert_file_over_the_cap_falls_back() {
    let home = HomeSandbox::new();
    let dir = tempfile::tempdir().expect("tempdir");
    let huge = dir.path().join("huge-bundle.pem");
    let file = std::fs::File::create(&huge).expect("create huge");
    file.set_len(MAX_BUNDLE_BYTES + 1).expect("sparse len");
    reset_unusable_warning_for_test();
    let _env = EnvPin::new(&home, &[("SSL_CERT_FILE", Some(huge.as_os_str()))]);

    let capture = crate::logline::LogLines::new();
    let _capture = capture.capture_here();
    assert_no_trust_roots(root_certs());
    assert_eq!(
        capture.snapshot(),
        vec![warning_for(&huge)],
        "an oversized bundle warns once, naming the path"
    );
}
