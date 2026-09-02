//! Plugin-tab render tests. The `herdr` row: the dot color carries the verdict,
//! the selector row right-aligns a `[f]` marker exactly when the check offers a
//! fix — the verdict logic itself is unit-tested in `tests/inline/tui_app.rs`,
//! these pin the render per drift state. And the delegates pane: its rows, its
//! agreement with what `monitor` reports for the same record, its overflow
//! marker, and its empty state.

use crate::herdr::{ConfigStatus, HerdrProbe, RegistryEntry, SidebarState};
use crate::mcp::jobs::{self, JobRecord, JobState, RecordKind, RunningSpec};
use crate::profile::{AppConfig, AppState};
use crate::tui::app::{App, Check, Health, herdr_check};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use std::path::PathBuf;

const W: u16 = 100;
const H: u16 = 24;

fn entry(enabled: bool, min: Option<&str>, warnings: Vec<&str>) -> RegistryEntry {
    RegistryEntry {
        enabled,
        version: Some("0.1.0".into()),
        min_herdr_version: min.map(str::to_string),
        plugin_root: None,
        source_kind: Some("github".into()),
        warnings: warnings.into_iter().map(str::to_string).collect(),
    }
}

fn probe(version: Option<&str>, entry: Option<RegistryEntry>, error: Option<&str>) -> HerdrProbe {
    HerdrProbe {
        version: version.map(str::to_string),
        entry,
        config_path: Some(PathBuf::from("/tmp/herdr/config.toml")),
        error: error.map(str::to_string),
    }
}

fn config(parsed: bool, key: Option<&str>, sidebar: SidebarState) -> ConfigStatus {
    ConfigStatus {
        parsed,
        bound_key: key.map(str::to_string),
        sidebar,
    }
}

fn healthy_probe() -> HerdrProbe {
    probe(
        Some("0.8.0"),
        Some(entry(true, Some("0.8.0"), vec![])),
        None,
    )
}

fn healthy_config() -> ConfigStatus {
    config(true, Some("prefix+a"), SidebarState::Templated)
}

fn app_with(check: Check) -> App {
    let mut app = App::new(AppConfig {
        state: AppState::default(),
        profiles: Vec::new(),
    });
    app.plugin.checks = vec![check];
    app.plugin.cursor = 0;
    app
}

fn render(app: &App) -> (Vec<String>, ratatui::buffer::Buffer) {
    let mut term = Terminal::new(TestBackend::new(W, H)).unwrap();
    term.draw(|f| super::draw(f, f.area(), app)).unwrap();
    let buf = term.backend().buffer().clone();
    (crate::testutil::buffer_rows(&buf), buf)
}

/// The dot carries the verdict hue and the selector row shows `[f]` exactly when
/// the check offers one. `expected` is the health the state should render.
fn assert_row(check: Check, expected: Health, expect_fix: bool) {
    assert_eq!(
        check.fix.is_some(),
        expect_fix,
        "fix offer for {:?}",
        check.detail
    );
    let app = app_with(check);
    let (rows, buf) = render(&app);
    let row_idx = rows
        .iter()
        .position(|r| r.contains("● herdr"))
        .unwrap_or_else(|| panic!("no herdr selector row:\n{}", rows.join("\n")));
    let row = &rows[row_idx];

    // Buffer COLUMN, not byte offset — the caret and dot are multi-byte.
    let byte = row.find('●').expect("dot renders");
    let col = row[..byte].chars().count();
    // Map the verdict to its theme hue here, NOT via `health_color`, so a
    // regression in the mapping itself reddens the test instead of moving both
    // sides in lockstep.
    let want = match expected {
        Health::Ok => super::theme::success_color(),
        Health::Warn => super::theme::warning_color(),
        Health::Danger => super::theme::danger_color(),
        Health::Idle => super::theme::text_dim_color(),
    };
    assert_eq!(
        buf.content[row_idx * W as usize + col].fg,
        want,
        "dot hue for {:?}:\n{}",
        expected,
        rows.join("\n")
    );

    // Split at the two adjacent pane borders so the detail pane's own `[f]` line
    // (a different screen row) can't satisfy the selector-marker check.
    let selector = row.split("││").next().unwrap_or(row);
    assert_eq!(
        selector.contains("[f]"),
        expect_fix,
        "selector `[f]` marker for {:?}:\n{}",
        expected,
        rows.join("\n")
    );
}

