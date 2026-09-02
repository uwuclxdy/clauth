//! Pure profile/usage → display-string formatters, plus the cross-surface
//! diagnostic messages. No UI dependencies, so the TUI, the CLI subcommands
//! (e.g. `clauth which`), and the headless daemon all share one spelling.
//!
//! Deliberately the ratatui-free tier only. Helpers that emit `Span`/`Style`
//! live in `tui/render/format.rs`; single-screen or domain-local display glue
//! stays with its owner. Folding those in would force ratatui into this shared
//! module or mint one-caller abstractions, so the split stands over a single
//! grab-bag `format.rs` (surveyed 2026-07-16).

use chrono::{DateTime, Datelike, Local, Timelike};

use crate::profile::Profile;
use crate::usage::{PlanTier, humanize_duration};

// ── Cross-surface diagnostics ───────────────────────────────────────────────
//
// A condition that surfaces on more than one surface — a CLI `bail!`, a daemon
// `logline!`, a TUI toast — is worded here once. Each surface used to spell the
// same event its own way and they drifted (one condition printed four different
// sentences). `head` is the at-a-glance summary; `detail` the cause and the
// recovery step.

/// One diagnostic, rendered per surface. Keep `head` short enough to read on a
/// toast's bold first line without wrapping; put the cause and next step in
/// `detail`.
pub(crate) struct Message {
    head: String,
    detail: Option<String>,
}

impl Message {
    /// Single-line form for a CLI `bail!` or a `logline!` body (`head: detail`).
    /// The caller prepends any `clauth `/`clauth daemon: ` log prefix.
    pub(crate) fn line(&self) -> String {
        match &self.detail {
            Some(d) => format!("{}: {}", self.head, d),
            None => self.head.clone(),
        }
    }

    /// Toast form: `head` on its own line, `detail` below it. The toast renderer
    /// styles line 1 bold and the rest dim, so the split reads as summary + note.
    pub(crate) fn toast(&self) -> String {
        match &self.detail {
            Some(d) => format!("{}\n{}", self.head, d),
            None => self.head.clone(),
        }
    }

    /// The next step alone, for a surface whose own first line ALREADY states
    /// the condition — the rotate toast opens with `refresh for 'X' failed`, so
    /// rendering a whole `Message` under it named the account three times.
    /// Falls back to `head` when there is no detail, so a caller never has to
    /// mint copy of its own; minting is the drift this module exists to stop.
    pub(crate) fn detail(&self) -> &str {
        self.detail.as_deref().unwrap_or(&self.head)
    }
}

/// What to tell the operator to do about a transient failure.
///
/// Travels INSIDE [`Transient`] rather than arriving as a parameter: three
/// surfaces render [`refresh_transient`], and a `kind` argument would re-scatter
/// this choice across exactly the call sites this module exists to unify.
#[derive(Debug)]
pub(crate) enum Retry {
    /// A transport failure — the connection is the thing worth checking.
    Connection,
    /// Upstream is throttling, busy, or briefly broken. Waiting is the fix, and
    /// telling someone to check their connection over a 429 is wrong advice.
    Wait,
    /// The cause already names its own next step, so a second one would
    /// contradict it (`check permissions on ~/.clauth` followed by `check your
    /// connection and retry` gives two different and incompatible reasons to
    /// retry, one of which is wrong).
    Stated,
    /// There is nothing left to retry in-process: `login_with` has no retry
    /// path around its code exchange, so whatever the status, the only action
    /// available is running `clauth login` again. Stated as the ABSENCE of a
    /// retry loop rather than as a fact about the code or the listener, because
    /// this correctly stops being true the moment someone adds one.
    Restart,
}

