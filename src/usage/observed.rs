//! Usage that Claude Code sessions observed on their own replies, folded into
//! the store while `/api/oauth/usage` is unreadable.
//!
//! Every `/v1/messages` reply carries the `anthropic-ratelimit-unified-*`
//! headers, and Claude Code hands their 5h and 7d figures to its status line
//! as `rate_limits`. A status line (ccstatusline-rs, or any other) may record
//! them per profile in `~/.clauth/observed-usage/<profile>.json`:
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

use serde::Deserialize;

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

#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
pub(crate) struct ObservedWindow {
    pub(crate) used_percentage: f64,
    pub(crate) resets_at: i64,
    #[serde(default)]
    pub(crate) seen_at: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub(crate) struct ObservedWindows {
    #[serde(default, rename = "5h")]
    pub(crate) five_hour: Option<ObservedWindow>,
    #[serde(default, rename = "7d")]
    pub(crate) seven_day: Option<ObservedWindow>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub(crate) struct Observed {
    schema: u64,
    #[serde(default)]
    pub(crate) windows: ObservedWindows,
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

    fn record(json: &str) -> Observed {
        parse(json.as_bytes()).expect("a schema-1 record")
    }

    fn weekly(used: f64, reset: i64, seen: i64) -> Observed {
        Observed {
            schema: SCHEMA,
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
        let o = record(
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
