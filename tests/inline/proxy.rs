#![allow(clippy::unwrap_used, clippy::expect_used)]
//! The proxy registry and its files, PATH discovery, the `manifest` read and
//! `clauth proxy enable|disable`. Every test holds a `HomeSandbox`; a test
//! driving a stub `clauth-<service>-proxy` (a `/bin/sh` script) is
//! `#[cfg(unix)]`, and hands discovery its `PATH` as an input.

use std::fs;
use std::time::SystemTime;

use super::*;
use crate::testutil::HomeSandbox;

fn write(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    fs::write(path, text).expect("write");
}

fn service(name: &str) -> Service {
    Service::parse(name).expect("a valid service")
}

#[cfg(unix)]
fn mode(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt as _;
    fs::metadata(path).expect("metadata").permissions().mode() & 0o777
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt as _;
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).expect("chmod");
}

/// A stub `clauth-<service>-proxy` in `dir` whose `manifest` prints `json`;
/// any other subcommand exits 2, as the core's CLI does.
#[cfg(unix)]
fn stub_proxy(dir: &Path, name: &str, json: &str) -> PathBuf {
    fs::create_dir_all(dir).expect("stub dir");
    crate::testutil::write_shim(
        dir,
        name,
        &format!("if [ \"$1\" = manifest ]; then printf '%s\\n' '{json}'; exit 0; fi\nexit 2"),
    )
}

#[cfg(unix)]
const ZCODE_MANIFEST: &str = r#"{"service":"zcode","display_name":"ZCode","description":"z.ai as the ZCode client","version":"1.2.0","contract":"1.0","capabilities":["events","count_tokens"],"drain_secs":30}"#;

fn registry_with(rows: &[(&str, u16, bool)]) -> Registry {
    Registry {
        rows: rows
            .iter()
            .map(|(name, port, enabled)| {
                (
                    service(name),
                    ProxyRow {
                        port: *port,
                        enabled: *enabled,
                        binary: None,
                    },
                )
            })
            .collect(),
    }
}

/// A registry holding `zcode` alone, on `port`, run from `binary`.
#[cfg(unix)]
fn zcode_row(port: u16, enabled: bool, binary: &Path) -> Registry {
    let mut registry = registry_with(&[("zcode", port, enabled)]);
    if let Some(row) = registry.rows.get_mut(&service("zcode")) {
        row.binary = Some(binary.to_path_buf());
    }
    registry
}

// ── the service name ────────────────────────────────────────────────────────

#[test]
fn a_service_follows_the_cores_charset() {
    for name in ["zcode", "a", "0", "q-1", &"a".repeat(32)] {
        assert_eq!(
            Service::parse(name).map(|s| s.as_str().to_string()),
            Ok(name.to_string()),
            "{name}"
        );
    }
    let rule = "a service is 1 to 32 characters of a-z, 0-9 and -, the first a letter or digit";
    for name in [
        "..",
        "a/b",
        "Zc",
        &"a".repeat(33),
        "",
        "-a",
        "a_b",
        "a\\b",
        "zcode ",
    ] {
        assert_eq!(
            Service::parse(name).map_err(|e| e.to_string()),
            Err(format!("invalid proxy service {name:?}: {rule}")),
            "{name}"
        );
    }
}

// ── files ───────────────────────────────────────────────────────────────────

#[test]
fn every_file_clauth_owns_for_a_proxy_sits_in_its_state_dir() {
    let home = HomeSandbox::new();
    let clauth = home.home().join(".clauth");
    let zcode = service("zcode");
    let dir = clauth.join("proxies").join("zcode");
    assert_eq!(registry_path().unwrap(), clauth.join("proxies.toml"));
    assert_eq!(state_dir(&zcode).unwrap(), dir);
    assert_eq!(
        admin_token_path(&zcode).unwrap(),
        dir.join("clauth-admin-token")
    );
    assert_eq!(
        child_marker_path(&zcode).unwrap(),
        dir.join("clauth-child.json")
    );
    assert_eq!(log_path(&zcode).unwrap(), dir.join("clauth.log"));
    assert_eq!(zcode.binary_name(), "clauth-zcode-proxy");
    assert_eq!(bind(9101).to_string(), "127.0.0.1:9101");
}

// ── the registry ────────────────────────────────────────────────────────────

