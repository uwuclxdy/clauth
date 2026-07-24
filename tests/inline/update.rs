#![allow(unsafe_code)]
//! Unit tests for `src/update.rs`.
//!
//! All tests are pure (no network, no FS, no extra threads).  The env-var tests
//! mutate `CLAUTH_NO_UPDATE` through `with_no_update_env`, which saves and
//! restores the original value so parallel test threads can't observe a leak.

use super::*;

// ── verify_sha256 ──────────────────────────────────────────────────────────

/// Compute the SHA-256 of `bytes` as a lowercase hex string — used by tests
/// to generate a ground-truth expected value without duplicating the
/// production algorithm.
fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest as _, Sha256};
    Sha256::digest(bytes)
        .iter()
        .fold(String::with_capacity(64), |mut s, b| {
            use std::fmt::Write as _;
            let _ = write!(s, "{b:02x}");
            s
        })
}

#[test]
fn verify_sha256_correct_bytes_passes() {
    let bytes = b"hello clauth update integrity";
    let hex = sha256_hex(bytes);
    assert!(verify_sha256(bytes, &hex), "correct digest must verify");
}

#[test]
fn verify_sha256_flipped_byte_fails() {
    let mut bytes = b"hello clauth update integrity".to_vec();
    let hex = sha256_hex(&bytes);
    bytes[0] ^= 0x01; // flip one bit
    assert!(!verify_sha256(&bytes, &hex), "flipped byte must not verify");
}

#[test]
fn verify_sha256_flipped_hex_char_fails() {
    let bytes = b"hello clauth update integrity";
    let mut hex = sha256_hex(bytes);
    // Flip the first hex nibble: '0'→'1', everything else → '0'.
    let first = hex.chars().next().unwrap_or('0');
    let replacement = if first == '0' { '1' } else { '0' };
    hex.replace_range(0..1, &replacement.to_string());
    assert!(!verify_sha256(bytes, &hex), "mutated hex must not verify");
}

#[test]
fn verify_sha256_empty_bytes() {
    // SHA-256 of empty slice is well-known.
    let hex = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
    assert!(verify_sha256(b"", hex));
}

#[test]
fn verify_sha256_rejects_wrong_length_hex() {
    assert!(!verify_sha256(b"anything", "deadbeef"));
}

#[test]
fn verify_sha256_uppercase_hex_also_accepted() {
    let bytes = b"case check";
    let hex = sha256_hex(bytes).to_uppercase();
    assert!(verify_sha256(bytes, &hex), "uppercase hex must verify too");
}

// ── parse_sums_line ────────────────────────────────────────────────────────

