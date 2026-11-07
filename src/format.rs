//! Profile/usage → display-string formatters (pure but for
//! [`credit_refused_label`]'s clock read), plus the cross-surface diagnostic
//! messages, so the TUI, the CLI subcommands (e.g. `clauth which`), and the
//! headless daemon all share one spelling. No entry emits a `Span`/`Style`; its
//! ratatui use is measurement only — [`grapheme_cells`] sums the grapheme widths
//! a terminal draws (via [`CellWidth`]) so a CLI table pads by the same measure.
//! Helpers that emit `Span`/`Style` live in `tui/render/format.rs`, beside the
//! theme they style with, so this tier never carries TUI styling; single-screen
//! or domain-local display glue stays with its owner, since folding it in would
//! mint one-caller abstractions, so the split stands over a single grab-bag
//! `format.rs`.

use chrono::{DateTime, Datelike, Local, Timelike};
use ratatui::buffer::CellWidth;
use ratatui::style::Style;
use ratatui::text::Span;

use crate::fallback::{NoPollReason, StartBlock, StartCandidate};
use crate::profile::{Profile, ProfileName};
use crate::profile_json::OauthAge;
use crate::usage::{PlanInfo, PlanTier, UsageInfo, humanize_duration};

// ── Cross-surface diagnostics ───────────────────────────────────────────────
//
// A condition that surfaces on more than one surface — a CLI `bail!`, a daemon
// `logline!`, a TUI toast — is worded here once. Each surface used to spell the
// same event its own way and they drifted (one condition printed four different
// sentences). `head` is the at-a-glance summary; `detail` the cause and the
// recovery step.