#[test]
fn a_saved_registry_reads_back_and_is_owner_only() {
    let home = HomeSandbox::new();
    let binary = home.home().join("opt").join("clauth-qwen-proxy");
    let mut expected = registry_with(&[("zcode", 9101, true), ("qwen", 9102, false)]);
    if let Some(row) = expected.rows.get_mut(&service("qwen")) {
        row.binary = Some(binary.clone());
    }

    Registry::update(|registry| {
        *registry = expected.clone();
        Ok(())
    })
    .expect("save");

    assert_eq!(Registry::load().expect("load"), expected);
    // TOML writes a path holding `\` (every Windows path) as a literal
    // string, so the bytes are pinned where the path is a basic one.
    #[cfg(unix)]
    {
        let path = registry_path().unwrap();
        assert_eq!(
            fs::read_to_string(&path).expect("read"),
            format!(
                "[qwen]\nport = 9102\nenabled = false\nbinary = \"{}\"\n\n[zcode]\nport = 9101\nenabled = true\n",
                binary.display()
            )
        );
        assert_eq!(mode(&path), 0o600, "the registry is owner-only");
    }
}

/// An update that changes nothing writes nothing: a hand-edited registry
/// keeps its bytes, its comment included, and its mtime.
#[test]
fn an_update_that_changes_nothing_leaves_the_file_alone() {
    let _home = HomeSandbox::new();
    let path = registry_path().unwrap();
    let text = "# by hand\n[zcode]\nport = 9101\nenabled = true\n";
    write(&path, text);
    let earlier = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    crate::testutil::set_mtime(&path, earlier);

    let seen = Registry::update(|registry| Ok(registry.clone())).expect("a no-op update");

    assert_eq!(seen, registry_with(&[("zcode", 9101, true)]));
    assert_eq!(fs::read_to_string(&path).expect("read"), text);
    assert_eq!(
        fs::metadata(&path)
            .expect("metadata")
            .modified()
            .expect("mtime"),
        earlier
    );
}

#[test]
fn a_missing_registry_reads_empty() {
    let _home = HomeSandbox::new();
    assert_eq!(Registry::load().expect("load"), Registry::default());
}

/// Every invalid row fails the load naming the row and the field; none is
/// dropped while the rest load.
#[test]
fn every_invalid_row_is_refused_naming_the_row_and_the_field() {
    let _home = HomeSandbox::new();
    let path = registry_path().unwrap();
    let rule = "a service is 1 to 32 characters of a-z, 0-9 and -, the first a letter or digit";
    for (text, message) in [
        (
            "[zcode]\nport = 9101\n[Zc]\nport = 9102\n".to_string(),
            format!("row \"Zc\": invalid proxy service \"Zc\": {rule}"),
        ),
        (
            "[\"..\"]\nport = 9102\n".to_string(),
            format!("row \"..\": invalid proxy service \"..\": {rule}"),
        ),
        (
            "[zcode]\nport = 0\n".to_string(),
            "row \"zcode\": port 0 is outside 1..=65535".to_string(),
        ),
        (
            "[zcode]\nport = 65536\n".to_string(),
            "row \"zcode\": port 65536 is outside 1..=65535".to_string(),
        ),
        (
            "[zcode]\nenabled = true\n".to_string(),
            "row \"zcode\": port is missing".to_string(),
        ),
        (
            "[zcode]\nport = 9101\nbinary = \"bin/clauth-zcode-proxy\"\n".to_string(),
            "row \"zcode\": binary must be an absolute path, got bin/clauth-zcode-proxy; drop the key to run clauth-zcode-proxy from PATH".to_string(),
        ),
        (
            "[qwen]\nport = 9101\n[zcode]\nport = 9101\n".to_string(),
            "rows \"qwen\" and \"zcode\" both hold port 9101".to_string(),
        ),
    ] {
        write(&path, &text);
        assert_eq!(
            Registry::load().map_err(|e| format!("{e:#}")),
            Err(format!("invalid proxy registry {}: {message}", path.display())),
            "{text}"
        );
    }
}

// ── discovery ───────────────────────────────────────────────────────────────

/// Two PATH dirs holding one service: the first dir's wins; each other
/// service is found wherever it first appears.
#[cfg(unix)]
#[test]
fn the_first_path_dir_holding_a_service_wins() {
    let home = HomeSandbox::new();
    let first = home.home().join("first");
    let second = home.home().join("second");
    stub_proxy(&first, "clauth-zcode-proxy", ZCODE_MANIFEST);
    stub_proxy(&second, "clauth-zcode-proxy", ZCODE_MANIFEST);
    stub_proxy(&second, "clauth-qwen-proxy", ZCODE_MANIFEST);
    let path = std::env::join_paths([&first, &second]).expect("PATH");

    assert_eq!(
        discover(&path),
        vec![
            Found {
                service: service("qwen"),
                binary: second.join("clauth-qwen-proxy"),
            },
            Found {
                service: service("zcode"),
                binary: first.join("clauth-zcode-proxy"),
            },
        ]
    );
}

