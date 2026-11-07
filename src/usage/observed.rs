//! Usage that Claude Code sessions observed on their own replies, folded into
//! the store while `/api/oauth/usage` is unreadable.
//!
//! Every `/v1/messages` reply carries the `anthropic-ratelimit-unified-*`
//! headers, and Claude Code hands their 5h and 7d figures to its status line
//! as `rate_limits`. `clauth statusline` ([`crate::statusline`]) records them
//! per profile in `~/.clauth/observed-usage/<profile>.json`; any other status
//! line may write the same record:
//!
//! ```json
//! {
//!   "schema": 1,
//!   "profile": "personal",
//!   "updated_at": 1730930000,
//!   "windows": {
//!     "5h": { "used_percentage": 12.0, "resets_at": 1730940000, "seen_at": 1730930000 },
//!     "7d": { "used_percentage": 35.0, "resets_at": 1731229200, "seen_at": 1730930000 }
//!   }
//! }
//! ```
//!
//! `used_percentage` is 0-100; `resets_at` and `seen_at` are epoch seconds.
//! A figure is a floor as of `seen_at`: usage inside one window never goes
//! down.
//!
//! When many sessions share an account, `/usage` answers 429 for long
//! stretches and the store keeps the last body, while those sessions keep
//! reading fresh figures off every reply. [`overlay`] lets a reading that is
//! newer than the stored body move that body's 5h and 7d windows, so the
//! surfaces and the auto-switch scan see current usage with no extra request.
//! It never touches the fetch status, the cadence, the streaks or the disk
//! cache: the poll stays visibly rate limited, the next live body overwrites
//! the store as usual, and a reading older than that body is ignored, so a
//! wrong reading lasts at most until the next live poll.
//!
//! The switch scans act only on a reading they trust ([`is_fresh`] beside the
//! `Fresh` status): a profile whose body a reading seen in the last
//! [`FRESH_SECS`] moved or confirmed, on a window still running, counts as
//! freshly read. The scans' own exhaustion and headroom gates still apply.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};

use serde::{Deserialize, Serialize};

use super::fetch::{UsageInfo, UsageWindow, epoch_secs_to_iso, iso_to_epoch_secs};

/// `~/.clauth/<OBSERVED_DIR>/`.
pub(crate) const OBSERVED_DIR: &str = "observed-usage";

/// The only file shape read; any other is skipped.
const SCHEMA: u64 = 1;

/// Two resets this close name the same window (header resets are whole
/// seconds, `/usage` resets carry a fraction).
const SAME_WINDOW_SECS: i64 = 600;

/// A record is a few hundred bytes; anything far larger is not one.
const MAX_FILE_BYTES: u64 = 64 * 1024;

/// How recent a reading must be for the switch scans to trust it like a live
/// `/usage` body: a few poll intervals, so an idle account's reading ages out
/// instead of standing in for a poll forever.
pub(crate) const FRESH_SECS: i64 = 300;

/// Profile name → the `seen_at` of its latest trusted reading. A leaf lock:
/// never held while another is taken.
static FRESH: LazyLock<Mutex<HashMap<String, i64>>> = LazyLock::new(Mutex::default);

#[derive(Debug, Clone, Copy, PartialEq, Deserialize, Serialize)]
pub(crate) struct ObservedWindow {
    pub(crate) used_percentage: f64,
    pub(crate) resets_at: i64,
    #[serde(default)]
    pub(crate) seen_at: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
pub(crate) struct ObservedWindows {
    #[serde(default, rename = "5h", skip_serializing_if = "Option::is_none")]
    pub(crate) five_hour: Option<ObservedWindow>,
    #[serde(default, rename = "7d", skip_serializing_if = "Option::is_none")]
    pub(crate) seven_day: Option<ObservedWindow>,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub(crate) struct Observed {
    schema: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) profile: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) source: Option<String>,
    #[serde(default)]
    pub(crate) updated_at: i64,
    #[serde(default)]
    pub(crate) windows: ObservedWindows,
}

// ── writing a record ────────────────────────────────────────────────────────

/// One window a status line input reported.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Reading {
    /// 0-100.
    pub(crate) used_percentage: f64,
    /// Epoch seconds.
    pub(crate) resets_at: i64,
}

