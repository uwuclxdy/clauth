//! Row-0 pins: the version sits behind the brand, the `[ herdr ]` tag between
//! them, and the `[ daemon ]` chip holds the right edge. The tag is the first
//! span the row sheds and the chip the second, so brand + version never clip.

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

/// The row-0 contract spelled from the outside: brand, the tag while it still
/// leaves room, the version behind both, then the chip on the right edge with
/// its content gap. Deriving the expected side independently is what pins the
/// shed order — the tag drops first, the chip second, and neither ever costs
/// the version a cell.
fn expected_row0(tag_wanted: bool, info_width: usize) -> String {
    let ver = format!(" v{VERSION}");
    let chip = "[ daemon ]";
    let tag = "  [ herdr ]";
    // The minimum the chip keeps from the content to its left.
    let content_gap = 3;
    let base = "clauth".chars().count() + ver.chars().count();
    let tag_fits = base + tag.chars().count() + chip.chars().count() + content_gap <= info_width;

    let mut row = String::from("clauth");
    if tag_wanted && tag_fits {
        row.push_str(tag);
    }
    row.push_str(&ver);
    if base + chip.chars().count() + content_gap <= info_width {
        let used = row.chars().count();
        row.push_str(&" ".repeat(info_width - used - chip.chars().count()));
        row.push_str(chip);
    } else {
        // A shed chip leaves the row short of the text column's width; the
        // buffer pads it out.
        row.push_str(&" ".repeat(info_width - row.chars().count()));
    }
    row
}

#[test]
fn the_version_sits_behind_the_brand_and_the_chip_holds_the_right_edge() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = app_with_mode(false);
    app.daemon_health = crate::daemon::DaemonHealth::Fresh;
    let width = 100;

    let (row0, _buf) = row0_render(&app, width);
    assert!(
        row0.starts_with(&format!("clauth v{VERSION}")),
        "the version must sit directly behind the brand: {row0:?}"
    );
    assert!(
        row0.trim_end().ends_with("[ daemon ]"),
        "the chip must hold the right edge: {row0:?}"
    );
    assert_eq!(
        row0,
        expected_row0(false, (width - 10) as usize),
        "row 0 must be the brand, version, gap, then the chip"
    );
}

#[test]
fn herdr_mode_shows_the_tag_between_the_brand_and_the_version() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = app_with_mode(true);
    app.daemon_health = crate::daemon::DaemonHealth::Fresh;
    let width = 100;

    let (row0, buf) = row0_render(&app, width);
    assert!(
        row0.starts_with(&format!("clauth  [ herdr ] v{VERSION}")),
        "the tag sits beside the brand, the version behind both: {row0:?}"
    );
    assert_eq!(
        row0,
        expected_row0(true, (width - 10) as usize),
        "row 0 must be the brand, tag, version, gap, then the chip"
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

/// The shed ladder: the tag goes one column past the width that fits the full
/// row, the chip at the width that no longer holds brand + version + chip with
/// the chip's three-cell content gap, and the version keeps its own cells at
/// both seams. Each boundary is derived the way the renderer derives it — off
/// the version string width, never a hardcoded column — so a version bump moves
/// the pins with it, and each is pinned on both sides, so a `<`/`<=` inversion
/// reds whichever way it leans.
#[test]
fn row0_sheds_the_tag_first_then_the_chip_never_the_version() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = app_with_mode(true);
    app.daemon_health = crate::daemon::DaemonHealth::Fresh;
    let ver = format!(" v{VERSION}");
    let base = "clauth".chars().count() + ver.chars().count();
    let tag_w = "  [ herdr ]".chars().count();
    let chip_w = "[ daemon ]".chars().count();
    let content_gap = 3;
    // The 10-column logo column is not the header's text column.
    let tag_seam = 10 + base + tag_w + chip_w + content_gap;
    let chip_seam = 10 + base + chip_w + content_gap;

    let (all_fit, _buf) = row0_render(&app, tag_seam as u16);
    assert!(
        all_fit.contains("[ herdr ]"),
        "at the exact fit width the tag must render: {all_fit:?}"
    );
    assert!(
        all_fit.trim_end().ends_with("   [ daemon ]"),
        "the chip holds the right edge beside the tag, keeping its content gap: {all_fit:?}"
    );

    let (tag_shed, _buf) = row0_render(&app, (tag_seam - 1) as u16);
    assert!(
        !tag_shed.contains("[ herdr ]"),
        "one column narrower the tag must shed: {tag_shed:?}"
    );
    assert!(
        tag_shed.trim_end().ends_with("[ daemon ]"),
        "the chip survives the tag it displaced: {tag_shed:?}"
    );
    assert!(
        tag_shed.starts_with(&format!("clauth v{VERSION}")),
        "the version keeps the brand's side through the tag's shed: {tag_shed:?}"
    );

    let (chip_at_seam, _buf) = row0_render(&app, chip_seam as u16);
    assert!(
        chip_at_seam.trim_end().ends_with("   [ daemon ]"),
        "the chip renders at the exact width that holds it and its content gap: {chip_at_seam:?}"
    );

    let (chip_shed, _buf) = row0_render(&app, (chip_seam - 1) as u16);
    assert!(
        !chip_shed.contains("[ daemon ]"),
        "one column narrower the chip must shed rather than crowd the content: {chip_shed:?}"
    );
    assert!(
        chip_shed.starts_with(&format!("clauth v{VERSION}")),
        "brand + version never clip, even with both shed: {chip_shed:?}"
    );

    // Both sides against the independently derived expectation, so the pins
    // cannot drift from the renderer's own fit rule.
    assert_eq!(all_fit, expected_row0(true, tag_seam - 10));
    assert_eq!(tag_shed, expected_row0(true, tag_seam - 11));
    assert_eq!(chip_at_seam, expected_row0(true, chip_seam - 10));
    assert_eq!(chip_shed, expected_row0(true, chip_seam - 11));
}

