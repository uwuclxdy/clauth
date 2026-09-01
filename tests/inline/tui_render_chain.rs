//! `member_detail`'s all-exhausted "resumes: <name> in ~<eta>" caption on the
//! Fallback tab (issue #10 follow-up), driven by `crate::fallback::soonest_resume`.

use super::*;
use crate::profile::{AppState, Profile, ProfileName};
use crate::usage::{UsageInfo, UsageWindow, epoch_secs_to_iso, now_epoch_secs};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use std::collections::BTreeMap;

/// ISO reset `secs` in the future.
fn reset_in(secs: i64) -> String {
    epoch_secs_to_iso(now_epoch_secs() + secs)
}

fn profile(name: &str, threshold: f64, util: f64, reset_secs: i64) -> Profile {
    Profile {
        name: name.into(),
        base_url: None,
        api_key: None,
        auto_start: false,
        env: BTreeMap::new(),
        models: Default::default(),
        fallback_threshold: Some(threshold),
        weekly_threshold: None,
        last_resort: false,
        preferred: false,
        rolling_token: false,
        max_auto_spend: None,
        check_weekly: true,
        check_scoped: true,
        bell_threshold: None,
        disabled: false,
        console: None,
        credentials: None,
        usage: Some(UsageInfo {
            five_hour: Some(UsageWindow {
                utilization: util,
                resets_at: Some(reset_in(reset_secs)),
            }),
            ..UsageInfo::default()
        }),
        fetch_status: None,
        provider: None,
        third_party_usage: None,
    }
}

fn config_with(profiles: Vec<Profile>, active: Option<&str>, chain: Vec<&str>) -> AppConfig {
    let names: Vec<ProfileName> = profiles.iter().map(|p| p.name.clone()).collect();
    AppConfig {
        state: AppState {
            active_profile: active.map(Into::into),
            profiles: names,
            fallback_chain: chain.into_iter().map(Into::into).collect(),
            ..AppState::default()
        },
        profiles,
    }
}

fn line_text(line: &Line<'static>) -> String {
    line.spans.iter().map(|s| s.content.as_ref()).collect()
}

