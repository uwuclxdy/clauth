use super::*;
use crate::profile::{AppConfig, AppState, Profile, ProfileName};
use crate::status::{Impact, Incident, IncidentUpdate, UpdatePhase};
use crate::tui::app::{App, Tab};
use crate::usage::{UsageInfo, UsageWindow};
use std::collections::BTreeMap;

/// A wide glyph component name fits its column whole: the name column is sized
/// by cells, so the wide name renders `日本語` instead of being cut to `日…` by a
/// char-derived width.
#[test]
fn transition_lines_keep_a_wide_name_whole() {
    let lines = transition_lines(
        &[(
            "日本語".to_string(),
            "operational".to_string(),
            "degraded".to_string(),
        )],
        0,
        60,
    );
    let text: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
    assert_eq!(text, "日本語  → degraded");
}

fn oauth(name: &str) -> Profile {
    Profile {
        name: name.into(),
        base_url: None,
        api_key: None,
        auto_start: true,
        usage_probe: false,
        usage_probe_interval_ms: crate::profile::DEFAULT_USAGE_PROBE_INTERVAL_MS,
        env: BTreeMap::new(),
        models: Default::default(),
        effort: None,
        fallback_threshold: Some(80.0),
        weekly_threshold: None,
        last_resort: false,
        preferred: false,
        preferred_days: Vec::new(),
        plan_ends: None,
        plan_renews: None,
        rolling_token: false,
        max_auto_spend: None,
        check_weekly: true,
        check_scoped: true,
        check_credit_cap: true,
        bell_threshold: None,
        disabled: false,
        console: None,
        credentials: None,
        has_long_lived_sidecar: false,
        unreadable_credentials_store: None,
        setup_token_digest: None,
        provider: None,
        typed_endpoint: Default::default(),
    }
}

fn status_app() -> App {
    let five_hour = [
        ("abcdefghijklmnopqrstuvwxyz", 12.0),
        ("cjk-日本語アカウント", 49.0),
        ("emoji-🧭⚠️-account", 63.0),
    ];
    let profiles: Vec<Profile> = five_hour.iter().map(|(name, _)| oauth(name)).collect();
    let profile_names: Vec<ProfileName> = profiles.iter().map(|p| p.name.clone()).collect();
    let mut app = App::new(AppConfig {
        state: AppState {
            active_profile: Some("cjk-日本語アカウント".into()),
            profiles: profile_names,
            ..AppState::default()
        },
        profiles,
    });
    for (name, pct) in five_hour {
        app.usage_store.lock().unwrap().insert(
            name.to_string(),
            UsageInfo {
                five_hour: Some(UsageWindow {
                    utilization: pct,
                    resets_at: None,
                }),
                ..UsageInfo::default()
            },
        );
    }
    app.tab = Tab::Status;
    app.status.incidents = vec![Incident {
        id: "6ptd5skgmy3v".into(),
        title: "elevated error rates on the api and console for 日本語 users".into(),
        link: "https://stspg.io/abc".into(),
        phase: UpdatePhase::Monitoring,
        impact: Impact::Minor,
        started_ms: 1_780_740_881_000,
        resolved_ms: None,
        components: vec![
            ("claude.ai".into(), "operational".into()),
            ("日本語サービス".into(), "degraded_performance".into()),
            ("🧭⚠️-router".into(), "major_outage".into()),
        ],
        updates: vec![IncidentUpdate {
            phase: UpdatePhase::Monitoring,
            at_ms: 1_780_741_000_000,
            text: "a fix has been implemented and we are monitoring".into(),
            transitions: vec![
                (
                    "日本語サービス".into(),
                    "degraded_performance".into(),
                    "operational".into(),
                ),
                (
                    "🧭⚠️-router".into(),
                    "major_outage".into(),
                    "degraded_performance".into(),
                ),
            ],
        }],
    }];
    app.status.cursor = 0;
    app
}

fn frame_rows(app: &App, w: u16, h: u16) -> Vec<String> {
    let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(w, h)).unwrap();
    term.draw(|f| crate::tui::render::draw(f, app)).unwrap();
    crate::testutil::buffer_rows(term.backend().buffer())
        .into_iter()
        .map(|r| r.trim_end().to_string())
        .collect()
}

