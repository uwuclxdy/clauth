//! Interleaved auto-start queue: queue membership, gap arithmetic, the
//! history-series pair classifier, and the per-tick election.

use super::{
    Candidate, FIVE_HOUR_SECS, auto_start_queue_members, decayed, elect_queue_member, queue_due,
    queue_gap_secs, queue_slot, series_open,
};
use crate::profile::{AppConfig, AppState, ClaudeCredentials, OAuthToken, Profile};

const INTERVAL_MS: u64 = 90_000;

fn candidate<'a>(name: &'a str, lapsed: bool, failures: u32) -> Candidate<'a> {
    Candidate {
        name,
        lapsed,
        failures,
    }
}

/// An account that CAN open a window: opted into auto-start, enabled, holding
/// an OAuth pair. Every exclusion test starts from one of these and breaks
/// exactly one thing.
fn warming(name: &str) -> Profile {
    let mut p = Profile::new(name.to_string(), None, None);
    p.auto_start = true;
    p.credentials = Some(ClaudeCredentials {
        claude_ai_oauth: Some(OAuthToken {
            access_token: format!("{name}-access"),
            refresh_token: Some(format!("{name}-refresh")),
            expires_at: None,
            scopes: None,
            subscription_type: None,
        }),
    });
    p
}

fn config_of(profiles: Vec<Profile>, chain: Vec<&str>) -> AppConfig {
    AppConfig {
        state: AppState {
            fallback_chain: chain.into_iter().map(Into::into).collect(),
            // The feature is opt-in (default off); these fixtures opt in.
            auto_start_queue: true,
            ..AppState::default()
        },
        profiles,
    }
}

/// Queue ORDER: the fallback chain is the user's own stated preference order, so
/// it comes first; everything else follows in display order, which for equal
/// rank means by name. This is the order the whole feature's fairness rests on
/// — the first slot is the one that opens on a cold queue.
#[test]
fn auto_start_queue_members_orders_by_chain_position_then_display_order() {
    // Deliberately NOT in chain order on disk, so a pass-through would fail.
    let config = config_of(
        vec![
            warming("zulu"),
            warming("alpha"),
            warming("second"),
            warming("first"),
        ],
        vec!["first", "second"],
    );

    assert_eq!(
        auto_start_queue_members(&config, &[]),
        vec!["first", "second", "alpha", "zulu"],
        "chain members lead in chain order, the rest follow alphabetically"
    );
}

/// Queue MEMBERSHIP: everything that cannot open a window is excluded, because a
/// member that can never kick would still hold a slot and thin every live
/// member's share of the 5h span. One rule, read by the scheduler's election,
/// the `status.json` feed, and the TUI's queue chips.
#[test]
fn auto_start_queue_members_excludes_everything_that_cannot_open_a_window() {
    let mut opted_out = warming("opted-out");
    opted_out.auto_start = false;
    let mut disabled = warming("disabled");
    disabled.disabled = true;
    let mut logged_out = warming("logged-out");
    logged_out.credentials = None;

    let mut config = config_of(
        vec![
            warming("live"),
            opted_out,
            disabled,
            logged_out,
            warming("quarantined"),
            warming("kick-blocked"),
        ],
        vec![],
    );
    config.state.auth_broken = vec!["quarantined".into()];

    assert_eq!(
        auto_start_queue_members(&config, &["kick-blocked".into()]),
        vec!["live"],
        "no auto-start, disabled, no OAuth pair, auth-broken, and a switch-grade \
         kick block each cost the slot"
    );
}

/// A hybrid — an OAuth pair stored behind a `base_url` — IS a queue member. The
/// kick spends the stored access token and never routes through `base_url`, so
/// where the account's `/v1/messages` go has no bearing on whether it can open
/// a window. The two surfaces disagreed about exactly this account before the
/// rule was shared: the scheduler kicked it, `status.json` published it as
/// holding no slot.
#[test]
fn auto_start_queue_members_keeps_a_hybrid_that_stores_an_oauth_pair() {
    let mut hybrid = warming("hybrid");
    hybrid.base_url = Some("https://api.example.com".to_string());
    let config = config_of(vec![hybrid], vec![]);

    assert_eq!(auto_start_queue_members(&config, &[]), vec!["hybrid"]);
}

