//! Registry of live `clauth start` sessions.
//!
//! One file per session at `~/.clauth/live_sessions/<sid>.json`, mirroring the
//! `~/.clauth/jobs/` convention ([`crate::mcp::jobs`]): a row is keyed by a
//! session id nobody else writes, so the file needs no ownership arbitration of
//! its own. Rows are filed by SESSION, never under the profile they launched on
//! — a session that swaps member would be misfiled the moment it moved.
//!
//! Two writers share a row, and both constraints they need are structural rather
//! than written down:
//!
//! - **The read is inside the lock, not just the write.** [`update`] is the only
//!   mutation path and it loads a FRESH row under [`with_state_lock`], hands the
//!   caller a borrow that cannot outlive the hold, and stores before releasing.
//!   There is deliberately no public load/store pair: a row read before a swap
//!   and written after would silently revert whatever the other writer put there
//!   in between.
//! - **Field ownership is a type, not a comment.** The daemon reaches its two
//!   fields through [`DaemonFields`] and the session its two through
//!   [`SessionFields`]; neither view can name the other's. Each still stores the
//!   whole freshly-loaded row, so writing one side preserves the other's.
//!
//! Liveness is the session's flock, exactly as for its runtime tree: a row is
//! dead once the marker named by its own `start_profile` + `isolated` +
//! `session_id` is no longer held, and `runtime::gc_stale_runtimes` reaps it
//! there (that module owns the marker layout; this one never rebuilds it).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::lock::with_state_lock;
use crate::profile::{AppConfig, atomic_write_600, clauth_dir, mkdir_700};
use crate::runtime::{SessionId, is_session_id};

/// One live session's row. Every field is written by exactly one of the two
/// writers; which one is enforced by the mutator views, not by this listing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct LiveSession {
    pub(crate) session_id: String,
    pub(crate) start_profile: String,
    /// Which harness's session this row describes. Codex rows count in the
    /// live tally and gate delete/disable/rotation exactly like claude ones —
    /// the registry is harness-blind everywhere it counts — but the swap
    /// executor and the daemon's per-session decision leg skip them (codex
    /// reads `auth.json` once at start, so a mid-session member change is a
    /// no-op the executor would publish as a success). `serde(default)` (=
    /// claude) is the upgrade gate: a row written by a clauth that predates
    /// the axis is a claude row, which is what it was.
    #[serde(default)]
    pub(crate) harness: crate::harness::Harness,
    pub(crate) pid: u32,
    pub(crate) started_at: u64,
    pub(crate) cwd: Option<PathBuf>,
    pub(crate) isolated: bool,
    /// Whether this session follows the shared fallback chain. Set once at
    /// registration and never mutated, so it needs no view of its own — which is
    /// also why it is safe for the daemon's decision leg to READ it while the
    /// session owns it. `serde(default)` is the upgrade gate: a row written by a
    /// clauth that predates the field must read as opted OUT, or the decision leg
    /// would move every already-running session off its launch account.
    #[serde(default)]
    pub(crate) follows_chain: bool,
    /// Daemon-owned: the member the decision leg wants this session on.
    #[serde(default)]
    pub(crate) intended_member: Option<String>,
    /// Daemon-owned: this session's position in the shared `fallback_chain`.
    #[serde(default)]
    pub(crate) chain_cursor: Option<usize>,
    /// Session-owned: the member this session's credential link resolves to.
    #[serde(default)]
    pub(crate) current_member: Option<String>,
    /// Session-owned: when this session last executed a swap.
    #[serde(default)]
    pub(crate) last_swap_at: Option<u64>,
    /// The credential source this session LAUNCHED on, as an absolute path.
    /// Set once at registration from the same value the runtime tree was built
    /// from, never mutated.
    ///
    /// A path rather than a decoded verdict, deliberately. What the rotation
    /// gate needs to know is whether this session is holding something
    /// rotatable, and the CONTENT at this path can change under a running
    /// session — `claude::heal_misfilled_sidecar` exists precisely because a
    /// rotating pair can land in a `session-token.json`. Freezing a boolean
    /// here would keep answering "refresh-less" while the file the session
    /// actually reads holds a live chain, so the test is made at rotation time
    /// against this path (`runtime::live_session_holds_rotatable`).
    ///
    /// `serde(default)` is the upgrade gate and the fail-closed direction at
    /// once: a row written by a clauth that predates the field reads `None`,
    /// which every consumer must treat as "assume rotatable", so the macOS
    /// rotation refusal keeps applying to it exactly as it does today.
    #[serde(default)]
    pub(crate) launch_store: Option<PathBuf>,
}

