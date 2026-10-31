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
//!   fields through [`DaemonFields`] and the session its execution fields
//!   through [`SessionFields`]; neither view can name the other's. Each
//!   still stores the whole freshly-loaded row, so writing one side preserves
//!   the other's.
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
use crate::runtime::{LaunchTransport, SessionId, is_session_id};

/// Who launched a session. The end user's own sessions — `clauth start` and
/// codex — are [`Launcher::Own`]; a one-shot headless run the clauth MCP
/// server spawned for a `delegate` request is [`Launcher::Delegate`]. Carried
/// into registration the way `follows_chain` is, so the mark is on the row
/// before any tally can read it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Launcher {
    Own,
    Delegate,
}

/// One live session's row. Every field is written by exactly one of the two
/// writers; which one is enforced by the mutator views, not by this listing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct LiveSession {
    pub(crate) session_id: String,
    pub(crate) start_profile: String,
    /// Which harness's session this row describes. Liveness gating reads the
    /// flock-held markers under the profile's `sessions*` dirs, whichever
    /// harness stamped them (the macOS rotation gate also reads each live
    /// marker's row, but only `background_hold` and `launch_store`), and the
    /// tally keys on the name, which one namespace keeps unambiguous; both stay
    /// harness-blind with no help from this field. The tag exists for the
    /// consumers that must tell rows APART: the swap executor and the daemon's
    /// per-session decision leg skip codex rows when those sessions exist
    /// (codex reads `auth.json` once at start, so a mid-session member change
    /// is a no-op the executor would publish as a success).
    /// `serde(default)` (= claude) is the upgrade gate: a row written by a
    /// clauth that predates the axis is a claude row, which is what it was.
    #[serde(default)]
    pub(crate) harness: crate::harness::Harness,
    pub(crate) pid: u32,
    /// The spawned `claude` child's pid, written by `clauth start` once the
    /// child exists, in the same update as `launch_families`. `None` until
    /// then, and for a row written by a clauth that predates the field (or a
    /// delegate row): such a row carries neither field, so the scheduler can
    /// join no spend-cap 429 to it and reads no launch demand off it, and no
    /// credit-cap trigger moves it. `serde` omits the key when absent, so a
    /// row the older clauth wrote round-trips byte-identical.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) claude_pid: Option<u32>,
    /// The launch demand's resolved model families, written by `clauth start`
    /// in the same update as `claude_pid`. `None` exactly when `claude_pid` is
    /// (see there). `serde` omits the key when absent, so an older row
    /// round-trips byte-identical.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) launch_families: Option<Vec<String>>,
    pub(crate) started_at: u64,
    #[serde(serialize_with = "serialize_cwd_lossy")]
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
    /// Daemon-owned: whether `intended_member` was chosen for a credit-cap leave
    /// (a credit-billed launch on a capped account, or the session's own
    /// spend-cap 429) — the one leave that may land on a member whose `[models]`
    /// differ. `serde(default)` keeps a row written by an older clauth or a
    /// manual `clauth switch` reading false, so every such move keeps the
    /// models-equal rule. Serialized absent when false.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub(crate) credit_leave: bool,
    /// Session-owned: whether the `clauth start` that launched this session
    /// understands the credit-leave reason (a same-binary capability written in
    /// the same update as `launch_families`). The daemon merges members whose
    /// `[models]` differ only for rows carrying this flag: a session still
    /// running an older binary reads `false` and keeps the strict chain, so a
    /// relaxed intent is never written where its executor would refuse it.
    /// `serde(default)` is the upgrade gate; serialized absent when false.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub(crate) credit_leave_aware: bool,
    /// Daemon-owned: this session's position in the shared `fallback_chain`.
    #[serde(default)]
    pub(crate) chain_cursor: Option<usize>,
    /// Session-owned: the member this session's credential link resolves to.
    #[serde(default)]
    pub(crate) current_member: Option<String>,
    /// Session-owned: every member the session has run on, in visit order, the
    /// launch member first. Seeded by [`LiveSession::starting`] with the launch
    /// member. The swap executor appends the incoming member in its own row
    /// update before the link moves, then relinks the tree at that member's
    /// store, and only then writes [`current_member`](Self::current_member) in
    /// a second row update whose failure is logged and not raised. A swap that
    /// dies between the two updates, or whose `current_member` write fails,
    /// leaves this list naming a member `current_member` does not.
    /// The live Claude Code child keeps every refresh token it was ever handed,
    /// so a handed-off row keeps EVERY member in this list live, not only the
    /// one it runs on now. `serde(default)` is the upgrade gate (a row written
    /// by a clauth that predates the field reads empty), and an empty list
    /// serializes absent so such a row round-trips byte-identical.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) visited_members: Vec<String>,
    /// Session-owned: when this session last executed a swap.
    #[serde(default)]
    pub(crate) last_swap_at: Option<u64>,
    /// The credential store this session's rotation verdict reads, as an
    /// absolute path. Seeded at registration from the same value the runtime
    /// tree was built from, then repointed by every swap onto the member the
    /// session then reads (on macOS, only once the swap's keychain legs have
    /// landed): the verdict must answer for the member the session HOLDS, or a
    /// session swapped onto a refreshless member keeps refusing the launch
    /// member's rotations.
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
    /// The transport the session booted with, recorded on every claude row as the
    /// exact value the executor snapshots, and `None` on codex rows (codex never
    /// swaps through this executor). The daemon's per-session decision leg judges
    /// members against it; a row written by an older clauth carries `None`, and
    /// the leg reconstructs the transport from the start profile as it reads now.
    /// Holds a digest of the `[env]` map, never the map itself, so no env VALUE
    /// (an inference credential may live there) is written to the row.
    #[serde(default)]
    pub(crate) boot_transport: Option<LaunchTransport>,
    /// Whether this row is a delegate: a one-shot headless run the clauth MCP
    /// server spawned. Set once at registration and never mutated, so the first
    /// read of the row already carries it — no tally reads a delegate row as an
    /// own session first. `serde(default)` is the upgrade gate: a row written
    /// by an older clauth reads as own until that server restarts.
    #[serde(default)]
    pub(crate) is_delegate: bool,
    /// Whether this row holds the account's fixed background pair
    /// (`runtime-bg`/`sessions-bg`) rather than a per-session tree. Set once at
    /// registration and never mutated; callers resolving the row's paths branch
    /// on it (its marker and tree dirs are the pair's, not `<sid>`-keyed).
    /// `serde(default)` is the upgrade gate: a row from an older clauth is a
    /// per-session row, which is what it was.
    #[serde(default)]
    pub(crate) background: bool,
    /// Whether this row is the DAEMON's hold of a tree it adopted — the fixed
    /// background pair, or a handed-off `--with-fallback` per-launch tree — as
    /// opposed to a `clauth start` launcher's row. The daemon holds a tree
    /// exactly while a Claude Code job names it, so the macOS rotation gate's
    /// marker term skips it and the job term decides the tree; the pid joins
    /// skip it because the hold is not a pane's process. An adopted per-launch
    /// row keeps `background` false, so it still resolves its per-session tree
    /// and stays switchable. `serde(default)` is the upgrade gate.
    #[serde(default)]
    pub(crate) background_hold: bool,
}

