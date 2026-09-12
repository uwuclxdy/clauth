//! macOS Keychain access for the `Claude Code-credentials` login item.
//!
//! Claude Code on macOS stores its OAuth login in the login Keychain (a generic
//! password: service `Claude Code-credentials`, account = the OS login name), NOT
//! in `~/.claude/.credentials.json`. So clauth's symlink swap is cosmetic on
//! macOS unless the switched account is also written here — Claude Code keeps
//! reading the Keychain.
//!
//! **Every write is a READ-MODIFY-WRITE.** Claude Code keeps ONE item holding
//! ONE JSON object: the login `claudeAiOauth` beside `mcpOAuth` (the
//! per-MCP-server logins, which belong to no Claude account) and four
//! account-scoped keys. `add-generic-password -U`
//! replaces that whole object, so a write serializing the login alone signed the
//! operator out of every MCP server on every switch. Which siblings survive is
//! [`Keep`]'s decision, and it mirrors the two rules the file path already has:
//! a switch imports from another account's store and takes an allowlist
//! (`claude::carry_live_extra_over`), a rotation rewrites this account's own item
//! and keeps everything (`profile::preserve_extra_blocks`).
//!
//! **The read costs ONE access prompt, ever.** macOS gates a read on the ITEM's
//! own ACL and binds the grant to the CALLING binary, so an "Always Allow"
//! against Apple's stable, code-signed `/usr/bin/security` sticks permanently,
//! where a grant against clauth's own `cargo build` binary would die at the next
//! rebuild under its changed ad-hoc signature. That is why this shells out
//! instead of linking `security-framework` (CCSwitcher's approach), and it is now
//! load-bearing for reads as well as writes. Measured at a console 2026-08-12: an
//! item ACL'd to a different binary (`-T /usr/bin/false`, the shape CC's own item
//! has) raised a dialog on the first `find-generic-password` and answered
//! silently on the second. Bound on that: the probe ran against a throwaway item,
//! never `Claude Code-credentials` itself, so persistence against CC's own item
//! follows from the grant being per-item rather than from an observation.
//!
//! **A failed read never fails the write.** A locked keychain, an ACL refusal on
//! a headless ssh session (`errSecInteractionNotAllowed`), or a dialog nobody
//! answers inside [`security_deadline`] degrades to writing the incoming blob
//! alone, and names the loss on the event line. Completing a switch is
//! load-bearing where preserving MCP logins is a convenience, the same posture
//! `claude.rs::carry_live_extra_best_effort` takes on the file path, and refusing
//! instead would strand every headless macOS switch on the outgoing account.
//! `claude.rs` wires all of it behind `#[cfg(target_os = "macos")]` + [`enabled`].
//!
//! **A successful write is only believed once read back.** `add-generic-password`
//! exiting 0 is the tool's word, not proof the item holds the bytes sent: the
//! `-i` line truncation (#66) exited 0 while the item held cut-off JSON, and
//! nothing surfaced until a later read failed to parse it. So every write reads
//! the item back RAW and byte-compares against the JSON it sent
//! ([`verify_outcome`], the ruling on what that proved in
//! [`write_disposition`]). Byte-equal is the only silent outcome. Different bytes,
//! or an item that reads back ABSENT after a reported success, is a write KNOWN
//! corrupt — the switch fails rather than complete on a lie, the read-back bytes
//! are quarantined first, and the profile store still holds the intended login,
//! so retrying the switch re-runs the write. A read-back that cannot run at all
//! (the hold's subprocess budget already spent, [`security_deadline`] expiry, a
//! read refusal over ssh) leaves the write LANDED BUT UNVERIFIED and the switch
//! COMPLETE: the same refusal-of-refusal a failed read takes, because an
//! unverifiable write must not fail a completed switch.
//!
//! **Unparseable bytes are quarantined, never destroyed.** A read that answers
//! with bytes which are not the JSON object CC expects is usually a TRUNCATED
//! one — the `claudeAiOauth` head survives with its tail cut (#66, #76) — and
//! the overwrite/delete that used to treat such a read as "unreadable" is what
//! turned one corruption into a lost login plus every MCP login in the item.
//! The two sites that act on a failed read — the merge that overwrites the item
//! and the sign-out that deletes it — park the raw bytes under
//! `~/.clauth/keychain-quarantine/` first ([`quarantine_item_bytes`], the
//! `atomic_write_600` posture the parked `mcp-logins.json` already holds) and
//! name the file on the event line. The write or delete still lands: quarantine
//! preserves the evidence, it does not refuse the switch.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde_json::Value;

use crate::logline::logline;
use crate::profile::ClaudeCredentials;

/// Apple's Keychain CLI. Absolute path so a hostile `PATH` can't shim it.
const SECURITY_BIN: &str = "/usr/bin/security";

/// Wall-clock ceiling for a single `security` invocation before it is killed
/// (TECH-3). A stuck keychain (an unanswered "Always Allow" ACL prompt, a locked
/// keychain, a hung home volume) must NOT pin the state flock forever: the daemon
/// runs the switch, hence this subprocess, inside `with_state_lock` on its
/// single-threaded run loop, so an unbounded child would wedge auto-switch, the
/// exact failure the daemon exists to prevent.
///
/// **10 s is the PER-CALL ceiling: the window one `security` invocation gets
/// before it is killed** — on the read leg, the window an operator has to
/// answer the one-time ACL dialog before the call gives up on them. A mirror
/// is now THREE invocations (a read, then a write or delete, then the write's
/// read-back verification, [`verify_write`]), where the deadlines above this
/// were sized against two: `lock.rs`'s 25 s state-lock timeout and the daemon's
/// 30 s `WATCHDOG_DEADLINE` sit against a 20 s mirror, and the daemon's
/// comment says to bound this shell-out rather than loosen them. Inside a hold
/// the clamp below still caps the AGGREGATE at that 20 s however many calls
/// share it; outside one (the rotation mirror, which `oauth.rs` runs after its
/// lock closure ends) a mirror's worst case is now 30 s against
/// `runtime::KEYCHAIN_MIRROR_BUDGET`'s 20 — under-covered by exactly the
/// verification call, deliberately left un-retuned here and reported to the
/// caller instead. What keeps that gap safe is the verify leg's own failure
/// mode: an UNVERIFIABLE write completes rather than failing, so the
/// under-coverage costs a lock-waiter's margin, never a correct switch.
///
/// This is the PER-CALL ceiling only. What a waiting peer actually feels is the
/// whole flock hold, and a hold can run more than one mirror
/// (`adopt_first_login`'s relink, then the switch's own), so [`security_deadline`]
/// clamps this to `lock::SUBPROCESS_BUDGET`, the aggregate one hold may spend.
///
/// Measured on `mac-6` 2026-08-12: a real `add-generic-password -U` costs 22-29 ms
/// and a `find-generic-password -w` 18-19 ms, so the happy path keeps a ~210x
/// margin against either bound. A deadline only ever binds a stuck keychain, where
/// both legs burn it. What the 10 s costs there is an operator with 10 s rather
/// than 20 s to answer the one-time ACL dialog — the READ leg, which degrades to a
/// login-only write and re-prompts next switch. The WRITE leg does not degrade: it
/// fails the switch, so a locked keychain that prompts for a password rather than
/// refusing outright has half as long to be answered before that.
const SECURITY_TIMEOUT: Duration = Duration::from_secs(10);

// `runtime::KEYCHAIN_MIRROR_BUDGET` still equals the READ+WRITE pair — the two
// invocations a mirror was when it was derived. The read-back verification
// (`verify_write`) is a third, riding PAST that budget at its own
// [`SECURITY_TIMEOUT`]. Retuning the budget to cover it means moving this
// assert to `* 3` and `runtime::ROTATION_LOCK_TIMEOUT`'s floor term with it, in
// one change — deliberately not done here (the under-coverage is reported to
// the caller instead; see [`SECURITY_TIMEOUT`]'s doc for why it is safe to
// leave). It is spelled over there because this module is macOS-gated while
// that deadline is one number on every host; the check is a compile error
// rather than a test because the quantity exists only in this build, so this
// is the only build that can make it.
//
// Still deliberately not `lock::SUBPROCESS_BUDGET`, which it coincides with:
// that bounds one state-flock hold's shell-outs in aggregate, and
// `oauth::apply_rotated_tokens_locked` runs its mirror after the closure ends,
// where `security_deadline` clamps nothing.
const _: () = assert!(
    SECURITY_TIMEOUT.as_millis() * 2 == crate::runtime::KEYCHAIN_MIRROR_BUDGET.as_millis(),
    "runtime::KEYCHAIN_MIRROR_BUDGET must stay two SECURITY_TIMEOUTs wide (the read+write pair; \
     the verify call rides past it)"
);

