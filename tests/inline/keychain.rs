//! KC-1 — Keychain read/write/delete round-trip. Uses a **throwaway service name**
//! unique to this process; it never touches the real `Claude Code-credentials`
//! item. The nine `#[ignore]`d tests in this file (KC-1's round-trip and
//! siblings legs, KC-3, KC-4, KC-5, KC-9, KC-10, KC-11, KC-12) are the only ones
//! that drive the real macOS Keychain (via the `/usr/bin/security` CLI the
//! shipped write/delete path uses), so each is `#[ignore]`d: they still mutate
//! the login Keychain (create + delete throwaway items) as a side effect. Run
//! them on demand instead:
//!     cargo test keychain -- --ignored
//! All other credential/divergence tests stay on the file model
//! (`keychain::enabled()` is false under `cfg(test)`), so `cargo test` never
//! touches the Keychain.

use super::{
    Keep, PutTransport, SECURITY_ARGV_VALUE_MAX, SECURITY_BIN, SECURITY_STDIN_LINE_MAX, SecurityOp,
    UnparseableItem, VerifyOutcome, WriteDisposition, add_generic_password_line, carried_raw,
    delete_at, disposition_verdict, keychain_service_for_config_dir, merge_and_put_at, merge_write,
    merged_blob, put_blob_at, put_transport, quarantine_path, quarantine_tail, read_blob_at,
    run_with_deadline, security_deadline, security_error, security_quote, sign_out_at,
    verify_outcome, write_disposition,
};
use crate::logline::LogLines;
use crate::profile::{ClaudeCredentials, OAuthToken};
use crate::testutil::HomeSandbox;
use std::path::{Path, PathBuf};

fn sample_creds(access: &str, refresh: &str) -> ClaudeCredentials {
    ClaudeCredentials {
        claude_ai_oauth: Some(OAuthToken {
            access_token: access.to_string(),
            refresh_token: Some(refresh.to_string()),
            expires_at: Some(1_900_000_000_000),
            scopes: Some(vec![
                "user:inference".to_string(),
                "user:profile".to_string(),
            ]),
            subscription_type: Some("max".to_string()),
        }),
    }
}

/// The login block of whatever the item holds, parsed back into the typed shape.
/// The item itself is a raw object with sibling keys beside the login, so the
/// read stays untyped and only this assertion narrows.
fn read_login(service: &str, account: &str) -> Option<OAuthToken> {
    let blob = read_blob_at(service, account).expect("read")?;
    serde_json::from_value::<ClaudeCredentials>(blob)
        .expect("the item holds a Claude credentials object")
        .claude_ai_oauth
}

/// Write `creds` as the whole item, the way a store file with no siblings would.
fn put_login(service: &str, account: &str, creds: &ClaudeCredentials, keep: Keep) {
    let login = serde_json::to_value(creds).expect("serialize");
    merge_and_put_at(service, account, &login, keep).expect("write");
}