/// The same chip seam without herdr mode: a plain launch sheds the chip at the
/// same width, gap included, and never at a narrower one.
#[test]
fn the_plain_launch_chip_seam_holds_the_same_content_gap() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = app_with_mode(false);
    app.daemon_health = crate::daemon::DaemonHealth::Fresh;
    let ver = format!(" v{VERSION}");
    let base = "clauth".chars().count() + ver.chars().count();
    let chip_w = "[ daemon ]".chars().count();
    let seam = 10 + base + chip_w + 3;

    let (at, _buf) = row0_render(&app, seam as u16);
    assert_eq!(
        at.trim_end(),
        format!("clauth v{VERSION}   [ daemon ]"),
        "at the exact width the chip keeps three cells from the version"
    );
    assert_eq!(at, expected_row0(false, seam - 10));

    let (shed, _buf) = row0_render(&app, (seam - 1) as u16);
    assert!(
        !shed.contains("[ daemon ]"),
        "one column narrower it sheds: {shed:?}"
    );
    assert_eq!(shed, expected_row0(false, seam - 11));
}

/// With the daemon absent the chip is dim, never gone: a plain launch's row 0
/// is the tagged row's shape minus the tag, at the same width.
#[test]
fn the_chip_renders_without_a_daemon_and_the_tag_only_in_herdr_mode() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut tagged = app_with_mode(true);
    tagged.daemon_health = crate::daemon::DaemonHealth::Absent;
    let mut plain = app_with_mode(false);
    plain.daemon_health = crate::daemon::DaemonHealth::Absent;
    let width = 100;
    let info_width = (width - 10) as usize;

    let (row0, _buf) = row0_render(&tagged, width);
    assert_eq!(
        row0,
        expected_row0(true, info_width),
        "the tag is herdr-mode's, the chip the row's — both render with no daemon"
    );

    let (row0, _buf) = row0_render(&plain, width);
    assert_eq!(
        row0,
        expected_row0(false, info_width),
        "herdr_mode=false renders the row without the tag, chip intact"
    );
    assert!(
        !row0.contains("[ herdr ]"),
        "no herdr mode, no tag: {row0:?}"
    );
    assert!(
        row0.trim_end().ends_with("[ daemon ]"),
        "the chip renders with the daemon absent: {row0:?}"
    );
}
