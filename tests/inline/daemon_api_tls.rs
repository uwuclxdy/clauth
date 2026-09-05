#![allow(clippy::unwrap_used, clippy::expect_used)]

//! Certificate loading: lego's file layout, the `tls.json` that points at it,
//! chain assembly, and the guard on the hostname that names those files.
//!
//! The `tls.json` cases redirect disk state into a [`HomeSandbox`] tempdir, so
//! nothing here reads or writes the operator's real `~/.clauth`.
//!
//! PEM decoding does not parse X.509, so these fixtures are synthetic blocks
//! written into a tempdir rather than real certificates, and no private key is
//! committed to this repository. That is enough to pin everything the loader
//! itself decides: which files it reads, how it merges the issuer chain, and
//! what it does when one is missing. Whether the resulting chain and key
//! actually make a working handshake is a property of the operator's lego
//! output, checked in the end-to-end run in wiki/Daemon.md, not here.

use super::*;

use std::path::PathBuf;

use crate::profile::clauth_dir;
use crate::testutil::HomeSandbox;

/// A PEM block of `kind` carrying `payload` (already base64).
fn pem(kind: &str, payload: &str) -> String {
    format!("-----BEGIN {kind}-----\n{payload}\n-----END {kind}-----\n")
}

fn cert_pem(payload: &str) -> String {
    pem("CERTIFICATE", payload)
}

const LEAF: &str = "AQIDBA==";
const ISSUER: &str = "BQYHCA==";

#[test]
fn lego_paths_use_legos_naming() {
    let paths = lego_paths_in(Path::new("/etc/lego/certificates"), "boson.example.org");
    assert_eq!(
        paths.cert,
        Path::new("/etc/lego/certificates/boson.example.org.crt")
    );
    assert_eq!(
        paths.issuer.as_deref(),
        Some(Path::new(
            "/etc/lego/certificates/boson.example.org.issuer.crt"
        )),
        "lego writes an issuer file, so the lego source always names one"
    );
    assert_eq!(
        paths.key,
        Path::new("/etc/lego/certificates/boson.example.org.key")
    );
}

#[test]
fn a_leaf_only_cert_gains_the_issuer() {
    let dir = tempfile::tempdir().expect("tempdir");
    let paths = lego_paths_in(dir.path(), "host.example");
    std::fs::write(&paths.cert, cert_pem(LEAF)).expect("write leaf");
    std::fs::write(
        paths.issuer.as_deref().expect("lego names an issuer"),
        cert_pem(ISSUER),
    )
    .expect("write issuer");

    let chain = load_chain(&paths.cert, paths.issuer.as_deref()).expect("chain");
    assert_eq!(chain.len(), 2, "leaf then issuer");
    assert_eq!(chain[0].as_ref(), &[1, 2, 3, 4], "the leaf stays first");
}

/// lego usually writes the full chain into `<fqdn>.crt`, so reading the issuer
/// file as well would otherwise repeat a certificate, which is a malformed
/// chain.
#[test]
fn an_issuer_already_in_the_leaf_file_is_not_duplicated() {
    let dir = tempfile::tempdir().expect("tempdir");
    let paths = lego_paths_in(dir.path(), "host.example");
    std::fs::write(
        &paths.cert,
        format!("{}{}", cert_pem(LEAF), cert_pem(ISSUER)),
    )
    .expect("write chain");
    std::fs::write(
        paths.issuer.as_deref().expect("lego names an issuer"),
        cert_pem(ISSUER),
    )
    .expect("write issuer");

    let chain = load_chain(&paths.cert, paths.issuer.as_deref()).expect("chain");
    assert_eq!(chain.len(), 2, "the repeated issuer is dropped");
}

#[test]
fn a_missing_issuer_file_is_fine() {
    let dir = tempfile::tempdir().expect("tempdir");
    let paths = lego_paths_in(dir.path(), "host.example");
    std::fs::write(&paths.cert, cert_pem(LEAF)).expect("write leaf");

    let chain = load_chain(&paths.cert, paths.issuer.as_deref()).expect("chain");
    assert_eq!(chain.len(), 1);
}

// ── --cert / --key ──────────────────────────────────────────────────────────