impl LiveSession {
    /// A row for a session starting now. Pid, start time, and cwd are read here
    /// rather than passed in, so every registration reports them the same way.
    pub(crate) fn starting(
        session_id: &SessionId,
        start_profile: &str,
        harness: crate::harness::Harness,
        isolated: bool,
        follows_chain: bool,
        launch_store: Option<PathBuf>,
    ) -> Self {
        Self {
            session_id: session_id.as_str().to_string(),
            start_profile: start_profile.to_string(),
            harness,
            pid: std::process::id(),
            started_at: crate::usage::now_ms(),
            cwd: std::env::current_dir().ok(),
            isolated,
            follows_chain,
            intended_member: None,
            chain_cursor: None,
            current_member: None,
            last_swap_at: None,
            launch_store,
        }
    }
}

/// The daemon's view of a row under [`update_as_daemon`]: the decision fields
/// and nothing else.
pub(crate) struct DaemonFields<'a>(&'a mut LiveSession);

impl DaemonFields<'_> {
    pub(crate) fn set_intended_member(&mut self, member: impl Into<String>) {
        self.0.intended_member = Some(member.into());
    }

    pub(crate) fn set_chain_cursor(&mut self, cursor: usize) {
        self.0.chain_cursor = Some(cursor);
    }
}

/// The session's view of a row under [`update_as_session`]: the execution fields
/// and nothing else.
pub(crate) struct SessionFields<'a>(&'a mut LiveSession);

impl SessionFields<'_> {
    pub(crate) fn set_current_member(&mut self, member: impl Into<String>) {
        self.0.current_member = Some(member.into());
    }

    pub(crate) fn set_last_swap_at(&mut self, at: u64) {
        self.0.last_swap_at = Some(at);
    }
}

/// Live sessions tallied by the account each one is CURRENTLY running as.
///
/// Built from the registry rather than from per-profile marker counts: a session
/// that swapped A→B holds B's marker AND keeps A's (nothing can observe the live
/// child dropping A's tokens, so A must not rotate), which makes a marker sum
/// report one child as two sessions on two accounts.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct LiveTally(std::collections::BTreeMap<String, MemberSessions>);

/// One account's slice of a [`LiveTally`]. All-zero for an account hosting none.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct MemberSessions {
    pub(crate) sessions: usize,
    /// How many of `sessions` the fallback chain is allowed to move.
    pub(crate) following: usize,
    /// The newest swap ONTO this account. `None` when no session here has ever
    /// swapped, which is also what says no `current_member` pickup lag applies.
    pub(crate) last_swap_at: Option<u64>,
}

impl LiveTally {
    /// Read the registry and drop rows whose session is gone. Row GC runs from
    /// `runtime::gc_stale_runtimes` at daemon STARTUP, not per tick, so a
    /// SIGKILLed session's row outlives it for the whole daemon run. Gates on
    /// the same predicate the decision leg does, so a row cannot be live for one
    /// and dead for the other.
    pub(crate) fn collect(config: &AppConfig) -> Self {
        let mut tally = Self::from_live_rows(list().into_iter().filter(|row| {
            let probe = crate::profile::ProfileName::from(
                row.current_member.as_deref().unwrap_or(&row.start_profile),
            );
            crate::runtime::session_row_is_live(&probe, row.isolated, &row.session_id)
        }));
        tally.add_bare_sessions(config);
        tally
    }

    /// Fold in the BARE `claude` sessions — started without `clauth start`, so
    /// they read the `~/.claude/.credentials.json` link clauth owns and burn the
    /// account it resolves to. They hold no registry row on purpose: a row reaches
    /// the daemon's swap-decision leg, which reads [`list`] directly, and that leg
    /// may only move sessions clauth supervises. Counting them HERE is what keeps
    /// them a display fact. What the count is actually taken from, and how loosely
    /// it stands in for a `claude` count, is
    /// [`crate::runtime::live_bare_sessions`].
    ///
    /// Attribution is resolved at READ time and never stored: `clauth switch` and
    /// the fallback chain both repoint that one shared link mid-session. It also
    /// reads the link rather than `active_profile`, which under a divergence names
    /// an account the bare session does not authenticate as.
    ///
    /// They count as `following` as well, because the chain genuinely moves them:
    /// a global auto-switch repoints the link and Claude Code re-reads it.
    ///
    /// An unreadable marker dir counts as ZERO — the OPPOSITE direction to
    /// [`crate::runtime::has_live_session`], which gates delete, disable,
    /// rename and rotation and so must read an unknown as live. This tally
    /// only renders, so
    /// folding an unknown in as live would put a session on screen that nothing
    /// produced.
    fn add_bare_sessions(&mut self, config: &AppConfig) {
        let bare = crate::runtime::live_bare_sessions().unwrap_or(0);
        if bare == 0 {
            return;
        }
        let Some((member, _)) = crate::which::resolve_global(config) else {
            return;
        };
        let slot = self.0.entry(member).or_default();
        slot.sessions += bare;
        slot.following += bare;
    }

