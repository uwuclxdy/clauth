//! Usage read off an inference response instead of a usage endpoint (#110).
//! Some accounts can only be measured that way: a `claude setup-token` login
//! lacks the `user:profile` scope `/api/oauth/usage` needs, and some providers
//! report usage on inference responses alone. The parse is per provider, one
//! submodule each; this module holds the reading every one of them returns,
//! so a caller never learns a provider's header names.
//!
//! A reading keeps each window under the provider's own name. Which of them
//! become the store's 5h, 7d or per-model windows, and what they are called on
//! screen, is the provider module's call too, made with the store mapping.

pub(crate) mod anthropic;

/// One rate-limit window an inference response reported.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ProbeWindow {
    /// The provider's own name for the window (`5h`, `7d`, `7d_oi` on
    /// Anthropic), lowercased like the header it came from. Kept as sent so a
    /// window the provider adds later arrives without a code change.
    pub(crate) name: String,
    /// Share of the window used, on the 0..100 scale every usage window uses.
    /// Never negative; a provider may report past 100.
    pub(crate) utilization: f64,
    /// When the window resets, in epoch seconds, as sent: one already past is
    /// kept for the caller to judge.
    pub(crate) resets_at: Option<i64>,
    /// The provider's status for this window alone.
    pub(crate) status: Option<ProbeStatus>,
}

/// Whether a provider let a request through, for one window or for the
/// response as a whole.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProbeStatus {
    Allowed,
    /// Let through, close to the limit.
    Warning,
    Rejected,
}

/// Everything one inference response said about the account's usage.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ProbeReading {
    /// Every window the response reported, in the order it reported them.
    pub(crate) windows: Vec<ProbeWindow>,
    /// The response-wide status.
    pub(crate) status: Option<ProbeStatus>,
    /// The response-wide reset, in epoch seconds, as sent (see the provider
    /// module for what it means there).
    pub(crate) resets_at: Option<i64>,
}
