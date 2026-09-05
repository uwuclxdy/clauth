//! `clauth jobs` — the operator's enumeration of the delegate job store.
//!
//! Reads `~/.clauth/jobs/` through `mcp::jobs::list`, the same parser the MCP
//! surface and the TUI's delegates pane read it through, and classifies each row
//! with the same `StoredJob::phase`. Two readers of one store is already a drift
//! risk this store carries; a third PARSER would be the drift itself, so there
//! is none here.
//!
//! Read-only, and deliberately so past sharing a parser: `jobs::list` destroys
//! nothing, and this command adds no sweep of its own. Stopping a delegate is
//! `monitor({job_ids, cancel: true})`'s job — one stop path, which is also why
//! the TUI pane binds no key.
//!
//! Why a separate command rather than a column on `clauth list`: a job is a
//! transient run, not an account, and its rows come and go inside one 5h window.

use anyhow::Result;

use crate::format::{humanize_span, truncate};
use crate::mcp::jobs::{self, JobPhase, RunningLiveness, StoredJob};
use crate::usage::now_ms;
use crate::{out, outln};

/// Longest tail rendered under a table row. A tail is already bounded by the
/// writer; this second bound is about the terminal, so a run that emitted one
/// very long line cannot push the table off the screen.
const TAIL_W: usize = 100;

/// `clauth jobs [--json]` — print the delegate job store.
///
/// An empty store is a success, not a failure: no delegate has run recently is
/// the normal state, and exiting non-zero for it would break every script that
/// polls this. `--json` emits `[]` there rather than nothing, so a consumer can
/// pipe it into `jq` unconditionally.
pub(crate) fn run(json: bool) -> Result<()> {
    // `jobs::list` swallows a `read_dir` failure into an empty Vec, which is
    // right for a reader that has other work and wrong for THIS one: an
    // unreadable store would print "no delegate jobs", an affirmative claim
    // about a directory nothing managed to open. A MISSING dir is not that —
    // it is the normal state of a box that has never run a delegate.
    let dir = jobs::jobs_dir()?;
    match std::fs::read_dir(&dir) {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => anyhow::bail!("cannot read the job store at {}: {e}", dir.display()),
    }
    let rows = rows(now_ms());
    if json {
        outln!("{}", rows_json(&rows));
    } else {
        out!("{}", render_table(&rows));
    }
    Ok(())
}

/// One row of the listing, derived once and rendered by either surface.
///
/// Every liveness figure comes from [`jobs::running_liveness`] — the same
/// derivation `monitor`'s running check renders for the calling model — so this
/// table and that reply cannot disagree about one record. Pure of the clock, so
/// a test drives it at a fixed `now`.
#[derive(Debug, Clone)]
pub(crate) struct JobRow {
    pub(crate) job_id: String,
    pub(crate) profile: String,
    pub(crate) phase: JobPhase,
    /// Seconds since the stamp the store keeps this record BY — its freshest
    /// sign of life while running, its finish once done, its last sign of life
    /// once orphaned. One question ("how long since this record last mattered"),
    /// which is exactly what `retention_anchor` answers, so a row is dated by
    /// the same field that decides how long it survives.
    pub(crate) age_secs: u64,
    /// The run's liveness at `now`, or `None` on a finished or orphaned record,
    /// where every countdown it would carry has already stopped meaning
    /// anything.
    pub(crate) live: Option<RunningLiveness>,
    /// The run's own session id — the handle `delegate({resume})` takes. Read
    /// here because a record outlives the server that wrote it, so after a
    /// crash `--json` is the only thing naming the transcript the spend went
    /// into; the table prints no session column. The `None` cases are
    /// [`jobs::JobRecord::session_id`]'s; on this surface the key is always
    /// present, `null` where the record carries none.
    pub(crate) session_id: Option<String>,
    /// Whether the run launched isolated. It rides beside `session_id` because
    /// it is what decides whether that handle is one at all: an isolated run's
    /// transcript lived in a throwaway tree, so a crash left nothing for
    /// `delegate({resume})` to reach. A script reading the id alone cannot tell
    /// the two apart.
    pub(crate) isolated: bool,
    /// The delegate's own last words, already bounded by the writer. Empty on a
    /// finished record: its envelope carries the whole result, so a tail beside
    /// it says nothing new.
    pub(crate) tail: String,
}

