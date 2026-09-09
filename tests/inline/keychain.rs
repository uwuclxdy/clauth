//! KC-1 — Keychain read/write/delete round-trip. Uses a **throwaway service name**
//! unique to this process; it never touches the real `Claude Code-credentials`
//! item. It is the ONLY test that drives the real macOS Keychain (via the
//! `/usr/bin/security` CLI the shipped write/delete path uses), so it is
//! `#[ignore]`d: it still mutates the login Keychain (creates + deletes a
//! throwaway item) as a side effect. Run it on demand instead:
//!     cargo test keychain_round_trip -- --ignored
//! All other credential/divergence tests stay on the file model
//! (`keychain::enabled()` is false under `cfg(test)`), so `cargo test` never
//! touches the Keychain.

use super::{
    Keep, delete_at, keychain_service_for_config_dir, merge_and_put_at, merge_write, merged_blob,
    put_blob_at, read_blob_at, run_with_deadline, security_quote,
};
use crate::profile::{ClaudeCredentials, OAuthToken};
use std::path::Path;

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
