//! What happened, that a producer hands the mascot. The [`Cue`] carries no
//! animation: each mascot answers cues with its own animations in
//! [`super::Mascot::react`], and the rank beside it decides which cue keeps
//! the one waiting slot.

/// Something the mascot reacts to. Every caller hands it to
/// [`super::Stage::cue`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Cue {
    /// The global active account changed, from any source.
    Switch,
    /// One live session moved to another account.
    SessionMoved,
    SessionStarted,
    LoginGained,
    UpdateInstalled,
    /// A 5h or 7d window rolled over to a later reset.
    WindowReset,
    DelegateDone,
    BellCrossed,
    NewIncident,
    /// The daemon runs a binary that was replaced on disk.
    DaemonReplaced,
    Quarantined,
    RefreshFailed,
    DelegateFailed,
    DaemonDown,
}

/// A cue's claim on the one waiting slot: a higher rank displaces a lower one
/// waiting there.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Rank {
    /// Good news nobody has to act on; also the hello's rank.
    Ambient,
    Activity,
    Notice,
    Fault,
}

impl Cue {
    pub(crate) fn rank(self) -> Rank {
        match self {
            Self::LoginGained | Self::UpdateInstalled | Self::WindowReset | Self::DelegateDone => {
                Rank::Ambient
            }
            Self::Switch | Self::SessionMoved | Self::SessionStarted => Rank::Activity,
            Self::BellCrossed | Self::NewIncident | Self::DaemonReplaced => Rank::Notice,
            Self::Quarantined | Self::RefreshFailed | Self::DelegateFailed | Self::DaemonDown => {
                Rank::Fault
            }
        }
    }
}
