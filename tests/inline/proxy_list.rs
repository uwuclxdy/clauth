#![allow(clippy::unwrap_used, clippy::expect_used)]
//! `clauth proxy list`: the table and the `--json` array render every row's
//! fields (a figure the record has not rendering `-` in the table, `null` in
//! JSON), the empty table says how to get one, and the union + manifest +
//! live-state join is driven end to end over stub proxies.

use super::*;

fn row(
    service: &str,
    version: Option<&str>,
    contract: Option<&str>,
    enabled: bool,
    port: Option<u16>,
    state: &str,
    reason: Option<&str>,
) -> Row {
    Row {
        service: service.to_string(),
        version: version.map(str::to_string),
        contract: contract.map(str::to_string),
        enabled,
        port,
        state: state.to_string(),
        reason: reason.map(str::to_string),
    }
}

#[test]
fn the_empty_table_names_the_enable_path() {
    assert_eq!(
        render_table(&[]),
        "no clauth proxies. install a clauth-<service>-proxy on PATH, then `clauth proxy enable <service>`.\n"
    );
}

#[test]
fn the_table_renders_every_row_field() {
    let rows = vec![
        row(
            "zcode",
            Some("1.2.0"),
            Some("1.0"),
            true,
            Some(9101),
            "healthy",
            None,
        ),
        row(
            "qwen",
            Some("0.9.1"),
            Some("1.0"),
            false,
            Some(9102),
            "disabled",
            None,
        ),
        row("codex", None, None, false, None, "not_registered", None),
    ];
    let table = render_table(&rows);
    assert_eq!(
        table,
        "SERVICE  VERSION  CONTRACT  ENABLED  PORT  STATE         \n\
         zcode    1.2.0    1.0       true     9101  healthy       \n\
         qwen     0.9.1    1.0       false    9102  disabled      \n\
         codex    -        -         false       -  not_registered\n"
    );
}

/// The version comes from the proxy binary's own manifest, so it can hold any
/// script: a column sizes and pads by drawn cells, so a wide-script version
/// widens its own column and every later column keeps its start.
#[test]
fn a_wide_script_version_keeps_the_later_columns_aligned() {
    let rows = vec![
        row(
            "zcode",
            Some("版本漢字"),
            Some("1.0"),
            true,
            Some(9101),
            "healthy",
            None,
        ),
        row(
            "qwen",
            Some("0.9.1"),
            Some("1.0"),
            false,
            Some(9102),
            "disabled",
            None,
        ),
    ];
    assert_eq!(
        render_table(&rows),
        "SERVICE  VERSION   CONTRACT  ENABLED  PORT  STATE   \n\
         zcode    版本漢字  1.0       true     9101  healthy \n\
         qwen     0.9.1     1.0       false    9102  disabled\n"
    );
}

/// A row with no reason renders no trailing lines and no blank line: the
/// reason lines appear only where a row has one.
#[test]
fn the_table_appends_each_rows_reason_after_a_blank_line() {
    let rows = vec![
        row(
            "qwen",
            None,
            None,
            false,
            Some(65000),
            "misconfigured",
            Some("row \"qwen\": unknown key \"enabeld\""),
        ),
        row(
            "zcode",
            Some("1.2.0"),
            Some("1.0"),
            true,
            Some(9101),
            "unobserved",
            None,
        ),
    ];
    let table = render_table(&rows);
    assert_eq!(
        table,
        concat!(
            "SERVICE  VERSION  CONTRACT  ENABLED   PORT  STATE        \n",
            "qwen     -        -         false    65000  misconfigured\n",
            "zcode    1.2.0    1.0       true      9101  unobserved   \n",
            "\n",
            "qwen: row \"qwen\": unknown key \"enabeld\"\n",
        )
    );
}

