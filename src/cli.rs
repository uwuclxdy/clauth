//! `clauth`'s command grammar as clap derive types. The doc comments here ARE
//! the help copy: clap maps a comment's first paragraph to `-h` and the whole
//! comment to `--help`, so each command's prose lives beside the variant it
//! documents and the root stays a two-column table.
//!
//! Three shapes are not plain subcommands and are load-bearing: a bare
//! `clauth` launches the TUI ([`Cli::command`] is `None`), a bare unrecognized
//! word switches to the profile of that name ([`Command::External`]), and
//! `clauth start <profile> <claude args…>` forwards every token `start` does
//! not declare to `claude` untouched, leading hyphens included.

use clap::{Args, Parser, Subcommand, ValueEnum};

use crate::runtime::Isolation;

#[derive(Parser, Debug)]
#[command(
    name = "clauth",
    version,
    about = "launcher and account manager for claude code",
    after_help = "With no command, clauth launches the TUI; `clauth <profile>` switches to that account and exits. \
                  The color depth can also be pinned in ~/.clauth/profiles.toml with `theme = \"full\"`."
)]
pub(crate) struct Cli {
    /// Force a color depth instead of auto-detecting one (TUI only).
    ///
    /// `display_order` keeps the propagated copy at the bottom of every
    /// subcommand's option list instead of clap's default slot near the top,
    /// where a TUI-only flag reads as one of that command's own.
    #[arg(long, global = true, value_name = "TIER", display_order = 900)]
    pub(crate) theme: Option<ThemeArg>,

    #[command(subcommand)]
    pub(crate) command: Option<Command>,
}

/// `--theme`'s two tiers. Auto-detection (`$COLORTERM`) picks one when the flag
/// and the config-file key are both absent.
#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
pub(crate) enum ThemeArg {
    /// 24-bit truecolor. Auto-detected when $COLORTERM is truecolor or 24bit.
    Full,
    /// The xterm-256 palette, safe on every terminal.
    Compatible,
}

#[derive(Subcommand, Debug)]
pub(crate) enum Command {
    /// Launch claude under a profile, in a per-profile CLAUDE_CONFIG_DIR
    ///
    /// Args clauth does not recognize go to `claude` untouched, leading hyphens
    /// included, so `clauth start acme -p "hi"` reaches claude with its own
    /// `-p`. Put clauth's own flags before the profile name; to send `claude` a
    /// spelling clauth shares (`--help`), separate it with `--`.
    Start(StartArgs),

    /// Add a new account, or re-authenticate an existing one in place
    ///
    /// Neither switches to it. Bare (no --base-url/--api-key) runs the browser
    /// OAuth flow and writes the minted tokens into the profile; passing either
    /// endpoint flag captures an API-key account instead, prompting for
    /// whatever a flag omitted (the key is read echo-off).
    ///
    /// An existing name re-authenticates in place: the fresh credential set
    /// replaces the old one while the profile's chain slot, env, and model
    /// settings survive.
    Login(LoginArgs),

    /// Remove a profile and all its credentials
    Delete {
        /// Profile to delete.
        profile: String,
        /// Skip the confirm prompt. Required on a non-TTY stdin, which gets no
        /// implicit yes for an irreversible delete.
        #[arg(long, short = 'y')]
        yes: bool,
        /// Delete even while a live `clauth start` session holds the profile.
        /// Independent of --yes, which does not override this guard.
        #[arg(long)]
        force: bool,
    },

    /// Hide a profile from auto-switch, usage polling, and the status feed
    ///
    /// Its dir and credentials stay on disk untouched. Refused for the active
    /// profile, or one holding a live session.
    Disable {
        /// Profile to disable.
        profile: String,
        /// Skip the confirm prompt. Required on a non-TTY stdin.
        #[arg(long, short = 'y')]
        yes: bool,
    },

    /// Restore a disabled profile to every operational surface
    Enable {
        /// Profile to re-enable.
        profile: String,
    },

    /// Print the profile owning the loaded .credentials.json
    ///
    /// CLAUDE_CONFIG_DIR-aware; prints `unknown` when nothing matches.
    Which {
        /// Emit JSON instead of the plain name.
        #[arg(long)]
        json: bool,
    },

    /// List Claude Code sessions as a table
    ///
    /// Exits 0 on success, 2 on a usage error, 1 on any other failure.
    Sessions {
        /// Emit a stable newest-first JSON array instead of the table.
        #[arg(long)]
        json: bool,
    },