/// Every transient cause clauth can state, as a CLOSED set.
///
/// Deliberately not one open `String` field. The historically-real accident is
/// `format!("{status}: {body}")` handed to a free-text cause, and that no longer
/// has an arm to land in: the caller must pick one that describes what actually
/// happened, which is a question a response body does not answer.
///
/// What the types ENFORCE is narrower than that, and worth stating exactly.
/// Only [`Self::Endpoint`] is sealed — `&'static str` cannot hold a
/// runtime-allocated response body, and it takes precisely what
/// `TokenFailure::user_message` returns. [`Self::RotationLockUnavailable`] and
/// [`Self::PersistFailed`] still hold a `String`; what keeps THOSE honest is
/// that each renders its own fixed sentence below and interpolates the value as
/// a profile name, so a body passed there would read as an account name and
/// nothing else. Sealing all four means a newtype only the callers can mint;
/// worth doing if a fifth arm ever needs a runtime value that is not a name.
#[derive(Clone, Debug)]
pub(crate) enum Cause {
    /// Already-canned copy from `oauth::TokenFailure`.
    Endpoint(&'static str),
    /// The per-profile rotation lock could not be CREATED or OPENED — a
    /// filesystem or permissions problem under `~/.clauth`.
    ///
    /// Not contention, despite what this arm used to say. `RotationGuard::
    /// acquire` ends in a blocking `File::lock()`, so a sibling worker or a live
    /// session holding the lock makes the caller WAIT; it can never surface
    /// here. The old copy told the operator to wait for an in-flight refresh
    /// that does not exist, and a test pinned that wording.
    RotationLockUnavailable(String),
    /// A poisoned mutex: another thread panicked, so it will not clear itself
    /// and a retry hint would be a lie.
    InternalLock,
    /// The refresh landed but the rotated pair could not be written.
    PersistFailed(String),
    /// CLA-ROLL: the rolling session token could not be written to (or restored
    /// into) the profile's sidecar. The chain itself is fine — what failed is
    /// the file in front of it, so this is a filesystem problem and not an
    /// account one.
    SidecarWriteFailed(String),
    /// CLA-ROLL: a live `clauth start` session is holding this profile's
    /// ROTATING pair, because it started before the sidecar was armed. Spending
    /// the refresh now revokes the chain under a running session, which is the
    /// exact death the static-token split exists to prevent.
    ///
    /// Distinct from [`Self::RotationLockUnavailable`] on purpose: nothing is
    /// locked and nothing is broken. The next step is the operator's, and it is
    /// specific enough that a generic retry hint would be wrong.
    LiveSessionOnRotatingChain(String),
    /// Another holder has the profile's rotation lock and the caller must not
    /// park behind it — the scheduler's CLA-ROLL re-stamp leg, which runs on a
    /// thread that cannot wait, and the account-mutation refusals, which decline
    /// rather than park on a form of the acquire that carries no deadline. Both are
    /// properties of the FORM, not of the lock: a session start takes a bounded
    /// acquire (`runtime::ROTATION_LOCK_TIMEOUT`), and neither of these callers
    /// wants that wait either. Genuine contention — the
    /// opposite claim from [`Self::RotationLockUnavailable`], which is why it is
    /// not that arm: the holder's own path usually re-stamps the sidecar itself,
    /// and the scan retries in minutes against an hours-wide horizon either way.
    ///
    /// Its rendered sentence is shared with every other surface refusing on a
    /// busy rotation lock — `actions::rotation_guard_for_mutation` and the
    /// TUI's session-token clear — deliberately: one condition an operator can
    /// hit from three surfaces reads as one condition, where three spellings
    /// sent someone looking for three. The clear and its test render this arm
    /// directly; the actions' `bail!` is the one restatement, and a test
    /// compares the two rather than trusting them to match.
    RotationLockHeld(String),
    /// CLA-ROLL: the usage chain's RECORDED grant cannot be told from a
    /// setup-token mint (no scope beyond the setup pair, no plan stamp), so
    /// stamping a rolling bearer from it is refused — the bearer could later
    /// be preserved as "the mint". Not a filesystem problem and not retryable
    /// in-process: only a fresh `clauth login` records the chain's real grant.
    RollingGrantUnrecorded(String),
    /// CLA-ROLL: the sidecar holds a rotating pair (mis-filled) with no live
    /// mint backup to heal it, and the caller runs on the thread that must not
    /// fall into the blocking vanilla gate (the scheduler's re-stamp leg —
    /// which also has no re-stamp work to do on a disengaged split). Not
    /// retryable in-process: only a fresh `clauth login <p> --setup-token`
    /// re-captures the mint.
    SidecarMisfilled(String),
    /// The cross-process state flock could not be taken inside its bounded
    /// wait — another clauth process is busy under `~/.clauth` (on macOS that
    /// flock is even held across `/usr/bin/security` shell-outs, bounded in
    /// aggregate by `lock::SUBPROCESS_BUDGET`). Surfaced by the CLA-ROLL
    /// sidecar repair and the gate's rotation-adoption leg alike. Genuine
    /// contention, not a fault: the holder finishes and a retry goes through.
    /// Distinct from [`Self::SidecarWriteFailed`] and
    /// [`Self::StateLockUnavailable`] on purpose — that copy prescribes a
    /// permissions check, which a busy sibling would send the operator on for
    /// nothing. Same contention-vs-fault split as
    /// [`Self::RotationLockUnavailable`] (fault) vs
    /// [`Self::RotationLockHeld`] (contention).
    StateLockBusy(String),
    /// The cross-process state flock could not be CREATED or OPENED during
    /// the gate's rotation-adoption leg — a filesystem or permissions problem
    /// under `~/.clauth`, not contention. The gate aborted rather than
    /// refresh from a pair a sibling may already have advanced. Distinct from
    /// [`Self::StateLockBusy`] on purpose — that copy says a busy sibling
    /// will finish, which a fault never does. Same contention-vs-fault split
    /// as [`Self::RotationLockUnavailable`] (fault) vs
    /// [`Self::RotationLockHeld`] (contention), one lock further down.
    StateLockUnavailable(String),
}

impl Cause {
    /// Whether the arm's rendered copy already names the operator's next step,
    /// so an appended retry hint would duplicate it (`RotationLockHeld` ends in
    /// `retry in a moment`) or contradict it (a permissions check or a re-login,
    /// which waiting never fixes). [`Transient`]'s constructors enforce the
    /// pairing against this rather than leaving it to convention.
    ///
    /// New arms default to self-prescribing: an arm joins the list below only
    /// by an explicit edit, so an unclassified arm refuses `Wait` loudly at
    /// construction instead of shipping the stutter.
    fn names_its_own_next_step(&self) -> bool {
        !matches!(
            self,
            Self::Endpoint(_) | Self::PersistFailed(_) | Self::StateLockBusy(_)
        )
    }