/// Serializes [`LiveSession::cwd`] as its lossy UTF-8 form, so a session whose
/// working directory is not UTF-8 still registers and lists (a row is JSON,
/// which is UTF-8; the lossy form is all it can hold).
fn serialize_cwd_lossy<S: serde::Serializer>(
    cwd: &Option<PathBuf>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    match cwd {
        Some(path) => {
            let lossy = path.to_string_lossy();
            serializer.serialize_str(&lossy)
        }
        None => serializer.serialize_none(),
    }
}

impl LiveSession {
    /// A row for a session starting now. Pid, start time, and cwd are read here
    /// rather than passed in, so every registration reports them the same way.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn starting(
        session_id: &SessionId,
        start_profile: &str,
        harness: crate::harness::Harness,
        isolated: bool,
        follows_chain: bool,
        launcher: Launcher,
        launch_store: Option<PathBuf>,
        boot_transport: Option<LaunchTransport>,
    ) -> Self {
        Self {
            session_id: session_id.as_str().to_string(),
            start_profile: start_profile.to_string(),
            harness,
            pid: std::process::id(),
            claude_pid: None,
            launch_families: None,
            started_at: crate::usage::now_ms(),
            cwd: std::env::current_dir().ok(),
            isolated,
            follows_chain,
            intended_member: None,
            chain_cursor: None,
            credit_leave: false,
            credit_leave_aware: false,
            current_member: None,
            last_swap_at: None,
            launch_store,
            boot_transport,
            is_delegate: matches!(launcher, Launcher::Delegate),
            background: false,
            background_hold: false,
            visited_members: vec![start_profile.to_string()],
        }
    }

    /// Whether the session this row names still runs: `current_member` first
    /// (where a swapped session holds its marker) then `start_profile` (where a
    /// session that never moved lives). One predicate, so a row can never be
    /// live for one consumer and dead for another.
    pub(crate) fn marker_is_held(&self) -> bool {
        let probe = crate::profile::ProfileName::from(
            self.current_member
                .as_deref()
                .unwrap_or(&self.start_profile),
        );
        crate::runtime::session_row_is_live(&probe, self.isolated, &self.session_id)
    }

    /// Whether this row is the daemon's hold of the account's FIXED background
    /// pair — the tree that exists while a Claude Code job names that account,
    /// one hold however many jobs, and ends with the last one. It is no `clauth
    /// start` session, so counting it would put a phantom session on screen and
    /// in the stop-shunt confirm. An adopted per-launch `--with-fallback` hold
    /// keeps `background` false and counts as the one real session it is; a
    /// launcher's own background row carries no hold flag at all.
    fn is_fixed_background_hold(&self) -> bool {
        self.background_hold && self.background
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

    pub(crate) fn set_credit_leave(&mut self, on: bool) {
        self.0.credit_leave = on;
    }
}