/// `--json` emits the typed shape, the `jobs --json` precedent: a figure the
/// record has not renders `null`, and `port` is an integer, never the table's
/// `-` sentinel or a string. `reason` is additive after `state`: `null` for a
/// row with none, the string for one with a reason.
#[test]
fn the_json_array_names_every_field_fixed() {
    let rows = vec![
        row(
            "zcode",
            Some("1.2.0"),
            Some("1.0"),
            true,
            Some(9101),
            "healthy",
            None,
        ),
        row(
            "codex",
            None,
            None,
            false,
            None,
            "not_registered",
            Some("row \"codex\": unknown key \"enabeld\""),
        ),
    ];
    assert_eq!(
        rows_json(&rows),
        r#"[
  {
    "service": "zcode",
    "version": "1.2.0",
    "contract": "1.0",
    "enabled": true,
    "port": 9101,
    "state": "healthy",
    "reason": null
  },
  {
    "service": "codex",
    "version": null,
    "contract": null,
    "enabled": false,
    "port": null,
    "state": "not_registered",
    "reason": "row \"codex\": unknown key \"enabeld\""
  }
]"#,
        "key order is part of the ruling: state then reason, never the reverse"
    );
}

/// `rows` joins the registry, PATH discovery and the record-only entries end
/// to end: a registered enabled row reads `unobserved` with its manifest's
/// version/contract, and a discovered-but-unregistered binary reads
/// `not_registered` with its manifest's version/contract.
#[cfg(unix)]
#[test]
fn list_rows_join_the_registry_discovery_and_record_only_entries() {
    use crate::testutil::HomeSandbox;

    let home = HomeSandbox::new();
    let dir_a = home.home().join("stub-a");
    let dir_b = home.home().join("stub-b");
    stub::write_proxy_stub(&dir_a, "zcode");
    stub::write_proxy_stub(&dir_b, "qwen");
    let port = stub::free_port();
    crate::proxy::enable("zcode", Some(port), Some(dir_a.as_os_str())).expect("enable zcode");
    let path = std::ffi::OsString::from(format!("{}:{}", dir_a.display(), dir_b.display()));

    let rows = rows(path.as_os_str()).expect("rows");

    assert_eq!(rows.len(), 2, "one row per service, in service order");
    assert_eq!(rows[0].service, "qwen");
    assert_eq!(rows[0].state, "not_registered");
    assert!(!rows[0].enabled);
    assert_eq!(rows[0].port, None);
    assert_eq!(rows[0].version.as_deref(), Some("1.2.0"));
    assert_eq!(rows[0].contract.as_deref(), Some("1.0"));
    assert_eq!(rows[1].service, "zcode");
    assert_eq!(rows[1].state, "unobserved");
    assert!(rows[1].enabled);
    assert_eq!(rows[1].port, Some(port));
    assert_eq!(rows[1].version.as_deref(), Some("1.2.0"));
    assert_eq!(rows[1].contract.as_deref(), Some("1.0"));
}

#[cfg(unix)]
use crate::daemon::gateway::tests::stub;

/// `list` reads N manifests concurrently under one aggregate bound: three
/// stubs whose `manifest` sleeps 3 s finish well under N × 3 s serial, with a
/// margin that survives a loaded box.
#[cfg(unix)]
#[test]
fn list_reads_manifests_concurrently_under_one_bound() {
    use crate::testutil::HomeSandbox;

    let home = HomeSandbox::new();
    let mut path_parts = Vec::new();
    for (i, service) in ["alpha", "beta", "gamma"].iter().enumerate() {
        let dir = home.home().join(format!("stub-{i}"));
        stub::write_proxy_stub(&dir, service);
        std::fs::write(dir.join("slow-manifest"), "").expect("slow-manifest");
        path_parts.push(dir.display().to_string());
    }
    let path = std::ffi::OsString::from(path_parts.join(":"));

    let started = std::time::Instant::now();
    let rows = rows(path.as_os_str()).expect("rows");
    let took = started.elapsed();
    assert_eq!(rows.len(), 3);
    assert!(
        took < std::time::Duration::from_secs(6),
        "N manifests finish about one bound, never N x 3 s serial: {took:?}"
    );
}

