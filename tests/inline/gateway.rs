#![allow(clippy::unwrap_used, clippy::expect_used)]
//! The gateway engine: the record and token file, discovery in shunt's order,
//! bind resolution, the env file, the version floor and `/health`, the admin
//! edit behind its `shunt check` gate, and the store move. Every test holds a
//! `HomeSandbox`; the gate tests drive a stub `shunt` written into it, a
//! `/bin/sh` script, so they are `#[cfg(unix)]`.

use std::fs;
use std::net::{SocketAddr, TcpListener};

use super::*;
use crate::testutil::{HomeSandbox, serve_endpoints};

fn write(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    fs::write(path, text).expect("write");
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

fn file_names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .expect("read_dir")
        .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// A record adopting a real `<home>/etc/shunt.toml`, the way adoption meets
/// a discovered config: the file exists and resolves.
fn adopted(home: &HomeSandbox) -> GatewayRecord {
    let config = home.home().join("etc").join("shunt.toml");
    write(&config, "[server]\n");
    GatewayRecord::new(config).expect("adoptable")
}

fn refusal(err: &anyhow::Error) -> &ConfigEditRefusal {
    err.downcast_ref::<ConfigEditRefusal>()
        .unwrap_or_else(|| panic!("a typed ConfigEditRefusal, got: {err:#}"))
}

// ── the record ──────────────────────────────────────────────────────────────

#[test]
fn a_saved_record_reads_back_and_is_owner_only() {
    let home = HomeSandbox::new();
    let mut record = adopted(&home);
    record.binary = Some(home.home().join("bin").join("shunt"));
    record.env_file = Some(home.home().join("tokens.env"));
    record.disabled = true;

    GatewayRecord::update(|slot| {
        *slot = Some(record.clone());
        Ok(())
    })
    .expect("adopt");

    assert_eq!(GatewayRecord::load().expect("load"), Some(record));
    #[cfg(unix)]
    {
        let path = record_path().expect("path");
        let canonical = fs::canonicalize(home.home()).expect("canonical home");
        let (c, h) = (canonical.display(), home.home().display());
        assert_eq!(
            fs::read_to_string(&path).expect("read"),
            format!(
                "config = \"{c}/etc/shunt.toml\"\nbinary = \"{h}/bin/shunt\"\nenv_file = \"{h}/tokens.env\"\ndisabled = true\n"
            )
        );
        assert_eq!(mode(&path), 0o600, "the record is owner-only");
    }
}

/// shunt parses `.yaml`/`.yml` (any case) as YAML, and a relative path would
/// resolve against whatever working directory the daemon has.
#[test]
fn the_record_refuses_a_config_it_cannot_edit_in_place() {
    let _home = HomeSandbox::new();
    for (config, message) in [
        (
            "shunt.toml",
            "the adopted shunt config must be an absolute path, got shunt.toml",
        ),
        (
            "/etc/shunt.yaml",
            "the adopted shunt config must be TOML, and /etc/shunt.yaml is YAML",
        ),
        (
            "/etc/Shunt.YML",
            "the adopted shunt config must be TOML, and /etc/Shunt.YML is YAML",
        ),
    ] {
        let err = GatewayRecord::new(PathBuf::from(config)).expect_err(config);
        assert_eq!(err.to_string(), message, "{config}");
    }
}

/// Every path the record holds resolves against whatever working directory
/// its reader has, so a hand-edited relative one fails the load, naming the
/// field and the fix.
#[test]
fn a_hand_edited_relative_record_fails_the_load() {
    let home = HomeSandbox::new();
    let path = record_path().expect("path");
    let config = home.home().join("etc").join("shunt.toml");
    let config = config.display();
    for (text, message) in [
        (
            "config = \"shunt.toml\"\n".to_string(),
            "the adopted shunt config must be an absolute path, got shunt.toml",
        ),
        (
            format!("config = '{config}'\nbinary = \"bin/shunt\"\n"),
            "the gateway's shunt binary must be an absolute path, got bin/shunt; drop the key to run shunt from PATH",
        ),
        (
            format!("config = '{config}'\nenv_file = \"tokens.env\"\n"),
            "the gateway's env file must be an absolute path, got tokens.env",
        ),
    ] {
        write(&path, &text);
        assert_eq!(
            GatewayRecord::load().map_err(|e| format!("{e:#}")),
            Err(format!(
                "invalid gateway record {}: {message}",
                path.display()
            )),
            "{text}"
        );
    }
}

/// The one write path: under the state flock the closure is handed the
/// record on disk (`None` before any adoption), and what it leaves lands.
#[test]
fn an_update_hands_the_closure_the_record_on_disk_and_lands_its_edit() {
    let home = HomeSandbox::new();
    let record = adopted(&home);

    let seen = GatewayRecord::update(|slot| {
        let seen = slot.clone();
        *slot = Some(record.clone());
        Ok(seen)
    })
    .expect("adopt");
    assert_eq!(seen, None, "no record before adoption");

    let seen = GatewayRecord::update(|slot| {
        let seen = slot.clone();
        if let Some(held) = slot.as_mut() {
            held.disabled = true;
        }
        Ok(seen)
    })
    .expect("edit");
    assert_eq!(seen, Some(record.clone()), "the adopted record, read back");
    assert_eq!(
        GatewayRecord::load().expect("load"),
        Some(GatewayRecord {
            disabled: true,
            ..record
        })
    );
}

/// An update that changes nothing writes nothing: a hand-edited record keeps
/// its bytes, its comment included, and its mtime.
#[test]
fn an_update_that_changes_nothing_leaves_the_file_alone() {
    let home = HomeSandbox::new();
    let record = adopted(&home);
    let path = record_path().expect("path");
    let text = format!(
        "# adopted by hand\nconfig = '{}'\n",
        record.config().display()
    );
    write(&path, &text);
    let earlier = std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    crate::testutil::set_mtime(&path, earlier);

    let seen = GatewayRecord::update(|slot| Ok(slot.clone())).expect("a no-op update");

    assert_eq!(seen, Some(record));
    assert_eq!(fs::read_to_string(&path).expect("read"), text);
    assert_eq!(
        fs::metadata(&path)
            .expect("metadata")
            .modified()
            .expect("mtime"),
        earlier
    );
}

/// An update replaces the record, never removes it: a closure that empties
/// the slot refuses and the file stays as it was.
#[test]
fn an_update_never_removes_the_record() {
    let home = HomeSandbox::new();
    let record = adopted(&home);
    GatewayRecord::update(|slot| {
        *slot = Some(record.clone());
        Ok(())
    })
    .expect("adopt");
    let path = record_path().expect("path");
    let before = fs::read_to_string(&path).expect("read");

    let emptied = GatewayRecord::update(|slot| {
        *slot = None;
        Ok(())
    });

    assert_eq!(
        emptied.map_err(|e| format!("{e:#}")),
        Err(format!(
            "an update never removes the gateway record {}",
            path.display()
        ))
    );
    assert_eq!(fs::read_to_string(&path).expect("read"), before);
}

/// A relative `binary` is refused at save too, before anything lands, not
/// only at load: the record file stays absent.
#[test]
fn an_update_refuses_a_relative_binary_before_it_lands() {
    let home = HomeSandbox::new();
    let mut record = adopted(&home);
    let path = record_path().expect("path");
    record.binary = Some(PathBuf::from("bin/shunt"));

    let err = GatewayRecord::update(|slot| {
        *slot = Some(record.clone());
        Ok(())
    })
    .expect_err("a relative binary never lands");

    assert_eq!(
        format!("{err:#}"),
        "the gateway's shunt binary must be an absolute path, got bin/shunt; drop the key to run shunt from PATH"
    );
    assert!(!path.exists(), "nothing was written");
}

// ── the admin token ─────────────────────────────────────────────────────────

#[test]
fn the_admin_token_is_minted_once_owner_only_and_long_enough() {
    let _home = HomeSandbox::new();
    let first = ensure_admin_token().expect("mint");
    let second = ensure_admin_token().expect("reuse");
    assert_eq!(first, second, "a second call reads the minted token back");
    assert_eq!(first.expose().len(), 64, "32 CSPRNG bytes, hex");
    assert!(
        first
            .expose()
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        "lowercase hex"
    );
    let path = admin_token_path().expect("path");
    assert_eq!(
        fs::read_to_string(&path).expect("read"),
        first.expose(),
        "the file holds the token alone"
    );
    #[cfg(unix)]
    assert_eq!(mode(&path), 0o600, "the token file is owner-only");
}

#[test]
fn the_admin_token_debug_never_prints_it() {
    let _home = HomeSandbox::new();
    let token = ensure_admin_token().expect("mint");
    assert_eq!(format!("{token:?}"), "AdminToken(<redacted>)");
}

#[test]
fn a_short_admin_token_file_is_refused_and_left_alone() {
    let _home = HomeSandbox::new();
    let path = admin_token_path().expect("path");
    let short = "a".repeat(31);
    write(&path, &short);
    let err = ensure_admin_token().expect_err("31 characters is below shunt's minimum");
    assert_eq!(
        format!("{err:#}"),
        format!(
            "the gateway admin token in {} is shorter than 32 characters, which shunt refuses; delete the file and clauth mints a new one",
            path.display()
        )
    );
    assert_eq!(fs::read_to_string(&path).expect("read"), short);
}

/// shunt's minimum is 32 characters, and a hand-set token of exactly that
/// length is used as it is, trimmed like a `${file:}` read.
#[test]
fn an_admin_token_file_at_shunts_minimum_length_is_accepted() {
    let _home = HomeSandbox::new();
    let path = admin_token_path().expect("path");
    let token = "b".repeat(32);
    write(&path, &format!("{token}\n"));
    assert_eq!(
        ensure_admin_token()
            .map(|minted| minted.expose().to_string())
            .map_err(|e| format!("{e:#}")),
        Ok(token)
    );
}

/// Each mint draws fresh from the CSPRNG: a deleted token file is minted
/// again as another token.
#[test]
fn a_re_minted_admin_token_differs_from_the_first() {
    let _home = HomeSandbox::new();
    let first = ensure_admin_token().expect("mint");
    fs::remove_file(admin_token_path().expect("path")).expect("delete the token file");
    let second = ensure_admin_token().expect("mint again");
    assert_ne!(first.expose(), second.expose(), "two mints, two tokens");
}

// ── discovery ───────────────────────────────────────────────────────────────

fn paths(list: &[&str]) -> Vec<PathBuf> {
    list.iter().map(PathBuf::from).collect()
}

#[test]
fn candidates_follow_shunts_search_order() {
    let _home = HomeSandbox::new();
    let inputs = DiscoveryInputs {
        cwd: Path::new("/work"),
        xdg_config_home: Some(OsStr::new("/xdg")),
        home: Some(Path::new("/home/u")),
        homebrew_prefix: Some(OsStr::new("/brew")),
    };
    assert_eq!(
        config_candidates(inputs),
        paths(&[
            "/work/shunt.toml",
            "/work/shunt.yaml",
            "/work/shunt.yml",
            "/xdg/shunt/shunt.toml",
            "/xdg/shunt/shunt.yaml",
            "/xdg/shunt/shunt.yml",
            "/brew/etc/shunt.toml",
            "/brew/etc/shunt.yaml",
            "/brew/etc/shunt.yml",
        ])
    );
}

/// An unset or empty `XDG_CONFIG_HOME` reads as `$HOME/.config`, an unset or
/// empty `HOMEBREW_PREFIX` as both stock prefixes, and no home as no XDG dir.
#[test]
fn candidates_fall_back_to_home_config_and_the_stock_brew_prefixes() {
    let _home = HomeSandbox::new();
    let fallback = paths(&[
        "/work/shunt.toml",
        "/work/shunt.yaml",
        "/work/shunt.yml",
        "/home/u/.config/shunt/shunt.toml",
        "/home/u/.config/shunt/shunt.yaml",
        "/home/u/.config/shunt/shunt.yml",
        "/opt/homebrew/etc/shunt.toml",
        "/opt/homebrew/etc/shunt.yaml",
        "/opt/homebrew/etc/shunt.yml",
        "/usr/local/etc/shunt.toml",
        "/usr/local/etc/shunt.yaml",
        "/usr/local/etc/shunt.yml",
    ]);
    for (xdg, brew) in [(None, None), (Some(OsStr::new("")), Some(OsStr::new("")))] {
        let inputs = DiscoveryInputs {
            cwd: Path::new("/work"),
            xdg_config_home: xdg,
            home: Some(Path::new("/home/u")),
            homebrew_prefix: brew,
        };
        assert_eq!(
            config_candidates(inputs),
            fallback,
            "xdg {xdg:?}, brew {brew:?}"
        );
    }
    let homeless = DiscoveryInputs {
        cwd: Path::new("/work"),
        xdg_config_home: None,
        home: None,
        homebrew_prefix: Some(OsStr::new("/brew")),
    };
    assert_eq!(
        config_candidates(homeless),
        paths(&[
            "/work/shunt.toml",
            "/work/shunt.yaml",
            "/work/shunt.yml",
            "/brew/etc/shunt.toml",
            "/brew/etc/shunt.yaml",
            "/brew/etc/shunt.yml",
        ])
    );
}

/// The sandbox's own dirs stand in for every search dir, the brew prefix
/// included, so no system path is ever probed.
fn sandbox_inputs<'a>(
    home: &'a Path,
    cwd: &'a Path,
    xdg: &'a OsStr,
    brew: &'a OsStr,
) -> DiscoveryInputs<'a> {
    DiscoveryInputs {
        cwd,
        xdg_config_home: Some(xdg),
        home: Some(home),
        homebrew_prefix: Some(brew),
    }
}

#[test]
fn the_first_existing_candidate_wins_as_an_absolute_path() {
    let home = HomeSandbox::new();
    let cwd = home.home().join("work");
    fs::create_dir_all(&cwd).expect("cwd");
    let xdg = home.home().join("xdg");
    let brew = home.home().join("brew");
    write(&xdg.join("shunt").join("shunt.toml"), "");
    write(&brew.join("etc").join("shunt.toml"), "");

    let found = discover_config_in(sandbox_inputs(
        home.home(),
        &cwd,
        xdg.as_os_str(),
        brew.as_os_str(),
    ))
    .expect("discover");
    assert_eq!(found, Some(xdg.join("shunt").join("shunt.toml")));

    // A relative XDG dir resolves against the adopting cwd, once.
    write(&cwd.join("rel-xdg").join("shunt").join("shunt.toml"), "");
    let found = discover_config_in(sandbox_inputs(
        home.home(),
        &cwd,
        OsStr::new("rel-xdg"),
        brew.as_os_str(),
    ))
    .expect("discover");
    assert_eq!(
        found,
        Some(cwd.join("rel-xdg").join("shunt").join("shunt.toml"))
    );
}

#[test]
fn a_yaml_config_shunt_loads_first_refuses_adoption() {
    let home = HomeSandbox::new();
    let cwd = home.home().join("work");
    let xdg = home.home().join("xdg");
    let brew = home.home().join("brew");
    write(&cwd.join("shunt.yml"), "server: {}\n");
    write(&xdg.join("shunt").join("shunt.toml"), "");
    fs::create_dir_all(&brew).expect("brew");

    let err = discover_config_in(sandbox_inputs(
        home.home(),
        &cwd,
        xdg.as_os_str(),
        brew.as_os_str(),
    ))
    .expect_err("never skips past the YAML to the later TOML");
    let yaml = err
        .downcast_ref::<YamlConfig>()
        .expect("a typed YamlConfig");
    assert_eq!(
        yaml,
        &YamlConfig {
            path: cwd.join("shunt.yml")
        }
    );
    assert_eq!(
        err.to_string(),
        format!(
            "shunt would load {}, a YAML config; clauth edits the adopted config in place and has no format-preserving YAML editor, so it adopts a TOML config only",
            cwd.join("shunt.yml").display()
        )
    );
}

