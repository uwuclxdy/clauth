//! The device list: its file, its checks, and the legacy import. Every test
//! that touches `~/.clauth` holds a [`HomeSandbox`], so nothing here reads or
//! writes the operator's real tree.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;

use crate::testutil::HomeSandbox;

/// A well-formed token no generator minted, for fixtures.
const TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
/// A second one, for the tests that need two.
const OTHER: &str = "fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210";

fn name(raw: &str) -> DeviceName {
    DeviceName::parse(raw).expect("a valid device name")
}

fn seed(name: &str, tier: Tier, token: &str) {
    seed_for_tests(name, tier, token).expect("seed a device");
}

fn seed_legacy(body: &str) {
    let path = legacy_path().expect("legacy path");
    crate::profile::mkdir_700(path.parent().expect("parent")).expect("mkdir");
    std::fs::write(&path, body).expect("seed auth_token.json");
}

/// An `auth_token.json` as the clauth before pairing wrote it; `tier: None` is
/// a file from before that field existed.
fn legacy_body(token: &str, tier: Option<&str>) -> String {
    let mut body = serde_json::json!({
        "schema": 1,
        "token": token,
        "created_at": "2026-01-02T03:04:05+00:00",
    });
    if let Some(tier) = tier {
        body["tier"] = tier.into();
    }
    body.to_string()
}

fn lines_containing(lines: &crate::logline::LogLines, needle: &str) -> usize {
    lines
        .snapshot()
        .iter()
        .filter(|line| line.contains(needle))
        .count()
}

/// What an import that makes `legacy` logs.
const IMPORTED: &str =
    "clauth daemon: device 'legacy' is imported (control); auth_token.json is deleted";
/// What an import of a token revoked since its import logs.
const REVOKED: &str = "clauth daemon: device 'legacy' was revoked since this token was imported, \
                       so it is not imported again; auth_token.json is deleted";

/// Run the import with every line it logs captured, and hold each line to the
/// never-log rule for the file the import read: no line carries its token or
/// its bytes.
fn import_logged() -> (anyhow::Result<()>, Vec<String>) {
    let body = std::fs::read_to_string(legacy_path().expect("path")).unwrap_or_default();
    let token = serde_json::from_str::<serde_json::Value>(&body)
        .ok()
        .and_then(|file| file["token"].as_str().map(str::to_string));
    let lines = crate::logline::LogLines::new();
    let capture = lines.capture_here();
    let result = import_legacy();
    drop(capture);
    let logged = lines.snapshot();
    for secret in [Some(body), token]
        .into_iter()
        .flatten()
        .filter(|secret| !secret.is_empty())
    {
        assert!(
            logged.iter().all(|line| !line.contains(&secret)),
            "a line the import logged carries the file's token or bytes"
        );
    }
    (result, logged)
}

// ── tokens ──────────────────────────────────────────────────────────────────

#[test]
fn a_generated_token_is_64_lowercase_hex() {
    let token = generate().expect("generate");
    assert_eq!(token.len(), 64);
    assert!(is_well_formed(&token), "{token} failed its own shape check");
}

#[test]
fn two_generations_differ() {
    // A wiring test, not a randomness one: a constant seed would pass every
    // other assertion in this file.
    assert_ne!(generate().expect("generate"), generate().expect("generate"));
}

#[test]
fn a_token_authenticates_only_as_itself() {
    let _home = HomeSandbox::new();
    seed("tray", Tier::Control, TOKEN);

    let found = authenticate(Some(TOKEN))
        .expect("read")
        .expect("the token must authenticate");
    assert_eq!((found.name.as_str(), &found.tier), ("tray", &Tier::Control));

    let mut near = TOKEN.to_string();
    near.pop();
    near.push('0');
    for wrong in [
        "",
        &TOKEN[..63],
        &format!("{TOKEN}0"),
        near.as_str(),
        &TOKEN.to_uppercase(),
    ] {
        assert!(
            authenticate(Some(wrong)).expect("read").is_none(),
            "{wrong:?} must not authenticate"
        );
    }
    assert!(
        authenticate(None).expect("read").is_none(),
        "no bearer is no device"
    );
}

/// The store holds the verifier and never the credential: a read of the file
/// yields nothing a client could present.
#[test]
fn the_store_holds_a_digest_and_never_the_token() {
    let _home = HomeSandbox::new();
    let token = add(&name("tray"), Tier::View, false).expect("add");
    let body = std::fs::read_to_string(store_path().expect("path")).expect("read the store");
    assert!(
        !body.contains(&token),
        "the plaintext token reached the store"
    );
    assert!(
        body.contains(&digest_hex(&token)),
        "the store must hold the token's digest to verify it"
    );
}

#[test]
fn a_device_rows_debug_never_renders_its_digest() {
    let device = Device::minted("tray", Tier::View, false, TOKEN, Joined::Add);
    let rendered = format!("{device:?}");
    assert!(!rendered.contains(&digest_hex(TOKEN)), "{rendered}");
    assert!(!rendered.contains(TOKEN), "{rendered}");
    assert!(rendered.contains("tray"), "{rendered}");
}

/// Unix-only: Windows has no mode bits, and `atomic_write_600` writes plainly
/// there, so there is no invariant left to assert.
#[cfg(unix)]
#[test]
fn the_store_and_its_dir_are_owner_only() {
    let _home = HomeSandbox::new();
    add(&name("tray"), Tier::View, false).expect("add");
    let dir = clauth_dir().expect("dir");
    assert!(
        store_path().expect("path").is_file(),
        "the store was written"
    );
    let loose = crate::testutil::owner_only_violations(&dir);
    assert!(
        loose.is_empty(),
        "the device list must inherit the 0600/0700 tree invariant; loose: {loose:#?}"
    );
}

