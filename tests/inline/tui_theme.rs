use ratatui::style::Color;

use crate::testutil::TierSandbox;

#[test]
fn full_tier_blends_two_rgb_colors() {
    let _tier = TierSandbox::new(super::Tier::Full);
    // 0.75 · (17,17,27) + 0.25 · (40,40,56) = (23,23,34)
    assert_eq!(
        super::blend_over(Color::Rgb(40, 40, 56), Color::Rgb(17, 17, 27), 0.75),
        Color::Rgb(23, 23, 34),
    );
}

#[test]
fn full_tier_returns_over_unchanged_when_either_side_is_not_rgb() {
    let _tier = TierSandbox::new(super::Tier::Full);
    // A non-RGB under-bg does not blend on the full tier; `over` survives.
    assert_eq!(
        super::blend_over(Color::Reset, Color::Rgb(17, 17, 27), 0.75),
        Color::Rgb(17, 17, 27),
    );
    assert_eq!(
        super::blend_over(Color::Rgb(40, 40, 56), Color::Reset, 0.75),
        Color::Reset,
    );
}

#[test]
fn compatible_tier_blends_in_rgb_and_snaps_to_the_nearest_index() {
    let _tier = TierSandbox::new(super::Tier::Compatible);
    // A painted BG_HOVER cell: 0.75·(17,17,27) + 0.25·(40,40,56) = (23,23,34),
    // nearest xterm-256 index 234.
    assert_eq!(
        super::blend_over(Color::Indexed(236), Color::Indexed(233), 0.75),
        Color::Indexed(234),
    );
    // An unknown/Reset under-bg counts as BG (30,30,46): 0.75·(17,17,27) +
    // 0.25·(30,30,46) = (20,20,32), nearest index 234.
    assert_eq!(
        super::blend_over(Color::Reset, Color::Indexed(233), 0.75),
        Color::Indexed(234),
    );
    // A painted BG cell blends to the same (20,20,32) → 234.
    assert_eq!(
        super::blend_over(Color::Indexed(235), Color::Indexed(233), 0.75),
        Color::Indexed(234),
    );
    // Two beneaths whose glass snaps away from 234, so a blend that dropped the
    // beneath would red here. A focused modal button (TEXT 189): 0.75·(17,17,27)
    // + 0.25·(205,214,244) = (64,66,81), nearest index 238. A focused border
    // (LINE_STRONG 240): 0.75·(17,17,27) + 0.25·(69,71,90) = (30,31,43),
    // nearest index 235.
    assert_eq!(
        super::blend_over(Color::Indexed(189), Color::Indexed(233), 0.75),
        Color::Indexed(238),
    );
    assert_eq!(
        super::blend_over(Color::Indexed(240), Color::Indexed(233), 0.75),
        Color::Indexed(235),
    );
}

#[test]
fn nearest_index_never_targets_a_terminal_defined_ansi_index() {
    // Indices 0..15 are terminal-defined, so the snap must land on 16..=255
    // even for an RGB exactly matching one of them.
    for (i, &(r, g, b)) in super::ANSI_RGB.iter().enumerate() {
        let snap = super::nearest_index((r, g, b));
        assert!(snap >= 16, "ANSI index {i} ({r},{g},{b}) snapped to {snap}");
    }
}