/// Discovery reads `HOME` raw, as shunt's `find_config_file` does: a set
/// `HOME` names the XDG fallback whatever the crate's home resolver says, an
/// empty one searches a cwd-relative `.config`, and an unset one adds no XDG
/// dir at all. The sandbox's own home holds a config each leg must not find.
#[test]
fn discovery_reads_home_the_way_shunt_does() {
    let home = HomeSandbox::new();
    let cwd = home.home().join("work");
    let brew = home.home().join("brew");
    let elsewhere = home.home().join("elsewhere");
    fs::create_dir_all(&brew).expect("brew");
    let in_config = |dir: &Path| dir.join(".config").join("shunt").join("shunt.toml");
    write(&in_config(&elsewhere), "");
    write(&in_config(&cwd), "");
    write(&in_config(home.home()), "");
    let discovered = |home: Option<&OsStr>| {
        discover_config_from(&cwd, |key| match key {
            "HOME" => home.map(OsStr::to_os_string),
            "HOMEBREW_PREFIX" => Some(brew.as_os_str().to_os_string()),
            _ => None,
        })
        .expect("discover")
    };

    assert_eq!(
        discovered(Some(elsewhere.as_os_str())),
        Some(in_config(&elsewhere)),
        "a set HOME"
    );
    assert_eq!(
        discovered(Some(OsStr::new(""))),
        Some(in_config(&cwd)),
        "an empty HOME: a cwd-relative .config"
    );
    assert_eq!(discovered(None), None, "no HOME: no XDG dir");
}

#[test]
fn no_config_anywhere_reads_as_none() {
    let home = HomeSandbox::new();
    let cwd = home.home().join("work");
    let xdg = home.home().join("xdg");
    let brew = home.home().join("brew");
    for dir in [&cwd, &xdg, &brew] {
        fs::create_dir_all(dir).expect("mkdir");
    }
    let found = discover_config_in(sandbox_inputs(
        home.home(),
        &cwd,
        xdg.as_os_str(),
        brew.as_os_str(),
    ))
    .expect("discover");
    assert_eq!(found, None);
}

// ── bind ────────────────────────────────────────────────────────────────────

fn addr(text: &str) -> SocketAddr {
    text.parse().expect("socket addr")
}

#[test]
fn the_bind_is_the_env_then_the_config_then_shunts_default() {
    let _home = HomeSandbox::new();
    let wildcard_v4 = "[server]\nbind = \"0.0.0.0:4000\"\n";
    for (config, env, configured, probe) in [
        (
            wildcard_v4,
            Some("127.0.0.1:5000"),
            "127.0.0.1:5000",
            "127.0.0.1:5000",
        ),
        (wildcard_v4, None, "0.0.0.0:4000", "127.0.0.1:4000"),
        (
            "[server]\nbind = \"[::]:4000\"\n",
            None,
            "[::]:4000",
            "[::1]:4000",
        ),
        (
            "[server]\nbind = \"192.168.1.5:4000\"\n",
            None,
            "192.168.1.5:4000",
            "192.168.1.5:4000",
        ),
        (
            "[providers.x]\nkind = \"anthropic\"\n",
            None,
            "127.0.0.1:3001",
            "127.0.0.1:3001",
        ),
    ] {
        assert_eq!(
            resolve_bind(config, env).expect("resolves"),
            GatewayBind {
                configured: addr(configured),
                probe: addr(probe),
            },
            "config {config:?}, env {env:?}"
        );
    }
}

#[test]
fn an_unusable_bind_is_refused_by_its_source_never_its_value() {
    let _home = HomeSandbox::new();
    for (config, env, expected, written, message) in [
        (
            "[server]\nbind = \"localhost:3001\"\n",
            None,
            BindRefusal::NotAnAddress {
                source: "[server].bind",
            },
            Some("localhost:3001"),
            "[server].bind is not an ip:port address; set it to an ip:port address like 127.0.0.1:3001",
        ),
        (
            "[server]\nbind = 3001 # the port alone\n",
            None,
            BindRefusal::NotAnAddress {
                source: "[server].bind",
            },
            Some("3001"),
            "[server].bind is not an ip:port address; set it to an ip:port address like 127.0.0.1:3001",
        ),
        (
            "[server.bind]\nx = 1 # a comment\n",
            None,
            BindRefusal::NotAnAddress {
                source: "[server].bind",
            },
            Some("{ x = 1 }"),
            "[server].bind is not an ip:port address; set it to an ip:port address like 127.0.0.1:3001",
        ),
        (
            "[server]\nbind = \"\"\n",
            None,
            BindRefusal::NotAnAddress {
                source: "[server].bind",
            },
            Some(""),
            "[server].bind is not an ip:port address; set it to an ip:port address like 127.0.0.1:3001",
        ),
        (
            "",
            Some("secret-looking-value"),
            BindRefusal::NotAnAddress { source: BIND_ENV },
            None,
            "SHUNT_SERVER__BIND is not an ip:port address; set it to an ip:port address like 127.0.0.1:3001",
        ),
        (
            "[server]\nbind = \"127.0.0.1:0\"\n",
            None,
            BindRefusal::OsAssignedPort {
                source: "[server].bind",
            },
            Some("127.0.0.1:0"),
            "[server].bind asks for an OS-assigned port, so clauth cannot know where the gateway listens; set it to a fixed port like 127.0.0.1:3001",
        ),
        (
            "[server]\nbind = \"${GATEWAY_BIND}\"\n",
            None,
            BindRefusal::ConfigReference,
            Some("${GATEWAY_BIND}"),
            "[server].bind is a ${...} reference, which clauth does not resolve; set SHUNT_SERVER__BIND in the gateway's env file to the address instead",
        ),
    ] {
        let err = resolve_bind(config, env).expect_err(config);
        // A config value rides its refusal for the Services row; an env value
        // never does.
        let (refusal, carried) = match err.downcast_ref::<ConfigBindRefused>() {
            Some(refused) => (Some(refused.refusal), Some(refused.written.as_str())),
            None => (err.downcast_ref::<BindRefusal>().copied(), None),
        };
        assert_eq!(refusal, Some(expected), "{config:?}");
        assert_eq!(carried, written, "{config:?}");
        assert_eq!(err.to_string(), message, "{config:?}");
    }
}

/// `SHUNT_SERVER__SHUTDOWN_TIMEOUT_SECONDS` from the record's env file
/// outranks `[server].shutdown_timeout_seconds`, its name matched in any case
/// like the bind's, over an empty inherited env.
#[test]
fn the_env_file_drain_bound_matches_its_name_in_any_case() {
    let home = HomeSandbox::new();
    let config = home.home().join("etc").join("shunt.toml");
    write(&config, "[server]\nshutdown_timeout_seconds = 9\n");
    let record = GatewayRecord::new(config).expect("record");

    for text in [
        "SHUNT_SERVER__SHUTDOWN_TIMEOUT_SECONDS=7\n",
        "shunt_server__shutdown_timeout_seconds=7\n",
    ] {
        let env = parse_env_file(text.as_bytes()).expect("env");
        assert_eq!(
            gateway_shutdown_timeout_in(&record, &env, std::iter::empty()),
            Duration::from_secs(7),
            "{text:?}"
        );
    }
    assert_eq!(
        gateway_shutdown_timeout_in(&record, &GatewayEnv::default(), std::iter::empty()),
        Duration::from_secs(9)
    );
}

/// `SHUNT_SERVER__BIND` from the record's env file outranks `[server].bind`,
/// its name matched in any case the way shunt's figment env layer matches
/// it, over an empty inherited env; with no env file the adopted config's
/// own bind is read.
#[test]
fn the_env_file_bind_outranks_the_adopted_configs() {
    let home = HomeSandbox::new();
    let config = home.home().join("etc").join("shunt.toml");
    write(&config, "[server]\nbind = \"127.0.0.1:4000\"\n");
    let record = GatewayRecord::new(config).expect("record");

    #[cfg_attr(not(unix), expect(unused_mut, reason = "the unix-only leg"))]
    let mut legs = vec![
        ("SHUNT_SERVER__BIND=127.0.0.1:5000\n", "127.0.0.1:5000"),
        ("shunt_server__bind=127.0.0.1:5000\n", "127.0.0.1:5000"),
        (
            "SHUNT_SERVER__BIND=127.0.0.1:5000\nShunt_Server__Bind=127.0.0.1:6000\n",
            "127.0.0.1:6000",
        ),
    ];
    // Two spellings are two variables on unix, and the child gets its env
    // sorted by name, so the greatest name is figment's last and winning
    // match whatever the file's order; Windows folds them into one.
    #[cfg(unix)]
    legs.push((
        "Shunt_Server__Bind=127.0.0.1:6000\nSHUNT_SERVER__BIND=127.0.0.1:5000\n",
        "127.0.0.1:6000",
    ));
    for (text, bind) in legs {
        let env = parse_env_file(text.as_bytes()).expect("env");
        assert_eq!(
            gateway_bind_in(&record, &env, std::iter::empty())
                .expect("bind")
                .configured,
            addr(bind),
            "{text:?}"
        );
    }
    assert_eq!(
        gateway_bind_in(&record, &GatewayEnv::default(), std::iter::empty())
            .expect("bind")
            .configured,
        addr("127.0.0.1:4000")
    );
}

/// The inherited `SHUNT_SERVER__BIND` value is normalized the way figment
/// reads it: surrounding whitespace trimmed, and a value wholly wrapped in
/// one pair of double quotes unwrapped. A quoted value holding a backslash is
/// refused, naming the source and the fix.
#[test]
fn the_inherited_bind_env_value_is_normalized_the_way_figment_reads_it() {
    let home = HomeSandbox::new();
    let config = home.home().join("etc").join("shunt.toml");
    write(&config, "[server]\nbind = \"127.0.0.1:4000\"\n");
    let record = GatewayRecord::new(config).expect("record");

    for (value, bind) in [
        (" 127.0.0.1:5000 ", "127.0.0.1:5000"),
        ("\"127.0.0.1:5000\"", "127.0.0.1:5000"),
    ] {
        let inherited = [(OsString::from(BIND_ENV), OsString::from(value))];
        assert_eq!(
            gateway_bind_in(&record, &GatewayEnv::default(), inherited)
                .expect("bind")
                .configured,
            addr(bind),
            "{value:?}"
        );
    }

    let inherited = [(
        OsString::from(BIND_ENV),
        OsString::from("\"127.0.0.1:5\\000\""),
    )];
    let err = gateway_bind_in(&record, &GatewayEnv::default(), inherited)
        .expect_err("a backslash inside the quotes");
    assert_eq!(
        err.to_string(),
        "SHUNT_SERVER__BIND holds a backslash inside its quotes, which figment would unescape and clauth does not; write the value literally"
    );
    match err.downcast_ref::<BindRefusal>() {
        Some(BindRefusal::QuotedEscape { source }) => assert_eq!(*source, BIND_ENV),
        other => panic!("a QuotedEscape refusal, got {other:?}"),
    }
}

/// The bind read mirrors figment 0.10.19 `value()` on the measured classes:
/// whitespace is ASCII only, and a value figment's `[`-array branch rejects
/// falls back to the raw untrimmed string. Each row is a measured `shunt check`
/// outcome.
#[test]
fn the_bind_read_mirrors_figments_value_parse() {
    let home = HomeSandbox::new();
    let config = home.home().join("etc").join("shunt.toml");
    write(&config, "[server]\nbind = \"127.0.0.1:4000\"\n");
    let record = GatewayRecord::new(config).expect("record");

    for (value, bind) in [
        ("\t127.0.0.1:4997", "127.0.0.1:4997"),
        ("[::1]:4997", "[::1]:4997"),
        ("\"[::1]:4997\"", "[::1]:4997"),
    ] {
        let inherited = [(OsString::from(BIND_ENV), OsString::from(value))];
        assert_eq!(
            gateway_bind_in(&record, &GatewayEnv::default(), inherited)
                .expect("bind")
                .configured,
            addr(bind),
            "{value:?}"
        );
    }
    for value in [" [::1]:4997 ", "\u{a0}\"127.0.0.1:4997\""] {
        let inherited = [(OsString::from(BIND_ENV), OsString::from(value))];
        let err = gateway_bind_in(&record, &GatewayEnv::default(), inherited)
            .expect_err("a value shunt refuses");
        assert_eq!(
            err.to_string(),
            "SHUNT_SERVER__BIND is not an ip:port address; set it to an ip:port address like 127.0.0.1:3001",
            "{value:?}"
        );
        match err.downcast_ref::<BindRefusal>() {
            Some(BindRefusal::NotAnAddress { source }) => assert_eq!(*source, BIND_ENV),
            other => panic!("a NotAnAddress refusal, got {other:?}"),
        }
    }
}

/// The drain read takes a number exactly where shunt's figment layer does, so
/// clauth's stop bound equals shunt's drain. Each row's verdict is the
/// installed shunt's `check` on that env value: a padding of Unicode whitespace
/// around the number is accepted, while a quoted `"30"` or a trailing separator
/// is refused (`invalid type: found string`), so it reads as shunt's maximum,
/// never a short drain.
#[test]
fn the_inherited_drain_env_value_is_normalized_too() {
    let home = HomeSandbox::new();
    let config = home.home().join("etc").join("shunt.toml");
    write(&config, "[server]\nshutdown_timeout_seconds = 9\n");
    let record = GatewayRecord::new(config).expect("record");

    for (value, expected) in [
        (" 30 ", Duration::from_secs(30)),
        ("\u{a0}30", Duration::from_secs(30)),
        ("30\u{a0}", Duration::from_secs(30)),
        (" \u{a0}30", Duration::from_secs(30)),
        ("\u{b}30", Duration::from_secs(30)),
        ("\"30\"", SHUNT_MAX_SHUTDOWN_TIMEOUT),
        ("\u{a0}30,", SHUNT_MAX_SHUTDOWN_TIMEOUT),
    ] {
        let inherited = [(OsString::from(SHUTDOWN_TIMEOUT_ENV), OsString::from(value))];
        assert_eq!(
            gateway_shutdown_timeout_in(&record, &GatewayEnv::default(), inherited),
            expected,
            "{value:?}"
        );
    }
}

// ── env ─────────────────────────────────────────────────────────────────────

fn env_of(pairs: &[(&str, &str)], skipped: &[usize]) -> GatewayEnv {
    GatewayEnv {
        vars: pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), OsString::from(v)))
            .collect(),
        skipped: skipped.to_vec(),
    }
}

/// systemd's `EnvironmentFile=` grammar, pinned against what systemd 261
/// itself loaded from these exact lines (`systemd-run --user --pipe -p
/// EnvironmentFile=`, measured 2026-09-27): lines 4-19 are the engine
/// review's probe files, the rest this round's. Backslashes unescape outside
/// quotes and continue a line before a newline; inside double quotes only
/// `\"` `\\` `` \` `` `\$` unescape and any other `\c` stays whole; single
/// quotes are literal; a quote may span lines, and one left open runs to the
/// end of the file. An assignment systemd would not load is skipped by line
/// number and the rest loads.
#[test]
fn environment_file_syntax_parses_to_the_spawn_pairs() {
    let _home = HomeSandbox::new();
    let text = concat!(
        "# a comment\n",
        "; another comment\n",
        "\n",
        "A=\"x\\\"y\"\n",
        "B=a\\b\n",
        "C='a\\b'\n",
        "D=\"a\\\\b\"\n",
        "E=a\"b c\"d\n",
        "export F=1\n",
        "G=ok\n",
        "9BAD=x\n",
        "H=v # not a comment\n",
        "I = spaced \n",
        "J=\"line1\nline2\"\n",
        "K=cont\\\ninued\n",
        "L=\"  \"\n",
        "M=\n",
        "QN=\"a\\qb\"\n",
        "QO=\"a\\`b\\$c\"\n",
        "QP=\"cont\\\ninued\"\n",
        "QR='multi\nline'\n",
        "QS=a\\ \n",
        "QT=\"a\" \"b\"\n",
        "QAC=\"a\"#b\n",
        "QBH=\\\"x\\\"\n",
        "\tQAB=\tval\t\n",
        "QAF  =x\n",
        "QNOEQ\n",
        "QU=crlf\r\n",
        "QAM=x\rQAN=y\n",
        "# comment \\\n",
        "QZ=after\n",
        "QDUP=first\n",
        "QDUP=second\n",
        "qap=lower\n",
        "QW=\"abc\n",
        "QX=1\n",
    );
    assert_eq!(
        parse_env_file(text.as_bytes()),
        Ok(env_of(
            &[
                ("A", "x\"y"),
                ("B", "ab"),
                ("C", "a\\b"),
                ("D", "a\\b"),
                ("E", "a\"b c\"d"),
                ("G", "ok"),
                ("H", "v # not a comment"),
                ("I", "spaced"),
                ("J", "line1\nline2"),
                ("K", "continued"),
                ("L", "  "),
                ("M", ""),
                ("QN", "a\\qb"),
                ("QO", "a`b$c"),
                ("QP", "continued"),
                ("QR", "multi\nline"),
                ("QS", "a "),
                ("QT", "ab"),
                ("QAC", "a#b"),
                ("QBH", "\"x\""),
                ("QAB", "val"),
                ("QAF", "x"),
                ("QU", "crlf"),
                ("QAM", "x"),
                ("QAN", "y"),
                ("QZ", "after"),
                ("QDUP", "second"),
                ("qap", "lower"),
                ("QW", "abc\nQX=1\n"),
            ],
            &[9, 11, 32],
        ))
    );
}