// ── names ───────────────────────────────────────────────────────────────────

#[test]
fn a_device_name_takes_the_profile_charset_trimmed() {
    assert_eq!(name("  phone ").as_str(), "phone");
    assert_eq!(name("a.b-c_d@e+f").as_str(), "a.b-c_d@e+f");
    for bad in ["", "   ", "two words", ".hidden", "slash/y", "new\nline"] {
        assert!(DeviceName::parse(bad).is_err(), "{bad:?} must be refused");
    }
}

#[test]
fn the_legacy_name_is_reserved_in_any_case() {
    for reserved in ["legacy", "LEGACY", "Legacy"] {
        let err = DeviceName::parse(reserved).expect_err("the import owns this name");
        assert_eq!(
            err.to_string(),
            "'legacy' is the name clauth gives the token it imports from an older build; pick \
             another name"
        );
    }
}

#[test]
fn add_refuses_a_taken_name_in_any_case() {
    let _home = HomeSandbox::new();
    add(&name("Phone"), Tier::View, false).expect("first add");
    let err = add(&name("phone"), Tier::Control, false).expect_err("the name is taken");
    assert_eq!(
        err.to_string(),
        "a device named 'Phone' already exists; revoke it first: clauth devices revoke Phone"
    );
    assert_eq!(read_store().expect("read").devices.len(), 1);
}

// ── revoke ──────────────────────────────────────────────────────────────────

#[test]
fn a_revoked_device_no_longer_authenticates() {
    let _home = HomeSandbox::new();
    seed("tray", Tier::Control, TOKEN);
    seed("phone", Tier::View, OTHER);

    let removed = revoke("TRAY").expect("revoke folds case like the name check");
    assert_eq!(removed.name, "tray");
    assert!(authenticate(Some(TOKEN)).expect("read").is_none());
    assert!(
        authenticate(Some(OTHER)).expect("read").is_some(),
        "revoking one device leaves the others"
    );
}

#[test]
fn revoking_an_unknown_name_names_it() {
    let _home = HomeSandbox::new();
    let err = revoke("ghost").expect_err("nothing to revoke");
    assert_eq!(
        err.to_string(),
        "no device named 'ghost'; `clauth devices` lists the paired ones"
    );
}

/// The lost-token fallback with a revoke that fails pins its sentence: the loss
/// and the cause are named, and the operator is pointed at the revoke command.
/// `fail_next_write` is the crate's seam for a store write that does not land.
#[test]
fn a_lost_token_line_with_a_failed_revoke_names_the_remove_command() {
    let _home = HomeSandbox::new();
    add(&name("tray"), Tier::View, false).expect("mint the device");
    fail_next_write();
    let err = revoke_lost(&name("tray"), None).expect_err("the revoke write fails");
    assert_eq!(
        err.to_string(),
        "the token for 'tray' never reached its reader and the device could not be removed: \
         injected failure writing the device list; remove it with `clauth devices revoke tray`"
    );
}

/// The same arm with the write error present AND the revoke failing — the
/// double failure — pins its sentence by exact words: the parenthetical names
/// the write error, the rollback cause names the injected failure, and the fix
/// points at the revoke command. The revoke failure rides the same
/// `fail_next_write` seam.
#[test]
fn a_lost_token_line_with_a_write_error_and_a_failed_revoke_pins_its_sentence() {
    let _home = HomeSandbox::new();
    add(&name("tray"), Tier::View, false).expect("mint the device");
    fail_next_write();
    let write_err = std::io::Error::other("full disk");
    let err = revoke_lost(&name("tray"), Some(write_err)).expect_err("the revoke write fails");
    assert_eq!(
        err.to_string(),
        "the token for 'tray' never reached its reader (full disk) and the device could not be \
         removed: injected failure writing the device list; remove it with `clauth devices revoke tray`"
    );
}

/// The lost-token fallback with a revoke that lands pins its success-arm
/// sentence by exact words, so a reword of the approved copy cannot ship green.
#[test]
fn a_lost_token_line_with_a_removed_device_pins_its_sentence() {
    let _home = HomeSandbox::new();
    add(&name("tray"), Tier::View, false).expect("mint the device");
    let err = revoke_lost(&name("tray"), None).expect_err("the revoke removes the device");
    assert_eq!(
        err.to_string(),
        "the token for 'tray' never reached its reader; the device was removed"
    );
}

/// The same arm under a write error renders the io error's Display in a
/// parenthetical, pinned exactly against the real message the helper produces.
#[test]
fn a_lost_token_line_with_a_write_error_pins_the_cause_and_removal() {
    let _home = HomeSandbox::new();
    add(&name("tray"), Tier::View, false).expect("mint the device");
    let write_err = std::io::Error::other("full disk");
    let err =
        revoke_lost(&name("tray"), Some(write_err)).expect_err("the revoke removes the device");
    assert_eq!(
        err.to_string(),
        "the token for 'tray' never reached its reader (full disk); the device was removed"
    );
}

// ── an unreadable store ─────────────────────────────────────────────────────

/// An unreadable list is refused, never read as empty: authenticating against
/// it would lock everyone out silently, and rewriting it would destroy the
/// rows it holds.
#[test]
fn an_unreadable_store_refuses_and_is_never_rewritten() {
    let _home = HomeSandbox::new();
    let path = store_path().expect("path");
    crate::profile::mkdir_700(path.parent().expect("parent")).expect("mkdir");
    std::fs::write(&path, b"{ not json").expect("damage the store");

    assert!(authenticate(Some(TOKEN)).is_err());
    assert!(add(&name("tray"), Tier::View, false).is_err());
    assert!(revoke("tray").is_err());
    assert_eq!(
        std::fs::read(&path).expect("read"),
        b"{ not json",
        "no writer may replace a store it could not read"
    );
}