    /// Resume a session under a chosen profile
    ///
    /// Prompts on a TTY, defaulting to the session's last-ran profile (the
    /// active profile when that is unknown). A KNOWN codex profile name
    /// instead switches the codex login and runs `codex resume --last` — the
    /// latest conversation continues under the new account (CDX-1c
    /// carryover).
    Resume {
        /// Session id, `latest`, or a codex profile name.
        target: String,
        /// Resume under this profile instead of prompting.
        #[arg(long, value_name = "NAME")]
        profile: Option<String>,
    },

    /// Print the resume command, workspace, and storage path for a session
    ///
    /// Never launches anything.
    Info {
        /// Session id, or `latest`.
        target: String,
    },

    /// Run the headless scheduler with no TUI
    ///
    /// Refreshes usage, auto-switches on exhaustion, and writes
    /// ~/.clauth/status.json. Exits at once when a daemon is already running.
    Daemon {
        /// Wait instead, and take over when the running daemon exits. For a
        /// launchd/systemd unit paired with a manual run.
        #[arg(long, conflicts_with_all = ["no_standby", "replace", "status"])]
        standby: bool,
        /// The default's explicit spelling, kept so a spawner or unit still
        /// passing it behaves unchanged.
        #[arg(long, conflicts_with_all = ["replace", "status"])]
        no_standby: bool,
        /// Terminate the running daemon and take over, for an in-place upgrade.
        #[arg(long, conflicts_with = "status")]
        replace: bool,
        /// Print the running daemon, or exit 1 with no output when none is.
        #[arg(long)]
        status: bool,
    },

    /// Print the usage / auto-switch snapshot as JSON
    ///
    /// The same shape the daemon writes to ~/.clauth/status.json.
    Status {
        /// Required — status has no other output mode.
        #[arg(long, required = true)]
        json: bool,
        /// Also list disabled profiles, hidden by default.
        #[arg(long)]
        all: bool,
        /// Alias for --all.
        #[arg(long)]
        disabled: bool,
    },

    /// Edit the auto-switch fallback chains
    ///
    /// `clauth fallback list | add|remove|up|down <profile> | threshold
    /// <profile> <pct> | last-resort <profile> on|off`. Membership ops route by
    /// the profile's harness (claude chain vs codex chain).
    Fallback {
        /// Subcommand and its args, parsed by the fallback grammar.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        rest: Vec<String>,
    },

    /// Feed a profile's session token from its clauth-private usage chain
    ///
    /// `clauth feed <profile> on|off`. On: full-scope bearers, so plan-gated
    /// models work in sessions. Off: restore the static long-lived mint.
    Feed {
        /// `<profile> on|off`, parsed by the feed grammar.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        rest: Vec<String>,
    },

    /// Run the codex injection proxy for in-session codex account fallback
    ///
    /// Point codex at it (`clauth proxy --print-config`): a mid-conversation
    /// 429 rotates to the next chain account and replays before codex sees a
    /// byte.
    Proxy {
        /// Proxy flags (`--port N`, `--print-config`), parsed by the proxy
        /// grammar.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        rest: Vec<String>,
    },

    /// Read-only health check of the daemon + macOS wiring
    ///
    /// LaunchAgent, lock, socket, Keychain grant, version skew.
    Doctor,

    /// Run the stdio MCP server (claude code launches this)
    Mcp,

    /// Print a shell completion script, or install one
    ///
    /// `clauth completions <bash|zsh|fish>` prints the script to stdout.
    /// `clauth completions install [shell]` writes it and wires it into the
    /// user's shell rc, detecting the shell from $SHELL when omitted.
    Completions {
        /// `bash`, `zsh`, `fish`, or `install`.
        #[arg(value_name = "SHELL|install")]
        target: String,
        /// With `install` only: which shell to install for.
        shell: Option<String>,
    },

    /// Print one profile name per line, for the shell completion scripts.
    #[command(name = "__complete", hide = true)]
    Complete,

    /// CC's `apiKeyHelper` body for an api-key profile: print the profile's
    /// stored key to stdout so the runtime settings.json never holds it.
    #[command(name = "__api-key", hide = true)]
    ApiKey {
        /// Profile whose key to mint.
        profile: String,
    },

    /// The bundled PostToolUse `asyncRewake` hook body: read the hook payload
    /// on stdin, wait for a background delegate, and wake the model.
    #[command(hide = true)]
    McpAwaitJob,

