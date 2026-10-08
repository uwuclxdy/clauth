#![allow(clippy::unwrap_used, clippy::expect_used)]
//! The client-token engine: the owner-only store, the add's mint and its
//! refusals, the last-token guard on the remove, and the require op through
//! the checked write path. Every test holds a `HomeSandbox`; the require
//! tests drive a stub `shunt` written into it, a `/bin/sh` script, so they
//! are `#[cfg(unix)]`.

use std::fs;

use super::*;
use crate::testutil::{EnvPin, HomeSandbox, no_shunt_env};

/// A record adopting `<home>/etc/shunt.toml` holding `config`, with an env
/// file holding `env_text`.
fn adopted(home: &HomeSandbox, config: &str, env_text: &str) -> GatewayRecord {
    let etc = home.home().join("etc");
    fs::create_dir_all(&etc).expect("etc");
    let path = etc.join("shunt.toml");
    fs::write(&path, config).expect("config");
    let env_file = home.home().join("tokens.env");
    fs::write(&env_file, env_text).expect("env file");
    let mut record = GatewayRecord::new(path).expect("adoptable");
    record.env_file = Some(env_file);
    record
}

fn store_text() -> Option<String> {
    fs::read_to_string(store_path().expect("path")).ok()
}

fn stored_names() -> Vec<String> {
    ClientTokens::load()
        .expect("load")
        .names()
        .map(str::to_string)
        .collect()
}

fn add(record: &GatewayRecord, profile: &str) -> String {
    add_client_token(record, profile)
        .expect("add")
        .expose()
        .to_string()
}

/// The add mints 32 CSPRNG bytes, hex, into an owner-only store keyed by
/// the profile and hands the token back; a second add for the profile
/// replaces it. The store's `Debug` names profiles alone.
#[test]
fn an_added_token_lands_in_an_owner_only_store_and_a_re_add_replaces_it() {
    let home = HomeSandbox::new();
    let _env = no_shunt_env(&home);
    let record = adopted(&home, "[server]\n", "");
    let first = add(&record, "kerry");
    assert_eq!(first.len(), 64, "32 bytes, hex");
    assert!(
        first
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        "lowercase hex"
    );
    assert_eq!(
        store_text(),
        Some(format!("[tokens]\nkerry = \"{first}\"\n"))
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = fs::metadata(store_path().expect("path"))
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "the store is owner-only");
    }

    let second = add(&record, "kerry");
    assert_ne!(second, first, "a fresh mint");
    assert_eq!(
        store_text(),
        Some(format!("[tokens]\nkerry = \"{second}\"\n")),
        "the re-add replaced the token"
    );
    assert_eq!(
        format!("{:?}", ClientTokens::load().expect("load")),
        r#"ClientTokens { profiles: ["kerry"] }"#
    );
}

/// A name shunt cannot carry as a client name refuses before any mint.
/// Every creation path runs `actions::validate_name_chars`, whose charset
/// holds none of these, so only a name from elsewhere (a hand-edited
/// `profiles.toml`) reaches this guard.
#[test]
fn a_name_shunt_cannot_carry_refuses_before_any_mint() {
    let home = HomeSandbox::new();
    let _env = no_shunt_env(&home);
    let record = adopted(&home, "[server]\n", "");
    for name in ["", "  ", " kerry", "kerry\t", "a:b", "a,b"] {
        assert_eq!(
            add_client_token(&record, name)
                .map(|_| ())
                .map_err(|e| format!("{e:#}")),
            Err(format!(
                "cannot add a client token for profile {name:?}: shunt cannot carry that as a client name, which must not be empty, hold ':' or ',', or start or end with whitespace"
            ))
        );
    }
    assert_eq!(store_text(), None, "nothing was written");
}

/// An add for a name the env file's own value already uses refuses, naming
/// the name, the variable and the env file, never a value.
#[test]
fn an_add_for_a_name_the_env_file_holds_refuses_naming_the_env_file() {
    let home = HomeSandbox::new();
    let _env = no_shunt_env(&home);
    let record = adopted(&home, "[server]\n", "SHUNT_CLIENT_TOKENS=alice:a1-secret\n");
    assert_eq!(
        add_client_token(&record, "alice")
            .map(|_| ())
            .map_err(|e| format!("{e:#}")),
        Err(format!(
            "cannot add a client token for profile \"alice\": the gateway's env file {} already names a client \"alice\" in SHUNT_CLIENT_TOKENS, and shunt refuses a duplicate name; remove that entry from the env file first",
            record.env_file.as_deref().expect("env file").display()
        ))
    );
    assert_eq!(store_text(), None, "nothing was written");
    add(&record, "kerry");
    assert_eq!(stored_names(), ["kerry"]);
}