/// A string where the `schema` number belongs would otherwise quote it in the
/// parser's message; the read names the position alone.
#[test]
fn a_mistyped_store_names_the_position() {
    let _home = HomeSandbox::new();
    let path = store_path().expect("path");
    crate::profile::mkdir_700(path.parent().expect("parent")).expect("mkdir");
    std::fs::write(&path, "{\"schema\": \"sk-c2probe-NOTAKEY\"}").expect("mistyped store");

    let err = match read_store() {
        Err(e) => e,
        Ok(_) => panic!("a mistyped store must fail the read"),
    };
    let rendered = format!("{err:#}");
    assert_eq!(
        rendered,
        format!(
            "{} is not a device list clauth can read (line 1, column 31)",
            path.display()
        )
    );
    assert!(
        err.source().is_none(),
        "the parse error must not ride as a source: {err:?}"
    );
}

/// A tier and a field this build does not know survive its rewrite: the row
/// was written by a newer clauth, and a downgrade adding a device must not
/// erase what that build meant.
#[test]
fn what_a_newer_build_wrote_survives_a_rewrite() {
    let _home = HomeSandbox::new();
    let path = store_path().expect("path");
    crate::profile::mkdir_700(path.parent().expect("parent")).expect("mkdir");
    let newer = serde_json::json!({
        "schema": 2,
        "future_top": "kept",
        "devices": [{
            "name": "wall",
            "tier": "readonly",
            "digest": digest_hex(TOKEN),
            "paired_at": "2026-01-02T03:04:05+00:00",
            "joined": "qr",
            "future_row": 7,
        }],
    });
    std::fs::write(&path, newer.to_string()).expect("seed");

    add(&name("tray"), Tier::View, false).expect("add");

    let back: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).expect("read")).expect("json");
    assert_eq!(
        back["schema"], 2,
        "a newer schema is never written back down"
    );
    assert_eq!(back["future_top"], "kept");
    let wall = &back["devices"][0];
    assert_eq!(
        (&wall["tier"], &wall["joined"], &wall["future_row"]),
        (
            &serde_json::json!("readonly"),
            &serde_json::json!("qr"),
            &serde_json::json!(7)
        )
    );
    let found = authenticate(Some(TOKEN))
        .expect("read")
        .expect("the row still verifies");
    assert_eq!(found.tier, Tier::Unknown("readonly".to_string()));
}

// ── legacy import ───────────────────────────────────────────────────────────

/// The upgrade path: the bytes the tray already holds authenticate as the
/// control device `legacy`, and the plaintext is gone from disk.
#[test]
fn the_legacy_token_becomes_the_legacy_control_device() {
    let _home = HomeSandbox::new();
    seed_legacy(&legacy_body(TOKEN, Some("control")));

    let (result, logged) = import_logged();
    result.expect("import");

    let device = authenticate(Some(TOKEN))
        .expect("read")
        .expect("the tray's bytes must still work");
    assert_eq!(
        (
            device.name.as_str(),
            &device.tier,
            &device.joined,
            device.paired_at.as_str()
        ),
        (
            "legacy",
            &Tier::Control,
            &Joined::Legacy,
            "2026-01-02T03:04:05+00:00"
        )
    );
    assert!(
        !legacy_path().expect("path").exists(),
        "the plaintext token must not outlive the import"
    );
    assert!(
        !device.sessions,
        "the import never grants sessions: the grant's whole surface is the CLI verbs"
    );
    assert_eq!(logged, vec![IMPORTED]);
}

/// Every `auth_token.json` from before the `tier` field was a control token.
#[test]
fn a_legacy_file_from_before_the_tier_field_imports_as_control() {
    let _home = HomeSandbox::new();
    seed_legacy(&legacy_body(TOKEN, None));
    let (result, logged) = import_logged();
    result.expect("import");
    let device = authenticate(Some(TOKEN)).expect("read").expect("imported");
    assert_eq!(device.tier, Tier::Control);
    assert_eq!(logged, vec![IMPORTED]);
}

/// A file carrying any tier but `control` is neither imported nor served, and
/// stays where it is for the clauth that wrote it: `view` as much as a tier
/// this build does not know.
#[test]
fn a_legacy_file_of_any_tier_but_control_is_neither_imported_nor_served() {
    for tier in ["view", "readonly"] {
        let _home = HomeSandbox::new();
        let body = legacy_body(TOKEN, Some(tier));
        seed_legacy(&body);

        let (result, logged) = import_logged();
        result.expect("a refused tier is not a failed start");

        assert!(authenticate(Some(TOKEN)).expect("read").is_none(), "{tier}");
        assert!(
            !store_path().expect("path").exists(),
            "nothing was imported, so nothing was written"
        );
        assert_eq!(
            std::fs::read_to_string(legacy_path().expect("path")).expect("the file stays"),
            body
        );
        assert_eq!(
            logged,
            vec![format!(
                "clauth daemon: auth_token.json carries tier {tier:?} rather than control, so \
                 it is neither imported nor served; run the clauth that wrote it, or delete \
                 the file"
            )],
            "the operator is told once why the token stopped working"
        );
    }
}

