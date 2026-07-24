//! The daemon's per-tick work, extracted from [`super::Daemon::run`]'s loop so it
//! is drivable in a test without the `sleep`/watchdog wrapper (TECH-5).
//!
//! [`Daemon::tick`](super::Daemon::tick) is one iteration of the run loop:
//! reload external config changes, execute any queued auto-switch / switch-off /
//! config edit, then rewrite `status.json`. The four drains and the token-list
//! rebuild live here; `write_status` and the loop scaffolding stay in `mod.rs`.
//! `tests/inline/daemon_mod.rs` pins each drain's current behavior against this
//! seam — this file changes NO runtime behavior versus the inlined loop body.

use std::collections::VecDeque;
use std::sync::atomic::Ordering;

use crate::actions::{switch_off, switch_profile};
use crate::fallback_config;
use crate::logline::logline;
use crate::profile::{load_config, reload_fingerprint};
use crate::usage::{
    Origin, PendingSwitchEntry, collect_third_party_entries, collect_tokens, is_idle, now_ms,
};

use super::{
    ConfigOp, LastError, LastSwitch, SwitchBackoff, active_diverged_unsaved, switch_backoff_ms,
};

/// Who the live `~/.claude/.credentials.json` login provably belongs to
/// (see `Daemon::follow_live_login`).
enum LiveOwner {
    Sibling(String),
    ActiveItself,
    Unknown(UnknownReason),
}

/// Why the live login couldn't be attributed — the reasons route to different
/// remedies, so collapsing them (the pre-RESCUE-1 behavior) turned a transient
/// probe outage into a permanent "resolve in the TUI" wedge.
enum UnknownReason {
    /// The probe PROVED an account that matches no stored anchor: a login
    /// someone made on purpose, alive and unowned — a human decision.
    ForeignAccount,
    /// The endpoint rejected the live access token (dead) — candidate for the
    /// dead-login rescue, pending the refresh leg's confirmation.
    AccessDead,
    /// Nothing proven either way: no access token, probe skipped (backoff), or
    /// a transport/throttle/shape failure. Retry later; never memoize.
    Unproven,
}

/// How long the follow/rescue network tier backs off after a round that proved
/// nothing (probe outage, throttle, transient refresh failure). Long enough to
/// ride out a throttling storm, short enough that a genuinely dead live login
/// is rescued within the half-hour instead of wedging for good.
const FOLLOW_PROBE_RETRY_MS: u64 = 30 * 60 * 1000;

/// The injected refresh-probe shape shared by `follow_live_login_with` and
/// `rescue_dead_live_login`: `oauth::refresh_result` in production, a closure
/// in tests.
type RefreshProbe<'a> =
    &'a dyn Fn(
        &str,
        Option<&str>,
    ) -> std::result::Result<crate::oauth::TokenResponse, crate::oauth::RefreshError>;

/// Map a switch [`Origin`] to the `status.json` `last_switch.trigger` string (TECH-8).
fn origin_trigger(origin: Origin) -> &'static str {
    match origin {
        Origin::User => "user",
        Origin::Scheduler => "scheduler",
    }
}

/// Profiles whose STORED access tokens are byte-identical, as
/// `(first-holder, duplicate)` pairs in profile order. No legitimate flow
/// produces this state — every pair means a capture wrote one profile's chain
/// over another's (see `Daemon::warn_duplicate_logins`).
pub(super) fn duplicate_login_pairs(config: &crate::profile::AppConfig) -> Vec<(String, String)> {
    let mut seen: Vec<(&str, &str)> = Vec::new(); // (access token, first holder)
    let mut pairs = Vec::new();
    for p in &config.profiles {
        let Some(tok) = p.access_token() else {
            continue;
        };
        if tok.is_empty() {
            continue;
        }
        match seen.iter().find(|(t, _)| *t == tok) {
            Some((_, first)) => pairs.push(((*first).to_string(), p.name.as_str().to_string())),
            None => seen.push((tok, p.name.as_str())),
        }
    }
    pairs
}

/// CAP-2: profiles whose identity ANCHORS (`account_id.json`) name the same
/// account, as `(first-holder, duplicate)` pairs in profile order. Catches
/// what the byte-identical check can't: two DIFFERENT token chains minted for
/// one account (the 2026-07-12 recurrence — a browser re-login into the wrong
/// account), which double-polls it exactly like a copied chain does. Anchors
/// only move through sanctioned captures and the usage fetcher's backfill, so
/// an equal pair is evidence, not coincidence.
/// Takes profile NAMES (not the config) so the caller can snapshot them under
/// the config lock and run the per-profile disk reads AFTER releasing it — a
/// stalled filesystem must never hold rank-Config against the fetcher threads.
pub(super) fn duplicate_account_pairs(names: &[String]) -> Vec<(String, String)> {
    let mut seen: Vec<(&str, &str)> = Vec::new(); // (account uuid, first holder)
    let mut pairs = Vec::new();
    let anchors: Vec<(&str, Option<String>)> = names
        .iter()
        .map(|name| {
            (
                name.as_str(),
                crate::profile_cache::load_profile_cache::<String>(
                    name,
                    crate::profile_cache::ACCOUNT_ID_CACHE_FILE,
                ),
            )
        })
        .collect();
    for (name, uuid) in &anchors {
        let Some(uuid) = uuid.as_deref().map(str::trim) else {
            continue;
        };
        // A whitespace-only anchor is shape drift, not an identity — two
        // blanks comparing equal must never pair (fetch_account_uuid contract).
        if uuid.is_empty() {
            continue;
        }
        match seen.iter().find(|(u, _)| *u == uuid) {
            Some((_, first)) => pairs.push(((*first).to_string(), (*name).to_string())),
            None => seen.push((uuid, name)),
        }
    }
    pairs
}

/// The account-anchor pairs worth their own logline: pairs already named by
/// the byte-identical token detector are dropped (a copied chain is also
/// anchor-identical once the backfill runs — one report per pair, under the
/// sharper token message). Both detectors emit `(first-holder, duplicate)` in
/// profile order, so equality is exact — no reordered-pair miss.
pub(super) fn account_only_pairs(
    token_pairs: &[(String, String)],
    account_pairs: Vec<(String, String)>,
) -> Vec<(String, String)> {
    account_pairs
        .into_iter()
        .filter(|p| !token_pairs.contains(p))
        .collect()
}

impl super::Daemon {
    /// One iteration of the run loop: reload external config, drain the queued
    /// auto-switch / switch-off / config edits, then rewrite `status.json`.
    /// Called each tick by [`super::Daemon::run`] (and directly by the inline
    /// tests). The initial `status.json` write and the watchdog heartbeat stay in
    /// `run`, so this method is exactly the observable per-tick work.
    pub(super) fn tick(&mut self) {
        self.reload_if_changed();
        self.follow_live_login();
        self.codex_follow_live();
        self.warn_duplicate_logins();
        self.drain_pending_switch();
        self.drain_pending_switch_off();
        self.drain_config_ops();
        self.write_status();
    }

