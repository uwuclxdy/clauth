use super::*;

/// Glyph rows of one arm. A claim about a cell's style reads `render`
/// directly, so the `CellKind` survives to the assertion.
fn rows(frame: &Frame, full_sprite: bool) -> Vec<String> {
    render(frame, full_sprite)
        .iter()
        .map(|row| row.iter().map(|(glyph, _)| *glyph).collect())
        .collect()
}

#[test]
fn rest_pose_matches_the_live_claude_code_capture() {
    assert_eq!(
        rows(&REST, true),
        [" ▐▛███▛█ ", "▝▜██████▀", " ▝▝   ▝▝ "].map(String::from)
    );
}

#[test]
fn animation_frame_counts_match_the_extracted_sequences() {
    let expected = [
        (Anim::Jump, 12),
        (Anim::Look, 11),
        (Anim::Celebrate, 15),
        (Anim::Skip, 14),
        (Anim::Spin, 10),
        (Anim::Peekaboo, 21),
        (Anim::Drop, 13),
        (Anim::Waddle, 19),
        (Anim::Peek, 20),
        (Anim::Wink, 6),
        (Anim::Sneeze, 12),
        (Anim::Turn, 19),
        (Anim::CoinHop, 14),
    ];
    for (anim, count) in expected {
        assert_eq!(anim.frames().len(), count, "{}", anim.name());
    }
}

#[test]
fn the_hello_and_launch_pools() {
    assert_eq!(HELLO.map(Anim::name), ["look", "wink"]);
    assert_eq!(
        BIG.map(Anim::name),
        [
            "jump",
            "celebrate",
            "skip",
            "spin",
            "peekaboo",
            "drop",
            "waddle",
            "peek",
            "turn",
            "coin-hop",
        ]
    );
}

/// The startup gate: `Apple_Terminal` alone selects the simplified arm, and an
/// unset `TERM_PROGRAM` selects the full one.
#[test]
fn the_simplified_arm_is_apple_terminal_alone() {
    assert!(!full_sprite(Some("Apple_Terminal")));
    assert!(full_sprite(Some("tmux")));
    assert!(full_sprite(Some("iTerm.app")));
    assert!(full_sprite(None));
}

/// Every event, the animation it plays, and its rank for the one waiting
/// slot (higher outranks lower).
const BINDINGS: [(Cue, &str, u8); 14] = [
    (Cue::Switch, "jump", 1),
    (Cue::SessionMoved, "skip", 1),
    (Cue::SessionStarted, "waddle", 1),
    (Cue::LoginGained, "celebrate", 0),
    (Cue::UpdateInstalled, "celebrate", 0),
    (Cue::WindowReset, "spin", 0),
    (Cue::DelegateDone, "wink", 0),
    (Cue::BellCrossed, "peek", 2),
    (Cue::NewIncident, "peek", 2),
    (Cue::DaemonReplaced, "peek", 2),
    (Cue::Quarantined, "sneeze", 3),
    (Cue::RefreshFailed, "sneeze", 3),
    (Cue::DelegateFailed, "sneeze", 3),
    (Cue::DaemonDown, "sneeze", 3),
];

/// A mascot past its launch animation, idle at `LAUNCH_DONE_MS`.
const LAUNCH_DONE_MS: u64 = 5_000;

fn settled() -> Stage<Clawd> {
    let mut clawd = Stage::new(Clawd::new());
    clawd.tick(0);
    clawd.tick(LAUNCH_DONE_MS);
    assert_eq!(
        clawd.playing_anim(),
        None,
        "the launch animation never ended"
    );
    clawd
}

/// The playing animation's name and start, read off the state the frame
/// reads.
fn now_playing(clawd: &Stage<Clawd>) -> Option<(&'static str, u64)> {
    clawd
        .playing
        .map(|playing| (playing.anim.name(), playing.start_ms))
}

#[test]
fn every_event_plays_its_bound_animation_at_once() {
    for (cue, name, _) in BINDINGS {
        assert_eq!(
            Clawd::new().react(cue).expect("bound").name(),
            name,
            "{cue:?}"
        );
        let mut clawd = settled();
        clawd.cue(cue, 30_000);
        assert_eq!(now_playing(&clawd), Some((name, 30_000)), "{cue:?}");
        assert!(
            clawd.frame(30_000).is_some(),
            "{cue:?} draws its first frame"
        );
    }
}