#[test]
fn nearest_index_matches_an_independent_bruteforce_over_a_sampled_set() {
    // The exhaustive 16.7M-point equality against an independent search is too
    // slow in a debug test; it was checked out-of-band (0 diffs). This sampled
    // pin walks every 997th RGB against a reference that rebuilds the palette
    // from the xterm-256 formula instead of the module's tables.
    fn reference_palette(i: u8) -> (u8, u8, u8) {
        let cube = [0u8, 1, 2, 3, 4, 5].map(|n| if n == 0 { 0 } else { 55 + 40 * n });
        match i {
            16..=231 => {
                let n = i - 16;
                (
                    cube[usize::from(n / 36)],
                    cube[usize::from((n / 6) % 6)],
                    cube[usize::from(n % 6)],
                )
            }
            _ => {
                let v = 8 + (i - 232) * 10;
                (v, v, v)
            }
        }
    }
    let reference = |(r, g, b): (u8, u8, u8)| -> u8 {
        let mut best = 16u8;
        let mut best_d = u32::MAX;
        for i in 16..=u8::MAX {
            let (pr, pg, pb) = reference_palette(i);
            let dr = i32::from(r) - i32::from(pr);
            let dg = i32::from(g) - i32::from(pg);
            let db = i32::from(b) - i32::from(pb);
            let d = (dr * dr + dg * dg + db * db) as u32;
            if d < best_d {
                best_d = d;
                best = i;
            }
        }
        best
    };
    let mut checked = 0usize;
    for v in (0..16_777_216usize).step_by(997) {
        let rgb = ((v >> 16) as u8, (v >> 8) as u8, v as u8);
        assert_eq!(super::nearest_index(rgb), reference(rgb), "rgb {rgb:?}");
        checked += 1;
    }
    assert!(checked > 16_000, "sampled {checked} points");
}

#[test]
fn indexed_rgb_resolves_the_theme_tokens_to_their_rgb_column() {
    // The surface tokens the toast blend reads: an index stands in for the RGB
    // its accessor pairs with it, never the terminal's generic value for it.
    assert_eq!(super::indexed_rgb(233), (17, 17, 27));
    assert_eq!(super::indexed_rgb(235), (30, 30, 46));
    assert_eq!(super::indexed_rgb(236), (40, 40, 56));
}

#[test]
fn nearest_index_matches_the_grayscale_surfaces() {
    // Toast blends stay near the grayscale ramp, where squared distance and the
    // theme's nearest-index column agree.
    assert_eq!(super::nearest_index((17, 17, 27)), 233);
    assert_eq!(super::nearest_index((30, 30, 46)), 235);
    assert_eq!(super::nearest_index((40, 40, 56)), 236);
    assert_eq!(super::nearest_index((20, 20, 32)), 234);
    assert_eq!(super::nearest_index((23, 23, 34)), 234);
}

/// Claude Code's working-line mark: six frames, eased by
/// `round((1 - cos(2πT/2000)) / 2 · 5)` over a 2 s period, T snapped down to
/// 64 ms. Expected frames worked by hand from that formula.
#[test]
fn the_active_claude_frame_follows_the_eased_two_second_cycle() {
    let frame = |ms| super::active_claude_frame(ms, false);
    // T=0: (1-1)/2*5 = 0.
    assert_eq!(frame(0), "·");
    // T=63 snaps to 0.
    assert_eq!(frame(63), "·");
    // T=250 snaps DOWN to 192: cos(2π·0.096) = 0.823 -> 0.44 -> 0. An unsnapped
    // or nearest-snapped T lands on index 1 instead, so this pins the direction.
    assert_eq!(frame(250), "·");
    // T=640: cos(2π·0.32) = -0.426 -> 3.56 -> 4.
    assert_eq!(frame(640), "✻");
    // T=1000 snaps to 960: cos(2π·0.48) = -0.992 -> 4.98 -> 5.
    assert_eq!(frame(1000), "✽");
    // T=1216: cos(2π·0.608) = -0.780 -> 4.45 -> 4, on the way back.
    assert_eq!(frame(1216), "✻");
    // T=448: cos(2π·0.224) = 0.163 -> 2.09 -> 2, the default terminals' ASCII frame.
    assert_eq!(frame(448), "*");
    // T=2000 wraps to the cycle start.
    assert_eq!(frame(2000), "·");
    // xterm-ghostty draws its own array: `✳` in place of `*`, `✻` twice at the end.
    assert_eq!(super::active_claude_frame(448, true), "✳");
    assert_eq!(super::active_claude_frame(1000, true), "✻");
}