#[test]
fn an_unusable_legacy_file_is_not_imported_and_says_so_once() {
    for bad in [
        "not json".to_string(),
        legacy_body("short", Some("control")),
        legacy_body(&TOKEN.to_uppercase(), Some("control")),
    ] {
        let _home = HomeSandbox::new();
        seed_legacy(&bad);

        let (result, logged) = import_logged();
        result.expect("an unusable file is not a failed start");

        assert!(
            !store_path().expect("path").exists(),
            "{bad:?} must import nothing"
        );
        assert!(legacy_path().expect("path").exists(), "{bad:?} stays put");
        assert_eq!(
            logged,
            vec![
                "clauth daemon: auth_token.json holds no usable token (bad JSON, or a token that \
                 is not 64 lowercase hex characters), so it is not imported; pair the client \
                 again with `clauth devices pair <name>` and delete the file"
            ],
            "{bad:?}"
        );
    }
}

/// A downgraded clauth minted a fresh `auth_token.json` after the import
/// deleted the old one; the next import hands `legacy` that token, the one the
/// client was given last.
#[test]
fn a_downgrade_minted_legacy_file_takes_over_the_legacy_device() {
    let _home = HomeSandbox::new();
    seed_legacy(&legacy_body(TOKEN, Some("control")));
    let (result, logged) = import_logged();
    result.expect("first import");
    assert_eq!(logged, vec![IMPORTED]);

    seed_legacy(&legacy_body(OTHER, Some("control")));
    let (result, logged) = import_logged();
    result.expect("second import");

    assert!(
        authenticate(Some(TOKEN)).expect("read").is_none(),
        "the old bytes are retired"
    );
    let device = authenticate(Some(OTHER))
        .expect("read")
        .expect("the downgrade's token works");
    assert_eq!(
        (device.name.as_str(), &device.tier),
        ("legacy", &Tier::Control)
    );
    assert_eq!(
        read_store().expect("read").devices.len(),
        1,
        "one legacy device, never two"
    );
    assert!(!legacy_path().expect("path").exists());
    assert_eq!(
        logged,
        vec![
            "clauth daemon: device 'legacy' now holds the token a downgraded clauth minted; \
             auth_token.json is deleted"
        ]
    );
}

/// The ordering: the list is on disk before the plaintext goes. A write that
/// fails must leave the client's only copy of its credential where it was.
#[test]
fn a_failed_store_write_keeps_the_legacy_file() {
    let _home = HomeSandbox::new();
    seed_legacy(&legacy_body(TOKEN, Some("control")));

    fail_next_write();
    let err = import_logged()
        .0
        .expect_err("the injected write failure propagates");
    assert!(
        format!("{err:#}").contains("injected failure writing the device list"),
        "{err:#}"
    );
    assert!(
        legacy_path().expect("path").exists(),
        "the plaintext must survive a write that did not land"
    );
    let (result, logged) = import_logged();
    result.expect("a later start retries");
    assert!(authenticate(Some(TOKEN)).expect("read").is_some());
    assert_eq!(logged, vec![IMPORTED]);
}

/// A crash between the write and the delete leaves both; the next import only
/// finishes the delete.
#[test]
fn an_interrupted_import_finishes_on_the_next_start() {
    let _home = HomeSandbox::new();
    let body = legacy_body(TOKEN, Some("control"));
    seed_legacy(&body);
    import_logged().0.expect("import");
    let store = std::fs::read(store_path().expect("path")).expect("read");
    seed_legacy(&body);

    let (result, logged) = import_logged();
    result.expect("re-import");

    assert_eq!(
        std::fs::read(store_path().expect("path")).expect("read"),
        store,
        "nothing to rewrite"
    );
    assert!(!legacy_path().expect("path").exists());
    assert_eq!(
        logged,
        vec!["clauth daemon: device 'legacy' already held its token; auth_token.json is deleted"]
    );
}

#[test]
fn no_legacy_file_is_no_import() {
    let _home = HomeSandbox::new();
    import_legacy().expect("nothing to import");
    assert!(!store_path().expect("path").exists());
}

// ── listing ─────────────────────────────────────────────────────────────────

#[test]
fn the_json_list_is_a_fixed_field_set_with_no_digest() {
    let _home = HomeSandbox::new();
    seed("tray", Tier::Control, TOKEN);
    let devices = read_store().expect("read").devices;
    let rows: serde_json::Value = serde_json::from_str(&list_json(&devices)).expect("json");
    assert_eq!(
        rows,
        serde_json::json!([{
            "name": "tray",
            "tier": "control",
            "paired_at": devices[0].paired_at,
            "joined": "add",
            "sessions": false,
        }])
    );
}

#[test]
fn the_table_pairs_the_local_stamp_with_its_age() {
    let device = Device {
        paired_at: "2026-01-02T03:04:05+00:00".to_string(),
        ..Device::minted("phone", Tier::View, false, TOKEN, Joined::Pair)
    };
    let epoch = iso_to_epoch_secs(&device.paired_at).expect("parse");
    let stamp = crate::format::local_stamp(epoch).expect("stamp");
    let table = render_table(&[device], epoch + 3 * 3600 + 300);
    let cell = format!("{stamp} (3h ago)");
    assert_eq!(
        table,
        format!(
            "{:<5}  {:<4}  {:<width$}  {:<6}  SESSIONS\nphone  view  {cell}  pair    no\n",
            "NAME",
            "TIER",
            "PAIRED AT",
            "JOINED",
            width = cell.chars().count()
        )
    );
}