#[test]
#[ignore = "touches the real login Keychain (throwaway service); macOS re-prompts each rebuild — run explicitly with --ignored"]
fn keychain_round_trip_on_temp_service() {
    let service = format!("clauth-test-{}", std::process::id());
    let account = "clauth-test-account";

    // Clean slate — delete is idempotent, read of an absent item is None.
    delete_at(&service, account).expect("pre-clean delete is idempotent");
    assert!(
        read_blob_at(&service, account)
            .expect("read absent")
            .is_none(),
        "temp service should start empty"
    );

    // Write, then read back the same tokens.
    let creds = sample_creds("sk-ant-oat01-TESTACCESS", "sk-ant-ort01-TESTREFRESH");
    put_login(&service, account, &creds, Keep::CarriedOnly);
    let oauth = read_login(&service, account).expect("oauth block round-trips");
    assert_eq!(oauth.access_token, "sk-ant-oat01-TESTACCESS");
    assert_eq!(
        oauth.refresh_token.as_deref(),
        Some("sk-ant-ort01-TESTREFRESH")
    );
    assert_eq!(oauth.subscription_type.as_deref(), Some("max"));

    // add-generic-password -U is add-or-update: a second write replaces in place.
    let updated = sample_creds("sk-ant-oat01-ROTATED", "sk-ant-ort01-ROTATED");
    put_login(&service, account, &updated, Keep::CarriedOnly);
    let rotated = read_login(&service, account).expect("oauth");
    assert_eq!(rotated.access_token, "sk-ant-oat01-ROTATED");

    // Hostile-content write via `security -i`: spaces, double quotes, and
    // backslashes in the secret must round-trip byte-identical through the
    // security_quote escaping (no real token looks like this; the point is
    // that the -i tokenizer can never mangle one that does).
    let hostile = sample_creds(r#"sk with spaces "quoted" back\slash"#, "rt-plain");
    put_login(&service, account, &hostile, Keep::CarriedOnly);
    let echoed = read_login(&service, account).expect("oauth");
    assert_eq!(echoed.access_token, r#"sk with spaces "quoted" back\slash"#);

    // Delete → absent; delete again is still Ok (idempotent).
    delete_at(&service, account).expect("delete");
    assert!(
        read_blob_at(&service, account)
            .expect("read after delete")
            .is_none()
    );
    delete_at(&service, account).expect("second delete idempotent");
}

/// The read-modify-write, end to end against a real Keychain item: the sibling
/// blocks Claude Code parks beside its login survive a write that models the
/// login alone, and which of them survive is the [`Keep`] the caller passed.
/// The pure merge is pinned on every platform through the helpers it routes to
/// (`tests/inline/claude.rs`); what only a Keychain can prove is that the read
/// leg reaches the item that the write leg just replaced.
#[test]
#[ignore = "touches the real login Keychain (throwaway service); macOS re-prompts each rebuild — run explicitly with --ignored"]
fn keychain_write_keeps_the_siblings_its_keep_allows() {
    let service = format!("clauth-test-merge-{}", std::process::id());
    let account = "clauth-test-account";
    delete_at(&service, account).expect("pre-clean delete is idempotent");

    // Claude Code's own item shape: one object, login plus siblings.
    let seeded = serde_json::json!({
        "claudeAiOauth": { "accessToken": "sk-ant-oat01-OUTGOING" },
        "mcpOAuth": { "linear": { "accessToken": "mock-linear" } },
        "organizationUuid": "org-outgoing"
    });
    put_blob_at(&service, account, &seeded).expect("seed");

    // A switch: the incoming account's login, carrying only what belongs to no
    // account. The outgoing org id must not follow the login it was minted with.
    put_login(
        &service,
        account,
        &sample_creds("sk-ant-oat01-INCOMING", "sk-ant-ort01-INCOMING"),
        Keep::CarriedOnly,
    );
    let after_switch = read_blob_at(&service, account)
        .expect("read")
        .expect("present");
    assert_eq!(
        after_switch["claudeAiOauth"]["accessToken"], "sk-ant-oat01-INCOMING",
        "the switch installs the incoming login"
    );
    assert_eq!(
        after_switch["mcpOAuth"]["linear"]["accessToken"], "mock-linear",
        "the MCP-server logins survive the -U replace"
    );
    assert!(
        after_switch.get("organizationUuid").is_none(),
        "the outgoing account's org id must not cross onto another login"
    );

    // A rotation of the same account: everything the item holds survives.
    put_blob_at(&service, account, &seeded).expect("re-seed");
    put_login(
        &service,
        account,
        &sample_creds("sk-ant-oat01-ROTATED", "sk-ant-ort01-ROTATED"),
        Keep::Everything,
    );
    let after_rotation = read_blob_at(&service, account)
        .expect("read")
        .expect("present");
    assert_eq!(
        after_rotation["claudeAiOauth"]["accessToken"], "sk-ant-oat01-ROTATED",
        "the rotation installs the fresh pair"
    );
    assert_eq!(
        after_rotation["organizationUuid"], "org-outgoing",
        "this account's own blocks stay put when only its token moved"
    );

    delete_at(&service, account).expect("delete");
}

// ── The merge itself (pure, no Keychain touched) ──────────────────────────────
//
// These pin WHICH rule each `Keep` routes to. The rules are pinned where they
// live (`claude::carry_live_extra_over`, `profile::preserve_extra_blocks`, both
// covered on every platform), but nothing there can catch the two arms being
// swapped here — and swapping them is exactly the defect this module exists to
// prevent, since `Keep::Everything` on a switch carries the outgoing account's
// org id and device token onto the incoming account's login.

fn item(login: &str) -> serde_json::Value {
    serde_json::json!({
        "claudeAiOauth": { "accessToken": login },
        "mcpOAuth": { "linear": { "accessToken": "mock-linear" } },
        "organizationUuid": "org-outgoing"
    })
}

#[test]
fn merged_blob_carries_only_the_allowlist_onto_a_different_login() {
    let incoming = serde_json::json!({ "claudeAiOauth": { "accessToken": "incoming" } });

    let merged = merged_blob(&incoming, Some(&item("outgoing")), Keep::CarriedOnly);

    assert_eq!(merged["claudeAiOauth"]["accessToken"], "incoming");
    assert_eq!(merged["mcpOAuth"]["linear"]["accessToken"], "mock-linear");
    assert!(
        merged.get("organizationUuid").is_none(),
        "an account-scoped key must not cross onto another account's login"
    );
}

#[test]
fn merged_blob_keeps_everything_when_the_login_is_unchanged() {
    let incoming = serde_json::json!({ "claudeAiOauth": { "accessToken": "same" } });

    let merged = merged_blob(&incoming, Some(&item("same")), Keep::CarriedOnly);

    assert_eq!(
        merged["organizationUuid"], "org-outgoing",
        "a relink installing the login the item already holds cannot have changed account, \
         so its own blocks stay"
    );
    assert_eq!(merged["mcpOAuth"]["linear"]["accessToken"], "mock-linear");
}

/// Claude Code's logged-out shell is a login block with the tokens blanked, and
/// two accounts' shells are equal to each other, so key equality alone would
/// read them as one login and carry the other account's blocks across. The
/// crate draws this line the same way twice already (`classify_link_at`, and the
/// link guard's "two blanks are two logged-out shells, never a match").
#[test]
fn merged_blob_treats_two_logged_out_shells_as_different_logins() {
    let shell = serde_json::json!({ "accessToken": "", "refreshToken": "", "expiresAt": 0 });
    let incoming = serde_json::json!({ "claudeAiOauth": shell });
    let existing = serde_json::json!({
        "claudeAiOauth": shell,
        "organizationUuid": "org-someone-else"
    });

    let merged = merged_blob(&incoming, Some(&existing), Keep::CarriedOnly);

    assert!(
        merged.get("organizationUuid").is_none(),
        "a blank login matching a blank login is two shells, not one account"
    );
}

#[test]
fn merged_blob_under_a_rotation_keeps_the_accounts_own_blocks() {
    let rotated = serde_json::json!({ "claudeAiOauth": { "accessToken": "rotated" } });

    let merged = merged_blob(&rotated, Some(&item("pre-rotation")), Keep::Everything);

    assert_eq!(merged["claudeAiOauth"]["accessToken"], "rotated");
    assert_eq!(merged["organizationUuid"], "org-outgoing");
    assert_eq!(merged["mcpOAuth"]["linear"]["accessToken"], "mock-linear");
}

#[test]
fn merged_blob_over_an_absent_item_is_the_incoming_store() {
    let incoming = serde_json::json!({ "claudeAiOauth": { "accessToken": "first" } });

    for keep in [Keep::CarriedOnly, Keep::Everything] {
        assert_eq!(
            merged_blob(&incoming, None, keep),
            incoming,
            "the first write has nothing to merge with"
        );
    }
}

// ── Whether the merge writes at all (pure, no Keychain touched) ───────────────
//
// The skip is what keeps the daemon's and the TUI's per-tick relink from costing
// a `security` subprocess every tick, against a budget the whole lock hold
// shares. It had no test on any platform: `merged_blob_*` above pin WHAT a write
// would contain, and every one of them is satisfied whether or not the write
// happens.

#[test]
fn merge_write_skips_a_relink_that_reproduces_the_item() {
    // The daemon/TUI steady state: the active profile's own login, already
    // installed, relinked again on a tick.
    let installed = item("live");

    assert_eq!(
        merge_write(&installed, Some(&installed), Keep::CarriedOnly),
        None,
        "a relink installing exactly what the item holds must not spend a write"
    );
    assert_eq!(
        merge_write(&installed, Some(&installed), Keep::Everything),
        None,
        "and a rotation mirror that changed nothing must not either"
    );
}

/// The skip is keyed on the MERGED result, not on the incoming blob, so a store
/// that merely lacks the item's siblings still skips: the carry puts them back
/// and the two compare equal. Nothing here is a write clauth would want, and the
/// naive `incoming != existing` spelling would perform one on every tick.
#[test]
fn merge_write_skips_when_only_the_carry_closes_the_difference() {
    let login_only = serde_json::json!({ "claudeAiOauth": { "accessToken": "live" } });

    assert_eq!(
        merge_write(&login_only, Some(&item("live")), Keep::CarriedOnly),
        None,
        "the item's own siblings are carried back onto an identical login, so \
         the merge reproduces it"
    );
}

#[test]
fn merge_write_writes_whenever_the_merge_changes_the_item() {
    // A rotation: same account, fresh token.
    let rotated = serde_json::json!({ "claudeAiOauth": { "accessToken": "rotated" } });
    assert!(
        merge_write(&rotated, Some(&item("stale")), Keep::Everything).is_some(),
        "a fresh token must reach the item"
    );

    // A switch: a different login, and the outgoing account's org id has to GO.
    // The write is needed for a removal here, which is the case an
    // additions-only assertion would miss.
    let merged =
        merge_write(&rotated, Some(&item("outgoing")), Keep::CarriedOnly).expect("a switch writes");
    assert!(
        merged.get("organizationUuid").is_none(),
        "the write exists to drop the outgoing account's blocks, not only to add"
    );
}

/// A read that FAILED merges as `None` (`blob_to_merge_with` logs and degrades),
/// which is indistinguishable here from an absent item — and both must WRITE.
/// Skipping on a failed read would drop the login as well as the siblings, which
/// is the one outcome the degrade exists to avoid.
#[test]
fn merge_write_always_writes_when_it_could_not_read_the_item() {
    let installed = item("live");

    for keep in [Keep::CarriedOnly, Keep::Everything] {
        assert_eq!(
            merge_write(&installed, None, keep),
            Some(installed.clone()),
            "with nothing to compare against, the incoming store is written whole"
        );
    }
}

// ── TECH-3: `security` subprocess deadline (no Keychain touched) ───────────────
//
// Exercise `run_with_deadline` with benign stand-in commands (`sleep` / `true`)
// so the timeout-and-kill path is proven without a real `/usr/bin/security`
// invocation — these run in `cargo test` (unlike the #[ignore]d round-trip).

#[test]
fn keychain_timeout_kills_a_hung_command() {
    use std::process::Command;
    use std::time::{Duration, Instant};

    let mut cmd = Command::new("/bin/sleep");
    cmd.arg("30");
    let start = Instant::now();
    let result = run_with_deadline(cmd, Duration::from_millis(300), None);
    let elapsed = start.elapsed();

    assert!(
        result.is_err(),
        "a command outrunning its deadline must return an error"
    );
    assert!(
        result.unwrap_err().to_string().contains("deadline"),
        "the error should name the deadline"
    );
    assert!(
        elapsed < Duration::from_secs(3),
        "the child must be killed near the deadline (was {elapsed:?}), not left to run 30s"
    );
}

// ── B2: the pipes drain concurrently with the poll loop (no Keychain touched) ──
//
// `run_with_deadline` used to read the child's piped output only after exit, on
// the premise "security produces only a few bytes of output". Past the ~64 KiB
// pipe buffer that premise is false — the 512 KiB argv cap admits items whose
// every read legs the child into write() blocking: the child never exits, dies
// at the deadline, and the read fails (the merge read as `None` with NO
// quarantine bytes; the verify read as a +10 s stall into `Unverified`). Both
// pipes are now drained by reader threads started before the poll loop.

/// A child that writes more than the pipe buffer (~64 KiB) to stdout and exits
/// must complete with ALL its bytes, not die at the deadline on a full pipe.
/// Pre-drain, this exact child blocked on `write(2)`, never exited, and was
/// killed at the deadline — the B2 defect.
#[test]
fn a_child_pasting_the_stdout_pipe_buffer_completes_with_all_its_bytes() {
    use std::process::Command;
    use std::time::Duration;

    let mut cmd = Command::new("/bin/sh");
    cmd.args(["-c", "head -c 100000 /dev/zero"]);
    let out = run_with_deadline(cmd, Duration::from_secs(5), None)
        .expect("a big-output child completes well inside its deadline");
    assert!(out.status.success(), "the child exits 0: {out:?}");
    assert_eq!(
        out.stdout.len(),
        100_000,
        "every byte the child wrote must come back, past the ~64 KiB pipe buffer"
    );
    assert!(
        out.stderr.is_empty(),
        "nothing was written to stderr: {out:?}"
    );
}

/// The stderr twin: a child pasting the stderr pipe buffer must complete with
/// all its bytes too — a full stderr pipe blocks the child exactly as a full
/// stdout one does.
#[test]
fn a_child_pasting_the_stderr_pipe_buffer_completes_with_all_its_bytes() {
    use std::process::Command;
    use std::time::Duration;

    let mut cmd = Command::new("/bin/sh");
    cmd.args(["-c", "head -c 100000 /dev/zero >&2"]);
    let out = run_with_deadline(cmd, Duration::from_secs(5), None)
        .expect("a big-stderr child completes well inside its deadline");
    assert!(out.status.success(), "the child exits 0: {out:?}");
    assert_eq!(
        out.stderr.len(),
        100_000,
        "every byte the child wrote to stderr must come back"
    );
    assert!(
        out.stdout.is_empty(),
        "nothing was written to stdout: {out:?}"
    );
}

/// A spent hold budget clamps to zero, and nothing can run in zero time, so the
/// refusal happens before the spawn. That ordering is the point: the payload is
/// written to the child's stdin BEFORE the deadline loop starts, so spawning
/// anyway would hand the credential JSON to a process killed at the first poll.
///
/// `/bin/echo` would exit 0 instantly if it were ever spawned, so the error here
/// cannot have come from the child.
#[test]
fn a_spent_budget_refuses_before_spawning_anything() {
    use std::process::Command;
    use std::time::Duration;

    let err = run_with_deadline(
        Command::new("/bin/echo"),
        Duration::ZERO,
        Some("pretend-credential-json\n"),
    )
    .expect_err("a spent budget must refuse, and /bin/echo would have succeeded");

    let msg = format!("{err:#}");
    assert!(
        msg.contains("budget is already spent"),
        "the refusal must name the budget rather than read as a timeout: {msg}"
    );
    assert!(
        !msg.contains("pretend-credential-json"),
        "the payload must never reach the error text: {msg}"
    );
}

#[test]
fn keychain_deadline_returns_output_for_a_fast_command() {
    use std::process::Command;
    use std::time::Duration;

    let cmd = Command::new("/usr/bin/true");
    let out = run_with_deadline(cmd, Duration::from_secs(5), None).expect("fast command succeeds");
    assert!(out.status.success(), "`true` exits 0 within the deadline");
}

// ── `security -i` plumbing: stdin transport + line quoting (no Keychain) ──────

#[test]
fn deadline_feeds_stdin_payload_and_closes_the_pipe() {
    use std::process::Command;
    use std::time::Duration;

    // `cat` exits only when stdin reaches EOF — proves the payload is written
    // AND the pipe is closed (a leaked handle would hang until the deadline).
    let cmd = Command::new("/bin/cat");
    let out = run_with_deadline(cmd, Duration::from_secs(5), Some("payload {\"a b\"}\n"))
        .expect("cat echoes stdin and exits on EOF");
    assert!(out.status.success());
    assert_eq!(String::from_utf8_lossy(&out.stdout), "payload {\"a b\"}\n");
}

#[test]
fn security_quote_escapes_quotes_backslashes_and_wraps() {
    assert_eq!(security_quote("plain").expect("quote"), "\"plain\"");
    assert_eq!(
        security_quote(r#"{"k": "a b"}"#).expect("quote"),
        r#""{\"k\": \"a b\"}""#
    );
    assert_eq!(
        security_quote(r"back\slash").expect("quote"),
        r#""back\\slash""#
    );
}

#[test]
fn security_quote_refuses_embedded_newlines() {
    // `-i` is a line protocol — a newline inside a value would parse as a
    // second command. Refusal must be loud, never a silent truncation.
    assert!(security_quote("a\nb").is_err());
    assert!(security_quote("a\rb").is_err());
}

// ── Namespaced Keychain service name (hash computation, no Keychain touched) ──

#[test]
fn keychain_service_name_is_deterministic() {
    let name1 = keychain_service_for_config_dir(Path::new("/tmp")).expect("first");
    let name2 = keychain_service_for_config_dir(Path::new("/tmp")).expect("second");
    assert_eq!(name1, name2, "same path must produce the same service name");
}

#[test]
fn keychain_service_name_differs_for_different_paths() {
    let name1 = keychain_service_for_config_dir(Path::new("/tmp")).expect("tmp");
    let name2 = keychain_service_for_config_dir(Path::new("/")).expect("root");
    assert_ne!(name1, name2, "different paths must produce different names");
}

#[test]
fn keychain_service_suffix_is_8_hex_chars() {
    let name = keychain_service_for_config_dir(Path::new("/tmp")).expect("name");
    let suffix = name
        .strip_prefix("Claude Code-credentials-")
        .expect("prefix matches");
    assert_eq!(suffix.len(), 8, "suffix must be exactly 8 characters");
    assert!(
        suffix
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
        "suffix must be lowercase hexadecimal: got {suffix}"
    );
}

#[test]
fn keychain_service_canonicalization_resolves_dot_dot() {
    // A path with `..` must resolve to the same name as the direct path.
    let tmp = tempfile::tempdir().expect("tempdir");
    let sub = tmp.path().join("sub");
    std::fs::create_dir(&sub).expect("create sub dir");

    let via_dotdot = keychain_service_for_config_dir(&sub.join("..")).expect("via dotdot");
    let direct = keychain_service_for_config_dir(tmp.path()).expect("direct");
    assert_eq!(
        via_dotdot, direct,
        "`sub/..` must canonicalize to the parent directory"
    );
}

// ── KC-2: the transport ceilings (pure) ───────────────────────────────────────
//
// `security -i` reads ONE command per line into a bounded buffer. Past it the
// value is silently cut and the tail parsed as another command, leaving the item
// holding truncated JSON -- a destroyed login AND destroyed `mcpOAuth`. Harmless
// while the mirror wrote a 1-2 KB login-only blob; reachable now that the blob
// carries Claude Code's sibling keys. The boundary decision is pure, so both
// ceilings are pinned without a Keychain.

#[test]
fn stdin_carries_a_line_up_to_the_ceiling() {
    assert_eq!(
        put_transport(SECURITY_STDIN_LINE_MAX, 100).expect("at the ceiling"),
        PutTransport::Stdin,
    );
    assert_eq!(
        put_transport(SECURITY_STDIN_LINE_MAX + 1, 100).expect("one past it"),
        PutTransport::Argv,
        "one byte past the ceiling must leave the stdin transport, not truncate",
    );
}

#[test]
fn argv_takes_over_up_to_its_own_ceiling() {
    assert_eq!(
        put_transport(1_000_000, SECURITY_ARGV_VALUE_MAX).expect("at the argv ceiling"),
        PutTransport::Argv,
    );
    // Past the argv cap the write is refused outright: the cap holds a ~2x
    // margin under the measured E2BIG break (which itself fails safe, leaving
    // the item untouched), and a value that large is already pathological --
    // its size is dominated by carried `mcpOAuth` entries.
    assert!(put_transport(1_000_000, SECURITY_ARGV_VALUE_MAX + 1).is_err());
}

/// The ceiling is a measured fact of `security(1)`, not a tunable: the other
/// transport tests key off `SECURITY_STDIN_LINE_MAX` symbolically, so a
/// drifted constant passes them all — and one drifted to 4098+ routes
/// over-cap lines through stdin where `security` truncates destructively.
/// Pinned to literals instead, both directions. Measured on `mac-6`
/// (macOS 26.5.2) 2026-09-01: a 4097-byte line including the `\n` (4096 of
/// text) round-trips intact 6/6, a 4098-byte line truncates at 4096, so 4096
/// incl `\n` (text <= 4095) is the safe key with 1-2 B of deliberate headroom.
#[test]
fn the_stdin_ceiling_is_pinned_to_the_measured_4096() {
    assert_eq!(
        put_transport(4096, 100).expect("a 4096-byte line resolves"),
        PutTransport::Stdin,
        "the constant drifted below 4096: conservative, but it abandons stdin for lines the \
         probe measured as safe"
    );
    assert_eq!(
        put_transport(4097, 100).expect("a 4097-byte line resolves"),
        PutTransport::Argv,
        "the constant drifted to 4097 or past: that spends the headroom under the destructive \
         4098-byte break, and past 4097 truncation returns"
    );
}

/// The refusal is the one user-facing string this module adds, and each clause
/// is a distinct claim — the size, the cap, why stdin cannot carry it, the
/// `mcpOAuth` remediation — so it is pinned by equality against a fixture that
/// fixes both interpolated values as literals. A deleted clause, a reworded
/// one, or a drifted `SECURITY_ARGV_VALUE_MAX` (the rendered literals no
/// longer match) reds here instead of shipping silently.
#[test]
fn the_over_cap_refusal_names_the_size_the_cap_and_the_remediation() {
    let err = put_transport(1_000_000, SECURITY_ARGV_VALUE_MAX + 1)
        .expect_err("one byte past the argv cap must refuse");
    assert_eq!(
        err.to_string(),
        "refusing to write a 524289-byte Keychain item: it is over the 524288-byte cap this \
         writer keeps below the exec limit (the stdin transport would truncate it instead of \
         failing). The item's size is dominated by the `mcpOAuth` entries carried beside the \
         login; disable MCP servers or plugins that carry OAuth entries until the item shrinks \
         below the cap",
    );
}

/// One `mcpOAuth`-shaped sibling entry, sized like the real ones Claude Code
/// writes (~420 escaped bytes each once quoting is applied).
fn mcp_entry(n: usize) -> (String, serde_json::Value) {
    (
        format!("plugin:test:server{n}|{n:016x}"),
        serde_json::json!({
            "serverName": format!("plugin:test:server{n}"),
            "serverUrl": format!("https://mcp.example{n}.com/mcp"),
            "accessToken": "",
            "discoveryState": {
                "authorizationServerUrl": format!("https://mcp.example{n}.com"),
                "resourceMetadataUrl":
                    format!("https://mcp.example{n}.com/.well-known/oauth-protected-resource"),
                "oauthMetadataFound": true
            },
            "clientId": format!("{n:0>36}"),
            "issuer": format!("https://mcp.example{n}.com"),
            "redirectUri": "http://localhost:3118/callback"
        }),
    )
}

/// An item whose composed `security -i` line exceeds that tool's 4096-byte input
/// cap. `entries` of the shape above plus a login is what a real switch carries.
fn oversized_item(entries: usize) -> serde_json::Value {
    let mut mcp = serde_json::Map::new();
    for n in 0..entries {
        let (key, value) = mcp_entry(n);
        mcp.insert(key, value);
    }
    serde_json::json!({
        "claudeAiOauth": {
            "accessToken": "sk-ant-oat01-OVERSIZE",
            "refreshToken": "sk-ant-ort01-OVERSIZE",
            "expiresAt": 1_900_000_000_000i64,
            "scopes": ["user:inference", "user:profile"],
            "subscriptionType": "max"
        },
        "mcpOAuth": mcp
    })
}

// KC-3 -- the ceiling through the REAL `security`, on a throwaway service. Sized
// like an operator carrying a dozen-plus OAuth MCP servers beside the login: the
// case that used to come back truncated.
#[test]
#[ignore = "touches the real login Keychain (throwaway service); macOS re-prompts each rebuild — run explicitly with --ignored"]
fn a_blob_past_the_stdin_ceiling_round_trips_intact() {
    let service = format!("clauth-ceiling-test-{}", std::process::id());
    let account = "clauth-test-account";
    delete_at(&service, account).expect("pre-clean");

    // Sized by VALUE around the cap; every leg's composed LINE still lands
    // past it — the 200-B access token, 74-B JSON scaffolding, 14 quote
    // escapes and the line wrapper (87 B here; the service name carries the
    // pid, so each digit of width moves it by one) add 375 B — so the loop
    // drives the argv transport at increasing depths. The under-ceiling side
    // of the boundary is KC-5's.
    for value_len in [
        SECURITY_STDIN_LINE_MAX - 200,
        SECURITY_STDIN_LINE_MAX + 200,
        32 * 1024,
    ] {
        let blob = serde_json::json!({
            "claudeAiOauth": { "accessToken": "a".repeat(200) },
            "mcpOAuth": { "srv": { "accessToken": "m".repeat(value_len) } },
        });
        put_blob_at(&service, account, &blob).expect("write");
        assert_eq!(
            read_blob_at(&service, account).expect("read").as_ref(),
            Some(&blob),
            "a {value_len}-byte value came back changed -- the transport truncated it",
        );
    }

    // The field shape that made this reachable (#76): an auto-switch carrying
    // twenty OAuth MCP discovery entries beside the login. Guarded so a
    // shrunken fixture cannot silently slide back under the cap and stop
    // exercising the argv branch.
    let realistic = oversized_item(20);
    let json_len = serde_json::to_string(&realistic).expect("serialize").len();
    assert!(
        json_len > SECURITY_STDIN_LINE_MAX,
        "fixture must exceed the line cap to exercise the argv branch, got {json_len}"
    );
    // The argv arm's disclosure, pinned where it fires: the capture diverts
    // the event line this thread raises, so this write leg can assert the
    // owner-ruled EDR give-up is actually disclosed, with the composed line's
    // own length and the literals the logline interpolates.
    let lines = LogLines::new();
    let _capture = lines.capture_here();
    put_blob_at(&service, account, &realistic).expect("write");
    drop(_capture);
    let realistic_line = add_generic_password_line(
        &service,
        account,
        &serde_json::to_string(&realistic).expect("serialize"),
    )
    .expect("compose");
    assert_eq!(
        lines.snapshot(),
        vec![format!(
            "clauth: Keychain item is {len} bytes on the `/usr/bin/security -i` line, over the \
             4096 cap; writing it through argv instead, where the token is visible to same-UID \
             `ps` for the life of the call",
            len = realistic_line.len()
        )],
        "the argv transport must disclose the ps exposure, naming the real line length and cap"
    );
    assert_eq!(
        read_blob_at(&service, account).expect("read").as_ref(),
        Some(&realistic),
        "the 20-entry mcpOAuth item came back changed -- the transport truncated it",
    );

    delete_at(&service, account).expect("cleanup");
}

/// KC-4 -- the rotation path over an oversized item. `Keep::Everything`
/// (`keychain_mirror_rotation`) carries every key except the login through
/// `profile::preserve_extra_blocks`, a superset of the allowlist the switch
/// path carries, so it composes an even longer line. Both funnel through
/// `put_blob_at`; this pins that the rotation caller is covered rather than
/// assumed.
#[test]
#[ignore = "touches the real login Keychain (throwaway service); macOS re-prompts each rebuild — run explicitly with --ignored"]
fn keychain_rotation_over_an_oversized_item_keeps_the_siblings() {
    let service = format!("clauth-test-rotate-{}", std::process::id());
    let account = "clauth-test-account";
    delete_at(&service, account).expect("pre-clean delete is idempotent");

    // Seed the item the rotation will read and merge onto.
    let seeded = oversized_item(20);
    put_blob_at(&service, account, &seeded).expect("seed writes");

    // Rotate: a fresh login for the same account, everything else carried.
    let rotated = sample_creds("sk-ant-oat01-ROTATED", "sk-ant-ort01-ROTATED");
    let login = serde_json::to_value(&rotated).expect("serialize");
    merge_and_put_at(&service, account, &login, Keep::Everything).expect("rotation writes");

    let back = read_blob_at(&service, account)
        .expect("read")
        .expect("the item exists");
    assert_eq!(
        back.get("mcpOAuth"),
        seeded.get("mcpOAuth"),
        "every sibling survives a rotation byte-identical"
    );
    assert_eq!(
        back.pointer("/claudeAiOauth/accessToken")
            .and_then(serde_json::Value::as_str),
        Some("sk-ant-oat01-ROTATED"),
        "the fresh login replaced the old one"
    );

    delete_at(&service, account).expect("cleanup");
}

/// KC-5's throwaway service: pid zero-padded to a fixed width, so the composed
/// line length is deterministic across runs (a bare pid's digit count varies).
fn edge_service() -> String {
    format!("clauth-ceiling-edge-{:0>10}", std::process::id())
}

/// KC-5's fixture: a login-shaped blob whose composed `security -i` line lands
/// just under the measured ceiling — the 3711-B `mcpOAuth` token plus the
/// 200-B access token, 74-B JSON scaffolding, 14 quote escapes and 91-B line
/// wrapper total 4090 B.
fn near_ceiling_blob() -> serde_json::Value {
    serde_json::json!({
        "claudeAiOauth": { "accessToken": "a".repeat(200) },
        "mcpOAuth": { "srv": { "accessToken": "m".repeat(3711) } },
    })
}

/// KC-5's length arithmetic, pinned where it runs on EVERY platform: the
/// ignored leg's window assert below executes only on macOS, so without this
/// the composed length is checked nowhere a Linux gate can see.
#[test]
fn the_near_ceiling_fixture_composes_just_under_the_measured_line_cap() {
    let json = serde_json::to_string(&near_ceiling_blob()).expect("serialize");
    let line =
        add_generic_password_line(&edge_service(), "clauth-test-account", &json).expect("compose");
    let line_len = line.len();
    assert!(
        (4090..=4096).contains(&line_len),
        "the fixture must compose a line just under the measured 4096-B ceiling, never over \
         it; composed {line_len} B"
    );
}

/// KC-5 -- the near side of the boundary KC-3 goes past: a line composed just
/// UNDER the measured ceiling, through the REAL `security`, so the stdin
/// transport is exercised at the boundary rather than only on KC-1's small
/// login-only blobs. The length is asserted off the composed string itself —
/// never the constant — so the leg cannot silently slide over the cap.
#[test]
#[ignore = "touches the real login Keychain (throwaway service); macOS re-prompts each rebuild — run explicitly with --ignored"]
fn a_line_just_under_the_stdin_ceiling_round_trips_intact() {
    let service = edge_service();
    let account = "clauth-test-account";
    delete_at(&service, account).expect("pre-clean delete is idempotent");

    let blob = near_ceiling_blob();
    let json = serde_json::to_string(&blob).expect("serialize");
    let line = add_generic_password_line(&service, account, &json).expect("compose");
    let line_len = line.len();
    assert!(
        (4090..=4096).contains(&line_len),
        "this leg must ride stdin just under the measured 4096-B ceiling, composed {line_len} B"
    );
    put_blob_at(&service, account, &blob).expect("write");
    assert_eq!(
        read_blob_at(&service, account).expect("read").as_ref(),
        Some(&blob),
        "a {line_len}-byte line under the ceiling came back changed -- the transport truncated it",
    );

    delete_at(&service, account).expect("cleanup");
}

// ── KC-6: what a read-back proves about a write (pure, no Keychain touched) ────
//
// `add-generic-password` exiting 0 is not proof the item holds the bytes sent:
// the `-i` truncation (#66) exited 0 while the item held cut-off JSON. The
// four-way decision — match, mismatch, absent, unreadable — is pure, so the
// whole truth table is pinned without a Keychain.

/// A read-back equal to the written JSON is the only silent success. The `-w`
/// output carries a trailing newline the written JSON does not, so the compare
/// normalizes trailing whitespace and nothing else.
#[test]
fn verify_outcome_matches_a_read_back_equal_to_the_written_json() {
    let json = r#"{"claudeAiOauth":{"accessToken":"sk-ant-oat01-A"}}"#;
    assert_eq!(
        verify_outcome(Ok(Some(format!("{json}\n"))), json),
        VerifyOutcome::Verified,
        "the `-w` trailing newline is not part of the value"
    );
    assert_eq!(
        verify_outcome(Ok(Some(json.to_string())), json),
        VerifyOutcome::Verified,
        "a read-back with no trailing whitespace at all is also a match"
    );
}

/// A read-back with different bytes is a KNOWN-CORRUPT write, and the differing
/// bytes ride the outcome — they are what the quarantine file must hold. The
/// compare is on BYTES: leading whitespace is a different value, not a
/// formatting difference, and must not pass as a match.
#[test]
fn verify_outcome_carries_the_read_back_bytes_of_a_mismatch() {
    let written = r#"{"claudeAiOauth":{"accessToken":"sk-ant-oat01-A"}}"#;
    let truncated = r#"{"claudeAiOauth":{"accessToken":"sk-ant-oat01-"#;
    assert_eq!(
        verify_outcome(Ok(Some(format!("{truncated}\n"))), written),
        VerifyOutcome::Corrupt(format!("{truncated}\n")),
        "the differing bytes ride the outcome, for the quarantine file to hold"
    );
    assert_eq!(
        verify_outcome(Ok(Some(format!(" {written}\n"))), written),
        VerifyOutcome::Corrupt(format!(" {written}\n")),
        "the compare is on bytes, so leading whitespace is a mismatch, not a match"
    );
}

/// An item that reads back ABSENT (exit 44) after a write that reported
/// success: the write did not land, which is as known-corrupt as a mismatch —
/// but with no bytes to quarantine, the variant carries none.
#[test]
fn verify_outcome_marks_an_absent_item_vanished() {
    assert_eq!(
        verify_outcome(Ok(None), r#"{"claudeAiOauth":{}}"#),
        VerifyOutcome::Vanished,
    );
}

/// A read-back that cannot run — a budget clamped to zero, a deadline, a read
/// refusal over ssh — carries the rendered cause for the event line and is
/// SUCCESS for the caller: the write's own exit code said it landed, and an
/// unverifiable write must not fail a completed switch.
#[test]
fn verify_outcome_marks_a_failed_read_unverified_with_the_cause() {
    assert_eq!(
        verify_outcome(
            Err(anyhow::anyhow!(
                "/usr/bin/security exceeded its 10s deadline"
            )),
            "x"
        ),
        VerifyOutcome::Unverified("/usr/bin/security exceeded its 10s deadline".to_string()),
    );
    // The whole chain renders, not just the outer message: read failures
    // arrive context-wrapped ("failed to run ... find-generic-password"
    // around the exit-code error), and the event line names the real cause.
    let chained =
        anyhow::anyhow!("Keychain read failed (security exit 36): interaction not allowed")
            .context("failed to run /usr/bin/security find-generic-password");
    assert_eq!(
        verify_outcome(Err(chained), "x"),
        VerifyOutcome::Unverified(
            "failed to run /usr/bin/security find-generic-password: Keychain read failed \
             (security exit 36): interaction not allowed"
                .to_string()
        ),
    );
}

// ── KC-6b: the RULING on what a read-back proved (pure, no Keychain) ───────────
//
// The classification above was pinned from the start; the ruling was not, and
// one-line flips of it — a corrupt write completing a switch, an unverifiable
// one failing it — shipped green with every test passing. The mapping and the
// verdict are pure, so the owner-ruled dispositions are pinned without a
// Keychain; `verify_write` only executes them.

/// M1's owner ruling, the failing half: a write KNOWN corrupt — different
/// read-back bytes, or an item that read back absent after a reported success
/// — FAILS the switch. The corrupt arm quarantines the carried bytes first;
/// the absent arm has none to quarantine.
#[test]
fn a_write_known_corrupt_fails_the_switch() {
    let bytes = r#"{"claudeAiOauth":{"accessToken":"sk-ant-oat01-TRUNC"#.to_string();
    assert_eq!(
        write_disposition(VerifyOutcome::Corrupt(bytes.clone())),
        WriteDisposition::QuarantineAndFail(bytes),
        "a mismatched read-back is quarantined, then the write fails"
    );
    assert_eq!(
        write_disposition(VerifyOutcome::Vanished),
        WriteDisposition::Fail,
        "an absent read-back fails the write too — no bytes to quarantine"
    );
    assert!(
        disposition_verdict(&WriteDisposition::QuarantineAndFail(String::new())).is_err(),
        "completing a switch over a known-corrupt write is the one outcome M1 refuses"
    );
    assert!(
        disposition_verdict(&WriteDisposition::Fail).is_err(),
        "an absent-after-success write fails the switch as surely as a mismatched one"
    );
}

/// M1's owner ruling, the completing half: an UNVERIFIABLE write — the
/// read-back could not run at all — completes the switch with the cause named
/// on the event line, and a VERIFIED one completes silently.
#[test]
fn an_unverifiable_write_completes_the_switch() {
    assert_eq!(
        write_disposition(VerifyOutcome::Unverified("read refused".to_string())),
        WriteDisposition::CompleteWithNote("read refused".to_string()),
        "a read-back that cannot run completes the switch, naming why"
    );
    assert_eq!(
        write_disposition(VerifyOutcome::Verified),
        WriteDisposition::CompleteSilently,
        "a matched read-back is the only silent outcome"
    );
    assert!(
        disposition_verdict(&WriteDisposition::CompleteWithNote("x".to_string())).is_ok(),
        "an unverifiable write must not fail a completed switch"
    );
    assert!(
        disposition_verdict(&WriteDisposition::CompleteSilently).is_ok(),
        "a verified write completes, silently"
    );
}

/// The read-back bytes ride `Corrupt` and `QuarantineAndFail` for quarantine,
/// but never through `Debug` — the same hand-written redaction as
/// `UnparseableItem`'s, so a future `{:?}` render cannot put a session on a
/// log line.
#[test]
fn the_verify_outcome_and_disposition_debug_never_print_the_bytes() {
    let corrupt = VerifyOutcome::Corrupt(
        r#"{"claudeAiOauth":{"accessToken":"sk-ant-oat01-SECRET"#.to_string(),
    );
    assert!(
        !format!("{corrupt:?}").contains("sk-ant-oat01-SECRET"),
        "VerifyOutcome's Debug prints a length, never the bytes: {corrupt:?}"
    );
    let disposition = write_disposition(corrupt);
    assert!(
        !format!("{disposition:?}").contains("sk-ant-oat01-SECRET"),
        "the disposition carries the same bytes and redacts them the same way: {disposition:?}"
    );
}

// ── KC-7: quarantine before overwrite/delete (pure decision + derivation) ──────
//
// Two sites used to destroy salvageable bytes on an unparseable read: the merge
// overwrote the item, the sign-out deleted it (#66/#76 — the corruption usually
// leaves an intact `claudeAiOauth` head). The salvage decision and the path it
// writes to are pure, so both are pinned without a Keychain.

/// Only an unparseable read has bytes to preserve; every other read failure saw
/// nothing and behaves exactly as it did before quarantine existed. The split
/// is what keeps a locked keychain or a spent budget from writing a quarantine
/// file full of nothing, and keeps the no-bytes event line (the re-authenticate
/// one) the one those failures take.
#[test]
fn carried_raw_quarantines_only_an_unparseable_reads_bytes() {
    let parse_error =
        serde_json::from_str::<serde_json::Value>("not json").expect_err("garbage fails to parse");
    let unparseable = anyhow::Error::new(UnparseableItem {
        raw: r#"{"claudeAiOauth":{"accessToken":"sk-ant-oat01-TRUNC"#.to_string(),
        parse_error,
    });
    assert_eq!(
        carried_raw(&unparseable),
        Some(r#"{"claudeAiOauth":{"accessToken":"sk-ant-oat01-TRUNC"#),
        "an unparseable read carries its raw bytes for the quarantine file"
    );
    // Every failure that saw no bytes: a deadline, a budget refusal, a UTF-8
    // failure, a read that errored on an exit code. None quarantine.
    for e in [
        anyhow::anyhow!("/usr/bin/security exceeded its 10.000s deadline and was killed"),
        anyhow::anyhow!(
            "/usr/bin/security not run: this lock hold's subprocess budget is already spent"
        ),
        anyhow::anyhow!("Keychain password is not UTF-8"),
        anyhow::anyhow!("Keychain read failed (security exit 36): interaction not allowed"),
    ] {
        assert!(
            carried_raw(&e).is_none(),
            "a read failure with no bytes must not quarantine: {e}"
        );
    }
}

/// The bytes ride the error for quarantine, but never through `Debug`: the
/// hand-written impl redacts `raw` to a length, because a derived `Debug` would
/// print live credential bytes and a stray `{:?}` on this error would put a
/// session on a log line (`ConsoleCredential`'s hand-written impl is the
/// precedent, `docs/security.md`).
#[test]
fn an_unparseable_items_debug_never_prints_the_bytes() {
    let parse_error =
        serde_json::from_str::<serde_json::Value>("not json").expect_err("parse fails");
    let item = UnparseableItem {
        raw: r#"{"claudeAiOauth":{"accessToken":"sk-ant-oat01-SECRET"#.to_string(),
        parse_error,
    };
    let rendered = format!("{item:?}");
    assert!(
        !rendered.contains("sk-ant-oat01-SECRET"),
        "Debug on the salvaged bytes must print a length, never the bytes: {rendered}"
    );
    assert!(
        rendered.contains("raw_len"),
        "the hand-written Debug is what keeps them off: {rendered}"
    );
}

/// The per-event path: tree-placed (the quarantine dir is a sibling of
/// `profiles/`), timestamped (UTC — a filename must not carry `:`, which
/// Finder renders as a path separator), pid-separated (the daemon and a TUI
/// can salvage the same item in the same second), and service-identifiable
/// (the bare item and a namespaced per-config-dir one can both exist). An
/// epoch past chrono's range degrades to the raw seconds rather than
/// panicking or colliding.
#[test]
fn quarantine_path_is_timestamped_pid_separated_and_service_identified() {
    let base = Path::new("/home/op/.clauth");
    assert_eq!(
        quarantine_path(base, "Claude Code-credentials", 1_771_234_565, 4213),
        PathBuf::from(
            "/home/op/.clauth/keychain-quarantine/20260216T093605Z-4213-Claude Code-credentials.json"
        ),
    );
    assert_eq!(
        quarantine_path(
            base,
            "Claude Code-credentials-0123abcd",
            1_771_234_565,
            4213
        ),
        PathBuf::from(
            "/home/op/.clauth/keychain-quarantine/20260216T093605Z-4213-Claude Code-credentials-0123abcd.json"
        ),
    );
    // A separator in the service cannot escape the dir.
    assert_eq!(
        quarantine_path(base, "../evil", 0, 1),
        PathBuf::from("/home/op/.clauth/keychain-quarantine/19700101T000000Z-1-.._evil.json"),
        "`/` is sanitized, never a path separator",
    );
    assert_eq!(
        quarantine_path(base, "s", i64::MAX, 1).file_name(),
        Some(std::ffi::OsStr::new("9223372036854775807-1-s.json")),
        "an out-of-range epoch degrades to the raw seconds, still unique per event",
    );
}

/// The wording that tells an operator where the salvaged bytes went, and how to
/// get them back — pinned by equality because it renders during an incident,
/// and a clause that silently drops off the line costs the recovery it names.
/// The `{e}` of the failure arm is an io error from the quarantine write
/// itself, never `security` stderr.
#[test]
fn quarantine_tail_names_the_file_and_the_recovery_or_the_loss() {
    let path = PathBuf::from(
        "/home/op/.clauth/keychain-quarantine/20260216T093605Z-4213-Claude Code-credentials.json",
    );
    assert_eq!(
        quarantine_tail(&Ok(path)),
        "raw bytes are preserved at /home/op/.clauth/keychain-quarantine/20260216T093605Z-4213-Claude Code-credentials.json: the `claudeAiOauth` login head usually survives this corruption, so re-authenticate any MCP server that reports a signed-out session, or slice the login out of the quarantined file",
    );
    assert_eq!(
        quarantine_tail(&Err(anyhow::anyhow!("permission denied"))),
        "raw bytes could not be preserved (permission denied): re-authenticate any MCP server \
         that reports a signed-out session, or sign in again at claude.ai",
    );
}

// ── KC-8: write errors never embed stderr (GH #66's echo) ──────────────────────
//
// `security` echoed an escaped fragment of the WRITTEN VALUE into its stderr on
// the truncated-line path (`security: unknown command "CJuYW1lIjoi…"`), and
// that text rode `security_error` into event lines and `daemon.log`. A write
// failure now reports the exit code and the stderr byte COUNT, never the
// bytes; read and delete failures keep the bytes — no credential is ever sent
// on those calls, and the text is diagnostic.

/// A `security` failure with the given exit code and stderr, in the shape
/// `run_with_deadline` hands back. `code << 8` is the unix wait status for
/// "exited with `code`"; the signal row below builds a killed child directly.
fn security_output(code: i32, stderr: &str) -> std::process::Output {
    use std::os::unix::process::ExitStatusExt;
    std::process::Output {
        status: std::process::ExitStatus::from_raw(code << 8),
        stdout: Vec::new(),
        stderr: stderr.as_bytes().to_vec(),
    }
}

/// The write-op message, pinned by full equality against a fixture that fixes
/// every interpolated value: exit 51 and a 57-byte stderr carrying exactly the
/// echoed-value shape observed in the field. The count is the RAW stderr
/// length — exactly the bytes withheld, all of them: the fixture pads both
/// ends (raw 57, trimmed 53) so an edit switching the count to the trimmed
/// length reds here instead of shipping a figure that does not name what was
/// withheld. The equality proves the marker absent and the count present; the
/// explicit `contains` assert names the defect class for whoever reads a
/// failure here.
#[test]
fn a_write_error_reports_the_exit_code_and_stderr_size_never_the_bytes() {
    let output = security_output(
        51,
        "  security: unknown command \"CJuYW1lIjoiZXhwIjoxNzA5fQ\"  ",
    );
    assert_eq!(
        output.stderr.len(),
        57,
        "the fixture must hold its padded shape"
    );
    let err = security_error(SecurityOp::Write, &output);
    assert_eq!(
        err.to_string(),
        "Keychain write failed (security exit 51): its 57-byte stderr is not shown, because \
         a write's stderr can echo the value being written",
    );
    assert!(
        !err.to_string().contains("CJuYW1lIjoi"),
        "the echoed value fragment must never reach a write error's text"
    );
    assert!(
        !err.to_string().contains("53-byte"),
        "the withheld figure is the raw byte count, never the trimmed one"
    );
    // A child killed by a signal has no exit code at all; the split holds.
    let signalled = {
        use std::os::unix::process::ExitStatusExt;
        std::process::Output {
            status: std::process::ExitStatus::from_raw(9),
            stdout: Vec::new(),
            stderr: b"x".to_vec(),
        }
    };
    assert_eq!(
        security_error(SecurityOp::Write, &signalled).to_string(),
        "Keychain write failed (security exit signal): its 1-byte stderr is not shown, because \
         a write's stderr can echo the value being written",
    );
}

/// The control: read and delete send no credential, so their errors keep the
/// stderr bytes — the diagnostic value (an ACL refusal reads differently from
/// a usage error) is why the embedding arm exists at all.
#[test]
fn read_and_delete_errors_still_embed_stderr() {
    let output = security_output(36, "SecKeychainSearchCopyNext: interaction not allowed");
    assert_eq!(
        security_error(SecurityOp::Read, &output).to_string(),
        "Keychain read failed (security exit 36): SecKeychainSearchCopyNext: interaction not allowed",
    );
    assert_eq!(
        security_error(SecurityOp::Delete, &output).to_string(),
        "Keychain delete failed (security exit 36): SecKeychainSearchCopyNext: interaction not allowed",
        "delete sends no credential either, and its stderr is diagnostic"
    );
}

// ── KC-9: the quarantine path over a REAL Keychain item ────────────────────────
//
// The merge site's salvage, end to end: a merge onto an item holding truncated
// JSON (#66/#76's defect shape — intact head, cut tail) must quarantine the
// corrupted bytes BEFORE the overwrite that would destroy them, still complete
// the write (the module's completing-the-switch posture), and say so on the
// event line. The healthy verify leg of that same write is silent, which this
// pins too: exactly one event line fires.

#[test]
#[ignore = "touches the real login Keychain (throwaway service); macOS re-prompts each rebuild — run explicitly with --ignored"]
fn a_merge_over_unparseable_bytes_quarantines_them_and_still_writes() {
    use std::process::Command;

    let service = format!("clauth-test-quarantine-{}", std::process::id());
    let account = "clauth-test-account";
    delete_at(&service, account).expect("pre-clean delete is idempotent");

    // Seed the defect: truncated JSON, an intact `claudeAiOauth` head with its
    // tail cut — the exact shape the `-i` line truncation left in the field.
    // Staged through the same `-i` line the real write used, so the item holds
    // precisely what a truncated write would have left.
    let garbage = r#"{"claudeAiOauth":{"accessToken":"sk-ant-oat01-TRUNCATED"#;
    let line = add_generic_password_line(&service, account, garbage).expect("compose seed line");
    let mut cmd = Command::new(SECURITY_BIN);
    cmd.arg("-i");
    let seeded = run_with_deadline(cmd, security_deadline(), Some(&line)).expect("run seed");
    assert!(
        seeded.status.success(),
        "seeding the garbage item: {seeded:?}"
    );

    // The quarantine write lands under `~/.clauth`, which under `cfg(test)`
    // must resolve to a sandbox — never the operator's real tree.
    let sandbox = HomeSandbox::new();
    let lines = LogLines::new();
    let _capture = lines.capture_here();

    // The merge path over the corrupted item: the read comes back unparseable,
    // the bytes are quarantined, the merge proceeds as over an absent item,
    // and the write lands and verifies (silently, on a healthy item).
    let incoming = serde_json::json!({
        "claudeAiOauth": { "accessToken": "sk-ant-oat01-INCOMING" }
    });
    merge_and_put_at(&service, account, &incoming, Keep::CarriedOnly)
        .expect("the switch still completes over a corrupted item");
    drop(_capture);

    // The event line: current shape plus the salvage, and the path it names is
    // the file that holds the bytes.
    let snapshot = lines.snapshot();
    assert_eq!(
        snapshot.len(),
        1,
        "one event line (the quarantine note; the healthy verify is silent): {snapshot:?}"
    );
    let event = &snapshot[0];
    assert!(
        event.starts_with(
            "clauth: could not read the macOS Keychain login before replacing it (Keychain item \
             is not valid JSON"
        ),
        "the line keeps its current shape: {event}"
    );
    let (_, salvaged) = event
        .split_once("— their raw bytes are preserved at ")
        .expect("the line names where the bytes went");
    let (path_text, _) = salvaged
        .split_once(": the `claudeAiOauth`")
        .expect("the line carries the recovery hint");
    let path = Path::new(path_text);
    assert!(
        path_text.contains(&service),
        "the quarantine file is service-identifiable: {path_text}"
    );
    assert_eq!(
        std::fs::read_to_string(path).expect("read the quarantined bytes"),
        format!("{garbage}\n"),
        "the quarantine file holds the item's raw bytes, trailing `-w` newline included"
    );

    // The write still completed: the item now holds the incoming store whole.
    assert_eq!(
        read_blob_at(&service, account).expect("read").as_ref(),
        Some(&incoming),
        "the merge over a corrupted item still writes the incoming store"
    );

    // `docs/security.md`'s tree invariant, at the new artifact: 0600 file
    // under a 0700 dir, exactly like the parked `mcp-logins.json`.
    {
        use std::os::unix::fs::PermissionsExt;
        let file_mode = std::fs::metadata(path)
            .expect("quarantine file metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(file_mode, 0o600, "the quarantined bytes are owner-only");
        let dir_mode = std::fs::metadata(path.parent().expect("quarantine dir"))
            .expect("quarantine dir metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(dir_mode, 0o700, "the quarantine dir is owner-only");
    }

    drop(sandbox);
    delete_at(&service, account).expect("cleanup");
}

/// KC-10 — the verify leg's CALL SITE and its `CompleteWithNote` ruling,
/// against a real Keychain. The stageable read failure is the module doc's own
/// probe shape: an item ACL'd to another binary (`-T /usr/bin/false`, the
/// shape CC's own item has). The write lands silently (writes are gated on the
/// keychain being unlocked, never the item's trust list); the verify READ then
/// hits the ACL — an unanswered dialog at a console, a read refusal headless —
/// either way the read cannot run, the write is named landed-but-unverified on
/// one event line, and the switch completes. No Linux leg can kill a deleted
/// `verify_write` call (nothing there can spawn `/usr/bin/security`); this leg
/// is what kills it, on macOS only.
#[test]
#[ignore = "touches the real login Keychain (throwaway service, ACL-seeded); the verify read blocks up to SECURITY_TIMEOUT at a console — run explicitly with --ignored"]
fn an_unverifiable_write_lands_and_completes_the_switch() {
    use std::process::Command;

    let service = format!("clauth-test-acl-{}", std::process::id());
    let account = "clauth-test-account";
    // Seed a FRESH item (no -U: -T belongs to the create) whose trust list
    // names only /usr/bin/false, so our own find cannot read it back quietly.
    let mut seed = Command::new(SECURITY_BIN);
    seed.args([
        "add-generic-password",
        "-s",
        &service,
        "-a",
        account,
        "-w",
        "seed-value",
        "-T",
        "/usr/bin/false",
    ]);
    let seeded = run_with_deadline(seed, security_deadline(), None).expect("seed the ACL'd item");
    assert!(
        seeded.status.success(),
        "seeding the ACL'd item: {seeded:?}"
    );

    // The staged arm completes with a note, but an unexpected Corrupt
    // read-back would quarantine — the sandbox keeps that off the operator's
    // tree (and `clauth_dir` panics unsandboxed under cfg(test)).
    let sandbox = HomeSandbox::new();
    let lines = LogLines::new();
    let _capture = lines.capture_here();

    let blob = serde_json::json!({ "claudeAiOauth": { "accessToken": "sk-ant-oat01-ACL" } });
    let result = put_blob_at(&service, account, &blob);
    drop(_capture);

    assert!(
        result.is_ok(),
        "an unverifiable write must not fail a completed switch: {result:?}"
    );
    let snapshot = lines.snapshot();
    assert_eq!(
        snapshot.len(),
        1,
        "exactly the landed-but-unverified line — the write itself is silent, and a healthy \
         verify says nothing: {snapshot:?}"
    );
    let event = &snapshot[0];
    assert!(
        event.starts_with(
            "clauth: the macOS Keychain write landed but could not be read back to verify ("
        ),
        "the line names the write as landed but unverified: {event}"
    );
    assert!(
        event.ends_with(
            "the switch stays complete. If Claude Code reports a signed-out or broken session, \
             retrying the switch re-runs the write"
        ),
        "and names the remedy: {event}"
    );

    drop(sandbox);
    // Best-effort cleanup: deleting an item ACL'd to another binary may itself
    // prompt, and a leftover throwaway item is the acceptable residue of a
    // failed cleanup — not worth burning SECURITY_TIMEOUT or redding the leg.
    let _ = delete_at(&service, account);
}

/// KC-11 — the sign-out site's quarantine-before-delete, against a real
/// Keychain item: the DESTRUCTIVE half of M2 (KC-9 pins the merge site's
/// overwrite half). A sign-out over an item holding truncated JSON salvages
/// the corrupted bytes BEFORE the delete that would destroy them, still
/// deletes, and says so on the event line. Drives `sign_out_at` on a throwaway
/// service — `keychain_sign_out` itself is hardwired to the real item.
#[test]
#[ignore = "touches the real login Keychain (throwaway service); macOS re-prompts each rebuild — run explicitly with --ignored"]
fn a_sign_out_over_unparseable_bytes_quarantines_them_and_still_deletes() {
    use std::process::Command;

    let service = format!("clauth-test-signout-{}", std::process::id());
    let account = "clauth-test-account";
    delete_at(&service, account).expect("pre-clean delete is idempotent");

    // Seed the defect exactly like KC-9: truncated JSON, an intact
    // `claudeAiOauth` head with its tail cut, staged through the same
    // `security -i` line a truncated write used.
    let garbage = r#"{"claudeAiOauth":{"accessToken":"sk-ant-oat01-TRUNCATED"#;
    let line = add_generic_password_line(&service, account, garbage).expect("compose seed line");
    let mut cmd = Command::new(SECURITY_BIN);
    cmd.arg("-i");
    let seeded = run_with_deadline(cmd, security_deadline(), Some(&line)).expect("run seed");
    assert!(
        seeded.status.success(),
        "seeding the garbage item: {seeded:?}"
    );

    // The quarantine write lands under `~/.clauth`, which under `cfg(test)`
    // must resolve to a sandbox — never the operator's real tree.
    let sandbox = HomeSandbox::new();
    let lines = LogLines::new();
    let _capture = lines.capture_here();

    // The sign-out over the corrupted item: the read comes back unparseable,
    // the bytes are quarantined, and the delete still runs.
    sign_out_at(&service, account).expect("the sign-out still completes over a corrupted item");
    drop(_capture);

    // The event line: current shape plus the salvage, and the path it names is
    // the file that holds the bytes.
    let snapshot = lines.snapshot();
    assert_eq!(
        snapshot.len(),
        1,
        "one event line — the quarantine note over the delete: {snapshot:?}"
    );
    let event = &snapshot[0];
    assert!(
        event.starts_with(
            "clauth: signed Claude Code out of the macOS Keychain by deleting the item: it could \
             not be read first (Keychain item is not valid JSON"
        ),
        "the line keeps its current shape: {event}"
    );
    let (_, salvaged) = event
        .split_once("— their raw bytes are preserved at ")
        .expect("the line names where the bytes went");
    let (path_text, _) = salvaged
        .split_once(": the `claudeAiOauth`")
        .expect("the line carries the recovery hint");
    let path = Path::new(path_text);
    assert!(
        path_text.contains(&service),
        "the quarantine file is service-identifiable: {path_text}"
    );
    assert_eq!(
        std::fs::read_to_string(path).expect("read the quarantined bytes"),
        format!("{garbage}\n"),
        "the quarantine file holds the item's raw bytes, trailing `-w` newline included"
    );

    // The delete still happened: the item is gone.
    assert!(
        read_blob_at(&service, account).expect("read").is_none(),
        "the sign-out still deletes the corrupted item"
    );

    drop(sandbox);
    delete_at(&service, account).expect("cleanup");
}

/// KC-12 — an item past the pipe buffer, end to end: a ~100 KiB blob (over the
/// ~64 KiB pipe buffer, under the 512 KiB argv cap) writes through the argv
/// transport, reads back byte-identical, and its verify leg completes
/// `Verified` — the only silent outcome. Pre-drain this leg died at the 10 s
/// deadline on BOTH read legs: the verify read stalled into `Unverified`, and
/// the read-back `expect` panicked after its own 10 s stall — the B2 deadlock.
#[test]
#[ignore = "touches the real login Keychain (throwaway service); macOS re-prompts each rebuild — run explicitly with --ignored"]
fn an_item_past_the_pipe_buffer_round_trips_and_verifies() {
    let service = format!("clauth-test-drain-{}", std::process::id());
    let account = "clauth-test-account";
    delete_at(&service, account).expect("pre-clean delete is idempotent");

    // ~100 KiB of value: past the pipe buffer, composed line far past the
    // 4096-byte stdin ceiling (argv transport), far under the 512 KiB cap.
    let blob = serde_json::json!({
        "claudeAiOauth": { "accessToken": "a".repeat(200) },
        "mcpOAuth": { "srv": { "accessToken": "m".repeat(100 * 1024) } },
    });

    // The write rides argv (one disclosure line) and its verify leg reads the
    // ~100 KiB item back through the drain — byte-identical, so SILENT: no
    // landed-but-unverified line may appear.
    let lines = LogLines::new();
    let _capture = lines.capture_here();
    put_blob_at(&service, account, &blob).expect("write + verify complete");
    drop(_capture);
    let snapshot = lines.snapshot();
    assert_eq!(
        snapshot.len(),
        1,
        "exactly the argv disclosure — a byte-identical read-back is the only silent verify \
         outcome: {snapshot:?}"
    );
    assert!(
        snapshot[0].contains("writing it through argv"),
        "the one line is the transport disclosure: {snapshot:?}"
    );
    assert!(
        !snapshot[0].contains("could not be read back to verify"),
        "the verify leg completed, not landed-unverified: {snapshot:?}"
    );

    // The read leg drains the same ~100 KiB and parses it back byte-identical.
    assert_eq!(
        read_blob_at(&service, account).expect("read").as_ref(),
        Some(&blob),
        "an item past the pipe buffer round-trips byte-identical through the drain"
    );

    delete_at(&service, account).expect("cleanup");
}