/// The deadline for the next `security` invocation: [`SECURITY_TIMEOUT`], clamped
/// to whatever the state-lock hold this call sits inside has left to spend
/// (`lock::clamp_to_hold_budget`). Outside a hold — `oauth.rs` mirrors a rotation
/// after its lock closure ends — it is [`SECURITY_TIMEOUT`] unchanged, because no
/// peer is waiting on that call to finish.
fn security_deadline() -> Duration {
    crate::lock::clamp_to_hold_budget(SECURITY_TIMEOUT)
}

/// Run `cmd` with a wall-clock deadline, killing (and reaping) the child if it
/// outlives `timeout`. Returns the collected [`Output`] on a normal exit, or an
/// error on spawn failure / timeout. Extracted so the deadline is unit-testable
/// with a benign hanging command (`sleep`) — no real Keychain is touched.
///
/// `stdin_payload`, when given, is written to the child's stdin which is then
/// closed (EOF) — the transport for `security -i`'s command line, keeping the
/// secret out of argv. Its length is bounded by [`SECURITY_STDIN_LINE_MAX`] by
/// construction (`put_blob_at` routes anything longer through argv instead), so
/// the single write before the poll loop cannot block on the pipe buffer.
///
/// `security` produces only a few bytes of output on the paths this module had
/// measured — but the 512 KiB argv cap admits items whose `find-generic-password
/// -w` print is past the pipe buffer (~64 KiB), so BOTH output pipes are drained
/// concurrently with the poll loop by reader threads started before it: a child
/// blocked on `write(2)` never exits, and read-after-exit would kill it at the
/// deadline instead of returning the item (the merge read as a no-bytes failure,
/// its siblings destroyed with nothing quarantined; the verify read as a +10 s
/// stall into `Unverified`). The join after exit waits for EOF exactly as
/// `wait_with_output` did; on the timeout path the readers are left to see EOF
/// on their own once the killed child is reaped, so a dead child's pipes never
/// hold the loop.
fn run_with_deadline(
    mut cmd: Command,
    timeout: Duration,
    stdin_payload: Option<&str>,
) -> Result<Output> {
    // A hold whose budget is spent clamps to zero. Refuse BEFORE the spawn: the
    // payload is written below before `deadline` even exists, so the write path
    // would otherwise hand the credential JSON to a process created only to be
    // killed. Measured on `mac-6` 2026-08-12: pre-fix that cost a real spawn at
    // ~1.6 ms, and the refusal now returns in ~15 µs having created nothing,
    // proven by a child whose `touch` side effect never appears.
    //
    // Zero is the ONLY value refused, and the reason is this loop's granularity
    // rather than the cost of a call. The loop `try_wait`s first and consults
    // `deadline` only on `None`, so nothing can expire before the first 25 ms
    // sleep: every non-zero timeout under ~25 ms grants ~25 ms in practice, and a
    // 1 ms deadline was measured letting a real child run to completion at exit 0
    // with no error at all. So a nearly-spent budget still buys one honest
    // attempt at a 13-29 ms `security` call, at the price of overrunning its own
    // remainder by up to one poll interval. That overrun is bounded and paid for:
    // 5 s separates `SUBPROCESS_BUDGET` from the `STATE_LOCK_TIMEOUT` a peer
    // waits out, which absorbs ~200 of them.
    if timeout.is_zero() {
        anyhow::bail!(
            "{SECURITY_BIN} not run: this lock hold's subprocess budget is already spent \
             (an earlier keychain call under the same lock took all of it)"
        );
    }
    let mut child = cmd
        .stdin(if stdin_payload.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("failed to spawn {SECURITY_BIN}"))?;
    if let Some(payload) = stdin_payload {
        use std::io::Write;
        // Write the payload, then close the pipe (drop of `stdin`) so the child
        // sees EOF and runs. On any write failure (e.g. EPIPE if it died early)
        // kill/wait the child before returning: a bare `?` would leak it as a
        // zombie, unlike the timeout and normal-exit paths below.
        let write_result: Result<()> = child
            .stdin
            .take()
            .context("child stdin unexpectedly absent")
            .and_then(|mut stdin| {
                stdin
                    .write_all(payload.as_bytes())
                    .with_context(|| format!("failed to write {SECURITY_BIN} stdin"))
            });
        if let Err(e) = write_result {
            let _ = child.kill();
            let _ = child.wait();
            return Err(e);
        }
    }
    // Drain both pipes on their own threads, started BEFORE the poll loop: the
    // child must be free to write past the pipe buffer while we wait for it to
    // exit. Taking the handles moves the read ends into the threads, so they —
    // not this fn — close them at EOF.
    let stdout_reader = drain_pipe(
        child
            .stdout
            .take()
            .context("child stdout unexpectedly absent")?,
    );
    let stderr_reader = drain_pipe(
        child
            .stderr
            .take()
            .context("child stderr unexpectedly absent")?,
    );
    let deadline = Instant::now() + timeout;
    loop {
        match child
            .try_wait()
            .with_context(|| format!("failed to poll {SECURITY_BIN}"))?
        {
            Some(status) => {
                let stdout = collect_drained(stdout_reader)?;
                let stderr = collect_drained(stderr_reader)?;
                return Ok(Output {
                    status,
                    stdout,
                    stderr,
                });
            }
            None => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    // The readers are deliberately NOT joined here: they see EOF
                    // once the reaped child's pipe ends close, and joining would
                    // re-introduce an unbounded wait if anything else inherited
                    // the write end. That case instead strands one reader thread
                    // with its buffer — bounded, and it never holds this loop.
                    // The accepted cost; do not "fix" it by re-adding the join.
                    // `{timeout:?}` rather than whole seconds: a clamped budget
                    // hands this sub-second values, which `as_secs()` prints as
                    // a nonsensical `0s`.
                    anyhow::bail!(
                        "{SECURITY_BIN} exceeded its {timeout:?} deadline and was killed \
                         (keychain locked, an ACL prompt left unanswered, or an earlier \
                         call under this lock already spent the hold's budget)"
                    );
                }
                std::thread::sleep(Duration::from_millis(25));
            }
        }
    }
}

/// Read `pipe` to EOF on its own thread, so a child producing more than the
/// pipe buffer cannot block on `write(2)` while the poll loop waits for it to
/// exit — see [`run_with_deadline`].
fn drain_pipe(
    mut pipe: impl std::io::Read + Send + 'static,
) -> std::thread::JoinHandle<std::io::Result<Vec<u8>>> {
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        pipe.read_to_end(&mut buf)?;
        Ok(buf)
    })
}

/// Reap a drain thread's bytes with the same error surface the post-exit
/// `wait_with_output` had ("failed to collect … output"). A panic in the reader
/// is not reachable — `read_to_end` does not panic — but the join is still
/// handled rather than unwrapped.
fn collect_drained(reader: std::thread::JoinHandle<std::io::Result<Vec<u8>>>) -> Result<Vec<u8>> {
    reader
        .join()
        .map_err(|_| anyhow::anyhow!("output drain thread panicked"))?
        .with_context(|| format!("failed to collect {SECURITY_BIN} output"))
}

/// Keychain generic-password service Claude Code reads/writes for its login.
/// The literal lives in `claude.rs` beside the pure rules that derive the
/// per-config-dir twins from it, so one spelling serves both sides.
const SERVICE: &str = crate::claude::CLAUDE_KEYCHAIN_SERVICE;

