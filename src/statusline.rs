//! `clauth statusline [-- <command>…]`: record the usage Claude Code hands its
//! status line, then hand the same input to the status line you already use.
//!
//! Claude Code reads the `anthropic-ratelimit-unified-*` headers off every
//! reply and passes their 5h and 7d figures to the status line command as
//! `rate_limits` (`used_percentage` 0-100, `resets_at` epoch seconds). No hook
//! event carries them, so the status line is the one place a session hands
//! them out. Set it as the status line, in front of your own:
//!
//! ```json
//! "statusLine": { "type": "command", "command": "clauth statusline -- ccstatusline --flag" }
//! ```
//!
//! Each render records the reading in `~/.clauth/observed-usage/<profile>.json`
//! ([`crate::usage::observed`]), which the scheduler folds into the store while
//! `/api/oauth/usage` is rate limited. With no `--` command it prints nothing.
//!
//! The profile is the session's: its `clauth start` runtime, else the owner
//! clauth's hook stamped on its session id, else the active profile, then
//! confirmed against each profile's stored 7d reset (accounts reset at
//! different hours of the week). A reading that cannot be attributed is
//! dropped. A session on a config dir clauth does not manage is never
//! recorded.
//!
//! The input carries the figures of the session's LAST reply but not when that
//! reply came, and Claude Code repaints the status line for as long as the
//! session is open: an idle session shows day-old figures on every repaint. So
//! each reading is dated by the session's last assistant entry in its
//! transcript (`transcript_path`), and a reading that cannot be dated is
//! dropped. That date is a lower bound (a subagent may have replied since), so
//! a reading is never taken for newer than it is.
//!
//! Recording is best effort and silent: the status line renders either way,
//! and the wrapped command's output and exit code pass through unchanged.

use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::usage::observed::{self, Reading, Source};

/// A status line input is a few kilobytes; read no more than this.
const INPUT_CAP: u64 = 1 << 20;

/// How much of the transcript's end each look for the last reply reads. An
/// assistant entry can run to hundreds of kilobytes, so a larger read follows
/// when the first finds none.
const TRANSCRIPT_TAILS: [u64; 2] = [256 * 1024, 4 * 1024 * 1024];

#[derive(Debug, Default, Deserialize)]
struct Period {
    used_percentage: Option<f64>,
    resets_at: Option<f64>,
}

impl Period {
    fn reading(&self) -> Option<Reading> {
        Reading::new(self.used_percentage?, self.resets_at?)
    }
}

#[derive(Debug, Default, Deserialize)]
struct RateLimits {
    five_hour: Option<Period>,
    seven_day: Option<Period>,
}

#[derive(Debug, Default, Deserialize)]
struct StatusInput {
    session_id: Option<String>,
    transcript_path: Option<String>,
    rate_limits: Option<RateLimits>,
}

/// One transcript line, read only as far as its kind and time.
#[derive(Debug, Deserialize)]
struct Entry {
    #[serde(rename = "type")]
    kind: Option<String>,
    timestamp: Option<String>,
}

/// When the session's last assistant entry was written, in epoch seconds:
/// the latest reply whose rate-limit headers the status line can be showing.
/// `None` when the transcript is unreadable or holds no reply.
fn last_reply_secs(path: &Path) -> Option<i64> {
    let mut file = std::fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    for cap in TRANSCRIPT_TAILS {
        let start = len.saturating_sub(cap);
        file.seek(SeekFrom::Start(start)).ok()?;
        let mut tail = Vec::new();
        Read::by_ref(&mut file)
            .take(cap)
            .read_to_end(&mut tail)
            .ok()?;
        if let Some(secs) = last_reply_in(&tail, start > 0) {
            return Some(secs);
        }
        if start == 0 {
            break;
        }
    }
    None
}

/// The last assistant entry's time in a tail of JSONL. `partial` drops the
/// first line, which a read that began mid-file cuts.
fn last_reply_in(tail: &[u8], partial: bool) -> Option<i64> {
    let body = if partial {
        &tail[tail.iter().position(|b| *b == b'\n')? + 1..]
    } else {
        tail
    };
    body.rsplit(|b| *b == b'\n')
        .filter(|line| line.windows(11).any(|w| w == b"\"assistant\""))
        .filter_map(|line| serde_json::from_slice::<Entry>(line).ok())
        .find(|entry| entry.kind.as_deref() == Some("assistant"))
        .and_then(|entry| crate::usage::iso_to_epoch_secs(entry.timestamp.as_deref()?))
}