/// The simplified sprite (Terminal.app) shows every bound animation as motion.
#[test]
fn every_bound_animation_reads_on_the_simplified_sprite() {
    for (cue, _, _) in BINDINGS {
        assert!(
            reads_as_motion(Clawd::new().react(cue).expect("bound")),
            "{cue:?}"
        );
    }
    for anim in HELLO {
        assert!(reads_as_motion(anim), "{}", anim.name());
    }
}

/// Nothing plays on a timer once the launch animation ends: not at the old
/// ten-second idle slot, not at the old thirty-minute one, not hours later.
#[test]
fn no_timer_plays_anything_after_launch() {
    let mut clawd = settled();
    for now in [
        10_000,
        20_000,
        30 * 60 * 1000,
        30 * 60 * 1000 + 10_000,
        3 * 60 * 60 * 1000,
    ] {
        clawd.tick(now);
        assert_eq!(now_playing(&clawd), None, "something played at {now} ms");
    }
}

#[test]
fn an_event_never_cuts_a_running_animation() {
    let mut clawd = settled();
    clawd.cue(Cue::Switch, 30_000);
    let jump_ends = 30_000 + frames_ms(Anim::Jump);
    clawd.cue(Cue::Quarantined, 30_080);
    clawd.tick(30_080);
    clawd.tick(jump_ends - 1);
    assert_eq!(now_playing(&clawd), Some(("jump", 30_000)));
    clawd.tick(jump_ends);
    assert_eq!(now_playing(&clawd), Some(("sneeze", jump_ends)));
}

/// One slot waits behind the running animation and keeps the highest-ranked
/// event; the others drop and never play.
#[test]
fn the_waiting_slot_keeps_the_highest_ranked_event() {
    let mut clawd = settled();
    clawd.cue(Cue::SessionMoved, 30_000);
    clawd.cue(Cue::DelegateDone, 30_100);
    clawd.cue(Cue::BellCrossed, 30_200);
    clawd.cue(Cue::Switch, 30_300);
    clawd.cue(Cue::WindowReset, 30_400);
    let skip_ends = 30_000 + frames_ms(Anim::Skip);
    clawd.tick(skip_ends);
    assert_eq!(now_playing(&clawd), Some(("peek", skip_ends)));
    let peek_ends = skip_ends + frames_ms(Anim::Peek);
    clawd.tick(peek_ends);
    assert_eq!(now_playing(&clawd), None, "a dropped event played");
    clawd.tick(peek_ends + 5_000);
    assert_eq!(now_playing(&clawd), None, "a dropped event played");
}

/// Each rank outranks the one below it in the slot, and only those.
#[test]
fn the_slot_ranks_follow_the_bindings() {
    for (low, low_name, low_rank) in BINDINGS {
        for (high, high_name, high_rank) in BINDINGS {
            if high_rank <= low_rank || low_name == "skip" || high_name == "skip" {
                continue;
            }
            let mut clawd = settled();
            clawd.cue(Cue::SessionMoved, 30_000);
            clawd.cue(low, 30_100);
            clawd.cue(high, 30_200);
            let skip_ends = 30_000 + frames_ms(Anim::Skip);
            clawd.tick(skip_ends);
            assert_eq!(
                now_playing(&clawd),
                Some((high_name, skip_ends)),
                "{high:?} outranks {low:?}"
            );

            let mut clawd = settled();
            clawd.cue(Cue::SessionMoved, 30_000);
            clawd.cue(high, 30_100);
            clawd.cue(low, 30_200);
            clawd.tick(skip_ends);
            assert_eq!(
                now_playing(&clawd),
                Some((high_name, skip_ends)),
                "{low:?} displaced {high:?}"
            );
        }
    }
}