    /// Not a command. Kept only to redirect anyone who guesses it.
    #[command(hide = true)]
    Run {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        rest: Vec<String>,
    },

    /// A bare word is a profile name: switch to it and exit. Declared last so
    /// every real subcommand above shadows a same-named profile, which is the
    /// precedence the hand-rolled dispatcher had.
    #[command(external_subcommand)]
    External(Vec<String>),
}

/// `clauth start`'s flags, the profile, and the `claude` passthrough.
#[derive(Args, Debug)]
pub(crate) struct StartArgs {
    /// Inject the profile's credentials into a clean throwaway runtime,
    /// dropping operator memory, plugins, and hooks. Run it in a clean cwd for
    /// a blind session.
    #[arg(long)]
    pub(crate) isolated: bool,
    /// Lift the run's transcripts + session sidecar state into the global
    /// store, overriding the profile's auto_rescue setting.
    #[arg(long, requires = "isolated", overrides_with = "no_rescue")]
    pub(crate) rescue: bool,
    /// Discard the run's isolated store, overriding the profile's auto_rescue
    /// setting.
    #[arg(long, requires = "isolated", overrides_with = "rescue")]
    pub(crate) no_rescue: bool,
    /// Profile to launch under.
    pub(crate) profile: String,
    /// Args handed to `claude` verbatim.
    #[arg(
        trailing_var_arg = true,
        allow_hyphen_values = true,
        value_name = "CLAUDE_ARGS"
    )]
    pub(crate) claude_args: Vec<String>,
}

impl StartArgs {
    /// `--isolated` selects the throwaway store; without it the start shares
    /// the profile's runtime tree.
    pub(crate) fn isolation(&self) -> Isolation {
        if self.isolated {
            Isolation::Isolated
        } else {
            Isolation::Shared
        }
    }

    /// `--rescue`/`--no-rescue` override the profile's auto_rescue setting;
    /// neither flag leaves it alone. clap's mutual `overrides_with` means the
    /// last one on the command line is the one that survives.
    pub(crate) fn rescue_override(&self) -> Option<bool> {
        match (self.rescue, self.no_rescue) {
            (true, _) => Some(true),
            (_, true) => Some(false),
            _ => None,
        }
    }
}

/// `clauth login`'s profile plus its auth-method flags.
#[derive(Args, Debug)]
pub(crate) struct LoginArgs {
    /// Profile to log in as. An existing name re-authenticates it in place.
    pub(crate) profile: String,
    /// API base url. Selects API-key mode.
    #[arg(long, value_name = "URL")]
    pub(crate) base_url: Option<String>,
    /// API key. Selects API-key mode. Visible in shell history and process
    /// listings, so prefer the echo-off prompt.
    #[arg(long, value_name = "KEY")]
    pub(crate) api_key: Option<String>,
    /// Capture a `claude setup-token` mint into the profile's long-lived
    /// session-token sidecar, pasted echo-off or piped on stdin. Takes effect
    /// on the next switch and touches nothing else about the profile.
    #[arg(long, conflicts_with_all = ["base_url", "api_key"])]
    pub(crate) setup_token: bool,
    /// Replace an existing long-lived token unprompted.
    #[arg(long, short = 'y', requires = "setup_token")]
    pub(crate) yes: bool,
    /// Default model for the profile: opus, sonnet, haiku, opusplan, or a full
    /// model id.
    #[arg(long, value_name = "ID")]
    pub(crate) model: Option<String>,
    /// Refuse to re-authenticate an existing profile — the race-proof CREATE
    /// for non-TTY callers (a menu-bar app never sees the confirm prompt).
    #[arg(long = "new", conflicts_with = "setup_token")]
    pub(crate) new_only: bool,
    /// Capture the live ~/.codex/auth.json (OpenAI Codex CLI) into a
    /// codex-harness profile instead of a browser OAuth login. Exclusive with
    /// the claude-shaped flags.
    #[arg(long, conflicts_with_all = ["base_url", "api_key", "model", "setup_token"])]
    pub(crate) codex: bool,
    /// With --codex only: mint a NEW codex login via the PKCE loopback flow
    /// straight into the profile store, never touching the live
    /// ~/.codex/auth.json.
    #[arg(long, requires = "codex")]
    pub(crate) browser: bool,
}

impl LoginArgs {
    /// API-key mode: capture a base_url + api_key pair instead of browser OAuth.
    pub(crate) fn is_api_mode(&self) -> bool {
        self.base_url.is_some() || self.api_key.is_some()
    }
}