/// The status detail's header pills, `started` stamp, stacked timeline and
/// component-transition rows render whole (or stack to their own line) at every
/// width from the floor up — never a clipped pill, stamp or transition name.
/// Pinned whole-row at 27/28/34/45/80/160 with an ASCII title, a CJK component
/// and an emoji component.
#[test]
fn the_status_detail_surfaces_render_whole_at_narrow_widths() {
    let _home = crate::testutil::HomeSandbox::new();
    let app = status_app();
    // The started/update stamps render through `chrono::Local`, so the pins
    // build them with the same formatter instead of hard-coding one zone's
    // wall clock (which red on any other box timezone). The header AGE is the
    // contract's ISO date (date only), distinct from the full `started` stamp.
    let started = crate::format::local_stamp(1_780_740_881)
        .expect("the started stamp resolves to a local wall clock");
    let started_date = crate::format::local_date(1_780_740_881)
        .expect("the started date resolves to a local wall clock");
    let at = crate::format::local_stamp(1_780_741_000)
        .expect("the update stamp resolves to a local wall clock");
    let stacked: Vec<(u16, Vec<String>)> = vec![
        (
            27,
            vec![
                "│ [ monitoring ]  [ minor │".to_string(),
                format!("│   {started_date}            │"),
                "│   ongoing               │".to_string(),
                "│ started                 │".to_string(),
                format!("│   {started}   │"),
                "│ components   ● claude.a │".to_string(),
                "│ https://stspg.io/abc    │".to_string(),
                "│ TIMELINE                │".to_string(),
                format!("│ {at}     │"),
                "│ monitoring              │".to_string(),
                "│ a fix has been          │".to_string(),
                "│ implemented and we are  │".to_string(),
                "│ monitoring              │".to_string(),
                "│ 日 本 語 …   → operational │".to_string(),
                "│ 🧭 ⚠️ -ro…  → degraded    │".to_string(),
            ],
        ),
        (
            28,
            vec![
                "│ [ monitoring ]  [ minor  │".to_string(),
                format!("│   {started_date}             │"),
                "│   ongoing                │".to_string(),
                "│ started                  │".to_string(),
                format!("│   {started}    │"),
                "│ components   ● claude.ai │".to_string(),
                "│ https://stspg.io/abc     │".to_string(),
                "│ TIMELINE                 │".to_string(),
                format!("│ {at}      │"),
                "│ monitoring               │".to_string(),
                "│ a fix has been           │".to_string(),
                "│ implemented and we are   │".to_string(),
                "│ monitoring               │".to_string(),
                "│ 日 本 語 サ …  → operational │".to_string(),
                "│ 🧭 ⚠️ -rou…  → degraded    │".to_string(),
            ],
        ),
        (
            34,
            vec![
                "│ [ monitoring ]  [ minor ]      │".to_string(),
                format!("│   {started_date}                   │"),
                "│   ongoing                      │".to_string(),
                "│ started                        │".to_string(),
                format!("│   {started}          │"),
                "│ components   ● claude.ai  +2   │".to_string(),
                "│ https://stspg.io/abc           │".to_string(),
                "│ TIMELINE                       │".to_string(),
                format!("│ {at}            │"),
                "│ monitoring                     │".to_string(),
                "│ a fix has been implemented and │".to_string(),
                "│ we are monitoring              │".to_string(),
                "│ 日 本 語 サ ー ビ ス   → operational  │".to_string(),
                "│ 🧭 ⚠️ -router     → degraded     │".to_string(),
            ],
        ),
        (
            45,
            vec![
                format!("│ [ monitoring ]  [ minor ]  {started_date}     │"),
                "│   ongoing                                 │".to_string(),
                format!("│ started      {started}          │"),
                "│ components   ● claude.ai  ● 日 本 語 サ ー ビ   │".to_string(),
                "│ https://stspg.io/abc                      │".to_string(),
                "│ TIMELINE                                  │".to_string(),
                format!("│ {at} monitoring            │"),
                "│ a fix has been implemented and we are     │".to_string(),
                "│ monitoring                                │".to_string(),
                "│ 日 本 語 サ ー ビ ス   → operational             │".to_string(),
                "│ 🧭 ⚠️ -router     → degraded                │".to_string(),
            ],
        ),
    ];
    for (w, expected) in stacked {
        let rows = frame_rows(&app, w, 40);
        let start = rows
            .iter()
            .position(|r| r.contains("╭─ elevated error rates"))
            .unwrap_or_else(|| panic!("detail title missing at {w}"));
        let end = rows[start + 1..]
            .iter()
            .position(|r| r.starts_with("╰─"))
            .map(|i| start + 1 + i)
            .unwrap_or_else(|| panic!("detail bottom missing at {w}"));
        let got: Vec<String> = rows[start + 1..end]
            .iter()
            .filter(|r| !r.trim_matches(['│', ' ']).is_empty())
            .cloned()
            .collect();
        assert_eq!(got, expected, "status detail panel at {w} cols");
    }
}

