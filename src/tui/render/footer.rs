//! Bottom strip: key hints, or a footer alert in place when one is active.

use ratatui::Frame;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::super::app::{
    App, ConfigFocus, ConfigRow, FallbackHint, FooterAlert, GLOBAL_CONFIG_ROWS, GlobalConfigRow,
    HERDR_OPTIONS, HerdrOption, LoginSession, PluginFocus, StatusFocus, Tab, TokenView,
    build_action_menu, config_rows, fallback_hint, has_sub_focus, herdr_config_writable,
};
use super::super::theme;
use super::format::spinner_frame;

const TAB_NAV: (&str, &str) = ("←→", "tabs");

pub(super) fn draw(frame: &mut Frame<'_>, area: Rect, app: &App) {
    // 1-col breathing room on each side; the alert row (which replaces this in
    // place) shares the same inset so the left margin never jumps.
    let area = inset_x(area, 1);

    // A login in flight owns the footer, independent of `footer_alert` so key
    // handling that clears alerts can't hide it.
    if let Some(session) = &app.login {
        // Any open modal owns esc/q (the login modal collapses, others handle
        // their own keys), so the hint flips to `q back` for the whole stack.
        let modal_open = !app.modals.is_empty();
        draw_login(frame, area, session, modal_open, app.tick_count);
        return;
    }

    // A live alert replaces the hint bar in place — one footer row, never stacked.
    if let Some(alert) = &app.footer_alert {
        draw_alert(frame, area, alert);
        return;
    }

    // `q` label: "back" in a sub-focus, "quit" at top level.
    // (While armed the alert row shows instead, so this label stays "quit".)
    let q_label: &str = if has_sub_focus(app) { "back" } else { "quit" };

    let tail: &[(&str, &str)] = match app.tab {
        Tab::Overview => &[("⇧↑↓", "reorder"), ("a", "actions"), ("?", "help")],
        Tab::Usage => &[
            ("↑↓", "account"),
            ("r", "refresh account"),
            ("a", "actions"),
            ("?", "help"),
        ],
        Tab::Tokens => match app.token_view {
            TokenView::Dashboard => &[
                ("↵", "models"),
                ("r", "reload"),
                ("c", "count cache"),
                ("t", "period"),
                ("a", "actions"),
                ("?", "help"),
            ],
            TokenView::Models => &[
                ("↑↓", "model"),
                ("c", "count cache"),
                ("t", "period"),
                ("a", "actions"),
                ("?", "help"),
            ],
        },
        Tab::Setup => match app.config_focus {
            ConfigFocus::Profiles => &[
                ("↑↓", "account"),
                ("↵", "configure"),
                ("n", "new"),
                ("a", "actions"),
                ("?", "help"),
            ],
            ConfigFocus::Actions => {
                // Row-aware: the `model` row cycles on space; env rows edit a value
                // or open the add-env key editor.
                match config_rows(app).get(app.config_action_cursor) {
                    Some(ConfigRow::Model) => &[
                        ("↑↓", "row"),
                        ("space", "cycle"),
                        ("↵", "custom"),
                        ("a", "actions"),
                        ("?", "help"),
                    ],
                    Some(ConfigRow::EnvEntry(_)) => &[
                        ("↑↓", "row"),
                        ("↵", "edit value"),
                        ("a", "actions"),
                        ("?", "help"),
                    ],
                    Some(ConfigRow::EnvAdd) => &[
                        ("↑↓", "row"),
                        ("↵", "add env"),
                        ("a", "actions"),
                        ("?", "help"),
                    ],
                    // The reveal chip has no `a` actions, so it isn't advertised.
                    Some(ConfigRow::ModelOverrideAdd) => {
                        &[("↑↓", "row"), ("↵", "add override"), ("?", "help")]
                    }
                    _ => &[
                        ("↑↓", "row"),
                        ("↵", "edit / toggle"),
                        ("a", "actions"),
                        ("?", "help"),
                    ],
                }
            }
        },
        Tab::Config => {
            if app.refresh_interval_draft.is_some() || app.weekly_threshold_draft.is_some() {
                &[("↵", "save"), ("←→", "caret"), ("esc", "cancel")]
            } else if GLOBAL_CONFIG_ROWS
                .get(app.global_config_cursor)
                .is_some_and(|r| {
                    matches!(
                        r,
                        GlobalConfigRow::RefreshInterval | GlobalConfigRow::WeeklyThreshold
                    )
                })
            {
                &[
                    ("↑↓", "row"),
                    ("space", "cycle"),
                    ("↵", "custom"),
                    ("?", "help"),
                ]
            } else {
                &[("↑↓", "row"), ("space/↵", "cycle / toggle"), ("?", "help")]
            }
        }
        Tab::Status => match app.status.focus {
            StatusFocus::List => &[
                ("↑↓", "incident"),
                ("↵", "open"),
                ("r", "refresh"),
                ("a", "actions"),
                ("?", "help"),
            ],
            StatusFocus::Detail => &[("↑↓", "scroll"), ("a", "actions"), ("?", "help")],
        },
        Tab::Plugin => plugin_hints(app),
        Tab::Fallback => match fallback_hint(app) {
            FallbackHint::Empty => &[("?", "help")],
            FallbackHint::ChainMember => &[
                ("↑↓", "move"),
                ("⇧↑↓", "reorder"),
                ("↵", "open"),
                ("a", "actions"),
                ("?", "help"),
            ],
            FallbackHint::ChainAdd => &[("↑↓", "move"), ("↵", "add"), ("?", "help")],
            FallbackHint::DetailThreshold => &[
                ("↑↓", "row"),
                ("+", "raise"),
                ("-", "lower"),
                ("↵", "type"),
                ("a", "actions"),
                ("?", "help"),
            ],
            FallbackHint::DetailThresholdEdit => {
                &[("↵", "save"), ("←→", "caret"), ("esc", "cancel")]
            }
            FallbackHint::DetailWeeklyAt => &[
                ("↑↓", "row"),
                ("+", "raise"),
                ("-", "lower"),
                ("↵", "type"),
                ("a", "actions"),
                ("?", "help"),
            ],
            FallbackHint::DetailWeeklyAtEdit => {
                &[("↵", "save"), ("←→", "caret"), ("esc", "cancel")]
            }
            FallbackHint::DetailCheckWeekly
            | FallbackHint::DetailCheckScoped
            | FallbackHint::DetailLastResort
            | FallbackHint::DetailPreferred => &[
                ("↑↓", "row"),
                ("space/↵", "toggle"),
                ("a", "actions"),
                ("?", "help"),
            ],
            FallbackHint::DetailMaxSpend => &[
                ("↑↓", "row"),
                ("↵", "type"),
                ("a", "actions"),
                ("?", "help"),
            ],
            FallbackHint::DetailMaxSpendEdit => {
                &[("↵", "save"), ("←→", "caret"), ("esc", "cancel")]
            }
            FallbackHint::DetailRemove => &[
                ("↑↓", "row"),
                ("↵", "remove"),
                ("a", "actions"),
                ("?", "help"),
            ],
            FallbackHint::DetailRemoveArmed => {
                &[("↵", "confirm remove"), ("esc", "cancel"), ("?", "help")]
            }
            FallbackHint::DetailAdd => &[("↑↓", "pick"), ("↵", "add"), ("?", "help")],
        },
    };

    // Suppress the trailing `q` hint only where `q` is fully captured by the
    // screen (threshold edit / max-spend edit / armed-remove / refresh-interval
    // edit own the keyboard entirely). Every other sub-focus shows `q back` via
    // `q_label` per the cloudy-tui contract.
    let show_q = !((app.tab == Tab::Fallback
        && matches!(
            fallback_hint(app),
            FallbackHint::DetailThresholdEdit
                | FallbackHint::DetailMaxSpendEdit
                | FallbackHint::DetailWeeklyAtEdit
                | FallbackHint::DetailRemoveArmed
        ))
        || (app.tab == Tab::Config && app.refresh_interval_draft.is_some())
        || (app.tab == Tab::Plugin && app.plugin.herdr_tag_draft.is_some()));

    let mut hints: Vec<(&str, &str)> = std::iter::once(TAB_NAV)
        .chain(tail.iter().copied())
        .collect();

    if show_q {
        hints.push(("q", q_label));
    }

    // `a` opens nothing where the context carries no action of its own (the
    // whole Fallback tab, a Setup text row). Reading the real menu keeps the
    // hint honest per row instead of leaving each arm's literal to drift.
    if build_action_menu(app).items.is_empty() {
        hints.retain(|(key, _)| *key != "a");
    }

    // Measured degradation for narrow terminals: while the row overflows, drop
    // the rightmost non-essential hint. Navigation (`←→`), discoverability
    // (`? help`), exits (`q`/`esc`) and armed confirms always survive; on a
    // desktop-width terminal everything fits and nothing changes.
    let row_width = |hints: &[(&str, &str)]| -> usize {
        let cells: usize = hints
            .iter()
            .map(|(k, l)| k.chars().count() + 1 + l.chars().count())
            .sum();
        cells + hints.len().saturating_sub(1) * 3
    };
    let essential = |(key, label): &(&str, &str)| {
        matches!(*key, "←→" | "?" | "q" | "esc") || label.starts_with("confirm")
    };
    while row_width(&hints) > area.width as usize {
        match hints.iter().rposition(|h| !essential(h)) {
            Some(i) => {
                hints.remove(i);
            }
            None => break,
        }
    }

    let mut spans: Vec<Span<'_>> = Vec::new();
    for (i, (key, label)) in hints.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled("   ", theme::faint()));
        }
        spans.push(Span::styled(*key, theme::accent().bold()));
        spans.push(Span::raw(" "));
        spans.push(Span::styled(*label, theme::dim()));
    }

    frame.render_widget(
        Paragraph::new(Line::from(spans))
            .style(theme::base())
            .alignment(Alignment::Left),
        area,
    );
}