fn resumes_line(lines: &[Line<'static>]) -> Option<String> {
    lines.iter().map(line_text).find(|t| t.contains("resumes:"))
}

/// `QueueView` is the one resolver every chip goes through: membership from
/// the shared rule, the slot from the anchor. Pinned here because a view that
/// answered `None` for everyone would silently remove every chip on every
/// surface without failing a single render test.
#[test]
fn auto_start_queue_view_resolves_slots_from_config_and_anchor() {
    use crate::profile::{ClaudeCredentials, OAuthToken};
    let opted = |name: &str| {
        let mut p = profile(name, 95.0, 10.0, 3600);
        p.auto_start = true;
        p.credentials = Some(ClaudeCredentials {
            claude_ai_oauth: Some(OAuthToken {
                access_token: format!("{name}-access"),
                refresh_token: None,
                expires_at: None,
                scopes: None,
                subscription_type: None,
            }),
        });
        p
    };
    let mut cfg = config_with(
        vec![opted("a"), opted("b"), profile("c", 95.0, 0.0, 3600)],
        Some("a"),
        vec!["a", "b"],
    );
    cfg.state.auto_start_queue = true;

    let none = std::collections::HashMap::new();
    let view = super::super::panes::QueueView::new(&cfg, &none, None);
    let slot = view.slot("a").expect("an opted-in member holds a slot");
    assert_eq!((slot.position, slot.total), (1, 2));
    assert_eq!(view.slot("b").map(|s| s.position), Some(2));
    assert_eq!(view.slot("c"), None, "no opt-in, no slot");
    assert_eq!(
        slot.next_in, None,
        "no anchor: the queue is due, nothing to count down"
    );

    // A switch-grade-blocked member holds no slot, and the queue re-sizes.
    let lifts = std::collections::HashMap::from([("a".to_string(), 0i64)]);
    let view = super::super::panes::QueueView::new(&cfg, &lifts, None);
    assert_eq!(view.slot("a"), None, "blocked: no slot");
    assert_eq!(view.slot("b").map(|s| (s.position, s.total)), Some((1, 1)));

    // The toggle off is a real off switch on this surface too.
    cfg.state.auto_start_queue = false;
    let view = super::super::panes::QueueView::new(&cfg, &none, None);
    assert_eq!(view.slot("a"), None);
}

/// The Fallback card's auto-start row: the queue slot as a pill, with the queue's
/// shared next-open trailing outside the brackets — the house `[ label ]
/// qualifier` shape the blocked-reason pills already use for a kick block's
/// lift ETA.
///
/// It gets its own `auto-start` key rather than stacking into the pill block
/// above, because that block is the BLOCKED-reason ladder and a queue slot is
/// not a reason the chain is routing around this member.
#[test]
fn auto_start_queue_chip_renders_the_slot_and_the_queues_next_open() {
    let cfg = config_with(vec![profile("a", 95.0, 10.0, 3600)], Some("a"), vec!["a"]);
    let card = |slot: Option<crate::usage::QueueSlot>, width: usize| {
        member_detail(
            &cfg,
            &crate::profile::ProfileName::from("a"),
            MemberCard {
                width,
                queue: slot,
                ..Default::default()
            },
        )
        .0
        .iter()
        .map(line_text)
        .collect::<Vec<_>>()
    };
    let slot = |next_in: Option<i64>| {
        Some(crate::usage::QueueSlot {
            position: 1,
            total: 2,
            next_in,
        })
    };
    let queue_row = |card: &[String]| {
        let at = card
            .iter()
            .position(|t| t.contains("auto-start"))
            .expect("a queue member's card carries the auto-start row");
        // The row explains itself: pill + countdown, no `└` tooltip beneath —
        // the trailing blank follows it directly.
        assert_eq!(card[at + 1], "", "no tooltip line under the queue row");
        card[at].clone()
    };

    // Exact-match, the house rule for a surface pin: a drifted separator,
    // gutter, pill shape, or padding must red, not just a vanished substring.
    // The countdown is right-aligned at the card's inner width.
    let row = queue_row(&card(slot(Some(8880)), 60));
    assert_eq!(
        row,
        "auto-start   [ queue 1/2 ]                    next in 2h 28m"
    );

    // A narrow card sheds the countdown's leading words, never its digits —
    // rendered inline this clipped to `next in 2`.
    let row = queue_row(&card(slot(Some(8880)), 34));
    assert_eq!(row, "auto-start   [ queue 1/2 ]  2h 28m");

    // Gap passed: the slot still renders (membership is the standing fact), the
    // countdown does not — there is nothing left to wait for.
    let row = queue_row(&card(slot(None), 60));
    assert_eq!(row, "auto-start   [ queue 1/2 ]");

    // A non-member's card is byte-identical to the pre-queue one: no row, no
    // blank line reserved for one.
    assert!(
        !card(None, 60).iter().any(|t| t.contains("auto-start")),
        "an account outside the queue gets no auto-start row"
    );
}

// Whole chain exhausted: the caption renders under whichever member is
// selected, naming the soonest-resuming one (b resets sooner than a).
#[test]
fn all_exhausted_shows_resumes_hint_under_any_selected_member() {
    let a = profile("a", 95.0, 100.0, 3600);
    let b = profile("b", 95.0, 100.0, 1800);
    let cfg = config_with(vec![a, b], Some("a"), vec!["a", "b"]);

    let on_a = member_detail(
        &cfg,
        &crate::profile::ProfileName::from("a"),
        MemberCard {
            width: 60,
            ..Default::default()
        },
    )
    .0;
    let hint_a = resumes_line(&on_a).expect("resumes hint renders while viewing member a");
    assert!(hint_a.contains("resumes: b in ~"), "{hint_a}");

    let on_b = member_detail(
        &cfg,
        &crate::profile::ProfileName::from("b"),
        MemberCard {
            width: 60,
            ..Default::default()
        },
    )
    .0;
    let hint_b = resumes_line(&on_b).expect("resumes hint renders while viewing member b");
    assert!(hint_b.contains("resumes: b in ~"), "{hint_b}");
}

// b still has headroom — chain isn't fully exhausted, caption stays hidden.
#[test]
fn partially_exhausted_chain_hides_resumes_hint() {
    let a = profile("a", 95.0, 100.0, 3600);
    let b = profile("b", 95.0, 20.0, 3600);
    let cfg = config_with(vec![a, b], Some("a"), vec!["a", "b"]);

    let lines = member_detail(
        &cfg,
        &crate::profile::ProfileName::from("a"),
        MemberCard {
            width: 60,
            ..Default::default()
        },
    )
    .0;
    assert!(
        resumes_line(&lines).is_none(),
        "must not show when the chain isn't fully exhausted"
    );
}

// ── help-hint wrapping + dynamic copy ────────────────────────────────────────

// A narrow detail pane wraps the selected row's hint into `└ `-led +
// indented continuation lines instead of clipping it off the pane edge.
#[test]
fn last_resort_hint_wraps_on_a_narrow_pane() {
    let a = profile("a", 95.0, 20.0, 3600);
    let b = profile("b", 95.0, 20.0, 3600);
    let cfg = config_with(vec![a, b], Some("a"), vec!["a", "b"]);

    // Focused on the `last resort` row at 28 cols.
    let lr = FALLBACK_ROWS
        .iter()
        .position(|r| *r == FallbackRow::LastResort)
        .unwrap();
    let lines = member_detail(
        &cfg,
        &crate::profile::ProfileName::from("a"),
        MemberCard {
            focused: true,
            row_cursor: lr,
            width: 28,
            ..Default::default()
        },
    )
    .0;
    let texts: Vec<String> = lines.iter().map(line_text).collect();
    let lead = texts
        .iter()
        .position(|t| t.starts_with(" └ "))
        .expect("hint leader line renders");
    assert!(
        texts[lead].chars().count() <= 28,
        "first hint line must fit the pane: {:?}",
        texts[lead]
    );
    // Exactly the leader's width, so the continuation stacks under the text
    // rather than under the `└` (or one cell past it).
    assert!(
        texts[lead + 1].starts_with("   ") && !texts[lead + 1].starts_with("    "),
        "hint continues on an indented line instead of clipping: {:?}",
        texts[lead + 1]
    );
}

// The last-resort hint names the member the exclusive mark would move from.
#[test]
fn last_resort_hint_names_the_currently_marked_member() {
    let a = profile("a", 95.0, 20.0, 3600);
    let mut b = profile("b", 95.0, 20.0, 3600);
    b.last_resort = true;
    let cfg = config_with(vec![a, b], Some("a"), vec!["a", "b"]);

    let lines = member_detail(
        &cfg,
        &crate::profile::ProfileName::from("a"),
        MemberCard {
            focused: true,
            row_cursor: 4,
            width: 80,
            ..Default::default()
        },
    )
    .0;
    let hint = lines
        .iter()
        .map(line_text)
        .find(|t| t.contains("└"))
        .expect("hint renders");
    assert!(hint.contains("instead of 'b'"), "{hint}");
}

// The per-account usage-gate rows render as toggles whose hint states the
// CURRENT walk behavior for this account, flipping wording with the gate.
#[test]
fn usage_gate_rows_hint_the_current_state() {
    let texts = |p: Profile, cursor: usize| -> Vec<String> {
        let cfg = config_with(vec![p], Some("a"), vec!["a"]);
        member_detail(
            &cfg,
            &crate::profile::ProfileName::from("a"),
            MemberCard {
                focused: true,
                row_cursor: cursor,
                width: 80,
                ..Default::default()
            },
        )
        .0
        .iter()
        .map(line_text)
        .collect()
    };

    // FALLBACK_ROWS[2] == CheckWeekly.
    let on = texts(profile("a", 95.0, 20.0, 3600), 2);
    assert!(on.iter().any(|t| t.contains("weekly gate")), "{on:?}");
    assert!(
        on.iter()
            .any(|t| t.contains("out of rotation") && t.contains("weekly usage")),
        "{on:?}"
    );
    let mut p = profile("a", 95.0, 20.0, 3600);
    p.check_weekly = false;
    let off = texts(p, 2);
    assert!(off.iter().any(|t| t.contains("isn't checked")), "{off:?}");

    // FALLBACK_ROWS[3] == CheckScoped.
    let on = texts(profile("a", 95.0, 20.0, 3600), 3);
    assert!(on.iter().any(|t| t.contains("scoped gate")), "{on:?}");
    assert!(on.iter().any(|t| t.contains("per-model week")), "{on:?}");
    let mut p = profile("a", 95.0, 20.0, 3600);
    p.check_scoped = false;
    let off = texts(p, 3);
    assert!(
        off.iter()
            .any(|t| t.contains("stays in rotation for other models")),
        "{off:?}"
    );
}

// A gated-on per-model week over the line pills the detail card with its
// label; the gate off drops it (chip and walk must not drift).
#[test]
fn scoped_spent_pill_names_the_window_and_respects_the_gate() {
    let scoped = |check_scoped: bool| -> Profile {
        let mut p = profile("a", 95.0, 20.0, 3600);
        p.check_scoped = check_scoped;
        if let Some(u) = p.usage.as_mut() {
            u.seven_day = Some(UsageWindow {
                utilization: 40.0,
                resets_at: Some(reset_in(5 * 86_400)),
            });
            u.weekly_scoped = vec![crate::usage::ScopedWindow {
                label: "7d fable".to_string(),
                window: UsageWindow {
                    utilization: 100.0,
                    resets_at: Some(reset_in(5 * 86_400)),
                },
            }];
        }
        p
    };

    let cfg = config_with(vec![scoped(true)], Some("a"), vec!["a"]);
    let lines = member_detail(
        &cfg,
        &crate::profile::ProfileName::from("a"),
        MemberCard {
            width: 80,
            ..Default::default()
        },
    )
    .0;
    let pill = lines
        .iter()
        .map(line_text)
        .find(|t| t.contains("7d fable"))
        .expect("scoped pill renders");
    assert!(pill.contains("other models ok"), "{pill}");

    let cfg = config_with(vec![scoped(false)], Some("a"), vec!["a"]);
    let lines = member_detail(
        &cfg,
        &crate::profile::ProfileName::from("a"),
        MemberCard {
            width: 80,
            ..Default::default()
        },
    )
    .0;
    assert!(
        !lines.iter().map(line_text).any(|t| t.contains("7d fable")),
        "gate off must drop the scoped pill"
    );
}

// The `max spend` hint names whichever half of the opt-in is holding spending
// back, and shows the REAL armed room when both are set. Five distinct copies,
// one per spend state — `spend_room` fails closed on money, so an unknown spend
// never reads as a $0 figure.
#[test]
fn max_spend_hint_covers_every_spend_state() {
    use crate::usage::SpendInfo;
    let hint = |budget_on: bool, ceiling: Option<f64>, spend: Option<SpendInfo>| -> String {
        let mut a = profile("a", 95.0, 40.0, 7200);
        a.max_auto_spend = ceiling;
        a.usage.as_mut().unwrap().spend = spend;
        let mut cfg = config_with(vec![a], Some("a"), vec!["a"]);
        cfg.state.spend_budget_switching = budget_on;
        max_spend_hint(
            &cfg,
            &crate::profile::ProfileName::from("a"),
            cfg.profiles[0].max_auto_spend.unwrap_or(0.0),
        )
    };
    let billing = |enabled: bool, used: Option<f64>| SpendInfo {
        enabled,
        used,
        limit: Some(20.0),
        percent: None,
        currency: None,
    };

    // 1. chain toggle off
    assert!(
        hint(false, Some(10.0), Some(billing(true, Some(1.0))))
            .contains("turn on allow extra usage")
    );
    // 2. no ceiling
    assert!(hint(true, None, Some(billing(true, Some(1.0)))).contains("type a ceiling"));
    // 3. account not billing
    assert!(
        hint(true, Some(10.0), Some(billing(false, Some(1.0))))
            .contains("isn't set up for paid usage")
    );
    // 4. spend unknown → the ceiling statement, never an invented $0 room
    let unknown = hint(true, Some(10.0), Some(billing(true, None)));
    assert!(unknown.contains("spends at most $10.00"), "{unknown}");
    // 5. armed → the real room: 0.9 * min(20, 10) - 1 = $8.00
    let armed = hint(true, Some(10.0), Some(billing(true, Some(1.0))));
    assert!(armed.contains("$8.00 left to spend"), "{armed}");
}

// ── key-column alignment ────────────────────────────────────────────────────

/// Column a row's value opens at: the first non-space cell past the key text.
/// `str::find` is byte-based, so re-count chars for the head to stay glyph-
/// accurate past any multi-byte arrow (e.g. `❯`).
fn value_col(key: &str, rendered: &str) -> usize {
    let after = rendered.find(key).expect("row renders its key") + key.len();
    let head_chars = rendered[..after].chars().count();
    let gap = rendered[after..].chars().take_while(|c| *c == ' ').count();
    head_chars + gap
}

// `last resort` is exactly the old `KEY_W` (11) chars, so a
// `saturating_sub(len).max(1)` pad pushed its value a column right of every
// other interactive row. The shared `key_cell` keeps the gap separate from the
// width, so every interactive row opens its value at the same column.
#[test]
fn last_resort_value_aligns_with_other_rows() {
    let a = profile("a", 95.0, 20.0, 3600);
    let cfg = config_with(vec![a], Some("a"), vec!["a"]);
    let texts: Vec<String> = member_detail(
        &cfg,
        &crate::profile::ProfileName::from("a"),
        MemberCard {
            focused: true,
            row_cursor: 1,
            width: 60,
            ..Default::default()
        },
    )
    .0
    .iter()
    .map(line_text)
    .collect();

    let rotate = texts
        .iter()
        .find(|t| t.contains("rotate at"))
        .expect("rotate at row");
    let last = texts
        .iter()
        .find(|t| t.contains("last resort"))
        .expect("last resort row");
    let remove = texts
        .iter()
        .find(|t| t.contains("remove"))
        .expect("remove row");

    let col = value_col("rotate at", rotate);
    assert_eq!(
        col,
        value_col("last resort", last),
        "`last resort` (== old KEY_W chars) must not push its value column right"
    );
    assert_eq!(
        col,
        value_col("remove", remove),
        "all rows share the value column"
    );

    let spend = texts
        .iter()
        .find(|t| t.contains("max spend"))
        .expect("max spend row");
    assert_eq!(
        col,
        value_col("max spend", spend),
        "all rows share the value column"
    );
}

/// The ceiling row reads as a state, not a bare number: $0 is the never-spend
/// default and must not look like a figure the operator dialled in. A set
/// ceiling renders as money, with the cents, since it is money.
#[test]
fn max_spend_row_renders_off_at_zero_and_dollars_when_set() {
    let cfg = config_with(vec![profile("a", 95.0, 20.0, 3600)], Some("a"), vec!["a"]);
    let row = |c: &crate::profile::AppConfig| -> String {
        member_detail(
            c,
            &crate::profile::ProfileName::from("a"),
            MemberCard {
                focused: true,
                row_cursor: 1,
                width: 60,
                ..Default::default()
            },
        )
        .0
        .iter()
        .map(line_text)
        .find(|t| t.contains("max spend"))
        .expect("max spend row")
    };
    assert!(
        row(&cfg).contains("off"),
        "unset reads as off: {:?}",
        row(&cfg)
    );
    assert!(!row(&cfg).contains('$'), "no dollar figure when off");

    let mut armed = config_with(vec![profile("a", 95.0, 20.0, 3600)], Some("a"), vec!["a"]);
    armed.profiles[0].max_auto_spend = Some(25.0);
    assert!(
        row(&armed).contains("$25.00"),
        "a set ceiling renders as money: {:?}",
        row(&armed)
    );
}

// ── disabled chain member (feature: per-account disable toggle) ─────────────

/// `Disabled` and `Canceled` share the `⊖` shape and split on hue alone — the
/// one deliberate departure from cloudy-tui's shape-names-the-state rule (see
/// `reason_marker`). Pinned here because giving either arm its own shape puts
/// the same account under two glyphs across the Overview's two panels: the
/// account row picks the canceled arm where this ladder picks the disabled one.
#[test]
fn disabled_and_canceled_share_the_marker_shape_and_split_on_hue() {
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let dis = reason_marker(&BlockedReason::Disabled);
    let can = reason_marker(&BlockedReason::Canceled);
    assert_eq!(dis.content, "⊖", "disabled marker shape");
    assert_eq!(can.content, "⊖", "canceled marker shape");
    assert_eq!(dis.style.fg, theme::faint().fg, "disabled reads uncharged");
    assert_eq!(can.style.fg, theme::danger().fg, "canceled reads dead");
}

/// A disabled chain member — still configured in `fallback_chain` on disk,
/// only the walk skips it (see `Profile::is_disabled`)
/// — dims its name in the Fallback selector and carries the `⊖` blocked-reason
/// marker, with the `[ disabled ]` label reaching the operator through the
/// detail card's `reason_pill`. The add-picker exclusion (a disabled account
/// can never be (re-)added) is a pure-logic concern covered separately in
/// `tests/inline/tui_app.rs`'s `chain_candidates_excludes_a_disabled_profile`.
#[test]
fn disabled_chain_member_dims_its_name_and_takes_the_blocked_reason_marker() {
    let _home = crate::testutil::HomeSandbox::new();
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let mut a = profile("xqzacct", 95.0, 10.0, 3600);
    a.disabled = true;
    let cfg = config_with(vec![a], None, vec!["xqzacct"]);
    let app = App::new(cfg);

    let (w, h) = (100u16, 14u16);
    let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
    term.draw(|f| super::draw(f, f.area(), &app)).unwrap();
    let buf = term.backend().buffer();
    let rows = crate::testutil::buffer_rows(buf);

    // The detail pane's own title border also carries the bare name
    // (`section_box_verbatim`), so require the marker alongside it to land on
    // the selector's list row specifically.
    let row_idx = rows
        .iter()
        .position(|r| r.contains("xqzacct") && r.contains('⊖'))
        .unwrap_or_else(|| {
            panic!(
                "no row carries both the member name and the ⊖ marker:\n{}",
                rows.join("\n")
            )
        });
    let row = &rows[row_idx];
    // Buffer COLUMN, not `str::find`'s byte offset — the pane border and the
    // marker are multi-byte, so the two diverge on exactly this row.
    let col_of = |needle: &str| -> usize {
        let byte = row
            .find(needle)
            .unwrap_or_else(|| panic!("{needle} renders"));
        row[..byte].chars().count()
    };
    let cell = &buf.content[row_idx * w as usize + col_of("xqzacct")];
    assert_eq!(
        Some(cell.fg),
        theme::dim().fg,
        "the disabled member's name cell renders dim, not name_color's active/inactive branch"
    );

    let marker_cell = &buf.content[row_idx * w as usize + col_of("⊖")];
    assert_eq!(
        Some(marker_cell.fg),
        theme::faint().fg,
        "the ⊖ marker is uncharged, matching ⋯ stale"
    );
    // ⊖ is shared with `Canceled` (see `reason_marker`), so the hue is the only
    // thing telling the two apart on this row.
    assert_ne!(
        Some(marker_cell.fg),
        theme::danger().fg,
        "a disabled member must not wear the canceled arm's danger hue"
    );

    // Both panes share this physical row, so split at the seam: the selector
    // half carries the marker alone, the label lives on the detail card's pill.
    let seam = row.find("││").expect("the two panes meet on this row");
    let (selector, detail) = row.split_at(seam);
    assert!(
        !selector.contains("disabled"),
        "the selector row carries the marker only, no inline chip: {selector}"
    );
    assert!(
        detail.contains("[ disabled ]"),
        "the detail card shows the `[ disabled ]` pill: {detail}"
    );
}

/// `BlockedReason::Disabled` outranks every other reason: a disabled account is
/// skipped as a candidate regardless of what its usage or credentials say, so
/// naming a quota/liveness block instead would describe a member nothing picks.
#[test]
fn blocked_reason_ranks_disabled_above_canceled_and_auth_broken() {
    use crate::fallback::{BlockedReason, blocked_reason};
    use crate::usage::PlanInfo;

    // Canceled subscription AND a broken login AND a maxed 5h window at once.
    let mut a = profile("acct", 50.0, 100.0, 3600);
    a.disabled = true;
    a.usage.as_mut().unwrap().plan = Some(PlanInfo {
        subscription_status: Some("canceled".to_string()),
        ..PlanInfo::default()
    });
    let mut cfg = config_with(vec![a], Some("other"), vec!["acct"]);
    cfg.state.auth_broken.push("acct".into());

    let p = cfg
        .find(&crate::profile::ProfileName::from("acct"))
        .unwrap();
    assert_eq!(
        blocked_reason(&cfg, p, None),
        Some(BlockedReason::Disabled),
        "disabled ranks first, above canceled and auth broken"
    );

    // Flipping only the disabled bit hands the row back to the next rung.
    let mut enabled = cfg.clone();
    enabled.profiles[0].disabled = false;
    assert_eq!(
        blocked_reason(
            &enabled,
            enabled
                .find(&crate::profile::ProfileName::from("acct"))
                .unwrap(),
            None
        ),
        Some(BlockedReason::Canceled),
        "without the disabled bit the canceled rung wins"
    );
}

/// The non-active guard: `snapshot_chain` / `next_target` deliberately never
/// drop a disabled ACTIVE member from the walk (the bit is candidate-only), so
/// claiming `Disabled` there would be the second opinion `blocked_reason` must
/// never be. Unreachable through `actions::disable_profile` today — which
/// refuses an active target — but the walk already guards it, so this does too.
#[test]
fn blocked_reason_never_reports_disabled_for_the_active_profile() {
    use crate::fallback::{BlockedReason, blocked_reason};

    let mut a = profile("acct", 95.0, 10.0, 3600);
    a.disabled = true;
    let cfg = config_with(vec![a], Some("acct"), vec!["acct"]);
    assert_eq!(
        blocked_reason(
            &cfg,
            cfg.find(&crate::profile::ProfileName::from("acct"))
                .unwrap(),
            None
        ),
        None,
        "a disabled ACTIVE member has headroom and reports no block"
    );

    // The same profile, no longer active, does report it.
    let mut inactive = cfg.clone();
    inactive.state.active_profile = Some("other".into());
    assert_eq!(
        blocked_reason(
            &inactive,
            inactive
                .find(&crate::profile::ProfileName::from("acct"))
                .unwrap(),
            None
        ),
        Some(BlockedReason::Disabled),
        "a disabled NON-active member reports the block"
    );
}

// ── blocked-reason pill (detail card, weekly-fallback §4) ────────────────────

// A member over its 5h threshold shows the worst-reason pill at the very top of
// the card, naming the block with its utilization % and reset countdown.
#[test]
fn blocked_member_shows_the_worst_reason_pill() {
    let cfg = config_with(vec![profile("a", 95.0, 97.0, 7200)], Some("a"), vec!["a"]);
    let lines = member_detail(
        &cfg,
        &crate::profile::ProfileName::from("a"),
        MemberCard {
            width: 60,
            ..Default::default()
        },
    )
    .0;
    let pill = line_text(&lines[0]);
    assert!(pill.contains('['), "renders as a status pill: {pill:?}");
    assert!(
        pill.contains("5h 97%"),
        "names the 5h block with %: {pill:?}"
    );
    // Tolerant on the exact bucket: the fixture's `now` and `blocked_reason`'s
    // `now` can straddle a whole second (7200 → "1h 59m"), so assert only that a
    // countdown suffix trails the pill, not its value.
    assert!(
        pill.contains("[ 5h 97% ]  "),
        "carries the reset countdown as a trailing suffix: {pill:?}"
    );
    assert!(!pill.contains('·'), "no middle-dot separator: {pill:?}");
}

// A switch-grade kick-rejected member — headroom, but the messages limiter won't
// let clauth start it — shows the `claude code blocked` pill driven by `kick_lift`.
#[test]
fn kick_rejected_member_shows_the_claude_code_blocked_pill() {
    let cfg = config_with(vec![profile("a", 95.0, 40.0, 7200)], Some("a"), vec!["a"]);
    let until = now_epoch_secs() + 7200;
    let lines = member_detail(
        &cfg,
        &crate::profile::ProfileName::from("a"),
        MemberCard {
            kick_lift: Some(until),
            width: 60,
            ..Default::default()
        },
    )
    .0;
    let pill = line_text(&lines[0]);
    // Bare pill + a faint countdown suffix OUTSIDE the brackets (no `·`), the
    // same shape the Usage-tab kick pill renders. The exact bucket stays tolerant
    // since the two `now` reads (fixture vs `blocked_reason`) can straddle a whole
    // second. The exact `lifts_in` value is range-checked in the
    // `blocked_reason_kick_*` unit test instead.
    assert!(
        pill.contains("[ claude code blocked ]  "),
        "renders the kick pill with a trailing lift countdown: {pill:?}"
    );
    assert!(!pill.contains('·'), "no middle-dot separator: {pill:?}");
}

// The canceled pill reads the short shared label, not the old verbose
// `subscription canceled`: the `└` hint carries the explanation, and the label
// comes from the one source both this card and the Usage status block read, so
// the two tabs can't drift apart again.
#[test]
fn canceled_member_shows_the_short_shared_label() {
    let rendered: String = reason_pill_spans(&BlockedReason::Canceled, ResetFmt::default())
        .iter()
        .map(|s| s.content.as_ref())
        .collect();
    assert_eq!(rendered, "[ canceled ]", "got {rendered:?}");
    assert!(
        !rendered.contains("subscription"),
        "the verbose wording moved to the hint line: {rendered:?}"
    );
}

// A member with headroom shows no pill — the card opens straight on `5h usage`
// (chain position moved to the selector's `#n` rail, so the card no longer
// restates it).
#[test]
fn headroom_member_shows_no_reason_pill() {
    let cfg = config_with(vec![profile("a", 95.0, 40.0, 7200)], Some("a"), vec!["a"]);
    let lines = member_detail(
        &cfg,
        &crate::profile::ProfileName::from("a"),
        MemberCard {
            width: 60,
            ..Default::default()
        },
    )
    .0;
    let first = line_text(&lines[0]);
    assert!(
        !first.contains('['),
        "no pill for a member with headroom: {first:?}"
    );
    assert!(
        first.contains("5h usage"),
        "card opens on the 5h gauge: {first:?}"
    );
}

/// The `rows_start` `member_detail` RETURNS must be the index of the FIRST
/// `FALLBACK_ROWS` row it actually pushed, at every header height — 0 pills,
/// 1 pill + its fix line, and the stacked 2. That figure is what
/// `draw_chain_detail` adds to the native-cursor row math, so a drift puts a
/// typed field's caret on the wrong row, which no text-only assertion catches.
/// `rotate at` is the first `FALLBACK_ROWS` entry, so pinning
/// `rows_start == position_of("rotate at")` is the whole contract in one
/// equality — and it stays honest through a header-block change, unlike the
/// hand-maintained `ROWS_BEFORE` it replaced.
///
/// PILL heights only: this sweep hands `member_detail` a default
/// `MemberSessions`, so `live_session_lines` early-returns and the session
/// block never enters it. That axis is
/// `member_detail_rows_start_clears_the_session_block_at_every_height`.
#[test]
fn member_detail_rows_start_indexes_the_first_fallback_row_at_every_header_height() {
    let at_width = |cfg: &AppConfig, width: usize| -> (usize, usize) {
        let (lines, rows_start) = member_detail(
            cfg,
            &crate::profile::ProfileName::from("a"),
            MemberCard {
                width,
                ..Default::default()
            },
        );
        let first_row_at = lines
            .iter()
            .position(|l| line_text(l).contains("rotate at"))
            .expect("the first FALLBACK_ROWS row renders");
        (rows_start, first_row_at)
    };
    let start_and_first_row = |cfg: &AppConfig| at_width(cfg, 60);

    // 0 pills: gauge + headroom + blank only.
    let healthy = config_with(vec![profile("a", 95.0, 10.0, 7200)], Some("a"), vec!["a"]);
    let (start, row) = start_and_first_row(&healthy);
    assert_eq!(start, row, "no pill: rows_start indexes the first row");
    assert_eq!(start, 3, "gauge + headroom + blank");

    // 1 pill: adds the pill row, its `└` fix line, and the separating blank.
    let one = config_with(vec![profile("a", 95.0, 100.0, 7200)], Some("a"), vec!["a"]);
    let (start, row) = start_and_first_row(&one);
    assert_eq!(start, row, "1 pill: rows_start indexes the first row");
    assert_eq!(start, 6, "pill + hint + blank on top of the 3 base rows");

    // 2 pills: disabled AND auth-broken stack, each with its own fix line.
    let mut d = profile("a", 95.0, 10.0, 7200);
    d.disabled = true;
    let mut two = config_with(vec![d], Some("other"), vec!["a"]);
    two.state.auth_broken.push("a".into());
    let (start, row) = start_and_first_row(&two);
    assert_eq!(start, row, "2 pills: rows_start indexes the first row");
    assert_eq!(
        start, 8,
        "two pill+hint pairs + blank on top of the 3 base rows"
    );

    // Narrow: the SAME config must produce a taller header, because each fix
    // line now wraps. This is what makes `rows_start` load-bearing rather than a
    // function of the pill count — and it is what stops the caret test's
    // "wrapped" cases from being duplicates of its wide ones.
    let (narrow_start, narrow_row) = at_width(&two, 30);
    assert_eq!(
        narrow_start, narrow_row,
        "wrapped: rows_start still indexes the first row"
    );
    assert!(
        narrow_start > start,
        "a 30-col pane must wrap the fix lines and push the rows down \
         (wide={start}, narrow={narrow_start}) — otherwise nothing here tests wrapping"
    );
}

/// The caret math end-to-end, through the real `draw` path and the real
/// `frame.set_cursor_position` — the caret must land on the buffer row that
/// actually carries the `rotate at` field it is editing. Asserting against the
/// RENDERED row rather than an arithmetic delta is what makes this immune to
/// the header block changing height: it restates the user-visible contract
/// ("the caret is in the field") instead of re-deriving the implementation's
/// own sum. Driven at 0, 1 and 2 pills, and with the `priority` row gone.
///
/// The narrow cases are the whole reason `rows_start` exists: below ~40 columns
/// each fix line WRAPS, so the header block's height stops being a function of
/// the pill count alone. Kept tall (40 rows) so the row is always on-pane —
/// clipping is a separate contract, pinned by
/// `typed_threshold_caret_is_not_set_when_the_row_is_clipped_off_the_pane`.
#[test]
fn typed_threshold_caret_lands_on_the_rotate_at_row_at_every_header_height() {
    let _home = crate::testutil::HomeSandbox::new();
    let check = |cfg: AppConfig, label: &str, w: u16, h: u16| {
        let mut app = App::new(cfg);
        app.fallback_focus = FallbackFocus::Detail;
        app.fallback_detail_cursor = FALLBACK_ROWS
            .iter()
            .position(|r| *r == FallbackRow::Threshold)
            .unwrap();
        app.fallback_threshold_draft = Some(InputState::new("80"));
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| super::draw(f, f.area(), &app)).unwrap();
        let caret = term
            .get_cursor_position()
            .expect("a typed field places the native caret");
        let rows = crate::testutil::buffer_rows(term.backend().buffer());
        let rendered_at = rows
            .iter()
            .position(|r| r.contains("rotate at"))
            .unwrap_or_else(|| panic!("[{label}] rotate at renders:\n{}", rows.join("\n")));
        assert_eq!(
            caret.y as usize,
            rendered_at,
            "[{label}] caret must sit on the rotate-at row, not {} rows off",
            (caret.y as i64) - (rendered_at as i64)
        );
    };

    let healthy = || config_with(vec![profile("a", 95.0, 10.0, 7200)], Some("a"), vec!["a"]);
    let one_pill = || config_with(vec![profile("a", 95.0, 100.0, 7200)], Some("a"), vec!["a"]);
    // 2 pills: disabled AND auth-broken, each with its own fix line.
    let two_pills = || {
        let mut d = profile("a", 95.0, 10.0, 7200);
        d.disabled = true;
        let mut two = config_with(vec![d], Some("other"), vec!["a"]);
        two.state.auth_broken.push("a".into());
        two
    };

    check(healthy(), "0 pills", 120, 30);
    check(one_pill(), "1 pill", 120, 30);
    check(two_pills(), "2 pills", 120, 30);

    // Wrapped hints: one fix line becomes 2-3 rows, so the header height is no
    // longer derivable from the pill count.
    check(one_pill(), "1 pill, wrapped", 34, 40);
    check(two_pills(), "2 pills, wrapped", 34, 40);
    check(two_pills(), "2 pills, hard-wrapped", 26, 40);
}

