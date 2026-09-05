//! Row-0 herdr-mode pins: the `[ herdr ]` tag sits after the brand, before
//! the daemon dot, in `TEXT_DIM` — and it is the one span the row sheds when
//! the version would stop right-aligning. The non-herdr render is pinned byte
//! for byte at two widths, so a plain launch cannot drift under the tag work.

use super::*;
use crate::profile::{AppConfig, AppState};
use ratatui::Terminal;
use ratatui::backend::TestBackend;

fn app_with_mode(herdr_mode: bool) -> App {
    App::new(AppConfig {
        state: AppState::default(),
        profiles: Vec::new(),
    })
    .with_herdr_mode(herdr_mode)
}

/// Row 0 past the 10-cell logo column, plus the buffer for style pins.
fn row0_render(app: &App, width: u16) -> (String, ratatui::buffer::Buffer) {
    let height = header_height(app);
    let mut term = Terminal::new(TestBackend::new(width, height)).expect("backend");
    term.draw(|f| {
        let area = f.area();
        super::draw(f, area, app);
    })
    .expect("draw");
    let buf = term.backend().buffer().clone();
    let rows = crate::testutil::buffer_rows(&buf);
    (rows[0].chars().skip(10).collect(), buf)
}

/// The row-0 contract spelled from the outside: brand, the tag while the full
/// row still fits the right-aligned version, then the daemon dot, gap, version.
/// Deriving the expected side independently is what pins the shed order — the
/// tag drops before the version loses its right edge.
fn expected_row0(tag_wanted: bool, daemon: bool, info_width: usize) -> String {
    let ver = format!("v{VERSION}");
    let base = "clauth".chars().count()
        + if daemon {
            "  ● daemon".chars().count()
        } else {
            0
        };
    let tag = "  [ herdr ]";
    let tag_fits = base + tag.chars().count() + ver.chars().count() <= info_width;

    let mut row = String::from("clauth");
    if tag_wanted && tag_fits {
        row.push_str(tag);
    }
    if daemon {
        row.push_str("  ● daemon");
    }
    let used = row.chars().count();
    row.push_str(&" ".repeat(info_width - used - ver.chars().count()));
    row.push_str(&ver);
    row
}

#[test]
fn herdr_mode_shows_the_tag_after_the_brand_before_the_daemon_dot() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = app_with_mode(true);
    app.daemon_health = crate::daemon::DaemonHealth::Fresh;
    let width = 100;

    let (row0, buf) = row0_render(&app, width);
    assert_eq!(
        row0,
        expected_row0(true, true, (width - 10) as usize),
        "row 0 must be the brand, tag, daemon dot, then the right-aligned version"
    );
    assert!(
        row0.ends_with(&format!("v{VERSION}")),
        "the version must keep the right edge"
    );

    // The whole tag — brackets included — renders TEXT_DIM, pinned by the
    // theme mapping itself rather than a restated color.
    let col = row0.find("[ herdr ]").expect("tag renders");
    assert_eq!(
        buf.content[10 + col].fg,
        super::theme::text_dim_color(),
        "the tag must render in TEXT_DIM"
    );
}

#[test]
fn herdr_mode_sheds_the_tag_instead_of_clipping_the_version_at_narrow_width() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = app_with_mode(true);
    app.daemon_health = crate::daemon::DaemonHealth::Fresh;
    let width = 40;

    let (row0, _buf) = row0_render(&app, width);
    assert_eq!(
        row0,
        expected_row0(true, true, (width - 10) as usize),
        "at 40 cols the tag must drop and the row read exactly like a plain launch"
    );
    assert!(
        !row0.contains("[ herdr ]"),
        "a tag that would clip the version must not render"
    );
    assert!(
        row0.ends_with(&format!("v{VERSION}")),
        "the version must keep the right edge at narrow width"
    );
}

/// The shed boundary itself, derived the way the renderer derives it — off
/// the version string width, never a hardcoded column — so a version bump
/// moves the pin with it. Pinned on both sides of the seam, so a `<`/`<=`
/// inversion in the fit rule reds whichever way it leans.
#[test]
fn the_tag_fits_exactly_at_the_boundary_and_sheds_one_column_narrower() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = app_with_mode(true);
    app.daemon_health = crate::daemon::DaemonHealth::Fresh;
    let ver = format!("v{VERSION}");
    // The renderer's fit rule: brand + daemon dot + tag + version against the
    // row width, where the row starts after the 10-column logo.
    let used = "clauth".chars().count() + "  ● daemon".chars().count();
    let tag_w = "  [ herdr ]".chars().count();
    let boundary = 10 + used + tag_w + ver.chars().count();

    let (at, _buf) = row0_render(&app, boundary as u16);
    assert!(
        at.contains("[ herdr ]"),
        "at the exact fit width the tag must render: {at:?}"
    );
    assert!(
        at.ends_with(&ver),
        "the version keeps its right edge at the boundary"
    );

    let (shed, _buf) = row0_render(&app, (boundary - 1) as u16);
    assert!(
        !shed.contains("[ herdr ]"),
        "one column narrower the tag must shed: {shed:?}"
    );
    assert!(
        shed.ends_with(&ver),
        "the version keeps its right edge one column past the boundary"
    );

    // Both sides against the independently derived expectation, so the pin
    // cannot drift from the renderer's own fit rule.
    assert_eq!(at, expected_row0(true, true, boundary - 10));
    assert_eq!(shed, expected_row0(true, true, boundary - 11));
}

/// The "byte-identical to today" half: a non-herdr launch renders the exact
/// pre-tag row 0 at both widths, daemon dot or not.
#[test]
fn a_non_herdr_launch_renders_row0_byte_identically_at_both_widths() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = app_with_mode(false);
    app.daemon_health = crate::daemon::DaemonHealth::Fresh;

    for width in [40u16, 100u16] {
        let (row0, _buf) = row0_render(&app, width);
        assert_eq!(
            row0,
            expected_row0(false, true, (width - 10) as usize),
            "herdr_mode=false must render the pre-tag row 0 at {width} cols"
        );
    }
}

#[test]
fn herdr_tag_renders_without_a_daemon_dot() {
    let _home = crate::testutil::HomeSandbox::new();
    let app = app_with_mode(true);
    let width = 100;

    let (row0, _buf) = row0_render(&app, width);
    assert_eq!(
        row0,
        expected_row0(true, false, (width - 10) as usize),
        "the tag is herdr-mode's, not the daemon dot's — it must render either way"
    );
}