    fn text(&self) -> String {
        match self {
            Self::Endpoint(canned) => (*canned).to_string(),
            Self::RotationLockUnavailable(profile) => {
                format!(
                    "could not lock '{profile}' for a token refresh; check permissions on ~/.clauth"
                )
            }
            Self::InternalLock => "clauth hit an internal lock error, restart clauth".to_string(),
            Self::SidecarWriteFailed(profile) => {
                format!(
                    "could not write '{profile}' session token · check permissions on ~/.clauth"
                )
            }
            Self::LiveSessionOnRotatingChain(profile) => {
                format!(
                    "'{profile}' has a live clauth start session holding its rotating chain \
                     (it started before the rolling token was armed); restart that session or \
                     retry once it ends"
                )
            }
            Self::RotationLockHeld(profile) => {
                format!("'{profile}' has a token rotation in progress, retry in a moment")
            }
            Self::RollingGrantUnrecorded(profile) => {
                format!(
                    "'{profile}' usage chain has no recorded grant beyond the setup-token \
                     scopes, so a rolling bearer cannot be told from a mint · run \
                     `clauth login {profile}` to record the chain's real grant"
                )
            }
            Self::SidecarMisfilled(profile) => {
                format!(
                    "'{profile}' session token holds a rotating pair and no live mint backup \
                     exists to heal it · re-capture with `clauth login {profile} --setup-token`"
                )
            }
            Self::StateLockBusy(profile) => {
                format!(
                    "another clauth process holds ~/.clauth's state lock · '{profile}' left \
                     unchanged"
                )
            }
            Self::StateLockUnavailable(profile) => {
                format!(
                    "could not lock '{profile}' for a token refresh; check permissions on ~/.clauth"
                )
            }
            Self::PersistFailed(profile) => {
                format!("refreshed '{profile}' but failed to persist the rotated tokens")
            }
        }
    }
}

/// A transient failure carrying its own next step, and the HTTP status when the
/// failure had one.
///
/// The status is deliberately separable: CLI stderr and the daemon log show it
/// (neither has a companion log to read it out of) while a toast and the MCP
/// payload do not.
pub(crate) struct Transient {
    cause: Cause,
    status: Option<u16>,
    retry: Retry,
}

impl Transient {
    /// # Panics
    ///
    /// If `cause` names its own next step and `retry` is [`Retry::Wait`]: the
    /// appended `retry in a moment` would duplicate or contradict the cause's
    /// own advice. Pair with [`Retry::Stated`] instead. Unreachable from every
    /// production site today — each passes a literal retry and none pairs a
    /// self-prescribing arm with `Wait` — so a wrong pairing fails here rather
    /// than shipping a stutter.
    pub(crate) fn new(cause: Cause, retry: Retry) -> Self {
        Self::refuse_contradicting_wait(&cause, &retry);
        Self {
            cause,
            status: None,
            retry,
        }
    }