/// The toggle is a real off switch on every surface at once: no queue, so
/// nothing to elect, publish, or render.
#[test]
fn auto_start_queue_members_is_empty_with_the_queue_toggle_off() {
    let mut config = config_of(vec![warming("a"), warming("b")], vec![]);
    config.state.auto_start_queue = false;

    assert!(auto_start_queue_members(&config, &[]).is_empty());
}

/// `queue_slot` is the render read: a 1-based position, the queue size the gap is
/// `5h / N` of, and the countdown to the queue's next opening. That countdown is
/// SHARED — the gate is global, so both members of a 2-queue report the same
/// figure, and it goes `None` once the gap has passed (the queue is due).
#[test]
fn auto_start_queue_slot_reports_the_position_and_the_queues_shared_countdown() {
    let now = 1_780_000_000i64;
    let members: Vec<crate::profile::ProfileName> = vec!["a".into(), "b".into()];
    let gap = queue_gap_secs(2, INTERVAL_MS);

    let a = queue_slot(&members, "a", Some(now - 600), INTERVAL_MS, now).unwrap();
    let b = queue_slot(&members, "b", Some(now - 600), INTERVAL_MS, now).unwrap();
    assert_eq!((a.position, a.total), (1, 2));
    assert_eq!((b.position, b.total), (2, 2));
    assert_eq!(a.next_in, Some(gap - 600));
    assert_eq!(
        a.next_in, b.next_in,
        "the queue gates globally, so every member counts down to the same open"
    );

    assert_eq!(
        queue_slot(&members, "a", Some(now - gap), INTERVAL_MS, now)
            .unwrap()
            .next_in,
        None,
        "once the gap has passed the queue is due, not counting down"
    );
    assert_eq!(
        queue_slot(&members, "a", None, INTERVAL_MS, now)
            .unwrap()
            .next_in,
        None,
        "a cold queue is due now, so there is nothing to count down to"
    );
    assert!(
        queue_slot(&members, "outsider", None, INTERVAL_MS, now).is_none(),
        "a non-member holds no slot"
    );
}

/// The gap is `5h / N` less a jitter tolerance, and the tolerance is CAPPED
/// rather than being one full refresh interval: `refresh_interval_ms` is user
/// settable to an hour, and spending it all on tolerance would quietly gut the
/// spacing (a 30-minute interval would cut the N=3 floor to 1h10m).
#[test]
fn auto_start_queue_gap_is_five_hours_over_n_less_a_capped_tolerance() {
    // N=2 → 2h30m, N=3 → 1h40m, each less 90s of tick tolerance.
    assert_eq!(queue_gap_secs(2, INTERVAL_MS), 2 * 3600 + 1800 - 90);
    assert_eq!(queue_gap_secs(3, INTERVAL_MS), 6000 - 90);

    // A one-hour interval must not eat an hour of the gap.
    let hour_interval = queue_gap_secs(3, 3_600_000);
    assert_eq!(
        hour_interval,
        6000 - 300,
        "tolerance is capped at 5 minutes, not one interval"
    );

    // Degenerate queue sizes stay sane rather than dividing by zero.
    assert_eq!(
        queue_gap_secs(0, INTERVAL_MS),
        queue_gap_secs(1, INTERVAL_MS)
    );
    assert!(queue_gap_secs(1, INTERVAL_MS) > 0);
}

/// The N=1 neutrality property: a single account's own window start IS the queue
/// anchor, so at lapse the gap is always already satisfied and nothing about a
/// one-account setup changes even with the toggle (default off) turned on.
#[test]
fn auto_start_queue_never_delays_a_single_account() {
    let now = 1_780_000_000i64;
    let gap = queue_gap_secs(1, INTERVAL_MS);
    // Opened 5h ago — i.e. it lapsed exactly now.
    let opened = now - FIVE_HOUR_SECS;
    assert!(
        queue_due(Some(opened), now, gap),
        "a lone account must kick the instant its window lapses"
    );
}