/// Every stored job as a row, live ones first and each band newest-mattering
/// first — `jobs::list_banded`'s order, which is also the order `monitor`'s
/// listing uses, so the two surfaces cannot present one store differently.
///
/// Banded rather than raw for the reason `list_banded` documents: the store's
/// retention order dates a finished record by its FINISH, so a burst of
/// completions pushes a still-running delegate below them.
pub(crate) fn rows(now: u64) -> Vec<JobRow> {
    jobs::list_banded(now)
        .iter()
        .map(|job| row(job, now))
        .collect()
}

fn row(job: &StoredJob, now: u64) -> JobRow {
    let phase = job.phase();
    let record = &job.record;
    JobRow {
        job_id: record.job_id.clone(),
        profile: record.profile.clone(),
        phase,
        age_secs: job.age_secs(now),
        live: phase.is_live().then(|| jobs::running_liveness(record, now)),
        session_id: record.session_id.clone(),
        isolated: record.isolated,
        tail: match phase {
            JobPhase::Done => String::new(),
            _ => record.tail.clone(),
        },
    }
}

/// The table `clauth jobs` prints.
///
/// Each time column asks ONE question and renders `-` where the record has no
/// answer, rather than one column meaning "elapsed" on a live row and "finished
/// ago" on a dead one. A tail rides its own indented continuation line, the
/// shape `running_status_prose` already uses for the same text, so no column can
/// wrap.
fn render_table(rows: &[JobRow]) -> String {
    if rows.is_empty() {
        return "no delegate jobs. `delegate({background: true})` from Claude Code starts one.\n"
            .to_string();
    }
    let w_state = col_width("STATE", rows.iter().map(|r| r.phase.label()));
    let w_id = col_width("JOB ID", rows.iter().map(|r| r.job_id.as_str()));
    let w_profile = col_width("PROFILE", rows.iter().map(|r| r.profile.as_str()));
    let ages: Vec<String> = rows.iter().map(|r| age_cell(r.age_secs)).collect();
    let elapsed: Vec<String> = rows.iter().map(elapsed_cell).collect();
    let outputs: Vec<String> = rows.iter().map(last_output_cell).collect();
    let w_age = col_width("AGE", ages.iter().map(String::as_str));
    let w_elapsed = col_width("ELAPSED", elapsed.iter().map(String::as_str));
    let w_output = col_width("LAST OUTPUT", outputs.iter().map(String::as_str));

    // KILL is the last column, so it is never padded and needs no width.
    let mut out = format!(
        "{:<w_state$}  {:<w_id$}  {:<w_profile$}  {:>w_age$}  {:>w_elapsed$}  {:>w_output$}  KILL IN\n",
        "STATE", "JOB ID", "PROFILE", "AGE", "ELAPSED", "LAST OUTPUT",
    );
    for ((r, age), (elapsed, output)) in rows
        .iter()
        .zip(&ages)
        .zip(elapsed.iter().zip(outputs.iter()))
    {
        out.push_str(&format!(
            "{:<w_state$}  {:<w_id$}  {:<w_profile$}  {:>w_age$}  {:>w_elapsed$}  {:>w_output$}  {}\n",
            r.phase.label(),
            r.job_id,
            r.profile,
            age,
            elapsed,
            output,
            kill_cell(r),
        ));
        if !r.tail.is_empty() {
            out.push_str(&format!("    \"{}\"\n", tail_cell(&r.tail)));
        }
    }
    out
}

