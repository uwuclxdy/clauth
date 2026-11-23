#![allow(unsafe_code)]
//! Unit tests for `src/update.rs`.
//!
//! The parse, digest and signature tests are pure. The download and check
//! tests run against loopback stubs on their own threads; the env-var tests
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
        assert!(
            updates_enabled(true),
            "saved on, unset env → updates enabled"
        );
    });
}

#[test]
fn updates_disabled_when_env_is_one() {
    with_no_update_env(Some("1"), || {
        assert!(
            !updates_enabled(true),
            "CLAUTH_NO_UPDATE=1 → updates disabled"
        );
    });
}

#[test]
fn updates_enabled_when_env_is_zero() {
    with_no_update_env(Some("0"), || {
        assert!(
            updates_enabled(true),
            "CLAUTH_NO_UPDATE=0 → updates still enabled"
        );
    });
}

#[test]
fn updates_enabled_when_env_is_other_value() {
    with_no_update_env(Some("true"), || {
        assert!(
            updates_enabled(true),
            "CLAUTH_NO_UPDATE=true (not '1') → still enabled"
        );
    });
}

/// The real opt-out check: with the env var set, `spawn` must short-circuit and
/// do no work, so the receiver never sees an `UpdateEvent`.  With the env set,
/// `spawn` returns before spawning a thread, so there is no network I/O and the
/// channel is closed immediately when `tx` drops — non-flaky.
#[test]
fn spawn_sends_nothing_when_update_disabled() {
    with_no_update_env(Some("1"), || {
        let (tx, rx) = std::sync::mpsc::channel();
        spawn(tx, true); // returns early; `tx` is moved in and dropped at return
        match rx.try_recv() {
            Err(std::sync::mpsc::TryRecvError::Disconnected)
            | Err(std::sync::mpsc::TryRecvError::Empty) => {}
            Ok(_) => panic!("spawn must not deliver an event when updates are disabled"),
        }
    });
}

/// The saved `[update].auto_update` toggle is the other half of the gate:
/// saved × env is a truth table over ONE predicate, and an env value of `0`
/// (or anything but `"1"`) cannot re-enable a saved off.
#[test]
fn updates_enabled_follows_the_saved_toggle() {
    with_no_update_env(None, || {
        assert!(updates_enabled(true), "saved on + env unset → enabled");
        assert!(!updates_enabled(false), "saved off + env unset → disabled");
    });
    with_no_update_env(Some("1"), || {
        assert!(!updates_enabled(true), "env 1 wins over saved on");
        assert!(!updates_enabled(false), "saved off + env 1 → disabled");
    });
    with_no_update_env(Some("0"), || {
        assert!(updates_enabled(true), "env 0 leaves saved on enabled");
        assert!(
            !updates_enabled(false),
            "env 0 cannot re-enable a saved off"
        );
    });
}

/// `spawn` returns no handle and the channel closes immediately when the saved
/// toggle is off — no thread, no network path.
#[test]
fn spawn_returns_no_handle_and_closes_the_channel_when_saved_off() {
    with_no_update_env(None, || {
        let (tx, rx) = std::sync::mpsc::channel();
        assert!(spawn(tx, false).is_none(), "saved off → no handle");
        match rx.try_recv() {
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {}
            Err(std::sync::mpsc::TryRecvError::Empty) | Ok(_) => {
                panic!("saved off must close the channel, not just skip sending")
            }
        }
    });
}

// ── spawn's thread-start failure ───────────────────────────────────────────

/// A thread the OS refuses to start is a logged `None`, never a panic: the
/// calling thread's capture holds exactly the failure line and the failed
/// start leaves no handle.
#[test]
#[cfg(unix)]
fn a_refused_update_thread_start_logs_once_and_returns_no_handle() {
    let home = crate::testutil::HomeSandbox::new();
    let _env = crate::testutil::EnvPin::new(&home, &[("CLAUTH_NO_UPDATE", None)]);
    let _refused = fail_update_spawns();
    let os_error = std::thread::Builder::new()
        .stack_size(usize::MAX)
        .spawn(|| {})
        .expect_err("no kernel grants this stack size")
        .to_string();
    let lines = crate::logline::LogLines::new();
    let (tx, rx) = std::sync::mpsc::channel();
    let thread = {
        let _capture = lines.capture_here();
        spawn(tx, true)
    };
    assert!(thread.is_none(), "a refused start leaves no handle");
    assert_eq!(
        lines.snapshot(),
        vec![format!(
            "update check failed: cannot start the update thread: {os_error}"
        )],
        "one line, exact wording"
    );
    assert_eq!(
        rx.try_recv().err(),
        Some(std::sync::mpsc::TryRecvError::Disconnected),
        "the refused start drops the sender"
    );
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

/// The refusal the signature check itself gives: a parse or key-id refusal
/// reads differently, so a test pinning this text proves which check refused.
const SIGNATURE_CHECK_REFUSAL: &str =
    "minisign signature did not verify; refusing update: The signature verification failed";

/// The global signature covers the trusted comment: an edited comment over an
/// untouched payload signature no longer verifies.
#[test]
fn verify_minisign_tampered_trusted_comment_fails() {
    let sig = TEST_SIG.replace("timestamp:1633700835", "timestamp:1633700836");
    assert_ne!(sig, TEST_SIG, "the tamper must change the signature text");
    let err = verify_minisign(TEST_PUBKEY, b"test", &sig).expect_err("an edited comment refuses");
    assert_eq!(err.to_string(), SIGNATURE_CHECK_REFUSAL);
}

/// The global signature is checked, not only the payload signature.
#[test]
fn verify_minisign_tampered_global_signature_fails() {
    let sig = TEST_SIG.replace("wLMDjy9F", "xLMDjy9F");
    assert_ne!(sig, TEST_SIG, "the tamper must change the signature text");
    let err = verify_minisign(TEST_PUBKEY, b"test", &sig).expect_err("an edited signature refuses");
    assert_eq!(err.to_string(), SIGNATURE_CHECK_REFUSAL);
}

/// A key carrying the signature's key id but other key bytes parses, matches
/// the id, and still refuses: the id alone authenticates nothing.
#[test]
fn verify_minisign_other_key_with_the_same_id_fails() {
    use base64::Engine as _;
    let engine = base64::engine::general_purpose::STANDARD;
    let mut raw = engine.decode(TEST_PUBKEY).expect("the test key decodes");
    // Layout: 2-byte algorithm, 8-byte key id, 32-byte Ed25519 key.
    raw[10] ^= 0x01;
    let key = engine.encode(&raw);
    assert_ne!(key, TEST_PUBKEY, "the tamper must change the key");
    PublicKey::from_base64(&key)
        .expect("the altered key still parses, so the refusal is the signature check");
    let err = verify_minisign(&key, b"test", TEST_SIG).expect_err("another key refuses");
    assert_eq!(err.to_string(), SIGNATURE_CHECK_REFUSAL);
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

// ── is_newer ───────────────────────────────────────────────────────────────

/// `MAJOR.MINOR.PATCH` compares as numbers: `0.9.0` sorts after `0.17.0` as
/// text and `0.100.0` before it, so a text comparison flips both. A leading
/// `v` is a tag prefix; an unparseable side is never newer.
#[test]
fn is_newer_compares_versions_as_numbers() {
    let cases = [
        ("0.17.0", "0.9.0", true, "a newer minor with more digits"),
        ("0.9.0", "0.17.0", false, "an older minor with fewer digits"),
        (
            "0.100.0",
            "0.17.0",
            true,
            "a newer minor that sorts first as text",
        ),
        ("0.17.1", "0.17.0", true, "a newer patch"),
        ("0.17.0", "0.17.0", false, "the same version"),
        ("v0.18.0", "0.17.0", true, "a tag's leading v"),
        ("dev", "0.17.0", false, "an unparseable tag"),
        ("0.17.0", "dev", false, "an unparseable current version"),
    ];
    for (tag, current, expected, case) in cases {
        assert_eq!(
            is_newer(tag, current),
            expected,
            "{case}: is_newer({tag:?}, {current:?})"
        );
    }
}

/// The major gate reads the leading major number alone, so a tag the full
/// parse rejects still counts when its major is higher; a tag whose leading
/// number does not parse never is.
#[test]
fn is_higher_major_reads_the_leading_major_alone() {
    let cases = [
        ("v1.0.0", true, "a parseable higher major"),
        ("v1.0.0-rc.1", true, "a prerelease of a higher major"),
        ("1.0", true, "a two-part higher major"),
        ("v0.20.0-rc.1", false, "a same-major prerelease"),
        ("v0.19.0", false, "the same version"),
        ("garbage", false, "an unreadable tag"),
    ];
    for (tag, expected, case) in cases {
        assert_eq!(
            is_higher_major(tag, "0.19.0"),
            expected,
            "{case}: is_higher_major({tag:?}, \"0.19.0\")"
        );
    }
}

// ── download bounds (the asset's listed size, a slow-link time budget) ──────

/// A loopback stub that reads one request head, writes `reply`, then holds the
/// socket open for `hold` before closing it: a hold longer than the caller's
/// bound is a body that stalls mid-read.
fn serve_once(reply: Vec<u8>, hold: Duration) -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        if let Ok((mut sock, _)) = listener.accept() {
            crate::testutil::read_request_head(&mut sock);
            let _ = sock.write_all(&reply);
            let _ = sock.flush();
            std::thread::sleep(hold);
        }
    });
    format!("http://{addr}/asset")
}