/// A store clauth cannot use refuses without quoting it: a parse failure
/// names the file alone (the parser's own message quotes the line, a
/// token), and a hand-edited entry shunt's grammar cannot carry fails the
/// spawn value naming its profile.
#[test]
fn a_store_clauth_cannot_use_refuses_naming_the_file_never_a_value() {
    let home = HomeSandbox::new();
    let _env = no_shunt_env(&home);
    let path = store_path().expect("path");
    fs::create_dir_all(path.parent().expect("parent")).expect("dir");
    fs::write(&path, "[tokens]\nkerry = \"tok-secret\n").expect("store");
    let mut got = vec![(
        "unparseable",
        ClientTokens::load()
            .map(|_| String::new())
            .map_err(|e| format!("{e:#}")),
    )];
    let mut want = vec![(
        "unparseable",
        Err(format!(
            "clauth's client-token store {} does not parse; delete it and add the tokens again",
            path.display()
        )),
    )];
    for token in ["a,b", " padded", ""] {
        fs::write(&path, format!("[tokens]\nkerry = {token:?}\n")).expect("store");
        let tokens = ClientTokens::load().expect("parses");
        got.push((
            token,
            tokens
                .spawn_value(None, "SHUNT_CLIENT_TOKENS", true)
                .map_err(|e| format!("{e:#}")),
        ));
        want.push((
            token,
            Err(format!(
                "clauth's client-token store {} holds an entry for profile \"kerry\" that shunt cannot read; remove that profile's token and add it again",
                path.display()
            )),
        ));
    }
    assert_eq!(got, want);
}

/// Removing the last token refuses exactly while the gateway requires one
/// (`[server.auth]` in the config or built by the env layer, no
/// `[[server.auth.jwt]]` entry) and the env file's value holds no pair.
#[test]
fn removing_the_last_token_refuses_only_while_the_gateway_would_run_open() {
    const LAST: &str = "cannot remove the last client token while the gateway requires tokens; add another token or remove [server.auth] from the config first";
    let jwt =
        "[server.auth]\n[[server.auth.jwt]]\nissuer = \"https://idp.example\"\naudience = \"a\"\n";
    let cases = [
        (
            "[server.auth]\n",
            "",
            &["kerry"][..],
            "kerry",
            Err(LAST),
            &["kerry"][..],
            "the table, nothing else",
        ),
        (
            "[server]\n",
            "SHUNT_SERVER__AUTH__HEADER=x-token\n",
            &["kerry"][..],
            "kerry",
            Err(LAST),
            &["kerry"][..],
            "the env layer builds the table",
        ),
        (
            "[server]\n",
            "",
            &["kerry"][..],
            "kerry",
            Ok(true),
            &[][..],
            "no table: the gateway runs open",
        ),
        (
            jwt,
            "",
            &["kerry"][..],
            "kerry",
            Ok(true),
            &[][..],
            "a jwt issuer",
        ),
        (
            "[server.auth]\n",
            "SHUNT_CLIENT_TOKENS=alice:a1\n",
            &["kerry"][..],
            "kerry",
            Ok(true),
            &[][..],
            "an env-file pair stays",
        ),
        (
            "[server.auth]\n",
            "",
            &["bob", "kerry"][..],
            "kerry",
            Ok(true),
            &["bob"][..],
            "another stored token stays",
        ),
        (
            "[server.auth]\n",
            "",
            &["kerry"][..],
            "nobody",
            Ok(false),
            &["kerry"][..],
            "a profile holding no token",
        ),
    ];
    // Every case's outcome and the store it leaves, compared in one
    // equality, so one wrong case does not hide another.
    let mut got = Vec::new();
    let mut want = Vec::new();
    for (config, env_text, held, remove, outcome, left, case) in cases {
        let home = HomeSandbox::new();
        let _env = no_shunt_env(&home);
        let record = adopted(&home, "[server]\n", "");
        for profile in held {
            add(&record, profile);
        }
        let record = adopted(&home, config, env_text);
        let outcome_now = remove_client_token(&record, remove).map_err(|e| format!("{e:#}"));
        got.push((case, outcome_now, stored_names()));
        want.push((
            case,
            outcome.map_err(str::to_string),
            left.iter()
                .map(|name| (*name).to_string())
                .collect::<Vec<_>>(),
        ));
    }
    assert_eq!(got, want);
}

