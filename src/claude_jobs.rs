//! Reader for Claude Code's background-job store
//! (`~/.claude/jobs/<id>/state.json`).
//!
//! `claude --bg` hands its session to Claude Code's background service and
//! returns at once, so a `clauth start <p> --bg` teardown — and any later GC
//! sweep — can find the runtime tree still in use by a session clauth's marker
//! flock never sees (the session is orphaned: `starting…`, no pid). The job
//! record is Claude Code's own statement that a session uses a config dir.
//!
//! One read walks the global `~/.claude/jobs` store and, beside it, each
//! runtime tree's own `jobs/` dir where that store is authoritative — a copy
//! host keeps the record inside the tree (its mirror never carries `jobs/`),
//! and an isolated tree links no `jobs/` so its store is tree-local too; a
//! symlinked `jobs/` is the global store already read and is never followed. A
//! tree-local record names only its own tree, so a record naming a different
//! tree is a stale copy — a copy-host mirror's leftover — and is skipped. One
//! read yields the tree and settings-file name sets. The spare term
//! ([`JobTrees::spares`] / [`JobTrees::spares_under`], GC and the destructive
//! gates) counts a job as live while its job dir exists (until `claude rm`),
//! whatever its `state`: a finished job may be revived (`claude attach`) and
//! the cost of sparing its tree is disk — except in an ISOLATED tree, which no
//! session can re-enter, where only an UNFINISHED job spares. A job dir with no
//! record is either a removed job's leftover — an empty `tmp/` and/or an
//! `exit-cause` file — or caught mid-create (an empty dir, or only staged
//! `state.json.tmp.*` records, newest mtime within [`MID_CREATE_GRACE`];
//! older reads as a leftover, an unreadable or future mtime stays mid-create);
//! neither names a job. A leftover is no job to any reader. A mid-create dir
//! names no tree yet either, so the live-session count skips it and keeps
//! counting the store's real records, while every gate that must not collect
//! a tree a job reads fails closed for it — the tree itself for a tree-local
//! dir, every tree for the global one, since the record about to land can name
//! any of them. Any other record-less dir is unreadable and spares, and so does
//! a record that fails both parses. Such a dir is STUCK, since it never clears
//! on its own, when it is a record-less dir of no known shape (at once) or an
//! unparseable record whose mtime is older than the same grace; a younger
//! record, or one whose mtime is unreadable or in the future, may be a write in
//! progress and is not stuck. The rotation refusal names a stuck job dir. The
//! settings term ([`JobTrees::spares_settings`]) spares a spawn-settings file
//! a job's `respawnFlags` name. The rotation term
//! ([`JobTrees::unfinished_under`]) and the live-session term
//! ([`JobTrees::unfinished_jobs`]) count only UNFINISHED jobs: a
//! `stopped`/`done`/`failed` job must not freeze the chain and holds no
//! session slot, and only `state` can say "finished" — `firstTerminalAt`
//! cannot (a revived `running` job keeps the stamp). A global-store failure is
//! global; a tree-local or profile-dir failure is SCOPED to that account's
//! trees.

use std::collections::{HashMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;

use crate::profile::claude_dir;
use crate::session_record::LenientString;

// Test seam: how many times THIS thread has read the job store, so a
// pass-level caller's test can pin "one read per pass". Thread-local: a
// concurrent test thread's reads land in its own cell and never pollute this
// count.
#[cfg(test)]
thread_local! {
    static JOB_TREES_READS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
pub(crate) fn job_trees_reads() -> usize {
    JOB_TREES_READS.with(std::cell::Cell::get)
}

#[cfg(test)]
pub(crate) fn reset_job_trees_reads() {
    JOB_TREES_READS.with(|c| c.set(0));
}

/// A job's `state.json`, reduced to the fields the spare, rotation and
/// settings-file rules need. Unknown fields are ignored: the record's key set
/// grew across Claude Code versions.
#[derive(Deserialize)]
struct JobState {
    #[serde(rename = "providerEnv")]
    provider_env: Option<ProviderEnv>,
    /// `String`, never a closed enum and never a strict parse: a future Claude
    /// Code shape writing `state` as a non-string reads empty
    /// ([`crate::session_record::lenient_string`]), and empty reads unfinished —
    /// a false "finished" would sign a running session out.
    #[serde(default, deserialize_with = "crate::session_record::lenient_string")]
    state: String,
    /// Never a strict parse: absent reads [`RespawnFlags::Absent`]; a list
    /// decodes through [`read_respawn_flags`] to [`RespawnFlags::Named`] or
    /// [`RespawnFlags::Unknown`]; any other value, or a token serde_json
    /// refuses inside it, fails this parse and [`JobCore`] reads the flags
    /// [`RespawnFlags::Unknown`]. Of the record's parse faults, only one that
    /// fails that parse too flips the store `Unreadable`.
    #[serde(
        default,
        rename = "respawnFlags",
        deserialize_with = "read_respawn_flags"
    )]
    respawn_flags: RespawnFlags,
    /// The conversation the job runs: its transcript lines carry this id
    /// (measured on 2.1.310). Lenient like `state`, so a retyped value names
    /// no conversation rather than flip the store unreadable.
    #[serde(
        default,
        rename = "sessionId",
        deserialize_with = "crate::session_record::lenient_string"
    )]
    session_id: String,
}

/// [`JobState`] minus the `respawnFlags` field: the fallback parse when
/// [`JobState`] fails. `respawnFlags` is unmodelled, so serde skips it through
/// [`serde::de::IgnoredAny`] — a path a token serde_json rejects inside the
/// field survives — and a record only this struct parses is one whose sole
/// fault is in `respawnFlags`. `providerEnv` and `state` share [`JobState`]'s
/// attributes, so a fault in either still fails both parses.
#[derive(Deserialize)]
struct JobCore {
    #[serde(rename = "providerEnv")]
    provider_env: Option<ProviderEnv>,
    #[serde(default, deserialize_with = "crate::session_record::lenient_string")]
    state: String,
    #[serde(
        default,
        rename = "sessionId",
        deserialize_with = "crate::session_record::lenient_string"
    )]
    session_id: String,
}