/// systemd refuses a whole file holding a NUL byte anywhere, or an
/// assignment that is not UTF-8, a skipped name's included; it loads one
/// whose comment or `=`-less line is not UTF-8 (measured, systemd 261). The
/// refusal names the line, never its text.
#[test]
fn a_file_systemd_refuses_whole_names_the_line_never_its_text() {
    let _home = HomeSandbox::new();
    for (bytes, expected, message) in [
        (
            &b"OK=1\n# sk-SECRET \0 in a comment\n"[..],
            EnvFileError {
                line: 2,
                kind: EnvFileErrorKind::NulByte,
            },
            "line 2 holds a NUL byte; systemd refuses such a file whole, and so does clauth: remove the byte",
        ),
        (
            &b"OK=1\n\nTOKEN=sk-caf\xe9\n"[..],
            EnvFileError {
                line: 3,
                kind: EnvFileErrorKind::NotUtf8,
            },
            "the assignment on line 3 is not UTF-8; systemd refuses such a file whole, and so does clauth: save it as UTF-8",
        ),
        (
            &b"OK=1\nexport TOKEN=\"sk-caf\xe9\"\n"[..],
            EnvFileError {
                line: 2,
                kind: EnvFileErrorKind::NotUtf8,
            },
            "the assignment on line 2 is not UTF-8; systemd refuses such a file whole, and so does clauth: save it as UTF-8",
        ),
    ] {
        assert_eq!(
            parse_env_file(bytes).map_err(|err| (err.to_string(), err)),
            Err((message.to_string(), expected)),
        );
    }
    assert_eq!(
        parse_env_file(b"# caf\xe9\nOK=1\nsk-caf\xe9\n"),
        Ok(env_of(&[("OK", "1")], &[3]))
    );
}

/// A sandbox path written through [`quoted`] reads back exactly, so a `\` in
/// a Windows tempdir name survives the parser; unquoted it would be dropped.
#[test]
fn a_quoted_sandbox_path_round_trips_through_the_env_file() {
    let home = HomeSandbox::new();
    let path = home.home().join("a\\b");
    fs::create_dir_all(&path).expect("a subdir whose name holds a backslash");
    let env =
        parse_env_file(format!("CODEX_AUTH_FILE={}\n", quoted(&path)).as_bytes()).expect("parses");
    assert_eq!(env.get("CODEX_AUTH_FILE"), Some(path.as_os_str()));
}

#[test]
fn the_gateway_env_debug_shows_key_names_only() {
    let _home = HomeSandbox::new();
    let env = parse_env_file(b"TOKEN=sk-SECRET\nOTHER=x\n").expect("parses");
    assert_eq!(
        format!("{env:?}"),
        r#"GatewayEnv { keys: ["TOKEN", "OTHER"] }"#
    );
}

/// The store env is laid over the env file, so an env file cannot point the
/// managed gateway at a standalone store, nor its codex and claude fallbacks
/// at another owner's login. The env file's skipped lines ride along.
#[test]
fn the_gateway_env_is_the_env_file_then_the_stores() {
    let home = HomeSandbox::new();
    let env_file = home.home().join("tokens.env");
    write(
        &env_file,
        "SHUNT_CLAUDE_ACCOUNTS_DIR=/elsewhere\nTOKEN=from-file\nexport SKIPPED=1\nCODEX_AUTH_FILE=/elsewhere/auth.json\ncodex_auth_file=/elsewhere/lower.json\nCLAUDE_CREDENTIALS=/elsewhere/.credentials.json\n",
    );
    let mut record = adopted(&home);
    record.env_file = Some(env_file);

    assert_eq!(
        gateway_env(&record)
            .map(|env| env.skipped_lines().to_vec())
            .map_err(|e| format!("{e:#}")),
        Ok(vec![3]),
        "the invalid line is skipped, the rest loads"
    );
    let env = gateway_env(&record).expect("env");
    let stores = home.home().join(".clauth").join("shunt");
    assert_eq!(env.get("TOKEN"), Some(OsStr::new("from-file")));
    for (key, pinned) in [
        (
            "SHUNT_CLAUDE_ACCOUNTS_DIR",
            stores.join("accounts").join("claude"),
        ),
        ("CODEX_AUTH_FILE", stores.join("codex-auth.json")),
        ("CLAUDE_CREDENTIALS", stores.join("claude-credentials.json")),
    ] {
        assert_eq!(env.get(key), Some(pinned.as_os_str()), "{key}");
    }
    assert_eq!(
        env.get("codex_auth_file"),
        None,
        "a store key's case variant is dropped"
    );
    assert_eq!(
        env.keys().collect::<Vec<_>>(),
        [
            "TOKEN",
            "SHUNT_CLAUDE_ACCOUNTS_DIR",
            "SHUNT_CODEX_ACCOUNTS_DIR",
            "SHUNT_KIMI_ACCOUNTS_DIR",
            "SHUNT_ANTIGRAVITY_ACCOUNTS_DIR",
            "SHUNT_XAI_AUTH_FILE",
            "SHUNT_CURSOR_AUTH_FILE",
            "SHUNT_ANTIGRAVITY_AUTH_FILE",
            "CODEX_AUTH_FILE",
            "CLAUDE_CREDENTIALS",
        ]
    );
}

/// The store-source lookup follows `var_os`'s name rule, exercised under both
/// folds so the linux suite pins the Windows rule too: identity matches only
/// the exact spelling; upper-casing matches any spelling, the last in file
/// order winning.
#[test]
fn the_env_value_lookup_follows_the_platforms_name_rule() {
    let vars = vec![
        ("CODEX_AUTH_FILE".to_string(), OsString::from("/upper")),
        ("codex_auth_file".to_string(), OsString::from("/lower")),
    ];
    let identity = |name: &str| name.to_string();
    let upper = |name: &str| name.to_ascii_uppercase();
    assert_eq!(
        env_value_folded(&vars, "CODEX_AUTH_FILE", identity),
        Some(OsStr::new("/upper")),
        "exact spelling"
    );
    assert_eq!(
        env_value_folded(&vars, "codex_auth_file", identity),
        Some(OsStr::new("/lower")),
        "a lower-case key is its own variable on unix"
    );
    assert_eq!(
        env_value_folded(&vars, "CODEX_AUTH_FILE", upper),
        Some(OsStr::new("/lower")),
        "case-insensitive, last spelling wins"
    );
    assert_eq!(
        env_value_folded(&vars, "codex_auth_file", upper),
        Some(OsStr::new("/lower")),
        "the folded key matches either spelling"
    );
    assert_eq!(env_value_folded(&vars, "CODEX_HOME", upper), None);
}

/// A key assigned more than once takes its last assignment's position in
/// `vars`, so the Windows fold's "last in `vars`" equals the last spelling in
/// file order; pinned through `parse_env_file`, never a hand-built `vars`.
#[test]
fn a_repeated_env_file_key_folds_to_its_last_assignment() {
    let _home = HomeSandbox::new();
    for text in [
        "SHUNT_XAI_AUTH_FILE=/a\nshunt_xai_auth_file=/b\nSHUNT_XAI_AUTH_FILE=/c\n",
        "shunt_xai_auth_file=/a\nSHUNT_XAI_AUTH_FILE=/b\nshunt_xai_auth_file=/c\n",
    ] {
        let env = parse_env_file(text.as_bytes()).expect("env");
        assert_eq!(
            env_value_folded(&env.vars, "SHUNT_XAI_AUTH_FILE", |name| name
                .to_ascii_uppercase()),
            Some(OsStr::new("/c")),
            "{text:?}"
        );
    }
}

// ── version floor + /health ─────────────────────────────────────────────────

#[test]
fn the_version_floor_by_hand() {
    let _home = HomeSandbox::new();
    assert_eq!(VERSION_FLOOR.to_string(), "0.48.0");
    let below = |read: &str| {
        Err(VersionRefusal {
            read: read.to_string(),
            floor: VERSION_FLOOR,
            kind: VersionRefusalKind::BelowFloor,
        })
    };
    let unreadable = |read: &str| {
        Err(VersionRefusal {
            read: read.to_string(),
            floor: VERSION_FLOOR,
            kind: VersionRefusalKind::Unreadable,
        })
    };
    for (read, expected) in [
        ("0.47.0", below("0.47.0")),
        ("0.48.0", Ok(())),
        ("0.49.1", Ok(())),
        ("1.0.0", Ok(())),
        // semver: a pre-release sorts before its release, so an rc of the
        // floor is below it, and an rc of a later minor is above it.
        ("0.48.0-rc.1", below("0.48.0-rc.1")),
        ("0.49.0-rc.1", Ok(())),
        // build metadata takes no part in precedence.
        ("0.48.0+g554d51b", Ok(())),
        ("garbage", unreadable("garbage")),
        ("", unreadable("")),
        ("0.48", unreadable("0.48")),
        ("0.48.0.1", unreadable("0.48.0.1")),
        ("v0.48.0", unreadable("v0.48.0")),
        ("0.+48.0", unreadable("0.+48.0")),
        ("0.48.0-", unreadable("0.48.0-")),
    ] {
        assert_eq!(check_version_floor(read), expected, "{read:?}");
    }
}

#[test]
fn the_version_refusal_names_what_it_read_and_the_floor() {
    let _home = HomeSandbox::new();
    assert_eq!(
        check_version_floor("0.47.0")
            .expect_err("below")
            .to_string(),
        "shunt 0.47.0 is older than 0.48.0, the oldest release clauth supervises"
    );
    assert_eq!(
        check_version_floor("garbage")
            .expect_err("unreadable")
            .to_string(),
        "shunt reported version \"garbage\", which does not read as a release; clauth supervises 0.48.0 or newer"
    );
}

fn listener_addr(base: &str) -> SocketAddr {
    addr(base.trim_start_matches("http://"))
}

