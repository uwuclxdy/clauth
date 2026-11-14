//! The daemon's per-tick work, extracted from [`super::Daemon::run`]'s loop so it
//! is drivable in a test without the `sleep`/watchdog wrapper.
//!
//! [`Daemon::tick`](super::Daemon::tick) is one iteration of the run loop:
//! reload external config changes, execute any queued auto-switch / switch-off,
//! then rewrite `status.json`. The drains and the token-list rebuild live here;
//! `write_status` and the loop scaffolding stay in `mod.rs`.
//! `tests/inline/daemon_mod.rs` pins each drain's behavior against this seam.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::Ordering;
use std::time::Instant;

use crate::actions::{switch_off_locked, switch_profile_locked_gated};
use crate::logline::logline;
use crate::profile::{ProfileName, load_config, reload_fingerprint};
use crate::usage::{
    PendingSwitchTarget, current_key_rejected_names, is_idle, now_ms, queued_switch_away_is_stale,
};

use super::{SwitchBackoff, active_diverged_unsaved, switch_backoff_ms};

/// How long a queued switch target keeps retrying (from its FIRST failed
/// attempt) before the daemon gives up on it. The scheduler re-evaluates the
/// chain every tick, so a still-correct target is simply re-queued fresh.
const SWITCH_RETRY_TTL_MS: u64 = 120_000;

impl super::Daemon {
    /// One iteration of the run loop: reload external config, drain the queued
    /// auto-switch / switch-off / config edits, then rewrite `status.json`.
    /// Called each tick by [`super::Daemon::run`] (and directly by the inline
    /// tests). The initial `status.json` write and the watchdog heartbeat stay in
    /// `run`, so this method is exactly the observable per-tick work.
    pub(super) fn tick(&mut self) {
        // One shared window for the WHOLE tick: the two drains below each take
        // their own state-lock acquisition, and per-acquisition arming handed
        // a tick draining a queued switch AND a queued switch-off a fresh
        // budget apiece — 40 s of stuck-keychain shell-outs, or two full
        // flock waits (2 × `STATE_LOCK_TIMEOUT` = 50 s), against the 30 s
        // watchdog that aborts the daemon mid-switch. The window bounds both:
        // this clamped scope caps each drain's flock wait to what the window
        // leaves, and once the window or the watchdog deadline is spent the
        // next drain is SKIPPED rather than handed a wait — its queue is
        // untouched, so the next tick retries with a fresh window. Inner holds
        // spend what this leaves them and disarm nothing
        // (`lock::SharedSubprocessBudget`).
        let _tick_budget =
            crate::lock::SharedSubprocessBudget::arm_clamped(crate::lock::subprocess_budget());
        // The watchdog aborts once `now - heartbeat > WATCHDOG_DEADLINE`, and
        // `run` stamps the heartbeat TICK before this tick began (right after
        // the previous tick, then it sleeps TICK) — so a tick outliving
        // WATCHDOG_DEADLINE - TICK past its own start cannot land its
        // heartbeat in time. The gates below skip the next drain once the
        // window or this deadline is spent.
        let deadline = Instant::now() + super::WATCHDOG_DEADLINE.saturating_sub(super::TICK);
        // Safety net: a forget noted without an immediate drain still lands
        // before this tick's status write; the per-site drains keep this a
        // no-op.
        self.apply_forgets();
        self.reload_if_changed();
        // A dead-login record moves with no config change, cleared by the
        // process that fetches usage or removed by hand, so the reload seed
        // never sees it; the TUI runs the same pass.
        crate::usage::sync_dead_logins(
            &self.config,
            &self.third_party_broken,
            &mut self.dead_login_stats,
        );
        self.log_day_claim_notices();
        #[cfg(target_os = "macos")]
        {
            if self
                .bare_item_scan_ticks
                .is_multiple_of(super::BARE_ITEM_SCAN_EVERY_TICKS)
            {
                crate::claude::correct_bare_keychain_item_detached();
            }
            if self
                .bare_item_scan_ticks
                .is_multiple_of(super::STALE_COPY_SWEEP_EVERY_TICKS)
            {
                crate::runtime::sweep_stale_copied_bearers_detached();
            }
            self.bare_item_scan_ticks = self.bare_item_scan_ticks.wrapping_add(1);
        }
        if !self.next_drain_skipped(deadline) {
            self.drain_pending_switch();
        }
        if !self.next_drain_skipped(deadline) {
            self.drain_pending_switch_off();
        }
        self.reconcile_background_holds();
        // The status build's codex read also feeds the name notices, so a tick
        // reads `codex-profiles.toml` once and cannot judge a codex roster its
        // body did not describe.
        let codex = self.write_status();
        self.log_name_notices(codex.as_ref());
        // Honour a strip the switch-time strip deferred (its sync lock was held)
        // with no live `clauth start` wrapper to reconcile it. Try-once: never
        // parks this tick behind a held sync lock, and no ranked lock is held
        // here, so entering the JsonSync rank is legal.
        crate::claude_json::try_honor_pending_strip();
        // Converge a broken plugin registration in the background. The gate is
        // agentgear's spawn-free pre-check (file reads, no `claude` spawn), and a
        // needed heal runs detached (throttled inside `heal_detached`), so this
        // never blocks the run loop.
        crate::plugin_host::heal_detached();
        // Same shape for the herdr plugin: update a stale install in the
        // background, throttled inside its own `heal_detached`. Its saved
        // `[update]` toggle gates the network leg like the binary update,
        // read off the config this tick's reload just refreshed (a poisoned
        // config mutex falls back to the absent-table default: on).
        let auto_update = self
            .config
            .lock()
            .map(|c| c.state.update.auto_update)
            .unwrap_or(true);
        crate::herdr::heal_detached(auto_update);
        // Keep MCP server logins in step across every account's store.
        // Throttled and detached inside, so it never spends this tick's window;
        // its toggle off the config this tick's reload refreshed, a poisoned
        // config mutex reading on like the file's default.
        let sync_mcp = self
            .config
            .lock()
            .map(|c| c.state.sync_mcp_logins)
            .unwrap_or(true);
        crate::runtime::sync_mcp_logins_detached(sync_mcp);
    }

