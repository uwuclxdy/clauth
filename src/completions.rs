use std::fs;

use anyhow::{Context, Result, bail};

use crate::out::{errln, out, outln};
use crate::profile::{home_dir, load_config};

const BASH: &str = r#"_clauth() {
    local cur prev
    cur="${COMP_WORDS[COMP_CWORD]}"
    prev="${COMP_WORDS[COMP_CWORD-1]}"
    if [ "$COMP_CWORD" -eq 1 ]; then
        local profiles
        profiles=$(clauth __complete 2>/dev/null)
        COMPREPLY=( $(compgen -W "${profiles} start login delete disable enable which list sessions resume info daemon status mcp completions --theme" -- "${cur}") )
    elif [ "$prev" = "--theme" ]; then
        COMPREPLY=( $(compgen -W "full compatible" -- "${cur}") )
    elif [ "${COMP_WORDS[1]}" = "login" ] && [ "${cur:0:2}" = "--" ]; then
        COMPREPLY=( $(compgen -W "--base-url --api-key --setup-token --yes -y --model" -- "${cur}") )
    elif [ "${COMP_WORDS[1]}" = "start" ] && [ "${cur:0:2}" = "--" ]; then
        COMPREPLY=( $(compgen -W "--isolated --rescue --no-rescue --with-fallback" -- "${cur}") )
    elif [ "${COMP_WORDS[1]}" = "daemon" ] && [ "${cur:0:2}" = "--" ]; then
        COMPREPLY=( $(compgen -W "--standby --no-standby --replace --status --listen --print-token --rotate-token" -- "${cur}") )
    elif [ "$prev" = "--isolated" ] || [ "$prev" = "--with-fallback" ] || [ "$prev" = "--profile" ]; then
        local profiles
        profiles=$(clauth __complete 2>/dev/null)
        COMPREPLY=( $(compgen -W "${profiles}" -- "${cur}") )
    elif [ "$COMP_CWORD" -eq 2 ] && { [ "$prev" = "start" ] || [ "$prev" = "login" ] || [ "$prev" = "delete" ] || [ "$prev" = "disable" ] || [ "$prev" = "enable" ]; }; then
        local profiles
        profiles=$(clauth __complete 2>/dev/null)
        COMPREPLY=( $(compgen -W "${profiles}" -- "${cur}") )
    elif [ "$COMP_CWORD" -eq 2 ] && [ "$prev" = "which" ]; then
        COMPREPLY=( $(compgen -W "--json" -- "${cur}") )
    elif [ "$COMP_CWORD" -eq 2 ] && [ "$prev" = "sessions" ]; then
        COMPREPLY=( $(compgen -W "--json --tokens" -- "${cur}") )
    elif [ "${COMP_WORDS[1]}" = "resume" ] && [ "${cur:0:2}" = "--" ]; then
        COMPREPLY=( $(compgen -W "--profile" -- "${cur}") )
    elif [ "${COMP_WORDS[1]}" = "delete" ] && [ "${cur:0:2}" = "--" ]; then
        COMPREPLY=( $(compgen -W "--yes -y --force" -- "${cur}") )
    elif [ "${COMP_WORDS[1]}" = "disable" ] && [ "${cur:0:2}" = "--" ]; then
        COMPREPLY=( $(compgen -W "--yes -y" -- "${cur}") )
    elif [ "${COMP_WORDS[1]}" = "status" ] && [ "${cur:0:2}" = "--" ]; then
        COMPREPLY=( $(compgen -W "--json --all --disabled" -- "${cur}") )
    elif [ "${COMP_WORDS[1]}" = "list" ] && [ "${cur:0:2}" = "--" ]; then
        COMPREPLY=( $(compgen -W "--all --disabled" -- "${cur}") )
    fi
    return 0
}
complete -F _clauth clauth
"#;