/// Longest `security -i` command line this writer will send, trailing `\n`
/// included. Measured on `mac-6` (macOS 26.5.2) 2026-09-01 against a throwaway
/// service: the tool reads one command per line into a 4096-byte buffer of
/// command TEXT. A 4097-byte line including the `\n` round-trips intact 6/6; a
/// 4098-byte line truncates at 4096, the write exits 1, and the tail re-parses
/// as `security: unknown command`. So `line_len <= 4096` including the `\n`
/// (text <= 4095) holds 1-2 bytes of headroom under the break — and the branch
/// must key on the exact line length, because the over-ceiling failure is
/// DESTRUCTIVE: the truncated head still executes, leaving the item holding
/// cut-off JSON.
///
/// This mattered nowhere until the mirror started preserving Claude Code's
/// sibling keys: a login-only blob is 1-2 KB and never came close, while a blob
/// carrying `mcpOAuth` for a dozen-plus OAuth MCP servers clears it easily.
const SECURITY_STDIN_LINE_MAX: usize = 4096;

/// Largest value this writer will put on `security`'s argv. Measured on `mac-6`
/// (macOS 26.5.2) 2026-09-01: there is NO `security`-internal ceiling — the
/// break is the exec limit itself, `E2BIG` (`Argument list too long`) at
/// 1,048,000 bytes of argv with `kern.argmax` = 1,048,576 — and that break
/// FAILS SAFE: exec never happens, so the item is left untouched. Values
/// round-trip intact through 1,047,900 bytes, so this cap holds a ~2x margin
/// under the one measured break; the margin, not the measurement, is what
/// other macOS versions get. A blob past it is refused rather than sent — not
/// the only non-destructive disposition (E2BIG past the break fails safe too),
/// but the only one that names the size and the remediation instead of
/// surfacing as a bare spawn failure.
const SECURITY_ARGV_VALUE_MAX: usize = 512 * 1024;

/// How [`put_blob_at`] hands one write to `security`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PutTransport {
    /// `security -i`, command line over stdin. Keeps the token out of this
    /// process's argv, hence out of Endpoint Security exec logging (PR #21).
    Stdin,
    /// `security add-generic-password …` with the value as a real argv word.
    /// Taken exactly when the `-i` command line would exceed
    /// [`SECURITY_STDIN_LINE_MAX`], where `security` truncates instead of
    /// failing — the EDR-log property is traded for an intact item. argv is
    /// still same-UID-or-root only, the tradeoff PR #21 called already
    /// accepted. Correctness wins: the alternative is a truncated item.
    Argv,
}

/// Pick the transport for a `-i` command line of `line_len` bytes carrying a
/// `value_len`-byte password. PURE, so the decision and both ceilings are pinned
/// without a Keychain.
fn put_transport(line_len: usize, value_len: usize) -> Result<PutTransport> {
    if line_len <= SECURITY_STDIN_LINE_MAX {
        return Ok(PutTransport::Stdin);
    }
    if value_len <= SECURITY_ARGV_VALUE_MAX {
        return Ok(PutTransport::Argv);
    }
    anyhow::bail!(
        "refusing to write a {value_len}-byte Keychain item: it is over the \
         {SECURITY_ARGV_VALUE_MAX}-byte cap this writer keeps below the exec limit (the stdin \
         transport would truncate it instead of failing). The item's size is dominated by the \
         `mcpOAuth` entries carried beside the login; disable MCP servers or plugins that carry \
         OAuth entries until the item shrinks below the cap"
    )
}

/// `security(1)` exit status for `errSecItemNotFound` (-25300). Returned when no
/// matching item exists; treated as "absent" (`None`) on read and a no-op on delete.
const EXIT_ITEM_NOT_FOUND: i32 = 44;

/// Whether the live-credential paths in `claude.rs` route through the Keychain.
/// `true` in the shipped binary; `false` under `cfg(test)` so the test suite
/// keeps the file/symlink model and NEVER touches the operator's real
/// `Claude Code-credentials` item. The CLI plumbing itself is covered by the KC-1
/// tests, which drive `read_blob_at` / `merge_and_put_at` / `put_blob_at` /
/// `delete_at` on a throwaway service directly; the merge RULES they carry are
/// pinned platform-independently where they live (`claude.rs`, `profile.rs`).
#[cfg(not(test))]
pub(crate) fn enabled() -> bool {
    true
}

#[cfg(test)]
pub(crate) fn enabled() -> bool {
    false
}

/// The Keychain `account` Claude Code stores its credential blob under: the OS
/// login name. Every `*-generic-password` call site in CC passes this same
/// `$USER`-derived value (its own fallback for an unusable `$USER` is the literal
/// `claude-code-user`, which clauth does not reproduce), so pinning the account
/// keeps clauth writing where CC reads.
///
/// A previous note here claimed a *separate* item at `account = "unknown"` held
/// `mcpOAuth`. That is wrong and was load-bearing for the wrong conclusion: CC
/// keeps ONE item holding one JSON blob, and `mcpOAuth` is a sibling key of
/// `claudeAiOauth` inside it (traced on 2.1.210 and
/// 2.1.227), which is what makes the read-modify-write below necessary.
fn account() -> Result<String> {
    std::env::var("USER")
        .or_else(|_| std::env::var("LOGNAME"))
        .context("cannot determine macOS login name ($USER/$LOGNAME unset) for Keychain access")
}

/// What a write may keep from the item it replaces. The incoming blob always
/// wins on the keys it carries; this decides what of the OLD one survives beside
/// it, and the two arms are the file path's own two rules.
#[derive(Clone, Copy)]
enum Keep {
    /// The incoming login belongs to THIS account, rotated by clauth
    /// (`oauth.rs`'s mirror, which fires only for the active profile and only
    /// once the live login is known not to be foreign). Nothing in the item is
    /// another account's, so every block survives.
    Everything,
    /// The incoming login may belong to another account (a switch, a relink).
    /// Only the account-independent keys cross; the outgoing account's org id,
    /// device token and gateway blocks do not, for the reason
    /// `strip_home_oauth_account` deletes the cached identity in
    /// `~/.claude.json` on every switch: a present-but-wrong one never
    /// self-corrects.
    CarriedOnly,
}

/// Read the item's password at `(service, account)` RAW — exactly the bytes
/// `security find-generic-password -w` prints, trailing newline included —
/// without parsing them. `Ok(None)` when the item is absent (exit 44); any
/// other failure is an error. Two consumers need the bytes rather than a
/// parsed object: the write's read-back verification byte-compares against what
/// was sent, and the quarantine path preserves what a parse rejected.
fn read_raw_at(service: &str, account: &str) -> Result<Option<String>> {
    let mut cmd = Command::new(SECURITY_BIN);
    cmd.args(["find-generic-password", "-s", service, "-a", account, "-w"]);
    let output = run_with_deadline(cmd, security_deadline(), None)
        .with_context(|| format!("failed to run {SECURITY_BIN} find-generic-password"))?;
    if output.status.success() {
        let raw = String::from_utf8(output.stdout).context("Keychain password is not UTF-8")?;
        Ok(Some(raw))
    } else if output.status.code() == Some(EXIT_ITEM_NOT_FOUND) {
        Ok(None)
    } else {
        Err(security_error(SecurityOp::Read, &output))
    }
}

/// Read the JSON blob stored at `(service, account)` via
/// `security find-generic-password -w`. `Ok(None)` when the item is absent
/// (exit 44); any other failure is an error. Returns the RAW object rather than
/// a typed [`ClaudeCredentials`], which models the login alone and would drop
/// the very siblings the read exists to preserve.
///
/// A read that returns bytes which are not JSON fails with [`UnparseableItem`]
/// carrying them, so the caller can quarantine before overwriting or deleting
/// the item; every other failure (a locked keychain, a timeout, a spent budget)
/// saw no bytes and carries none.
fn read_blob_at(service: &str, account: &str) -> Result<Option<Value>> {
    let Some(raw) = read_raw_at(service, account)? else {
        return Ok(None);
    };
    match serde_json::from_str(raw.trim_end()) {
        Ok(blob) => Ok(Some(blob)),
        Err(parse_error) => Err(anyhow::Error::new(UnparseableItem { raw, parse_error })),
    }
}