/// A delegate's own last words, with the C0/C1 control characters and the
/// explicit bidi formatting characters removed.
///
/// Named classes rather than a claim to be "safe": this text is ANOTHER
/// account's model output arriving verbatim on the operator's terminal, and the
/// next reader has to be able to tell what is and is not handled here.
///
/// **Controls** (`char::is_control`, so C0 + DEL + C1): the table is the first
/// clauth surface that prints a tail raw — the MCP replies are JSON (serde
/// escapes C0 as `\uXXXX`) and the TUI goes through ratatui. `tail_line`
/// collapses whitespace runs, which removes newlines and tabs and leaves
/// `\x1b`, `\x07` and `\x00` alone, since none of those is
/// `char::is_whitespace`. So an escape sequence would reach the terminal intact,
/// and truncation could cut one in half and leave a dangling `\x1b[`.
///
/// **Bidi formatting** ([`reorders_display`]): `is_control` is FALSE for these,
/// so the first bullet does not reach them, and they are the Trojan-Source
/// class — one `U+202E` reverses the display order of the rest of the line, so a
/// delegate's words can be made to read as something they are not, on the one
/// surface an operator opened to read them.
///
/// What is deliberately NOT stripped, so the boundary is a decision rather than
/// an oversight: the zero-width marks (`U+200B` ZWSP, `U+200C` ZWNJ, `U+200D`
/// ZWJ). None of them can reorder anything, and stripping ZWJ would shatter
/// every multi-person and profession emoji — `U+1F468 ZWJ U+1F469 ZWJ U+1F467`
/// is a family emoji, and a delegate emitting one is routine. They also carry
/// orthographic meaning inside real words in Persian and Hindi, which is the
/// rarer of the two cases.
///
/// Stripped BEFORE truncating, so the width bound counts what is actually
/// shown. `is_control` is the same predicate `herdr.rs` and `claude.rs` already
/// guard their own boundaries with.
///
/// **`--json` is deliberately NOT filtered, and it does not escape this class
/// either.** `serde_json` escapes below `0x20`, so a C0 byte comes back as
/// `\u0007`; every bidi character above it is emitted as its literal UTF-8
/// bytes. That is correct for a machine format — a `jq` consumer should get what
/// the delegate actually wrote — but it means an operator eyeballing
/// `clauth jobs --json` in a terminal sees the reordering this function strips
/// from the table. Measured, not assumed: an earlier version of this sentence
/// said `--json` "escapes rather than drops", which is true of C0 and false of
/// everything here.
///
/// Only the TAIL is filtered. The other columns are clauth-minted (`job_id`,
/// the state word, the figures) or roster-resolved (`profile`, which
/// `preflight_target` takes as an already-resolved account), so none of them
/// carries model-supplied text today. **That is a provenance argument, not a
/// structural one**: the day a column starts carrying a caller-supplied label,
/// it needs this filter too.
fn tail_cell(tail: &str) -> String {
    let printable: String = tail
        .chars()
        .filter(|c| !c.is_control() && !reorders_display(*c))
        .collect();
    truncate(&printable, TAIL_W)
}

/// Unicode's `Bidi_Control` property, all twelve codepoints: the embeddings and
/// overrides (`U+202A..=U+202E`), the isolates (`U+2066..=U+2069`), and the
/// three directional marks — `U+200E` LRM, `U+200F` RLM, and `U+061C` ALM, the
/// Arabic-script twin of RLM, which an earlier version of this set missed while
/// calling itself closed.
///
/// Spelled out rather than tested by general category, because the crate carries
/// no Unicode tables and this task takes no new dependency. The cost of spelling
/// it out is that it is only closed against the Unicode version it was written
/// from; `Bidi_Control` has been stable for many releases, and a new member
/// would surface as a codepoint reaching the terminal rather than as a wrong
/// figure.
///
/// Legitimate right-to-left text needs none of them: the implicit bidi algorithm
/// renders a plain Arabic or Hebrew tail correctly on its own, measured on
/// both.
fn reorders_display(c: char) -> bool {
    matches!(
        c,
        '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}' | '\u{200e}' | '\u{200f}' | '\u{061c}'
    )
}

/// Width of one column: its header, or the widest cell under it.
fn col_width<'a>(header: &str, cells: impl Iterator<Item = &'a str>) -> usize {
    cells
        .map(|c| c.chars().count())
        .max()
        .unwrap_or(0)
        .max(header.chars().count())
}

/// How long since this record last mattered. Always answerable — every record
/// has a retention anchor — so this column never renders `-`.
fn age_cell(secs: u64) -> String {
    duration_cell(secs)
}

/// How long the run has been going, `-` on a record whose run is over. A
/// finished record still carries `started_at`, so an elapsed figure IS
/// derivable there — and it would count the hours a done envelope then sat in
/// the store waiting to be collected, which is not what the word means.
fn elapsed_cell(row: &JobRow) -> String {
    row.live
        .map_or_else(|| "-".to_string(), |live| duration_cell(live.elapsed_secs))
}

