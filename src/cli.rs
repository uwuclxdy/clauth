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
    /// OAuth flow and writes the minted tokens into the profile; --manual runs
    /// the same login without a browser on this host (prints a link to open
    /// anywhere, then takes the code that page shows, for ssh sessions);
    /// passing either endpoint flag captures an API-key account instead,
    /// prompting for whatever a flag omitted (the key is read echo-off).
    ///
    /// An existing name re-authenticates in place: the fresh credential set
    /// replaces the old one while the profile's chain slot, env, and model
    /// settings survive. A browser login also leaves a stored endpoint and a
    /// working api key standing, since it renews the subscription login and
    /// that account's inference runs on the key. On an Alibaba Model Studio profile a bare login opens
    /// that console instead, capturing the session its usage figures need; that
    /// session expires 48 hours after the aliyun sign-in behind it rather than
    /// after this login, so it can arrive with minutes left. That profile's
    /// endpoint and api key are left untouched.
    ///
    /// Starting an Alibaba account from nothing therefore takes two steps: give
    /// it a Model Studio endpoint first (a Qwen preset on the Setup tab, or
    /// --base-url here), then run a bare login on that name. Which console a
    /// session is captured from is read off the endpoint, so a name with no
    /// endpoint yet has no console to open.
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

    /// Restore or remove a profile's long-lived session token
    ///
    /// The bare form is the inverse of `rolling-token`: it restores the static
    /// `claude setup-token` mint the rolling token superseded, so sessions go
    /// back to a year-scale bearer carrying two scopes, which nothing has to
    /// re-stamp.
    ///
    /// `--clear` is the FULL exit instead: it removes the long-lived token,
    /// the preserved mint backup, and the rolling-token flag together, so the
    /// profile's stored OAuth login is what switches install again and
    /// nothing re-creates a sidecar afterwards. The live credentials are
    /// relinked when the profile is active, and the clear is refused when the
    /// profile stores no other login.
    #[command(name = "static-token")]
    StaticToken {
        /// Profile whose long-lived token to operate on.
        profile: String,
        /// Remove the long-lived token, its preserved mint backup, and the
        /// rolling-token flag, instead of restoring the mint.
        #[arg(long)]
        clear: bool,
        /// Skip `--clear`'s confirm prompt. Required on a non-TTY stdin.
        #[arg(long, short = 'y', requires = "clear")]
        yes: bool,
    },

    /// Restore a disabled profile to every operational surface
    Enable {
        /// Profile to re-enable.
        profile: String,
    },

    /// Serve a profile's sessions a rolling token from its usage chain
    ///
    /// The daemon re-stamps `session-token.json` with the usage chain's current
    /// access token: full scopes and the account's `subscriptionType`, but NO
    /// refresh token. Sessions run bearers that unlock plan-gated models while
    /// the rotating chain stays clauth-private.
    ///
    /// Needs the clauth daemon running: the bearer dies in hours, and the
    /// daemon's scan is what re-stamps it before then.
    #[command(name = "rolling-token")]
    RollingToken {
        /// Profile to arm.
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

    /// List the delegate jobs clauth is holding
    ///
    /// One row per record in ~/.clauth/jobs/: background runs still going,
    /// blocking runs whose caller is still waiting, finished results nothing
    /// has collected yet, and runs whose server died. Read-only — stopping one
    /// is the `monitor` tool's job. An empty store exits 0.
    Jobs {
        /// Emit a stable newest-first JSON array instead of the table. The
        /// field set is fixed; a figure the record does not have is null.
        #[arg(long)]
        json: bool,
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

    /// Run the stdio MCP server (claude code launches this)
    Mcp,

    /// Set the herdr plugin up, or take it back out
    ///
    /// `clauth herdr install` runs herdr's own installer, then adds the two
    /// things a herdr plugin cannot declare for itself: the keybinding that
    /// opens the dashboard, and the sidebar row that renders which account
    /// each Claude Code pane burns. Both land in the user's herdr
    /// `config.toml`, and herdr validates the result before it is written.
    /// `clauth herdr uninstall` reverses both halves together.
    Herdr {
        #[command(subcommand)]
        cmd: HerdrCommand,
    },

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

    /// The bundled UserPromptSubmit / PostToolUse / SessionStart hook body: read
    /// the hook payload on stdin and tell the conversation when the account
    /// behind it changed.
    #[command(hide = true)]
    HookProfileChangedNote,

    /// The bundled SessionStart self-heal body: repair a broken plugin
    /// registration through agentgear. Prints only when something changed, so a
    /// healthy session start says nothing.
    #[command(hide = true)]
    SelfHeal,

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
    /// Uses a clean throwaway runtime, without your CLAUDE.md, plugins, hooks,
    /// skills, MCP servers or tools. Run it in a clean cwd for a blind session.
    /// Useful for testing or benchmarking. Transcripts and session state are
    /// lifted into the global store as clauth start shuts down, so the session
    /// stays resumable and its tokens counted. A hard kill skips that.
    #[arg(long)]
    pub(crate) isolated: bool,
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
}

/// `clauth login`'s profile plus its auth-method flags.
///
/// Capturing a long-lived token lives here as `--setup-token` because it IS a
/// login; removing one is `clauth static-token <profile> --clear`, a verb rather
/// than a flag, matching how `enable`/`disable` toggle per-profile state.
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
    /// Log in without a browser on this host: prints a link to open on any
    /// device, then takes the code that page shows. Same credential as the
    /// bare login; for ssh sessions and headless machines.
    #[arg(long, conflicts_with_all = ["base_url", "api_key", "setup_token"])]
    pub(crate) manual: bool,
    /// Replace an existing long-lived token unprompted.
    #[arg(long, short = 'y', requires = "setup_token")]
    pub(crate) yes: bool,
    /// Default model for the profile: opus, sonnet, haiku, opusplan, or a full
    /// model id.
    #[arg(long, value_name = "ID")]
    pub(crate) model: Option<String>,
}