const ZSH: &str = r#"#compdef clauth
_clauth() {
    if (( CURRENT == 2 )); then
        local -a profiles
        profiles=("${(@f)$(clauth __complete 2>/dev/null)}")
        _describe 'profile' profiles
        _values 'subcommand' \
            'start[launch claude with that profile]' \
            'login[log in via browser OAuth or an API key]' \
            'delete[remove a profile and its credentials]' \
            'disable[hide a profile from auto-switch and usage polling]' \
            'enable[restore a disabled profile]' \
            'which[print profile owning the loaded credentials]' \
            'list[list accounts as a table with per-profile usage]' \
            'sessions[list Claude Code sessions (add --json / --tokens)]' \
            'resume[resume a session under a chosen profile]' \
            'info[print resume command + storage path for a session]' \
            'daemon[run the headless scheduler with no TUI]' \
            'status[print the usage / auto-switch snapshot as JSON]' \
            'mcp[run the stdio MCP server]' \
            'completions[emit shell completion script]'
        _values 'option' '--theme[force a color depth instead of auto-detecting]'
    elif (( CURRENT >= 3 )) && [[ "${words[CURRENT-1]}" == "--theme" ]]; then
        _values 'tier' 'full[24-bit truecolor]' 'compatible[xterm-256 palette, safe on every terminal]'
    elif (( CURRENT == 3 )) && [[ "${words[2]}" == (start|login|delete|disable|enable) ]]; then
        local -a profiles
        profiles=("${(@f)$(clauth __complete 2>/dev/null)}")
        _describe 'profile' profiles
        [[ "${words[2]}" == start ]] && _values 'flag' '--isolated[clean isolated runtime; drops operator config]' \
            '--rescue[isolated only: lift transcripts + sidecars into the global store]' \
            '--no-rescue[isolated only: discard the isolated store]' \
            '--with-fallback[follow the fallback chain; needs a running daemon]'
    elif (( CURRENT == 4 )) && [[ "${words[2]}" == start && "${words[3]}" == (--isolated|--with-fallback) ]]; then
        local -a profiles
        profiles=("${(@f)$(clauth __complete 2>/dev/null)}")
        _describe 'profile' profiles
    elif (( CURRENT == 4 )) && [[ "${words[2]}" == resume && "${words[3]}" == --profile ]]; then
        local -a profiles
        profiles=("${(@f)$(clauth __complete 2>/dev/null)}")
        _describe 'profile' profiles
    elif (( CURRENT == 3 )) && [[ "${words[2]}" == which ]]; then
        _values 'flag' '--json[emit JSON instead of plain name]'
    elif (( CURRENT == 3 )) && [[ "${words[2]}" == sessions ]]; then
        _values 'flag' '--json[emit the stable machine-readable array]' \
            '--tokens[add token totals + cost; reads every transcript in full]'
    elif (( CURRENT >= 3 )) && [[ "${words[2]}" == resume ]]; then
        _values 'flag' '--profile[resume under this profile instead of prompting]'
    elif (( CURRENT >= 4 )) && [[ "${words[2]}" == login ]]; then
        _values 'flag' '--base-url[API base url]' '--api-key[API key (prompted echo-off if omitted)]' '--setup-token[capture a claude setup-token mint as a long-lived login]' '--yes[replace an existing long-lived token unprompted]' '-y[replace an existing long-lived token unprompted]' '--model[set the default model before signing in]'
    elif (( CURRENT >= 4 )) && [[ "${words[2]}" == delete ]]; then
        _values 'flag' '--yes[skip the confirm prompt]' '-y[skip the confirm prompt]' '--force[override the live-session guard]'
    elif (( CURRENT >= 4 )) && [[ "${words[2]}" == disable ]]; then
        _values 'flag' '--yes[skip the confirm prompt]' '-y[skip the confirm prompt]'
    elif (( CURRENT >= 3 )) && [[ "${words[2]}" == daemon ]]; then
        _values 'flag' \
            '--standby[wait and take over when the running daemon exits]' \
            '--no-standby[explicit spelling of the default]' \
            '--replace[terminate the running daemon and take over]' \
            '--status[print the running daemon, or exit 1 when none is]' \
            '--listen[also serve the REST API over TLS, default 0.0.0.0:8443]' \
            '--print-token[print the REST API auth token and exit]' \
            '--rotate-token[replace the REST API auth token and exit]'
    elif (( CURRENT >= 3 )) && [[ "${words[2]}" == status ]]; then
        _values 'flag' '--json[print the status snapshot as JSON]' '--all[also list disabled profiles]' '--disabled[also list disabled profiles]'
    elif (( CURRENT >= 3 )) && [[ "${words[2]}" == list ]]; then
        _values 'flag' '--all[also list disabled profiles]' '--disabled[also list disabled profiles]'
    fi
}
_clauth "$@"
"#;