/// A job record reduced to its `sessionId`, read leniently: a missing,
/// unreadable or unparseable record, or a `sessionId` that is not a Claude
/// Code session id (a lowercase uuid, `8-4-4-4-12` hex), reads `None`.
#[derive(Deserialize)]
struct JobSessionRecord {
    #[serde(default, rename = "sessionId")]
    session_id: Option<String>,
}

/// `~/.claude/jobs/<id>/state.json`'s `sessionId`, `None` when the record or
/// the field is missing or unreadable, or when the id is not a Claude Code
/// session id — a lowercase uuid (`8-4-4-4-12` hex). The value is a
/// best-effort convenience the sessions route carries; a non-matching id is
/// never relayed.
pub(crate) fn job_session_id(id: &str) -> Option<String> {
    let record = std::fs::read(
        crate::profile::claude_dir()
            .ok()?
            .join("jobs")
            .join(id)
            .join("state.json"),
    )
    .ok()?;
    serde_json::from_slice::<JobSessionRecord>(&record)
        .ok()
        .and_then(|record| record.session_id)
        .filter(|sid| is_session_uuid(sid))
}

/// Whether `sid` is a Claude Code session id: 8-4-4-4-12 lowercase hex.
fn is_session_uuid(sid: &str) -> bool {
    sid.len() == 36
        && sid.bytes().enumerate().all(|(index, byte)| match index {
            8 | 13 | 18 | 23 => byte == b'-',
            _ => byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte),
        })
}

/// Claude Code's terminal agent states. `stopped` measured on 2.1.304
/// (`claude stop`); `done`/`failed` from `claude agents --json`'s state list,
/// never seen in a record; any other string reads unfinished.
const FINISHED_STATES: [&str; 3] = ["stopped", "done", "failed"];

#[derive(Deserialize)]
struct ProviderEnv {
    #[serde(rename = "CLAUDE_CONFIG_DIR")]
    claude_config_dir: Option<String>,
}

/// The name sets one readable job-store walk yields. The fields are private
/// behind the query methods: `unfinished ⊆ named` and
/// `unfinished_by_job ⊆ named_by_job` must hold, and a caller reading one set
/// directly could break it.
#[derive(Debug)]
pub(crate) struct NamedTrees {
    /// Canonical identities of every config dir a live job names, whatever its
    /// `state`. Empty when no job names a tree (or no job exists). A job
    /// naming a dir that no longer resolves contributes nothing: its tree is
    /// already gone.
    named: HashSet<PathBuf>,
    /// Canonical tree id -> the job ids (job dir names) whose record names that
    /// tree, from the global store or a tree-local one. Two jobs can name one
    /// tree, so the set — not the tree's presence in `named` — says when the
    /// jobs naming a tree changed.
    named_by_job: HashMap<PathBuf, HashSet<String>>,
    /// The UNFINISHED half of `named_by_job`, for the live-session term: a
    /// finished job (`stopped`/`done`/`failed`) still spares its tree (it may
    /// be revived) but no longer RUNS, so the session count never reads it.
    unfinished_by_job: HashMap<PathBuf, HashSet<String>>,
    /// Canonical identities of every config dir an UNFINISHED job names — a
    /// subset of `named` (an unfinished job is also live for the spare term).
    unfinished: HashSet<PathBuf>,
    /// Conversation id -> the canonical config dir the job running it names,
    /// for every job in `named` whose record names its conversation.
    tree_by_session: HashMap<String, PathBuf>,
    /// Conversation ids a job record names with no config dir: a plain
    /// `claude --bg` on `~/.claude`.
    global_sessions: HashSet<String>,
    /// Canonical identities of every settings file a job's `respawnFlags` name
    /// (a path following a `--settings` element).
    named_settings: HashSet<PathBuf>,
    /// Canonical identities of every config dir whose job's `respawnFlags` are
    /// not a list of strings, so no settings file is recoverable from them and
    /// the caller falls back to the tree rule.
    unknown_flags_trees: HashSet<PathBuf>,
    /// Scoped fail-closed: every dir (a runtime tree, or a profile dir) whose
    /// OWN job store could not be read. Only trees under one of these read as
    /// unreadable — a global store failure is the separate
    /// [`JobTrees::Unreadable`] arm.
    unreadable: Vec<UnreadableStore>,
    /// The GLOBAL store was caught mid-create: a job dir there holds no record
    /// yet. It names no tree and the count skips it, but the record about to
    /// land can name any tree, so every gate fails closed for all of them.
    /// `Some(store)` is the dir to name in a log line.
    global_mid_create: Option<PathBuf>,
}

/// One scoped unreadable: the canonical identity a spare/block query matches
/// against, and the store dir to name in the rotation refusal's `{path}`.
#[derive(Debug)]
struct UnreadableStore {
    /// Canonical identity of the tree or profile dir whose store failed.
    scope: PathBuf,
    /// The store dir the operator can act on (the tree's `jobs/`, or the
    /// profile dir).
    store: PathBuf,
    /// The store was READ, but held a job dir caught mid-create. Gates fail
    /// closed for the scope; the live-session count skips the dir and counts
    /// the store's other records.
    mid_create: bool,
    /// The job dir that never clears on its own and made the store unreadable
    /// ([`StoreReadFault::stuck`]); the rotation refusal names it. `None` for
    /// every other fault, and for a mid-create store, which clears by itself.
    stuck_dir: Option<PathBuf>,
}