/// `list` reads a daemon's live `proxies` only when a fresh daemon owns the
/// feed: a stale feed with no daemon reads the record-only entries (the
/// `status --json` producer), so a binary-less row reads `binary_missing` and
/// a registered row reads `unobserved`, never the stale live slot.
#[cfg(unix)]
#[test]
fn a_daemonless_feed_reads_record_only_not_live() {
    use crate::testutil::HomeSandbox;

    let home = HomeSandbox::new();
    let dir = home.home().join("stub");
    stub::write_proxy_stub(&dir, "zcode");
    let port = stub::free_port();
    crate::proxy::enable("zcode", Some(port), Some(dir.as_os_str())).expect("enable zcode");
    let clauth = crate::profile::clauth_dir().expect("dir");
    let registry = std::fs::read_to_string(clauth.join("proxies.toml")).expect("registry");
    std::fs::write(
        clauth.join("proxies.toml"),
        format!(
            "{registry}\n[qwen]\nport = 9102\nenabled = true\nbinary = \"/gone/clauth-qwen-proxy\"\n"
        ),
    )
    .expect("add qwen");
    std::fs::write(
        clauth.join("status.json"),
        r#"{"schema":1,"generated_at":"2020-01-01T00:00:00Z","proxies":[{"service":"zcode","state":"healthy","binary":"/x","port":9101,"pid":4242,"version":"9.9.9","contract":"9.9","answerer":null,"restarts":0,"last_exit":null,"reason":null,"since":"2020-01-01T00:00:00Z"}]}"#,
    )
    .expect("a stale feed");

    let rows = rows(dir.as_os_str()).expect("rows");
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].service, "qwen");
    assert_eq!(
        rows[0].state, "binary_missing",
        "the record-only entry, not a live guess"
    );
    assert_eq!(rows[1].service, "zcode");
    assert_eq!(
        rows[1].state, "unobserved",
        "a daemonless stale feed reads record-only, never `healthy`"
    );
    assert_eq!(
        rows[1].version.as_deref(),
        Some("1.2.0"),
        "the manifest, not the stale feed's 9.9.9"
    );
}

/// A registry that does not read fails `list` by name (the way `disable` does
/// on the same file), and `entries` never drops the live slot it holds.
#[cfg(unix)]
#[test]
fn an_unreadable_registry_errs_in_list_and_keeps_the_live_slot_in_entries() {
    use crate::testutil::HomeSandbox;

    let _home = HomeSandbox::new();
    let clauth = crate::profile::clauth_dir().expect("dir");
    std::fs::create_dir_all(&clauth).expect("mkdir");
    std::fs::write(
        clauth.join("proxies.toml"),
        "[qwen]\nport = 9101\n[zcode]\nport = 9101\n",
    )
    .expect("registry");

    let path = std::ffi::OsString::new();
    let err = match rows(path.as_os_str()) {
        Ok(_) => panic!("the list must refuse an unreadable registry"),
        Err(err) => err,
    };
    assert_eq!(
        format!("{err:#}"),
        format!(
            "invalid proxy registry {}: rows \"qwen\" and \"zcode\" both hold port 9101",
            clauth.join("proxies.toml").display()
        ),
        "the list refuses naming the registry error"
    );

    let slot = ProxySlot {
        service: "zcode".to_string(),
        state: ProxyState::Healthy,
        binary: Some("x".to_string()),
        port: Some(9101),
        pid: Some(4242),
        version: Some("1.2.0".to_string()),
        contract: Some("1.0".to_string()),
        answerer: None,
        restarts: 0,
        last_exit: None,
        reason: None,
        since: None,
    };
    let slots = [slot.clone()];
    assert_eq!(
        crate::daemon::proxies::entries(Some(&slots)),
        vec![slot],
        "entries keeps the live slot it holds over the unreadable registry"
    );
}

