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

use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde_json::Value;
use sha2::{Digest, Sha256};

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
/// **10 s because a mirror is TWO invocations**, a read and then a write or
/// delete, where it used to be one. `lock.rs`'s 25 s state-lock timeout and the
/// daemon's 30 s `WATCHDOG_DEADLINE` were sized against a 20 s mirror, and the
/// daemon's comment says to bound this shell-out rather than loosen them, so
/// halving keeps one mirror at the 20 s they assume.
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

// A mirror is TWO invocations, so one costs `2 x SECURITY_TIMEOUT` at worst, and
// that is the quantity `runtime::ROTATION_LOCK_TIMEOUT`'s floor budgets for this
// leg. It is spelled over there because this module is macOS-gated while that
// deadline is one number on every host; the check is a compile error rather than
// a test because the quantity exists only in this build, so this is the only
// build that can make it.
//
// Deliberately not `lock::SUBPROCESS_BUDGET`, which the two coincide with today:
// that bounds one state-flock hold's shell-outs in aggregate, and
// `oauth::apply_rotated_tokens_locked` runs its mirror after the closure ends,
// where `security_deadline` clamps nothing.
const _: () = assert!(
    SECURITY_TIMEOUT.as_millis() * 2 == crate::runtime::KEYCHAIN_MIRROR_BUDGET.as_millis(),
    "runtime::KEYCHAIN_MIRROR_BUDGET must stay two security invocations wide"
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
/// secret out of argv. The payload is a few KB and the write happens before the
/// poll loop; a macOS pipe buffer is 64 KB, so the single write cannot block.
///
/// `security` produces only a few bytes of output, so buffering it in the pipe
/// while we poll cannot deadlock on a full pipe buffer.
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
    let deadline = Instant::now() + timeout;
    loop {
        match child
            .try_wait()
            .with_context(|| format!("failed to poll {SECURITY_BIN}"))?
        {
            Some(_status) => {
                return child
                    .wait_with_output()
                    .with_context(|| format!("failed to collect {SECURITY_BIN} output"));
            }
            None => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
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

/// Keychain generic-password service Claude Code reads/writes for its login.
const SERVICE: &str = "Claude Code-credentials";

/// Longest command line `security -i`'s tokenizer reads as ONE command.
///
/// Measured on macOS 15 / Darwin 25 against a throwaway service: a 3900-byte
/// value round-trips byte-identical, a 4100-byte value does NOT. Past the
/// ceiling `security` does not refuse — it splits the line, executes the head
/// with a TRUNCATED `-w` value, and reports the tail as `unknown command`. The
/// item is left holding the truncated JSON, so a write that overruns this
/// silently destroys whatever the item held.
///
/// This mattered nowhere until the mirror started preserving Claude Code's
/// sibling keys: a login-only blob is 1-2 KB and never came close, while a blob
/// carrying `mcpOAuth` for a dozen-plus OAuth MCP servers clears it easily.
const SECURITY_STDIN_LINE_MAX: usize = 4096;

/// Largest value that survives the argv transport intact, same measurement run:
/// 64 KiB round-trips, 128 KiB does not. NOT an `ARG_MAX` limit — that is 1 MiB
/// on this host — so the ceiling is `security`/Keychain's own. A blob past this
/// is refused rather than written, because every transport we have would mangle
/// it and a mangled item is a lost login plus lost MCP servers.
const SECURITY_ARGV_VALUE_MAX: usize = 64 * 1024;

/// How [`put_blob_at`] hands one write to `security`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PutTransport {
    /// `security -i`, command line over stdin. Keeps the token out of this
    /// process's argv, hence out of Endpoint Security exec logging (PR #21).
    Stdin,
    /// `security add-generic-password …` with the value as a real argv word.
    /// Gives up the EDR-log property to stay under [`SECURITY_STDIN_LINE_MAX`];
    /// argv is still same-UID-or-root only, the tradeoff PR #21 called already
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
        "refusing to write a {value_len}-byte Keychain item: past the {SECURITY_ARGV_VALUE_MAX}-byte          ceiling every `security` transport truncates instead of failing, which would destroy the          login and any MCP server logins stored beside it"
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

/// Read the JSON blob stored at `(service, account)` via
/// `security find-generic-password -w`. `Ok(None)` when the item is absent
/// (exit 44); any other failure is an error. Returns the RAW object rather than
/// a typed [`ClaudeCredentials`], which models the login alone and would drop
/// the very siblings the read exists to preserve.
fn read_blob_at(service: &str, account: &str) -> Result<Option<Value>> {
    let mut cmd = Command::new(SECURITY_BIN);
    cmd.args(["find-generic-password", "-s", service, "-a", account, "-w"]);
    let output = run_with_deadline(cmd, security_deadline(), None)
        .with_context(|| format!("failed to run {SECURITY_BIN} find-generic-password"))?;
    if output.status.success() {
        // `-w` prints only the password (our JSON) followed by a trailing newline.
        let json = String::from_utf8(output.stdout).context("Keychain password is not UTF-8")?;
        let blob =
            serde_json::from_str(json.trim_end()).context("Keychain item is not valid JSON")?;
        Ok(Some(blob))
    } else if output.status.code() == Some(EXIT_ITEM_NOT_FOUND) {
        Ok(None)
    } else {
        Err(security_error("read", &output))
    }
}

/// The item's current contents for a merge. An absent item is the first-write
/// case and merges as `None` quietly; a read that FAILS also merges as `None`,
/// but names the loss on the event line first (module doc: the write still
/// lands, because a refused switch is worse than a lost MCP login).
fn blob_to_merge_with(service: &str, account: &str) -> Option<Value> {
    match read_blob_at(service, account) {
        Ok(blob) => blob,
        Err(e) => {
            logline!(
                "clauth: could not read the macOS Keychain login before replacing it ({e:#}). \
                 The MCP server logins it held are replaced by whatever this profile last \
                 stored, which on macOS is older than the item's own set: re-authenticate any \
                 MCP server that reports a signed-out session, and any that starts failing"
            );
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

/// Add-or-update the item at `(service, account)` with `blob` as its password,
/// the whole `{"claudeAiOauth":{…}, …}` JSON object Claude Code expects, via
/// `security add-generic-password -U`. `-U` updates the item in place when it
/// already exists (created by Claude Code) and adds it otherwise. Callers go
/// through [`merge_and_put_at`] unless they have already derived the whole
/// object, as the sign-out has.
///
/// The command line is fed to `security -i` over **stdin**, not argv, so the
/// token never appears in this process's own argv — keeping it out of
/// process-exec logging (Endpoint Security `es_event_exec_t`, i.e. most EDR
/// agents), which captures full command lines at exec time but not pipe
/// contents. (Plain same-UID `ps` exposure was already an accepted tradeoff —
/// TECH-9 #17: argv is readable only by the same UID or root on macOS, and a
/// same-UID process already owns the 0o600 credential files — but the EDR log
/// store was the one residual argv-only sink, and `-i` closes it.) `-i`'s
/// tokenizer needs the [`security_quote`] escaping for values with whitespace;
/// the inner command's exit code propagates as `security -i`'s own exit code
/// (verified: 0 on success, 44 for `errSecItemNotFound`, 2 on usage error).
/// The no-value `-w` prompt form is still unusable here — it reads from the
/// controlling *tty* (`readpassphrase`), not stdin, so a pipe can't feed it.
///
/// A write is gated on the keychain being UNLOCKED, never on the target item's
/// trust list, so it lands silently against an item ACL'd to Claude Code alone
/// and raises no dialog of its own (measured on `mac-6` 2026-08-12). It also
/// does not re-ACL the item, which is why the read leg keeps needing its own
/// one-time grant.
fn put_blob_at(service: &str, account: &str, blob: &Value) -> Result<()> {
    let json = serde_json::to_string(blob).context("failed to serialize the Keychain item")?;
    let line = format!(
        "add-generic-password -U -s {} -a {} -w {}\n",
        security_quote(service)?,
        security_quote(account)?,
        security_quote(&json)?,
    );
    // Past `-i`'s line ceiling the tokenizer truncates the value instead of
    // refusing, so the transport is chosen by size, not by preference.
    let output = match put_transport(line.len(), json.len())? {
        PutTransport::Stdin => {
            let mut cmd = Command::new(SECURITY_BIN);
            cmd.arg("-i");
            run_with_deadline(cmd, security_deadline(), Some(&line))
        }
        PutTransport::Argv => {
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
    if output.status.success() {
        Ok(())
    } else {
        Err(security_error("write", &output))
    }
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
        Err(security_error("delete", &output))
    }
}

/// Build an error from a failed `security` invocation, including its stderr and
/// exit code (never the password, which travels only on the child's stdin —
/// `put_blob_at` via `security -i`, and is never echoed to stderr).
fn security_error(op: &str, output: &std::process::Output) -> anyhow::Error {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let code = output
        .status
        .code()
        .map_or_else(|| "signal".to_string(), |c| c.to_string());
    anyhow::anyhow!(
        "Keychain {op} failed (security exit {code}): {}",
        stderr.trim()
    )
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
    anyhow::ensure!(
        store.is_object(),
        "refusing to install a credential store that is not a JSON object into the Keychain"
    );
    merge_and_put_at(SERVICE, &account()?, store, Keep::CarriedOnly)
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

/// Sign Claude Code out: drop the account-scoped keys and keep what belongs to
/// no account, so a wrap-off or a forced relink onto a login-less profile stops
/// the item serving an account without taking every MCP-server login with it. An
/// item left holding nothing else is deleted outright, which keeps the
/// clean-absence state this had before the strip. Idempotent, and an absent item
/// is success.
///
/// It is DESTRUCTIVE and the operator has no other copy of what it drops, so
/// both acting branches say so on the event line: two of the callers discard the
/// result (`daemon`, `tui`), and a switch that quietly deleted a login would
/// otherwise leave no local trace of why Claude Code is logged out. Only
/// `force_link_profile_credentials` and `clear_claude_credentials` reach it,
/// never the guarded relink, so a path that never meant to change accounts
/// cannot destroy a login clauth does not hold (`claude::keychain_mirror_source`).
///
/// A read that fails deletes instead: whatever it could not preserve is worth
/// less than the item continuing to authenticate an account the operator just
/// switched away from.
pub(crate) fn keychain_sign_out() -> Result<()> {
    let account = account()?;
    // The two `None` cases part here rather than sharing an early return: an
    // absent item is already signed out and says nothing, while a read that
    // FAILED takes the most destructive branch there is, deleting the item whole
    // without having seen what it held.
    let mut blob = match read_blob_at(SERVICE, &account) {
        Ok(None) => return delete_at(SERVICE, &account),
        Ok(Some(blob)) => blob,
        Err(e) => {
            logline!(
                "clauth: signed Claude Code out of the macOS Keychain by deleting the item: it \
                 could not be read first ({e:#}), so the MCP server logins stored beside the \
                 login went with it. Re-authenticate any MCP server that reports a signed-out \
                 session"
            );
            return delete_at(SERVICE, &account);
        }
    };
    match crate::claude::strip_account_credentials(&mut blob) {
        crate::claude::SignOut::Delete => {
            logline!(
                "clauth: signed Claude Code out of the macOS Keychain (the profile now active \
                 stores no Claude login). Run `clauth <name>` to put one back"
            );
            delete_at(SERVICE, &account)
        }
        crate::claude::SignOut::Write => {
            logline!(
                "clauth: signed Claude Code out of the macOS Keychain (the profile now active \
                 stores no Claude login); its MCP server logins were kept"
            );
            put_blob_at(SERVICE, &account, &blob)
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
///
/// The suffix is the first 8 hex chars of the SHA-256 of the canonicalized
/// directory path, matching CC's `sha256(configDir).toString('hex').slice(0, 8)`.
#[allow(
    dead_code,
    reason = "caller lands with the macOS swap-executor Keychain write"
)]
pub(crate) fn keychain_service_for_config_dir(config_dir: &Path) -> Result<String> {
    // Canonicalize: CC resolves symlinks before hashing, and a relative path
    // would produce a different hash than the absolute one CC computes.
    let canonical = std::fs::canonicalize(config_dir).with_context(|| {
        format!(
            "failed to canonicalize config dir: {}",
            config_dir.display()
        )
    })?;
    let path_str = canonical.to_string_lossy();
    let hash = Sha256::digest(path_str.as_bytes());
    let suffix = format!(
        "{:02x}{:02x}{:02x}{:02x}",
        hash[0], hash[1], hash[2], hash[3]
    );
    Ok(format!("{SERVICE}-{suffix}"))
}

#[cfg(test)]
#[path = "../tests/inline/keychain.rs"]
mod tests;