/// A Keychain read that answered with bytes this module cannot parse as the
/// JSON object Claude Code expects. The bytes ride the error — never its
/// `Display`, which reaches event lines — because they are usually a TRUNCATED
/// version of the real item (the `claudeAiOauth` head survives with its tail
/// cut, #66/#76), and the two sites that act on a failed read used to destroy
/// them by overwriting or deleting the item unseen.
struct UnparseableItem {
    /// The item's password exactly as `security -w` printed it, trailing
    /// newline included.
    raw: String,
    parse_error: serde_json::Error,
}

// Hand-written like `ConsoleCredential`'s: a derived
// `Debug` would print `raw` — live credential bytes — and a stray `{:?}` on
// this error would put a session on a log line. The length is all a debug
// reader needs.
impl std::fmt::Debug for UnparseableItem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UnparseableItem")
            .field("raw_len", &self.raw.len())
            .field("parse_error", &self.parse_error)
            .finish()
    }
}

impl std::fmt::Display for UnparseableItem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The bytes stay off the Display: this text reaches event lines, and
        // the point of carrying them on the struct is to keep them out of
        // exactly those lines until the quarantine file takes them.
        write!(f, "Keychain item is not valid JSON: {}", self.parse_error)
    }
}

impl std::error::Error for UnparseableItem {}

/// The raw item bytes a failed read carried, when it carried any. Only an
/// unparseable read has bytes to preserve — the item answered, just not with
/// JSON this module can merge; every other read failure (a locked keychain, a
/// timeout, a budget clamped to zero) saw nothing and returns `None`, behaving
/// exactly as it did before quarantine existed. PURE so the split — which
/// failures quarantine, which do not — is pinned without a Keychain.
fn carried_raw(e: &anyhow::Error) -> Option<&str> {
    e.downcast_ref::<UnparseableItem>().map(|u| u.raw.as_str())
}

/// Where salvaged Keychain bytes land: `~/.clauth/keychain-quarantine/`, a
/// sibling of `profiles/` in the owner-only tree (the parked
/// `mcp-logins.json`'s placement is the precedent).
const QUARANTINE_DIR: &str = "keychain-quarantine";

/// The per-event file salvaged bytes from `service` land in, under `base`'s
/// quarantine directory: a UTC timestamp names the event, the pid keeps two
/// processes that salvage the same item in the same second apart (the daemon
/// and a TUI each mirror on their own ticks), and the service name identifies
/// WHICH item the file holds — the bare item and a namespaced per-config-dir
/// one can both exist. Per-event, not per-salvage-unique: two salvages by the
/// SAME process in the same second on the same service land on one path and
/// the second rename silently replaces the first — reachable when one write's
/// read leg salvages unparseable bytes and its verify leg then quarantines a
/// corrupt read-back inside that second, and accepted because the surviving
/// file then holds the later bytes of the same incident and the event line
/// names the same path either way. PURE (the `put_transport` pattern) so the
/// derivation is pinned without a Keychain.
fn quarantine_path(base: &Path, service: &str, epoch_secs: i64, pid: u32) -> PathBuf {
    // Compact UTC rather than `epoch_secs_to_iso`'s shape: a filename on macOS
    // must not carry `:` (Finder renders it as a path separator), and an
    // out-of-range epoch degrades to the raw seconds — still unique per event,
    // still sortable, just not pretty.
    let stamp = chrono::DateTime::from_timestamp(epoch_secs, 0).map_or_else(
        || epoch_secs.to_string(),
        |dt| dt.format("%Y%m%dT%H%M%SZ").to_string(),
    );
    // `/` and NUL are the bytes a path separator could hide in; spaces and
    // dashes (all the real service names carry) stay legible.
    let service = service.replace(['/', '\0'], "_");
    base.join(QUARANTINE_DIR)
        .join(format!("{stamp}-{pid}-{service}.json"))
}

/// Write `raw` — bytes salvaged from the item at `service`, which this module
/// is about to overwrite or delete — to its quarantine file, and return the
/// path. `atomic_write_600` owns the posture (0700 dir on create, 0600 file
/// before the rename — the 0600/0700 tree invariant): the bytes are
/// exactly as sensitive as a `credentials.json`, so they inherit its placement
/// rather than landing outside the tree.
fn quarantine_item_bytes(service: &str, raw: &str) -> Result<PathBuf> {
    let path = quarantine_path(
        &crate::profile::clauth_dir()?,
        service,
        crate::usage::now_epoch_secs(),
        std::process::id(),
    );
    crate::profile::atomic_write_600(&path, raw).with_context(|| {
        format!(
            "failed to quarantine the Keychain item's bytes at {}",
            path.display()
        )
    })?;
    Ok(path)
}

/// How an event line names where salvaged bytes went: the file plus the
/// recovery hint when the quarantine landed, the loss plus the re-auth hint
/// when the write itself failed. Shared by the two sites that act on a failed
/// read, so their wording cannot drift apart. PURE so the operator-facing text
/// is pinned without staging an incident.
fn quarantine_tail(quarantined: &Result<PathBuf>) -> String {
    match quarantined {
        Ok(path) => format!(
            "raw bytes are preserved at {}: the `claudeAiOauth` login head usually survives this \
             corruption, so re-authenticate any MCP server that reports a signed-out session, or \
             slice the login out of the quarantined file",
            path.display()
        ),
        Err(e) => format!(
            "raw bytes could not be preserved ({e}): re-authenticate any MCP server that reports \
             a signed-out session, or sign in again at claude.ai"
        ),
    }
}

/// The item's current contents for a merge. An absent item is the first-write
/// case and merges as `None` quietly; a read that FAILS also merges as `None`,
/// but names the loss on the event line first (module doc: the write still
/// lands, because a refused switch is worse than a lost MCP login). A read
/// that failed WITH BYTES quarantines them before returning — the overwrite
/// below is exactly what would otherwise destroy them (#66/#76).
fn blob_to_merge_with(service: &str, account: &str) -> Option<Value> {
    match read_blob_at(service, account) {
        Ok(blob) => blob,
        Err(e) => {
            match carried_raw(&e) {
                Some(raw) => logline!(
                    "clauth: could not read the macOS Keychain login before replacing it ({e:#}). \
                     The MCP server logins it held are replaced by whatever this profile last \
                     stored, which on macOS is older than the item's own set — their {}",
                    quarantine_tail(&quarantine_item_bytes(service, raw))
                ),
                None => logline!(
                    "clauth: could not read the macOS Keychain login before replacing it ({e:#}). \
                     The MCP server logins it held are replaced by whatever this profile last \
                     stored, which on macOS is older than the item's own set: re-authenticate \
                     any MCP server that reports a signed-out session, and any that starts failing"
                ),
            }
            None
        }
    }
}

/// The object to write: `incoming`, plus whatever [`Keep`] lets the item it
/// replaces hand over. Pure, and covered by `merged_blob_*` in this module's
/// tests, which run in the ordinary macOS suite and touch no Keychain. The rules
/// themselves are pinned where they live, so a platform that cannot compile this
/// module still guards them.
///
/// [`Keep::CarriedOnly`] widens to [`Keep::Everything`] when the item already
/// holds the exact login being installed. That is a relink rather than a switch:
/// the account cannot have changed, so dropping its own org id and device token
/// would be a loss with no wrong-account risk to justify it. A login that
/// DIFFERS still takes the allowlist, including a rotation of the same account,
/// which this cannot recognise (`oauth.rs`'s mirror passes `Everything`
/// explicitly because only it holds that knowledge).
fn merged_blob(incoming: &Value, existing: Option<&Value>, keep: Keep) -> Value {
    const LOGIN: &str = "claudeAiOauth";
    let mut out = incoming.clone();
    // A NON-EMPTY access token on both sides, never mere key presence: Claude
    // Code's logged-out shell is a login block with the tokens blanked, and two
    // accounts' shells are equal to each other. `classify_link_at` and the link
    // guard both draw the line the same way — two blanks are two logged-out
    // shells, never a match — and a shell matching here would carry the OTHER
    // account's org id and device token onto this one.
    let live_token = |v: Option<&Value>| -> Option<String> {
        v?.get(LOGIN)?
            .get("accessToken")?
            .as_str()
            .filter(|t| !t.is_empty())
            .map(str::to_string)
    };
    let same_login = live_token(Some(&out)).is_some()
        && live_token(Some(&out)) == live_token(existing)
        && existing.and_then(|e| e.get(LOGIN)) == out.get(LOGIN);
    match keep {
        Keep::Everything => crate::profile::preserve_extra_blocks(&mut out, existing),
        Keep::CarriedOnly if same_login => {
            crate::profile::preserve_extra_blocks(&mut out, existing);
        }
        Keep::CarriedOnly => {
            if let (Some(out_obj), Some(existing_obj)) =
                (out.as_object_mut(), existing.and_then(Value::as_object))
            {
                crate::claude::carry_live_extra_over(out_obj, existing_obj);
            }
        }
    }
    out
}