/// A column sizes and pads by drawn cells: a device name of four wide glyphs
/// (eight cells) lays the table out exactly as an eight-letter ASCII name does.
#[test]
fn a_wide_script_device_name_pads_like_its_ascii_twin() {
    const WIDE: &str = "漢字漢字";
    const TWIN: &str = "abcdefgh";
    let table = |first: &str| {
        let device = |name: &str| Device {
            paired_at: "2026-01-02T03:04:05+00:00".to_string(),
            ..Device::minted(name, Tier::View, false, TOKEN, Joined::Pair)
        };
        let epoch = iso_to_epoch_secs("2026-01-02T03:04:05+00:00").expect("parse");
        render_table(&[device(first), device("tray")], epoch + 300)
    };
    let twin = table(TWIN);
    assert!(
        twin.contains(TWIN),
        "the twin table renders the twin: {twin}"
    );
    assert_eq!(table(WIDE), twin.replace(TWIN, WIDE));
}

#[test]
fn an_unparseable_stamp_renders_as_no_data() {
    assert_eq!(paired_cell("synthetic-fixture", 0), "-");
}

/// The paired cell drops the age half once the age would be a date (30 days
/// and past): the full local stamp already carries the date, so a second copy
/// reads as a duplicate. Younger rows keep `stamp (age)` (`4w ago` at 29 days).
#[test]
fn the_paired_cell_drops_the_age_half_once_it_would_be_a_date() {
    let iso = "2026-01-02T03:04:05+00:00";
    let epoch = iso_to_epoch_secs(iso).expect("parse");
    let stamp = crate::format::local_stamp(epoch).expect("stamp");
    assert_eq!(
        paired_cell(iso, epoch + 29 * 86_400),
        format!("{stamp} (4w ago)")
    );
    assert_eq!(paired_cell(iso, epoch + 30 * 86_400), stamp);
}

#[test]
fn an_empty_list_names_the_pair_command() {
    assert_eq!(
        render_table(&[], 0),
        "no devices are paired. `clauth devices pair <name>` pairs one.\n"
    );
}

#[test]
fn an_empty_store_is_announced_at_listener_start() {
    let _home = HomeSandbox::new();
    let lines = crate::logline::LogLines::new();
    let _capture = lines.capture_here();
    note_at_start();
    assert_eq!(lines_containing(&lines, "no device is paired yet"), 1);
}

/// A list that does not read is announced at listener start with the condition
/// that lifts the refusal, in the words the per-request line uses.
#[test]
fn an_unreadable_store_is_announced_at_listener_start() {
    let _home = HomeSandbox::new();
    let path = store_path().expect("path");
    crate::profile::mkdir_700(path.parent().expect("parent")).expect("mkdir");
    std::fs::write(&path, b"{ not json").expect("damage the store");
    let Err(err) = read_store() else {
        panic!("a damaged list must not read");
    };
    let lines = crate::logline::LogLines::new();
    let _capture = lines.capture_here();

    note_at_start();

    assert_eq!(
        lines.snapshot(),
        vec![format!(
            "clauth daemon: every REST request is refused until the device list reads: {err:#}"
        )]
    );
}

/// A legacy token revoked after its import stays revoked, even when the import
/// could not delete `auth_token.json`: the device list remembers which token it
/// imported, and a file still holding that token is not imported again. A
/// different token in the file is a downgrade's fresh mint, and imports.
#[test]
fn a_revoked_legacy_token_is_not_imported_again() {
    let _home = HomeSandbox::new();
    let body = legacy_body(TOKEN, Some("control"));
    seed_legacy(&body);
    import_logged().0.expect("import");
    // The state a failed import delete and a failed revoke delete leave: the
    // list remembers the import and the plaintext is still on disk.
    seed_legacy(&body);
    fail_next_legacy_delete();
    let err = revoke("legacy").expect_err("the revoke's own delete fails");
    assert!(err.downcast_ref::<LegacyLeft>().is_some(), "{err:#}");

    let (result, logged) = import_logged();
    result.expect("the next start");

    assert!(
        authenticate(Some(TOKEN)).expect("read").is_none(),
        "a revoked token must stay revoked"
    );
    assert!(
        !legacy_path().expect("path").exists(),
        "the revoked plaintext is deleted"
    );
    assert_eq!(logged, vec![REVOKED]);

    seed_legacy(&legacy_body(OTHER, Some("control")));
    let (result, logged) = import_logged();
    result.expect("a downgrade's fresh token");
    let device = authenticate(Some(OTHER))
        .expect("read")
        .expect("the fresh token imports");
    assert_eq!(device.name, "legacy");
    assert_eq!(logged, vec![IMPORTED]);
}

/// The downgrade arm records the token it hands `legacy` too: once that token
/// is revoked, a copy of it a failed delete left behind is not imported again.
#[test]
fn a_revoked_token_a_downgrade_handed_legacy_is_not_imported_again() {
    let _home = HomeSandbox::new();
    seed_legacy(&legacy_body(TOKEN, Some("control")));
    import_logged().0.expect("the upgrade's import");
    let downgrade = legacy_body(OTHER, Some("control"));
    seed_legacy(&downgrade);
    import_logged().0.expect("the downgrade's token takes over");
    // The state a failed import delete and a failed revoke delete leave: the
    // list holds the downgrade's token and its plaintext is still on disk.
    seed_legacy(&downgrade);
    fail_next_legacy_delete();
    let err = revoke("legacy").expect_err("the revoke's own delete fails");
    assert!(err.downcast_ref::<LegacyLeft>().is_some(), "{err:#}");

    let (result, logged) = import_logged();
    result.expect("the next start");

    assert!(
        authenticate(Some(OTHER)).expect("read").is_none(),
        "a revoked token must stay revoked"
    );
    assert!(
        !legacy_path().expect("path").exists(),
        "the revoked plaintext is deleted"
    );
    assert_eq!(logged, vec![REVOKED]);
}

