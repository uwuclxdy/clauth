#![allow(unsafe_code)]
//! Showcase — a fake-data TUI for taking README screenshots. Compiled ONLY
//! under `#[cfg(test)]` (included via `#[path]` into `crate::tui`), so none of
//! this ships in the `clauth` binary and it lives outside `src/`.
//!
//! Launch it in a real terminal (it takes over the screen; press q twice to quit):
//!
//! ```text
//! cargo test showcase -- --ignored --nocapture
//! ```
//!
//! All tests redirect `~/.clauth` and `~/.claude` into a tempdir via
//! [`crate::profile::set_home_override`] so real files are never touched.

use std::collections::BTreeMap;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use ratatui::crossterm::event::{
    self, Event, KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers,
};
use tempfile::TempDir;

use super::{TICK, app, render};
use crate::profile::{AppConfig, AppState, Profile, ProfileName};
use crate::usage::{
    ExtraUsage, FetchStatus, PlanInfo, PlanTier, ProfileActivity, ScopedWindow, SpendInfo,
    UsageInfo, UsageWindow, now_ms,
};

// ── Launch ──────────────────────────────────────────────────────────────────

#[test]
#[ignore = "interactive TUI; run with `cargo test showcase -- --ignored --nocapture` in a real terminal"]
fn showcase() {
    run(demo_config()).expect("showcase loop");
}

/// Redirect home into a tempdir so all disk ops land on scratch space.
struct ShowcaseHome {
    _home_lock: crate::lockorder::RankedGuard<'static, ()>,
    _tmp: TempDir,
}

impl ShowcaseHome {
    fn new() -> Self {
        let _home_lock = crate::profile::HOME_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let tmp = TempDir::new().expect("tempdir for showcase");
        let path = tmp.path().to_path_buf();
        std::fs::create_dir_all(&path).expect("create temp home");
        crate::profile::set_home_override(path);

        // Pre-seed usage history files on disk so on_tick → apply_usage doesn't
        // overwrite the in-memory seed with an empty/partial reload.
        for (name, entries) in &build_synthetic_history() {
            let history_path = crate::profile::profile_history_path(name).expect("history path");
            std::fs::create_dir_all(history_path.parent().unwrap()).expect("history dir");
            let content: String = entries
                .iter()
                .map(|(ts, usage)| {
                    let usage_json = serde_json::to_string(usage).unwrap_or_default();
                    let name_json =
                        serde_json::to_string(name).unwrap_or_else(|_| format!(r#""{}""#, name));
                    format!(
                        r#"{{"ts":{},"name":{},"usage":{}}}"#,
                        ts, name_json, usage_json
                    )
                })
                .collect::<Vec<_>>()
                .join("\n")
                + "\n";
            std::fs::write(&history_path, content).expect("write seeded history");
        }

        Self {
            _home_lock,
            _tmp: tmp,
        }
    }
}

/// Build the same synthetic history used by [`seed_history`], returned as a map
/// so callers can write it to disk or populate the in-memory cache.
/// Build resets-less per-model weekly windows for the synthetic history (history
/// snapshots carry only utilization; the pace math reads live `resets_at`).
fn weekly_history(entries: &[(&str, f64)]) -> Vec<ScopedWindow> {
    entries
        .iter()
        .map(|(label, util)| ScopedWindow {
            label: (*label).to_string(),
            window: UsageWindow {
                utilization: *util,
                resets_at: None,
            },
        })
        .collect()
}

fn build_synthetic_history() -> std::collections::HashMap<String, Vec<(u64, UsageInfo)>> {
    use std::collections::HashMap;
    let now = now_ms();

    let mut personal: Vec<(u64, UsageInfo)> = Vec::with_capacity(40);
    for i in 0..=30 {
        let ts = now - (30 - i) as u64 * 480_000;
        let pct = 45.0 + (i as f64 / 30.0) * (64.3 - 45.0);
        personal.push((
            ts,
            UsageInfo {
                five_hour: Some(UsageWindow {
                    utilization: pct,
                    resets_at: None,
                }),
                weekly_scoped: weekly_history(&[("7d sonnet", 22.1), ("7d opus", 8.4)]),
                ..UsageInfo::default()
            },
        ));
    }
    for i in 0..=10 {
        let ts = now - (10 - i) as u64 * 43_200_000;
        personal.push((
            ts,
            UsageInfo {
                five_hour: None,
                weekly_scoped: weekly_history(&[("7d sonnet", 22.1), ("7d opus", 8.4)]),
                ..UsageInfo::default()
            },
        ));
    }

    let mut work: Vec<(u64, UsageInfo)> = Vec::with_capacity(40);
    for i in 0..=30 {
        let ts = now - (30 - i) as u64 * 480_000;
        let pct = 55.0 + (i as f64 / 30.0) * (88.7 - 55.0);
        work.push((
            ts,
            UsageInfo {
                five_hour: Some(UsageWindow {
                    utilization: pct,
                    resets_at: None,
                }),
                weekly_scoped: weekly_history(&[("7d sonnet", 61.2), ("7d opus", 33.9)]),
                ..UsageInfo::default()
            },
        ));
    }
    for i in 0..=10 {
        let ts = now - (10 - i) as u64 * 43_200_000;
        work.push((
            ts,
            UsageInfo {
                five_hour: None,
                weekly_scoped: weekly_history(&[("7d sonnet", 61.2), ("7d opus", 33.9)]),
                ..UsageInfo::default()
            },
        ));
    }

    let mut side: Vec<(u64, UsageInfo)> = Vec::with_capacity(40);
    for i in 0..=30 {
        let ts = now - (30 - i) as u64 * 480_000;
        let pct = 5.0 + (i as f64 / 30.0) * (12.0 - 5.0);
        side.push((
            ts,
            UsageInfo {
                five_hour: Some(UsageWindow {
                    utilization: pct,
                    resets_at: None,
                }),
                ..UsageInfo::default()
            },
        ));
    }

    let mut cache = HashMap::new();
    cache.insert("personal".to_string(), personal);
    cache.insert("work".to_string(), work);
    cache.insert("side-project".to_string(), side);
    cache
}

/// Same as [`super::run`] but with home redirected into a tempdir.
fn run(config: AppConfig) -> Result<()> {
    let _home = ShowcaseHome::new();
    let mut terminal = ratatui::try_init()?;
    let outcome = showcase_loop(&mut terminal, config);
    ratatui::restore();
    outcome
}

/// Real event loop without startup reconciliation — no bootstrap/scheduler spawns.
fn showcase_loop(terminal: &mut ratatui::DefaultTerminal, config: AppConfig) -> Result<()> {
    let mut application = app::App::new(config);
    seed_usage(&application);
    seed_timers(&application);
    seed_history(&application);
    let mut last_tick = Instant::now();

    while !application.quit {
        terminal.draw(|frame| render::draw(frame, &application))?;

        let timeout = TICK.saturating_sub(last_tick.elapsed());
        if event::poll(timeout)? {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    app::handle_key(&mut application, key);
                }
                Event::Resize(_, _) => {}
                _ => {}
            }
        }

        if last_tick.elapsed() >= TICK {
            app::on_tick(&mut application);
            last_tick = Instant::now();
        }
    }