/// Why [`read_jobs_dir`] could not read a store.
#[derive(Debug)]
struct StoreReadFault {
    /// The dir the fault is about: the store itself, or the offending job dir.
    path: PathBuf,
    /// `path` is a job dir that never clears on its own, so a retry cannot
    /// help: a record-less dir of neither the leftover nor the mid-create
    /// shape, or a record that fails both parses and whose own mtime is older
    /// than [`MID_CREATE_GRACE`]. A younger record, or one whose mtime is
    /// unreadable or in the future, can be a write in progress; every other
    /// fault (the store or a job dir that cannot be listed, a record that
    /// cannot be read, a config dir that cannot be resolved) is not stuck
    /// either.
    stuck: bool,
}

impl StoreReadFault {
    /// A fault a retry may clear.
    fn transient(path: PathBuf) -> Self {
        Self { path, stuck: false }
    }

    /// A job dir that never clears on its own.
    fn stuck(path: PathBuf) -> Self {
        Self { path, stuck: true }
    }
}

/// The answer to "which runtime trees and settings files must a sweep or gate
/// spare?".
#[derive(Debug)]
pub(crate) enum JobTrees {
    /// A readable store, as [`NamedTrees`]. Boxed: `NamedTrees` dwarfs the
    /// `Unreadable` arm (clippy::large_enum_variant).
    Named(Box<NamedTrees>),
    /// The store could not be trusted — a job dir's `state.json` was missing,
    /// unreadable, or unparseable, or a named config dir could not be
    /// canonicalized while it might still exist. `offending` is the deep dir a
    /// log line names (never the record's contents); `store` is the store dir
    /// the rotation refusal names (the conventional global store, or the tree's
    /// `jobs/`). `stuck` is true when `offending` is a job dir that never
    /// clears on its own ([`StoreReadFault::stuck`]): the rotation refusal then
    /// names `offending` and tells the operator to delete it, instead of
    /// telling them to retry.
    Unreadable {
        offending: PathBuf,
        store: PathBuf,
        stuck: bool,
    },
}

/// What the job store says about the tree one conversation runs in
/// ([`JobTrees::session_tree`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SessionTree {
    /// No job record names the conversation.
    Untracked,
    /// A job runs the conversation on `~/.claude`: a plain `claude --bg`.
    Global,
    /// The canonical config dir the job running the conversation names.
    Named(PathBuf),
    /// The store could not be read, so the answer is missing.
    Unknown,
}

impl JobTrees {
    /// The tree the job running conversation `session_id` names, off the
    /// record's `sessionId` and `providerEnv.CLAUDE_CONFIG_DIR`.
    pub(crate) fn session_tree(&self, session_id: &str) -> SessionTree {
        match self {
            JobTrees::Unreadable { .. } => SessionTree::Unknown,
            JobTrees::Named(sets) => match sets.tree_by_session.get(session_id) {
                Some(tree) => SessionTree::Named(tree.clone()),
                None if sets.global_sessions.contains(session_id) => SessionTree::Global,
                None => SessionTree::Untracked,
            },
        }
    }

    /// Whether `tree` must be spared: a live job names it, the store was
    /// unreadable (fail closed, the direction `is_session_alive` takes), or a
    /// scoped unreadable covers the tree (its own store, or its profile dir,
    /// could not be read).
    ///
    /// `tree` is canonicalized here so it and the named set share one spelling
    /// ([`crate::canonical::identity`], the compare-or-key purpose). A
    /// `NotFound` canonicalize — the tree is already gone — reads as unnamed:
    /// nothing to spare, and the caller still collects an orphaned sessions
    /// dir. Any other canonicalize failure reads as named.
    pub(crate) fn spares(&self, tree: &Path) -> bool {
        match self {
            JobTrees::Unreadable { .. } => true,
            JobTrees::Named(sets) if sets.global_mid_create.is_some() => true,
            JobTrees::Named(sets) if sets.named.is_empty() && sets.unreadable.is_empty() => false,
            JobTrees::Named(sets) => named_or_affected(tree, &sets.named, &sets.unreadable),
        }
    }

    /// Whether a live job NAMES `tree` (the named set holds its canonical
    /// identity), distinct from [`JobTrees::spares`], which also spares a tree
    /// covered by a scoped unreadable. The GC per-pair spare log uses this so a
    /// scoped-unreadable spare never claims a job names the tree.
    pub(crate) fn names(&self, tree: &Path) -> bool {
        match self {
            JobTrees::Unreadable { .. } => false,
            JobTrees::Named(sets) => {
                if sets.named.is_empty() {
                    return false;
                }
                match crate::canonical::identity(tree) {
                    Ok(id) => sets.named.contains(&id),
                    Err(_) => false,
                }
            }
        }
    }

    /// Whether an UNFINISHED job names `tree` — the subset of
    /// [`JobTrees::names`] that still blocks a macOS rotation — or the tree's
    /// own store is unreadable (fail closed). Distinct from
    /// [`JobTrees::spares`], which covers a LIVE job of any state, and from
    /// [`JobTrees::names`], which covers any live job.
    pub(crate) fn unfinished_names(&self, tree: &Path) -> bool {
        match self {
            JobTrees::Unreadable { .. } => true,
            JobTrees::Named(sets) if sets.global_mid_create.is_some() => true,
            JobTrees::Named(sets) if sets.unfinished.is_empty() && sets.unreadable.is_empty() => {
                false
            }
            JobTrees::Named(sets) => named_or_affected(tree, &sets.unfinished, &sets.unreadable),
        }
    }

    /// The set of job ids whose record names `tree`, from any store that names
    /// it (the global store or the tree's own `jobs/`). Empty when no job names
    /// it or the store is unreadable. Distinct from [`JobTrees::names`]: two
    /// jobs can name one tree, and the daemon's copy-host gate re-arms its one
    /// re-check when THIS set changes (a new job beside an older one), not only
    /// when it empties.
    pub(crate) fn job_ids_naming(&self, tree: &Path) -> HashSet<String> {
        match self {
            JobTrees::Unreadable { .. } => HashSet::new(),
            JobTrees::Named(sets) => match crate::canonical::identity(tree) {
                Ok(id) => sets.named_by_job.get(&id).cloned().unwrap_or_default(),
                Err(_) => HashSet::new(),
            },
        }
    }