const FISH: &str = r#"function __clauth_profiles
    clauth __complete 2>/dev/null
end
complete -c clauth -f
complete -c clauth -f -n __fish_is_first_token -a "(__clauth_profiles)" -d Profile
complete -c clauth -f -n __fish_is_first_token -a start -d "Launch claude with that profile's runtime"
complete -c clauth -f -n __fish_is_first_token -a login -d "Log in via browser OAuth or an API key"
complete -c clauth -f -n __fish_is_first_token -a delete -d "Remove a profile and its credentials"
complete -c clauth -f -n __fish_is_first_token -a disable -d "Hide a profile from auto-switch and usage polling"
complete -c clauth -f -n __fish_is_first_token -a enable -d "Restore a disabled profile"
complete -c clauth -f -n __fish_is_first_token -a which -d "Print profile owning the loaded credentials"
complete -c clauth -f -n __fish_is_first_token -a list -d "List accounts as a table with per-profile usage"
complete -c clauth -f -n __fish_is_first_token -a sessions -d "List Claude Code sessions"
complete -c clauth -f -n __fish_is_first_token -a resume -d "Resume a session under a chosen profile"
complete -c clauth -f -n __fish_is_first_token -a info -d "Print resume command + storage path"
complete -c clauth -f -n __fish_is_first_token -a completions -d "Emit shell completion script"
complete -c clauth -f -n __fish_is_first_token -a daemon -d "Run the headless scheduler with no TUI"
complete -c clauth -f -n __fish_is_first_token -a status -d "Print the usage / auto-switch snapshot as JSON"
complete -c clauth -f -n __fish_is_first_token -a mcp -d "Run the stdio MCP server"
complete -c clauth -f -n __fish_is_first_token -a --theme -d "Force a color depth instead of auto-detecting"
complete -c clauth -f -n 'set -l t (commandline -opc); and test "$t[-1]" = "--theme"' -a "full compatible"
complete -c clauth -f -n "__fish_seen_subcommand_from start login delete disable enable" -a "(__clauth_profiles)" -d Profile
complete -c clauth -f -n "__fish_seen_subcommand_from start" -a --isolated -d "Clean isolated runtime; drops operator config"
complete -c clauth -f -n "__fish_seen_subcommand_from start" -a --rescue -d "Isolated only: lift transcripts + sidecars into the global store"
complete -c clauth -f -n "__fish_seen_subcommand_from start" -a --no-rescue -d "Isolated only: discard the isolated store"
complete -c clauth -f -n "__fish_seen_subcommand_from start" -a --with-fallback -d "Follow the fallback chain; needs a running daemon"
complete -c clauth -f -n "__fish_seen_subcommand_from which" -a --json -d "Emit JSON"
complete -c clauth -f -n "__fish_seen_subcommand_from sessions" -a --json -d "Emit the stable machine-readable array"
complete -c clauth -f -n "__fish_seen_subcommand_from sessions" -a --tokens -d "Add token totals + cost; reads every transcript in full"
complete -c clauth -f -n "__fish_seen_subcommand_from resume" -a --profile -d "Resume under this profile instead of prompting"
complete -c clauth -f -n "__fish_seen_subcommand_from login" -a --base-url -d "API base url"
complete -c clauth -f -n "__fish_seen_subcommand_from login" -a --api-key -d "API key (prompted echo-off if omitted)"
complete -c clauth -f -n "__fish_seen_subcommand_from login" -a --setup-token -d "Capture a claude setup-token mint as a long-lived login"
complete -c clauth -f -n "__fish_seen_subcommand_from login" -a --yes -d "Replace an existing long-lived token unprompted"
complete -c clauth -f -n "__fish_seen_subcommand_from login" -a -y -d "Replace an existing long-lived token unprompted"
complete -c clauth -f -n "__fish_seen_subcommand_from login" -a --model -d "Set default model before signing in"
complete -c clauth -f -n "__fish_seen_subcommand_from delete" -a --yes -d "Skip the confirm prompt"
complete -c clauth -f -n "__fish_seen_subcommand_from delete" -a -y -d "Skip the confirm prompt"
complete -c clauth -f -n "__fish_seen_subcommand_from delete" -a --force -d "Override the live-session guard"
complete -c clauth -f -n "__fish_seen_subcommand_from disable" -a --yes -d "Skip the confirm prompt"
complete -c clauth -f -n "__fish_seen_subcommand_from disable" -a -y -d "Skip the confirm prompt"
complete -c clauth -f -n "__fish_seen_subcommand_from status" -a --json -d "Print the status snapshot as JSON"
complete -c clauth -f -n "__fish_seen_subcommand_from status" -a --all -d "Also list disabled profiles"
complete -c clauth -f -n "__fish_seen_subcommand_from status" -a --disabled -d "Also list disabled profiles"
complete -c clauth -f -n "__fish_seen_subcommand_from list" -a --all -d "Also list disabled profiles"
complete -c clauth -f -n "__fish_seen_subcommand_from list" -a --disabled -d "Also list disabled profiles"
complete -c clauth -f -n "__fish_seen_subcommand_from daemon" -a --standby -d "Wait and take over when the running daemon exits"
complete -c clauth -f -n "__fish_seen_subcommand_from daemon" -a --no-standby -d "Explicit spelling of the default"
complete -c clauth -f -n "__fish_seen_subcommand_from daemon" -a --replace -d "Terminate the running daemon and take over"
complete -c clauth -f -n "__fish_seen_subcommand_from daemon" -a --status -d "Print the running daemon, or exit 1 when none is"
complete -c clauth -f -n "__fish_seen_subcommand_from daemon" -a --listen -d "Also serve the REST API over TLS, default 0.0.0.0:8443"
complete -c clauth -f -n "__fish_seen_subcommand_from daemon" -a --print-token -d "Print the REST API auth token and exit"
complete -c clauth -f -n "__fish_seen_subcommand_from daemon" -a --rotate-token -d "Replace the REST API auth token and exit"
"#;