pub(crate) const DAEMON_STOPPING: &str = "the daemon is stopping; retry once it is back";

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
    /// There is nothing left to retry in-process: `PendingLogin::run` has no
    /// retry path around its code exchange, so whatever the status, the only
    /// action available is starting the login again (CLI or TUI). Stated as the ABSENCE
    /// of a retry loop rather than as a fact about the code or the listener,
    /// because this correctly stops being true the moment someone adds one.
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
/// `TokenFailure::user_message` returns. Every other value-bearing arm holds a
/// `String`; what keeps THOSE honest is that each renders its own fixed
/// sentence below and interpolates the value as a profile name, so a body
/// passed there would read as an account name and nothing else. Sealing them
/// means a newtype only the callers can mint; worth doing if an arm ever needs
/// a runtime value that is not a name.
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
    /// The store held a login the refreshed chain does not own (an install
    /// that landed during the refresh, or before memory caught up), so the
    /// store kept it over the minted pair, and that login is itself expiring.
    /// Nothing failed: the retry spends the kept login.
    LoginChanged(String),
    /// CLA-ROLL: the rolling session token could not be written to (or restored
    /// into) the profile's sidecar. The chain itself is fine — what failed is
    /// the file in front of it, so this is a filesystem problem and not an
    /// account one.
    SidecarWriteFailed(String),
    /// CLA-ROLL: a live `clauth start` session is still holding this profile's
    /// ROTATING pair, because it started before the sidecar was armed. The
    /// session converges onto the sidecar in place on its own next swap poll,
    /// so the refusal is a short-retry transient, never a restart: measured, a
    /// rotation that lands first fails that session's next refresh with
    /// `invalid_grant` and can blank its own Keychain item, while the first
    /// spend's descendant survives.
    ///
    /// Distinct from [`Self::RotationLockUnavailable`] on purpose: nothing is
    /// locked and nothing is broken. The next step is the operator's, and it is
    /// specific enough that a generic retry hint would be wrong.
    LiveSessionOnRotatingChain(String),
    /// An UNFINISHED Claude Code background job still reads this profile's
    /// rotating pair, so rotating would strand it the same way a live session is
    /// stranded. Distinct from [`Self::LiveSessionOnRotatingChain`] because the
    /// next step is the operator's and different: no session is running, so
    /// waiting changes nothing — only `claude stop` (or letting the job
    /// finish) frees the chain.
    BackgroundJobOnRotatingChain(String),
    /// Claude Code's background-job store could not be read, so clauth cannot
    /// rule out an unfinished job on a rotatable tree. A GLOBAL store failure
    /// blocks every profile on macOS until it clears; a tree-local failure
    /// blocks only the profile whose store failed. Same fail-closed direction
    /// as the other two arms, and its own copy because none of their next steps
    /// apply: nothing is running to wait for and nothing is there to stop. The
    /// `store` is the dir the refusal names (the global store or the account's
    /// own `jobs/`).
    UnreadableJobStoreOnRotatingChain { profile: String, store: String },
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
    /// `retry in a moment`) or contradict it (a permissions check followed by
    /// `check your connection and retry`). [`Transient`]'s constructors enforce
    /// the pairing against this rather than leaving it to convention.
    ///
    /// New arms default to self-prescribing: an arm joins the list below only
    /// by an explicit edit, so an unclassified arm refuses every suffix-bearing
    /// retry loudly at construction instead of shipping the stutter.
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
                format!("could not write '{profile}' session token. Check permissions on ~/.clauth")
            }
            Self::LiveSessionOnRotatingChain(profile) => {
                format!(
                    "'{profile}' has a live clauth start session still on its rotating login; retry in a moment"
                )
            }
            Self::BackgroundJobOnRotatingChain(profile) => {
                format!(
                    "'{profile}' has a background session still on its rotating login; end it with claude stop to rotate"
                )
            }
            Self::UnreadableJobStoreOnRotatingChain { profile, store } => {
                format!(
                    "can't read claude code's background job list ({store}), so '{profile}' is not rotated; retry in a moment"
                )
            }
            Self::RotationLockHeld(profile) => {
                format!("'{profile}' has a token rotation in progress, retry in a moment")
            }
            Self::RollingGrantUnrecorded(profile) => {
                format!(
                    "'{profile}' usage chain has no recorded grant beyond the setup-token \
                     scopes, so a rolling bearer cannot be told from a mint. Run \
                     `clauth login {profile}` to record the chain's real grant"
                )
            }
            Self::SidecarMisfilled(profile) => {
                format!(
                    "'{profile}' session token holds a rotating pair and no live mint backup \
                     exists to heal it. Re-capture with `clauth login {profile} --setup-token`"
                )
            }
            Self::StateLockBusy(profile) => {
                format!(
                    "another clauth process holds ~/.clauth's state lock. '{profile}' left \
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
            Self::LoginChanged(profile) => {
                format!(
                    "'{profile}' login changed during its refresh and the new login expires \
                     soon. Retry in a moment"
                )
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
#[derive(Debug)]
pub(crate) struct Transient {
    cause: Cause,
    status: Option<u16>,
    retry: Retry,
}

impl Transient {
    /// # Panics
    ///
    /// If `cause` names its own next step and `retry` appends advice
    /// ([`Retry::Wait`], [`Retry::Connection`], [`Retry::Restart`]): the
    /// appended hint would duplicate or contradict the cause's own advice.
    /// Pair with [`Retry::Stated`] instead. Unreachable from every production
    /// site today — each passes a literal retry and none pairs a
    /// self-prescribing arm with a suffix-bearing retry — so a wrong pairing
    /// fails here rather than shipping a stutter.
    pub(crate) fn new(cause: Cause, retry: Retry) -> Self {
        Self::refuse_contradicting_suffix(&cause, &retry);
        Self {
            cause,
            status: None,
            retry,
        }
    }

    /// [`Self::new`] with an HTTP status. The same pairing rule applies: the
    /// status sits between the two clauses but does not un-stutter them.
    pub(crate) fn with_status(cause: Cause, status: u16, retry: Retry) -> Self {
        Self::refuse_contradicting_suffix(&cause, &retry);
        Self {
            cause,
            status: Some(status),
            retry,
        }
    }

    fn refuse_contradicting_suffix(cause: &Cause, retry: &Retry) {
        assert!(
            !cause.names_its_own_next_step() || matches!(retry, Retry::Stated),
            "cause {cause:?} names its own next step; retry {retry:?} would duplicate or \
             contradict it"
        );
    }

    fn suffix(&self) -> &'static str {
        match self.retry {
            Retry::Connection => ": check your connection and retry",
            Retry::Wait => ": retry in a moment",
            Retry::Stated => "",
            Retry::Restart => ": start the login again for a fresh code",
        }
    }

    /// Cause + next step, no status. TUI toasts and the MCP `reason`.
    pub(crate) fn text(&self) -> String {
        format!("{}{}", self.cause.text(), self.suffix())
    }

    /// Whether nothing failed: the refresh landed, a login change kept the
    /// store's login, and that login is expiring. Surfaces drop their failure
    /// head and failure tone for it and show [`Self::text`] alone.
    pub(crate) fn is_login_change(&self) -> bool {
        matches!(self.cause, Cause::LoginChanged(_))
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
/// CLI/MCP switch bail, the daemon tick log, the TUI switch toast, and —
/// through `oauth::third_party_dead_chain_copy`'s `None` case — the MCP
/// pre-flight's quarantine arm, the rotate toast and the quarantine's own log
/// line, wherever the profile neither serves its own inference nor is keyless
/// (`Profile::is_keyless`, or a proxy account with no key). `clauth
/// rolling-token`'s dead-chain bail takes that same `None` case but words its
/// own sentence, since it also has to say the arming did not happen.
pub(crate) fn login_expired(name: &crate::profile::ProfileName) -> Message {
    Message {
        head: format!("login for '{name}' has expired"),
        detail: Some(format!(
            "refresh token revoked or invalid: run `clauth login {name}`"
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
/// line, so the surfaces cannot spell one state two ways. A key the provider
/// refused renders [`api_key_rejected`] on those surfaces instead. A proxy
/// account (`proxy` is its runtime claim's service) takes its key only from a
/// login through its proxy, so the fix names that login instead.
pub(crate) fn third_party_keyless(
    name: &crate::profile::ProfileName,
    proxy: Option<&crate::proxy::Service>,
) -> String {
    format!(
        "profile has no api key: {name} (run `{}`)",
        key_fix_command(name, proxy)
    )
}

/// The command that re-captures `name`'s inference key: `--api-key`, or the
/// login through its proxy for a proxy account, the only login clauth takes
/// for one.
fn key_fix_command(
    name: &crate::profile::ProfileName,
    proxy: Option<&crate::proxy::Service>,
) -> String {
    match proxy {
        Some(service) => crate::actions::proxy_login_command(name.as_str(), service),
        None => format!("clauth login {name} --api-key <key>"),
    }
}

/// A third-party api key the provider's dead-key record names rejected
/// ([`crate::fallback::third_party_key_rejected`] is the predicate): the key is
/// the credential its sessions spend, so the fix re-captures it with
/// `--api-key`, the command that also lifts a keyless profile — a bare
/// `clauth login <name>` runs the browser flow and leaves the key untouched.
/// Rendered by the MCP pre-flight's key-rejected arm and by
/// `oauth::third_party_dead_chain_copy`'s callers. A proxy account's fix is
/// the login through its proxy, as in [`third_party_keyless`].
pub(crate) fn api_key_rejected(
    name: &crate::profile::ProfileName,
    proxy: Option<&crate::proxy::Service>,
) -> String {
    format!(
        "api key rejected: {name} (run `{}`)",
        key_fix_command(name, proxy)
    )
}

/// An OAuth account's setup token that the usage probe found refused
/// ([`crate::fallback::third_party_key_rejected`] is the predicate, split by
/// [`crate::usage::dead_login_is_setup_token`]): the token is the credential
/// its sessions spend, so the fix re-mints it with `--setup-token`. The
/// api-key sentence ([`api_key_rejected`]) names a credential the account
/// does not hold. Rendered by the MCP pre-flight's token-rejected arm.
pub(crate) fn setup_token_rejected(name: &crate::profile::ProfileName) -> String {
    format!("setup token rejected: {name} (run `clauth login {name} --setup-token`)")
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
/// session, not a key. The `[env]`-token half is guarded too (owner ruling
/// 2026-09-02): a profile with no usable api key — field or env-carried —
/// renders [`third_party_keyless`] instead. The Alibaba console half renders
/// [`third_party_dead_console`] when the verdict matches its current credential
/// AND a console was captured; a console-less Alibaba profile whose verdict
/// matches lands on this sentence instead, since "expired" would be false.
///
/// Three sites route through `oauth::third_party_dead_chain_copy`:
/// the shared arm's up-front quarantine bail, the manual-rotate toast,
/// and the quarantine's own `mark_auth_broken` log line.
/// `RollingArmRefusal::sentence`'s `DeadChainKeptMint` arm carries a fourth
/// dead-chain sentence and is deliberately NOT routed: `classify_armed_sidecar`'s
/// `chain_is_broken` comes from the arm's own gate, which cannot reach
/// `Broken` for a profile with a `base_url`, so a third-party branch there is
/// dead code.
/// The MCP pre-flight admits that target instead of refusing it, so it renders
/// nothing (owner ruling 2026-08-30).
/// The command is backticked to match [`third_party_keyless`], which renders
/// beside it on the same surfaces. A proxy
/// account's fix is the login through its proxy, as in [`third_party_keyless`].
pub(crate) fn third_party_dead_chain(
    name: &crate::profile::ProfileName,
    proxy: Option<&crate::proxy::Service>,
) -> String {
    format!(
        "stored OAuth chain is dead, its api key still works: {name} \
         (run `{}` to clear the quarantine)",
        key_fix_command(name, proxy)
    )
}

/// An Alibaba profile whose stored OAuth chain is dead while its console
/// session has expired too: the split state, named so the reader learns both
/// halves — the api key still serves inference, and one command restores the
/// console. `cmd_login` diverts a bare `clauth login <name>` on Alibaba to the
/// console capture flow, so the command is exactly that.
///
/// Rendered only by `oauth::third_party_dead_chain_copy`'s own-endpoint arm,
/// where the profile's stored `AuthExpired` verdict matches its current
/// credential fingerprint AND a console was actually captured: an Alibaba
/// verdict records a dead console session, never a dead key, so this sentence
/// replaces the dead-chain one the other providers render there. Without a
/// console the verdict means "never captured", where "expired" and "re-capture"
/// are both false, so that profile keeps [`third_party_dead_chain`].
///
/// The key clause claims what its sibling's does and is guarded no further: the
/// arm proves a well-formed key, or merely a non-empty `[env]` token, never a
/// live one, and an Alibaba verdict cannot speak to the key at all since its
/// usage fetch never sends one. The gate is also wider than the verdict:
/// `credential_fingerprint` hashes the api key as well as the console, so a key
/// change alone retires a still-true console verdict and this sentence stops
/// rendering. A running scheduler re-writes the verdict on its next tick;
/// `cmd_rolling_token` loads a config and fetches nothing, so with no daemon and
/// no TUI that window has no bound.
/// The command is backticked to match [`third_party_keyless`] and
/// [`third_party_dead_chain`] (owner ruling: house style).
pub(crate) fn third_party_dead_console(name: &crate::profile::ProfileName) -> String {
    format!(
        "console session expired, stored OAuth chain is dead: {name} \
         (run `clauth login {name}` to re-capture the console; the api key still serves inference)"
    )
}

/// A refresh that failed for a transient reason: this switch is refused but the
/// login is not quarantined. The next step comes from `err`'s own [`Retry`], so
/// a throttle is never told to check its connection. A login change failed
/// nothing, so its sentence stands alone under no failure head.
pub(crate) fn refresh_transient(name: &crate::profile::ProfileName, err: &Transient) -> Message {
    if err.is_login_change() {
        return Message {
            head: err.text(),
            detail: None,
        };
    }
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
    if err.is_login_change() {
        return refresh_transient(name, err);
    }
    Message {
        head: format!("could not refresh '{name}' before switching"),
        detail: Some(err.text_with_status()),
    }
}

/// [`refresh_transient_cli`] for `clauth __oauth-token`, which refreshes to print
/// a token rather than to switch, so the head drops "before switching".
pub(crate) fn refresh_transient_oauth_token(
    name: &crate::profile::ProfileName,
    err: &Transient,
) -> Message {
    if err.is_login_change() {
        return refresh_transient(name, err);
    }
    Message {
        head: format!("could not refresh '{name}'"),
        detail: Some(err.text_with_status()),
    }
}

/// A refusal over a live `~/.claude` login no stored profile is proven to own:
/// the headless switch on a diverged outgoing profile, and `clauth
/// __oauth-token` on an active profile whose live login it can't tie to itself.
pub(crate) fn unsaved_live_login(name: &str) -> String {
    format!("'{name}' has a login clauth hasn't saved, {RESOLVE_IN_TUI}")
}

/// The CLI switch over an unsaved live login whose outgoing account runs on a
/// long-lived token: that account's store cannot take an OAuth login, so the
/// switch does not offer to save it there.
pub(crate) fn long_lived_cannot_hold_login(name: &str) -> String {
    format!(
        "'{name}' runs on a long-lived token and cannot hold this login; save it as another \
         account first: `clauth capture <new name>`"
    )
}

/// `clauth __oauth-token` on an expired token it will not refresh itself (the
/// active profile, or a live session's own chain): the chain is usually fine,
/// so the fix is the `claude` run that refreshes it, not a re-login.
pub(crate) fn expired_token_refreshed_by_claude(name: &crate::profile::ProfileName) -> String {
    format!(
        "'{name}': its token has expired and a running `claude` owns its refresh, run `claude` \
         on it once and retry"
    )
}

/// The one spelling for "go fix this in the app". The surface is the `clauth`
/// TUI, never a bare "the TUI" (which reads as some other UI).
pub(crate) const RESOLVE_IN_TUI: &str = "resolve the divergence in the `clauth` TUI";

/// `message` without its closing `; {RESOLVE_IN_TUI} first` pointer, for a
/// surface where that pointer leads nowhere or is said elsewhere. `None` on a
/// message that does not close with it, which each caller answers its own way.
pub(crate) fn without_resolve_pointer(message: &str) -> Option<&str> {
    message.strip_suffix(&format!("; {RESOLVE_IN_TUI} first"))
}

/// The one spelling for a stored login that exists but cannot be read.
pub(crate) const STORED_LOGIN_UNREADABLE: &str = "its stored login could not be read";

/// The `s` a count needs: singular at one.
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
/// their own guard and reached three different words. `src/tui/render/services.rs`
/// still spells its own `just now`, deliberately — a pane's phrasing is its own
/// — and folding that one in is owed.
pub(crate) fn humanize_span(secs: u64) -> String {
    if secs == 0 {
        return "0s".to_string();
    }
    humanize_duration(secs as i64)
}

/// One grapheme's cells as the buffer lays them down
/// ([`CellWidth::cell_width`]): `unicode-width` plus one cell per halfwidth
/// katakana sound mark (`U+FF9E`/`U+FF9F`), a mark `Span::width` reports zero
/// where the buffer lays one down, so `ｶﾞ` draws 2.
pub(crate) fn grapheme_cell_width(symbol: &str) -> usize {
    usize::from(symbol.cell_width())
}

/// The terminal cells `s` occupies: each grapheme's [`grapheme_cell_width`]
/// summed over `styled_graphemes`, the measure the buffer draws by (control
/// graphemes dropped). The whole-string [`Span::width`] folds an Arabic
/// lam-alef ligature to one cell while the buffer draws it per grapheme as two,
/// and `chars().count()` spends one cell on a two-cell CJK glyph, so a value
/// sized by either overruns its budget.
pub(crate) fn grapheme_cells(s: &str) -> usize {
    Span::raw(s)
        .styled_graphemes(Style::default())
        .map(|g| grapheme_cell_width(g.symbol))
        .sum()
}

/// The width of one table column: the header or the widest cell under it,
/// counted in drawn cells, so a wide-script cell widens its own column instead
/// of shifting the ones after it.
pub(crate) fn col_width<'a>(header: &str, cells: impl Iterator<Item = &'a str>) -> usize {
    cells
        .map(grapheme_cells)
        .chain(std::iter::once(grapheme_cells(header)))
        .max()
        .unwrap_or(0)
}

/// `value` plus the spaces that bring it to `width` drawn cells: trailing for a
/// left-aligned column, leading for a right-aligned one. `format!`'s own width
/// pads by `char` count, which leaves a wide-script cell short by the extra
/// cells its glyphs draw.
pub(crate) fn pad_cells(value: &str, width: usize, right_align: bool) -> String {
    let fill = " ".repeat(width.saturating_sub(grapheme_cells(value)));
    if right_align {
        format!("{fill}{value}")
    } else {
        format!("{value}{fill}")
    }
}

/// Whether `s` fits a cap of `max` on both measures a cap bounds: the cells
/// [`grapheme_cells`] draws, and the characters — a char is at most four bytes,
/// so a character cap also bounds the bytes one grapheme can carry. The
/// characters are counted first: a string over the cap stops there, and the
/// cell walk then sees at most `max` characters.
fn cap_fits(s: &str, max: usize) -> bool {
    s.chars().nth(max).is_none() && grapheme_cells(s) <= max
}

/// The byte length of the longest prefix of `s` that fits `budget` on both
/// measures [`cap_fits`] bounds, every grapheme kept whole. A control
/// character is one piece that spends its character and no cell, so a caller's
/// line structure survives the cut and a run of controls stops at the char
/// budget; keeping control characters off a terminal stays the caller's job
/// ([`terminal_safe`], or the preview seam's own mapping). A control character
/// breaks a grapheme cluster wherever it sits, except that a carriage return
/// and the line feed right behind it are one cluster (UAX #29 GB3), so the two
/// go as one piece and no piece straddles a cluster.
fn cap_prefix_len(s: &str, budget: usize) -> usize {
    let (mut cells, mut chars, mut len) = (0, 0, 0);
    let mut segments = s.split_inclusive(char::is_control).peekable();
    while let Some(segment) = segments.next() {
        let control = segment.chars().next_back().filter(|c| c.is_control());
        let head = &segment[..segment.len() - control.map_or(0, char::len_utf8)];
        let mut head_len = 0;
        for grapheme in Span::raw(head).styled_graphemes(Style::default()) {
            let (w, n) = (
                grapheme_cell_width(grapheme.symbol),
                grapheme.symbol.chars().count(),
            );
            if cells + w > budget || chars + n > budget {
                return len + head_len;
            }
            cells += w;
            chars += n;
            head_len += grapheme.symbol.len();
        }
        len += head_len;
        if let Some(c) = control {
            let line_feed = c == '\r' && segments.peek().is_some_and(|next| *next == "\n");
            let cost = if line_feed { 2 } else { 1 };
            if chars + cost > budget {
                return len;
            }
            chars += cost;
            len += c.len_utf8();
            if line_feed {
                segments.next();
                len += '\n'.len_utf8();
            }
        }
    }
    len
}

/// Trailing-ellipsis truncation to a cap of `max` on both measures a terminal
/// lays text out by ([`cap_fits`]): the cells [`grapheme_cells`] draws, and the
/// characters, which also bound the bytes. A grapheme the content budget
/// (`max` less the 3-cell, 3-character `...`) cannot hold whole is left out,
/// and the cut stops at the first grapheme either measure cannot take. Control
/// characters are kept, spending their characters and no cell, so an over-cap
/// multi-line input keeps its line structure and a run of controls stops at
/// the char budget; keeping control characters off a terminal stays the
/// caller's job. At or under the cap the whole text comes back unchanged, and
/// a cap under 3 renders dots alone.
pub(crate) fn truncate(s: &str, max: usize) -> String {
    if cap_fits(s, max) {
        return s.to_string();
    }
    if max < 3 {
        return ".".repeat(max);
    }
    format!("{}...", &s[..cap_prefix_len(s, max - 3)])
}

/// `text` with each character rendered by `piece`, cut at whole pieces to a cap
/// of `max` on the escaped body. The body is measured by [`cap_fits`] — the
/// cells a terminal draws it in and its characters — and a piece the content
/// budget (`max` less the 3-cell `...`) cannot hold is left out, never split,
/// so an escape never loses its tail and a control character survives the cut
/// as its escape. The body stops being built at the first piece that would
/// carry it past `max` characters, that piece included so the cut sees the
/// grapheme it joins; nothing after it is built, which already proves the rest
/// of the text over the cap, so no body far over it is ever built whole, and
/// the fit and the cut both lie inside what was built. The kept text is
/// wrapped in a pair of double quotes when `quoted` (the two quote cells sit
/// outside `max`), `...` appended outside the quotes (at the end, bare) when
/// anything was cut; at or under the cap the whole text comes back, and a bound
/// under 3 cells renders dots alone.
pub(crate) fn truncate_escaped(
    text: &str,
    max: usize,
    piece: impl Fn(char) -> String,
    quoted: bool,
) -> String {
    let mut body = String::new();
    let mut ends = Vec::new();
    let mut chars = 0;
    let mut cut_short = false;
    for c in text.chars() {
        let escaped = piece(c);
        let escaped_chars = escaped.chars().count();
        if chars + escaped_chars > max {
            cut_short = true;
            body.push_str(&escaped);
            ends.push(body.len());
            break;
        }
        chars += escaped_chars;
        body.push_str(&escaped);
        ends.push(body.len());
    }
    let wrap = |s: &str| {
        if quoted {
            format!("\"{s}\"")
        } else {
            s.to_string()
        }
    };
    if !cut_short && cap_fits(&body, max) {
        return wrap(&body);
    }
    if max < 3 {
        return ".".repeat(max);
    }
    // Both callers' `piece` escapes every control character, so every character
    // of `body` reaches the grapheme walk and the cut lands on one of its
    // boundaries; backing off to the last whole piece keeps an escape intact.
    let fit = cap_prefix_len(&body, max - 3);
    let cut = ends
        .iter()
        .copied()
        .take_while(|&end| end <= fit)
        .last()
        .unwrap_or(0);
    format!("{}...", wrap(&body[..cut]))
}

/// Whether a character must not reach a terminal as itself: a control
/// character or a display-reordering one. The one predicate set
/// [`terminal_safe`] drops, the preview seam maps to a space and a delegate
/// reply escapes, so none of them can drift apart.
pub(crate) fn unsafe_on_terminal(c: char) -> bool {
    c.is_control() || reorders_display(c)
}

/// Untrusted text with its control and bidi formatting characters dropped: it
/// reaches a terminal, where an escape sequence could retitle it or hide a
/// line, and a bidi override could reorder the line to read as something it is
/// not.
pub(crate) fn terminal_safe(s: &str) -> String {
    s.chars().filter(|c| !unsafe_on_terminal(*c)).collect()
}

/// Strip ANSI CSI sequences (`ESC [ params/intermediates final`, params in
/// `0x20..=0x3F`, final in `0x40..=0x7E`) from `text`, so a reader gets the
/// text, not CSI bytes. Any byte outside those ranges before the
/// final byte ends the sequence without eating text: the `ESC` is kept and
/// scanning resumes after it, so a malformed CSI never swallows a line.
pub(crate) fn strip_ansi_csi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(esc) = rest.find('\x1b') {
        out.push_str(&rest[..esc]);
        let after = &rest[esc + 1..];
        if let Some(body) = after.strip_prefix('[') {
            match body
                .char_indices()
                .find(|(_, c)| ('@'..='~').contains(c) || !('\u{20}'..='\u{3f}').contains(c))
            {
                Some((at, c)) if ('@'..='~').contains(&c) => rest = &body[at + c.len_utf8()..],
                _ => {
                    // No final byte, or a non-parameter byte: not a CSI.
                    out.push('\x1b');
                    rest = after;
                }
            }
        } else {
            out.push('\x1b');
            rest = after;
        }
    }
    out.push_str(rest);
    out
}

/// Unicode's `Bidi_Control` property, all twelve codepoints: the embeddings and
/// overrides (`U+202A..=U+202E`), the isolates (`U+2066..=U+2069`), and the
/// three directional marks — `U+200E` LRM, `U+200F` RLM, and `U+061C` ALM, the
/// Arabic-script twin of RLM.
///
/// Spelled out rather than tested by general category, because the crate carries
/// no Unicode tables. The cost of spelling it out is that it is only closed
/// against the Unicode version it was written from; `Bidi_Control` has been
/// stable for many releases, and a new member would surface as a codepoint
/// reaching the terminal rather than as a wrong figure.
///
/// Legitimate right-to-left text needs none of them: the implicit bidi algorithm
/// renders a plain Arabic or Hebrew tail correctly on its own, measured on
/// both.
pub(crate) fn reorders_display(c: char) -> bool {
    matches!(
        c,
        '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}' | '\u{200e}' | '\u{200f}' | '\u{061c}'
    )
}

/// Whether a terminal may draw `c` as a blank cell: whitespace, `U+2800` BRAILLE
/// PATTERN BLANK, and the Hangul fillers `U+115F`, `U+3164` and `U+FFA0`. Width
/// tables disagree on the last two (glibc's `wcwidth` gives 0 for both, East
/// Asian Width calls them wide and halfwidth), so they count as blank: a text
/// holding only them shows nothing.
pub(crate) fn is_blank(c: char) -> bool {
    c.is_whitespace() || matches!(c, '\u{2800}' | '\u{115F}' | '\u{3164}' | '\u{FFA0}')
}

/// `Unicode 18.0.0` `DerivedCoreProperties.txt`: the
/// `Default_Ignorable_Code_Point` ranges, characters a terminal draws no glyph
/// for. Some of them, the Hangul fillers included, can occupy a blank cell.
pub(crate) fn draws_nothing(c: char) -> bool {
    matches!(
        c,
        '\u{00AD}'
            | '\u{034F}'
            | '\u{061C}'
            | '\u{115F}'..='\u{1160}'
            | '\u{17B4}'..='\u{17B5}'
            | '\u{180B}'..='\u{180F}'
            | '\u{200B}'..='\u{200F}'
            | '\u{202A}'..='\u{202E}'
            | '\u{2060}'..='\u{206F}'
            | '\u{3164}'
            | '\u{FE00}'..='\u{FE0F}'
            | '\u{FEFF}'
            | '\u{FFA0}'
            | '\u{FFF0}'..='\u{FFF8}'
            | '\u{1BCA0}'..='\u{1BCA3}'
            | '\u{1D173}'..='\u{1D17A}'
            | '\u{E0000}'..='\u{E0FFF}'
    )
}

/// Whether `text` shows nothing: blanks and `draws_nothing` characters only, a
/// soft hyphen included. Callers pass `terminal_safe` output.
pub(crate) fn shows_nothing(text: &str) -> bool {
    text.chars().all(|c| is_blank(c) || draws_nothing(c))
}

/// `text` with every character `keep` rejects written as its `\u{..}` escape.
pub(crate) fn escape_unless(text: &str, keep: impl Fn(char) -> bool) -> String {
    text.chars()
        .map(|c| {
            if keep(c) {
                c.to_string()
            } else {
                format!("\\u{{{:04x}}}", u32::from(c))
            }
        })
        .collect()
}

/// The account's plan: a fetched plan wins when it names one (a classified
/// tier or the server's own plan name), else the OAuth token's
/// `subscription_type` claim, else `None` when neither names one. A surface
/// renders its own no-data form on `None` rather than a bare "Claude" that
/// reads as a real plan.
pub(crate) fn account_tier(profile: &Profile, usage: Option<&UsageInfo>) -> Option<PlanInfo> {
    // A fetched plan wins, but an UNCLASSIFIED one is not an answer: fall
    // through the way `profile_json::tier_label` does, or this surface reads
    // "no data" while that one shows the token's tier for the very same account.
    let fetched = usage
        .and_then(|u| u.plan.as_ref())
        .filter(|p| p.is_classified())
        .cloned();
    fetched.or_else(|| {
        // No fetched plan yet — fall back to the OAuth token's subscription_type.
        let sub = profile
            .credentials
            .as_ref()
            .and_then(|c| c.claude_ai_oauth.as_ref())
            .and_then(|o| o.subscription_type.as_deref());
        match PlanTier::from_subscription_type(sub) {
            PlanTier::Unknown => None,
            tier => Some(PlanInfo {
                tier,
                ..PlanInfo::default()
            }),
        }
    })
}

/// Percent from API `f64`: at most two decimals, without trailing zeros.
pub(crate) fn format_pct(pct: f64) -> String {
    let rounded = format!("{pct:.2}");
    if rounded == "-0.00" {
        return "0%".to_string();
    }
    let trimmed = rounded.trim_end_matches('0').trim_end_matches('.');
    format!("{trimmed}%")
}

/// Absolute API amount: whole numbers render bare, fractions at two decimals →
/// `42`, `42.35`. The one shared spelling for a bar's `used / total` figures;
/// a surface-local twin of this is a drift, not a specialization.
pub(crate) fn format_amount(n: f64) -> String {
    if n.fract() == 0.0 {
        format!("{n:.0}")
    } else {
        format!("{n:.2}")
    }
}

/// A token threshold in its display form: exact millions as `{n}M` (`2M`),
/// whole thousands below a million as `{n}k` (`600k`), anything else plain.
/// The one rule behind the hook note, the Config row's custom-value append,
/// its hint, and the editor seed.
pub(crate) fn format_threshold_tokens(v: u64) -> String {
    if v.is_multiple_of(1_000_000) {
        format!("{}M", v / 1_000_000)
    } else if v < 1_000_000 && v.is_multiple_of(1000) {
        format!("{}k", v / 1000)
    } else {
        v.to_string()
    }
}

/// Group the integer part of non-negative `n` with `,` thousands separators,
/// keeping `decimals` fractional digits: `(12345.6, 2)` → `12,345.60`. The one
/// shared grouping helper behind every full-precision detail-pane count.
pub(crate) fn group_thousands(n: f64, decimals: usize) -> String {
    let s = format!("{n:.decimals$}");
    let (int, frac) = s.split_once('.').map_or((s.as_str(), ""), |(i, f)| (i, f));
    let digits = int.len();
    let mut out = String::with_capacity(digits + digits / 3 + 1 + frac.len());
    for (i, ch) in int.chars().enumerate() {
        if i > 0 && (digits - i) % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    if !frac.is_empty() {
        out.push('.');
        out.push_str(frac);
    }
    out
}

/// The local wall-clock date as `YYYY-MM-DD` — the contract's absolute-age
/// spelling (an ISO date, no time). [`local_stamp`] is this plus the time of
/// day. Returns `None` when the instant falls outside chrono's representable
/// range.
pub(crate) fn local_date(epoch: i64) -> Option<String> {
    let naive = DateTime::from_timestamp(epoch, 0)?
        .with_timezone(&Local)
        .naive_local();
    Some(format!(
        "{:04}-{:02}-{:02}",
        naive.year(),
        naive.month(),
        naive.day(),
    ))
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
        "{} {:02}:{:02}:{:02}",
        local_date(epoch)?,
        naive.hour(),
        naive.minute(),
        naive.second(),
    ))
}

// ── diagnostic chip words + start-walk rendering ────────────────────────────

/// Canonical `[ label ]` wording for the diagnostic states whose text pill
/// surfaces on more than one tab, and the `--explain` row verdicts. One source
/// so the same account state never wears two words on two tabs.
pub(crate) const DIAG_DISABLED: &str = "disabled";
pub(crate) const DIAG_CANCELED: &str = "canceled";
pub(crate) const DIAG_ORG_REFUSED: &str = "org refused";
/// The fix line under [`DIAG_ORG_REFUSED`], on the Fallback card and the Usage
/// card alike.
pub(crate) const FIX_ORG_REFUSED: &str =
    "its organization refuses this account, no login fixes that";
pub(crate) const DIAG_AUTH_BROKEN: &str = "auth broken";
pub(crate) const DIAG_KEY_REJECTED: &str = "key rejected";
pub(crate) const DIAG_TOKEN_REJECTED: &str = "token rejected";
pub(crate) const DIAG_LOGGED_OUT: &str = "logged out";

/// The word a dead usage credential (`FetchStatus::AuthExpired`) wears on the
/// Usage tab and in `clauth list`, by its cause. An account whose dead login is
/// its setup token ([`crate::usage::dead_login_is_setup_token`]) only gets the
/// verdict from the usage probe, so that token was refused. "expired" only
/// when a session actually lapsed; an account that
/// never stored one reaches this state too (a working api key does not
/// authenticate the usage gateway), and telling that operator something expired
/// sends them hunting for what to renew instead of to the login they have not
/// run. Past a refused setup token and a captured console, a non-Alibaba
/// profile has no session at all, so its verdict can only mean the api key
/// itself was rejected.
pub(crate) fn auth_expired_label(profile: &Profile) -> &'static str {
    if crate::usage::dead_login_is_setup_token(profile) {
        DIAG_TOKEN_REJECTED
    } else if profile.console.is_some() {
        "login expired"
    } else if profile.provider != Some(crate::providers::Provider::Alibaba) {
        DIAG_KEY_REJECTED
    } else {
        "login needed"
    }
}
pub(crate) const DIAG_BUDGET_SPENT: &str = "extra usage spent";
pub(crate) const DIAG_CREDIT_CAP: &str = "credit cap spent";
/// The legend names the whole state in one row, where the card's pill and its
/// faint suffix carry the two halves.
pub(crate) const DIAG_CREDIT_CAP_LEGEND: &str = "credit cap spent, other models ok";
/// A member on record as refusing a credit-billed request: the Usage card's
/// pill, its age trailing as a bare suffix, and the start walk's verdict,
/// followed by how long ago ([`credit_refused_label`]).
pub(crate) const DIAG_CREDITS_REFUSED: &str = "credits refused";
/// The qualifier a still-serving rung trails outside its brackets: a model's
/// spent week and a spent credit cap both leave the rest of the account usable.
pub(crate) const DIAG_OTHER_MODELS_OK: &str = "other models ok";
/// The `⊘` legend row's scoped-week sense: each legend row's full text lives in
/// one constant.
pub(crate) const DIAG_SCOPED_SPENT_LEGEND: &str = "one model's week spent, other models ok";
pub(crate) const DIAG_KICK: &str = "claude code blocked";
pub(crate) const DIAG_WEEKLY_SPENT: &str = "weekly spent";
pub(crate) const DIAG_WEEKLY_SOFT: &str = "past the weekly switch line, still serving";
/// The monthly-quota spellings of the two weekly pill words above: a
/// monthly-only member blocked by its marked 30d quota is never called
/// "weekly", whatever surface renders the block.
pub(crate) const DIAG_MONTHLY_SPENT: &str = "30d spent";
pub(crate) const DIAG_MONTHLY_SOFT: &str =
    "monthly quota past the weekly switch line, still serving";
pub(crate) const DIAG_STALE: &str = "stale data";
/// The Fallback pill for a member clauth cannot measure: it says what clauth
/// CANNOT DO, not what has not happened yet. "no usage yet" reads as "the first
/// poll is still coming", the opposite of this state, and it is what the
/// `--explain` age cell prints for an account whose first poll really is
/// coming. The verb is the one the add confirm and
/// the daemon line already use ("can't read ... usage"), so every surface says
/// one thing.
pub(crate) const DIAG_NO_USAGE: &str = "can't read usage";

/// A seconds age as the relative ladder (`4m ago`, `2h ago`, `3d ago`), open
/// ended into weeks. The TUI's `relative_age` (`tui/render/format.rs`) and the
/// CLI's session/device age cells call [`spoken_age`], which switches to the
/// local ISO date at 30 days.
pub(crate) fn humanize_age(secs: u64) -> String {
    let mins = secs / 60;
    let hours = mins / 60;
    let days = hours / 24;
    if secs < 60 {
        "just now".to_string()
    } else if mins < 60 {
        format!("{mins}m ago")
    } else if hours < 24 {
        format!("{hours}h ago")
    } else if days < 7 {
        format!("{days}d ago")
    } else {
        format!("{}w ago", days / 7)
    }
}

/// The 30-day line where [`spoken_age`] switches from the relative ladder to
/// the local ISO date. Shared so a stamp-paired cell drops its age half at the
/// same instant the age itself becomes a date (the full stamp already carries
/// it), and the start walk derives the same arm.
pub(crate) fn age_takes_a_date(age_secs: u64) -> bool {
    age_secs >= 30 * 24 * 3600
}

/// A point-in-time age as a person reads it: one relative unit under 30 days
/// (`4m ago`, `just now` under a minute), the local ISO date (`2026-04-12`) at
/// 30 days and beyond. `age_secs` is the elapsed seconds; `epoch` is the
/// instant's epoch seconds, used only for the date arm. The one shared
/// formatter behind the TUI's `relative_age` and the CLI's `clauth sessions` /
/// `clauth devices` age cells, so every person-facing age reads the same shape.
/// `None` when the instant falls outside chrono's representable range.
pub(crate) fn spoken_age(age_secs: u64, epoch: i64) -> Option<String> {
    if age_takes_a_date(age_secs) {
        local_date(epoch)
    } else {
        Some(humanize_age(age_secs))
    }
}

/// The one-line label for a start verdict, sharing the TUI's chip words where
/// one exists ([`crate::fallback::StartBlock`]).
pub(crate) fn start_block_label(block: &StartBlock) -> String {
    match block {
        StartBlock::Disabled => DIAG_DISABLED.to_string(),
        StartBlock::AuthBroken => DIAG_AUTH_BROKEN.to_string(),
        StartBlock::LoggedOut => DIAG_LOGGED_OUT.to_string(),
        StartBlock::Canceled => DIAG_CANCELED.to_string(),
        StartBlock::OrgRefused => DIAG_ORG_REFUSED.to_string(),
        StartBlock::KickRejected => DIAG_KICK.to_string(),
        StartBlock::NotOauth => "not an oauth account".to_string(),
        StartBlock::WeeklySpent { monthly } => {
            if *monthly {
                DIAG_MONTHLY_SPENT.to_string()
            } else {
                DIAG_WEEKLY_SPENT.to_string()
            }
        }
        StartBlock::WeeklySoft { pct, monthly } => format!(
            "{} {}",
            if *monthly { "30d" } else { "weekly" },
            format_pct(*pct)
        ),
        StartBlock::FiveHour { pct } => format!("5h {}", format_pct(*pct)),
        StartBlock::ScopedSpent { label, pct } => {
            format!("{label} {}, {DIAG_OTHER_MODELS_OK}", format_pct(*pct))
        }
        StartBlock::CreditCap => DIAG_CREDIT_CAP_LEGEND.to_string(),
        StartBlock::CreditRefused { at_ms } => credit_refused_label(*at_ms),
    }
}

/// [`DIAG_CREDITS_REFUSED`] with the refusal's age, `credits refused 2h ago`,
/// read as the age cell beside it reads its own ([`spoken_age`]): the date
/// from 30 days on.
pub(crate) fn credit_refused_label(at_ms: u64) -> String {
    let age_secs = crate::usage::now_ms().saturating_sub(at_ms) / 1000;
    let age = spoken_age(age_secs, (at_ms / 1000) as i64).unwrap_or_else(|| "-".to_string());
    format!("{DIAG_CREDITS_REFUSED} {age}")
}

fn start_verdict(row: &StartCandidate) -> String {
    match &row.block {
        Some(block) => start_block_label(block),
        None => "ok".to_string(),
    }
}

fn start_age_cell(row: &StartCandidate) -> String {
    match row.age {
        OauthAge::Dated(ms) => {
            let age_secs = ms / 1000;
            // The date arm is `now - age` (the reading's own instant, derived,
            // never a fresh fetch); `now` is the walk's reference instant.
            let cell = format!(
                "usage {}",
                spoken_age(age_secs, row.now - age_secs as i64).unwrap_or_else(|| "-".to_string())
            );
            if row.stale {
                format!("{cell} (stale)")
            } else {
                cell
            }
        }
        OauthAge::Undated => "usage undated (stale)".to_string(),
        // The default absence is "not read yet". When the walk found no fetch
        // leg will ever read this member's usage, its answer rides the row and
        // the cell names why instead, in the fallback pill's own words where it
        // has them.
        OauthAge::Absent => match row.no_poll {
            Some(NoPollReason::CannotMeasure) => DIAG_NO_USAGE.to_string(),
            Some(NoPollReason::ApiKeyOnly) => "api key, no usage windows".to_string(),
            None => "no usage yet".to_string(),
        },
    }
}

/// The member rows of a `--explain` start walk, one line per chain member in
/// chain order: `*` for the pick, then the name and verdict each padded to the
/// column's widest in terminal cells (a CJK name keeps its column), then the
/// cache-age cell. Pure, so the byte layout is pinned without capturing stdout.
pub(crate) fn render_start_walk(rows: &[StartCandidate], pick: Option<usize>) -> String {
    let name_w = rows
        .iter()
        .map(|r| grapheme_cells(r.name.as_str()))
        .max()
        .unwrap_or(0);
    let verdict_w = rows
        .iter()
        .map(|r| grapheme_cells(&start_verdict(r)))
        .max()
        .unwrap_or(0);
    rows.iter()
        .enumerate()
        .map(|(i, r)| {
            let marker = if Some(i) == pick { '*' } else { ' ' };
            format!(
                "{marker} {}  {}   {}",
                pad_cells(r.name.as_str(), name_w, false),
                pad_cells(&start_verdict(r), verdict_w, false),
                start_age_cell(r),
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn start_demand_suffix(demand: &[String]) -> String {
    if demand.is_empty() {
        String::new()
    } else {
        format!(" for {}", demand.join(" + "))
    }
}

/// The `--explain` first line for a picked name.
pub(crate) fn start_pick_line(name: &str, demand: &[String]) -> String {
    format!("would start on '{name}'{}", start_demand_suffix(demand))
}

/// The stderr line a real `--auto` launch prints before `start::run`.
pub(crate) fn start_launch_line(name: &str, demand: &[String]) -> String {
    format!(
        "clauth: starting on '{name}'{}",
        start_demand_suffix(demand)
    )
}

/// The `--auto` no-member refusal: the first line plus the explain rows
/// rendered with no `*` on any of them. `rows` is never empty: its one caller
/// refuses a chain with no resolved member first.
pub(crate) fn start_refusal(demand: &[String], rows: &[StartCandidate]) -> String {
    format!(
        "--auto found no chain member with headroom{}\n{}",
        start_demand_suffix(demand),
        render_start_walk(rows, None)
    )
}

/// The `--auto` refusal when the fallback chain names no configured profile at
/// all: every chain name, in chain order, so the user sees the chain to repair
/// rather than a headroom verdict on members that do not exist. No explain
/// rows exist to print — nothing in the chain was judged. A chain entry is
/// never re-validated on load, so every character a profile name cannot hold
/// prints as its `\u{..}` escape: the line cannot drive or reorder the
/// terminal, and a dangling name never reads as another, valid one. An empty
/// entry prints as `""` so it stays visible.
pub(crate) fn start_missing_refusal(names: &[ProfileName]) -> String {
    let names = names
        .iter()
        .map(|name| {
            if name.is_empty() {
                "\"\"".to_owned()
            } else {
                escape_unless(name, crate::actions::is_name_char)
            }
        })
        .collect::<Vec<_>>()
        .join(", ");
    format!("--auto found no chain member that exists ({names})")
}

// ── Parse-position diagnostics ─────────────────────────────────────────────
//
// A parse error of a file clauth reads can carry the parser's own message,
// which quotes the offending source line (toml) or the offending value
// (serde_json), and that text can hold a credential. Every surface therefore
// renders only the position — `failed to parse <label> at line L, column C`,
// `failed to parse <label> at line L` for a json error at column 0, or the
// bare `failed to parse <label>` for an empty file (an end-of-input error at
// line 1, column 0), an io failure, a `from_value` error (which carries line
// 0), or a span-less toml error — so a secret in the file never reaches a log
// line or an error print. The json position is the parser's own line and
// column; the toml column is a char count from the line start.

/// A parse position to render: a 1-based line and column, or a line alone
/// when the error names a line but no column. `Copy` so callers pass it by
/// value like the `(usize, usize)` pair it replaces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ParsePosition {
    /// A 1-based line and column.
    LineColumn { line: usize, column: usize },
    /// A 1-based line alone — a serde_json error with `column() == 0`, which
    /// is never a real 1-based column.
    Line(usize),
}

/// The 1-based `(line, column)` of a byte `start` in `text`, counted in chars
/// from the line start. `None` when `start` falls past the end of `text` or
/// sits off a UTF-8 char boundary. The shared core the toml and toml_edit
/// helpers call, so the column math lives once. toml's own column also counts
/// chars from the line start, but falls back to the byte offset from the line
/// start when the char at the span's start is multi-byte; the two differ there
/// after multi-byte text, and at EOF after a trailing newline.
pub(crate) fn toml_position_from_span(text: &str, start: usize) -> Option<ParsePosition> {
    let head = text.get(..start)?;
    let line = head.matches('\n').count() + 1;
    let column = match head.rfind('\n') {
        Some(nl) => head[nl + 1..].chars().count() + 1,
        None => head.chars().count() + 1,
    };
    Some(ParsePosition::LineColumn { line, column })
}

/// The 1-based `(line, column)` of a `toml::de::Error`, via
/// [`toml_position_from_span`]. `None` when the error carries no span (a typed
/// deserialize over a `toml::Value`).
pub(crate) fn toml_parse_position(text: &str, e: &toml::de::Error) -> Option<ParsePosition> {
    toml_position_from_span(text, e.span()?.start)
}

/// The position of a `serde_json::Error`: a 1-based line and column, a line
/// alone when the error reports `column() == 0` (a whole-value type error or a
/// file torn right after a newline), or `None` when there is nothing to name —
/// an io failure, a `line() == 0` error, or an unexpected end of input at line
/// 1 column 0 (an empty file).
pub(crate) fn json_parse_position(e: &serde_json::Error) -> Option<ParsePosition> {
    if e.line() == 0 {
        return None;
    }
    match e.classify() {
        serde_json::error::Category::Io => None,
        // An unexpected end of input at line 1 column 0 is an empty file: no
        // line has content to name, so the bare frame stays bare. A file torn
        // right after a newline ends at column 0 on line >= 2 and names the
        // line.
        serde_json::error::Category::Eof if e.line() == 1 && e.column() == 0 => None,
        _ if e.column() == 0 => Some(ParsePosition::Line(e.line())),
        _ => Some(ParsePosition::LineColumn {
            line: e.line(),
            column: e.column(),
        }),
    }
}

/// The position of a `serde_json::Error` in a network body — never a file:
/// a body names serde's line whenever serde has one, so an empty reply (an
/// end-of-input error at line 1, column 0) renders `line 1` where
/// [`json_parse_position`] keeps the empty-file carve-out and stays bare. Every
/// other error renders exactly as [`json_parse_position`] renders it.
pub(crate) fn json_body_position(e: &serde_json::Error) -> Option<ParsePosition> {
    json_parse_position(e).or_else(|| (e.line() > 0).then_some(ParsePosition::Line(e.line())))
}

/// The shared position fragment — `line L, column C` or `line L` — or `None`
/// when there is no position to name.
pub(crate) fn parse_position(position: Option<ParsePosition>) -> Option<String> {
    position.map(|pos| match pos {
        ParsePosition::LineColumn { line, column } => format!("line {line}, column {column}"),
        ParsePosition::Line(line) => format!("line {line}"),
    })
}

/// A fresh `failed to parse <label>` error carrying no parser error as its
/// source: every surface printing `{e}`/`{e:#}` renders the position and
/// nothing else (`{e:?}` can append a backtrace under `RUST_BACKTRACE`).
pub(crate) fn parse_error(
    label: impl std::fmt::Display,
    position: Option<ParsePosition>,
) -> anyhow::Error {
    match parse_position(position) {
        Some(pos) => anyhow::anyhow!("failed to parse {label} at {pos}"),
        None => anyhow::anyhow!("failed to parse {label}"),
    }
}

/// A sentence-framed parse refusal — `<frame> (line L, column C)`, or
/// `<frame> (line L)` for a line-only position — as a fresh, source-free
/// error, for the sites whose own copy states the failure in a sentence rather
/// than the `failed to parse <label>` frame. A `None` position renders the
/// bare frame.
pub(crate) fn parse_refusal(
    frame: impl std::fmt::Display,
    position: Option<ParsePosition>,
) -> anyhow::Error {
    match parse_position(position) {
        Some(pos) => anyhow::anyhow!("{frame} ({pos})"),
        None => anyhow::anyhow!("{frame}"),
    }
}

#[cfg(test)]
#[path = "../tests/inline/format.rs"]
mod tests;