/// The incident detail is named after the selected incident's own identity, so
/// its title keeps the incident's original case (the Panel proper-noun
/// exception); the no-selection fallback stays an uppercase structural title.
#[test]
fn the_incident_detail_title_keeps_the_incident_case() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = status_app();
    // A real status.claude.com title is mixed case, so "keeps the case" must
    // mean exactly these bytes — a forced lowercase would pass an all-lowercase
    // fixture and a forced uppercase would pass a structural label.
    app.status.incidents[0].title = "Elevated Errors on Claude.ai".into();
    let rows = frame_rows(&app, 160, 40);
    let title = rows
        .iter()
        .find(|r| r.contains("╭─ Elevated Errors on Claude.ai"))
        .expect("the detail title renders");
    assert!(
        title.contains("╭─ Elevated Errors on Claude.ai ─"),
        "the incident identity keeps its case: {title:?}"
    );

    // No selection → the structural `status` title stays uppercase.
    let mut bare = status_app();
    bare.status.incidents.clear();
    let rows = frame_rows(&bare, 160, 40);
    assert!(
        rows.iter().any(|r| r.contains(" STATUS ")),
        "the structural fallback title stays uppercase: {rows:?}"
    );
}

/// A panel title wider than its top border ends in `…` inside the border
/// instead of running into the corner.
#[test]
fn a_panel_title_that_cannot_fit_ends_in_ellipsis_inside_the_border() {
    let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(34, 3)).unwrap();
    term.draw(|f| {
        let block = crate::tui::render::panes::section_box(
            "elevated error rates on the api and console for 日本語 users",
            false,
            false,
            f.area().width,
        );
        f.render_widget(block, f.area());
    })
    .unwrap();
    let rows = crate::testutil::buffer_rows(term.backend().buffer());
    assert_eq!(
        rows[0], "╭─ ELEVATED ERROR RATES ON THE … ╮",
        "34-col title ends in … inside the border"
    );
    let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(45, 3)).unwrap();
    term.draw(|f| {
        let block = crate::tui::render::panes::section_box(
            "elevated error rates on the api and console for 日本語 users",
            false,
            false,
            f.area().width,
        );
        f.render_widget(block, f.area());
    })
    .unwrap();
    let rows = crate::testutil::buffer_rows(term.backend().buffer());
    assert_eq!(
        rows[0], "╭─ ELEVATED ERROR RATES ON THE API AND CON… ╮",
        "45-col title ends in … inside the border"
    );
}

/// The loading title budget subtracts the trailing spinner suffix (2 cells),
/// so a title within 2 cells of the border budget ends in `…` instead of
/// pushing the spinner into the top-right corner.
#[test]
fn a_loading_title_leaves_room_for_the_spinner_suffix() {
    let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(30, 3)).unwrap();
    term.draw(|f| {
        let block = crate::tui::render::panes::section_box_loading(
            "ABCDEFGHIJKLMNOPQRSTUVWX",
            false,
            false,
            "⠋",
            f.area().width,
        );
        f.render_widget(block, f.area());
    })
    .unwrap();
    let rows = crate::testutil::buffer_rows(term.backend().buffer());
    assert_eq!(
        rows[0], "╭─ ABCDEFGHIJKLMNOPQRSTUV… ⠋ ╮",
        "the spinner keeps the corner and the title ends in …"
    );
}