// ── delegates pane ──────────────────────────────────────────────────────────────

/// A realistic wall clock rather than a round synthetic one: every row's state is
/// chosen by comparing its own stamps against this, and a year-2096 `now` routes
/// whole classes into one branch.
///
/// Only the PURE tests may pin it. A `TestBackend` render reaches the pane
/// through `draw`, which reads the real clock, so every render fixture below is
/// seeded relative to `now_ms()` and asserts what the clock cannot move — the
/// state words, the accounts, which fields are present, and the steer line. The
/// exact figures are pinned where `now` is an argument.
const NOW: u64 = 1_800_000_000_000;

fn app_with_delegates(delegates: Vec<jobs::StoredJob>) -> App {
    let mut app = App::new(AppConfig {
        state: AppState::default(),
        profiles: Vec::new(),
    });
    app.plugin.delegates = delegates;
    app
}

/// The streaming shape a real reserve writes: no wall clock, the default idle
/// guard.
fn running_spec(job_id: &str, profile: &str, started_at: u64, kind: RecordKind) -> RunningSpec {
    RunningSpec {
        job_id: job_id.to_string(),
        profile: profile.to_string(),
        started_at,
        recorded_at: started_at,
        timeout_secs: 0,
        endpoint: None,
        isolated: false,
        idle_secs: Some(300),
        kind,
    }
}

/// Seed one row of each state through the store's OWN writers, then list it back
/// — so the fixture is bytes the producer really emits rather than a struct
/// literal that agrees with whatever the fields are today.
fn seed_every_state(now: u64) -> Vec<jobs::StoredJob> {
    // Freshest first once listed: each anchor is further back than the last.
    jobs::write_heartbeat(
        &running_spec("d-bg-0", "uwuclxdy", now - 134_000, RecordKind::Collectable),
        now - 12_000,
        "reading the plan doc",
    )
    .unwrap();
    jobs::write_heartbeat(
        &running_spec("d-blk-0", "kerry", now - 40_000, RecordKind::Liveness),
        now - 30_000,
        "still thinking",
    )
    .unwrap();
    // Written as bytes rather than through `write_done`, which stamps the real
    // clock: when a job finished is exactly the field this row is dated by, so
    // the test has to choose it.
    let dir = jobs::jobs_dir().unwrap();
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("d-old-0.json"),
        serde_json::to_vec(&serde_json::json!({
            "job_id": "d-old-0",
            "profile": "DS8",
            "state": "done",
            "started_at": now - 1_800_000,
            "done_at": now - 900_000,
            "envelope": { "result": "finished a while back" },
        }))
        .unwrap(),
    )
    .unwrap();
    // Output landing on this very millisecond: the one input that reaches
    // `age_phrase`'s zero branch, without which the row reads `now ago`.
    jobs::write_heartbeat(
        &running_spec("d-fresh-0", "glm2", now - 5_000, RecordKind::Collectable),
        now,
        "just spoke",
    )
    .unwrap();
    // Silent far past the corpse window: its server is gone.
    jobs::write_running(&running_spec(
        "d-dead-0",
        "glm1",
        now - 90_000_000,
        RecordKind::Collectable,
    ))
    .unwrap();
    jobs::list_banded(now)
}

/// The exact figures, at a `now` the test owns: elapsed, last-output age and the
/// deadline that lands first, each from the fields the heartbeat already writes.
#[test]
fn a_delegate_row_carries_the_figures_its_own_record_holds() {
    let _home = crate::testutil::HomeSandbox::new();
    let cells = super::delegate_cells(&seed_every_state(NOW), NOW);
    let facts = |account: &str| -> String {
        cells
            .iter()
            .find(|c| c.profile == account)
            .unwrap_or_else(|| panic!("no row for `{account}`"))
            .facts
            .join(" · ")
    };
    assert_eq!(
        facts("uwuclxdy"),
        "elapsed 2m 14s · last output 12s ago · idle-kill in 4m 48s",
        "a running row counts from its own stamps",
    );
    assert_eq!(
        facts("kerry"),
        "elapsed 40s · last output 30s ago · idle-kill in 4m 30s",
        "and a blocking one reads identically — only its spelling differs",
    );
    assert_eq!(
        facts("glm2"),
        "elapsed 5s · last output just now · idle-kill in 5m",
        "a run that spoke this millisecond reads `just now`, never `now ago`",
    );
    assert_eq!(
        facts("DS8"),
        "finished 15m ago",
        "a finished job is dated by its finish, and says nothing it no longer has",
    );
    assert_eq!(
        facts("glm1"),
        "last seen 1d 1h ago",
        "a corpse by when it was last heard from",
    );
}