/// The startup detection reads through `env`, the only seam it has to the
/// process environment; each case is `(env pairs, tier, ghostty marks)`.
#[test]
fn term_dumb_and_no_color_leave_the_detected_tier_alone() {
    use super::Tier::{Compatible, Full};
    type Case<'a> = (&'a [(&'a str, &'a str)], super::Tier, bool);
    let cases: [Case; 9] = [
        // Controls: the inputs detection does read.
        (&[], Compatible, false),
        (&[("COLORTERM", "truecolor")], Full, false),
        (&[("COLORTERM", "TrueColor")], Full, false),
        (&[("TERM", "xterm-ghostty")], Compatible, true),
        // `TERM=dumb` picks what its `COLORTERM` picks, nothing of its own.
        (&[("TERM", "dumb")], Compatible, false),
        (&[("TERM", "dumb"), ("COLORTERM", "truecolor")], Full, false),
        (&[("TERM", "dumb"), ("COLORTERM", "24bit")], Full, false),
        // `NO_COLOR` is the terminal library's to honour, never a tier.
        (&[("NO_COLOR", "1")], Compatible, false),
        (
            &[("NO_COLOR", "1"), ("COLORTERM", "truecolor")],
            Full,
            false,
        ),
    ];
    for (pairs, tier, ghostty) in cases {
        let env = |key: &str| {
            pairs
                .iter()
                .find(|(k, _)| *k == key)
                .map(|(_, v)| (*v).to_string())
        };
        assert_eq!(super::detect_with(env), (tier, ghostty), "{pairs:?}");
    }
}

/// The colour SGR parameters (`30`-`49`, `58`/`59`, `90`-`107`) in `bytes`,
/// one entry per escape that carries any.
fn colour_sgrs(bytes: &[u8]) -> Vec<String> {
    let text = String::from_utf8_lossy(bytes);
    let mut found = Vec::new();
    let mut rest = text.as_ref();
    while let Some(at) = rest.find("\x1b[") {
        rest = &rest[at + 2..];
        let Some(end) = rest.find(|c: char| !(c.is_ascii_digit() || c == ';' || c == ':')) else {
            break;
        };
        let (params, tail) = rest.split_at(end);
        if tail.starts_with('m')
            && params.split([';', ':']).any(|p| {
                p.parse::<u16>()
                    .is_ok_and(|n| matches!(n, 30..=49 | 58 | 59 | 90..=107))
            })
        {
            found.push(params.to_string());
        }
        rest = tail;
    }
    found
}

#[test]
fn colour_sgrs_finds_colour_and_skips_other_escapes() {
    assert_eq!(
        colour_sgrs(b"\x1b[38;2;1;2;3mA\x1b[0m\x1b[1mB\x1b[mC\x1b[2;5HD\x1b[48;5;235m"),
        ["38;2;1;2;3", "48;5;235"]
    );
    assert_eq!(
        colour_sgrs(b"\x1b[0m\x1b[1;4m\x1b[m\x1b[10;20H"),
        Vec::<String>::new()
    );
}

/// Every tab drawn through the real terminal backend, on `tier`, with the
/// terminal library's own `NO_COLOR` switch forced to `no_color` — or, with
/// `None`, left as crossterm reads it from `NO_COLOR` on first use.
fn draw_every_tab_bytes(tier: super::Tier, no_color: Option<bool>) -> Vec<u8> {
    use ratatui::crossterm::style::Colored;
    use ratatui::{Terminal, backend::CrosstermBackend};
    // Forcing the switch is process-wide, so its window is this call: any test
    // asserting colour bytes from crossterm in the same process while it runs
    // would see the forced value. A panic would leave the switch flipped for every
    // later test in this process: restore it on the way out either way.
    struct Restore(bool);
    impl Drop for Restore {
        fn drop(&mut self) {
            Colored::set_ansi_color_disabled(self.0);
        }
    }

    let _tier = TierSandbox::new(tier);
    let name = crate::profile::ProfileName::from("acct");
    let profile = crate::testutil::blank_profile(&name);
    let mut app = crate::tui::app::App::new(crate::profile::AppConfig {
        state: crate::profile::AppState {
            profiles: vec![name],
            preset_binds_offered: Some(crate::presets::builtin_binds_digest()),
            ..crate::profile::AppState::default()
        },
        profiles: vec![profile],
    });
    let _restore = no_color.map(|no_color| {
        let restore = Restore(Colored::ansi_color_disabled_memoized());
        Colored::set_ansi_color_disabled(no_color);
        restore
    });
    // A fixed viewport: the backend never asks a real terminal for its size.
    let out = Shared::default();
    let mut term = Terminal::with_options(
        CrosstermBackend::new(out.clone()),
        ratatui::TerminalOptions {
            viewport: ratatui::Viewport::Fixed(ratatui::layout::Rect::new(0, 0, 120, 30)),
        },
    )
    .expect("terminal");
    for tab in crate::tui::app::Tab::ALL {
        app.tab = tab;
        term.draw(|f| crate::tui::render::draw(f, &app))
            .expect("draw");
    }
    drop(term);
    out.0.take()
}