pub(crate) fn run(command: &[String]) -> Result<()> {
    let mut input = Vec::new();
    // An unreadable stdin still runs the wrapped command, on what was read.
    let _ = std::io::stdin().take(INPUT_CAP).read_to_end(&mut input);
    record(&input);
    let Some((program, args)) = command.split_first() else {
        return Ok(());
    };
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .spawn()
        .with_context(|| format!("could not run the status line command {program:?}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        // A command that exits without reading its input is not an error.
        let _ = stdin.write_all(&input);
    }
    let status = child.wait()?;
    if !status.success() {
        std::process::exit(status.code().unwrap_or(1));
    }
    Ok(())
}

/// Record the input's `rate_limits` for the session's profile. Silent.
fn record(input: &[u8]) {
    let Ok(parsed) = serde_json::from_slice::<StatusInput>(input) else {
        return;
    };
    let Some(limits) = &parsed.rate_limits else {
        return;
    };
    let five_hour = limits.five_hour.as_ref().and_then(Period::reading);
    let seven_day = limits.seven_day.as_ref().and_then(Period::reading);
    if five_hour.is_none() && seven_day.is_none() {
        return;
    }
    let now = crate::usage::now_epoch_secs();
    // A clock that put the transcript ahead of this process reads as now.
    let Some(seen_at) = parsed
        .transcript_path
        .as_deref()
        .and_then(|p| last_reply_secs(Path::new(p)))
        .map(|secs| secs.min(now))
    else {
        return;
    };
    let config_dir = std::env::var_os("CLAUDE_CONFIG_DIR").filter(|d| !d.is_empty());
    let claude_dir = crate::profile::claude_dir().ok();
    let Some(candidate) = candidate(
        config_dir.as_deref().map(Path::new),
        claude_dir.as_deref(),
        parsed.session_id.as_deref(),
    ) else {
        return;
    };
    let Some(profile) = observed::attribute(
        candidate,
        seven_day.map(|r| r.resets_at),
        &observed::weekly_resets(),
    ) else {
        return;
    };
    if let Some(dir) = observed::dir()
        && let Err(e) = observed::record(&dir, &profile, five_hour, seven_day, seen_at, now)
    {
        crate::logline::to_logfile(format_args!(
            "clauth statusline: could not record observed usage for {profile}: {e}"
        ));
    }
}

/// The session's candidate profile. The outer `None` means the session runs on
/// a config dir clauth does not manage and must not be recorded at all; the
/// inner one means clauth has no candidate and only the weekly reset can name
/// the profile.
fn candidate(
    config_dir: Option<&Path>,
    claude_dir: Option<&Path>,
    session_id: Option<&str>,
) -> Option<Option<(String, Source)>> {
    if let Some(dir) = config_dir {
        if let Some(name) = crate::which::session_profile_from_config_dir(dir) {
            return Some(Some((name, Source::ConfigDir)));
        }
        if claude_dir != Some(dir) {
            return None;
        }
    }
    if let Some(owner) = session_id.and_then(crate::sessions::owner_of) {
        return Some(Some((owner, Source::SessionOwner)));
    }
    Some(
        crate::profile::active_profile_name()
            .map(|name| (name.as_str().to_owned(), Source::Global)),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_read_the_status_line_input_shape() {
        let input = br#"{"session_id":"s","model":{"id":"m"},"rate_limits":{
            "five_hour":{"used_percentage":12.5,"resets_at":1794010000},
            "seven_day":{"used_percentage":43,"resets_at":1794301200}}}"#;
        let parsed: StatusInput = serde_json::from_slice(input).expect("parses");
        let limits = parsed.rate_limits.expect("rate limits");
        assert_eq!(
            limits.five_hour.as_ref().and_then(Period::reading),
            Reading::new(12.5, 1_794_010_000.0)
        );
        assert_eq!(
            limits.seven_day.as_ref().and_then(Period::reading),
            Reading::new(43.0, 1_794_301_200.0)
        );
    }

    #[test]
    fn should_read_an_input_without_rate_limits() {
        let parsed: StatusInput = serde_json::from_slice(br#"{"session_id":"s"}"#).expect("parses");
        assert!(parsed.rate_limits.is_none());
    }

    #[test]
    fn should_date_a_reading_by_the_last_assistant_entry() {
        let jsonl = concat!(
            r#"{"type":"user","timestamp":"2026-11-07T11:00:00.000Z"}"#,
            "\n",
            r#"{"type":"assistant","timestamp":"2026-11-07T11:00:05.000Z","message":{"content":"the word \"assistant\" again"}}"#,
            "\n",
            r#"{"type":"system","timestamp":"2026-11-07T11:30:00.000Z"}"#,
            "\n",
            r#"{"type":"user","timestamp":"2026-11-07T11:31:00.000Z","message":"type assistant"}"#,
            "\n",
        );
        assert_eq!(
            last_reply_in(jsonl.as_bytes(), false),
            crate::usage::iso_to_epoch_secs("2026-11-07T11:00:05.000Z")
        );
    }

    #[test]
    fn should_skip_the_cut_first_line_of_a_partial_tail() {
        // the cut line still looks like an assistant entry's end
        let tail = b"ssistant\",\"timestamp\":\"2026-11-07T12:00:00Z\"}\n{\"type\":\"user\"}\n";
        assert_eq!(last_reply_in(tail, true), None);
        let whole = br#"{"type":"assistant","timestamp":"2026-11-07T12:00:00Z"}"#;
        assert!(last_reply_in(whole, false).is_some());
    }

    #[test]
    fn should_find_a_reply_in_a_transcript_file_past_the_first_tail() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("t.jsonl");
        let big = "x".repeat(300 * 1024);
        let reply = r#"{"type":"assistant","timestamp":"2026-11-07T10:00:00Z"}"#;
        let filler = format!(r#"{{"type":"user","message":"{big}"}}"#);
        std::fs::write(&path, format!("{reply}\n{filler}\n")).expect("writes");
        assert_eq!(
            last_reply_secs(&path),
            crate::usage::iso_to_epoch_secs("2026-11-07T10:00:00Z")
        );
        assert_eq!(last_reply_secs(&dir.path().join("missing.jsonl")), None);
    }

    #[test]
    fn should_name_a_clauth_runtime_by_its_profile() {
        let dir = Path::new("/home/u/.clauth/profiles/work/runtime");
        assert_eq!(
            candidate(Some(dir), Some(Path::new("/home/u/.claude")), None),
            Some(Some(("work".to_owned(), Source::ConfigDir)))
        );
    }

    #[test]
    fn should_skip_a_config_dir_clauth_does_not_manage() {
        let foreign = Path::new("/home/u/elsewhere");
        assert_eq!(
            candidate(Some(foreign), Some(Path::new("/home/u/.claude")), None),
            None
        );
    }
}