#[test]
fn parse_sums_line_valid() {
    let line =
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855  clauth-linux-x86_64";
    let (hex, name) = parse_sums_line(line).expect("valid line must parse");
    assert_eq!(
        hex,
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
    assert_eq!(name, "clauth-linux-x86_64");
}

#[test]
fn parse_sums_line_rejects_single_space() {
    // sha256sum uses two spaces; one space is not valid.
    let line =
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855 clauth-linux-x86_64";
    assert!(parse_sums_line(line).is_none());
}

#[test]
fn parse_sums_line_rejects_short_hex() {
    let line = "deadbeef  clauth-linux-x86_64";
    assert!(parse_sums_line(line).is_none());
}

// ── find_expected_sha ──────────────────────────────────────────────────────

const SUMS_TEXT: &str = "\
e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855  clauth-linux-x86_64\n\
aabbccdd00112233445566778899aabbccddeeff00112233445566778899aabb  clauth-macos-aarch64\n\
";

#[test]
fn find_expected_sha_finds_matching_asset() {
    let hex = find_expected_sha(SUMS_TEXT, "clauth-linux-x86_64").expect("should find it");
    assert_eq!(
        hex,
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
}

#[test]
fn find_expected_sha_returns_none_for_missing_asset() {
    assert!(find_expected_sha(SUMS_TEXT, "clauth-windows-x86_64.exe").is_none());
}

#[test]
fn find_expected_sha_empty_sums_file() {
    assert!(find_expected_sha("", "clauth-linux-x86_64").is_none());
}

// ── updates_enabled (env opt-out) + spawn suppression ──────────────────────

/// Set/restore `CLAUTH_NO_UPDATE` around a closure.
///
/// # Safety
/// `set_var`/`remove_var` are unsafe in Rust 2024 because they aren't
/// thread-safe in a multi-threaded process. Serialized here by `HOME_TEST_LOCK`
/// (the one mutex every env mutator across the suite takes) and undone before
/// the closure returns, so no other thread observes a torn value.
fn with_no_update_env<F: FnOnce()>(val: Option<&str>, f: F) {
    let _guard = crate::profile::HOME_TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let saved = std::env::var("CLAUTH_NO_UPDATE").ok();
    // SAFETY: test-only, single-threaded execution, restored unconditionally.
    unsafe {
        match val {
            Some(v) => std::env::set_var("CLAUTH_NO_UPDATE", v),
            None => std::env::remove_var("CLAUTH_NO_UPDATE"),
        }
    }
    f();
    // SAFETY: same as above.
    unsafe {
        match &saved {
            Some(v) => std::env::set_var("CLAUTH_NO_UPDATE", v),
            None => std::env::remove_var("CLAUTH_NO_UPDATE"),
        }
    }
}

#[test]
fn updates_enabled_when_env_unset() {
    with_no_update_env(None, || {
        assert!(updates_enabled(), "unset env → updates enabled");
    });
}

#[test]
fn updates_disabled_when_env_is_one() {
    with_no_update_env(Some("1"), || {
        assert!(!updates_enabled(), "CLAUTH_NO_UPDATE=1 → updates disabled");
    });
}

#[test]
fn updates_enabled_when_env_is_zero() {
    with_no_update_env(Some("0"), || {
        assert!(
            updates_enabled(),
            "CLAUTH_NO_UPDATE=0 → updates still enabled"
        );
    });
}

#[test]
fn updates_enabled_when_env_is_other_value() {
    with_no_update_env(Some("true"), || {
        assert!(
            updates_enabled(),
            "CLAUTH_NO_UPDATE=true (not '1') → still enabled"
        );
    });
}

// ---------------------------------------------------------------------------
// FORK build gate (TECH-2): the fork has no release pipeline, so the upstream
// self-updater must never download-and-replace this binary — doing so would
// swap in an upstream build without the daemon/keychain features.
// ---------------------------------------------------------------------------

#[test]
fn fork_build_never_self_replaces_even_off_cargo() {
    // `false` is the non-cargo path — the one that WOULD download_and_replace on
    // upstream. A fork build refuses it, so download_and_replace is unreachable.
    assert!(
        !should_self_replace(false),
        "fork build must not self-replace even on the non-cargo path"
    );
    // A cargo install never self-replaces either (upgrade via `cargo install`).
    assert!(!should_self_replace(true));
}

#[test]
fn fork_build_spawn_returns_none() {
    let (tx, _rx) = std::sync::mpsc::channel();
    assert!(
        spawn(tx).is_none(),
        "a fork build never starts the background update thread"
    );
}

/// The real opt-out check: with the env var set, `spawn` must short-circuit and
/// do no work, so the receiver never sees an `UpdateEvent`.  With the env set,
/// `spawn` returns before spawning a thread, so there is no network I/O and the
/// channel is closed immediately when `tx` drops — non-flaky.
#[test]
fn spawn_sends_nothing_when_update_disabled() {
    with_no_update_env(Some("1"), || {
        let (tx, rx) = std::sync::mpsc::channel();
        spawn(tx); // returns early; `tx` is moved in and dropped at return
        match rx.try_recv() {
            Err(std::sync::mpsc::TryRecvError::Disconnected)
            | Err(std::sync::mpsc::TryRecvError::Empty) => {}
            Ok(_) => panic!("spawn must not deliver an event when updates are disabled"),
        }
    });
}

// ── derive_sums_url ────────────────────────────────────────────────────────

#[test]
fn derive_sums_url_replaces_asset_name() {
    let asset = "clauth-linux-x86_64";
    let url = "https://github.com/uwuclxdy/clauth/releases/download/v0.5.5/clauth-linux-x86_64";
    let sums = derive_sums_url(url, asset);
    assert_eq!(
        sums,
        "https://github.com/uwuclxdy/clauth/releases/download/v0.5.5/sha256sums.txt"
    );
}

// ── verify_minisign (detached-signature authenticity) ──────────────────────

/// Official `minisign-verify` test vector: this key + signature authenticate the
/// exact bytes `b"test"`.
const TEST_PUBKEY: &str = "RWQf6LRCGA9i53mlYecO4IzT51TGPpvWucNSCh1CBM0QTaLn73Y7GFO3";
const TEST_SIG: &str = "untrusted comment: signature from minisign secret key
RUQf6LRCGA9i559r3g7V1qNyJDApGip8MfqcadIgT9CuhV3EMhHoN1mGTkUidF/z7SrlQgXdy8ofjb7bNJJylDOocrCo8KLzZwo=
trusted comment: timestamp:1633700835\tfile:test\tprehashed
wLMDjy9FLAuxZ3q4NlEvkgtyhrr0gtTu6KC4KBJdITbbOeAi1zBIYo0v4iTgt8jJpIidRJnp94ABQkJAgAooBQ==";

#[test]
fn verify_minisign_empty_key_is_inactive() {
    // Empty pinned key ⇒ enforcement off: even nonsense inputs return Ok, so the
    // updater stays on SHA-256-only integrity during rollout.
    assert!(verify_minisign("", b"anything", "not a signature").is_ok());
}

#[test]
fn verify_minisign_known_good_vector_passes() {
    verify_minisign(TEST_PUBKEY, b"test", TEST_SIG).expect("official vector must verify");
}

#[test]
fn verify_minisign_tampered_payload_fails() {
    // Same valid key + signature, one transposed byte in the payload — the core
    // authenticity guarantee: a swapped sums file no longer verifies.
    assert!(verify_minisign(TEST_PUBKEY, b"tset", TEST_SIG).is_err());
}

#[test]
fn verify_minisign_garbage_key_fails() {
    assert!(verify_minisign("not-a-valid-key", b"test", TEST_SIG).is_err());
}

#[test]
fn verify_minisign_malformed_signature_fails() {
    assert!(verify_minisign(TEST_PUBKEY, b"test", "untrusted comment: x\nzzz").is_err());
}

#[test]
fn pinned_public_key_parses_when_set() {
    // A pinned key must decode — a typo would fail-close EVERY auto-update.
    // No-op while the key is the empty placeholder.
    let key = MINISIGN_PUBLIC_KEY.trim();
    if !key.is_empty() {
        PublicKey::from_base64(key).expect("pinned MINISIGN_PUBLIC_KEY must be a valid key");
    }
}
