//! The 5h usage bell: one level-crossing latch behind both sinks, the TUI's
//! toast and the daemon's `bell_command`.
//!
//! A reading rings once when its 5h utilization reaches the account's
//! `bell_threshold`, stays silent while it holds there, and re-arms once it
//! falls back below. Only a `Fresh` reading moves the latch: a stale or
//! synthetic figure (a just-kicked 0%, an outage's frozen number) neither rings
//! nor clears a standing bell.

use std::collections::HashSet;

use anyhow::{Context, Result, bail};

/// Which accounts have already rung for their current crossing. In-process on
/// purpose: a restart above the line rings once more, where a persisted latch
/// would swallow the notification after a crash.
#[derive(Debug, Default)]
pub(crate) struct BellLatch {
    ringing: HashSet<String>,
}

impl BellLatch {
    /// Feed one reading. `Some(message)` when this reading rings.
    pub(crate) fn observe(
        &mut self,
        name: &str,
        threshold: Option<f64>,
        util: Option<f64>,
        fresh: bool,
    ) -> Option<String> {
        let (Some(threshold), Some(util)) = (threshold, util) else {
            return None;
        };
        if !fresh {
            return None;
        }
        if util < threshold {
            self.ringing.remove(name);
            return None;
        }
        self.ringing
            .insert(name.to_string())
            .then(|| message(name, util))
    }

    /// Whether `name` rang for a crossing it has not yet fallen back from.
    pub(crate) fn is_ringing(&self, name: &str) -> bool {
        self.ringing.contains(name)
    }
}

/// The alert's text, shared by the toast and `bell_command`'s `%s`.
pub(crate) fn message(name: &str, util: f64) -> String {
    format!("alert: {name} at {}", crate::format::format_pct(util))
}

/// Split a `bell_command` template into argv by POSIX quoting rules, then put
/// `message` in place of every `%s`. Substituting after the split keeps the
/// message one argument whatever it holds, so no character in an account name
/// can add an argument.
pub(crate) fn command_argv(template: &str, message: &str) -> Result<Vec<String>> {
    let Some(words) = shlex::split(template) else {
        // The template stays out of the error: it lands in daemon.log, and a
        // webhook template can carry a token.
        bail!("bell_command has an unbalanced quote or a trailing backslash");
    };
    if words.is_empty() {
        bail!("bell_command is empty");
    }
    Ok(words.iter().map(|w| w.replace("%s", message)).collect())
}

/// Run `template` with `message` substituted, without a shell. The child is
/// reaped on a detached thread: waiting inline would let a hung notifier stall
/// the daemon tick into its watchdog, and not waiting would leave a zombie per
/// bell.
pub(crate) fn run_command(template: &str, message: &str) -> Result<()> {
    let argv = command_argv(template, message)?;
    let (program, args) = argv.split_first().context("bell_command is empty")?;
    let mut command = std::process::Command::new(program);
    command
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // A daemon started without a console (a scheduled task, a service) would
        // otherwise give each console notifier a visible window of its own.
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    let mut child = command
        .spawn()
        .with_context(|| format!("starting bell_command program {program:?}"))?;
    std::thread::Builder::new()
        .name("bell-reap".to_string())
        .spawn(move || {
            let _ = child.wait();
        })
        .context("starting the bell_command reaper thread")?;
    Ok(())
}

#[cfg(test)]
#[path = "../tests/inline/bell.rs"]
mod tests;