/// A writer whose bytes the test reads back after the backend has them.
#[derive(Clone, Default)]
struct Shared(std::rc::Rc<std::cell::RefCell<Vec<u8>>>);

impl std::io::Write for Shared {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.borrow_mut().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Under `NO_COLOR` no frame of any tab carries a colour escape, on either
/// tier: clauth draws its colours only through the terminal library, which
/// drops them there. The control draws the same frames with the switch off.
/// This pins the switch's effect; the re-entered-process test below pins that
/// the environment's `NO_COLOR` is what sets that switch.
#[test]
fn no_color_strips_every_colour_from_every_tab_on_both_tiers() {
    let _home = crate::testutil::HomeSandbox::new();
    for tier in [super::Tier::Full, super::Tier::Compatible] {
        assert!(
            !colour_sgrs(&draw_every_tab_bytes(tier, Some(false))).is_empty(),
            "{tier:?}: the control draws colour"
        );
        assert_eq!(
            colour_sgrs(&draw_every_tab_bytes(tier, Some(true))),
            Vec::<String>::new(),
            "{tier:?}: no colour under NO_COLOR"
        );
    }
}

/// Set only by the parent below: absent, the child test returns at once, so a
/// normal suite run passes it trivially.
#[cfg(any(target_os = "linux", target_os = "macos"))]
const NO_COLOR_REENTRY_ENV: &str = "CLAUTH_TEST_NO_COLOR_REENTRY";

/// The re-entry target: the process reads `NO_COLOR` from its own environment,
/// as crossterm does, and draws every tab on both tiers without touching the
/// switch. Only the parent below sets `NO_COLOR`.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn an_env_no_color_process_draws_no_colour_on_both_tiers() {
    if std::env::var_os(NO_COLOR_REENTRY_ENV).is_none() {
        return;
    }
    let _home = crate::testutil::HomeSandbox::new();
    for tier in [super::Tier::Full, super::Tier::Compatible] {
        assert_eq!(
            colour_sgrs(&draw_every_tab_bytes(tier, None)),
            Vec::<String>::new(),
            "{tier:?}: a process that read NO_COLOR draws no colour"
        );
    }
}

/// `NO_COLOR` in the environment, never a switch this process flips: the child
/// above re-enters this binary with `NO_COLOR=1` and must pass, then again
/// without it and must fail, which proves that child ran and would see colour.
/// A filter matching no test exits 0, so the failing control is what keeps
/// this pin from passing vacuously. Residual: `tui::open_terminal`'s own
/// setup (its `Stdout` sink, raw mode, the alternate screen) is not exercised,
/// nor is a production backend other than `CrosstermBackend`; windows runs no
/// re-entry, so there only the switch-level test above holds.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn no_color_from_the_environment_strips_colour_in_a_reentered_process() {
    let child = "tui::theme::tests::an_env_no_color_process_draws_no_colour_on_both_tiers";
    let status = crate::testutil::reentry_command(child, &[])
        .env(NO_COLOR_REENTRY_ENV, "1")
        .env("NO_COLOR", "1")
        .status()
        .expect("re-enter the test binary");
    assert!(status.success(), "NO_COLOR=1 must draw no colour: {status}");
    let status = crate::testutil::reentry_command(child, &[])
        .env(NO_COLOR_REENTRY_ENV, "1")
        .env_remove("NO_COLOR")
        .status()
        .expect("re-enter the test binary");
    assert!(
        !status.success(),
        "without NO_COLOR the same child draws colour: {status}"
    );
}