/// The three hues the four states map onto, off the styled buffer.
///
/// The pane had NO colour assertion at all until the `JobPhase` fold, and the
/// arm that mattered was `blocking`: it is LIVE but not COLLECTABLE, so a fold
/// that reconstructed the hue from `is_collectable()` instead of the live band
/// would have recoloured it to `done`'s success green with every test in the
/// repo still green. The rendered rows are plain strings; only the buffer holds
/// the style.
///
/// Mapped to the theme here rather than through `state_color`, so a regression
/// in that mapping reds this instead of moving both sides together — the same
/// rule `assert_row` plays by for the health dot.
#[test]
fn each_delegate_state_carries_its_own_hue() {
    let _home = crate::testutil::HomeSandbox::new();
    let app = app_with_delegates(seed_every_state(crate::usage::now_ms()));
    let (rows, buf) = render(&app);
    let screen = rows.join("\n");

    let hue = |account: &str| {
        let row_idx = rows
            .iter()
            .position(|r| r.contains(account))
            .unwrap_or_else(|| panic!("no row for `{account}`:\n{screen}"));
        let row = &rows[row_idx];
        // Buffer COLUMN, not byte offset — the dot is multi-byte.
        let byte = row.find(['●', '○']).expect("state dot renders");
        let col = row[..byte].chars().count();
        buf.content[row_idx * W as usize + col].fg
    };

    assert_eq!(
        hue("uwuclxdy"),
        super::theme::accent_color(),
        "a running delegate is accent:\n{screen}"
    );
    assert_eq!(
        hue("kerry"),
        super::theme::accent_color(),
        "and so is a blocking one — it bands with running, not with done:\n{screen}"
    );
    assert_eq!(
        hue("DS8"),
        super::theme::success_color(),
        "a finished job is success:\n{screen}"
    );
    assert_eq!(
        hue("glm1"),
        super::theme::text_dim_color(),
        "an orphan is dim; the word carries the charge:\n{screen}"
    );
}

#[test]
fn the_delegates_pane_names_each_state_and_carries_the_steer_line() {
    let _home = crate::testutil::HomeSandbox::new();
    let app = app_with_delegates(seed_every_state(crate::usage::now_ms()));
    let (rows, _) = render(&app);
    let screen = rows.join("\n");

    // Each row is identified by its own ACCOUNT, so a needle can never be read
    // off the wrong line.
    let row_for = |account: &str| -> String {
        rows.iter()
            .find(|r| r.contains(account))
            .unwrap_or_else(|| panic!("no row for `{account}`:\n{screen}"))
            .clone()
    };
    assert!(
        row_for("uwuclxdy").contains("● running"),
        "a background job reads as running:\n{screen}"
    );
    assert!(
        row_for("kerry").contains("● blocking"),
        "a run whose caller still holds the line reads apart from it:\n{screen}"
    );
    assert!(
        row_for("DS8").contains("● done"),
        "a finished job reads as done:\n{screen}"
    );
    assert!(
        row_for("glm1").contains("○ orphaned"),
        "and a corpse is drawn as one, never as live:\n{screen}"
    );

    // The liveness fields reach the screen. Their VALUES move with the real
    // clock this path reads, and are pinned exactly by the test above.
    let running = row_for("uwuclxdy");
    for needle in ["elapsed ", "last output ", "idle-kill in "] {
        assert!(
            running.contains(needle),
            "`{needle}` missing from the running row:\n{running}"
        );
    }
    assert!(
        running.contains("\"reading the"),
        "the delegate's own words ride last, quoted so they cannot read as \
         clauth's:\n{running}"
    );
    assert!(
        running.contains('…'),
        "and the tail is what gives way when the row runs out of width, rather \
         than pushing a figure off it:\n{running}"
    );
    assert!(
        row_for("DS8").contains("finished "),
        "a done row is dated by its finish:\n{screen}"
    );
    assert!(
        screen.contains("manage delegates in clauth app on web or mobile (coming soon)"),
        "the steer line renders under the list:\n{screen}"
    );
}