/// A short pane can no longer strand a typed row: the focused card SCROLLS the
/// cursored row into view (the header block alone can outgrow the pane — two
/// pills, each dragging a fix line that wraps), and the caret math subtracts
/// the same scroll, so the caret lands on the row's RENDERED position rather
/// than its absolute index. `set_cursor_position` takes an absolute row and a
/// real terminal clamps an out-of-range one onto the last line, so the caret
/// must also never be set past the pane.
#[test]
fn typed_threshold_row_scrolls_into_view_and_carries_the_caret() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut d = profile("a", 95.0, 10.0, 7200);
    d.disabled = true;
    let mut cfg = config_with(vec![d], Some("other"), vec!["a"]);
    cfg.state.auth_broken.push("a".into());

    let mut app = App::new(cfg);
    app.fallback_focus = FallbackFocus::Detail;
    app.fallback_detail_cursor = FALLBACK_ROWS
        .iter()
        .position(|r| *r == FallbackRow::Threshold)
        .unwrap();
    app.fallback_threshold_draft = Some(InputState::new("80"));

    // 26x17: without the scroll, the two wrapped fix lines pushed the rows
    // past the pane's last line (the pre-scroll revision of this test pinned
    // exactly that clip).
    let (w, h) = (26u16, 17u16);
    let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
    term.draw(|f| super::draw(f, f.area(), &app)).unwrap();

    let rows = crate::testutil::buffer_rows(term.backend().buffer());
    let rendered_at = rows
        .iter()
        .position(|r| r.contains("rotate at"))
        .unwrap_or_else(|| {
            panic!(
                "the typed row must scroll into view on a short pane:\n{}",
                rows.join("\n")
            )
        });
    let caret = term.get_cursor_position().unwrap();
    assert_eq!(
        caret.y as usize, rendered_at,
        "caret must sit on the row's rendered (scrolled) position",
    );
    assert!(
        (caret.y as usize) < h as usize,
        "caret must never be set past the terminal's last row (y={}, h={h})",
        caret.y
    );
}

