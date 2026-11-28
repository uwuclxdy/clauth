//! The Services → herdr `held question dot` row: where it sits in the
//! options walk on each OS, the flip that persists it and runs the pass at
//! once, and the Services tab refresh that rebuilds clauth's file on a moved
//! herdr cache. The pass itself is pinned in `tests/inline/herdr_detect.rs`;
//! these drive it through the TUI's own seams against the same herdr shim.

use super::{HERDR_OPTIONS, HerdrOption};

/// Every row's label in walk order, off Windows.
#[cfg(not(windows))]
#[test]
fn the_row_follows_delegate_row_text_and_restore_notice_stays_last() {
    let labels: Vec<&str> = HERDR_OPTIONS.iter().map(|row| row.label()).collect();
    assert_eq!(
        labels,
        vec![
            "popup width",
            "pane tag",
            "tag refresh",
            "border label",
            "delegate dot",
            "delegate row text",
            "held question dot",
            "restore notice",
        ]
    );
    assert_eq!(HERDR_OPTIONS.get(6), Some(&HerdrOption::HeldQuestionDot));
}

/// On Windows the row is absent from the list, so from the walk too.
#[cfg(windows)]
#[test]
fn the_row_is_absent_on_windows() {
    let labels: Vec<&str> = HERDR_OPTIONS.iter().map(|row| row.label()).collect();
    assert_eq!(
        labels,
        vec![
            "popup width",
            "pane tag",
            "tag refresh",
            "border label",
            "delegate dot",
            "delegate row text",
            "restore notice",
        ]
    );
    assert_eq!(HERDR_OPTIONS.last(), Some(&HerdrOption::RestoreNotice));
}

#[cfg(not(windows))]
mod flip {
    use std::path::{Path, PathBuf};

    use crate::herdr_detect::tests::{BLOCK, CACHE_V1, CACHE_V2, HEADER, ONE_SERVER, SHIM};
    use crate::profile::{AppConfig, AppState};
    use crate::testutil::{EnvPin, HomeSandbox, write_shim};
    use crate::tui::app::{
        App, HERDR_OPTIONS, HerdrOption, ShuntRefresh, Tab, drain_service_probes, handle_key,
        join_test_workers, recompute_services_checks,
    };
    use ratatui::crossterm::event::KeyCode;