    Ok(())
}

/// RFC3339-ish string for `now + offset` (matches `iso_to_epoch_secs` format).
fn future_iso(offset: Duration) -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
        + offset.as_secs();
    // no chrono dep; YYYY-MM-DDTHH:MM:SS+00:00 shape iso_to_epoch_secs parses
    let (y, mo, d, h, mi, sec) = epoch_to_parts(secs);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{sec:02}+00:00")
}

fn epoch_to_parts(secs: u64) -> (u64, u64, u64, u64, u64, u64) {
    let s = secs % 60;
    let m = (secs / 60) % 60;
    let h = (secs / 3600) % 24;
    let days = secs / 86400;
    // Gregorian civil calendar — Howard Hinnant's algorithm, unsigned edition.
    let z = days + 719468;
    let era = z / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if mo <= 2 { y + 1 } else { y };
    (y, mo, d, h, m, s)
}

#[allow(clippy::too_many_arguments)]
fn oauth_profile(
    name: &str,
    plan_type: &str,
    tier: &str,
    has_max: bool,
    has_pro: bool,
    auto_start: bool,
    fallback_threshold: Option<f64>,
    five_util: f64,
    five_resets_in: Option<Duration>,
    weekly: &[(&str, f64, Duration)],
    extra: Option<ExtraUsage>,
    spend: Option<SpendInfo>,
    fetch_status: Option<FetchStatus>,
) -> Profile {
    let five_hour = Some(UsageWindow {
        utilization: five_util,
        resets_at: five_resets_in.map(future_iso),
    });
    let weekly_scoped = weekly
        .iter()
        .map(|(label, util, reset)| ScopedWindow {
            label: (*label).to_string(),
            window: UsageWindow {
                utilization: *util,
                resets_at: Some(future_iso(*reset)),
            },
        })
        .collect();
    Profile {
        harness: Default::default(),
        name: name.into(),
        base_url: None,
        api_key: None,
        auto_start,
        env: BTreeMap::new(),
        models: Default::default(),
        fallback_threshold,
        weekly_threshold: None,
        last_resort: false,
        session_feed: false,
        max_auto_spend: None,
        check_weekly: true,
        check_scoped: true,
        bell_threshold: None,
        disabled: false,
        credentials: None,
        usage: Some(UsageInfo {
            plan: Some(PlanInfo {
                tier: PlanTier::from_profile(Some(plan_type), has_max, has_pro, Some(tier)),
                subscription_status: None,
            }),
            five_hour,
            seven_day: None,
            weekly_scoped,
            window_dollars: Vec::new(),
            extra_usage: extra,
            spend,
            codex_rate_limit_reached: None,
        }),
        fetch_status,
        provider: None,
        third_party_usage: None,
    }
}

fn api_profile(name: &str) -> Profile {
    Profile {
        harness: Default::default(),
        name: name.into(),
        base_url: Some("https://api.example.com".to_string()),
        api_key: Some(
            "sk-ant-api03-demo0000000000000000000000000000000000000000000000".to_string(),
        ),
        auto_start: false,
        env: BTreeMap::new(),
        models: Default::default(),
        fallback_threshold: None,
        weekly_threshold: None,
        last_resort: false,
        session_feed: false,
        max_auto_spend: None,
        check_weekly: true,
        check_scoped: true,
        bell_threshold: None,
        disabled: false,
        credentials: None,
        usage: None,
        fetch_status: None,
        provider: None,
        third_party_usage: None,
    }
}

fn failed_profile(name: &str) -> Profile {
    Profile {
        harness: Default::default(),
        name: name.into(),
        base_url: None,
        api_key: None,
        auto_start: false,
        env: BTreeMap::new(),
        models: Default::default(),
        fallback_threshold: Some(90.0),
        weekly_threshold: None,
        last_resort: false,
        session_feed: false,
        max_auto_spend: None,
        check_weekly: true,
        check_scoped: true,
        bell_threshold: None,
        disabled: false,
        credentials: None,
        usage: None,
        fetch_status: Some(FetchStatus::Failed),
        provider: None,
        third_party_usage: None,
    }
}