/// A lapsed window can never be the member that gates the queue: it was opened
/// at least 5h ago and the gap is at most 5h, so the walk cannot deadlock.
#[test]
fn auto_start_queue_is_never_gated_by_a_lapsed_window() {
    let now = 1_780_000_000i64;
    for n in 1..=8 {
        let gap = queue_gap_secs(n, INTERVAL_MS);
        assert!(gap <= FIVE_HOUR_SECS, "gap must never exceed the window");
        assert!(
            queue_due(Some(now - FIVE_HOUR_SECS), now, gap),
            "n={n}: a 5h-old open must always satisfy the gap"
        );
    }
}

/// The gate holds a member until the gap has passed, and a queue with no anchor
/// at all is due (otherwise the first ever auto-start could never fire).
#[test]
fn auto_start_queue_due_holds_until_the_gap_passes() {
    let now = 1_780_000_000i64;
    let gap = queue_gap_secs(3, INTERVAL_MS);

    assert!(queue_due(None, now, gap), "a cold queue is always due");
    assert!(
        !queue_due(Some(now - 60), now, gap),
        "a member that opened a minute ago holds the queue shut"
    );
    assert!(
        !queue_due(Some(now - gap + 1), now, gap),
        "one second short of the gap still holds"
    );
    assert!(
        queue_due(Some(now - gap), now, gap),
        "exactly the gap is due"
    );
}

/// The idle-window shape. `/usage` reports `resets_at` a full duration out
/// for a window that never opened — value-identical to a just-kicked window in
/// any SINGLE sample, but across two it SLIDES with the clock while a real
/// boundary holds still. A sliding tail yields no anchor at all, so the gate
/// falls through to "due" rather than jamming shut.
#[test]
fn auto_start_queue_series_open_rejects_the_sliding_idle_shape() {
    let t = 1_780_000_000i64;
    // Idle profile polled every 90s: resets_at tracks now + 5h exactly.
    let sliding = [
        (t, t + FIVE_HOUR_SECS),
        (t + 90, t + 90 + FIVE_HOUR_SECS),
        (t + 180, t + 180 + FIVE_HOUR_SECS),
    ];
    assert_eq!(series_open(&sliding), None);
    assert!(queue_due(
        series_open(&sliding),
        t + 180,
        queue_gap_secs(2, INTERVAL_MS)
    ));
    assert_eq!(series_open(&[]), None);
    assert_eq!(series_open(&[(t, t + 600)]), None); // one sample proves nothing

    // A 10s cadence is legal (`MIN_REFRESH_INTERVAL_MS`) and sits BELOW the
    // slide baseline, where the slide test cannot engage. Only the kick
    // signature's one-way band confirms there — an idle pair's `dr = +dt` is
    // outside it. A symmetric band would confirm this pair (`dr = 10 <= 61`)
    // and hand the idle shape back its anchor on every sub-baseline cadence.
    let fast_idle = [(t, t + FIVE_HOUR_SECS), (t + 10, t + 10 + FIVE_HOUR_SECS)];
    assert_eq!(series_open(&fast_idle), None);

    // The band's edges, as literals: `[-61, +1]` and not a step wider. `+1`
    // absorbs epoch truncation of two floored reads; `-61` is the one-way
    // minute floor plus that same truncation.
    let b = t + 600;
    let open = |r| Some(r - FIVE_HOUR_SECS);
    assert_eq!(series_open(&[(t, b), (t + 5, b + 1)]), open(b + 1));
    assert_eq!(series_open(&[(t, b), (t + 5, b + 2)]), None);
    assert_eq!(series_open(&[(t, b), (t + 5, b - 61)]), open(b));
    assert_eq!(series_open(&[(t, b), (t + 5, b - 62)]), None);

    // A confirmed open EARLIER in the series still anchors: the sliding tail
    // only says the profile is idle NOW, not that it never opened.
    let boundary = t + 600;
    let mixed = [
        (t - 7200, boundary),
        (t - 7110, boundary - 1), // sub-second recompute jitter, rounded
        (t, t + FIVE_HOUR_SECS),
        (t + 90, t + 90 + FIVE_HOUR_SECS),
    ];
    assert_eq!(series_open(&mixed), Some(boundary - FIVE_HOUR_SECS));
}