    /// Whether a scoped unreadable is `profile` itself — the account's profile
    /// dir could not be enumerated. The daemon skips such an account: the
    /// adoption build reads its config and probes its link mode under that
    /// dir, so queueing it would start a failing backoff streak that re-takes
    /// the locks every interval. A tree-local store failure (a scope UNDER the
    /// profile) does not count: adoption never reads a tree's own `jobs/`.
    pub(crate) fn profile_dir_unreadable(&self, profile: &Path) -> bool {
        match self {
            JobTrees::Unreadable { .. } => true,
            JobTrees::Named(sets) => {
                if sets.unreadable.is_empty() {
                    return false;
                }
                match crate::canonical::identity(profile) {
                    Ok(id) => sets.unreadable.iter().any(|u| u.scope == id),
                    Err(e) if e.kind() == io::ErrorKind::NotFound => false,
                    Err(_) => true,
                }
            }
        }
    }

    /// The store dir every scoped STORE FAILURE names, for the sweep-level GC
    /// log: one entry per scoped unreadable, once per sweep, empty under a
    /// global [`JobTrees::Unreadable`] (the global line already covers that). A
    /// store caught mid-create was read, so it names no store here.
    pub(crate) fn scoped_unreadable_stores(&self) -> Vec<&Path> {
        match self {
            JobTrees::Unreadable { .. } => Vec::new(),
            JobTrees::Named(sets) => sets
                .unreadable
                .iter()
                .filter(|u| !u.mid_create)
                .map(|u| u.store.as_path())
                .collect(),
        }
    }

    /// Every scoped store caught mid-create as `(spared tree, store dir)`, for
    /// the sweep-level GC log: the store was read, so it is no store failure,
    /// but its record-less dir can name the tree the next instant, so the tree
    /// is spared and the log names the dir. The tree is the store's parent, the
    /// walked spelling the per-pair line prints — never `scope`'s canonical
    /// one. One entry per such store; empty under a global
    /// [`JobTrees::Unreadable`].
    pub(crate) fn scoped_mid_create_stores(&self) -> Vec<(&Path, &Path)> {
        match self {
            JobTrees::Unreadable { .. } => Vec::new(),
            JobTrees::Named(sets) => sets
                .unreadable
                .iter()
                .filter(|u| u.mid_create)
                .map(|u| (u.store.parent().unwrap_or(&u.scope), u.store.as_path()))
                .collect(),
        }
    }

    /// The global store dir when the store was caught mid-create, for the
    /// sweep-level GC log: every spare term fails closed for every tree, and
    /// this names the dir a record is about to land in. `None` on a readable
    /// store and under [`JobTrees::Unreadable`] (its own arm names the store).
    pub(crate) fn global_mid_create_store(&self) -> Option<&Path> {
        match self {
            JobTrees::Unreadable { .. } => None,
            JobTrees::Named(sets) => sets.global_mid_create.as_deref(),
        }
    }

    /// Whether a live job names a tree UNDER `dir`, the store is unreadable, or
    /// a scoped unreadable covers `dir` (its own profile dir, or a tree-local
    /// store under it). Used by [`crate::runtime::has_live_session`]: a job
    /// naming any tree under a profile's dir is a live session for every gate
    /// that carries (delete, disable, rename), since the marker flock never
    /// sees the `claude --bg` session still reading its config dir. The macOS
    /// rotation gate uses the narrower [`JobTrees::unfinished_under`] term
    /// instead.
    pub(crate) fn spares_under(&self, dir: &Path) -> bool {
        match self {
            JobTrees::Unreadable { .. } => true,
            JobTrees::Named(sets) => {
                sets.global_mid_create.is_some()
                    || named_under(&sets.named, dir)
                    || unreadable_under(dir, &sets.unreadable)
            }
        }
    }

    /// Every job Claude Code still RUNS, as `(job id, canonical tree)`, for the
    /// live-session count: one entry per job, so two jobs on one shared tree
    /// read as two sessions. The trees come from the snapshot's own records,
    /// never from the configured profiles, so a job on an account the config no
    /// longer holds is counted on the tree's own account — a session does not
    /// stop spending because its account was deleted from the roster.
    ///
    /// A finished job (`stopped`/`done`/`failed`) holds no slot and names
    /// nothing here — its dir outlives `claude stop` until `claude rm` — and an
    /// unknown or missing `state` reads unfinished, the direction
    /// [`JobTrees::unfinished_under`] takes. Trees under a store that could not
    /// be read are left out and make the second element true, so the caller
    /// knows the count is short; a store caught mid-create is not one of those
    /// (it was read, and its record-less dir names no tree). `None` under a
    /// global [`JobTrees::Unreadable`]: the count has no reading to give, the
    /// direction [`crate::live_sessions::LiveTally`] takes for its other
    /// unknown reads.
    pub(crate) fn unfinished_jobs(&self) -> Option<(Vec<(String, PathBuf)>, bool)> {
        match self {
            JobTrees::Unreadable { .. } => None,
            JobTrees::Named(sets) => {
                let readable = |tree: &PathBuf| {
                    !sets
                        .unreadable
                        .iter()
                        .any(|u| !u.mid_create && tree.starts_with(&u.scope))
                };
                let jobs = sets
                    .unfinished_by_job
                    .iter()
                    .filter(|(tree, _)| readable(tree))
                    .flat_map(|(tree, job_ids)| {
                        job_ids.iter().map(move |id| (id.clone(), tree.clone()))
                    })
                    .collect();
                Some((jobs, sets.unreadable.iter().any(|u| !u.mid_create)))
            }
        }
    }