/// Run `clauth devices revoke <name>` with its stdout captured.
fn run_revoke_printed(name: &str) -> (anyhow::Result<()>, String) {
    let out = crate::out::OutCapture::new();
    let guard = out.capture_here();
    let result = run_revoke(name);
    drop(guard);
    (
        result,
        String::from_utf8(out.snapshot()).expect("utf-8 stdout"),
    )
}

/// The revoke itself deletes a plaintext copy of the token it revoked, which a
/// failed import delete left behind: a pre-pairing clauth started before the
/// next `--listen` start would otherwise still serve that token.
#[test]
fn revoking_legacy_deletes_the_plaintext_copy_of_its_token() {
    let _home = HomeSandbox::new();
    let body = legacy_body(TOKEN, Some("control"));
    seed_legacy(&body);
    import_logged().0.expect("import");
    // The state a failed import delete leaves.
    seed_legacy(&body);

    let (result, printed) = run_revoke_printed("legacy");
    result.expect("revoke");

    assert!(
        !legacy_path().expect("path").exists(),
        "the revoke deletes the plaintext copy of the token it revoked"
    );
    assert!(authenticate(Some(TOKEN)).expect("read").is_none());
    assert_eq!(
        printed,
        "clauth: revoked device 'legacy'; its next request is refused.\n\
         clauth: deleted ~/.clauth/auth_token.json, which still held its token.\n"
    );
}

/// A file holding another token is a downgrade's fresh mint, not the revoked
/// one: the revoke leaves it for the next start's import and says nothing.
#[test]
fn revoking_legacy_keeps_a_file_holding_another_token() {
    let _home = HomeSandbox::new();
    seed_legacy(&legacy_body(TOKEN, Some("control")));
    import_logged().0.expect("import");
    let fresh = legacy_body(OTHER, Some("control"));
    seed_legacy(&fresh);

    let (result, printed) = run_revoke_printed("legacy");
    result.expect("revoke");

    assert_eq!(
        std::fs::read_to_string(legacy_path().expect("path")).expect("the file stays"),
        fresh
    );
    assert_eq!(
        printed,
        "clauth: revoked device 'legacy'; its next request is refused.\n"
    );
}

/// A delete that fails leaves the device revoked and the command failing, its
/// error naming the file an older clauth would still serve.
#[test]
fn a_failed_plaintext_delete_fails_the_revoke_and_names_the_file() {
    let _home = HomeSandbox::new();
    let body = legacy_body(TOKEN, Some("control"));
    seed_legacy(&body);
    import_logged().0.expect("import");
    seed_legacy(&body);
    fail_next_legacy_delete();

    let (result, printed) = run_revoke_printed("legacy");
    let err = result.expect_err("a plaintext token left behind fails the command");

    assert_eq!(
        format!("{err:#}"),
        "revoked device 'legacy', but ~/.clauth/auth_token.json still holds its token and \
         could not be deleted (injected failure deleting auth_token.json); an older clauth \
         would still accept it, so delete the file by hand"
    );
    assert_eq!(printed, "");
    assert!(authenticate(Some(TOKEN)).expect("read").is_none());
    assert_eq!(
        std::fs::read_to_string(legacy_path().expect("path")).expect("the file stays"),
        body
    );
    assert!(
        !FAIL_NEXT_LEGACY_DELETE.load(std::sync::atomic::Ordering::Acquire),
        "the revoke reached the delete"
    );
}

fn damage_the_push_store() {
    let home = profile_home();
    std::fs::write(
        home.join(".clauth").join("push-subscriptions.json"),
        b"{ not json",
    )
    .expect("damage the subscription list");
}

fn profile_home() -> std::path::PathBuf {
    legacy_path()
        .expect("path")
        .parent()
        .and_then(std::path::Path::parent)
        .expect("home")
        .to_path_buf()
}

/// A push row that cannot be dropped never keeps the plaintext token on disk:
/// both leftovers are tried, and the deletion is said before the push error.
#[test]
fn a_failed_push_drop_still_deletes_the_plaintext_copy() {
    let _home = HomeSandbox::new();
    let body = legacy_body(TOKEN, Some("control"));
    seed_legacy(&body);
    import_logged().0.expect("import");
    seed_legacy(&body);
    damage_the_push_store();

    let (result, printed) = run_revoke_printed("legacy");
    let err = result.expect_err("the push row could not be dropped");

    assert!(
        err.downcast_ref::<super::super::push::SubscriptionLeft>()
            .is_some(),
        "the push leftover keeps its type: {err:#}"
    );
    assert!(
        !legacy_path().expect("path").exists(),
        "the plaintext copy is deleted even though the push drop failed"
    );
    assert_eq!(
        printed,
        "clauth: revoked device 'legacy'; its next request is refused.\n\
         clauth: deleted ~/.clauth/auth_token.json, which still held its token.\n"
    );
    assert!(authenticate(Some(TOKEN)).expect("read").is_none());
}

/// Both leftovers failing: the token file's sentence leads with its instruction,
/// then the push row's, each cause in its own place.
#[test]
fn both_leftovers_failing_name_the_file_first() {
    let _home = HomeSandbox::new();
    let body = legacy_body(TOKEN, Some("control"));
    seed_legacy(&body);
    import_logged().0.expect("import");
    seed_legacy(&body);
    damage_the_push_store();
    fail_next_legacy_delete();

    let (result, printed) = run_revoke_printed("legacy");
    let text = format!("{:#}", result.expect_err("both leftovers failed"));

    let head = "revoked device 'legacy', but ~/.clauth/auth_token.json still holds its token \
                and could not be deleted (injected failure deleting auth_token.json); an older \
                clauth would still accept it, so delete the file by hand; the device's push \
                subscription could not be dropped either (";
    let tail = "): it receives no push, since nothing is pushed to a pairing the device list no \
                longer holds, and the first subscription write after the list reads again drops \
                it; to clear it now, move ~/.clauth/push-subscriptions.json aside, and every \
                subscribed device then subscribes again";
    assert!(text.starts_with(head), "{text}");
    assert!(text.ends_with(tail), "{text}");
    let cause = &text[head.len()..text.len() - tail.len()];
    assert!(
        !cause.is_empty() && !cause.contains("auth_token"),
        "the push cause sits alone between them: {cause:?}"
    );
    assert_eq!(printed, "");
    assert_eq!(
        std::fs::read_to_string(legacy_path().expect("path")).expect("the file stays"),
        body
    );
}