/// Finding shape from review round 2: a 40x24 terminal (~14 inner rows after
/// borders and the left chain) with a blocked member fills the card past the
/// pane, and the card has no scrollbar — the LAST rows must stay reachable by
/// walking the cursor down, not fall off the bottom.
#[test]
fn remove_row_stays_reachable_on_a_40x24_pane() {
    let _home = crate::testutil::HomeSandbox::new();
    // One blocked member: the pill block + fix line eat header rows.
    let cfg = config_with(vec![profile("a", 95.0, 100.0, 7200)], Some("a"), vec!["a"]);
    let mut app = App::new(cfg);
    app.fallback_focus = FallbackFocus::Detail;
    app.fallback_detail_cursor = FALLBACK_ROWS
        .iter()
        .position(|r| *r == FallbackRow::Remove)
        .unwrap();

    let mut term = Terminal::new(TestBackend::new(40, 24)).unwrap();
    term.draw(|f| super::draw(f, f.area(), &app)).unwrap();
    let rows = crate::testutil::buffer_rows(term.backend().buffer());
    assert!(
        rows.iter().any(|r| r.contains("remove")),
        "the cursored last row must scroll into view at 40x24:\n{}",
        rows.join("\n")
    );
}

/// The Fallback detail card keeps BOTH facts for a disabled member: the
/// `[ disabled ]` pill says the operator excluded it, the health pill beneath
/// says it is also broken. Before this, `Disabled` ranking first meant the card
/// showed only the exclusion and the dead login was invisible tab-wide.
#[test]
fn member_detail_stacks_the_health_pill_under_disabled() {
    let mut d = profile("a", 95.0, 10.0, 7200);
    d.disabled = true;
    let mut cfg = config_with(vec![d], Some("other"), vec!["a"]);
    cfg.state.auth_broken.push("a".into());

    // Both pills on one `├│└` rail, each with its own fix line. The first row
    // carries the `status` key so the rail has a column to anchor against; the
    // second bridges with `│` at col 0 while the rail is still open.
    let (lines, _) = member_detail(
        &cfg,
        &crate::profile::ProfileName::from("a"),
        MemberCard {
            width: 60,
            ..Default::default()
        },
    );
    let block: Vec<String> = lines.iter().take(4).map(line_text).collect();
    assert_eq!(
        block,
        vec![
            "status       [ disabled ]".to_string(),
            "├ excluded from the walk, enable it on the setup tab".to_string(),
            "│            [ auth broken ]".to_string(),
            "└ re-login with clauth login a".to_string(),
        ],
        "both facts stack on one rail, each naming its own fix"
    );

    // An ENABLED but auth-broken member is unchanged: one pill, lone `└`.
    let mut e = profile("a", 95.0, 10.0, 7200);
    e.disabled = false;
    let mut enabled = config_with(vec![e], Some("other"), vec!["a"]);
    enabled.state.auth_broken.push("a".into());
    let (lines, _) = member_detail(
        &enabled,
        &crate::profile::ProfileName::from("a"),
        MemberCard {
            width: 60,
            ..Default::default()
        },
    );
    assert_eq!(
        lines.iter().take(2).map(line_text).collect::<Vec<_>>(),
        vec![
            "status       [ auth broken ]".to_string(),
            "└ re-login with clauth login a".to_string(),
        ],
        "a single pill stays a lone `└` — nothing to connect"
    );
}