/// Plugin tab hints. `f` only fixes a row that actually offers one — never
/// advertised where pressing it is a no-op. The herdr detail walks focusable
/// option rows instead of scrolling, so its hints name the row's own keys.
fn plugin_hints(app: &App) -> &'static [(&'static str, &'static str)] {
    match app.plugin.focus {
        PluginFocus::List => {
            if app.plugin.selected_fix().is_some() {
                &[
                    ("↑↓", "row"),
                    ("↵", "detail"),
                    ("r", "refresh"),
                    ("f", "fix"),
                    ("?", "help"),
                ]
            } else {
                &[
                    ("↑↓", "row"),
                    ("↵", "detail"),
                    ("r", "refresh"),
                    ("?", "help"),
                ]
            }
        }
        PluginFocus::Detail => plugin_detail_hints(app),
    }
}

/// Plugin detail hints, row-aware for the herdr options section: the tag
/// editor owns the keyboard while open (⏎ saves, ⎋ discards), the
/// tag-refresh row advertises its stepper keys, the delegate-row row its
/// confirm — and an inert delegate-row row advertises no activation key at
/// all, since the key is a no-op there.
fn plugin_detail_hints(app: &App) -> &'static [(&'static str, &'static str)] {
    if app.plugin.herdr_tag_draft.is_some() {
        return &[("↵", "save"), ("←→", "caret"), ("esc", "cancel")];
    }
    let fix = app.plugin.selected_fix().is_some();
    if !app
        .plugin
        .selected_check()
        .is_some_and(|c| c.label == "herdr")
    {
        return if fix {
            &[
                ("↑↓", "scroll"),
                ("r", "refresh"),
                ("f", "fix"),
                ("?", "help"),
            ]
        } else {
            &[("↑↓", "scroll"), ("r", "refresh"), ("?", "help")]
        };
    }
    // `r` and `f` keep working while the options rows hold the cursor, so they
    // keep their hints (f only when the check offers a fix).
    match HERDR_OPTIONS.get(app.plugin.herdr_options_cursor) {
        Some(HerdrOption::TagRefresh) => {
            if fix {
                &[
                    ("↑↓", "row"),
                    ("+", "raise"),
                    ("-", "lower"),
                    ("↵", "type"),
                    ("r", "refresh"),
                    ("f", "fix"),
                    ("?", "help"),
                ]
            } else {
                &[
                    ("↑↓", "row"),
                    ("+", "raise"),
                    ("-", "lower"),
                    ("↵", "type"),
                    ("r", "refresh"),
                    ("?", "help"),
                ]
            }
        }
        Some(HerdrOption::DelegateRowText) if herdr_config_writable(app) => {
            if fix {
                &[
                    ("↑↓", "row"),
                    ("space/↵", "rewrite row"),
                    ("r", "refresh"),
                    ("f", "fix"),
                    ("?", "help"),
                ]
            } else {
                &[
                    ("↑↓", "row"),
                    ("space/↵", "rewrite row"),
                    ("r", "refresh"),
                    ("?", "help"),
                ]
            }
        }
        // The inert delegate-row row advertises no activation key — it is a
        // no-op there.
        Some(HerdrOption::DelegateRowText) => {
            if fix {
                &[("↑↓", "row"), ("r", "refresh"), ("f", "fix"), ("?", "help")]
            } else {
                &[("↑↓", "row"), ("r", "refresh"), ("?", "help")]
            }
        }
        _ => {
            if fix {
                &[
                    ("↑↓", "row"),
                    ("space/↵", "cycle / toggle"),
                    ("r", "refresh"),
                    ("f", "fix"),
                    ("?", "help"),
                ]
            } else {
                &[
                    ("↑↓", "row"),
                    ("space/↵", "cycle / toggle"),
                    ("r", "refresh"),
                    ("?", "help"),
                ]
            }
        }
    }
}