/// A landed kick writes the exact synthetic stamp, then the next fresh body
/// re-reports the same window minute-floored (same-instant duplicate lines
/// included — the merge path writes both). Each agreeing pair confirms the
/// boundary; within a mixed pair the larger value — the exact stamp — wins,
/// and the newest confirmed pair overall decides, so an all-API tail lands
/// within the minute floor. A NEW window seen exactly once at the tail is a
/// single unconfirmable reading and must not move the anchor.
#[test]
fn auto_start_queue_series_open_confirms_kicked_windows_and_distrusts_a_single_reading() {
    let t = 1_780_000_000i64;
    let kick = t + 14; // kick lands at :14 past the minute
    let synthetic = kick + FIVE_HOUR_SECS;
    let floored = synthetic - 14; // the API floors the boundary to the minute
    let series = [
        (t - 60, t - 30), // stale lapsed reading from the previous window
        (kick, synthetic),
        (kick, floored), // same-instant duplicate from the merge path
        (kick + 95, floored),
    ];
    // Newest confirmed pair is the all-API one: the anchor sits on the floored
    // boundary, inside the minute of the true kick instant.
    assert_eq!(series_open(&series), Some(floored - FIVE_HOUR_SECS));
    // Without the API tail, the mixed synthetic/floored pair confirms and the
    // exact stamp wins.
    assert_eq!(series_open(&series[..3]), Some(kick));

    // The newest window appears in one sample only: unconfirmed, so the
    // anchor stays on the previous confirmed boundary.
    let single = [
        (t, t + 600),
        (t + 90, t + 600),
        (t + 7200, t + 7200 + FIVE_HOUR_SECS),
    ];
    assert_eq!(series_open(&single), Some(t + 600 - FIVE_HOUR_SECS));
}

/// The election picks one member per tick in queue order — the serialisation
/// that stops two fetch workers from both kicking in the same tick.
#[test]
fn auto_start_queue_election_picks_one_lapsed_member_in_queue_order() {
    assert_eq!(
        elect_queue_member(&[
            candidate("a", false, 0),
            candidate("b", true, 0),
            candidate("c", true, 0),
        ]),
        Some("b"),
        "the first lapsed member in queue order wins"
    );
    assert_eq!(
        elect_queue_member(&[candidate("a", false, 0), candidate("b", false, 0)]),
        None,
        "nothing lapsed, nothing elected"
    );
    assert_eq!(elect_queue_member(&[]), None);
}

/// Head-of-line blocking is the failure this rule exists for: the macOS
/// `rotation_blocked_for` carve-out leaves a member permanently kick-incapable
/// while recording no `KickBlock` and never setting `auth_broken`, so it is
/// invisible to every other exclusion. Elected forever, it would starve every
/// member behind it.
#[test]
fn auto_start_queue_election_skips_a_member_that_keeps_failing() {
    // Literal 2 = MAX_ELECTION_FAILURES, deliberately NOT written in terms of
    // the constant: a fixture that moves with it survives the constant drifting
    // to any value, which is the opposite of a pin.
    let dead = 2;
    assert_eq!(
        elect_queue_member(&[
            candidate("stuck", true, dead),
            candidate("healthy", true, 0),
        ]),
        Some("healthy"),
        "a member past the failure limit must not block the one behind it"
    );
    assert_eq!(
        elect_queue_member(&[candidate("warming", true, 1), candidate("healthy", true, 0),]),
        Some("warming"),
        "under the limit it keeps its slot"
    );
}

/// When EVERY lapsed member is past the limit the queue still probes rather than
/// going silent — one request per tick is the right price for noticing that an
/// account recovered.
#[test]
fn auto_start_queue_election_still_probes_when_every_member_is_failing() {
    let dead = 5;
    assert_eq!(
        elect_queue_member(&[candidate("a", true, dead), candidate("b", true, dead)]),
        Some("a"),
        "an all-failing queue keeps retrying its head"
    );
}