/// `list` reads a fresh daemon's live `proxies` through the typed feed read: a
/// published slot's state, version and contract win, and a registered row the
/// feed does not carry yet reads its record-only verdict, never
/// `not_registered`.
#[cfg(unix)]
#[test]
fn list_reads_a_fresh_daemons_live_slots_and_falls_back_per_row() {
    use crate::testutil::HomeSandbox;

    let home = HomeSandbox::new();
    let dir_z = home.home().join("stub-z");
    let dir_q = home.home().join("stub-q");
    stub::write_proxy_stub(&dir_z, "zcode");
    stub::write_proxy_stub(&dir_q, "qwen");
    let port = stub::free_port();
    crate::proxy::enable("zcode", Some(port), Some(dir_z.as_os_str())).expect("enable zcode");
    crate::proxy::enable("qwen", Some(stub::free_port()), Some(dir_q.as_os_str()))
        .expect("enable qwen");
    let clauth = crate::profile::clauth_dir().expect("dir");
    let now = crate::usage::epoch_secs_to_iso(crate::usage::now_epoch_secs());
    // A fresh feed carrying zcode's healthy slot alone: qwen's row is not yet
    // published (the daemon trails the registry by one status write).
    std::fs::write(
        clauth.join("status.json"),
        format!(
            r#"{{"schema":1,"generated_at":"{now}","proxies":[{{"service":"zcode","state":"healthy","binary":"/x","port":{port},"pid":4242,"version":"9.9.9","contract":"9.9","answerer":null,"restarts":0,"last_exit":null,"reason":null,"since":"{now}"}}]}}"#
        ),
    )
    .expect("a fresh feed");
    let _held = crate::daemon::hold_daemon_lock();

    let path = std::ffi::OsString::from(format!("{}:{}", dir_z.display(), dir_q.display()));
    let rows = rows(path.as_os_str()).expect("rows");
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].service, "qwen");
    assert_eq!(
        rows[0].state, "unobserved",
        "a registered row absent from the feed reads its record-only verdict, never not_registered"
    );
    assert_eq!(rows[1].service, "zcode");
    assert_eq!(rows[1].state, "healthy", "the live slot wins");
    assert_eq!(
        rows[1].version.as_deref(),
        Some("9.9.9"),
        "the feed's version, not the manifest"
    );
    assert_eq!(rows[1].contract.as_deref(), Some("9.9"));
}

// ── P5 residue ──────────────────────────────────────────────────────────────

/// A row carrying a key clauth does not know reads `misconfigured`, its
/// reason naming the key and the table, and the other rows run.
#[cfg(unix)]
#[test]
fn a_row_with_an_unknown_key_reads_misconfigured_naming_it_and_the_others_run() {
    use crate::testutil::HomeSandbox;

    let home = HomeSandbox::new();
    let dir = home.home().join("stub");
    stub::write_proxy_stub(&dir, "zcode");
    crate::proxy::enable("zcode", Some(stub::free_port()), Some(dir.as_os_str()))
        .expect("enable zcode");
    let path = crate::proxy::registry_path().expect("path");
    let registry = std::fs::read_to_string(&path).expect("registry");
    std::fs::write(
        &path,
        format!("{registry}\n[qwen]\nport = 65000\nenabeld = true\n"),
    )
    .expect("hand edit");

    let rows = rows(dir.as_os_str()).expect("one refused row leaves the list readable");

    assert_eq!(rows.len(), 2, "one row per service");
    assert_eq!(
        (rows[0].service.as_str(), rows[0].state.as_str()),
        ("qwen", "misconfigured")
    );
    assert_eq!(
        rows[0].reason.as_deref(),
        Some("row \"qwen\": unknown key \"enabeld\""),
        "the refused row's reason rides the list row"
    );
    assert_eq!(
        (rows[1].service.as_str(), rows[1].state.as_str()),
        ("zcode", "unobserved")
    );
    assert_eq!(
        rows[1].reason.as_deref(),
        None,
        "a row with nothing to explain has no reason"
    );
    let entries = crate::daemon::proxies::entries(None);
    let qwen = entries
        .iter()
        .find(|slot| slot.service == "qwen")
        .expect("the refused row is in the feed");
    assert_eq!(qwen.state, ProxyState::Misconfigured);
    assert_eq!(
        qwen.reason.as_deref(),
        Some("row \"qwen\": unknown key \"enabeld\"")
    );
}

