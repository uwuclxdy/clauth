//! The playback rules every mascot shares: the launch, the collision slot and
//! the clocks, all here so a mascot holds no clock of its own.
//!
//! Nothing plays on a clock. The launch plays one animation from the mascot's
//! own pool on the first tick the art is shown — a startup slow enough to
//! miss it still gets it, counted from that first tick. After it, each
//! [`Cue`] plays the animation the mascot answers it with, and a key after
//! five quiet minutes plays a small hello from the mascot's own pool (the
//! `reduce_motion` setting stands in for the source's `prefersReducedMotion`
//! check: it holds the mascot on its rest pose). A cue never cuts a running
//! animation: one waiting slot keeps the highest-ranked cue behind it, a cue
//! that waited past three seconds drops, and one animation plays at most once
//! per twenty seconds, counted from its last start (the launch included; a
//! dropped request does not restart it). While the header draws no art, cues
//! drop and the clock stops: the launch waits for the first shown tick, and
//! hiding drops the playing and the waiting animation. Frames advance on the
//! TUI's own 80 ms tick rather than the source's 60 ms redraw: the frame
//! index is `elapsed / 80 ms`, so a frame whose tick runs late is skipped
//! rather than queued, and an extra draw repeats the current frame.

use std::collections::HashMap;

use ratatui::text::Line;

use super::{Cue, Mascot, Rank};

/// One frame lasts this long: the TUI's own tick.
pub(crate) const FRAME_MS: u64 = 80;
/// One animation plays at most once per this span, whichever cue asks for it.
const REPEAT_GAP_MS: u64 = 20_000;
/// A cue waiting behind the running animation for longer than this drops: it
/// would play too late to read as a reaction to its event.
const MAX_WAIT_MS: u64 = 3_000;
/// Keyless span after which a key plays the hello.
const QUIET_MS: u64 = 5 * 60 * 1000;

/// One animation running, with its start: the frame index reads off it.
#[derive(Clone, Copy)]
pub(crate) struct Playing<A> {
    pub(crate) anim: A,
    pub(crate) start_ms: u64,
}

/// A cue held behind the running animation.
#[derive(Clone, Copy)]
struct Waiting<A> {
    anim: A,
    rank: Rank,
    since_ms: u64,
}

/// The playback state for one mascot: what runs, what waits behind it, and
/// the clocks the repeat and hello rules read. The render path reads
/// [`Stage::draw`] and never mutates it; [`Stage::tick`], [`Stage::cue`] and
/// [`Stage::key_pressed`] move it.
pub(crate) struct Stage<M: Mascot> {
    mascot: M,
    pub(crate) playing: Option<Playing<M::Anim>>,
    waiting: Option<Waiting<M::Anim>>,
    /// The launch animation has not played yet: it opens on the first tick
    /// the art is shown, so startup work before it cannot clip it.
    launch_pending: bool,
    /// When each animation last started, for the twenty-second repeat rule.
    last_start_ms: HashMap<M::Anim, u64>,
    /// The last key, or the TUI's open before any key.
    last_key_ms: u64,
    /// Whether the header draws the art.
    shown: bool,
    /// `reduce_motion`: nothing plays at all.
    still: bool,
    seed: u64,
}

impl<M: Mascot> Stage<M> {
    pub(crate) fn new(mascot: M) -> Self {
        Self {
            mascot,
            playing: None,
            waiting: None,
            launch_pending: true,
            last_start_ms: HashMap::new(),
            last_key_ms: 0,
            shown: true,
            still: false,
            seed: random_seed(),
        }
    }

    pub(crate) fn tick(&mut self, now_ms: u64) {
        if self.still || !self.shown {
            return;
        }
        if self.launch_pending {
            self.launch_pending = false;
            let pool = self.mascot.launch();
            let Some(&anim) = self.pick(pool.len()).and_then(|pick| pool.get(pick)) else {
                return;
            };
            self.start(anim, now_ms);
            return;
        }
        self.settle(now_ms);
    }

    /// An event happened: play the animation the mascot answers it with, or
    /// hold it in the waiting slot behind the running one. A cue the mascot
    /// ignores plays nothing.
    pub(crate) fn cue(&mut self, cue: Cue, now_ms: u64) {
        if self.still || !self.shown {
            return;
        }
        let Some(anim) = self.mascot.react(cue) else {
            return;
        };
        self.request(anim, cue.rank(), now_ms);
    }

    /// A key was pressed: after five quiet minutes it plays the hello. Every
    /// key restarts the quiet span, a key the mascot cannot show included.
    pub(crate) fn key_pressed(&mut self, now_ms: u64) {
        let quiet = now_ms.saturating_sub(self.last_key_ms) >= QUIET_MS;
        self.last_key_ms = now_ms;
        if !quiet || self.still || !self.shown {
            return;
        }
        let hello = self.mascot.hello();
        if let Some(&anim) = self.pick(hello.len()).and_then(|pick| hello.get(pick)) {
            self.request(anim, Rank::Ambient, now_ms);
        }
    }