    /// [`Self::new`] with an HTTP status. The same pairing rule applies: the
    /// status sits between the two clauses but does not un-stutter them.
    pub(crate) fn with_status(cause: Cause, status: u16, retry: Retry) -> Self {
        Self::refuse_contradicting_wait(&cause, &retry);
        Self {
            cause,
            status: Some(status),
            retry,
        }
    }

    fn refuse_contradicting_wait(cause: &Cause, retry: &Retry) {
        assert!(
            !cause.names_its_own_next_step() || !matches!(retry, Retry::Wait),
            "cause {cause:?} names its own next step; retry {retry:?} would duplicate or \
             contradict it"
        );
    }

    fn suffix(&self) -> &'static str {
        match self.retry {
            Retry::Connection => ": check your connection and retry",
            Retry::Wait => ": retry in a moment",
            Retry::Stated => "",
            Retry::Restart => ": run clauth login again for a fresh code",
        }
    }

    /// Cause + next step, no status. TUI toasts and the MCP `reason`.
    pub(crate) fn text(&self) -> String {
        format!("{}{}", self.cause.text(), self.suffix())
    }

    /// The causes only a fresh `clauth login` clears — no in-process retry
    /// can: an unrecorded chain grant ([`Cause::RollingGrantUnrecorded`]) and
    /// a mis-filled sidecar with nothing live to heal it
    /// ([`Cause::SidecarMisfilled`]). The scheduler paces these on the same
    /// long leash as a `Broken` verdict — a minutes-scale retry against a
    /// condition no retry can clear is pure log noise. The re-login itself
    /// stamps NOTHING scheduler-side (a browser login writes only
    /// `credentials.json`, and a `--setup-token` re-mint writes a mint, which
    /// disarms rather than re-arms), which is why these holds carry a
    /// credential-file watch in the scheduler: a write to any watched file —
    /// the fix, or clauth's own successful rotation, either of which is
    /// reason to re-judge — releases the leash on the next scan instead of
    /// waiting out the clock.
    pub(crate) fn permanent_until_relogin(&self) -> bool {
        matches!(
            self.cause,
            Cause::RollingGrantUnrecorded(_) | Cause::SidecarMisfilled(_)
        )
    }

    /// Cause + status + next step. CLI stderr and the daemon log, the two
    /// surfaces with no companion log to read the status out of.
    pub(crate) fn text_with_status(&self) -> String {
        match self.status {
            Some(s) => format!("{} (HTTP {s}){}", self.cause.text(), self.suffix()),
            None => self.text(),
        }
    }
}