/// The session's view of a row under [`update_as_session`]: the execution fields
/// and nothing else.
pub(crate) struct SessionFields<'a>(&'a mut LiveSession);

impl SessionFields<'_> {
    pub(crate) fn set_current_member(&mut self, member: impl Into<String>) {
        self.0.current_member = Some(member.into());
    }

    /// Append a member to the session's visit history, in visit order. A
    /// re-visit does not duplicate. The swap executor calls this in the same
    /// update as [`set_current_member`](Self::set_current_member), so the
    /// history always contains the member the row names.
    pub(crate) fn record_visited_member(&mut self, member: impl Into<String>) {
        let member = member.into();
        if !self.0.visited_members.contains(&member) {
            self.0.visited_members.push(member);
        }
    }

    /// Record that this session's launcher understands the credit-leave reason.
    /// Written by `clauth start` in the same update as `launch_families`; an
    /// older launcher leaves the field absent, and the daemon reads that as
    /// false.
    pub(crate) fn set_credit_leave_aware(&mut self) {
        self.0.credit_leave_aware = true;
    }

    pub(crate) fn set_last_swap_at(&mut self, at: u64) {
        self.0.last_swap_at = Some(at);
    }

    /// Point the row's rotation verdict at the store the session reads now.
    /// The swap executor calls this once that is settled — inside its state-flock
    /// row update off macOS, after the keychain legs on macOS (the session's
    /// Claude Code resolves the item first, so the store it reads moves only
    /// when those legs land).
    pub(crate) fn set_launch_store(&mut self, store: PathBuf) {
        self.0.launch_store = Some(store);
    }

    /// Re-key the row onto the process that IS the session. A delegate's row is
    /// registered by the `clauth mcp` that spawns it — `std::process::id()` at
    /// register time reads the mcp, not the delegate child — and the herdr
    /// pane-tag walk joins rows to processes by pid, so a row keyed on the mcp
    /// names a delegate's account for the pane hosting its parent session.
    pub(crate) fn set_pid(&mut self, pid: u32) {
        self.0.pid = pid;
    }

    /// Record the directory the session's child actually runs in. A delegate's
    /// row is registered by the `clauth mcp` that spawns it, whose cwd is the
    /// server's, not the child's; the re-key that moves the pid onto the child
    /// must move the cwd with it, or a surface reading the row (`GET /panes`,
    /// `clauth sessions --live`) reports where the server sat until the child's
    /// own record names its dir.
    pub(crate) fn set_cwd(&mut self, cwd: impl Into<PathBuf>) {
        self.0.cwd = Some(cwd.into());
    }

    /// Record the spawned `claude` child's pid, once `clauth start` has it. The
    /// daemon's join of the session's own spend-cap 429 keys off this: it reads
    /// the child's session record and matches its `sessionId` to a transcript
    /// conversation id.
    pub(crate) fn set_claude_pid(&mut self, pid: u32) {
        self.0.claude_pid = Some(pid);
    }

    /// Record the launch demand's resolved families, written by `clauth start`
    /// beside `claude_pid`. The daemon's credit-billed leave check reads this,
    /// never re-deriving the demand from today's config (the launch env and
    /// args are gone by the time the daemon scans).
    pub(crate) fn set_launch_families(&mut self, families: Vec<String>) {
        self.0.launch_families = Some(families);
    }
}

