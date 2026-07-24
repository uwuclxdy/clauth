//! `~/.clauth/clauthd.sock` — the daemon's control socket.
//!
//! Newline-delimited JSON, one command per connection:
//! ```text
//! → {"cmd":"snapshot"}                        ← {"ok":true,"status": <status.json body>}
//! → {"cmd":"switch","profile":"work"}         ← {"ok":true} | {"ok":false,"error":"…"}
//! → {"cmd":"refresh","profile":"work"}        ← {"ok":true}   (profile optional = all)
//! → {"cmd":"fallback_add","profile":"work"}   ← {"ok":true}   (append to the chain)
//! → {"cmd":"fallback_remove","profile":"work"}← {"ok":true}
//! → {"cmd":"fallback_move","profile":"work","dir":"up"}   ← {"ok":true}  (dir: up|down)
//! → {"cmd":"set_threshold","profile":"work","value":90}  ← {"ok":true}  (0..=100)
//! → {"cmd":"set_last_resort","profile":"work","value":true}  ← {"ok":true}
//! → {"cmd":"set_member_weekly","profile":"work","value":90}  ← {"ok":true}  (null clears)
//! → {"cmd":"set_check_weekly","profile":"work","value":false}  ← {"ok":true}
//! → {"cmd":"set_check_scoped","profile":"work","value":false}  ← {"ok":true}
//! → {"cmd":"set_wrap_off","value":true}       ← {"ok":true}
//! → {"cmd":"set_weekly_threshold","value":98}  ← {"ok":true}  (50..=100, chain-global)
//! → {"cmd":"rename","profile":"work","new_name":"work2"} ← {"ok":true} | {"ok":false,"error":"…"}
//! ```
//! Every command only *enqueues* — `switch`/`refresh` into `pending_switch`/
//! `refetch_queue`, and the fallback-config edits into `pending_config_ops` — that
//! the main loop already drains. No mutation happens on the socket thread. So an
//! `ok` reply means "accepted"; the caller polls `status.json` to see it land.

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use serde::Deserialize;

use super::{ConfigOp, PendingConfigOps};
use crate::fallback_config::MoveDir;
use crate::logline::logline;
use crate::profile::ConfigHandle;
use crate::usage::{Origin, PendingSwitch, RefetchQueue, enqueue_pending_switch, now_ms};

/// Per-read/write wall-clock ceiling on the socket (TECH-10). `SO_RCVTIMEO` is
/// per-read, so a client that connects and stays SILENT is dropped after this;
/// one that trickles bytes without a newline is instead bounded by
/// [`MAX_COMMAND_BYTES`] below (whichever trips first). Either way a stuck peer
/// can't park its handler, and a peer that stops reading can't block our reply.
const SOCKET_IO_TIMEOUT: Duration = Duration::from_secs(2);

/// Hard cap on a single command line (TECH-10). A newline-less byte stream is
/// bounded here so a hostile/buggy client can't grow the line buffer without
/// limit and OOM the daemon; past the cap the read ends and the (unparseable)
/// partial errors out cleanly.
const MAX_COMMAND_BYTES: u64 = 64 * 1024;

/// The shared stores the socket enqueues into. Cloned `Arc`s only — no lock rank,
/// so `Clone` is cheap and per-connection handoff to a worker thread is free.
#[derive(Clone)]
pub(crate) struct SocketHandles {
    pub(crate) config: ConfigHandle,
    pub(crate) pending_switch: PendingSwitch,
    pub(crate) pending_config_ops: PendingConfigOps,
    pub(crate) refetch_queue: RefetchQueue,
    /// Pulsed after any enqueue so the main loop drains it immediately (sub-tick
    /// interactive latency) instead of on its next ~1s sleep boundary.
    pub(crate) waker: Arc<super::waker::TickWaker>,
}

#[derive(Deserialize)]
struct Command {
    cmd: String,
    #[serde(default)]
    profile: Option<String>,
    /// `fallback_move` direction: `"up"` | `"down"`.
    #[serde(default)]
    dir: Option<String>,
    /// `set_threshold` (number 0..=100) / `set_last_resort` (bool) /
    /// `set_wrap_off` (bool).
    #[serde(default)]
    value: Option<serde_json::Value>,
    /// `rename` target: the new profile name.
    #[serde(default)]
    new_name: Option<String>,
}