    /// The shim's dir, with `HERDR_BIN_PATH` and `XDG_STATE_HOME` pinned at
    /// the sandbox.
    struct Herdr<'a> {
        dir: PathBuf,
        state: PathBuf,
        _env: EnvPin<'a>,
    }

    impl Herdr<'_> {
        fn override_path(&self) -> PathBuf {
            self.dir.join("herdr/agent-detection/claude.toml")
        }

        fn write_cache(&self, text: &str) {
            let path = self.state.join("herdr/agent-detection/remote/claude.toml");
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }

        fn log(&self) -> String {
            std::fs::read_to_string(self.dir.join("herdr.log")).unwrap_or_default()
        }

        fn clear_log(&self) {
            let log = self.dir.join("herdr.log");
            if log.exists() {
                std::fs::remove_file(log).unwrap();
            }
        }

        fn reloads(&self) -> usize {
            self.log().matches("server reload-agent-manifests").count()
        }
    }

    fn shimmed(home: &HomeSandbox) -> Herdr<'_> {
        let dir = home.home().join("shim");
        std::fs::create_dir_all(&dir).unwrap();
        // The shim's own file must not sit where herdr's config root does.
        let bin = write_shim(&dir, "herdr-shim", SHIM);
        std::fs::write(dir.join("sessions.json"), ONE_SERVER).unwrap();
        let state = home.home().join("xdg-state");
        let env = EnvPin::new(
            home,
            &[
                ("HERDR_BIN_PATH", Some(bin.as_os_str())),
                ("XDG_STATE_HOME", Some(state.as_os_str())),
            ],
        );
        Herdr {
            dir,
            state,
            _env: env,
        }
    }

    /// The herdr options section, descended, its pass worker running the real
    /// pass.
    fn options_app() -> App {
        let mut app = App::new(AppConfig {
            state: AppState::default(),
            profiles: Vec::new(),
        });
        app.tab = Tab::Services;
        let probe = crate::herdr::HerdrProbe {
            version: Some("0.9.5".to_string()),
            entry: None,
            config_path: None,
            error: None,
        };
        app.services.checks = vec![crate::tui::app::herdr_check(&probe, None)];
        app.services.herdr = Some(Some(probe));
        app.services.cursor = 0;
        app.services.focus = crate::tui::app::ServicesFocus::Detail;
        app.services.herdr_detect_pass.prober = Some(crate::herdr_detect::run_now);
        app
    }

    fn held_question_dot() -> bool {
        crate::profile::load_config()
            .expect("load persisted knobs")
            .state
            .herdr
            .held_question_dot
    }

    fn profiles_toml(home: &Path) -> String {
        std::fs::read_to_string(home.join(".clauth/profiles.toml")).unwrap_or_default()
    }

    /// The flip persists `[herdr] held_question_dot` at once and runs the
    /// pass on the flip itself: no tick, no refresh in between.
    #[test]
    fn the_flip_persists_the_knob_and_writes_herdrs_override_at_once() {
        let home = HomeSandbox::new();
        let herdr = shimmed(&home);
        herdr.write_cache(CACHE_V1);
        crate::profile::save_app_state(&AppState::default()).unwrap();
        assert!(
            !profiles_toml(home.home()).contains("held_question_dot"),
            "a profiles file without the key"
        );
        assert!(!held_question_dot(), "defaults off");

        let mut app = options_app();
        app.services.herdr_options_cursor = HERDR_OPTIONS
            .iter()
            .position(|row| *row == HerdrOption::HeldQuestionDot)
            .unwrap();
        handle_key(&mut app, crate::testutil::key(KeyCode::Char(' ')));
        let saved: toml::Table = profiles_toml(home.home()).parse().unwrap();
        assert_eq!(
            saved
                .get("herdr")
                .and_then(|herdr| herdr.get("held_question_dot"))
                .and_then(toml::Value::as_bool),
            Some(true),
            "persisted under [herdr]:\n{}",
            profiles_toml(home.home())
        );
        assert!(held_question_dot());

        join_test_workers();
        drain_service_probes(&mut app);
        assert_eq!(
            std::fs::read_to_string(herdr.override_path()).unwrap(),
            format!("{HEADER}{CACHE_V1}{BLOCK}")
        );
        assert_eq!(herdr.reloads(), 1);
        assert_eq!(
            app.services.herdr_detection,
            vec!["detection: herdr manifest 2026.10.11.1 + clauth rule".to_string()]
        );

        handle_key(&mut app, crate::testutil::key(KeyCode::Char(' ')));
        assert!(!held_question_dot());
        join_test_workers();
        drain_service_probes(&mut app);
        assert!(!herdr.override_path().exists(), "off removes clauth's file");
        assert_eq!(herdr.reloads(), 2);
        assert_eq!(app.services.herdr_detection, Vec::<String>::new());
    }

    /// Toggle on with nothing applied: no cached manifest and no override, so
    /// the card says what it waits for and nothing is written; once herdr
    /// caches a manifest, the next refresh applies the rule and the card
    /// gives way to check 1's row.
    #[test]
    fn the_flip_with_nothing_applied_shows_the_waiting_row_until_herdr_caches() {
        let home = HomeSandbox::new();
        let herdr = shimmed(&home);
        crate::profile::save_app_state(&AppState::default()).unwrap();
        let mut app = options_app();
        app.services.herdr_options_cursor = HERDR_OPTIONS
            .iter()
            .position(|row| *row == HerdrOption::HeldQuestionDot)
            .unwrap();
        handle_key(&mut app, crate::testutil::key(KeyCode::Char(' ')));
        join_test_workers();
        drain_service_probes(&mut app);
        assert_eq!(
            app.services.herdr_detection,
            vec!["detection: waiting for herdr to download its claude manifest".to_string()]
        );
        assert!(!herdr.override_path().exists(), "nothing is applied");
        assert_eq!(herdr.reloads(), 0);

        herdr.write_cache(CACHE_V1);
        recompute_services_checks(&mut app, false, ShuntRefresh::Keep);
        join_test_workers();
        drain_service_probes(&mut app);
        assert_eq!(
            app.services.herdr_detection,
            vec!["detection: herdr manifest 2026.10.11.1 + clauth rule".to_string()]
        );
        assert_eq!(
            std::fs::read_to_string(herdr.override_path()).unwrap(),
            format!("{HEADER}{CACHE_V1}{BLOCK}")
        );
    }

    /// A first refresh with the knob already on and no record yet: the
    /// watcher starts the pass, applies the rule, and lands the card.
    #[test]
    fn a_first_refresh_with_the_knob_on_applies_the_rule() {
        let home = HomeSandbox::new();
        let herdr = shimmed(&home);
        herdr.write_cache(CACHE_V1);
        let mut app = options_app();
        app.save_state_delta(|s, _held| s.herdr.held_question_dot = true);

        recompute_services_checks(&mut app, false, ShuntRefresh::Keep);
        join_test_workers();
        drain_service_probes(&mut app);
        assert_eq!(
            std::fs::read_to_string(herdr.override_path()).unwrap(),
            format!("{HEADER}{CACHE_V1}{BLOCK}")
        );
        assert_eq!(
            app.services.herdr_detection,
            vec!["detection: herdr manifest 2026.10.11.1 + clauth rule".to_string()]
        );
    }

    /// With no daemon, the Services tab refresh is the watcher: a moved cache
    /// version rebuilds clauth's file, an unmoved one runs nothing.
    #[test]
    fn a_tab_refresh_rebuilds_on_a_moved_cache_version() {
        let home = HomeSandbox::new();
        let herdr = shimmed(&home);
        herdr.write_cache(CACHE_V1);
        let mut app = options_app();
        app.save_state_delta(|s, _held| s.herdr.held_question_dot = true);
        crate::herdr_detect::run_now();
        herdr.write_cache(CACHE_V2);
        herdr.clear_log();

        recompute_services_checks(&mut app, false, ShuntRefresh::Keep);
        join_test_workers();
        drain_service_probes(&mut app);
        assert_eq!(
            std::fs::read_to_string(herdr.override_path()).unwrap(),
            format!("{HEADER}{CACHE_V2}{BLOCK}")
        );
        assert_eq!(herdr.reloads(), 1);
        assert_eq!(
            app.services.herdr_detection,
            vec!["detection: herdr manifest 2026.11.30.2 + clauth rule".to_string()]
        );

        herdr.clear_log();
        recompute_services_checks(&mut app, false, ShuntRefresh::Keep);
        join_test_workers();
        drain_service_probes(&mut app);
        assert_eq!(herdr.log(), "", "an unmoved version runs nothing");
    }

    /// With the toggle off a refresh reads nothing of herdr's for the
    /// feature and runs no herdr.
    #[test]
    fn a_tab_refresh_with_the_toggle_off_runs_no_herdr() {
        let home = HomeSandbox::new();
        let herdr = shimmed(&home);
        herdr.write_cache(CACHE_V1);
        let mut app = options_app();

        recompute_services_checks(&mut app, false, ShuntRefresh::Keep);
        join_test_workers();
        drain_service_probes(&mut app);
        assert_eq!(herdr.log(), "");
        assert!(!herdr.override_path().exists());
        assert_eq!(app.services.herdr_detection, Vec::<String>::new());
    }
}