fn demo_config() -> AppConfig {
    let max20 = oauth_profile(
        "personal",
        "claude_max",
        "default_claude_max_20x",
        true,
        false,
        true,
        Some(80.0),
        64.3,
        Some(Duration::from_secs(2 * 3600 + 17 * 60)), // ~2h17m
        &[
            ("7d sonnet", 22.1, Duration::from_secs(5 * 86400 + 6 * 3600)), // ~5d
            ("7d opus", 8.4, Duration::from_secs(6 * 86400 + 2 * 3600)),    // ~6d
            ("7d fable", 14.0, Duration::from_secs(4 * 86400 + 3600)), // dynamically-detected model
        ],
        None,
        None,
        None,
    );

    let extra = ExtraUsage {
        is_enabled: true,
        monthly_limit: Some(100.00),
        used_credits: Some(42.50),
        utilization: Some(42.5),
        currency: Some("USD".to_string()),
        ..Default::default()
    };
    let max5 = oauth_profile(
        "work",
        "claude_max",
        "default_claude_max_5x",
        true,
        false,
        true,
        Some(90.0),
        88.7,
        Some(Duration::from_secs(45 * 60)), // ~45m
        &[
            ("7d sonnet", 61.2, Duration::from_secs(3 * 86400 + 9 * 3600)), // ~3d
            ("7d opus", 33.9, Duration::from_secs(6 * 86400 + 3600)),       // ~6d
        ],
        Some(extra),
        Some(SpendInfo {
            enabled: true,
            used: Some(3.20),
            limit: Some(10.00),
            percent: Some(32.0),
            currency: Some("USD".to_string()),
        }),
        Some(FetchStatus::Cached), // cached → warning underline
    );

    let pro = oauth_profile(
        "side-project",
        "claude_pro",
        "default_claude_pro",
        false,
        true,
        false,
        Some(100.0),
        12.0,
        Some(Duration::from_secs(4 * 3600 + 5 * 60)),
        &[],
        None,
        None,
        None,
    );

    let api = api_profile("bedrock-dev");

    let stale = failed_profile("research");

    let names: Vec<ProfileName> = [
        "personal",
        "work",
        "side-project",
        "bedrock-dev",
        "research",
    ]
    .iter()
    .map(|s| (*s).into())
    .collect();

    AppConfig {
        state: AppState {
            active_profile: Some("personal".into()),
            profiles: names,
            fallback_chain: vec!["personal".into(), "work".into(), "side-project".into()],
            ..AppState::default()
        },
        profiles: vec![max20, max5, pro, api, stale],
    }
}

// ── Seeding ─────────────────────────────────────────────────────────────────

/// Seed the live usage stores from demo profile data, as a real fetch worker would.
/// Without this, the first `on_tick` → `apply_usage` blanks all windows to `-`.
fn seed_usage(application: &app::App) {
    let snapshot: Vec<(String, Option<UsageInfo>, Option<FetchStatus>)> = {
        let cfg = application.config();
        cfg.profiles
            .iter()
            .map(|p| (p.name.to_string(), p.usage.clone(), p.fetch_status))
            .collect()
    };
    if let Ok(mut store) = application.usage_store.lock() {
        for (name, usage, _) in &snapshot {
            if let Some(u) = usage {
                store.insert(name.clone(), u.clone());
            }
        }
    }
    if let Ok(mut status) = application.usage_status.lock() {
        for (name, _, fetch_status) in &snapshot {
            if let Some(s) = fetch_status {
                status.insert(name.clone(), *s);
            }
        }
    }
}

/// Seed timer sources so the overview shows a spinner and countdowns (no
/// scheduler runs in the demo). Both stores are leaf-rank, in-memory only.
fn seed_timers(application: &app::App) {
    let now = now_ms();
    if let Ok(mut next) = application.next_refresh_per_profile.lock() {
        next.insert("work".to_string(), now + 43_000); // ~43s
        next.insert("side-project".to_string(), now + 78_000); // ~78s
    }
    if let Ok(mut activity) = application.activity.lock() {
        activity.insert("personal".to_string(), ProfileActivity::Fetching);
    }
}

/// Seed `history_cache` with mock usage data so `compute_burn_rates_from_history`
/// has entries to derive burn-rate predictions for the usage-tab detail pane.
///
/// Each profile gets a synthetic timeline: ~30 entries over the last ~4h for 5h
/// windows, and ~12 entries over the last ~5d for 7d windows, with plausible
/// utilization ramps so the burn-rate math yields non-zero results.
///
/// The same data is pre-seeded as disk files by [`ShowcaseHome::new`] so
/// `apply_usage`'s history reload preserves the full timeline.
fn seed_history(application: &app::App) {
    let cache = build_synthetic_history();
    unsafe {
        let app_ptr = application as *const app::App as *mut app::App;
        (*app_ptr).history_cache = cache;
    }
}

// ── Unit tests on demo data ──────────────────────────────────────────────────

#[test]
fn demo_config_has_expected_profiles() {
    let cfg = demo_config();
    assert_eq!(cfg.profiles.len(), 5);
    assert_eq!(cfg.state.active_profile.as_deref(), Some("personal"));
    assert_eq!(cfg.state.fallback_chain.len(), 3);

    let personal = cfg.profiles.iter().find(|p| p.name == "personal");
    assert!(personal.is_some_and(|p| p.auto_start && p.base_url.is_none()));

    let work = cfg.profiles.iter().find(|p| p.name == "work");
    assert!(work.is_some_and(|p| {
        p.fetch_status == Some(FetchStatus::Cached)
            && p.usage
                .as_ref()
                .and_then(|u| u.extra_usage.as_ref())
                .is_some_and(|e| e.is_enabled)
    }));

    let api = cfg.profiles.iter().find(|p| p.name == "bedrock-dev");
    assert!(api.is_some_and(|p| !p.is_oauth()));

    let failed = cfg.profiles.iter().find(|p| p.name == "research");
    assert!(
        failed.is_some_and(|p| p.fetch_status == Some(FetchStatus::Failed) && p.usage.is_none())
    );
}