/// Spawn the control-socket listener thread. Best-effort: a bind failure is
/// logged and the daemon runs without a socket (status.json polling still works).
pub(crate) fn spawn(sock_path: PathBuf, status_path: PathBuf, handles: SocketHandles) {
    std::thread::Builder::new()
        .name("clauth-daemon-sock".into())
        .spawn(move || {
            if let Err(e) = serve(&sock_path, &status_path, &handles) {
                logline!("clauth daemon: control socket unavailable: {e}");
            }
        })
        .ok();
}

fn serve(sock_path: &Path, status_path: &Path, h: &SocketHandles) -> Result<()> {
    // A stale socket from a crashed daemon blocks bind; remove it first. The
    // socket is chmod'd 0o600 below and ~/.clauth is now enforced 0o700 (TECH-9
    // #13) — doubly user-private. (Any same-UID process can still connect; the
    // socket authority is same-uid by design — see SECURITY.md.)
    let _ = std::fs::remove_file(sock_path);
    let listener = UnixListener::bind(sock_path).context("failed to bind clauthd.sock")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(sock_path, std::fs::Permissions::from_mode(0o600));
    }
    for stream in listener.incoming() {
        let Ok(s) = stream else { continue };
        // Give every connection a 2s read/write deadline so a client that connects
        // and sends nothing can't park its handler, then service it on a
        // short-lived per-connection thread: a slow read no longer serializes the
        // accept loop, and a panic in handle() dies with its thread instead of
        // taking down the listener (#3/#7). Handles are Arc clones — cheap to move.
        //
        // No cap on concurrent connection threads by design: socket authority is
        // same-UID (SECURITY.md § Fork surfaces) and a same-UID process can already
        // read the tokens directly, so a thread-spawn flood is not an escalation;
        // each thread is short-lived (2s/64KiB bounded) and a spawn failure under
        // pressure is swallowed while the accept loop survives.
        let _ = s.set_read_timeout(Some(SOCKET_IO_TIMEOUT));
        let _ = s.set_write_timeout(Some(SOCKET_IO_TIMEOUT));
        let status_path = status_path.to_path_buf();
        let h = h.clone();
        std::thread::Builder::new()
            .name("clauth-sock-conn".into())
            .spawn(move || handle(s, &status_path, &h))
            .ok();
    }
    Ok(())
}

/// Read one command line, dispatch it, write the JSON response, close.
fn handle(stream: UnixStream, status_path: &Path, h: &SocketHandles) {
    let read_half = match stream.try_clone() {
        Ok(s) => s,
        Err(_) => return,
    };
    // Cap the line: a client that never sends '\n' yields at most `MAX_COMMAND_BYTES`
    // before EOF (no unbounded growth), and the stream's read timeout bounds the
    // wait for a client that sends nothing at all.
    let mut reader = BufReader::new(read_half.take(MAX_COMMAND_BYTES));
    let mut line = String::new();
    if reader.read_line(&mut line).is_err() || line.trim().is_empty() {
        return;
    }
    let response = dispatch(line.trim(), status_path, h);
    let mut w = stream;
    let _ = writeln!(w, "{response}");
}

