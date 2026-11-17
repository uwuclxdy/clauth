//! The restart-handover probe's answer, driven against the real binary.
//!
//! The running daemon probes a replacement with its own argv plus the hidden
//! `--__exec-handover` flag before it stops anything. A replacement that will
//! serve the REST API loads its certificate first: a certificate it cannot
//! load answers the cert-refused marker and the reason instead of the
//! capability token, so the old image keeps serving rather than exec into a
//! start that dies on the certificate.
//!
//! Unix only: the child resolves its home through `$HOME`, which only Unix
//! lets a test point at a sandbox, and only a Unix daemon ever probes a
//! replacement.
#![cfg(unix)]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "fixture helpers outside #[test] unwrap and expect their setup; an allow, since clippy 1.100 no longer fires there and 1.99 still does"
)]

use std::process::{Command, Stdio};

const TOKEN: &str = "clauth-exec-handover";
const CERT_REFUSED: &str = "clauth-exec-handover-cert-refused\n";

/// Run `clauth daemon <args> --__exec-handover` in a sandbox HOME; returns
/// (exit success, stdout).
fn probe(args: &[&str], no_api: bool) -> (bool, String) {
    let home = tempfile::tempdir().unwrap();
    probe_in(home.path(), args, no_api)
}

/// [`probe`] against a caller-owned sandbox HOME, so a test can read what the
/// answer left there.
fn probe_in(home: &std::path::Path, args: &[&str], no_api: bool) -> (bool, String) {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_clauth"));
    cmd.arg("daemon")
        .args(args)
        .arg("--__exec-handover")
        .env("HOME", home)
        .env_remove("CLAUDE_CONFIG_DIR")
        .env_remove("CODEX_HOME")
        .env_remove("CLAUTH_NO_API")
        .env("CLAUTH_NO_UPDATE", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if no_api {
        cmd.env("CLAUTH_NO_API", "1");
    }
    let out = cmd.output().expect("spawn clauth");
    (out.status.success(), String::from_utf8(out.stdout).unwrap())
}

/// `--listen` with an explicit certificate pair under `dir` that does not exist.
fn missing_cert_args(dir: &std::path::Path) -> Vec<String> {
    vec![
        "--listen".into(),
        "127.0.0.1:0".into(),
        "--cert".into(),
        dir.join("gone.crt").display().to_string(),
        "--key".into(),
        dir.join("gone.key").display().to_string(),
    ]
}

/// A `--listen` replacement whose certificate cannot be read answers the
/// cert-refused marker and a one-line reason, never the capability token.
#[test]
fn a_listen_probe_with_an_unreadable_certificate_answers_cert_refused() {
    let scratch = tempfile::tempdir().unwrap();
    let args = missing_cert_args(scratch.path());
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let (ok, stdout) = probe(&args, false);
    assert!(
        stdout.starts_with(CERT_REFUSED),
        "an unreadable certificate answers the cert-refused marker: {stdout:?}"
    );
    assert!(!ok, "a cert-refused answer exits non-zero");
    let reason = &stdout[CERT_REFUSED.len()..];
    assert!(
        !reason.trim().is_empty() && !reason.contains('\n'),
        "the reason is one non-empty line: {reason:?}"
    );
}

/// Controls: a probe with no `--listen`, and a `--listen` probe under the
/// REST kill switch, load no certificate and answer exactly the token.
#[test]
fn a_probe_that_serves_no_api_answers_the_bare_token() {
    let (ok, stdout) = probe(&[], false);
    assert!(ok, "no --listen: exit 0");
    assert_eq!(stdout, TOKEN, "no --listen: the bare token");
    let scratch = tempfile::tempdir().unwrap();
    let args = missing_cert_args(scratch.path());
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let (ok, stdout) = probe(&args, true);
    assert!(ok, "CLAUTH_NO_API=1: exit 0");
    assert_eq!(
        stdout, TOKEN,
        "CLAUTH_NO_API=1 reads no certificate: the bare token"
    );
}

/// A `--listen` probe whose explicit certificate pair is readable loads it and
/// answers exactly the bare token, exit 0: the certificate read the answer
/// runs is the one the exec'd start would serve, and a capable replacement's
/// answer stays byte-for-byte today's.
#[test]
fn a_listen_probe_with_a_readable_certificate_answers_the_bare_token() {
    use base64::Engine as _;
    let scratch = tempfile::tempdir().unwrap();
    let cert_path = scratch.path().join("leaf.crt");
    std::fs::write(
        &cert_path,
        include_str!("fixtures/tls-root-bundle/leaf.crt"),
    )
    .unwrap();
    // The fixture's key is PKCS#8 DER; the daemon reads PEM, so wrap it.
    let der = include_bytes!("fixtures/tls-root-bundle/leaf.key.der");
    let mut pem = String::from("-----BEGIN PRIVATE KEY-----\n");
    for line in base64::engine::general_purpose::STANDARD
        .encode(der)
        .as_bytes()
        .chunks(64)
    {
        pem.push_str(std::str::from_utf8(line).unwrap());
        pem.push('\n');
    }
    pem.push_str("-----END PRIVATE KEY-----\n");
    let key_path = scratch.path().join("leaf.key.pem");
    std::fs::write(&key_path, pem).unwrap();
    let cert_arg = cert_path.display().to_string();
    let key_arg = key_path.display().to_string();
    let args = [
        "--listen",
        "127.0.0.1:0",
        "--cert",
        cert_arg.as_str(),
        "--key",
        key_arg.as_str(),
    ];
    let (ok, stdout) = probe(&args, false);
    assert!(ok, "a readable certificate answers exit 0");
    assert_eq!(
        stdout, TOKEN,
        "the readable certificate answers exactly the bare token"
    );
}

/// A replacement on the lego source reads its certificate directory from
/// `~/.clauth/tls.json` and writes nothing under the home: no state lock
/// beside it, and the config left as written. The config points at an empty
/// directory, so the cert-refused marker proves the lego read ran without
/// depending on this host's `/etc/lego` tree.
#[test]
fn a_lego_source_probe_reads_the_config_without_writing_the_home() {
    let home = tempfile::tempdir().unwrap();
    let certs = tempfile::tempdir().unwrap();
    let clauth_dir = home.path().join(".clauth");
    std::fs::create_dir_all(&clauth_dir).unwrap();
    let config = clauth_dir.join("tls.json");
    let seeded = serde_json::json!({ "schema": 1, "cert_dir": certs.path() }).to_string();
    std::fs::write(&config, &seeded).unwrap();

    let (ok, stdout) = probe_in(home.path(), &["--listen", "127.0.0.1:0"], false);
    assert!(
        stdout.starts_with(CERT_REFUSED),
        "an empty certificate directory answers cert-refused: {stdout:?}"
    );
    assert!(!ok, "a cert-refused answer exits non-zero");
    let reason = &stdout[CERT_REFUSED.len()..];
    assert!(
        reason.contains(&certs.path().display().to_string()),
        "the reason names the seeded certificate directory: {reason:?}"
    );
    assert!(
        !clauth_dir.join(".lock").exists(),
        "the answer takes no state lock under the home it reads"
    );
    assert_eq!(
        std::fs::read_to_string(&config).expect("the config stays"),
        seeded,
        "the answer leaves the config exactly as it found it"
    );
}

/// The value-less `--listen` spelling (clap fills its default): the appended
/// probe flag is not eaten as the address, and under the REST kill switch the
/// answer is the bare token.
#[test]
fn a_value_less_listen_probe_under_the_kill_switch_answers_the_bare_token() {
    let (ok, stdout) = probe(&["--listen"], true);
    assert!(ok, "CLAUTH_NO_API=1: exit 0");
    assert_eq!(
        stdout, TOKEN,
        "the appended flag is not taken as --listen's value"
    );
}

/// A `--replace` probe answers before the start's claim: the bare token, and
/// no `~/.clauth` for a start that never got there.
#[test]
fn a_replace_probe_without_listen_answers_the_bare_token_and_touches_no_home() {
    let home = tempfile::tempdir().unwrap();
    let (ok, stdout) = probe_in(home.path(), &["--replace"], false);
    assert!(ok, "a --replace probe exits 0");
    assert_eq!(stdout, TOKEN, "a --replace probe answers the bare token");
    assert!(
        !home.path().join(".clauth").exists(),
        "the answer precedes every claim and home write"
    );
}