fn ok_reply(content_length: usize, body: &[u8]) -> Vec<u8> {
    let mut reply =
        format!("HTTP/1.1 200 OK\r\nContent-Length: {content_length}\r\nConnection: close\r\n\r\n")
            .into_bytes();
    reply.extend_from_slice(body);
    reply
}

/// Runs `f` on its own thread and fails the test when it has not returned
/// within `within`: a call with no bound hangs, and a hang must read as a red
/// assertion, never as a wedged suite.
fn ends_within<T: Send + 'static>(within: Duration, f: impl FnOnce() -> T + Send + 'static) -> T {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(f());
    });
    rx.recv_timeout(within)
        .unwrap_or_else(|_| panic!("the call did not end within {within:?}"))
}

/// The bug this pins: ureq's `read_to_vec` caps a body at 10 MiB by default,
/// and every release binary since v0.14.1 is larger (the linux asset of
/// v0.17.0 is 16449936 bytes), so every self-update failed. The cap is the
/// asset's own listed size.
#[test]
fn an_asset_past_ten_mib_downloads_whole() {
    let size = 11 * 1024 * 1024;
    let body = vec![7u8; size];
    let url = serve_once(ok_reply(size, &body), Duration::from_secs(1));
    let got = ends_within(Duration::from_secs(60), move || {
        fetch_asset(&url, size as u64, Duration::from_secs(50))
    })
    .expect("an 11 MiB asset listed at 11 MiB downloads");
    assert_eq!(got.len(), size);
}

/// The listed size is a ceiling: a server sending more than the release
/// listing declared is refused, never read into memory past it.
#[test]
fn a_body_past_the_listed_size_is_refused() {
    let body = vec![1u8; 2000];
    let url = serve_once(ok_reply(2000, &body), Duration::from_secs(1));
    let got = ends_within(Duration::from_secs(20), move || {
        fetch_asset(&url, 1000, Duration::from_secs(10))
    });
    assert!(
        got.is_err(),
        "a 2000-byte body against a 1000-byte listing must be refused"
    );
}

/// The ceiling sits exactly at the listed size: one byte past it is refused,
/// and a body of exactly the listed size reads whole (ureq's `limit(n)` refuses
/// an n-byte body, so the bound the code passes is not the size itself).
#[test]
fn the_listed_size_is_the_exact_ceiling() {
    let exact = vec![2u8; 1000];
    let url = serve_once(ok_reply(1000, &exact), Duration::from_secs(1));
    let got = ends_within(Duration::from_secs(20), move || {
        fetch_asset(&url, 1000, Duration::from_secs(10))
    })
    .expect("a body of exactly the listed size reads whole");
    assert_eq!(got.len(), 1000);

    let over = vec![2u8; 1001];
    let url = serve_once(ok_reply(1001, &over), Duration::from_secs(1));
    let got = ends_within(Duration::from_secs(20), move || {
        fetch_asset(&url, 1000, Duration::from_secs(10))
    });
    assert!(
        got.is_err(),
        "a body one byte past the listed size must be refused"
    );
}

/// A body that stops arriving ends at the call's budget: the stub sends its
/// headers and 10 of 1000 bytes, then holds the socket silent for 8 s.
#[test]
fn a_stalled_download_ends_at_its_budget() {
    let url = serve_once(ok_reply(1000, &[0u8; 10]), Duration::from_secs(8));
    let started = std::time::Instant::now();
    let got = ends_within(Duration::from_secs(6), move || {
        fetch_asset(&url, 1000, Duration::from_millis(500))
    });
    assert!(
        got.is_err(),
        "a stalled body must fail, not return a short read"
    );
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "the 500 ms budget must end the call, took {:?}",
        started.elapsed()
    );
}

/// The budget pays the metadata bound plus the listed size at 16 KiB/s, whole
/// seconds rounded up, so a slow link still finishes: a 16 MiB binary gets
/// 35 + 1024 s.
#[test]
fn the_download_budget_pays_a_slow_link() {
    assert_eq!(download_budget(0), Duration::from_secs(35));
    assert_eq!(download_budget(1), Duration::from_secs(36));
    assert_eq!(download_budget(16 * 1024), Duration::from_secs(36));
    assert_eq!(download_budget(16 * 1024 + 1), Duration::from_secs(37));
    assert_eq!(
        download_budget(16 * 1024 * 1024),
        Duration::from_secs(35 + 1024)
    );
}

// ── metadata calls (release listing, sums, signature) ──────────────────────

/// The release listing, sums file and signature calls share one agent whose
/// whole call is bounded: ureq 3.4.2's header bound stops at the headers.
#[test]
fn the_metadata_agent_bounds_the_whole_call_at_35_s() {
    assert_eq!(METADATA_CALL_BOUND, Duration::from_secs(35));
    assert_eq!(
        make_agent().config().timeouts().global,
        Some(METADATA_CALL_BOUND)
    );
}

#[test]
fn a_stalled_metadata_body_ends_at_its_bound() {
    let url = serve_once(ok_reply(1000, b"{\"tag_"), Duration::from_secs(8));
    let started = std::time::Instant::now();
    let got = ends_within(Duration::from_secs(6), move || {
        metadata_agent(Duration::from_millis(500))
            .get(&url)
            .call()
            .map_err(|e| e.to_string())
            .and_then(|mut r| r.body_mut().read_to_string().map_err(|e| e.to_string()))
    });
    assert!(got.is_err(), "a stalled metadata body must fail");
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "the 500 ms bound must end the call, took {:?}",
        started.elapsed()
    );
}

// ── the whole check, against a multi-route stub ────────────────────────────

/// One route of [`serve_paths`]: a fixed reply, or headers plus a short body
/// and then a socket held silent for 8 s (a stalled body).
enum Route {
    Reply(Vec<u8>),
    Stall,
}