/// Route one command to its store enqueue / file read. Pure over `(line, path,
/// handles)` so the command semantics are unit-testable without a real socket.
fn dispatch(line: &str, status_path: &Path, h: &SocketHandles) -> String {
    let cmd: Command = match serde_json::from_str(line) {
        Ok(c) => c,
        Err(e) => return err(&format!("bad command: {e}")),
    };
    match cmd.cmd.as_str() {
        "snapshot" => match std::fs::read_to_string(status_path) {
            // status.json on disk is pretty-printed (multi-line); the protocol
            // is newline-delimited, so the reply must be ONE line — embedding
            // the body verbatim hands a line-reading client truncated JSON.
            // Re-serialize compact (Value's Display), and a body that doesn't
            // parse (should never happen: writes are atomic) errors instead of
            // corrupting the reply frame.
            Ok(body) => match serde_json::from_str::<serde_json::Value>(&body) {
                Ok(status) => format!("{{\"ok\":true,\"status\":{status}}}"),
                Err(e) => err(&format!("status.json unparseable: {e}")),
            },
            Err(e) => err(&format!("no status.json yet: {e}")),
        },
        "switch" => {
            let Some(profile) = cmd.profile else {
                return err("switch requires a profile");
            };
            match resolve(h, &profile) {
                Some(name) => {
                    // AUTH-1/AUTH-2: refuse a switch to a revoked/expired login up
                    // front, so the tap gets an immediate branchable error instead
                    // of a silent enqueue-then-skip in the drain.
                    if h.config
                        .lock()
                        .map(|c| c.is_auth_broken(&name))
                        .unwrap_or(false)
                    {
                        return err_code(
                            "auth_broken",
                            &format!("login for '{name}' has expired; run: clauth login {name}"),
                        );
                    }
                    // Origin::User: this explicit tap clears any queued auto-target
                    // OF THE SAME HARNESS and outranks a same-tick scheduler switch
                    // in the drain, so the operator's choice is never silently
                    // overridden (TECH-6) — while the other slot's queued intent
                    // survives (CDX-4 §0.15).
                    let harness = h
                        .config
                        .lock()
                        .ok()
                        .and_then(|c| c.find(&name).map(|p| p.harness))
                        .unwrap_or_default();
                    if let Ok(mut q) = h.pending_switch.lock() {
                        enqueue_pending_switch(&mut q, name, harness, Origin::User, now_ms());
                    }
                    h.waker.wake(); // apply the switch this instant, not next tick
                    ok()
                }
                None => err_code("unknown_profile", &format!("unknown profile '{profile}'")),
            }
        }
        "refresh" => {
            let names = match cmd.profile {
                Some(p) => match resolve(h, &p) {
                    Some(n) => vec![n],
                    None => return err_code("unknown_profile", &format!("unknown profile '{p}'")),
                },
                None => all_names(h),
            };
            if let Ok(mut q) = h.refetch_queue.lock() {
                for n in names {
                    q.insert(n);
                }
            }
            // No wake here: the usage refetch is drained by the scheduler thread on
            // its own cadence, not by the main-loop tick — a wake would only trigger
            // a no-op status write, not an earlier fetch.
            ok()
        }
        "fallback_add" | "fallback_remove" | "fallback_move" | "set_threshold"
        | "set_last_resort" | "set_member_weekly" | "set_check_weekly" | "set_check_scoped" => {
            // Resolve + validate the profile up front so an unknown name errors on
            // the socket rather than silently no-op'ing in the drain.
            let Some(raw) = cmd.profile.as_deref() else {
                return err(&format!("{} requires a profile", cmd.cmd));
            };
            let Some(name) = resolve(h, raw) else {
                return err_code("unknown_profile", &format!("unknown profile '{raw}'"));
            };
            let op = match cmd.cmd.as_str() {
                "fallback_add" => ConfigOp::FallbackAdd(name),
                "fallback_remove" => ConfigOp::FallbackRemove(name),
                "fallback_move" => match cmd.dir.as_deref().and_then(MoveDir::parse) {
                    Some(dir) => ConfigOp::FallbackMove(name, dir),
                    None => return err("fallback_move requires dir: \"up\" or \"down\""),
                },
                "set_threshold" => match cmd.value.as_ref().and_then(serde_json::Value::as_f64) {
                    Some(v) if (0.0..=100.0).contains(&v) => ConfigOp::SetThreshold(name, v),
                    _ => return err("set_threshold requires a numeric value within 0..=100"),
                },
                "set_last_resort" => {
                    match cmd.value.as_ref().and_then(serde_json::Value::as_bool) {
                        Some(on) => ConfigOp::SetLastResort(name, on),
                        None => return err("set_last_resort requires a boolean value"),
                    }
                }
                // Per-account weekly-line override: a number sets it, an
                // explicit null (or absent value) clears back to the
                // chain-wide default.
                "set_member_weekly" => match cmd.value.as_ref() {
                    None | Some(serde_json::Value::Null) => ConfigOp::SetMemberWeekly(name, None),
                    Some(v) => match v.as_f64() {
                        Some(v) if (0.0..=100.0).contains(&v) => {
                            ConfigOp::SetMemberWeekly(name, Some(v))
                        }
                        _ => {
                            return err(
                                "set_member_weekly requires a numeric value within 0..=100, or null to clear",
                            );
                        }
                    },
                },
                "set_check_weekly" | "set_check_scoped" => {
                    let scoped = cmd.cmd == "set_check_scoped";
                    match cmd.value.as_ref().and_then(serde_json::Value::as_bool) {
                        Some(on) => ConfigOp::SetUsageGate(name, scoped, on),
                        None => return err(&format!("{} requires a boolean value", cmd.cmd)),
                    }
                }
                _ => unreachable!("outer match limits these arms"),
            };
            enqueue_config(h, op);
            ok()
        }
        "set_wrap_off" => match cmd.value.as_ref().and_then(serde_json::Value::as_bool) {
            Some(on) => {
                enqueue_config(h, ConfigOp::SetWrapOff(on));
                ok()
            }
            None => err("set_wrap_off requires a boolean value"),
        },
        "set_weekly_threshold" => match cmd.value.as_ref().and_then(serde_json::Value::as_f64) {
            // Same band the write-side op enforces — reject on the socket so a
            // bad value errors at the caller instead of a silent drain failure.
            Some(v)
                if (crate::profile::MIN_WEEKLY_SWITCH_PCT
                    ..=crate::profile::MAX_WEEKLY_SWITCH_PCT)
                    .contains(&v) =>
            {
                enqueue_config(h, ConfigOp::SetWeeklyThreshold(v));
                ok()
            }
            _ => err("set_weekly_threshold requires a numeric value within 50..=100"),
        },
        "rename" => {
            let Some(raw) = cmd.profile.as_deref() else {
                return err("rename requires a profile");
            };
            let Some(old) = resolve(h, raw) else {
                return err_code("unknown_profile", &format!("unknown profile '{raw}'"));
            };
            let Some(new_name) = cmd.new_name.as_deref() else {
                return err("rename requires new_name");
            };
            // Validate charset + collision synchronously so a taken/invalid name errors
            // on the socket instead of a silent drain failure (matches set_threshold's
            // up-front range check). Exclude `old` so a case-only self-rename is allowed.
            let names: Vec<String> = match h.config.lock() {
                Ok(c) => c.names().iter().map(|s| s.to_string()).collect(),
                Err(_) => return err("config unavailable"),
            };
            let existing: Vec<&str> = names.iter().map(String::as_str).collect();
            if let Err(e) =
                crate::actions::validate_profile_name(new_name, &existing, Some(old.as_str()))
            {
                return err(&format!("{e}"));
            }
            enqueue_config(h, ConfigOp::Rename(old, new_name.trim().to_string()));
            ok()
        }
        other => err(&format!("unknown cmd '{other}'")),
    }
}