/// M9's own verify line: what the pane draws and what `monitor` tells the model
/// about ONE record must not be able to disagree.
///
/// Every figure asserted here is read OUT of `monitor`'s payload and then looked
/// for in the rendered row, so a pane that grew a second copy of the arithmetic
/// reds this even when its own numbers look plausible.
#[test]
fn the_delegates_pane_reports_what_monitor_reports_for_the_same_record() {
    let _home = crate::testutil::HomeSandbox::new();
    // A pinned-`--output-format` run, so BOTH deadlines are present and the pane
    // has to pick the one that lands first — and one handed off mid-flight, so
    // `recorded_at` sits well after `started_at`. That gap is what makes the
    // test discriminate: on a record where the two are equal (every job that
    // started out background), a pane counting elapsed from the wrong field
    // agrees with `monitor` by accident.
    let spec = RunningSpec {
        timeout_secs: 900,
        idle_secs: Some(300),
        recorded_at: NOW - 120_000,
        ..running_spec(
            "d-pin-0",
            "uwuclxdy",
            NOW - 200_000,
            RecordKind::Collectable,
        )
    };
    jobs::write_heartbeat(&spec, NOW - 47_000, "mid-run").unwrap();

    let stored = jobs::list_banded(NOW);
    let record: &JobRecord = &stored[0].record;
    assert_eq!(record.state, JobState::Running, "fixture control");

    let payload = crate::mcp::running_payload_for_test(&record.job_id, record, NOW);
    let secs = |key: &str| {
        payload
            .get(key)
            .and_then(serde_json::Value::as_u64)
            .unwrap_or_else(|| panic!("monitor reports {key}: {payload}"))
    };
    let cells = super::delegate_cells(&stored, NOW);
    let facts = cells[0].facts.join(" · ");

    assert!(
        facts.contains(&format!(
            "elapsed {}",
            crate::usage::humanize_duration(secs("elapsed_secs") as i64)
        )),
        "elapsed disagrees with monitor's {payload}: {facts}"
    );
    assert!(
        facts.contains(&format!(
            "last output {} ago",
            crate::usage::humanize_duration(secs("last_output_secs_ago") as i64)
        )),
        "last-output age disagrees with monitor's {payload}: {facts}"
    );
    // The idle guard lands first on this fixture, so that is the countdown the
    // row spends its one cell on — and it is monitor's own figure.
    let idle = secs("idle_kill_in_secs");
    assert!(
        idle < secs("wall_kill_in_secs"),
        "fixture control: the idle guard is the deadline that fires: {payload}"
    );
    assert!(
        facts.contains(&format!(
            "idle-kill in {}",
            crate::usage::humanize_duration(idle as i64)
        )),
        "the next deadline disagrees with monitor's {payload}: {facts}"
    );
}

/// A record an older server wrote carries no deadlines at all, and both surfaces
/// have to say so rather than counting down from a default. Real bytes: the
/// shape is one only a previous version emitted, so a struct literal would agree
/// with whatever the fields are today and prove nothing about the wire.
#[test]
fn a_record_from_an_older_server_reads_as_liveness_not_recorded() {
    let _home = crate::testutil::HomeSandbox::new();
    let dir = jobs::jobs_dir().unwrap();
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("d-legacy-0.json"),
        format!(
            r#"{{"job_id":"d-legacy-0","profile":"uwuclxdy","state":"running","started_at":{}}}"#,
            NOW - 61_000
        ),
    )
    .unwrap();

    let cells = super::delegate_cells(&jobs::list_banded(NOW), NOW);
    let facts = cells[0].facts.join(" · ");
    assert!(
        facts.contains("elapsed 1m 1s"),
        "the one figure such a record still supports: {facts}"
    );
    assert!(
        facts.contains("liveness not recorded"),
        "and the rest is named absent, never counted down from a default: {facts}"
    );
    assert!(
        !facts.contains("kill in"),
        "no deadline is invented for it: {facts}"
    );
}