/// A loopback stub answering by request path. `make` receives the stub's base
/// URL (`http://127.0.0.1:<port>`) so a listing can point its asset at the same
/// stub. Every path requested is recorded, in order, in the returned list; an
/// unrouted path gets a 404. The accept loop ends 20 s after the stub starts.
fn serve_paths(
    make: impl FnOnce(&str) -> Vec<(String, Route)>,
) -> (String, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let routes = make(&base);
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let record = std::sync::Arc::clone(&seen);
    std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        while std::time::Instant::now() < deadline {
            let (mut sock, _) = match listener.accept() {
                Ok(pair) => pair,
                Err(_) => {
                    std::thread::sleep(Duration::from_millis(10));
                    continue;
                }
            };
            let _ = sock.set_nonblocking(false);
            let path = read_request_path(&mut sock);
            record.lock().unwrap().push(path.clone());
            let route = routes.iter().find(|(p, _)| *p == path).map(|(_, r)| r);
            std::thread::spawn({
                let reply = match route {
                    Some(Route::Reply(bytes)) => Some(bytes.clone()),
                    Some(Route::Stall) => None,
                    None => Some(
                        b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                            .to_vec(),
                    ),
                };
                move || match reply {
                    Some(bytes) => {
                        let _ = sock.write_all(&bytes);
                        let _ = sock.flush();
                        std::thread::sleep(Duration::from_millis(200));
                    }
                    None => {
                        let _ = sock.write_all(&ok_reply(1000, b"{\"ta"));
                        let _ = sock.flush();
                        std::thread::sleep(Duration::from_secs(8));
                    }
                }
            });
        }
    });
    (base, seen)
}

/// Reads a request head and returns its path (`GET /x HTTP/1.1` -> `/x`).
fn read_request_path(sock: &mut std::net::TcpStream) -> String {
    let head = crate::testutil::read_request_head(sock);
    String::from_utf8_lossy(&head)
        .split_whitespace()
        .nth(1)
        .unwrap_or_default()
        .to_owned()
}

/// Builds a stub's routes from its base URL.
type RouteTable = Box<dyn Fn(&str) -> Vec<(String, Route)>>;

fn json_reply(body: &str) -> Route {
    Route::Reply(ok_reply(body.len(), body.as_bytes()))
}

/// This platform's asset name; every CI target ships one.
fn platform_asset() -> &'static str {
    asset_name().expect("every target the suite runs on ships a release asset")
}

/// A newer same-major release (a higher major is never downloaded) listing one
/// asset for this platform at `size`, served from `<base>/dl/<asset>`, so its
/// sums file is `<base>/dl/sha256sums.txt`.
fn listing(base: &str, size: Option<u64>) -> String {
    let asset = platform_asset();
    let size = size.map(|n| format!(",\"size\":{n}")).unwrap_or_default();
    format!(
        "{{\"tag_name\":\"v0.999.0\",\"assets\":[{{\"name\":\"{asset}\",\"browser_download_url\":\"{base}/dl/{asset}\"{size}}}]}}"
    )
}

/// Runs one whole check against `<base>/releases/latest` with the metadata
/// calls bounded at `bound`, on its own thread with a log capture installed
/// there. Returns the captured lines, the elapsed time, and whether an event
/// was sent.
fn run_whole_check(base: &str, bound: Duration) -> (Vec<String>, Duration, bool) {
    let url = format!("{base}/releases/latest");
    let started = std::time::Instant::now();
    let (lines, sent) = ends_within(Duration::from_secs(15), move || {
        let lines = crate::logline::LogLines::new();
        let (tx, rx) = std::sync::mpsc::channel();
        let gate = ReplaceGate::new();
        {
            let _guard = lines.capture_here();
            run_check_with(&tx, &url, &metadata_agent(bound), &gate);
        }
        (lines.snapshot(), rx.try_recv().is_ok())
    });
    (lines, started.elapsed(), sent)
}

fn assert_one_failure_line(lines: &[String], case: &str) {
    assert_eq!(
        lines.len(),
        1,
        "{case}: one line per failed check: {lines:?}"
    );
    assert!(
        lines[0].contains("update check failed"),
        "{case}: the line names the failed check: {lines:?}"
    );
}

/// Each metadata call takes the check's own bounded agent: a stall on the
/// listing, the sums file or the signature ends the check at the 500 ms bound
/// with one line, never a hang.
#[test]
fn a_stall_on_any_metadata_call_ends_the_check_at_its_bound() {
    let _home = crate::testutil::HomeSandbox::new();
    let sums_line = format!("{}  {}\n", "a".repeat(64), platform_asset());
    let cases: [(&str, RouteTable); 3] = [
        (
            "the listing",
            Box::new(|_| vec![("/releases/latest".to_owned(), Route::Stall)]),
        ),
        (
            "the sums file",
            Box::new(|base| {
                vec![
                    (
                        "/releases/latest".to_owned(),
                        json_reply(&listing(base, Some(1000))),
                    ),
                    ("/dl/sha256sums.txt".to_owned(), Route::Stall),
                ]
            }),
        ),
        (
            "the signature",
            Box::new(move |base| {
                vec![
                    (
                        "/releases/latest".to_owned(),
                        json_reply(&listing(base, Some(1000))),
                    ),
                    (
                        "/dl/sha256sums.txt".to_owned(),
                        Route::Reply(ok_reply(sums_line.len(), sums_line.as_bytes())),
                    ),
                    ("/dl/sha256sums.txt.minisig".to_owned(), Route::Stall),
                ]
            }),
        ),
    ];
    for (case, make) in cases {
        let (base, seen) = serve_paths(make);
        let (lines, took, sent) = run_whole_check(&base, Duration::from_millis(500));
        assert_one_failure_line(&lines, case);
        assert!(!sent, "{case}: a failed check sends no event");
        assert!(
            took < Duration::from_secs(4),
            "{case}: the 500 ms bound must end the check, took {took:?} ({:?})",
            seen.lock().unwrap()
        );
    }
}

/// A listing past `MAX_ASSET_BYTES` fails the check before any request but the
/// listing's own: no sums file, no signature, no asset.
#[test]
fn an_asset_listed_past_the_ceiling_fails_before_any_other_request() {
    let _home = crate::testutil::HomeSandbox::new();
    let (base, seen) = serve_paths(|base| {
        vec![(
            "/releases/latest".to_owned(),
            json_reply(&listing(base, Some(MAX_ASSET_BYTES + 1))),
        )]
    });
    let (lines, _, sent) = run_whole_check(&base, Duration::from_secs(5));
    assert_one_failure_line(&lines, "past the ceiling");
    assert!(!sent);
    assert_eq!(*seen.lock().unwrap(), vec!["/releases/latest".to_owned()]);
}

/// The ceiling is inclusive: an asset listed at exactly `MAX_ASSET_BYTES` goes
/// on to fetch its sums file (stalled here, so the check still fails, after
/// that request).
#[test]
fn an_asset_listed_at_the_ceiling_goes_on_to_its_sums() {
    let _home = crate::testutil::HomeSandbox::new();
    let (base, seen) = serve_paths(|base| {
        vec![
            (
                "/releases/latest".to_owned(),
                json_reply(&listing(base, Some(MAX_ASSET_BYTES))),
            ),
            ("/dl/sha256sums.txt".to_owned(), Route::Stall),
        ]
    });
    let (lines, _, _) = run_whole_check(&base, Duration::from_millis(500));
    assert_one_failure_line(&lines, "at the ceiling");
    assert_eq!(
        *seen.lock().unwrap(),
        vec![
            "/releases/latest".to_owned(),
            "/dl/sha256sums.txt".to_owned()
        ]
    );
}

