//! Claude Code's `<config dir>/sessions/<pid>.json` record — the fields clauth
//! reads, and the one reader every caller shares.
//!
//! Claude Code rewrites the record in place, so one read can land mid-write and
//! the stamp is never re-taken: the shared reader retries once, so the torn-read
//! retry lives here rather than in each caller.

use std::path::{Path, PathBuf};

use serde::Deserialize;

/// The fields of a Claude Code session record (`<config dir>/sessions/<pid>.json`)
/// clauth reads.
#[derive(serde::Deserialize, Clone)]
pub(crate) struct SessionRecord {
    #[serde(rename = "sessionId")]
    pub(crate) session_id: String,
    pub(crate) pid: u32,
    /// The working dir the session itself runs in. A session can move its
    /// working dir after launch (`--worktree`), and its own record names where
    /// it runs; empty means the record names none.
    ///
    /// Read leniently like `proc_start`: every caller parses this struct, and a
    /// record writing `cwd` as a non-string must not fail their parses.
    #[serde(rename = "cwd", default, deserialize_with = "lenient_string")]
    pub(crate) cwd: String,
    /// The name the record carries (the human label Claude Code shows), for the
    /// `switch --list` name cell. Read leniently like `cwd`: a non-string reads
    /// empty, which the listing maps to no name.
    #[serde(default, deserialize_with = "lenient_string")]
    pub(crate) name: String,
    /// The status the record carries, for the `switch --list --json` status
    /// cell. Read leniently like `name`.
    #[serde(default, deserialize_with = "lenient_string")]
    pub(crate) status: String,
    /// The process start time the record carries, as the record spells it. Both
    /// callers that compare it — the bridge's linux resolver and the scheduler's
    /// ambiguity scan, the latter through [`process_matches_record`] — read it
    /// against `/proc/<pid>/stat` field 22 (measured equal); the macOS form is
    /// unmeasured, so nothing there compares it. `default` is the upgrade gate:
    /// a record written before the field existed reads empty and fails the
    /// linux compare rather than being trusted. The bridge is unix-only, so
    /// Windows reads no field beyond `session_id` and `pid`.
    ///
    /// Read leniently: every other caller parses this struct for `session_id`/
    /// `pid`, and a future Claude Code version writing `procStart` as a
    /// non-string must not fail their parses, so a non-string reads as empty
    /// (A#16) — which the linux compare then rejects.
    #[serde(rename = "procStart", default, deserialize_with = "lenient_string")]
    #[cfg_attr(not(unix), allow(dead_code))]
    pub(crate) proc_start: String,
    /// When the session started, in epoch milliseconds (an integer on
    /// 2.1.305). Read leniently like `procStart`: any other shape reads `None`,
    /// which a caller treats as unknown.
    #[serde(rename = "startedAt", default, deserialize_with = "lenient_u64")]
    pub(crate) started_at: Option<u64>,
}

/// A well-formed JSON string as itself, any other value as empty; a token
/// serde_json refuses (an out-of-range number, a lone-surrogate escape) fails
/// the parse. The one lenient-string reader shared by `procStart`, the record's
/// `cwd` and the job store's `state` and `sessionId`, so a future Claude Code
/// version retyping any of them must not fail every record parse. A non-string
/// degrades to empty, which the field's own rule then reads (an empty `state`
/// is unfinished).
pub(crate) fn lenient_string<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<String, D::Error> {
    let flag = LenientString::deserialize(deserializer)?;
    Ok(flag.0.unwrap_or_default())
}

/// A well-formed JSON string as itself, any other shape as `None`; a token
/// serde_json refuses (an out-of-range number, a lone-surrogate escape) fails
/// the parse. The one lenient element reader shared by [`lenient_string`], the
/// job store's `respawnFlags` decoder, and the tps spend-cap 429 reader. A
/// nested sequence or map drains itself element by element through
/// [`serde::de::IgnoredAny`], which serde_json skips iteratively and outside
/// its recursion limit.
pub(crate) struct LenientString(pub(crate) Option<String>);

impl<'de> serde::Deserialize<'de> for LenientString {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_any(LenientStringVisitor)
    }
}

struct LenientStringVisitor;