impl Reading {
    /// A finite, non-negative figure with a positive reset, else `None`.
    pub(crate) fn new(used_percentage: f64, resets_at: f64) -> Option<Self> {
        let valid = used_percentage.is_finite()
            && used_percentage >= 0.0
            && resets_at.is_finite()
            && resets_at > 0.0;
        // `as` saturates on a float past i64's range; the check above has
        // already refused NaN and the infinities.
        let resets_at = resets_at.floor() as i64;
        valid.then_some(Self {
            used_percentage,
            resets_at,
        })
    }
}

/// An unchanged window is re-written at most this often, only to advance its
/// `seen_at`.
const CONFIRM_EVERY_SECS: i64 = 30;

/// The window to store after `incoming`, or `None` when the stored one stands
/// unchanged. Same rules as [`fold_window`]: within one window the higher
/// figure wins, a later reset starts a new window, an earlier one is an old
/// reply and is ignored.
fn next_window(stored: Option<ObservedWindow>, incoming: Reading, now: i64) -> Option<ObservedWindow> {
    let fresh = |r: Reading| ObservedWindow {
        used_percentage: r.used_percentage,
        resets_at: r.resets_at,
        seen_at: now,
    };
    let Some(stored) = stored else {
        return Some(fresh(incoming));
    };
    let gap = incoming.resets_at - stored.resets_at;
    if gap > SAME_WINDOW_SECS {
        Some(fresh(incoming))
    } else if gap < -SAME_WINDOW_SECS {
        None
    } else if incoming.used_percentage > stored.used_percentage {
        Some(ObservedWindow {
            used_percentage: incoming.used_percentage,
            resets_at: stored.resets_at,
            seen_at: now,
        })
    } else if now - stored.seen_at >= CONFIRM_EVERY_SECS {
        Some(ObservedWindow {
            seen_at: now,
            ..stored
        })
    } else {
        None
    }
}

/// Fold one input's windows into `existing`. `None` when nothing changed
/// enough to be worth a write.
pub(crate) fn fold_record(
    existing: Option<Observed>,
    profile: &str,
    five_hour: Option<Reading>,
    seven_day: Option<Reading>,
    now: i64,
) -> Option<Observed> {
    let mut record = existing.unwrap_or_else(|| Observed {
        schema: SCHEMA,
        profile: None,
        source: None,
        updated_at: 0,
        windows: ObservedWindows::default(),
    });
    let mut changed = record.updated_at == 0;
    for (slot, incoming) in [
        (&mut record.windows.five_hour, five_hour),
        (&mut record.windows.seven_day, seven_day),
    ] {
        if let Some(incoming) = incoming
            && let Some(next) = next_window(*slot, incoming, now)
        {
            *slot = Some(next);
            changed = true;
        }
    }
    changed.then(|| Observed {
        profile: Some(profile.to_owned()),
        source: Some("clauth".to_owned()),
        updated_at: now,
        ..record
    })
}

/// Fold an input into `dir/<profile>.json` and write it back (0o600, atomic).
pub(crate) fn record(
    dir: &Path,
    profile: &str,
    five_hour: Option<Reading>,
    seven_day: Option<Reading>,
    now: i64,
) -> std::io::Result<()> {
    if !file_name_safe(profile) {
        return Ok(());
    }
    let Some(next) = fold_record(read(dir, profile), profile, five_hour, seven_day, now) else {
        return Ok(());
    };
    std::fs::create_dir_all(dir)?;
    let bytes = serde_json::to_vec(&next).map_err(std::io::Error::other)?;
    crate::profile::atomic_write_600(&dir.join(format!("{profile}.json")), bytes)
}

// ── whose reading it is ─────────────────────────────────────────────────────

/// Where a reading's candidate profile came from, strongest first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Source {
    /// A `clauth start` runtime's `CLAUDE_CONFIG_DIR`.
    ConfigDir,
    /// The owner clauth's own hook stamped on the session id.
    SessionOwner,
    /// The global login: the active profile, which a switch may have moved
    /// since the session last read its credentials.
    Global,
}

const WEEK_SECS: i64 = 7 * 24 * 60 * 60;

/// Two weekly resets this close (mod one week) name the same account. Header
/// resets are whole seconds and `/usage` ones carry a fraction, both on the
/// hour, so ten minutes cannot join two accounts an hour apart.
const FINGERPRINT_SECS: i64 = 600;

/// Whether two weekly resets fall at the same moment of the week.
pub(crate) fn same_hour_of_week(a: i64, b: i64) -> bool {
    let d = (a - b).rem_euclid(WEEK_SECS);
    d <= FINGERPRINT_SECS || d >= WEEK_SECS - FINGERPRINT_SECS
}