/// The registry wins in both directions: a fresh feed's slot for a service
/// the registry no longer holds reads `not_registered`, never the feed's word.
#[cfg(unix)]
#[test]
fn a_feed_slot_for_a_row_gone_from_the_registry_reads_not_registered() {
    use crate::testutil::HomeSandbox;

    let home = HomeSandbox::new();
    let dir_z = home.home().join("stub-z");
    let dir_q = home.home().join("stub-q");
    stub::write_proxy_stub(&dir_z, "zcode");
    stub::write_proxy_stub(&dir_q, "qwen");
    let port = stub::free_port();
    crate::proxy::enable("zcode", Some(port), Some(dir_z.as_os_str())).expect("enable zcode");
    let clauth = crate::profile::clauth_dir().expect("dir");
    let now = crate::usage::epoch_secs_to_iso(crate::usage::now_epoch_secs());
    // qwen's row is gone from the registry; the daemon has not reaped it yet.
    std::fs::write(
        clauth.join("status.json"),
        format!(
            r#"{{"schema":1,"generated_at":"{now}","proxies":[{{"service":"qwen","state":"healthy","binary":"/q","port":65000,"pid":4243,"version":"1.2.0","contract":"1.0","answerer":null,"restarts":0,"last_exit":null,"reason":"STALE qwen reason","since":"{now}"}},{{"service":"zcode","state":"healthy","binary":"/x","port":{port},"pid":4242,"version":"1.2.0","contract":"1.0","answerer":null,"restarts":0,"last_exit":null,"reason":null,"since":"{now}"}}]}}"#
        ),
    )
    .expect("a fresh feed");
    let _held = crate::daemon::hold_daemon_lock();

    let path = std::ffi::OsString::from(format!("{}:{}", dir_z.display(), dir_q.display()));
    let rows = rows(path.as_os_str()).expect("rows");

    assert_eq!(rows.len(), 2, "one row per service");
    assert_eq!(rows[0].service, "qwen");
    assert_eq!(rows[0].state, "not_registered", "the registry wins");
    assert!(!rows[0].enabled);
    assert_eq!(rows[0].port, None);
    assert_eq!(rows[1].service, "zcode");
    assert_eq!(rows[1].state, "healthy");
    assert_eq!(
        rows[0].reason.as_deref(),
        None,
        "a row gone from the registry carries no reason: the registry wins both ways"
    );
    let table = render_table(&rows);
    assert!(
        !table.contains("STALE qwen reason"),
        "a not_registered row prints no reason line: {table}"
    );
    let json: serde_json::Value = serde_json::from_str(&rows_json(&rows)).unwrap();
    assert_eq!(
        json[0].get("reason"),
        Some(&serde_json::Value::Null),
        "a not_registered row emits reason null"
    );
}

