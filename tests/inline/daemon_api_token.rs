//! The REST API's bearer token: shape, persistence, and the constant-time
//! check. What these pin is that the credential an operator copies to another
//! machine keeps working across restarts, and that a file which is not a token
//! is never treated as one.
//!
//! All disk state is redirected into a [`HomeSandbox`] tempdir, so nothing here
//! reads or writes the operator's real `~/.clauth/auth_token.json`.

use super::*;

use crate::testutil::HomeSandbox;
// The 0600 tree invariant, and so the helper that checks it, is Unix-only —
// `atomic_write_600` falls back to a plain write elsewhere. Both imports serve
// only `token_file_is_owner_only`, which is gated the same way.
#[cfg(unix)]
use crate::profile::clauth_dir;
#[cfg(unix)]
use crate::testutil::owner_only_violations;

#[test]
fn generated_token_is_64_lowercase_hex() {
    let token = generate().expect("generate");
    assert_eq!(token.len(), 64, "a SHA-256 hex digest is 64 chars");
    assert!(is_well_formed(&token), "{token} failed its own shape check");
    assert_eq!(token, token.to_lowercase(), "hex must be lowercase");
}

#[test]
fn two_generations_differ() {
    // Not a randomness test, a wiring test: a constant seed would sail through
    // every other assertion in this file.
    let a = generate().expect("generate");
    let b = generate().expect("generate");
    assert_ne!(a, b);
}

#[test]
fn token_persists_across_calls() {
    let _home = HomeSandbox::new();
    let first = load_or_create().expect("first load");
    let second = load_or_create().expect("second load");
    assert_eq!(
        first, second,
        "a restart must not invalidate the token already copied to a client"
    );
}

/// Unix-only: Windows has no mode bits, and `atomic_write_600` writes plainly
/// there, so there is no invariant left to assert.
#[cfg(unix)]
#[test]
fn token_file_is_owner_only() {
    let _home = HomeSandbox::new();
    load_or_create().expect("load");
    let left = owner_only_violations(&clauth_dir().expect("clauth dir"));
    assert!(
        left.is_empty(),
        "the token file must inherit the 0600 tree invariant; still loose: {left:#?}"
    );
}

#[test]
fn rotate_replaces_the_stored_token() {
    let _home = HomeSandbox::new();
    let original = load_or_create().expect("load");
    let rotated = rotate().expect("rotate");
    assert_ne!(original, rotated);
    assert_eq!(
        load_or_create().expect("reload"),
        rotated,
        "rotation must persist, not just return a new value"
    );
}

/// A file that is not a well-formed token is replaced rather than trusted: half
/// a token is not a weaker token, it is no token.
#[test]
fn malformed_token_files_are_regenerated() {
    for bad in [
        r#"{"schema":1,"token":"short","created_at":"x"}"#,
        r#"{"schema":1,"token":"NOTHEXNOTHEXNOTHEXNOTHEXNOTHEXNOTHEXNOTHEXNOTHEXNOTHEXNOTHEXZZZZ","created_at":"x"}"#,
        // Upper-case hex: not what `generate` emits, so not what we accept.
        r#"{"schema":1,"token":"AAAABBBBCCCCDDDDEEEEFFFF00001111AAAABBBBCCCCDDDDEEEEFFFF00001111","created_at":"x"}"#,
        "not json at all",
        "",
    ] {
        let _home = HomeSandbox::new();
        let path = token_path().expect("path");
        crate::profile::mkdir_700(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(&path, bad).expect("seed");

        let token = load_or_create().expect("load");
        assert!(
            is_well_formed(&token),
            "{bad:?} should have been replaced, got {token:?}"
        );
    }
}

/// A file from a newer clauth is reused rather than rotated: a downgrade must
/// not silently invalidate the token every client already holds.
#[test]
fn future_schema_token_is_reused_when_well_formed() {
    let _home = HomeSandbox::new();
    let future = "a".repeat(64);
    let path = token_path().expect("path");
    crate::profile::mkdir_700(path.parent().expect("parent")).expect("mkdir");
    std::fs::write(
        &path,
        format!(r#"{{"schema":99,"token":"{future}","created_at":"x"}}"#),
    )
    .expect("seed");

    assert_eq!(load_or_create().expect("load"), future);
}

#[test]
fn verify_accepts_the_exact_token_only() {
    let token = generate().expect("generate");
    let auth = AuthToken::from_plaintext(&token);

    assert!(auth.verify(&token));
    assert!(!auth.verify(""), "empty must not pass");
    assert!(!auth.verify(&token[..63]), "a truncation must not pass");
    assert!(
        !auth.verify(&format!("{token}x")),
        "a token with a suffix must not pass"
    );

    // Flip the last character: the digest compare means a near-miss is no
    // closer to passing than a wildly wrong value.
    let mut near = token.clone();
    let last = if near.ends_with('a') { 'b' } else { 'a' };
    near.pop();
    near.push(last);
    assert!(!auth.verify(&near));
}

/// The token must not be reachable through a formatter. `AuthToken` holds a
/// digest, and a digest still verifies a guess offline.
#[test]
fn auth_token_debug_hides_the_digest() {
    let auth = AuthToken::from_plaintext(&generate().expect("generate"));
    assert_eq!(format!("{auth:?}"), "AuthToken(<redacted>)");
}