#[test]
fn future_iso_parses() {
    use crate::usage::iso_to_epoch_secs;
    let s = future_iso(Duration::from_secs(3600));
    assert!(iso_to_epoch_secs(&s).is_some());
}

/// Synthetic history entries must be parseable by the burn-rate engine.
#[test]
fn seed_history_yields_burn_rates() {
    let _home = ShowcaseHome::new();
    let app = app::App::new(demo_config());
    seed_usage(&app);
    seed_history(&app);

    let personal = app.history_cache.get("personal").unwrap();
    assert!(
        personal.len() >= 30,
        "personal must have enough history entries for burn-rate window"
    );

    let work = app.history_cache.get("work").unwrap();
    assert!(
        work.len() >= 30,
        "work must have enough history entries for burn-rate window"
    );
}

/// Headless render gate: draws every tab and a post-tick frame through
/// `TestBackend` on the shared demo data. The `term.draw(...).unwrap()` is the
/// no-panic invariant for each surface; the content asserts prove the populated
/// (non-empty-state) layout actually rendered. Runs under plain `cargo test` —
/// no TTY, no `--ignored` — so CI gates that the surfaces render.
#[test]
fn headless_showcase_renders() {
    // Home redirected into a tempdir + disk history seeded; held for the whole
    // fn so on_tick writes land on scratch. Acquired BEFORE App::new so no
    // RankedMutex is ever held while taking HOME_TEST_LOCK, now the outermost
    // rank — the lock-order check enforces that ordering.
    let _home = ShowcaseHome::new();
    let mut app = app::App::new(demo_config());
    seed_usage(&app);
    seed_timers(&app);
    seed_history(&app);

    // 120 wide so the overview name column keeps "personal"/"work" un-truncated.
    let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 30)).unwrap();

    fn flatten(term: &ratatui::Terminal<ratatui::backend::TestBackend>) -> String {
        crate::testutil::buffer_rows(term.backend().buffer()).concat()
    }

    // Every tab renders without panicking and keeps the header brand.
    for tab in app::Tab::ALL {
        app.tab = tab;
        term.draw(|f| render::draw(f, &app)).unwrap();
        let frame = flatten(&term);
        assert!(frame.contains("clauth"), "header brand renders on {tab:?}");
    }

    // One tick exercises the drain/apply_usage/reload paths against the tempdir.
    app.tab = app::Tab::Overview;
    app::on_tick(&mut app);
    term.draw(|f| render::draw(f, &app)).unwrap();
    let overview = flatten(&term);

    assert!(overview.contains("clauth"), "header brand survives a tick");
    assert!(
        overview.contains("personal"),
        "active demo profile populates the overview list"
    );
    assert!(
        overview.contains("work"),
        "second profile row renders in the overview list"
    );
    // Tab-bar labels render (lowercase, un-truncated at 120w); `setup`/`config`
    // are tab-only words on the overview surface, so finding them proves the bar.
    assert!(
        overview.contains("setup") && overview.contains("config"),
        "tab bar renders its labels"
    );

    // Pace marker: off by default, and flipping `show_pace` overlays extra `│`
    // ticks on the usage bars. Panel borders already draw `│`, so assert the
    // count rises rather than mere presence.
    let bars = |term: &ratatui::Terminal<ratatui::backend::TestBackend>| {
        flatten(term).matches('│').count()
    };
    app.tab = app::Tab::Usage;
    term.draw(|f| render::draw(f, &app)).unwrap();
    let personal_usage = flatten(&term);
    // The dynamically-detected per-model window (no hardcoded field) renders as
    // its own bar for the active profile.
    assert!(
        personal_usage.contains("7d fable"),
        "a weekly_scoped model window renders as a dynamic bar"
    );
    let without_pace = bars(&term);
    app.config().state.show_pace = true;
    term.draw(|f| render::draw(f, &app)).unwrap();
    assert!(
        bars(&term) > without_pace,
        "show_pace overlays ideal-pace markers on the usage bars ({without_pace} → {})",
        bars(&term),
    );

    // Select the profile carrying a populated spend cap. Real accounts return it
    // disabled, so the demo is the only place the guarded spend bar is observable.
    app.profile_cursor = 1; // personal → work
    term.draw(|f| render::draw(f, &app)).unwrap();
    let work_usage = flatten(&term);
    assert!(
        work_usage.contains("spend") && work_usage.contains("$3.20 / $10.00"),
        "the spend/credit-cap bar renders for an account with a cap"
    );
}

// ── Non-interactive driver ──────────────────────────────────────────────────
//
// Feeds synthetic key events through `handle_key` / `on_tick` so CI can prove
// every action — switch, edit, toggle, reorder, threshold, delete, create —
// without a TTY.  HOME_OVERRIDE points at a tempdir so all disk operations
// (save_profile, save_app_state, flock, symlinks, history) land on scratch
// space and never touch the real filesystem.

use crate::testutil::key;

fn key_shift(code: KeyCode) -> KeyEvent {
    KeyEvent {
        code,
        modifiers: KeyModifiers::SHIFT,
        kind: KeyEventKind::Press,
        state: KeyEventState::NONE,
    }
}

fn key_ctrl(code: KeyCode) -> KeyEvent {
    KeyEvent {
        code,
        modifiers: KeyModifiers::CONTROL,
        kind: KeyEventKind::Press,
        state: KeyEventState::NONE,
    }
}

fn press(app: &mut app::App, code: KeyCode) {
    app::handle_key(app, key(code));
}

/// Type a string one `Char` event at a time.
fn type_str(app: &mut app::App, s: &str) {
    for c in s.chars() {
        app::handle_key(app, key(KeyCode::Char(c)));
    }
}