/// A file that is not executable, one whose `<service>` is no service, one
/// not shaped `clauth-<service>-proxy`, and a relative or missing dir are
/// all skipped; a later dir still supplies the skipped service.
#[cfg(unix)]
#[test]
fn discovery_skips_what_is_not_a_runnable_proxy() {
    let home = HomeSandbox::new();
    let first = home.home().join("first");
    let second = home.home().join("second");
    let plain = stub_proxy(&first, "clauth-zcode-proxy", ZCODE_MANIFEST);
    set_mode(&plain, 0o644);
    stub_proxy(&first, "clauth-Bad-proxy", ZCODE_MANIFEST);
    stub_proxy(&first, "clauth-x-proxy.sh", ZCODE_MANIFEST);
    stub_proxy(&first, "clauth-proxy", ZCODE_MANIFEST);
    stub_proxy(&first, "clauth--proxy", ZCODE_MANIFEST);
    fs::create_dir_all(first.join("clauth-dir-proxy")).expect("a dir named like a proxy");
    stub_proxy(&second, "clauth-zcode-proxy", ZCODE_MANIFEST);
    let path = std::env::join_paths([
        PathBuf::from("relative"),
        home.home().join("missing"),
        first,
        second.clone(),
    ])
    .expect("PATH");

    assert_eq!(
        discover(&path),
        vec![Found {
            service: service("zcode"),
            binary: second.join("clauth-zcode-proxy"),
        }]
    );
}

/// Inside one dir each service takes its best suffix whatever order the dir
/// lists them in: on Windows a native `.exe` over a `.cmd`/`.bat` shim over
/// an extensionless sh shim; elsewhere only the bare name is a program.
#[test]
fn discovery_ranks_a_dirs_suffixes_best_first() {
    let dir = PathBuf::from("/d");
    let names = [
        "clauth-zcode-proxy",
        "clauth-zcode-proxy.cmd",
        "clauth-zcode-proxy.EXE",
        "clauth-qwen-proxy",
        "clauth-qwen-proxy.bat",
        "clauth-z-proxy",
    ];
    let expected_windows: BTreeMap<Service, PathBuf> = [
        (service("qwen"), dir.join("clauth-qwen-proxy.bat")),
        (service("z"), dir.join("clauth-z-proxy")),
        (service("zcode"), dir.join("clauth-zcode-proxy.EXE")),
    ]
    .into_iter()
    .collect();
    let expected_bare: BTreeMap<Service, PathBuf> = [
        (service("qwen"), dir.join("clauth-qwen-proxy")),
        (service("z"), dir.join("clauth-z-proxy")),
        (service("zcode"), dir.join("clauth-zcode-proxy")),
    ]
    .into_iter()
    .collect();
    for listing in [names.to_vec(), names.iter().rev().copied().collect()] {
        let paths = || listing.iter().map(|name| dir.join(name));
        assert_eq!(
            best_in_dir(paths(), WINDOWS_SUFFIXES),
            expected_windows,
            "{listing:?}"
        );
        assert_eq!(best_in_dir(paths(), &[""]), expected_bare, "{listing:?}");
    }
}

// ── the manifest ────────────────────────────────────────────────────────────

#[cfg(unix)]
#[test]
fn a_manifest_reads_back_typed() {
    let home = HomeSandbox::new();
    let binary = stub_proxy(
        &home.home().join("bin"),
        "clauth-zcode-proxy",
        ZCODE_MANIFEST,
    );
    assert_eq!(
        read_manifest(&service("zcode"), &binary).map_err(|e| format!("{e:#}")),
        Ok(Manifest {
            service: service("zcode"),
            display_name: "ZCode".to_string(),
            description: Some("z.ai as the ZCode client".to_string()),
            version: "1.2.0".to_string(),
            contract: "1.0".to_string(),
            capabilities: vec!["events".to_string(), "count_tokens".to_string()],
            drain_secs: 30,
        })
    );

    let bare = stub_proxy(
        &home.home().join("bare"),
        "clauth-zcode-proxy",
        r#"{"service":"zcode","display_name":"ZCode","version":"1.2.0","contract":"1.7","capabilities":[]}"#,
    );
    assert_eq!(
        read_manifest(&service("zcode"), &bare).map_err(|e| format!("{e:#}")),
        Ok(Manifest {
            service: service("zcode"),
            display_name: "ZCode".to_string(),
            description: None,
            version: "1.2.0".to_string(),
            contract: "1.7".to_string(),
            capabilities: Vec::new(),
            drain_secs: 0,
        }),
        "a minor above 1.0 is spoken; drain_secs defaults to 0"
    );
}

