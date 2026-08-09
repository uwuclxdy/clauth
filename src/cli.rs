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

use std::net::SocketAddr;

use clap::{Args, Parser, Subcommand, ValueEnum};

use crate::runtime::Isolation;

/// Where a value-less `--listen` binds. Every interface, because the flag's
/// whole purpose is a client on a different machine; a loopback default would
/// parse fine and then serve nobody.
pub(crate) const DEFAULT_LISTEN: &str = "0.0.0.0:8443";

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

    /// List accounts as a table with each profile's usage
    ///
    /// Reads the same on-disk usage caches `status --json` prints, so the
    /// numbers match, and never fetches. The active profile is marked `*` and
    /// always shown; disabled profiles are hidden unless `--all`/`--disabled`.
    List {
        /// Also list disabled profiles, hidden by default.
        #[arg(long)]
        all: bool,
        /// Alias for --all.
        #[arg(long)]
        disabled: bool,
    },

    /// List Claude Code sessions as a table
    ///
    /// Exits 0 on success, 2 on a usage error, 1 on any other failure.
    Sessions {
        /// Emit a stable newest-first JSON array instead of the table. The field
        /// set is fixed; `tokens` and `cost` are null without `--tokens`.
        #[arg(long)]
        json: bool,
        /// Add each session's token total and cost. Reads every transcript in
        /// full, so a large store takes a while; omitted, both stay blank.
        #[arg(long)]
        tokens: bool,
    },

    /// Resume a session under a chosen profile
    ///
    /// Prompts on a TTY, defaulting to the session's last-ran profile (the
    /// active profile when that is unknown).
    Resume {
        /// Session id, or `latest`.
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
        #[arg(long, conflicts_with_all = ["no_standby", "replace", "status", "print_token", "rotate_token"])]
        standby: bool,
        /// The default's explicit spelling, kept so a spawner or unit still
        /// passing it behaves unchanged.
        #[arg(long, conflicts_with_all = ["replace", "status", "print_token", "rotate_token"])]
        no_standby: bool,
        /// Terminate the running daemon and take over, for an in-place upgrade.
        #[arg(long, conflicts_with_all = ["status", "print_token", "rotate_token"])]
        replace: bool,
        /// Print the running daemon, or exit 1 with no output when none is.
        #[arg(long, conflicts_with_all = ["listen", "print_token", "rotate_token"])]
        status: bool,
        /// Also serve the REST API over TLS; bare --listen means 0.0.0.0:8443
        ///
        /// For running the daemon on one machine and a client (clauth-tray) on
        /// another. TLS comes from this host's lego certificate — from
        /// /etc/lego/certificates on macOS and Linux, and from
        /// %AppData%\lego\certificates on Windows, either overridable in
        /// ~/.clauth/tls.json. Every request needs the bearer token from
        /// `--print-token`.
        ///
        /// The value-less spelling binds every interface, matching what the
        /// flag is for — a client on another machine. It is the same exposure
        /// the spelled-out form always had, just less to type; the flag itself
        /// still has to be passed, so nothing listens by accident.
        #[arg(
            long,
            value_name = "ADDR:PORT",
            num_args = 0..=1,
            default_missing_value = DEFAULT_LISTEN,
            conflicts_with_all = ["print_token", "rotate_token"],
        )]
        listen: Option<SocketAddr>,
        /// Print the REST API's auth token, creating it on first use, and exit.
        #[arg(long, conflicts_with = "rotate_token")]
        print_token: bool,
        /// Replace the REST API's auth token with a fresh one, print it, exit.
        ///
        /// Every client holding the old token starts getting 401s.
        #[arg(long)]
        rotate_token: bool,
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
    /// Follow the fallback chain, moving to the next account as each runs out
    ///
    /// The session starts on this profile and swaps onto the next chain member
    /// when its window is spent, instead of stopping where the account does. If
    /// a chain member is marked preferred (the home account), the session also
    /// returns to it once it reads clear and fresh again. Needs a running
    /// `clauth daemon` to decide the switches, and a profile that is already a
    /// chain member. Not available with --isolated, on a non-OAuth account, on
    /// macOS, or on a Windows host without symlink privilege — each of those is
    /// refused by name at launch.
    #[arg(long, conflicts_with = "isolated")]
    pub(crate) with_fallback: bool,
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
}

impl LoginArgs {
    /// API-key mode: capture a base_url + api_key pair instead of browser OAuth.
    pub(crate) fn is_api_mode(&self) -> bool {
        self.base_url.is_some() || self.api_key.is_some()
    }
}