/// More delegates than the pane can hold: the last row says how many did not
/// fit. A scrollbar would be the contract's overflow signal, but this pane binds
/// no key, so it would advertise a scroll that cannot happen.
#[test]
fn the_delegates_pane_marks_its_overflow_with_a_count() {
    let _home = crate::testutil::HomeSandbox::new();
    let now = crate::usage::now_ms();
    for i in 0..9 {
        jobs::write_heartbeat(
            &running_spec(
                &format!("d-many-{i}"),
                &format!("acct{i}"),
                now - 10_000 - i as u64,
                RecordKind::Collectable,
            ),
            now - 1_000 - i as u64,
            "working",
        )
        .unwrap();
    }
    let app = app_with_delegates(jobs::list_banded(now));
    let (rows, _) = render(&app);
    let screen = rows.join("\n");

    assert!(
        screen.contains("acct0"),
        "the newest delegate is the one kept:\n{screen}"
    );
    assert!(
        !screen.contains("acct8"),
        "the oldest is the one dropped:\n{screen}"
    );
    let marker = rows
        .iter()
        .find(|r| r.contains("more"))
        .unwrap_or_else(|| panic!("no overflow marker:\n{screen}"));
    assert!(
        marker.contains("+4 more"),
        "the marker counts what did not fit, and 9 rows into a 5-row list leaves \
         4: {marker}"
    );
}

/// A live row must survive the truncation that a burst of finished ones causes.
///
/// `jobs::list` orders on the retention anchor, which for a `done` record is its
/// FINISH — so every background job that landed a second ago outranks a blocking
/// run that last spoke twenty seconds ago. On anchor order alone the row this
/// pane exists for is the first one evicted, and the pane binds no key, so
/// nothing reaches it afterwards.
#[test]
fn a_live_delegate_outranks_finished_ones_however_recently_they_landed() {
    let _home = crate::testutil::HomeSandbox::new();
    let now = crate::usage::now_ms();
    // The row the whole pane exists for: a blocking run, three minutes in.
    jobs::write_heartbeat(
        &running_spec("d-blk-0", "kerry", now - 180_000, RecordKind::Liveness),
        now - 20_000,
        "still thinking",
    )
    .unwrap();
    // A second live row, anchored NEWER than every finished one. Without it the
    // live row is also the OLDEST record in the store, and a mutant that merely
    // reverses the anchor order bands correctly by accident — the same
    // "the mutant is non-equivalent and the fixture cannot tell" shape this file
    // already records for pdqsort. Interleaved, no monotone reordering of the
    // anchor can produce the banded answer.
    jobs::write_heartbeat(
        &running_spec("d-run-9", "fresh", now - 30_000, RecordKind::Collectable),
        now - 500,
        "just spoke",
    )
    .unwrap();
    let dir = jobs::jobs_dir().unwrap();
    std::fs::create_dir_all(&dir).unwrap();
    for i in 0..7 {
        std::fs::write(
            dir.join(format!("d-bg-{i}.json")),
            serde_json::to_vec(&serde_json::json!({
                "job_id": format!("d-bg-{i}"),
                "profile": format!("bg{i}"),
                "state": "done",
                "started_at": now - 60_000,
                "done_at": now - 1_000,
                "envelope": { "result": "landed" },
            }))
            .unwrap(),
        )
        .unwrap();
    }

    let app = app_with_delegates(jobs::list_banded(now));
    let (rows, _) = render(&app);
    let screen = rows.join("\n");

    assert!(
        rows.iter().any(|r| r.contains("more")),
        "fixture control: more delegates than the pane holds, so something is \
         evicted:\n{screen}"
    );
    assert!(
        screen.contains("kerry") && screen.contains("fresh"),
        "and it is never a live one — BOTH survive, whatever their anchors:\n{screen}"
    );
    assert!(
        rows.iter()
            .any(|r| r.contains("kerry") && r.contains("● blocking")),
        "which still reads as what it is:\n{screen}"
    );
}