/// A skip's rank holds in the waiting slot against every other rank: it
/// displaces the lower one and drops before the higher, whichever arrives
/// first. Each pair runs anchored on a running jump — a third animation, so
/// neither pair member's own repeat window can swallow it.
#[test]
fn a_skip_holds_its_rank_in_the_waiting_slot_against_every_other_rank() {
    for (other, winner) in [
        (Cue::WindowReset, "skip"),
        (Cue::BellCrossed, "peek"),
        (Cue::Quarantined, "sneeze"),
    ] {
        let jump_ends = 30_000 + frames_ms(Anim::Jump);
        let mut clawd = settled();
        clawd.cue(Cue::Switch, 30_000);
        clawd.cue(Cue::SessionMoved, 30_100);
        clawd.cue(other, 30_200);
        clawd.tick(jump_ends);
        assert_eq!(
            now_playing(&clawd),
            Some((winner, jump_ends)),
            "{other:?} meets a skip waiting first"
        );

        let mut clawd = settled();
        clawd.cue(Cue::Switch, 30_000);
        clawd.cue(other, 30_100);
        clawd.cue(Cue::SessionMoved, 30_200);
        clawd.tick(jump_ends);
        assert_eq!(
            now_playing(&clawd),
            Some((winner, jump_ends)),
            "a skip meets {other:?} waiting first"
        );
    }
}

/// A waiting event plays after waiting exactly three seconds and drops after
/// any longer wait.
#[test]
fn a_waiting_event_drops_after_three_seconds() {
    let mut clawd = settled();
    clawd.cue(Cue::Switch, 30_000);
    clawd.cue(Cue::Quarantined, 30_100);
    clawd.tick(33_100);
    assert_eq!(now_playing(&clawd), Some(("sneeze", 33_100)));

    let mut clawd = settled();
    clawd.cue(Cue::Switch, 30_000);
    clawd.cue(Cue::Quarantined, 30_100);
    clawd.tick(33_101);
    assert_eq!(now_playing(&clawd), None, "a stale event played");
}

/// One animation plays at most once per twenty seconds, whichever event asks
/// for it; another animation is not held back.
#[test]
fn one_animation_plays_at_most_once_per_twenty_seconds() {
    let mut clawd = settled();
    clawd.cue(Cue::BellCrossed, 30_000);
    clawd.tick(40_000);
    clawd.cue(Cue::Switch, 40_000);
    assert_eq!(now_playing(&clawd), Some(("jump", 40_000)));
    clawd.tick(45_000);
    clawd.cue(Cue::NewIncident, 49_999);
    assert_eq!(now_playing(&clawd), None, "peek played twice in 20 s");
    clawd.cue(Cue::DaemonReplaced, 50_000);
    assert_eq!(now_playing(&clawd), Some(("peek", 50_000)));

    // A dropped request does not restart the twenty seconds.
    let mut clawd = settled();
    clawd.cue(Cue::Quarantined, 30_000);
    clawd.tick(40_000);
    clawd.cue(Cue::RefreshFailed, 45_000);
    clawd.cue(Cue::DaemonDown, 50_000);
    assert_eq!(now_playing(&clawd), Some(("sneeze", 50_000)));
}

/// While the header draws no art, events drop and nothing advances: the
/// launch animation waits for the art, and a hidden event never plays later.
#[test]
fn a_hidden_mascot_drops_events_and_stops_its_clock() {
    let mut clawd = Stage::new(Clawd::new());
    clawd.set_shown(false);
    clawd.tick(0);
    clawd.tick(2_000);
    assert_eq!(now_playing(&clawd), None, "the launch played unseen");
    clawd.set_shown(true);
    clawd.tick(2_080);
    let launch = clawd.playing.expect("no launch once the art shows");
    assert!(BIG.contains(&launch.anim));
    assert_eq!(launch.start_ms, 2_080);

    let mut clawd = settled();
    clawd.set_shown(false);
    clawd.cue(Cue::Quarantined, 30_000);
    clawd.tick(30_000);
    clawd.set_shown(true);
    clawd.tick(30_080);
    assert_eq!(now_playing(&clawd), None, "a hidden event played later");

    let mut clawd = settled();
    clawd.cue(Cue::Switch, 30_000);
    clawd.cue(Cue::Quarantined, 30_100);
    clawd.set_shown(false);
    clawd.tick(30_500);
    clawd.set_shown(true);
    clawd.tick(30_000 + frames_ms(Anim::Jump));
    assert_eq!(
        now_playing(&clawd),
        None,
        "the waiting event survived a hide"
    );
}