/// The selector rail shows `#n` and keeps a CONSTANT width across one- and
/// two-digit positions, so every name in the list starts on the same column.
/// A ragged rail is what a bare `{}` would give, and it is invisible in any
/// test that only renders a short chain.
#[test]
fn selector_rail_shows_hash_n_at_constant_width() {
    let _home = crate::testutil::HomeSandbox::new();
    // 10 members so the list spans `#1` through `#10`. Names are deliberately
    // non-prefixing (`ma`..`mj`, never `m1`..`m10`): `m1` is a substring of `m10`
    // and `#1` of `#10`, so a naive `contains` lookup would match the wrong row
    // and then measure a column that happens to agree anyway.
    let names: Vec<String> = (b'a'..=b'j').map(|c| format!("m{}", c as char)).collect();
    let profiles: Vec<_> = names.iter().map(|n| profile(n, 95.0, 10.0, 7200)).collect();
    let chain: Vec<&str> = names.iter().map(|s| s.as_str()).collect();
    let cfg = config_with(profiles, None, chain);
    let app = App::new(cfg);

    let (w, h) = (100u16, 20u16);
    let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
    term.draw(|f| super::draw(f, f.area(), &app)).unwrap();
    // Both panes share every physical row and the detail card repeats the
    // selected member's name in its TITLE (on a border row, which carries no
    // `││` seam to split on) — so identify selector rows by the rail's own `#`
    // rather than by name, and measure the name column within those.
    // Walk forward from the rail's own `#` (past its digits and the gap) to find
    // where the name starts. Searching the row for the name STRING instead would
    // collide with the detail pane sharing the same physical row — `max spend`
    // contains `ma`, which silently reported a name column of 34.
    let rows = crate::testutil::buffer_rows(term.backend().buffer());
    let mut name_cols = Vec::new();
    for row in &rows {
        let chars: Vec<char> = row.chars().collect();
        let Some(hash) = chars.iter().position(|c| *c == '#') else {
            continue;
        };
        let mut i = hash + 1;
        while i < chars.len() && chars[i].is_ascii_digit() {
            i += 1;
        }
        while i < chars.len() && chars[i] == ' ' {
            i += 1;
        }
        // Our members are the only `m?` tokens that can follow a rail ordinal.
        if chars.get(i) == Some(&'m') {
            name_cols.push(i);
        }
    }
    assert_eq!(
        name_cols.len(),
        names.len(),
        "every member must render a `#n` selector row; got {name_cols:?}"
    );
    assert_eq!(
        name_cols
            .iter()
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        1,
        "every name must start on the same column; got {name_cols:?}"
    );

    // …and the ordinals really are `#n`, spanning both digit widths — otherwise
    // a rail that dropped the `#` entirely would still pass the column check.
    let joined = rows.join("\n");
    assert!(joined.contains("#1"), "first position renders as #1");
    assert!(joined.contains("#10"), "tenth position renders as #10");
}