/// Live sessions tallied by the account each one is CURRENTLY running as.
///
/// Built from the registry rather than from per-profile marker counts: a session
/// that swapped A→B holds B's marker AND keeps A's (nothing can observe the live
/// child dropping A's tokens, so A must not rotate), which makes a marker sum
/// report one child as two sessions on two accounts.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct LiveTally {
    members: std::collections::BTreeMap<String, MemberSessions>,
    /// Delegate sessions across the fleet: one-shot headless runs the clauth
    /// MCP spawned. Split out for the header chip's own/delegates count; every
    /// per-account surface keeps counting them inside `MemberSessions::sessions`.
    delegates: usize,
    /// Bare sessions whose credential link resolves to no enabled profile:
    /// live, but on no account clauth attributes them to, so they reach
    /// [`LiveTally::total`] only.
    unattributed: usize,
    /// The registry, the bare-marker dir, the job store or a profile dir could
    /// not be listed (it exists but refuses, or `~/.clauth` itself does not
    /// resolve), or a running job's supervisor record could not be read, so
    /// [`LiveTally::total`] has no reading to give.
    unread: bool,
}

/// One account's slice of a [`LiveTally`]. All-zero for an account hosting none.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct MemberSessions {
    pub(crate) sessions: usize,
    /// How many of `sessions` are delegate runs: one-shot headless runs a
    /// clauth MCP spawned, counted inside `sessions` as every per-account
    /// surface counts them.
    pub(crate) delegates: usize,
    /// How many of `sessions` the fallback chain is allowed to move.
    pub(crate) following: usize,
    /// The newest swap ONTO this account. `None` when no session here has ever
    /// swapped, which is also what says no `current_member` pickup lag applies.
    pub(crate) last_swap_at: Option<u64>,
}

impl LiveTally {
    /// Every live session on the account it currently runs as: registry rows
    /// whose marker is still held, the bare `claude` sessions the credential
    /// link resolves onto, and the background jobs Claude Code still runs after
    /// their launcher released its marker.
    ///
    /// A row is dropped once its session is gone. Row GC runs from
    /// `runtime::gc_stale_runtimes` at daemon STARTUP, not per tick, so a
    /// SIGKILLed session's row outlives it for the whole daemon run. Gates on
    /// the same predicate the decision leg does, so a row cannot be live for one
    /// and dead for the other.
    pub(crate) fn collect(config: &AppConfig) -> Self {
        Self::collect_in(config, &crate::claude_jobs::job_trees())
    }