/// How long since the run's last line of output.
///
/// Three spellings over FOUR inputs, and the dash is the overloaded one: an age
/// where a stamp exists, `never` where a run is going and has said nothing yet,
/// and `-` for both a record with no run AND one written before the liveness
/// fields existed, which has a run going and no way to answer. Collapsing
/// `never` into the dash as well would report a delegate that has produced
/// nothing exactly like a finished one, which is the distinction worth a
/// spelling.
///
/// The two dash causes are NOT told apart here, deliberately: a table column has
/// one cell, and `monitor`'s running check is the surface that separates them
/// (`liveness not recorded (started under an older clauth)`). `--json` separates
/// them structurally instead — `elapsed_secs` is non-null exactly when a run is
/// going.
fn last_output_cell(row: &JobRow) -> String {
    let Some(live) = row.live else {
        return "-".to_string();
    };
    match live.last_output_secs_ago {
        Some(secs) => duration_cell(secs),
        // A record from before the liveness fields existed knows nothing about
        // its own output; one that has them and no stamp has genuinely said
        // nothing.
        None if !live.recorded => "-".to_string(),
        None => "never".to_string(),
    }
}

/// Which deadlines are still counting down, both of them where both exist.
///
/// Both figures rather than whichever fires first: they come straight off
/// [`RunningLiveness`] with no arithmetic of their own, and picking a winner
/// would be a second derivation of a question the TUI pane already answers its
/// own way for a row that has one cell to spend.
///
/// A dash here does NOT mean "clauth knows there is no deadline". A run
/// legitimately has neither — a streaming run has no wall clock, a
/// pinned-`--output-format` one no idle guard — and a record written before the
/// liveness fields carries neither because that server recorded nothing. The
/// first is clauth knowing there is none, the second is clauth not knowing, and
/// this column renders both as `-`. That split `monitor` keeps (`no wall clock` against
/// `liveness not recorded`); a table cell cannot, so it claims neither.
fn kill_cell(row: &JobRow) -> String {
    let Some(live) = row.live else {
        return "-".to_string();
    };
    let mut parts = Vec::new();
    if let Some(secs) = live.idle_kill_in_secs {
        parts.push(format!("idle {}", duration_cell(secs)));
    }
    if let Some(secs) = live.wall_kill_in_secs {
        parts.push(format!("wall {}", duration_cell(secs)));
    }
    if parts.is_empty() {
        return "-".to_string();
    }
    parts.join(", ")
}

/// A duration as a cell. The zero-boundary rule is [`humanize_span`]'s, shared
/// with `monitor`'s listing rather than guarded a second time here.
fn duration_cell(secs: u64) -> String {
    humanize_span(secs)
}

/// The `--json` array: newest-mattering first, one object per stored job.
///
/// The field set is FIXED and every key is always present, a figure the record
/// does not have rendering `null` — the shape `clauth sessions --json` already
/// established, and the one a `jq` filter can be written against without
/// probing for keys. That is deliberately the opposite of the MCP surface's
/// absent-means-structurally-none rule: a model reads prose and pays for every
/// key, a script reads this and pays for every missing one.
fn rows_json(rows: &[JobRow]) -> String {
    let array: Vec<serde_json::Value> = rows.iter().map(row_json).collect();
    serde_json::to_string_pretty(&array).unwrap_or_else(|_| "[]".to_string())
}

fn row_json(row: &JobRow) -> serde_json::Value {
    serde_json::json!({
        "job_id": row.job_id,
        "profile": row.profile,
        "state": row.phase.label(),
        "collectable": row.phase.is_collectable(),
        "session_id": row.session_id,
        "isolated": row.isolated,
        "age_secs": row.age_secs,
        "elapsed_secs": row.live.map(|l| l.elapsed_secs),
        "last_output_secs_ago": row.live.and_then(|l| l.last_output_secs_ago),
        "idle_kill_in_secs": row.live.and_then(|l| l.idle_kill_in_secs),
        "wall_kill_in_secs": row.live.and_then(|l| l.wall_kill_in_secs),
        "tail": row.tail,
    })
}

#[cfg(test)]
#[path = "../tests/inline/jobs_cli.rs"]
mod tests;