/// The profile a reading belongs to, or `None` when that cannot be told.
///
/// Every account's 7d window resets at its own hour of the week, so the
/// reading's 7d reset confirms the candidate against `prints` (each profile's
/// stored 7d reset). A reading another profile's reset matches goes to that
/// profile; one that matches none or several, with no confirmed candidate, is
/// dropped. With nothing to confirm against, only a candidate the session names
/// for itself is trusted: a global session may have moved with a switch.
pub(crate) fn attribute(
    candidate: Option<(String, Source)>,
    weekly_reset: Option<i64>,
    prints: &[(String, i64)],
) -> Option<String> {
    let Some(reset) = weekly_reset.filter(|_| !prints.is_empty()) else {
        return candidate
            .filter(|(_, source)| *source != Source::Global)
            .map(|(name, _)| name);
    };
    let matches: Vec<&str> = prints
        .iter()
        .filter(|(_, r)| same_hour_of_week(*r, reset))
        .map(|(n, _)| n.as_str())
        .collect();
    if let Some((name, source)) = &candidate {
        if matches.contains(&name.as_str()) {
            return Some(name.clone());
        }
        let has_print = prints.iter().any(|(n, _)| n == name);
        if !has_print && matches.is_empty() && *source != Source::Global {
            return Some(name.clone());
        }
    }
    match matches.as_slice() {
        [only] => Some((*only).to_owned()),
        _ => None,
    }
}

/// Each profile's stored 7d reset, read off its usage cache under
/// `~/.clauth/profiles/`, for [`attribute`].
pub(crate) fn weekly_resets() -> Vec<(String, i64)> {
    let Ok(profiles) = crate::profile::clauth_dir().map(|d| d.join("profiles")) else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(profiles) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| {
            let raw = entry.file_name();
            let raw = raw.to_str().filter(|n| file_name_safe(n))?;
            let name = crate::profile::ProfileName::from(raw);
            let info: UsageInfo = crate::profile_cache::load_profile_cache(
                &name,
                crate::profile_cache::USAGE_CACHE_FILE,
            )?;
            let reset = iso_to_epoch_secs(info.seven_day?.resets_at.as_deref()?)?;
            Some((name.as_str().to_owned(), reset))
        })
        .collect()
}

/// A schema-1 record, else `None`.
pub(crate) fn parse(bytes: &[u8]) -> Option<Observed> {
    let observed: Observed = serde_json::from_slice(bytes).ok()?;
    (observed.schema == SCHEMA).then_some(observed)
}

/// `~/.clauth/observed-usage/`, when the home resolves.
pub(crate) fn dir() -> Option<PathBuf> {
    crate::profile::clauth_dir()
        .ok()
        .map(|d| d.join(OBSERVED_DIR))
}

