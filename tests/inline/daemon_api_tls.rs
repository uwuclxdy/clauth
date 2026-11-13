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
//! output, which no test here checks.

use super::*;

use std::net::IpAddr;
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
    // Sandboxed: on Windows the default resolves under the operator's real
    // %AppData%, and the env var is read before `dirs`' known-folder lookup,
    // so the pin keeps the resolution inside the fixture on that leg. This
    // test spawns no binary, so the in-process pin is enough.
    let _home = HomeSandbox::new();
    #[cfg(not(unix))]
    let _appdata =
        crate::testutil::EnvPin::new(&_home, &[("AppData", Some(_home.home().as_os_str()))]);
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

    // The round trip, not the key names: a file that persisted an empty or
    // wrong path would pass a `contains("cert_dir")` check while every later
    // start silently served from somewhere else.
    let written: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).expect("read back"))
            .expect("the written file parses back");
    assert_eq!(
        written["schema"],
        serde_json::json!(1),
        "the file an operator opens has to show the schema it is read with: {written}"
    );
    assert_eq!(
        written["cert_dir"].as_str().map(Path::new),
        Some(dir.as_path()),
        "what was written parses back to what was configured: {written}"
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

/// Serving certificates out of a directory the operator thinks they moved away
/// from is worse than refusing to start.
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

/// A newer schema is read, not rejected, because the one field this build
/// needs is a path either way.
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

// ── tailnet-range bind refusal ─────────────────────────────────────────────

/// The two ranges are Tailscale's own (`net/tsaddr`'s `CGNATRange` and
/// `TailscaleULARange`), pinned at their exact boundaries.
#[test]
fn tailscale_range_matches_only_tailscale_assigned_addresses() {
    for (addr, inside) in [
        // CGNATRange 100.64.0.0/10: the two edges and the two just outside.
        ("100.63.255.255", false),
        ("100.64.0.0", true),
        ("100.127.255.255", true),
        ("100.128.0.0", false),
        // TailscaleULARange fd7a:115c:a1e0::/48.
        ("fd7a:115c:a1df:ffff:ffff:ffff:ffff:ffff", false),
        ("fd7a:115c:a1e0::", true),
        ("fd7a:115c:a1e0:ffff:ffff:ffff:ffff:ffff", true),
        ("fd7a:115c:a1e1::", false),
        // Neither range, plus the IPv4-mapped form that must canonicalize.
        ("0.0.0.0", false),
        ("127.0.0.1", false),
        ("::1", false),
        ("::ffff:100.64.1.2", true),
    ] {
        let ip: IpAddr = addr.parse().expect("parse address");
        assert_eq!(
            tailscale_range(ip).is_some(),
            inside,
            "{addr} should be {}",
            if inside { "inside" } else { "outside" }
        );
    }
}

/// The IPv4-mapped form maps to its IPv4 address, and the range it names is
/// the IPv4 one — so the refusal the operator sees is the range their
/// `--listen` spelled out.
#[test]
fn an_ipv4_mapped_tailnet_address_canonicalizes_to_the_ipv4_range() {
    let mapped: IpAddr = "::ffff:100.64.1.2".parse().expect("mapped addr");
    assert_eq!(tailscale_range(mapped), Some(TAILSCALE_IPV4_RANGE));
}

/// The tailnet refusal for a known IPv4 bind, word-for-word. Pinned by
/// equality, never by substring: a wording drift or a wrong range name must
/// fail the suite, not pass on a fragment.
const TAILNET_REFUSAL_V4: &str = "100.64.1.2 is in the 100.64.0.0/10 range Tailscale assigns addresses from, and this host's lego certificate is not available; run `tailscale cert <machine>.<tailnet>.ts.net` and pass `--cert <machine>.<tailnet>.ts.net.crt --key <machine>.<tailnet>.ts.net.key` to `clauth daemon --listen`";

/// The same refusal for a known IPv6 bind. Repeated rather than derived: the
/// interpolated range must be the IPv6 one.
const TAILNET_REFUSAL_V6: &str = "fd7a:115c:a1e0::1 is in the fd7a:115c:a1e0::/48 range Tailscale assigns addresses from, and this host's lego certificate is not available; run `tailscale cert <machine>.<tailnet>.ts.net` and pass `--cert <machine>.<tailnet>.ts.net.crt --key <machine>.<tailnet>.ts.net.key` to `clauth daemon --listen`";

/// The seam that takes the lego paths: a missing `.crt` on a tailnet-range
/// bind becomes the tailnet refusal, pinned word-for-word, with the lego cause
/// kept in the chain; on any other bind it stays lego's missing-file error.
#[test]
fn a_missing_certificate_on_a_tailnet_bind_becomes_the_tailscale_refusal() {
    let dir = tempfile::tempdir().expect("tempdir");
    let paths = lego_paths_in(dir.path(), "host.example");
    let lego_cause = format!(
        "failed to read the TLS certificate {}",
        paths.cert.display()
    );

    let tailnet: IpAddr = "100.64.1.2".parse().expect("tailnet v4 addr");
    let Err(err) = load_lego_or_refuse(tailnet, &paths) else {
        panic!("an absent certificate must fail");
    };
    assert_eq!(
        err.to_string(),
        TAILNET_REFUSAL_V4,
        "the IPv4 tailnet bind gets the exact refusal sentence"
    );
    assert_eq!(
        err.chain()
            .nth(1)
            .expect("the lego cause stays in the chain")
            .to_string(),
        lego_cause,
        "the lego path that was looked for stays in the chain"
    );

    let tailnet_v6: IpAddr = "fd7a:115c:a1e0::1".parse().expect("tailnet v6 addr");
    let Err(err) = load_lego_or_refuse(tailnet_v6, &paths) else {
        panic!("an absent certificate must fail");
    };
    assert_eq!(
        err.to_string(),
        TAILNET_REFUSAL_V6,
        "the IPv6 tailnet bind gets the exact refusal sentence with its range"
    );

    let elsewhere: IpAddr = "192.0.2.1".parse().expect("non-tailnet addr");
    let Err(err) = load_lego_or_refuse(elsewhere, &paths) else {
        panic!("an absent certificate must fail");
    };
    assert_eq!(
        err.to_string(),
        lego_cause,
        "a non-tailnet bind keeps the plain lego error, no refusal"
    );
}

/// A `.crt` that cannot even be stat'ed — here the "directory" holding it is a
/// regular file, so every read and stat returns ENOTDIR — is not a not-found,
/// and must NOT become the tailnet refusal: the refusal is for a certificate
/// that is genuinely absent, and the operator's real problem here is their
/// `cert_dir`.
///
/// Unix only: Windows reports a path through a regular file as
/// `ERROR_PATH_NOT_FOUND`, which std maps to `NotFound`, so there this path
/// reads as absent and the refusal is the expected answer.
#[cfg(unix)]
#[test]
fn a_certificate_path_that_cannot_be_statd_keeps_todays_error() {
    let dir = tempfile::tempdir().expect("tempdir");
    let blocker = dir.path().join("cert_dir_is_a_file");
    std::fs::write(&blocker, "not a directory").expect("write blocker");
    let paths = lego_paths_in(&blocker, "host.example");
    let tailnet: IpAddr = "100.64.1.2".parse().expect("tailnet addr");

    let Err(err) = load_lego_or_refuse(tailnet, &paths) else {
        panic!("an unreadable certificate path must fail");
    };
    assert_eq!(
        err.to_string(),
        format!(
            "failed to read the TLS certificate {}",
            paths.cert.display()
        ),
        "a stat failure that is not a not-found keeps today's lego error"
    );
}

/// A `.crt` inside a directory the daemon cannot search (a root-run lego's
/// `0o700` certificate directory beside a user-run daemon) is present, not
/// absent, so it keeps today's error rather than becoming the tailnet refusal.
/// Unix only, and skipped under root, which can search any directory.
#[cfg(unix)]
#[test]
fn a_certificate_behind_an_unsearchable_directory_keeps_todays_error() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let dir = tempfile::tempdir().expect("tempdir");
    if std::fs::metadata(dir.path()).expect("stat tempdir").uid() == 0 {
        eprintln!("SKIPPING: running as root, which can search any directory");
        return;
    }
    let locked = dir.path().join("certificates");
    std::fs::create_dir(&locked).expect("mkdir");
    let paths = lego_paths_in(&locked, "host.example");
    std::fs::write(&paths.cert, "present").expect("write cert");
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).expect("lock dir");
    let tailnet: IpAddr = "100.64.1.2".parse().expect("tailnet addr");

    let result = load_lego_or_refuse(tailnet, &paths);
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o700)).expect("unlock dir");

    let Err(err) = result else {
        panic!("a certificate behind an unsearchable directory must fail to load");
    };
    assert_eq!(
        err.to_string(),
        format!(
            "failed to read the TLS certificate {}",
            paths.cert.display()
        ),
        "EACCES is not a not-found, so it keeps today's lego error"
    );
}