/// The height negotiation, pinned from BOTH sides of every constant it reads.
/// No render test reaches it — every one of them runs at 100x24, where the room
/// always exceeds what the pane wants — so without this the whole
/// drop-rather-than-clip half of the function is unexecuted, including the floor
/// pairing its own doc comment leans on.
#[test]
fn the_delegates_pane_drops_whole_rather_than_clipping_when_the_tab_needs_the_rows() {
    use super::delegates_height;

    // An empty store wants the empty state's own 4 rows plus chrome, and the
    // empty state is the one thing that cannot be shown in part.
    assert_eq!(delegates_height(0, 19), 0, "one row short: the pane drops");
    assert_eq!(delegates_height(0, 20), 7, "exactly enough: it draws");

    // One delegate wants 4 rows, which is under the clipping floor — the floor
    // must not refuse a pane that already fits.
    assert_eq!(delegates_height(1, 16), 0);
    assert_eq!(
        delegates_height(1, 17),
        4,
        "a pane that fits is not refused"
    );

    // Two or more want at least 5, which IS the floor, so the list can never be
    // left with a single row holding nothing but an overflow marker.
    assert_eq!(delegates_height(2, 17), 0);
    assert_eq!(delegates_height(2, 18), 5);

    // Past the cap the pane stops growing and the marker carries the rest.
    assert_eq!(
        delegates_height(9, 24),
        9,
        "capped at DELEGATE_ROWS_MAX + chrome"
    );
    assert_eq!(
        delegates_height(9, 200),
        9,
        "and a tall terminal buys no more"
    );
    assert_eq!(
        delegates_height(9, 20),
        7,
        "while a short one clips to what is left, never past it",
    );
}

/// Within a band the listing's newest-first order has to survive the band sort.
///
/// The fixture INTERLEAVES the two bands, which is what makes it a real
/// permutation rather than a no-op: on an input already grouped by rank, pdqsort
/// short-circuits and an unstable sort returns the same vector, so a same-rank or
/// pre-grouped fixture cannot fail whatever the sort does.
#[test]
fn the_band_sort_keeps_each_band_newest_first() {
    let _home = crate::testutil::HomeSandbox::new();
    let now = crate::usage::now_ms();
    let dir = jobs::jobs_dir().unwrap();
    std::fs::create_dir_all(&dir).unwrap();
    for i in 0..20u64 {
        jobs::write_heartbeat(
            &running_spec(
                &format!("d-run-{i:02}"),
                &format!("run{i:02}"),
                now - 900_000,
                RecordKind::Collectable,
            ),
            now - 1_000 - i * 1_000,
            "working",
        )
        .unwrap();
        std::fs::write(
            dir.join(format!("d-dun-{i:02}.json")),
            serde_json::to_vec(&serde_json::json!({
                "job_id": format!("d-dun-{i:02}"),
                "profile": format!("dun{i:02}"),
                "state": "done",
                "started_at": now - 900_000,
                "done_at": now - 1_500 - i * 1_000,
            }))
            .unwrap(),
        )
        .unwrap();
    }

    let order: Vec<String> = super::delegate_cells(&jobs::list_banded(now), now)
        .into_iter()
        .map(|c| c.profile)
        .collect();
    let (live, finished) = order.split_at(20);
    assert!(
        live.iter().all(|n| n.starts_with("run")),
        "the live band comes first, whole: {order:?}"
    );
    let want_live: Vec<String> = (0..20).map(|i| format!("run{i:02}")).collect();
    assert_eq!(live, want_live, "and newest-first inside it");
    let want_done: Vec<String> = (0..20).map(|i| format!("dun{i:02}")).collect();
    assert_eq!(
        finished, want_done,
        "the finished band keeps its own order too, which is what decides which \
         rows the overflow marker swallows",
    );
}

/// An empty store still renders the pane, so the steer line is reachable before
/// anyone has ever run a delegate.
#[test]
fn the_delegates_pane_renders_its_empty_state_with_the_steer_line() {
    let _home = crate::testutil::HomeSandbox::new();
    let app = app_with_delegates(Vec::new());
    let (rows, _) = render(&app);
    let screen = rows.join("\n");

    assert!(
        screen.contains("no delegates"),
        "the empty state renders:\n{screen}"
    );
    assert!(
        screen.contains("manage delegates in clauth app on web or mobile (coming soon)"),
        "and the steer line with it:\n{screen}"
    );
}

#[test]
fn herdr_row_renders_ok_dot_without_fix() {
    let _home = crate::testutil::HomeSandbox::new();
    let check = herdr_check(&healthy_probe(), Some(&healthy_config()));
    assert_row(check, Health::Ok, false);
}