    /// Whether `file` must be spared: a job's `respawnFlags` name it, a job
    /// whose flags are unreadable names one of `own_trees` (the tree rule's
    /// fallback), the store was unreadable (fail closed), or a scoped
    /// unreadable covers one of `own_trees`. `file` and each own tree are
    /// canonicalized like the tree set. The fallback is loose by design: when
    /// `own_trees` holds both layouts of a shared file, a job naming the bare
    /// fake `runtime` spares the Real-mode file beside it too.
    pub(crate) fn spares_settings(&self, file: &Path, own_trees: &[PathBuf]) -> bool {
        match self {
            JobTrees::Unreadable { .. } => true,
            JobTrees::Named(sets) => {
                if sets.global_mid_create.is_some() {
                    return true;
                }
                if sets.named_settings.is_empty()
                    && sets.unknown_flags_trees.is_empty()
                    && sets.unreadable.is_empty()
                {
                    return false;
                }
                holds_canonical(&sets.named_settings, file)
                    || own_trees
                        .iter()
                        .any(|tree| holds_canonical(&sets.unknown_flags_trees, tree))
                    || own_trees
                        .iter()
                        .any(|tree| named_or_affected(tree, &HashSet::new(), &sets.unreadable))
            }
        }
    }

    /// The canonical tree paths every UNFINISHED job names UNDER `dir`, so the
    /// rotation gate can read each tree's `.credentials.json` and apply the
    /// same holds-a-refresh-token narrowing the marker term gets. `Err(`[`StoreRefusal`]`)`
    /// when the store (or a scoped store under `dir`) is unreadable — fail
    /// closed: the caller blocks and names the store, or the stuck job dir
    /// ([`StoreRefusal::stuck_dir`]) when one made the store unreadable. A finished job
    /// (`stopped`/`done`/`failed`) is not mid-task, so it names nothing here;
    /// an unknown or missing `state` reads unfinished by construction.
    ///
    /// `dir` is canonicalized so it and the set share one spelling; a
    /// `NotFound` canonicalize — the dir is already gone — reads as empty, and
    /// any other failure reads as unreadable, like [`JobTrees::spares`].
    pub(crate) fn unfinished_under(&self, dir: &Path) -> Result<Vec<PathBuf>, StoreRefusal> {
        match self {
            JobTrees::Unreadable {
                store,
                offending,
                stuck,
            } => Err(StoreRefusal {
                store: store.clone(),
                stuck_dir: stuck.then(|| offending.clone()),
            }),
            JobTrees::Named(sets) => {
                if sets.global_mid_create.is_some() {
                    // The conventional location every other global-store
                    // refusal names, never this host's absolute path. A
                    // mid-create dir clears on its own, so none is stuck.
                    return Err(StoreRefusal {
                        store: PathBuf::from("~/.claude/jobs"),
                        stuck_dir: None,
                    });
                }
                let dir_id = match crate::canonical::identity(dir) {
                    Ok(id) => id,
                    Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
                    Err(_) => {
                        return Err(StoreRefusal {
                            store: dir.to_path_buf(),
                            stuck_dir: None,
                        });
                    }
                };
                if let Some(unreadable) = sets
                    .unreadable
                    .iter()
                    .find(|u| u.scope == dir_id || u.scope.starts_with(&dir_id))
                {
                    return Err(StoreRefusal {
                        store: unreadable.store.clone(),
                        stuck_dir: unreadable.stuck_dir.clone(),
                    });
                }
                Ok(sets
                    .unfinished
                    .iter()
                    .filter(|named| named.starts_with(&dir_id))
                    .cloned()
                    .collect())
            }
        }
    }
}

/// Why [`JobTrees::unfinished_under`] refused: the rotation gate could not
/// judge the store.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct StoreRefusal {
    /// The store dir the refusal names.
    pub(crate) store: PathBuf,
    /// The job dir that never clears on its own and made the store unreadable,
    /// when one did: the refusal then names it and says to delete it.
    pub(crate) stuck_dir: Option<PathBuf>,
}