/// A store key safe to join onto the directory as a file name.
fn file_name_safe(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('.')
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

/// The record for `name` in `dir`, when there is a readable schema-1 one.
pub(crate) fn read(dir: &Path, name: &str) -> Option<Observed> {
    if !file_name_safe(name) {
        return None;
    }
    let path = dir.join(format!("{name}.json"));
    if std::fs::metadata(&path).ok()?.len() > MAX_FILE_BYTES {
        return None;
    }
    parse(&std::fs::read(path).ok()?)
}

/// When the stored body was fetched, in epoch seconds.
fn body_secs(info: &UsageInfo) -> Option<i64> {
    info.fetched_at
        .map(|ms| i64::try_from(ms / 1000).unwrap_or(i64::MAX))
}

/// A finite, non-negative figure on a window still running at `now`, seen
/// after the stored body was fetched.
fn usable(seen: &ObservedWindow, body_secs: Option<i64>, now: i64) -> bool {
    seen.used_percentage.is_finite()
        && seen.used_percentage >= 0.0
        && seen.resets_at > now
        && body_secs.is_none_or(|fetched| seen.seen_at > fetched)
}

/// Fold one observed window into a stored one. `body_secs` is when the stored
/// body was fetched; only a reading seen after it may move the window.
fn fold_window(
    slot: &mut Option<UsageWindow>,
    seen: &ObservedWindow,
    body_secs: Option<i64>,
    now: i64,
) -> bool {
    if !usable(seen, body_secs, now) {
        return false;
    }
    let stored_reset = slot
        .as_ref()
        .and_then(|w| w.resets_at.as_deref())
        .and_then(iso_to_epoch_secs);
    match (slot.as_mut(), stored_reset) {
        // The same window: the higher figure is the later one.
        (Some(window), Some(reset)) if (seen.resets_at - reset).abs() <= SAME_WINDOW_SECS => {
            if seen.used_percentage > window.utilization {
                window.utilization = seen.used_percentage;
                true
            } else {
                false
            }
        }
        // A reading about a window that ended before the stored one began.
        (_, Some(reset)) if seen.resets_at < reset - SAME_WINDOW_SECS => false,
        // No stored window, one with no reset, or a later window began.
        _ => {
            *slot = Some(UsageWindow {
                utilization: seen.used_percentage,
                resets_at: Some(epoch_secs_to_iso(seen.resets_at)),
            });
            true
        }
    }
}

/// The latest `seen_at` among `observed`'s windows a fold would trust: a
/// finite figure on a window still running at `now`, seen after the stored
/// body was fetched. `None` when there is none.
pub(crate) fn usable_seen_at(info: &UsageInfo, observed: &Observed, now: i64) -> Option<i64> {
    let body_secs = body_secs(info);
    [&observed.windows.seven_day, &observed.windows.five_hour]
        .into_iter()
        .flatten()
        .filter(|w| usable(w, body_secs, now))
        .map(|w| w.seen_at)
        .max()
}

/// Record that `name`'s body carries a reading seen at `seen_at`.
pub(crate) fn note_fresh(name: &str, seen_at: i64) {
    if let Ok(mut fresh) = FRESH.lock() {
        let at = fresh.entry(name.to_owned()).or_insert(seen_at);
        *at = (*at).max(seen_at);
    }
}

/// Whether `name` carries a reading seen within [`FRESH_SECS`] of `now`.
pub(crate) fn is_fresh(name: &str, now: i64) -> bool {
    FRESH
        .lock()
        .ok()
        .and_then(|fresh| fresh.get(name).copied())
        .is_some_and(|seen| seen <= now && now - seen <= FRESH_SECS)
}

/// Fold a record into a stored body. Returns whether any window moved. A plan
/// the store marks as having no 5h limit keeps its 5h window empty.
pub(crate) fn overlay(info: &mut UsageInfo, observed: &Observed, now: i64) -> bool {
    let body_secs = body_secs(info);
    let mut moved = false;
    if let Some(seen) = &observed.windows.seven_day {
        moved |= fold_window(&mut info.seven_day, seen, body_secs, now);
    }
    if !info.no_five_hour_limit
        && let Some(seen) = &observed.windows.five_hour
    {
        moved |= fold_window(&mut info.five_hour, seen, body_secs, now);
    }
    moved
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_794_000_000;
    const WEEK_RESET: i64 = 1_794_301_200;
    const BODY_AT: i64 = NOW - 3_000;

    fn parsed(json: &str) -> Observed {
        parse(json.as_bytes()).expect("a schema-1 record")
    }

    fn weekly(used: f64, reset: i64, seen: i64) -> Observed {
        Observed {
            schema: SCHEMA,
            profile: None,
            source: None,
            updated_at: 0,
            windows: ObservedWindows {
                five_hour: None,
                seven_day: Some(ObservedWindow {
                    used_percentage: used,
                    resets_at: reset,
                    seen_at: seen,
                }),
            },
        }
    }

    fn body(seven_day: Option<(f64, i64)>) -> UsageInfo {
        UsageInfo {
            seven_day: seven_day.map(|(u, r)| UsageWindow {
                utilization: u,
                resets_at: Some(epoch_secs_to_iso(r)),
            }),
            fetched_at: Some(u64::try_from(BODY_AT).unwrap() * 1000),
            ..UsageInfo::default()
        }
    }

    fn seven(info: &UsageInfo) -> (f64, Option<i64>) {
        let w = info.seven_day.as_ref().expect("a 7d window");
        (
            w.utilization,
            w.resets_at.as_deref().and_then(iso_to_epoch_secs),
        )
    }

    #[test]
    fn should_parse_the_documented_record() {
        let o = parsed(
            r#"{"schema":1,"profile":"p","updated_at":1,"windows":{
                "5h":{"used_percentage":12.0,"resets_at":20,"seen_at":10},
                "7d":{"used_percentage":35.5,"resets_at":30,"seen_at":11}}}"#,
        );
        assert_eq!(
            o.windows.five_hour,
            Some(ObservedWindow {
                used_percentage: 12.0,
                resets_at: 20,
                seen_at: 10
            })
        );
        assert_eq!(o.windows.seven_day.map(|w| w.used_percentage), Some(35.5));
    }

    #[test]
    fn should_skip_another_schema_and_garbage() {
        assert_eq!(parse(br#"{"schema":2,"windows":{}}"#), None);
        assert_eq!(parse(b"not json"), None);
        assert_eq!(parse(br#"{"windows":{}}"#), None);
    }

    #[test]
    fn should_raise_a_stale_body_to_a_newer_reading_of_the_same_window() {
        let mut info = body(Some((30.0, WEEK_RESET)));
        assert!(overlay(&mut info, &weekly(41.0, WEEK_RESET, NOW), NOW));
        assert_eq!(seven(&info), (41.0, Some(WEEK_RESET)));
    }

    #[test]
    fn should_match_a_whole_second_reset_to_a_fractional_one() {
        let mut info = body(None);
        info.seven_day = Some(UsageWindow {
            utilization: 30.0,
            resets_at: Some("2026-11-10T09:00:00.783034+00:00".into()),
        });
        assert!(overlay(&mut info, &weekly(41.0, WEEK_RESET, NOW), NOW));
        assert_eq!(seven(&info).0, 41.0);
    }

    #[test]
    fn should_never_lower_a_window() {
        let mut info = body(Some((50.0, WEEK_RESET)));
        assert!(!overlay(&mut info, &weekly(41.0, WEEK_RESET, NOW), NOW));
        assert_eq!(seven(&info).0, 50.0);
    }

    #[test]
    fn should_ignore_a_reading_older_than_the_body() {
        let mut info = body(Some((30.0, WEEK_RESET)));
        assert!(!overlay(
            &mut info,
            &weekly(90.0, WEEK_RESET, BODY_AT - 1),
            NOW
        ));
        assert!(!overlay(&mut info, &weekly(90.0, WEEK_RESET, BODY_AT), NOW));
        assert_eq!(seven(&info).0, 30.0);
    }

    #[test]
    fn should_start_the_window_a_later_reset_names() {
        // the body still holds last week's window, already lapsed
        let mut info = body(Some((97.0, NOW - 60)));
        let next_week = NOW - 60 + 7 * 86_400;
        assert!(overlay(&mut info, &weekly(2.0, next_week, NOW), NOW));
        assert_eq!(seven(&info), (2.0, Some(next_week)));
    }

    #[test]
    fn should_ignore_a_reading_of_an_earlier_window() {
        let mut info = body(Some((5.0, WEEK_RESET + 7 * 86_400)));
        assert!(!overlay(&mut info, &weekly(97.0, WEEK_RESET, NOW), NOW));
        assert_eq!(seven(&info).0, 5.0);
    }

    #[test]
    fn should_ignore_a_lapsed_or_malformed_reading() {
        let mut info = body(None);
        assert!(!overlay(&mut info, &weekly(40.0, NOW, NOW), NOW));
        assert!(!overlay(&mut info, &weekly(f64::NAN, WEEK_RESET, NOW), NOW));
        assert!(!overlay(&mut info, &weekly(-1.0, WEEK_RESET, NOW), NOW));
        assert!(info.seven_day.is_none());
    }

    #[test]
    fn should_fill_a_window_the_body_lacks() {
        let mut info = body(None);
        assert!(overlay(&mut info, &weekly(35.0, WEEK_RESET, NOW), NOW));
        assert_eq!(seven(&info), (35.0, Some(WEEK_RESET)));
    }

    #[test]
    fn should_fill_a_body_never_fetched() {
        let mut info = UsageInfo::default();
        assert!(overlay(&mut info, &weekly(35.0, WEEK_RESET, 1), NOW));
        assert_eq!(seven(&info).0, 35.0);
    }

    #[test]
    fn should_leave_the_5h_window_of_a_plan_without_one() {
        let five = ObservedWindow {
            used_percentage: 20.0,
            resets_at: NOW + 3_600,
            seen_at: NOW,
        };
        let o = Observed {
            schema: SCHEMA,
            profile: None,
            source: None,
            updated_at: 0,
            windows: ObservedWindows {
                five_hour: Some(five),
                seven_day: None,
            },
        };
        let mut marked = body(None);
        marked.no_five_hour_limit = true;
        assert!(!overlay(&mut marked, &o, NOW));
        assert!(marked.five_hour.is_none());
        let mut plain = body(None);
        assert!(overlay(&mut plain, &o, NOW));
        assert_eq!(plain.five_hour.map(|w| w.utilization), Some(20.0));
    }

    #[test]
    fn should_trust_only_a_newer_live_reading_for_freshness() {
        let info = body(Some((30.0, WEEK_RESET)));
        assert_eq!(
            usable_seen_at(&info, &weekly(30.0, WEEK_RESET, NOW), NOW),
            Some(NOW)
        );
        // a confirmation that moves nothing still counts as a fresh read
        assert_eq!(
            usable_seen_at(&info, &weekly(10.0, WEEK_RESET, NOW - 5), NOW),
            Some(NOW - 5)
        );
        assert_eq!(
            usable_seen_at(&info, &weekly(30.0, WEEK_RESET, BODY_AT), NOW),
            None
        );
        assert_eq!(usable_seen_at(&info, &weekly(30.0, NOW, NOW), NOW), None);
    }

    #[test]
    fn should_age_a_fresh_mark_out_after_fresh_secs() {
        let name = "observed-fresh-test-profile";
        assert!(!is_fresh(name, NOW));
        note_fresh(name, NOW);
        // an older note never pulls the mark back
        note_fresh(name, NOW - 1_000);
        assert!(is_fresh(name, NOW));
        assert!(is_fresh(name, NOW + FRESH_SECS));
        assert!(!is_fresh(name, NOW + FRESH_SECS + 1));
        // a mark from the future is not trusted
        assert!(!is_fresh(name, NOW - 1));
    }

    fn reading(used: f64, reset: i64) -> Reading {
        Reading {
            used_percentage: used,
            resets_at: reset,
        }
    }

    #[test]
    fn should_create_a_record_from_the_first_reading() {
        let r = fold_record(None, "p", None, Some(reading(35.0, WEEK_RESET)), NOW).expect("a record");
        assert_eq!(r.profile.as_deref(), Some("p"));
        assert_eq!(r.updated_at, NOW);
        assert_eq!(
            r.windows.seven_day,
            Some(ObservedWindow {
                used_percentage: 35.0,
                resets_at: WEEK_RESET,
                seen_at: NOW
            })
        );
        let back = parse(&serde_json::to_vec(&r).expect("serializes")).expect("round trips");
        assert_eq!(back, r);
    }

    #[test]
    fn should_never_lower_a_recorded_window() {
        let r = fold_record(None, "p", None, Some(reading(40.0, WEEK_RESET)), NOW).expect("a record");
        assert_eq!(
            fold_record(Some(r.clone()), "p", None, Some(reading(35.0, WEEK_RESET)), NOW + 1),
            None
        );
        let up = fold_record(Some(r), "p", None, Some(reading(41.0, WEEK_RESET)), NOW + 1).expect("raised");
        assert_eq!(up.windows.seven_day.map(|w| w.used_percentage), Some(41.0));
    }

    #[test]
    fn should_record_a_new_window_and_ignore_an_older_one() {
        let r = fold_record(None, "p", Some(reading(90.0, NOW + 100)), None, NOW).expect("a record");
        let next = fold_record(Some(r), "p", Some(reading(3.0, NOW + 18_000)), None, NOW + 200)
            .expect("a new window");
        assert_eq!(
            next.windows.five_hour.map(|w| (w.used_percentage, w.resets_at)),
            Some((3.0, NOW + 18_000))
        );
        assert_eq!(
            fold_record(Some(next), "p", Some(reading(95.0, NOW + 100)), None, NOW + 300),
            None
        );
    }

    #[test]
    fn should_confirm_an_unchanged_window_every_thirty_seconds() {
        let r = fold_record(None, "p", None, Some(reading(35.0, WEEK_RESET)), NOW).expect("a record");
        assert_eq!(
            fold_record(Some(r.clone()), "p", None, Some(reading(35.0, WEEK_RESET)), NOW + 29),
            None
        );
        let later = fold_record(Some(r), "p", None, Some(reading(35.0, WEEK_RESET)), NOW + 30)
            .expect("confirmed");
        assert_eq!(later.windows.seven_day.map(|w| w.seen_at), Some(NOW + 30));
    }

    #[test]
    fn should_write_and_reread_a_record_on_disk() {
        let dir = tempfile::tempdir().expect("tempdir");
        record(dir.path(), "p", None, Some(reading(35.0, WEEK_RESET)), NOW).expect("writes");
        let got = read(dir.path(), "p").expect("reads back");
        assert_eq!(got.windows.seven_day.map(|w| w.used_percentage), Some(35.0));
        record(dir.path(), "../evil", None, Some(reading(1.0, WEEK_RESET)), NOW).expect("refused quietly");
        assert!(!dir.path().join("../evil.json").exists());
    }

    #[test]
    fn should_refuse_a_non_finite_or_negative_reading() {
        assert!(Reading::new(f64::NAN, 1.0).is_none());
        assert!(Reading::new(-1.0, 1.0).is_none());
        assert!(Reading::new(1.0, f64::INFINITY).is_none());
        assert!(Reading::new(1.0, 0.0).is_none());
        assert_eq!(Reading::new(35.0, 99.9), Some(reading(35.0, 99)));
    }

    // 2026-11-10T09:00Z and 2026-11-11T07:00Z: two accounts' weekly resets.
    const PERSONAL: i64 = WEEK_RESET;
    const WORK: i64 = 1_794_380_400;

    fn prints() -> Vec<(String, i64)> {
        vec![("personal".into(), PERSONAL), ("work".into(), WORK)]
    }

    #[test]
    fn should_confirm_a_candidate_its_weekly_reset_matches() {
        let got = attribute(Some(("personal".into(), Source::Global)), Some(PERSONAL), &prints());
        assert_eq!(got.as_deref(), Some("personal"));
    }

    #[test]
    fn should_move_a_reading_to_the_profile_its_weekly_reset_names() {
        // a global session still on `work` after the global switch to personal
        let got = attribute(Some(("personal".into(), Source::Global)), Some(WORK), &prints());
        assert_eq!(got.as_deref(), Some("work"));
    }

    #[test]
    fn should_match_a_reset_a_week_on_and_a_fraction_off() {
        assert_eq!(attribute(None, Some(PERSONAL + WEEK_SECS + 1), &prints()).as_deref(), Some("personal"));
    }

    #[test]
    fn should_drop_a_reading_none_or_several_profiles_match() {
        assert_eq!(attribute(Some(("personal".into(), Source::Global)), Some(PERSONAL + 3_600), &prints()), None);
        let twins = vec![("a".into(), PERSONAL), ("b".into(), PERSONAL - WEEK_SECS)];
        assert_eq!(attribute(None, Some(PERSONAL), &twins), None);
    }

    #[test]
    fn should_trust_only_a_named_session_without_a_fingerprint() {
        assert_eq!(attribute(Some(("tgp".into(), Source::ConfigDir)), None, &prints()).as_deref(), Some("tgp"));
        assert_eq!(
            attribute(Some(("tgp".into(), Source::SessionOwner)), Some(PERSONAL + 3_600), &prints()).as_deref(),
            Some("tgp")
        );
        assert_eq!(attribute(Some(("personal".into(), Source::Global)), None, &prints()), None);
        assert_eq!(attribute(Some(("personal".into(), Source::Global)), Some(PERSONAL), &[]), None);
    }

    #[test]
    fn should_not_trust_a_named_session_another_profile_fingerprints() {
        let got = attribute(Some(("tgp".into(), Source::ConfigDir)), Some(PERSONAL), &prints());
        assert_eq!(got.as_deref(), Some("personal"));
    }

    #[test]
    fn should_read_only_safe_file_names() {
        assert!(file_name_safe("brane-danielnaves"));
        assert!(file_name_safe("a.b_c"));
        assert!(!file_name_safe("../x"));
        assert!(!file_name_safe(".hidden"));
        assert!(!file_name_safe("a/b"));
        assert!(!file_name_safe(""));
    }

    #[test]
    fn should_read_a_record_from_disk_and_skip_a_missing_or_oversized_one() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("p.json"),
            br#"{"schema":1,"windows":{"7d":{"used_percentage":1.0,"resets_at":2,"seen_at":3}}}"#,
        )
        .unwrap();
        assert!(read(dir.path(), "p").is_some());
        assert!(read(dir.path(), "q").is_none());
        assert!(read(dir.path(), "../p").is_none());
        let big = vec![b' '; usize::try_from(MAX_FILE_BYTES).unwrap() + 1];
        std::fs::write(dir.path().join("big.json"), big).unwrap();
        assert!(read(dir.path(), "big").is_none());
    }
}