impl<'de> serde::de::Visitor<'de> for LenientStringVisitor {
    type Value = LenientString;

    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("a string or any other JSON value")
    }

    fn visit_str<E>(self, v: &str) -> Result<LenientString, E> {
        Ok(LenientString(Some(v.to_owned())))
    }

    fn visit_string<E>(self, v: String) -> Result<LenientString, E> {
        Ok(LenientString(Some(v)))
    }

    fn visit_seq<A>(self, mut seq: A) -> Result<LenientString, A::Error>
    where
        A: serde::de::SeqAccess<'de>,
    {
        while seq.next_element::<serde::de::IgnoredAny>()?.is_some() {}
        Ok(LenientString(None))
    }

    fn visit_map<A>(self, mut map: A) -> Result<LenientString, A::Error>
    where
        A: serde::de::MapAccess<'de>,
    {
        while map
            .next_entry::<serde::de::IgnoredAny, serde::de::IgnoredAny>()?
            .is_some()
        {}
        Ok(LenientString(None))
    }

    fn visit_unit<E>(self) -> Result<LenientString, E> {
        Ok(LenientString(None))
    }

    fn visit_bool<E>(self, _: bool) -> Result<LenientString, E> {
        Ok(LenientString(None))
    }

    fn visit_i64<E>(self, _: i64) -> Result<LenientString, E> {
        Ok(LenientString(None))
    }

    fn visit_u64<E>(self, _: u64) -> Result<LenientString, E> {
        Ok(LenientString(None))
    }

    fn visit_f64<E>(self, _: f64) -> Result<LenientString, E> {
        Ok(LenientString(None))
    }
}

/// A JSON integer that fits a `u64` as itself, any other value as `None`; a
/// token serde_json refuses fails the parse. A nested sequence or map drains
/// through [`serde::de::IgnoredAny`], as in [`LenientString`].
pub(crate) fn lenient_u64<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<u64>, D::Error> {
    deserializer.deserialize_any(LenientU64Visitor)
}

struct LenientU64Visitor;

impl<'de> serde::de::Visitor<'de> for LenientU64Visitor {
    type Value = Option<u64>;

    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("an integer or any other JSON value")
    }

    fn visit_u64<E>(self, v: u64) -> Result<Option<u64>, E> {
        Ok(Some(v))
    }

    fn visit_i64<E>(self, v: i64) -> Result<Option<u64>, E> {
        Ok(u64::try_from(v).ok())
    }

    fn visit_f64<E>(self, _: f64) -> Result<Option<u64>, E> {
        Ok(None)
    }

    fn visit_str<E>(self, _: &str) -> Result<Option<u64>, E> {
        Ok(None)
    }

    fn visit_bool<E>(self, _: bool) -> Result<Option<u64>, E> {
        Ok(None)
    }

    fn visit_unit<E>(self) -> Result<Option<u64>, E> {
        Ok(None)
    }

    fn visit_seq<A>(self, mut seq: A) -> Result<Option<u64>, A::Error>
    where
        A: serde::de::SeqAccess<'de>,
    {
        while seq.next_element::<serde::de::IgnoredAny>()?.is_some() {}
        Ok(None)
    }

    fn visit_map<A>(self, mut map: A) -> Result<Option<u64>, A::Error>
    where
        A: serde::de::MapAccess<'de>,
    {
        while map
            .next_entry::<serde::de::IgnoredAny, serde::de::IgnoredAny>()?
            .is_some()
        {}
        Ok(None)
    }
}

/// The record for `pid`, `None` when it does not name `pid` itself with a
/// non-empty session id. The torn-read retry lives here: a first read that lands
/// mid-write fails to parse, so a second read decides and the stamp is never
/// re-taken.
pub(crate) fn session_record(dir: &Path, pid: u32) -> Option<SessionRecord> {
    let path = dir.join(format!("{pid}.json"));
    let record = (0..2)
        .find_map(|_| serde_json::from_slice::<SessionRecord>(&std::fs::read(&path).ok()?).ok())?;
    (record.pid == pid && !record.session_id.is_empty()).then_some(record)
}

/// The sessions dir a live-session row's Claude Code record lives in: a
/// background row writes through the account's fixed `runtime-bg` tree, else
/// the runtime tree [`crate::runtime::session_sessions_tree`] picks (a
/// fake-link box holds the record off `~/.claude`), else `~/.claude/sessions`.
pub(crate) fn session_sessions_dir(row: &crate::live_sessions::LiveSession) -> Option<PathBuf> {
    if row.background {
        let profile = crate::profile::ProfileName::from(row.start_profile.as_str());
        return crate::runtime::background_runtime_tree(&profile)
            .ok()
            .map(|tree| tree.join("sessions"));
    }
    if let Some(tree) =
        crate::runtime::session_sessions_tree(&row.start_profile, row.isolated, &row.session_id)
    {
        return Some(tree.join("sessions"));
    }
    crate::profile::claude_dir()
        .ok()
        .map(|d| d.join("sessions"))
}

