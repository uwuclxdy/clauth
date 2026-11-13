//! The header mascot's seam: what happened ([`Cue`]), how one mascot answers
//! it ([`Mascot`]), and the playback rules every mascot shares ([`Stage`]).
//!
//! A producer hands the stage a [`Cue`]; the stage decides when it plays and
//! asks its mascot which animation answers the cue and how to draw it. The
//! mascot itself holds no clock: its own size, frame tables and colour
//! mapping travel with it, and the stage knows it only through the trait.

mod cue;
mod stage;

pub(crate) use cue::{Cue, Rank};
pub(crate) use stage::Stage;
#[cfg(test)]
pub(crate) use stage::{FRAME_MS, Playing};

use ratatui::text::Line;

/// One mascot: its animations and which one answers each cue. The playback
/// rules live in [`Stage`], so a mascot holds no clock of its own.
pub(crate) trait Mascot {
    type Anim: Copy + Eq + std::hash::Hash + std::fmt::Debug + 'static;
    /// The animations the launch picks one from; an empty pool plays nothing.
    fn launch(&self) -> &'static [Self::Anim];
    /// The animations the after-quiet hello picks one from; an empty pool
    /// plays nothing.
    fn hello(&self) -> &'static [Self::Anim];
    /// The animation answering `cue`, `None` for a cue this mascot ignores.
    fn react(&self, cue: Cue) -> Option<Self::Anim>;
    /// How many frames `anim` runs.
    fn frame_count(&self, anim: Self::Anim) -> usize;
    /// The mascot's own cell size, `(columns, rows)`: the header's art fit
    /// reads it, and the header gives it one more column of padding.
    const SIZE: (usize, usize);
    /// The styled lines drawing `anim`'s frame `index`; `None` paints the
    /// rest pose. Colours come from the theme module.
    fn draw(&self, anim: Option<Self::Anim>, index: usize) -> Vec<Line<'static>>;
    /// The startup verdict on whether the terminal paints the full sprite
    /// arm. A mascot with one arm ignores it.
    fn set_full_sprite(&mut self, full_sprite: bool) {
        let _ = full_sprite;
    }
}

#[cfg(test)]
#[path = "../../../tests/inline/tui_mascot.rs"]
mod tests;