    /// Tally rows already known to be live. Attribution is `current_member`,
    /// which the executor writes only on a session's FIRST swap — so a session
    /// that never moved (every pinned one, and every follower before it swaps)
    /// is still running as the account it launched on.
    fn from_live_rows(rows: impl IntoIterator<Item = LiveSession>) -> Self {
        let mut per_member: std::collections::BTreeMap<String, MemberSessions> =
            std::collections::BTreeMap::new();
        for row in rows {
            let member = row.current_member.unwrap_or(row.start_profile);
            let slot = per_member.entry(member).or_default();
            slot.sessions += 1;
            slot.following += usize::from(row.follows_chain);
            slot.last_swap_at = slot.last_swap_at.max(row.last_swap_at);
        }
        Self(per_member)
    }

    /// A tally straight from rows, for tests in other modules that need a fleet
    /// without laying registry files down. Skips the liveness filter, which is
    /// the half [`LiveTally::collect`] adds and this module's own tests pin.
    #[cfg(test)]
    pub(crate) fn of(rows: impl IntoIterator<Item = LiveSession>) -> Self {
        Self::from_live_rows(rows)
    }

    /// One account's sessions.
    pub(crate) fn member(&self, name: &crate::profile::ProfileName) -> MemberSessions {
        self.0.get(name.as_str()).copied().unwrap_or_default()
    }
}

fn registry_dir() -> Result<PathBuf> {
    Ok(clauth_dir()?.join("live_sessions"))
}

/// Path of one session's row. The id shape is validated first: `list` reads ids
/// back off disk and the daemon's decision leg passes them around, so this join
/// must never take a `..` or a separator from a file someone else wrote.
fn row_path(session_id: &str) -> Result<PathBuf> {
    anyhow::ensure!(
        is_session_id(session_id),
        "not a session id: {session_id:?}"
    );
    Ok(registry_dir()?.join(format!("{session_id}.json")))
}

/// Owner-only like every `~/.clauth` write: a row carries the session's cwd and
/// which account it is running as.
fn write_row(row: &LiveSession) -> Result<()> {
    let path = row_path(&row.session_id)?;
    if let Some(parent) = path.parent() {
        mkdir_700(parent).with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let bytes = serde_json::to_vec(row)?;
    atomic_write_600(&path, &bytes).with_context(|| format!("failed to write {}", path.display()))
}

/// File a starting session's row. Called once the session's liveness marker is
/// flock-held, so a row never exists without something for GC to test it by.
pub(crate) fn register(row: &LiveSession) -> Result<()> {
    with_state_lock(|_held| write_row(row))
}

/// Drop a session's row. Idempotent — a row already reaped by GC is not an
/// error. Takes the state lock so it cannot land between an [`update`]'s load and
/// its store and leave the row resurrected.
pub(crate) fn unregister(session_id: &str) -> Result<()> {
    let path = row_path(session_id)?;
    with_state_lock(|_held| match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("failed to remove {}", path.display())),
    })
}

/// Snapshot of every registered row. Read-only: the returned rows are owned
/// copies, so nothing a caller does to one reaches disk. An unreadable or
/// unparseable file is skipped rather than failing the sweep.
pub(crate) fn list() -> Vec<LiveSession> {
    let Ok(dir) = registry_dir() else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| read_row(&entry.path()))
        .collect()
}

fn read_row(path: &Path) -> Option<LiveSession> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

/// One session's own row, or `None` when it is absent or unparseable. Read
/// WITHOUT the state lock, which is sound because [`write_row`] renames over the
/// path: a reader sees the whole old row or the whole new one. A caller that
/// intends to WRITE what it read must go through [`update`] instead — a
/// load-here/store-later pair reverts whatever the other writer landed in
/// between.
pub(crate) fn get(session_id: &str) -> Option<LiveSession> {
    read_row(&row_path(session_id).ok()?)
}

/// The one mutation path: load a FRESH row inside the state lock, edit it
/// through a borrow that cannot escape the hold, store it before releasing. A
/// missing row is an error naming the id, never a silent no-op.
fn update(session_id: &str, edit: impl FnOnce(&mut LiveSession)) -> Result<()> {
    let path = row_path(session_id)?;
    with_state_lock(|_held| {
        let bytes = std::fs::read(&path)
            .with_context(|| format!("no live-session row for {session_id}"))?;
        let mut row: LiveSession = serde_json::from_slice(&bytes)
            .with_context(|| format!("unreadable live-session row for {session_id}"))?;
        edit(&mut row);
        write_row(&row)
    })
}

/// Edit the daemon-owned fields of one row. The session's own fields are carried
/// through untouched by construction: the row is reloaded here, not supplied.
pub(crate) fn update_as_daemon(
    session_id: &str,
    edit: impl FnOnce(&mut DaemonFields<'_>),
) -> Result<()> {
    update(session_id, |row| edit(&mut DaemonFields(row)))
}

/// Edit the session-owned fields of one row. Mirror of [`update_as_daemon`].
pub(crate) fn update_as_session(
    session_id: &str,
    edit: impl FnOnce(&mut SessionFields<'_>),
) -> Result<()> {
    update(session_id, |row| edit(&mut SessionFields(row)))
}

#[cfg(test)]
#[path = "../tests/inline/live_sessions.rs"]
mod tests;