#[test]
fn herdr_row_renders_danger_dot_on_registry_warnings() {
    let _home = crate::testutil::HomeSandbox::new();
    let probe = probe(
        Some("0.8.0"),
        Some(entry(true, None, vec!["plugin root is gone"])),
        None,
    );
    let check = herdr_check(&probe, Some(&healthy_config()));
    assert_row(check, Health::Danger, false);
}

#[test]
fn herdr_row_renders_danger_dot_on_registry_error() {
    let _home = crate::testutil::HomeSandbox::new();
    let probe = probe(
        Some("0.8.0"),
        None,
        Some("herdr's plugin list did not parse"),
    );
    let check = herdr_check(&probe, Some(&healthy_config()));
    assert_row(check, Health::Danger, false);
}

#[test]
fn herdr_row_renders_warn_dot_without_fix_when_not_installed() {
    let _home = crate::testutil::HomeSandbox::new();
    let check = herdr_check(&probe(Some("0.8.0"), None, None), Some(&healthy_config()));
    assert_row(check, Health::Warn, false);
}

#[test]
fn herdr_row_renders_warn_dot_without_fix_when_version_too_old() {
    let _home = crate::testutil::HomeSandbox::new();
    let probe = probe(
        Some("0.7.0"),
        Some(entry(true, Some("0.8.0"), vec![])),
        None,
    );
    let check = herdr_check(&probe, Some(&healthy_config()));
    assert_row(check, Health::Warn, false);
}

#[test]
fn herdr_row_renders_warn_dot_without_fix_when_config_does_not_parse() {
    let _home = crate::testutil::HomeSandbox::new();
    let check = herdr_check(
        &healthy_probe(),
        Some(&config(false, None, SidebarState::Absent)),
    );
    assert_row(check, Health::Warn, false);
}

#[test]
fn herdr_row_renders_warn_dot_and_offers_fix_when_key_unbound() {
    let _home = crate::testutil::HomeSandbox::new();
    let check = herdr_check(
        &healthy_probe(),
        Some(&config(true, None, SidebarState::Templated)),
    );
    assert_row(check, Health::Warn, true);
}

#[test]
fn herdr_row_renders_warn_dot_and_offers_fix_when_sidebar_untemplated() {
    let _home = crate::testutil::HomeSandbox::new();
    let check = herdr_check(
        &healthy_probe(),
        Some(&config(true, Some("prefix+a"), SidebarState::Absent)),
    );
    assert_row(check, Health::Warn, true);
}

#[test]
fn herdr_row_renders_warn_dot_without_fix_when_config_unreadable() {
    let _home = crate::testutil::HomeSandbox::new();
    let check = herdr_check(&healthy_probe(), None);
    assert_row(check, Health::Warn, false);
}

// ── herdr options ─────────────────────────────────────────────────────────────

/// The herdr detail with its options section: the probe + config verdict the
/// recompute caches, the check built from them, focus descended into the
/// detail so the rows render focusable.
fn herdr_options_app(config: ConfigStatus) -> App {
    let mut app = App::new(AppConfig {
        state: AppState::default(),
        profiles: Vec::new(),
    });
    let probe = healthy_probe();
    app.plugin.herdr = Some(Some(probe.clone()));
    app.plugin.herdr_config = Some(config.clone());
    app.plugin.checks = vec![herdr_check(&probe, Some(&config))];
    app.plugin.cursor = 0;
    app.plugin.focus = crate::tui::app::PluginFocus::Detail;
    app
}