/// A skip must never be permanent. A skipped member is never elected, so it can
/// never land the kick that would clear its streak — without a decay one
/// transient blockage (a live Claude Code session holding the Keychain item)
/// would eject an account from the queue for the rest of the process's life.
#[test]
fn auto_start_queue_failure_streak_decays_so_a_skip_is_never_permanent() {
    let now = 1_780_000_000i64;
    assert!(!decayed(now - 60, now), "a fresh failure still counts");
    assert!(
        !decayed(now - 3599, now),
        "just inside the window still counts"
    );
    assert!(decayed(now - 3600, now), "an hour old is forgiven");

    // The full path, not a synonym for the queue-order case: a member driven
    // past the skip limit by RECORDED failures is skipped, and the same member
    // — with the streak an hour older — is elected again, because
    // `queue_failures` reports a decayed streak as zero.
    let state = super::new_state();
    let recovered: crate::profile::ProfileName = "recovered".into();
    super::note_queue_kick_failed(&state, &recovered, now - 3600);
    super::note_queue_kick_failed(&state, &recovered, now - 3600);
    let skipped = super::queue_failures(&state, &recovered, now - 3600);
    assert_eq!(skipped, 2, "two recorded failures reach the skip limit");
    assert_eq!(
        elect_queue_member(&[
            candidate("recovered", true, skipped),
            candidate("healthy", true, 0),
        ]),
        Some("healthy"),
        "at the limit the slot passes to the healthy sibling"
    );
    let forgiven = super::queue_failures(&state, &recovered, now);
    assert_eq!(forgiven, 0, "an hour on, the streak reads as zero");
    assert_eq!(
        elect_queue_member(&[
            candidate("recovered", true, forgiven),
            candidate("healthy", true, 0),
        ]),
        Some("recovered"),
        "once forgiven it takes its queue slot back"
    );
}

/// The restart path reads the writer's REAL output — every profile here is
/// seeded through [`crate::profile::append_usage_sample_at`], bridge lines
/// included, not a handwritten fixture (review round 2: the fixtures omitted
/// bridges, and on real files the `(fresh, bridge)` pair — old payload,
/// new timestamp, `dr = 0` — confirmed the idle shape at every cadence while
/// the pins stayed green). Startup seeding confirms a fixed boundary from two
/// samples, an idle series yields no anchor at all, and the recovered anchor
/// feeds the published next opening.
#[test]
fn auto_start_queue_history_seed_and_next_open_replay_real_usage_files() {
    use crate::usage::{UsageInfo, UsageWindow, epoch_secs_to_iso};
    let _home = crate::testutil::HomeSandbox::new();
    let sample = 1_780_000_000i64;
    let boundary = sample + 600;

    let reading = |reset: i64| UsageInfo {
        five_hour: Some(UsageWindow {
            utilization: 0.0,
            resets_at: Some(epoch_secs_to_iso(reset)),
        }),
        ..UsageInfo::default()
    };
    // Replay a poll sequence through the real writer: cold fill first (no
    // bridge), then each later sample bridged exactly as the scheduler's
    // apply path does with `prev` = the store value the outcome replaces.
    let write_history = |name: &str, pairs: &[(i64, i64)]| {
        let name: crate::profile::ProfileName = name.into();
        let mut prev: Option<UsageInfo> = None;
        for (ts, reset) in pairs {
            let next = reading(*reset);
            crate::profile::append_usage_sample_at(&name, prev.as_ref(), &next, *ts as u64 * 1000);
            prev = Some(next);
        }
    };

    write_history(
        "opened",
        &[
            (sample, boundary),
            (sample + 90, boundary - 1),
            (sample + 180, boundary),
        ],
    );
    write_history(
        "idle",
        &[
            (sample, sample + FIVE_HOUR_SECS),
            (sample + 90, sample + 90 + FIVE_HOUR_SECS),
            (sample + 180, sample + 180 + FIVE_HOUR_SECS),
        ],
    );
    // The writer really bridged: 3 samples -> 5 lines (cold fill, then two
    // bridge+fresh pairs). A fixture without them would pin nothing about the
    // reader's bridge handling.
    assert_eq!(
        crate::profile::load_usage_history(&"idle".into()).len(),
        5,
        "the seeded file must carry the writer's bridge lines"
    );

    let opened = vec![crate::profile::ProfileName::from("opened")];
    let expected = boundary - FIVE_HOUR_SECS;
    let seeded = super::new_state();
    super::seed_queue_anchor(&seeded, &opened);
    assert_eq!(
        super::queue_anchor_cached(&seeded),
        Some(expected),
        "startup seeding recovers the newest confirmed boundary minus 5h"
    );

    // A cold tick with no cached anchor: `queue_due(None, ..)` is true, so the
    // replay runs. `now`/`gap` are the election's own, and any `now` well past
    // the seeded samples leaves the queue due.
    let now = sample + 10 * FIVE_HOUR_SECS;
    let gap = super::queue_gap_secs(2, INTERVAL_MS);
    let cold = super::new_state();
    assert_eq!(
        super::queue_anchor(&cold, &opened, now, gap),
        Some(expected),
        "the cold tick fallback replays the same history series"
    );
    assert_eq!(
        super::queue_anchor(&super::new_state(), &["idle".into()], now, gap),
        None,
        "a sliding idle tail never becomes an anchor, bridges included"
    );
    assert_eq!(
        super::next_queue_open_secs(Some(expected), 2, INTERVAL_MS),
        Some(expected + 2 * 3600 + 1800 - 90),
        "the published next opening is the recovered anchor plus the queue gap"
    );
}

