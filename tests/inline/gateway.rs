#![allow(clippy::unwrap_used, clippy::expect_used)]
//! The gateway engine: the record and token file, discovery in shunt's order,
//! bind resolution, the env file, the version floor and `/health`, the admin
//! edit behind its `shunt check` gate, and the store move. Every test holds a
//! `HomeSandbox`; the gate tests drive a stub `shunt` written into it, a
//! `/bin/sh` script, so they are `#[cfg(unix)]`.

use std::fs;
use std::net::{SocketAddr, TcpListener};

use super::*;
use crate::testutil::{
    EnvPin, HomeSandbox, no_shunt_env, request_header, request_path, serve_endpoints,
    serve_endpoints_raw,
};

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
            "the shunt config must be an absolute path, got shunt.toml",
        ),
        (
            "/etc/shunt.yaml",
            "the shunt config must be TOML, and /etc/shunt.yaml is YAML",
        ),
        (
            "/etc/Shunt.YML",
            "the shunt config must be TOML, and /etc/Shunt.YML is YAML",
        ),
    ] {
        let err = GatewayRecord::new(PathBuf::from(config)).expect_err(config);
        assert_eq!(err.to_string(), message, "{config}");
    }
}

/// An adopt refuses a config the gateway could never run: on top of the
/// record's own checks, one that does not parse as TOML, which a bind from
/// the env would otherwise let through. A config that parses is adoptable.
#[test]
fn an_adopt_refuses_a_config_that_does_not_parse() {
    let home = HomeSandbox::new();
    let config = home.home().join("shunt.toml");
    write(&config, "[server]\nbind = \"127.0.0.1:4200\"\n");
    assert_eq!(
        adoptable_record(config.clone())
            .expect("adoptable")
            .config(),
        std::fs::canonicalize(&config).expect("canonical")
    );

    write(&config, "a = 1\n[server\n");
    let err = adoptable_record(config.clone()).expect_err("unparsed");
    assert_eq!(
        err.downcast_ref::<ConfigUnparsed>(),
        Some(&ConfigUnparsed(
            "the shunt config does not parse as TOML (line 2)".to_string()
        ))
    );
    assert_eq!(
        err.to_string(),
        "the shunt config does not parse as TOML (line 2)"
    );

    let yaml = home.home().join("shunt.yaml");
    write(&yaml, "server:\n  bind: 127.0.0.1:4200\n");
    assert!(
        adoptable_record(yaml)
            .expect_err("yaml")
            .downcast_ref::<NotToml>()
            .is_some(),
        "the record's own checks still run first"
    );
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
            "the shunt config must be an absolute path, got shunt.toml",
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
    assert_eq!(format!("{token:?}"), "SecretToken(<redacted>)");
    let client = mint_token().expect("mint");
    assert_eq!(
        format!("{client:?}"),
        "SecretToken(<redacted>)",
        "a client token is the same redacted type"
    );
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

/// A profile's `base_url` names the gateway when its port (written or the
/// scheme's default) is the bind's and its host is the probe address, or
/// `localhost` over a loopback probe; nothing else counts.
#[test]
fn a_base_url_names_the_gateway_by_its_probe_address_and_port() {
    let _home = HomeSandbox::new();
    let bind = |configured: &str, probe: &str| GatewayBind {
        configured: configured.parse().expect("configured"),
        probe: probe.parse().expect("probe"),
    };
    let loopback = bind("127.0.0.1:3067", "127.0.0.1:3067");
    let wildcard = bind("0.0.0.0:3067", "127.0.0.1:3067");
    let v6 = bind("[::1]:3067", "[::1]:3067");
    let lan = bind("192.168.1.5:3067", "192.168.1.5:3067");
    let port_80 = bind("127.0.0.1:80", "127.0.0.1:80");
    let port_443 = bind("127.0.0.1:443", "127.0.0.1:443");
    let mapped = bind("[::ffff:127.0.0.1]:3067", "[::ffff:127.0.0.1]:3067");
    for (url, bind, names) in [
        ("http://127.0.0.1:3067", &loopback, true),
        ("http://127.0.0.1:3067/", &loopback, true),
        ("http://127.0.0.1:3067/v1/messages", &loopback, true),
        ("HTTP://LOCALHOST:3067", &loopback, true),
        ("https://127.0.0.1:3067", &loopback, true),
        ("http://[::ffff:127.0.0.1]:3067", &loopback, true),
        ("http://127.0.0.1:3068", &loopback, false),
        ("http://127.0.0.2:3067", &loopback, false),
        ("http://[::1]:3067", &loopback, false),
        ("http://example.com:3067", &loopback, false),
        ("127.0.0.1:3067", &loopback, false),
        ("ftp://127.0.0.1:3067", &loopback, false),
        ("not a url", &loopback, false),
        ("http://127.0.0.1:3067", &wildcard, true),
        ("http://localhost:3067", &wildcard, true),
        ("http://192.168.1.5:3067", &wildcard, false),
        ("http://[::1]:3067", &v6, true),
        ("http://localhost:3067", &v6, true),
        ("http://127.0.0.1:3067", &v6, false),
        ("http://192.168.1.5:3067", &lan, true),
        ("http://localhost:3067", &lan, false),
        ("http://127.0.0.1", &port_80, true),
        ("http://127.0.0.1:/x", &port_80, true),
        ("https://127.0.0.1", &port_443, true),
        ("http://127.0.0.1", &loopback, false),
        ("http://0.0.0.0:3067", &loopback, true),
        ("http://0.0.0.0:3067", &wildcard, true),
        ("http://0.0.0.0:3067", &v6, false),
        ("http://[::]:3067", &v6, true),
        ("http://[::]:3067", &loopback, false),
        ("http://127.0.0.1:3067", &mapped, true),
    ] {
        assert_eq!(url_names_bind(url, bind), names, "{url:?} against {bind:?}");
    }
}

/// The displayed bind honours a `SHUNT_SERVER__BIND` the running daemon
/// recorded, matched to the singleton holder by pid; with no daemon, or a
/// record naming another pid, the adopted files alone decide.
#[test]
fn the_displayed_bind_reads_the_daemon_recorded_bind_matched_by_pid() {
    let home = HomeSandbox::new();
    let config = home.home().join("etc").join("shunt.toml");
    fs::create_dir_all(config.parent().unwrap()).unwrap();
    fs::write(&config, "[server]\nbind = \"127.0.0.1:3067\"\n").unwrap();
    let record = GatewayRecord::new(config).unwrap();
    let configured = |record: &GatewayRecord| {
        displayed_gateway_bind(record)
            .expect("the bind resolves")
            .configured
    };
    let files: SocketAddr = "127.0.0.1:3067".parse().unwrap();
    assert_eq!(configured(&record), files, "no daemon: the files");

    let held = crate::daemon::hold_daemon_lock();
    let pid_file = home.home().join(".clauth").join("clauthd.pid");
    fs::write(&pid_file, format!("{}\n", std::process::id())).unwrap();
    write_daemon_env_from(&env_pairs(&[(BIND_ENV, "192.168.1.5:3067")]))
        .expect("record the daemon env");
    assert_eq!(
        configured(&record),
        "192.168.1.5:3067".parse::<SocketAddr>().unwrap(),
        "the running daemon's recorded bind"
    );

    fs::write(&pid_file, format!("{}\n", std::process::id() + 1)).unwrap();
    assert_eq!(
        configured(&record),
        files,
        "a record naming another pid: the files"
    );

    // The pid stamp and the record outlive a daemon: with the lock free they
    // name no running daemon.
    fs::write(&pid_file, format!("{}\n", std::process::id())).unwrap();
    drop(held);
    assert_eq!(configured(&record), files, "no lock held: the files");
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

/// A process env as `(name, value)` pairs, for the daemon record's capture.
fn env_pairs(pairs: &[(&str, &str)]) -> Vec<(OsString, OsString)> {
    pairs
        .iter()
        .map(|(name, value)| (OsString::from(name), OsString::from(value)))
        .collect()
}

fn env_of(pairs: &[(&str, &str)], skipped: &[usize]) -> GatewayEnv {
    GatewayEnv {
        vars: pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), OsString::from(v)))
            .collect(),
        skipped: skipped.to_vec(),
        client_tokens: None,
        tokens_variable: None,
        unset: Vec::new(),
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

// ── client tokens in the spawn env ──────────────────────────────────────────

/// [`with_env_file`] with `config` as the config's text.
fn with_config(home: &HomeSandbox, config: &str, env_text: &str) -> GatewayRecord {
    let record = with_env_file(home, env_text);
    write(record.config(), config);
    record
}

fn spawn_env(
    record: &GatewayRecord,
    inherited: &[(&str, &str)],
) -> std::result::Result<GatewayEnv, String> {
    let pairs = inherited
        .iter()
        .map(|(k, v)| (OsString::from(k), OsString::from(v)))
        .collect();
    gateway_env_in(record, || Ok(TokensInherited::Process(pairs))).map_err(|e| format!("{e:#}"))
}

fn add_token(record: &GatewayRecord, profile: &str) -> String {
    crate::gateway_tokens::add_client_token(record, profile)
        .expect("add a client token")
        .expose()
        .to_string()
}

/// The env file's own pairs come first, then the store's in name order,
/// joined with `,`; with no store the env file's value passes through
/// untouched; and the env carries the digest of the store bytes it read.
#[test]
fn the_spawn_env_joins_the_env_files_own_pairs_then_the_store() {
    use sha2::{Digest as _, Sha256};
    let home = HomeSandbox::new();
    let _env = no_shunt_env(&home);
    let record = with_config(
        &home,
        "[server]\n",
        "SHUNT_CLIENT_TOKENS=alice:a1\nOTHER=x\n",
    );
    let env = spawn_env(&record, &[]).expect("env");
    assert_eq!(env.get("SHUNT_CLIENT_TOKENS"), Some(OsStr::new("alice:a1")));
    assert_eq!(env.client_tokens_digest(), None, "no store, no digest");

    let kerry = add_token(&record, "kerry");
    let env = spawn_env(&record, &[]).expect("env");
    assert_eq!(
        env.get("SHUNT_CLIENT_TOKENS"),
        Some(OsString::from(format!("alice:a1,kerry:{kerry}")).as_os_str())
    );
    let store = home
        .home()
        .join(".clauth")
        .join("gateway-client-tokens.toml");
    assert_eq!(
        env.client_tokens_digest(),
        Some(<[u8; 32]>::from(Sha256::digest(
            fs::read(&store).expect("store")
        ))),
        "the digest of the store bytes the env was built from"
    );

    let bob = add_token(&record, "bob");
    assert_eq!(
        spawn_env(&record, &[])
            .expect("env")
            .get("SHUNT_CLIENT_TOKENS"),
        Some(OsString::from(format!("alice:a1,bob:{bob},kerry:{kerry}")).as_os_str())
    );

    write(record.env_file.as_deref().expect("env file"), "OTHER=x\n");
    assert_eq!(
        spawn_env(&record, &[])
            .expect("env")
            .get("SHUNT_CLIENT_TOKENS"),
        Some(OsString::from(format!("bob:{bob},kerry:{kerry}")).as_os_str()),
        "no env-file pair: the store's alone"
    );
}

/// The variable is `SHUNT_SERVER__AUTH__TOKENS_ENV` from the spawn env in
/// any case (the env file's over the inherited env), as figment reads it,
/// else `[server.auth].tokens_env`, else shunt's `SHUNT_CLIENT_TOKENS`.
#[test]
fn the_tokens_variable_is_the_env_layer_then_the_configs_key_then_shunts_default() {
    let home = HomeSandbox::new();
    let _env = no_shunt_env(&home);
    let record = with_config(&home, "[server]\n", "");
    let kerry = add_token(&record, "kerry");
    let pair = OsString::from(format!("kerry:{kerry}"));
    let keyed = "[server.auth]\ntokens_env = \"CFG_TOKENS\"\n";
    for (config, env_text, inherited, variable) in [
        ("[server]\n", "", &[][..], "SHUNT_CLIENT_TOKENS"),
        (keyed, "", &[][..], "CFG_TOKENS"),
        (
            keyed,
            "shunt_server__auth__tokens_env=ENV_TOKENS\n",
            &[][..],
            "ENV_TOKENS",
        ),
        (
            keyed,
            "",
            &[("SHUNT_SERVER__AUTH__TOKENS_ENV", "\"QUOTED_TOKENS\"")][..],
            "QUOTED_TOKENS",
        ),
        (
            keyed,
            "SHUNT_SERVER__AUTH__TOKENS_ENV=FILE_TOKENS\n",
            &[("SHUNT_SERVER__AUTH__TOKENS_ENV", "INHERITED_TOKENS")][..],
            "FILE_TOKENS",
        ),
    ] {
        write(record.config(), config);
        write(record.env_file.as_deref().expect("env file"), env_text);
        let env = spawn_env(&record, inherited).expect("env");
        assert_eq!(
            env.get(variable),
            Some(pair.as_os_str()),
            "{config:?} {env_text:?}"
        );
        for other in ["SHUNT_CLIENT_TOKENS", "CFG_TOKENS"] {
            if other != variable {
                assert_eq!(env.get(other), None, "{other} beside {variable}");
            }
        }
    }
}

/// A tokens variable clauth cannot set refuses the spawn env, naming where it
/// came from and the fix; with no client token stored nothing reads it.
#[test]
fn a_tokens_variable_clauth_cannot_set_refuses_by_where_it_came_from() {
    let home = HomeSandbox::new();
    let _env = no_shunt_env(&home);
    let record = with_config(&home, "[server.auth]\ntokens_env = \"${TOK}\"\n", "");
    assert!(
        spawn_env(&record, &[]).is_ok(),
        "no client token stored: the variable is not read"
    );
    write(record.config(), "[server]\n");
    add_token(&record, "kerry");
    let not_a_name = |source: &str| {
        format!(
            "{source} is not a variable name clauth can set; set it to a plain variable name like SHUNT_CLIENT_TOKENS"
        )
    };
    for (config, env_text, message) in [
        (
            "[server.auth]\ntokens_env = \"${TOK}\"\n",
            "",
            "[server.auth].tokens_env is a ${...} reference, which clauth does not resolve; write the variable's name there by hand, or set SHUNT_SERVER__AUTH__TOKENS_ENV in the gateway's env file".to_string(),
        ),
        (
            "[server.auth]\ntokens_env = 7\n",
            "",
            not_a_name("[server.auth].tokens_env"),
        ),
        (
            "[server]\n",
            "SHUNT_SERVER__AUTH__TOKENS_ENV=\n",
            not_a_name("SHUNT_SERVER__AUTH__TOKENS_ENV"),
        ),
        (
            "[server]\n",
            "SHUNT_SERVER__AUTH__TOKENS_ENV='\"A\\B\"'\n",
            "SHUNT_SERVER__AUTH__TOKENS_ENV holds a backslash inside its quotes, which figment would unescape and clauth does not; write the value literally".to_string(),
        ),
    ] {
        write(record.config(), config);
        write(record.env_file.as_deref().expect("env file"), env_text);
        assert_eq!(
            spawn_env(&record, &[]).map(|_| ()),
            Err(message),
            "{config:?} {env_text:?}"
        );
    }
}

/// A store name the env file's own value also holds (the env file edited
/// after the add) fails the spawn env closed rather than hand shunt a
/// duplicate it refuses whole; the refusal names the names, never a value.
#[test]
fn a_store_name_the_env_file_also_holds_refuses_the_spawn_env_by_name() {
    let home = HomeSandbox::new();
    let _env = no_shunt_env(&home);
    let record = with_config(&home, "[server.auth]\n", "");
    add_token(&record, "kerry");
    let env_file = record.env_file.clone().expect("env file");
    write(
        &env_file,
        "SHUNT_CLIENT_TOKENS=kerry:k0-secret,bob:b0-secret\n",
    );
    assert_eq!(
        spawn_env(&record, &[]).map(|_| ()),
        Err(format!(
            "the gateway's env file {} and clauth's client-token store both name a client \"kerry\" in SHUNT_CLIENT_TOKENS, and shunt refuses a duplicate name; remove that entry from the env file, or remove clauth's token for that profile",
            env_file.display()
        ))
    );
    write(&env_file, "");
    add_token(&record, "bob");
    write(
        &env_file,
        "SHUNT_CLIENT_TOKENS=kerry:k0-secret,bob:b0-secret\n",
    );
    assert_eq!(
        spawn_env(&record, &[]).map(|_| ()),
        Err(format!(
            "the gateway's env file {} and clauth's client-token store both name clients \"bob\", \"kerry\" in SHUNT_CLIENT_TOKENS, and shunt refuses a duplicate name; remove those entries from the env file, or remove clauth's token for those profiles",
            env_file.display()
        ))
    );
}

/// The join's base is the value shunt reads without clauth: the env file's,
/// else the one the gateway inherits, the daemon's own env at the spawn.
#[test]
fn the_spawn_joins_onto_the_inherited_value_when_the_env_file_sets_none() {
    let home = HomeSandbox::new();
    let _env = no_shunt_env(&home);
    let record = with_config(&home, "[server]\n", "OTHER=x\n");
    let _pin = EnvPin::new(
        &home,
        &[("SHUNT_CLIENT_TOKENS", Some(OsStr::new("alice:a1")))],
    );
    let kerry = add_token(&record, "kerry");
    assert_eq!(
        gateway_env(&record)
            .expect("env")
            .get("SHUNT_CLIENT_TOKENS"),
        Some(OsString::from(format!("alice:a1,kerry:{kerry}")).as_os_str()),
        "the inherited pairs stay"
    );
    write(
        record.env_file.as_deref().expect("env file"),
        "SHUNT_CLIENT_TOKENS=bob:b1\n",
    );
    assert_eq!(
        gateway_env(&record)
            .expect("env")
            .get("SHUNT_CLIENT_TOKENS"),
        Some(OsString::from(format!("bob:b1,kerry:{kerry}")).as_os_str()),
        "the env file's value outranks the inherited one, as shunt reads it"
    );
}

/// With no daemon the TUI reads the inherited value from its own env, as
/// `start daemon` would hand it on: a name it holds refuses the add.
#[test]
fn with_no_daemon_an_add_refuses_a_name_the_inherited_value_holds() {
    let home = HomeSandbox::new();
    let _env = no_shunt_env(&home);
    let record = with_config(&home, "[server]\n", "");
    let _pin = EnvPin::new(
        &home,
        &[("SHUNT_CLIENT_TOKENS", Some(OsStr::new("alice:a1-secret")))],
    );
    assert_eq!(
        crate::gateway_tokens::add_client_token(&record, "alice")
            .map(|_| ())
            .map_err(|e| format!("{e:#}")),
        Err("cannot add a client token for profile \"alice\": the environment the gateway inherits already names a client \"alice\" in SHUNT_CLIENT_TOKENS, and shunt refuses a duplicate name; remove that entry from that environment first".to_string())
    );
}

/// Under a running daemon the TUI reads the daemon's recorded inherited
/// value, never its own env; a record written before the daemon recorded the
/// tokens variable refuses until the daemon restarts.
#[test]
fn under_a_daemon_the_tui_reads_the_daemons_recorded_inherited_value() {
    let home = HomeSandbox::new();
    let _env = no_shunt_env(&home);
    let record = with_config(&home, "[server]\n", "");
    save_record(&record);
    let _pin = EnvPin::new(&home, &[("SHUNT_CLIENT_TOKENS", None)]);
    let held = crate::daemon::hold_daemon_lock();
    let pid_file = home.home().join(".clauth").join("clauthd.pid");
    fs::write(&pid_file, format!("{}\n", std::process::id())).unwrap();
    write_daemon_env_from(&env_pairs(&[("SHUNT_CLIENT_TOKENS", "alice:a1-secret")]))
        .expect("record the daemon env");
    assert_eq!(
        crate::gateway_tokens::add_client_token(&record, "alice")
            .map(|_| ())
            .map_err(|e| format!("{e:#}")),
        Err("cannot add a client token for profile \"alice\": the environment the gateway inherits already names a client \"alice\" in SHUNT_CLIENT_TOKENS, and shunt refuses a duplicate name; remove that entry from that environment first".to_string())
    );

    let path = home.home().join(".clauth").join("gateway-env.json");
    let mut old: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    let fields = old.as_object_mut().expect("an object");
    fields.remove("auth_env");
    fields.remove("tokens");
    fs::write(&path, serde_json::to_vec(&old).unwrap()).unwrap();
    assert_eq!(
        crate::gateway_tokens::add_client_token(&record, "kerry")
            .map(|_| ())
            .map_err(|e| format!("{e:#}")),
        Err("cannot add a client token for profile \"kerry\": the running daemon recorded no value for SHUNT_CLIENT_TOKENS, the variable shunt reads its client tokens from; restart the daemon so it records it".to_string())
    );
    drop(held);
}

/// The unmanaged token names read by shunt's grammar from the variable the
/// spawn sets, never a value: entries trimmed, the name before the first
/// `:`, empty entries skipped.
#[test]
fn the_unmanaged_token_names_follow_shunts_grammar() {
    let home = HomeSandbox::new();
    let _env = no_shunt_env(&home);
    let _pin = EnvPin::new(&home, &[("SHUNT_CLIENT_TOKENS", None), ("CFG", None)]);
    let record = with_config(
        &home,
        "[server]\n",
        "SHUNT_CLIENT_TOKENS=\" alice : a1 ,bob:b:2,,\"\nCFG=carol:c3\n",
    );
    assert_eq!(
        unmanaged_token_names(&record).expect("names"),
        ["alice", "bob"]
    );
    write(record.config(), "[server.auth]\ntokens_env = \"CFG\"\n");
    assert_eq!(unmanaged_token_names(&record).expect("names"), ["carol"]);
    let mut bare = record.clone();
    bare.env_file = None;
    assert_eq!(
        unmanaged_token_names(&bare).expect("names"),
        Vec::<String>::new()
    );
}

/// A tokens value shunt's grammar refuses (shunt refuses the whole load on
/// it) is never read as holding no pair: every reader refuses, naming where
/// the value came from and never the value. Each outcome is collected so one
/// wrong reader does not hide another.
#[test]
fn a_tokens_value_shunt_cannot_parse_is_never_read_as_holding_no_pair() {
    let home = HomeSandbox::new();
    let _env = no_shunt_env(&home);
    let _pin = EnvPin::new(&home, &[("SHUNT_CLIENT_TOKENS", None)]);
    let record = with_config(&home, "[server.auth]\n", "");
    let env_file = record.env_file.clone().expect("env file");
    let unparsed = format!(
        "the gateway's env file {} holds a SHUNT_CLIENT_TOKENS value shunt cannot parse, and shunt refuses to start on it; fix that value first",
        env_file.display()
    );
    let fmt = |r: Result<()>| r.map_err(|e| format!("{e:#}"));
    let mut got = Vec::new();
    let mut want = Vec::new();
    for bad in ["broken-secret", "a:1,a:2", ",,", "ci:"] {
        write(&env_file, &format!("SHUNT_CLIENT_TOKENS={bad}\n"));
        got.push((
            bad,
            "names",
            fmt(unmanaged_token_names(&record).map(|_| ())),
        ));
        want.push((bad, "names", Err(unparsed.clone())));
        got.push((
            bad,
            "add",
            fmt(crate::gateway_tokens::add_client_token(&record, "kerry").map(|_| ())),
        ));
        want.push((
            bad,
            "add",
            Err(format!(
                "cannot add a client token for profile \"kerry\": {unparsed}"
            )),
        ));
    }
    // A token stored before the env file turned unparseable: the spawn join
    // and the last token's remove refuse, and the open config's require too.
    write(&env_file, "");
    add_token(&record, "kerry");
    write(&env_file, "SHUNT_CLIENT_TOKENS=broken-secret\n");
    got.push((
        "broken-secret",
        "spawn",
        fmt(gateway_env(&record).map(|_| ())),
    ));
    want.push(("broken-secret", "spawn", Err(unparsed.clone())));
    got.push((
        "broken-secret",
        "remove",
        fmt(crate::gateway_tokens::remove_client_token(&record, "kerry").map(|_| ())),
    ));
    want.push((
        "broken-secret",
        "remove",
        Err(format!("cannot remove the last client token: {unparsed}")),
    ));
    write(record.config(), "[server]\n");
    let store = home
        .home()
        .join(".clauth")
        .join("gateway-client-tokens.toml");
    fs::remove_file(&store).expect("empty the store");
    got.push((
        "broken-secret",
        "require",
        fmt(crate::gateway_tokens::require_client_tokens(&record).map(|_| ())),
    ));
    want.push((
        "broken-secret",
        "require",
        Err(format!("cannot require client tokens: {unparsed}")),
    ));
    assert_eq!(got, want);
}

/// Under a running daemon whose inherited tokens value does not parse, the
/// TUI's add refuses on the record's state, naming the inherited env.
#[test]
fn under_a_daemon_an_unparseable_inherited_value_refuses_the_add() {
    let home = HomeSandbox::new();
    let _env = no_shunt_env(&home);
    let record = with_config(&home, "[server]\n", "");
    save_record(&record);
    let _pin = EnvPin::new(&home, &[("SHUNT_CLIENT_TOKENS", None)]);
    let held = crate::daemon::hold_daemon_lock();
    fs::write(
        home.home().join(".clauth").join("clauthd.pid"),
        format!("{}\n", std::process::id()),
    )
    .unwrap();
    write_daemon_env_from(&env_pairs(&[("SHUNT_CLIENT_TOKENS", "no-colon-secret")]))
        .expect("record the daemon env");
    assert_eq!(
        crate::gateway_tokens::add_client_token(&record, "kerry")
            .map(|_| ())
            .map_err(|e| format!("{e:#}")),
        Err("cannot add a client token for profile \"kerry\": the environment the gateway inherits holds a SHUNT_CLIENT_TOKENS value shunt cannot parse, and shunt refuses to start on it; fix that value first".to_string())
    );
    drop(held);
}

/// A daemon starting with no gateway adopted still records shunt's default
/// tokens variable (names only), so a gateway adopted while it runs reads it.
#[test]
fn a_daemon_with_no_adopted_gateway_records_shunts_default_variable() {
    let home = HomeSandbox::new();
    let _env = no_shunt_env(&home);
    let daemon_env = EnvPin::new(
        &home,
        &[("SHUNT_CLIENT_TOKENS", Some(OsStr::new("ci:abc")))],
    );
    let held = daemon_record_now(&home);
    drop(daemon_env);
    let value: serde_json::Value = serde_json::from_slice(
        &fs::read(home.home().join(".clauth").join("gateway-env.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        value["tokens"],
        serde_json::json!([["SHUNT_CLIENT_TOKENS", {"names": ["ci"]}]])
    );
    drop(held);
}

/// A gateway adopted while a daemon that started without one runs: the
/// record answers for shunt's default variable and the one the daemon's env
/// layer names, so the add reads the daemon's inherited names instead of
/// refusing the variable as unrecorded.
#[test]
fn a_gateway_adopted_under_a_running_daemon_takes_client_tokens() {
    let home = HomeSandbox::new();
    let _env = no_shunt_env(&home);
    let daemon_env = EnvPin::new(
        &home,
        &[
            (
                "SHUNT_SERVER__AUTH__TOKENS_ENV",
                Some(OsStr::new("FOO_TOKENS")),
            ),
            ("FOO_TOKENS", Some(OsStr::new("alice:a1-secret"))),
        ],
    );
    let held = daemon_record_now(&home);
    drop(daemon_env);
    let record = with_config(&home, "[server]\n", "");
    save_record(&record);
    assert_eq!(
        crate::gateway_tokens::add_client_token(&record, "alice")
            .map(|_| ())
            .map_err(|e| format!("{e:#}")),
        Err(clash_inherited("alice", "FOO_TOKENS"))
    );
    assert!(
        crate::gateway_tokens::add_client_token(&record, "kerry").is_ok(),
        "a name the daemon's value does not hold takes a token"
    );
    drop(held);
}

/// With the tokens variable unrecorded (a config naming another one after
/// the daemon started) and no token stored, the TUI's check env still lays
/// the daemon's recorded `[server.auth]` keys, removes the TUI's own, and sets
/// the variable empty so the TUI's own value never reaches the check.
#[test]
fn the_check_env_keeps_the_daemons_auth_keys_when_its_variable_is_unrecorded() {
    let home = HomeSandbox::new();
    let _env = no_shunt_env(&home);
    let record = with_config(&home, "[server]\n", "");
    save_record(&record);
    let daemon_env = EnvPin::new(
        &home,
        &[(
            "SHUNT_SERVER__AUTH__HEADER",
            Some(OsStr::new("daemon-header")),
        )],
    );
    let held = daemon_record_now(&home);
    drop(daemon_env);
    write(
        record.config(),
        "[server.auth]\ntokens_env = \"LATER_TOKENS\"\n",
    );
    let _tui_env = EnvPin::new(
        &home,
        &[
            ("SHUNT_SERVER__AUTH__JWT", Some(OsStr::new("tui-only"))),
            ("LATER_TOKENS", Some(OsStr::new("tui-secret"))),
        ],
    );
    let env = checked_env(&record).expect("the check env");
    let mut command = Command::new("true");
    env.apply(&mut command);
    let mut laid: Vec<(String, Option<String>)> = command
        .get_envs()
        .filter_map(|(name, value)| {
            let name = name.to_string_lossy().into_owned();
            (name.starts_with("SHUNT_SERVER__AUTH__") || name == "LATER_TOKENS")
                .then(|| (name, value.map(|v| v.to_string_lossy().into_owned())))
        })
        .collect();
    laid.sort();
    assert_eq!(
        laid,
        [
            ("LATER_TOKENS".to_string(), Some(String::new())),
            (
                "SHUNT_SERVER__AUTH__HEADER".to_string(),
                Some("daemon-header".to_string())
            ),
            ("SHUNT_SERVER__AUTH__JWT".to_string(), None),
        ]
    );
    drop(held);
}

/// With a `tokens_env` clauth cannot read (a `${…}` reference) and no token
/// stored, the TUI's check env still lays the daemon's recorded
/// `[server.auth]` keys and removes the TUI's own: only the variable, whose
/// name is unknown, goes unset.
#[test]
fn the_check_env_keeps_the_daemons_auth_keys_when_its_variable_cannot_be_read() {
    let home = HomeSandbox::new();
    let _env = no_shunt_env(&home);
    let record = with_config(&home, "[server]\n", "");
    save_record(&record);
    let daemon_env = EnvPin::new(
        &home,
        &[(
            "SHUNT_SERVER__AUTH__HEADER",
            Some(OsStr::new("daemon-header")),
        )],
    );
    let held = daemon_record_now(&home);
    drop(daemon_env);
    write(record.config(), "[server.auth]\ntokens_env = \"${TOK}\"\n");
    let _tui_env = EnvPin::new(
        &home,
        &[("SHUNT_SERVER__AUTH__JWT", Some(OsStr::new("tui-only")))],
    );
    let env = checked_env(&record).expect("the check env");
    let mut command = Command::new("true");
    env.apply(&mut command);
    let mut laid: Vec<(String, Option<String>)> = command
        .get_envs()
        .filter_map(|(name, value)| {
            let name = name.to_string_lossy().into_owned();
            name.starts_with("SHUNT_SERVER__AUTH__")
                .then(|| (name, value.map(|v| v.to_string_lossy().into_owned())))
        })
        .collect();
    laid.sort();
    assert_eq!(
        laid,
        [
            (
                "SHUNT_SERVER__AUTH__HEADER".to_string(),
                Some("daemon-header".to_string())
            ),
            ("SHUNT_SERVER__AUTH__JWT".to_string(), None),
        ]
    );
    drop(held);
}

/// A store entry shunt's grammar cannot carry (a hand edit) refuses the
/// spawn only where shunt reads the tokens; elsewhere it joins as it is.
#[test]
fn a_store_entry_shunt_cannot_carry_refuses_only_where_shunt_reads_the_tokens() {
    let home = HomeSandbox::new();
    let _env = no_shunt_env(&home);
    let record = with_config(&home, "[server]\n", "");
    let store = home
        .home()
        .join(".clauth")
        .join("gateway-client-tokens.toml");
    write(&store, "[tokens]\nkerry = \"a,b\"\n");
    let spawned = |record: &GatewayRecord| {
        gateway_env(record)
            .map(|env| {
                env.get("SHUNT_CLIENT_TOKENS")
                    .map(|value| value.to_string_lossy().into_owned())
            })
            .map_err(|e| format!("{e:#}"))
    };
    let open = spawned(&record);
    write(record.config(), "[server.auth]\n");
    assert_eq!(
        (open, spawned(&record)),
        (
            Ok(Some("kerry:a,b".to_string())),
            Err(format!(
                "clauth's client-token store {} holds an entry for profile \"kerry\" that shunt cannot read; remove that profile's token and add it again",
                store.display()
            ))
        )
    );
}

/// The daemon's record keeps the inherited tokens variable's client names,
/// never a token, in no spelling: the record holds no credential.
#[test]
fn the_daemon_record_keeps_client_names_never_a_token() {
    let home = HomeSandbox::new();
    let _env = no_shunt_env(&home);
    save_record(&with_config(&home, "[server]\n", ""));
    let held = crate::daemon::hold_daemon_lock();
    fs::write(
        home.home().join(".clauth").join("clauthd.pid"),
        format!("{}\n", std::process::id()),
    )
    .unwrap();
    write_daemon_env_from(&env_pairs(&[("SHUNT_CLIENT_TOKENS", "ci:abc")]))
        .expect("record the daemon env");
    let bytes = fs::read(home.home().join(".clauth").join("gateway-env.json")).unwrap();
    let text = String::from_utf8(bytes.clone()).expect("JSON text");
    assert!(!text.contains("abc"), "the token's text appears nowhere");
    assert!(
        !text.replace(char::is_whitespace, "").contains("97,98,99"),
        "nor its bytes as a JSON byte array"
    );
    let mut value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    value.as_object_mut().expect("an object").remove("identity");
    assert_eq!(
        value,
        serde_json::json!({
            "env": [],
            "auth_env": [],
            "tokens": [["SHUNT_CLIENT_TOKENS", {"names": ["ci"]}]],
        })
    );
    drop(held);
}

/// Adopt `record`, so a daemon starting now records the tokens variable its
/// config names.
fn save_record(record: &GatewayRecord) {
    GatewayRecord::update(|slot| {
        *slot = Some(record.clone());
        Ok(())
    })
    .expect("save the record");
}

/// Pose a daemon holding the singleton under this process's pid, and write
/// its env record from this process's env as it stands.
fn daemon_record_now(home: &HomeSandbox) -> std::fs::File {
    let held = crate::daemon::hold_daemon_lock();
    fs::write(
        home.home().join(".clauth").join("clauthd.pid"),
        format!("{}\n", std::process::id()),
    )
    .unwrap();
    write_daemon_env().expect("record the daemon env");
    held
}

fn clash_inherited(profile: &str, variable: &str) -> String {
    format!(
        "cannot add a client token for profile {profile:?}: the environment the gateway inherits already names a client {profile:?} in {variable}, and shunt refuses a duplicate name; remove that entry from that environment first"
    )
}

/// A daemon inheriting the env-layer override in another case
/// (`shunt_server__auth__tokens_env`, which figment and the spawn read)
/// records it, so the TUI derives the variable the spawn does.
#[test]
fn the_record_captures_a_case_variant_auth_key_the_spawn_reads() {
    let home = HomeSandbox::new();
    let _env = no_shunt_env(&home);
    let record = with_config(&home, "[server]\n", "");
    save_record(&record);
    let daemon_env = EnvPin::new(
        &home,
        &[
            (
                "shunt_server__auth__tokens_env",
                Some(OsStr::new("FOO_TOKENS")),
            ),
            ("FOO_TOKENS", Some(OsStr::new("alice:a1-secret"))),
        ],
    );
    let held = daemon_record_now(&home);
    drop(daemon_env);
    assert_eq!(
        crate::gateway_tokens::add_client_token(&record, "alice")
            .map(|_| ())
            .map_err(|e| format!("{e:#}")),
        Err(clash_inherited("alice", "FOO_TOKENS"))
    );
    drop(held);
}

/// The TUI matches the daemon's record to the singleton holder by pid alone:
/// the start-time half spawns `ps` or PowerShell off linux, which a read on
/// the UI thread must not. A record naming the holder's pid under another
/// start time still reads.
#[test]
fn the_tui_matches_the_daemons_record_by_pid_alone() {
    let home = HomeSandbox::new();
    let _env = no_shunt_env(&home);
    let record = with_config(&home, "[server]\n", "");
    save_record(&record);
    let daemon_env = EnvPin::new(
        &home,
        &[("SHUNT_CLIENT_TOKENS", Some(OsStr::new("alice:a1-secret")))],
    );
    let held = daemon_record_now(&home);
    drop(daemon_env);
    let path = home.home().join(".clauth").join("gateway-env.json");
    let mut value: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    value["identity"]["start"] = serde_json::Value::from("not-this-process-start");
    fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
    assert_eq!(
        crate::gateway_tokens::add_client_token(&record, "alice")
            .map(|_| ())
            .map_err(|e| format!("{e:#}")),
        Err(clash_inherited("alice", "SHUNT_CLIENT_TOKENS"))
    );
    drop(held);
}

/// The `shunt check` env the TUI builds under a daemon carries the
/// `[server.auth]` keys the daemon's spawn would, from the daemon's record,
/// and none of the TUI's own: a recorded override is laid, a TUI-only key is
/// removed, and the variable carries the recorded names with stand-in tokens
/// beside the store's pairs.
#[test]
fn the_check_env_carries_the_daemons_auth_keys_never_the_tuis() {
    let home = HomeSandbox::new();
    let _env = no_shunt_env(&home);
    let record = with_config(&home, "[server]\n", "");
    save_record(&record);
    let daemon_env = EnvPin::new(
        &home,
        &[
            (
                "SHUNT_SERVER__AUTH__TOKENS_ENV",
                Some(OsStr::new("FOO_TOKENS")),
            ),
            ("FOO_TOKENS", Some(OsStr::new("alice:a1-secret"))),
        ],
    );
    let held = daemon_record_now(&home);
    drop(daemon_env);
    let _tui_env = EnvPin::new(
        &home,
        &[
            ("SHUNT_SERVER__AUTH__HEADER", Some(OsStr::new("tui-only"))),
            ("FOO_TOKENS", Some(OsStr::new("tui-secret"))),
        ],
    );
    let kerry = add_token(&record, "kerry");
    let env = checked_env(&record).expect("the check env");
    let mut command = Command::new("true");
    env.apply(&mut command);
    let mut laid: Vec<(String, Option<String>)> = command
        .get_envs()
        .filter_map(|(name, value)| {
            let name = name.to_string_lossy().into_owned();
            (name.starts_with("SHUNT_SERVER__AUTH__") || name == "FOO_TOKENS")
                .then(|| (name, value.map(|v| v.to_string_lossy().into_owned())))
        })
        .collect();
    laid.sort();
    assert_eq!(
        laid,
        [
            (
                "FOO_TOKENS".to_string(),
                Some(format!("alice:clauth-check-stand-in,kerry:{kerry}"))
            ),
            ("SHUNT_SERVER__AUTH__HEADER".to_string(), None),
            (
                "SHUNT_SERVER__AUTH__TOKENS_ENV".to_string(),
                Some("FOO_TOKENS".to_string())
            ),
        ]
    );
    drop(held);
}

/// Under a daemon whose inherited tokens variable is unset, the check env
/// sets it empty, so the TUI's own value never reaches the check.
#[test]
fn the_check_env_never_carries_the_tuis_own_tokens_value() {
    let home = HomeSandbox::new();
    let _env = no_shunt_env(&home);
    let record = with_config(&home, "[server]\n", "");
    save_record(&record);
    let held = daemon_record_now(&home);
    let _tui_env = EnvPin::new(
        &home,
        &[("SHUNT_CLIENT_TOKENS", Some(OsStr::new("tui-secret")))],
    );
    let env = checked_env(&record).expect("the check env");
    let mut command = Command::new("true");
    env.apply(&mut command);
    assert_eq!(
        command
            .get_envs()
            .filter(|(name, _)| *name == OsStr::new("SHUNT_CLIENT_TOKENS"))
            .map(|(_, value)| value.map(|v| v.to_string_lossy().into_owned()))
            .collect::<Vec<_>>(),
        [Some(String::new())]
    );
    drop(held);
}

/// With no `[server.auth]`, shunt never reads the tokens variable, so a
/// malformed or colliding base passes through joined, refusing nothing; with
/// the table the same base refuses the spawn.
#[test]
fn the_spawn_refuses_a_bad_base_only_where_shunt_reads_the_tokens() {
    let home = HomeSandbox::new();
    let _env = no_shunt_env(&home);
    let record = with_config(&home, "[server]\n", "");
    let env_file = record.env_file.clone().expect("env file");
    let kerry = add_token(&record, "kerry");
    let unparsed = format!(
        "the gateway's env file {} holds a SHUNT_CLIENT_TOKENS value shunt cannot parse, and shunt refuses to start on it; fix that value first",
        env_file.display()
    );
    let collision = format!(
        "the gateway's env file {} and clauth's client-token store both name a client \"kerry\" in SHUNT_CLIENT_TOKENS, and shunt refuses a duplicate name; remove that entry from the env file, or remove clauth's token for that profile",
        env_file.display()
    );
    let mut got = Vec::new();
    let mut want = Vec::new();
    for (base, refusal) in [
        ("broken-secret", &unparsed),
        ("kerry:k0-secret", &collision),
    ] {
        write(&env_file, &format!("SHUNT_CLIENT_TOKENS={base}\n"));
        for (config, outcome) in [
            ("[server]\n", Ok(Some(format!("{base},kerry:{kerry}")))),
            ("[server.auth]\n", Err(refusal.clone())),
        ] {
            write(record.config(), config);
            got.push((
                base,
                config,
                gateway_env(&record)
                    .map(|env| {
                        env.get("SHUNT_CLIENT_TOKENS")
                            .map(|value| value.to_string_lossy().into_owned())
                    })
                    .map_err(|e| format!("{e:#}")),
            ));
            want.push((base, config, outcome));
        }
    }
    assert_eq!(got, want);
}

/// A daemon starting over a config that names its own tokens variable
/// records that variable's names, so the TUI reads them and never refuses
/// the variable as unrecorded.
#[test]
fn a_daemon_records_the_variable_its_config_names() {
    let home = HomeSandbox::new();
    let _env = no_shunt_env(&home);
    let record = with_config(&home, "[server.auth]\ntokens_env = \"CFG_TOKENS\"\n", "");
    save_record(&record);
    let daemon_env = EnvPin::new(
        &home,
        &[("CFG_TOKENS", Some(OsStr::new("alice:a1-secret")))],
    );
    let held = daemon_record_now(&home);
    drop(daemon_env);
    assert_eq!(
        crate::gateway_tokens::add_client_token(&record, "alice")
            .map(|_| ())
            .map_err(|e| format!("{e:#}")),
        Err(clash_inherited("alice", "CFG_TOKENS"))
    );
    drop(held);
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
        "the shunt config does not parse as TOML (line 2)"
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
            "the shunt config must be TOML, and {} is YAML",
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
/// chatty check never stalls on its pipe and never grows the refusal. The
/// single 70 000-byte line has no newline, so the cap's trailing partial line
/// is dropped whole, and the held text is the one cut marker line — never a
/// blank modal.
#[cfg(unix)]
#[test]
fn a_checks_stderr_is_capped() {
    let g = gate(USER_CONFIG);
    write(&g.stub.join("loud"), "");
    let err = add_admin_write_key(&g.record).expect_err("the check fails");
    match refusal(&err) {
        ConfigEditRefusal::CheckFailed { stderr, .. } => {
            assert_eq!(stderr.text(), "output cut; the cut line is withheld");
        }
        other => panic!("a CheckFailed refusal, got {other:?}"),
    }
    assert_eq!(fs::read_to_string(&g.config).expect("read"), USER_CONFIG);
}

/// The masking set scans the candidate's PARSED string leaves the way shunt
/// does: a `${` in a comment must not swallow a later real reference, a
/// TOML-escaped reference resolves, an array element resolves, and a
/// `${...}` in a KEY (which shunt never substitutes) is not added.
#[test]
fn the_masking_set_scans_parsed_string_leaves_not_raw_text() {
    let home = HomeSandbox::new();
    let config = home.home().join("etc").join("shunt.toml");
    write(
        &config,
        "# a comment with ${ a stray\n[server]\nnote = \"\\u0024{ZZ_A}\"\nlist = [\"${ZZ_B}\"]\n\"${ZZ_K}\" = \"key not a value\"\n",
    );
    let env = GatewayEnv::default();
    let inherited: Vec<(OsString, OsString)> = vec![
        ("ZZ_A".into(), "hunter2.pass+word".into()),
        ("ZZ_B".into(), "other.secret.value".into()),
        ("ZZ_K".into(), "key.secret.value".into()),
    ];
    let values = check_masking_values(&config, &env, &inherited);
    assert_eq!(
        values,
        [
            "hunter2.pass+word".to_string(),
            "other.secret.value".to_string()
        ],
    );
}

/// A `${file:}` naming a non-regular file (a char device) is skipped, never
/// opened, so the worker can neither block nor steal the terminal.
#[cfg(unix)]
#[test]
fn a_non_regular_file_reference_is_skipped() {
    let home = HomeSandbox::new();
    let config = home.home().join("etc").join("shunt.toml");
    write(&config, "[server]\nnote = \"${file:/dev/null}\"\n");
    let env = GatewayEnv::default();
    let inherited: Vec<(OsString, OsString)> = Vec::new();
    let values = check_masking_values(&config, &env, &inherited);
    assert!(
        values.is_empty(),
        "a non-regular file is skipped: {values:?}"
    );
}

/// A secret file's contribution is its whole trimmed contents plus each
/// non-empty trimmed line, so a single quoted line is masked too.
#[cfg(unix)]
#[test]
fn a_secret_files_lines_are_masked() {
    let home = HomeSandbox::new();
    let dir = home.home().join("etc");
    std::fs::create_dir_all(&dir).unwrap();
    let tok = dir.join("tokens.txt");
    write(&tok, "first.secret.value\nsecond.secret.value\n");
    let config = home.home().join("etc").join("shunt.toml");
    write(
        &config,
        &format!("[server]\nnote = \"${{file:{}}}\"\n", tok.display()),
    );
    let env = GatewayEnv::default();
    let inherited: Vec<(OsString, OsString)> = Vec::new();
    let values = check_masking_values(&config, &env, &inherited);
    assert!(
        values.iter().any(|v| v == "first.secret.value"),
        "{values:?}"
    );
    assert!(
        values.iter().any(|v| v == "second.secret.value"),
        "{values:?}"
    );
    assert!(
        values
            .iter()
            .any(|v| v == "first.secret.value\nsecond.secret.value"),
        "{values:?}"
    );
}

/// An over-cap secret file contributes the complete lines within its first cap
/// bytes (the trailing partial line dropped), so its first line is masked.
#[cfg(unix)]
#[test]
fn an_over_cap_files_complete_lines_are_masked() {
    let home = HomeSandbox::new();
    let dir = home.home().join("etc");
    std::fs::create_dir_all(&dir).unwrap();
    let big = dir.join("big.txt");
    let mut content = "first.line.secret\n".to_string();
    content.push_str(&"filler\n".repeat(20_000));
    write(&big, &content);
    let config = home.home().join("etc").join("shunt.toml");
    write(
        &config,
        &format!("[server]\nnote = \"${{file:{}}}\"\n", big.display()),
    );
    let env = GatewayEnv::default();
    let inherited: Vec<(OsString, OsString)> = Vec::new();
    let values = check_masking_values(&config, &env, &inherited);
    assert!(
        values.iter().any(|v| v == "first.line.secret"),
        "{:?}",
        values.iter().take(3).collect::<Vec<_>>()
    );
}

/// The masking set takes every inherited `SHUNT_*` value (shunt layers them
/// over the config), except the store-pin keys clauth itself chooses.
#[test]
fn the_masking_set_covers_inherited_shunt_env_values() {
    let home = HomeSandbox::new();
    let config = home.home().join("etc").join("shunt.toml");
    write(&config, "[server]\n");
    let env = GatewayEnv::default();
    let inherited: Vec<(OsString, OsString)> = vec![
        (
            "SHUNT_PROVIDERS__ACME__SERVICE_TIER".into(),
            "tier.secret.value".into(),
        ),
        ("SHUNT_CLAUDE_ACCOUNTS_DIR".into(), "/store/claude".into()),
    ];
    let values = check_masking_values(&config, &env, &inherited);
    assert!(
        values.iter().any(|v| v == "tier.secret.value"),
        "{values:?}"
    );
    assert!(!values.iter().any(|v| v == "/store/claude"), "{values:?}");
}

/// `tokens_file` resolves its path the way shunt reads it: `~/` expands over
/// the check's HOME, and a relative path resolves against the config's
/// directory.
#[cfg(unix)]
#[test]
fn the_tokens_file_path_resolves_tilde_and_relative() {
    let home = HomeSandbox::new();
    let home_dir = home.home().to_path_buf();
    let etc = home_dir.join("etc");
    std::fs::create_dir_all(&etc).unwrap();
    write(&home_dir.join("tok.txt"), "tilde.secret.value\n");
    let env = GatewayEnv::default();
    let inherited: Vec<(OsString, OsString)> =
        vec![("HOME".into(), home_dir.clone().into_os_string())];

    let config = etc.join("shunt.toml");
    write(&config, "[server.admin]\ntokens_file = \"~/tok.txt\"\n");
    let values = check_masking_values(&config, &env, &inherited);
    assert!(
        values.iter().any(|v| v == "tilde.secret.value"),
        "{values:?}"
    );

    write(&etc.join("rel.txt"), "relative.secret.value\n");
    let config2 = etc.join("other.toml");
    write(&config2, "[server.admin]\ntokens_file = \"rel.txt\"\n");
    let values2 = check_masking_values(&config2, &env, &inherited);
    assert!(
        values2.iter().any(|v| v == "relative.secret.value"),
        "{values2:?}"
    );

    // A `${...}`-shaped path substitutes first, then the file is read.
    write(&etc.join("var.txt"), "dollar.secret.value\n");
    let config3 = etc.join("var.toml");
    write(&config3, "[server.admin]\ntokens_file = \"${TF}\"\n");
    let inherited3: Vec<(OsString, OsString)> = vec![
        ("HOME".into(), home_dir.into_os_string()),
        ("TF".into(), etc.join("var.txt").into_os_string()),
    ];
    let values3 = check_masking_values(&config3, &env, &inherited3);
    assert!(
        values3.iter().any(|v| v == "dollar.secret.value"),
        "{values3:?}"
    );
}

/// A `$${` escape is a literal `${`, never a reference, in the shared scanner.
#[test]
fn a_double_dollar_escape_is_not_a_reference() {
    let home = HomeSandbox::new();
    let config = home.home().join("etc").join("shunt.toml");
    write(&config, "[server]\nnote = \"$${ZZ_A}\"\n");
    let env = GatewayEnv::default();
    let inherited: Vec<(OsString, OsString)> = vec![("ZZ_A".into(), "hunter2.pass+word".into())];
    let values = check_masking_values(&config, &env, &inherited);
    assert!(
        !values.iter().any(|v| v == "hunter2.pass+word"),
        "{values:?}"
    );
}

/// A `${file:}` naming a FIFO is skipped by stat without opening it, so the
/// call returns promptly and the FIFO is never read.
#[cfg(unix)]
#[test]
fn a_fifo_file_reference_is_skipped_without_opening() {
    let home = HomeSandbox::new();
    let dir = home.home().join("etc");
    std::fs::create_dir_all(&dir).unwrap();
    let fifo = dir.join("fifo");
    std::process::Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .expect("mkfifo");
    assert_eq!(secret_file_segments(fifo.to_str().unwrap()), None);
}

/// The masking set takes shunt's default token env vars, every config-named
/// `*_env` key (a `${...}`-valued one resolved first), and their values.
#[test]
fn the_masking_set_covers_env_named_secret_vars() {
    let home = HomeSandbox::new();
    let config = home.home().join("etc").join("shunt.toml");
    write(
        &config,
        "[server]\n[server.auth]\ntokens_env = \"${TOKENS_NAME}\"\n[server.gateway]\nusers_env = \"MY_USERS\"\n",
    );
    let env = GatewayEnv::default();
    let inherited: Vec<(OsString, OsString)> = vec![
        ("TOKENS_NAME".into(), "MY_TOKENS".into()),
        ("MY_TOKENS".into(), "tok.en+val.ue".into()),
        ("MY_USERS".into(), "user.secret.value".into()),
        ("SHUNT_CLIENT_TOKENS".into(), "default.tok.value".into()),
    ];
    let values = check_masking_values(&config, &env, &inherited);
    assert!(values.iter().any(|v| v == "tok.en+val.ue"), "{values:?}");
    assert!(
        values.iter().any(|v| v == "user.secret.value"),
        "{values:?}"
    );
    assert!(
        values.iter().any(|v| v == "default.tok.value"),
        "{values:?}"
    );
}

/// The masking set reads `[server.admin].tokens_file`'s trimmed contents, the
/// one `*_file` key shunt reads as a secret file.
#[test]
fn the_masking_set_reads_the_tokens_file() {
    let home = HomeSandbox::new();
    let dir = home.home().join("etc");
    std::fs::create_dir_all(&dir).unwrap();
    let tok = dir.join("tokens.txt");
    write(&tok, "  ops:tok.en+val.ue  \n");
    let config = home.home().join("etc").join("shunt.toml");
    write(
        &config,
        &format!("[server.admin]\ntokens_file = \"{}\"\n", tok.display()),
    );
    let env = GatewayEnv::default();
    let inherited: Vec<(OsString, OsString)> = Vec::new();
    let values = check_masking_values(&config, &env, &inherited);
    assert!(
        values.iter().any(|v| v == "ops:tok.en+val.ue"),
        "{values:?}"
    );
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

/// `mask_check_output` hides every secret value wherever it appears (the
/// longest match winning at each position), then any leftover key-shaped
/// token, and leaves ordinary lines alone. Exact equality per shape.
#[test]
fn mask_check_output_hides_env_values_and_key_tokens() {
    let secrets: Vec<String> = vec![
        "sk-SECRET-value-123".to_string(),
        "/home/uwuclxdy".to_string(),
        "plain.secret.v".to_string(),
        "plain.secret.value".to_string(),
    ];
    for (input, want) in [
        // An env value inside quotes: the env pass masks it, the token pass
        // leaves the `sk-` stand-in's 3-char head alone.
        (
            "invalid token: found \"sk-SECRET-value-123\"",
            "invalid token: found \"sk-…\"",
        ),
        // A non-key-shaped value appearing twice is masked at both sites.
        ("plain.secret.v and plain.secret.v", "pla… and pla…"),
        // Two overlapping values: the longer one wins the match.
        ("found plain.secret.value", "found pla…"),
        // An `sk-` key NOT in the env: the token pass catches it.
        ("missing key sk-ant-api03-abcdef", "missing key sk-…"),
        // A long hex-ish id (20+ chars, letter + digit).
        ("id 0123456789abcdef0123456789abcdef", "id 012…"),
        // A non-`sk-` env value (a path) is masked too.
        ("home is /home/uwuclxdy today", "home is /ho… today"),
        // A normal error line is left untouched.
        ("config error: boom", "config error: boom"),
    ] {
        assert_eq!(mask_check_output(input, &secrets), want, "input: {input}");
    }
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
    KeptFile::NoHome { store }
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

/// Plan then move with the sandbox `HOME` as the inherited env, so every
/// store's default resolves under the sandbox home, never a production
/// fallback.
fn move_stores(record: &GatewayRecord) -> Result<StoreMove> {
    move_stores_with(record, sandbox_home())
}

/// Plan then move over `inherited`, the way the card's flow does over the
/// reader's env, so a test injects one instead of reading the process's own.
fn move_stores_with(
    record: &GatewayRecord,
    inherited: impl IntoIterator<Item = (OsString, OsString)>,
) -> Result<StoreMove> {
    let inherited: Vec<(OsString, OsString)> = inherited.into_iter().collect();
    let plan = plan_standalone_stores_in(
        record,
        inherited.iter().cloned(),
        CodexHomeSource::Inherited,
    )?;
    move_standalone_stores_in(
        record,
        GatewaySilent::for_test(),
        &plan,
        inherited,
        CodexHomeSource::Inherited,
    )
}

/// The sandbox `HOME`, injected as the inherited env: the home every test
/// that does not exercise the home rules passes.
fn sandbox_home() -> [(OsString, OsString); 1] {
    [(
        OsString::from("HOME"),
        home_dir().expect("the sandbox home").into_os_string(),
    )]
}

/// The SHA-256 of a file's bytes, so a before/after comparison is a byte
/// identity check, never a name or length check.
fn sha256(path: &Path) -> [u8; 32] {
    use sha2::{Digest as _, Sha256};
    Sha256::digest(fs::read(path).expect("read")).into()
}

/// Write the daemon env record naming the current test process as the live
/// holder (the pid sidecar stamped first), with the given env values. The
/// caller holds the daemon lock for the whole read that follows.
fn write_env_record_naming_self(
    home: &HomeSandbox,
    codex_home: Option<&Path>,
    home_env: Option<&Path>,
) {
    std::fs::write(
        home.home().join(".clauth").join("clauthd.pid"),
        format!("{}\n", std::process::id()),
    )
    .expect("stamp the holder pid");
    let pid = std::process::id();
    let identity = DaemonIdentity {
        pid,
        start: Some(
            crate::daemon::gateway::process_start_time(pid).expect("the test process start time"),
        ),
    };
    write_env_record(DaemonEnvRecord {
        identity,
        env: record_env(codex_home, home_env),
        auth_env: Vec::new(),
        tokens: Vec::new(),
    })
    .expect("write the env record");
}

/// A record's env for tests: `CODEX_HOME` and `HOME` when `Some`, each as its
/// platform-native bytes.
fn record_env(codex_home: Option<&Path>, home_env: Option<&Path>) -> Vec<(String, Vec<u8>)> {
    let mut env = Vec::new();
    if let Some(value) = codex_home {
        env.push(("CODEX_HOME".to_string(), env_value_bytes(value.as_os_str())));
    }
    if let Some(home) = home_env {
        env.push(("HOME".to_string(), env_value_bytes(home.as_os_str())));
    }
    env
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
            kept: vec![KeptFile::At {
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
                KeptFile::At {
                    path: caps.clone(),
                    reason: KeptReason::LeftBehind,
                },
                KeptFile::At {
                    path: a_b.clone(),
                    reason: KeptReason::LeftBehind,
                },
                KeptFile::At {
                    path: backup.clone(),
                    reason: KeptReason::LeftBehind,
                },
                KeptFile::At {
                    path: link.clone(),
                    reason: KeptReason::LeftBehind,
                },
                KeptFile::At {
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

    let result = move_stores_with(
        &record,
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

    let result = move_stores_with(&record, [(OsString::from("HOME"), OsString::new())])
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
    let result = move_stores_with(&record, std::iter::empty()).expect("move");

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
/// standalone resolved it against a working directory clauth cannot know. The
/// codex `~/.codex` guard resolves the same home the store defaults use, so
/// its refusal is the first to fire.
#[test]
fn a_relative_home_still_refuses_the_move() {
    let home = HomeSandbox::new();
    let record = with_env_file(&home, "HOME=rel-home\n");

    let result = move_stores(&record);

    assert_eq!(
        result.map(|m| m.moved).map_err(|e| e.to_string()),
        Err(
            "the standalone's home names no absolute directory, so clauth cannot tell where the codex CLI's own login (~/.codex/auth.json) sits; nothing was moved; set HOME to an absolute path in the env file, then run the move again"
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
            kept: vec![KeptFile::At {
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
                kept: vec![KeptFile::At {
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
            kept: vec![KeptFile::At {
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
            kept: vec![KeptFile::At {
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
            kept: vec![KeptFile::At {
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
            kept: vec![KeptFile::At {
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
            kept: vec![KeptFile::At {
                path: login.clone(),
                reason: KeptReason::CodexLogin,
            }],
        })
    );

    let record = with_env_file(&home, &format!("CODEX_AUTH_FILE={}\n", quoted(&login)));
    assert_eq!(
        move_stores_with(
            &record,
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
            kept: vec![KeptFile::At {
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
            move_stores_with(
                &record,
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
                kept: vec![KeptFile::At {
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
            move_stores_with(
                &record,
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
                kept: vec![KeptFile::At {
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
        move_stores_with(
            &record,
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
                KeptFile::At {
                    path: d.join("auth.json"),
                    reason: KeptReason::CodexLogin,
                },
                KeptFile::At {
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
        let err = move_stores_with(
            &record,
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
        let err = move_stores_with(
            &record,
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
            kept: vec![KeptFile::At {
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

    let plan = plan_standalone_stores_in(&record, std::iter::empty(), CodexHomeSource::Inherited)
        .expect("plan");
    let err = move_standalone_stores_in(
        &record,
        GatewaySilent::for_test_at(other),
        &plan,
        std::iter::empty(),
        CodexHomeSource::Inherited,
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

/// A `CODEX_HOME` set only in the daemon's recorded env (not the test
/// process's env, not the env file) keeps the login `CODEX_AUTH_FILE` names at
/// that home: the plan reads the running daemon's record through the reader,
/// so the codex CLI's own login stays at its source.
#[test]
fn a_codex_home_set_only_in_the_daemons_record_keeps_its_auth_json() {
    let home = HomeSandbox::new();
    let custom = home.home().join("custom-codex");
    let login = custom.join("auth.json");
    write(&login, "codex-cli-login");
    let record = with_env_file(&home, &format!("CODEX_AUTH_FILE={}\n", quoted(&login)));

    let _held = crate::daemon::hold_daemon_lock();
    write_env_record_naming_self(&home, Some(&custom), Some(home.home()));

    let plan = plan_standalone_stores(&record).expect("plan");
    assert_eq!(plan.moved, Vec::<MovedFile>::new(), "nothing moves");
    assert_eq!(
        plan.kept,
        vec![KeptFile::At {
            path: login.clone(),
            reason: KeptReason::CodexLogin,
        }],
        "only the codex login is kept, as the codex CLI's own"
    );
    assert_eq!(fs::read_to_string(&login).expect("kept"), "codex-cli-login");
    assert!(!home.home().join(".clauth").join("shunt").exists());
}

/// The reader returns the record only when it names the daemon holding the
/// singleton now: a record naming another pid, or this pid with another start
/// token, is a dead daemon's and refuses the plan rather than reading as the
/// running daemon's env.
#[test]
fn the_env_record_reads_only_the_live_holders_record() {
    let home = HomeSandbox::new();
    let _held = crate::daemon::hold_daemon_lock();
    std::fs::write(
        home.home().join(".clauth").join("clauthd.pid"),
        format!("{}\n", std::process::id()),
    )
    .expect("stamp the holder pid");
    let pid = std::process::id();
    let start =
        crate::daemon::gateway::process_start_time(pid).expect("the test process start time");

    let live = DaemonEnvRecord {
        identity: DaemonIdentity {
            pid,
            start: Some(start.clone()),
        },
        env: vec![(
            "CODEX_HOME".to_string(),
            env_value_bytes(OsStr::new("/live")),
        )],
        auth_env: Vec::new(),
        tokens: Vec::new(),
    };
    write_env_record(live.clone()).expect("write");
    assert_eq!(
        recorded_env_for_live_daemon().expect("a live record"),
        Some(vec![(
            OsString::from("CODEX_HOME"),
            OsString::from("/live"),
        )])
    );

    // A record naming another pid is a dead daemon's: the plan refuses.
    write_env_record(DaemonEnvRecord {
        identity: DaemonIdentity {
            pid: pid + 1,
            start: Some(start.clone()),
        },
        env: vec![(
            "CODEX_HOME".to_string(),
            env_value_bytes(OsStr::new("/other")),
        )],
        auth_env: Vec::new(),
        tokens: Vec::new(),
    })
    .expect("write");
    assert_eq!(
        recorded_env_for_live_daemon()
            .expect_err("a stale record refuses")
            .to_string(),
        "the running daemon recorded no environment; restart it, then look at the plan again"
    );

    // A record naming this pid with another start token is another instance's.
    write_env_record(DaemonEnvRecord {
        identity: DaemonIdentity {
            pid,
            start: Some("another-token".to_string()),
        },
        env: vec![(
            "CODEX_HOME".to_string(),
            env_value_bytes(OsStr::new("/other")),
        )],
        auth_env: Vec::new(),
        tokens: Vec::new(),
    })
    .expect("write");
    assert_eq!(
        recorded_env_for_live_daemon()
            .expect_err("a stale record refuses")
            .to_string(),
        "the running daemon recorded no environment; restart it, then look at the plan again"
    );
}

/// The bind env the Services card reads is the live daemon's recorded one,
/// the env an adopt's probe reads, never this process's own; a record that
/// names no bind reads as none, and a stale record refuses.
#[test]
fn the_card_reads_the_live_daemons_recorded_bind() {
    let home = HomeSandbox::new();
    let _held = crate::daemon::hold_daemon_lock();
    std::fs::write(
        home.home().join(".clauth").join("clauthd.pid"),
        format!("{}\n", std::process::id()),
    )
    .expect("stamp the holder pid");
    let pid = std::process::id();
    let start =
        crate::daemon::gateway::process_start_time(pid).expect("the test process start time");
    let record = |pid, env: Vec<(String, Vec<u8>)>| DaemonEnvRecord {
        identity: DaemonIdentity {
            pid,
            start: Some(start.clone()),
        },
        env,
        auth_env: Vec::new(),
        tokens: Vec::new(),
    };

    write_env_record(record(
        pid,
        vec![(
            BIND_ENV.to_string(),
            env_value_bytes(OsStr::new("127.0.0.1:4700")),
        )],
    ))
    .expect("write");
    assert_eq!(
        inherited_bind_env().expect("a live record"),
        Some("127.0.0.1:4700".to_string())
    );

    write_env_record(record(
        pid,
        vec![(
            "CODEX_HOME".to_string(),
            env_value_bytes(OsStr::new("/live")),
        )],
    ))
    .expect("write");
    assert_eq!(inherited_bind_env().expect("a live record"), None);

    write_env_record(record(pid + 1, Vec::new())).expect("write");
    assert_eq!(
        inherited_bind_env()
            .expect_err("a stale record refuses")
            .to_string(),
        "the running daemon recorded no environment; restart it, then look at the plan again"
    );
}

/// A record naming this process while no daemon holds the singleton is not
/// trusted: the reader falls back to the caller's own env, because presence is
/// the lock, never a matching record alone.
#[test]
fn a_record_naming_this_process_without_the_singleton_held_falls_back() {
    let home = HomeSandbox::new();
    // No daemon lock held.
    std::fs::create_dir_all(home.home().join(".clauth")).expect("mkdir");
    std::fs::write(
        home.home().join(".clauth").join("clauthd.pid"),
        format!("{}\n", std::process::id()),
    )
    .expect("stamp the holder pid");
    let pid = std::process::id();
    write_env_record(DaemonEnvRecord {
        identity: DaemonIdentity {
            pid,
            start: Some(crate::daemon::gateway::process_start_time(pid).expect("start")),
        },
        env: vec![(
            "CODEX_HOME".to_string(),
            env_value_bytes(OsStr::new("/live")),
        )],
        auth_env: Vec::new(),
        tokens: Vec::new(),
    })
    .expect("write");
    assert_eq!(recorded_env_for_live_daemon().expect("no daemon"), None);
}

/// The daemon writes the record through the production path naming itself,
/// owner-only on unix, and unset values round-trip as unset (absent) rather
/// than as an empty value.
#[test]
fn the_daemon_env_record_is_0600_and_unset_values_round_trip_as_unset() {
    let home = HomeSandbox::new();
    let _held = crate::daemon::hold_daemon_lock();
    std::fs::write(
        home.home().join(".clauth").join("clauthd.pid"),
        format!("{}\n", std::process::id()),
    )
    .expect("stamp the holder pid");

    write_daemon_env().expect("write");
    let record = read_env_record().expect("read").expect("present");
    assert_eq!(
        record.identity,
        DaemonIdentity {
            pid: std::process::id(),
            start: Some(
                crate::daemon::gateway::process_start_time(std::process::id())
                    .expect("the test process start time")
            ),
        }
    );
    #[cfg(unix)]
    assert_eq!(
        mode(&home.home().join(".clauth").join("gateway-env.json")),
        0o600
    );

    // An unset value is an absent key, never an empty one.
    write_env_record(DaemonEnvRecord {
        identity: record.identity.clone(),
        env: Vec::new(),
        auth_env: Vec::new(),
        tokens: Vec::new(),
    })
    .expect("write unset");
    let unset = read_env_record().expect("read").expect("present");
    assert!(unset.env.is_empty());
    assert_eq!(
        recorded_pairs(&unset.env).expect("a test record decodes"),
        Vec::new()
    );
}

/// The daemon's capture records exactly the [`INHERITED_KEYS`] it inherited,
/// byte-exact, and nothing else: every one of the four keys the plan, move and
/// probe read is present, so dropping one (or recording nothing) reds here.
#[test]
fn the_daemon_env_capture_records_exactly_the_four_inherited_keys() {
    let home = HomeSandbox::new();
    let _held = crate::daemon::hold_daemon_lock();
    std::fs::write(
        home.home().join(".clauth").join("clauthd.pid"),
        format!("{}\n", std::process::id()),
    )
    .expect("stamp the holder pid");

    write_daemon_env_from(&env_pairs(&[
        ("CODEX_HOME", "/codex"),
        ("HOME", "/home"),
        ("USERPROFILE", "/winhome"),
        ("SHUNT_SERVER__BIND", "127.0.0.1:3001"),
        ("UNRELATED", "/not-recorded"),
    ]))
    .expect("write");

    let record = read_env_record().expect("read").expect("present");
    assert_eq!(
        record.env,
        vec![
            (
                "CODEX_HOME".to_string(),
                env_value_bytes(OsStr::new("/codex"))
            ),
            ("HOME".to_string(), env_value_bytes(OsStr::new("/home"))),
            (
                "USERPROFILE".to_string(),
                env_value_bytes(OsStr::new("/winhome"))
            ),
            (
                "SHUNT_SERVER__BIND".to_string(),
                env_value_bytes(OsStr::new("127.0.0.1:3001"))
            ),
        ]
    );
}

/// `own_env`'s injected source over hand-built pairs, last assignment wins on
/// a key (the way the process env's lookup reads), so a test drives the
/// production body without reading the process's own env.
fn env_source(pairs: &[(OsString, OsString)]) -> impl Fn(&str) -> Option<OsString> + '_ {
    move |key| {
        pairs
            .iter()
            .rev()
            .find(|(name, _)| name == OsStr::new(key))
            .map(|(_, value)| value.clone())
    }
}

/// The fallback scrubs a clauth codex home exactly as `start daemon` scrubs
/// it, keeps a directory the user owns, and reads both homes (HOME and
/// USERPROFILE) straight from the source — the shared body production runs.
#[test]
fn the_fallback_scrubs_a_clauth_codex_home_and_keeps_the_users_own() {
    let home = HomeSandbox::new();
    let clauth_codex = home
        .home()
        .join(".clauth")
        .join("profiles")
        .join("p")
        .join("codex-home");
    let own = home.home().join("own-codex");
    let home_env = home.home().as_os_str().to_os_string();

    assert_eq!(
        own_env(env_source(&[
            (OsString::from("CODEX_HOME"), clauth_codex.into_os_string()),
            (OsString::from("HOME"), home_env.clone()),
            (OsString::from("USERPROFILE"), home_env.clone()),
        ])),
        vec![
            (OsString::from("HOME"), home_env.clone()),
            (OsString::from("USERPROFILE"), home_env.clone()),
        ]
    );
    assert_eq!(
        own_env(env_source(&[
            (OsString::from("CODEX_HOME"), own.clone().into_os_string()),
            (OsString::from("HOME"), home_env.clone()),
            (OsString::from("USERPROFILE"), home_env.clone()),
        ])),
        vec![
            (OsString::from("CODEX_HOME"), own.into_os_string()),
            (OsString::from("HOME"), home_env.clone()),
            (OsString::from("USERPROFILE"), home_env),
        ]
    );
}

/// The inherited-env fallback must fail loudly with no `HomeSandbox` held,
/// instead of reading the operator's real `$HOME` and planning/moving the real
/// `~/.shunt`. The fence is the source [`inherited_env`] passes
/// (`inherited_var`), whose HOME/USERPROFILE resolve through
/// `profile::home_dir` — which panics with no override held in every leg (the
/// `HOME_OVERRIDE` guard is process-wide, so it also fires on a worker thread).
/// The test holds `HOME_TEST_LOCK` (every `HomeSandbox` holds it for its whole
/// lifetime) so the process-global override is empty for its duration: a
/// sibling test's sandbox would otherwise make `home_dir` return that sandbox
/// instead of panicking.
#[test]
#[should_panic(expected = "HomeSandbox")]
fn the_inherited_env_fallback_panics_with_no_sandbox_held() {
    let _serial = crate::profile::HOME_TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    super::own_env(super::inherited_var);
}

/// Planning then moving an unchanged tree moves exactly the planned files.
#[test]
fn plan_then_move_moves_exactly_the_planned_files() {
    let home = HomeSandbox::new();
    let src = home.home().join(".shunt");
    let dst = home.home().join(".clauth").join("shunt");
    let claude = src.join("accounts").join("claude");
    write(&claude.join("main.json"), "claude-main");
    write(&src.join("xai-auth.json"), "xai");
    let record = adopted(&home);
    let inherited: Vec<(OsString, OsString)> = sandbox_home().to_vec();

    let plan = plan_standalone_stores_in(
        &record,
        inherited.iter().cloned(),
        CodexHomeSource::Inherited,
    )
    .expect("plan");
    let result = move_standalone_stores_in(
        &record,
        GatewaySilent::for_test(),
        &plan,
        inherited.iter().cloned(),
        CodexHomeSource::Inherited,
    )
    .expect("move");

    assert_eq!(result.moved, plan.moved);
    assert_eq!(result.kept, plan.kept);
    assert_eq!(
        fs::read_to_string(dst.join("accounts").join("claude").join("main.json")).expect("moved"),
        "claude-main"
    );
    assert_eq!(
        fs::read_to_string(dst.join("xai-auth.json")).expect("moved"),
        "xai"
    );
    assert!(!claude.join("main.json").exists());
    assert!(!src.join("xai-auth.json").exists());
}

/// A file added to a store after the plan refuses the move with the new
/// refusal, moving nothing: every source's bytes are unchanged and the
/// managed root was never created.
#[test]
fn the_move_refuses_when_a_file_was_added_to_a_store_since_the_plan() {
    let home = HomeSandbox::new();
    let src = home.home().join(".shunt");
    let dst = home.home().join(".clauth").join("shunt");
    let claude = src.join("accounts").join("claude");
    write(&claude.join("main.json"), "claude-main");
    write(&src.join("xai-auth.json"), "xai");
    let record = adopted(&home);
    let inherited: Vec<(OsString, OsString)> = sandbox_home().to_vec();

    let plan = plan_standalone_stores_in(
        &record,
        inherited.iter().cloned(),
        CodexHomeSource::Inherited,
    )
    .expect("plan");
    write(&claude.join("extra.json"), "claude-extra");
    let main = claude.join("main.json");
    let xai = src.join("xai-auth.json");
    let extra = claude.join("extra.json");
    let before = [sha256(&main), sha256(&xai), sha256(&extra)];

    let err = move_standalone_stores_in(
        &record,
        GatewaySilent::for_test(),
        &plan,
        inherited.iter().cloned(),
        CodexHomeSource::Inherited,
    )
    .expect_err("the tree changed since the plan");
    assert_eq!(
        err.to_string(),
        "the stores changed since the plan was shown; look at the plan again"
    );
    assert!(matches!(
        err.downcast_ref::<StoreMoveRefusal>(),
        Some(StoreMoveRefusal::PlanChanged)
    ));

    assert_eq!(
        [sha256(&main), sha256(&xai), sha256(&extra)],
        before,
        "no source changed"
    );
    assert!(!dst.exists());
}

/// A daemon holding the singleton with no record naming it (missing, stale,
/// unreadable or unparseable) refuses the plan and the move, moving nothing,
/// until that daemon restarts and records its env.
#[test]
fn a_daemon_holding_the_singleton_with_no_record_refuses_the_plan_and_move() {
    let home = HomeSandbox::new();
    let record = adopted(&home);
    let _held = crate::daemon::hold_daemon_lock();
    std::fs::write(
        home.home().join(".clauth").join("clauthd.pid"),
        format!("{}\n", std::process::id()),
    )
    .expect("stamp the holder pid");
    // No gateway-env.json written.

    let err = plan_standalone_stores(&record).expect_err("a daemon with no record refuses");
    assert_eq!(
        err.to_string(),
        "the running daemon recorded no environment; restart it, then look at the plan again"
    );
    assert!(matches!(
        err.downcast_ref::<StoreMoveRefusal>(),
        Some(StoreMoveRefusal::DaemonEnvUnrecorded)
    ));

    let empty = StoreMovePlan {
        moved: Vec::new(),
        kept: Vec::new(),
    };
    let err = move_standalone_stores(&record, GatewaySilent::for_test(), &empty)
        .expect_err("the move refuses too");
    assert_eq!(
        err.to_string(),
        "the running daemon recorded no environment; restart it, then look at the plan again"
    );
    assert!(!home.home().join(".clauth").join("shunt").exists());
}

/// A relative `CODEX_HOME` in the running daemon's record refuses with the
/// daemon copy, naming the daemon as the place to fix it: the env-file and
/// clauth-own copies stay theirs.
#[test]
fn a_daemon_whose_recorded_codex_home_is_relative_refuses_with_the_daemon_copy() {
    let home = HomeSandbox::new();
    let record = adopted(&home);
    let _held = crate::daemon::hold_daemon_lock();
    write_env_record_naming_self(&home, Some(Path::new("rel")), None);

    let err = plan_standalone_stores(&record).expect_err("a relative daemon CODEX_HOME refuses");
    assert_eq!(
        err.to_string(),
        "CODEX_HOME in the running daemon's environment is a relative path; restart the daemon with an absolute CODEX_HOME"
    );
    assert!(matches!(
        err.downcast_ref::<StoreMoveRefusal>(),
        Some(StoreMoveRefusal::RelativeCodexHome {
            source: CodexHomeSource::DaemonRecord
        })
    ));
}

/// A kept-only change between plan and move (a non-account file appearing in a
/// dir store) refuses with `PlanChanged`, moving nothing, so the confirmed
/// "N stay behind" count cannot go stale.
#[test]
fn the_move_refuses_when_only_a_kept_entry_changes_since_the_plan() {
    let home = HomeSandbox::new();
    let src = home.home().join(".shunt");
    let dst = home.home().join(".clauth").join("shunt");
    let claude = src.join("accounts").join("claude");
    write(&claude.join("main.json"), "claude-main");
    write(&src.join("xai-auth.json"), "xai");
    let record = adopted(&home);
    let inherited: Vec<(OsString, OsString)> = sandbox_home().to_vec();

    let plan = plan_standalone_stores_in(
        &record,
        inherited.iter().cloned(),
        CodexHomeSource::Inherited,
    )
    .expect("plan");
    let notes = claude.join("notes.txt");
    write(&notes, "not-a-store-entry");
    let main = claude.join("main.json");
    let xai = src.join("xai-auth.json");
    let before = [sha256(&main), sha256(&xai), sha256(&notes)];

    let err = move_standalone_stores_in(
        &record,
        GatewaySilent::for_test(),
        &plan,
        inherited.iter().cloned(),
        CodexHomeSource::Inherited,
    )
    .expect_err("the kept list changed since the plan");
    assert_eq!(
        err.to_string(),
        "the stores changed since the plan was shown; look at the plan again"
    );
    assert!(matches!(
        err.downcast_ref::<StoreMoveRefusal>(),
        Some(StoreMoveRefusal::PlanChanged)
    ));
    assert_eq!(
        [sha256(&main), sha256(&xai), sha256(&notes)],
        before,
        "no source changed"
    );
    assert!(!dst.exists());
}

/// A non-UTF-8 env value survives the record losslessly on unix, never a lossy
/// U+FFFD path: the round trip is against a hand-built pair, not a value the
/// codec under test computed.
#[cfg(unix)]
#[test]
fn a_non_utf8_env_value_round_trips_through_the_record_losslessly() {
    use std::os::unix::ffi::OsStrExt as _;
    let _sandbox = HomeSandbox::new();
    let value = OsStr::from_bytes(b"/tmp/\xff");
    write_env_record(DaemonEnvRecord {
        identity: DaemonIdentity {
            pid: 1,
            start: Some("s".to_string()),
        },
        env: vec![("HOME".to_string(), env_value_bytes(value))],
        auth_env: Vec::new(),
        tokens: Vec::new(),
    })
    .expect("write");
    let record = read_env_record().expect("read").expect("present");
    assert_eq!(
        recorded_pairs(&record.env).expect("a test record decodes"),
        vec![(OsString::from("HOME"), OsString::from(value))]
    );
}

/// On Windows an odd byte length in a recorded value is a corrupt record,
/// refused through the reader with the approved refusal, never silently
/// truncated to whole UTF-16 units.
#[cfg(windows)]
#[test]
fn an_odd_length_env_value_is_refused_not_truncated() {
    let home = HomeSandbox::new();
    let _held = crate::daemon::hold_daemon_lock();
    std::fs::write(
        home.home().join(".clauth").join("clauthd.pid"),
        format!("{}\n", std::process::id()),
    )
    .expect("stamp the holder pid");
    let pid = std::process::id();
    write_env_record(DaemonEnvRecord {
        identity: DaemonIdentity {
            pid,
            start: Some(crate::daemon::gateway::process_start_time(pid).expect("start")),
        },
        // One byte is an odd length, which the Windows decoder refuses.
        env: vec![("CODEX_HOME".to_string(), vec![0x61])],
        auth_env: Vec::new(),
        tokens: Vec::new(),
    })
    .expect("write");

    assert_eq!(
        recorded_env_for_live_daemon()
            .expect_err("an odd-length value refuses")
            .to_string(),
        "the running daemon recorded no environment; restart it, then look at the plan again"
    );
}

/// The `~/.codex` guard follows the same resolved home the store defaults use
/// (the daemon's recorded `HOME`), not clauth's own home: a login at
/// `<daemon HOME>/.codex/auth.json` stays.
#[test]
fn the_default_codex_home_follows_the_resolved_home() {
    let home = HomeSandbox::new();
    let daemon_home = home.home().join("daemon-home");
    let login = daemon_home.join(".codex").join("auth.json");
    write(&login, "codex-cli-login");
    let record = with_env_file(&home, &format!("CODEX_AUTH_FILE={}\n", quoted(&login)));

    let _held = crate::daemon::hold_daemon_lock();
    write_env_record_naming_self(&home, None, Some(&daemon_home));

    let plan = plan_standalone_stores(&record).expect("plan");
    assert_eq!(plan.moved, Vec::<MovedFile>::new());
    assert_eq!(
        plan.kept,
        vec![KeptFile::At {
            path: login.clone(),
            reason: KeptReason::CodexLogin,
        }],
        "the login at the daemon's ~/.codex is kept"
    );
}

/// The `~/.codex` guard refuses a relative resolved home with its own refusal,
/// naming the codex CLI's login rather than a store key, instead of silently
/// stopping matching.
#[test]
fn the_default_codex_home_refuses_a_relative_home() {
    let home = HomeSandbox::new();
    let env_file = home.home().join("tokens.env");
    write(&env_file, "HOME=rel-home\n");
    let named = read_env_file(&env_file).expect("parse");
    let err = default_codex_home(&named, &[]).expect_err("a relative home refuses");
    assert!(matches!(
        err.downcast_ref::<StoreMoveRefusal>(),
        Some(StoreMoveRefusal::RelativeCodexLoginHome)
    ));
}

/// A store default under a relative home refuses by the store's own key. The
/// codex guard fires first in the plan, so only a direct call reaches this
/// arm; it stays pinned so a reorder of the plan cannot drop it unseen.
#[test]
fn a_store_default_under_a_relative_home_refuses_by_its_key() {
    let home = HomeSandbox::new();
    let env_file = home.home().join("tokens.env");
    write(&env_file, "HOME=rel-home\n");
    let named = read_env_file(&env_file).expect("parse");
    for key in ["SHUNT_CLAUDE_ACCOUNTS_DIR", "SHUNT_XAI_AUTH_FILE"] {
        let store = STORES.iter().find(|s| s.env == key).expect("store");
        let err = default_store_path(store, &named, &[]).expect_err("a relative home refuses");
        assert_eq!(
            err.to_string(),
            format!(
                "the standalone's home names no absolute directory, so shunt's default for {key} is relative to the standalone's working directory, which clauth cannot tell; nothing was moved; set HOME to an absolute path in the env file, then run the move again"
            )
        );
    }
}

/// An unreadable holder renders its cause chain once under `{:#}`: the
/// wrapper's head is the inner error's head, and its source is the inner
/// error's source, never the inner error itself.
#[test]
fn an_unreadable_holder_renders_its_chain_once() {
    let err = anyhow::Error::from(NoHolder::Unreadable(anyhow::anyhow!("x").context("y")));
    assert_eq!(format!("{err:#}"), "y: x");
}

/// The codex CLI's own login under clauth's own home is guarded even when the
/// env file (or the daemon's record) names another `HOME`: the `~/.codex`
/// guard keeps clauth's own home unconditionally, and the resolved home's
/// `.codex` is a second guard, never a replacement.
#[test]
fn the_codex_login_in_clauths_own_home_stays_when_the_env_file_names_another_home() {
    let home = HomeSandbox::new();
    let svc_home = home.home().join("svc-home");
    let login = home.home().join(".codex").join("auth.json");
    write(&login, "codex-cli-login");
    let record = with_env_file(
        &home,
        &format!(
            "HOME={}\nCODEX_AUTH_FILE={}\n",
            quoted(&svc_home),
            quoted(&login)
        ),
    );

    let _held = crate::daemon::hold_daemon_lock();
    write_env_record_naming_self(&home, None, Some(&svc_home));

    let plan = plan_standalone_stores(&record).expect("plan");
    assert_eq!(plan.moved, Vec::<MovedFile>::new());
    assert_eq!(
        plan.kept,
        vec![KeptFile::At {
            path: login.clone(),
            reason: KeptReason::CodexLogin,
        }],
        "the codex CLI's own login under clauth's home is kept"
    );
    assert_eq!(fs::read_to_string(&login).expect("kept"), "codex-cli-login");
}

/// The record carries `USERPROFILE` through to the store defaults: a daemon
/// whose record holds only `USERPROFILE` (no `HOME`) still resolves a
/// `HomeThenUserProfile` store's default.
#[test]
fn a_record_with_userprofile_resolves_a_store_default() {
    let home = HomeSandbox::new();
    let winhome = home.home().join("winhome");
    let win_cursor = winhome.join(".shunt").join("cursor-auth.json");
    write(&win_cursor, "cursor-win");
    let record = adopted(&home);

    let _held = crate::daemon::hold_daemon_lock();
    std::fs::write(
        home.home().join(".clauth").join("clauthd.pid"),
        format!("{}\n", std::process::id()),
    )
    .expect("stamp the holder pid");
    let pid = std::process::id();
    write_env_record(DaemonEnvRecord {
        identity: DaemonIdentity {
            pid,
            start: Some(crate::daemon::gateway::process_start_time(pid).expect("start")),
        },
        env: vec![(
            "USERPROFILE".to_string(),
            env_value_bytes(winhome.as_os_str()),
        )],
        auth_env: Vec::new(),
        tokens: Vec::new(),
    })
    .expect("write");

    let plan = plan_standalone_stores(&record).expect("plan");
    assert_eq!(
        plan.moved,
        vec![MovedFile {
            from: win_cursor.clone(),
            to: home
                .home()
                .join(".clauth")
                .join("shunt")
                .join("cursor-auth.json"),
        }]
    );
}

/// The card's probe function reads the same inherited env the plan and move
/// use, so a `SHUNT_SERVER__BIND` only in the daemon's record (not the
/// caller's shell) still names the probe the move checks.
#[test]
fn the_gateway_probe_reads_the_same_env_as_the_plan() {
    let home = HomeSandbox::new();
    let record = adopted(&home);
    let _held = crate::daemon::hold_daemon_lock();
    std::fs::write(
        home.home().join(".clauth").join("clauthd.pid"),
        format!("{}\n", std::process::id()),
    )
    .expect("stamp the holder pid");
    let pid = std::process::id();
    write_env_record(DaemonEnvRecord {
        identity: DaemonIdentity {
            pid,
            start: Some(crate::daemon::gateway::process_start_time(pid).expect("start")),
        },
        env: vec![
            ("HOME".to_string(), env_value_bytes(home.home().as_os_str())),
            (
                "SHUNT_SERVER__BIND".to_string(),
                env_value_bytes(OsStr::new("127.0.0.1:3999")),
            ),
        ],
        auth_env: Vec::new(),
        tokens: Vec::new(),
    })
    .expect("write");

    assert_eq!(
        gateway_probe(&record).expect("probe"),
        addr("127.0.0.1:3999")
    );
}

// ── chatgpt_oauth providers needing a pool login ───────────────────────────

/// A test token long enough for shunt to accept (>= 32 characters).
const POOL_TOKEN: &str = "test-admin-token-0123456789abcdef0123456789abcdef";

/// A pool body whose `chatgpt_oauth` providers A and D hold no accounts, B
/// holds one and C is `claude_oauth`; the extra fields exercise the parse's
/// tolerance of everything clauth does not read.
const POOL_BODY: &str = r#"{"providers":[
  {"provider":"A","auth":"chatgpt_oauth","accounts":[]},
  {"provider":"B","auth":"chatgpt_oauth","accounts":[{"name":"mine","plan":"plus"}]},
  {"provider":"C","auth":"claude_oauth","accounts":[]},
  {"provider":"D","auth":"chatgpt_oauth","accounts":[]}
]}"#;

/// A record whose config binds the gateway at `addr`.
fn pool_record(home: &HomeSandbox, addr: SocketAddr) -> GatewayRecord {
    let config = home.home().join("etc").join("shunt.toml");
    write(&config, &format!("[server]\nbind = \"{addr}\"\n"));
    GatewayRecord::new(config).expect("adoptable")
}

/// The admin token alone in the sandbox's `~/.clauth/gateway-admin-token`.
fn write_pool_token(home: &HomeSandbox, token: &str) {
    write(
        &home.home().join(".clauth").join("gateway-admin-token"),
        token,
    );
}

/// The pinned `CODEX_AUTH_FILE` under the sandbox's `~/.clauth/shunt`.
fn pinned_codex_auth_file(home: &HomeSandbox) -> PathBuf {
    home.home()
        .join(".clauth")
        .join("shunt")
        .join("codex-auth.json")
}

/// A bound-then-dropped listener's address: a port nothing answers.
fn closed_port() -> SocketAddr {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind");
    let addr = listener.local_addr().expect("addr");
    drop(listener);
    addr
}

#[test]
fn the_pool_login_list_names_every_loginless_chatgpt_oauth_provider() {
    let home = HomeSandbox::new();
    write_pool_token(&home, POOL_TOKEN);
    let (base, seen) = serve_endpoints(1, |_, _| (200, POOL_BODY.to_string()));
    let record = pool_record(&home, listener_addr(&base));
    assert_eq!(
        chatgpt_oauth_providers_needing_login(&record).expect("providers"),
        vec!["A".to_string(), "D".to_string()]
    );
    seen.join().expect("listener");
}

#[test]
fn the_pool_read_sends_the_token_and_hits_the_pool_route() {
    let home = HomeSandbox::new();
    write_pool_token(&home, POOL_TOKEN);
    let (base, seen) = serve_endpoints_raw(1, |_, _| (200, POOL_BODY.to_string()));
    let record = pool_record(&home, listener_addr(&base));
    chatgpt_oauth_providers_needing_login(&record).expect("providers");
    let raw = seen.join().expect("listener");
    assert_eq!(request_path(&raw[0]), "/admin/api/pool");
    assert_eq!(
        request_header(&raw[0], "x-api-key").as_deref(),
        Some(POOL_TOKEN)
    );
}

#[test]
fn the_pinned_single_file_short_circuits_the_pool_read() {
    let home = HomeSandbox::new();
    write_pool_token(&home, POOL_TOKEN);
    write(&pinned_codex_auth_file(&home), "{}");
    // The bind points at a port nothing answers: a short-circuit never dials.
    let record = pool_record(&home, closed_port());
    assert!(
        chatgpt_oauth_providers_needing_login(&record)
            .expect("empty")
            .is_empty()
    );
}

#[test]
fn a_directory_at_the_pinned_path_still_reads_the_pool() {
    let home = HomeSandbox::new();
    write_pool_token(&home, POOL_TOKEN);
    fs::create_dir_all(pinned_codex_auth_file(&home)).expect("dir");
    let (base, seen) = serve_endpoints(1, |_, _| (200, POOL_BODY.to_string()));
    let record = pool_record(&home, listener_addr(&base));
    assert_eq!(
        chatgpt_oauth_providers_needing_login(&record).expect("providers"),
        vec!["A".to_string(), "D".to_string()]
    );
    seen.join().expect("listener");
}

#[test]
fn a_missing_admin_token_is_the_typed_error_and_nothing_is_written() {
    let home = HomeSandbox::new();
    let record = pool_record(&home, closed_port());
    let err = chatgpt_oauth_providers_needing_login(&record).expect_err("no token");
    let typed = err
        .downcast_ref::<ChatgptOauthProvidersError>()
        .expect("a typed ChatgptOauthProvidersError");
    assert!(
        matches!(typed, ChatgptOauthProvidersError::NoAdminToken),
        "a missing token file is the NoAdminToken variant, got {typed:?}"
    );
    assert_eq!(format!("{err:#}"), "no gateway admin token");
    assert!(
        !home
            .home()
            .join(".clauth")
            .join("gateway-admin-token")
            .exists(),
        "the pool read must never mint a token"
    );
}

#[test]
fn a_refused_pool_read_is_the_unreachable_typed_error() {
    let home = HomeSandbox::new();
    write_pool_token(&home, POOL_TOKEN);
    let addr = closed_port();
    let record = pool_record(&home, addr);
    let err = chatgpt_oauth_providers_needing_login(&record).expect_err("unreachable");
    let typed = err
        .downcast_ref::<ChatgptOauthProvidersError>()
        .expect("a typed ChatgptOauthProvidersError");
    assert!(
        matches!(typed, ChatgptOauthProvidersError::Unreachable { addr: refused } if *refused == addr),
        "a refused connection is the Unreachable variant, got {typed:?}"
    );
    assert_eq!(
        format!("{err:#}"),
        format!("the gateway at {addr} did not answer")
    );
}

#[test]
fn each_pool_status_is_its_own_typed_error_without_the_body() {
    let home = HomeSandbox::new();
    for (status, message) in [
        (
            401u16,
            "the gateway did not accept clauth's admin token (status 401)",
        ),
        (
            403u16,
            "the gateway did not accept clauth's admin token (status 403)",
        ),
        (404u16, "the gateway has no admin pool route (status 404)"),
        (
            500u16,
            "the gateway could not read its pool (status 500); its log names the provider",
        ),
        (502u16, "the gateway's pool answered status 502"),
    ] {
        write_pool_token(&home, POOL_TOKEN);
        let marker = format!("marker-{status}-body");
        let (base, seen) = serve_endpoints(1, move |_, _| (status, marker.clone()));
        let record = pool_record(&home, listener_addr(&base));
        let err = chatgpt_oauth_providers_needing_login(&record).expect_err("typed");
        let text = format!("{err:#}");
        assert_eq!(text, message, "status {status}");
        assert!(
            !text.contains("marker"),
            "the body never reaches the error: {text}"
        );
        assert!(
            !text.contains(POOL_TOKEN),
            "the token never reaches the error: {text}"
        );
        seen.join().expect("listener");
    }
}

#[test]
fn a_non_json_pool_body_is_its_own_typed_error() {
    let home = HomeSandbox::new();
    write_pool_token(&home, POOL_TOKEN);
    let (base, seen) = serve_endpoints(1, |_, _| (200, "not json".to_string()));
    let record = pool_record(&home, listener_addr(&base));
    let err = chatgpt_oauth_providers_needing_login(&record).expect_err("unparseable");
    let typed = err
        .downcast_ref::<ChatgptOauthProvidersError>()
        .expect("a typed ChatgptOauthProvidersError");
    assert!(
        matches!(typed, ChatgptOauthProvidersError::Unparseable),
        "a non-JSON body is the Unparseable variant, got {typed:?}"
    );
    assert_eq!(
        format!("{err:#}"),
        "the gateway's pool response does not parse"
    );
    seen.join().expect("listener");
}

#[test]
fn an_oversized_pool_body_is_its_own_typed_error() {
    let home = HomeSandbox::new();
    write_pool_token(&home, POOL_TOKEN);
    let body = "x".repeat((POOL_BODY_LIMIT + 1) as usize);
    let (base, seen) = serve_endpoints(1, move |_, _| (200, body.clone()));
    let record = pool_record(&home, listener_addr(&base));
    let err = chatgpt_oauth_providers_needing_login(&record).expect_err("too large");
    let typed = err
        .downcast_ref::<ChatgptOauthProvidersError>()
        .expect("a typed ChatgptOauthProvidersError");
    assert!(
        matches!(typed, ChatgptOauthProvidersError::TooLarge { limit } if *limit == POOL_BODY_LIMIT),
        "an over-cap body is the TooLarge variant, got {typed:?}"
    );
    assert_eq!(
        format!("{err:#}"),
        format!("the gateway's pool response exceeds {POOL_BODY_LIMIT} bytes")
    );
    seen.join().expect("listener");
}

#[test]
fn an_unknown_auth_mode_is_ignored_and_the_parse_succeeds() {
    let home = HomeSandbox::new();
    write_pool_token(&home, POOL_TOKEN);
    let body = r#"{"providers":[
      {"provider":"A","auth":"future_oauth","accounts":[]},
      {"provider":"B","auth":"chatgpt_oauth","accounts":[]}
    ]}"#;
    let (base, seen) = serve_endpoints(1, |_, _| (200, body.to_string()));
    let record = pool_record(&home, listener_addr(&base));
    assert_eq!(
        chatgpt_oauth_providers_needing_login(&record).expect("providers"),
        vec!["B".to_string()]
    );
    seen.join().expect("listener");
}

/// JSON of another shape is the same typed error as a body that is not JSON.
#[test]
fn a_pool_body_of_another_shape_is_the_unparseable_typed_error() {
    let home = HomeSandbox::new();
    write_pool_token(&home, POOL_TOKEN);
    let (base, seen) = serve_endpoints(1, |_, _| (200, r#"{"accounts":[]}"#.to_string()));
    let record = pool_record(&home, listener_addr(&base));
    let err = chatgpt_oauth_providers_needing_login(&record).expect_err("unparseable");
    assert_eq!(
        format!("{err:#}"),
        "the gateway's pool response does not parse"
    );
    seen.join().expect("listener");
}

/// A 200 whose body stops before its declared length is its own typed error,
/// never a transport failure's "did not answer", and carries none of the body.
#[test]
fn a_pool_body_that_breaks_off_is_its_own_typed_error() {
    let home = HomeSandbox::new();
    write_pool_token(&home, POOL_TOKEN);
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind");
    let addr = listener.local_addr().expect("addr");
    listener
        .set_nonblocking(true)
        .expect("nonblocking listener");
    // A nonblocking accept: a read that returns without dialing ends the wait
    // at once, so the test reds instead of parking on a blocking `accept`.
    // The flag is read BEFORE each `accept`: a dial completes before the read
    // returns, so once the flag reads set, any dialed connection is already
    // queued for that `accept`.
    let read_returned = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let returned = std::sync::Arc::clone(&read_returned);
    let server = std::thread::spawn(move || {
        use std::io::{Read as _, Write as _};
        let mut stream = loop {
            let read_done = returned.load(std::sync::atomic::Ordering::SeqCst);
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    if read_done {
                        return false;
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(e) => panic!("accept: {e}"),
            }
        };
        stream.set_nonblocking(false).expect("blocking stream");
        let mut request = [0u8; 4096];
        let _ = stream.read(&mut request).expect("request");
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\n{\"marker-broken")
            .expect("head");
        true
    });
    let record = pool_record(&home, addr);
    let err = chatgpt_oauth_providers_needing_login(&record).expect_err("broken");
    read_returned.store(true, std::sync::atomic::Ordering::SeqCst);
    assert!(server.join().expect("server"), "the pool read never dialed");
    let typed = err
        .downcast_ref::<ChatgptOauthProvidersError>()
        .expect("a typed ChatgptOauthProvidersError");
    assert!(
        matches!(typed, ChatgptOauthProvidersError::BodyBroken),
        "a body that breaks off is the BodyBroken variant, got {typed:?}"
    );
    assert_eq!(format!("{err:#}"), "the gateway's pool response broke off");
}

/// A live daemon with no env record naming it: the gateway's address cannot be
/// resolved the way the daemon spawned it, so the pool read refuses with its
/// own typed error, never the store move's copy.
#[test]
fn a_daemon_with_no_env_record_is_the_pool_reads_own_typed_error() {
    let home = HomeSandbox::new();
    write_pool_token(&home, POOL_TOKEN);
    let record = pool_record(&home, closed_port());
    let _held = crate::daemon::hold_daemon_lock();
    let err = chatgpt_oauth_providers_needing_login(&record).expect_err("unrecorded");
    let typed = err
        .downcast_ref::<ChatgptOauthProvidersError>()
        .expect("a typed ChatgptOauthProvidersError");
    assert!(
        matches!(typed, ChatgptOauthProvidersError::DaemonEnvUnrecorded),
        "an unrecorded daemon is the DaemonEnvUnrecorded variant, got {typed:?}"
    );
    assert_eq!(
        format!("{err:#}"),
        "the running daemon recorded no environment; restart it"
    );
}

/// Both gateway clients ask their target directly with every phase bounded:
/// a proxy the builder arrived with (as ureq's default takes one from the
/// env) is dropped, no redirect is followed, and a wedged gateway cannot park
/// the caller. Health: 4 s connect + 2 s response = 6 s; pool: 4 s + 20 s = 24 s.
/// The real agents are built under an env proxy, so a constructor that
/// re-reads the env after `direct_config` shows one.
#[test]
fn the_gateway_clients_go_direct_and_bound_every_phase() {
    let home = HomeSandbox::new();
    // `NO_PROXY=*` keeps the pin inert for a test outside the sandbox lock
    // whose lazily built shared agent reads the env while the pin stands: its
    // proxy then excludes every host.
    let _proxy = EnvPin::new(
        &home,
        &[
            ("ALL_PROXY", Some(OsStr::new("http://127.0.0.1:9"))),
            ("NO_PROXY", Some(OsStr::new("*"))),
        ],
    );
    for (response, global) in [(2u64, 6u64), (20, 24)] {
        let preset = ureq::Agent::config_builder()
            .proxy(Some(ureq::Proxy::new("http://127.0.0.1:9").expect("proxy")));
        let agent: ureq::Agent = direct_config(preset, Duration::from_secs(response))
            .build()
            .into();
        let config = agent.config();
        assert!(config.proxy().is_none(), "response {response}: no proxy");
        assert_eq!(config.max_redirects(), 0, "response {response}");
        let timeouts = config.timeouts();
        assert_eq!(timeouts.connect, Some(Duration::from_secs(4)));
        assert_eq!(timeouts.recv_response, Some(Duration::from_secs(response)));
        assert_eq!(timeouts.recv_body, Some(Duration::from_secs(response)));
        assert_eq!(timeouts.global, Some(Duration::from_secs(global)));
    }
    for (agent, global) in [(health_agent(), 6u64), (pool_agent(), 24)] {
        assert_eq!(
            agent.config().timeouts().global,
            Some(Duration::from_secs(global))
        );
        assert!(agent.config().proxy().is_none());
    }
    // The daemon derives its probe deadlines from the constant, so the built
    // bound and the constant must stay one number.
    assert_eq!(
        health_agent().config().timeouts().global,
        Some(HEALTH_PROBE_TIMEOUT)
    );
}