    /// [`LiveTally::collect`] against a job store the caller already read, so a
    /// pass-level caller reads the store once and shares that snapshot (the
    /// [`crate::runtime::has_live_session_in`] precedent).
    pub(crate) fn collect_in(config: &AppConfig, jobs: &crate::claude_jobs::JobTrees) -> Self {
        let rows = read_registry();
        let unread = rows.is_none();
        let rows = rows.unwrap_or_default();
        let live: Vec<LiveSession> = rows
            .iter()
            .filter(|row| !row.is_fixed_background_hold() && row.marker_is_held())
            .cloned()
            .collect();
        let mut tally = Self::from_live_rows(live.clone());
        tally.unread = unread;
        tally.add_bare_sessions(config);
        tally.add_background_jobs(config, jobs, &rows, &live);
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
    /// An unreadable marker dir adds no session to any account and leaves the
    /// fleet total unread — the OPPOSITE direction to
    /// [`crate::runtime::has_live_session`], which gates delete, disable,
    /// rename and rotation and so must read an unknown as live. This tally
    /// only renders, so folding an unknown in as live would put a session on
    /// screen that nothing produced, and folding it in as zero would show a
    /// total nothing read.
    fn add_bare_sessions(&mut self, config: &AppConfig) {
        let Some(bare) = crate::runtime::live_bare_sessions() else {
            self.unread = true;
            return;
        };
        if bare == 0 {
            return;
        }
        let Some((member, _)) = crate::which::resolve_global(config) else {
            self.unattributed += bare;
            return;
        };
        let slot = self.members.entry(member).or_default();
        slot.sessions += bare;
        slot.following += bare;
    }

    /// Fold in the background jobs Claude Code still runs whose `clauth start`
    /// launcher already exited: the marker flock released, so no row above
    /// counts the session, while the job store still names its tree — one of
    /// the account's slots is in use. A counted row on the same tree is the
    /// same session, so each tree's jobs are netted against the rows holding it
    /// ([`crate::runtime::live_background_jobs`]). A job on an account the
    /// config no longer holds counts on the account whose dir its tree sits
    /// under, the way a registry row of a deleted account already lands here:
    /// the session still spends that account's window.
    ///
    /// An unreadable job store adds no session to the account it covers and
    /// leaves the fleet total unread, the direction
    /// [`LiveTally::add_bare_sessions`] takes: a bg session nothing could read
    /// must not read as idle on screen. The unknown stays with the account it
    /// covers: a TREE-local store contributes no jobs for THAT account alone,
    /// while every readable account's running job still counts on its own
    /// account. [`LiveTally::own`] and [`LiveTally::delegates`] are single
    /// fleet numbers, so the one unknown contribution still leaves the fleet
    /// total unknown — folding it in as zero is the direction this tally
    /// already refuses.
    fn add_background_jobs(
        &mut self,
        config: &AppConfig,
        jobs: &crate::claude_jobs::JobTrees,
        rows: &[LiveSession],
        counted: &[LiveSession],
    ) {
        let (background, unread) =
            crate::runtime::live_background_jobs(config, jobs, rows, counted);
        self.unread |= unread;
        for job in background {
            let slot = self.members.entry(job.member).or_default();
            slot.sessions += 1;
            slot.following += usize::from(job.follows_chain);
        }
    }

    /// Tally rows already known to be live. Attribution is `current_member`,
    /// which the executor writes only on a session's FIRST swap — so a session
    /// that never moved (every pinned one, and every follower before it swaps)
    /// is still running as the account it launched on.
    fn from_live_rows(rows: impl IntoIterator<Item = LiveSession>) -> Self {
        let mut per_member: std::collections::BTreeMap<String, MemberSessions> =
            std::collections::BTreeMap::new();
        let mut delegates = 0;
        for row in rows {
            let is_delegate = usize::from(row.is_delegate);
            delegates += is_delegate;
            let member = row.current_member.unwrap_or(row.start_profile);
            let slot = per_member.entry(member).or_default();
            slot.sessions += 1;
            slot.delegates += is_delegate;
            slot.following += usize::from(row.follows_chain);
            slot.last_swap_at = slot.last_swap_at.max(row.last_swap_at);
        }
        Self {
            members: per_member,
            delegates,
            unattributed: 0,
            unread: false,
        }
    }

    /// A tally straight from rows, for tests in other modules that need a fleet
    /// without laying registry files down. Skips the liveness filter and the
    /// job-store term, which are the halves [`LiveTally::collect`] adds and this
    /// module's own tests pin.
    #[cfg(test)]
    pub(crate) fn of(rows: impl IntoIterator<Item = LiveSession>) -> Self {
        Self::from_live_rows(rows)
    }

    /// One account's sessions.
    pub(crate) fn member(&self, name: &crate::profile::ProfileName) -> MemberSessions {
        self.members.get(name.as_str()).copied().unwrap_or_default()
    }

    /// A tally that read `rows` but not every dir it counts from (the bare
    /// markers or the job store refused), for tests in other modules.
    #[cfg(test)]
    pub(crate) fn partly_read(rows: impl IntoIterator<Item = LiveSession>) -> Self {
        Self {
            unread: true,
            ..Self::from_live_rows(rows)
        }
    }

    /// A tally whose reads failed, for render tests in other modules.
    #[cfg(test)]
    pub(crate) fn unread() -> Self {
        Self {
            unread: true,
            ..Self::default()
        }
    }

    /// Every live session across the fleet: those on a deleted account and bare
    /// ones on a link no profile matches included. `None` when a dir or record it counts
    /// from could not be read.
    pub(crate) fn total(&self) -> Option<usize> {
        if self.unread {
            return None;
        }
        Some(self.members.values().map(|m| m.sessions).sum::<usize>() + self.unattributed)
    }

    /// Own (non-delegate) live sessions across the fleet: `clauth start` and
    /// codex rows plus bare sessions. `None` when a dir or record it counts from could
    /// not be read.
    pub(crate) fn own(&self) -> Option<usize> {
        if self.unread {
            return None;
        }
        Some(
            self.members.values().map(|m| m.sessions).sum::<usize>() + self.unattributed
                - self.delegates,
        )
    }

    /// Delegate live sessions across the fleet. `None` when a dir or record it counts
    /// from could not be read.
    pub(crate) fn delegates(&self) -> Option<usize> {
        if self.unread {
            return None;
        }
        Some(self.delegates)
    }
}

pub(crate) fn registry_dir() -> Result<PathBuf> {
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
/// unparseable file is skipped rather than failing the sweep, and so is an
/// unlistable registry.
pub(crate) fn list() -> Vec<LiveSession> {
    read_registry().unwrap_or_default()
}

/// The running sessions' rows: drop the daemon's hold of the account's fixed
/// background pair, which stands for no launch session, and every row whose
/// marker is no longer held, so a stale row awaiting GC never reads as live
/// here. An adopted per-launch `--with-fallback` hold is a real session and
/// stays. `None` when the registry cannot be listed or located, an empty vec
/// when no row is live.
pub(crate) fn live_rows() -> Option<Vec<LiveSession>> {
    read_registry().map(|rows| {
        rows.into_iter()
            .filter(|row| !row.is_fixed_background_hold() && row.marker_is_held())
            .collect()
    })
}

/// [`list`], telling an absent registry (nothing ever registered: empty) from
/// one that cannot be listed or located (`None`).
fn read_registry() -> Option<Vec<LiveSession>> {
    let dir = registry_dir().ok()?;
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Some(Vec::new()),
        Err(_) => return None,
    };
    Some(
        entries
            .flatten()
            .filter_map(|entry| read_row(&entry.path()))
            .collect(),
    )
}

fn read_row(path: &Path) -> Option<LiveSession> {
    // A regular file only, bounded: the row path was never typed, and an
    // `open` on a FIFO blocks forever — the registry read runs every tally
    // tick. A non-regular or unreadable row drops the way an unreadable one
    // always did.
    let bytes =
        crate::runtime::read_regular_capped(path, crate::runtime::RECORD_READ_CAP).ok()??;
    serde_json::from_slice(&bytes).ok()
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
        let mut row: LiveSession = serde_json::from_slice(&bytes).map_err(|e| {
            crate::format::parse_refusal(
                format!("unreadable live-session row for {session_id}"),
                crate::format::json_parse_position(&e),
            )
        })?;
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
