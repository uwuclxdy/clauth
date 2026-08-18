//! The daemon's per-tick work, extracted from [`super::Daemon::run`]'s loop so it
//! is drivable in a test without the `sleep`/watchdog wrapper.
//!
//! [`Daemon::tick`](super::Daemon::tick) is one iteration of the run loop:
//! reload external config changes, execute any queued auto-switch / switch-off,
//! then rewrite `status.json`. The drains and the token-list rebuild live here;
//! `write_status` and the loop scaffolding stay in `mod.rs`.
//! `tests/inline/daemon_mod.rs` pins each drain's behavior against this seam.

use std::sync::atomic::Ordering;

use crate::actions::{switch_off, switch_profile};
use crate::logline::logline;
use crate::profile::{load_config, reload_fingerprint};
use crate::usage::{collect_third_party_entries, collect_tokens, is_idle, now_ms};

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
        self.reload_if_changed();
        self.drain_pending_switch();
        self.drain_pending_switch_off();
        self.write_status();
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

    /// Execute the queued switch. Drains the target set atomically and attempts
    /// the pick. A target that can't land THIS tick — still mid-fetch/rotation,
    /// outgoing active has unsaved diverged credentials (the daemon can't
    /// prompt), or a transient refresh failure — is RE-QUEUED until it lands or
    /// its retry window closes, rather than dropped after one attempt. Every
    /// skip/failure logs once per distinct reason (see [`SwitchBackoff`]).
    pub(super) fn drain_pending_switch(&mut self) {
        // The set holds at most one scheduler target in practice
        // (`scan_auto_switch` skips while one is pending); `min` keeps the
        // pick deterministic anyway.
        let target: Option<String> = self
            .pending_switch
            .lock()
            .map(|mut g| {
                let t = g.iter().min().cloned();
                g.clear();
                t
            })
            .unwrap_or_default();
        let Some(target) = target else {
            return;
        };
        let target = crate::profile::ProfileName::from(target);
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
                logline!(
                    "clauth daemon: gave up switching to '{target}': {}",
                    b.reason
                );
                self.switch_backoff = None;
                return;
            }
            if now < b.not_before {
                self.requeue_quiet(&target);
                return;
            }
        }

        // Still mid-fetch/rotation — switching now would race the worker on the
        // single-use token/TokenList. Defer, keep retrying.
        if !is_idle(&self.activity, &target) {
            self.fail_switch(&target, now, "target is mid-fetch");
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
                &target,
                now,
                &format!(
                    "active '{active}' has unsaved credentials; {}",
                    crate::format::RESOLVE_IN_TUI
                ),
            );
            return;
        }

        // AUTH-1 (Incident C): never install a stale/dead token. Refresh an expiring
        // target before install; a revoked token is quarantined (`auth_broken`) and
        // dropped — retrying can't help until `clauth login`. The gate does its HTTP
        // refresh with no config lock held, so it cannot wedge the run loop mid-lock.
        match crate::oauth::ensure_installable(&self.config, &target, crate::oauth::refresh_result)
        {
            crate::oauth::AuthGate::Ready | crate::oauth::AuthGate::Refreshed => {}
            crate::oauth::AuthGate::Broken => {
                // The gate persisted `auth_broken`; adopt that fingerprint so the
                // next tick's reload doesn't treat our own write as external.
                // Terminal failure (drop, not retry) — clear any backoff.
                self.last_reload_fp = reload_fingerprint();
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
                self.fail_switch(
                    &target,
                    now,
                    &format!("refresh failed transiently ({})", e.text_with_status()),
                );
                return;
            }
        }

        // Hold the state flock across the switch AND the post-write fingerprint
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
            // Destination-based: a move landing on the preferred (home) account
            // logs as a return whether the return pass or an exhaustion walk onto
            // a clear preferred put us there. Captured under the same lock so the
            // log names the two apart without threading the cause through the
            // switch action.
            let returning = cfg.find(&target).is_some_and(|p| p.preferred);
            // A delete landing between the early drop above and this hold is
            // caught by `switch_profile`'s own fresh membership gate
            // (`ensure_switch_target_ok`), which runs inside this same flock.
            crate::lock::with_state_lock(|_held| {
                switch_profile(&mut cfg, &target)?;
                Ok((reload_fingerprint(), returning))
            })
        };
        match result {
            Ok((fp, returning)) => {
                self.rebuild_tokens();
                self.last_reload_fp = fp;
                self.switch_backoff = None;
                if returning {
                    logline!("clauth daemon: returned to preferred account '{target}'");
                } else {
                    logline!("clauth daemon: switched to '{target}'");
                }
            }
            Err(e) => {
                self.fail_switch(&target, now, &format!("switch failed: {e}"));
            }
        }
    }

    /// Handle a switch that couldn't execute (busy / diverged / transient / switch
    /// error). Advances exponential backoff for this target, DEDUPS the failure
    /// log (emits only when the target or reason changes — a stuck switch never
    /// logs 1/tick), and re-queues the target until its retry window closes.
    /// A change of target resets the backoff.
    fn fail_switch(&mut self, target: &crate::profile::ProfileName, now: u64, reason: &str) {
        let (attempts, retry_until) = match &self.switch_backoff {
            Some(b) if b.target == target.as_str() => (b.attempts + 1, b.retry_until),
            _ => (1, now.saturating_add(SWITCH_RETRY_TTL_MS)),
        };
        // Dedup: only log when the (target, reason) pair changed since last time.
        let changed = self
            .switch_backoff
            .as_ref()
            .is_none_or(|b| b.target != target.as_str() || b.reason != reason);
        if changed {
            logline!("clauth daemon: deferring switch to '{target}': {reason}");
            self.switch_failure_logs += 1;
        }
        if now < retry_until {
            self.switch_backoff = Some(SwitchBackoff {
                target: target.to_string(),
                attempts,
                not_before: now.saturating_add(switch_backoff_ms(attempts)),
                reason: reason.to_string(),
                retry_until,
            });
            self.requeue_quiet(target);
        } else {
            // Retry window closed — give up and stop tracking this target.
            logline!("clauth daemon: gave up switching to '{target}': {reason}");
            self.switch_backoff = None;
        }
    }

    /// Re-queue a retry target. A NEWER target that arrived since the drain
    /// supersedes it — the scheduler's later decision wins. No logging — the
    /// backoff/dedup path owns the observability.
    fn requeue_quiet(&mut self, target: &crate::profile::ProfileName) {
        if let Ok(mut q) = self.pending_switch.lock()
            && q.is_empty()
        {
            q.insert(target.to_string());
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
        // Same flock-held fingerprint capture as `drain_pending_switch`.
        let result = {
            #[allow(
                clippy::expect_used,
                reason = "config mutex poisoning is unrecoverable"
            )]
            let mut cfg = self.config.lock().expect("config poisoned");
            crate::lock::with_state_lock(|_held| {
                switch_off(&mut cfg)?;
                Ok(reload_fingerprint())
            })
        };
        match result {
            Ok(fp) => {
                self.rebuild_tokens();
                self.last_reload_fp = fp;
                logline!("clauth daemon: switched off: all accounts spent");
            }
            Err(e) => logline!("clauth daemon: switch-off failed: {e}"),
        }
    }
}