#[test]
fn a_shunt_health_answer_reads_its_version() {
    let _home = HomeSandbox::new();
    let (base, seen) = serve_endpoints(1, |_, _| {
        (200, r#"{"status":"ok","version":"0.47.0"}"#.to_string())
    });
    assert_eq!(
        probe_health(listener_addr(&base)).expect("probe"),
        Health::Shunt {
            version: "0.47.0".to_string()
        }
    );
    assert_eq!(seen.join().expect("listener"), ["/health"]);
}

#[test]
fn something_else_answering_is_not_read_as_shunt() {
    let _home = HomeSandbox::new();
    for (status, body) in [
        (404, "not found".to_string()),
        (200, "<html>hello</html>".to_string()),
        (200, r#"{"status":"ok"}"#.to_string()),
        // Past the 4 KiB body bound: shunt's body is 34 bytes.
        (200, "x".repeat(5000)),
    ] {
        let shown = body.chars().take(20).collect::<String>();
        let (base, _seen) = serve_endpoints(1, move |_, _| (status, body.clone()));
        assert_eq!(
            probe_health(listener_addr(&base)).map_err(|e| format!("{e:#}")),
            Ok(Health::NotShunt { status }),
            "{status} {shown}"
        );
    }
}

#[test]
fn a_closed_port_reads_as_silent() {
    let _home = HomeSandbox::new();
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind");
    let closed = listener.local_addr().expect("addr");
    drop(listener);
    assert_eq!(
        probe_health(closed).expect("probe"),
        Health::Silent(GatewaySilent { addr: closed }),
        "the proof names the address it was probed at"
    );
}

/// Only a refused connection is silent: a listener that takes the
/// connection and drops it is someone holding the port.
#[test]
fn a_listener_that_drops_the_connection_is_not_silent() {
    let _home = HomeSandbox::new();
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind");
    let addr = listener.local_addr().expect("addr");
    let dropper = std::thread::spawn(move || drop(listener.accept()));
    let probed = probe_health(addr).map_err(|e| format!("{e:#}"));
    dropper.join().expect("the listener thread");
    assert!(
        !matches!(probed, Ok(Health::Silent(_))),
        "a port someone holds is never silent: {probed:?}"
    );
}

/// A listener that takes the connection and never answers is not silent: it
/// fails the probe inside the short response bound (2 s, under the 6 s
/// end-to-end ceiling this asserts below) instead of parking the caller or
/// passing for an empty port.
#[test]
fn a_listener_that_never_answers_fails_the_probe_within_its_bound() {
    let _home = HomeSandbox::new();
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind");
    let started = std::time::Instant::now();
    let probed = probe_health(listener.local_addr().expect("addr"));
    assert!(
        probed.is_err(),
        "a stuck answerer fails the probe: {probed:?}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "bounded by the response bound, not the end-to-end ceiling; took {:?}",
        started.elapsed()
    );
    drop(listener);
}

// ── the admin entry: pure edits ─────────────────────────────────────────────

const KEY_REF: &str = "${file:/h/.clauth/gateway-admin-token}";

#[test]
fn the_admin_need_names_what_the_config_lacks() {
    let _home = HomeSandbox::new();
    for (text, expected) in [
        ("[providers.x]\nkind = \"a\"\n", AdminNeed::AdminTable),
        ("[server]\nbind = \"127.0.0.1:1\"\n", AdminNeed::AdminTable),
        ("[server.admin]\nheader = \"h\"\n", AdminNeed::WriteKey),
        (
            "[server]\nadmin = { header = \"h\" }\n",
            AdminNeed::WriteKey,
        ),
        (
            "[[server.admin.write_keys]]\nid = \"ops\"\nkey = \"${file:/k}\"\n",
            AdminNeed::WriteKey,
        ),
        (
            "[[server.admin.write_keys]]\nid = \"ops\"\nkey = \"${file:/k}\"\n\n[[server.admin.write_keys]]\nid = \"clauth\"\nkey = \"${file:/h/.clauth/gateway-admin-token}\"\n",
            AdminNeed::Neither,
        ),
        (
            "[server.admin]\nwrite_keys = [{ id = \"clauth\", key = '${file:/h/.clauth/gateway-admin-token}' }]\n",
            AdminNeed::Neither,
        ),
    ] {
        assert_eq!(
            admin_need_of(text, KEY_REF).expect(text),
            expected,
            "{text}"
        );
    }
    for (text, expected, message) in [
        (
            "[[server.admin.write_keys]]\nid = \"clauth\"\nkey = \"${file:/other}\"\n",
            ConfigEditRefusal::ForeignClauthKey,
            "[server.admin] already has a write key with id \"clauth\" holding another key; clauth adds none beside it; remove that entry, then run the edit again",
        ),
        (
            "server = 3\n",
            ConfigEditRefusal::UnexpectedShape {
                what: "[server] is not a table",
            },
            "[server] is not a table; clauth edits only a [server.admin] table and its write_keys array",
        ),
        (
            "[server]\nadmin = true\n",
            ConfigEditRefusal::UnexpectedShape {
                what: "[server.admin] is not a table",
            },
            "[server.admin] is not a table; clauth edits only a [server.admin] table and its write_keys array",
        ),
        (
            "[server.admin]\nwrite_keys = \"x\"\n",
            ConfigEditRefusal::UnexpectedShape {
                what: "[server.admin].write_keys is not an array of tables",
            },
            "[server.admin].write_keys is not an array of tables; clauth edits only a [server.admin] table and its write_keys array",
        ),
    ] {
        let err = admin_need_of(text, KEY_REF).expect_err(text);
        assert_eq!(refusal(&err), &expected, "{text}");
        assert_eq!(err.to_string(), message, "{text}");
    }
}

/// The user's config may hold literal upstream keys, so a parse failure names
/// the line and never quotes it.
#[test]
fn a_config_that_does_not_parse_names_the_line_never_its_text() {
    let _home = HomeSandbox::new();
    let err = admin_need_of("[server]\napi_key = sk-SECRET\n", KEY_REF)
        .expect_err("a bare value is not TOML");
    assert_eq!(
        format!("{err:#}"),
        "the adopted shunt config does not parse as TOML (line 2)"
    );
}

/// The shapes the gate tests do not reach: an inline `write_keys` array, an
/// inline `[server.admin]`, and the table offer on a config with no
/// `[server]` or an inline one. Each keeps the user's bytes around the lines
/// it adds.
#[test]
fn every_admin_shape_gains_exactly_the_entry() {
    let _home = HomeSandbox::new();
    for (text, step, expected) in [
        (
            "[server.admin]\nwrite_keys = [{ id = \"ops\", key = \"${file:/k}\" }]\n",
            AdminNeed::WriteKey,
            "[server.admin]\nwrite_keys = [{ id = \"ops\", key = \"${file:/k}\" }, { id = \"clauth\", key = \"${file:/h/.clauth/gateway-admin-token}\" }]\n",
        ),
        (
            "[server]\nadmin = { header = \"h\" }\n",
            AdminNeed::WriteKey,
            "[server]\nadmin = { header = \"h\", write_keys = [{ id = \"clauth\", key = \"${file:/h/.clauth/gateway-admin-token}\" }] }\n",
        ),
        (
            "[server]\nadmin = {header = \"h\"}\n",
            AdminNeed::WriteKey,
            "[server]\nadmin = {header = \"h\", write_keys = [{ id = \"clauth\", key = \"${file:/h/.clauth/gateway-admin-token}\" }]}\n",
        ),
        (
            "[server]\nadmin = { header = \"h\"   }\n",
            AdminNeed::WriteKey,
            "[server]\nadmin = { header = \"h\", write_keys = [{ id = \"clauth\", key = \"${file:/h/.clauth/gateway-admin-token}\" }]   }\n",
        ),
        (
            "# providers only\n[providers.x]\nkind = \"a\"\n",
            AdminNeed::AdminTable,
            "# providers only\n[providers.x]\nkind = \"a\"\n\n[server.admin]\n\n[[server.admin.write_keys]]\nid = \"clauth\"\nkey = \"${file:/h/.clauth/gateway-admin-token}\"\n",
        ),
        (
            "server = { bind = \"127.0.0.1:1\" }\n",
            AdminNeed::AdminTable,
            "server = { bind = \"127.0.0.1:1\", admin = { write_keys = [{ id = \"clauth\", key = \"${file:/h/.clauth/gateway-admin-token}\" }] } }\n",
        ),
    ] {
        assert_eq!(
            plan_admin_edit(text, KEY_REF, step).expect(text),
            Some(expected.to_string()),
            "{text}"
        );
    }
}

#[test]
fn an_edit_for_the_wrong_step_or_an_existing_entry_plans_nothing() {
    let _home = HomeSandbox::new();
    let present = "[[server.admin.write_keys]]\nid = \"clauth\"\nkey = \"${file:/h/.clauth/gateway-admin-token}\"\n";
    assert_eq!(
        plan_admin_edit(present, KEY_REF, AdminNeed::WriteKey).expect("present"),
        None
    );
    assert_eq!(
        plan_admin_edit(present, KEY_REF, AdminNeed::AdminTable).expect("present"),
        None
    );
    let err = plan_admin_edit("[server]\n", KEY_REF, AdminNeed::WriteKey).expect_err("no table");
    assert_eq!(
        refusal(&err),
        &ConfigEditRefusal::Needs(AdminNeed::AdminTable)
    );
    assert_eq!(
        err.to_string(),
        "the config has no [server.admin] table; adding one enables shunt's admin API and is its own step"
    );
    let err = plan_admin_edit("[server.admin]\n", KEY_REF, AdminNeed::AdminTable)
        .expect_err("table present");
    assert_eq!(
        refusal(&err),
        &ConfigEditRefusal::Needs(AdminNeed::WriteKey)
    );
    assert_eq!(
        err.to_string(),
        "the config already has a [server.admin] table; clauth's write key goes into it instead"
    );
}

// ── the admin entry: the write gate, over a stub `shunt` ────────────────────

#[cfg(unix)]
fn write_shim(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    write(&path, &format!("#!/bin/sh\n{body}\n"));
    set_mode(&path, 0o755);
    path
}

/// A sandbox holding an adopted config (0640, so the gate's mode carry is
/// visible), an env file naming the sandbox as `HOME`, and a stub `shunt`
/// that records each `check` call: its argv, the candidate it was handed,
/// whether the token file existed and what the env file's variables read as.
/// Markers in the stub dir steer it: `slow` records its pid first and then
/// sleeps 60 s; `fail` exits 1 with one stderr line, `loud` with 70,000
/// bytes of it, and `held` after leaving a 60 s `sleep` holding its stderr
/// (its pid in `held-pid`); `edit` appends a line to the ORIGINAL config
/// mid-check.
#[cfg(unix)]
struct Gate {
    home: HomeSandbox,
    etc: PathBuf,
    config: PathBuf,
    stub: PathBuf,
    record: GatewayRecord,
}

#[cfg(unix)]
fn gate(fixture: &str) -> Gate {
    let home = HomeSandbox::new();
    fs::create_dir_all(home.home().join("etc")).expect("etc");
    // Adoption records the canonical path; a tempdir under a symlinked
    // prefix (macOS `/var`) would otherwise read as a different path.
    let etc = fs::canonicalize(home.home().join("etc")).expect("canonical etc");
    let config = etc.join("shunt.toml");
    write(&config, fixture);
    set_mode(&config, 0o640);
    let stub = home.home().join("stub");
    fs::create_dir_all(&stub).expect("stub dir");
    let env_file = home.home().join("tokens.env");
    write(
        &env_file,
        &format!(
            "GATEWAY_TEST_SECRET=from-env-file\nHOME={}\n",
            home.home().display()
        ),
    );
    let token = admin_token_path().expect("token path");
    let body = format!(
        "d='{stub}'\n\
         if [ -e \"$d/slow\" ]; then echo $$ > \"$d/pid\"; exec sleep 60; fi\n\
         echo call >> \"$d/calls\"\n\
         printf '%s\\n' \"$@\" > \"$d/args\"\n\
         pwd -P > \"$d/cwd\"\n\
         cp \"$3\" \"$d/candidate\"\n\
         if [ -f '{token}' ]; then echo present > \"$d/token-at-check\"; fi\n\
         printf '%s' \"$HOME\" > \"$d/home-at-check\"\n\
         printf '%s' \"$GATEWAY_TEST_SECRET\" > \"$d/env-at-check\"\n\
         if [ -e \"$d/edit\" ]; then echo '# edited meanwhile' >> '{config}'; fi\n\
         if [ -e \"$d/fail\" ]; then echo 'config error: boom' >&2; exit 1; fi\n\
         if [ -e \"$d/loud\" ]; then head -c 70000 /dev/zero | tr '\\000' x >&2; exit 1; fi\n\
         if [ -e \"$d/loud-success\" ]; then head -c 200000 /dev/zero | tr '\\000' x >&2 || exit 1; fi\n\
         if [ -e \"$d/held\" ]; then echo 'config error: boom' >&2; sleep 60 >&2 & echo $! > \"$d/held-pid\"; exit 1; fi\n\
         exit 0",
        stub = stub.display(),
        token = token.display(),
        config = config.display(),
    );
    let binary = write_shim(&stub, "shunt", &body);
    let mut record = GatewayRecord::new(config.clone()).expect("record");
    record.binary = Some(binary);
    record.env_file = Some(env_file);
    Gate {
        home,
        etc,
        config,
        stub,
        record,
    }
}

#[cfg(unix)]
impl Gate {
    fn key_ref(&self) -> String {
        format!(
            "${{file:{}}}",
            self.home
                .home()
                .join(".clauth")
                .join("gateway-admin-token")
                .display()
        )
    }

    fn read(&self, name: &str) -> String {
        fs::read_to_string(self.stub.join(name)).unwrap_or_default()
    }
}

#[cfg(unix)]
const USER_CONFIG: &str = "# my gateway, hand-tuned\n\
[server]\n\
bind = \"127.0.0.1:3067\" # the port clauth probes\n\
\n\
[server.admin]\n\
# the user's own credentials stay as they are\n\
tokens_file = \"/etc/shunt/admin-tokens\"\n\
tokens_env = \"MY_ADMIN_TOKENS\"\n\
header = \"x-shunt-admin-token\"\n\
\n\
[[server.admin.write_keys]]\n\
id = \"ops\"\n\
key = \"${file:/etc/shunt/ops-key}\"\n\
\n\
# providers after the admin block\n\
[providers.anthropic]\n\
kind = \"anthropic\"\n";

/// [`USER_CONFIG`] with clauth's entry added after the user's own key and
/// every other byte kept.
#[cfg(unix)]
fn user_config_with_key(key_ref: &str) -> String {
    format!(
        "# my gateway, hand-tuned\n\
         [server]\n\
         bind = \"127.0.0.1:3067\" # the port clauth probes\n\
         \n\
         [server.admin]\n\
         # the user's own credentials stay as they are\n\
         tokens_file = \"/etc/shunt/admin-tokens\"\n\
         tokens_env = \"MY_ADMIN_TOKENS\"\n\
         header = \"x-shunt-admin-token\"\n\
         \n\
         [[server.admin.write_keys]]\n\
         id = \"ops\"\n\
         key = \"${{file:/etc/shunt/ops-key}}\"\n\
         \n\
         [[server.admin.write_keys]]\n\
         id = \"clauth\"\n\
         key = \"{key_ref}\"\n\
         \n\
         # providers after the admin block\n\
         [providers.anthropic]\n\
         kind = \"anthropic\"\n"
    )
}

#[cfg(unix)]
#[test]
fn the_write_key_lands_after_a_passing_check_keeping_the_users_bytes() {
    let g = gate(USER_CONFIG);
    assert_eq!(
        add_admin_write_key(&g.record).expect("edit"),
        AdminEdit::Written
    );

    let expected = user_config_with_key(&g.key_ref());
    assert_eq!(fs::read_to_string(&g.config).expect("read"), expected);
    assert_eq!(
        mode(&g.config),
        0o640,
        "the original's mode bits carry over"
    );

    let args = g.read("args");
    let args: Vec<&str> = args.lines().collect();
    assert_eq!(args[..2], ["check", "--config"], "argv: {args:?}");
    let candidate = Path::new(args[2]);
    assert_eq!(candidate.parent(), Some(g.etc.as_path()), "a sibling");
    assert!(
        candidate
            .file_name()
            .expect("name")
            .to_string_lossy()
            .starts_with(".shunt.toml.tmp."),
        "a hidden staging name: {candidate:?}"
    );
    assert_eq!(g.read("candidate"), expected, "the check saw what landed");
    assert_eq!(g.read("token-at-check"), "present\n");
    assert_eq!(
        g.read("home-at-check"),
        g.home.home().display().to_string(),
        "the check runs under the sandbox HOME, never the real one"
    );
    assert_eq!(g.read("env-at-check"), "from-env-file");
    assert_eq!(
        gateway_cwd(&g.record).expect("cwd"),
        g.etc.as_path(),
        "the config's own dir, the one source the spawn shares"
    );
    assert_eq!(
        g.read("cwd"),
        format!("{}\n", g.etc.display()),
        "the check ran there, not in the caller's cwd"
    );
    assert_eq!(file_names(&g.etc), ["shunt.toml"], "no staging file left");
}

/// Adoption records a symlinked config's target: the gateway runs and the
/// edit lands on the target in the target's own dir, and the user's link
/// keeps pointing at it. A TOML-named link onto a YAML file is YAML.
#[cfg(unix)]
#[test]
fn adoption_records_a_symlinked_configs_target_and_the_link_survives_the_edit() {
    let g = gate(USER_CONFIG);
    let dotfiles = g.home.home().join("dotfiles");
    fs::create_dir_all(&dotfiles).expect("dotfiles");
    let link = dotfiles.join("shunt.toml");
    std::os::unix::fs::symlink(&g.config, &link).expect("symlink");

    let mut record = GatewayRecord::new(link.clone()).expect("adopt through the link");
    assert_eq!(
        record.config(),
        g.config.as_path(),
        "the record holds the target"
    );
    record.binary = g.record.binary.clone();
    record.env_file = g.record.env_file.clone();

    assert_eq!(
        add_admin_write_key(&record).expect("edit"),
        AdminEdit::Written
    );
    assert_eq!(fs::read_link(&link).expect("still a link"), g.config);
    assert_eq!(
        fs::read_to_string(&link).expect("through the link"),
        user_config_with_key(&g.key_ref())
    );
    assert_eq!(
        file_names(&dotfiles),
        ["shunt.toml"],
        "nothing staged beside the link"
    );
    assert_eq!(g.read("cwd"), format!("{}\n", g.etc.display()));

    let yaml = g.etc.join("real.yaml");
    write(&yaml, "server: {}\n");
    let disguised = dotfiles.join("other.toml");
    std::os::unix::fs::symlink(&yaml, &disguised).expect("symlink");
    let err = GatewayRecord::new(disguised).expect_err("the target is YAML");
    assert_eq!(
        err.to_string(),
        format!(
            "the adopted shunt config must be TOML, and {} is YAML",
            yaml.display()
        )
    );
}

/// A check that outruns its bound is killed and reaped, never left running,
/// and the edit refuses with the config untouched. The 30 s ceiling sits far
/// above the 2 s bound and far below the stub's 60 s sleep, so only a check
/// left to finish on its own crosses it.
#[cfg(unix)]
#[test]
fn a_check_that_outruns_its_bound_is_stopped_and_writes_nothing() {
    let g = gate(USER_CONFIG);
    write(&g.stub.join("slow"), "");
    let _bound = CheckTimeoutOverride::set(Duration::from_secs(2));
    let started = std::time::Instant::now();

    let err = add_admin_write_key(&g.record).expect_err("the check outran its bound");

    assert!(
        started.elapsed() < Duration::from_secs(30),
        "stopped at the bound, not the stub's 60 s sleep: {:?}",
        started.elapsed()
    );
    let binary = g.stub.join("shunt");
    assert_eq!(
        refusal(&err),
        &ConfigEditRefusal::CheckTimedOut {
            binary: binary.clone(),
            config: g.config.clone(),
            after: Duration::from_secs(2),
        }
    );
    assert_eq!(
        err.to_string(),
        format!(
            "`{bin} check` ran past 2s and was stopped; the config is unchanged; run `{bin} check --config {config}` yourself to see why it does not finish, then try again",
            bin = binary.display(),
            config = g.config.display()
        )
    );
    assert_eq!(fs::read_to_string(&g.config).expect("read"), USER_CONFIG);
    assert_eq!(
        file_names(&g.etc),
        ["shunt.toml"],
        "the staging file is gone"
    );
    let pid = g.read("pid");
    assert!(!pid.trim().is_empty(), "the stub recorded its pid");
    let alive = std::process::Command::new("kill")
        .args(["-0", pid.trim()])
        .stderr(std::process::Stdio::null())
        .status()
        .expect("kill -0");
    assert!(!alive.success(), "the stopped check {pid:?} was killed");
}

/// A check's stderr is kept up to 64 KiB; the rest is drained unread, so a
/// chatty check never stalls on its pipe and never grows the refusal.
#[cfg(unix)]
#[test]
fn a_checks_stderr_is_capped() {
    let g = gate(USER_CONFIG);
    write(&g.stub.join("loud"), "");
    let err = add_admin_write_key(&g.record).expect_err("the check fails");
    match refusal(&err) {
        ConfigEditRefusal::CheckFailed { stderr, .. } => {
            assert_eq!(stderr.text(), "x".repeat(64 * 1024));
        }
        other => panic!("a CheckFailed refusal, got {other:?}"),
    }
    assert_eq!(fs::read_to_string(&g.config).expect("read"), USER_CONFIG);
}

/// A check that writes more than the cap plus the pipe buffer and then
/// succeeds still lands its edit: the rest is drained unread, never stopping
/// the check's exit.
#[cfg(unix)]
#[test]
fn a_check_that_passes_after_writing_past_the_cap_lands_its_edit() {
    let g = gate(USER_CONFIG);
    write(&g.stub.join("loud-success"), "");
    assert_eq!(
        add_admin_write_key(&g.record).expect("the check passed"),
        AdminEdit::Written
    );
    assert_eq!(
        fs::read_to_string(&g.config).expect("read"),
        user_config_with_key(&g.key_ref())
    );
}

/// A check that exits while something it started still holds its stderr is
/// read only until the check's own bound, never until that holder exits.
#[cfg(unix)]
#[test]
fn a_checks_stderr_is_read_within_the_checks_bound() {
    let g = gate(USER_CONFIG);
    write(&g.stub.join("held"), "");
    let _bound = CheckTimeoutOverride::set(Duration::from_secs(2));
    let started = std::time::Instant::now();

    let err = add_admin_write_key(&g.record).expect_err("the check fails");
    let took = started.elapsed();
    let held = g.read("held-pid");
    let stopped = std::process::Command::new("kill")
        .arg(held.trim())
        .status()
        .expect("kill the held sleep");

    assert!(
        took < Duration::from_secs(30),
        "bounded by the check's 2 s, not the holder's 60 s: {took:?}"
    );
    assert!(
        stopped.success(),
        "the stub's sleep {held:?} was still holding stderr"
    );
    match refusal(&err) {
        ConfigEditRefusal::CheckFailed { stderr, code, .. } => {
            assert_eq!((stderr.text(), *code), ("config error: boom", Some(1)));
        }
        other => panic!("a CheckFailed refusal, got {other:?}"),
    }
}

/// `shunt check`'s errors quote substituted values, which may come from the
/// env file, so a refusal's `Debug` never prints its stderr.
#[test]
fn a_check_refusals_debug_never_prints_its_stderr() {
    let _home = HomeSandbox::new();
    let refusal = ConfigEditRefusal::CheckFailed {
        binary: PathBuf::from("/opt/shunt/bin/shunt"),
        config: PathBuf::from("/etc/shunt/shunt.toml"),
        code: Some(1),
        stderr: CheckStderr("invalid type: found string \"sk-SECRET\"".to_string()),
    };
    assert_eq!(
        format!("{refusal:?}"),
        r#"CheckFailed { binary: "/opt/shunt/bin/shunt", config: "/etc/shunt/shunt.toml", code: Some(1), stderr: CheckStderr(<redacted>) }"#
    );
}

#[cfg(unix)]
#[test]
fn a_failing_check_leaves_the_config_byte_identical() {
    let g = gate(USER_CONFIG);
    write(&g.stub.join("fail"), "");
    let err = add_admin_write_key(&g.record).expect_err("the check fails");
    let binary = g.stub.join("shunt");
    assert_eq!(
        refusal(&err),
        &ConfigEditRefusal::CheckFailed {
            binary: binary.clone(),
            config: g.config.clone(),
            code: Some(1),
            stderr: CheckStderr("config error: boom".to_string()),
        }
    );
    assert_eq!(
        err.to_string(),
        format!(
            "`{} check` refused clauth's edit of {} (exit 1); the config is unchanged",
            binary.display(),
            g.config.display()
        )
    );
    assert_eq!(fs::read_to_string(&g.config).expect("read"), USER_CONFIG);
    assert_eq!(
        file_names(&g.etc),
        ["shunt.toml"],
        "the staging file is gone"
    );
}

#[cfg(unix)]
const TABLE_WITHOUT_KEYS: &str = "[server]\n\
bind = \"127.0.0.1:3067\"\n\
\n\
[server.admin]\n\
header = \"x-shunt-admin-token\"\n\
\n\
[providers.anthropic]\n\
kind = \"anthropic\"\n";

#[cfg(unix)]
#[test]
fn a_second_call_never_duplicates_the_entry() {
    let g = gate(TABLE_WITHOUT_KEYS);
    assert_eq!(
        add_admin_write_key(&g.record).expect("first"),
        AdminEdit::Written
    );
    let expected = format!(
        "[server]\n\
         bind = \"127.0.0.1:3067\"\n\
         \n\
         [server.admin]\n\
         header = \"x-shunt-admin-token\"\n\
         \n\
         [[server.admin.write_keys]]\n\
         id = \"clauth\"\n\
         key = \"{}\"\n\
         \n\
         [providers.anthropic]\n\
         kind = \"anthropic\"\n",
        g.key_ref()
    );
    assert_eq!(fs::read_to_string(&g.config).expect("read"), expected);

    assert_eq!(
        add_admin_write_key(&g.record).expect("second"),
        AdminEdit::AlreadyPresent
    );
    assert_eq!(fs::read_to_string(&g.config).expect("read"), expected);
    assert_eq!(g.read("calls"), "call\n", "the second call ran no check");
    assert_eq!(admin_need(&g.config).expect("need"), AdminNeed::Neither);
}

#[cfg(unix)]
const NO_ADMIN: &str = "# no admin yet\n\
[server]\n\
bind = \"127.0.0.1:3067\"\n\
\n\
[providers.anthropic]\n\
kind = \"anthropic\"\n";

#[cfg(unix)]
#[test]
fn the_table_offer_adds_an_admin_table_carrying_the_entry() {
    let g = gate(NO_ADMIN);
    assert_eq!(admin_need(&g.config).expect("need"), AdminNeed::AdminTable);

    let err = add_admin_write_key(&g.record).expect_err("no table to add a key to");
    assert_eq!(
        refusal(&err),
        &ConfigEditRefusal::Needs(AdminNeed::AdminTable)
    );
    assert_eq!(fs::read_to_string(&g.config).expect("read"), NO_ADMIN);
    assert_eq!(g.read("calls"), "", "a refused step runs no check");

    assert_eq!(
        add_admin_table(&g.record).expect("offer"),
        AdminEdit::Written
    );
    assert_eq!(
        fs::read_to_string(&g.config).expect("read"),
        format!(
            "# no admin yet\n\
             [server]\n\
             bind = \"127.0.0.1:3067\"\n\
             \n\
             [server.admin]\n\
             \n\
             [[server.admin.write_keys]]\n\
             id = \"clauth\"\n\
             key = \"{}\"\n\
             \n\
             [providers.anthropic]\n\
             kind = \"anthropic\"\n",
            g.key_ref()
        )
    );
    assert_eq!(admin_need(&g.config).expect("need"), AdminNeed::Neither);
}

#[cfg(unix)]
#[test]
fn a_config_edited_during_the_check_is_never_overwritten() {
    let g = gate(USER_CONFIG);
    write(&g.stub.join("edit"), "");
    let err = add_admin_write_key(&g.record).expect_err("the original moved");
    assert_eq!(
        refusal(&err),
        &ConfigEditRefusal::ChangedDuringEdit {
            path: g.config.clone()
        }
    );
    assert_eq!(
        err.to_string(),
        format!(
            "{} changed while clauth's edit was being checked; nothing was written; run the edit again",
            g.config.display()
        )
    );
    assert_eq!(
        fs::read_to_string(&g.config).expect("read"),
        format!("{USER_CONFIG}# edited meanwhile\n"),
        "the concurrent edit survives"
    );
    assert_eq!(
        file_names(&g.etc),
        ["shunt.toml"],
        "the staging file is gone"
    );
}

#[cfg(unix)]
#[test]
fn a_symlinked_config_is_refused() {
    let g = gate(USER_CONFIG);
    let link = g.etc.join("linked.toml");
    std::os::unix::fs::symlink(&g.config, &link).expect("symlink");
    let record = GatewayRecord {
        config: link.clone(),
        ..g.record.clone()
    };
    let err = add_admin_write_key(&record).expect_err("a link is never renamed over");
    assert_eq!(
        refusal(&err),
        &ConfigEditRefusal::Symlink { path: link.clone() }
    );
    assert_eq!(
        err.to_string(),
        format!(
            "{} is a symlink; clauth lands its edit by renaming over the config, which would replace the link",
            link.display()
        )
    );
    assert!(
        fs::symlink_metadata(&link)
            .expect("link")
            .file_type()
            .is_symlink()
    );
    assert_eq!(fs::read_to_string(&g.config).expect("read"), USER_CONFIG);
    assert_eq!(g.read("calls"), "", "refused before any check");

    // A link created after adoption, at the path the record holds.
    let real = g.etc.join("real.toml");
    fs::rename(&g.config, &real).expect("move the file away");
    std::os::unix::fs::symlink(&real, &g.config).expect("symlink");
    let err = add_admin_write_key(&g.record).expect_err("the adopted path became a link");
    assert_eq!(
        refusal(&err),
        &ConfigEditRefusal::Symlink {
            path: g.config.clone()
        }
    );
    assert_eq!(fs::read_to_string(&real).expect("read"), USER_CONFIG);
    assert_eq!(fs::read_link(&g.config).expect("still a link"), real);
    assert_eq!(g.read("calls"), "", "refused before any check");
}

#[test]
fn a_missing_shunt_binary_is_named_and_nothing_is_written() {
    let home = HomeSandbox::new();
    let config = home.home().join("etc").join("shunt.toml");
    let fixture = "[server.admin]\nheader = \"h\"\n";
    write(&config, fixture);
    let mut record = GatewayRecord::new(config.clone()).expect("record");
    let binary = home.home().join("nowhere").join("shunt");
    record.binary = Some(binary.clone());

    let err = add_admin_write_key(&record).expect_err("no binary");
    assert_eq!(
        refusal(&err),
        &ConfigEditRefusal::ShuntMissing {
            binary: binary.clone()
        }
    );
    assert_eq!(
        err.to_string(),
        format!(
            "cannot run {}: no such file; install shunt (brew, a release binary or cargo install --git) or point the gateway at its binary",
            binary.display()
        )
    );
    assert_eq!(fs::read_to_string(&config).expect("read"), fixture);
    assert_eq!(
        file_names(config.parent().expect("etc")),
        ["shunt.toml"],
        "the staging file is gone"
    );
}

// ── the stores ──────────────────────────────────────────────────────────────

#[test]
fn the_store_env_points_every_store_under_clauth() {
    let home = HomeSandbox::new();
    let root = home.home().join(".clauth").join("shunt");
    let accounts = root.join("accounts");
    assert_eq!(
        store_env().expect("env"),
        vec![
            ("SHUNT_CLAUDE_ACCOUNTS_DIR", accounts.join("claude")),
            ("SHUNT_CODEX_ACCOUNTS_DIR", accounts.join("codex")),
            ("SHUNT_KIMI_ACCOUNTS_DIR", accounts.join("kimi")),
            (
                "SHUNT_ANTIGRAVITY_ACCOUNTS_DIR",
                accounts.join("antigravity")
            ),
            ("SHUNT_XAI_AUTH_FILE", root.join("xai-auth.json")),
            ("SHUNT_CURSOR_AUTH_FILE", root.join("cursor-auth.json")),
            (
                "SHUNT_ANTIGRAVITY_AUTH_FILE",
                root.join("antigravity-auth.json")
            ),
            ("CODEX_AUTH_FILE", root.join("codex-auth.json")),
            ("CLAUDE_CREDENTIALS", root.join("claude-credentials.json")),
        ]
    );
}

/// `(relative path under ~/.shunt, bytes)` for one credential in each store.
const STANDALONE: [(&[&str], &str); 8] = [
    (&["accounts", "claude", "main.json"], "claude-main"),
    (&["accounts", "claude", "work.json"], "claude-work"),
    (&["accounts", "codex", "a.json"], "codex-a"),
    (&["accounts", "kimi", "k.json"], "kimi-k"),
    (&["accounts", "antigravity", "g.json"], "antigravity-g"),
    (&["xai-auth.json"], "xai"),
    (&["cursor-auth.json"], "cursor"),
    (&["antigravity-auth.json"], "antigravity-singleton"),
];

fn under(root: &Path, segments: &[&str]) -> PathBuf {
    segments.iter().fold(root.to_path_buf(), |p, s| p.join(s))
}

/// A store whose default home could not be determined, listed with its key.
fn no_home(store: &'static str) -> KeptFile {
    KeptFile {
        path: PathBuf::new(),
        reason: KeptReason::NoHome { store },
    }
}

/// [`adopted`] with its env file holding `text`.
fn with_env_file(home: &HomeSandbox, text: &str) -> GatewayRecord {
    let env_file = home.home().join("tokens.env");
    write(&env_file, text);
    let mut record = adopted(home);
    record.env_file = Some(env_file);
    record
}

/// A sandbox path as the env file's single-quoted value, so the parser keeps
/// a backslash verbatim on every platform: a Windows tempdir name holds `\`,
/// which an unquoted env file would drop (`B=a\b` parses to `ab`).
fn quoted(path: &Path) -> String {
    format!("'{}'", path.display())
}

/// [`move_standalone_stores_in`] with the sandbox `HOME` as the inherited
/// env, so every store's default resolves under the sandbox home, never a
/// production fallback.
fn move_stores(record: &GatewayRecord) -> Result<StoreMove> {
    move_standalone_stores_in(record, GatewaySilent::for_test(), sandbox_home())
}

/// The sandbox `HOME`, injected as the inherited env: the home every test
/// that does not exercise the home rules passes.
fn sandbox_home() -> [(OsString, OsString); 1] {
    [(
        OsString::from("HOME"),
        home_dir().expect("the sandbox home").into_os_string(),
    )]
}

/// Every store moves file by file into an owner-only layout that holds
/// nothing else afterwards: no staging name beside a moved file. The codex
/// CLI's own `~/.codex/auth.json` is no store and stays.
#[test]
fn the_store_move_lands_every_credential_owner_only() {
    let home = HomeSandbox::new();
    let src = home.home().join(".shunt");
    let dst = home.home().join(".clauth").join("shunt");
    for (segments, bytes) in STANDALONE {
        let path = under(&src, segments);
        write(&path, bytes);
        #[cfg(unix)]
        set_mode(&path, 0o644);
    }
    let codex_login = home.home().join(".codex").join("auth.json");
    write(&codex_login, "codex-cli-login");

    let result = move_stores(&adopted(&home)).expect("move");

    assert_eq!(
        result,
        StoreMove {
            moved: STANDALONE
                .iter()
                .map(|(segments, _)| MovedFile {
                    from: under(&src, segments),
                    to: under(&dst, segments),
                })
                .collect(),
            kept: Vec::new(),
        }
    );
    for (segments, bytes) in STANDALONE {
        assert_eq!(
            fs::read_to_string(under(&dst, segments)).expect("moved"),
            bytes,
            "{segments:?}"
        );
        assert!(
            !under(&src, segments).exists(),
            "{segments:?} left its source"
        );
        #[cfg(unix)]
        assert_eq!(mode(&under(&dst, segments)), 0o600, "{segments:?}");
    }
    assert_eq!(
        file_names(&dst.join("accounts").join("claude")),
        ["main.json", "work.json"]
    );
    assert_eq!(file_names(&dst.join("accounts").join("codex")), ["a.json"]);
    assert_eq!(
        file_names(&dst),
        [
            "accounts",
            "antigravity-auth.json",
            "cursor-auth.json",
            "xai-auth.json"
        ]
    );
    assert_eq!(
        fs::read_to_string(&codex_login).expect("the codex login"),
        "codex-cli-login"
    );
    #[cfg(unix)]
    for dir in [
        home.home().join(".clauth"),
        dst.clone(),
        dst.join("accounts"),
        dst.join("accounts").join("claude"),
        dst.join("accounts").join("antigravity"),
    ] {
        assert_eq!(mode(&dir), 0o700, "{dir:?}");
    }
}

#[test]
fn a_collision_refuses_the_whole_move_leaving_both_sides_byte_identical() {
    let home = HomeSandbox::new();
    let src = home.home().join(".shunt");
    let dst = home.home().join(".clauth").join("shunt");
    let main = ["accounts", "claude", "main.json"];
    let codex = ["accounts", "codex", "a.json"];
    let kimi = ["accounts", "kimi", "k.json"];
    write(&under(&src, &main), "source-main");
    write(&under(&src, &codex), "source-codex");
    write(&under(&src, &kimi), "kimi-k");
    write(&under(&dst, &main), "destination-main");
    write(&under(&dst, &codex), "destination-codex");

    let err = move_stores(&adopted(&home)).expect_err("a collision");
    assert_eq!(
        err.to_string(),
        format!(
            "the managed store already holds {}, {}; nothing was moved; compare each with its standalone copy, remove the one you no longer need, then run the move again",
            under(&dst, &main).display(),
            under(&dst, &codex).display()
        )
    );
    match err.downcast_ref::<StoreMoveRefusal>() {
        Some(StoreMoveRefusal::Collision { paths }) => {
            assert_eq!(paths, &[under(&dst, &main), under(&dst, &codex)]);
        }
        other => panic!("a Collision refusal, got {other:?}"),
    }
    for (segments, source, destination) in [
        (&main, "source-main", "destination-main"),
        (&codex, "source-codex", "destination-codex"),
    ] {
        assert_eq!(
            fs::read_to_string(under(&src, segments)).expect("src"),
            source
        );
        assert_eq!(
            fs::read_to_string(under(&dst, segments)).expect("dst"),
            destination
        );
    }
    assert_eq!(
        fs::read_to_string(under(&src, &kimi)).expect("kimi src"),
        "kimi-k",
        "nothing moves once any destination collides"
    );
    assert!(!under(&dst, &kimi).exists());
}

/// The collision check runs before any move; a destination that appears
/// after it is still never overwritten, because the publish is a hard link
/// that refuses an existing name.
#[test]
fn a_destination_appearing_after_the_collision_check_is_never_overwritten() {
    let home = HomeSandbox::new();
    let from = home.home().join("from.json");
    let to = home.home().join("managed").join("to.json");
    write(&from, "source-bytes");
    write(&to, "late-arrival");

    match move_credential(&from, &to) {
        Err(MoveFailure::BeforeCopy(e)) => {
            assert_eq!(e.kind(), std::io::ErrorKind::AlreadyExists);
        }
        other => panic!("refused before the copy landed, got {other:?}"),
    }
    assert_eq!(fs::read_to_string(&from).expect("src"), "source-bytes");
    assert_eq!(fs::read_to_string(&to).expect("dst"), "late-arrival");
    assert_eq!(
        file_names(to.parent().expect("managed")),
        ["to.json"],
        "no staging file left"
    );
}

/// A destination dir the move cannot write into stops it after the claude
/// store landed: the codex credential is at its source only, the kimi one
/// after it untouched, and the refusal says so and what to do.
#[cfg(unix)]
#[test]
fn a_failure_part_way_leaves_each_credential_at_its_source_or_destination() {
    let home = HomeSandbox::new();
    let src = home.home().join(".shunt");
    let dst = home.home().join(".clauth").join("shunt");
    let main = ["accounts", "claude", "main.json"];
    let work = ["accounts", "claude", "work.json"];
    let codex = ["accounts", "codex", "a.json"];
    let kimi = ["accounts", "kimi", "k.json"];
    for (segments, bytes) in [
        (&main, "claude-main"),
        (&work, "claude-work"),
        (&codex, "codex-a"),
        (&kimi, "kimi-k"),
    ] {
        write(&under(&src, segments), bytes);
    }
    let locked = dst.join("accounts").join("codex");
    fs::create_dir_all(&locked).expect("codex dst");
    set_mode(&locked, 0o500);

    let result = move_stores(&adopted(&home));
    set_mode(&locked, 0o700);

    let err = result.expect_err("the codex dir refuses the copy");
    assert_eq!(
        err.to_string(),
        format!(
            "moving {codex} failed (Permission denied (os error 13)); 2 file(s) had moved, {codex} is still at its source only, and every file after it was left untouched; fix the cause, then run the move again",
            codex = under(&src, &codex).display()
        )
    );
    match err.downcast_ref::<StoreMoveRefusal>() {
        Some(StoreMoveRefusal::FailedBeforeCopy {
            moved,
            failed,
            cause,
        }) => {
            assert_eq!(
                moved,
                &[
                    MovedFile {
                        from: under(&src, &main),
                        to: under(&dst, &main),
                    },
                    MovedFile {
                        from: under(&src, &work),
                        to: under(&dst, &work),
                    },
                ]
            );
            assert_eq!(failed, &under(&src, &codex));
            assert_eq!(cause.kind(), std::io::ErrorKind::PermissionDenied);
        }
        other => panic!("a FailedBeforeCopy refusal, got {other:?}"),
    }
    assert_eq!(
        fs::read_to_string(under(&dst, &main)).expect("landed"),
        "claude-main"
    );
    assert!(!under(&src, &main).exists());
    assert_eq!(
        fs::read_to_string(under(&src, &codex)).expect("kept"),
        "codex-a"
    );
    assert_eq!(file_names(&locked), Vec::<String>::new(), "no stray copy");
    assert_eq!(
        fs::read_to_string(under(&src, &kimi)).expect("untouched"),
        "kimi-k"
    );
    assert!(!under(&dst, &kimi).exists());
}

/// A source dir the move cannot unlink from stops it after the credential
/// was published at its destination: it is at both places, and the refusal
/// says so and what to do.
#[cfg(unix)]
#[test]
fn a_failure_after_the_copy_leaves_the_credential_at_both_places_and_says_so() {
    let home = HomeSandbox::new();
    let src = home.home().join(".shunt");
    let dst = home.home().join(".clauth").join("shunt");
    let main = ["accounts", "claude", "main.json"];
    let work = ["accounts", "claude", "work.json"];
    let codex = ["accounts", "codex", "a.json"];
    let kimi = ["accounts", "kimi", "k.json"];
    for (segments, bytes) in [
        (&main, "claude-main"),
        (&work, "claude-work"),
        (&codex, "codex-a"),
        (&kimi, "kimi-k"),
    ] {
        write(&under(&src, segments), bytes);
    }
    let locked = src.join("accounts").join("codex");
    set_mode(&locked, 0o500);

    let result = move_stores(&adopted(&home));
    set_mode(&locked, 0o700);

    let err = result.expect_err("the codex source dir refuses the unlink");
    assert_eq!(
        err.to_string(),
        format!(
            "moving {from} failed after it was copied to {to} (Permission denied (os error 13)): it is now at both places; delete {from} once {to} reads correctly, then run the move again; 2 file(s) had moved before it, and every file after it was left untouched",
            from = under(&src, &codex).display(),
            to = under(&dst, &codex).display()
        )
    );
    match err.downcast_ref::<StoreMoveRefusal>() {
        Some(StoreMoveRefusal::FailedAfterCopy {
            moved,
            failed,
            cause,
        }) => {
            assert_eq!(
                moved,
                &[
                    MovedFile {
                        from: under(&src, &main),
                        to: under(&dst, &main),
                    },
                    MovedFile {
                        from: under(&src, &work),
                        to: under(&dst, &work),
                    },
                ]
            );
            assert_eq!(
                failed,
                &MovedFile {
                    from: under(&src, &codex),
                    to: under(&dst, &codex),
                }
            );
            assert_eq!(cause.kind(), std::io::ErrorKind::PermissionDenied);
        }
        other => panic!("a FailedAfterCopy refusal, got {other:?}"),
    }
    for place in [under(&src, &codex), under(&dst, &codex)] {
        assert_eq!(fs::read_to_string(&place).expect("both"), "codex-a");
    }
    assert_eq!(
        file_names(&dst.join("accounts").join("codex")),
        ["a.json"],
        "no staging name beside it"
    );
    assert_eq!(
        fs::read_to_string(under(&src, &kimi)).expect("untouched"),
        "kimi-k"
    );
    assert!(!under(&dst, &kimi).exists());
}

#[cfg(unix)]
#[test]
fn a_link_inside_a_store_is_left_behind_and_the_files_around_it_move() {
    let home = HomeSandbox::new();
    let src = home.home().join(".shunt");
    let dst = home.home().join(".clauth").join("shunt");
    let main = under(&src, &["accounts", "claude", "main.json"]);
    write(&main, "claude-main");
    let link = under(&src, &["accounts", "claude", "link.json"]);
    std::os::unix::fs::symlink(&main, &link).expect("symlink");

    assert_eq!(
        move_stores(&adopted(&home)).expect("a link inside a store never refuses"),
        StoreMove {
            moved: vec![MovedFile {
                from: main.clone(),
                to: under(&dst, &["accounts", "claude", "main.json"]),
            }],
            kept: vec![KeptFile {
                path: link.clone(),
                reason: KeptReason::LeftBehind,
            }],
        }
    );
    assert_eq!(
        fs::read_to_string(under(&dst, &["accounts", "claude", "main.json"])).expect("moved"),
        "claude-main"
    );
    assert!(!main.exists(), "the account file left its source");
    assert_eq!(
        fs::read_link(&link).expect("still a link"),
        main,
        "the link stays"
    );
}

/// A dir store moves only the top-level regular `<[a-z0-9-]+>.json` account
/// files shunt serves; a subdir, a link and a non-account file stay behind in
/// the old dir and are listed as left behind.
#[cfg(unix)]
#[test]
fn a_dir_store_moves_only_the_account_files_and_lists_the_rest_as_left_behind() {
    let home = HomeSandbox::new();
    let src = home.home().join(".shunt");
    let dst = home.home().join(".clauth").join("shunt");
    let claude = ["accounts", "claude"];
    let main = under(&src, &["accounts", "claude", "main.json"]);
    write(&main, "claude-main");
    let backup = under(&src, &["accounts", "claude", "backup"]);
    write(&backup.join("old.json"), "claude-old");
    let link = under(&src, &["accounts", "claude", "linked.json"]);
    std::os::unix::fs::symlink(&main, &link).expect("symlink");
    let notes = under(&src, &["accounts", "claude", "notes.txt"]);
    write(&notes, "not an account");
    // No lower-case twin: a case-insensitive filesystem (macOS's APFS) folds
    // `Main.json` onto `main.json`.
    let caps = under(&src, &["accounts", "claude", "Upper.json"]);
    write(&caps, "capitalized stem");
    let a_b = under(&src, &["accounts", "claude", "a_b.json"]);
    write(&a_b, "underscored stem");

    assert_eq!(
        move_stores(&adopted(&home)).expect("the rest never refuses"),
        StoreMove {
            moved: vec![MovedFile {
                from: main.clone(),
                to: under(&dst, &claude).join("main.json"),
            }],
            kept: vec![
                KeptFile {
                    path: caps.clone(),
                    reason: KeptReason::LeftBehind,
                },
                KeptFile {
                    path: a_b.clone(),
                    reason: KeptReason::LeftBehind,
                },
                KeptFile {
                    path: backup.clone(),
                    reason: KeptReason::LeftBehind,
                },
                KeptFile {
                    path: link.clone(),
                    reason: KeptReason::LeftBehind,
                },
                KeptFile {
                    path: notes.clone(),
                    reason: KeptReason::LeftBehind,
                },
            ],
        }
    );
    assert_eq!(
        fs::read_to_string(&caps).expect("kept"),
        "capitalized stem",
        "a capitalized stem is no account"
    );
    assert_eq!(
        fs::read_to_string(&a_b).expect("kept"),
        "underscored stem",
        "an underscored stem is no account"
    );
    assert_eq!(
        fs::read_to_string(backup.join("old.json")).expect("subdir kept"),
        "claude-old",
        "the subdir and its file stay in the old dir"
    );
    assert_eq!(fs::read_link(&link).expect("still a link"), main);
    assert_eq!(
        fs::read_to_string(&notes).expect("notes kept"),
        "not an account"
    );
    assert_eq!(
        fs::read_to_string(under(&dst, &claude).join("main.json")).expect("moved"),
        "claude-main"
    );
    assert!(!main.exists());
}

/// A store that is itself a link (a single file or a store dir root) refuses
/// the move naming the link, and both link targets stay byte-identical.
#[cfg(unix)]
#[test]
fn a_store_that_is_a_link_refuses_the_move_naming_the_link() {
    // A single-file store: ~/.shunt/xai-auth.json linked onto a sandbox file.
    {
        let home = HomeSandbox::new();
        let target = home.home().join("real-xai.json");
        write(&target, "xai-real");
        let link = home.home().join(".shunt").join("xai-auth.json");
        fs::create_dir_all(link.parent().expect(".shunt")).expect(".shunt");
        std::os::unix::fs::symlink(&target, &link).expect("symlink");

        let err = move_stores(&adopted(&home)).expect_err("a symlinked store file");
        assert_eq!(
            err.to_string(),
            format!(
                "{} is a link, and the move takes over only a store that is the directory or file itself; nothing was moved; replace the link with what it points at, or point the env file at the target, then run the move again",
                link.display()
            )
        );
        match err.downcast_ref::<StoreMoveRefusal>() {
            Some(StoreMoveRefusal::StoreLink { path }) => assert_eq!(path, &link),
            other => panic!("a StoreLink refusal, got {other:?}"),
        }
        assert_eq!(fs::read_to_string(&target).expect("target"), "xai-real");
        assert!(!home.home().join(".clauth").join("shunt").exists());
    }
    // A store dir root: ~/.shunt/accounts/codex linked onto a sandbox dir.
    {
        let home = HomeSandbox::new();
        let target = home.home().join("real-codex-dir");
        fs::create_dir_all(&target).expect("dir");
        write(&target.join("a.json"), "codex-a");
        let link = home.home().join(".shunt").join("accounts").join("codex");
        fs::create_dir_all(link.parent().expect("accounts")).expect("accounts");
        std::os::unix::fs::symlink(&target, &link).expect("symlink");

        let err = move_stores(&adopted(&home)).expect_err("a symlinked store dir");
        assert_eq!(
            err.to_string(),
            format!(
                "{} is a link, and the move takes over only a store that is the directory or file itself; nothing was moved; replace the link with what it points at, or point the env file at the target, then run the move again",
                link.display()
            )
        );
        match err.downcast_ref::<StoreMoveRefusal>() {
            Some(StoreMoveRefusal::StoreLink { path }) => assert_eq!(path, &link),
            other => panic!("a StoreLink refusal, got {other:?}"),
        }
        assert_eq!(
            fs::read_to_string(target.join("a.json")).expect("target"),
            "codex-a"
        );
        assert!(!home.home().join(".clauth").join("shunt").exists());
    }
}

/// A dir store whose path is a regular file refuses naming the store kind,
/// and a single-file store whose path is a directory likewise.
#[test]
fn a_store_of_the_wrong_kind_refuses_naming_the_kind() {
    // A dir store (~/.shunt/accounts/codex) that is a regular file.
    {
        let home = HomeSandbox::new();
        let store = home.home().join(".shunt").join("accounts").join("codex");
        write(&store, "a file where a dir was expected");

        let err = move_stores(&adopted(&home)).expect_err("a file for a dir store");
        assert_eq!(
            err.to_string(),
            format!(
                "{} is not a directory, and the env file or shunt's default names it as an account store; nothing was moved; point the store at a directory, then run the move again",
                store.display()
            )
        );
        match err.downcast_ref::<StoreMoveRefusal>() {
            Some(StoreMoveRefusal::StoreNotDir { path }) => assert_eq!(path, &store),
            other => panic!("a StoreNotDir refusal, got {other:?}"),
        }
        assert_eq!(
            fs::read_to_string(&store).expect("kept"),
            "a file where a dir was expected"
        );
        assert!(!home.home().join(".clauth").join("shunt").exists());
    }
    // A single-file store (~/.shunt/xai-auth.json) that is a directory.
    {
        let home = HomeSandbox::new();
        let store = home.home().join(".shunt").join("xai-auth.json");
        fs::create_dir_all(&store).expect("a dir where a file was expected");

        let err = move_stores(&adopted(&home)).expect_err("a dir for a file store");
        assert_eq!(
            err.to_string(),
            format!(
                "{} is not a regular file, and the env file or shunt's default names it as an account store; nothing was moved; point the store at a regular file, then run the move again",
                store.display()
            )
        );
        match err.downcast_ref::<StoreMoveRefusal>() {
            Some(StoreMoveRefusal::StoreNotFile { path }) => assert_eq!(path, &store),
            other => panic!("a StoreNotFile refusal, got {other:?}"),
        }
        assert!(store.is_dir(), "the dir stays");
        assert!(!home.home().join(".clauth").join("shunt").exists());
    }
}

/// The record's env file names where each standalone store lived, and a
/// blank value means what shunt makes of it for that store: an account dir
/// set empty or to whitespace is unset (shunt's `env_path_override`), and so
/// is an empty `SHUNT_ANTIGRAVITY_AUTH_FILE`, so their defaults move; shunt
/// reads `SHUNT_XAI_AUTH_FILE` raw, so an empty one named no store at all,
/// and the default xai file, which that standalone never used, stays.
#[test]
fn an_env_file_names_each_store_source_and_a_blank_value_reads_as_shunt_reads_it() {
    let home = HomeSandbox::new();
    let src = home.home().join(".shunt");
    let dst = home.home().join(".clauth").join("shunt");
    let custom = home.home().join("custom").join("codex");
    let record = with_env_file(
        &home,
        &format!(
            "SHUNT_CODEX_ACCOUNTS_DIR={}\nSHUNT_CLAUDE_ACCOUNTS_DIR=   \nSHUNT_KIMI_ACCOUNTS_DIR=\"  \"\nSHUNT_XAI_AUTH_FILE=\nSHUNT_ANTIGRAVITY_AUTH_FILE=\n",
            quoted(&custom)
        ),
    );
    let main = ["accounts", "claude", "main.json"];
    let stale = ["accounts", "codex", "stale.json"];
    let kimi = ["accounts", "kimi", "k.json"];
    let xai = ["xai-auth.json"];
    let antigravity = ["antigravity-auth.json"];
    write(&custom.join("a.json"), "custom-codex");
    for (segments, bytes) in [
        (&stale[..], "default-codex"),
        (&main[..], "claude-main"),
        (&kimi[..], "kimi-k"),
        (&xai[..], "xai"),
        (&antigravity[..], "antigravity-singleton"),
    ] {
        write(&under(&src, segments), bytes);
    }

    let result = move_stores(&record).expect("move");

    assert_eq!(
        result,
        StoreMove {
            moved: vec![
                MovedFile {
                    from: under(&src, &main),
                    to: under(&dst, &main),
                },
                MovedFile {
                    from: custom.join("a.json"),
                    to: under(&dst, &["accounts", "codex", "a.json"]),
                },
                MovedFile {
                    from: under(&src, &kimi),
                    to: under(&dst, &kimi),
                },
                MovedFile {
                    from: under(&src, &antigravity),
                    to: under(&dst, &antigravity),
                },
            ],
            kept: Vec::new(),
        }
    );
    assert_eq!(
        fs::read_to_string(under(&dst, &["accounts", "codex", "a.json"])).expect("moved"),
        "custom-codex"
    );
    assert!(!custom.join("a.json").exists(), "left its named source");
    assert_eq!(
        fs::read_to_string(under(&src, &stale)).expect("kept"),
        "default-codex",
        "the default codex dir is not the named source"
    );
    assert!(!under(&dst, &stale).exists());
    assert_eq!(
        fs::read_to_string(under(&src, &xai)).expect("kept"),
        "xai",
        "an empty SHUNT_XAI_AUTH_FILE named no store"
    );
    assert!(!under(&dst, &xai).exists());
}

/// On unix a lower-case store key is a different variable, as shunt's exact
/// `var_os` reads it: the default store moves and the file the lower-case key
/// names stays untouched.
#[cfg(unix)]
#[test]
fn a_lower_case_store_key_on_unix_leaves_its_file_untouched() {
    let home = HomeSandbox::new();
    let src = home.home().join(".shunt");
    let dst = home.home().join(".clauth").join("shunt");
    let xai = under(&src, &["xai-auth.json"]);
    write(&xai, "xai");
    let other = home.home().join("other.json");
    write(&other, "other-xai");
    let record = with_env_file(&home, &format!("shunt_xai_auth_file={}\n", quoted(&other)));

    assert_eq!(
        move_stores(&record).expect("move"),
        StoreMove {
            moved: vec![MovedFile {
                from: xai.clone(),
                to: under(&dst, &["xai-auth.json"]),
            }],
            kept: Vec::new(),
        }
    );
    assert_eq!(
        fs::read_to_string(under(&dst, &["xai-auth.json"])).expect("moved"),
        "xai"
    );
    assert!(!xai.exists(), "the default xai file left its source");
    assert_eq!(fs::read_to_string(&other).expect("untouched"), "other-xai");
}

/// A relative store path in the env file resolved against whatever cwd the
/// standalone had, and a whitespace value is one for every store shunt does
/// not read blank-as-unset. The move refuses before moving anything, naming
/// the key and never the value.
#[test]
fn a_relative_store_source_refuses_the_move_naming_only_its_key() {
    for (line, key) in [
        (
            "SHUNT_XAI_AUTH_FILE=relative/xai-secret-path.json",
            "SHUNT_XAI_AUTH_FILE",
        ),
        ("SHUNT_CURSOR_AUTH_FILE=\"  \"", "SHUNT_CURSOR_AUTH_FILE"),
        (
            "SHUNT_ANTIGRAVITY_AUTH_FILE=\" \"",
            "SHUNT_ANTIGRAVITY_AUTH_FILE",
        ),
        ("CODEX_AUTH_FILE=\" \"", "CODEX_AUTH_FILE"),
    ] {
        let home = HomeSandbox::new();
        let main = under(
            &home.home().join(".shunt"),
            &["accounts", "claude", "main.json"],
        );
        write(&main, "claude-main");
        let record = with_env_file(&home, &format!("{line}\n"));

        let result = move_stores(&record);

        assert_eq!(
            result.as_ref().map(|_| ()).map_err(ToString::to_string),
            Err(format!(
                "the env file sets {key} to a relative path, and clauth cannot tell which directory the standalone resolved it against; nothing was moved; set {key} to an absolute path in the env file, then run the move again"
            )),
            "{line}"
        );
        match result.expect_err(line).downcast_ref::<StoreMoveRefusal>() {
            Some(StoreMoveRefusal::RelativeSource { key: named }) => assert_eq!(*named, key),
            other => panic!("a RelativeSource refusal, got {other:?}"),
        }
        assert_eq!(fs::read_to_string(&main).expect("kept"), "claude-main");
        assert!(!home.home().join(".clauth").join("shunt").exists());
    }
}

/// A store's default is shunt's default, resolved under the standalone's own
/// `HOME` (the env file's value over the inherited env), never clauth's home:
/// an env file `HOME` pointing elsewhere moves the store from there while
/// clauth's own `~/.shunt` stays untouched.
#[test]
fn the_default_store_root_is_the_home_the_env_file_gave_the_standalone() {
    let home = HomeSandbox::new();
    let alt = home.home().join("alt");
    let main = ["accounts", "claude", "main.json"];
    write(&under(&alt.join(".shunt"), &main), "alt-main");
    // A file under clauth's own `~/.shunt`: the move reads only the
    // standalone's home (the env file's `alt`), so this stays untouched.
    let own = home
        .home()
        .join(".shunt")
        .join("accounts")
        .join("claude")
        .join("other.json");
    write(&own, "own-home");
    let record = with_env_file(&home, &format!("HOME={}\n", quoted(&alt)));

    let result = move_stores(&record).expect("move");

    assert_eq!(
        result,
        StoreMove {
            moved: vec![MovedFile {
                from: under(&alt.join(".shunt"), &main),
                to: under(&home.home().join(".clauth").join("shunt"), &main),
            }],
            kept: Vec::new(),
        }
    );
    assert_eq!(
        fs::read_to_string(under(&home.home().join(".clauth").join("shunt"), &main))
            .expect("moved"),
        "alt-main"
    );
    assert!(
        !under(&alt.join(".shunt"), &main).exists(),
        "left its source"
    );
    assert_eq!(
        fs::read_to_string(&own).expect("still there"),
        "own-home",
        "clauth's own home was not read"
    );
}

/// Each store family's default home follows its shunt site's rule: the
/// account dirs, cursor and antigravity read `HOME` (non-empty) else
/// `USERPROFILE` (non-empty), xai reads raw `HOME` with no `USERPROFILE`
/// fallback. With no `HOME` and a `USERPROFILE` set, cursor's default sits
/// under `USERPROFILE` while xai has no determinable home: it is listed, and
/// its default under clauth's own home is never read.
#[test]
fn each_familys_default_home_follows_its_shunt_site_rule() {
    let home = HomeSandbox::new();
    let winhome = home.home().join("winhome");
    let win_cursor = winhome.join(".shunt").join("cursor-auth.json");
    let win_xai = winhome.join(".shunt").join("xai-auth.json");
    write(&win_cursor, "cursor-win");
    write(&win_xai, "xai-win");
    let sandbox_xai = home.home().join(".shunt").join("xai-auth.json");
    write(&sandbox_xai, "xai-home");
    let record = adopted(&home);

    let result = move_standalone_stores_in(
        &record,
        GatewaySilent::for_test(),
        [(
            OsString::from("USERPROFILE"),
            winhome.as_os_str().to_os_string(),
        )],
    )
    .expect("move");

    let dst = home.home().join(".clauth").join("shunt");
    assert_eq!(
        result,
        StoreMove {
            moved: vec![MovedFile {
                from: win_cursor.clone(),
                to: dst.join("cursor-auth.json"),
            }],
            kept: vec![no_home("SHUNT_XAI_AUTH_FILE")],
        }
    );
    assert_eq!(
        fs::read_to_string(dst.join("cursor-auth.json")).expect("moved"),
        "cursor-win",
        "cursor fell back to USERPROFILE"
    );
    assert_eq!(
        fs::read_to_string(&win_xai).expect("untouched"),
        "xai-win",
        "xai never reads USERPROFILE"
    );
    assert_eq!(
        fs::read_to_string(&sandbox_xai).expect("untouched"),
        "xai-home",
        "xai's default under clauth's own home was not read"
    );
}

/// A family whose shunt site finds no home at all — `HOME` empty (unset) and
/// no non-empty `USERPROFILE` — is listed, not refused: every store with a
/// default but no determinable home appears in the plan, and nothing moves.
#[test]
fn a_store_with_no_home_is_listed_not_refused() {
    let home = HomeSandbox::new();
    let main = under(
        &home.home().join(".shunt"),
        &["accounts", "claude", "main.json"],
    );
    write(&main, "claude-main");
    let record = adopted(&home);

    let result = move_standalone_stores_in(
        &record,
        GatewaySilent::for_test(),
        [(OsString::from("HOME"), OsString::new())],
    )
    .expect("no home lists the stores, never refuses");

    assert_eq!(result.moved, Vec::<MovedFile>::new());
    assert_eq!(
        result.kept,
        vec![
            no_home("SHUNT_CLAUDE_ACCOUNTS_DIR"),
            no_home("SHUNT_CODEX_ACCOUNTS_DIR"),
            no_home("SHUNT_KIMI_ACCOUNTS_DIR"),
            no_home("SHUNT_ANTIGRAVITY_ACCOUNTS_DIR"),
            no_home("SHUNT_XAI_AUTH_FILE"),
            no_home("SHUNT_CURSOR_AUTH_FILE"),
            no_home("SHUNT_ANTIGRAVITY_AUTH_FILE"),
        ]
    );
    assert_eq!(fs::read_to_string(&main).expect("kept"), "claude-main");
    assert!(!home.home().join(".clauth").join("shunt").exists());
}

/// A store whose default home clauth cannot determine — no `HOME`, no
/// `USERPROFILE` — is listed with its fix, never moved from a guessed path:
/// the env-file-named claude account moves, the default xai file under
/// clauth's own `~/.shunt` stays untouched, and every other default store is
/// listed.
#[test]
fn a_store_with_no_determinable_home_is_listed_not_moved() {
    let home = HomeSandbox::new();
    let claude = home.home().join("claude-store");
    let claude_main = claude.join("main.json");
    write(&claude_main, "claude-main");
    let sandbox_xai = home.home().join(".shunt").join("xai-auth.json");
    write(&sandbox_xai, "xai-home");
    let record = with_env_file(
        &home,
        &format!("SHUNT_CLAUDE_ACCOUNTS_DIR={}\n", quoted(&claude)),
    );

    let dst = home.home().join(".clauth").join("shunt");
    let result = move_standalone_stores_in(&record, GatewaySilent::for_test(), std::iter::empty())
        .expect("move");

    assert_eq!(
        result,
        StoreMove {
            moved: vec![MovedFile {
                from: claude_main.clone(),
                to: dst.join("accounts").join("claude").join("main.json"),
            }],
            kept: vec![
                no_home("SHUNT_CODEX_ACCOUNTS_DIR"),
                no_home("SHUNT_KIMI_ACCOUNTS_DIR"),
                no_home("SHUNT_ANTIGRAVITY_ACCOUNTS_DIR"),
                no_home("SHUNT_XAI_AUTH_FILE"),
                no_home("SHUNT_CURSOR_AUTH_FILE"),
                no_home("SHUNT_ANTIGRAVITY_AUTH_FILE"),
            ],
        }
    );
    assert_eq!(
        fs::read_to_string(dst.join("accounts").join("claude").join("main.json")).expect("moved"),
        "claude-main"
    );
    assert!(!claude_main.exists(), "left its source");
    assert_eq!(
        fs::read_to_string(&sandbox_xai).expect("untouched"),
        "xai-home",
        "xai's default under clauth's own home was not read"
    );
}

/// A home the env file names as a relative path still refuses, since the
/// standalone resolved it against a working directory clauth cannot know.
#[test]
fn a_relative_home_still_refuses_the_move() {
    let home = HomeSandbox::new();
    let record = with_env_file(&home, "HOME=rel-home\n");

    let result = move_stores(&record);

    assert_eq!(
        result.map(|m| m.moved).map_err(|e| e.to_string()),
        Err(
            "the standalone's home names no absolute directory, so shunt's default for SHUNT_CLAUDE_ACCOUNTS_DIR is relative to the standalone's working directory, which clauth cannot tell; nothing was moved; set HOME to an absolute path in the env file, then run the move again"
                .to_string()
        )
    );
    assert!(!home.home().join(".clauth").join("shunt").exists());
}

/// Two store keys naming one file move it once: the duplicate is known at
/// plan time, so the second store is listed, never copied, and the move
/// succeeds.
#[test]
fn two_store_keys_naming_one_file_move_it_once_and_list_the_second() {
    let home = HomeSandbox::new();
    let shared = home.home().join("shared.json");
    write(&shared, "shared");
    let record = with_env_file(
        &home,
        &format!(
            "SHUNT_XAI_AUTH_FILE={}\nSHUNT_CURSOR_AUTH_FILE={}\n",
            quoted(&shared),
            quoted(&shared)
        ),
    );

    let dst = home.home().join(".clauth").join("shunt");
    assert_eq!(
        move_stores(&record).expect("the duplicate is listed, never a refusal"),
        StoreMove {
            moved: vec![MovedFile {
                from: shared.clone(),
                to: dst.join("xai-auth.json"),
            }],
            kept: vec![KeptFile {
                path: shared.clone(),
                reason: KeptReason::DuplicateSource,
            }],
        }
    );
    assert_eq!(
        fs::read_to_string(dst.join("xai-auth.json")).expect("the first store's copy"),
        "shared"
    );
    assert!(!shared.exists(), "the source was removed once");
}

/// `CODEX_AUTH_FILE` in the env file names the standalone's own codex file,
/// which moves in beside the other stores. A path that is another owner's
/// login stays where it is and the move names it with the reason, decided
/// by canonical path and never by the file's bytes: the codex CLI's own
/// `~/.codex/auth.json` (spelled through a link, or itself a clauth codex
/// profile's link), or anything under `~/.clauth`.
#[test]
fn a_named_codex_file_moves_in_unless_it_is_another_owners_login() {
    fn codex_login(home: &Path) -> PathBuf {
        home.join(".codex").join("auth.json")
    }
    fn profile_login(home: &Path) -> PathBuf {
        home.join(".clauth")
            .join("codex")
            .join("work")
            .join("auth.json")
    }
    fn named_default(home: &Path) -> PathBuf {
        write(&codex_login(home), "codex-cli-login");
        codex_login(home)
    }
    fn named_profile(home: &Path) -> PathBuf {
        write(&profile_login(home), "clauth-profile-login");
        profile_login(home)
    }
    #[cfg(unix)]
    fn named_link_onto_the_default(home: &Path) -> PathBuf {
        write(&codex_login(home), "codex-cli-login");
        let link = home.join("links").join("auth.json");
        fs::create_dir_all(link.parent().expect("links")).expect("links");
        std::os::unix::fs::symlink(codex_login(home), &link).expect("symlink");
        link
    }
    #[cfg(unix)]
    fn named_default_linked_onto_a_profile(home: &Path) -> PathBuf {
        write(&profile_login(home), "clauth-profile-login");
        fs::create_dir_all(home.join(".codex")).expect(".codex");
        std::os::unix::fs::symlink(profile_login(home), codex_login(home)).expect("symlink");
        codex_login(home)
    }

    {
        let home = HomeSandbox::new();
        let own = home.home().join("standalone").join("codex-auth.json");
        write(&own, "standalone-codex");
        let record = with_env_file(&home, &format!("CODEX_AUTH_FILE={}\n", quoted(&own)));
        let to = home
            .home()
            .join(".clauth")
            .join("shunt")
            .join("codex-auth.json");

        assert_eq!(
            move_stores(&record).map_err(|e| format!("{e:#}")),
            Ok(StoreMove {
                moved: vec![MovedFile {
                    from: own.clone(),
                    to: to.clone(),
                }],
                kept: Vec::new(),
            })
        );
        assert_eq!(fs::read_to_string(&to).expect("moved"), "standalone-codex");
        assert!(!own.exists(), "left its source");
    }

    type Named = fn(&Path) -> PathBuf;
    #[cfg_attr(not(unix), expect(unused_mut, reason = "the unix-only legs"))]
    let mut legs: Vec<(Named, KeptReason)> = vec![
        (named_default, KeptReason::CodexLogin),
        (named_profile, KeptReason::ClauthOwned),
    ];
    #[cfg(unix)]
    legs.extend([
        (named_link_onto_the_default as Named, KeptReason::CodexLogin),
        (named_default_linked_onto_a_profile, KeptReason::CodexLogin),
    ]);
    for (named, reason) in legs {
        let home = HomeSandbox::new();
        let path = named(home.home());
        let login = fs::read_to_string(&path).expect("the login");
        let record = with_env_file(&home, &format!("CODEX_AUTH_FILE={}\n", quoted(&path)));

        assert_eq!(
            move_stores(&record).map_err(|e| format!("{e:#}")),
            Ok(StoreMove {
                moved: Vec::new(),
                kept: vec![KeptFile {
                    path: path.clone(),
                    reason,
                }],
            }),
            "{path:?}"
        );
        assert_eq!(fs::read_to_string(&path).expect("still there"), login);
        assert!(
            !home
                .home()
                .join(".clauth")
                .join("shunt")
                .join("codex-auth.json")
                .exists()
        );
    }
}

/// A hard link onto the codex CLI's login is another owner's login even though
/// its canonical path differs: the link count catches it on every platform.
#[test]
fn a_hard_link_onto_the_codex_login_named_by_codex_auth_file_stays() {
    let home = HomeSandbox::new();
    let login = home.home().join(".codex").join("auth.json");
    write(&login, "codex-cli-login");
    let hard = home.home().join("hard").join("auth.json");
    fs::create_dir_all(hard.parent().expect("hard")).expect("hard");
    std::fs::hard_link(&login, &hard).expect("hard link");
    let record = with_env_file(&home, &format!("CODEX_AUTH_FILE={}\n", quoted(&hard)));

    assert_eq!(
        move_stores(&record).map_err(|e| format!("{e:#}")),
        Ok(StoreMove {
            moved: Vec::new(),
            kept: vec![KeptFile {
                path: hard.clone(),
                reason: KeptReason::HardLink,
            }],
        })
    );
    assert_eq!(
        fs::read_to_string(&login).expect("login"),
        "codex-cli-login"
    );
    assert_eq!(fs::read_to_string(&hard).expect("hard"), "codex-cli-login");
    assert!(!home.home().join(".clauth").join("shunt").exists());
}

/// An unreadable link count keeps the file rather than moving it: the guard
/// fails closed, with a reason naming that the count could not be read.
#[test]
fn an_unreadable_link_count_keeps_the_file() {
    let home = HomeSandbox::new();
    let own = home.home().join("standalone").join("codex-auth.json");
    write(&own, "standalone-codex");
    let record = with_env_file(&home, &format!("CODEX_AUTH_FILE={}\n", quoted(&own)));

    let _forced = UnreadableLinkCount::set();
    assert_eq!(
        move_stores(&record).map_err(|e| format!("{e:#}")),
        Ok(StoreMove {
            moved: Vec::new(),
            kept: vec![KeptFile {
                path: own.clone(),
                reason: KeptReason::LinkCountUnreadable,
            }],
        })
    );
    assert_eq!(fs::read_to_string(&own).expect("kept"), "standalone-codex");
    assert!(!home.home().join(".clauth").join("shunt").exists());
}

/// A dir store file hard-linked onto the codex CLI's own login is kept even
/// inside a dir store: the per-file owner check runs there too, and both of
/// the login's names stay byte-identical.
#[test]
fn a_dir_store_file_hard_linked_onto_the_codex_login_stays() {
    let home = HomeSandbox::new();
    let src = home.home().join(".shunt");
    let dst = home.home().join(".clauth").join("shunt");
    let login = home.home().join(".codex").join("auth.json");
    write(&login, "codex-cli-login");
    let me = under(&src, &["accounts", "codex", "me.json"]);
    fs::create_dir_all(me.parent().expect("codex dir")).expect("codex dir");
    std::fs::hard_link(&login, &me).expect("hard link");
    let a = under(&src, &["accounts", "codex", "a.json"]);
    write(&a, "codex-a");

    assert_eq!(
        move_stores(&adopted(&home)).expect("move"),
        StoreMove {
            moved: vec![MovedFile {
                from: a.clone(),
                to: under(&dst, &["accounts", "codex", "a.json"]),
            }],
            kept: vec![KeptFile {
                path: me.clone(),
                reason: KeptReason::HardLink,
            }],
        }
    );
    assert_eq!(fs::read_to_string(&me).expect("kept"), "codex-cli-login");
    assert_eq!(fs::read_to_string(&login).expect("kept"), "codex-cli-login");
    assert_eq!(
        fs::read_to_string(under(&dst, &["accounts", "codex", "a.json"])).expect("moved"),
        "codex-a"
    );
    assert!(!a.exists(), "a.json left its source");
}

/// A dir store whose path is the codex CLI's own home stays whole: none of
/// its files (the login, config, history) move.
#[test]
fn a_dir_store_at_the_codex_home_stays_whole() {
    let home = HomeSandbox::new();
    let codex_home = home.home().join(".codex");
    write(&codex_home.join("auth.json"), "codex-cli-login");
    write(&codex_home.join("config.toml"), "codex-config");
    let record = with_env_file(
        &home,
        &format!("SHUNT_CODEX_ACCOUNTS_DIR={}\n", quoted(&codex_home)),
    );

    assert_eq!(
        move_stores(&record).map_err(|e| format!("{e:#}")),
        Ok(StoreMove {
            moved: Vec::new(),
            kept: vec![KeptFile {
                path: codex_home.clone(),
                reason: KeptReason::CodexLogin,
            }],
        })
    );
    assert_eq!(
        fs::read_to_string(codex_home.join("auth.json")).expect("kept"),
        "codex-cli-login"
    );
    assert_eq!(
        fs::read_to_string(codex_home.join("config.toml")).expect("kept"),
        "codex-config"
    );
    assert!(!home.home().join(".clauth").join("shunt").exists());
}

/// `CODEX_AUTH_FILE=$CODEX_HOME/auth.json` stays, with `CODEX_HOME` from the
/// env file and then from the inherited env.
#[test]
fn a_codex_file_at_a_custom_codex_home_stays() {
    let home = HomeSandbox::new();
    let custom = home.home().join("custom-codex");
    let login = custom.join("auth.json");
    write(&login, "codex-cli-login");

    let record = with_env_file(
        &home,
        &format!(
            "CODEX_HOME={}\nCODEX_AUTH_FILE={}\n",
            quoted(&custom),
            quoted(&login)
        ),
    );
    assert_eq!(
        move_stores(&record).map_err(|e| format!("{e:#}")),
        Ok(StoreMove {
            moved: Vec::new(),
            kept: vec![KeptFile {
                path: login.clone(),
                reason: KeptReason::CodexLogin,
            }],
        })
    );

    let record = with_env_file(&home, &format!("CODEX_AUTH_FILE={}\n", quoted(&login)));
    assert_eq!(
        move_standalone_stores_in(
            &record,
            GatewaySilent::for_test(),
            [
                (
                    OsString::from("CODEX_HOME"),
                    custom.as_os_str().to_os_string()
                ),
                (
                    OsString::from("HOME"),
                    home.home().as_os_str().to_os_string()
                ),
            ],
        )
        .map_err(|e| format!("{e:#}")),
        Ok(StoreMove {
            moved: Vec::new(),
            kept: vec![KeptFile {
                path: login.clone(),
                reason: KeptReason::CodexLogin,
            }],
        })
    );
    assert_eq!(fs::read_to_string(&login).expect("kept"), "codex-cli-login");
    assert!(!home.home().join(".clauth").join("shunt").exists());
}

/// Every absolute `CODEX_HOME` from both sources is guarded at once: with the
/// env file's `CODEX_HOME=<a>` and the inherited `CODEX_HOME=<b>`, the login
/// `CODEX_AUTH_FILE` names at either home stays, neither source outranking
/// the other.
#[test]
fn both_codex_home_sources_are_guarded_at_once() {
    for named in ["a", "b"] {
        let home = HomeSandbox::new();
        let a = home.home().join("a");
        let b = home.home().join("b");
        write(&a.join("auth.json"), "codex-cli-login-a");
        write(&b.join("auth.json"), "codex-cli-login-b");
        let login = home.home().join(named).join("auth.json");
        let record = with_env_file(
            &home,
            &format!(
                "CODEX_HOME={}\nCODEX_AUTH_FILE={}\n",
                quoted(&a),
                quoted(&login)
            ),
        );

        assert_eq!(
            move_standalone_stores_in(
                &record,
                GatewaySilent::for_test(),
                [
                    (OsString::from("CODEX_HOME"), b.as_os_str().to_os_string()),
                    (
                        OsString::from("HOME"),
                        home.home().as_os_str().to_os_string()
                    ),
                ],
            )
            .map_err(|e| format!("{e:#}")),
            Ok(StoreMove {
                moved: Vec::new(),
                kept: vec![KeptFile {
                    path: login.clone(),
                    reason: KeptReason::CodexLogin,
                }],
            }),
            "CODEX_AUTH_FILE at {named}"
        );
        assert_eq!(
            fs::read_to_string(a.join("auth.json")).expect("kept"),
            "codex-cli-login-a"
        );
        assert_eq!(
            fs::read_to_string(b.join("auth.json")).expect("kept"),
            "codex-cli-login-b"
        );
        assert!(!home.home().join(".clauth").join("shunt").exists());
    }
}

/// A dir store pointed at either `CODEX_HOME` stays whole while both sources
/// set one (the env file's `<a>`, the inherited `<b>`): neither its login nor
/// any other account-shaped file in it moves.
#[test]
fn a_dir_store_at_either_codex_home_stays_whole() {
    for store in ["a", "b"] {
        let home = HomeSandbox::new();
        let a = home.home().join("a");
        let b = home.home().join("b");
        let dir = home.home().join(store);
        write(&dir.join("auth.json"), "codex-cli-login");
        write(&dir.join("x.json"), "codex-x");
        let record = with_env_file(
            &home,
            &format!(
                "CODEX_HOME={}\nSHUNT_CODEX_ACCOUNTS_DIR={}\n",
                quoted(&a),
                quoted(&dir)
            ),
        );

        assert_eq!(
            move_standalone_stores_in(
                &record,
                GatewaySilent::for_test(),
                [
                    (OsString::from("CODEX_HOME"), b.as_os_str().to_os_string()),
                    (
                        OsString::from("HOME"),
                        home.home().as_os_str().to_os_string()
                    ),
                ],
            )
            .map_err(|e| format!("{e:#}")),
            Ok(StoreMove {
                moved: Vec::new(),
                kept: vec![KeptFile {
                    path: dir.clone(),
                    reason: KeptReason::CodexLogin,
                }],
            }),
            "the dir store at {store}"
        );
        assert_eq!(
            fs::read_to_string(dir.join("auth.json")).expect("kept"),
            "codex-cli-login"
        );
        assert_eq!(
            fs::read_to_string(dir.join("x.json")).expect("kept"),
            "codex-x"
        );
        assert!(!home.home().join(".clauth").join("shunt").exists());
    }
}

/// Every inherited entry named `CODEX_HOME` is guarded, not only the last: an
/// env block can hold the name twice, and which one the codex CLI reads is its
/// own env lookup's choice.
#[test]
fn every_inherited_codex_home_entry_is_guarded() {
    let home = HomeSandbox::new();
    let c = home.home().join("c");
    let d = home.home().join("d");
    write(&c.join("auth.json"), "codex-cli-login-c");
    write(&d.join("auth.json"), "codex-cli-login-d");
    let record = with_env_file(
        &home,
        &format!(
            "CODEX_AUTH_FILE={}\nSHUNT_XAI_AUTH_FILE={}\n",
            quoted(&c.join("auth.json")),
            quoted(&d.join("auth.json"))
        ),
    );

    assert_eq!(
        move_standalone_stores_in(
            &record,
            GatewaySilent::for_test(),
            [
                (OsString::from("CODEX_HOME"), c.as_os_str().to_os_string()),
                (OsString::from("CODEX_HOME"), d.as_os_str().to_os_string()),
                (
                    OsString::from("HOME"),
                    home.home().as_os_str().to_os_string()
                ),
            ],
        )
        .map_err(|e| format!("{e:#}")),
        Ok(StoreMove {
            moved: Vec::new(),
            kept: vec![
                KeptFile {
                    path: d.join("auth.json"),
                    reason: KeptReason::CodexLogin,
                },
                KeptFile {
                    path: c.join("auth.json"),
                    reason: KeptReason::CodexLogin,
                },
            ],
        })
    );
    assert_eq!(
        fs::read_to_string(c.join("auth.json")).expect("kept"),
        "codex-cli-login-c"
    );
    assert_eq!(
        fs::read_to_string(d.join("auth.json")).expect("kept"),
        "codex-cli-login-d"
    );
    assert!(!home.home().join(".clauth").join("shunt").exists());
}

/// A relative `CODEX_HOME` from either source refuses the whole move before
/// anything moves, naming the key and never the value; a whitespace-only
/// value is a relative path, so it refuses too (only an empty one is unset,
/// see [`an_empty_codex_home_is_unset`]).
#[test]
fn a_relative_codex_home_refuses_the_move_naming_the_key() {
    {
        let home = HomeSandbox::new();
        let record = with_env_file(&home, "CODEX_HOME=rel-codex\n");
        let err = move_stores(&record).expect_err("a relative CODEX_HOME in the env file");
        assert_eq!(
            err.to_string(),
            "the env file sets CODEX_HOME to a relative path, and clauth cannot tell which codex login it names; nothing was moved; set CODEX_HOME to an absolute path in the env file, then run the move again"
        );
        assert!(!home.home().join(".clauth").join("shunt").exists());
    }
    {
        let home = HomeSandbox::new();
        let record = adopted(&home);
        let err = move_standalone_stores_in(
            &record,
            GatewaySilent::for_test(),
            [(OsString::from("CODEX_HOME"), OsString::from("rel-codex"))],
        )
        .expect_err("a relative CODEX_HOME in the inherited env");
        assert_eq!(
            err.to_string(),
            "CODEX_HOME in clauth's own environment is a relative path, and clauth cannot tell which codex login it names; nothing was moved; set CODEX_HOME to an absolute path, then run the move again"
        );
        assert!(!home.home().join(".clauth").join("shunt").exists());
    }
    {
        let home = HomeSandbox::new();
        let record = with_env_file(&home, "CODEX_HOME=' '\n");
        let err = move_stores(&record).expect_err("a whitespace CODEX_HOME in the env file");
        assert_eq!(
            err.to_string(),
            "the env file sets CODEX_HOME to a relative path, and clauth cannot tell which codex login it names; nothing was moved; set CODEX_HOME to an absolute path in the env file, then run the move again"
        );
        assert!(!home.home().join(".clauth").join("shunt").exists());
    }
    {
        let home = HomeSandbox::new();
        let record = adopted(&home);
        let err = move_standalone_stores_in(
            &record,
            GatewaySilent::for_test(),
            [(OsString::from("CODEX_HOME"), OsString::from(" "))],
        )
        .expect_err("a whitespace CODEX_HOME in the inherited env");
        assert_eq!(
            err.to_string(),
            "CODEX_HOME in clauth's own environment is a relative path, and clauth cannot tell which codex login it names; nothing was moved; set CODEX_HOME to an absolute path, then run the move again"
        );
        assert!(!home.home().join(".clauth").join("shunt").exists());
    }
}

/// Only an empty `CODEX_HOME` is unset (the codex CLI's rule): the move
/// proceeds and the default `~/.codex` login stays guarded.
#[test]
fn an_empty_codex_home_is_unset() {
    let home = HomeSandbox::new();
    let codex_login = home.home().join(".codex").join("auth.json");
    write(&codex_login, "codex-cli-login");
    let record = with_env_file(
        &home,
        &format!("CODEX_HOME=\nCODEX_AUTH_FILE={}\n", quoted(&codex_login)),
    );
    assert_eq!(
        move_stores(&record).map_err(|e| format!("{e:#}")),
        Ok(StoreMove {
            moved: Vec::new(),
            kept: vec![KeptFile {
                path: codex_login.clone(),
                reason: KeptReason::CodexLogin,
            }],
        })
    );
    assert_eq!(
        fs::read_to_string(&codex_login).expect("kept"),
        "codex-cli-login"
    );
}

/// The move's silent proof must name the record's probe address: a proof
/// minted elsewhere refuses, naming both and the fix.
#[test]
fn the_move_refuses_a_silent_proof_from_another_address() {
    let home = HomeSandbox::new();
    let record = adopted(&home);
    let other: SocketAddr = "127.0.0.1:3999".parse().expect("addr");

    let err = move_standalone_stores_in(
        &record,
        GatewaySilent::for_test_at(other),
        std::iter::empty(),
    )
    .expect_err("a proof from another address");
    assert_eq!(
        err.to_string(),
        "the silent proof was minted at 127.0.0.1:3999, not the gateway's probe address 127.0.0.1:3001; probe 127.0.0.1:3001 and run the move again"
    );
    match err.downcast_ref::<StoreMoveRefusal>() {
        Some(StoreMoveRefusal::SilentMismatch { silent, probe }) => {
            assert_eq!(*silent, other);
            assert_eq!(*probe, addr("127.0.0.1:3001"));
        }
        other => panic!("a SilentMismatch refusal, got {other:?}"),
    }
    assert!(!home.home().join(".clauth").join("shunt").exists());
}