/// Whether `path`'s canonical identity is named OR lies under a scoped
/// unreadable. A `NotFound` canonicalize — the path is already gone — reads as
/// not spared, any other failure as spared (fail closed).
fn named_or_affected(
    path: &Path,
    named: &HashSet<PathBuf>,
    unreadable: &[UnreadableStore],
) -> bool {
    match crate::canonical::identity(path) {
        Ok(id) => {
            named.contains(&id)
                || unreadable
                    .iter()
                    .any(|u| id == u.scope || id.starts_with(&u.scope))
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => false,
        Err(_) => true,
    }
}

/// Whether `dir`'s canonical identity IS a scoped unreadable, or has one under
/// it (a tree-local store in this account failed).
fn unreadable_under(dir: &Path, unreadable: &[UnreadableStore]) -> bool {
    match crate::canonical::identity(dir) {
        Ok(dir_id) => unreadable
            .iter()
            .any(|u| u.scope == dir_id || u.scope.starts_with(&dir_id)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => false,
        Err(_) => true,
    }
}

/// Whether any canonical identity in `set` lies under `dir`'s canonical
/// identity. `dir` is canonicalized so it and the set share one spelling (the
/// same [`crate::canonical::identity`] form `spares` uses). A `NotFound`
/// canonicalize — the dir is already gone — reads as unnamed: nothing under it
/// exists to spare. Any other failure reads as named.
fn named_under(set: &HashSet<PathBuf>, dir: &Path) -> bool {
    if set.is_empty() {
        return false;
    }
    match crate::canonical::identity(dir) {
        Ok(dir_id) => set.iter().any(|named| named.starts_with(&dir_id)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => false,
        Err(_) => true,
    }
}

/// Whether `set` holds `path`'s canonical identity, fail closed: a `NotFound`
/// canonicalize — the path is already gone — reads as not held (nothing to
/// spare), any other failure as held.
fn holds_canonical(set: &HashSet<PathBuf>, path: &Path) -> bool {
    match crate::canonical::identity(path) {
        Ok(id) => set.contains(&id),
        Err(e) if e.kind() == io::ErrorKind::NotFound => false,
        Err(_) => true,
    }
}

/// Deserializer for [`JobState::respawn_flags`]: walks the value into a
/// [`RespawnFlags`] without ever building a `serde_json::Value`; a non-list
/// value errors, and [`JobCore`]'s fallback then reads the flags
/// [`RespawnFlags::Unknown`].
fn read_respawn_flags<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<RespawnFlags, D::Error> {
    deserializer.deserialize_any(RespawnFlagsVisitor)
}

/// What a job's `respawnFlags` name: [`RespawnFlags::Absent`] when the field
/// is missing, [`RespawnFlags::Named`] the settings values after `--settings`
/// (either spelling), or [`RespawnFlags::Unknown`] when the field is present
/// but not a list of strings, and the caller must fall back to the tree rule.
#[derive(Default)]
enum RespawnFlags {
    #[default]
    Absent,
    Named(Vec<String>),
    Unknown,
}

struct RespawnFlagsVisitor;

impl<'de> serde::de::Visitor<'de> for RespawnFlagsVisitor {
    type Value = RespawnFlags;

    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("a list of strings")
    }

    fn visit_seq<A>(self, mut seq: A) -> Result<RespawnFlags, A::Error>
    where
        A: serde::de::SeqAccess<'de>,
    {
        let mut named = Vec::new();
        let mut unknown = false;
        while let Some(flag) = seq.next_element::<LenientString>()? {
            let Some(flag) = flag.0 else {
                unknown = true;
                continue;
            };
            if unknown {
                continue;
            }
            if let Some(value) = flag.strip_prefix("--settings=") {
                named.push(value.to_string());
            } else if flag == "--settings" {
                match seq.next_element::<LenientString>()? {
                    Some(LenientString(Some(value))) => named.push(value),
                    _ => unknown = true,
                }
            }
        }
        if unknown {
            Ok(RespawnFlags::Unknown)
        } else {
            Ok(RespawnFlags::Named(named))
        }
    }
}

/// Read Claude Code's job stores once: the global `~/.claude/jobs` and, beside
/// it, each runtime tree's own `jobs/` dir where that store is authoritative —
/// a REAL directory in an ISOLATED tree (which never links the store) or in a
/// SHARED tree (a linked-host shared tree's `jobs` is a symlink, already read
/// above, so a real dir there was written by Claude Code itself). A tree-local
/// record names only its own tree — Claude Code writes `CLAUDE_CONFIG_DIR` =
/// the tree — so a record naming a different tree is a stale copy and is
/// skipped.
///
/// Every entry that is a DIRECTORY is a job (plain files like `pins.json` are
/// not); a missing store dir reads as no jobs. A global-store failure makes the
/// whole read unreadable; a tree-local or profile-dir failure is SCOPED to that
/// dir's trees. Any job dir whose `state.json` is missing, unreadable, or
/// unparseable makes its STORE unreadable — EXCEPT a missing record whose dir
/// is a removed job's leftover or a creation caught mid-create
/// ([`recordless_job_dir`]; only while its newest mtime is within
/// [`MID_CREATE_GRACE`], older reads as a leftover): a leftover reads as no
/// job, and a mid-create dir is skipped while the gates fail closed for it.
pub(crate) fn job_trees() -> JobTrees {
    #[cfg(test)]
    JOB_TREES_READS.with(|c| c.set(c.get() + 1));
    let mut sets = NamedTrees {
        named: HashSet::new(),
        named_by_job: HashMap::new(),
        unfinished_by_job: HashMap::new(),
        unfinished: HashSet::new(),
        tree_by_session: HashMap::new(),
        global_sessions: HashSet::new(),
        named_settings: HashSet::new(),
        unknown_flags_trees: HashSet::new(),
        unreadable: Vec::new(),
        global_mid_create: None,
    };

    let jobs_dir = match claude_dir() {
        Ok(dir) => dir.join("jobs"),
        // Fail closed: a store this reader cannot even locate must spare, never
        // collect. `claude_dir` fails only when `home_dir` fails (no home), so
        // there is no real path to name — log the conventional location rather
        // than an empty path.
        Err(_) => {
            return JobTrees::Unreadable {
                offending: PathBuf::from("~/.claude/jobs"),
                store: PathBuf::from("~/.claude/jobs"),
                stuck: false,
            };
        }
    };
    // The global store is authoritative for every account, so a failure here
    // stays global and the refusal names the conventional store location.
    match read_jobs_dir(&jobs_dir, None, false, &mut sets) {
        Ok(false) => {}
        Ok(true) => sets.global_mid_create = Some(jobs_dir.clone()),
        Err(fault) => {
            return JobTrees::Unreadable {
                offending: fault.path,
                store: PathBuf::from("~/.claude/jobs"),
                stuck: fault.stuck,
            };
        }
    }

    let runtime_trees = match crate::runtime::runtime_trees() {
        Ok(trees) => trees,
        // A `profiles/` root this walk cannot enumerate means no tree's store
        // can be seen: global.
        Err(dir) => {
            return JobTrees::Unreadable {
                offending: dir.clone(),
                store: dir,
                stuck: false,
            };
        }
    };
    for profile in runtime_trees.unreadable_profiles {
        record_unreadable(&mut sets.unreadable, &profile, &profile, false, None);
    }
    for tree in runtime_trees.trees {
        let jobs = tree.join("jobs");
        match std::fs::symlink_metadata(&jobs) {
            // A symlinked `jobs` IS the global store (the SHARED link on a
            // linked host), already read above; following it would read every
            // record twice.
            Ok(meta) if meta.file_type().is_symlink() => {}
            Ok(meta) if meta.file_type().is_dir() => {
                let isolated = tree
                    .file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|name| !crate::runtime::is_shared_runtime_dir_name(name));
                let owner = match crate::canonical::identity(&tree) {
                    Ok(id) => id,
                    Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                    Err(_) => tree.clone(),
                };
                match read_jobs_dir(&jobs, Some(&owner), isolated, &mut sets) {
                    Ok(false) => {}
                    // A mid-create dir clears on its own: never a stuck dir.
                    Ok(true) => record_unreadable(&mut sets.unreadable, &tree, &jobs, true, None),
                    Err(fault) => record_unreadable(
                        &mut sets.unreadable,
                        &tree,
                        &jobs,
                        false,
                        fault.stuck.then_some(fault.path),
                    ),
                }
            }
            Ok(_) => record_unreadable(&mut sets.unreadable, &tree, &jobs, false, None),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(_) => record_unreadable(&mut sets.unreadable, &tree, &jobs, false, None),
        }
    }

    JobTrees::Named(Box::new(sets))
}

/// Push one scoped store verdict: `scope` (a tree or profile dir) is
/// canonicalized for the spare/block matches, and `store` is the dir the
/// refusal names. `mid_create` marks a store that was read but held a job dir
/// caught mid-create — the gates fail closed for the scope, and the
/// live-session count keeps counting the store's other records. `stuck_dir` is
/// the job dir that never clears on its own when one made the store unreadable
/// ([`StoreReadFault::stuck`]), `None` otherwise.
fn record_unreadable(
    unreadable: &mut Vec<UnreadableStore>,
    scope: &Path,
    store: &Path,
    mid_create: bool,
    stuck_dir: Option<PathBuf>,
) {
    let scope = match crate::canonical::identity(scope) {
        Ok(id) => id,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return,
        Err(_) => scope.to_path_buf(),
    };
    unreadable.push(UnreadableStore {
        scope,
        store: store.to_path_buf(),
        mid_create,
        stuck_dir,
    });
}

/// Read one job-store dir into the sets. `owner` is `Some(canonical tree id)`
/// for a tree-local store and `None` for the global store: a tree-local record
/// naming a DIFFERENT tree is a stale copy (a copy-host mirror's leftover) and
/// is skipped. `isolated`
/// keeps only UNFINISHED jobs — a finished job in an isolated tree (which no
/// session can re-enter) spares nothing and blocks nothing. A missing dir reads
/// as no jobs; any other failure returns the offending dir, which the caller
/// maps to a global or scoped unreadable, with [`StoreReadFault::stuck`] saying
/// whether the offending job dir never clears on its own. The record rules are
/// [`job_trees`]'s:
/// a plain file is no job, a job naming no config dir contributes nothing, and
/// a record-less dir is a removed job's leftover or a creation caught
/// mid-create (newest mtime within [`MID_CREATE_GRACE`]; older reads as a
/// leftover; [`recordless_job_dir`]), both skipped, or unreadable. `Ok(true)`
/// says the store held a mid-create dir, which the caller records so the gates
/// fail closed for it while the count still reads the store's other records.
fn read_jobs_dir(
    jobs_dir: &Path,
    owner: Option<&Path>,
    isolated: bool,
    sets: &mut NamedTrees,
) -> Result<bool, StoreReadFault> {
    let entries = match std::fs::read_dir(jobs_dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(_) => return Err(StoreReadFault::transient(jobs_dir.to_path_buf())),
    };
    let mut mid_create = false;
    for entry in entries {
        let entry = entry.map_err(|_| StoreReadFault::transient(jobs_dir.to_path_buf()))?;
        let job_dir = entry.path();
        let is_dir = entry
            .file_type()
            .map_err(|_| StoreReadFault::transient(job_dir.clone()))?
            .is_dir();
        if !is_dir {
            continue;
        }
        // A regular file only, bounded: the record path was never typed, and an
        // `open` on a FIFO blocks forever (a device read never ends) — the
        // reader runs in the 1 Hz tally and every MCP `profiles` call. A
        // non-regular or unreadable record fails the STORE read closed.
        let record = match crate::runtime::read_regular_capped(
            &job_dir.join("state.json"),
            crate::runtime::RECORD_READ_CAP,
        ) {
            Ok(Some(bytes)) => bytes,
            Ok(None) => match recordless_job_dir(&job_dir) {
                Ok(RecordlessJobDir::Removed) => continue,
                Ok(RecordlessJobDir::MidCreate) => {
                    mid_create = true;
                    continue;
                }
                // Of no known shape: nothing clears it but deleting it.
                Ok(RecordlessJobDir::Unknown) => return Err(StoreReadFault::stuck(job_dir)),
                // A dir that cannot be listed may be listable next time.
                Err(_) => return Err(StoreReadFault::transient(job_dir)),
            },
            Err(()) => return Err(StoreReadFault::transient(job_dir)),
        };
        let state: JobState = match serde_json::from_slice(&record) {
            Ok(state) => state,
            // A non-list `respawnFlags`, a repeated `respawnFlags` key, or a
            // token serde_json refuses as one of its elements fails the derive
            // parse; re-read the record without the field rather than spare
            // every tree. A record that fails this too stays unreadable.
            Err(_) => match serde_json::from_slice::<JobCore>(&record) {
                Ok(core) => JobState {
                    provider_env: core.provider_env,
                    state: core.state,
                    respawn_flags: RespawnFlags::Unknown,
                    session_id: core.session_id,
                },
                // A record that fails both parses is a write in progress while
                // it is young (Claude Code falls back to an in-place write when
                // the rename fails) and stuck once it is old. An unreadable or
                // future mtime cannot prove the age, so it reads young.
                Err(_) => {
                    let old = std::fs::metadata(job_dir.join("state.json"))
                        .and_then(|meta| meta.modified())
                        .ok()
                        .and_then(|mtime| mtime.elapsed().ok())
                        .is_some_and(|age| age > MID_CREATE_GRACE);
                    return Err(StoreReadFault {
                        path: job_dir,
                        stuck: old,
                    });
                }
            },
        };
        let Some(config_dir) = state.provider_env.and_then(|env| env.claude_config_dir) else {
            // A plain `claude --bg` on `~/.claude` names no tree, only its
            // conversation.
            if !state.session_id.is_empty() {
                sets.global_sessions.insert(state.session_id);
            }
            continue;
        };
        let tree_id = match crate::canonical::identity(&config_dir) {
            Ok(id) => id,
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(_) => return Err(StoreReadFault::transient(job_dir)),
        };
        // A tree-local record names only its own tree; anything else is a
        // stale copy of the old global store.
        if owner.is_some_and(|owner| tree_id.as_path() != owner) {
            continue;
        }
        let finished = FINISHED_STATES.contains(&state.state.as_str());
        if isolated && finished {
            continue;
        }
        sets.named.insert(tree_id.clone());
        if !finished {
            sets.unfinished.insert(tree_id.clone());
        }
        if let Some(job_id) = job_dir.file_name().and_then(|name| name.to_str()) {
            sets.named_by_job
                .entry(tree_id.clone())
                .or_default()
                .insert(job_id.to_owned());
            if !finished {
                sets.unfinished_by_job
                    .entry(tree_id.clone())
                    .or_default()
                    .insert(job_id.to_owned());
            }
        }
        if !state.session_id.is_empty() {
            sets.tree_by_session
                .insert(state.session_id, tree_id.clone());
        }
        match state.respawn_flags {
            RespawnFlags::Absent => {}
            RespawnFlags::Named(values) => {
                for value in values {
                    // Inline JSON names no file; a value that cannot
                    // resolve as a path scopes the fallback to this
                    // job's own tree, never the whole store.
                    if crate::cc_exempt::is_inline_settings(&value) {
                        continue;
                    }
                    match crate::canonical::identity(&value) {
                        Ok(settings_id) => {
                            sets.named_settings.insert(settings_id);
                        }
                        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                        Err(_) => {
                            sets.unknown_flags_trees.insert(tree_id.clone());
                        }
                    }
                }
            }
            RespawnFlags::Unknown => {
                sets.unknown_flags_trees.insert(tree_id);
            }
        }
    }
    Ok(mid_create)
}

/// What a job dir with no `state.json` is.
enum RecordlessJobDir {
    /// A removed job's leftover: entries are a NON-EMPTY subset of {`tmp` = an
    /// empty directory, `exit-cause` = a regular file}. `claude rm` can leave
    /// the job's supervisor running, and that supervisor recreates the dir this
    /// way (an empty `tmp/`, then `exit-cause` once signalled); a lone bare
    /// job's supervisor exits instead. A mid-create shape past
    /// [`MID_CREATE_GRACE`] reads as this too. No reader counts it as a job.
    Removed,
    /// Caught mid-create: an empty dir, or one holding only staged
    /// `state.json.tmp.*` records. Claude Code creates the dir, stages the
    /// record, renames it, and only then creates `tmp/` (measured 5/5, 2.1.308),
    /// so a creation never makes the leftover shape and a `tmp/` holding
    /// anything is neither.
    ///
    /// Only while the shape is YOUNG: once its newest mtime is older than
    /// [`MID_CREATE_GRACE`] by the wall clock it reads as an abandoned creation,
    /// [`RecordlessJobDir::Removed`]. Without the bound one leftover dir in the
    /// global store holds every gate closed for good: no macOS rotation, no
    /// tree collection, no background-session adoption, no MCP-login sync
    /// write, and every account reads live. The bound's residuals: a creation
    /// frozen across the grace (a suspended machine) reads abandoned until its
    /// rename lands; an mtime ahead of this host's clock (a stepped-back clock,
    /// a network home whose server runs ahead) cannot prove its age and stays
    /// mid-create until the clock passes it, and a server running behind
    /// shortens the grace by its skew.
    MidCreate,
    /// Any other record-less dir: unreadable, fail closed.
    Unknown,
}

/// How long a record-less job dir may keep the mid-create shape before it is
/// read as an abandoned creation, so a leftover cannot hold the gates closed
/// for good.
const MID_CREATE_GRACE: Duration = Duration::from_secs(10 * 60);

/// Classify a record-less job dir. A list that cannot be read is `Err`, which
/// the caller reads as unreadable (fail closed).
fn recordless_job_dir(job_dir: &Path) -> io::Result<RecordlessJobDir> {
    // The newest mtime across the dir and its entries: a staged record being
    // written keeps the shape young even inside an older dir.
    let mut newest = std::fs::metadata(job_dir)
        .and_then(|meta| meta.modified())
        .ok();
    let mut seen = false;
    let mut leftover = true;
    let mut mid_create = true;
    for entry in std::fs::read_dir(job_dir)? {
        let entry = entry?;
        seen = true;
        let name = entry.file_name();
        let file_type = entry.file_type()?;
        if name == "tmp" {
            if !file_type.is_dir() || std::fs::read_dir(entry.path())?.next().is_some() {
                leftover = false;
            }
        } else if name == "exit-cause" {
            if !file_type.is_file() {
                leftover = false;
            }
        } else {
            leftover = false;
        }
        let staged = name
            .to_str()
            .is_some_and(|name| name.starts_with("state.json.tmp."));
        if !(staged && file_type.is_file()) {
            mid_create = false;
        }
        // An entry whose mtime cannot be read could be the young one: `None`
        // keeps the shape young (fail closed).
        newest = newest
            .zip(entry.metadata().and_then(|meta| meta.modified()).ok())
            .map(|(a, b)| a.max(b));
    }
    if seen && leftover {
        return Ok(RecordlessJobDir::Removed);
    }
    if mid_create {
        // An unreadable mtime, or one in the future, cannot prove the shape is
        // old: it stays mid-create (fail closed).
        let abandoned = newest
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age > MID_CREATE_GRACE);
        if abandoned {
            return Ok(RecordlessJobDir::Removed);
        }
        return Ok(RecordlessJobDir::MidCreate);
    }
    Ok(RecordlessJobDir::Unknown)
}

#[cfg(test)]
#[path = "../tests/inline/claude_jobs.rs"]
mod tests;