/// Quote `s` for `security -i`'s line tokenizer: wrap in `"…"` with `\` → `\\`
/// and `"` → `\"`. Verified empirically (macOS 15 / Darwin 25): an escaped
/// quoted string round-trips byte-identical through `add-generic-password -w`,
/// including embedded spaces, double quotes, and backslashes; an UNquoted value
/// containing whitespace is split into separate argv words (usage error).
/// Embedded newlines are refused — `-i` is a line protocol, and a `\n` inside a
/// value would be parsed as a second command.
fn security_quote(s: &str) -> Result<String> {
    if s.contains('\n') || s.contains('\r') {
        anyhow::bail!("refusing to pass a value with an embedded newline to `security -i`");
    }
    Ok(format!(
        "\"{}\"",
        s.replace('\\', "\\\\").replace('"', "\\\"")
    ))
}

/// Merge `incoming` over whatever the item at `(service, account)` already holds
/// (per `keep`) and write the result back. The read leg is what keeps CC's
/// sibling blocks alive across the `-U` replace; see the module doc for the ACL
/// prompt it costs and the posture when it fails.
///
/// Whether a write happens at all is [`merge_write`]'s call, which is where the
/// skip and its reasons live.
fn merge_and_put_at(service: &str, account: &str, incoming: &Value, keep: Keep) -> Result<()> {
    let existing = blob_to_merge_with(service, account);
    match merge_write(incoming, existing.as_ref(), keep) {
        Some(blob) => put_blob_at(service, account, &blob),
        None => Ok(()),
    }
}

/// The object [`merge_and_put_at`] must write, or `None` when the merge
/// reproduces the item byte for byte and the write is skipped.
///
/// Split out of the IO so the skip is pinned without a Keychain — it is the one
/// decision in this module that costs nothing when wrong in the cheap direction
/// and a subprocess per tick when wrong in the other, and it had no test at all.
/// The rules it composes are pinned on every platform where they live
/// (`claude::carry_live_extra_over`, `profile::preserve_extra_blocks`); this and
/// [`merged_blob`] are pinned in the ordinary macOS suite, which is as wide as a
/// `#[cfg(target_os = "macos")]` module reaches.
///
/// `None` is load-bearing rather than tidy: the daemon and the TUI relink the
/// active profile on a tick, so the common call installs a login the item already
/// holds, and each avoided write is one fewer `security` subprocess drawing on a
/// budget the whole lock hold shares ([`security_deadline`]). A read that FAILED
/// merges as `None` (`blob_to_merge_with`), which compares equal to no blob, so
/// that path always writes — losing the item's siblings is the accepted cost of
/// completing the switch, and skipping the write would lose the LOGIN too.
fn merge_write(incoming: &Value, existing: Option<&Value>, keep: Keep) -> Option<Value> {
    let blob = merged_blob(incoming, existing, keep);
    if existing == Some(&blob) {
        return None;
    }
    Some(blob)
}

/// The `security -i` command line that writes `json` at `(service, account)`:
/// one `add-generic-password -U`, every value [`security_quote`]-escaped,
/// newline-terminated (`-i` is a line protocol). Split out of [`put_blob_at`]
/// so the exact bytes the transport decision keys on are assertable without a
/// Keychain.
fn add_generic_password_line(service: &str, account: &str, json: &str) -> Result<String> {
    Ok(format!(
        "add-generic-password -U -s {} -a {} -w {}\n",
        security_quote(service)?,
        security_quote(account)?,
        security_quote(json)?,
    ))
}

/// What reading an item back after a reported-successful write proves about
/// that write. Carries what the caller needs to act: the differing bytes to
/// quarantine on [`VerifyOutcome::Corrupt`], the rendered cause to name on
/// [`VerifyOutcome::Unverified`].
#[derive(PartialEq, Eq)]
enum VerifyOutcome {
    /// Read back byte-equal to the JSON that was written.
    Verified,
    /// Read back with different bytes, which the variant carries for
    /// quarantine. The item is KNOWN corrupt: completing the switch would lie.
    Corrupt(String),
    /// Read back absent after a write that reported success. Equally known
    /// corrupt, but there are no bytes to quarantine — the write did not land.
    Vanished,
    /// The read-back itself could not run (a budget clamped to zero, a
    /// deadline, a read refusal over ssh). The write's own exit code said it
    /// landed, and an unverifiable write must not fail a completed switch, so
    /// this arm is success for the caller — with the rendered cause for the
    /// event line.
    Unverified(String),
}

// Hand-written like `UnparseableItem`'s: `Corrupt` carries live read-back
// credential bytes, and a derived `Debug` would print them. The cause on
// `Unverified` is rendered error text, never the bytes, so it prints.
impl std::fmt::Debug for VerifyOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            VerifyOutcome::Verified => f.write_str("Verified"),
            VerifyOutcome::Corrupt(raw) => f
                .debug_struct("Corrupt")
                .field("raw_len", &raw.len())
                .finish(),
            VerifyOutcome::Vanished => f.write_str("Vanished"),
            VerifyOutcome::Unverified(cause) => {
                f.debug_struct("Unverified").field("cause", cause).finish()
            }
        }
    }
}

/// Decide what a read-back proves about `written`. PURE (the `put_transport`
/// pattern) so the whole truth table — match, mismatch, absent, unreadable —
/// is pinned without a Keychain. The comparison is BYTES, with only the `-w`
/// output's trailing whitespace (which a written JSON never carries)
/// normalized away: a value that differs by even leading whitespace is not
/// what was written and must read as corrupt, never as a match.
fn verify_outcome(read_back: Result<Option<String>>, written: &str) -> VerifyOutcome {
    match read_back {
        Err(e) => VerifyOutcome::Unverified(format!("{e:#}")),
        Ok(None) => VerifyOutcome::Vanished,
        Ok(Some(raw)) if raw.trim_end() == written => VerifyOutcome::Verified,
        Ok(Some(raw)) => VerifyOutcome::Corrupt(raw),
    }
}

/// What a read-back outcome makes [`verify_write`] do. Carries the payload
/// each disposition needs: the read-back bytes to quarantine on
/// [`WriteDisposition::QuarantineAndFail`], the rendered cause to name on
/// [`WriteDisposition::CompleteWithNote`].
#[derive(PartialEq, Eq)]
enum WriteDisposition {
    /// The read-back matched: nothing to say, nothing to do.
    CompleteSilently,
    /// Quarantine the carried bytes, name them on the event line, and FAIL the
    /// write — the switch must not complete on a write known corrupt.
    QuarantineAndFail(String),
    /// FAIL the write: known corrupt, with no bytes to quarantine.
    Fail,
    /// Complete the switch, naming the carried cause on the event line — an
    /// unverifiable write must not fail a completed switch.
    CompleteWithNote(String),
}

// Hand-written like `UnparseableItem`'s: `QuarantineAndFail` carries the same
// live read-back credential bytes, and a derived `Debug` would print them.
impl std::fmt::Debug for WriteDisposition {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WriteDisposition::CompleteSilently => f.write_str("CompleteSilently"),
            WriteDisposition::QuarantineAndFail(raw) => f
                .debug_struct("QuarantineAndFail")
                .field("raw_len", &raw.len())
                .finish(),
            WriteDisposition::Fail => f.write_str("Fail"),
            WriteDisposition::CompleteWithNote(cause) => f
                .debug_struct("CompleteWithNote")
                .field("cause", cause)
                .finish(),
        }
    }
}

