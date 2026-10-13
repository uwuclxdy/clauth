//! Row-0 pins: the version sits behind the brand, the `[ herdr ]` tag between
//! them, and the `[ shunt ]  [ daemon ]` chips hold the right edge. The row
//! sheds the tag first, the shunt chip second and the daemon chip third, so
//! brand + version never clip.

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

/// The two chips together, two cells apart, shunt first.
const CHIPS: &str = "[ shunt ]  [ daemon ]";
const DAEMON_CHIP: &str = "[ daemon ]";

/// The row-0 contract spelled from the outside: brand, the tag while it still
/// leaves room, the version behind both, then the chips on the right edge with
/// their content gap. Deriving the expected side independently is what pins the
/// shed order — the tag drops first, the shunt chip second, the daemon chip
/// third, and none ever costs the version a cell.
fn expected_row0(tag_wanted: bool, info_width: usize) -> String {
    let ver = format!(" v{VERSION}");
    let tag = "  [ herdr ]";
    // The minimum the chips keep from the content to their left.
    let content_gap = 3;
    let base = "clauth".chars().count() + ver.chars().count();
    let chips_w = CHIPS.chars().count();
    let tag_fits = base + tag.chars().count() + chips_w + content_gap <= info_width;

    let mut row = String::from("clauth");
    if tag_wanted && tag_fits {
        row.push_str(tag);
    }
    row.push_str(&ver);
    let chips = if base + chips_w + content_gap <= info_width {
        Some(CHIPS)
    } else if base + DAEMON_CHIP.chars().count() + content_gap <= info_width {
        Some(DAEMON_CHIP)
    } else {
        None
    };
    if let Some(chips) = chips {
        let used = row.chars().count();
        row.push_str(&" ".repeat(info_width - used - chips.chars().count()));
        row.push_str(chips);
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
/// row, the shunt chip at the width that no longer holds brand + version + both
/// chips with their three-cell content gap, the daemon chip at the width that
/// no longer holds it alone with that gap, and the version keeps its own cells
/// at every seam. Each boundary is derived the way the renderer derives it —
/// off the version string width, never a hardcoded column — so a version bump
/// moves the pins with it, and each is pinned on both sides, so a `<`/`<=`
/// inversion reds whichever way it leans.
#[test]
fn row0_sheds_the_tag_then_the_shunt_chip_then_the_daemon_chip_never_the_version() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = app_with_mode(true);
    app.daemon_health = crate::daemon::DaemonHealth::Fresh;
    let ver = format!(" v{VERSION}");
    let base = "clauth".chars().count() + ver.chars().count();
    let tag_w = "  [ herdr ]".chars().count();
    let chips_w = CHIPS.chars().count();
    let daemon_w = DAEMON_CHIP.chars().count();
    let content_gap = 3;
    // The 10-column logo column is not the header's text column.
    let tag_seam = 10 + base + tag_w + chips_w + content_gap;
    let shunt_seam = 10 + base + chips_w + content_gap;
    let daemon_seam = 10 + base + daemon_w + content_gap;

    let (all_fit, _buf) = row0_render(&app, tag_seam as u16);
    assert!(
        all_fit.contains("[ herdr ]"),
        "at the exact fit width the tag must render: {all_fit:?}"
    );
    assert!(
        all_fit.trim_end().ends_with(&format!("   {CHIPS}")),
        "the chips hold the right edge beside the tag, keeping their content gap: {all_fit:?}"
    );

    let (tag_shed, _buf) = row0_render(&app, (tag_seam - 1) as u16);
    assert!(
        !tag_shed.contains("[ herdr ]"),
        "one column narrower the tag must shed: {tag_shed:?}"
    );
    assert!(
        tag_shed.trim_end().ends_with(CHIPS),
        "both chips survive the tag they displaced: {tag_shed:?}"
    );
    assert!(
        tag_shed.starts_with(&format!("clauth v{VERSION}")),
        "the version keeps the brand's side through the tag's shed: {tag_shed:?}"
    );

    let (shunt_at_seam, _buf) = row0_render(&app, shunt_seam as u16);
    assert_eq!(
        shunt_at_seam.trim_end(),
        format!("clauth v{VERSION}   {CHIPS}"),
        "both chips render at the exact width that holds them and their content gap"
    );

    let (shunt_shed, _buf) = row0_render(&app, (shunt_seam - 1) as u16);
    assert!(
        !shunt_shed.contains("[ shunt ]"),
        "one column narrower the shunt chip sheds first: {shunt_shed:?}"
    );
    assert!(
        shunt_shed.trim_end().ends_with(DAEMON_CHIP),
        "the daemon chip outlives the shunt chip: {shunt_shed:?}"
    );

    let (daemon_at_seam, _buf) = row0_render(&app, daemon_seam as u16);
    assert_eq!(
        daemon_at_seam.trim_end(),
        format!("clauth v{VERSION}   {DAEMON_CHIP}"),
        "the daemon chip renders at the exact width that holds it and its content gap"
    );

    let (daemon_shed, _buf) = row0_render(&app, (daemon_seam - 1) as u16);
    assert!(
        !daemon_shed.contains("[ daemon ]") && !daemon_shed.contains("[ shunt ]"),
        "one column narrower no chip renders rather than crowd the content: {daemon_shed:?}"
    );
    assert!(
        daemon_shed.starts_with(&format!("clauth v{VERSION}")),
        "brand + version never clip, even with every chip shed: {daemon_shed:?}"
    );

    // Every side against the independently derived expectation, so the pins
    // cannot drift from the renderer's own fit rule.
    assert_eq!(all_fit, expected_row0(true, tag_seam - 10));
    assert_eq!(tag_shed, expected_row0(true, tag_seam - 11));
    assert_eq!(shunt_at_seam, expected_row0(true, shunt_seam - 10));
    assert_eq!(shunt_shed, expected_row0(true, shunt_seam - 11));
    assert_eq!(daemon_at_seam, expected_row0(true, daemon_seam - 10));
    assert_eq!(daemon_shed, expected_row0(true, daemon_seam - 11));
}

/// The same seams without herdr mode: a plain launch sheds each chip at the
/// same width, gap included, and never at a narrower one.
#[test]
fn the_plain_launch_chip_seams_hold_the_same_content_gap() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = app_with_mode(false);
    app.daemon_health = crate::daemon::DaemonHealth::Fresh;
    let ver = format!(" v{VERSION}");
    let base = "clauth".chars().count() + ver.chars().count();
    let shunt_seam = 10 + base + CHIPS.chars().count() + 3;
    let daemon_seam = 10 + base + DAEMON_CHIP.chars().count() + 3;

    let (at, _buf) = row0_render(&app, shunt_seam as u16);
    assert_eq!(
        at.trim_end(),
        format!("clauth v{VERSION}   {CHIPS}"),
        "at the exact width the chips keep three cells from the version"
    );
    assert_eq!(at, expected_row0(false, shunt_seam - 10));
    let (shed, _buf) = row0_render(&app, (shunt_seam - 1) as u16);
    assert_eq!(shed, expected_row0(false, shunt_seam - 11));

    let (at, _buf) = row0_render(&app, daemon_seam as u16);
    assert_eq!(
        at.trim_end(),
        format!("clauth v{VERSION}   {DAEMON_CHIP}"),
        "at the exact width the daemon chip keeps three cells from the version"
    );
    assert_eq!(at, expected_row0(false, daemon_seam - 10));
    let (shed, _buf) = row0_render(&app, (daemon_seam - 1) as u16);
    assert!(
        !shed.contains("[ daemon ]"),
        "one column narrower it sheds: {shed:?}"
    );
    assert_eq!(shed, expected_row0(false, daemon_seam - 11));
}

/// With the daemon absent both chips are faint, never gone: a plain launch's
/// row 0 is the tagged row's shape minus the tag, at the same width.
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
        row0.trim_end().ends_with(CHIPS),
        "both chips render with the daemon absent: {row0:?}"
    );
}