    /// Whether the header draws the art this frame. Hiding drops the running
    /// and the waiting animation: neither would be seen, and replaying them
    /// on a later show would react to something long past.
    pub(crate) fn set_shown(&mut self, shown: bool) {
        self.shown = shown;
        if !shown {
            self.playing = None;
            self.waiting = None;
        }
    }

    /// The `reduce_motion` setting. Turning it on stills the mascot at once
    /// and spends the launch, so turning it off later replays nothing.
    pub(crate) fn set_reduce_motion(&mut self, still: bool) {
        self.still = still;
        if still {
            self.playing = None;
            self.waiting = None;
            self.launch_pending = false;
        }
    }

    /// The animation running now, if any.
    #[cfg(test)]
    pub(crate) fn playing_anim(&self) -> Option<M::Anim> {
        self.playing.map(|playing| playing.anim)
    }

    /// Render tests pin one animation instead of sampling the random launch
    /// choice. Never compiled into the binary.
    #[cfg(test)]
    pub(crate) fn play(&mut self, anim: M::Anim, start_ms: u64) {
        self.playing = Some(Playing { anim, start_ms });
    }

    /// The lines to draw now, from the mascot's own draw: the running
    /// animation's frame, or the rest pose between animations.
    pub(crate) fn draw(&self, now_ms: u64) -> Vec<Line<'static>> {
        let Some(playing) = self.playing else {
            return self.mascot.draw(None, 0);
        };
        let index = usize::try_from(now_ms.saturating_sub(playing.start_ms) / FRAME_MS)
            .unwrap_or(usize::MAX);
        if index >= self.mascot.frame_count(playing.anim) {
            return self.mascot.draw(None, 0);
        }
        self.mascot.draw(Some(playing.anim), index)
    }

    /// The startup verdict on whether the terminal paints the full sprite
    /// arm; a mascot with one arm ignores it.
    pub(crate) fn set_full_sprite(&mut self, full_sprite: bool) {
        self.mascot.set_full_sprite(full_sprite);
    }

    /// Retire an animation whose last frame has passed, then start the
    /// waiting cue unless it waited too long or its animation played too
    /// recently.
    fn settle(&mut self, now_ms: u64) {
        if let Some(playing) = self.playing {
            let frames_ms = self.mascot.frame_count(playing.anim) as u64 * FRAME_MS;
            if now_ms.saturating_sub(playing.start_ms) < frames_ms {
                return;
            }
            self.playing = None;
        }
        if self.launch_pending {
            return;
        }
        if let Some(waiting) = self.waiting.take()
            && now_ms.saturating_sub(waiting.since_ms) <= MAX_WAIT_MS
            && self.due(waiting.anim, now_ms)
        {
            self.start(waiting.anim, now_ms);
        }
    }

    pub(crate) fn request(&mut self, anim: M::Anim, rank: Rank, now_ms: u64) {
        self.settle(now_ms);
        if !self.due(anim, now_ms) {
            return;
        }
        if self.playing.is_none() && !self.launch_pending {
            self.start(anim, now_ms);
            return;
        }
        if self.waiting.is_none_or(|waiting| rank > waiting.rank) {
            self.waiting = Some(Waiting {
                anim,
                rank,
                since_ms: now_ms,
            });
        }
    }

    /// Whether `anim` may start: at most once per [`REPEAT_GAP_MS`], counted
    /// from its last start, the launch's included.
    fn due(&self, anim: M::Anim, now_ms: u64) -> bool {
        self.last_start_ms
            .get(&anim)
            .is_none_or(|&started| now_ms.saturating_sub(started) >= REPEAT_GAP_MS)
    }

    fn start(&mut self, anim: M::Anim, now_ms: u64) {
        self.playing = Some(Playing {
            anim,
            start_ms: now_ms,
        });
        self.last_start_ms.insert(anim, now_ms);
    }

    /// The pool entry to play, `None` for an empty pool: a mascot whose
    /// launch or hello pool is empty plays nothing instead of panicking.
    fn pick(&mut self, len: usize) -> Option<usize> {
        if len == 0 {
            None
        } else {
            Some((self.next_random() % len as u64) as usize)
        }
    }

    fn next_random(&mut self) -> u64 {
        self.seed ^= self.seed << 13;
        self.seed ^= self.seed >> 7;
        self.seed ^= self.seed << 17;
        self.seed
    }
}

#[cfg(not(test))]
fn random_seed() -> u64 {
    let mut bytes = [0u8; 8];
    match getrandom::fill(&mut bytes) {
        Ok(()) => u64::from_le_bytes(bytes) | 1,
        // Cosmetic choice only: a fixed pick is a fine fallback for a failed
        // entropy read, and the seed must stay non-zero for the xorshift.
        Err(_) => 1,
    }
}

/// Render tests sample specific animations; a fixed seed keeps every run on
/// the same choice.
#[cfg(test)]
fn random_seed() -> u64 {
    0x9E37_79B9_7F4A_7C15
}