/// A certificate that is present but does not parse keeps today's error even
/// on a tailnet bind: only "the certificate is not there" turns into the
/// refusal, not "the certificate is wrong".
#[test]
fn an_unparseable_certificate_on_a_tailnet_bind_keeps_todays_error() {
    let dir = tempfile::tempdir().expect("tempdir");
    let paths = lego_paths_in(dir.path(), "host.example");
    std::fs::write(&paths.cert, "not a certificate").expect("write malformed leaf");
    let tailnet: IpAddr = "100.64.1.2".parse().expect("tailnet addr");

    let Err(err) = load_lego_or_refuse(tailnet, &paths) else {
        panic!("a malformed certificate must fail");
    };
    let msg = format!("{err:#}");
    assert!(
        !msg.contains("tailscale cert"),
        "an unparseable certificate keeps today's error: {msg}"
    );
    assert!(
        msg.contains("host.example.crt"),
        "and still names the file: {msg}"
    );
}

// ── FQDN-failure arm ───────────────────────────────────────────────────────

/// The `fqdn().map_err(|cause| refuse_on_tailnet(listen, cause))` wiring: a
/// lego bind whose FQDN lookup fails maps to the tailnet refusal on a
/// tailnet-range address, with the lookup error kept in the chain.
#[test]
fn an_fqdn_failure_on_a_tailnet_bind_becomes_the_tailscale_refusal() {
    let _home = HomeSandbox::new();
    let tailnet: IpAddr = "100.64.1.2".parse().expect("tailnet addr");
    *FQDN_OVERRIDE.lock().expect("fqdn override") = Some("forced fqdn failure".to_string());
    let Err(err) = server_config(&CertSource::Lego, tailnet) else {
        panic!("a failed FQDN lookup must fail the lego load");
    };
    *FQDN_OVERRIDE.lock().expect("fqdn override") = None;
    assert_eq!(
        err.to_string(),
        TAILNET_REFUSAL_V4,
        "a failed lookup on a tailnet bind gets the same refusal sentence"
    );
    assert_eq!(
        err.chain()
            .nth(1)
            .expect("the lookup error stays in the chain")
            .to_string(),
        "forced fqdn failure",
        "the lookup cause stays in the chain"
    );
}

/// The same failed lookup on any other bind passes the lookup error through
/// untouched — no refusal, no range mention.
#[test]
fn an_fqdn_failure_on_a_non_tailnet_bind_stays_the_lookup_error() {
    let _home = HomeSandbox::new();
    let elsewhere: IpAddr = "192.0.2.1".parse().expect("non-tailnet addr");
    *FQDN_OVERRIDE.lock().expect("fqdn override") = Some("forced fqdn failure".to_string());
    let Err(err) = server_config(&CertSource::Lego, elsewhere) else {
        panic!("a failed FQDN lookup must fail the lego load");
    };
    *FQDN_OVERRIDE.lock().expect("fqdn override") = None;
    assert_eq!(
        err.to_string(),
        "forced fqdn failure",
        "a non-tailnet bind keeps the plain lookup error"
    );
}