/// The six rows render their real values and glyphs on BOTH tiers — the toggle
/// glyph is the one control the tier changes, so each tier pins its own.
#[test]
fn herdr_options_render_all_six_rows_on_both_tiers() {
    let _home = crate::testutil::HomeSandbox::new();
    let app = herdr_options_app(healthy_config());
    let row_with = |rows: &[String], label: &str| -> String {
        rows.iter()
            .find(|r| r.contains(label))
            .unwrap_or_else(|| panic!("no `{label}` row"))
            .clone()
    };

    let full = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let (rows, _) = render(&app);
    let screen = rows.join("\n");
    assert!(screen.contains("OPTIONS"), "the eyebrow renders:\n{screen}");
    assert!(
        row_with(&rows, "popup width").contains("popup width  [fit]  half  split-right  split-top"),
        "the focused cycle row brackets its selection:\n{screen}"
    );
    assert!(
        row_with(&rows, "pane tag").contains("─●"),
        "pane tag on:\n{screen}"
    );
    assert!(
        row_with(&rows, "tag refresh").contains("5s"),
        "tag refresh default 5s:\n{screen}"
    );
    assert!(
        row_with(&rows, "border label").contains("○─"),
        "border label off:\n{screen}"
    );
    assert!(
        row_with(&rows, "delegate dot").contains("─●"),
        "delegate dot on:\n{screen}"
    );
    assert!(
        row_with(&rows, "delegate row text").contains("○─"),
        "delegate row text off:\n{screen}"
    );
    drop(full);

    let compatible = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Compatible);
    let (rows, _) = render(&app);
    let screen = rows.join("\n");
    assert!(
        row_with(&rows, "pane tag").contains("[on]"),
        "pane tag [on]:\n{screen}"
    );
    assert!(
        row_with(&rows, "border label").contains("[off]"),
        "border label [off]:\n{screen}"
    );
    assert!(
        row_with(&rows, "delegate dot").contains("[on]"),
        "delegate dot [on]:\n{screen}"
    );
    assert!(
        row_with(&rows, "delegate row text").contains("[off]"),
        "delegate row text [off]:\n{screen}"
    );
    drop(compatible);
}

/// While focus sits on the selector, the option rows render blurred: no caret,
/// and the cycle row carries its selection by color alone (no brackets).
#[test]
fn herdr_options_render_blurred_when_focus_sits_on_the_selector() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = herdr_options_app(healthy_config());
    app.plugin.focus = crate::tui::app::PluginFocus::List;
    let (rows, _) = render(&app);
    let screen = rows.join("\n");
    let width_row = rows
        .iter()
        .find(|r| r.contains("popup width"))
        .unwrap_or_else(|| panic!("no popup width row:\n{screen}"));
    assert!(
        width_row.contains("popup width  fit  half  split-right  split-top"),
        "a blurred cycle row drops its brackets:\n{screen}"
    );
    assert!(
        !width_row.contains('❯'),
        "the caret renders only inside the focused pane:\n{screen}"
    );
}

/// The `delegate row text` row renders whole-faint with a tooltip while focused
/// when herdr's config does not parse — the one state where the write it would
/// trigger cannot happen. The hue is pinned off the styled buffer, not the
/// glyph text.
#[test]
fn delegate_row_text_renders_inert_with_tooltip_when_herdr_config_does_not_parse() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = herdr_options_app(config(false, None, SidebarState::Absent));
    app.plugin.herdr_options_cursor = 5;
    let (rows, buf) = render(&app);
    let screen = rows.join("\n");

    let row_idx = rows
        .iter()
        .position(|r| r.contains("delegate row text"))
        .unwrap_or_else(|| panic!("no delegate row text row:\n{screen}"));
    let row = &rows[row_idx];
    assert!(
        screen.contains("herdr's config doesn't parse, so clauth can't rewrite the row"),
        "the tooltip renders under the focused inert row:\n{screen}"
    );
    for needle in ["❯", "delegate row text"] {
        let byte = row
            .find(needle)
            .unwrap_or_else(|| panic!("no `{needle}`:\n{row}"));
        let col = row[..byte].chars().count();
        assert_eq!(
            buf.content[row_idx * W as usize + col].fg,
            super::theme::text_faint_color(),
            "`{needle}` renders faint on the inert row:\n{screen}"
        );
    }
}

/// The tag-refresh editor renders the edit gutter, the sunken buffer with its
/// unit, and the range sub-line — the Config-tab refresh editor's shape.
#[test]
fn herdr_tag_refresh_editor_renders_the_edit_state() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = herdr_options_app(healthy_config());
    app.plugin.herdr_options_cursor = 2;
    app.plugin.herdr_tag_draft = Some(crate::tui::app::InputState::new("5"));
    let (rows, _) = render(&app);
    let screen = rows.join("\n");
    let tag_row = rows
        .iter()
        .find(|r| r.contains("tag refresh"))
        .unwrap_or_else(|| panic!("no tag refresh row:\n{screen}"));
    assert!(
        tag_row.contains("✎ tag refresh  5 s"),
        "the edit gutter + buffer + unit render:\n{screen}"
    );
    assert!(
        screen.contains("min is 1 s"),
        "the range sub-line renders while typing:\n{screen}"
    );
}