/// A file that exists but cannot be read may still hold the revoked token: the
/// revoke fails naming it rather than passing over it.
#[test]
fn an_unreadable_plaintext_file_fails_the_revoke() {
    let _home = HomeSandbox::new();
    seed_legacy(&legacy_body(TOKEN, Some("control")));
    import_logged().0.expect("import");
    // A directory in the file's place reads as an error on every platform.
    let path = legacy_path().expect("path");
    std::fs::create_dir(&path).expect("a directory where the file was");

    let (result, printed) = run_revoke_printed("legacy");
    let err = result.expect_err("an unreadable file fails the command");

    assert_eq!(
        format!("{err:#}"),
        format!(
            "revoked device 'legacy', but ~/.clauth/auth_token.json could not be read ({}), \
             so clauth cannot tell whether it still holds that token; check the file and \
             delete it if it does",
            crate::testutil::directory_read_error(&path)
        )
    );
    assert_eq!(printed, "");
    assert!(authenticate(Some(TOKEN)).expect("read").is_none());
    assert!(path.is_dir(), "nothing is deleted");
}

/// An unreadable file and a push row that stayed: the file's sentence first,
/// then the device's push row, each cause in its own place.
#[test]
fn an_unreadable_file_and_a_failed_push_drop_name_both() {
    let _home = HomeSandbox::new();
    seed_legacy(&legacy_body(TOKEN, Some("control")));
    import_logged().0.expect("import");
    let path = legacy_path().expect("path");
    std::fs::create_dir(&path).expect("a directory where the file was");
    damage_the_push_store();

    let (result, printed) = run_revoke_printed("legacy");
    let text = format!("{:#}", result.expect_err("both leftovers failed"));

    let head = format!(
        "revoked device 'legacy', but ~/.clauth/auth_token.json could not be read ({}), so \
         clauth cannot tell whether it still holds that token; check the file and delete it if \
         it does; the device's push subscription could not be dropped either (",
        crate::testutil::directory_read_error(&path)
    );
    let tail = "): it receives no push, since nothing is pushed to a pairing the device list no \
                longer holds, and the first subscription write after the list reads again drops \
                it; to clear it now, move ~/.clauth/push-subscriptions.json aside, and every \
                subscribed device then subscribes again";
    assert!(text.starts_with(&head), "{text}");
    assert!(text.ends_with(tail), "{text}");
    assert!(text.len() > head.len() + tail.len(), "{text}");
    assert_eq!(printed, "");
    assert!(path.is_dir(), "nothing is deleted");
}

/// Of the paired devices, only the one the list imported can have its token in
/// `auth_token.json`, so revoking any other never reads the file: an unreadable
/// one is no leftover of theirs.
#[test]
fn revoking_another_device_leaves_an_unreadable_plaintext_file_alone() {
    let _home = HomeSandbox::new();
    seed_legacy(&legacy_body(TOKEN, Some("control")));
    import_logged().0.expect("import");
    seed("phone", Tier::View, OTHER);
    let path = legacy_path().expect("path");
    std::fs::create_dir(&path).expect("a directory where the file was");

    let (result, printed) = run_revoke_printed("phone");
    result.expect("the phone's token was never in the file");

    assert_eq!(
        printed,
        "clauth: revoked device 'phone'; its next request is refused.\n"
    );
    assert!(path.is_dir(), "nothing is deleted");
}

// ── sessions grant ─────────────────────────────────────────────────────────

/// A row written before the sessions field existed loads with the grant off,
/// and the next rewrite writes the field typed — never swallowed into the
/// row's carried-extra map.
#[test]
fn a_pre_sessions_row_loads_false_and_reserializes_the_field() {
    let _home = HomeSandbox::new();
    let path = store_path().expect("path");
    crate::profile::mkdir_700(path.parent().expect("parent")).expect("mkdir");
    let older = serde_json::json!({
        "schema": 1,
        "devices": [{
            "name": "wall",
            "tier": "control",
            "digest": digest_hex(TOKEN),
            "paired_at": "2026-01-02T03:04:05+00:00",
            "joined": "add",
        }],
    });
    std::fs::write(&path, older.to_string()).expect("seed");

    let device = authenticate(Some(TOKEN))
        .expect("read")
        .expect("the row still verifies");
    assert!(!device.sessions, "a pre-field row loads with the grant off");
    assert!(
        !device.extra.contains_key("sessions"),
        "the flatten map must not swallow the typed field"
    );

    add(&name("tray"), Tier::View, false).expect("add");

    let back: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).expect("read")).expect("json");
    assert_eq!(
        back["devices"][0]["sessions"],
        serde_json::json!(false),
        "the rewrite writes the field, typed"
    );
}