/// A key after five minutes with no key plays one small hello; a key sooner,
/// or the next key after the hello, plays nothing. The TUI's start counts as
/// the last key.
#[test]
fn a_key_after_five_quiet_minutes_plays_one_hello() {
    const QUIET_MS: u64 = 5 * 60 * 1000;
    let mut clawd = settled();
    clawd.key_pressed(QUIET_MS - 1);
    assert_eq!(now_playing(&clawd), None, "a hello before five minutes");

    let mut clawd = settled();
    clawd.key_pressed(QUIET_MS);
    let hello = clawd.playing.expect("no hello after five quiet minutes");
    assert!(HELLO.contains(&hello.anim), "{}", hello.anim.name());
    assert_eq!(hello.start_ms, QUIET_MS);
    clawd.tick(QUIET_MS + 2_000);
    clawd.key_pressed(QUIET_MS + 2_000);
    assert_eq!(now_playing(&clawd), None, "a second hello");

    // Each key restarts the quiet five minutes.
    let mut clawd = settled();
    clawd.key_pressed(200_000);
    clawd.key_pressed(200_000 + QUIET_MS - 1);
    assert_eq!(now_playing(&clawd), None, "the quiet span ignored a key");
    clawd.key_pressed(200_000 + QUIET_MS - 1 + QUIET_MS);
    assert!(clawd.playing.is_some(), "no hello after a fresh quiet span");
}

/// `reduce_motion` keeps the mascot on its rest pose: no launch, no event, no
/// hello moves it.
#[test]
fn reduce_motion_keeps_the_mascot_still() {
    let mut clawd = Stage::new(Clawd::new());
    clawd.set_reduce_motion(true);
    clawd.tick(0);
    assert!(clawd.frame(0).is_none(), "the launch moved a still mascot");
    for (cue, _, _) in BINDINGS {
        clawd.cue(cue, 30_000);
        clawd.tick(30_000);
        assert!(
            clawd.frame(30_000).is_none(),
            "{cue:?} moved a still mascot"
        );
    }
    clawd.key_pressed(10 * 60 * 1000);
    assert!(
        clawd.frame(10 * 60 * 1000).is_none(),
        "a hello moved a still mascot"
    );

    // Turned on mid-animation, it stills at once.
    let mut clawd = settled();
    clawd.cue(Cue::Switch, 30_000);
    clawd.set_reduce_motion(true);
    assert!(
        clawd.frame(30_080).is_none(),
        "a running animation kept moving"
    );
}

#[test]
fn launch_opens_with_a_big_animation_on_the_first_tick() {
    let mut clawd = Stage::new(Clawd::new());
    assert!(
        clawd.playing.is_none(),
        "construction must not start the launch animation"
    );
    assert!(clawd.frame(0).is_none(), "rest draws before the first tick");
    clawd.tick(0);
    let playing = match clawd.playing {
        Some(playing) => playing,
        None => panic!("no launch animation"),
    };
    assert!(BIG.contains(&playing.anim));
    assert_eq!(playing.start_ms, 0);
    assert!(clawd.frame(0).is_some());

    // A startup slow enough to miss the animation's whole length still gets
    // it: the clock starts at the first tick, not at construction.
    let mut slow = Stage::new(Clawd::new());
    slow.tick(5_000);
    let playing = slow.playing.expect("no launch animation on a slow start");
    assert!(BIG.contains(&playing.anim));
    assert_eq!(playing.start_ms, 5_000);
}

#[test]
fn frames_advance_once_per_tui_tick() {
    let frames = Anim::Wink.frames();
    let mut clawd = Stage::new(Clawd::new());
    clawd.playing = Some(Playing {
        anim: Anim::Wink,
        start_ms: 0,
    });
    assert_eq!(clawd.frame(0), Some(&frames[0]));
    assert_eq!(clawd.frame(FRAME_MS - 1), Some(&frames[0]));
    assert_eq!(clawd.frame(FRAME_MS), Some(&frames[1]));
    assert_eq!(clawd.frame(FRAME_MS * frames.len() as u64), None);
}