// ── require client tokens ───────────────────────────────────────────────────

/// A stub `shunt` whose `check` records one `call` line and the tokens
/// variable it ran with, each run.
#[cfg(unix)]
struct Rig {
    _home: HomeSandbox,
    record: GatewayRecord,
    stub: std::path::PathBuf,
}

#[cfg(unix)]
fn rig(config: &str, env_text: &str) -> Rig {
    use std::os::unix::fs::PermissionsExt as _;
    let home = HomeSandbox::new();
    let mut record = adopted(&home, config, env_text);
    let stub = home.home().join("stub");
    fs::create_dir_all(&stub).expect("stub dir");
    let binary = stub.join("shunt");
    fs::write(
        &binary,
        format!(
            "#!/bin/sh\nd='{}'\necho call >> \"$d/calls\"\nprintf '%s\\n' \"${{SHUNT_CLIENT_TOKENS-unset}}\" >> \"$d/tokens\"\nexit 0\n",
            stub.display()
        ),
    )
    .expect("stub");
    fs::set_permissions(&binary, fs::Permissions::from_mode(0o755)).expect("chmod");
    record.binary = Some(binary);
    Rig {
        _home: home,
        record,
        stub,
    }
}

#[cfg(unix)]
impl Rig {
    fn read(&self, name: &str) -> String {
        fs::read_to_string(self.stub.join(name)).unwrap_or_default()
    }

    fn config(&self) -> String {
        fs::read_to_string(self.record.config()).expect("config")
    }
}

/// With no token in the store or the env file the op refuses before any
/// check or write. The record names a `shunt` that does not exist, so a
/// check reached would refuse as a missing binary instead, and no real
/// `shunt` can run.
#[test]
fn require_refuses_before_any_check_while_no_token_exists() {
    let home = HomeSandbox::new();
    let _env = no_shunt_env(&home);
    let original = "# shunt\n[server]\nbind = \"127.0.0.1:3001\"\n";
    let mut record = adopted(&home, original, "SHUNT_CLIENT_TOKENS=\n");
    record.binary = Some(home.home().join("no-such-shunt"));
    assert_eq!(
        require_client_tokens(&record).map_err(|e| format!("{e:#}")),
        Err("cannot require client tokens: no client token exists yet; add one first".to_string())
    );
    assert_eq!(
        fs::read_to_string(record.config()).expect("config"),
        original,
        "nothing was written"
    );
}

/// Once a token exists the op adds an empty `[server.auth]` through `shunt
/// check` run with that token in shunt's default variable.
#[cfg(unix)]
#[test]
fn require_adds_the_table_checked_with_the_token_in_shunts_variable() {
    let original = "# shunt\n[server]\nbind = \"127.0.0.1:3001\"\n";
    let rig = rig(original, "");
    let _env = no_shunt_env(&rig._home);
    let kerry = add(&rig.record, "kerry");
    assert_eq!(
        require_client_tokens(&rig.record).expect("require"),
        Applied {
            written: true,
            restart_only: false,
            cascade: None,
        }
    );
    assert_eq!(rig.config(), format!("{original}\n[server.auth]\n"));
    assert_eq!(rig.read("calls"), "call\n", "one check");
    assert_eq!(
        rig.read("tokens"),
        format!("kerry:{kerry}\n"),
        "the check ran with the token in shunt's default variable"
    );

    assert_eq!(
        require_client_tokens(&rig.record).expect("require"),
        Applied {
            written: false,
            restart_only: false,
            cascade: None,
        },
        "a config that has the table writes nothing"
    );
    assert_eq!(rig.read("calls"), "call\n", "and checks nothing");
}

/// An env-file pair alone is a token that lets the table land; a config
/// that already has the table writes nothing even with no token at all.
#[cfg(unix)]
#[test]
fn require_takes_an_env_file_pair_and_leaves_an_existing_table_alone() {
    let paired = rig("[server]\n", "SHUNT_CLIENT_TOKENS=alice:a1\n");
    let paired_env = no_shunt_env(&paired._home);
    assert!(
        require_client_tokens(&paired.record)
            .expect("require")
            .written
    );
    assert_eq!(paired.config(), "[server]\n\n[server.auth]\n");
    assert_eq!(paired.read("tokens"), "alice:a1\n");
    drop(paired_env);
    drop(paired);

    let existing = rig("[server.auth]\n", "");
    let _env = no_shunt_env(&existing._home);
    assert!(
        !require_client_tokens(&existing.record)
            .expect("require")
            .written
    );
    assert_eq!(existing.config(), "[server.auth]\n");
    assert_eq!(existing.read("calls"), "", "nothing was checked");
}

