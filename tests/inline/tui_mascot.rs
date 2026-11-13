#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::{Cue, Mascot, Stage};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Move {
    Wave,
    Hop,
    Blink,
}

/// A stand-in second mascot: its own launch and hello move, a long hop for a
/// switch, a short blink for the daemon going down, nothing for any other cue.
struct Dot;

impl Mascot for Dot {
    type Anim = Move;

    const SIZE: (usize, usize) = (1, 3);

    fn draw(&self, _anim: Option<Move>, _index: usize) -> Vec<ratatui::text::Line<'static>> {
        vec![ratatui::text::Line::raw("o"); 3]
    }

    fn launch(&self) -> &'static [Move] {
        &[Move::Wave]
    }

    fn hello(&self) -> &'static [Move] {
        &[Move::Wave]
    }

    fn react(&self, cue: Cue) -> Option<Move> {
        if cue == Cue::Switch {
            Some(Move::Hop)
        } else if cue == Cue::DaemonDown {
            Some(Move::Blink)
        } else {
            None
        }
    }

    /// Four frames (320 ms) each, the hop fifty (4 s).
    fn frame_count(&self, anim: Move) -> usize {
        if anim == Move::Hop { 50 } else { 4 }
    }
}

/// A Dot stage whose launch has played out by 1 s.
fn idle_dot() -> Stage<Dot> {
    let mut stage = Stage::new(Dot);
    stage.tick(0);
    assert_eq!(
        stage.playing_anim(),
        Some(Move::Wave),
        "the launch played the mascot's own move"
    );
    stage.tick(1_000);
    assert_eq!(stage.playing_anim(), None, "a four-frame move outran 1 s");
    stage
}

#[test]
fn a_cue_plays_the_mascots_own_answer_and_an_ignored_cue_plays_nothing() {
    let mut stage = idle_dot();
    stage.cue(Cue::WindowReset, 30_000);
    assert_eq!(
        stage.playing_anim(),
        None,
        "a cue the mascot ignores played a move"
    );
    stage.cue(Cue::Switch, 30_000);
    assert_eq!(stage.playing_anim(), Some(Move::Hop));
}

#[test]
fn a_running_move_is_never_cut_and_the_waiting_cue_plays_after_it() {
    let mut stage = idle_dot();
    stage.cue(Cue::DaemonDown, 30_000);
    stage.cue(Cue::Switch, 30_100);
    stage.tick(30_100);
    assert_eq!(
        stage.playing_anim(),
        Some(Move::Blink),
        "a later cue cut the running move"
    );
    stage.tick(30_400);
    assert_eq!(stage.playing_anim(), Some(Move::Hop));
}

#[test]
fn a_cue_waiting_past_three_seconds_drops() {
    let mut stage = idle_dot();
    stage.cue(Cue::Switch, 30_000);
    stage.cue(Cue::DaemonDown, 30_500);
    stage.tick(34_100);
    assert_eq!(
        stage.playing_anim(),
        None,
        "a cue held 3.6 s behind the hop still played"
    );
}

#[test]
fn one_move_plays_at_most_once_per_twenty_seconds() {
    let mut stage = idle_dot();
    stage.cue(Cue::DaemonDown, 30_000);
    assert_eq!(stage.playing_anim(), Some(Move::Blink));
    stage.tick(31_000);
    stage.cue(Cue::DaemonDown, 40_000);
    stage.tick(40_100);
    assert_eq!(
        stage.playing_anim(),
        None,
        "the same move played twice inside 20 s"
    );
    stage.tick(43_200);
    assert_eq!(
        stage.playing_anim(),
        None,
        "the repeat waited and played once due"
    );
    stage.cue(Cue::DaemonDown, 50_000);
    assert_eq!(stage.playing_anim(), Some(Move::Blink));
}

#[test]
fn reduce_motion_and_a_hidden_mascot_play_nothing() {
    let mut stage = idle_dot();
    stage.set_reduce_motion(true);
    stage.cue(Cue::DaemonDown, 30_000);
    assert_eq!(stage.playing_anim(), None, "reduce motion played a move");

    stage.set_reduce_motion(false);
    stage.set_shown(false);
    stage.cue(Cue::DaemonDown, 31_000);
    assert_eq!(stage.playing_anim(), None, "a hidden mascot played a move");

    stage.set_shown(true);
    stage.cue(Cue::DaemonDown, 32_000);
    assert_eq!(
        stage.playing_anim(),
        Some(Move::Blink),
        "control: a shown, moving mascot plays the cue"
    );
}

/// A mascot with no launch or hello animation: it answers a switch only.
struct Mute;

impl Mascot for Mute {
    type Anim = Move;

    const SIZE: (usize, usize) = (1, 3);

    fn draw(&self, _anim: Option<Move>, _index: usize) -> Vec<ratatui::text::Line<'static>> {
        vec![ratatui::text::Line::raw("o"); 3]
    }

    fn launch(&self) -> &'static [Move] {
        &[]
    }

    fn hello(&self) -> &'static [Move] {
        &[]
    }

    fn react(&self, cue: Cue) -> Option<Move> {
        (cue == Cue::Switch).then_some(Move::Hop)
    }

    fn frame_count(&self, _anim: Move) -> usize {
        4
    }
}

#[test]
fn an_empty_launch_or_hello_pool_plays_nothing_and_cues_still_play() {
    let mut stage = Stage::new(Mute);
    stage.tick(0);
    assert_eq!(stage.playing_anim(), None, "an empty launch pool played");
    stage.key_pressed(5 * 60_000 + 1_000);
    assert_eq!(stage.playing_anim(), None, "an empty hello pool played");
    stage.cue(Cue::Switch, 30 * 60_000);
    assert_eq!(
        stage.playing_anim(),
        Some(Move::Hop),
        "control: the mascot still answers its cue"
    );
}

#[test]
fn a_key_after_five_quiet_minutes_plays_the_mascots_own_hello() {
    let mut stage = idle_dot();
    stage.key_pressed(60_000);
    assert_eq!(stage.playing_anim(), None, "a key one minute in said hello");
    stage.key_pressed(60_000 + 5 * 60_000 + 1_000);
    assert_eq!(stage.playing_anim(), Some(Move::Wave));
}