/// `login` flags in completion order. The single source the bash/zsh/fish
/// scripts in `completions.rs` splice in, so a new login flag is added here
/// and nowhere else.
pub(crate) const LOGIN_FLAGS: &[&str] = &[
    "--base-url",
    "--api-key",
    "--setup-token",
    "--manual",
    "--yes",
    "-y",
    "--model",
];

impl LoginArgs {
    /// API-key mode: capture a base_url + api_key pair instead of browser OAuth.
    pub(crate) fn is_api_mode(&self) -> bool {
        self.base_url.is_some() || self.api_key.is_some()
    }
}

/// `clauth herdr <cmd>`: install and uninstall the plugin and its config wiring.
#[derive(Subcommand, Debug)]
pub(crate) enum HerdrCommand {
    /// Install the plugin into herdr and wire it into herdr's own config
    ///
    /// herdr's installer prints every command the plugin would run as you and
    /// asks before registering it; this passes that prompt straight through
    /// rather than answering it. Run from a clauth checkout it links the local
    /// `herdr-plugin/` directory instead of fetching the published one.
    Install {
        /// Key that opens the dashboard, in herdr's own binding syntax
        /// (`prefix+a`, `ctrl+alt+c`). Prompted for when omitted.
        #[arg(long, value_name = "SPEC")]
        key: Option<String>,
        /// Install the plugin and leave herdr's config.toml untouched.
        #[arg(long)]
        no_config: bool,
        /// Skip both confirm prompts, herdr's install preview included.
        /// Required on a non-TTY stdin, which gets no prompt.
        #[arg(long, short = 'y')]
        yes: bool,
    },

    /// Uninstall the plugin from herdr and drop the config clauth added
    ///
    /// Runs herdr's uninstall, then takes the keybinding and sidebar row `install` wrote back out of herdr's `config.toml`, leaving anything else in the file alone.
    Uninstall {
        /// Uninstall the plugin and leave herdr's config.toml untouched.
        #[arg(long)]
        no_config: bool,
        /// Skip the confirm prompt. Required on a non-TTY stdin, which gets no prompt.
        #[arg(long, short = 'y')]
        yes: bool,
    },

    /// Print one herdr knob for the plugin scripts
    ///
    /// `clauth herdr config get <key>` prints the knob's value on its own
    /// line, shell-shaped (`fit|half|split-right|split-top`, `on|off`, or the
    /// bare number).
    /// Hidden from help: this is the scripts' read path, not a human surface.
    #[command(hide = true)]
    Config {
        #[command(subcommand)]
        cmd: HerdrConfigCommand,
    },
}

/// `clauth herdr config <cmd>`: the plugin scripts' read path for the knobs
/// persisted under `[herdr]` in profiles.toml.
#[derive(Subcommand, Debug)]
pub(crate) enum HerdrConfigCommand {
    /// Print one knob's value on its own line
    Get {
        /// Knob name: popup_width, pane_tag, tag_watch_secs, border_label,
        /// delegate_dot, delegate_row_text.
        key: String,
    },
}