/// Under a running daemon the require op's check carries the client names
/// the daemon's spawn would: a pair only the daemon inherited is a token that
/// lets the table land, and the check sees its name with a stand-in token,
/// since the daemon's record keeps names, never a token.
#[cfg(unix)]
#[test]
fn require_under_a_daemon_checks_with_the_daemons_inherited_pairs() {
    let rig = rig("[server]\n", "");
    let _env = no_shunt_env(&rig._home);
    let home = &rig._home;
    let adopted = rig.record.clone();
    GatewayRecord::update(|slot| {
        *slot = Some(adopted);
        Ok(())
    })
    .expect("adopt the gateway");
    let held = crate::daemon::hold_daemon_lock();
    fs::write(
        home.home().join(".clauth").join("clauthd.pid"),
        format!("{}\n", std::process::id()),
    )
    .expect("pid file");
    let daemon_env = crate::testutil::EnvPin::new(
        home,
        &[(
            "SHUNT_CLIENT_TOKENS",
            Some(std::ffi::OsStr::new("alice:a1")),
        )],
    );
    crate::gateway::write_daemon_env().expect("the daemon records its env");
    drop(daemon_env);
    let _tui_env = crate::testutil::EnvPin::new(home, &[("SHUNT_CLIENT_TOKENS", None)]);
    assert!(require_client_tokens(&rig.record).expect("require").written);
    assert_eq!(rig.config(), "[server]\n\n[server.auth]\n");
    assert_eq!(rig.read("tokens"), "alice:clauth-check-stand-in\n");
    drop(held);
}

/// A `[server.auth]` built by the env layer alone, from the env the gateway
/// inherits (no daemon: this process's), keeps the last token: shunt would
/// refuse to run open on it.
#[test]
fn an_inherited_env_layer_table_keeps_the_last_token() {
    let home = HomeSandbox::new();
    let _env = no_shunt_env(&home);
    let _layer = EnvPin::new(
        &home,
        &[(
            "SHUNT_SERVER__AUTH__HEADER",
            Some(std::ffi::OsStr::new("x-token")),
        )],
    );
    let record = adopted(&home, "[server]\n", "");
    add(&record, "kerry");
    assert_eq!(
        remove_client_token(&record, "kerry").map_err(|e| format!("{e:#}")),
        Err("cannot remove the last client token while the gateway requires tokens; add another token or remove [server.auth] from the config first".to_string())
    );
    assert_eq!(stored_names(), ["kerry"]);
}

/// An inline `jwt = [{ … }]` issuer, like a `[[server.auth.jwt]]` one, lets
/// shunt run on no client token, so the last token may go.
#[test]
fn an_inline_jwt_issuer_lets_the_last_token_go() {
    let home = HomeSandbox::new();
    let _env = no_shunt_env(&home);
    let record = adopted(
        &home,
        "[server.auth]\njwt = [{ issuer = \"https://idp.example\", audience = \"a\" }]\n",
        "",
    );
    add(&record, "kerry");
    assert_eq!(
        remove_client_token(&record, "kerry").map_err(|e| format!("{e:#}")),
        Ok(true)
    );
    assert_eq!(stored_names(), Vec::<String>::new());
}

/// A delete planned while another token remained reads nothing of the
/// gateway; if that other token goes before the delete's hold, the token
/// became the last unguarded, so the delete refuses and asks for a rerun
/// rather than read the gateway under the flock.
#[test]
fn a_delete_planned_with_other_tokens_refuses_when_its_token_became_the_last() {
    let home = HomeSandbox::new();
    let _env = no_shunt_env(&home);
    let record = adopted(&home, "[server.auth]\n", "");
    add(&record, "a");
    add(&record, "b");
    let plan = plan_profile_delete("a").expect("plan");
    assert_eq!(
        remove_client_token(&record, "b").map_err(|e| format!("{e:#}")),
        Ok(true)
    );
    assert_eq!(
        crate::lock::with_state_lock(|held| refuse_profile_delete(plan.as_ref(), "a", held))
            .map_err(|e| format!("{e:#}")),
        Err("cannot delete profile \"a\": its client token became the gateway's last while the delete was being prepared; run the delete again".to_string())
    );
}