#[test]
fn the_eye_facing_shift_and_effect_art_matches_the_extracted_frames() {
    assert_eq!(
        rows(&Anim::Wink.frames()[0], true),
        [" ▐▛███▂█ ", "▝▜██████▀", " ▝▝   ▝▝ "].map(String::from)
    );
    assert_eq!(
        rows(&Anim::Turn.frames()[0], true),
        [" ▐█▜██▛█ ", "▝▜██████▀", " ▝▝   ▝▝ "].map(String::from)
    );
    assert_eq!(
        rows(&Anim::Drop.frames()[0], true),
        ["         ", "         ", "         "].map(String::from)
    );
    assert_eq!(
        rows(&Anim::CoinHop.frames()[2], true),
        ["▗▟▛███▛█▄", " ▜██████▘", " ▝▝▁▁▁▝▝ "].map(String::from)
    );
    assert_eq!(
        rows(&Anim::Waddle.frames()[1], true),
        ["█▟       ", "██▀      ", "         "].map(String::from)
    );
}

#[test]
fn every_animation_opens_on_its_extracted_first_frame() {
    let expected = [
        (Anim::Jump, ["         ", " ▐▛███▛█ ", "·▜██████·"]),
        (Anim::Look, [" ▐█▟███▟ ", "▝▜██████▀", " ▝▝   ▝▝ "]),
        (Anim::Celebrate, ["         ", " ▐▛███▛█ ", "·▜██████·"]),
        (Anim::Skip, ["         ", "         ", "         "]),
        (Anim::Spin, [" ▐▟███▟█ ", "▝▜██████▀", " ▝▝   ▝▝ "]),
        (Anim::Peekaboo, ["         ", "         ", "         "]),
        (Anim::Drop, ["         ", "         ", "         "]),
        (Anim::Waddle, ["         ", "         ", "         "]),
        (Anim::Peek, ["         ", "         ", "         "]),
        (Anim::Wink, [" ▐▛███▂█ ", "▝▜██████▀", " ▝▝   ▝▝ "]),
        (Anim::Sneeze, ["▗▟▂███▂█▄", " ▜██████▘", " ▝▝   ▝▝ "]),
        (Anim::Turn, [" ▐█▜██▛█ ", "▝▜██████▀", " ▝▝   ▝▝ "]),
        (Anim::CoinHop, ["         ", " ▐▛███▛█ ", "▝▜██████▀"]),
    ];
    for (anim, art) in expected {
        assert_eq!(
            rows(&anim.frames()[0], true),
            art.map(String::from),
            "{}",
            { anim.name() }
        );
    }
}

#[test]
fn every_facing_rotation_matches_its_extracted_art() {
    let expected = [
        (Facing::Right12, [" ▐█▜██▛█ ", "▝▜██████▀", " ▝▝   ▝▝ "]),
        (Facing::Right30, ["  █▛██▛▌ ", " ▝█████▛ ", "  ▘▘  ▘▘ "]),
        (Facing::Right55, ["  ▐█▛█▜  ", "  ▐████  ", "  ▝▝ ▝▝  "]),
        (Facing::Right75, ["   ██▛▌  ", "   ███▌  ", "   ▘  ▘  "]),
        (Facing::Edge, ["   ▐██   ", "   ▐██   ", "   ▝ ▝   "]),
        (Facing::Back105, ["   ███▌  ", "   ███▌  ", "   ▘  ▘  "]),
        (Facing::Back125, ["  ▐████  ", "  ▐████  ", "  ▝▝ ▝▝  "]),
        (Facing::Back150, ["  █████▌ ", " ▝█████▛ ", "  ▘▘  ▘▘ "]),
        (Facing::Back, [" ▐██████ ", "▝▜██████▀", " ▝▝   ▝▝ "]),
        (Facing::Left75, ["   ▛██▌  ", "   ███▌  ", "   ▘  ▘  "]),
        (Facing::Left55, ["  ▐▜▛██  ", "  ▐████  ", "  ▝▝ ▝▝  "]),
        (Facing::Left30, ["  ▛██▛█▌ ", " ▝█████▛ ", "  ▘▘  ▘▘ "]),
        (Facing::Left12, [" ▐▛███▜█ ", "▝▜██████▀", " ▝▝   ▝▝ "]),
    ];
    for (facing, art) in expected {
        let frame = Frame {
            pose: Pose::Facing(facing),
            ..REST
        };
        assert_eq!(rows(&frame, true), art.map(String::from), "{facing:?}");
    }
}

