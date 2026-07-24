//! `clauth doctor` PURE core — check types + classification, no IO/syscalls.
//! Everything here is deterministic and unit-tested; the impure probes that shell
//! out to launchctl/codesign/security live in the parent `doctor` module.

use std::path::Path;
use std::time::{Duration, SystemTime};

/// Outcome of one check. `Warn` prints but does not fail the run; `Fail` sets a
/// non-zero exit so `clauth doctor` is usable in a monitoring one-liner.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Status {
    Pass,
    Warn,
    Fail,
}

impl Status {
    fn tag(self) -> &'static str {
        match self {
            Status::Pass => "PASS",
            Status::Warn => "WARN",
            Status::Fail => "FAIL",
        }
    }
}

/// One diagnostic line: a name, an outcome, human detail, and (when not passing)
/// the exact command that fixes it.
pub(super) struct Check {
    pub(super) name: String,
    pub(super) status: Status,
    detail: String,
    fix: Option<String>,
}

impl Check {
    fn new(name: &str, status: Status, detail: impl Into<String>, fix: Option<&str>) -> Self {
        Check {
            name: name.to_string(),
            status,
            detail: detail.into(),
            fix: fix.map(str::to_string),
        }
    }
    pub(super) fn pass(name: &str, detail: impl Into<String>) -> Self {
        Check::new(name, Status::Pass, detail, None)
    }
    pub(super) fn warn(name: &str, detail: impl Into<String>, fix: &str) -> Self {
        Check::new(name, Status::Warn, detail, Some(fix))
    }
    pub(super) fn fail(name: &str, detail: impl Into<String>, fix: &str) -> Self {
        Check::new(name, Status::Fail, detail, Some(fix))
    }

    /// One or two lines: the status line, plus a `fix:` hint when not passing.
    pub(super) fn render(&self) -> String {
        let mut s = format!("[{}] {} — {}", self.status.tag(), self.name, self.detail);
        if self.status != Status::Pass
            && let Some(fix) = &self.fix
        {
            s.push_str(&format!("\n         fix: {fix}"));
        }
        s
    }
}

/// The run exits non-zero iff any check FAILed (WARN does not fail).
pub(super) fn exit_code(checks: &[Check]) -> i32 {
    i32::from(checks.iter().any(|c| c.status == Status::Fail))
}

/// `status.json` freshness from its age. The daemon rewrites the file every 1s
/// tick UNCONDITIONALLY (independent of the usage `refresh_interval_ms`, which
/// only governs Anthropic re-fetch cadence), and a wedged tick is force-restarted
/// by the watchdog at ~60s — so a healthy file is never more than a few seconds
/// stale. `<=10s` Pass; `<=75s` Warn (just past the watchdog window); else Fail
/// (the daemon is dead or not writing).
pub(super) fn freshness(age: Duration) -> Status {
    if age <= Duration::from_secs(10) {
        Status::Pass
    } else if age <= Duration::from_secs(75) {
        Status::Warn
    } else {
        Status::Fail
    }
}

/// Version/schema skew between this binary and the daemon's `status.json`. A
/// schema mismatch is a FAIL (the read format diverged); a version mismatch with
/// matching schema is a WARN (a daemon restart adopts the new binary).
pub(super) fn skew(
    bin_ver: &str,
    bin_schema: u64,
    status_ver: Option<&str>,
    status_schema: Option<u64>,
) -> (Status, String) {
    match (status_ver, status_schema) {
        (Some(sv), Some(ss)) => {
            if ss != bin_schema {
                (
                    Status::Fail,
                    format!("status.json schema {ss} ≠ binary schema {bin_schema}"),
                )
            } else if sv != bin_ver {
                (
                    Status::Warn,
                    format!("daemon {sv} ≠ CLI {bin_ver} (schema {ss} matches)"),
                )
            } else {
                (Status::Pass, format!("{bin_ver}, schema {bin_schema}"))
            }
        }
        _ => (
            Status::Warn,
            "no clauth_version/schema in status.json (old daemon or none written)".to_string(),
        ),
    }
}

/// Read + parse `status.json`, returning `(version, schema, generated_at_ms)`.
pub(super) fn read_status(
    status_path: &Path,
) -> Option<(Option<String>, Option<u64>, Option<u64>)> {
    let body = std::fs::read_to_string(status_path).ok()?;
    let v: serde_json::Value = serde_json::from_str(&body).ok()?;
    let ver = v
        .get("clauth_version")
        .and_then(|x| x.as_str())
        .map(str::to_string);
    let schema = v.get("schema").and_then(serde_json::Value::as_u64);
    let gen_at = v
        .get("generated_at")
        .and_then(|x| x.as_str())
        .and_then(iso_to_ms);
    Some((ver, schema, gen_at))
}

/// Minimal ISO-8601 → epoch-ms for the daemon's `generated_at`, which is written
/// as `YYYY-MM-DDTHH:MM:SS+00:00` (always UTC; see `usage::fetch::epoch_secs_to_iso`).
/// Only the first 19 chars (the calendar/clock fields) are read; the always-UTC
/// offset is ignored, so a trailing `+00:00`, `Z`, or `.fff` all parse the same.
/// Best-effort: an unparseable stamp yields `None` and the caller falls back to
/// the file mtime.
fn iso_to_ms(s: &str) -> Option<u64> {
    if s.len() < 19 {
        return None;
    }
    let num = |a: usize, b: usize| s.get(a..b)?.parse::<i64>().ok();
    let (y, mo, d) = (num(0, 4)?, num(5, 7)?, num(8, 10)?);
    let (h, mi, se) = (num(11, 13)?, num(14, 16)?, num(17, 19)?);
    // Days since epoch via a civil-date algorithm (Howard Hinnant's days_from_civil).
    let y = if mo <= 2 { y - 1 } else { y };
    let era = (if y >= 0 { y } else { y - 399 }) / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if mo > 2 { mo - 3 } else { mo + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    Some(((days * 86400 + h * 3600 + mi * 60 + se) * 1000) as u64)
}

pub(super) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
#[path = "../../tests/inline/doctor.rs"]
mod tests;