/// Desktop pin: the status detail's side-by-side rows replace the pre-S9 clip —
/// the CJK component-transition name was measured by char count, clipping the
/// name to `日 本 …` and `operational` to `operation`, and mis-aligning the arrow.
#[test]
fn the_status_detail_desktop_rows_replace_the_pre_s9_clip() {
    let _home = crate::testutil::HomeSandbox::new();
    let app = status_app();
    let started = crate::format::local_stamp(1_780_740_881)
        .expect("the started stamp resolves to a local wall clock");
    let started_date = crate::format::local_date(1_780_740_881)
        .expect("the started date resolves to a local wall clock");
    let at = crate::format::local_stamp(1_780_741_000)
        .expect("the update stamp resolves to a local wall clock");
    let rows = frame_rows(&app, 80, 40);
    let got: Vec<String> = rows
        .iter()
        .filter(|r| {
            r.contains("[ monitoring ]")
                || (r.contains("started") && !r.contains(&at))
                || r.contains(&at)
                || r.contains("→ operational")
                || r.contains("→ degraded")
        })
        .cloned()
        .collect();
    assert_eq!(
        got,
        vec![
            format!(
                "│ ❯ elevated error ra… ││ [ monitoring ]  [ minor ]  {started_date}        ongoing │"
            ),
            format!("│        ● monitoring  ││ started      {started}                     │"),
            format!("│                      ││ {at} monitoring    a fix has been     │"),
            "│                      ││                                   日 …  → operational │"
                .to_string(),
            "│                      ││                                   🧭 …  → degraded    │"
                .to_string(),
        ],
        "status detail rows at 80 cols"
    );
}

/// Desktop pin: the status detail's side-by-side transition rows render whole
/// before and after at 160 — pre-S9 sized the transition column by char count,
/// only shifting the layout. Whole before, re-measured by cells now, not clips
/// replaced.
#[test]
fn the_status_detail_desktop_rows_measure_by_cells() {
    let _home = crate::testutil::HomeSandbox::new();
    let app = status_app();
    let started = crate::format::local_stamp(1_780_740_881)
        .expect("the started stamp resolves to a local wall clock");
    let started_date = crate::format::local_date(1_780_740_881)
        .expect("the started date resolves to a local wall clock");
    let at = crate::format::local_stamp(1_780_741_000)
        .expect("the update stamp resolves to a local wall clock");
    let rows = frame_rows(&app, 160, 40);
    let got: Vec<String> = rows
        .iter()
        .filter(|r| {
            r.contains("[ monitoring ]")
                || (r.contains("started") && !r.contains(&at))
                || r.contains(&at)
                || r.contains("→ operational")
                || r.contains("→ degraded")
        })
        .cloned()
        .collect();
    assert_eq!(
        got,
        vec![
            format!("│ ❯ elevated error rates on the api a… ││ [ monitoring ]  [ minor ]  {started_date}                                                                        ongoing │"),
            format!("│   [ minor ]            ● monitoring  ││ started      {started}                                                                                     │"),
            format!("│                                      ││ {at} monitoring    a fix has been implemented and we are monitoring                                   │"),
            "│                                      ││                                   日 本 語 サ ー ビ ス   → operational                                                      │".to_string(),
            "│                                      ││                                   🧭 ⚠️ -router     → degraded                                                         │".to_string(),
        ],
        "status detail rows at 160 cols"
    );
}

fn ongoing(phase: UpdatePhase, impact: Impact) -> Incident {
    Incident {
        id: "x".into(),
        title: "an incident".into(),
        link: String::new(),
        phase,
        impact,
        started_ms: 1_780_740_881_000,
        resolved_ms: None,
        components: Vec::new(),
        updates: Vec::new(),
    }
}

fn line_text(line: &Line<'_>) -> String {
    line.spans.iter().map(|s| s.content.as_ref()).collect()
}

/// The foreground of each char of `line`, in order (every glyph here is one cell).
fn char_fgs(line: &Line<'_>) -> Vec<(char, Option<ratatui::style::Color>)> {
    line.spans
        .iter()
        .flat_map(|s| s.content.chars().map(move |c| (c, s.style.fg)))
        .collect()
}

/// The rendered cell width of `line`: every span measured the way the buffer
/// draws it, not by `Span::width` (which folds a lam-alef ligature to one cell).
fn line_cells(line: &Line<'_>) -> usize {
    line.spans.iter().map(|s| grapheme_cells(&s.content)).sum()
}