/// `size` is required: a listing whose asset carries none fails the parse and
/// logs, never reading as 0.
#[test]
fn a_listing_without_a_size_fails_the_check() {
    let _home = crate::testutil::HomeSandbox::new();
    let (base, seen) = serve_paths(|base| {
        vec![(
            "/releases/latest".to_owned(),
            json_reply(&listing(base, None)),
        )]
    });
    let (lines, _, sent) = run_whole_check(&base, Duration::from_secs(5));
    assert_one_failure_line(&lines, "no size");
    assert!(!sent);
    assert_eq!(*seen.lock().unwrap(), vec!["/releases/latest".to_owned()]);
}

/// A newer release listing no binary for this platform is a failure (cloudy,
/// 2026-10-23): one line, no further request, no event.
#[test]
fn a_newer_release_without_this_platforms_asset_logs_one_line() {
    let _home = crate::testutil::HomeSandbox::new();
    let (base, seen) = serve_paths(|_| {
        vec![(
            "/releases/latest".to_owned(),
            json_reply("{\"tag_name\":\"v0.999.0\",\"assets\":[]}"),
        )]
    });
    let (lines, _, sent) = run_whole_check(&base, Duration::from_secs(5));
    assert_one_failure_line(&lines, "no asset");
    assert!(!sent);
    assert_eq!(*seen.lock().unwrap(), vec!["/releases/latest".to_owned()]);
}

/// A failed listing fetch (a 500 here) writes one line and sends no event.
#[test]
fn a_failed_update_check_logs_one_line() {
    let _home = crate::testutil::HomeSandbox::new();
    let (base, _) = serve_paths(|_| {
        vec![(
            "/releases/latest".to_owned(),
            Route::Reply(
                b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    .to_vec(),
            ),
        )]
    });
    let (lines, _, sent) = run_whole_check(&base, Duration::from_secs(5));
    assert_one_failure_line(&lines, "a 500 listing");
    assert!(!sent, "a failed check sends no event");
}

/// A check that finds nothing newer is no failure and writes nothing.
#[test]
fn an_up_to_date_check_logs_nothing() {
    let _home = crate::testutil::HomeSandbox::new();
    let (base, seen) = serve_paths(|_| {
        vec![(
            "/releases/latest".to_owned(),
            json_reply("{\"tag_name\":\"v0.0.1\",\"assets\":[]}"),
        )]
    });
    let (lines, _, sent) = run_whole_check(&base, Duration::from_secs(5));
    assert_eq!(lines, Vec::<String>::new());
    assert!(!sent, "nothing newer sends no event");
    assert_eq!(*seen.lock().unwrap(), vec!["/releases/latest".to_owned()]);
}

/// A test build's update thread ends before any request: the listing URL is
/// pointed at a listener that accepts nothing, so a request would queue there
/// and `accept` would hand it back; `WouldBlock` proves none was made.
#[test]
fn a_test_build_update_thread_never_reaches_the_network() {
    let home = crate::testutil::HomeSandbox::new();
    let _env = crate::testutil::EnvPin::new(&home, &[("CLAUTH_NO_UPDATE", None)]);
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let _api = ApiUrlSandbox::new(
        &home,
        &format!("http://{}/releases/latest", listener.local_addr().unwrap()),
    );
    let (tx, rx) = std::sync::mpsc::channel();
    let thread = spawn(tx, true).expect("a saved-on toggle spawns the thread");
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while !thread.handle.is_finished() && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(thread.handle.is_finished(), "the thread ends at once");
    assert_eq!(
        listener.accept().map(|_| ()).map_err(|e| e.kind()),
        Err(std::io::ErrorKind::WouldBlock),
        "a test build's check makes no request"
    );
    assert_eq!(
        rx.try_recv().err(),
        Some(std::sync::mpsc::TryRecvError::Disconnected),
        "the thread ended and sent nothing"
    );
}

// ── staging and the replace step ───────────────────────────────────────────

/// The verified bytes are staged in a fresh private directory per update, so
/// two clauth processes updating at once never write, truncate or read each
/// other's file between the sha256 check and the self-replace's copy.
#[test]
fn verified_bytes_stage_in_a_private_dir_per_update() {
    let (dir_a, file_a) = stage_verified(b"first binary").unwrap();
    let (dir_b, file_b) = stage_verified(b"second binary").unwrap();
    assert_ne!(dir_a.path(), dir_b.path(), "each update gets its own dir");
    assert!(file_a.starts_with(dir_a.path()) && file_b.starts_with(dir_b.path()));
    assert_eq!(std::fs::read(&file_a).unwrap(), b"first binary");
    assert_eq!(std::fs::read(&file_b).unwrap(), b"second binary");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(dir_a.path()), 0o700, "no other user can open the dir");
        assert_eq!(mode(&file_a), 0o755, "the staged binary is executable");
    }
    let gone = dir_a.path().to_path_buf();
    drop(dir_a);
    assert!(!gone.exists(), "the staging dir goes with its guard");
}

/// The quit's wait detaches a running check after `detach_after`, but never
/// while the replace step runs: an exit there would strand the self-replace's
/// temp file (up to 256 MiB) beside the user's binary.
#[test]
fn the_exit_wait_outlasts_a_running_replace_but_not_a_download() {
    let make = |replace_for: Duration| {
        let gate = std::sync::Arc::new(ReplaceGate::new());
        let shared = std::sync::Arc::clone(&gate);
        let handle = std::thread::spawn(move || {
            if !replace_for.is_zero() {
                let _replace = shared.begin().expect("nothing gave up yet");
                std::thread::sleep(replace_for);
            }
            std::thread::sleep(Duration::from_secs(10));
        });
        while !replace_for.is_zero() && !gate.is_replacing() {
            std::thread::sleep(Duration::from_millis(5));
        }
        (
            UpdateThread {
                handle,
                gate: std::sync::Arc::clone(&gate),
            },
            gate,
        )
    };

    let (thread, _) = make(Duration::from_millis(1500));
    let started = std::time::Instant::now();
    thread.wait_on_exit(Duration::from_millis(100));
    let took = started.elapsed();
    assert!(
        took >= Duration::from_millis(1300) && took < Duration::from_secs(5),
        "a running replace holds the exit until it ends: {took:?}"
    );

    let (thread, gate) = make(Duration::ZERO);
    let started = std::time::Instant::now();
    thread.wait_on_exit(Duration::from_millis(100));
    let took = started.elapsed();
    assert!(
        took < Duration::from_secs(2),
        "a running download detaches at the wait: {took:?}"
    );
    assert!(
        gate.begin().is_none(),
        "once the wait detached, the check can no longer start a replace"
    );
}

/// The gate between the exit wait and the replace step: once the wait gives
/// up, a replace that has not started never starts (an exit would cut it
/// short); while a replace runs, the wait cannot give up; after it ends, it can.
#[test]
fn the_replace_gate_refuses_a_replace_after_the_exit_gave_up() {
    let gate = ReplaceGate::new();
    assert!(gate.abandon(), "an idle gate gives up at once");
    assert!(
        gate.begin().is_none(),
        "no replace starts after the give-up"
    );
    assert!(gate.abandon(), "giving up again stays given up");

    let gate = ReplaceGate::new();
    let replace = gate.begin().expect("an idle gate lets a replace start");
    assert!(gate.is_replacing());
    assert!(!gate.abandon(), "the wait cannot give up mid-replace");
    drop(replace);
    assert!(!gate.is_replacing());
    assert!(
        gate.abandon(),
        "after the replace ends the wait can give up"
    );
    assert!(gate.begin().is_none());
}

// ── the replace step ────────────────────────────────────────────────────────