/// A `manifest` that outruns its bound is killed and reaped, never left
/// running, and refused naming the bound. The 30 s ceiling sits far above
/// the 2 s bound and far below the stub's 60 s sleep.
#[cfg(unix)]
#[test]
fn a_manifest_that_outruns_its_bound_is_stopped_and_refused() {
    let home = HomeSandbox::new();
    let dir = home.home().join("bin");
    fs::create_dir_all(&dir).expect("dir");
    let binary = crate::testutil::write_shim(
        &dir,
        "clauth-zcode-proxy",
        &format!("echo $$ > '{}'; exec sleep 60", dir.join("pid").display()),
    );
    let started = std::time::Instant::now();

    let err = read_manifest_within(&service("zcode"), &binary, Duration::from_secs(2))
        .expect_err("the manifest outran its bound");

    assert!(
        started.elapsed() < Duration::from_secs(30),
        "stopped at the bound, not the stub's 60 s sleep: {:?}",
        started.elapsed()
    );
    assert_eq!(
        err.to_string(),
        format!(
            "`{} manifest` ran past 2s and was stopped",
            binary.display()
        )
    );
    let pid = fs::read_to_string(dir.join("pid")).expect("the stub recorded its pid");
    let alive = std::process::Command::new("kill")
        .args(["-0", pid.trim()])
        .stderr(std::process::Stdio::null())
        .status()
        .expect("kill -0");
    assert!(!alive.success(), "the stopped manifest {pid:?} was reaped");
}