/// An ongoing incident's selector row names its phase beside the dot: the dot
/// keeps the phase colour, the word reads `TEXT_DIM`, and a pill that fits
/// beside them stays.
#[test]
fn an_ongoing_incident_names_its_phase_beside_its_dot() {
    let rows = incident_rows(
        &ongoing(UpdatePhase::Monitoring, Impact::Minor),
        false,
        true,
        36,
    );
    assert_eq!(line_text(&rows[1]), "  [ minor ]            ● monitoring ");
    let fgs = char_fgs(&rows[1]);
    assert_eq!(
        fgs[23],
        ('●', Some(theme::info_color())),
        "the dot keeps the monitoring phase colour"
    );
    for (i, (c, fg)) in fgs[25..35].iter().enumerate() {
        assert_eq!(
            *fg,
            Some(theme::text_dim_color()),
            "phase word char {i} ({c:?}) reads TEXT_DIM"
        );
    }
}

/// The phase word outranks the impact pill: the pill stays while both fit
/// beside a 2-cell gap and drops whole the cell they no longer do.
#[test]
fn the_impact_pill_gives_way_to_the_phase_word_when_both_do_not_fit() {
    let cases = [
        (
            UpdatePhase::Monitoring,
            Impact::Minor,
            26,
            "  [ minor ]  ● monitoring ",
            "both fit at 26",
        ),
        (
            UpdatePhase::Monitoring,
            Impact::Minor,
            25,
            "            ● monitoring ",
            "the pill drops one cell short",
        ),
        (
            UpdatePhase::Investigating,
            Impact::Critical,
            20,
            "    ● investigating ",
            "the longest codified word holds the 80-col selector alone",
        ),
    ];
    for (phase, impact, width, want, case) in cases {
        let rows = incident_rows(&ongoing(phase, impact), false, true, width);
        assert_eq!(line_text(&rows[1]), want, "{case}");
    }
}

/// A feed phase word too wide for the selector truncates with `…` instead of
/// dropping, so the dot never stands bare.
#[test]
fn an_over_wide_phase_word_truncates_rather_than_dropping() {
    let rows = incident_rows(
        &ongoing(
            UpdatePhase::Other("degraded performance in eu".into()),
            Impact::None,
        ),
        false,
        true,
        20,
    );
    assert_eq!(line_text(&rows[1]), "   ● degraded perf… ");
}

/// A selected ongoing row tints the phase word's cells without recolouring
/// them: the word stays `TEXT_DIM` on `BG_HOVER`, the dot keeps its phase
/// colour, and the tint still reaches the row's last cell with the pill kept
/// and with it dropped.
#[test]
fn a_selected_ongoing_row_tints_the_phase_word_without_recolouring_it() {
    let word: Vec<char> = UpdatePhase::Monitoring.word().chars().collect();
    for (width, want) in [
        (36, "  [ minor ]            ● monitoring "),
        (25, "            ● monitoring "),
    ] {
        let rows = incident_rows(
            &ongoing(UpdatePhase::Monitoring, Impact::Minor),
            true,
            true,
            width,
        );
        assert_eq!(line_text(&rows[1]), want, "the row at {width} cols");
        let cells: Vec<(char, Style)> = rows[1]
            .spans
            .iter()
            .flat_map(|span| span.content.chars().map(move |c| (c, span.style)))
            .collect();
        assert_eq!(
            cells.len(),
            width,
            "the tint spans the content width at {width} cols"
        );
        for (i, (c, style)) in cells.iter().enumerate() {
            assert_eq!(
                style.bg,
                Some(theme::bg_hover()),
                "cell {i} ({c:?}) carries the row tint at {width} cols"
            );
        }
        let dot = cells
            .iter()
            .position(|(c, _)| *c == '●')
            .expect("the phase dot renders");
        assert_eq!(
            cells[dot].1.fg,
            Some(theme::info_color()),
            "the dot keeps its phase colour at {width} cols"
        );
        assert!(
            cells[dot].1.add_modifier.contains(Modifier::BOLD),
            "the dot stays bold at {width} cols"
        );
        for (i, (c, style)) in cells[dot + 2..dot + 2 + word.len()].iter().enumerate() {
            assert_eq!(*c, word[i], "phase word char {i} at {width} cols");
            assert_eq!(
                style.fg,
                Some(theme::text_dim_color()),
                "phase word char {c:?} stays TEXT_DIM at {width} cols"
            );
            assert!(
                !style.add_modifier.contains(Modifier::BOLD),
                "phase word char {c:?} stays unbolded at {width} cols"
            );
        }
    }
}

