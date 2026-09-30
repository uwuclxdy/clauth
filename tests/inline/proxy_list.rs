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
) -> Row {
    Row {
        service: service.to_string(),
        version: version.map(str::to_string),
        contract: contract.map(str::to_string),
        enabled,
        port,
        state: state.to_string(),
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
        ),
        row(
            "qwen",
            Some("0.9.1"),
            Some("1.0"),
            false,
            Some(9102),
            "disabled",
        ),
        row("codex", None, None, false, None, "not_registered"),
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

/// `--json` emits the typed shape, the `jobs --json` precedent: a figure the
/// record has not renders `null`, and `port` is an integer, never the table's
/// `-` sentinel or a string.
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
        ),
        row("codex", None, None, false, None, "not_registered"),
    ];
    let got: serde_json::Value = serde_json::from_str(&rows_json(&rows)).unwrap();
    assert_eq!(
        got,
        serde_json::json!([
            {"service": "zcode", "version": "1.2.0", "contract": "1.0", "enabled": true, "port": 9101, "state": "healthy"},
            {"service": "codex", "version": null, "contract": null, "enabled": false, "port": null, "state": "not_registered"},
        ])
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