    /// Hand this tick's background-tree work to the hold worker: adopt every
    /// configured account's fixed `runtime-bg` whose tree a Claude Code job
    /// names, and every handed-off `--with-fallback` per-launch tree a job
    /// names; release every hold no job names any more.
    ///
    /// This reads the job store, a config snapshot and, for the strip list,
    /// `profiles.toml` plus the `config.toml` of the account it names active
    /// (of every roster account when it names none), without the state flock,
    /// under the config mutex; then it returns. The blocking half — the
    /// rotation lock, the state flock, the tree build — runs on the worker. A
    /// tick that waited for it here would spend the run loop's watchdog budget
    /// on another process's lock.
    pub(super) fn reconcile_background_holds(&mut self) {
        let Ok(cfg) = self.config.lock() else {
            return;
        };
        let names: Vec<ProfileName> = cfg.profiles.iter().map(|p| p.name.clone()).collect();
        // A strip list that does not read off disk this tick strips the
        // snapshot's keys alone; the next tick reads it again.
        let stale_env_keys = match crate::actions::outgoing_env_keys_now(&cfg) {
            Ok(keys) => {
                self.strip_list_failure = None;
                keys
            }
            Err(e) => {
                let kind = e
                    .chain()
                    .find_map(|cause| cause.downcast_ref::<std::io::Error>())
                    .map_or_else(|| "unparsed".to_string(), |io| format!("{:?}", io.kind()));
                if self.strip_list_failure.as_deref() != Some(kind.as_str()) {
                    logline!(
                        "clauth daemon: could not re-read the active account's env keys, \
                         background sessions use the loaded copy: {e:#}"
                    );
                    self.strip_list_failure = Some(kind);
                }
                crate::actions::outgoing_env_keys(&cfg)
            }
        };
        drop(cfg);
        self.background.reconcile(names, stale_env_keys);
    }