/// An abandoned gate refuses the replace step without touching the bytes: no
/// replace starts once the exit wait has given up.
#[test]
fn the_replace_step_refuses_an_abandoned_gate() {
    let gate = ReplaceGate::new();
    gate.abandon();
    assert!(
        !replace_verified(b"payload", &gate, |_| Ok(())).expect("the step answers"),
        "no replace starts after the give-up"
    );
}

/// A failed replace fails the whole step: the error passes through unchanged,
/// the self-update mark clears, and the gate returns to idle.
#[test]
fn the_injected_replace_failure_fails_the_replace_step() {
    let _home = crate::testutil::HomeSandbox::new();
    reset_self_update_mark_for_test();
    let _clear = ClearMarkAtEnd;
    let gate = ReplaceGate::new();
    let err = replace_verified(b"payload", &gate, |_| {
        Err(std::io::Error::other("rename refused"))
    })
    .expect_err("the failed replace fails the step");
    assert_eq!(
        err.to_string(),
        "rename refused",
        "the error passes through"
    );
    assert!(!replaced_by_self_update(), "the mark clears on the failure");
    assert!(!gate.is_replacing(), "the gate returns to idle");
}

/// A successful replace marks the run as self-replaced from before the call,
/// holds the gate while it runs, and returns both to their rest states after.
#[test]
fn the_injected_replace_success_marks_the_run() {
    let _home = crate::testutil::HomeSandbox::new();
    reset_self_update_mark_for_test();
    let _clear = ClearMarkAtEnd;
    let gate = ReplaceGate::new();
    let mut seen_during = None;
    let replaced = replace_verified(b"payload", &gate, |staged| {
        assert!(staged.is_file(), "the staged file exists at the call");
        seen_during = Some((replaced_by_self_update(), gate.is_replacing()));
        Ok(())
    })
    .expect("the replace succeeds");
    assert!(replaced, "the step reports the replace");
    assert_eq!(
        seen_during,
        Some((true, true)),
        "the mark is set and the gate held while the call runs"
    );
    assert!(replaced_by_self_update(), "the mark stays after success");
    assert!(!gate.is_replacing(), "the gate returns to idle");
}

// ── replace_marked ─────────────────────────────────────────────────────────

/// Clears the process-global self-update mark when a test ends, failing or
/// not, so a red mark test cannot leave later tests in the same process
/// reading a set mark. Stores directly: a fixture's `Drop` never asserts.
/// Bind it after the `HomeSandbox`, so it drops while `HOME_TEST_LOCK` is
/// still held.
struct ClearMarkAtEnd;

impl Drop for ClearMarkAtEnd {
    fn drop(&mut self) {
        REPLACED_BY_SELF_UPDATE.store(false, Ordering::SeqCst);
    }
}

/// The replace step marks the replacement as this process's own before the
/// replace call runs (a probe during the call already reads it) and keeps the
/// mark once the call succeeds.
#[test]
fn a_successful_replace_is_marked_from_before_the_call() {
    let _home = crate::testutil::HomeSandbox::new();
    reset_self_update_mark_for_test();
    let _clear = ClearMarkAtEnd;
    assert!(!replaced_by_self_update(), "no replace yet, no mark");

    let mut seen_during = None;
    replace_marked(std::path::Path::new("staged"), |staged| {
        assert_eq!(
            staged,
            std::path::Path::new("staged"),
            "the staged path passes through"
        );
        seen_during = Some(replaced_by_self_update());
        Ok(())
    })
    .expect("the replace succeeds");

    assert_eq!(
        seen_during,
        Some(true),
        "the mark is set before the call runs"
    );
    assert!(replaced_by_self_update(), "and stays after it succeeds");
}

/// A failed replace has not put the update's binary in place, so the mark
/// clears and the error reaches the caller unchanged.
#[test]
fn a_failed_replace_clears_the_mark() {
    let _home = crate::testutil::HomeSandbox::new();
    reset_self_update_mark_for_test();
    let _clear = ClearMarkAtEnd;

    let mut seen_during = None;
    let err = replace_marked(std::path::Path::new("staged"), |_| {
        seen_during = Some(replaced_by_self_update());
        Err(std::io::Error::other("rename refused"))
    })
    .expect_err("the replace fails");

    assert_eq!(
        err.to_string(),
        "rename refused",
        "the error passes through"
    );
    assert_eq!(seen_during, Some(true), "the mark was set during the call");
    assert!(!replaced_by_self_update(), "and clears once it fails");
}

/// A replace call that panics clears the mark as it unwinds: a mark left set
/// would silence the changed-binary toasts for the rest of the run.
#[test]
fn a_panicking_replace_clears_the_mark() {
    let _home = crate::testutil::HomeSandbox::new();
    reset_self_update_mark_for_test();
    let _clear = ClearMarkAtEnd;

    let unwound = std::panic::catch_unwind(|| {
        let _ = replace_marked(std::path::Path::new("staged"), |_| -> std::io::Result<()> {
            panic!("the replace call panicked")
        });
    });

    assert!(unwound.is_err(), "the panic propagates");
    assert!(
        !replaced_by_self_update(),
        "and the mark clears as it unwinds"
    );
}

// ── the 0.19.0 updater: major gate, once-per-version lines, hourly loop ─────

/// A throwaway minisign key pair made for these tests (`minisign -G -W`, its
/// secret key deleted after signing): this key authenticates
/// [`FIXTURE_SUMS`] through [`FIXTURE_SIG`], so a whole check can install
/// without the pinned release key.
const FIXTURE_PUBKEY: &str = "RWQruHmINY+pA/OHCzu0kAzJY9ztjuOqdUoKkwHtVaplRRHs6C6YV9n1";

/// The release asset every fixture listing serves; its SHA-256 is the hex in
/// [`FIXTURE_SUMS`] (`sha256sum`, hand-run).
const FIXTURE_ASSET: &[u8] = b"clauth 0.19.1 test binary";

/// One sums line per shipped platform asset, all over [`FIXTURE_ASSET`], so
/// one signature serves every OS the suite runs on.
const FIXTURE_SUMS: &str = "\
713ad070c37a7c00affb754f91ba9591949c2023decff4053eea29c41214d61b  clauth-linux-x86_64
713ad070c37a7c00affb754f91ba9591949c2023decff4053eea29c41214d61b  clauth-macos-x86_64
713ad070c37a7c00affb754f91ba9591949c2023decff4053eea29c41214d61b  clauth-macos-aarch64
713ad070c37a7c00affb754f91ba9591949c2023decff4053eea29c41214d61b  clauth-windows-x86_64.exe
";

/// `minisign -S` over [`FIXTURE_SUMS`] with the [`FIXTURE_PUBKEY`] pair.
const FIXTURE_SIG: &str = "untrusted comment: signature from minisign secret key
RUQruHmINY+pA+JcylFATi3yCEVoJlPODZADg2rzZrjz9kEGwhQB0i4jVXzKj7CG0mSW4JN5Ky+BX4F9FTupitGHfIOyfH6xeAw=
trusted comment: clauth updater test fixture
el1IOzUf+nYVIwSutDWbAWU6iO++JJZnXCCikBwQokAXLUkdVNd6pIxLoCUrf2DFYB58/Kkednn7rPo9eUvDBA==
";

/// The fixture pair verifies on its own, so a check that refuses it fails for
/// the reason its test names, never for a bad fixture.
#[test]
fn the_fixture_signature_verifies_the_fixture_sums() {
    verify_minisign(FIXTURE_PUBKEY, FIXTURE_SUMS.as_bytes(), FIXTURE_SIG)
        .expect("the fixture pair verifies");
    assert_eq!(
        find_expected_sha(FIXTURE_SUMS, platform_asset()).as_deref(),
        Some(sha256_hex(FIXTURE_ASSET).as_str())
    );
}