/// A login whose refresh token is dead: re-login is the only fix. Shared by the
/// CLI/MCP switch bail, the daemon tick log, the TUI switch toast, the MCP
/// pre-flight's quarantine arm, and — through
/// `oauth::third_party_dead_chain_copy`'s `None` case — the rotate toast and
/// the quarantine's own log line, wherever the profile neither serves its own
/// inference nor is a recognised keyless one. `clauth rolling-token`'s dead-chain bail takes that
/// same `None` case but words its own sentence, since it also has to say the
/// arming did not happen.
pub(crate) fn login_expired(name: &crate::profile::ProfileName) -> Message {
    Message {
        head: format!("login for '{name}' has expired"),
        detail: Some(format!(
            "refresh token revoked or invalid: run clauth login {name}"
        )),
    }
}

/// A third-party profile with no inference auth source: an api key is the only
/// credential that fixes it, so the fix names the `--api-key` command — a bare
/// `clauth login <name>` on a third-party profile runs the browser flow (OAuth
/// for most providers, the console flow on Alibaba) and leaves the missing key
/// missing, while `--api-key` also lifts any quarantine the profile carries
/// (`clauth login` is the documented quarantine recovery, AUTH-1 in
/// `actions.rs`). Rendered by the MCP pre-flight's keyless arm, the
/// rolling-token bail, the manual-rotate toast and the quarantine's own log
/// line, so the surfaces cannot spell one state two ways. Those last three
/// render it for a key the profile's `AuthExpired` verdict pronounces dead,
/// too — Alibaba excepted, whose verdict records a dead console session:
/// `oauth::third_party_dead_chain_copy` treats such a key as no credential.
pub(crate) fn third_party_keyless(name: &crate::profile::ProfileName) -> String {
    format!("profile has no api key: {name} (run `clauth login {name} --api-key <key>`)")
}

/// A third-party profile whose stored OAuth chain is dead while it still has
/// an inference auth source: the split state, named so the reader learns the
/// account is not dead and what clears the quarantine (an api-mode login
/// replaces the credential set and lifts the flag, AUTH-1 in `actions.rs`).
///
/// The sentence is owner-ruled verbatim and claims more than the predicate
/// behind it proves: `has_inference_auth` is satisfied by a well-formed key OR
/// an `[env]` token, and well-formed is not live. The key half is guarded
/// where a verdict can speak to it — `oauth::third_party_dead_chain_copy`
/// consults the per-credential `AuthExpired` verdict and renders
/// [`third_party_keyless`] instead when the record matches the profile's
/// current credential, except on Alibaba, whose verdict records a dead console
/// session, not a key. The `[env]`-token half and the Alibaba console half
/// stay unguarded: open copy questions the owner has not ruled yet.
///
/// Three sites route through `oauth::third_party_dead_chain_copy`:
/// `cmd_rolling_token`'s up-front dead-chain bail, the manual-rotate toast,
/// and the quarantine's own `mark_auth_broken` log line. `report_armed_sidecar`
/// carries a fourth dead-chain sentence and is deliberately NOT routed: its
/// `chain_is_broken` comes from the arm's own gate, which cannot reach `Broken`
/// for a profile with a `base_url`, so a third-party branch there is dead code.
/// The MCP pre-flight admits that target instead of refusing it, so it renders
/// nothing (owner ruling 2026-08-30).
/// The command is backticked to match [`third_party_keyless`], which renders
/// beside it on the same surfaces (owner ruling: house style).
pub(crate) fn third_party_dead_chain(name: &crate::profile::ProfileName) -> String {
    format!(
        "stored OAuth chain is dead, its api key still works: {name} \
         (run `clauth login {name} --api-key <key>` to clear the quarantine)"
    )
}

/// A refresh that failed for a transient reason: this switch is refused but the
/// login is not quarantined. The next step comes from `err`'s own [`Retry`], so
/// a throttle is never told to check its connection.
pub(crate) fn refresh_transient(name: &crate::profile::ProfileName, err: &Transient) -> Message {
    Message {
        head: format!("could not refresh '{name}' before switching"),
        detail: Some(err.text()),
    }
}