/// Every named way `src/` could turn colour back on: crossterm's process-wide
/// switch (`set_ansi_color_disabled`, `force_color_output`), clap's own
/// `ColorChoice::Always`, and any code naming `NO_COLOR` at all, which is how a
/// scrub of it that names the key is spelled (`remove_var`, `set_var` to empty,
/// a key constant); clauth itself never reads it.
/// The switch lives in crossterm's `Colored` display path, which is the only
/// place the TUI's colour is dropped under `NO_COLOR`, so an override call is
/// the one way colour gets past it that the backend-level draw test cannot
/// see.
const BANNED_COLOUR_OVERRIDE: [&str; 4] = [
    "set_ansi_color_disabled",
    "force_color_output",
    "ColorChoice::Always",
    "NO_COLOR",
];

/// Whether `code` names `banned` as a whole word: an identifier carrying the
/// banned name after a prefix (`unset_ansi_color_disabled`) is no call of the
/// banned name, so the match must start at a left boundary. A bare substring
/// match would flag one.
fn names_banned(code: &str, banned: &str) -> bool {
    code.match_indices(banned).any(|(at, _)| {
        at == 0
            || !code[..at]
                .chars()
                .next_back()
                .is_some_and(|c| c.is_alphanumeric() || c == '_')
    })
}

/// The colour-SGR spellings a Rust source line can hold, rewritten to the raw
/// ESC byte so [`colour_sgrs`] can read them: the string-escape spellings
/// `\x1b` / `\x1B`, and any `\u{…}` whose hex value is ESC (`\u{1b}`,
/// `\u{1B}`, leading zeros included — the digits are parsed, not matched, so
/// no spelling list can grow stale on them). A `\u{…}` holding any other
/// value stays unrewritten, and a raw ESC already in the text passes through
/// untouched.
fn escape_spellings_to_esc(code: &str) -> String {
    let mut out = String::with_capacity(code.len());
    let mut rest = code;
    while let Some(at) = rest.find("\\u{") {
        let tail = &rest[at + 3..];
        let hex_end = tail
            .find(|c: char| !c.is_ascii_hexdigit())
            .unwrap_or(tail.len());
        if tail[hex_end..].starts_with('}') && u32::from_str_radix(&tail[..hex_end], 16) == Ok(0x1b)
        {
            out.push_str(&rest[..at]);
            out.push('\x1b');
            rest = &tail[hex_end + 1..];
        } else {
            out.push_str(&rest[..at + 3]);
            rest = tail;
        }
    }
    out.push_str(rest);
    for spelling in ["\\x1b", "\\x1B"] {
        out = out.replace(spelling, "\x1b");
    }
    out
}