/// Every route a release tagged `tag` needs for a whole install: the listing,
/// the sums file, its signature and this platform's asset, so a check that
/// downloads finds every request answered.
fn release_routes(base: &str, tag: &str) -> Vec<(String, Route)> {
    let asset = platform_asset();
    let listing = format!(
        "{{\"tag_name\":\"{tag}\",\"assets\":[{{\"name\":\"{asset}\",\"browser_download_url\":\"{base}/dl/{asset}\",\"size\":{}}}]}}",
        FIXTURE_ASSET.len()
    );
    vec![
        ("/releases/latest".to_owned(), json_reply(&listing)),
        (
            "/dl/sha256sums.txt".to_owned(),
            Route::Reply(ok_reply(FIXTURE_SUMS.len(), FIXTURE_SUMS.as_bytes())),
        ),
        (
            "/dl/sha256sums.txt.minisig".to_owned(),
            Route::Reply(ok_reply(FIXTURE_SIG.len(), FIXTURE_SIG.as_bytes())),
        ),
        (
            format!("/dl/{asset}"),
            Route::Reply(ok_reply(FIXTURE_ASSET.len(), FIXTURE_ASSET)),
        ),
    ]
}

/// An updater running as `current` that trusts the fixture key and counts its
/// replace calls in `replaced`, never touching the test binary.
fn fixture_updater(
    current: &'static str,
    can_self_install: bool,
    replaced: &std::sync::Arc<std::sync::atomic::AtomicUsize>,
) -> Updater {
    let count = std::sync::Arc::clone(replaced);
    Updater {
        current,
        public_key: FIXTURE_PUBKEY,
        can_self_install,
        replace: Box::new(move |_| {
            count.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }),
        ..Updater::new()
    }
}

/// One event as a comparable word: `UpdateEvent` carries no `PartialEq`.
fn describe(event: &UpdateEvent) -> String {
    match event {
        UpdateEvent::Installed(v) => format!("installed {v}"),
        UpdateEvent::Available(v) => format!("available {v}"),
        UpdateEvent::MajorAvailable(v) => format!("major {v}"),
    }
}

/// Runs `checks` checks of one `updater` against `<base>/releases/latest` on a
/// thread with a log capture, and hands the updater back with the captured
/// lines and every event sent, in order.
fn run_checks(updater: Updater, base: &str, checks: usize) -> (Updater, Vec<String>, Vec<String>) {
    let url = format!("{base}/releases/latest");
    ends_within(Duration::from_secs(30), move || {
        let mut updater = updater;
        let lines = crate::logline::LogLines::new();
        let (tx, rx) = std::sync::mpsc::channel();
        let gate = ReplaceGate::new();
        let agent = metadata_agent(Duration::from_secs(5));
        {
            let _guard = lines.capture_here();
            for _ in 0..checks {
                updater.check(&tx, &url, &agent, &gate);
            }
        }
        let events = rx.try_iter().map(|e| describe(&e)).collect();
        (updater, lines.snapshot(), events)
    })
}

/// Runs one check of `updater` on the CALLING thread, with a log capture
/// installed there: the plugin-host self-exe seam is per-thread, so a test
/// that pins it cannot hand its check to [`run_checks`]'s thread.
#[cfg(unix)]
fn run_one_check_here(updater: &mut Updater, base: &str) -> (Vec<String>, Vec<String>) {
    let url = format!("{base}/releases/latest");
    let lines = crate::logline::LogLines::new();
    let (tx, rx) = std::sync::mpsc::channel();
    let gate = ReplaceGate::new();
    let agent = metadata_agent(Duration::from_secs(5));
    {
        let _guard = lines.capture_here();
        updater.check(&tx, &url, &agent, &gate);
    }
    let events = rx.try_iter().map(|e| describe(&e)).collect();
    (lines.snapshot(), events)
}

/// The line a higher major release is told by.
const MAJOR_LINE: &str = "v1.0.0 is out and is not installed automatically: read its release note, then update by hand (https://github.com/uwuclxdy/clauth/releases/tag/v1.0.0)";

/// A release whose major version is higher than the running binary's is never
/// downloaded, on a build that can self-install and on a cargo build alike:
/// the listing is the only request, the approved line is logged once per
/// version, and one `MajorAvailable` event carries the version.
#[test]
fn a_higher_major_release_downloads_nothing_and_says_so_once() {
    let _home = crate::testutil::HomeSandbox::new();
    reset_self_update_mark_for_test();
    let _clear = ClearMarkAtEnd;
    for can_self_install in [true, false] {
        let case = format!("can_self_install = {can_self_install}");
        let replaced = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (base, seen) = serve_paths(|base| release_routes(base, "v1.0.0"));
        let updater = fixture_updater("0.19.0", can_self_install, &replaced);
        let (_, lines, events) = run_checks(updater, &base, 2);
        assert_eq!(
            *seen.lock().unwrap(),
            vec!["/releases/latest".to_owned(), "/releases/latest".to_owned()],
            "{case}: each check reads the listing and nothing else"
        );
        assert_eq!(lines, vec![MAJOR_LINE.to_owned()], "{case}: one line");
        assert_eq!(events, vec!["major 1.0.0".to_owned()], "{case}: one event");
        assert_eq!(replaced.load(Ordering::SeqCst), 0, "{case}: no replace");
    }
}

/// A higher major tagged as a prerelease the full version parse rejects is
/// still a major release: told once per version by the tag as the listing
/// spells it, and downloaded never.
#[test]
fn a_prerelease_of_a_higher_major_is_told_once_with_the_tag_as_listed() {
    let _home = crate::testutil::HomeSandbox::new();
    reset_self_update_mark_for_test();
    let _clear = ClearMarkAtEnd;
    let replaced = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let (base, seen) = serve_paths(|base| release_routes(base, "v1.0.0-rc.1"));
    let updater = fixture_updater("0.19.0", true, &replaced);
    let (_, lines, events) = run_checks(updater, &base, 2);
    assert_eq!(
        *seen.lock().unwrap(),
        vec!["/releases/latest".to_owned(), "/releases/latest".to_owned()],
        "each check reads the listing and nothing else"
    );
    assert_eq!(
        lines,
        vec![
            "v1.0.0-rc.1 is out and is not installed automatically: read its release note, \
             then update by hand (https://github.com/uwuclxdy/clauth/releases/tag/v1.0.0-rc.1)"
                .to_owned()
        ],
        "one line, naming the tag as listed"
    );
    assert_eq!(events, vec!["major 1.0.0-rc.1".to_owned()], "one event");
    assert_eq!(replaced.load(Ordering::SeqCst), 0, "no replace");
}

/// A newer same-major release installs through the verified path once: the
/// second check, in a process whose own binary the first one replaced,
/// downloads nothing and says nothing more.
#[test]
fn a_same_major_release_installs_once_and_says_so() {
    let _home = crate::testutil::HomeSandbox::new();
    reset_self_update_mark_for_test();
    let _clear = ClearMarkAtEnd;
    let replaced = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let (base, seen) = serve_paths(|base| release_routes(base, "v0.19.1"));
    let updater = fixture_updater("0.19.0", true, &replaced);
    let (_, lines, events) = run_checks(updater, &base, 2);
    assert_eq!(
        *seen.lock().unwrap(),
        vec![
            "/releases/latest".to_owned(),
            "/dl/sha256sums.txt".to_owned(),
            "/dl/sha256sums.txt.minisig".to_owned(),
            format!("/dl/{}", platform_asset()),
            "/releases/latest".to_owned(),
        ]
    );
    assert_eq!(replaced.load(Ordering::SeqCst), 1, "one replace");
    assert_eq!(lines, vec!["updated to v0.19.1".to_owned()]);
    assert_eq!(events, vec!["installed 0.19.1".to_owned()]);
}