/// A removal reads the gateway only when its token is the store's last:
/// with another token left, a read that would refuse if made (the env file
/// gone) blocks nothing.
#[test]
fn a_removal_with_other_tokens_left_reads_nothing_of_the_gateway() {
    let home = HomeSandbox::new();
    let _env = no_shunt_env(&home);
    let record = adopted(&home, "[server.auth]\n", "");
    add(&record, "a");
    add(&record, "b");
    fs::remove_file(home.home().join("tokens.env")).expect("remove the env file");
    assert_eq!(
        remove_client_token(&record, "a").map_err(|e| format!("{e:#}")),
        Ok(true)
    );
    assert_eq!(stored_names(), ["b"]);
}

/// A removal planned while another token remained reads nothing of the
/// gateway; if that other token goes before the removal's hold, the token
/// became the last unguarded, so the removal refuses and asks for a rerun.
#[test]
fn a_removal_whose_token_became_the_last_refuses_and_asks_for_a_rerun() {
    let home = HomeSandbox::new();
    let _env = no_shunt_env(&home);
    let record = adopted(&home, "[server.auth]\n", "");
    add(&record, "a");
    add(&record, "b");
    let other = record.clone();
    set_pre_removal_hold_hook(
        "a",
        std::sync::Arc::new(move || {
            remove_client_token(&other, "b").expect("remove the other token");
        }),
    );
    assert_eq!(
        remove_client_token(&record, "a").map_err(|e| format!("{e:#}")),
        Err("cannot remove the client token of profile \"a\": its client token became the gateway's last while the removal was being prepared; run the removal again".to_string())
    );
    assert_eq!(stored_names(), ["a"]);
}

/// A store that cannot be read refuses an add opening on the op, typed,
/// naming the file and the fix.
#[cfg(unix)]
#[test]
fn an_unreadable_store_refuses_the_add_naming_the_file() {
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
    let home = HomeSandbox::new();
    let _env = no_shunt_env(&home);
    let record = adopted(&home, "[server]\n", "");
    if fs::metadata(record.config()).expect("stat").uid() == 0 {
        eprintln!("SKIPPING: running as root, which can read any file");
        return;
    }
    add(&record, "a");
    let path = store_path().expect("path");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).expect("chmod");
    assert_eq!(
        add_client_token(&record, "kerry")
            .map(|_| ())
            .map_err(|e| format!("{e:#}")),
        Err(format!(
            "cannot add a client token for profile \"kerry\": clauth's client-token store {} cannot be read (Permission denied (os error 13)); repair its permissions or remove it",
            path.display()
        ))
    );
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).expect("chmod back");
}

/// The require op's own reads open on its op like every other engine op's:
/// an unreadable store refuses `cannot require client tokens: …`, typed.
#[cfg(unix)]
#[test]
fn an_unreadable_store_refuses_require_on_its_op() {
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
    let home = HomeSandbox::new();
    let _env = no_shunt_env(&home);
    let record = adopted(&home, "[server]\n", "");
    if fs::metadata(record.config()).expect("stat").uid() == 0 {
        eprintln!("SKIPPING: running as root, which can read any file");
        return;
    }
    add(&record, "a");
    let path = store_path().expect("path");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).expect("chmod");
    let outcome = require_client_tokens(&record)
        .map(|_| ())
        .map_err(|e| format!("{e:#}"));
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).expect("chmod back");
    assert_eq!(
        outcome,
        Err(format!(
            "cannot require client tokens: clauth's client-token store {} cannot be read (Permission denied (os error 13)); repair its permissions or remove it",
            path.display()
        ))
    );
}

/// A rename planned while the profile held no token read nothing of the
/// gateway; a token gained before the rename's hold was never checked
/// against the clients the gateway already names, so the move refuses and
/// asks for a rerun.
#[test]
fn a_rename_whose_profile_gained_a_token_after_its_plan_refuses() {
    let home = HomeSandbox::new();
    let _env = no_shunt_env(&home);
    let record = adopted(&home, "[server]\n", "");
    let planned = refuse_profile_rename("old", "new").expect("plan");
    assert!(!planned, "no token at the plan");
    add(&record, "old");
    assert_eq!(
        crate::lock::with_state_lock(|held| rename_profile_token("old", "new", planned, held))
            .map_err(|e| format!("{e:#}")),
        Err("cannot rename profile \"old\" to \"new\": the profile gained a client token while the rename was being prepared; run the rename again".to_string())
    );
    assert_eq!(stored_names(), ["old"]);
}