/// The bridge rule end to end, on the two shapes that must land on OPPOSITE
/// sides of it. An idle stretch bridges on every poll, and the pair
/// `(fresh@T-90, bridge@T-1)` — old payload, new timestamp, `dr = 0` — reads
/// as a held-still boundary unless bridges are dropped; that false anchor sat
/// ~now and held the queue for a full gap after every spawn. A landed kick
/// produces the ONE bridge that carries new information (the synthetic store
/// stamp `mark_window_open` wrote), which differs from its predecessor,
/// survives the drop, and confirms same-instant against the API's floored
/// report.
#[test]
fn auto_start_queue_series_reader_drops_the_writers_bridge_lines() {
    use crate::usage::{UsageInfo, UsageWindow, epoch_secs_to_iso};
    let _home = crate::testutil::HomeSandbox::new();
    let t = 1_780_000_000i64;

    let reading = |reset: i64| UsageInfo {
        five_hour: Some(UsageWindow {
            utilization: 0.0,
            resets_at: Some(epoch_secs_to_iso(reset)),
        }),
        ..UsageInfo::default()
    };
    let append = |name: &str, prev: Option<&UsageInfo>, next: &UsageInfo, ts: i64| {
        crate::profile::append_usage_sample_at(&name.into(), prev, next, ts as u64 * 1000);
    };

    // Idle polls at the default cadence, through the real writer.
    let idle: Vec<UsageInfo> = [t, t + 90, t + 180, t + 270]
        .iter()
        .map(|ts| reading(ts + FIVE_HOUR_SECS))
        .collect();
    append("p", None, &idle[0], t);
    append("p", Some(&idle[0]), &idle[1], t + 90);
    append("p", Some(&idle[1]), &idle[2], t + 180);
    append("p", Some(&idle[2]), &idle[3], t + 270);
    assert_eq!(
        crate::profile::load_usage_history(&"p".into()).len(),
        7,
        "4 idle samples write 7 lines: a cold fill, then bridge+fresh pairs"
    );
    assert_eq!(
        series_open(&super::member_series(&"p".into())),
        None,
        "an idle history yields no anchor, bridges dropped"
    );

    // A kick lands mid-minute: the store holds the exact synthetic stamp when
    // the floored fresh body arrives, so the writer bridges the synthetic —
    // byte-different from its predecessor, and the same-instant pair confirms
    // with the exact stamp winning.
    let kick = t + 360 + 14;
    let synthetic = reading(kick + FIVE_HOUR_SECS);
    let floored = reading(kick + FIVE_HOUR_SECS - 14);
    append("p", Some(&synthetic), &floored, kick);
    assert_eq!(
        series_open(&super::member_series(&"p".into())),
        Some(kick),
        "the kick's synthetic/floored pair still confirms same-instant"
    );
}

