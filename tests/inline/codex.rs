//! Codex live-file mechanics under a sandboxed HOME — never the real
//! `~/.codex`. Fixture tokens are fakes (`at-alpha` style).

use super::*;
use crate::testutil::HomeSandbox;

fn auth_bytes(access: &str, account_id: &str) -> Vec<u8> {
    serde_json::json!({
        "auth_mode": "chatgpt",
        "tokens": {
            "access_token": access,
            "refresh_token": format!("rt-{access}"),
            "account_id": account_id,
        },
        "agent_identity": { "unmodeled": ["round", "trip"] },
    })
    .to_string()
    .into_bytes()
}

fn write_codex_config(home: &std::path::Path, body: &str) {
    let dir = home.join(".codex");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("config.toml"), body).unwrap();
}

#[test]
fn store_mode_is_file_by_default_and_lenient() {
    let sandbox = HomeSandbox::new();
    // No ~/.codex at all → file mode.
    assert!(store_mode().is_file());
    // Explicit file → file mode.
    write_codex_config(sandbox.home(), "cli_auth_credentials_store = \"file\"\n");
    assert!(store_mode().is_file());
    // Unparseable TOML → file mode (codex's own default; doctor surfaces it).
    write_codex_config(sandbox.home(), "not [ valid toml");
    assert!(store_mode().is_file());
}

#[test]
fn store_mode_reports_non_file_modes() {
    let sandbox = HomeSandbox::new();
    for mode in ["keyring", "auto", "ephemeral"] {
        write_codex_config(
            sandbox.home(),
            &format!("cli_auth_credentials_store = \"{mode}\"\n"),
        );
        assert_eq!(store_mode(), StoreMode::Other(mode.to_string()));
    }
}

// §0.3 raw round-trip: stored and live bytes are copied verbatim — unmodeled
// fields (agent_identity here) survive because nothing reserializes.
#[test]
fn profile_and_live_writes_round_trip_bytes_exactly() {
    let _sandbox = HomeSandbox::new();
    let bytes = auth_bytes("at-alpha", "acct-alpha");
    write_profile_auth("alpha", &bytes).expect("store");
    assert_eq!(
        read_profile_auth("alpha").unwrap().as_deref(),
        Some(&bytes[..])
    );

    write_live(&bytes).expect("install");
    assert_eq!(read_live().unwrap().as_deref(), Some(&bytes[..]));

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for path in [
            profile_auth_path("alpha").unwrap(),
            live_auth_path().unwrap(),
        ] {
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "{} must be 0600", path.display());
        }
    }
}

#[test]
fn read_helpers_return_none_when_absent() {
    let _sandbox = HomeSandbox::new();
    assert!(read_live().unwrap().is_none());
    assert!(read_profile_auth("ghost").unwrap().is_none());
}

#[test]
fn live_owner_matches_by_account_id_anchor() {
    let alpha = auth_bytes("at-alpha", "acct-alpha");
    let beta = auth_bytes("at-beta-ROTATED", "acct-beta");
    let live = CodexAuthFile::parse(&auth_bytes("at-beta", "acct-beta")).unwrap();
    let candidates = [("alpha", &alpha[..]), ("beta", &beta[..])];
    // The anchor, not the token, decides ownership: beta's stored copy holds a
    // different (rotated) access token but the same account.
    assert_eq!(
        live_owner(&live, candidates).as_deref(),
        Some("beta"),
        "ownership is by account_id"
    );

    let foreign = CodexAuthFile::parse(&auth_bytes("at-x", "acct-foreign")).unwrap();
    assert!(live_owner(&foreign, candidates).is_none());

    // An unparseable candidate is skipped, never fatal.
    let garbage: &[u8] = b"not json";
    assert_eq!(
        live_owner(&live, [("bad", garbage), ("beta", &beta[..])]).as_deref(),
        Some("beta")
    );
}

// Same shape as claude.rs's quarantine test: 25 archives → newest 20 kept,
// chronological names, and the claude-side `.credentials.json` retention is
// untouched by codex archives (independent suffix filters).
#[test]
fn archive_prunes_to_the_newest_twenty() {
    let sandbox = HomeSandbox::new();
    for i in 0..25 {
        write_live(&auth_bytes(&format!("at-foreign-{i:02}"), "acct-f")).unwrap();
        archive_live_auth("live").expect("archive");
    }
    let dir = sandbox.home().join(".clauth/quarantine");
    let mut names: Vec<String> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| e.ok()?.file_name().into_string().ok())
        .filter(|n| n.ends_with(".codex-auth.json"))
        .collect();
    names.sort();
    assert_eq!(names.len(), 20, "retention keeps the newest 20");
    let newest = std::fs::read_to_string(dir.join(&names[19])).unwrap();
    assert!(newest.contains("at-foreign-24"), "newest survives");
    let oldest = std::fs::read_to_string(dir.join(&names[0])).unwrap();
    assert!(oldest.contains("at-foreign-05"), "oldest kept is #5");
}