/// Shrink a rect by `pad` columns on each side (clamped), leaving the row intact.
fn inset_x(area: Rect, pad: u16) -> Rect {
    Rect {
        x: area.x.saturating_add(pad),
        width: area.width.saturating_sub(pad.saturating_mul(2)),
        ..area
    }
}

/// Render a footer alert in place of the hint bar.
/// `! <message>` — glyph in `WARNING`, message in `TEXT_DIM`.
fn draw_alert(frame: &mut Frame<'_>, area: Rect, alert: &FooterAlert) {
    let FooterAlert::Warn(msg) = alert;
    let spans = vec![
        Span::styled("! ", Style::default().fg(theme::warning_color())),
        Span::styled(msg.as_str(), theme::dim()),
    ];
    frame.render_widget(
        Paragraph::new(Line::from(spans))
            .style(theme::base())
            .alignment(Alignment::Left),
        area,
    );
}

/// Login-in-progress line. Independent of `footer_alert` so key handling that
/// clears alerts never hides it. The authorize URL and live stage render in
/// the login modal; this row is the collapsed view. The trailing hint tracks
/// what esc/q actually do this frame: with any modal open they go to the
/// modal (`q back`); collapsed, both cancel the login (`esc cancel`).
fn draw_login(
    frame: &mut Frame<'_>,
    area: Rect,
    session: &LoginSession,
    modal_open: bool,
    tick: u64,
) {
    let (key, action) = if modal_open {
        ("   q ", "back")
    } else {
        ("   esc ", "cancel")
    };
    let spans = vec![
        Span::styled(format!("{} ", spinner_frame(tick)), theme::accent()),
        Span::styled(
            format!(
                "logging in '{}' (complete it in your browser)",
                session.name
            ),
            theme::dim(),
        ),
        Span::styled(key, theme::accent().bold()),
        Span::styled(action, theme::dim()),
    ];
    frame.render_widget(
        Paragraph::new(Line::from(spans))
            .style(theme::base())
            .alignment(Alignment::Left),
        area,
    );
}