/// A process whose binary on disk was replaced since its start (here posed as
/// another process's install) installs nothing more until it restarts: a newer
/// same-major release is neither downloaded nor announced, while a higher
/// major is still told.
#[test]
fn a_process_whose_binary_was_replaced_installs_nothing() {
    let _home = crate::testutil::HomeSandbox::new();
    reset_self_update_mark_for_test();
    let _clear = ClearMarkAtEnd;
    let replaced = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));

    let (base, seen) = serve_paths(|base| release_routes(base, "v0.19.1"));
    let updater = Updater {
        binary_changed: Box::new(|| true),
        ..fixture_updater("0.19.0", true, &replaced)
    };
    let (updater, lines, events) = run_checks(updater, &base, 1);
    assert_eq!(*seen.lock().unwrap(), vec!["/releases/latest".to_owned()]);
    assert_eq!(lines, Vec::<String>::new());
    assert_eq!(events, Vec::<String>::new());

    let (base, _) = serve_paths(|base| release_routes(base, "v1.0.0"));
    let (_, lines, events) = run_checks(updater, &base, 1);
    assert_eq!(lines, vec![MAJOR_LINE.to_owned()]);
    assert_eq!(events, vec!["major 1.0.0".to_owned()]);
    assert_eq!(replaced.load(Ordering::SeqCst), 0, "no replace");
}

/// A binary that changes on disk after the download began installs nothing:
/// the changed-binary rule is read again after the hash check, so bytes
/// fetched before another process's install never replace its binary.
#[test]
fn a_binary_that_changes_during_the_download_installs_nothing() {
    let _home = crate::testutil::HomeSandbox::new();
    reset_self_update_mark_for_test();
    let _clear = ClearMarkAtEnd;
    let replaced = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let reads = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counted = std::sync::Arc::clone(&reads);
    let (base, seen) = serve_paths(|base| release_routes(base, "v0.19.1"));
    let updater = Updater {
        binary_changed: Box::new(move || counted.fetch_add(1, Ordering::SeqCst) > 0),
        ..fixture_updater("0.19.0", true, &replaced)
    };
    let (_, lines, events) = run_checks(updater, &base, 1);
    assert_eq!(
        *seen.lock().unwrap(),
        vec![
            "/releases/latest".to_owned(),
            "/dl/sha256sums.txt".to_owned(),
            "/dl/sha256sums.txt.minisig".to_owned(),
            format!("/dl/{}", platform_asset()),
        ],
        "the release is downloaded whole before the second read refuses the replace"
    );
    assert_eq!(replaced.load(Ordering::SeqCst), 0, "no replace");
    assert_eq!(lines, Vec::<String>::new(), "no line");
    assert_eq!(events, Vec::<String>::new(), "no event");
    assert_eq!(
        reads.load(Ordering::SeqCst),
        2,
        "the rule is read before the download and again after the hash check"
    );
}

/// A process whose own binary on disk changed under it (something other than
/// its own self-update) installs nothing more: `Updater::new()`'s own
/// disk-change predicate, driven through the plugin-host seam with a real
/// replace of the recorded file.
#[cfg(unix)]
#[test]
fn a_binary_replaced_on_disk_installs_nothing() {
    let home = crate::testutil::HomeSandbox::new();
    reset_self_update_mark_for_test();
    let _clear = ClearMarkAtEnd;
    let recorded = crate::testutil::write_shim(home.home(), "self-exe-recorded", "exit 0");
    let _self = crate::plugin_host::SelfExeOverride::pin(Some(recorded.as_path()));
    let replacement =
        crate::testutil::write_shim(home.home(), "self-exe-replacement", "exit 0 # replaced");
    std::fs::rename(&replacement, &recorded).expect("rename-replace the recorded file");

    let replaced = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let (base, seen) = serve_paths(|base| release_routes(base, "v0.19.1"));
    let mut updater = fixture_updater("0.19.0", true, &replaced);
    let (lines, events) = run_one_check_here(&mut updater, &base);
    assert_eq!(*seen.lock().unwrap(), vec!["/releases/latest".to_owned()]);
    assert_eq!(replaced.load(Ordering::SeqCst), 0, "no replace");
    assert_eq!(lines, Vec::<String>::new(), "no line");
    assert_eq!(events, Vec::<String>::new(), "no event");
}

/// A build that cannot self-install (a cargo build, or no prebuilt asset) logs
/// `update available` once per version: a repeat check of the same version is
/// silent, a newer one is told again.
#[test]
fn a_build_that_cannot_self_install_says_update_available_once_per_version() {
    let _home = crate::testutil::HomeSandbox::new();
    reset_self_update_mark_for_test();
    let _clear = ClearMarkAtEnd;
    let replaced = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));

    let (base, seen) = serve_paths(|base| release_routes(base, "v0.19.1"));
    let updater = fixture_updater("0.19.0", false, &replaced);
    let (updater, lines, events) = run_checks(updater, &base, 2);
    assert_eq!(
        *seen.lock().unwrap(),
        vec!["/releases/latest".to_owned(), "/releases/latest".to_owned()],
        "nothing is downloaded"
    );
    assert_eq!(lines, vec!["update available: v0.19.1".to_owned()]);
    assert_eq!(events, vec!["available 0.19.1".to_owned()]);

    let (base, _) = serve_paths(|base| release_routes(base, "v0.19.2"));
    let (_, lines, events) = run_checks(updater, &base, 1);
    assert_eq!(lines, vec!["update available: v0.19.2".to_owned()]);
    assert_eq!(events, vec!["available 0.19.2".to_owned()]);
    assert_eq!(replaced.load(Ordering::SeqCst), 0, "no replace");
}

/// The loop every daemon and TUI runs: a check at its start and after each
/// hour's wait (the wait is injected, so no test sleeps an hour), skipped
/// while the toggle reads off, which sends no request.
#[test]
fn the_loop_checks_at_start_and_after_each_hour_while_enabled() {
    let _home = crate::testutil::HomeSandbox::new();
    let (base, seen) = serve_paths(|_| {
        vec![(
            "/releases/latest".to_owned(),
            json_reply("{\"tag_name\":\"v0.0.1\",\"assets\":[]}"),
        )]
    });
    let url = format!("{base}/releases/latest");
    let waits = ends_within(Duration::from_secs(30), move || {
        let replaced = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut updater = fixture_updater("0.19.0", true, &replaced);
        let (tx, _rx) = std::sync::mpsc::channel();
        let gate = ReplaceGate::new();
        let agent = metadata_agent(Duration::from_secs(5));
        let mut enabled = [true, false, true].into_iter();
        let mut waits = Vec::new();
        run_loop(
            &mut updater,
            &tx,
            &url,
            &agent,
            &gate,
            || enabled.next().expect("one toggle read per round"),
            |interval| {
                waits.push(interval);
                waits.len() < 3
            },
        );
        waits
    });
    assert_eq!(waits, vec![Duration::from_secs(3600); 3]);
    assert_eq!(
        *seen.lock().unwrap(),
        vec!["/releases/latest".to_owned(), "/releases/latest".to_owned()],
        "the round read off sends no request"
    );
}