/// Every `reason_fix` arm must be reachable and non-empty: a match arm that can
/// never fire is dead copy, and an empty hint renders a bare `└` with nothing
/// after it. Enumerates one representative of each variant.
#[test]
fn every_reason_fix_variant_is_reachable_and_non_empty() {
    let all = [
        BlockedReason::Disabled,
        BlockedReason::Canceled,
        BlockedReason::AuthBroken,
        BlockedReason::WeeklySpent { resets_in: None },
        BlockedReason::KickRejected { lifts_in: 60 },
        BlockedReason::BudgetSpent,
        BlockedReason::FiveHour {
            pct: 99.0,
            resets_in: None,
        },
        BlockedReason::WeeklySoft { pct: 85.0 },
        BlockedReason::Stale,
    ];
    let mut seen: Vec<String> = Vec::new();
    for reason in all {
        let fix = reason_fix(&reason, &crate::profile::ProfileName::from("acct"));
        assert!(!fix.trim().is_empty(), "{reason:?} has no fix copy");
        assert_eq!(
            fix,
            fix.to_lowercase(),
            "{reason:?} fix must stay lowercase like every other hint: {fix}"
        );
        assert!(
            !seen.contains(&fix),
            "{reason:?} reuses another variant's copy ({fix}) — the arm is indistinguishable"
        );
        seen.push(fix);
    }
    assert_eq!(seen.len(), 9, "every variant contributed a distinct fix");
}

// ── `max spend` dims while inert (spend budget off) ──────────────────────────

fn span_style(line: &Line<'static>, needle: &str) -> Option<ratatui::style::Style> {
    line.spans
        .iter()
        .find(|s| s.content.contains(needle))
        .map(|s| s.style)
}

// A set ceiling with the chain-wide `spend budget` OFF spends nothing, so it must
// not read as armed: render the value faint. Flip spend budget on and the same
// ceiling renders in ACCENT.
#[test]
fn max_spend_dims_when_spend_budget_is_off() {
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let mut cfg = config_with(vec![profile("a", 95.0, 40.0, 3600)], Some("a"), vec!["a"]);
    cfg.profiles[0].max_auto_spend = Some(25.0);

    let off = member_detail(
        &cfg,
        &crate::profile::ProfileName::from("a"),
        MemberCard {
            focused: true,
            row_cursor: 2,
            width: 60,
            ..Default::default()
        },
    )
    .0;
    let off_val = off
        .iter()
        .find_map(|l| span_style(l, "$25.00"))
        .expect("max spend ceiling renders");
    assert_eq!(
        off_val.fg,
        crate::tui::theme::faint().fg,
        "an inert ceiling (spend budget off) renders faint"
    );

    cfg.state.spend_budget_switching = true;
    let on = member_detail(
        &cfg,
        &crate::profile::ProfileName::from("a"),
        MemberCard {
            focused: true,
            row_cursor: 2,
            width: 60,
            ..Default::default()
        },
    )
    .0;
    let on_val = on
        .iter()
        .find_map(|l| span_style(l, "$25.00"))
        .expect("max spend ceiling renders");
    assert_eq!(
        on_val.fg,
        crate::tui::theme::accent().fg,
        "an armed ceiling (spend budget on) renders in accent"
    );
}

// The `weekly at` row reads as a state: the chain default faint when unset, a
// member-set figure in accent with the default alongside, and the whole row
// dimmed (inert) while the member's weekly gate is off.
#[test]
fn weekly_at_row_distinguishes_default_override_and_gated_off() {
    let row_texts = |p: Profile| -> Vec<String> {
        let cfg = config_with(vec![p], Some("a"), vec!["a"]);
        member_detail(
            &cfg,
            &crate::profile::ProfileName::from("a"),
            MemberCard {
                focused: true,
                row_cursor: 1,
                width: 80,
                ..Default::default()
            },
        )
        .0
        .iter()
        .map(line_text)
        .collect()
    };

    let unset = row_texts(profile("a", 95.0, 20.0, 3600));
    let row = unset
        .iter()
        .find(|t| t.contains("weekly at"))
        .expect("weekly at row renders");
    assert!(row.contains("98%"), "{row}");
    assert!(
        !row.contains("chain default"),
        "unset row must not restate the deleted chain-default label: {row}"
    );
    assert!(
        unset
            .iter()
            .any(|t| t
                .contains("switches away from this account at the chain's shared weekly level")),
        "{unset:?}"
    );

    let mut p = profile("a", 95.0, 20.0, 3600);
    p.weekly_threshold = Some(90.0);
    let set = row_texts(p);
    let row = set
        .iter()
        .find(|t| t.contains("weekly at"))
        .expect("weekly at row renders");
    assert!(row.contains("90%"), "{row}");
    assert!(row.contains("default:"), "{row}");
    assert!(
        set.iter()
            .any(|t| t
                .contains("switches away from this account once weekly usage passes this level")),
        "{set:?}"
    );

    let mut p = profile("a", 95.0, 20.0, 3600);
    p.check_weekly = false;
    let gated = row_texts(p);
    assert!(
        gated
            .iter()
            .any(|t| t.contains("weekly gate is off, this line isn't checked")),
        "{gated:?}"
    );
}

// `weekly at`'s "default: N%" reminder must mirror `rotate at`: render only
// when the override actually differs from the chain default, not
// unconditionally (it used to always render once a value was set).
#[test]
fn weekly_at_default_reminder_only_shows_when_value_differs_from_default() {
    let row_texts = |p: Profile| -> Vec<String> {
        let cfg = config_with(vec![p], Some("a"), vec!["a"]);
        member_detail(
            &cfg,
            &crate::profile::ProfileName::from("a"),
            MemberCard {
                focused: true,
                row_cursor: 1,
                width: 80,
                ..Default::default()
            },
        )
        .0
        .iter()
        .map(line_text)
        .collect()
    };

    // Chain-wide default is 98% (`DEFAULT_WEEKLY_SWITCH_PCT`); an override set
    // to exactly that value must not carry the reminder.
    let mut at_default = profile("a", 95.0, 20.0, 3600);
    at_default.weekly_threshold = Some(98.0);
    let lines = row_texts(at_default);
    let row = lines
        .iter()
        .find(|t| t.contains("weekly at"))
        .expect("weekly at row renders");
    assert!(row.contains("98%"), "{row}");
    assert!(
        !row.contains("default:"),
        "no reminder when the override equals the chain default: {row}"
    );

    let mut off_default = profile("a", 95.0, 20.0, 3600);
    off_default.weekly_threshold = Some(90.0);
    let lines = row_texts(off_default);
    let row = lines
        .iter()
        .find(|t| t.contains("weekly at"))
        .expect("weekly at row renders");
    assert!(
        row.contains("default:"),
        "reminder renders when the override differs from the chain default: {row}"
    );
}

// Bug 6 class fix: the edit-mode glyph pairs ACCENT + bold, matching the
// selection caret `❯` — this card rendered it accent-only.
#[test]
fn edit_glyph_is_bold_like_the_selection_caret() {
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let input = InputState::new("80");
    let line = detail_row(
        FallbackRow::Threshold,
        true,
        MemberRow {
            threshold: 80.0,
            weekly_override: None,
            weekly_default: 98.0,
            check_weekly: true,
            check_scoped: true,
            last_resort: false,
            preferred: false,
            max_spend: 0.0,
            spend_budget: false,
            armed_remove: false,
        },
        Some(&input),
    );
    let glyph = &line.spans[0];
    assert!(
        glyph
            .style
            .add_modifier
            .contains(ratatui::style::Modifier::BOLD),
        "edit glyph must be bold: {glyph:?}"
    );
    assert_eq!(
        glyph.style.fg,
        theme::accent().fg,
        "edit glyph stays accent"
    );
}

// ── live sessions on the chain ───────────────────────────────────────────────