/// Claude Code's simplified arm (`bt` in the source): a body edge either side
/// of a body-filled eye band, then the short feet row.
#[test]
fn the_simplified_sprite_paints_the_extracted_eye_band() {
    let want = [
        (Eyes::Open, "\u{2597} \u{2597}   \u{2596} \u{2596}"),
        (Eyes::Left, "\u{2597} \u{2598}   \u{2598} \u{2596}"),
        (Eyes::Right, "\u{2597} \u{259D}   \u{259D} \u{2596}"),
        (Eyes::Closed, "\u{2597} \u{2582}   \u{2582} \u{2596}"),
        (Eyes::Wink, "\u{2597} \u{2597}   \u{2582} \u{2596}"),
    ];
    for (eyes, row0) in want {
        let cell = frame(face(eyes, Arms::Down, Feet::Both), Opts::default());
        assert_eq!(
            rows(&cell, false),
            [row0, "         ", " \u{2598}\u{2598}   \u{259D}\u{259D} "].map(String::from),
            "{eyes:?}"
        );
    }
}

/// The band is the only body-background run in the simplified arm; the body
/// edges and the feet row are plain body glyphs.
#[test]
fn the_simplified_sprite_kinds_are_body_edges_over_a_lid_band() {
    use CellKind::{Body, Empty, Lid};
    let cell = frame(face(Eyes::Open, Arms::Down, Feet::Both), Opts::default());
    let expected = [
        [
            ('\u{2597}', Body),
            (' ', Lid),
            ('\u{2597}', Lid),
            (' ', Lid),
            (' ', Lid),
            (' ', Lid),
            ('\u{2596}', Lid),
            (' ', Lid),
            ('\u{2596}', Body),
        ],
        [
            (' ', Empty),
            (' ', Lid),
            (' ', Lid),
            (' ', Lid),
            (' ', Lid),
            (' ', Lid),
            (' ', Lid),
            (' ', Lid),
            (' ', Empty),
        ],
        [
            (' ', Empty),
            ('\u{2598}', Body),
            ('\u{2598}', Body),
            (' ', Body),
            (' ', Body),
            (' ', Body),
            ('\u{259D}', Body),
            ('\u{259D}', Body),
            (' ', Empty),
        ],
    ];
    assert_eq!(render(&cell, false), expected);
}

/// Every turned pose paints as an open face in the simplified arm, because
/// the simplified sprite has no facings.
#[test]
fn the_simplified_sprite_paints_every_facing_as_an_open_face() {
    let open = ["▗ ▗   ▖ ▖", "         ", " ▘▘   ▝▝ "].map(String::from);
    const ALL: [Facing; 13] = [
        Facing::Right12,
        Facing::Right30,
        Facing::Right55,
        Facing::Right75,
        Facing::Edge,
        Facing::Back105,
        Facing::Back125,
        Facing::Back150,
        Facing::Back,
        Facing::Left75,
        Facing::Left55,
        Facing::Left30,
        Facing::Left12,
    ];
    for facing in ALL {
        let turned = Frame {
            pose: Pose::Facing(facing),
            ..REST
        };
        assert_eq!(rows(&turned, false), open, "{facing:?}");
    }
}

/// Claude Code filters its launch pool for the simplified sprite: no facing
/// frame anywhere, and the first frame must not be the open-eyed rest pose.
#[test]
fn the_simplified_launch_pool_drops_facings_and_rest_pose_openings() {
    assert_eq!(
        big_pool(false),
        [
            Anim::Jump,
            Anim::Celebrate,
            Anim::Skip,
            Anim::Spin,
            Anim::Peekaboo,
            Anim::Drop,
            Anim::Waddle,
            Anim::Peek,
        ]
        .as_slice()
    );
}