/// A bridge need not duplicate its file predecessor byte-for-byte: the
/// `/profile`-despite-429 overlay advances the store's `plan` in place with
/// no history line, so the next bridge re-stamps the SAME 5h window under a
/// changed payload. Whole-payload identity missed exactly that bridge — the
/// pair `(fresh, plan-drifted bridge)` has `dr = 0` and confirmed — so the
/// drop is judged on the 5h-window projection alone.
#[test]
fn auto_start_queue_series_reader_drops_a_plan_drifted_bridge() {
    use crate::usage::{PlanInfo, UsageInfo, UsageWindow, epoch_secs_to_iso};
    let _home = crate::testutil::HomeSandbox::new();
    let t = 1_780_000_000i64;

    let reading = |reset: i64| UsageInfo {
        five_hour: Some(UsageWindow {
            utilization: 0.0,
            resets_at: Some(epoch_secs_to_iso(reset)),
        }),
        ..UsageInfo::default()
    };
    let name: crate::profile::ProfileName = "p".into();
    let idle = reading(t + FIVE_HOUR_SECS);
    crate::profile::append_usage_sample_at(&name, None, &idle, t as u64 * 1000);

    // Between polls the overlay flips the plan on the live entry; the next
    // fresh poll bridges that mutated value — same 5h window, new payload.
    let mut drifted = idle.clone();
    drifted.plan = Some(PlanInfo {
        subscription_status: Some("canceled".to_string()),
        ..PlanInfo::default()
    });
    let next = reading(t + 90 + FIVE_HOUR_SECS);
    crate::profile::append_usage_sample_at(&name, Some(&drifted), &next, (t + 90) as u64 * 1000);

    assert_eq!(
        crate::profile::load_usage_history(&name).len(),
        3,
        "the drifted bridge really landed as its own line"
    );
    assert_eq!(
        series_open(&super::member_series(&name)),
        None,
        "a re-stamped 5h window is no observation, whatever else drifted"
    );
}

/// An open the queue did NOT fire still has to gate it. A real Claude Code
/// session on a member account opens that account's 5h window; nothing in
/// clauth kicked, so `note_queue_open` never runs, and the in-memory anchor is
/// the only thing the gate used to consult once seeded. The member behind that
/// open could then be elected seconds after it — re-collapsing the two windows
/// the queue exists to separate, on every cycle, because the anchor re-phases
/// only around our own kicks (review round 4).
///
/// So [`super::queue_anchor`] composes the two by `max` instead of
/// short-circuiting, and the second leg pins the bound that keeps that cheap:
/// a cached anchor already inside the gap is returned WITHOUT the replay,
/// because `max` can only move an anchor forward and forward is deeper into
/// the gap — the skip cannot change a gate answer, only leave the value one
/// derivation stale where it does not matter.
#[test]
fn auto_start_queue_anchor_folds_in_an_out_of_band_open() {
    use crate::usage::{UsageInfo, UsageWindow, epoch_secs_to_iso};
    let _home = crate::testutil::HomeSandbox::new();
    let now = 1_780_000_000i64;
    // Ten minutes ago, by something that is not us.
    let opened_at = now - 600;
    let boundary = opened_at + FIVE_HOUR_SECS;

    let reading = |reset: i64| UsageInfo {
        five_hour: Some(UsageWindow {
            utilization: 0.0,
            resets_at: Some(epoch_secs_to_iso(reset)),
        }),
        ..UsageInfo::default()
    };
    // Through the real writer, bridges and all: the boundary holds still across
    // three polls while the clock advances, which is what confirms it.
    let mut prev: Option<UsageInfo> = None;
    for (ts, reset) in [
        (now - 180, boundary),
        (now - 90, boundary - 1),
        (now, boundary),
    ] {
        let next = reading(reset);
        crate::profile::append_usage_sample_at(
            &"held".into(),
            prev.as_ref(),
            &next,
            ts as u64 * 1000,
        );
        prev = Some(next);
    }

    let members = vec![crate::profile::ProfileName::from("held")];
    let gap = queue_gap_secs(2, INTERVAL_MS);

    // The anchor our own kicks left behind: four hours old, well past the gap.
    let state = super::new_state();
    let stale = now - 4 * 3600;
    super::note_queue_open(&state, &"held".into(), stale);
    assert!(
        queue_due(Some(stale), now, gap),
        "the cached anchor alone would let the next member kick right now"
    );
    assert_eq!(
        super::queue_anchor(&state, &members, now, gap),
        Some(opened_at),
        "the out-of-band open supersedes the older in-memory anchor"
    );
    assert!(
        !queue_due(super::queue_anchor(&state, &members, now, gap), now, gap),
        "and the queue is held shut for a full gap after it"
    );
    assert_eq!(
        super::queue_anchor_cached(&state),
        Some(opened_at),
        "the derivation is folded back into memory, so the render read agrees"
    );

    // Cached inside the gap and OLDER than the derivation: the replay would
    // move it, and is skipped anyway because it could not change the answer.
    let inside = super::new_state();
    super::note_queue_open(&inside, &"held".into(), now - 900);
    assert_eq!(
        super::queue_anchor(&inside, &members, now, gap),
        Some(now - 900),
        "a cached anchor already inside the gap answers without a disk replay"
    );
}