/// One live session's registry row, running as the account it launched on. The
/// swap attribution itself is pinned in `live_sessions.rs`'s own tests.
fn live_row(
    session_id: &str,
    member: &str,
    follows_chain: bool,
    last_swap_at: Option<u64>,
) -> crate::live_sessions::LiveSession {
    crate::live_sessions::LiveSession {
        follows_chain,
        last_swap_at,
        ..crate::testutil::live_row(session_id, member)
    }
}

/// The tally renders in the DETAIL PANE ONLY (owner call, 2026-07-26): it used
/// to ride the selector row too, so one number appeared twice on one screen and
/// the compact copy was the one that could not explain itself. The selector row
/// carries the member's name and its blocked-reason marker, nothing else.
///
/// This is a DELETION pin, so presence assertions cannot guard it: a retargeted
/// `.contains` stays green the moment the span comes back. The pane is asserted
/// whole, plus an explicit absence of the follower glyph.
#[test]
fn a_chain_row_carries_no_live_session_tally() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = App::new(config_with(
        vec![
            profile("busy", 95.0, 10.0, 3600),
            profile("idle", 95.0, 10.0, 3600),
        ],
        None,
        vec!["busy", "idle"],
    ));
    app.live_sessions = crate::live_sessions::LiveTally::of([
        live_row("4242-0", "busy", true, None),
        live_row("4242-1", "busy", false, None),
    ]);

    let pane = chain_selector_pane(&app, 100, 8);
    assert_eq!(
        pane,
        vec![
            "╭ CHAIN ─────────────────────╮".to_string(),
            "│ ❯  #1 busy                 │".to_string(),
            "│    #2 idle                 │".to_string(),
            "│                            │".to_string(),
            "│                            │".to_string(),
            "│                            │".to_string(),
            "│                            │".to_string(),
            "╰────────────────────────────╯".to_string(),
        ],
    );
    assert!(
        !pane.iter().any(|row| row.contains('⇄')),
        "the follower mark belongs to the detail pane alone:\n{}",
        pane.join("\n")
    );
}

/// Every row of the Fallback tab's chain selector pane, borders included, sliced
/// off the rendered frame at the pane's own width. Returned whole so a pin is an
/// exact vec rather than a substring probe — a `.contains` on one row cannot see
/// a glyph that got appended past the pane and clipped.
fn chain_selector_pane(app: &App, w: u16, h: u16) -> Vec<String> {
    let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
    term.draw(|f| super::draw(f, f.area(), app)).unwrap();
    let rows = crate::testutil::buffer_rows(term.backend().buffer());
    let pane_w = rows
        .iter()
        .find_map(|r| r.find('╮').map(|b| r[..b].chars().count() + 1))
        .expect("the chain pane's top-right corner");
    rows.iter()
        .map(|r| r.chars().take(pane_w).collect())
        .collect()
}

/// Text of every line in the member card, trimmed, for exact-vec assertions.
fn card_texts(lines: &[Line<'static>]) -> Vec<String> {
    lines
        .iter()
        .map(|l| {
            l.spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>()
                .trim_end()
                .to_string()
        })
        .collect()
}

/// The block, whole, for a member hosting sessions none of which has swapped:
/// the count under the app-wide `live` key with the follower qualifier (since
/// one of the two rides the chain), and NOTHING else. A session that never
/// swapped has no repointed credential link, so §12's pickup lag does not apply
/// to it and claiming it would over-warn in the other direction.
#[test]
fn the_session_block_emits_the_follower_qualifier_when_one_session_follows() {
    let sessions = crate::live_sessions::LiveTally::of([
        live_row("4242-0", "a", true, None),
        live_row("4242-1", "a", false, None),
    ])
    .member(&crate::profile::ProfileName::from("a"));

    assert_eq!(
        card_texts(&live_session_lines(sessions, 60)),
        vec!["live         2, 1 with fallback".to_string()],
    );
}

/// A pure-pinned count has no movable split to name, so the qualifier stays
/// off. `following > 0` is the gate, not `sessions > 1`. Pairs with the
/// mixed-fixture test above to pin both arms of the branch.
#[test]
fn the_session_block_omits_the_qualifier_when_no_session_follows() {
    let sessions = crate::live_sessions::LiveTally::of([
        live_row("4242-0", "a", false, None),
        live_row("4242-1", "a", false, None),
    ])
    .member(&crate::profile::ProfileName::from("a"));

    assert_eq!(
        card_texts(&live_session_lines(sessions, 60)),
        vec!["live         2".to_string()],
    );
}

/// Once a session HAS swapped onto this member, `current_member` stops being an
/// instantaneous fact: Claude Code re-reads its credentials on its next request
/// and nothing observes the pickup.
/// The card says so rather than inventing a "not yet picked up" state the
/// registry cannot see.
#[test]
fn the_session_block_dates_the_last_swap_and_says_when_it_is_picked_up() {
    // Mid-unit (3h30m), so the wall clock cannot walk the fixture across a
    // `relative_age` boundary between building it and rendering it.
    let swapped_at = crate::usage::now_ms().saturating_sub(3 * 3_600_000 + 1_800_000);
    let sessions =
        crate::live_sessions::LiveTally::of([live_row("4242-0", "a", true, Some(swapped_at))])
            .member(&crate::profile::ProfileName::from("a"));

    assert_eq!(
        card_texts(&live_session_lines(sessions, 60)),
        vec![
            "live         1, 1 with fallback".to_string(),
            "last swap    3h ago".to_string(),
            " └ picked up on the session's next request".to_string(),
        ],
    );
}

/// A swap that landed inside the second the card renders reads `just now`.
/// `relative_age` owns that arm, which is what retired the `max(1)` guard the
/// old two-unit formatter needed to keep from rendering the phrase `now ago`.
#[test]
fn a_swap_this_very_second_reads_as_just_now() {
    let sessions = crate::live_sessions::LiveTally::of([live_row(
        "4242-0",
        "a",
        false,
        Some(crate::usage::now_ms()),
    )])
    .member(&crate::profile::ProfileName::from("a"));

    assert_eq!(
        card_texts(&live_session_lines(sessions, 60)),
        vec![
            "live         1".to_string(),
            "last swap    just now".to_string(),
            " └ picked up on the session's next request".to_string(),
        ],
    );
}

/// The age line follows the cloudy-tui Time-formatting contract: ONE unit, the
/// largest that is at least 1, and an absolute ISO date at 30 days and beyond.
/// The two-unit `humanize_duration` the countdowns use would render `1d 4h ago`
/// here and never reach a date at all — it stays on the countdowns, where a
/// duration is what is being shown.
///
/// Every relative fixture sits MID-unit so the wall clock cannot walk it across
/// a boundary mid-test; the ISO case uses a fixed epoch, so its expectation is a
/// literal rather than a date recomputed from the code under test.
#[test]
fn the_last_swap_age_renders_one_unit_and_a_date_past_thirty_days() {
    let age_line = |at: u64| -> String {
        let sessions =
            crate::live_sessions::LiveTally::of([live_row("4242-0", "a", false, Some(at))])
                .member(&crate::profile::ProfileName::from("a"));
        card_texts(&live_session_lines(sessions, 60))
            .into_iter()
            .find(|t| t.starts_with("last swap"))
            .expect("the block dates a swap")
            .trim_start_matches("last swap")
            .trim()
            .to_string()
    };
    let ago = |ms: u64| crate::usage::now_ms().saturating_sub(ms);

    assert_eq!(age_line(ago(30_000)), "just now");
    assert_eq!(age_line(ago(5 * 60_000 + 30_000)), "5m ago");
    assert_eq!(age_line(ago(2 * 3_600_000 + 1_800_000)), "2h ago");
    assert_eq!(age_line(ago(3 * 86_400_000 + 43_200_000)), "3d ago");
    assert_eq!(age_line(ago(12 * 86_400_000)), "1w ago");
    // 2023-11-14T22:13:20Z — permanently past 30 days, so the arm is the date.
    assert_eq!(age_line(1_700_000_000_000), "2023-11-14");
}

/// An account hosting nothing says nothing — no row at all rather than
/// `sessions 0`, matching the Overview cell's blank and the selector's.
#[test]
fn the_session_block_is_absent_when_nothing_is_live() {
    assert_eq!(
        card_texts(&live_session_lines(
            crate::live_sessions::MemberSessions::default(),
            60
        )),
        Vec::<String>::new(),
    );
}

/// Position and rhythm: the block sits ABOVE the 5h gauge with its own
/// trailing blank separating the two. `rows_start` is read off the buffer, so
/// a block inserted anywhere else would move the native caret off every row
/// it points at.
#[test]
fn the_member_card_places_the_session_block_above_the_five_hour_gauge() {
    let cfg = config_with(vec![profile("a", 95.0, 10.0, 3600)], None, vec!["a"]);
    let sessions = crate::live_sessions::LiveTally::of([live_row("4242-0", "a", true, None)])
        .member(&crate::profile::ProfileName::from("a"));

    let (lines, rows_start) = member_detail(
        &cfg,
        &crate::profile::ProfileName::from("a"),
        MemberCard {
            sessions,
            width: 60,
            ..Default::default()
        },
    );
    let texts = card_texts(&lines);

    assert_eq!(
        &texts[..rows_start],
        [
            "live         1, 1 with fallback",
            "",
            "5h usage     ██░░░░░░░░░░░░░░░░░░░│  10% used",
            "             85% until rotate",
            "",
        ],
    );
}

/// A leg nothing calls is a leg that passes every unit test and ships nothing:
/// the member card has to read the app's own tally, not a default. The selector
/// pane is asserted alongside it so the tally's absence there is pinned on the
/// SAME fixture that proves the tally is non-empty.
#[test]
fn the_fallback_tab_reads_the_apps_live_session_tally() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = App::new(config_with(
        vec![profile("busy", 95.0, 10.0, 3600)],
        None,
        vec!["busy"],
    ));
    app.live_sessions =
        crate::live_sessions::LiveTally::of([live_row("4242-0", "busy", true, None)]);

    // The selector pane exactly, and the card's own row exactly. The card
    // reading `Default::default()` instead of the app's tally leaves every unit
    // test above it green.
    assert_eq!(
        chain_selector_pane(&app, 120, 6),
        vec![
            "╭ CHAIN ───────────────────────────╮".to_string(),
            "│ ❯  #1 busy                       │".to_string(),
            "│                                  │".to_string(),
            "│                                  │".to_string(),
            "│                                  │".to_string(),
            "╰──────────────────────────────────╯".to_string(),
        ],
    );

    let mut term = Terminal::new(TestBackend::new(120, 20)).unwrap();
    term.draw(|f| super::draw(f, f.area(), &app)).unwrap();
    let rows = crate::testutil::buffer_rows(term.backend().buffer());
    // The padded key cell, not the bare word: `live` is short enough that a
    // frame-wide search for it would match any prose on the screen.
    const KEY: &str = "live         ";
    let card = rows
        .iter()
        .find(|r| r.contains(KEY))
        .unwrap_or_else(|| panic!("the card carries no session row:\n{}", rows.join("\n")));
    assert_eq!(
        card.split('│')
            .find(|seg| seg.contains(KEY))
            .map(str::trim_end),
        Some(" live         1, 1 with fallback"),
    );
}