#[test]
fn add_with_sessions_persists_the_grant() {
    let _home = HomeSandbox::new();
    add(&name("tray"), Tier::Control, true).expect("add");
    let store = read_store().expect("read");
    assert_eq!(
        (
            store.devices[0].name.as_str(),
            store.devices[0].tier.as_str(),
            store.devices[0].sessions,
        ),
        ("tray", "control", true)
    );
}

#[test]
fn run_add_with_sessions_persists_the_grant() {
    let _home = HomeSandbox::new();
    run_add("tray", true, true).expect("run add");
    let store = read_store().expect("read");
    assert_eq!(
        (store.devices[0].name.as_str(), store.devices[0].sessions),
        ("tray", true)
    );
}

#[test]
fn allow_sessions_grants_a_control_device_and_persists() {
    let _home = HomeSandbox::new();
    seed("tray", Tier::Control, TOKEN);
    let (name, granted) = allow_sessions("tray").expect("grant");
    assert_eq!((name.as_str(), granted), ("tray", true));
    let store = read_store().expect("read");
    assert!(store.devices[0].sessions, "the grant persists");
    let rows: serde_json::Value = serde_json::from_str(&list_json(&store.devices)).expect("json");
    assert_eq!(rows[0]["sessions"], serde_json::json!(true));
}

#[test]
fn allow_sessions_refuses_a_view_device() {
    let _home = HomeSandbox::new();
    seed("tray", Tier::View, TOKEN);
    let err = allow_sessions("tray").expect_err("a view device cannot mint sessions");
    assert_eq!(
        err.to_string(),
        "a device without the control tier cannot mint sessions; revoke 'tray' and re-pair it \
         with --control"
    );
}

/// A row a newer clauth paired carries a tier this build does not know; the
/// refusal names the control tier, not "view", so it holds for both.
#[test]
fn allow_sessions_refuses_a_device_of_an_unknown_tier() {
    let _home = HomeSandbox::new();
    let path = store_path().expect("path");
    crate::profile::mkdir_700(path.parent().expect("parent")).expect("mkdir");
    let store = serde_json::json!({
        "schema": 1,
        "devices": [{
            "name": "future",
            "tier": "admin",
            "digest": digest_hex(TOKEN),
            "paired_at": "2026-01-02T03:04:05+00:00",
            "joined": "pair",
        }],
    });
    std::fs::write(&path, store.to_string()).expect("seed");

    let err = allow_sessions("future").expect_err("an unknown tier cannot mint sessions");
    assert_eq!(
        err.to_string(),
        "a device without the control tier cannot mint sessions; revoke 'future' and re-pair it \
         with --control"
    );
}

/// The two verbs share one missing-name sentence, byte for byte, and both find
/// a name whose argv is padded — the `trim()` and the sentence are one path.
#[test]
fn revoke_and_allow_sessions_share_the_missing_name_sentence_and_trim() {
    let _home = HomeSandbox::new();
    seed("tray", Tier::Control, TOKEN);

    let from_revoke = revoke(" ghost ")
        .expect_err("no device holds that name")
        .to_string();
    let from_allow = allow_sessions(" ghost ")
        .expect_err("no device holds that name")
        .to_string();
    assert_eq!(
        from_revoke, from_allow,
        "both verbs share the missing-name sentence"
    );
    assert_eq!(
        from_revoke,
        "no device named ' ghost '; `clauth devices` lists the paired ones"
    );

    // A padded, case-folded name resolves on both verbs.
    let (name, granted) = allow_sessions(" TRAY ").expect("padded grant");
    assert_eq!((name.as_str(), granted), ("tray", true));
}

/// The already-granted arm is a no-op: no rewrite, so a write failure cannot
/// turn the no-op's exit 0 into an exit 1. `fail_next_write` is the
/// platform-free probe: armed before the call, still armed after it.
#[test]
fn allow_sessions_on_a_granted_device_is_a_no_op() {
    let _home = HomeSandbox::new();
    seed("tray", Tier::Control, TOKEN);
    let (_, granted) = allow_sessions("tray").expect("grant");
    assert!(granted);
    let path = store_path().expect("path");
    let before = std::fs::read(&path).expect("read");
    fail_next_write();
    let (name, granted) = allow_sessions("TRAY").expect("second grant");
    assert_eq!((name.as_str(), granted), ("tray", false));
    let after = std::fs::read(&path).expect("read");
    assert_eq!(before, after, "the no-op must not rewrite the store");
    assert!(
        FAIL_NEXT_WRITE.swap(false, std::sync::atomic::Ordering::AcqRel),
        "the no-op must not call write_store"
    );
}

#[test]
fn the_table_lists_sessions_yes_or_no() {
    let mut granted = Device::minted("tray", Tier::Control, true, TOKEN, Joined::Add);
    let mut ungranted_control = Device::minted("phone", Tier::Control, false, OTHER, Joined::Pair);
    let mut ungranted_view = Device::minted("wall", Tier::View, false, TOKEN, Joined::Add);
    let iso = "2026-01-02T03:04:05+00:00";
    granted.paired_at = iso.to_string();
    ungranted_control.paired_at = iso.to_string();
    ungranted_view.paired_at = iso.to_string();
    let now = iso_to_epoch_secs(iso).expect("parse") + 3 * 3600 + 300;
    let cell = paired_cell(iso, now);
    let table = render_table(&[granted, ungranted_control, ungranted_view], now);
    assert_eq!(
        table,
        format!(
            "{:<5}  {:<7}  {:<width$}  {:<6}  SESSIONS\n\
             tray   control  {cell}  add     yes\n\
             phone  control  {cell}  pair    no\n\
             wall   view     {cell}  add     no\n",
            "NAME",
            "TIER",
            "PAIRED AT",
            "JOINED",
            width = cell.chars().count()
        )
    );
}