/// Push a validated config edit onto the queue the main loop drains. A poisoned
/// lock drops the edit rather than panicking the socket thread — the caller sees
/// no change land in `status.json` and can retry.
fn enqueue_config(h: &SocketHandles, op: ConfigOp) {
    if let Ok(mut q) = h.pending_config_ops.lock() {
        q.push(op);
    }
    // Wake the loop so the edit applies this instant, not on the next ~1s tick.
    h.waker.wake();
}

/// Case-insensitively resolve a raw profile name to its canonical form, or `None`.
fn resolve(h: &SocketHandles, profile: &str) -> Option<String> {
    h.config.lock().ok().and_then(|c| c.canonical_name(profile))
}

/// Every profile name — the `refresh`-all set. A credential-less name enqueued
/// here is a harmless no-op: the scheduler's `merge_forced` only fetches forced
/// names that appear in a fetch snapshot, so it silently ignores the rest.
fn all_names(h: &SocketHandles) -> Vec<String> {
    h.config
        .lock()
        .ok()
        .map(|c| {
            c.profiles
                .iter()
                .map(|p| p.name.as_str().to_string())
                .collect()
        })
        .unwrap_or_default()
}

fn ok() -> String {
    "{\"ok\":true}".to_string()
}

fn err(msg: &str) -> String {
    err_code("invalid_value", msg)
}

/// AUTH-2: an `ok:false` reply carrying a stable machine-branchable `error_code`
/// alongside the human-readable `error` (Swift branches on the code; the prose
/// stays for humans). Codes: `unknown_profile` (name did not resolve), `busy`
/// (target mid-fetch/rotation — reserved for a future synchronous check; the
/// drain currently retries), `auth_broken` (target's login is revoked/expired —
/// run `clauth login`), `invalid_value` (malformed command or out-of-range arg).
fn err_code(code: &str, msg: &str) -> String {
    serde_json::json!({ "ok": false, "error": msg, "error_code": code }).to_string()
}

#[cfg(test)]
#[path = "../../tests/inline/daemon_socket.rs"]
mod tests;