/// The due/lapsed bounds on the replay leave one hole, and this is the patch
/// for it: while a member's kick AND its `/usage` refresh are both failing it
/// stays lapsed, the queue stays due, and the election keeps probing it by
/// design — so every refresh tick would re-parse every member's history for as
/// long as the outage lasts. No fresh body lands during one, so nothing is
/// appended, so memoizing the derivation against the FILES closes exactly that
/// window (review round 4, found by an adversarial pass over the round-4 fix).
///
/// The second half is the anti-regression half, and the one that matters: a
/// memo that went stale would silently undo the out-of-band fix it is bolted
/// onto, so a genuinely newer open must still be picked up across it.
#[test]
fn auto_start_queue_anchor_memoizes_the_replay_against_the_history_files() {
    use crate::usage::{UsageInfo, UsageWindow, epoch_secs_to_iso};
    let _home = crate::testutil::HomeSandbox::new();
    let now = 1_780_000_000i64;
    let reading = |reset: i64| UsageInfo {
        five_hour: Some(UsageWindow {
            utilization: 0.0,
            resets_at: Some(epoch_secs_to_iso(reset)),
        }),
        ..UsageInfo::default()
    };
    let mut prev: Option<UsageInfo> = None;
    // Three polls holding one boundary still: a confirmed open at `at`.
    let confirm_open_at = |at: i64, prev: &mut Option<UsageInfo>| {
        let boundary = at + FIVE_HOUR_SECS;
        for (ts, reset) in [
            (at - 420, boundary),
            (at - 330, boundary - 1),
            (at - 240, boundary),
        ] {
            let next = reading(reset);
            crate::profile::append_usage_sample_at(
                &"held".into(),
                prev.as_ref(),
                &next,
                ts as u64 * 1000,
            );
            *prev = Some(next);
        }
    };

    let members = vec![crate::profile::ProfileName::from("held")];
    let gap = queue_gap_secs(2, INTERVAL_MS);
    let first_open = now - 600;
    confirm_open_at(first_open, &mut prev);

    // Stable while the files are: the fingerprint is the whole basis for
    // skipping a parse, so a spurious change would skip nothing.
    let signature = super::history_signature(&members);
    assert_eq!(
        signature,
        super::history_signature(&members),
        "the fingerprint must not move on its own"
    );

    let state = super::new_state();
    super::note_queue_open(&state, &"held".into(), now - 4 * 3600);
    assert_eq!(
        super::queue_anchor(&state, &members, now, gap),
        Some(first_open),
        "the first derivation still folds in the out-of-band open"
    );
    assert_eq!(
        state.lock().unwrap().derived,
        Some((signature, Some(first_open))),
        "and is memoized against the fingerprint it was derived at"
    );

    // A second, newer out-of-band open, one gap later so the queue is due
    // again. The memo must NOT answer for it.
    let later = now + gap + 10;
    let second_open = later - 300;
    confirm_open_at(second_open, &mut prev);
    assert_ne!(
        super::history_signature(&members),
        signature,
        "an append has to move the fingerprint, or the memo goes stale"
    );
    assert_eq!(
        super::queue_anchor(&state, &members, later, gap),
        Some(second_open),
        "a newer confirmed open is picked up across the memo"
    );
}