/// Both flags present is the only thing that leaves lego behind.
///
/// The CLI already refuses a half pair, so this is the second line: a bug that
/// dropped one of them would otherwise fall back to lego silently, and the
/// operator would be told this host has no certificate for a name they never
/// asked it to serve.
#[test]
fn cert_source_is_explicit_only_when_both_files_are_named() {
    let cert = PathBuf::from("/srv/tailscale/node.crt");
    let key = PathBuf::from("/srv/tailscale/node.key");

    let CertSource::Explicit(paths) = CertSource::from_flags(Some(cert.clone()), Some(key.clone()))
    else {
        panic!("both files named must give an explicit source");
    };
    assert_eq!((paths.cert, paths.key), (cert.clone(), key.clone()));
    assert_eq!(
        paths.issuer, None,
        "`tailscale cert` writes no issuer file, and none is invented beside the leaf"
    );

    for (c, k) in [
        (Some(cert.clone()), None),
        (None, Some(key.clone())),
        (None, None),
    ] {
        assert!(
            matches!(CertSource::from_flags(c, k), CertSource::Lego),
            "anything short of both files stays on this host's lego certificate"
        );
    }
}

/// An explicit certificate does not pick up a sibling issuer file.
///
/// `None` has to mean "do not look" rather than "the file is missing": a
/// `--cert` pointed into a directory that happens to hold a lego-shaped
/// `<name>.issuer.crt` must serve exactly the chain in the file the operator
/// named, and nothing that merely sits next to it.
#[test]
fn an_explicit_cert_ignores_an_issuer_file_beside_it() {
    let dir = tempfile::tempdir().expect("tempdir");
    let cert = dir.path().join("node.crt");
    let key = dir.path().join("node.key");
    std::fs::write(&cert, cert_pem(LEAF)).expect("write leaf");
    // A file the lego path would have merged in, left here on purpose.
    std::fs::write(dir.path().join("node.issuer.crt"), cert_pem(ISSUER)).expect("write issuer");

    let CertSource::Explicit(paths) = CertSource::from_flags(Some(cert), Some(key)) else {
        panic!("explicit");
    };
    let chain = load_chain(&paths.cert, paths.issuer.as_deref()).expect("chain");
    assert_eq!(
        chain.len(),
        1,
        "only the named file is read, sibling or no sibling"
    );
}

#[test]
fn a_missing_certificate_names_the_path_it_wanted() {
    let dir = tempfile::tempdir().expect("tempdir");
    let paths = lego_paths_in(dir.path(), "host.example");

    let err = load_chain(&paths.cert, paths.issuer.as_deref())
        .expect_err("a missing certificate must not be silently empty");
    assert!(
        format!("{err:#}").contains("host.example.crt"),
        "the operator has to be told which file: {err:#}"
    );
}

#[test]
fn an_empty_certificate_file_is_an_error_not_an_empty_chain() {
    let dir = tempfile::tempdir().expect("tempdir");
    let paths = lego_paths_in(dir.path(), "host.example");
    std::fs::write(&paths.cert, "").expect("write empty");

    assert!(
        load_chain(&paths.cert, paths.issuer.as_deref()).is_err(),
        "an empty chain would fail later, at handshake time, with no context"
    );
}

#[test]
fn a_missing_key_names_the_path_it_wanted() {
    let dir = tempfile::tempdir().expect("tempdir");
    let paths = lego_paths_in(dir.path(), "host.example");

    let err = load_key(&paths.key).expect_err("a missing key must error");
    assert!(format!("{err:#}").contains("host.example.key"), "{err:#}");
}

#[test]
fn keys_load_in_every_encoding_lego_might_have_written() {
    for kind in ["PRIVATE KEY", "RSA PRIVATE KEY", "EC PRIVATE KEY"] {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = lego_paths_in(dir.path(), "host.example");
        std::fs::write(&paths.key, pem(kind, LEAF)).expect("write key");
        assert!(
            load_key(&paths.key).is_ok(),
            "{kind} should load without the caller branching on it"
        );
    }
}

/// This value becomes a filename under `/etc`, so anything that is not
/// plausibly a hostname is refused before it is joined into a path.
#[test]
fn a_hostname_that_could_escape_the_certificate_directory_is_refused() {
    for bad in [
        "",
        "../../etc/shadow",
        "host/../../root",
        "host name",
        "host\0name",
        "-leading-hyphen",
        ".leading-dot",
        "trailing-dot.",
        "double..dot",
        "host\nname",
        r"windows\path",
    ] {
        assert!(
            validate_fqdn(bad).is_err(),
            "{bad:?} must not be accepted as a hostname"
        );
    }
}

#[test]
fn a_real_fqdn_is_accepted() {
    for good in [
        "boson.example.org",
        "higgs.example.org",
        "host",
        "a-b.c-d.example",
    ] {
        assert!(validate_fqdn(good).is_ok(), "{good:?} should be accepted");
    }
}

#[test]
fn an_over_long_hostname_is_refused() {
    let long = format!("{}.example", "a".repeat(250));
    assert!(validate_fqdn(&long).is_err());
}