    /// Log once, per day and per config change, what today's day lists are
    /// doing that the operator did not write them to do. The headless half of
    /// the TUI's toast (`tui::app::warn_day_claim_notices`); nothing is broken
    /// by either state, but a run with no operator watching still has to leave
    /// the record of which lists were in play.
    ///
    /// Placed after `reload_if_changed` so an edit made this tick is what gets
    /// reported, and gated on the messages so the per-tick re-derivation does
    /// not repaint the same line until midnight.
    pub(super) fn log_day_claim_notices(&mut self) {
        // Clone the live broken map first (rank 295 below config 400), then
        // intersect with the SAME config snapshot the notices scan reads, so a
        // repair landing since the last tick drops the name at once.
        let broken_snapshot: HashMap<String, u64> = self
            .third_party_broken
            .lock()
            .map(|m| m.clone())
            .unwrap_or_default();
        #[allow(
            clippy::expect_used,
            reason = "config mutex poisoning is unrecoverable"
        )]
        let notices = {
            let cfg = self.config.lock().expect("config mutex poisoned");
            let key_rejected: HashSet<ProfileName> =
                current_key_rejected_names(&broken_snapshot, &cfg.profiles)
                    .into_iter()
                    .collect();
            cfg.day_claim_notices_today(&key_rejected)
        };
        if notices == self.day_claim_notices {
            return;
        }
        for msg in notices
            .iter()
            .filter(|m| !self.day_claim_notices.contains(m))
        {
            logline!("clauth: {msg}");
        }
        self.day_claim_notices = notices;
    }

    /// Log once per change which stored accounts the printed commands cannot
    /// address: the headless half of `tui::app::warn_name_notices`, gated the
    /// way [`Self::log_day_claim_notices`] is. `codex` is the tick's own roster
    /// read — the one `write_status` built `status.json` from — never a fresh
    /// load: two reads in one tick can disagree. A FAILED read judges the codex
    /// half against the last successful one, so the read's recovery re-raises
    /// nothing.
    pub(super) fn log_name_notices(&mut self, codex: Option<&crate::codex_profiles::CodexState>) {
        if let Some(codex) = codex {
            self.last_codex_roster.clear();
            self.last_codex_roster.extend_from_slice(codex.profiles());
        }
        #[allow(
            clippy::expect_used,
            reason = "config mutex poisoning is unrecoverable"
        )]
        let notices = self
            .config
            .lock()
            .expect("config mutex poisoned")
            .name_notices(self.last_codex_roster.iter());
        if notices == self.name_notices {
            return;
        }
        for notice in notices.iter().filter(|n| !self.name_notices.contains(n)) {
            logline!("{}", notice.cli_line());
        }
        self.name_notices = notices;
    }

    /// Whether the next drain must be skipped rather than handed a wait: the
    /// tick's shared window (keychain shell-outs AND flock waits) is spent, or
    /// the watchdog deadline is past. A skipped drain leaves its queue
    /// untouched — the first drain's re-queued switch and the pending
    /// switch-off retry on the next tick with a fresh window — and logs once
    /// so the skip is visible on the event line.
    fn next_drain_skipped(&self, deadline: Instant) -> bool {
        if !super::drains_exhausted(
            crate::lock::armed_budget_remaining(),
            Instant::now(),
            deadline,
        ) {
            return false;
        }
        let switch_pending = self.pending_switch.lock().is_ok_and(|g| !g.is_empty());
        let off_pending = self.pending_switch_off.lock().is_ok_and(|g| *g);
        if switch_pending || off_pending {
            logline!("clauth daemon: skipping queued switch work: tick budget or deadline spent");
        }
        true
    }

    /// Rebuild the scheduler's token snapshots from the current config (after a
    /// switch or a reload). Drops the `config` guard before taking the token
    /// lists — `TOKENS` is outer of `CONFIG`, so folding them inverts lock order.
    fn rebuild_tokens(&self) {
        // Snapshot under the config guard, then read each profile's anchor +
        // stored token off disk AFTER it drops — the anchor read must not widen
        // the config-lock hold.
        let (seeds, third_party) = {
            #[allow(clippy::expect_used, reason = "mutex poisoning is unrecoverable")]
            let cfg = self.config.lock().expect("config poisoned");
            (
                crate::usage::collect_token_seeds(&cfg),
                crate::usage::collect_third_party_entries(&cfg.profiles),
            )
        };
        let tokens = crate::usage::build_token_entries(seeds);
        #[allow(clippy::expect_used, reason = "mutex poisoning is unrecoverable")]
        {
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
        match load_config() {
            Ok(new_config) => {
                self.refresh_interval
                    .store(new_config.state.refresh_interval_ms, Ordering::Relaxed);
                let profile_names: Vec<ProfileName> =
                    new_config.profiles.iter().map(|p| p.name.clone()).collect();
                let mut probes_off = Vec::new();
                if let Ok(mut c) = self.config.lock() {
                    probes_off = crate::usage::probes_switched_off(&c, &new_config);
                    *c = new_config;
                }
                // The reload re-read disk, so it adopts the fingerprint it just
                // re-read. The token list is rebuilt BEFORE the compare's
                // forget (the epoch bump): a partition landing between the two
                // must clone the REBUILT entry, never the old entry under a
                // post-bump epoch — that pairing stamps cadence for the old
                // credential and delays the new login's first fetch. The
                // reverse window, a partition between the rebuild and the
                // compare while the snapshot half still holds the old identity,
                // drops the rebuilt entry as stale for one tick; it self-heals
                // on the next compare, never a wrong reading.
                self.rebuild_tokens();
                crate::usage::reload_and_forget(&self.memory_stores(), &profile_names);
                self.dead_login_stats.clear();
                // A probe switched off elsewhere leaves the account as a
                // restart shows it. After the compare, since its forget
                // re-seeds the reading from disk.
                for name in &probes_off {
                    crate::usage::forget_probe_reading(&self.memory_stores(), name);
                }
                self.last_reload_fp = current;
                self.last_reload_error = None;
                logline!("clauth daemon: reloaded config");
            }
            Err(e) => {
                // The load failed, so the token list cannot rebuild. Still run
                // the compare against the still-current config: it applies this
                // process's pending forgets. An anchored copy-back leaves its
                // stale entry (the identity compare reads clean because the
                // anchor did not move), so that entry keeps fetching with its
                // old token until a reload succeeds.
                let profile_names: Vec<ProfileName> = self
                    .config
                    .lock()
                    .map(|c| c.profiles.iter().map(|p| p.name.clone()).collect())
                    .unwrap_or_default();
                crate::usage::reload_and_forget(&self.memory_stores(), &profile_names);
                // Keep the last-good fingerprint, so every tick retries the
                // load until one succeeds, a repair that keeps the broken
                // write's mtime included. The failure logs once per distinct
                // text; re-reading the same broken write every tick must not
                // re-log.
                let text = e.to_string();
                if self.last_reload_error.as_deref() != Some(text.as_str()) {
                    logline!("clauth daemon: config reload failed: {e}");
                    self.last_reload_error = Some(text);
                }
            }
        }
        // Either arm, after its forget: a verdict another process recorded
        // since the last seed reaches the walks, against the config in force.
        crate::usage::seed_dead_logins(&self.config, &self.third_party_broken);
    }

    /// Execute the queued switch. Drains the target set atomically and attempts
    /// the pick. A target that can't land THIS tick — still mid-fetch/rotation,
    /// outgoing active has unsaved diverged credentials (the daemon can't
    /// prompt), or a transient refresh failure — is RE-QUEUED until it lands or
    /// its retry window closes, rather than dropped after one attempt. Every
    /// skip/failure logs once per distinct reason (see [`SwitchBackoff`]).
    pub(super) fn drain_pending_switch(&mut self) {
        // The generation this drain started under: the gate's ticket is built
        // from it, so a release landing anywhere after the guard — including
        // in the span before the gate is even built — spends the ticket.
        let generation = self.fetch_lease.generation();
        // The set holds at most one scheduler target in practice
        // (`scan_auto_switch` skips while one is pending); `min_by` keeps the
        // pick deterministic anyway.
        let decision: Option<PendingSwitchTarget> = self
            .pending_switch
            .lock()
            .map(|mut g| {
                let d = g.iter().min_by(|a, b| a.target.cmp(&b.target)).cloned();
                g.clear();
                d
            })
            .unwrap_or_default();
        // A decision queued under a ticket this instance no longer holds must
        // not execute — the holder that took over decides. The record clears
        // here (the queue drained above, the backoff below), so the next
        // holder's scan re-derives the walk from scratch. Checked on every
        // drain, so a deferred retry past a release never fires.
        if !self.fetch_lease.fetches_here() {
            self.switch_backoff = None;
            return;
        }
        let Some(decision) = decision else {
            return;
        };
        let target = crate::profile::ProfileName::from(decision.target.clone());
        let now = now_ms();

        // A vanished target (deleted out-of-process after the enqueue — the
        // queue holds a raw name this process alone owns) is DROPPED, not
        // retried: no re-login can resurrect a profile that no longer exists,
        // and attempting it would run `switch_profile`'s side effects against
        // a ghost. Read off DISK, not the in-memory config: the reload that
        // would have seen the delete runs only at the top of this tick, so a
        // delete landing after it leaves the in-memory list stale here. The
        // logline keeps the drop observable.
        let target_exists = crate::profile::is_configured(&target).unwrap_or(false);
        if !target_exists {
            logline!(
                "clauth daemon: dropping queued switch to '{target}': profile no longer exists (deleted?)"
            );
            if self
                .switch_backoff
                .as_ref()
                .is_some_and(|b| b.target == target.as_str())
            {
                self.switch_backoff = None;
            }
            return;
        }

        // The drain re-reads the target's login ON DISK, lock-free — the
        // in-memory `Profile` is stale until the next reload, and a login
        // deleted out-of-process after the enqueue must still stop the link.
        // `stored_login` answers through the same three facts as
        // `Profile::holds_login`, off the committed store, never the state
        // flock. Only the PROVEN-none outcome drops (linking it would install
        // an empty login into the global slot); an unreadable store — one that
        // exists but cannot be read or parsed — defers through the backoff
        // below, and the give-up marks the member skipped until its store reads
        // again, never linking on a guess.
        let stored = crate::profile::stored_login(&target);
        let unreadable = match stored {
            crate::profile::StoredLogin::HoldsNone => {
                logline!(
                    "clauth daemon: dropping queued switch to '{target}': it has no login or api key"
                );
                if self
                    .switch_backoff
                    .as_ref()
                    .is_some_and(|b| b.target == target.as_str())
                {
                    self.switch_backoff = None;
                }
                return;
            }
            crate::profile::StoredLogin::HoldsLogin => false,
            crate::profile::StoredLogin::Unknown => true,
        };

        // Backoff gate: if this target is inside its backoff window from a prior
        // failure, re-queue without attempting or logging — this is what turns a
        // stuck switch's 1/tick retry+log storm into a spaced, deduped one. The
        // TTL is checked here too: near the retry window's edge a capped backoff
        // step can reach past it, and gating on `not_before` alone would keep
        // requeueing a target whose window has already closed.
        if let Some(b) = &self.switch_backoff
            && b.target == target.as_str()
        {
            if now >= b.retry_until {
                if unreadable {
                    // The give-up line is the only trace of why the member
                    // leaves the walk, so it names the unreadable store whatever
                    // the last deferral was.
                    let last = if b.reason == crate::format::STORED_LOGIN_UNREADABLE {
                        String::new()
                    } else {
                        format!(" (last deferral: {})", b.reason)
                    };
                    logline!(
                        "clauth daemon: gave up switching to '{target}': {}{last}; skipping it until it reads again",
                        crate::format::STORED_LOGIN_UNREADABLE
                    );
                    self.mark_skipped_unreadable(&target);
                } else if b.reason.is_empty() {
                    // Only quiet waits deferred it (a bare-item lookup).
                    logline!(
                        "clauth daemon: gave up switching to '{target}' (clauth could not look \
                         up whose login the macOS Keychain holds in time)"
                    );
                } else {
                    logline!(
                        "clauth daemon: gave up switching to '{target}' (last deferral: {})",
                        b.reason
                    );
                }
                self.switch_backoff = None;
                return;
            }
            if now < b.not_before {
                self.requeue_quiet(&decision);
                return;
            }
        }

        // AFTER the backoff gate: a pass inside `not_before` re-queues quietly
        // above. The first two retries are immediate (`switch_backoff_ms(1..=2)`
        // is 0, so three consecutive drains each reach this defer), and only past
        // that does the widening gap make it fire once per fresh retry, never once
        // per tick.
        if unreadable {
            self.fail_switch(&decision, now, crate::format::STORED_LOGIN_UNREADABLE);
            return;
        }

        // Still mid-fetch/rotation — switching now would race the worker on the
        // single-use token/TokenList. Defer, keep retrying.
        if !is_idle(&self.activity, &target) {
            self.fail_switch(&decision, now, "target is mid-fetch");
            return;
        }

        // Outgoing active has an uncaptured re-login — the daemon can't prompt, so
        // leave it for the operator; the divergence may resolve, so keep retrying.
        let outgoing = self
            .config
            .lock()
            .ok()
            .and_then(|c| c.state.active_profile.as_ref().cloned());
        if let Some(active) = outgoing
            && active != target
            && active_diverged_unsaved(&active)
        {
            self.fail_switch(
                &decision,
                now,
                &format!(
                    "active '{active}' has unsaved credentials; {}",
                    crate::format::RESOLVE_IN_TUI
                ),
            );
            return;
        }

        // Above the AUTH-1 gate: when the target owns the bare item's login,
        // the gate would refresh the target's consumed predecessor and
        // quarantine it, while the settle saves the live successor into it
        // first. Off the loop: a `/profile` lookup it needs runs on a worker,
        // and the switch defers quietly until it answers. A refusal defers
        // like any other failure, retried inside the backoff window.
        #[cfg(target_os = "macos")]
        let _placed = match crate::claude::settle_bare_keychain_item_off_loop(
            crate::claude::BareItemWriter::Switch,
        ) {
            Ok(settled) => {
                if let Some(notice) = &settled.notice {
                    logline!("clauth daemon: {notice}");
                }
                crate::claude::BareItemPlacement::enter(settled.placed)
            }
            Err(e) if crate::claude::is_bare_item_lookup_pending(&e) => {
                self.defer_switch_quietly(&decision, now);
                return;
            }
            Err(e) => {
                self.fail_switch(&decision, now, &format!("{e:#}"));
                return;
            }
        };

        // AUTH-1 (Incident C): never install a stale/dead token. Refresh an expiring
        // target before install; a revoked token is quarantined (`auth_broken`) and
        // dropped — retrying can't help until `clauth login`. The gate does its HTTP
        // refresh with no config lock held, so it cannot wedge the run loop mid-lock.
        //
        // The spend is claimed inside the gate, under the rotation lock, right
        // before the POST: a stop signal that lands while the gate is parked on
        // the lock refuses the spend (the gate answers `Stopping`) and the
        // switch stays queued rather than failing. The claim carries this
        // instance's lease ticket, so a watchdog release in that same window
        // refuses the refresh too.
        let gate = crate::usage::SpendGate::new(
            self.shutting_down.as_ref(),
            Some(self.fetch_lease.ticket_at(generation)),
        );
        let answer = crate::oauth::ensure_installable_from_main_loop(
            &self.config,
            &target,
            crate::oauth::refresh_result,
            Some((&self.activity, gate)),
        );
        // The decision came from the queue, not from the user: a lease this
        // instance lost while the gate ran must not execute the switch — the
        // holder that took over decides. The record clears with the skip, as
        // on the guard above.
        if !self.fetch_lease.fetches_here() {
            self.switch_backoff = None;
            return;
        }
        match answer {
            crate::oauth::AuthGate::Ready | crate::oauth::AuthGate::Refreshed => {}
            crate::oauth::AuthGate::Stopping => {
                self.requeue_quiet(&decision);
                return;
            }
            crate::oauth::AuthGate::Broken => {
                // Terminal failure (drop, not retry) — clear any backoff. The
                // quarantine write is left unadopted: the next tick's reload
                // re-reads disk and rebuilds the token list with the flag.
                logline!(
                    "clauth daemon: {}",
                    crate::format::login_expired(&target).line()
                );
                self.switch_backoff = None;
                return;
            }
            crate::oauth::AuthGate::Transient(e) => {
                // The daemon log is an operator surface with no companion log of
                // its own, so it names the status like CLI stderr does.
                let reason = if e.is_login_change() {
                    e.text()
                } else {
                    format!("refresh failed transiently ({})", e.text_with_status())
                };
                self.fail_switch(&decision, now, &reason);
                return;
            }
        }

        // The config guard is taken FIRST (Config ranks outer of the state
        // flock — the order `lockorder` asserts), held across the switch, and
        // dropped before the republish below so its disk sweep runs under no
        // config guard.
        let broken_snapshot: HashMap<String, u64> = self
            .third_party_broken
            .lock()
            .map(|m| m.clone())
            .unwrap_or_default();
        let result = {
            #[allow(
                clippy::expect_used,
                reason = "config mutex poisoning is unrecoverable"
            )]
            let mut cfg = self.config.lock().expect("config poisoned");
            let key_rejected: HashSet<ProfileName> =
                current_key_rejected_names(&broken_snapshot, &cfg.profiles)
                    .into_iter()
                    .collect();
            // Revalidate under the fresh config (reloaded at the top of this
            // tick). Only a record that CARRIES a key-rejection cause can be
            // stale: a repair landing between the scan's queue and this dispatch
            // changes the active's fingerprint, so the switch-away it motivated
            // is dropped instead of moving off the just-repaired account. A
            // cause-absent (ordinary exhaustion/home) record executes even beside
            // an ambient stale mark.
            if queued_switch_away_is_stale(decision.key_rejected_cause.as_ref(), &cfg.profiles) {
                logline!(
                    "clauth daemon: dropping queued switch to '{target}': active '{}' was re-keyed",
                    decision
                        .key_rejected_cause
                        .as_ref()
                        .map(|(active, _)| active.as_str())
                        .unwrap_or("")
                );
                return;
            }
            // Destination-based: a move landing on the preferred (home) account
            // logs as a return whether the return pass or an exhaustion walk onto
            // a clear preferred put us there. Captured under the same lock so the
            // log names the two apart without threading the cause through the
            // switch action.
            let returning = cfg.is_home_today(&target, &key_rejected);
            // A delete landing between the early drop above and this hold is
            // caught by `switch_profile`'s own fresh membership gate
            // (`ensure_switch_target_ok`), which runs inside this same flock.
            crate::lock::with_state_lock(|_held| {
                // The AUTH-1 gate ran above (`ensure_installable_from_main_loop`).
                let changed = switch_profile_locked_gated(&mut cfg, &target, true)?;
                Ok((returning, changed))
            })
        };
        match result {
            Ok((returning, changed)) => {
                // Strip AFTER the flock: the strip try-onces the .claude.json
                // sync lock, ranked OUTSIDE the state flock, so it must never
                // run under the hold above, and a strip it cannot take is
                // deferred to the next reconcile (never a tick wait).
                if changed {
                    crate::claude_json::strip_home_oauth_account();
                }
                self.rebuild_tokens();
                // Drain the switch's own forgets. The write is left unadopted:
                // the next tick's reload re-reads disk, so a copy-back that
                // landed on another profile is picked up instead of swallowed.
                self.apply_forgets();
                self.switch_backoff = None;
                if returning {
                    logline!("clauth daemon: returned to preferred account '{target}'");
                } else {
                    logline!("clauth daemon: switched to '{target}'");
                }
                // The `cfg` guard is dropped at the block's end above, so this
                // republish holds no config lock. A no-op switch (target already
                // active) skips it: the feed on disk already names this account.
                if changed {
                    crate::daemon::publish_status(&self.config);
                }
            }
            Err(e) => {
                // Apply so a half-switched capture does not leave the old
                // reading in the daemon's stores.
                self.apply_forgets();
                self.fail_switch(&decision, now, &format!("switch failed: {e}"));
            }
        }
    }

    /// Handle a switch that couldn't execute (unreadable store / busy / diverged /
    /// transient / switch error). Advances exponential backoff for this target, DEDUPS the failure
    /// log (emits only when the target or reason changes — a stuck switch never
    /// logs 1/tick), and re-queues the decision for the retry window the backoff
    /// gate owns (the gate's give-up branch is the one that closes it). A change
    /// of target resets the backoff. The decision (target + cause) is re-queued
    /// whole so a later repair still invalidates it at dispatch.
    fn fail_switch(&mut self, decision: &PendingSwitchTarget, now: u64, reason: &str) {
        let target = decision.target.as_str();
        let (attempts, retry_until) = match &self.switch_backoff {
            Some(b) if b.target == target => (b.attempts + 1, b.retry_until),
            _ => (1, now.saturating_add(SWITCH_RETRY_TTL_MS)),
        };
        // Dedup: only log when the (target, reason) pair changed since last time.
        let changed = self
            .switch_backoff
            .as_ref()
            .is_none_or(|b| b.target != target || b.reason != reason);
        if changed {
            logline!("clauth daemon: deferring switch to '{target}': {reason}");
            self.switch_failure_logs += 1;
        }
        self.switch_backoff = Some(SwitchBackoff {
            target: target.to_string(),
            attempts,
            not_before: now.saturating_add(switch_backoff_ms(attempts)),
            reason: reason.to_string(),
            retry_until,
        });
        self.requeue_quiet(decision);
    }

    /// [`Self::fail_switch`] for a wait that is no failure (a bare-item
    /// lookup still running): the same backoff and retry window, logging
    /// nothing and keeping the last reason a failure gave.
    #[cfg(target_os = "macos")]
    fn defer_switch_quietly(&mut self, decision: &PendingSwitchTarget, now: u64) {
        let (attempts, retry_until, reason) = match &self.switch_backoff {
            Some(b) if b.target == decision.target => {
                (b.attempts + 1, b.retry_until, b.reason.clone())
            }
            _ => (1, now.saturating_add(SWITCH_RETRY_TTL_MS), String::new()),
        };
        self.switch_backoff = Some(SwitchBackoff {
            target: decision.target.clone(),
            attempts,
            not_before: now.saturating_add(switch_backoff_ms(attempts)),
            reason,
            retry_until,
        });
        self.requeue_quiet(decision);
    }

    /// Re-queue a retry decision. A NEWER target that arrived since the drain
    /// supersedes it — the scheduler's later decision wins. No logging — the
    /// backoff/dedup path owns the observability.
    fn requeue_quiet(&mut self, decision: &PendingSwitchTarget) {
        if let Ok(mut q) = self.pending_switch.lock()
            && q.is_empty()
        {
            q.insert(decision.clone());
        }
    }

    /// Mark a member whose stored login stays unreadable as skipped, so the
    /// auto-switch walk passes over it until the scan's lock-free re-read drops
    /// the name. In-memory only, never persisted.
    fn mark_skipped_unreadable(&self, target: &crate::profile::ProfileName) {
        if let Ok(mut s) = self.skipped_unreadable.lock() {
            s.insert(target.as_str().to_string());
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
        // A switch-off queued under a ticket this instance no longer holds
        // must not execute — the holder that took over decides. The flag was
        // consumed above, so the record clears with the skip.
        if !self.fetch_lease.fetches_here() {
            return;
        }
        let active = self
            .config
            .lock()
            .ok()
            .and_then(|c| c.state.active_profile.as_ref().cloned());
        let Some(active) = active else {
            return; // already off
        };
        if active_diverged_unsaved(&active) {
            logline!(
                "clauth daemon: skipping switch-off: active '{active}' has unsaved credentials"
            );
            return;
        }
        // Settled before the guard and the flock, off the loop, as in
        // `drain_pending_switch`. A pending lookup or a refusal drops this
        // switch-off: the scheduler's next scan queues it again while the
        // chain stays spent, and settles off the memoized lookup then.
        #[cfg(target_os = "macos")]
        let _placed = match crate::claude::settle_bare_keychain_item_off_loop(
            crate::claude::BareItemWriter::SignOut,
        ) {
            Ok(settled) => {
                if let Some(notice) = &settled.notice {
                    logline!("clauth daemon: {notice}");
                }
                crate::claude::BareItemPlacement::enter(settled.placed)
            }
            Err(e) if crate::claude::is_bare_item_lookup_pending(&e) => return,
            Err(e) => {
                logline!("clauth daemon: switch-off failed: {e:#}");
                return;
            }
        };
        // Same guard-then-flock shape as `drain_pending_switch`: the guard is
        // taken first, held across the locked switch-off, and dropped before
        // the gated republish below.
        let result = {
            #[allow(
                clippy::expect_used,
                reason = "config mutex poisoning is unrecoverable"
            )]
            let mut cfg = self.config.lock().expect("config poisoned");
            crate::lock::with_state_lock(|_held| {
                let changed = switch_off_locked(&mut cfg)?;
                Ok(changed)
            })
        };
        match result {
            Ok(changed) => {
                if changed {
                    crate::claude_json::strip_home_oauth_account();
                }
                self.rebuild_tokens();
                // Drain the switch-off's own forgets; the write is left
                // unadopted, so the next tick's reload re-reads disk.
                self.apply_forgets();
                logline!("clauth daemon: switched off: all accounts spent");
                if changed {
                    crate::daemon::publish_status(&self.config);
                }
            }
            Err(e) => {
                // Apply so the dropped login's reading does not survive in the
                // daemon's stores.
                self.apply_forgets();
                if !crate::actions::switch_off_hold_said(&e) {
                    logline!("clauth daemon: switch-off failed: {e}");
                }
            }
        }
    }
}