pub(crate) fn print_script(shell: &str) -> Result<()> {
    let script = match shell {
        "bash" => BASH,
        "zsh" => ZSH,
        "fish" => FISH,
        _ => bail!("unsupported shell '{shell}', expected: bash, zsh, fish"),
    };
    out!("{script}");
    Ok(())
}

pub(crate) fn print_profile_names() {
    let Ok(config) = load_config() else {
        return;
    };
    for name in config.names() {
        outln!("{name}");
    }
}

pub(crate) fn install(shell: Option<&str>) -> Result<()> {
    let shell = match shell {
        Some(s) => s.to_string(),
        None => detect_shell()?,
    };

    match shell.as_str() {
        "bash" => install_rc("bash", BASH, ".bashrc"),
        "zsh" => install_rc("zsh", ZSH, ".zshrc"),
        "fish" => install_fish(),
        s => bail!("unsupported shell '{s}', expected: bash, zsh, fish"),
    }
}

fn detect_shell() -> Result<String> {
    let path = std::env::var("SHELL").context(
        "$SHELL not set; pass the shell explicitly: clauth completions install <bash|zsh|fish>",
    )?;
    let name = path.rsplit('/').next().unwrap_or("");
    match name {
        "bash" | "zsh" | "fish" => Ok(name.to_string()),
        other => bail!(
            "unrecognized shell '{other}' from $SHELL; pass it explicitly: clauth completions install <bash|zsh|fish>"
        ),
    }
}