/// An impact pill's fit is measured in cells, so a wide-glyph impact word
/// neither overruns the row nor clips the phase word beside it.
#[test]
fn a_wide_impact_pill_measures_its_word_in_cells() {
    let cases = [
        (
            Impact::Other("重大".into()),
            26,
            "  [ 重大 ]   ● monitoring ",
            "a 2-glyph impact keeps the phase word whole at 26",
        ),
        (
            Impact::Other("重大障害発生中".into()),
            40,
            "  [ 重大障害発生中 ]       ● monitoring ",
            "a 7-glyph impact keeps the phase word whole at 40",
        ),
    ];
    for (impact, width, want, case) in cases {
        let rows = incident_rows(
            &ongoing(UpdatePhase::Monitoring, impact),
            false,
            true,
            width,
        );
        assert_eq!(line_text(&rows[1]), want, "{case}");
        assert_eq!(
            line_cells(&rows[1]),
            width,
            "the row holds {width} cells: {case}"
        );
    }
}

/// A zero-cell phase word (a feed string that draws nothing) never panics the
/// render path, and the row still holds exactly the selector's content width
/// at every width down to zero.
#[test]
fn a_zero_cell_phase_word_holds_the_row_at_every_narrow_width() {
    let incident = ongoing(UpdatePhase::from_status("\u{200b}"), Impact::None);
    assert_eq!(
        grapheme_cells(&incident.phase.word()),
        0,
        "the feed word draws no cell"
    );
    for width in 0..=8usize {
        let rows = incident_rows(&incident, false, true, width);
        assert_eq!(
            line_cells(&rows[1]),
            width,
            "the row holds content_w {width}"
        );
        assert_eq!(
            line_cells(&rows[0]),
            width,
            "the title line holds content_w {width}"
        );
    }
}

/// When the cut would keep no phase-word grapheme, the dot stands alone rather
/// than beside a lone `…`; an ASCII word still cuts with `…` as before.
#[test]
fn a_phase_word_with_no_room_for_a_grapheme_shows_the_bare_dot() {
    let rows = incident_rows(
        &ongoing(UpdatePhase::Other("障害対応中".into()), Impact::None),
        false,
        true,
        8,
    );
    assert_eq!(line_text(&rows[1]), "      ● ");
    assert_eq!(line_cells(&rows[1]), 8);
    let rows = incident_rows(
        &ongoing(UpdatePhase::Monitoring, Impact::None),
        false,
        true,
        8,
    );
    assert_eq!(line_text(&rows[1]), "   ● m… ");
    assert_eq!(line_cells(&rows[1]), 8);
}

/// A resolved row never overruns: an impact word too wide for the row
/// truncates inside brackets that stay whole, and the age gives way.
#[test]
fn a_resolved_rows_long_impact_word_truncates_inside_whole_brackets() {
    let incident = Incident {
        id: "x".into(),
        title: "an incident".into(),
        link: String::new(),
        phase: UpdatePhase::Resolved,
        impact: Impact::Other("partial service disruption".into()),
        started_ms: 1_780_740_881_000,
        resolved_ms: Some(1_780_741_881_000),
        components: Vec::new(),
        updates: Vec::new(),
    };
    let rows = incident_rows(&incident, false, true, 20);
    let text = line_text(&rows[1]);
    assert_eq!(text, "  [ partial servi… ]");
    assert!(text.ends_with(" ]"), "the closing bracket stays whole");
    assert_eq!(line_cells(&rows[1]), 20);
}

fn resolved(impact: Impact) -> Incident {
    Incident {
        resolved_ms: Some(1_780_741_881_000),
        ..ongoing(UpdatePhase::Resolved, impact)
    }
}