/// Map a read-back outcome to its disposition. PURE, and pinned with
/// [`disposition_verdict`] as its other half: the classification
/// ([`verify_outcome`]) was pinned from the start, but the RULING — which
/// outcomes fail a switch, which complete it — lived only in `verify_write`'s
/// arms, and a one-line flip of it (a corrupt write completing a switch) left
/// every test in the suite green. Both halves of the ruling now live in pure,
/// truth-table-pinned code; [`verify_write`] is only the executor.
fn write_disposition(outcome: VerifyOutcome) -> WriteDisposition {
    match outcome {
        VerifyOutcome::Verified => WriteDisposition::CompleteSilently,
        VerifyOutcome::Corrupt(raw) => WriteDisposition::QuarantineAndFail(raw),
        VerifyOutcome::Vanished => WriteDisposition::Fail,
        VerifyOutcome::Unverified(cause) => WriteDisposition::CompleteWithNote(cause),
    }
}

/// The verdict each disposition hands the caller — the owner-ruled M1
/// enforcement in one pure place: a write KNOWN corrupt fails the switch, an
/// UNVERIFIABLE one completes it. PURE and pinned beside the truth table so
/// the `Err`/`Ok` literals live in tested code rather than in the executor's
/// arms.
fn disposition_verdict(disposition: &WriteDisposition) -> Result<()> {
    match disposition {
        WriteDisposition::CompleteSilently | WriteDisposition::CompleteWithNote(_) => Ok(()),
        WriteDisposition::QuarantineAndFail(_) => Err(anyhow::anyhow!(
            "Keychain write verified corrupt: the item read back different bytes than were \
             written"
        )),
        WriteDisposition::Fail => Err(anyhow::anyhow!(
            "Keychain write verified corrupt: the item is absent after a write that reported \
             success"
        )),
    }
}

/// The read-back half of [`put_blob_at`]: read the item RAW, and execute the
/// disposition the pure pair rules — [`write_disposition`] decides,
/// [`disposition_verdict`] rules, this fn runs the side effects each
/// disposition names (quarantine, event lines) and hands the ruling back
/// verbatim. `Verified` is silent. `Corrupt` quarantines the read-back bytes
/// and fails the write; `Vanished` fails it — a write known corrupt must not
/// complete a switch, and the profile store still holds the intended login so
/// the retry is safe. `Unverified` names the write as landed but unverified
/// and lets it stand: the module's completing-the-switch posture, applied to
/// a write whose only failure is that nobody could check it.
fn verify_write(service: &str, account: &str, written: &str) -> Result<()> {
    let disposition = write_disposition(verify_outcome(read_raw_at(service, account), written));
    let verdict = disposition_verdict(&disposition);
    match &disposition {
        // Silence is the disposition, not an oversight.
        WriteDisposition::CompleteSilently => {}
        WriteDisposition::QuarantineAndFail(raw) => match quarantine_item_bytes(service, raw) {
            Ok(path) => logline!(
                "clauth: the macOS Keychain write reported success but the item read back with \
                     different bytes, so it is known corrupt and the switch was not completed. The \
                     corrupted item's raw bytes are preserved at {}; the profile store still holds \
                     the intended login, so retry the switch to re-run the write",
                path.display()
            ),
            Err(e) => logline!(
                "clauth: the macOS Keychain write reported success but the item read back with \
                     different bytes, so it is known corrupt and the switch was not completed. \
                     Preserving the corrupted item's raw bytes failed ({e}); the profile store \
                     still holds the intended login, so retry the switch to re-run the write"
            ),
        },
        WriteDisposition::Fail => {
            logline!(
                "clauth: the macOS Keychain write reported success but the item read back absent, \
                 so the write is known corrupt and the switch was not completed. The profile \
                 store still holds the intended login, so retry the switch to re-run the write"
            );
        }
        WriteDisposition::CompleteWithNote(cause) => {
            logline!(
                "clauth: the macOS Keychain write landed but could not be read back to verify \
                 ({cause}); the switch stays complete. If Claude Code reports a signed-out or \
                 broken session, retrying the switch re-runs the write"
            );
        }
    }
    verdict
}

/// Add-or-update the item at `(service, account)` with `blob` as its password,
/// the whole `{"claudeAiOauth":{…}, …}` JSON object Claude Code expects, via
/// `security add-generic-password -U`. `-U` updates the item in place when it
/// already exists (created by Claude Code) and adds it otherwise. Callers go
/// through [`merge_and_put_at`] unless they have already derived the whole
/// object, as the sign-out has.
///
/// The transport is chosen by size ([`put_transport`]). A command line that
/// fits [`SECURITY_STDIN_LINE_MAX`] goes to `security -i` over **stdin**, so
/// the token never appears in this process's own argv — keeping it out of
/// process-exec logging (Endpoint Security `es_event_exec_t`, i.e. most EDR
/// agents), which captures full command lines at exec time but not pipe
/// contents. A line past the cap rides argv instead (the value as a real `-w`
/// argument word), DELIBERATELY giving up that EDR property only for blobs too
/// large to keep it: past the cap `security -i` does not refuse but truncates,
/// and a truncated item is a lost login plus lost MCP-server logins. (Plain
/// same-UID `ps` exposure was already an accepted tradeoff: argv is readable
/// only by the same UID or root on macOS, and a same-UID process already owns
/// the 0o600 credential files.) `-i`'s tokenizer needs the [`security_quote`]
/// escaping for values with whitespace, which the argv branch does not — argv
/// words reach `security` verbatim; the inner command's exit code propagates
/// as `security -i`'s own exit code (verified: 0 on success, 44 for
/// `errSecItemNotFound`, 2 on usage error). The no-value `-w` prompt form is
/// still unusable here — it reads from the controlling *tty*
/// (`readpassphrase`), not stdin, so a pipe can't feed it.
///
/// A write is gated on the keychain being UNLOCKED, never on the target item's
/// trust list, so it lands silently against an item ACL'd to Claude Code alone
/// and raises no dialog of its own (measured on `mac-6` 2026-08-12). It also
/// does not re-ACL the item, which is why the read leg keeps needing its own
/// one-time grant.
///
/// A write that reports success is then VERIFIED: the item is read back raw
/// and byte-compared against the JSON just sent ([`verify_write`]). The tool's
/// exit code alone proved nothing — the `-i` truncation the transport ceilings
/// exist to prevent exited 0 while the item held cut-off JSON. A byte-equal
/// read-back is the only silent success; different bytes or an absent item
/// fail the switch with the read-back bytes quarantined first, and a read-back
/// that cannot run completes the switch as landed-but-unverified (the budget
/// the whole hold shares is one reason it cannot run, which is exactly why
/// that arm must not fail the write).
fn put_blob_at(service: &str, account: &str, blob: &Value) -> Result<()> {
    let json = serde_json::to_string(blob).context("failed to serialize the Keychain item")?;
    let line = add_generic_password_line(service, account, &json)?;
    // Past `-i`'s line ceiling the tokenizer truncates the value instead of
    // refusing, so the transport is chosen by size, not by preference.
    let output = match put_transport(line.len(), json.len())? {
        PutTransport::Stdin => {
            let mut cmd = Command::new(SECURITY_BIN);
            cmd.arg("-i");
            run_with_deadline(cmd, security_deadline(), Some(&line))
        }
        PutTransport::Argv => {
            logline!(
                "clauth: Keychain item is {} bytes on the `{SECURITY_BIN} -i` line, over the \
                 {SECURITY_STDIN_LINE_MAX} cap; writing it through argv instead, where the token \
                 is visible to same-UID `ps` for the life of the call",
                line.len()
            );
            // No `security_quote` here: argv words reach `security` verbatim, so
            // the `-i` tokenizer's escaping would be written INTO the password.
            let mut cmd = Command::new(SECURITY_BIN);
            cmd.args([
                "add-generic-password",
                "-U",
                "-s",
                service,
                "-a",
                account,
                "-w",
                &json,
            ]);
            run_with_deadline(cmd, security_deadline(), None)
        }
    }
    .with_context(|| format!("failed to run {SECURITY_BIN} add-generic-password"))?;
    if !output.status.success() {
        return Err(security_error(SecurityOp::Write, &output));
    }
    verify_write(service, account, &json)
}