fn install_rc(shell: &str, script: &str, rc_name: &str) -> Result<()> {
    let home = home_dir()?;
    let completions_dir = home.join(".clauth").join("completions");
    crate::profile::mkdir_700(&completions_dir)?;
    let script_path = completions_dir.join(format!("clauth.{shell}"));
    crate::profile::atomic_write_600(&script_path, script)
        .with_context(|| format!("failed to write {}", script_path.display()))?;

    let rc_path = home.join(rc_name);
    let source_line = format!("source \"{}\"", script_path.display());

    let existing = fs::read_to_string(&rc_path).unwrap_or_default();
    let already = existing.lines().any(|l| l.trim() == source_line);

    if !already {
        let mut new = existing;
        if !new.is_empty() && !new.ends_with('\n') {
            new.push('\n');
        }
        new.push_str(&format!("\n# clauth completions\n{source_line}\n"));
        fs::write(&rc_path, new)
            .with_context(|| format!("failed to update {}", rc_path.display()))?;
    }

    Ok(())
}

/// Env var: set to `1` to skip the first-launch completions auto-install
/// entirely (only `"1"` opts out, matching `CLAUTH_NO_UPDATE`).
const NO_COMPLETIONS_ENV: &str = "CLAUTH_NO_COMPLETIONS";

fn completions_opt_out() -> bool {
    std::env::var(NO_COMPLETIONS_ENV).as_deref() == Ok("1")
}

/// Outcome of asking the user whether to install completions.
enum Consent {
    /// Install — explicit yes, or the default-Yes empty answer.
    Yes,
    /// User declined — record it so we never ask again.
    No,
    /// Couldn't ask (not a TTY): skip WITHOUT recording, so the next
    /// interactive launch still gets to ask. Never edits an rc unattended.
    CannotAsk,
}

/// Parse a `[Y/n]` answer with a default-Yes policy: empty, `y`, or `yes`
/// (case-insensitive) install; anything else declines.
fn answer_is_yes(input: &str) -> bool {
    let a = input.trim();
    a.is_empty() || a.eq_ignore_ascii_case("y") || a.eq_ignore_ascii_case("yes")
}

/// Ask, on a TTY, before appending a completions `source` line to `~/{rc_name}`.
/// Returns `CannotAsk` (never `Yes`) when stdin/stdout isn't a terminal, so a
/// shell rc is never edited non-interactively.
fn ask_install_completions(rc_name: &str) -> Consent {
    use std::io::IsTerminal as _;
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        return Consent::CannotAsk;
    }
    out!("clauth: install shell completions? appends a source line to ~/{rc_name} [Y/n] ");
    let mut line = String::new();
    if std::io::stdin().read_line(&mut line).is_err() {
        return Consent::CannotAsk;
    }
    if answer_is_yes(&line) {
        Consent::Yes
    } else {
        Consent::No
    }
}

pub(crate) fn auto_install_once() {
    if completions_opt_out() {
        return;
    }
    let Ok(home) = home_dir() else { return };
    let clauth_dir = home.join(".clauth");
    let sentinel = clauth_dir.join(".completions_installed");
    if sentinel.exists() {
        return;
    }

    let Ok(shell) = detect_shell() else {
        return;
    };

    // bash/zsh append a `source` line to a shell rc → require explicit consent.
    // fish writes only into its own completions dir (the conventional location),
    // so it installs without a prompt.
    let consent = match shell.as_str() {
        "bash" => ask_install_completions(".bashrc"),
        "zsh" => ask_install_completions(".zshrc"),
        _ => Consent::Yes,
    };

    if matches!(consent, Consent::CannotAsk) {
        return; // don't record the sentinel — re-prompt on the next interactive launch
    }

    let _ = crate::profile::mkdir_700(&clauth_dir);
    let _ = crate::profile::atomic_write_600(&sentinel, "");

    if matches!(consent, Consent::Yes)
        && let Err(e) = install(Some(&shell))
    {
        errln!("clauth: could not install completions: {e}");
        errln!("clauth: run `clauth completions install` later to retry");
    }
}

fn install_fish() -> Result<()> {
    let home = home_dir()?;
    let dir = home.join(".config").join("fish").join("completions");
    fs::create_dir_all(&dir)?;
    let path = dir.join("clauth.fish");
    fs::write(&path, FISH).with_context(|| format!("failed to write {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
#[path = "../tests/inline/completions.rs"]
mod tests;