/// Every facing cuts out exactly the column span Claude Code's facing table
/// names (2.1.310 binary, `eo`, copied by hand as `[from, to)` per row): the
/// cut-out cells of each row are that span and nothing else, and the feet row
/// is never cut.
#[test]
fn every_facing_cuts_the_span_the_source_table_names() {
    let table = [
        (Facing::Right12, (2, 8), (2, 7)),
        (Facing::Right30, (2, 7), (2, 7)),
        (Facing::Right55, (3, 7), (3, 7)),
        (Facing::Right75, (3, 6), (3, 6)),
        (Facing::Edge, (4, 6), (4, 6)),
        (Facing::Back105, (3, 6), (3, 6)),
        (Facing::Back125, (3, 7), (3, 7)),
        (Facing::Back150, (2, 7), (2, 7)),
        (Facing::Back, (2, 8), (2, 7)),
        (Facing::Left75, (3, 6), (3, 6)),
        (Facing::Left55, (3, 7), (3, 7)),
        (Facing::Left30, (2, 7), (2, 7)),
        (Facing::Left12, (2, 8), (2, 7)),
    ];
    for (facing, r0, r1) in table {
        let art = render(&frame(Pose::Facing(facing), Opts::default()), true);
        for (row, (from, to)) in [(0, r0), (1, r1), (2, (0, 0))] {
            let cut: Vec<usize> = (0..COLS)
                .filter(|&col| art[row][col].1 == CellKind::CutOut)
                .collect();
            assert_eq!(
                cut,
                (from..to).collect::<Vec<usize>>(),
                "{facing:?} row {row}"
            );
        }
    }
}

/// An equal-rank cue arriving behind the one waiting in the slot drops,
/// whichever of the two lands first: the slot keeps its first occupant at
/// that rank.
#[test]
fn an_equal_rank_event_drops_from_the_waiting_slot() {
    let mut clawd = settled();
    clawd.cue(Cue::SessionMoved, 30_000);
    clawd.cue(Cue::SessionStarted, 30_100);
    clawd.cue(Cue::Switch, 30_200);
    let skip_ends = 30_000 + frames_ms(Anim::Skip);
    clawd.tick(skip_ends);
    assert_eq!(now_playing(&clawd), Some(("waddle", skip_ends)));
    let waddle_ends = skip_ends + frames_ms(Anim::Waddle);
    clawd.tick(waddle_ends);
    assert_eq!(now_playing(&clawd), None, "the dropped jump played");

    let mut clawd = settled();
    clawd.cue(Cue::SessionMoved, 30_000);
    clawd.cue(Cue::Switch, 30_100);
    clawd.cue(Cue::SessionStarted, 30_200);
    clawd.tick(skip_ends);
    assert_eq!(now_playing(&clawd), Some(("jump", skip_ends)));
    let jump_ends = skip_ends + frames_ms(Anim::Jump);
    clawd.tick(jump_ends);
    assert_eq!(now_playing(&clawd), None, "the dropped waddle played");
}

/// The launch's own animation sits inside the twenty-second window: asking
/// for it again right after the launch drops, while another animation still
/// plays. A launch pick has no bound cue of its own, so the request goes in
/// directly at the same rank an event uses.
#[test]
fn the_launch_animation_counts_in_the_twenty_second_window() {
    let mut clawd = Stage::new(Clawd::new());
    clawd.tick(0);
    let launched = clawd.playing.expect("the launch animation").anim;
    let launch_ends = frames_ms(launched);
    clawd.tick(launch_ends);
    assert_eq!(now_playing(&clawd), None, "the launch never ended");
    clawd.request(launched, Rank::Activity, launch_ends + 1);
    assert_eq!(
        now_playing(&clawd),
        None,
        "the launch's animation repeated inside twenty seconds"
    );
    clawd.request(Anim::Wink, Rank::Activity, launch_ends + 1);
    assert_eq!(
        now_playing(&clawd).map(|playing| playing.0),
        Some("wink"),
        "a different animation was held back by the launch's window"
    );
}