/// The Claude Code record a live-session row names: the delegate child's
/// record for a delegate (its row is re-keyed onto that pid), else the
/// `claude` child's; `None` when the row carries no pid or no record reads.
pub(crate) fn live_record(row: &crate::live_sessions::LiveSession) -> Option<SessionRecord> {
    let pid = if row.is_delegate {
        row.pid
    } else {
        row.claude_pid?
    };
    let dir = session_sessions_dir(row)?;
    session_record(&dir, pid)
}

/// The dir a live row's session runs in: its Claude Code record's non-empty
/// `cwd`, else the dir clauth launched it from.
pub(crate) fn live_cwd(
    row: &crate::live_sessions::LiveSession,
    record: Option<&SessionRecord>,
) -> Option<String> {
    record
        .map(|r| r.cwd.as_str())
        .filter(|cwd| !cwd.is_empty())
        .map(str::to_owned)
        .or_else(|| row.cwd.as_ref().map(|p| p.to_string_lossy().into_owned()))
}

/// Whether `/proc/<pid>/stat` field 22 (the start time in clock ticks since
/// boot) equals `proc_start` (measured 2.1.301). The one field-22 compare,
/// shared by [`process_matches_record`] and the bridge's gone-or-moved check.
#[cfg(target_os = "linux")]
pub(crate) fn proc_start_matches(pid: u32, proc_start: &str) -> bool {
    let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return false;
    };
    let Some(rest) = stat.rsplit_once(')') else {
        return false;
    };
    let Some(field) = rest.1.split_whitespace().nth(19) else {
        return false;
    };
    field == proc_start
}

/// Whether the process holding `pid` is the one `record` names: on linux the
/// record's `proc_start` equals `/proc/<pid>/stat` field 22 (a record with an
/// empty start field fails the compare — it predates the field); on macOS only
/// liveness is compared (the `procStart` form there is unmeasured); elsewhere
/// false — the accepted degraded case, where a record's process can never be
/// matched.
pub(crate) fn process_matches_record(pid: u32, record: &SessionRecord) -> bool {
    #[cfg(target_os = "linux")]
    {
        // A Claude Code that stops writing `procStart` makes no conversation
        // ambiguous on linux — the accepted cost of never counting an empty
        // start, which a crashed session's recycled-pid record would exploit.
        !record.proc_start.is_empty() && proc_start_matches(pid, &record.proc_start)
    }
    #[cfg(target_os = "macos")]
    {
        let _ = record;
        crate::runtime::namespaced_keychain_ledger::pid_alive(pid)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = (pid, record);
        false
    }
}

/// The parent of process `pid`, where it can be read: `/proc/<pid>/stat` on
/// linux, `proc_pidinfo` on macOS. Elsewhere none, the same degraded case as
/// [`process_matches_record`].
#[cfg(target_os = "linux")]
pub(crate) fn parent_pid(pid: u32) -> Option<u32> {
    // Field 4 of `/proc/<pid>/stat` is the ppid; the comm (field 2) may hold
    // spaces and parentheses, so the split runs from the closing parenthesis.
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let rest = stat.rsplit_once(')')?.1;
    rest.split_whitespace().nth(1)?.parse().ok()
}

/// See the linux [`parent_pid`].
#[cfg(target_os = "macos")]
pub(crate) fn parent_pid(pid: u32) -> Option<u32> {
    crate::runtime::proc_bsdinfo(pid).map(|info| info.pbi_ppid)
}

/// See the linux [`parent_pid`].
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub(crate) fn parent_pid(_pid: u32) -> Option<u32> {
    None
}

/// How far up a process's parents [`first_ancestor`] looks: a `clauth start`
/// child or a delegate's child sits one level below its row's process, and a
/// shell or a nested `claude` between them adds a level each.
pub(crate) const ANCESTOR_DEPTH: usize = 4;

/// The first answer `probe` gives over `pid`'s first [`ANCESTOR_DEPTH`]
/// parents, nearest first. The walk ends at an unreadable parent and at pid 1
/// or below: the init process and the kernel adopt orphans, so no session is
/// theirs.
pub(crate) fn first_ancestor<T>(pid: u32, mut probe: impl FnMut(u32) -> Option<T>) -> Option<T> {
    let mut pid = pid;
    for _ in 0..ANCESTOR_DEPTH {
        pid = parent_pid(pid)?;
        if pid <= 1 {
            return None;
        }
        if let Some(found) = probe(pid) {
            return Some(found);
        }
    }
    None
}