/// A resolved pill that exactly fills the row stays whole: the trailing pad
/// cell gives way first, as it does for the age.
#[test]
fn a_resolved_pill_that_exactly_fills_the_row_stays_whole() {
    let cases = [
        (Impact::Minor, 11, "  [ minor ]"),
        (Impact::Critical, 14, "  [ critical ]"),
        (Impact::Other("重大".into()), 10, "  [ 重大 ]"),
    ];
    for (impact, width, want) in cases {
        let rows = incident_rows(&resolved(impact), false, true, width);
        assert_eq!(line_text(&rows[1]), want, "exact fit at {width}");
        assert_eq!(line_cells(&rows[1]), width, "row cells at {width}");
    }
}

/// A resolved pill whose cut would keep no impact grapheme drops whole, never
/// rendering `[ … ]`; the row stays exactly its width.
#[test]
fn a_resolved_pill_with_no_room_for_a_grapheme_drops_whole() {
    let cases = [
        (Impact::Minor, 7),
        (Impact::Critical, 7),
        (Impact::Other("重大".into()), 8),
    ];
    for (impact, width) in cases {
        let rows = incident_rows(&resolved(impact), false, true, width);
        assert_eq!(
            line_text(&rows[1]),
            " ".repeat(width),
            "no pill and no age at {width}"
        );
    }
}

/// On the Compatible tier the selector row keeps the theme's snapped xterm
/// indexes: INFO 117 on the dot, TEXT_DIM 145 on the word, BG_HOVER 236 on the
/// tint. A raw RGB or a Full-only style would leave 24-bit cells.
#[test]
fn a_compatible_row_keeps_its_snapped_xterm_colours() {
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Compatible);
    let word: Vec<char> = UpdatePhase::Monitoring.word().chars().collect();
    let rows = incident_rows(
        &ongoing(UpdatePhase::Monitoring, Impact::Minor),
        true,
        true,
        36,
    );
    assert_eq!(line_text(&rows[1]), "  [ minor ]            ● monitoring ");
    let cells: Vec<(char, Style)> = rows[1]
        .spans
        .iter()
        .flat_map(|span| span.content.chars().map(move |c| (c, span.style)))
        .collect();
    let dot = cells
        .iter()
        .position(|(c, _)| *c == '●')
        .expect("the phase dot renders");
    assert_eq!(
        cells[dot].1.fg,
        Some(ratatui::style::Color::Indexed(117)),
        "the dot snaps INFO to 117"
    );
    assert!(
        cells[dot].1.add_modifier.contains(Modifier::BOLD),
        "the dot stays bold"
    );
    for (i, (c, style)) in cells[dot + 2..dot + 2 + word.len()].iter().enumerate() {
        assert_eq!(*c, word[i], "phase word char {i}");
        assert_eq!(
            style.fg,
            Some(ratatui::style::Color::Indexed(145)),
            "phase word char {c:?} snaps TEXT_DIM to 145"
        );
    }
    for (i, (c, style)) in cells.iter().enumerate() {
        assert_eq!(
            style.bg,
            Some(ratatui::style::Color::Indexed(236)),
            "cell {i} ({c:?}) snaps BG_HOVER to 236"
        );
    }
}

/// A lam-alef phase word folds to one cell under `Span::width` but draws as
/// two, so the trailing pad measures the same cells the row does: line 2 still
/// holds exactly `content_w`.
#[test]
fn a_ligature_phase_word_does_not_lengthen_the_row() {
    let rows = incident_rows(
        &ongoing(UpdatePhase::Other("لا".into()), Impact::None),
        false,
        true,
        20,
    );
    assert_eq!(line_cells(&rows[1]), 20);
}

/// A resolved incident under a minute reads `lasted 1m`, never `lasted 45s`
/// (the status caller floors the span at 60 s).
#[test]
fn a_sub_minute_resolved_incident_reads_one_minute() {
    let started_ms = 1_780_740_881_000u64;
    let incident = Incident {
        id: "sub-minute".into(),
        title: "brief blip".into(),
        link: "https://example.com/".into(),
        phase: UpdatePhase::Resolved,
        impact: Impact::None,
        started_ms,
        resolved_ms: Some(started_ms + 45_000),
        components: vec![],
        updates: vec![],
    };
    let lines = detail_lines(&incident, 160);
    let duration = lines[0].spans.last().expect("a duration span");
    assert_eq!(duration.content.as_ref(), "lasted 1m");
}