/// No named colour path in `src/` outside the theme module and the terminal
/// library: no call poking crossterm's process-wide colour switch, and no
/// hand-spelled colour SGR escape — the draw test above pins what the backend drops under
/// `NO_COLOR`; this pins the paths that never reach the backend, the stderr
/// spinner and `out` writers included. Known limits shared with
/// `no_bare_print_macro_under_src`: a `//` inside a string literal hides the
/// rest of its line, and a banned name or escape inside a block comment reds.
/// What it cannot see at all: an ESC not adjacent to its parameters in one
/// literal on one line — split literals, `concat!`, a `char` built at runtime
/// (`char::from(27)`, `'\x1b'`), `format!`-supplied parameters, a byte array,
/// a literal continued across lines, an escape in a non-`.rs` file `src/`
/// includes; any escape that is not a 7-bit SGR, such as an 8-bit CSI
/// (`\u{9b}`) or an OSC colour set (`\x1b]4;`, `\x1b]10;`, `\x1b]11;`); a
/// banned name split across literals or lines; a scrub that never names the
/// key (removing every variable in a loop); and a colouring library other
/// than crossterm and clap, whose calls hold no escape and no banned name. A
/// spelling that emits no ESC false-fires: a raw string (`r"\x1b[31m"`) or an
/// escaped backslash before it (`"\\x1b[31m"`).
#[test]
fn no_colour_override_or_sgr_literal_under_src() {
    let scanned =
        crate::testutil::rs_files(&std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src"));
    // An unreadable `src/` would leave the scan empty and the assertion below
    // vacuous, which is the one way this test could pass while blind.
    assert!(
        scanned.len() > 10,
        "the scan found {} files under src/ — it is not reading the crate",
        scanned.len()
    );
    let mut offenders = Vec::new();
    for file in scanned {
        let text = std::fs::read_to_string(&file).expect("read");
        for (n, line) in text.lines().enumerate() {
            // Comments name the escapes freely; only code counts.
            let code = line.split("//").next().unwrap_or_default();
            for name in BANNED_COLOUR_OVERRIDE {
                if names_banned(code, name) {
                    offenders.push(format!("{}:{}: {name}", file.display(), n + 1));
                }
            }
            for params in colour_sgrs(escape_spellings_to_esc(code).as_bytes()) {
                offenders.push(format!(
                    "{}:{}: colour SGR \\x1b[{params}m",
                    file.display(),
                    n + 1
                ));
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "draw colour through the theme module and the terminal library only: {offenders:?}"
    );
}

/// The scan's matchers on lines they must flag and lines they must not: the
/// spinner's cursor wipe and the platform module's OSC 52 clipboard write are
/// not colour, a SGR colour escape in any spelling, an override call, clap's
/// forced colour and a `NO_COLOR` scrub are, and neither a read of the
/// switch's state nor a prefixed identifier is a call of a banned name.
#[test]
fn the_colour_scan_flags_sgr_literals_and_overrides_and_skips_other_escapes() {
    assert!(names_banned(
        "Colored::set_ansi_color_disabled(false);",
        "set_ansi_color_disabled"
    ));
    assert!(names_banned(
        "crossterm::style::force_color_output(true);",
        "force_color_output"
    ));
    assert!(names_banned(
        "let _ = clap::ColorChoice::Always;",
        "ColorChoice::Always"
    ));
    for scrub in [
        "unsafe { std::env::remove_var(\"NO_COLOR\") };",
        "unsafe { std::env::set_var(\"NO_COLOR\", \"\") };",
        "const KEY: &str = \"NO_COLOR\";",
    ] {
        assert!(names_banned(scrub, "NO_COLOR"), "{scrub}");
    }
    assert!(!names_banned(
        "const CLAUTH_NO_COLOR_HINT: &str = \"x\";",
        "NO_COLOR"
    ));
    assert!(!names_banned(
        "let memo = Colored::ansi_color_disabled_memoized();",
        "set_ansi_color_disabled"
    ));
    assert!(!names_banned(
        "unset_ansi_color_disabled(true);",
        "set_ansi_color_disabled"
    ));
    assert!(!names_banned(
        "my_force_color_output();",
        "force_color_output"
    ));
    assert_eq!(
        colour_sgrs(escape_spellings_to_esc(r#"write!(w, "\x1b[31mred")"#).as_bytes()),
        ["31"]
    );
    assert_eq!(
        colour_sgrs(escape_spellings_to_esc(r#"write!(w, "\u{1b}[38;5;1m")"#).as_bytes()),
        ["38;5;1"]
    );
    assert_eq!(
        colour_sgrs(escape_spellings_to_esc(r#"write!(w, "\x1B[31m")"#).as_bytes()),
        ["31"]
    );
    assert_eq!(
        colour_sgrs(escape_spellings_to_esc(r#"write!(w, "\u{1B}[31m")"#).as_bytes()),
        ["31"]
    );
    assert_eq!(
        colour_sgrs(escape_spellings_to_esc(r#"write!(w, "\u{001b}[31m")"#).as_bytes()),
        ["31"]
    );
    assert_eq!(
        colour_sgrs(escape_spellings_to_esc(r#"write!(w, "\u{1c}[31m")"#).as_bytes()),
        Vec::<String>::new()
    );
    assert_eq!(
        colour_sgrs(escape_spellings_to_esc(r#"write!(err, "\r\x1b[2K")"#).as_bytes()),
        Vec::<String>::new()
    );
    assert_eq!(
        colour_sgrs(escape_spellings_to_esc(r#"write!(w, "\x1b]52;c;{}\x07")"#).as_bytes()),
        Vec::<String>::new()
    );
}

/// Every mark the render code draws comes from this module: a literal in a
/// render file is a second copy that drifts from its theme constant.
#[test]
fn no_mark_glyph_is_spelled_in_a_render_file() {
    const MARKS: [char; 10] = ['❯', '●', '○', '✓', '▶', '▼', '✎', '┃', '┊', '◌'];
    let render = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/tui/render");
    let scanned = crate::testutil::rs_files(&render);
    assert!(
        scanned.len() > 10,
        "the scan found {} files under src/tui/render — it is not reading the tree",
        scanned.len()
    );
    let mut offenders = Vec::new();
    for file in scanned {
        let text = std::fs::read_to_string(&file).expect("readable render file");
        for (n, line) in text.lines().enumerate() {
            let code = line.split("//").next().unwrap_or_default();
            for mark in MARKS {
                if code.contains(mark) {
                    offenders.push(format!("{}:{}: {mark}", file.display(), n + 1));
                }
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "draw marks through the theme module: {offenders:?}"
    );
}

/// The reviewed provider palette: Anthropic's crest is `ACCENT_2`, Codex's the
/// neutral `TEXT` glint, z.ai's a dark sweep below `TEXT_DIM`, the rest each
/// provider's own color.
#[test]
fn each_provider_crest_is_its_reviewed_color() {
    use super::{CrestProvider as P, crest_rgb};
    let expected = [
        (P::Anthropic, (217, 119, 87)),
        (P::DeepSeek, (0x67, 0x99, 0xFE)),
        (P::Zai, (0x8A, 0x90, 0xA6)),
        (P::Alibaba, (0xFF, 0x6A, 0x00)),
        (P::OpenRouter, (0xC8, 0xFF, 0x00)),
        (P::MiniMax, (0xFF, 0x51, 0x51)),
        (P::Codex, (205, 214, 244)),
    ];
    assert_eq!(
        expected.len(),
        P::ALL.len(),
        "every provider has a pinned crest"
    );
    for (provider, rgb) in expected {
        assert_eq!(crest_rgb(provider), rgb, "{provider:?}'s crest");
    }
}

/// WCAG 2.x relative luminance of an sRGB triple.
fn luminance((r, g, b): (u8, u8, u8)) -> f64 {
    let lin = |c: u8| {
        let c = f64::from(c) / 255.0;
        if c <= 0.04045 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * lin(r) + 0.7152 * lin(g) + 0.0722 * lin(b)
}

fn contrast(a: (u8, u8, u8), b: (u8, u8, u8)) -> f64 {
    let (la, lb) = (luminance(a), luminance(b));
    (la.max(lb) + 0.05) / (la.min(lb) + 0.05)
}

/// Every crest differs from the `TEXT_DIM` base it rides and holds 4.5:1
/// against both `BG` (30, 30, 46) and `BG_HOVER` (40, 40, 56), the backgrounds
/// a type cell sits on.
#[test]
fn every_provider_crest_clears_the_contrast_floor() {
    use super::{CrestProvider, crest_rgb};
    assert!(
        contrast((255, 255, 255), (0, 0, 0)) > 20.9,
        "control: the formula gives white on black its 21:1"
    );
    for provider in CrestProvider::ALL {
        let rgb = crest_rgb(provider);
        assert_ne!(
            rgb,
            (166, 173, 200),
            "{provider:?}'s crest differs from TEXT_DIM"
        );
        for (bg_name, bg) in [("BG", (30, 30, 46)), ("BG_HOVER", (40, 40, 56))] {
            let ratio = contrast(rgb, bg);
            assert!(ratio >= 4.5, "{provider:?} on {bg_name}: {ratio:.3}:1");
        }
    }
}