/// [`refresh_transient`] for CLI stderr, which additionally names the HTTP
/// status. Split as a second constructor rather than a flag, because `line()`
/// serves BOTH the CLI bail and the MCP payload — the surface split cannot be
/// made on the renderer.
pub(crate) fn refresh_transient_cli(
    name: &crate::profile::ProfileName,
    err: &Transient,
) -> Message {
    Message {
        head: format!("could not refresh '{name}' before switching"),
        detail: Some(err.text_with_status()),
    }
}

/// The one spelling for "go fix this in the app". The surface is the `clauth`
/// TUI, never a bare "the TUI" (which reads as some other UI).
pub(crate) const RESOLVE_IN_TUI: &str = "resolve the divergence in the clauth TUI";

/// The `s` a count needs, per cloudy-tui's counts rule: singular at one.
pub(crate) fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

/// A duration as a LENGTH, never as an instant.
///
/// `humanize_duration` spells zero `now`, which is right where the figure is a
/// countdown to something and wrong wherever it is a span: `elapsed now` and
/// `finished now ago` both read as broken. Every value above zero is that
/// function's, so this is the zero boundary and nothing else.
///
/// One helper because the rule kept being re-answered: three surfaces wrote
/// their own guard and reached three different words. `src/tui/render/plugin.rs`
/// still spells its own `just now`, deliberately — a pane's phrasing is its own
/// — and folding that one in is owed.
pub(crate) fn humanize_span(secs: u64) -> String {
    if secs == 0 {
        return "0s".to_string();
    }
    humanize_duration(secs as i64)
}

/// Trailing-ellipsis truncation to `max` chars (counts `char`s, not bytes).
pub(crate) fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    if max == 0 {
        return String::new();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// The account's tier: a fetched plan wins when classified, else the OAuth
/// token's `subscription_type` claim, else `None` when neither names one. A
/// surface renders its own no-data form on `None` rather than a bare "Claude"
/// that reads as a real plan.
pub(crate) fn account_tier(profile: &Profile) -> Option<PlanTier> {
    // A fetched tier wins, but an UNCLASSIFIED one is not an answer: fall through
    // the way `profile_json::tier_label` does, or this surface reads "no data"
    // while that one shows the token's tier for the very same account.
    let fetched = profile
        .usage
        .as_ref()
        .and_then(|u| u.plan.as_ref())
        .map(|p| p.tier.clone())
        .filter(|t| *t != PlanTier::Unknown);
    fetched.or_else(|| {
        // No fetched plan yet — fall back to the OAuth token's subscription_type.
        let sub = profile
            .credentials
            .as_ref()
            .and_then(|c| c.claude_ai_oauth.as_ref())
            .and_then(|o| o.subscription_type.as_deref());
        match PlanTier::from_subscription_type(sub) {
            PlanTier::Unknown => None,
            tier => Some(tier),
        }
    })
}

/// Percent from API `f64`: drops trailing `.0` on whole numbers → `42%`, `42.3%`.
pub(crate) fn format_pct(pct: f64) -> String {
    if pct.fract() == 0.0 {
        format!("{pct:.0}%")
    } else {
        format!("{pct}%")
    }
}

/// The one LOCAL prose-stamp formatter: an epoch-seconds instant as
/// `YYYY-MM-DD HH:MM:SS` in the operator's local wall clock. A second spelling
/// of a LOCAL stamp is a bug in its caller, not a new helper. Machine timestamps
/// that stay UTC by design — the daemon `logline!` prefix and `clauth sessions
/// --json`'s `updated` — do not route through here.
/// Returns `None` when the instant falls outside chrono's representable range.
pub(crate) fn local_stamp(epoch: i64) -> Option<String> {
    let naive = DateTime::from_timestamp(epoch, 0)?
        .with_timezone(&Local)
        .naive_local();
    Some(format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        naive.year(),
        naive.month(),
        naive.day(),
        naive.hour(),
        naive.minute(),
        naive.second(),
    ))
}

#[cfg(test)]
#[path = "../tests/inline/format.rs"]
mod tests;