    /// CDX-1 codex follow (docs/codex-support/PLAN.md §0.4): keep the stored
    /// snapshots honest against the live `~/.codex/auth.json`, one direction
    /// only — this NEVER writes the live file (only a switch does). Tiers:
    /// adopt-back a rotated chain into its owning profile's store; sync the
    /// active-codex marker when the live login belongs to a different stored
    /// profile (a `codex login`/manual swap outside clauth); log-once (memo)
    /// for anything it must leave alone (foreign / anchorless / unparseable).
    /// Runs under `with_state_lock` so it serializes with a concurrent
    /// switch's store→live install (T6 lock discipline).
    fn codex_follow_live(&mut self) {
        enum Step {
            /// Nothing to do (no live file / logged-out shell / clean state).
            Quiet,
            /// Live belongs to a stored profile; store and marker are now true.
            Owned {
                owner: String,
                adopted: bool,
                synced_fp: Option<crate::profile::ReloadFingerprint>,
            },
            /// Live must be left alone; log once per distinct state.
            LeaveAlone { key: u64, why: String },
        }

        #[allow(
            clippy::expect_used,
            reason = "config mutex poisoning is unrecoverable"
        )]
        let mut cfg = self.config.lock().expect("config poisoned");
        // Claude-only installs never pay for a live read.
        if !cfg.profiles.iter().any(|p| p.is_codex()) {
            return;
        }

        let step = crate::lock::with_state_lock(|| -> anyhow::Result<Step> {
            let Some(bytes) = crate::codex::read_live()? else {
                return Ok(Step::Quiet);
            };
            let Ok(live) = crate::codex::CodexAuthFile::parse(&bytes) else {
                return Ok(Step::LeaveAlone {
                    key: 1,
                    why: "is unparseable — leaving it alone".to_string(),
                });
            };
            if !live.has_login() {
                return Ok(Step::Quiet); // logged-out shell: nothing to sync
            }
            if live.account_id().is_none() {
                return Ok(Step::LeaveAlone {
                    key: live.fingerprint().unwrap_or(2),
                    why: "carries no account identity — leaving it alone".to_string(),
                });
            }
            let candidates: Vec<(String, Vec<u8>)> = cfg
                .profiles
                .iter()
                .filter(|p| p.is_codex())
                .filter_map(|p| {
                    let stored = crate::codex::read_profile_auth(&p.name).ok().flatten()?;
                    Some((p.name.to_string(), stored))
                })
                .collect();
            let owner = crate::codex::live_owner(
                &live,
                candidates.iter().map(|(n, b)| (n.as_str(), b.as_slice())),
            );
            let Some(owner) = owner else {
                return Ok(Step::LeaveAlone {
                    key: live.fingerprint().unwrap_or(3),
                    why: "matches no stored profile — leaving it alone (capture it with \
                          `clauth login <name> --codex`)"
                        .to_string(),
                });
            };
            let adopted = candidates
                .iter()
                .find(|(n, _)| *n == owner)
                .is_some_and(|(_, stored)| stored[..] != bytes[..]);
            if adopted {
                crate::codex::write_profile_auth(&owner, &bytes)?;
            }
            // Sync the marker while still under the flock, and hand the fresh
            // profiles.toml mtime out so the next reload_if_changed doesn't
            // treat our own write as an external edit.
            let synced_fp = if !cfg.is_active_codex(&owner) {
                let target = owner.clone();
                // Wholesale re-sync (mirrors the claude follow at its
                // marker write): `merged` is the freshest on-disk state plus
                // our delta, and we adopt this write's fingerprint below —
                // copying one field back would drop a same-tick external edit
                // until an unrelated later write re-triggered the reload.
                cfg.state = crate::profile::update_app_state(move |s| {
                    s.active_codex_profile = Some(target.as_str().into());
                })?;
                Some(reload_fingerprint())
            } else {
                None
            };
            Ok(Step::Owned {
                owner,
                adopted,
                synced_fp,
            })
        });
        drop(cfg);

        match step {
            Ok(Step::Quiet) => self.codex_follow_memo = None,
            Ok(Step::Owned {
                owner,
                adopted,
                synced_fp,
            }) => {
                self.codex_follow_memo = None;
                if adopted {
                    logline!("clauth daemon: adopted codex's refreshed login back into '{owner}'");
                }
                if let Some(fp) = synced_fp {
                    self.last_reload_fp = fp;
                    logline!(
                        "clauth daemon: the live codex login belongs to '{owner}' — marking \
                         it codex-active"
                    );
                }
            }
            Ok(Step::LeaveAlone { key, why }) => {
                if self.codex_follow_memo != Some(key) {
                    logline!("clauth daemon: the live codex login {why}");
                    self.codex_follow_memo = Some(key);
                }
            }
            Err(e) => {
                use std::hash::{Hash, Hasher};
                let mut h = std::collections::hash_map::DefaultHasher::new();
                e.to_string().hash(&mut h);
                let key = h.finish();
                if self.codex_follow_memo != Some(key) {
                    logline!("clauth daemon: codex follow failed: {e}; retrying");
                    self.codex_follow_memo = Some(key);
                }
            }
        }
    }

    /// CAP-1 tripwire: two profiles storing byte-identical access tokens means
    /// one was captured over with the other's chain — from that moment the
    /// pair double-polls ONE account (a rate-limit pin that looks like an API
    /// outage) while the clobbered profile's own account is orphaned. Nothing
    /// legitimate produces this state, so name it the moment it appears
    /// (memoized: once per distinct duplicate set, not per tick).
    fn warn_duplicate_logins(&mut self) {
        use std::hash::{Hash, Hasher};
        // Snapshot under the lock (in-memory only: tokens live in config, the
        // names are cheap clones); the per-profile anchor-file READS run after
        // release so slow disk never holds rank-Config against the fetchers.
        let (token_pairs, names) = {
            #[allow(clippy::expect_used, reason = "mutex poisoning is unrecoverable")]
            let cfg = self.config.lock().expect("config poisoned");
            let token_pairs = duplicate_login_pairs(&cfg);
            let names: Vec<String> = cfg
                .profiles
                .iter()
                .map(|p| p.name.as_str().to_string())
                .collect();
            (token_pairs, names)
        };
        let account_pairs = account_only_pairs(&token_pairs, duplicate_account_pairs(&names));
        let memo = if token_pairs.is_empty() && account_pairs.is_empty() {
            None
        } else {
            let mut h = std::collections::hash_map::DefaultHasher::new();
            (&token_pairs, &account_pairs).hash(&mut h);
            Some(h.finish())
        };
        if memo == self.dup_memo {
            return;
        }
        self.dup_memo = memo;
        for (first, second) in &token_pairs {
            logline!(
                "clauth daemon: profiles '{first}' and '{second}' hold the SAME login — \
                 one was captured over the other's account and they now double-poll it; \
                 re-run `clauth login` on the one that lost its own account"
            );
        }
        for (first, second) in &account_pairs {
            // Name the account when its email half is cached — "SAME ACCOUNT
            // (user@x)" turns the warning from a puzzle into an instruction.
            let email = crate::profile_cache::load_profile_cache::<String>(
                first,
                crate::profile_cache::ACCOUNT_EMAIL_CACHE_FILE,
            )
            .map(|e| format!(" ({e})"))
            .unwrap_or_default();
            logline!(
                "clauth daemon: profiles '{first}' and '{second}' are anchored to the \
                 SAME ACCOUNT{email} under different tokens — a re-login minted the \
                 wrong account and they now double-poll it; re-run `clauth login` on \
                 the one that lost its own account (with the browser logged into THAT \
                 account)"
            );
        }
    }

    /// Follow Claude Code to a sibling account (unattended sibling-divergence
    /// self-heal). When the ACTIVE profile's live link is Diverged and the
    /// live login PROVABLY belongs to a different stored profile, clauth's
    /// bookkeeping is simply behind reality — CC is already using that
    /// account (a `/login` into a known sibling, or a switch that
    /// half-landed). Capture the live pair into its own profile and make
    /// that profile active: no Keychain write, no live-file write, nothing
    /// logged out. This is what un-wedges the "deferring switch … unsaved
    /// credentials" loop without a human in the TUI.
    ///
    /// Proof of ownership, strongest first — the same bar as the adopt path,
    /// NOT the TUI's local `~/.claude.json` hint (unattended capture into a
    /// profile must never mis-attribute a login):
    ///   1. exact token equality with a sibling's stored pair (free, local);
    ///   2. the live token's account uuid fetched over the network matching
    ///      the sibling's cached identity anchor.
    ///
    /// A login matching the ACTIVE profile's own anchor is the adopt path's
    /// domain (same account, fresher pair) — left alone here. A login the
    /// endpoint CONFIRMS dead is the rescue's domain (RESCUE-1,
    /// [`Self::rescue_dead_live_login`]): a dead pair protects nothing, so
    /// parking it on a human wedges every switch behind a TUI nobody opens
    /// while the running `claude` stays signed out (observed 2026-07-14: a
    /// probe outage misclassified the active's own rotation as foreign, the
    /// live lineage died, and the daemon deferred switches for a day).
    /// Anything genuinely unprovable is left to the TUI, logged once per
    /// unique login; probe outages retry on a timer instead of memoizing the
    /// failure against the login.
    fn follow_live_login(&mut self) {
        let cfg = self.config.clone();
        let gate = move |name: &str| {
            crate::oauth::ensure_installable(&cfg, name, crate::oauth::refresh_result)
        };
        self.follow_live_login_with(
            &|tok| crate::usage::probe_account_identity(tok),
            &|rt, scopes| crate::oauth::refresh_result(rt, scopes),
            &gate,
        );
    }

    /// [`Self::follow_live_login_inner`] plus the RESCUE-2b durability wrap:
    /// any change to the memo/backoff pair is persisted so a daemon restart
    /// (launchd respawn, `pkill` deploy) can't void an armed anti-storm window
    /// or re-probe a memoized foreign login on every boot.
    pub(super) fn follow_live_login_with(
        &mut self,
        identity: &dyn Fn(&str) -> crate::usage::IdentityProbe,
        refresh: RefreshProbe,
        install_gate: &dyn Fn(&str) -> crate::oauth::AuthGate,
    ) {
        let before = (self.follow_memo, self.follow_retry_at);
        self.follow_live_login_inner(identity, refresh, install_gate);
        if (self.follow_memo, self.follow_retry_at) != before {
            super::save_follow_state(super::FollowState {
                memo: self.follow_memo,
                retry_at: self.follow_retry_at,
            });
        }
    }

    fn follow_live_login_inner(
        &mut self,
        identity: &dyn Fn(&str) -> crate::usage::IdentityProbe,
        refresh: RefreshProbe,
        install_gate: &dyn Fn(&str) -> crate::oauth::AuthGate,
    ) {
        use crate::claude::{LinkState, classify_credentials_link};

        let Some(active) = ({
            #[allow(clippy::expect_used, reason = "mutex poisoning is unrecoverable")]
            let cfg = self.config.lock().expect("config poisoned");
            cfg.state.active_profile.as_deref().map(str::to_string)
        }) else {
            self.follow_memo = None;
            return;
        };
        if !matches!(classify_credentials_link(&active), Ok(LinkState::Diverged)) {
            self.follow_memo = None;
            return;
        }
        // First login on a credential-less profile stays the TUI's
        // silent-adopt domain.
        if crate::claude::is_first_login(&active).unwrap_or(false) {
            return;
        }
        let fingerprint = crate::claude::live_credentials_fingerprint();
        if fingerprint.is_none() {
            // No access token in the live file. Timer-gated as a class so a
            // Broken/Transient stored chain doesn't re-run the install gate
            // (or re-log) every tick.
            if crate::usage::now_ms() < self.follow_retry_at {
                return;
            }
            // Not a login at all: CC's logged-out SHELL (blanked tokens,
            // `expiresAt: 0` — written when its own refresh dies), or a file
            // with no OAuth block left. It still classifies Diverged, so
            // before RESCUE-1b it wedged every switch behind a TUI decision
            // about NOTHING (observed 2026-07-15). Nothing to probe, nothing
            // to preserve — reclaim on the spot, gated like every reclaim.
            let empty_shell = || {
                matches!(
                    crate::claude::read_claude_credentials(),
                    Ok(Some(live)) if crate::claude::live_login_is_empty(&live)
                )
            };
            if empty_shell() {
                self.reclaim_live_slot(
                    &active,
                    "is a logged-out shell (no tokens left)",
                    &empty_shell,
                    install_gate,
                );
                return;
            }
            // RESCUE-2b: blank access token but a refresh token PRESENT — a
            // torn write or an exotic client state. When that refresh token
            // byte-matches the active profile's own stored chain, the file is
            // a degraded copy of the SAME chain and relinking loses nothing:
            // reclaim. Anything else is left alone — but visibly, on the
            // timer, instead of the silent per-tick no-op this state used to
            // be (it never reached the tier-1 equality check below).
            let live_refresh = crate::claude::read_claude_credentials()
                .ok()
                .flatten()
                .and_then(|c| c.refresh_token().map(str::to_string))
                .filter(|t| !t.is_empty());
            let stored_refresh = {
                #[allow(clippy::expect_used, reason = "mutex poisoning is unrecoverable")]
                let cfg = self.config.lock().expect("config poisoned");
                cfg.find(&active)
                    .and_then(|p| p.refresh_token().map(str::to_string))
                    .filter(|t| !t.is_empty())
            };
            match (live_refresh, stored_refresh) {
                (Some(live_rt), Some(stored_rt)) if live_rt == stored_rt => {
                    let still_degraded = move || {
                        matches!(
                            crate::claude::read_claude_credentials(),
                            Ok(Some(l)) if l.access_token().filter(|t| !t.is_empty()).is_none()
                                && l.refresh_token() == Some(live_rt.as_str())
                        )
                    };
                    self.reclaim_live_slot(
                        &active,
                        "lost its access token but still holds the active profile's own \
                         refresh chain",
                        &still_degraded,
                        install_gate,
                    );
                }
                _ => {
                    self.follow_retry_at = crate::usage::now_ms() + FOLLOW_PROBE_RETRY_MS;
                    logline!(
                        "clauth daemon: live login for '{active}' has no access token but an \
                         unrecognized refresh token — leaving it alone; retrying"
                    );
                }
            }
            return;
        }
        if fingerprint == self.follow_memo {
            return; // same login we already examined — nothing new to prove
        }

        let Ok(Some(live)) = crate::claude::read_claude_credentials() else {
            self.follow_memo = fingerprint;
            return;
        };
        // The network tier retries on a timer, not per tick: a probe that
        // proved nothing (outage, throttle) or a rescue that couldn't finish
        // must be re-attempted — memoizing the failure against the login was
        // how one bad probe wedged the daemon for good — but not at 1 Hz.
        // Token equality (tier 1) stays per-tick: it is free and local.
        let network_ok = crate::usage::now_ms() >= self.follow_retry_at;
        let owner = self.identify_live_owner(&active, &live, network_ok.then_some(identity));
        let owner = match owner {
            LiveOwner::Sibling(name) => name,
            LiveOwner::ActiveItself => {
                // Same account, fresher pair — `try_adopt_live_rotation`
                // heals this on the rotation leg. Quietly stand down.
                self.follow_memo = fingerprint;
                return;
            }
            LiveOwner::Unknown(reason) => {
                match reason {
                    UnknownReason::ForeignAccount => {
                        // A PROVEN account clauth doesn't hold: a live login
                        // someone made on purpose. Overwriting it would destroy
                        // their only copy — a human decision, memoized so it
                        // logs once per unique login.
                        self.follow_memo = fingerprint;
                        logline!(
                            "clauth daemon: live login for '{active}' matches no stored \
                             account — resolve in the TUI"
                        );
                    }
                    UnknownReason::AccessDead if network_ok => {
                        self.rescue_dead_live_login(
                            &active,
                            &live,
                            fingerprint,
                            refresh,
                            install_gate,
                        );
                    }
                    UnknownReason::AccessDead | UnknownReason::Unproven => {
                        if network_ok {
                            // Proved nothing this round — try again on the timer.
                            self.follow_retry_at = crate::usage::now_ms() + FOLLOW_PROBE_RETRY_MS;
                        }
                    }
                }
                return;
            }
        };

        // RESCUE-2b: the adoption below mutates profile storage. Its failures
        // spend nothing, so they retry on the shared timer — memoizing them
        // (the pre-RESCUE-2 behavior) wedged a legitimately owned login behind
        // one transient local error for good. The attempt itself is timer-gated
        // so a sticky failure can't re-log at tick rate. Accepted trade: an
        // UNRELATED armed window (probe outage) also delays a tier-1 adoption
        // by up to 30 min — benign, CC already runs fine on the sibling's own
        // login; only clauth's active-pointer bookkeeping lags.
        if !network_ok {
            return;
        }
        let snapshot = match crate::actions::capture_snapshot() {
            Ok(s) => s,
            Err(e) => {
                self.follow_retry_at = crate::usage::now_ms() + FOLLOW_PROBE_RETRY_MS;
                logline!("clauth daemon: could not capture the live login: {e:#}; retrying");
                return;
            }
        };
        let result = {
            #[allow(clippy::expect_used, reason = "mutex poisoning is unrecoverable")]
            let mut cfg = self.config.lock().expect("config poisoned");
            // Re-check under the lock: an interleaved switch changes the story.
            if cfg.state.active_profile.as_deref() != Some(active.as_str()) {
                return;
            }
            crate::actions::overwrite_captured_profile(&mut cfg, &owner, snapshot).and_then(|()| {
                // Narrow delta (TECH-7): this writer owns only
                // `active_profile`; a concurrent login/chain edit keeps
                // its own fields.
                cfg.state = crate::profile::update_app_state(|s| {
                    s.active_profile = Some(owner.as_str().into());
                })?;
                Ok(())
            })
        };
        match result {
            Ok(()) => {
                // Adopt our own write's fingerprint so the next tick doesn't
                // treat it as an external change.
                self.last_reload_fp = reload_fingerprint();
                self.rebuild_tokens();
                self.follow_memo = None;
                self.follow_retry_at = 0;
                logline!(
                    "clauth daemon: following claude code to '{owner}' — the live login is \
                     its account (was '{active}')"
                );
            }
            Err(e) => {
                self.follow_retry_at = crate::usage::now_ms() + FOLLOW_PROBE_RETRY_MS;
                logline!(
                    "clauth daemon: failed to follow the live login to '{owner}': {e:#}; retrying"
                );
            }
        }
    }

    /// Ownership verdict for the live login, per the two proof tiers above.
    /// `identity` is `None` when the network tier is inside its retry backoff
    /// — tier 1 (token equality) still runs, and a miss reads as
    /// [`UnknownReason::Unproven`] rather than a fresh probe verdict.
    fn identify_live_owner(
        &self,
        active: &str,
        live: &crate::profile::ClaudeCredentials,
        identity: Option<&dyn Fn(&str) -> crate::usage::IdentityProbe>,
    ) -> LiveOwner {
        let live_access = live.access_token().filter(|t| !t.is_empty());
        let live_refresh = live.refresh_token().filter(|t| !t.is_empty());
        {
            #[allow(clippy::expect_used, reason = "mutex poisoning is unrecoverable")]
            let cfg = self.config.lock().expect("config poisoned");
            if let Some(p) = cfg.profiles.iter().find(|p| {
                (live_refresh.is_some() && p.refresh_token() == live_refresh)
                    || (live_access.is_some() && p.access_token() == live_access)
            }) {
                return if p.name.as_str() == active {
                    LiveOwner::ActiveItself
                } else {
                    LiveOwner::Sibling(p.name.as_str().to_string())
                };
            }
        }
        // Network tier — no config lock held across HTTP.
        let (Some(token), Some(identity)) = (live_access, identity) else {
            return LiveOwner::Unknown(UnknownReason::Unproven);
        };
        let uuid = match identity(token) {
            crate::usage::IdentityProbe::Proven(id) => id.uuid,
            crate::usage::IdentityProbe::Rejected => {
                return LiveOwner::Unknown(UnknownReason::AccessDead);
            }
            crate::usage::IdentityProbe::Indeterminate => {
                return LiveOwner::Unknown(UnknownReason::Unproven);
            }
        };
        if uuid.trim().is_empty() {
            return LiveOwner::Unknown(UnknownReason::Unproven);
        }
        #[allow(clippy::expect_used, reason = "mutex poisoning is unrecoverable")]
        let cfg = self.config.lock().expect("config poisoned");
        let mut anchors_complete = true;
        for p in &cfg.profiles {
            let anchor = crate::profile_cache::load_profile_cache::<String>(
                p.name.as_str(),
                crate::profile_cache::ACCOUNT_ID_CACHE_FILE,
            )
            .filter(|anchor| !anchor.trim().is_empty());
            match anchor {
                Some(anchor) if anchor == uuid => {
                    return if p.name.as_str() == active {
                        LiveOwner::ActiveItself
                    } else {
                        LiveOwner::Sibling(p.name.as_str().to_string())
                    };
                }
                Some(_) => {}
                None => {
                    // Only a profile that HOLDS a login can be the live
                    // login's owner; credential-less (API-key / not-yet-
                    // logged-in) profiles don't weaken the verdict.
                    let has_login = p.access_token().is_some_and(|t| !t.is_empty())
                        || p.refresh_token().is_some_and(|t| !t.is_empty());
                    if has_login {
                        anchors_complete = false;
                    }
                }
            }
        }
        if anchors_complete {
            LiveOwner::Unknown(UnknownReason::ForeignAccount)
        } else {
            // RESCUE-2b: a proven uuid that matches no ANCHORED profile proves
            // foreignness only when every stored login is anchored. Anchors are
            // legitimately absent in real flows (dropped on an unproven
            // re-login, backfilled by the next /profile poll), so with coverage
            // incomplete the live login could still be an owned account —
            // memoizing ForeignAccount here would wedge it behind the TUI for
            // good. Unproven retries on the timer instead. This branch only
            // runs on a completed network probe, so the log is once per
            // 30-min window, not per tick.
            logline!(
                "clauth daemon: live login for '{active}' matches no anchored account, but \
                 some stored logins have no identity anchor yet — cannot prove it foreign; \
                 retrying once anchors backfill"
            );
            LiveOwner::Unknown(UnknownReason::Unproven)
        }
    }

    /// RESCUE-1: reclaim the live slot from an endpoint-confirmed-DEAD foreign
    /// login. The divergence guard exists to protect a login the operator may
    /// hold nowhere else — but a dead pair protects nothing, and "protecting"
    /// it wedges every switch behind the TUI while the running `claude` stays
    /// signed out ("Login expired") for good.
    ///
    /// The access token was already `Rejected` by the identity endpoint; that
    /// alone is not proof (access tokens die of old age every 8h). The refresh
    /// leg settles it, with each outcome handled loss-free:
    ///   * **endpoint-confirmed dead** (`RefreshError::Invalid`) — nothing to
    ///     preserve; re-link the active profile's stored chain over the corpse
    ///     (AUTH-1-gated: only an installable stored login may take the slot,
    ///     and only while the corpse is still the login we probed).
    ///   * **refresh SUCCEEDED** — the pair was alive after all, and the probe
    ///     just consumed its single-use refresh token, so the fresh pair must
    ///     land straight back in the live file (`write_live_oauth_pair`). The
    ///     login survives; the next round re-identifies it with the fresh
    ///     access token and takes the follow/adopt path.
    ///   * **transient** — proves nothing; retry on the timer.
    fn rescue_dead_live_login(
        &mut self,
        active: &str,
        live: &crate::profile::ClaudeCredentials,
        fingerprint: Option<u64>,
        refresh: RefreshProbe,
        install_gate: &dyn Fn(&str) -> crate::oauth::AuthGate,
    ) {
        let retry_at = crate::usage::now_ms() + FOLLOW_PROBE_RETRY_MS;
        let scopes = live.scopes_joined();
        match live.refresh_token().filter(|t| !t.is_empty()) {
            None => {
                // Access token rejected and no refresh token to chase: the
                // login is unusable by anyone. Fall through to the reclaim.
            }
            Some(rt) => match refresh(rt, scopes.as_deref()) {
                Err(crate::oauth::RefreshError::Invalid(_)) => {
                    // Confirmed dead — fall through to the reclaim.
                }
                Ok(tokens) => {
                    // Concurrent-write guard: the fingerprint is re-verified
                    // INSIDE `write_live_oauth_pair`'s state flock (RESCUE-2c)
                    // — a fingerprint moved during the refresh roundtrip means
                    // a fresh CC login landed in the file, and our rotated
                    // pair continues the CORPSE's lineage that login just
                    // superseded; discarding it loses nothing.
                    match crate::claude::write_live_oauth_pair(&tokens, fingerprint) {
                        Ok(crate::claude::LiveWriteBack::Written) => {
                            // The login is healthy again — running sessions are
                            // unblocked NOW. The remaining follow/adopt
                            // bookkeeping re-probes on the ordinary timer: an
                            // instant retry would let a pathologically
                            // still-401ing (yet refreshable) token drive a
                            // per-tick rotation storm, spending a single-use
                            // refresh token every second.
                            self.follow_retry_at = retry_at;
                            logline!(
                                "clauth daemon: live login for '{active}' was alive after \
                                 all — rotated it in place; re-identifying on the next \
                                 probe window"
                            );
                        }
                        Ok(crate::claude::LiveWriteBack::Superseded) => {
                            // Benign: a fresh login (or a profile's own store)
                            // took the slot mid-rescue. Nothing lost — the next
                            // tick examines the fresh state from scratch.
                            // Deliberately no retry-timer arm: the common case
                            // is a REAL fresh login that must be probed/adopted
                            // promptly, and throttling it to protect against a
                            // hypothetical external writer planting a new dead
                            // login every tick would trade a real UX cost for
                            // a storm only CC itself could produce.
                        }
                        Err(e) => {
                            // The rotated pair could not be persisted — the
                            // live chain is now broken through our own probe.
                            // Loud, and terminal for this login: a re-login is
                            // the only recovery, exactly like a CC-side
                            // refresh crash at the same instant.
                            self.follow_memo = fingerprint;
                            logline!(
                                "clauth daemon: rescued live login for '{active}' but could \
                                 not write it back ({e:#}) — its chain is lost; run \
                                 `clauth login` or /login to recover"
                            );
                        }
                    }
                    return;
                }
                Err(crate::oauth::RefreshError::Transient(e)) => {
                    self.follow_retry_at = retry_at;
                    logline!(
                        "clauth daemon: live login for '{active}' looks dead but the \
                         endpoint would not confirm it ({e:#}); retrying"
                    );
                    return;
                }
            },
        }
        self.reclaim_live_slot(
            active,
            "is dead (endpoint-confirmed, matches no stored account)",
            // Overwrite exactly the corpse we probed: a fingerprint moved by
            // a concurrent CC write means fresh credentials landed there —
            // leave them and start over next tick.
            &|| crate::claude::live_credentials_fingerprint() == fingerprint,
            install_gate,
        );
    }

    /// The guarded reclaim shared by the dead-login rescue and the logged-out
    /// shell path: the active profile's stored chain takes the live slot back.
    /// `cause` names why the live login forfeited protection (log wording);
    /// `still_unchanged` re-verifies — as late as possible, after the gate's
    /// potential HTTP refresh — that the live file still holds exactly what
    /// the caller judged, so a concurrently landed CC login is never
    /// overwritten.
    fn reclaim_live_slot(
        &mut self,
        active: &str,
        cause: &str,
        still_unchanged: &dyn Fn() -> bool,
        install_gate: &dyn Fn(&str) -> crate::oauth::AuthGate,
    ) {
        let retry_at = crate::usage::now_ms() + FOLLOW_PROBE_RETRY_MS;
        // AUTH-1: never install a dead token — the stored chain must itself
        // be installable before it may take the live slot back.
        match install_gate(active) {
            crate::oauth::AuthGate::Ready | crate::oauth::AuthGate::Refreshed => {}
            crate::oauth::AuthGate::Broken => {
                self.follow_retry_at = retry_at;
                logline!(
                    "clauth daemon: live login for '{active}' {cause} but its stored login \
                     is dead too — run: clauth login {active}"
                );
                return;
            }
            crate::oauth::AuthGate::Transient(e) => {
                self.follow_retry_at = retry_at;
                logline!(
                    "clauth daemon: live login for '{active}' {cause} but its stored login \
                     could not be refreshed ({e:#}); retrying"
                );
                return;
            }
        }
        // RESCUE-2c: `still_unchanged` is evaluated INSIDE the state flock,
        // immediately before the relink — a concurrently landed CC login can
        // no longer slip into the check→mutate window and be destroyed.
        match crate::claude::force_link_profile_credentials_if(active, still_unchanged) {
            Ok(true) => {
                self.follow_memo = None;
                self.follow_retry_at = 0;
                logline!(
                    "clauth daemon: live login for '{active}' {cause} — reclaimed the live \
                     slot with '{active}'s stored login; running sessions are signed back in"
                );
            }
            Ok(false) => {
                // Superseded mid-reclaim — a fresh login landed. Start over
                // next tick; nothing was touched.
            }
            Err(e) => {
                self.follow_retry_at = retry_at;
                logline!("clauth daemon: failed to reclaim the live slot for '{active}': {e:#}");
            }
        }
    }

    /// Rebuild the scheduler's token snapshots from the current config (after a
    /// switch or a reload). Drops the `config` guard before taking the token
    /// lists — `TOKENS` is outer of `CONFIG`, so folding them inverts lock order.
    fn rebuild_tokens(&self) {
        #[allow(clippy::expect_used, reason = "mutex poisoning is unrecoverable")]
        {
            let tokens = collect_tokens(&self.config.lock().expect("config poisoned"));
            let third_party =
                collect_third_party_entries(&self.config.lock().expect("config poisoned").profiles);
            *self.usage_tokens.lock().expect("usage_tokens poisoned") = tokens;
            *self
                .third_party_tokens
                .lock()
                .expect("third_party_tokens poisoned") = third_party;
        }
    }

    /// Reload `profiles.toml` when it changed on disk (external `clauth login`,
    /// TUI edit). Replaces the shared config and rebuilds token lists so the
    /// scheduler picks up added/removed profiles.
    pub(super) fn reload_if_changed(&mut self) {
        let current = reload_fingerprint();
        if current == self.last_reload_fp {
            return;
        }
        self.last_reload_fp = current;
        match load_config() {
            Ok(new_config) => {
                self.refresh_interval
                    .store(new_config.state.refresh_interval_ms, Ordering::Relaxed);
                if let Ok(mut c) = self.config.lock() {
                    *c = new_config;
                }
                self.rebuild_tokens();
                logline!("clauth daemon: reloaded config after an external change");
            }
            Err(e) => logline!("clauth daemon: config reload failed: {e}"),
        }
    }

    /// Execute the queued switch. Drains the whole queue atomically, picks the
    /// winner (User outranks Scheduler, last-writer-wins — [`select_switch_winner`]),
    /// and attempts it. A winner that can't land THIS tick — target still
    /// mid-fetch/rotation, outgoing active has unsaved diverged credentials (the
    /// daemon can't prompt), or a transient refresh failure — is RE-QUEUED until it
    /// lands or its retry window closes, rather than dropped after one attempt
    /// (TECH-6, finding #4: a user tap during a fetch window used to evaporate
    /// after the `{ok:true}` ack). Every skip/failure records a [`LastError`] so the
    /// deferral is observable in `status.json` ('no silent failures').
    pub(super) fn drain_pending_switch(&mut self) {
        let entries: VecDeque<PendingSwitchEntry> = self
            .pending_switch
            .lock()
            .map(|mut g| std::mem::take(&mut *g))
            .unwrap_or_default();
        if entries.is_empty() {
            return;
        }
        // One winner PER HARNESS per round (CDX-4 §0.15): the two active
        // slots are independent, so a deferred claude switch must never hold
        // a codex rotation hostage (or vice versa). `switch_backoff` remains
        // a single slot keyed by target — with both harnesses stuck at once
        // the backoffs overwrite each other, costing extra log lines, never
        // correctness (the per-entry retry TTL still bounds every retry).
        for harness in [
            crate::profile::Harness::Claude,
            crate::profile::Harness::Codex,
        ] {
            if let Some(winner) = crate::usage::select_switch_winner_for(&entries, harness) {
                self.attempt_switch_winner(winner);
            }
        }
    }

    /// Attempt one drained winner (see [`Self::drain_pending_switch`]).
    fn attempt_switch_winner(&mut self, winner: PendingSwitchEntry) {
        let now = now_ms();

        // A vanished target (deleted out-of-process after the enqueue — the
        // queue holds a raw name this process alone owns) is DROPPED, not
        // retried: no re-login can resurrect a profile that no longer exists,
        // and attempting it would run `switch_profile`'s side effects against
        // a ghost. Recorded in last_error so the drop is observable.
        let target_exists = self
            .config
            .lock()
            .map(|c| c.find(&winner.target).is_some())
            .unwrap_or(false);
        if !target_exists {
            logline!(
                "clauth daemon: dropping queued switch to '{}': profile no longer exists (deleted?)",
                winner.target
            );
            self.set_last_error(
                now,
                format!(
                    "dropped switch to '{}': profile no longer exists (deleted?)",
                    winner.target
                ),
            );
            if self
                .switch_backoff
                .get(&winner.harness)
                .is_some_and(|b| b.target == winner.target)
            {
                self.switch_backoff.remove(&winner.harness);
            }
            return;
        }

        // Backoff gate (TECH-8): if this target is inside its backoff window from a
        // prior failure, re-queue without attempting or logging — this is what turns
        // a stuck switch's 1/tick retry+log storm into a spaced, deduped one. The
        // entry's TTL is checked here too: near the retry window's edge a capped
        // backoff step can reach past it, and gating on `not_before` alone would
        // keep requeueing an entry whose window has already closed. Keyed by
        // harness so a stuck claude target never gates a codex rotation.
        if let Some(b) = self.switch_backoff.get(&winner.harness)
            && b.target == winner.target
        {
            if now >= winner.retry_until {
                logline!(
                    "clauth daemon: gave up switching to '{}': {}",
                    winner.target,
                    b.reason
                );
                self.switch_backoff.remove(&winner.harness);
                return;
            }
            if now < b.not_before {
                self.requeue_quiet(winner);
                return;
            }
        }

        // CDX-1 T6: a codex target takes the codex path — none of the claude
        // gates below (fetch activity, OAuth install gate, claude divergence)
        // apply to a profile that is never in either fetch leg.
        let target_is_codex = self
            .config
            .lock()
            .map(|c| c.find(&winner.target).is_some_and(|p| p.is_codex()))
            .unwrap_or(false);
        if target_is_codex {
            self.drain_codex_switch(winner, now);
            return;
        }

        // Still mid-fetch/rotation — switching now would race the worker on the
        // single-use token/TokenList. Defer, keep retrying.
        if !is_idle(&self.activity, &winner.target) {
            self.fail_switch(winner, now, "target is mid-fetch");
            return;
        }

        // Outgoing active has an uncaptured re-login. A SCHEDULER switch defers
        // to the operator (the daemon can't prompt, and the divergence may
        // resolve — keep retrying). A USER switch IS the operator's decision
        // (RESCUE-2): the tap outranks preserving a login clauth doesn't own,
        // so archive the unsaved login into `~/.clauth/quarantine/` (loss-free)
        // and proceed with discard semantics — before this, a socket-originated
        // user switch was structurally unable to get past a foreign live login
        // and wedged until that login died (observed 2026-07-16, ~33h).
        let outgoing = self
            .config
            .lock()
            .ok()
            .and_then(|c| c.state.active_profile.as_deref().map(str::to_string));
        let mut discard_diverged = false;
        if let Some(active) = &outgoing
            && active != &winner.target
            && active_diverged_unsaved(active)
        {
            if winner.origin == Origin::User {
                discard_diverged = true;
            } else {
                self.fail_switch(
                    winner,
                    now,
                    &format!(
                        "active '{active}' has unsaved credentials; {}",
                        crate::format::RESOLVE_IN_TUI
                    ),
                );
                return;
            }
        }

        // AUTH-1 (Incident C): never install a stale/dead token. Refresh an expiring
        // target before install; a revoked token is quarantined (`auth_broken`) and
        // dropped — retrying can't help until `clauth login`. The gate does its HTTP
        // refresh with no config lock held, so it cannot wedge the run loop mid-lock.
        match crate::oauth::ensure_installable(
            &self.config,
            &winner.target,
            crate::oauth::refresh_result,
        ) {
            crate::oauth::AuthGate::Ready | crate::oauth::AuthGate::Refreshed => {}
            crate::oauth::AuthGate::Broken => {
                // The gate persisted `auth_broken`; adopt that fingerprint so the
                // next tick's reload doesn't treat our own write as external.
                // Terminal failure (drop, not retry) — clear any backoff for this
                // target.
                self.last_reload_fp = reload_fingerprint();
                let msg = crate::format::login_expired(&winner.target).line();
                logline!("clauth daemon: {msg}");
                self.set_last_error(now, msg);
                self.switch_backoff.remove(&winner.harness);
                return;
            }
            crate::oauth::AuthGate::Transient(e) => {
                self.fail_switch(winner, now, &format!("refresh failed transiently ({e})"));
                return;
            }
        }

        // TECH-7: hold the state flock across the switch AND the post-write
        // fingerprint
        // read, so an external write can't slip into the save→read window and be
        // adopted as our own (the :354 self-adoption gap). `with_state_lock` is
        // re-entrant, so `switch_profile`'s inner acquisition nests without
        // deadlock; `reload_fingerprint()` is read while we still hold the flock.
        let result = {
            #[allow(
                clippy::expect_used,
                reason = "config mutex poisoning is unrecoverable"
            )]
            let mut cfg = self.config.lock().expect("config poisoned");
            crate::lock::with_state_lock(|| {
                // RESCUE-2: re-check under the flock — the divergence may have
                // resolved (follow adopted it) since the gate above; archive +
                // discard only what is still genuinely unsaved.
                let discard =
                    discard_diverged && outgoing.as_deref().is_some_and(active_diverged_unsaved);
                if let Some(active) = outgoing.as_deref().filter(|_| discard) {
                    let dest = crate::claude::archive_live_credentials(active)?;
                    logline!(
                        "clauth daemon: archived '{active}'s unsaved live login to {} — a \
                         user switch outranks it",
                        dest.display()
                    );
                    crate::actions::switch_profile_discard(&mut cfg, &winner.target)?;
                } else {
                    switch_profile(&mut cfg, &winner.target)?;
                }
                Ok(reload_fingerprint())
            })
        };
        match result {
            Ok(fp) => {
                self.rebuild_tokens();
                self.last_reload_fp = fp;
                // TECH-8: record the hero event; clear any failure backoff/dedup.
                self.last_switch = Some(LastSwitch {
                    from: outgoing.clone(),
                    to: Some(winner.target.clone()),
                    at_ms: now,
                    trigger: origin_trigger(winner.origin),
                });
                self.switch_backoff.remove(&winner.harness);
                logline!("clauth daemon: switched to '{}'", winner.target);
            }
            Err(e) => {
                self.fail_switch(winner, now, &format!("switch failed: {e}"));
            }
        }
    }

    /// CDX-1 T6: the codex arm of `drain_pending_switch`. The origin decides
    /// what happens to a FOREIGN live login: a User switch (socket tap — the
    /// operator's decision, RESCUE-2 semantics) archives it to quarantine and
    /// proceeds; a Scheduler switch refuses and retries via the shared
    /// backoff, exactly like the claude divergence defer.
    fn drain_codex_switch(&mut self, winner: PendingSwitchEntry, now: u64) {
        let policy = if winner.origin == Origin::User {
            crate::actions::ForeignLivePolicy::Archive
        } else {
            crate::actions::ForeignLivePolicy::Refuse
        };
        let outgoing = self
            .config
            .lock()
            .ok()
            .and_then(|c| c.state.active_codex_profile.as_deref().map(str::to_string));
        // Same TECH-7 shape as the claude arm: hold the flock across the
        // switch AND the post-write mtime read.
        let result = {
            #[allow(
                clippy::expect_used,
                reason = "config mutex poisoning is unrecoverable"
            )]
            let mut cfg = self.config.lock().expect("config poisoned");
            crate::lock::with_state_lock(|| {
                let report =
                    crate::actions::codex_switch_profile(&mut cfg, &winner.target, policy)?;
                Ok((report, reload_fingerprint()))
            })
        };
        match result {
            Ok((report, fp)) => {
                self.last_reload_fp = fp;
                if let Some(owner) = &report.adopted_back {
                    logline!(
                        "clauth daemon: adopted codex's refreshed login back into '{owner}' first"
                    );
                }
                if let Some(path) = &report.archived {
                    logline!(
                        "clauth daemon: archived the outgoing codex login to {} — a user \
                         switch outranks it",
                        path.display()
                    );
                }
                self.last_switch = Some(LastSwitch {
                    from: outgoing,
                    to: Some(winner.target.clone()),
                    at_ms: now,
                    trigger: origin_trigger(winner.origin),
                });
                self.switch_backoff.remove(&winner.harness);
                logline!("clauth daemon: codex switched to '{}'", winner.target);
            }
            Err(e) => self.fail_switch(winner, now, &format!("codex switch failed: {e}")),
        }
    }

    /// Record the most recent switch skip/failure reason for `status.json` (TECH-6).
    fn set_last_error(&mut self, at_ms: u64, message: impl Into<String>) {
        self.last_error = Some(LastError {
            at_ms,
            message: message.into(),
        });
    }

    /// Handle a switch that couldn't execute (busy / diverged / transient / switch
    /// error). Advances exponential backoff for this target, DEDUPS the failure log
    /// (emits only when the target or reason changes — a stuck switch no longer logs
    /// 1/tick, TECH-8 finding #38), and re-queues the entry until its retry window
    /// closes. A change of target resets the backoff.
    fn fail_switch(&mut self, entry: PendingSwitchEntry, now: u64, reason: &str) {
        // All backoff/dedup state is keyed by the entry's HARNESS (CDX-4 review
        // MED) so a stuck claude switch and a stuck codex switch keep separate
        // slots and never wipe each other's spacing.
        let slot = self.switch_backoff.get(&entry.harness);
        let attempts = match slot {
            Some(b) if b.target == entry.target => b.attempts + 1,
            _ => 1,
        };
        // Dedup: only log when the (target, reason) pair changed since last time.
        let changed = slot.is_none_or(|b| b.target != entry.target || b.reason != reason);
        if changed {
            logline!(
                "clauth daemon: deferring switch to '{}': {reason}",
                entry.target
            );
            self.set_last_error(
                now,
                format!("deferring switch to '{}': {reason}", entry.target),
            );
            self.switch_failure_logs += 1;
        }
        if now < entry.retry_until {
            self.switch_backoff.insert(
                entry.harness,
                SwitchBackoff {
                    target: entry.target.clone(),
                    attempts,
                    not_before: now.saturating_add(switch_backoff_ms(attempts)),
                    reason: reason.to_string(),
                },
            );
            self.requeue_quiet(entry);
        } else {
            // Retry window closed — give up and stop tracking this target.
            self.set_last_error(
                now,
                format!("gave up switching to '{}': {reason}", entry.target),
            );
            self.switch_backoff.remove(&entry.harness);
        }
    }

    /// Re-queue an entry honoring Origin precedence (a superseding target that
    /// arrived since the drain is never clobbered; a Scheduler retry yields to a
    /// User request that landed in the gap). No logging — the backoff/dedup path
    /// owns the observability.
    fn requeue_quiet(&mut self, entry: PendingSwitchEntry) {
        if let Ok(mut q) = self.pending_switch.lock() {
            let superseded = q.iter().any(|e| e.target == entry.target)
                || (entry.origin == Origin::Scheduler
                    && q.iter().any(|e| e.origin == Origin::User));
            if !superseded {
                q.push_back(entry);
            }
        }
    }

    /// Execute a queued wrap-off (whole chain exhausted). Same divergence guard.
    pub(super) fn drain_pending_switch_off(&mut self) {
        let off = self
            .pending_switch_off
            .lock()
            .map(|mut g| std::mem::replace(&mut *g, false))
            .unwrap_or(false);
        if !off {
            return;
        }
        let active = self
            .config
            .lock()
            .ok()
            .and_then(|c| c.state.active_profile.as_deref().map(str::to_string));
        let Some(active) = active else {
            return; // already off
        };
        if active_diverged_unsaved(&active) {
            logline!(
                "clauth daemon: skipping switch-off: active '{active}' has unsaved credentials"
            );
            return;
        }
        // TECH-7: same flock-held fingerprint capture as `drain_pending_switch`.
        let result = {
            #[allow(
                clippy::expect_used,
                reason = "config mutex poisoning is unrecoverable"
            )]
            let mut cfg = self.config.lock().expect("config poisoned");
            crate::lock::with_state_lock(|| {
                switch_off(&mut cfg)?;
                Ok(reload_fingerprint())
            })
        };
        match result {
            Ok(fp) => {
                self.rebuild_tokens();
                self.last_reload_fp = fp;
                // TECH-8: a wrap-off is an executed switch (to = none).
                self.last_switch = Some(LastSwitch {
                    from: Some(active.clone()),
                    to: None,
                    at_ms: now_ms(),
                    trigger: "wrap_off",
                });
                logline!("clauth daemon: switched off: all accounts spent");
            }
            Err(e) => logline!("clauth daemon: switch-off failed: {e}"),
        }
    }

    /// Apply fallback-config edits the socket queued (add/remove/reorder chain
    /// members, per-member threshold, wrap-off). Each edit mutates + persists via
    /// the shared [`fallback_config`] primitives (transactional against their own
    /// writes). We adopt a fresh reload fingerprint only when an edit actually
    /// (re)wrote `profiles.toml` (the primitive returned `Ok(true)`), so the next
    /// tick's `reload_if_changed` skips *our* write but still catches an unrelated
    /// external edit that landed the same tick. A threshold edit touches only the
    /// profile's `config.toml` (`Ok(false)`) and deliberately does NOT adopt: the
    /// fingerprint covers config.toml mtimes too (0.12.0+), so our own write
    /// triggers one redundant self-reload next tick — harmless (disk already
    /// equals memory) and the price of never swallowing a same-tick external
    /// edit. The token lists are untouched by chain/threshold/wrap-off edits, so
    /// no `rebuild_tokens` is needed (unlike a switch).
    ///
    /// Lock order: drain the queue (rank `PendingConfigOps`) into a `Vec` and
    /// release it *before* taking `config` (rank `Config`, lower/outer), matching
    /// `drain_pending_switch`. `save_app_state`/`save_profile` take the state
    /// flock (inner of `config`) — the established save order.
    pub(super) fn drain_config_ops(&mut self) {
        let ops: Vec<ConfigOp> = self
            .pending_config_ops
            .lock()
            .map(|mut g| std::mem::take(&mut *g))
            .unwrap_or_default();
        if ops.is_empty() {
            return;
        }
        let mut wrote_state = false;
        {
            #[allow(
                clippy::expect_used,
                reason = "config mutex poisoning is unrecoverable"
            )]
            let mut cfg = self.config.lock().expect("config poisoned");
            for op in ops {
                let result = match op {
                    ConfigOp::FallbackAdd(name) => fallback_config::add(&mut cfg, &name),
                    ConfigOp::FallbackRemove(name) => fallback_config::remove(&mut cfg, &name),
                    ConfigOp::FallbackMove(name, dir) => {
                        fallback_config::move_member(&mut cfg, &name, dir)
                    }
                    ConfigOp::SetThreshold(name, value) => {
                        fallback_config::set_threshold(&mut cfg, &name, value)
                    }
                    ConfigOp::SetLastResort(name, on) => {
                        fallback_config::set_last_resort(&mut cfg, &name, on)
                    }
                    ConfigOp::SetMemberWeekly(name, value) => {
                        fallback_config::set_member_weekly(&mut cfg, &name, value)
                    }
                    ConfigOp::SetUsageGate(name, scoped, on) => {
                        fallback_config::set_usage_gate(&mut cfg, &name, scoped, on)
                    }
                    ConfigOp::SetWrapOff(on) => fallback_config::set_wrap_off(&mut cfg, on),
                    ConfigOp::SetWeeklyThreshold(v) => {
                        fallback_config::set_weekly_threshold(&mut cfg, v)
                    }
                    ConfigOp::Rename(old, new) => fallback_config::rename(&mut cfg, &old, &new),
                };
                match result {
                    Ok(state_written) => wrote_state |= state_written,
                    Err(e) => logline!("clauth daemon: config edit failed: {e}"),
                }
            }
        }
        if wrote_state {
            self.last_reload_fp = reload_fingerprint();
        }
    }
}