/// A `not_registered` row takes no version/contract from a stale feed slot
/// either (the same `row.is_some()` guard as the reason): the values come from
/// the discovered binary's manifest, never the feed's stale word.
#[cfg(unix)]
#[test]
fn a_not_registered_row_ignores_a_stale_slots_version_and_contract() {
    use crate::testutil::HomeSandbox;

    let home = HomeSandbox::new();
    let dir_q = home.home().join("stub-q");
    stub::write_proxy_stub(&dir_q, "qwen");
    let clauth = crate::profile::clauth_dir().expect("dir");
    std::fs::create_dir_all(&clauth).expect("clauth dir");
    let now = crate::usage::epoch_secs_to_iso(crate::usage::now_epoch_secs());
    // qwen has no registry row; a stale feed slot still carries it with a
    // version/contract the discovered binary's manifest does not print.
    std::fs::write(
        clauth.join("status.json"),
        format!(
            r#"{{"schema":1,"generated_at":"{now}","proxies":[{{"service":"qwen","state":"healthy","binary":"/q","port":65000,"pid":4243,"version":"9.9.9","contract":"9.9","answerer":null,"restarts":0,"last_exit":null,"reason":"STALE qwen reason","since":"{now}"}}]}}"#
        ),
    )
    .expect("a stale feed");
    let _held = crate::daemon::hold_daemon_lock();

    let rows = rows(dir_q.as_os_str()).expect("rows");

    assert_eq!(rows.len(), 1, "one row for the one discovered service");
    assert_eq!(rows[0].service, "qwen");
    assert_eq!(rows[0].state, "not_registered");
    assert_eq!(
        rows[0].version.as_deref(),
        Some("1.2.0"),
        "the discovered binary's manifest version, never the stale feed's 9.9.9"
    );
    assert_eq!(
        rows[0].contract.as_deref(),
        Some("1.0"),
        "the discovered binary's manifest contract, never the stale feed's 9.9"
    );
}

/// A fresh daemon's slot whose state carries a reason hands it to the row:
/// the `manifest_refused` slot's reason rides `rows[i].reason` and renders as
/// the `<service>: <reason>` line after the table.
#[cfg(unix)]
#[test]
fn a_fresh_daemons_slot_reason_rides_the_row_and_renders() {
    use crate::testutil::HomeSandbox;

    let home = HomeSandbox::new();
    let dir = home.home().join("stub");
    stub::write_proxy_stub(&dir, "zcode");
    let port = stub::free_port();
    crate::proxy::enable("zcode", Some(port), Some(dir.as_os_str())).expect("enable zcode");
    let clauth = crate::profile::clauth_dir().expect("dir");
    let now = crate::usage::epoch_secs_to_iso(crate::usage::now_epoch_secs());
    std::fs::write(
        clauth.join("status.json"),
        format!(
            r#"{{"schema":1,"generated_at":"{now}","proxies":[{{"service":"zcode","state":"manifest_refused","binary":"/x","port":{port},"pid":4242,"version":"1.2.0","contract":"1.0","answerer":null,"restarts":0,"last_exit":null,"reason":"manifest refused: boom","since":"{now}"}}]}}"#
        ),
    )
    .expect("a fresh feed");
    let _held = crate::daemon::hold_daemon_lock();

    let rows = rows(dir.as_os_str()).expect("rows");
    assert_eq!(rows.len(), 1, "one row for the one registered service");
    assert_eq!(
        rows[0].reason.as_deref(),
        Some("manifest refused: boom"),
        "the live slot's reason rides the row"
    );
    let table = render_table(&rows);
    assert!(
        table.ends_with("\nzcode: manifest refused: boom\n"),
        "the reason renders as the <service>: <reason> line: {table}"
    );
}