/// Each manifest the contract refuses is refused by name: the message names
/// the binary and both sides of the mismatch.
#[cfg(unix)]
#[test]
fn every_manifest_the_contract_refuses_is_refused_naming_why() {
    let home = HomeSandbox::new();
    let zcode = service("zcode");
    let dir = home.home().join("bin");
    let bin = dir.join("clauth-zcode-proxy");
    let b = bin.display();
    let with = |fields: &str| {
        format!(
            r#"{{"service":"zcode","display_name":"ZCode","version":"1.2.0","capabilities":[],{fields}}}"#
        )
    };
    for (json, message) in [
        (
            r#"{"service":"qwen","display_name":"Q","version":"1.0.0","contract":"1.0","capabilities":[]}"#
                .to_string(),
            format!("{b} declares service \"qwen\" in its manifest, and its name says \"zcode\""),
        ),
        (
            with(r#""contract":"2.0""#),
            format!("{b} speaks contract major 2 (\"2.0\"), and clauth speaks major 1"),
        ),
        (
            with(r#""contract":"1""#),
            format!("{b} declares contract \"1\", which is not MAJOR.MINOR"),
        ),
        (
            with(r#""contract":"+1.0""#),
            format!("{b} declares contract \"+1.0\", which is not MAJOR.MINOR"),
        ),
        (
            with(r#""contract":"1.0","drain_secs":3601"#),
            format!("{b} declares drain_secs 3601, outside 0..=3600"),
        ),
        (
            with(r#""contract":"1.0","drain_secs":-1"#),
            format!("{b} declares drain_secs -1, outside 0..=3600"),
        ),
        (
            r#"{"service":"zcode"}"#.to_string(),
            format!(
                "`{b} manifest` printed no contract manifest: \"missing field `display_name` at line 1 column 19\""
            ),
        ),
    ] {
        stub_proxy(&dir, "clauth-zcode-proxy", &json);
        assert_eq!(
            read_manifest(&zcode, &bin).map_err(|e| e.to_string()),
            Err(message),
            "{json}"
        );
    }

    crate::testutil::write_shim(&dir, "clauth-zcode-proxy", "echo nope >&2; exit 3");
    assert_eq!(
        read_manifest(&zcode, &bin).map_err(|e| e.to_string()),
        Err(format!("`{b} manifest` failed (exit 3)"))
    );

    assert_eq!(
        read_manifest(&zcode, &dir.join("absent")).map_err(|e| e.to_string()),
        Err(format!(
            "cannot run {}: no such file",
            dir.join("absent").display()
        ))
    );
}

/// A manifest past 64 KiB is no manifest, and the rest of it is drained
/// rather than stalling the child on a full pipe.
#[cfg(unix)]
#[test]
fn an_oversize_manifest_is_refused() {
    let home = HomeSandbox::new();
    let dir = home.home().join("bin");
    fs::create_dir_all(&dir).expect("dir");
    let binary = crate::testutil::write_shim(
        &dir,
        "clauth-zcode-proxy",
        "head -c 70000 /dev/zero | tr '\\000' x",
    );
    assert_eq!(
        read_manifest(&service("zcode"), &binary).map_err(|e| e.to_string()),
        Err(format!(
            "`{} manifest` printed more than 65536 bytes, which is no manifest",
            binary.display()
        ))
    );
}

// ── enable and disable ──────────────────────────────────────────────────────

/// The first enable picks a free loopback port in 9101..=9199, creates the
/// state dir 0700, mints the admin token 0600 and writes the row enabled,
/// recording the binary it resolved on PATH.
#[cfg(unix)]
#[test]
fn enable_picks_a_free_port_mints_the_token_and_writes_the_row() {
    let home = HomeSandbox::new();
    let bin = home.home().join("bin");
    let binary = stub_proxy(&bin, "clauth-zcode-proxy", ZCODE_MANIFEST);
    let zcode = service("zcode");

    let bound = enable("zcode", None, Some(bin.as_os_str())).expect("enable");

    assert_eq!(bound.ip(), std::net::IpAddr::V4(Ipv4Addr::LOCALHOST));
    assert!(
        (9101..=9199).contains(&bound.port()),
        "a port outside every ephemeral range: {bound}"
    );
    assert_eq!(
        Registry::load().expect("load"),
        zcode_row(bound.port(), true, &binary)
    );
    let dir = state_dir(&zcode).unwrap();
    assert_eq!(mode(&dir), 0o700, "the state dir is owner-only");
    assert_eq!(mode(dir.parent().expect("proxies")), 0o700);
    let token = admin_token_path(&zcode).unwrap();
    assert_eq!(mode(&token), 0o600, "the admin token is owner-only");
    let text = fs::read_to_string(&token).expect("token");
    assert_eq!(text.len(), 64, "32 CSPRNG bytes, hex");
    assert!(
        text.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        "lowercase hex"
    );
}

/// A re-enable keeps the recorded port and the minted token: the proxy's
/// profiles carry the port, and a disable in between changes neither.
#[cfg(unix)]
#[test]
fn a_re_enable_keeps_the_port_and_the_token() {
    let home = HomeSandbox::new();
    let bin = home.home().join("bin");
    let binary = stub_proxy(&bin, "clauth-zcode-proxy", ZCODE_MANIFEST);
    let first = enable("zcode", None, Some(bin.as_os_str())).expect("enable");
    let token_path = admin_token_path(&service("zcode")).unwrap();
    let token = fs::read_to_string(&token_path).expect("token");

    disable("zcode").expect("disable");
    let again = enable("zcode", None, Some(bin.as_os_str())).map_err(|e| e.to_string());
    let same_port =
        enable("zcode", Some(first.port()), Some(bin.as_os_str())).map_err(|e| e.to_string());

    assert_eq!(
        (again, same_port),
        (Ok(first), Ok(first)),
        "a re-enable, bare and with --port naming the recorded port"
    );
    assert_eq!(fs::read_to_string(&token_path).ok(), Some(token));
    assert_eq!(
        Registry::load().expect("load"),
        zcode_row(first.port(), true, &binary)
    );
}

/// A re-enable resolves the binary on the CLI's PATH again and records the
/// new hit, so the daemon runs what the user's PATH now names.
#[cfg(unix)]
#[test]
fn a_re_enable_records_the_binary_path_now_resolves_to() {
    let home = HomeSandbox::new();
    let first_dir = home.home().join("first");
    let second_dir = home.home().join("second");
    let first = stub_proxy(&first_dir, "clauth-zcode-proxy", ZCODE_MANIFEST);
    let second = stub_proxy(&second_dir, "clauth-zcode-proxy", ZCODE_MANIFEST);
    let bound = enable("zcode", None, Some(first_dir.as_os_str())).expect("enable");
    assert_eq!(
        Registry::load().expect("load"),
        zcode_row(bound.port(), true, &first)
    );

    let again = enable("zcode", None, Some(second_dir.as_os_str())).map_err(|e| e.to_string());

    assert_eq!(again, Ok(bound));
    assert_eq!(
        Registry::load().expect("load"),
        zcode_row(bound.port(), true, &second)
    );
}

/// A PATH with no hit keeps the recorded binary while it exists; once it is
/// gone the re-enable is refused naming it, and the row stays.
#[cfg(unix)]
#[test]
fn a_re_enable_off_path_keeps_the_recorded_binary_while_it_exists() {
    let home = HomeSandbox::new();
    let bin = home.home().join("bin");
    let empty = home.home().join("empty");
    fs::create_dir_all(&empty).expect("dir");
    let binary = stub_proxy(&bin, "clauth-zcode-proxy", ZCODE_MANIFEST);
    let bound = enable("zcode", None, Some(bin.as_os_str())).expect("enable");

    let kept = enable("zcode", None, Some(empty.as_os_str())).map_err(|e| e.to_string());
    assert_eq!(kept, Ok(bound));
    assert_eq!(
        Registry::load().expect("load"),
        zcode_row(bound.port(), true, &binary)
    );

    fs::remove_file(&binary).expect("remove the binary");
    let err = enable("zcode", None, Some(empty.as_os_str())).expect_err("the binary is gone");
    assert!(err.downcast_ref::<crate::UsageError>().is_some(), "{err:#}");
    assert_eq!(
        Err::<(), _>(err.to_string()),
        Err(format!(
            "no clauth-zcode-proxy on PATH, and the recorded {} is gone; install the proxy, then enable it again",
            binary.display()
        ))
    );
    assert_eq!(
        Registry::load().expect("load"),
        zcode_row(bound.port(), true, &binary)
    );
}

/// `--port` on a re-enable that differs from the recorded one is refused
/// naming the recorded port, and the row stays as it was.
#[cfg(unix)]
#[test]
fn a_re_enable_asking_another_port_is_refused_naming_the_recorded_one() {
    let home = HomeSandbox::new();
    let bin = home.home().join("bin");
    let binary = stub_proxy(&bin, "clauth-zcode-proxy", ZCODE_MANIFEST);
    let first = enable("zcode", None, Some(bin.as_os_str())).expect("enable");
    let other = if first.port() == 65535 {
        1024
    } else {
        first.port() + 1
    };

    let err = enable("zcode", Some(other), Some(bin.as_os_str())).expect_err("another port");

    assert_eq!(
        err.to_string(),
        format!(
            "proxy \"zcode\" keeps its recorded port {}, which its profiles' base_url carries; drop --port",
            first.port()
        )
    );
    assert_eq!(
        Registry::load().expect("load"),
        zcode_row(first.port(), true, &binary)
    );
}

/// A proxy on another contract major is refused before anything is written:
/// no row, no state dir.
#[cfg(unix)]
#[test]
fn enable_refuses_a_foreign_contract_and_writes_nothing() {
    let home = HomeSandbox::new();
    let bin = home.home().join("bin");
    let binary = stub_proxy(
        &bin,
        "clauth-zcode-proxy",
        r#"{"service":"zcode","display_name":"ZCode","version":"1.2.0","contract":"2.1","capabilities":[]}"#,
    );

    let err = enable("zcode", None, Some(bin.as_os_str())).expect_err("contract 2");

    assert_eq!(
        err.to_string(),
        format!(
            "{} speaks contract major 2 (\"2.1\"), and clauth speaks major 1",
            binary.display()
        )
    );
    assert!(!registry_path().unwrap().exists(), "no registry written");
    assert!(!state_dir(&service("zcode")).unwrap().exists());
}

#[test]
fn enable_without_the_binary_on_path_is_refused_naming_it() {
    let home = HomeSandbox::new();
    let empty = home.home().join("empty");
    fs::create_dir_all(&empty).expect("dir");
    let err = enable("zcode", None, Some(empty.as_os_str())).expect_err("no binary");
    assert!(err.downcast_ref::<crate::UsageError>().is_some(), "{err:#}");
    assert_eq!(
        err.to_string(),
        "no clauth-zcode-proxy on PATH; install the proxy, then enable it again"
    );
    assert_eq!(
        enable("Zc", None, Some(empty.as_os_str())).map_err(|e| e.to_string()),
        Err(
            "invalid proxy service \"Zc\": a service is 1 to 32 characters of a-z, 0-9 and -, the first a letter or digit"
                .to_string()
        ),
        "the name is refused before any lookup"
    );
}

/// The picked port is the first of 9101..=9199 that binds on loopback and no
/// row holds; a range with none refuses naming the range and `--port`, and
/// never reaches past either end.
#[test]
fn the_first_free_port_of_the_range_no_row_holds_is_picked() {
    let zcode = service("zcode");
    let registry = registry_with(&[("qwen", 9101, false)]);
    assert_eq!(
        pick_port(&Registry::default(), &zcode, None, |_| Ok(())).map_err(|e| e.to_string()),
        Ok(9101)
    );
    assert_eq!(
        pick_port(&registry, &zcode, None, |port| if port == 9102 {
            Err(PortBusy::Answers)
        } else {
            Ok(())
        })
        .map_err(|e| e.to_string()),
        Ok(9103)
    );
    let err = pick_port(&registry, &zcode, None, |port| {
        if (9102..=9199).contains(&port) {
            Err(PortBusy::Unbindable(std::io::ErrorKind::AddrInUse))
        } else {
            Ok(())
        }
    })
    .expect_err("the range is spent");
    assert!(err.downcast_ref::<crate::UsageError>().is_some(), "{err:#}");
    assert_eq!(
        err.to_string(),
        "every port in 9101..=9199 is recorded for a proxy or taken on 127.0.0.1; pick one with --port"
    );
}

#[test]
fn a_port_another_row_holds_is_refused_naming_that_row() {
    let registry = registry_with(&[("qwen", 9101, false)]);
    assert_eq!(
        pick_port(&registry, &service("zcode"), Some(9101), |_| unreachable!(
            "an asked port is never probed"
        ))
        .map_err(|e| e.to_string()),
        Err("port 9101 is recorded for proxy \"qwen\"; pick another --port".to_string())
    );
}

/// A port something listens on, on the wildcard address or on loopback, is
/// never taken: the pick skips it and `--port` refuses naming it. The connect
/// probe is what catches the wildcard listener where a specific bind beside
/// it is admitted (macOS, the BSDs); once the listener is gone the port is
/// free again.
#[test]
fn a_port_something_answers_on_is_skipped_and_refused() {
    for address in [Ipv4Addr::UNSPECIFIED, Ipv4Addr::LOCALHOST] {
        let listener = TcpListener::bind((address, 0)).expect("hold a port");
        let port = listener.local_addr().expect("addr").port();

        // The listener stays held across the real-socket assert, so a
        // concurrent test cannot re-take the ephemeral port here.
        assert_eq!(
            probe_port(port),
            Err(PortBusy::Answers),
            "{address}:{port} is held"
        );
        let err = pick_port(
            &Registry::default(),
            &service("zcode"),
            Some(port),
            probe_port,
        )
        .expect_err("a held --port");
        assert!(err.downcast_ref::<crate::UsageError>().is_some(), "{err:#}");
        assert_eq!(
            err.to_string(),
            format!(
                "port {port} already answers on 127.0.0.1; pick another --port, or drop it for a free one"
            ),
            "{address}"
        );
        drop(listener);

        // A free asked port is taken as it is, through the probe seam: the
        // real `probe_port` on a just-dropped ephemeral port races another
        // test's listener taking it back.
        assert_eq!(
            pick_port(&Registry::default(), &service("zcode"), Some(port), |_| Ok(
                ()
            ),)
            .map_err(|e| e.to_string()),
            Ok(port),
            "a free asked port is taken as it is"
        );
    }
}

/// A `--port` nothing answers on but that cannot be bound is refused naming
/// the bind's error.
#[test]
fn an_asked_port_that_cannot_be_bound_is_refused_naming_why() {
    let err = pick_port(&Registry::default(), &service("zcode"), Some(80), |_| {
        Err(PortBusy::Unbindable(std::io::ErrorKind::PermissionDenied))
    })
    .expect_err("refused");
    assert!(err.downcast_ref::<crate::UsageError>().is_some(), "{err:#}");
    assert_eq!(
        err.to_string(),
        "port 80 cannot be bound on 127.0.0.1 (permission denied); pick another --port, or drop it for a free one"
    );
}

/// Whether another thread can take the state flock within 5 s; the handle
/// is joined by the caller once its own hold ends, so a waiter never
/// outlives the sandbox.
#[cfg(unix)]
fn state_lock_free_elsewhere() -> (bool, std::thread::JoinHandle<()>) {
    let (sent, received) = std::sync::mpsc::channel();
    let waiter = std::thread::spawn(move || {
        let _ = crate::lock::with_state_lock(|_| Ok(()));
        let _ = sent.send(());
    });
    let free = received.recv_timeout(Duration::from_secs(5)).is_ok();
    (free, waiter)
}

/// `enable` probes ports with the state flock free, the range walk and
/// `--port` alike: a probe can take seconds (a refused loopback connect on
/// Windows takes about 2 s), and every other writer waits on the flock.
#[cfg(unix)]
#[test]
fn enable_probes_ports_with_the_state_lock_free() {
    let home = HomeSandbox::new();
    let bin = home.home().join("bin");
    stub_proxy(&bin, "clauth-zcode-proxy", ZCODE_MANIFEST);
    stub_proxy(
        &bin,
        "clauth-qwen-proxy",
        &ZCODE_MANIFEST.replace("\"zcode\"", "\"qwen\""),
    );
    let probed = std::sync::Mutex::new(Vec::new());
    let waiters = std::sync::Mutex::new(Vec::new());
    let probe = |port: u16| {
        let (free, waiter) = state_lock_free_elsewhere();
        probed.lock().expect("probed").push((port, free));
        waiters.lock().expect("waiters").push(waiter);
        Ok(())
    };

    let picked = enable_with("zcode", None, Some(bin.as_os_str()), probe);
    let asked = enable_with("qwen", Some(9150), Some(bin.as_os_str()), probe);
    for waiter in waiters.into_inner().expect("waiters") {
        waiter.join().expect("a waiter");
    }

    assert_eq!(
        (
            picked.map_err(|e| e.to_string()),
            asked.map_err(|e| e.to_string())
        ),
        (Ok(bind(9101)), Ok(bind(9150)))
    );
    assert_eq!(
        probed.into_inner().expect("probed"),
        vec![(9101, true), (9150, true)],
        "each port probed once, with the flock free"
    );
}

/// A port a concurrent enable records between the probe and the write is
/// refused naming the row that took it, and that row stands.
#[cfg(unix)]
#[test]
fn a_port_recorded_while_it_was_probed_is_refused_naming_the_row() {
    let home = HomeSandbox::new();
    let bin = home.home().join("bin");
    stub_proxy(&bin, "clauth-zcode-proxy", ZCODE_MANIFEST);
    let raced = std::cell::Cell::new(false);

    let err = enable_with("zcode", None, Some(bin.as_os_str()), |port| {
        if !raced.replace(true) {
            Registry::update(|registry| {
                registry.rows.insert(
                    service("qwen"),
                    ProxyRow {
                        port,
                        enabled: true,
                        binary: None,
                    },
                );
                Ok(())
            })
            .expect("a concurrent enable");
        }
        Ok(())
    })
    .map_err(|e| e.to_string());

    assert_eq!(
        err,
        Err(
            "port 9101 was recorded for proxy \"qwen\" while clauth checked it; run the enable again"
                .to_string()
        )
    );
    assert_eq!(
        Registry::load().expect("load"),
        registry_with(&[("qwen", 9101, true)])
    );
}

/// A hand-set proxy token shorter than the gateway's floor is refused and
/// left alone, naming the proxy and the file.
#[test]
fn a_short_proxy_token_is_refused_and_left_alone() {
    let _home = HomeSandbox::new();
    let zcode = service("zcode");
    let path = admin_token_path(&zcode).unwrap();
    write(&path, "short\n");
    assert_eq!(
        ensure_proxy_token(&zcode).map_err(|e| format!("{e:#}")),
        Err(format!(
            "the admin token of proxy \"zcode\" in {} is shorter than 32 characters; delete the file and clauth mints a new one",
            path.display()
        ))
    );
    assert_eq!(fs::read_to_string(&path).expect("read"), "short\n");
}

/// A disable keeps the row, its port, the token and the state dir, and a
/// second one changes nothing.
#[cfg(unix)]
#[test]
fn disable_keeps_the_row_port_token_and_state_dir() {
    let home = HomeSandbox::new();
    let bin = home.home().join("bin");
    let binary = stub_proxy(&bin, "clauth-zcode-proxy", ZCODE_MANIFEST);
    let bound = enable("zcode", None, Some(bin.as_os_str())).expect("enable");
    let token_path = admin_token_path(&service("zcode")).unwrap();
    let token = fs::read_to_string(&token_path).expect("token");

    disable("zcode").expect("disable");
    let after_one = fs::read_to_string(registry_path().unwrap()).expect("registry");
    disable("zcode").expect("a second disable");

    assert_eq!(
        Registry::load().expect("load"),
        zcode_row(bound.port(), false, &binary)
    );
    assert_eq!(
        fs::read_to_string(registry_path().unwrap()).expect("registry"),
        after_one
    );
    assert_eq!(fs::read_to_string(&token_path).ok(), Some(token));
    assert!(state_dir(&service("zcode")).unwrap().is_dir());
}

#[test]
fn disable_of_an_unregistered_service_names_enable() {
    let _home = HomeSandbox::new();
    let err = disable("qwen").expect_err("an unregistered service is refused");
    assert!(err.downcast_ref::<crate::UsageError>().is_some(), "{err:#}");
    assert_eq!(
        err.to_string(),
        "no proxy \"qwen\" is registered; register it with `clauth proxy enable qwen`"
    );
    assert!(!registry_path().unwrap().exists(), "nothing written");
}