/// Delete the item at `(service, account)` via `security delete-generic-password`.
/// Idempotent — a missing item (exit 44) is `Ok`.
fn delete_at(service: &str, account: &str) -> Result<()> {
    let mut cmd = Command::new(SECURITY_BIN);
    cmd.args(["delete-generic-password", "-s", service, "-a", account]);
    let output = run_with_deadline(cmd, security_deadline(), None)
        .with_context(|| format!("failed to run {SECURITY_BIN} delete-generic-password"))?;
    if output.status.success() || output.status.code() == Some(EXIT_ITEM_NOT_FOUND) {
        Ok(())
    } else {
        Err(security_error(SecurityOp::Delete, &output))
    }
}

/// Delete the NAMESPACED Keychain item at an already-derived `service` — the
/// stale-runtime GC's collector for a runtime tree it just removed
/// (`runtime::gc_one_pair`). The item holds a login only that tree's dir hash
/// resolves, so once the dir is gone it is inert clutter; this is the
/// collector half of the m4 LEAVE ruling (2026-09-12:
/// teardown pays no `security` subprocess, the GC pays it here instead).
///
/// Guarded on the service SHAPE (`claude::is_namespaced_keychain_service`):
/// this deletes by name, and a caller handing it the bare item — or anything
/// else the naming rule cannot have produced — would destroy a login no GC
/// decision explains. Idempotent like [`delete_at`]: a missing item is `Ok`,
/// so a re-run after a half-completed sweep costs one subprocess and nothing
/// else.
pub(crate) fn delete_namespaced_item(service: &str) -> Result<()> {
    anyhow::ensure!(
        crate::claude::is_namespaced_keychain_service(service),
        "refusing to delete Keychain item `{service}` through the GC path: it is not a \
         per-config-dir item name"
    );
    delete_at(service, &account()?)
}

/// Which `security` operation a failed invocation was running. WRITE is the
/// only one that puts a credential on the command line at all, so the split —
/// which failures may embed the tool's stderr and which must not — lives in
/// the type, not in a string an op could misspell into the wrong arm.
#[derive(Debug, Clone, Copy)]
enum SecurityOp {
    Read,
    Write,
    Delete,
}

impl SecurityOp {
    fn as_str(self) -> &'static str {
        match self {
            SecurityOp::Read => "read",
            SecurityOp::Write => "write",
            SecurityOp::Delete => "delete",
        }
    }
}

/// Build an error from a failed `security` invocation, including its exit code.
/// READ and DELETE embed the tool's stderr verbatim: no credential is ever sent
/// on those calls, so the text cannot be one, and it is diagnostic (an ACL
/// refusal reads differently from a usage error). WRITE does not — BY
/// CONSTRUCTION rather than by measurement: `security` HAS echoed an escaped
/// fragment of the written value into exactly this builder (`unknown command
/// "<tail>"`, observed in the field, GH #66), and this text rides event lines
/// into `daemon.log` — so a write failure reports the exit code and the stderr
/// byte COUNT, never the bytes.
fn security_error(op: SecurityOp, output: &std::process::Output) -> anyhow::Error {
    let code = output
        .status
        .code()
        .map_or_else(|| "signal".to_string(), |c| c.to_string());
    match op {
        SecurityOp::Write => anyhow::anyhow!(
            "Keychain {} failed (security exit {code}): its {}-byte stderr is not shown, because \
             a write's stderr can echo the value being written",
            op.as_str(),
            output.stderr.len()
        ),
        SecurityOp::Read | SecurityOp::Delete => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::anyhow!(
                "Keychain {} failed (security exit {code}): {}",
                op.as_str(),
                stderr.trim()
            )
        }
    }
}

/// Install `store`, the whole JSON object the file layer put in the live slot,
/// as Claude Code's login. This is what makes an account switch real on macOS:
/// Claude Code reads this on next launch. The MCP-server logins in the item being
/// replaced come across ([`Keep::CarriedOnly`]); the outgoing account's own
/// blocks do not.
///
/// Refuses a non-object blob rather than writing it. A store file is external
/// input to this layer, and CC parses the item's password as one JSON object, so
/// anything else would leave CC with a credential it cannot read and clauth with
/// no signal that it happened.
pub(crate) fn keychain_install(store: &Value) -> Result<()> {
    install_at(SERVICE, store)
}

/// Install `store` as the login for a SPECIFIC config dir's item: the
/// namespaced `Claude Code-credentials-<sha256(dir)[0:8]>` service a
/// `clauth start` session's Claude Code reads (it sets `CLAUDE_CONFIG_DIR`,
/// so CC namespaces its Keychain item per dir), rather than the bare item a
/// global `claude` reads. The multi-session swap executor writes this item
/// alongside the credential link it repoints — on macOS CC resolves the
/// Keychain FIRST, so the file repoint alone moves nothing a session reads.
///
/// Same merge codepath, same [`Keep::CarriedOnly`] (the MCP-login carry needs
/// no rule of its own), and the same non-object refusal as [`keychain_install`]:
/// one Keychain path, two service names.
pub(crate) fn keychain_install_for_config_dir(store: &Value, config_dir: &Path) -> Result<()> {
    let service = keychain_service_for_config_dir(config_dir)?;
    install_at(&service, store)
}

/// Read the whole JSON object the per-config-dir item for `config_dir` holds,
/// or `None` when no such item exists. The swap executor's carry-back reads it
/// before [`keychain_install_for_config_dir`] overwrites it: a session's CC
/// keeps its refreshed pair only there. Same read discipline as
/// [`read_blob_at`], over the derived service.
pub(crate) fn read_config_dir_item(config_dir: &Path) -> Result<Option<Value>> {
    let service = keychain_service_for_config_dir(config_dir)?;
    read_blob_at(&service, &account()?)
}

/// Whether a failed item read failed because the bytes were unparseable — the
/// truncated-write class (`UnparseableItem`, #66/#76). The carry-back treats
/// this as "nothing to carry" rather than an error, so the swap's item write
/// can heal the corruption instead of skipping inertly.
pub(crate) fn read_failed_unparseable(e: &anyhow::Error) -> bool {
    carried_raw(e).is_some()
}

fn install_at(service: &str, store: &Value) -> Result<()> {
    anyhow::ensure!(
        store.is_object(),
        "refusing to install a credential store that is not a JSON object into the Keychain"
    );
    merge_and_put_at(service, &account()?, store, Keep::CarriedOnly)
}

/// Mirror `creds` after clauth rotated THIS account's own chain (`oauth.rs`).
/// Same account by construction, so every block the item holds survives beside
/// the fresh login ([`Keep::Everything`]): the Keychain twin of the store
/// rewrite `profile::serialize_credentials_preserving_extra` performs on the
/// same rotation.
pub(crate) fn keychain_mirror_rotation(creds: &ClaudeCredentials) -> Result<()> {
    let login = serde_json::to_value(creds).context("failed to serialize Claude credentials")?;
    merge_and_put_at(SERVICE, &account()?, &login, Keep::Everything)
}

/// What a read of the real `Claude Code-credentials` item's login tells the
/// split-path mirror gate ([`item_login_state`]). The two SPLIT callers of
/// [`keychain_mirror_rotation`] (the rotation hook mirroring the freshly
/// stamped sidecar, and the rolling re-stamp leg) write `Keep::Everything` —
/// every sibling block in the item survives — so they must establish the
/// item's login is clauth's own FIRST: once CC migrates into the Keychain and
/// deletes the plaintext file, the file layer stops being evidence, and an
/// out-of-band `/login` as another account leaves the item holding B while
/// clauth still believes A is active — the next re-stamp would preserve B's
/// `organizationUuid`/`trustedDeviceToken`/`enterpriseGateway` beside A's
/// bearer, a mixed identity that never self-corrects.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ItemLoginState {
    /// The item's login is one clauth put there, or the item is absent, or it
    /// holds a blank (logged-out) shell. Proceed with the mirror.
    Ours,
    /// The item holds a non-empty login matching no bearer clauth knows it
    /// wrote or replaced. Skip: leave the item intact rather than layer this
    /// account's bearer under its blocks.
    NotOurs,
    /// The item answered with bytes that are not valid JSON — almost always a
    /// TRUNCATED version of clauth's own write (#66/#76). PROCEED: the
    /// mirror's own read leg quarantines the bytes and the write heals the
    /// item, exactly the behavior this gate must not lose.
    Corrupt,
    /// The item could not be read at all (a locked keychain, a spent budget,
    /// a headless refusal). Skip: a skipped mirror only delays the new bearer,
    /// while preserving blocks nobody could read has no upside. Carries the
    /// rendered cause for the event line.
    Unreadable(String),
}