/// A registered row the fresh feed does not carry yet reads its record-only
/// verdict, and that verdict's reason (a missing token file) rides the row.
#[cfg(unix)]
#[test]
fn a_record_only_verdicts_reason_rides_the_fallback_row() {
    use crate::testutil::HomeSandbox;

    let home = HomeSandbox::new();
    let dir = home.home().join("stub");
    stub::write_proxy_stub(&dir, "zcode");
    let port = stub::free_port();
    crate::proxy::enable("zcode", Some(port), Some(dir.as_os_str())).expect("enable zcode");
    let zcode = crate::proxy::Service::parse("zcode").expect("service");
    let token_path = crate::proxy::admin_token_path(&zcode).expect("token path");
    std::fs::remove_file(&token_path).expect("remove the token");
    let clauth = crate::profile::clauth_dir().expect("dir");
    let now = crate::usage::epoch_secs_to_iso(crate::usage::now_epoch_secs());
    // A fresh feed carrying no proxies: zcode's row is not yet published.
    std::fs::write(
        clauth.join("status.json"),
        format!(r#"{{"schema":1,"generated_at":"{now}","proxies":[]}}"#),
    )
    .expect("a fresh empty feed");
    let _held = crate::daemon::hold_daemon_lock();

    let rows = rows(dir.as_os_str()).expect("rows");
    assert_eq!(rows.len(), 1, "one row for the one registered service");
    assert_eq!(rows[0].service, "zcode");
    assert_eq!(rows[0].state, "no_token");
    let want = format!(
        "the admin token of proxy \"zcode\" in {} is missing; run `clauth proxy enable zcode`",
        token_path.display()
    );
    assert_eq!(
        rows[0].reason.as_deref(),
        Some(want.as_str()),
        "the record-only verdict's reason rides the fallback row"
    );
}

/// The unreadable-registry line is deduplicated per error, and a good read
/// in between resets the memo, so the same error coming back is logged again.
/// The line carries the bare `clauth:` prefix of this reader's process role
/// (`status --json`, stamping off).
#[test]
fn the_registry_error_logs_again_after_a_good_read() {
    use crate::testutil::HomeSandbox;

    let _home = HomeSandbox::new();
    // The `clauth:` prefix comes from `role(stamped())`: pin it, since a prior
    // sibling test that ran `serve()` flips the process-global stamp on.
    let _cli = crate::logline::StampOverride::new(false);
    let path = crate::proxy::registry_path().expect("path");
    std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    let bad = "[qwen]\nport = 9101\n[zcode]\nport = 9101\n";
    let lines = crate::logline::LogLines::new();
    let _capture = lines.capture_here();
    let registry_lines = || {
        lines
            .snapshot()
            .into_iter()
            .filter(|line| line.contains("cannot read the proxy registry"))
            .collect::<Vec<_>>()
    };

    std::fs::write(&path, bad).expect("bad registry");
    crate::daemon::proxies::entries(None);
    crate::daemon::proxies::entries(None);
    assert_eq!(
        registry_lines(),
        vec![format!(
            "clauth: cannot read the proxy registry: invalid proxy registry {}: rows \"qwen\" and \"zcode\" both hold port 9101",
            path.display()
        )],
        "one line per distinct error, the process role's prefix"
    );

    std::fs::write(&path, "[qwen]\nport = 9101\n").expect("good registry");
    crate::daemon::proxies::entries(None);
    std::fs::write(&path, bad).expect("bad again");
    crate::daemon::proxies::entries(None);

    assert_eq!(registry_lines().len(), 2, "a good read resets the memo");
}

/// The registry-error line's prefix derives from the process role: inside the
/// daemon (stamping on) every line reads `clauth daemon:`.
#[test]
fn the_registry_error_prefix_follows_the_process_role() {
    assert_eq!(crate::logline::role(false), "clauth");
    assert_eq!(crate::logline::role(true), "clauth daemon");
}

/// `entries(None)` publishes the refused rows beside the valid ones in
/// service order: the misconfigured `qwen` row sorts before `zcode`, never
/// trailing it.
#[cfg(unix)]
#[test]
fn entries_names_a_refused_row_in_service_order() {
    use crate::testutil::HomeSandbox;

    let home = HomeSandbox::new();
    let dir = home.home().join("stub");
    stub::write_proxy_stub(&dir, "zcode");
    crate::proxy::enable("zcode", Some(stub::free_port()), Some(dir.as_os_str()))
        .expect("enable zcode");
    let path = crate::proxy::registry_path().expect("path");
    let registry = std::fs::read_to_string(&path).expect("registry");
    std::fs::write(
        &path,
        format!("{registry}\n[qwen]\nport = 65000\nenabeld = true\n"),
    )
    .expect("hand edit");

    let services: Vec<String> = crate::daemon::proxies::entries(None)
        .iter()
        .map(|slot| slot.service.clone())
        .collect();

    assert_eq!(services, vec!["qwen".to_string(), "zcode".to_string()]);
}