/// The platform default is the one thing here that cannot be checked the same
/// way on every target, so each target checks its own.
#[test]
fn the_default_certificate_directory_matches_the_platform() {
    let dir = default_cert_dir().expect("the platform default must resolve");

    #[cfg(unix)]
    assert_eq!(
        dir,
        Path::new("/etc/lego/certificates"),
        "macOS and Linux share the one path every unit file already agrees with"
    );

    #[cfg(not(unix))]
    {
        // Not pinned to a literal: %AppData% is per-user and moves with the
        // profile, so hard-coding C:\Users\… would pin the test to whoever ran
        // it. The layout under it is the contract.
        assert!(
            dir.ends_with(r"lego\certificates"),
            "windows default should sit under %AppData%: {}",
            dir.display()
        );
        assert!(dir.is_absolute(), "{} must be absolute", dir.display());
    }
}

/// `tls.json` is created on first use, so an operator has a file to edit rather
/// than a documented path to retype.
#[test]
fn tls_config_is_written_with_the_platform_default_on_first_use() {
    let _home = HomeSandbox::new();
    let path = clauth_dir().expect("clauth dir").join("tls.json");
    assert!(!path.exists(), "sandbox starts without one");

    let dir = cert_dir().expect("first read creates the file");
    assert_eq!(
        dir,
        default_cert_dir().expect("default"),
        "first use takes the default"
    );
    assert!(
        path.exists(),
        "the default should be persisted, not just returned"
    );

    let body = std::fs::read_to_string(&path).expect("read back");
    assert!(
        body.contains("cert_dir") && body.contains("schema"),
        "the file an operator opens has to show both fields: {body}"
    );
}

/// `tls.json` inherits the tree's 0600, like every other file clauth writes.
///
/// It holds no key material — it names a directory — but the pin is on the tree
/// and not on the secrecy of any one file: a mode that drifts loose here is a
/// mode nobody notices drifting loose in the file beside it.
#[cfg(unix)]
#[test]
fn tls_config_is_owner_only() {
    let _home = HomeSandbox::new();
    cert_dir().expect("first read creates the file");
    let left = crate::testutil::owner_only_violations(&clauth_dir().expect("clauth dir"));
    assert!(
        left.is_empty(),
        "tls.json must inherit the 0600 tree invariant; still loose: {left:#?}"
    );
}

#[test]
fn an_edited_cert_dir_is_honored_across_restarts() {
    let _home = HomeSandbox::new();
    let path = clauth_dir().expect("clauth dir").join("tls.json");
    cert_dir().expect("create the default");

    std::fs::write(&path, r#"{"schema":1,"cert_dir":"/opt/certs/lego"}"#).expect("edit");
    assert_eq!(
        cert_dir().expect("read edited"),
        Path::new("/opt/certs/lego"),
        "an edit must survive; re-writing the default would undo the operator"
    );
    assert_eq!(
        cert_dir().expect("read again"),
        Path::new("/opt/certs/lego"),
        "and it must still survive the next start"
    );
}

/// The deliberate divergence from `auth_token.json`, which silently replaces a
/// file it cannot parse: serving certificates out of a directory the operator
/// thinks they moved away from is worse than refusing to start.
#[test]
fn a_malformed_tls_config_refuses_rather_than_reverting_to_the_default() {
    let _home = HomeSandbox::new();
    let path = clauth_dir().expect("clauth dir").join("tls.json");
    // Nothing has created ~/.clauth yet: these cases seed the file directly
    // instead of letting `cert_dir` write it.
    crate::profile::mkdir_700(path.parent().expect("parent")).expect("mkdir");

    for bad in ["{", "", "[]", r#"{"schema":1}"#] {
        std::fs::write(&path, bad).expect("write malformed");
        let err = cert_dir().expect_err("malformed config must not be ignored");
        assert!(
            format!("{err:#}").contains("tls.json"),
            "the operator has to be told which file: {err:#}"
        );
    }

    std::fs::write(&path, r#"{"schema":1,"cert_dir":"   "}"#).expect("write blank");
    assert!(
        cert_dir().is_err(),
        "a blank cert_dir would resolve every certificate path to a bare filename"
    );
}

/// Same downgrade rule as `auth_token.json`: a newer schema is read, not
/// rejected, because the one field this build needs is a path either way.
#[test]
fn a_newer_schema_is_still_read() {
    let _home = HomeSandbox::new();
    let path = clauth_dir().expect("clauth dir").join("tls.json");
    crate::profile::mkdir_700(path.parent().expect("parent")).expect("mkdir");
    std::fs::write(
        &path,
        r#"{"schema":99,"cert_dir":"/srv/lego","future_knob":true}"#,
    )
    .expect("write newer");

    assert_eq!(
        cert_dir().expect("a newer schema must not be fatal"),
        Path::new("/srv/lego"),
        "a downgrade must not strand the operator's configured directory"
    );
}