/// Read the real item and classify its login for the split-path mirror gate
/// ([`ItemLoginState`]). "Ours" for a bearer that changes on every re-stamp
/// is decided by RECOGNITION: the caller passes the bearers it knows clauth
/// wrote or is replacing (the sidecar's pre-stamp bearer, the pre-rotation
/// chain token, the bearer about to be written), and the item's login must
/// match one of them.
///
/// This is a `security` SUBPROCESS: callers keep it out of any state-flock
/// hold (`oauth.rs` runs it after the lock closure ends, beside the write it
/// gates).
pub(crate) fn item_login_state(ours: &[&str]) -> ItemLoginState {
    let account = match account() {
        Ok(a) => a,
        Err(e) => return ItemLoginState::Unreadable(format!("{e:#}")),
    };
    match read_blob_at(SERVICE, &account) {
        Ok(blob) => {
            if login_blob_is_ours(blob.as_ref(), ours) {
                ItemLoginState::Ours
            } else {
                ItemLoginState::NotOurs
            }
        }
        Err(e) if carried_raw(&e).is_some() => ItemLoginState::Corrupt,
        Err(e) => ItemLoginState::Unreadable(format!("{e:#}")),
    }
}

/// The pure core of [`item_login_state`]: the recognition rule over an
/// already-read item blob, so the truth table is pinned without a Keychain
/// (the `put_transport` pattern). Blank means logged-out, the same line
/// [`merged_blob`]'s `live_token` draws: two logged-out shells are equal to
/// each other, so an empty token is never a match and never foreign.
fn login_blob_is_ours(blob: Option<&Value>, ours: &[&str]) -> bool {
    let Some(token) = blob
        .and_then(|b| b.get("claudeAiOauth"))
        .and_then(|l| l.get("accessToken"))
        .and_then(Value::as_str)
        .filter(|t| !t.is_empty())
    else {
        return true;
    };
    ours.contains(&token)
}

/// Sign Claude Code out of the real `Claude Code-credentials` item:
/// [`sign_out_at`] at [`SERVICE`], under the OS login account.
///
/// It is DESTRUCTIVE and the operator has no other copy of what it drops, so
/// both acting branches say so on the event line: two of the callers discard the
/// result (`daemon`, `tui`), and a switch that quietly deleted a login would
/// otherwise leave no local trace of why Claude Code is logged out. Only
/// `force_link_profile_credentials` and `clear_claude_credentials` reach it,
/// never the guarded relink, so a path that never meant to change accounts
/// cannot destroy a login clauth does not hold (`claude::keychain_mirror_source`).
pub(crate) fn keychain_sign_out() -> Result<()> {
    sign_out_at(SERVICE, &account()?)
}

/// Sign out the NAMESPACED item for a config dir — the start-site twin of
/// [`keychain_sign_out`], for a session whose profile stores no Claude login
/// (an api-key or endpoint profile). Same strip-and-maybe-delete core: the
/// session's Claude Code resolves this item before any file, so a departed
/// account's login left in it (an endpoint recapture's leftover under a
/// shared or recycled runtime dir) would keep serving that account.
pub(crate) fn keychain_sign_out_for_config_dir(config_dir: &Path) -> Result<()> {
    sign_out_at(&keychain_service_for_config_dir(config_dir)?, &account()?)
}

/// The sign-out core over an arbitrary `(service, account)`, parameterized the
/// way [`read_blob_at`]/[`put_blob_at`]/[`delete_at`] are so the e2e leg
/// (`tests/inline/keychain.rs`'s sign-out quarantine pin) drives a THROWAWAY
/// item — the hardwired [`SERVICE`] could only be driven against the
/// operator's real one.
///
/// Drop the account-scoped keys and keep what belongs to no account, so a
/// wrap-off or a forced relink onto a login-less profile stops the item serving
/// an account without taking every MCP-server login with it. An item left
/// holding nothing else is deleted outright, which keeps the clean-absence
/// state this had before the strip. Idempotent, and an absent item is success.
///
/// A read that fails deletes instead: whatever it could not preserve is worth
/// less than the item continuing to authenticate an account the operator just
/// switched away from. A read that failed WITH BYTES quarantines them first
/// ([`quarantine_item_bytes`]) — the delete still removes the login, but the
/// evidence survives it, and the event line says where.
fn sign_out_at(service: &str, account: &str) -> Result<()> {
    // The two `None` cases part here rather than sharing an early return: an
    // absent item is already signed out and says nothing, while a read that
    // FAILED takes the most destructive branch there is, deleting the item
    // whole — now with its raw bytes quarantined first whenever the read
    // brought any back (#66/#76).
    let mut blob = match read_blob_at(service, account) {
        Ok(None) => return delete_at(service, account),
        Ok(Some(blob)) => blob,
        Err(e) => {
            match carried_raw(&e) {
                Some(raw) => logline!(
                    "clauth: signed Claude Code out of the macOS Keychain by deleting the item: \
                     it could not be read first ({e:#}), so the MCP server logins stored beside \
                     the login went with it — their {}",
                    quarantine_tail(&quarantine_item_bytes(service, raw))
                ),
                None => logline!(
                    "clauth: signed Claude Code out of the macOS Keychain by deleting the item: it \
                     could not be read first ({e:#}), so the MCP server logins stored beside the \
                     login went with it. Re-authenticate any MCP server that reports a signed-out \
                     session"
                ),
            }
            return delete_at(service, account);
        }
    };
    match crate::claude::strip_account_credentials(&mut blob) {
        crate::claude::SignOut::Delete => {
            logline!(
                "clauth: signed Claude Code out of the macOS Keychain (the profile now active \
                 stores no Claude login). Run `clauth <name>` to put one back"
            );
            delete_at(service, account)
        }
        crate::claude::SignOut::Write => {
            logline!(
                "clauth: signed Claude Code out of the macOS Keychain (the profile now active \
                 stores no Claude login); its MCP server logins were kept"
            );
            put_blob_at(service, account, &blob)
        }
        crate::claude::SignOut::Nothing => Ok(()),
    }
}

/// Derive the Keychain service name for a given `CLAUDE_CONFIG_DIR`.
///
/// Claude Code on macOS namespaces its Keychain item per config directory:
/// `Claude Code-credentials-<sha256(dir)[0:8]>`. A bare (non-clauth) `claude`
/// uses the unsuffixed `Claude Code-credentials` because its config dir IS
/// `~/.claude`; a `clauth start` session sets `CLAUDE_CONFIG_DIR` to its
/// per-session runtime tree, so CC there reads a namespaced item that clauth
/// never wrote — and on its first token write, CC migrates credentials INTO
/// the namespaced item and DELETES the plaintext file, after which clauth's
/// stored refresh token goes stale.
///
/// This function returns the namespaced service name exactly as CC computes it.
/// Callers that write credentials for a per-session config dir must write to
/// THIS service, not the bare [`SERVICE`], or the session's CC never reads them.
/// The naming rule itself is pure and lives in `claude.rs`
/// (`namespaced_keychain_service`), pinned on every platform; this wrapper is
/// the CANONICALIZE half, which needs the dir to exist — which is why the
/// stale-runtime GC derives a doomed tree's service before it removes the
/// tree, never after.
///
/// The suffix is the first 8 hex chars of the SHA-256 of the canonicalized
/// directory path, matching CC's `sha256(configDir).toString('hex').slice(0, 8)`.
pub(crate) fn keychain_service_for_config_dir(config_dir: &Path) -> Result<String> {
    // Canonicalize: CC resolves symlinks before hashing, and a relative path
    // would produce a different hash than the absolute one CC computes.
    let canonical = std::fs::canonicalize(config_dir).with_context(|| {
        format!(
            "failed to canonicalize config dir: {}",
            config_dir.display()
        )
    })?;
    Ok(crate::claude::namespaced_keychain_service(&canonical))
}

#[cfg(test)]
#[path = "../tests/inline/keychain.rs"]
mod tests;