/// The loop's toggle is read fresh each round from the saved `[update]`
/// table and `CLAUTH_NO_UPDATE`, so turning `auto-update` off stops a running
/// daemon's or TUI's next check.
#[test]
fn the_loop_toggle_reads_the_saved_setting_and_the_env_fresh() {
    let home = crate::testutil::HomeSandbox::new();
    {
        let _env = crate::testutil::EnvPin::new(&home, &[("CLAUTH_NO_UPDATE", None)]);
        assert!(updates_enabled_now(), "nothing saved reads on");
        let mut state = crate::profile::load_app_state().expect("load state");
        state.update.auto_update = false;
        crate::profile::save_app_state(&state).expect("save off");
        assert!(!updates_enabled_now(), "saved off reads off");
        state.update.auto_update = true;
        crate::profile::save_app_state(&state).expect("save on");
        assert!(updates_enabled_now(), "saved on again reads on");
    }
    let _env = crate::testutil::EnvPin::new(
        &home,
        &[("CLAUTH_NO_UPDATE", Some(std::ffi::OsStr::new("1")))],
    );
    assert!(!updates_enabled_now(), "CLAUTH_NO_UPDATE=1 reads off");
}

/// The loop `spawn`'s thread runs composes the saved toggle and the gate's
/// stop-aware wait: a stopped gate gives one round per call, and the round
/// reads the toggle from disk, so an off round sends no request at all.
#[test]
fn the_hourly_loop_reads_the_saved_toggle_through_the_production_composition() {
    let home = crate::testutil::HomeSandbox::new();
    let _env = crate::testutil::EnvPin::new(&home, &[("CLAUTH_NO_UPDATE", None)]);
    let replaced = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let (base, seen) = serve_paths(|_| {
        vec![(
            "/releases/latest".to_owned(),
            json_reply("{\"tag_name\":\"v0.0.1\",\"assets\":[]}"),
        )]
    });
    let url = format!("{base}/releases/latest");
    let agent = metadata_agent(Duration::from_secs(5));
    let gate = ReplaceGate::new();
    gate.stop();
    let (tx, _rx) = std::sync::mpsc::channel();

    let mut state = crate::profile::load_app_state().expect("load state");
    state.update.auto_update = false;
    crate::profile::save_app_state(&state).expect("save off");
    run_hourly(
        &mut fixture_updater("0.19.0", true, &replaced),
        &tx,
        &url,
        &agent,
        &gate,
    );
    assert_eq!(
        *seen.lock().unwrap(),
        Vec::<String>::new(),
        "the round read off sends no request"
    );

    state.update.auto_update = true;
    crate::profile::save_app_state(&state).expect("save on");
    run_hourly(
        &mut fixture_updater("0.19.0", true, &replaced),
        &tx,
        &url,
        &agent,
        &gate,
    );
    assert_eq!(
        *seen.lock().unwrap(),
        vec!["/releases/latest".to_owned()],
        "the round read on reads the listing"
    );
    assert_eq!(replaced.load(Ordering::SeqCst), 0, "no release to install");

    // The wait half: an unstopped gate sits out the hour after its round, and
    // the exit wait's stop ends it.
    let gate = Arc::new(ReplaceGate::new());
    let shared = Arc::clone(&gate);
    let handle = std::thread::spawn(move || {
        let (tx, _rx) = std::sync::mpsc::channel();
        run_hourly(
            &mut fixture_updater("0.19.0", true, &replaced),
            &tx,
            &url,
            &agent,
            &shared,
        );
    });
    let deadline = Instant::now() + Duration::from_secs(5);
    while seen.lock().unwrap().len() < 2 {
        assert!(
            Instant::now() < deadline,
            "the round never read the listing"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    // Only a false green can come of this settle: a loop that ends after its
    // round has finished long before it passes.
    std::thread::sleep(Duration::from_millis(300));
    assert!(!handle.is_finished(), "the loop ended after one round");
    let started = Instant::now();
    UpdateThread { handle, gate }.wait_on_exit(Duration::from_secs(30));
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "the stop ends the hour's wait"
    );
}

/// An updater that never installs; the loops below never let it check.
fn idle_updater() -> Updater {
    Updater {
        current: "0.19.0",
        public_key: "",
        can_self_install: false,
        binary_changed: Box::new(|| false),
        replace: Box::new(|_| Err(std::io::Error::other("no replace in this test"))),
        ..Updater::new()
    }
}

/// A loop sleeping out its hour between checks ends as soon as the exit wait
/// asks: the quit is held for well under `detach_after`, the thread itself
/// returns, and no round runs after the stop.
#[test]
fn the_exit_wait_ends_a_loop_sleeping_between_checks() {
    let _home = crate::testutil::HomeSandbox::new();
    let gate = Arc::new(ReplaceGate::new());
    let shared = Arc::clone(&gate);
    let rounds = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counted = Arc::clone(&rounds);
    let (asleep_tx, asleep_rx) = std::sync::mpsc::channel();
    let (ended_tx, ended_rx) = std::sync::mpsc::channel();
    let handle = std::thread::spawn(move || {
        let (tx, _rx) = std::sync::mpsc::channel();
        run_loop(
            &mut idle_updater(),
            &tx,
            "http://127.0.0.1:9/releases/latest",
            &metadata_agent(Duration::from_secs(1)),
            &shared,
            || {
                counted.fetch_add(1, Ordering::SeqCst);
                false
            },
            |interval| {
                asleep_tx.send(()).expect("the test holds the receiver");
                shared.sleep_unless_stopped(interval)
            },
        );
        let _ = ended_tx.send(());
    });
    asleep_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("the loop reached its hour's sleep");

    let started = Instant::now();
    UpdateThread { handle, gate }.wait_on_exit(Duration::from_secs(30));
    let took = started.elapsed();
    assert!(
        took < Duration::from_secs(5),
        "a sleeping loop holds the exit: {took:?}"
    );
    assert_eq!(
        ended_rx.recv_timeout(Duration::from_secs(2)),
        Ok(()),
        "the loop itself ends, not only the wait"
    );
    assert_eq!(rounds.load(Ordering::SeqCst), 1, "no round after the stop");
}

/// The production wait sits out its interval when nothing asks it to stop,
/// and returns at once, ending the loop, once a stop was asked.
#[test]
fn the_loop_wait_sits_out_its_interval_unless_stopped() {
    let gate = ReplaceGate::new();
    let started = Instant::now();
    assert!(
        gate.sleep_unless_stopped(Duration::from_millis(200)),
        "an unstopped wait lets the loop go on"
    );
    let took = started.elapsed();
    assert!(
        took >= Duration::from_millis(200),
        "the wait sits out its interval: {took:?}"
    );

    gate.stop();
    let started = Instant::now();
    assert!(
        !gate.sleep_unless_stopped(Duration::from_secs(10)),
        "a stopped wait ends the loop"
    );
    let took = started.elapsed();
    assert!(
        took < Duration::from_secs(1),
        "a stopped wait returns at once: {took:?}"
    );
}

/// The daemon's spawn starts the loop its saved toggle allows: saved on, the
/// loop thread starts; saved off, it does not. The line is the spawn's own
/// verdict, so a stand-in for the `spawn` call, or for the toggle it passes,
/// reds here.
#[test]
fn the_daemon_spawn_starts_the_loop_the_saved_toggle_allows() {
    let home = crate::testutil::HomeSandbox::new();
    let _env = crate::testutil::EnvPin::new(&home, &[("CLAUTH_NO_UPDATE", None)]);
    let lines = crate::logline::LogLines::new();
    {
        let _guard = lines.capture_here();
        spawn_for_daemon();
        let mut state = crate::profile::load_app_state().expect("load state");
        state.update.auto_update = false;
        crate::profile::save_app_state(&state).expect("save off");
        spawn_for_daemon();
    }
    assert_eq!(
        lines.snapshot(),
        vec![
            "the update spawn ran (loop started: true)".to_owned(),
            "the update spawn ran (loop started: false)".to_owned(),
        ]
    );
}