/// Drain `on_tick` until `pred` holds or budget exhausted (async worker results).
fn settle(app: &mut app::App, what: &str, mut pred: impl FnMut(&app::App) -> bool) {
    for _ in 0..400 {
        app::on_tick(app);
        if pred(app) {
            return;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    panic!("'{what}' never settled after draining ticks");
}

/// Helper: read profile fields without holding the config guard across handle_key.
fn base_url_of(app: &app::App, name: &str) -> Option<String> {
    app.config().find(name).and_then(|p| p.base_url.clone())
}

fn auto_start_of(app: &app::App, name: &str) -> bool {
    app.config()
        .find(name)
        .map(|p| p.auto_start)
        .unwrap_or(false)
}

fn threshold_of(app: &app::App, name: &str) -> Option<f64> {
    app.config().find(name).and_then(|p| p.fallback_threshold)
}

#[test]
fn demo_data_drives_all_actions() {
    let _home = ShowcaseHome::new();
    let mut app = app::App::new(demo_config());
    seed_usage(&app);
    seed_timers(&app);
    seed_history(&app);

    // reconcile_startup never called → no bootstrap, no scheduler.
    assert!(!app.reconcile_done && !app.bootstrap_started);

    // Pre-drive: timer slot has a spinner for the active profile and a countdown for an idle one.
    assert_eq!(
        app.activity.lock().unwrap().get("personal").copied(),
        Some(ProfileActivity::Fetching),
        "active profile must show a spinner in the timer slot"
    );
    assert!(
        app.next_refresh_per_profile
            .lock()
            .unwrap()
            .contains_key("work"),
        "idle profiles must show a refresh countdown in the timer slot"
    );

    // ── Tab navigation ──
    use app::Tab;
    assert_eq!(app.tab, Tab::Overview);
    press(&mut app, KeyCode::Right);
    assert_eq!(app.tab, Tab::Usage);
    press(&mut app, KeyCode::Right);
    assert_eq!(app.tab, Tab::Tokens);
    press(&mut app, KeyCode::Right);
    assert_eq!(app.tab, Tab::Setup);
    press(&mut app, KeyCode::Right);
    assert_eq!(app.tab, Tab::Fallback);
    press(&mut app, KeyCode::Right);
    assert_eq!(app.tab, Tab::Config);
    press(&mut app, KeyCode::Right);
    assert_eq!(app.tab, Tab::Status);
    press(&mut app, KeyCode::Right);
    assert_eq!(app.tab, Tab::Plugin);
    press(&mut app, KeyCode::Right);
    assert_eq!(app.tab, Tab::Overview, "→ wraps back to Overview");
    press(&mut app, KeyCode::Left);
    assert_eq!(app.tab, Tab::Plugin, "← wraps to the last tab");
    // Seven ← from the last tab walk back to the first.
    for _ in 0..7 {
        press(&mut app, KeyCode::Left);
    }
    assert_eq!(app.tab, Tab::Overview);

    // ── Switch ──
    assert_eq!(
        app.config().state.active_profile.as_deref(),
        Some("personal")
    );
    press(&mut app, KeyCode::Down); // 0 (personal) → 1 (work)
    press(&mut app, KeyCode::Enter); // request switch → confirm modal
    assert_eq!(app.modals.len(), 1, "switch raises a confirm modal");
    press(&mut app, KeyCode::Enter); // accept (choice defaults to yes)
    assert!(app.modals.is_empty(), "confirming pops the modal");
    settle(&mut app, "switch to work", |a| {
        a.config().state.active_profile.as_deref() == Some("work")
    });

    // Seeded usage windows must survive on_tick → apply_usage during the switch.
    {
        let cfg = app.config();
        let util = cfg
            .find("personal")
            .and_then(|p| p.usage.as_ref())
            .and_then(|u| u.five_hour.as_ref())
            .map(|w| w.utilization);
        assert_eq!(
            util,
            Some(64.3),
            "seeded 5h utilization must survive on_tick → apply_usage"
        );
    }

    // State file written to the tempdir, not the real home.
    assert!(
        crate::profile::app_state_mtime().is_some(),
        "app state must be written (to the tempdir)"
    );

    // ── Edit ── (cursor at "work"/1 after switch; one ↓ → "side-project"/2)
    press(&mut app, KeyCode::Right); // Overview → Usage
    press(&mut app, KeyCode::Right); // Usage → Tokens
    press(&mut app, KeyCode::Right); // Tokens → Setup
    assert_eq!(app.tab, Tab::Setup);
    assert_eq!(app.profile_cursor, 1, "cursor carried over from the switch");
    press(&mut app, KeyCode::Down); // 1 → 2 (side-project)
    press(&mut app, KeyCode::Enter); // focus the detail pane
    assert_eq!(app.config_focus, app::ConfigFocus::Actions);
    assert!(app.config_draft.is_some());
    press(&mut app, KeyCode::Down); // Name → AutoStart (OAuth row)
    press(&mut app, KeyCode::Down); // AutoStart → BaseUrl
    press(&mut app, KeyCode::Enter); // start capturing the field
    assert_eq!(
        app.config_draft.as_ref().and_then(|d| d.active),
        Some(app::ConfigRow::BaseUrl)
    );
    type_str(&mut app, "https://proxy.test");
    press(&mut app, KeyCode::Enter); // commit the field
    assert_eq!(
        base_url_of(&app, "side-project").as_deref(),
        Some("https://proxy.test"),
        "editing the BaseUrl field must persist it"
    );
    press(&mut app, KeyCode::Esc); // back out of the detail pane
    assert_eq!(app.config_focus, app::ConfigFocus::Profiles);

    // ── Toggle ──
    assert!(auto_start_of(&app, "personal"), "demo seeds personal ON");
    press(&mut app, KeyCode::Up); // 2 → 1 (work)
    press(&mut app, KeyCode::Up); // → 0 (personal)
    press(&mut app, KeyCode::Enter); // focus detail for personal
    // auto-start sits right below name (OAuth-only); one step down reaches it.
    press(&mut app, KeyCode::Down); // Name → AutoStart
    press(&mut app, KeyCode::Enter); // flip it
    assert!(
        !auto_start_of(&app, "personal"),
        "auto-start must toggle off"
    );
    press(&mut app, KeyCode::Esc);

    // ── Reorder ──
    press(&mut app, KeyCode::Right); // Config → Fallback
    assert_eq!(app.tab, Tab::Fallback);
    {
        let cfg = app.config();
        assert_eq!(
            cfg.state.fallback_chain,
            vec!["personal", "work", "side-project"]
        );
    }
    app::handle_key(&mut app, key_shift(KeyCode::Down)); // head → one step down
    {
        let cfg = app.config();
        assert_eq!(
            cfg.state.fallback_chain,
            vec!["work", "personal", "side-project"],
            "⇧↓ reorders the chain"
        );
    }
    assert_eq!(app.chain_cursor, 1, "cursor follows the moved member");

    // ── Set threshold (stepper) ── "personal" is chain index 1, 80% → 85%
    assert_eq!(threshold_of(&app, "personal"), Some(80.0));
    press(&mut app, KeyCode::Enter); // enter member detail pane
    assert_eq!(app.fallback_focus, app::FallbackFocus::Detail);
    press(&mut app, KeyCode::Char('+')); // +5 step
    assert_eq!(
        threshold_of(&app, "personal"),
        Some(85.0),
        "the + stepper bumps the threshold by 5"
    );

    // ── Set threshold (inline editor) ──
    press(&mut app, KeyCode::Enter); // open inline editor on Threshold row
    assert!(app.fallback_threshold_draft.is_some());
    press(&mut app, KeyCode::Backspace); // clear "85" (2 chars)
    press(&mut app, KeyCode::Backspace);

    // Out-of-range value: commit is blocked and the draft stays open so the
    // inline Invalid-input treatment shows.
    type_str(&mut app, "150");
    press(&mut app, KeyCode::Enter); // commit attempt — rejected
    assert!(
        app.fallback_threshold_draft.is_some(),
        "an out-of-range threshold keeps the editor open (inline invalid, no toast)"
    );
    assert_eq!(
        threshold_of(&app, "personal"),
        Some(85.0),
        "the rejected value never persists"
    );

    // ctrl+w wipes the bad input as one word, then a valid value commits.
    app::handle_key(&mut app, key_ctrl(KeyCode::Char('w')));
    assert_eq!(
        app.fallback_threshold_draft
            .as_ref()
            .map(|d| d.value.as_str()),
        Some(""),
        "ctrl+w clears the whole typed run"
    );
    type_str(&mut app, "50");
    press(&mut app, KeyCode::Enter); // commit
    assert!(app.fallback_threshold_draft.is_none());
    assert_eq!(
        threshold_of(&app, "personal"),
        Some(50.0),
        "the inline editor sets an absolute threshold"
    );
    press(&mut app, KeyCode::Esc); // leave the detail pane
    assert_eq!(app.fallback_focus, app::FallbackFocus::Chain);

    // ── Delete ──
    let before = app.profile_count();
    press(&mut app, KeyCode::Left); // Fallback → Setup
    assert_eq!(app.tab, Tab::Setup);
    for _ in 0..4 {
        press(&mut app, KeyCode::Down); // 0 → 4 ("research")
    }
    press(&mut app, KeyCode::Enter); // focus detail
    press(&mut app, KeyCode::Up); // Name → Delete (wraps to last row)
    press(&mut app, KeyCode::Enter); // arm
    assert!(
        app.config_draft
            .as_ref()
            .is_some_and(|d| d.armed_action == Some(app::ConfigRow::Delete)),
        "first ⏎ arms the delete row"
    );
    press(&mut app, KeyCode::Enter); // confirm
    assert_eq!(app.profile_count(), before - 1, "delete drops one profile");
    assert!(
        app.config().find("research").is_none(),
        "the deleted profile is gone from the config"
    );

    // ── Create ──
    let before = app.profile_count();
    // Navigate to the + new row (last slot in Setup list)
    press(&mut app, KeyCode::Down); // 4 → end ("+ new")
    press(&mut app, KeyCode::Enter); // focus detail (auto-positions on Name)
    assert_eq!(app.config_focus, app::ConfigFocus::Actions);
    assert!(app.config_draft.is_some());

    // Fill the create form: Name, BaseUrl, ApiKey
    // Cursor starts on first row (Name)
    press(&mut app, KeyCode::Enter); // start capturing Name
    type_str(&mut app, "sandbox-test");
    press(&mut app, KeyCode::Enter); // commit Name

    // Navigate down to BaseUrl, fill it
    press(&mut app, KeyCode::Down); // Name → BaseUrl
    press(&mut app, KeyCode::Enter); // start capturing
    type_str(&mut app, "https://api.sandbox.test");
    press(&mut app, KeyCode::Enter); // commit BaseUrl

    // Move to ApiKey, fill it
    press(&mut app, KeyCode::Down); // BaseUrl → ApiKey
    press(&mut app, KeyCode::Enter); // start capturing
    type_str(&mut app, "sk-test-key-0000");
    press(&mut app, KeyCode::Enter); // commit ApiKey

    // Navigate to Create row and commit (skipping over the base model row —
    // left untouched, so the created profile's default stays unset).
    press(&mut app, KeyCode::Down); // ApiKey → Model
    press(&mut app, KeyCode::Down); // Model → Create

    assert!(
        app.config_draft
            .as_ref()
            .map(|d| d.editing_name.is_none())
            .unwrap_or(false),
        "Create row must only appear in new-account mode"
    );

    press(&mut app, KeyCode::Enter); // commit Create
    assert_eq!(
        app.profile_count(),
        before + 1,
        "create must add one profile"
    );
    let created = {
        let cfg = app.config();
        cfg.find("sandbox-test")
            .map(|p| (p.base_url.clone(), p.api_key.clone()))
    };
    assert!(
        created.is_some(),
        "the created profile must exist in config"
    );
    assert_eq!(
        created.as_ref().and_then(|(b, _)| b.as_deref()),
        Some("https://api.sandbox.test"),
        "created profile must preserve base_url"
    );
    assert_eq!(
        created.as_ref().and_then(|(_, k)| k.as_deref()),
        Some("sk-test-key-0000"),
        "created profile must preserve api_key"
    );

    // Clean up — delete what we just created. "sandbox-test" has base_url set →
    // is_oauth = false → delete is the last row (after name/endpoint/model rows).
    let before = app.profile_count();
    press(&mut app, KeyCode::Enter); // focus detail for sandbox-test
    assert_eq!(app.config_focus, app::ConfigFocus::Actions);
    press(&mut app, KeyCode::Up); // Name → Delete (wraps to last row)
    press(&mut app, KeyCode::Enter); // arm
    assert!(
        app.config_draft
            .as_ref()
            .is_some_and(|d| d.armed_action == Some(app::ConfigRow::Delete))
    );
    press(&mut app, KeyCode::Enter); // confirm delete
    assert_eq!(
        app.profile_count(),
        before - 1,
        "delete the newly created profile"
    );
    assert!(
        app.config().find("sandbox-test").is_none(),
        "sandbox-test must be gone after delete"
    );

    // ── History cache seeded for usage predictions ──
    {
        let personal = app.history_cache.get("personal");
        assert!(
            personal.is_some(),
            "history_cache must be seeded for personal"
        );
        let personal = personal.unwrap();
        assert!(
            personal.len() >= 30,
            "personal must have enough history entries for burn rates"
        );

        // Burn rates should compute from the seeded history after apply_usage
        // (the first on_tick calls apply_usage which populates profile.usage
        // from usage_store, which was seeded in seed_usage).
    }

    // ── Usage tab shows prediction data ──
    // Navigate to usage tab and verify the profile detail renders with history.
    // The renderer uses history_cache for burn-rate computation.
    press(&mut app, KeyCode::Right); // Setup → Fallback
    press(&mut app, KeyCode::Right); // Fallback → Config
    press(&mut app, KeyCode::Right); // Config → Status
    press(&mut app, KeyCode::Right); // Status → Plugin
    // One more right wraps back to Overview
    press(&mut app, KeyCode::Right); // Plugin → Overview
    assert_eq!(app.tab, Tab::Overview);

    // ── Quit ──
    press(&mut app, KeyCode::Char('q'));
    assert!(app.armed_quit, "first q arms the quit");
    assert!(!app.quit, "first q does not quit yet");
    assert!(app.footer_alert.is_some(), "first q sets a footer alert");

    // any non-q key disarms the alert (Esc at top level is a no-op but still disarms)
    press(&mut app, KeyCode::Esc);
    assert!(!app.armed_quit, "unhandled key disarms quit");
    assert!(app.footer_alert.is_none(), "disarm clears footer alert");

    // x dismisses the alert directly when no toasts are queued
    app.toasts.clear(); // drain any toasts from earlier actions
    press(&mut app, KeyCode::Char('q'));
    assert!(app.footer_alert.is_some(), "re-arm sets alert");
    press(&mut app, KeyCode::Char('x'));
    assert!(app.footer_alert.is_none(), "x dismisses the footer alert");
    assert!(!app.armed_quit, "x also disarms quit");

    press(&mut app, KeyCode::Char('q'));
    press(&mut app, KeyCode::Char('q'));
    assert!(app.quit, "second q confirms quit");

    // All disk writes landed in the tempdir — clean-up on drop.
}

// ── Tab / BackTab (#14) ──────────────────────────────────────────────────────

#[test]
fn tab_backtab_cycle_screens_like_arrow_keys_at_top_level() {
    use app::Tab;
    let _home = ShowcaseHome::new();
    let mut app = app::App::new(demo_config());

    assert_eq!(app.tab, Tab::Overview);
    press(&mut app, KeyCode::Tab);
    assert_eq!(app.tab, Tab::Usage);
    press(&mut app, KeyCode::Tab);
    assert_eq!(app.tab, Tab::Tokens);
    press(&mut app, KeyCode::Tab);
    assert_eq!(app.tab, Tab::Setup);
    press(&mut app, KeyCode::Tab);
    assert_eq!(app.tab, Tab::Fallback);
    press(&mut app, KeyCode::Tab);
    assert_eq!(app.tab, Tab::Config);
    press(&mut app, KeyCode::Tab);
    assert_eq!(app.tab, Tab::Status);
    press(&mut app, KeyCode::Tab);
    assert_eq!(app.tab, Tab::Plugin);
    press(&mut app, KeyCode::Tab);
    assert_eq!(app.tab, Tab::Overview, "Tab wraps back to Overview");

    press(&mut app, KeyCode::BackTab);
    assert_eq!(app.tab, Tab::Plugin, "BackTab wraps to the last tab");
    // Seven BackTab from the last tab walk back to the first, same as ←.
    for _ in 0..7 {
        press(&mut app, KeyCode::BackTab);
    }
    assert_eq!(app.tab, Tab::Overview);
}

#[test]
fn tab_key_does_not_leak_past_modal_or_field_capture() {
    use app::Tab;
    let _home = ShowcaseHome::new();
    let mut app = app::App::new(demo_config());

    // ── Setup field capture: Tab/BackTab must stay inert, not switch tabs ──
    press(&mut app, KeyCode::Tab); // Overview → Usage
    press(&mut app, KeyCode::Tab); // Usage → Tokens
    press(&mut app, KeyCode::Tab); // Tokens → Setup
    assert_eq!(app.tab, Tab::Setup);
    press(&mut app, KeyCode::Enter); // focus detail pane for "personal" (cursor 0)
    assert_eq!(app.config_focus, app::ConfigFocus::Actions);
    press(&mut app, KeyCode::Down); // Name → AutoStart
    press(&mut app, KeyCode::Down); // AutoStart → BaseUrl
    press(&mut app, KeyCode::Enter); // start capturing BaseUrl
    assert_eq!(
        app.config_draft.as_ref().and_then(|d| d.active),
        Some(app::ConfigRow::BaseUrl)
    );

    press(&mut app, KeyCode::Tab);
    assert_eq!(
        app.tab,
        Tab::Setup,
        "Tab must not leak past the field-capture guard to switch tabs"
    );
    assert_eq!(
        app.config_draft.as_ref().and_then(|d| d.active),
        Some(app::ConfigRow::BaseUrl),
        "Tab must not disturb the in-progress field capture"
    );
    press(&mut app, KeyCode::BackTab);
    assert_eq!(
        app.tab,
        Tab::Setup,
        "BackTab must not leak past the field-capture guard to switch tabs"
    );

    press(&mut app, KeyCode::Esc); // stop capturing the field
    press(&mut app, KeyCode::Esc); // back out of the detail pane
    assert_eq!(app.config_focus, app::ConfigFocus::Profiles);

    // ── Confirm modal: Tab keeps cycling the yes/no choice, unchanged ──
    press(&mut app, KeyCode::BackTab); // Setup → Tokens
    press(&mut app, KeyCode::BackTab); // Tokens → Usage
    press(&mut app, KeyCode::BackTab); // Usage → Overview
    assert_eq!(app.tab, Tab::Overview);
    press(&mut app, KeyCode::Down); // cursor 0 (personal, active) → 1 (work)
    press(&mut app, KeyCode::Enter); // request switch → confirm modal
    assert_eq!(app.modals.len(), 1, "switch raises a confirm modal");
    let choice = |app: &app::App| match app.modals.last() {
        Some(app::Modal::Confirm(state)) => state.choice,
        other => panic!("expected a Confirm modal, got {other:?}"),
    };
    let before = choice(&app);

    press(&mut app, KeyCode::Tab);
    assert_eq!(
        choice(&app),
        !before,
        "Tab must still cycle the modal's yes/no focus"
    );
    assert_eq!(
        app.tab,
        Tab::Overview,
        "Tab consumed by the modal must not also switch tabs"
    );

    press(&mut app, KeyCode::Esc); // dismiss without confirming
    assert!(app.modals.is_empty());
    assert_eq!(
        app.config().state.active_profile.as_deref(),
        Some("personal"),
        "cancelling the modal must not have switched profiles"
    );
}

// ── reset display + clock notation (issue #39) ───────────────────────────────

/// Both rows persist through the same `save_app_state` path every other Config
/// row uses, and the notation row is a TRUE disabled row while resets render
/// relative — dimming it in the renderer while the key still cycled it would
/// let an invisible value drift.
#[test]
fn config_reset_rows_cycle_and_persist_with_the_clock_row_gated() {
    use crate::profile::{ClockFormat, ResetDisplay};
    use app::{GLOBAL_CONFIG_ROWS, GlobalConfigRow, Tab};
    let _home = ShowcaseHome::new();
    let mut app = app::App::new(demo_config());
    app.tab = Tab::Config;

    let cursor_to = |app: &mut app::App, row: GlobalConfigRow| {
        app.global_config_cursor = GLOBAL_CONFIG_ROWS
            .iter()
            .position(|r| *r == row)
            .expect("row is in the config list");
    };
    let shape = |app: &app::App| app.config().state.reset_display();
    let notation = |app: &app::App| app.config().state.clock_format();

    // The clock row no-ops while the countdown is relative.
    cursor_to(&mut app, GlobalConfigRow::ClockNotation);
    assert_eq!(shape(&app), ResetDisplay::Relative);
    press(&mut app, KeyCode::Char(' '));
    assert_eq!(
        notation(&app),
        ClockFormat::H24,
        "the clock row must stay inert while no reset renders a clock"
    );

    // Cycling the shape row wraps back to the stock relative form.
    cursor_to(&mut app, GlobalConfigRow::ResetShape);
    press(&mut app, KeyCode::Char(' '));
    assert_eq!(shape(&app), ResetDisplay::Clock);
    press(&mut app, KeyCode::Char(' '));
    assert_eq!(shape(&app), ResetDisplay::Both);
    press(&mut app, KeyCode::Char(' '));
    assert_eq!(shape(&app), ResetDisplay::Relative);

    // With a clock on screen the notation row cycles and persists.
    press(&mut app, KeyCode::Char(' '));
    assert_eq!(shape(&app), ResetDisplay::Clock);
    cursor_to(&mut app, GlobalConfigRow::ClockNotation);
    press(&mut app, KeyCode::Char(' '));
    assert_eq!(notation(&app), ClockFormat::H12);
    press(&mut app, KeyCode::Char(' '));
    assert_eq!(notation(&app), ClockFormat::H24);

    // Both landed on disk, not just in the in-memory config.
    let reloaded = crate::profile::load_config().expect("reload state");
    assert_eq!(reloaded.state.reset_display(), ResetDisplay::Clock);
    assert_eq!(reloaded.state.clock_format(), ClockFormat::H24);
}