/// Three members, one healthy and active, one disabled, one out of 5h headroom
/// — the fixture both marker pins run on. `blockedname` is 11 chars so it
/// overruns the narrow pane's name budget while `hot` still fits it.
fn marker_app() -> App {
    let mut off = profile("blockedname", 95.0, 10.0, 7200);
    off.disabled = true;
    App::new(config_with(
        vec![
            profile("ok", 95.0, 10.0, 7200),
            off,
            profile("hot", 50.0, 99.0, 7200),
        ],
        Some("ok"),
        vec!["ok", "blockedname", "hot"],
    ))
}

/// The blocked-reason marker owns the row's LAST content column, and nothing
/// else pins that. Both surviving `chain_selector_pane` assertions run on
/// fixtures whose members are all healthy, so `if let Some(reason)` never
/// executes in either and the right-align arithmetic is free to drift; the pin
/// that covered it incidentally went out with its own subject in `94e0245`.
///
/// The pane is asserted WHOLE, because a `.contains` on a row cannot see a
/// marker that got appended past the pane and clipped. This is the wide case,
/// where every name fits and a drift reads as a column shift; the narrow case
/// is its own pin below.
#[test]
fn the_blocked_reason_marker_holds_the_rows_last_content_column() {
    let _home = crate::testutil::HomeSandbox::new();
    // 26 content cells: `⊖` and `◔` both sit one cell in from the pane's right
    // border, and the healthy active member carries no marker at all.
    assert_eq!(
        chain_selector_pane(&marker_app(), 100, 8),
        vec![
            "╭ CHAIN ─────────────────────╮".to_string(),
            "│ ❯  #1 ok                   │".to_string(),
            "│    #2 blockedname        ⊖ │".to_string(),
            "│    #3 hot                ◔ │".to_string(),
            "│                            │".to_string(),
            "│                            │".to_string(),
            "│                            │".to_string(),
            "╰────────────────────────────╯".to_string(),
        ],
    );
}

/// The narrow counterpart: a name too long for the row gives up the cells the
/// marker needs, rather than the marker being appended past the pane for
/// ratatui to discard — which rendered a BLOCKED account identically to a
/// healthy one. The clamp is charged ONLY to rows carrying a marker, so
/// `blockedname` truncates at 8 cells while unmarked names keep their full
/// width; that is why the name column's width tracks blocked state.
#[test]
fn a_narrow_pane_clamps_a_marked_members_name_rather_than_dropping_its_marker() {
    let _home = crate::testutil::HomeSandbox::new();
    // 16 content cells: the rail eats 6, the marker and its gap 2, leaving 8 for
    // the name — so `blockedname` truncates with `…` and keeps its `⊖`, while
    // `hot` fits with room to spare.
    assert_eq!(
        chain_selector_pane(&marker_app(), 60, 8),
        vec![
            "╭ CHAIN ───────────╮".to_string(),
            "│ ❯  #1 ok         │".to_string(),
            "│    #2 blocked… ⊖ │".to_string(),
            "│    #3 hot      ◔ │".to_string(),
            "│                  │".to_string(),
            "│                  │".to_string(),
            "│                  │".to_string(),
            "╰──────────────────╯".to_string(),
        ],
    );
}

/// `rows_start` against the SESSION BLOCK, which the header-height sweep above
/// cannot see: it passes a default `MemberSessions`, so `live_session_lines`
/// early-returns and contributes nothing. The block is pushed immediately
/// BEFORE `rows_start` is read, so its 2-to-5 lines move the anchor as surely as
/// a pill does — and a caret on the wrong row is invisible to every text
/// assertion, which is the whole reason `rows_start` is read off the buffer.
///
/// Three heights, one per thing the block can add: the count alone, the count
/// plus a dated swap and its always-on caveat, and that caveat WRAPPED on a
/// narrow pane. The swap stamp sits mid-unit so the wall clock cannot walk the
/// fixture across a `relative_age` boundary mid-test.
#[test]
fn member_detail_rows_start_clears_the_session_block_at_every_height() {
    let cfg = config_with(vec![profile("a", 95.0, 10.0, 7200)], None, vec!["a"]);
    let start_and_first_row =
        |sessions: crate::live_sessions::MemberSessions, width: usize| -> (usize, usize, usize) {
            let (lines, rows_start) = member_detail(
                &cfg,
                &crate::profile::ProfileName::from("a"),
                MemberCard {
                    sessions,
                    width,
                    ..Default::default()
                },
            );
            let first_row_at = lines
                .iter()
                .position(|l| line_text(l).contains("rotate at"))
                .expect("the first FALLBACK_ROWS row renders");
            (rows_start, first_row_at, lines.len())
        };

    // Baseline: no live session, so the block contributes nothing.
    let (bare, row, _) = start_and_first_row(Default::default(), 60);
    assert_eq!(bare, row, "no sessions: rows_start indexes the first row");
    assert_eq!(bare, 3, "gauge + headroom + blank");

    // Count only: the block's own leading blank plus the `live` row.
    let counted = crate::live_sessions::LiveTally::of([live_row("4242-0", "a", true, None)])
        .member(&crate::profile::ProfileName::from("a"));
    let (start, row, _) = start_and_first_row(counted, 60);
    assert_eq!(start, row, "count only: rows_start indexes the first row");
    assert_eq!(start, bare + 2, "blank + the `live` row");

    // Plus a dated swap, which always drags its caveat line.
    let swapped_at = crate::usage::now_ms().saturating_sub(3 * 3_600_000 + 1_800_000);
    let dated =
        crate::live_sessions::LiveTally::of([live_row("4242-0", "a", true, Some(swapped_at))])
            .member(&crate::profile::ProfileName::from("a"));
    let (start, row, _) = start_and_first_row(dated, 60);
    assert_eq!(start, row, "dated swap: rows_start indexes the first row");
    assert_eq!(start, bare + 4, "blank + `live` + `last swap` + the caveat");

    // The caveat wrapped: the SAME tally must push the rows further down on a
    // narrow pane, which is what makes `rows_start` load-bearing rather than a
    // function of the block's shape.
    let (narrow, narrow_row, _) = start_and_first_row(dated, 30);
    assert_eq!(
        narrow, narrow_row,
        "wrapped caveat: rows_start still indexes the first row"
    );
    assert!(
        narrow > start,
        "a 30-col pane must wrap the caveat and push the rows down \
         (wide={start}, narrow={narrow}) — otherwise nothing here tests wrapping"
    );
}
